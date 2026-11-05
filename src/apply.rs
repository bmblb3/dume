//! Apply a recipe to the ticked hits: delete them, or run the recipe's script once per path.
//!
//! Every path is checked again just before it is touched: it must still be strictly inside
//! the directory the person is in, reached through real directories only, of the kind the
//! scan saw, and (files) unchanged since the scan. A failing check skips the path.
//!
//! There is an unavoidable window between these checks and the delete or script: another
//! process could swap a component for a symlink in between. std's `remove_dir_all` does not
//! follow a symlink at the top or below it, and both act on the very `PathBuf` that was just
//! checked (no re-resolving), so the window is only that race, not a stale path.

use crate::layout::check_private;
use crate::recipe::{Apply, Hit, Recipe};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    Done,
    Skipped(String),
    Failed(String),
}

/// One verdict per hit, in order. `root` is the canonical scan root, `here` the canonical
/// directory the person is in (inside `root`). `cancel` is checked before each hit, so a
/// running script finishes but nothing after it starts. `each` is called after every hit.
pub fn run(
    recipe: &Recipe,
    root: &Path,
    here: &Path,
    hits: &[Hit],
    bin: Option<&Path>,
    cancel: &AtomicBool,
    each: &mut dyn FnMut(usize, &Hit, &Verdict),
) -> Vec<Verdict> {
    let mut out = Vec::with_capacity(hits.len());
    for (i, hit) in hits.iter().enumerate() {
        let v = if cancel.load(Ordering::Relaxed) {
            Verdict::Skipped("cancelled".into())
        } else {
            match guard(root, here, hit) {
                Err(why) => Verdict::Skipped(why),
                Ok(target) => match &recipe.apply {
                    Apply::Delete => delete(&target, hit.is_dir),
                    Apply::Script(s) => script(s, here, &target, bin),
                },
            }
        };
        each(i, hit, &v);
        out.push(v);
    }
    out
}

/// The absolute path of `hit` if it is still safe to act on.
fn guard(root: &Path, here: &Path, hit: &Hit) -> Result<PathBuf, String> {
    let rel = Path::new(OsStr::from_bytes(&hit.path));
    // `join` would replace `root` with an absolute path, and `..` could climb out.
    if hit.path.is_empty() || !rel.components().all(|c| matches!(c, Component::Normal(_))) {
        return Err("not a plain path below the scan root".into());
    }
    let target = root.join(rel);
    let inner: Vec<_> = match target.strip_prefix(here) {
        Ok(r) => r.components().collect(),
        Err(_) => return Err("outside the current directory".into()),
    };
    let Some((_, parents)) = inner.split_last() else {
        return Err("is the current directory itself".into());
    };
    let mut dir = here.to_path_buf();
    for c in std::iter::once(None).chain(parents.iter().map(Some)) {
        if let Some(c) = c {
            dir.push(c);
        }
        match std::fs::symlink_metadata(&dir) {
            Ok(md) if md.file_type().is_dir() => {}
            Ok(md) if md.file_type().is_symlink() => {
                return Err(format!("{} is now a symlink", dir.display()));
            }
            Ok(_) => return Err(format!("{} is no longer a directory", dir.display())),
            Err(_) => return Err("no longer exists".into()),
        }
    }
    let md = std::fs::symlink_metadata(&target).map_err(|_| "no longer exists".to_string())?;
    let ft = md.file_type();
    if ft.is_symlink() {
        return Err("is now a symlink".into());
    }
    match (hit.is_dir, ft.is_dir()) {
        (true, false) => return Err("is now a file".into()),
        (false, true) => return Err("is now a directory".into()),
        _ => {}
    }
    // Directories: the rescan before cleaning is the freshness guarantee.
    if !hit.is_dir && hit.mtime.is_some_and(|m| md.mtime() as u64 != m) {
        return Err("changed since the scan".into());
    }
    Ok(target)
}

fn delete(target: &Path, is_dir: bool) -> Verdict {
    let r = if is_dir {
        std::fs::remove_dir_all(target)
    } else {
        std::fs::remove_file(target)
    };
    match r {
        Ok(()) => Verdict::Done,
        Err(e) => Verdict::Failed(e.to_string()),
    }
}

/// Run `s` with `target` as `$1` in `here`; `bin` (shared tools) goes first on `PATH`.
fn script(s: &Path, here: &Path, target: &Path, bin: Option<&Path>) -> Verdict {
    // Checked again here: the file may have changed since the recipes were loaded.
    if let Err(e) = check_private(s) {
        return Verdict::Failed(e);
    }
    let mut paths: Vec<PathBuf> = bin.map(Path::to_path_buf).into_iter().collect();
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let path: OsString = std::env::join_paths(paths).unwrap_or_default();
    let out = Command::new(s)
        .arg(target)
        .current_dir(here)
        .env("PATH", path)
        .env("DUME_HERE", here)
        .env("DUME_PATH", target)
        .stdin(Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => Verdict::Done,
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            let last = err.lines().map(str::trim).rfind(|l| !l.is_empty());
            Verdict::Failed(last.map_or_else(|| o.status.to_string(), str::to_string))
        }
        Err(e) => Verdict::Failed(format!("cannot run {}: {e}", s.display())),
    }
}
