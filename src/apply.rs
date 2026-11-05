//! Apply a recipe to the ticked hits: delete them, or run the recipe's script once per path.
//!
//! Every path is checked again just before it is touched: it must still be strictly inside
//! the directory the person is in, reached through real directories only, on the same
//! filesystem, of the kind the scan saw, and (files) unchanged since the scan. A failing
//! check skips the path.
//!
//! Deletion never resolves a path string after the check. `here` is opened once, each parent
//! is opened with `openat(.., O_NOFOLLOW)` from the one above, and the leaf is stat'ed,
//! unlinked or emptied relative to its parent's fd; a directory is emptied entry by entry
//! through fds the same way, never entering another device. So swapping a parent for a
//! symlink after the check cannot redirect the delete: the fds still name the checked
//! directories. What remains: a concurrent writer can add files while a directory is being
//! emptied; the final `rmdir` then fails and the path is reported `Failed`. The leaf itself
//! swapped for a symlink after the check is unlinked as a link, never followed.
//!
//! Scripts get the path as a string, so a script (being a separate program) is subject to
//! the usual check-then-use race on its own path resolution.

use crate::layout::check_private;
use crate::recipe::{Apply, Hit, Recipe};
use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;
use std::ffi::{OsStr, OsString};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
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
            match check(root, here, hit) {
                Err(why) => Verdict::Skipped(why),
                Ok(c) => match &recipe.apply {
                    Apply::Delete => delete(&c, &|_, _| {}),
                    Apply::Script(s) => script(s, here, &c.target, bin),
                },
            }
        };
        each(i, hit, &v);
        out.push(v);
    }
    out
}

/// A hit that passed the checks: its parent directory held open, and the leaf name in it.
#[doc(hidden)]
pub struct Checked {
    parent: OwnedFd,
    leaf: OsString,
    target: PathBuf,
    is_dir: bool,
    dev: u64,
    /// The uid whose files may be deleted (the process's own).
    me: u32,
}

const DIR: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

fn why(what: &Path, e: Errno) -> String {
    match e {
        Errno::NOENT => "no longer exists".into(),
        Errno::LOOP => format!("{} is now a symlink", what.display()),
        Errno::NOTDIR => format!("{} is no longer a directory", what.display()),
        e => format!("{}: {}", what.display(), std::io::Error::from(e)),
    }
}

/// `openat` a real directory; a symlink (which Linux reports as ENOTDIR with
/// `O_DIRECTORY|O_NOFOLLOW`) is reported as ELOOP.
fn open_dir(fd: &OwnedFd, name: &OsStr) -> Result<OwnedFd, Errno> {
    rustix::fs::openat(fd, name, DIR, Mode::empty()).map_err(|e| {
        let link = rustix::fs::statat(fd, name, AtFlags::SYMLINK_NOFOLLOW)
            .is_ok_and(|st| FileType::from_raw_mode(st.st_mode) == FileType::Symlink);
        if link { Errno::LOOP } else { e }
    })
}

/// Exposed for tests: the checks of [`run`], keeping the fds for [`delete`].
#[doc(hidden)]
pub fn check(root: &Path, here: &Path, hit: &Hit) -> Result<Checked, String> {
    check_as(root, here, hit, rustix::process::getuid().as_raw())
}

/// [`check`] as if the process were uid `me` (tests can't chown).
#[doc(hidden)]
pub fn check_as(root: &Path, here: &Path, hit: &Hit, me: u32) -> Result<Checked, String> {
    let rel = Path::new(OsStr::from_bytes(&hit.path));
    // `join` would replace `root` with an absolute path, and `..` could climb out.
    if hit.path.is_empty() || !rel.components().all(|c| matches!(c, Component::Normal(_))) {
        return Err("not a plain path below the scan root".into());
    }
    let target = root.join(rel);
    let inner: Vec<&OsStr> = match target.strip_prefix(here) {
        Ok(r) => r.components().map(|c| c.as_os_str()).collect(),
        Err(_) => return Err("outside the current directory".into()),
    };
    let Some((leaf, parents)) = inner.split_last() else {
        return Err("is the current directory itself".into());
    };
    let mut fd = rustix::fs::open(here, DIR, Mode::empty()).map_err(|e| why(here, e))?;
    let dev = rustix::fs::fstat(&fd).map_err(|e| why(here, e))?.st_dev as u64;
    let mut dir = here.to_path_buf();
    for c in parents {
        dir.push(c);
        fd = open_dir(&fd, c).map_err(|e| why(&dir, e))?;
        if rustix::fs::fstat(&fd).map_err(|e| why(&dir, e))?.st_dev as u64 != dev {
            return Err("is on another filesystem".into());
        }
    }
    let st =
        rustix::fs::statat(&fd, *leaf, AtFlags::SYMLINK_NOFOLLOW).map_err(|e| why(&target, e))?;
    match (FileType::from_raw_mode(st.st_mode), hit.is_dir) {
        (FileType::Symlink, _) => return Err("is now a symlink".into()),
        (FileType::Directory, false) => return Err("is now a directory".into()),
        (FileType::Directory, true) => {}
        (_, true) => return Err("is now a file".into()),
        _ => {}
    }
    if st.st_dev as u64 != dev {
        return Err("is on another filesystem".into());
    }
    if st.st_uid != me {
        return Err("is owned by someone else".into());
    }
    // Directories: the rescan before cleaning is the freshness guarantee.
    if !hit.is_dir && hit.mtime.is_some_and(|m| st.st_mtime as u64 != m) {
        return Err("changed since the scan".into());
    }
    Ok(Checked {
        parent: fd,
        leaf: leaf.to_os_string(),
        target,
        is_dir: hit.is_dir,
        dev,
        me,
    })
}

/// Exposed for tests: delete a checked hit. `fix(name, stat)` may alter an entry's stat
/// (a no-op in [`run`]; tests set `st_dev`/`st_uid` because they can't mount or chown).
#[doc(hidden)]
pub fn delete(c: &Checked, fix: &dyn Fn(&OsStr, &mut Stat)) -> Verdict {
    let fail = |e: Errno| {
        Verdict::Failed(match e {
            Errno::NAMETOOLONG => "folder is nested too deeply".into(),
            e => std::io::Error::from(e).to_string(),
        })
    };
    if !c.is_dir {
        return match rustix::fs::unlinkat(&c.parent, &c.leaf, AtFlags::empty()) {
            Ok(()) => Verdict::Done,
            Err(e) => fail(e),
        };
    }
    let mut kept = Kept::default();
    let r = rustix::fs::openat(&c.parent, &c.leaf, DIR, Mode::empty())
        .and_then(|fd| empty(&fd, c, &PathBuf::new(), &mut kept, fix, 0));
    if let Err(e) = r {
        return fail(e);
    }
    if !kept.mounts.is_empty() {
        let names: Vec<_> = kept
            .mounts
            .iter()
            .map(|m| m.display().to_string())
            .collect();
        return Verdict::Failed(format!("contains a mount point: {}", names.join(", ")));
    }
    if let Some(first) = kept.theirs.first() {
        return Verdict::Failed(format!(
            "contains files owned by other people: {}",
            first.display()
        ));
    }
    match rustix::fs::unlinkat(&c.parent, &c.leaf, AtFlags::REMOVEDIR) {
        Ok(()) => Verdict::Done,
        Err(e) => fail(e),
    }
}

/// What [`empty`] left in place, relative to the deleted directory.
#[derive(Default)]
struct Kept {
    /// Subdirectories on another device.
    mounts: Vec<PathBuf>,
    /// Entries owned by another uid (the scan never showed them to the user).
    theirs: Vec<PathBuf>,
}

/// Deeper than this, [`empty`] gives up with `NAMETOOLONG` (one open fd per level).
const MAX_DEPTH: usize = 256;

/// Remove everything in the directory `fd` that is on `c.dev` and owned by `c.me`; the rest
/// is left alone, recorded in `kept`, and its parent directories are kept too.
fn empty(
    fd: &OwnedFd,
    c: &Checked,
    at: &Path,
    kept: &mut Kept,
    fix: &dyn Fn(&OsStr, &mut Stat),
    depth: usize,
) -> Result<(), Errno> {
    if depth >= MAX_DEPTH {
        return Err(Errno::NAMETOOLONG);
    }
    // Names first: deleting while reading a directory can skip entries on NFS.
    let mut names = Vec::new();
    for ent in Dir::read_from(fd)? {
        let n = ent?.file_name().to_bytes().to_vec();
        if n != b"." && n != b".." {
            names.push(OsString::from_vec(n));
        }
    }
    for name in names {
        let mut st: Stat = rustix::fs::statat(fd, &name, AtFlags::SYMLINK_NOFOLLOW)?;
        fix(&name, &mut st);
        let path = at.join(&name);
        if st.st_uid != c.me {
            kept.theirs.push(path);
            continue;
        }
        if FileType::from_raw_mode(st.st_mode) != FileType::Directory {
            rustix::fs::unlinkat(fd, &name, AtFlags::empty())?;
            continue;
        }
        if st.st_dev as u64 != c.dev {
            kept.mounts.push(path);
            continue;
        }
        let before = kept.mounts.len() + kept.theirs.len();
        let sub = rustix::fs::openat(fd, &name, DIR, Mode::empty())?;
        empty(&sub, c, &path, kept, fix, depth + 1)?;
        if kept.mounts.len() + kept.theirs.len() == before {
            rustix::fs::unlinkat(fd, &name, AtFlags::REMOVEDIR)?;
        }
    }
    Ok(())
}

/// Run `s` with `target` as `$1` in `here`; `bin` (shared tools) goes first on `PATH`.
fn script(s: &Path, here: &Path, target: &Path, bin: Option<&Path>) -> Verdict {
    // Checked again here: the file may have changed since the recipes were loaded.
    if let Err(e) = check_private(s) {
        return Verdict::Failed(e);
    }
    let old = std::env::var_os("PATH").unwrap_or_default();
    // An empty entry would mean the working directory, i.e. `here`.
    let paths = bin
        .map(Path::to_path_buf)
        .into_iter()
        .chain(std::env::split_paths(&old))
        .filter(|p| !p.as_os_str().is_empty());
    let Ok(path) = std::env::join_paths(paths) else {
        return Verdict::Failed("cannot build PATH".into());
    };
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
