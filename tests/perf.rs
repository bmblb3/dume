use dumelib::db::{Db, refresh};
use dumelib::recipe::{Apply, Recipe, select};
use rusqlite::{Connection, params};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::time::Instant;

fn timed<T>(what: &str, f: impl FnOnce() -> T) -> T {
    let t = Instant::now();
    let v = f();
    let s = t.elapsed();
    println!("{what}: {s:?}");
    assert!(s.as_secs() < 30, "{what} too slow");
    v
}

/// ~1M rows in a db with the schema of src/db.rs, rooted at a temp dir that also
/// holds a real small directory `real/` to refresh.
#[test]
#[ignore]
fn scan_and_query_a_million_entries() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().to_path_buf();
    let dbp = root.join("dume.db");
    for i in 0..10 {
        std::fs::create_dir_all(root.join("real")).unwrap();
        std::fs::write(root.join(format!("real/{i}")), vec![1u8; 100]).unwrap();
    }
    let mut c = Connection::open(&dbp).unwrap();
    c.execute_batch(
        "PRAGMA journal_mode=DELETE; PRAGMA synchronous=OFF;
         CREATE TABLE entries(path BLOB PRIMARY KEY, parent BLOB, name BLOB, is_dir INTEGER,
           asize INTEGER, dsize INTEGER, ino INTEGER, nlink INTEGER, mtime INTEGER NULL,
           tree_size INTEGER);
         CREATE TABLE meta(key TEXT PRIMARY KEY, value);",
    )
    .unwrap();
    let tx = c.transaction().unwrap();
    let mut n = 0i64;
    {
        let mut ins = tx
            .prepare("INSERT INTO entries VALUES(?,?,?,?,?,?,?,?,NULL,?)")
            .unwrap();
        let mut put = |path: &str, parent: Option<&str>, name: &str, dir: bool, size: i64| {
            n += 1;
            let nm = name.as_bytes();
            let p = parent.map(str::as_bytes);
            ins.execute(params![path.as_bytes(), p, nm, dir, size, size, n, 1, size])
                .unwrap();
        };
        // "big" has 100k files; 900 other dirs have 1000 each; "real" has 10.
        put("", None, "", true, 0);
        for (d, files) in std::iter::once(("big".to_string(), 100_000))
            .chain((0..900).map(|i| (format!("d{i}"), 1000)))
            .chain([("real".to_string(), 10)])
        {
            put(&d, Some(""), &d, true, files * 100);
            for i in 0..files {
                // Mix of numeric names (recipe hits) and others.
                let f = if i % 10 == 0 {
                    i.to_string()
                } else {
                    format!("f{i}")
                };
                put(&format!("{d}/{f}"), Some(&d), &f, false, 100);
            }
        }
        println!("rows: {n}");
    }
    tx.execute_batch(
        "CREATE INDEX entries_parent ON entries(parent); CREATE INDEX entries_name ON entries(name); CREATE INDEX entries_ino ON entries(ino) WHERE nlink>1;",
    )
    .unwrap();
    {
        let mut m = tx.prepare("INSERT INTO meta VALUES(?,?)").unwrap();
        m.execute(params!["root", root.as_os_str().as_bytes()])
            .unwrap();
        m.execute(params!["dev", 0i64]).unwrap();
        m.execute(params!["uid", 1000i64]).unwrap();
        m.execute(params!["scanned", 0i64]).unwrap();
        m.execute(params!["progver", "perf"]).unwrap();
    }
    tx.commit().unwrap();
    // Plan of refresh's UNDER predicate.
    let plan: Vec<String> = c
        .prepare("EXPLAIN QUERY PLAN DELETE FROM entries WHERE (path=?1 OR (path>=?2 AND path<?3))")
        .unwrap()
        .query_map(
            params![b"real".as_slice(), b"real/".as_slice(), b"real0".as_slice()],
            |r| r.get(3),
        )
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    println!("UNDER plan: {plan:?}");
    c.close().unwrap();

    let db = Db::open_ro(&dbp).unwrap();
    let kids = timed("children(root)", || db.children(&root).unwrap());
    assert_eq!(kids.len(), 902);
    let kids = timed("children(big)", || db.children(&root.join("big")).unwrap());
    assert_eq!(kids.len(), 100_000);
    timed("total_under(root)", || db.total_under(&root).unwrap());
    let r = Recipe {
        name: "t".into(),
        apply: Apply::Delete,
        sql: "SELECT path FROM entries WHERE name REGEXP '^[0-9]+$'".into(),
        file: PathBuf::new(),
    };
    let sel = timed("select REGEXP", || select(&db, &r, &root).unwrap());
    println!("selected: {}", sel.count);
    drop(db);
    let rf = timed("refresh(real)", || {
        refresh(&dbp, &[root.join("real")], 2, Default::default()).unwrap()
    });
    assert_eq!(rf.files, 10);
}
