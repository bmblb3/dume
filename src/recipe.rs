//! SQL cleaning recipes: `*.sql` files in the actions directory.
//!
//! ```sql
//! -- dume-name: Clean OpenFOAM case
//! -- dume-apply: delete | clean.sh
//! SELECT path FROM entries WHERE ...
//! ```

use crate::db::Db;
use crate::layout::check_private;
use regex::bytes::Regex;
use rusqlite::functions::FunctionFlags;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub enum Apply {
    Delete,
    Script(PathBuf),
}

#[derive(Clone, Debug)]
pub struct Recipe {
    pub name: String,
    pub apply: Apply,
    pub sql: String,
    pub file: PathBuf,
}

/// Recipes in `dir` sorted by name, plus a message for each `.sql` file skipped. A missing
/// directory means no recipes; one that others can write to is refused, and so are files
/// (and scripts) they can write to.
pub fn load(dir: &Path) -> io::Result<(Vec<Recipe>, Vec<String>)> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), Vec::new())),
        Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", dir.display()))),
    };
    check_private(dir).map_err(io::Error::other)?;
    let (mut out, mut skipped) = (Vec::new(), Vec::new());
    for ent in rd {
        let path = ent?.path();
        let fname = path.file_name().unwrap_or_default().to_string_lossy();
        if path.extension().is_none_or(|e| e != "sql") || fname.starts_with('.') {
            continue;
        }
        match parse(dir, &path) {
            Ok(r) => out.push(r),
            Err(e) => skipped.push(format!("{fname}: {e}")),
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok((out, skipped))
}

fn parse(dir: &Path, path: &Path) -> Result<Recipe, String> {
    regular(path)?;
    check_private(path)?;
    let sql = std::fs::read_to_string(path).map_err(|e| format!("unreadable: {e}"))?;
    let (mut name, mut apply) = (None, Apply::Delete);
    let mut in_header = true;
    for line in sql.lines() {
        let t = line.trim();
        let c = t.strip_prefix("--").map(str::trim);
        if in_header && c.is_none() && !t.is_empty() {
            in_header = false;
        }
        let Some(c) = c else { continue };
        if !in_header {
            if c.starts_with("dume-name:") || c.starts_with("dume-apply:") {
                return Err("dume header after the query starts; headers must come first".into());
            }
            continue;
        }
        if let Some(v) = c.strip_prefix("dume-name:") {
            name = Some(v.trim().to_string()).filter(|n| !n.is_empty());
        } else if let Some(v) = c.strip_prefix("dume-apply:") {
            apply = match v.trim() {
                "delete" => Apply::Delete,
                s => Apply::Script(script(dir, s)?),
            };
        }
    }
    let name = name.ok_or("missing `-- dume-name:` header (headers must come before the query)")?;
    Ok(Recipe {
        name,
        apply,
        sql,
        file: path.to_path_buf(),
    })
}

/// A regular file, not a symlink (a link could point at a file outside this directory).
fn regular(path: &Path) -> Result<(), String> {
    let ft = std::fs::symlink_metadata(path)
        .map_err(|e| e.to_string())?
        .file_type();
    if ft.is_symlink() {
        return Err("is a symlink".into());
    }
    if !ft.is_file() {
        return Err("not a regular file".into());
    }
    Ok(())
}

fn script(dir: &Path, name: &str) -> Result<PathBuf, String> {
    if name.is_empty() || name == ".." || name.contains('/') {
        return Err(format!(
            "dume-apply `{name}`: not a plain file name in this directory"
        ));
    }
    let p = dir.join(name);
    regular(&p).map_err(|e| format!("script `{name}`: {e}"))?;
    let md = std::fs::metadata(&p).map_err(|e| format!("script `{name}`: {e}"))?;
    if md.permissions().mode() & 0o111 == 0 {
        return Err(format!("script `{name}`: not an executable file"));
    }
    check_private(&p)?;
    Ok(p)
}

/// A selected path (relative to the scan root) with its size: `tree_size`, so
/// directories count their subtree and repeat hardlinks count 0.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub path: Vec<u8>,
    pub size: u64,
    pub is_dir: bool,
    pub mtime: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct Selection {
    pub hits: Vec<Hit>,
    pub count: u64,
    pub size: u64,
}

/// Run `r` on a fresh read-only connection that may only SELECT from `entries`,
/// a temp view of the rows strictly below `here`. Returned paths are mapped back
/// to real rows under `here`, nested hits dropped, sorted by path.
pub fn select(db: &Db, r: &Recipe, here: &Path) -> Result<Selection, String> {
    let rel = db.rel(here).map_err(|e| e.to_string())?;
    let mut conn = Connection::open_with_flags(db.path(), OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| e.to_string())?;
    sandbox(&mut conn, &rel).map_err(|e| e.to_string())?;
    let mut paths = run(&conn, &r.sql).map_err(|e| e.to_string())?;
    paths.retain(|p| under(p, &rel));
    paths.sort_unstable();
    paths.dedup();

    let mut st = conn
        .prepare("SELECT is_dir, tree_size, mtime FROM main.entries WHERE path=?")
        .map_err(|e| e.to_string())?;
    let mut dirs = HashSet::new();
    let mut sel = Selection::default();
    // Sorted, so a directory comes before everything inside it.
    for path in paths {
        let inside = path
            .iter()
            .enumerate()
            .any(|(i, &b)| b == b'/' && dirs.contains(&path[..i]));
        if inside {
            continue;
        }
        let row = st.query_row([&path], |row| {
            Ok((
                row.get::<_, bool>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<i64>>(2)?,
            ))
        });
        let (is_dir, size, mtime) = match row {
            Ok(v) => v,
            Err(rusqlite::Error::QueryReturnedNoRows) => continue,
            Err(e) => return Err(e.to_string()),
        };
        if is_dir {
            dirs.insert(path.clone());
        }
        sel.size += size as u64;
        sel.hits.push(Hit {
            path,
            size: size as u64,
            is_dir,
            mtime: mtime.map(|m| m as u64),
        });
    }
    sel.count = sel.hits.len() as u64;
    Ok(sel)
}

/// Strictly below `rel` (the root is the empty path).
fn under(p: &[u8], rel: &[u8]) -> bool {
    if rel.is_empty() {
        !p.is_empty()
    } else {
        p.len() > rel.len() && p.starts_with(rel) && p[rel.len()] == b'/'
    }
}

fn hex(b: &[u8]) -> String {
    let mut s = String::from("x'");
    for c in b {
        let _ = write!(s, "{c:02x}");
    }
    s + "'"
}

/// REGEXP, the scoped `entries` view, then an authorizer allowing only reads.
fn sandbox(conn: &mut Connection, rel: &[u8]) -> rusqlite::Result<()> {
    let cache = RefCell::new(HashMap::<String, Regex>::new());
    conn.create_scalar_function(
        "regexp",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        move |ctx| {
            let bad = |m: &str| rusqlite::Error::UserFunctionError(m.into());
            let pat = match ctx.get_raw(0) {
                ValueRef::Null => return Ok(None),
                ValueRef::Text(t) => {
                    std::str::from_utf8(t).map_err(|_| bad("REGEXP pattern is not UTF-8"))?
                }
                _ => return Err(bad("REGEXP pattern must be text")),
            };
            let v = match ctx.get_raw(1) {
                ValueRef::Null => return Ok(None),
                ValueRef::Text(b) | ValueRef::Blob(b) => b,
                _ => return Err(bad("REGEXP value must be text or blob")),
            };
            let mut c = cache.borrow_mut();
            if !c.contains_key(pat) {
                let re = Regex::new(pat).map_err(|e| bad(&format!("REGEXP: {e}")))?;
                c.insert(pat.to_string(), re);
            }
            Ok(Some(c[pat].is_match(v)))
        },
    )?;
    let range = if rel.is_empty() {
        "path > x''".to_string()
    } else {
        let (mut lo, mut hi) = (rel.to_vec(), rel.to_vec());
        lo.push(b'/');
        hi.push(b'/' + 1);
        format!("path > {} AND path < {}", hex(&lo), hex(&hi))
    };
    conn.execute_batch(&format!(
        "CREATE TEMP VIEW entries AS SELECT * FROM main.entries WHERE {range}"
    ))?;
    conn.authorizer(Some(|ctx: AuthContext<'_>| match ctx.action {
        AuthAction::Select | AuthAction::Recursive => Authorization::Allow,
        AuthAction::Read {
            table_name: "entries",
            ..
        } => Authorization::Allow,
        AuthAction::Function { function_name }
            if !function_name.eq_ignore_ascii_case("load_extension") =>
        {
            Authorization::Allow
        }
        _ => Authorization::Deny,
    }))
}

/// The `path` values of the recipe's single read-only statement.
fn run(conn: &Connection, sql: &str) -> rusqlite::Result<Vec<Vec<u8>>> {
    let mut st = conn.prepare(sql)?;
    let no_path = || rusqlite::Error::InvalidColumnName("recipe must return a path column".into());
    if !st.readonly() {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let i = st.column_index("path").map_err(|_| no_path())?;
    let mut rows = st.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        if let ValueRef::Text(b) | ValueRef::Blob(b) = row.get_ref(i)? {
            out.push(b.to_vec());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn put(dir: &Path, name: &str, text: &str, mode: u32) {
        let p = dir.join(name);
        fs::write(&p, text).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn dir() -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        fs::set_permissions(t.path(), fs::Permissions::from_mode(0o755)).unwrap();
        t
    }

    #[test]
    fn parses_headers_and_sql_body() {
        let t = dir();
        put(t.path(), "b.sql", "-- dume-name: Bee\nSELECT 1;\n", 0o644);
        put(
            t.path(),
            "a.sql",
            "-- dume-name: Ay\n-- dume-apply: run.sh\nSELECT 2;\n",
            0o644,
        );
        put(t.path(), "run.sh", "#!/bin/sh\n", 0o755);
        put(t.path(), "notes.txt", "ignored", 0o644);
        let (r, skipped) = load(t.path()).unwrap();
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].name, "Ay");
        assert_eq!(r[0].apply, Apply::Script(t.path().join("run.sh")));
        assert!(r[0].sql.contains("SELECT 2;"));
        assert_eq!(r[1].name, "Bee");
        assert_eq!(r[1].apply, Apply::Delete);
        assert_eq!(r[1].file, t.path().join("b.sql"));
    }

    #[test]
    fn skips_missing_name_and_unknown_apply() {
        let t = dir();
        put(t.path(), "noname.sql", "SELECT 1;", 0o644);
        put(
            t.path(),
            "bad.sql",
            "-- dume-name: X\n-- dume-apply: frobnicate\nSELECT 1;",
            0o644,
        );
        fs::write(t.path().join("bin.sql"), [0xff, 0xfe]).unwrap();
        let (r, skipped) = load(t.path()).unwrap();
        assert!(r.is_empty());
        assert_eq!(skipped.len(), 3, "{skipped:?}");
        assert!(skipped.iter().any(|s| s.contains("noname.sql")));
        assert!(skipped.iter().any(|s| s.contains("bad.sql")));
        assert!(skipped.iter().any(|s| s.contains("bin.sql")));
    }

    #[test]
    fn refuses_group_writable_dir_and_file() {
        let t = dir();
        put(t.path(), "ok.sql", "-- dume-name: Ok\nSELECT 1;", 0o644);
        put(t.path(), "w.sql", "-- dume-name: W\nSELECT 1;", 0o664);
        let (r, skipped) = load(t.path()).unwrap();
        assert_eq!(r.len(), 1);
        assert!(skipped.iter().any(|s| s.contains("w.sql")));
        fs::set_permissions(t.path(), fs::Permissions::from_mode(0o775)).unwrap();
        assert!(load(t.path()).is_err());
    }

    #[test]
    fn script_must_exist_and_be_executable_and_inside_dir() {
        let t = dir();
        for (f, s) in [
            ("missing", "nope.sh"),
            ("noexec", "plain.sh"),
            ("escape", "../x"),
            ("sub", "sub/x.sh"),
        ] {
            put(
                t.path(),
                &format!("{f}.sql"),
                &format!("-- dume-name: {f}\n-- dume-apply: {s}\nSELECT 1;"),
                0o644,
            );
        }
        put(t.path(), "plain.sh", "#!/bin/sh\n", 0o644);
        put(
            t.path(),
            "gw.sql",
            "-- dume-name: gw\n-- dume-apply: gw.sh\nSELECT 1;",
            0o644,
        );
        put(t.path(), "gw.sh", "#!/bin/sh\n", 0o775);
        let (r, skipped) = load(t.path()).unwrap();
        assert!(r.is_empty(), "{r:?}");
        assert_eq!(skipped.len(), 5, "{skipped:?}");
    }

    #[test]
    fn long_license_comment_before_header_still_parses() {
        let t = dir();
        let lic = "-- license line\n".repeat(40);
        put(
            t.path(),
            "l.sql",
            &format!("{lic}-- dume-name: L\n-- dume-apply: run.sh\nSELECT 1;"),
            0o644,
        );
        put(t.path(), "run.sh", "#!/bin/sh\n", 0o755);
        let (r, skipped) = load(t.path()).unwrap();
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(r[0].apply, Apply::Script(t.path().join("run.sh")));
    }

    #[test]
    fn late_apply_header_after_sql_is_skipped_not_defaulted_to_delete() {
        let t = dir();
        put(
            t.path(),
            "late.sql",
            "-- dume-name: Late\nSELECT 1;\n-- dume-apply: run.sh\n",
            0o644,
        );
        let (r, skipped) = load(t.path()).unwrap();
        assert!(r.is_empty(), "{r:?}");
        assert!(skipped[0].contains("after the query starts"), "{skipped:?}");
    }

    #[test]
    fn symlinked_recipe_and_symlinked_script_are_skipped() {
        use std::os::unix::fs::symlink;
        let t = dir();
        let other = dir();
        put(other.path(), "r.sql", "-- dume-name: R\nSELECT 1;", 0o644);
        put(other.path(), "s.sh", "#!/bin/sh\n", 0o755);
        symlink(other.path().join("r.sql"), t.path().join("link.sql")).unwrap();
        symlink(other.path().join("s.sh"), t.path().join("s.sh")).unwrap();
        put(
            t.path(),
            "x.sql",
            "-- dume-name: X\n-- dume-apply: s.sh\nSELECT 1;",
            0o644,
        );
        let (r, skipped) = load(t.path()).unwrap();
        assert!(r.is_empty(), "{r:?}");
        assert_eq!(skipped.len(), 2, "{skipped:?}");
        assert!(
            skipped.iter().all(|s| s.contains("is a symlink")),
            "{skipped:?}"
        );
    }
}
