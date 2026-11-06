use dumelib::bench;
use dumelib::db::scan_to_db;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

fn setup() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("r");
    for f in ["a/100/U", "a/200/U", "a/constant/x", "b/f"] {
        let p = root.join(f);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"hello").unwrap();
    }
    let db = t.path().join("x.db");
    let uid = rustix::process::getuid().as_raw();
    scan_to_db(&root, &db, uid, 1, Default::default()).unwrap();
    (t, root, db)
}

fn actions(t: &Path) -> std::path::PathBuf {
    let dir = t.join("actions");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    for (n, sql) in [
        (
            "good.sql",
            "-- dume-name: Time dirs\nSELECT path FROM entries WHERE name REGEXP '^[0-9]+$'",
        ),
        ("bad.sql", "-- dume-name: Broken\nSELEC path FROM entries"),
    ] {
        let p = dir.join(n);
        std::fs::write(&p, sql).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    dir
}

fn run(db: &Path, dir: &Path, actions: Option<&Path>, parallel: usize) -> String {
    let mut out = Vec::new();
    bench::run(db, dir, actions, parallel, 2, &mut out).unwrap();
    String::from_utf8(out).unwrap()
}

#[test]
fn bench_reports_each_recipe_and_parallel_wall_time() {
    let (t, root, db) = setup();
    let a = actions(t.path());
    let s = run(&db, &root, Some(&a), 2);
    assert!(s.contains("rows in view: 9"), "{s}");
    assert!(s.contains("scan only"), "{s}");
    let row = |n: &str| s.lines().find(|l| l.contains(n)).unwrap_or("").to_string();
    assert!(row("Time dirs").contains("2 paths"), "{s}");
    assert!(row("Broken").contains("failed"), "{s}");
    assert!(s.contains("sequential total"), "{s}");
    assert!(s.contains("parallel (2 threads)"), "{s}");
    assert!(!s.contains("MISMATCH"), "{s}");
}

#[test]
fn bench_without_actions_still_times_the_scan() {
    let (_t, root, db) = setup();
    let s = run(&db, &root, None, 4);
    assert!(s.contains("scan only"), "{s}");
    assert!(!s.contains("sequential total"), "{s}");
}
