//! Per-user SQLite scan store (rollback journal, never WAL; file mode 0600).
//!
//! `entries(path BLOB PRIMARY KEY, parent BLOB, name BLOB, is_dir, asize, dsize,
//! ino, nlink, mtime NULL, tree_size)`, indexed on `parent` and `name`. Paths are
//! raw bytes relative to the scan root with no leading `/`; the root directory has
//! the empty path and a NULL parent. Directories with no kept files are not stored.
//! A directory's `tree_size` is the sum of `dsize` of the files beneath it; a
//! file's `tree_size` equals its `dsize`, except that a hardlinked inode (nlink>1)
//! counts once in the whole db: later links keep their `dsize` but have `tree_size` 0.
//! `meta(key TEXT PRIMARY KEY, value)` holds root, dev, uid, scanned (unix secs),
//! progver.
//!
//! A partial index on `ino` (nlink>1 rows) serves hardlink lookups in `refresh`.
//! `import_json` builds the same schema from an old `dume.json` (mtime NULL).
//!
//! A scan is built in a sibling `.dume.db.tmp` in one transaction, then renamed
//! over the target, so readers see either the old or the new database.

use crate::json;
use crate::merge;
use crate::scan::{DirView, FileRec, Progress, Scan, scan_progress};
use rusqlite::{Connection, OpenFlags, params};
use rustix::fs::{FlockOperation, flock};
use std::io::{self, Write};
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

/// Build a database with `make(tmp)` beside `db` and rename it into place; the
/// previous database survives any failure. Takes the writer lock.
fn atomic(db: &Path, make: impl FnOnce(&Path) -> io::Result<u64>) -> io::Result<u64> {
    let _lock = lock_writer(db)?;
    let tmp = db.with_file_name(".dume.db.tmp");
    let _ = std::fs::remove_file(&tmp);
    let r = make(&tmp).and_then(|n| {
        std::fs::rename(&tmp, db)?;
        Ok(n)
    });
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// A file row as read back from the database.
pub struct DbFile {
    pub name: Vec<u8>,
    pub asize: u64,
    pub dsize: u64,
    pub ino: u64,
    pub nlink: u64,
    pub mtime: Option<i64>,
}

/// Events of a depth-first walk; every `Open` is matched by a `Close`.
pub enum Ev<'a> {
    Open(&'a [u8]),
    File(&'a DbFile),
    Close,
}

impl Db {
    /// Walk everything under the stored directory `here`, files before
    /// subdirectories, each sorted by name. Holds one directory's rows at a time.
    pub fn walk(&self, here: &Path, f: &mut dyn FnMut(Ev) -> io::Result<()>) -> io::Result<()> {
        let rel = self.rel(here)?;
        let found = self
            .conn
            .query_row(
                "SELECT 1 FROM entries WHERE path=? AND is_dir=1",
                [&rel],
                |_| Ok(()),
            )
            .is_ok();
        if !found {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} is not in the database", here.display()),
            ));
        }
        self.walk_dir(&rel, f)
    }

    fn walk_dir(&self, rel: &[u8], f: &mut dyn FnMut(Ev) -> io::Result<()>) -> io::Result<()> {
        let mut st = self
            .conn
            .prepare_cached("SELECT name, is_dir, asize, dsize, ino, nlink, mtime FROM entries WHERE parent=? ORDER BY is_dir, name")
            .map_err(err)?;
        let rows: Vec<(bool, DbFile)> = st
            .query_map([rel], |r| {
                Ok((
                    r.get(1)?,
                    DbFile {
                        name: r.get(0)?,
                        asize: r.get::<_, i64>(2)? as u64,
                        dsize: r.get::<_, i64>(3)? as u64,
                        ino: r.get::<_, i64>(4)? as u64,
                        nlink: r.get::<_, i64>(5)? as u64,
                        mtime: r.get(6)?,
                    },
                ))
            })
            .map_err(err)?
            .collect::<Result<_, _>>()
            .map_err(err)?;
        drop(st);
        for (is_dir, r) in &rows {
            if *is_dir {
                f(Ev::Open(&r.name))?;
                self.walk_dir(&join(rel, &r.name), f)?;
                f(Ev::Close)?;
            } else {
                f(Ev::File(r))?;
            }
        }
        Ok(())
    }

    /// Stream the whole database as an ncdu export, matching the scanner's output
    /// (every nlink>1 file carries `hlnkc`, so ncdu counts each inode once).
    pub fn write_json(&self, out: &mut impl Write) -> io::Result<()> {
        let m = self.meta()?;
        let mut buf = Vec::with_capacity(1 << 20);
        buf.extend_from_slice(b"[1,2,{\"progname\":\"dume\",\"progver\":");
        json::push_str(&mut buf, &m.progver);
        let _ = write!(
            buf,
            ",\"uid\":{},\"timestamp\":{}}},[{{\"name\":",
            m.uid, m.scanned
        );
        json::push_name(&mut buf, m.root.as_os_str().as_bytes());
        let _ = write!(buf, ",\"dev\":{}}}", m.dev);
        self.walk(&m.root, &mut |ev| {
            match ev {
                Ev::Open(n) => {
                    buf.extend_from_slice(b",[{\"name\":");
                    json::push_name(&mut buf, n);
                    buf.extend_from_slice(b"}");
                }
                Ev::File(r) => json::push_file(
                    &mut buf,
                    &r.name,
                    (r.asize, r.dsize, r.ino, r.nlink),
                    r.mtime,
                ),
                Ev::Close => buf.push(b']'),
            }
            if buf.len() >= 1 << 20 {
                out.write_all(&buf)?;
                buf.clear();
            }
            Ok(())
        })?;
        buf.extend_from_slice(b"]]\n");
        out.write_all(&buf)?;
        out.flush()
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
    atomic(db, |tmp| {
        let (scan, dev) = scan_progress(root, uid, threads, false, progress, true)?;
        let p = plan(|f| scan.for_each_dir(f), b"", &|_| false);
        write_db(tmp, &p, root, dev, uid)?;
        Ok(scan.files)
    })
}

/// Convert an old `dume.json` export into a database at `db` (mtime NULL).
/// Returns the number of files.
pub fn import_json(json: &Path, db: &Path) -> io::Result<u64> {
    let ex = merge::Export::load(json)?;
    let uid = ex.uid()?;
    // (parent index, name, files), parents before children.
    let mut dirs: Vec<(Option<usize>, Vec<u8>, Vec<FileRec>)> = Vec::new();
    let mut files = 0;
    let mut stack = vec![(None, ex.subtree(&[])?)];
    while let Some((parent, d)) = stack.pop() {
        let id = dirs.len();
        let name = d[0]["name"]
            .as_str()
            .unwrap_or_default()
            .as_bytes()
            .to_vec();
        let mut recs = Vec::new();
        for item in &d[1..] {
            if item.is_array() {
                continue;
            }
            let n = |k| item[k].as_u64().unwrap_or(0);
            recs.push(FileRec {
                name: item["name"]
                    .as_str()
                    .unwrap_or_default()
                    .as_bytes()
                    .to_vec(),
                asize: n("asize"),
                dsize: n("dsize"),
                ino: n("ino"),
                nlink: item["nlink"].as_u64().unwrap_or(1),
                mtime: 0,
            });
        }
        files += recs.len() as u64;
        dirs.push((parent, name, recs));
        // Reverse so children pop in document order (any order is valid).
        for c in d[1..].iter().rev().filter_map(|v| v.as_array()) {
            stack.push((Some(id), c.as_slice()));
        }
    }
    atomic(db, |tmp| {
        let mut p = plan(
            |f| {
                for (id, (parent, name, files)) in dirs.iter().enumerate() {
                    f(DirView {
                        id,
                        parent: *parent,
                        name,
                        files,
                    });
                }
            },
            b"",
            &|_| false,
        );
        p.null_mtime = true;
        write_db(
            tmp,
            &p,
            Path::new(&ex.root_name()),
            ex.dev().unwrap_or(0),
            uid,
        )?;
        Ok(files)
    })
}

fn write_db(tmp: &Path, plan: &Plan, root: &Path, dev: u64, uid: u32) -> io::Result<()> {
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
    std::fs::File::open(tmp)?.sync_all()
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
    /// Store mtime as NULL (rows imported from JSON have none).
    null_mtime: bool,
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
fn plan(
    each_dir: impl FnOnce(&mut dyn FnMut(DirView)),
    base: &[u8],
    held: &dyn Fn(u64) -> bool,
) -> Plan {
    // Directory ids ascend parent-before-child, so a reverse pass sums children first.
    let mut up = Vec::new();
    let mut dirs: Vec<PDir> = Vec::new();
    // Later hardlinks to an inode already seen contribute 0 (matches report/JSON).
    let mut seen = std::collections::HashSet::new();
    each_dir(&mut |d| {
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
    Plan {
        dirs,
        tree,
        keep,
        null_mtime: false,
    }
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
                    (!self.null_mtime).then_some(f.mtime),
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
        let p = plan(|f| scan.for_each_dir(f), rel, &held);
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
