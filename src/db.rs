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
//! A partial index on `ino` (nlink>1 rows) serves hardlink lookups in `refresh`.
//!
//! A scan is built in a sibling `.dume.db.tmp` in one transaction, then renamed
//! over the target, so readers see either the old or the new database.

use crate::scan::{FileRec, Progress, Scan, scan_progress};
use rusqlite::{Connection, OpenFlags, params};
use rustix::fs::{FlockOperation, flock};
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
    let _lock = lock_writer(db)?;
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

    let plan = plan(&scan, b"", &|_| false);

    let tx = conn.transaction().map_err(err)?;
    {
        plan.insert(&tx)?;
        tx.execute_batch("CREATE INDEX entries_parent ON entries(parent); CREATE INDEX entries_name ON entries(name); CREATE INDEX entries_ino ON entries(ino) WHERE nlink>1;")
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
    std::fs::File::open(tmp)?.sync_all()?;
    Ok(scan.files)
}

/// Held while this process is the single writer of a scan database.
pub struct WriterLock(#[allow(dead_code)] std::fs::File);

/// Exclusive non-blocking `flock` on the sidecar `<db>.lock` (created 0600, never
/// removed). Note: on NFS `flock` may be emulated with byte-range locks; accepted.
pub fn lock_writer(db: &Path) -> io::Result<WriterLock> {
    let mut name = db.as_os_str().to_owned();
    name.push(".lock");
    let f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(name)?;
    flock(&f, FlockOperation::NonBlockingLockExclusive).map_err(|e| {
        if e == rustix::io::Errno::WOULDBLOCK {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "another dume is updating this scan",
            )
        } else {
            e.into()
        }
    })?;
    Ok(WriterLock(f))
}

/// Rows for one scan, laid out under `base` (the scan root's path relative to the
/// db root; empty for a whole-tree scan).
struct Plan {
    dirs: Vec<PDir>,
    tree: Vec<u64>,
    keep: Vec<bool>,
}

struct PDir {
    path: Vec<u8>,
    parent: Option<Vec<u8>>,
    name: Vec<u8>,
    files: Vec<FileRec>,
    /// What each file adds to `tree_size` (0 for repeat hardlinks).
    counted: Vec<u64>,
}

fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    if dir.is_empty() {
        name.to_vec()
    } else {
        [dir, b"/", name].concat()
    }
}

/// `held(ino)`: the inode already counts elsewhere, so repeats here contribute 0.
fn plan(scan: &Scan, base: &[u8], held: &dyn Fn(u64) -> bool) -> Plan {
    // Directory ids ascend parent-before-child, so a reverse pass sums children first.
    let mut up = Vec::new();
    let mut dirs: Vec<PDir> = Vec::new();
    // Later hardlinks to an inode already seen contribute 0 (matches report/JSON).
    let mut seen = std::collections::HashSet::new();
    scan.for_each_dir(|d| {
        let counted = d
            .files
            .iter()
            .map(|f| {
                if f.nlink > 1 && (held(f.ino) || !seen.insert(f.ino)) {
                    0
                } else {
                    f.dsize
                }
            })
            .collect();
        let (path, parent, name) = match d.parent {
            None if base.is_empty() => (Vec::new(), None, d.name.to_vec()),
            None => {
                let (par, n) = ancestors_split(base);
                (base.to_vec(), Some(par), n)
            }
            Some(p) => (
                join(&dirs[p].path, d.name),
                Some(dirs[p].path.clone()),
                d.name.to_vec(),
            ),
        };
        up.push(d.parent);
        dirs.push(PDir {
            path,
            parent,
            name,
            files: d.files.to_vec(),
            counted,
        });
    });
    let mut tree: Vec<u64> = dirs.iter().map(|d| d.counted.iter().sum()).collect();
    let mut keep: Vec<bool> = dirs.iter().map(|d| !d.files.is_empty()).collect();
    for i in (0..dirs.len()).rev() {
        if let Some(p) = up[i] {
            tree[p] += tree[i];
            keep[p] |= keep[i];
        }
    }
    Plan { dirs, tree, keep }
}

/// (parent path, name) of a non-empty relative path.
fn ancestors_split(p: &[u8]) -> (Vec<u8>, Vec<u8>) {
    match p.iter().rposition(|&c| c == b'/') {
        Some(i) => (p[..i].to_vec(), p[i + 1..].to_vec()),
        None => (Vec::new(), p.to_vec()),
    }
}

/// Proper ancestors of a relative path, nearest first, ending with the root ("").
fn ancestors(p: &[u8]) -> Vec<Vec<u8>> {
    let mut v = Vec::new();
    let mut cur = p.to_vec();
    while !cur.is_empty() {
        cur = ancestors_split(&cur).0;
        v.push(cur.clone());
    }
    v
}

impl Plan {
    fn insert(&self, c: &Connection) -> io::Result<()> {
        let mut ins = c
            .prepare_cached("INSERT INTO entries VALUES(?,?,?,?,?,?,?,?,?,?)")
            .map_err(err)?;
        for (i, d) in self.dirs.iter().enumerate() {
            if !self.keep[i] {
                continue;
            }
            ins.execute(params![
                d.path,
                d.parent,
                d.name,
                1,
                0,
                0,
                0,
                0,
                None::<i64>,
                self.tree[i] as i64
            ])
            .map_err(err)?;
            for (f, &cnt) in d.files.iter().zip(&d.counted) {
                ins.execute(params![
                    join(&d.path, &f.name),
                    d.path,
                    f.name,
                    0,
                    f.asize as i64,
                    f.dsize as i64,
                    f.ino as i64,
                    f.nlink as i64,
                    f.mtime,
                    cnt as i64
                ])
                .map_err(err)?;
            }
        }
        Ok(())
    }
}

pub struct Refreshed {
    pub files: u64,
    pub errors: u64,
}

/// Add `delta` to `tree_size` of every proper ancestor of `path`.
fn adjust(tx: &Connection, path: &[u8], delta: i64) -> io::Result<()> {
    if delta != 0 {
        for a in ancestors(path) {
            tx.execute(
                "UPDATE entries SET tree_size=tree_size+? WHERE path=?",
                params![delta, a],
            )
            .map_err(err)?;
        }
    }
    Ok(())
}

/// Re-scan each of `dirs` (absolute, under the db's root) and replace its rows,
/// fixing ancestors' `tree_size`, all in one transaction on the live database: any
/// failure leaves it untouched. A vanished directory just has its rows removed.
pub fn refresh(
    db: &Path,
    dirs: &[PathBuf],
    threads: usize,
    progress: Arc<Progress>,
) -> io::Result<Refreshed> {
    let _lock = lock_writer(db)?;
    let mut conn =
        Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_WRITE).map_err(err)?;
    let m = meta_of(&conn).map_err(err)?;
    let mut rels = Vec::new();
    for d in dirs {
        let r = d.strip_prefix(&m.root).ok().filter(|r| {
            r.components()
                .all(|c| matches!(c, std::path::Component::Normal(_)))
        });
        let r =
            r.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path outside scan root"))?;
        rels.push(r.as_os_str().as_bytes().to_vec());
    }
    let tx = conn.transaction().map_err(err)?;
    let mut out = Refreshed {
        files: 0,
        errors: 0,
    };
    for (d, rel) in dirs.iter().zip(&rels) {
        let scan = match scan_progress(d, m.uid, threads, false, progress.clone(), true) {
            Ok((s, _)) => Some(s),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e), // dropping `tx` rolls everything back
        };
        if let Some(s) = &scan {
            out.files += s.files;
            out.errors += s.errors;
        }
        replace(&tx, rel, scan.as_ref())?;
    }
    tx.commit().map_err(err)?;
    Ok(out)
}

fn replace(tx: &Connection, rel: &[u8], scan: Option<&Scan>) -> io::Result<()> {
    // Subtree at `rel`; the empty path means everything.
    const UNDER: &str = "(?1=x'' OR path=?1 OR (path>=?2 AND path<?3))";
    let (lo, hi) = ([rel, b"/"].concat(), [rel, b"0"].concat());
    let old: i64 = tx
        .query_row(
            "SELECT COALESCE(SUM(tree_size),0) FROM entries WHERE path=?",
            [rel],
            |r| r.get(0),
        )
        .map_err(err)?;
    // Hardlinks: an inode counts once in the whole db (tree_size>0 on one row, 0 on
    // repeats). Removing the subtree may remove the counting row of an inode that
    // still has links outside; promote one of those so the total stays right.
    let counted: Vec<i64> = {
        let mut st = tx
            .prepare(&format!("SELECT DISTINCT ino FROM entries WHERE is_dir=0 AND nlink>1 AND tree_size>0 AND {UNDER}"))
            .map_err(err)?;
        st.query_map(params![rel, lo, hi], |r| r.get(0))
            .map_err(err)?
            .collect::<Result<_, _>>()
            .map_err(err)?
    };
    tx.execute(
        &format!("DELETE FROM entries WHERE {UNDER}"),
        params![rel, lo, hi],
    )
    .map_err(err)?;
    adjust(tx, rel, -old)?;
    for ino in counted {
        let top: Option<(Vec<u8>, i64, i64)> = tx
            .query_row(
                "SELECT path, dsize, tree_size FROM entries WHERE is_dir=0 AND nlink>1 AND ino=? ORDER BY tree_size DESC, path LIMIT 1",
                [ino],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map(Some)
            .or_else(|e| if e == rusqlite::Error::QueryReturnedNoRows { Ok(None) } else { Err(e) })
            .map_err(err)?;
        if let Some((p, dsize, 0)) = top {
            tx.execute("UPDATE entries SET tree_size=dsize WHERE path=?", [&p])
                .map_err(err)?;
            adjust(tx, &p, dsize)?;
        }
    }
    if let Some(scan) = scan {
        // Rows left in the db are all outside the subtree now.
        let held = |ino: u64| {
            tx.query_row(
                "SELECT 1 FROM entries WHERE is_dir=0 AND nlink>1 AND ino=? LIMIT 1",
                [ino as i64],
                |_| Ok(()),
            )
            .is_ok()
        };
        let p = plan(scan, rel, &held);
        if p.keep[0] {
            for a in ancestors(rel) {
                let (par, name) = if a.is_empty() {
                    (None, Vec::new())
                } else {
                    let (x, n) = ancestors_split(&a);
                    (Some(x), n)
                };
                tx.execute(
                    "INSERT OR IGNORE INTO entries VALUES(?,?,?,1,0,0,0,0,NULL,0)",
                    params![a, par, name],
                )
                .map_err(err)?;
            }
            p.insert(tx)?;
            adjust(tx, rel, p.tree[0] as i64)?;
        }
    }
    // Prune ancestors left with nothing in them.
    for a in ancestors(rel) {
        tx.execute(
            "DELETE FROM entries WHERE path=?1 AND is_dir=1 AND NOT EXISTS (SELECT 1 FROM entries c WHERE c.parent=?1)",
            [&a],
        )
        .map_err(err)?;
    }
    Ok(())
}
