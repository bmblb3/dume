use dumelib::db::{Db, lock_writer, refresh, scan_to_db};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

fn uid() -> u32 {
    rustix::process::getuid().as_raw()
}

fn write(p: &Path, n: usize) {
    std::fs::write(p, vec![1u8; n]).unwrap();
}

#[test]
fn scan_to_db_roundtrips_names_and_sizes() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("r");
    std::fs::create_dir_all(root.join("a/b")).unwrap();
    std::fs::create_dir(root.join("empty")).unwrap();
    let bad = OsStr::from_bytes(b"bad\xff\nq\"name");
    write(&root.join(bad), 10);
    write(&root.join("a/f"), 5000);
    write(&root.join("a/b/g"), 9000);
    let dbp = t.path().join("x.db");
    let n = scan_to_db(&root, &dbp, uid(), 2, Default::default()).unwrap();
    assert_eq!(n, 3);

    let db = Db::open_ro(&dbp).unwrap();
    let m = db.meta().unwrap();
    assert_eq!(m.uid, uid());
    assert_eq!(m.root, root);

    let top = db.children(&root).unwrap();
    let names: Vec<&[u8]> = top.iter().map(|c| c.name.as_slice()).collect();
    assert_eq!(names, [b"a".as_slice(), bad.as_bytes()]); // no "empty"
    assert!(top[0].is_dir && !top[1].is_dir);

    let a = db.children(&root.join("a")).unwrap();
    let names: Vec<&[u8]> = a.iter().map(|c| c.name.as_slice()).collect();
    assert_eq!(names, [b"b".as_slice(), b"f".as_slice()]);
    assert!(a[0].size >= a[1].size);
    let ab = db.total_under(&root.join("a/b")).unwrap();
    assert!(ab >= 9000);
    assert_eq!(db.total_under(&root.join("a")).unwrap(), ab + a[1].size);
    assert_eq!(db.total_under(&root).unwrap(), top[0].size + top[1].size);
}

#[test]
fn failed_scan_keeps_old_db() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("r");
    std::fs::create_dir(&root).unwrap();
    write(&root.join("f"), 1);
    let dbp = t.path().join("x.db");
    scan_to_db(&root, &dbp, uid(), 1, Default::default()).unwrap();
    let before = std::fs::read(&dbp).unwrap();
    assert!(
        scan_to_db(
            &t.path().join("missing"),
            &dbp,
            uid(),
            1,
            Default::default()
        )
        .is_err()
    );
    assert_eq!(std::fs::read(&dbp).unwrap(), before);
    assert!(!dumelib::db::tmp_path(&dbp).exists());
}

/// A tree with enough files that a refresh writes several pages.
fn many_files(t: &Path, n: usize) -> std::path::PathBuf {
    let root = t.join("r");
    std::fs::create_dir_all(root.join("d")).unwrap();
    for i in 0..n {
        write(&root.join(format!("d/f{i}")), 10);
    }
    root
}

/// What a writer killed mid-transaction leaves: `dst` plus a journal no process holds.
/// The transaction is kept open on `src` while both files are copied.
fn crashed_copy(src: &Path, dst: &Path) {
    let c = rusqlite::Connection::open(src).unwrap();
    c.execute_batch(
        "PRAGMA cache_size=1; BEGIN; UPDATE entries SET asize = asize + 1, dsize = dsize + 7;",
    )
    .unwrap();
    let journal = dumelib::db::journal_path(src);
    assert!(journal.exists(), "the open transaction has a journal");
    std::fs::copy(src, dst).unwrap();
    std::fs::copy(&journal, dumelib::db::journal_path(dst)).unwrap();
    c.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn interrupted_update_is_named_and_a_rescan_recovers() {
    let t = tempfile::tempdir().unwrap();
    let root = many_files(t.path(), 300);
    let good = t.path().join("good.db");
    scan_to_db(&root, &good, uid(), 1, Default::default()).unwrap();
    let dbp = t.path().join("dume.db");
    crashed_copy(&good, &dbp);

    // Every read-only open fails while the journal is there; say why, not "older dume".
    let e = Db::open_ro(&dbp).err().unwrap().to_string();
    assert!(e.contains("interrupted"), "{e}");
    assert!(!e.contains("older dume"), "{e}");

    // A full rescan replaces the file and drops the journal that belonged to the old one.
    scan_to_db(&root, &dbp, uid(), 1, Default::default()).unwrap();
    assert!(!dumelib::db::journal_path(&dbp).exists());
    let db = Db::open_ro(&dbp).unwrap();
    assert_eq!(
        db.total_under(&root).unwrap(),
        Db::open_ro(&good).unwrap().total_under(&root).unwrap()
    );
}

#[test]
fn tmp_file_is_per_database() {
    let p = |s: &str| dumelib::db::tmp_path(Path::new(s));
    assert_eq!(p("/s/dume.db"), Path::new("/s/.dume.db.tmp"));
    assert_eq!(p("/var/tmp/alice.db"), Path::new("/var/tmp/.alice.db.tmp"));
    assert_ne!(p("/var/tmp/alice.db"), p("/var/tmp/bob.db"));
}

#[test]
fn db_file_is_0600_and_uses_rollback_journal() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("r");
    std::fs::create_dir(&root).unwrap();
    write(&root.join("f"), 1);
    let dbp = t.path().join("x.db");
    scan_to_db(&root, &dbp, uid(), 1, Default::default()).unwrap();
    let mode = std::fs::metadata(&dbp).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let c = rusqlite::Connection::open(&dbp).unwrap();
    let j: String = c
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(j, "delete");
}

#[test]
fn hardlinks_are_counted_once_in_tree_size() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("r");
    std::fs::create_dir_all(root.join("a")).unwrap();
    std::fs::create_dir_all(root.join("b")).unwrap();
    write(&root.join("a/f"), 8000);
    std::fs::hard_link(root.join("a/f"), root.join("b/g")).unwrap();
    let dbp = t.path().join("x.db");
    assert_eq!(
        scan_to_db(&root, &dbp, uid(), 1, Default::default()).unwrap(),
        2
    );
    let db = Db::open_ro(&dbp).unwrap();
    let one = std::fs::metadata(root.join("a/f")).unwrap().blocks() * 512;
    assert_eq!(db.total_under(&root).unwrap(), one);
    assert_eq!(db.children(&root.join("a")).unwrap().len(), 1);
    assert_eq!(db.children(&root.join("b")).unwrap().len(), 1);
}

fn dump(db: &Path) -> Vec<(Vec<u8>, bool, i64, i64)> {
    let c = rusqlite::Connection::open(db).unwrap();
    let mut st = c
        .prepare("SELECT path, is_dir, dsize, tree_size FROM entries ORDER BY path")
        .unwrap();
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// Fresh scan of `root` into a scratch db next to `dbp`, dumped.
fn fresh(root: &Path, dbp: &Path) -> Vec<(Vec<u8>, bool, i64, i64)> {
    let f = dbp.with_file_name("fresh.db");
    scan_to_db(root, &f, uid(), 1, Default::default()).unwrap();
    dump(&f)
}

fn tree(t: &Path) -> std::path::PathBuf {
    let root = t.join("r");
    std::fs::create_dir_all(root.join("a/b")).unwrap();
    std::fs::create_dir_all(root.join("x/y")).unwrap();
    write(&root.join("a/f"), 5000);
    write(&root.join("a/b/g"), 9000);
    write(&root.join("x/y/h"), 100);
    write(&root.join("c"), 10);
    root
}

#[test]
fn refresh_updates_changed_dir_and_ancestor_sizes() {
    let t = tempfile::tempdir().unwrap();
    let root = tree(t.path());
    let dbp = t.path().join("x.db");
    scan_to_db(&root, &dbp, uid(), 1, Default::default()).unwrap();
    write(&root.join("a/b/g"), 20000);
    write(&root.join("a/b/new"), 4000);
    std::fs::create_dir(root.join("a/b/d")).unwrap();
    write(&root.join("a/b/d/z"), 1);
    // x/y was empty-of-files-before? give a previously unstored dir content too
    std::fs::create_dir(root.join("e")).unwrap();
    let r = refresh(&dbp, &[root.join("a/b")], 2, Default::default()).unwrap();
    assert_eq!(r.files, 3);
    assert_eq!(dump(&dbp), fresh(&root, &dbp));
    write(&root.join("e/q"), 7);
    refresh(&dbp, &[root.join("e")], 1, Default::default()).unwrap();
    assert_eq!(dump(&dbp), fresh(&root, &dbp));
    refresh(&dbp, std::slice::from_ref(&root), 1, Default::default()).unwrap();
    assert_eq!(dump(&dbp), fresh(&root, &dbp));
    assert!(refresh(&dbp, &[t.path().to_path_buf()], 1, Default::default()).is_err());
    assert_eq!(dump(&dbp), fresh(&root, &dbp));
}

#[test]
fn refresh_removes_vanished_dir() {
    let t = tempfile::tempdir().unwrap();
    let root = tree(t.path());
    let dbp = t.path().join("x.db");
    scan_to_db(&root, &dbp, uid(), 1, Default::default()).unwrap();
    std::fs::remove_dir_all(root.join("a/b")).unwrap();
    std::fs::remove_dir_all(root.join("x/y")).unwrap();
    refresh(
        &dbp,
        &[root.join("a/b"), root.join("x/y")],
        1,
        Default::default(),
    )
    .unwrap();
    assert_eq!(dump(&dbp), fresh(&root, &dbp)); // x pruned, a kept
    assert!(
        Db::open_ro(&dbp)
            .unwrap()
            .children(&root.join("x"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn refresh_keeps_hardlinks_counted_once() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("r");
    std::fs::create_dir_all(root.join("a")).unwrap();
    std::fs::create_dir_all(root.join("b")).unwrap();
    write(&root.join("a/f"), 8000);
    std::fs::hard_link(root.join("a/f"), root.join("b/g")).unwrap();
    let dbp = t.path().join("x.db");
    scan_to_db(&root, &dbp, uid(), 1, Default::default()).unwrap();
    let one = std::fs::metadata(root.join("a/f")).unwrap().blocks() as i64 * 512;
    for d in ["a", "b"] {
        refresh(&dbp, &[root.join(d)], 1, Default::default()).unwrap();
        let db = Db::open_ro(&dbp).unwrap();
        assert_eq!(db.total_under(&root).unwrap() as i64, one);
    }
    // drop whichever link holds the count; the other must take it over
    for d in ["a", "b"] {
        std::fs::remove_file(root.join(d).join(if d == "a" { "f" } else { "g" })).unwrap();
        refresh(&dbp, &[root.join(d)], 1, Default::default()).unwrap();
        let db = Db::open_ro(&dbp).unwrap();
        assert_eq!(
            db.total_under(&root).unwrap() as i64,
            if d == "a" { one } else { 0 }
        );
    }
}

#[test]
fn refresh_cancelled_midway_leaves_db_unchanged() {
    let t = tempfile::tempdir().unwrap();
    let root = tree(t.path());
    let dbp = t.path().join("x.db");
    scan_to_db(&root, &dbp, uid(), 1, Default::default()).unwrap();
    let before = dump(&dbp);
    write(&root.join("a/b/new"), 4000);
    // first dir applies fine, second (a regular file) fails -> all rolled back
    let r = refresh(
        &dbp,
        &[root.join("a/b"), root.join("c")],
        1,
        Default::default(),
    );
    assert!(r.is_err());
    assert_eq!(dump(&dbp), before);
}

#[test]
fn second_writer_is_refused_while_first_holds_lock() {
    let t = tempfile::tempdir().unwrap();
    let dbp = t.path().join("x.db");
    let g = lock_writer(&dbp).unwrap();
    let e = lock_writer(&dbp).err().unwrap();
    assert_eq!(e.to_string(), "another dume is updating this scan");
    let root = tree(t.path());
    let e = scan_to_db(&root, &dbp, uid(), 1, Default::default()).unwrap_err();
    assert_eq!(e.to_string(), "another dume is updating this scan");
    drop(g);
    lock_writer(&dbp).unwrap();
    let mode = std::fs::metadata(t.path().join("x.db.lock"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}
