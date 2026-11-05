//! Cleaning actions: executable scripts kept outside this repo, run once per matched directory.
//!
//! A script declares what it is for in leading comments:
//!
//! ```sh
//! #!/bin/sh
//! # dume-name: Clean OpenFOAM case
//! # dume-pattern: case
//! fd '^[0-9]*$|^polyMesh$' | xargs rm -rf
//! ```
//!
//! It is run with the matched directory as its working directory and as `$1`
//! (also `$DUME_DIR`, `$DUME_PATTERN`). `dume-pattern` may list several patterns,
//! comma-separated, or `*` for any.

use crate::layout::check_private;
use std::ffi::OsString;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Debug)]
pub struct Action {
    pub name: String,
    pub patterns: Vec<String>,
    pub path: PathBuf,
}

impl Action {
    pub fn applies_to(&self, pattern: &str) -> bool {
        self.patterns.iter().any(|p| p == "*" || p == pattern)
    }
}

/// `(name, patterns)` from the leading comment block of a script.
pub fn parse_header(text: &str) -> (Option<String>, Vec<String>) {
    let (mut name, mut patterns) = (None, Vec::new());
    for line in text.lines().take(30) {
        let Some(c) = line.trim().strip_prefix('#') else {
            if line.trim().is_empty() {
                continue;
            }
            break;
        };
        let c = c.trim();
        if let Some(v) = c.strip_prefix("dume-name:") {
            name = Some(v.trim().to_string()).filter(|n| !n.is_empty());
        } else if let Some(v) = c.strip_prefix("dume-pattern:") {
            patterns.extend(
                v.split(',')
                    .map(|p| p.trim().to_string())
                    .filter(|p| !p.is_empty()),
            );
        }
    }
    (name, patterns)
}

/// Executable regular files in `dir`, sorted by name, plus a warning for each one skipped.
/// A missing directory means no actions; one that others can write to is refused, and so are
/// scripts they can write to (these run as every user).
pub fn load(dir: &Path) -> io::Result<(Vec<Action>, Vec<String>)> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), Vec::new())),
        Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", dir.display()))),
    };
    check_private(dir).map_err(io::Error::other)?;
    let (mut out, mut skipped) = (Vec::new(), Vec::new());
    for ent in rd {
        let path = ent?.path();
        let Ok(md) = std::fs::metadata(&path) else {
            continue;
        };
        let hidden = path
            .file_name()
            .is_some_and(|n| n.as_encoded_bytes().starts_with(b"."));
        if !md.is_file() || md.permissions().mode() & 0o111 == 0 || hidden {
            continue;
        }
        if let Err(e) = check_private(&path) {
            skipped.push(e);
            continue;
        }
        let head: Vec<u8> = std::fs::read(&path)
            .unwrap_or_default()
            .into_iter()
            .take(4096)
            .collect();
        let (name, patterns) = parse_header(&String::from_utf8_lossy(&head));
        let stem = path
            .file_stem()
            .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
        out.push(Action {
            name: name.unwrap_or(stem),
            patterns,
            path,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok((out, skipped))
}

pub struct Outcome {
    pub dir: PathBuf,
    pub ok: bool,
    pub msg: String,
}

/// Run `action` in `dir`. `root` is the canonical report root; `dir` must be a real directory
/// below it with no symlink on the way, so a swapped-in symlink can't redirect a deletion.
/// `bin` (shared tools like fd) goes first on the script's `PATH`.
pub fn run_on(
    action: &Action,
    pattern: &str,
    root: &Path,
    dir: &Path,
    bin: Option<&Path>,
) -> Outcome {
    let fail = |msg: String| Outcome {
        dir: dir.to_path_buf(),
        ok: false,
        msg,
    };
    match dir.canonicalize() {
        Ok(c) if c == dir && c.starts_with(root) && c != root => {}
        Ok(_) => return fail("skipped: path resolves elsewhere (symlink?)".into()),
        Err(e) => return fail(format!("skipped: {e}")),
    }
    // Checked again here: the file may have changed since it was listed.
    if let Err(e) = check_private(&action.path) {
        return fail(e);
    }
    let mut paths: Vec<PathBuf> = bin.map(Path::to_path_buf).into_iter().collect();
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let path: OsString = std::env::join_paths(paths).unwrap_or_default();
    let out = Command::new(&action.path)
        .env("PATH", path)
        .arg(dir)
        .current_dir(dir)
        .env("DUME_DIR", dir)
        .env("DUME_PATTERN", pattern)
        .stdin(Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => Outcome {
            dir: dir.to_path_buf(),
            ok: true,
            msg: String::new(),
        },
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            let last = err.lines().last().unwrap_or("").trim();
            fail(format!("{}: {last}", o.status))
        }
        Err(e) => fail(format!("cannot run {}: {e}", action.path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header() {
        let (n, p) = parse_header(
            "#!/bin/sh\n# dume-name: Clean OpenFOAM case\n# dume-pattern: case, run_#\n\n# dume-pattern: x\nrm -rf a\n# dume-name: ignored\n",
        );
        assert_eq!(n.as_deref(), Some("Clean OpenFOAM case"));
        assert_eq!(p, ["case", "run_#", "x"]);
        assert_eq!(parse_header("echo hi\n# dume-name: late"), (None, vec![]));
    }

    #[test]
    fn applies() {
        let a = |p: &[&str]| Action {
            name: String::new(),
            patterns: p.iter().map(|s| s.to_string()).collect(),
            path: PathBuf::new(),
        };
        assert!(a(&["case"]).applies_to("case"));
        assert!(!a(&["case"]).applies_to("run_#"));
        assert!(a(&["*"]).applies_to("anything"));
        assert!(!a(&[]).applies_to("case"));
    }
}
