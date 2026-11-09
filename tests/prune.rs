use dumelib::db::{Db, refresh, scan_to_db, scan_to_db_pruned};
use dumelib::scan::Prune;
use std::path::{Path, PathBuf};

fn uid() -> u32 {
    rustix::process::getuid().as_raw()
}

fn write(p: &Path, n: usize) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, vec![1u8; n]).unwrap();
}

fn globs(g: &[&str]) -> Vec<String> {
    g.iter().map(|s| s.to_string()).collect()
}

fn rows(db: &Path) -> Vec<String> {
    let c = rusqlite::Connection::open(db).unwrap();
    let mut st = c.prepare("SELECT path FROM entries ORDER BY path").unwrap();
    st.query_map([], |r| r.get::<_, Vec<u8>>(0))
        .unwrap()
        .map(|p| String::from_utf8(p.unwrap()).unwrap())
        .collect()
}

fn tree(t: &Path) -> PathBuf {
    let root = t.join("r");
    write(&root.join("keep"), 100);
    write(&root.join("p/.git/objects/aa/f1"), 4096);
    write(&root.join("p/.git/HEAD"), 10);
    write(&root.join("p/src/main.c"), 50);
    write(&root.join("case/processor0/0.1/U"), 8192);
    write(&root.join("case/processor12/0.1/U"), 8192);
    write(&root.join("case/processors/keep"), 1); // `processor[0-9]*` must not match this
    root
}

const G: [&str; 2] = [".git", "processor[0-9]*"];

#[test]
fn pruned_dirs_are_sized_but_not_stored() {
    let t = tempfile::tempdir().unwrap();
    let root = tree(t.path());
    let (full, pruned) = (t.path().join("full.db"), t.path().join("pruned.db"));
    scan_to_db(&root, &full, uid(), 2, Default::default()).unwrap();
    scan_to_db_pruned(&root, &pruned, uid(), 2, Default::default(), &globs(&G)).unwrap();

    let (a, b) = (Db::open_ro(&full).unwrap(), Db::open_ro(&pruned).unwrap());
    assert_eq!(a.total_under(&root).unwrap(), b.total_under(&root).unwrap());
    assert_eq!(
        a.total_under(&root.join("p/.git")).unwrap(),
        b.total_under(&root.join("p/.git")).unwrap()
    );

    let rows = rows(&pruned);
    assert!(rows.contains(&"p/.git".to_string()));
    assert!(rows.contains(&"case/processor0".to_string()));
    assert!(rows.contains(&"case/processors/keep".to_string()));
    assert!(
        rows.iter()
            .all(|r| !r.starts_with("p/.git/") && !r.contains("processor0/"))
    );

    let kids = b.children(&root.join("p")).unwrap();
    let git = kids.iter().find(|c| c.name == b".git").unwrap();
    assert!(git.pruned && git.is_dir);
    assert!(!kids.iter().find(|c| c.name == b"src").unwrap().pruned);

    // The confirm screen's file count survives pruning.
    let e = b.entry(&root.join("p/.git")).unwrap().unwrap();
    assert_eq!(e.files, 2);
    assert_eq!(
        e.files,
        a.entry(&root.join("p/.git")).unwrap().unwrap().files
    );
    assert_eq!(b.meta().unwrap().prune, globs(&G));
}

#[test]
fn hardlink_between_pruned_and_normal_counts_once() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("r");
    write(&root.join("a/.git/obj"), 8192);
    std::fs::create_dir_all(root.join("b")).unwrap();
    std::fs::hard_link(root.join("a/.git/obj"), root.join("b/link")).unwrap();
    let dbp = t.path().join("x.db");
    scan_to_db_pruned(&root, &dbp, uid(), 1, Default::default(), &globs(&[".git"])).unwrap();
    let db = Db::open_ro(&dbp).unwrap();
    let one = std::fs::metadata(root.join("b/link")).unwrap();
    use std::os::unix::fs::MetadataExt;
    assert_eq!(db.total_under(&root).unwrap(), one.blocks() * 512);
}

#[test]
fn refresh_keeps_pruning() {
    let t = tempfile::tempdir().unwrap();
    let root = tree(t.path());
    let dbp = t.path().join("x.db");
    scan_to_db_pruned(&root, &dbp, uid(), 1, Default::default(), &globs(&G)).unwrap();
    let before = rows(&dbp);

    write(&root.join("p/.git/objects/bb/f2"), 4096);
    // A folder holding a pruned one, then the pruned one itself (what `x` does).
    refresh(&dbp, &[root.join("p")], 1, Default::default()).unwrap();
    refresh(&dbp, &[root.join("p/.git")], 1, Default::default()).unwrap();
    assert_eq!(rows(&dbp), before);
    let db = Db::open_ro(&dbp).unwrap();
    assert_eq!(db.entry(&root.join("p/.git")).unwrap().unwrap().files, 3);

    // Whole-tree refresh must not treat a root that matches a glob as pruned.
    refresh(&dbp, std::slice::from_ref(&root), 1, Default::default()).unwrap();
    assert_eq!(rows(&dbp), before);
}

#[test]
fn old_databases_are_refused() {
    let t = tempfile::tempdir().unwrap();
    let dbp = t.path().join("old.db");
    let c = rusqlite::Connection::open(&dbp).unwrap();
    c.execute_batch(
        "CREATE TABLE entries(path BLOB, tree_size INTEGER); CREATE TABLE meta(key, value);",
    )
    .unwrap();
    drop(c);
    assert!(
        Db::open_ro(&dbp)
            .err()
            .unwrap()
            .to_string()
            .contains("older dume")
    );
}

#[test]
fn unreadable_databases_are_not_called_old() {
    let t = tempfile::tempdir().unwrap();
    let dbp = t.path().join("junk.db");
    std::fs::write(&dbp, vec![7u8; 4096]).unwrap();
    let e = Db::open_ro(&dbp).err().unwrap().to_string();
    assert!(!e.contains("older dume"), "{e}");
    assert!(e.contains("not a database"), "{e}");
}

#[test]
fn locked_databases_say_locked_not_old() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("data");
    write(&root.join("a/f"), 10);
    let dbp = t.path().join("dume.db");
    scan_to_db(&root, &dbp, uid(), 1, Default::default()).unwrap();
    // A writer in the middle of a commit holds an exclusive lock.
    let w = rusqlite::Connection::open(&dbp).unwrap();
    w.execute_batch("BEGIN EXCLUSIVE; INSERT INTO meta VALUES('x', 1);")
        .unwrap();
    let r = rusqlite::Connection::open_with_flags(&dbp, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap();
    r.busy_timeout(std::time::Duration::ZERO).unwrap();
    let e = r
        .prepare("SELECT files FROM entries LIMIT 0")
        .err()
        .unwrap();
    assert_eq!(
        e.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy)
    );
    // Through dume: the 5 s default wait, then a message that names the lock.
    let e = Db::open_ro(&dbp).err().unwrap().to_string();
    assert!(!e.contains("older dume"), "{e}");
    assert!(e.contains("locked"), "{e}");
    w.execute_batch("ROLLBACK").unwrap();
    Db::open_ro(&dbp).unwrap();
}

#[test]
fn glob_patterns() {
    let p = Prune::new(&globs(&[".git", "processor[0-9]*", "dyn?mic*", "[!a]x"])).unwrap();
    for yes in ["processor0", "processor12", ".git", "dynamicCode", "bx"] {
        assert!(p.matches(yes.as_bytes()), "{yes}");
    }
    for no in ["processors", "xgit", "a.git", "processor", "ax", "dynmic"] {
        assert!(!p.matches(no.as_bytes()), "{no}");
    }
    assert!(Prune::new(&globs(&["a/b"])).is_err());
    assert!(Prune::new(&globs(&["[abc"])).is_err());
    assert!(!Prune::new(&[]).unwrap().matches(b"x"));
}

#[test]
fn json_export_keeps_pruned_sizes() {
    let t = tempfile::tempdir().unwrap();
    let root = tree(t.path());
    let dbp = t.path().join("x.db");
    scan_to_db_pruned(&root, &dbp, uid(), 1, Default::default(), &globs(&G)).unwrap();
    let mut out = Vec::new();
    Db::open_ro(&dbp).unwrap().write_json(&mut out).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert!(String::from_utf8_lossy(&out).contains("(not indexed)"));
    assert!(v.is_array());
}
