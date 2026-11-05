use dumelib::db::{Db, scan_to_db};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
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
    assert!(!t.path().join(".dume.db.tmp").exists());
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
