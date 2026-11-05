use dumelib::db::{Db, scan_to_db};
use dumelib::recipe::{Apply, Recipe, Selection, select};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

const FOAM: &str = "SELECT path FROM entries \
    WHERE path REGEXP '(^|/)([0-9]+|polyMesh|dynamicCode|processor[0-9]*)$' \
    OR name REGEXP '^core'";

fn recipe(sql: &str) -> Recipe {
    Recipe {
        name: "t".into(),
        apply: Apply::Delete,
        sql: sql.into(),
        file: PathBuf::new(),
    }
}

fn put(p: &Path, n: usize) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, vec![1u8; n]).unwrap();
}

/// OpenFOAM-like tree under `r`; returns the database.
fn foam() -> (tempfile::TempDir, PathBuf, Db) {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("r");
    for f in [
        "proj/case/0/U",
        "proj/case/100/U",
        "proj/case/200/U",
        "proj/case/processor0/0/U",
        "proj/case/processor0/100/U",
        "proj/case/processor0/constant/x",
        "proj/case/constant/polyMesh/points",
        "proj/case/core.123",
        "proj/case/system/controlDict",
        "proj/case.bak/100/U",
        "proj/case2/100/U",
    ] {
        put(&root.join(f), 5000);
    }
    let dbp = t.path().join("x.db");
    let uid = rustix::process::getuid().as_raw();
    scan_to_db(&root, &dbp, uid, 1, Default::default()).unwrap();
    let db = Db::open_ro(&dbp).unwrap();
    (t, root, db)
}

fn paths(s: &Selection) -> Vec<&[u8]> {
    s.hits.iter().map(|h| h.path.as_slice()).collect()
}

#[test]
fn regexp_selects_dirs_and_files_with_nesting_deduped() {
    let (_t, root, db) = foam();
    let case = root.join("proj/case");
    let s = select(&db, &recipe(FOAM), &case).unwrap();
    assert_eq!(
        paths(&s),
        [
            b"proj/case/0".as_slice(),
            b"proj/case/100",
            b"proj/case/200",
            b"proj/case/constant/polyMesh",
            b"proj/case/core.123",
            b"proj/case/processor0",
        ]
    );
    let p0 = s
        .hits
        .iter()
        .find(|h| h.path.ends_with(b"processor0"))
        .unwrap();
    assert!(p0.is_dir);
    assert_eq!(p0.size, db.total_under(&case.join("processor0")).unwrap());
    let core = s
        .hits
        .iter()
        .find(|h| h.path.ends_with(b"core.123"))
        .unwrap();
    assert!(!core.is_dir && core.size > 0 && core.mtime.is_some());
    assert_eq!(s.count, 6);
    assert_eq!(s.size, s.hits.iter().map(|h| h.size).sum::<u64>());
}

#[test]
fn scoped_to_here_only() {
    let (_t, root, db) = foam();
    let s = select(&db, &recipe(FOAM), &root.join("proj/case/processor0")).unwrap();
    assert_eq!(
        paths(&s),
        [
            b"proj/case/processor0/0".as_slice(),
            b"proj/case/processor0/100"
        ]
    );
    // Only the view is readable: the whole table is not.
    let case = root.join("proj/case");
    for sql in [
        "SELECT path FROM main.entries",
        "SELECT e.path FROM entries v JOIN main.entries e ON e.parent = v.path",
        "SELECT name AS path FROM sqlite_schema",
        "SELECT value AS path FROM meta",
    ] {
        let e = select(&db, &recipe(sql), &case).unwrap_err();
        assert!(
            e.contains("prohibited") || e.contains("not authorized"),
            "{sql}: {e}"
        );
    }
    let s = select(&db, &recipe("SELECT path FROM temp.entries"), &case).unwrap();
    assert!(s.count > 0);
    // A subquery aliased `entries` passes the authorizer (SQLite reports the
    // alias as the accessor), but the Rust filter still keeps hits under `here`.
    let sql = "WITH entries AS (SELECT * FROM main.entries) SELECT path FROM entries";
    let s = select(&db, &recipe(sql), &case.join("processor0")).unwrap();
    assert!(s.count > 0);
    assert!(
        s.hits
            .iter()
            .all(|h| h.path.starts_with(b"proj/case/processor0/"))
    );
    // Invented paths cannot escape `here`.
    let sql = "SELECT path FROM entries WHERE name REGEXP '^100$' \
               UNION ALL SELECT 'proj/case/nope' UNION ALL SELECT '../x' UNION ALL SELECT x''";
    let s = select(&db, &recipe(sql), &case).unwrap();
    assert_eq!(
        paths(&s),
        [b"proj/case/100".as_slice(), b"proj/case/processor0/100"]
    );
    // `here` itself is never a hit.
    let s = select(&db, &recipe(sql), &root.join("proj/case/100")).unwrap();
    assert!(s.hits.is_empty());
    assert!(select(&db, &recipe(FOAM), Path::new("/elsewhere")).is_err());
}

#[test]
fn here_is_scan_root() {
    let (_t, root, db) = foam();
    let s = select(&db, &recipe("SELECT path FROM entries"), &root).unwrap();
    assert_eq!(paths(&s), [b"proj".as_slice()]);
    assert_eq!(s.size, db.total_under(&root).unwrap());
    let s = select(&db, &recipe(FOAM), &root).unwrap();
    assert!(paths(&s).contains(&b"proj/case2/100".as_slice()));
    assert!(paths(&s).contains(&b"proj/case.bak/100".as_slice()));
}

#[test]
fn bad_sql_missing_path_column_and_attach_are_errors_not_panics() {
    let (t, root, db) = foam();
    let e = select(&db, &recipe("SELECT name FROM entries"), &root).unwrap_err();
    assert!(e.contains("recipe must return a path column"), "{e}");
    let other = t.path().join("o.db");
    for sql in [
        "SELEC path FROM entries".to_string(),
        format!("ATTACH DATABASE '{}' AS o", other.display()),
        "PRAGMA table_info(entries)".into(),
        "SELECT name AS path FROM pragma_table_info('entries')".into(),
        "SELECT load_extension('/nonexistent') AS path".into(),
        "".into(),
    ] {
        assert!(select(&db, &recipe(&sql), &root).is_err(), "{sql}");
    }
    assert!(!other.exists());
}

#[test]
fn read_only_connection_cannot_write() {
    let (_t, root, db) = foam();
    let before = select(&db, &recipe("SELECT path FROM entries"), &root).unwrap();
    for sql in [
        "DELETE FROM entries",
        "DELETE FROM main.entries RETURNING path",
        "INSERT INTO main.entries(path) VALUES(x'00') RETURNING path",
        "UPDATE main.entries SET tree_size=0 RETURNING path",
        "CREATE TABLE t(path)",
        "CREATE TEMP TABLE t AS SELECT path FROM entries",
        "DROP VIEW entries",
    ] {
        assert!(select(&db, &recipe(sql), &root).is_err(), "{sql}");
    }
    let after = select(&db, &recipe("SELECT path FROM entries"), &root).unwrap();
    assert_eq!(before.size, after.size);
    assert_eq!(db.total_under(&root).unwrap(), before.size);
}

#[test]
fn multiple_statements_are_rejected() {
    let (_t, root, db) = foam();
    let sql = "SELECT path FROM entries; DELETE FROM entries";
    assert!(select(&db, &recipe(sql), &root).is_err());
    // A trailing semicolon and comment are fine; so is a CTE.
    let sql =
        "WITH t AS (SELECT path FROM entries WHERE name REGEXP '^U$') SELECT path FROM t; -- end";
    assert_eq!(select(&db, &recipe(sql), &root).unwrap().count, 7);
}

#[test]
fn invalid_regex_is_an_error() {
    let (_t, root, db) = foam();
    let e = select(
        &db,
        &recipe("SELECT path FROM entries WHERE name REGEXP '('"),
        &root,
    )
    .unwrap_err();
    assert!(e.contains("REGEXP"), "{e}");
}

#[test]
fn non_select_statement_message() {
    let (_t, root, db) = foam();
    let e = select(&db, &recipe("BEGIN"), &root).unwrap_err();
    assert!(e.contains("recipe must be a single SELECT query"), "{e}");
}

#[test]
fn non_utf8_name_is_selected_exactly() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("r");
    put(&root.join("d").join(OsStr::from_bytes(b"caf\xe9")), 5000);
    put(&root.join("d/caf\u{e9}"), 5000);
    let dbp = t.path().join("x.db");
    let uid = rustix::process::getuid().as_raw();
    scan_to_db(&root, &dbp, uid, 1, Default::default()).unwrap();
    let db = Db::open_ro(&dbp).unwrap();
    let sql = r"SELECT path FROM entries WHERE name REGEXP '(?-u)^caf\xE9$'";
    let s = select(&db, &recipe(sql), &root).unwrap();
    assert_eq!(paths(&s), [b"d/caf\xe9".as_slice()]);
    assert!(s.size > 0);
}

#[test]
fn shipped_examples_parse() {
    use std::os::unix::fs::PermissionsExt;
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("actions");
    std::fs::create_dir(&dir).unwrap();
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/share/dume/actions.example");
    let mode =
        |p: &Path, m| std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
    mode(&dir, 0o755);
    for e in std::fs::read_dir(src).unwrap() {
        let p = e.unwrap().path();
        let to = dir.join(p.file_name().unwrap());
        std::fs::copy(&p, &to).unwrap();
        mode(
            &to,
            if p.extension().unwrap() == "sql" {
                0o644
            } else {
                0o755
            },
        );
    }
    let (rs, skipped) = dumelib::recipe::load(&dir).unwrap();
    assert!(skipped.is_empty(), "{skipped:?}");
    assert_eq!(rs.len(), 3, "{rs:?}");

    let foam = rs.iter().find(|r| r.name.starts_with("OpenFOAM")).unwrap();
    let (_t, root, db) = foam_small();
    let s = select(&db, foam, &root).unwrap();
    let got: std::collections::BTreeSet<_> = paths(&s)
        .iter()
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect();
    let want: std::collections::BTreeSet<String> =
        ["0", "100", "processor0", "constant/polyMesh", "core.123"]
            .map(String::from)
            .into();
    assert_eq!(got, want);
}

fn foam_small() -> (tempfile::TempDir, PathBuf, Db) {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("r");
    for f in [
        "0/U",
        "100/U",
        "processor0/0/U",
        "constant/polyMesh/points",
        "constant/transportProperties",
        "core.123",
        "system/controlDict",
    ] {
        put(&root.join(f), 5000);
    }
    let dbp = t.path().join("x.db");
    let uid = rustix::process::getuid().as_raw();
    scan_to_db(&root, &dbp, uid, 1, Default::default()).unwrap();
    let db = Db::open_ro(&dbp).unwrap();
    (t, root, db)
}
