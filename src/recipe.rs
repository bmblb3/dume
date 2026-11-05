//! SQL cleaning recipes: `*.sql` files in the actions directory.
//!
//! ```sql
//! -- dume-name: Clean OpenFOAM case
//! -- dume-apply: delete | clean.sh
//! SELECT path FROM entries WHERE ...
//! ```

use crate::layout::check_private;
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
