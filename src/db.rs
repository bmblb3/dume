//! Per-user SQLite scan store (rollback journal, never WAL; file mode 0600).
//!
//! `entries(path BLOB PRIMARY KEY, parent BLOB, name BLOB, is_dir, asize, dsize,
//! ino, nlink, mtime NULL, tree_size)`, indexed on `parent` and `name`. Paths are
//! raw bytes relative to the scan root with no leading `/`; the root directory has
//! the empty path and a NULL parent. Directories with no kept files are not stored.
//! A directory's `tree_size` is the sum of `dsize` of the files beneath it; a
//! file's `tree_size` equals its `dsize`. Hardlinks are stored as scanned (the
//! scanner does not flag repeats), so `dsize` is not de-duplicated per inode.
//! `meta(key TEXT PRIMARY KEY, value)` holds root, dev, uid, scanned (unix secs),
//! progver.
//!
//! A scan is built in a sibling `.dume.db.tmp` in one transaction, then renamed
//! over the target, so readers see either the old or the new database.

use crate::scan::{Progress, scan_progress};
use rusqlite::{Connection, OpenFlags, params};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn err(e: rusqlite::Error) -> io::Error {
    io::Error::other(e)
}

pub struct Db {
    conn: Connection,
    root: PathBuf,
}

pub struct Meta {
    pub root: PathBuf,
    pub dev: u64,
    pub uid: u32,
    pub scanned: u64,
    pub progver: String,
}

pub struct Child {
    pub name: Vec<u8>,
    pub size: u64,
    pub is_dir: bool,
}

fn meta_of(c: &Connection) -> rusqlite::Result<Meta> {
    let get = |k: &str| {
        c.query_row("SELECT value FROM meta WHERE key=?", [k], |r| {
            r.get::<_, rusqlite::types::Value>(0)
        })
    };
    let int = |k| -> rusqlite::Result<i64> {
        match get(k)? {
            rusqlite::types::Value::Integer(i) => Ok(i),
            _ => Err(rusqlite::Error::InvalidQuery),
        }
    };
    let blob = |k| -> rusqlite::Result<Vec<u8>> {
        match get(k)? {
            rusqlite::types::Value::Blob(b) => Ok(b),
            rusqlite::types::Value::Text(t) => Ok(t.into_bytes()),
            _ => Err(rusqlite::Error::InvalidQuery),
        }
    };
    Ok(Meta {
        root: PathBuf::from(std::ffi::OsString::from_vec(blob("root")?)),
        dev: int("dev")? as u64,
        uid: int("uid")? as u32,
        scanned: int("scanned")? as u64,
        progver: String::from_utf8_lossy(&blob("progver")?).into_owned(),
    })
}

impl Db {
    pub fn open_ro(path: &Path) -> io::Result<Db> {
        let conn =
            Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(err)?;
        let root = meta_of(&conn).map_err(err)?.root;
        Ok(Db { conn, root })
    }

    pub fn meta(&self) -> io::Result<Meta> {
        meta_of(&self.conn).map_err(err)
    }

    /// Path relative to the scan root, as stored (empty for the root).
    fn rel(&self, here: &Path) -> io::Result<Vec<u8>> {
        let r = here
            .strip_prefix(&self.root)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path outside scan root"))?;
        Ok(r.as_os_str().as_bytes().to_vec())
    }

    /// Direct children of `here`, largest first, ties by name.
    pub fn children(&self, here: &Path) -> io::Result<Vec<Child>> {
        let mut st = self
            .conn
            .prepare_cached("SELECT name, tree_size, is_dir FROM entries WHERE parent=? ORDER BY tree_size DESC, name")
            .map_err(err)?;
        let rows = st
            .query_map([self.rel(here)?], |r| {
                Ok(Child {
                    name: r.get(0)?,
                    size: r.get::<_, i64>(1)? as u64,
                    is_dir: r.get(2)?,
                })
            })
            .map_err(err)?;
        rows.collect::<Result<_, _>>().map_err(err)
    }

    /// Total `dsize` of the files under `here` (0 if nothing is stored there).
    pub fn total_under(&self, here: &Path) -> io::Result<u64> {
        let t: Option<i64> = self
            .conn
            .query_row(
                "SELECT tree_size FROM entries WHERE path=?",
                [self.rel(here)?],
                |r| r.get(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                e => Err(e),
            })
            .map_err(err)?;
        Ok(t.unwrap_or(0) as u64)
    }
}

/// Scan `root` for `uid`'s files into the database at `db`, replacing it
/// atomically. Returns the number of files kept.
pub fn scan_to_db(
    root: &Path,
    db: &Path,
    uid: u32,
    threads: usize,
    progress: Arc<Progress>,
) -> io::Result<u64> {
    let tmp = db.with_file_name(".dume.db.tmp");
    let _ = std::fs::remove_file(&tmp);
    let r = build(root, &tmp, uid, threads, progress).and_then(|n| {
        std::fs::rename(&tmp, db)?;
        Ok(n)
    });
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

fn build(
    root: &Path,
    tmp: &Path,
    uid: u32,
    threads: usize,
    progress: Arc<Progress>,
) -> io::Result<u64> {
    let (scan, dev) = scan_progress(root, uid, threads, false, progress, true)?;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(tmp)?;
    let mut conn = Connection::open(tmp).map_err(err)?;
    conn.execute_batch(
        "PRAGMA journal_mode=DELETE; PRAGMA synchronous=OFF;
         CREATE TABLE entries(path BLOB PRIMARY KEY, parent BLOB, name BLOB, is_dir INTEGER,
           asize INTEGER, dsize INTEGER, ino INTEGER, nlink INTEGER, mtime INTEGER NULL,
           tree_size INTEGER);
         CREATE TABLE meta(key TEXT PRIMARY KEY, value);",
    )
    .map_err(err)?;

    // Directory ids ascend parent-before-child, so a reverse pass sums children first.
    let mut dirs = Vec::new();
    scan.for_each_dir(|d| dirs.push((d.parent, d.name.to_vec(), d.files.to_vec())));
    let mut tree: Vec<u64> = dirs
        .iter()
        .map(|d| d.2.iter().map(|f| f.dsize).sum())
        .collect();
    let mut keep: Vec<bool> = dirs.iter().map(|d| !d.2.is_empty()).collect();
    for i in (0..dirs.len()).rev() {
        if let Some(p) = dirs[i].0 {
            tree[p] += tree[i];
            keep[p] |= keep[i];
        }
    }
    let mut paths: Vec<Vec<u8>> = Vec::with_capacity(dirs.len());
    for d in &dirs {
        paths.push(match d.0 {
            None => Vec::new(),
            Some(p) if paths[p].is_empty() => d.1.clone(),
            Some(p) => [paths[p].as_slice(), b"/", &d.1].concat(),
        });
    }

    let tx = conn.transaction().map_err(err)?;
    {
        let mut ins = tx
            .prepare("INSERT INTO entries VALUES(?,?,?,?,?,?,?,?,?,?)")
            .map_err(err)?;
        for (i, d) in dirs.iter().enumerate() {
            if !keep[i] {
                continue;
            }
            let parent = d.0.map(|p| &paths[p]);
            ins.execute(params![
                paths[i],
                parent,
                d.1,
                1,
                0,
                0,
                0,
                0,
                None::<i64>,
                tree[i] as i64
            ])
            .map_err(err)?;
            for f in &d.2 {
                let p = if paths[i].is_empty() {
                    f.name.clone()
                } else {
                    [paths[i].as_slice(), b"/", &f.name].concat()
                };
                ins.execute(params![
                    p,
                    paths[i],
                    f.name,
                    0,
                    f.asize as i64,
                    f.dsize as i64,
                    f.ino as i64,
                    f.nlink as i64,
                    f.mtime,
                    f.dsize as i64
                ])
                .map_err(err)?;
            }
        }
        tx.execute_batch("CREATE INDEX entries_parent ON entries(parent); CREATE INDEX entries_name ON entries(name);")
            .map_err(err)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let mut m = tx.prepare("INSERT INTO meta VALUES(?,?)").map_err(err)?;
        m.execute(params!["root", root.as_os_str().as_bytes()])
            .map_err(err)?;
        m.execute(params!["dev", dev as i64]).map_err(err)?;
        m.execute(params!["uid", uid as i64]).map_err(err)?;
        m.execute(params!["scanned", now as i64]).map_err(err)?;
        m.execute(params!["progver", env!("CARGO_PKG_VERSION")])
            .map_err(err)?;
    }
    tx.commit().map_err(err)?;
    conn.close().map_err(|(_, e)| err(e))?;
    Ok(scan.files)
}
