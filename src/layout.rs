//! Where the shared installation lives: `PREFIX/bin/dume` finds `PREFIX/share/dume/`.
//!
//! The prefix comes from the executable's real path (symlinks resolved), or `DUME_COMMON`.
//! Nothing is searched for in the cwd, `$HOME` or `$PATH`.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub prefix: PathBuf,
    /// `PREFIX/bin`: dume itself plus shared tools (fd, rg, ...); put on PATH for actions.
    pub bin: PathBuf,
    /// `PREFIX/share/dume`: `config.toml` and `actions/`.
    pub share: PathBuf,
}

/// Work out the layout from the executable path and the `DUME_COMMON` override.
pub fn resolve(exe: io::Result<PathBuf>, common: Option<PathBuf>) -> Result<Layout, String> {
    let prefix = match common {
        Some(p) => p
            .canonicalize()
            .map_err(|e| format!("DUME_COMMON={}: {e}", p.display()))?,
        None => {
            let exe =
                exe.map_err(|e| format!("cannot find my own path ({e}); set DUME_COMMON=PREFIX"))?;
            if exe.to_string_lossy().ends_with(" (deleted)") {
                return Err("the dume binary was replaced while running; start it again".into());
            }
            let bin = exe
                .parent()
                .filter(|b| b.file_name().is_some_and(|n| n == "bin"));
            bin.and_then(Path::parent).map(Path::to_path_buf).ok_or_else(|| {
                format!(
                    "{} is not installed as PREFIX/bin/dume; install it there or set DUME_COMMON=PREFIX",
                    exe.display()
                )
            })?
        }
    };
    let share = prefix.join("share/dume");
    if !share.is_dir() {
        return Err(format!(
            "{} not found (expected PREFIX/share/dume with config.toml and actions/ next to PREFIX/bin)",
            share.display()
        ));
    }
    Ok(Layout {
        bin: prefix.join("bin"),
        share,
        prefix,
    })
}

pub fn locate() -> Result<Layout, String> {
    resolve(
        std::env::current_exe(),
        std::env::var_os("DUME_COMMON").map(PathBuf::from),
    )
}

/// Refuse anything that group or others could rewrite: shared config and scripts run as each user.
pub fn check_private(path: &Path) -> Result<(), String> {
    let mode = std::fs::metadata(path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .permissions()
        .mode();
    if mode & 0o022 != 0 {
        return Err(format!(
            "{}: writable by group or others (mode {:o}); refusing to use it",
            path.display(),
            mode & 0o7777
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    fn prefix() -> (tempfile::TempDir, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().canonicalize().unwrap();
        fs::create_dir_all(p.join("bin")).unwrap();
        fs::create_dir_all(p.join("share/dume")).unwrap();
        (t, p)
    }

    #[test]
    fn from_exe_path() {
        let (_t, p) = prefix();
        let l = resolve(Ok(p.join("bin/dume")), None).unwrap();
        assert_eq!(l.prefix, p);
        assert_eq!(l.bin, p.join("bin"));
        assert_eq!(l.share, p.join("share/dume"));
    }

    #[test]
    fn env_override_wins_and_is_canonical() {
        let (_t, p) = prefix();
        let link = p.join("alias");
        symlink(&p, &link).unwrap();
        let l = resolve(Ok("/nowhere/dume".into()), Some(link)).unwrap();
        assert_eq!(l.prefix, p);
    }

    #[test]
    fn errors_are_specific() {
        let (_t, p) = prefix();
        let e = resolve(Ok(p.join("dume")), None).unwrap_err();
        assert!(
            e.contains("PREFIX/bin/dume") && e.contains("DUME_COMMON"),
            "{e}"
        );
        let e = resolve(Err(io::Error::other("no procfs")), None).unwrap_err();
        assert!(e.contains("DUME_COMMON"), "{e}");
        let e = resolve(
            Ok(format!("{}/bin/dume (deleted)", p.display()).into()),
            None,
        )
        .unwrap_err();
        assert!(e.contains("replaced"), "{e}");
        fs::remove_dir(p.join("share/dume")).unwrap();
        let e = resolve(Ok(p.join("bin/dume")), None).unwrap_err();
        assert!(e.contains("share/dume"), "{e}");
        assert!(resolve(Ok("x".into()), Some(p.join("missing"))).is_err());
    }

    #[test]
    fn private_check() {
        let (_t, p) = prefix();
        let f = p.join("c");
        fs::write(&f, "x").unwrap();
        for (mode, ok) in [(0o644, true), (0o600, true), (0o664, false), (0o646, false)] {
            fs::set_permissions(&f, fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(check_private(&f).is_ok(), ok, "{mode:o}");
        }
        assert!(check_private(&p.join("nope")).is_err());
    }
}
