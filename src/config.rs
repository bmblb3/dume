//! Team-wide settings (`PREFIX/share/dume/config.toml`) and where each user's scan lives.

use crate::layout::{self, Layout};
use serde::Deserialize;
use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    root: PathBuf,
    threads: Option<u16>,
    stale_days: Option<u64>,
    rescan_skip_secs: Option<u64>,
}

#[derive(Debug)]
pub struct Config {
    /// The tree everyone scans (canonical).
    pub root: PathBuf,
    pub threads: usize,
    /// A scan older than this is flagged as stale.
    pub stale_days: u64,
    /// `c` skips the rescan of a folder refreshed this recently (0: always rescan).
    pub rescan_skip_secs: u64,
}

pub fn parse(text: &str) -> Result<Config, String> {
    let raw: Raw = toml::from_str(text).map_err(|e| e.to_string())?;
    if !raw.root.is_absolute() {
        return Err(format!("root = {:?} must be an absolute path", raw.root));
    }
    let threads = raw.threads.unwrap_or(32);
    if threads == 0 {
        return Err("threads must be at least 1".into());
    }
    Ok(Config {
        root: raw.root,
        threads: threads as usize,
        stale_days: raw.stale_days.unwrap_or(7),
        rescan_skip_secs: raw.rescan_skip_secs.unwrap_or(300),
    })
}

pub fn load(layout: &Layout) -> Result<Config, String> {
    let file = layout.share.join("config.toml");
    layout::check_private(&layout.share)?;
    if !file.exists() {
        return Err(format!(
            "{} not found; copy config.toml.example there and set root",
            file.display()
        ));
    }
    layout::check_private(&file)?;
    let text = std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    let mut cfg = parse(&text).map_err(|e| format!("{}: {e}", file.display()))?;
    cfg.root = cfg
        .root
        .canonicalize()
        .map_err(|e| format!("{}: root {}: {e}", file.display(), cfg.root.display()))?;
    if !cfg.root.is_dir() {
        return Err(format!(
            "{}: root {} is not a directory",
            file.display(),
            cfg.root.display()
        ));
    }
    Ok(cfg)
}

/// `$<var>`, else `$XDG_STATE_HOME/dume/<name>` (default `~/.local/state/dume/<name>`).
fn state_file(
    env: impl Fn(&str) -> Option<OsString>,
    var: &str,
    name: &str,
) -> Result<PathBuf, String> {
    if let Some(f) = env(var) {
        return Ok(f.into());
    }
    let state = env("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute()) // the XDG spec says to ignore relative values
        .or_else(|| env("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .ok_or_else(|| format!("HOME is not set; set {var} to the {name} path"))?;
    Ok(state.join("dume").join(name))
}

/// The user's export: `$DUME_JSON`, else `$XDG_STATE_HOME/dume/dume.json`
/// (default `~/.local/state/dume/dume.json`).
pub fn export_path(env: impl Fn(&str) -> Option<OsString>) -> Result<PathBuf, String> {
    state_file(env, "DUME_JSON", "dume.json")
}

/// The user's scan database: `$DUME_DB`, else `$XDG_STATE_HOME/dume/dume.db`
/// (default `~/.local/state/dume/dume.db`).
pub fn db_path(env: impl Fn(&str) -> Option<OsString>) -> Result<PathBuf, String> {
    state_file(env, "DUME_DB", "dume.db")
}

/// Where earlier versions kept the export: 0.6's [`export_path`] if it exists, else `~/dume.json`.
pub fn legacy_export(env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let home = env("HOME").map(|h| PathBuf::from(h).join("dume.json"));
    export_path(&env).ok().filter(|p| p.is_file()).or(home)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_defaults() {
        let c = parse("root = \"/x\"").unwrap();
        assert_eq!((c.root, c.threads, c.stale_days), ("/x".into(), 32, 7));
        let c = parse("root = \"/x\"\nthreads = 4\nstale_days = 1").unwrap();
        assert_eq!((c.threads, c.stale_days), (4, 1));
    }

    #[test]
    fn rejects_bad_config() {
        assert!(parse("").is_err(), "root is required");
        assert!(parse("root = \"rel/path\"").is_err());
        assert!(parse("root = \"/x\"\nrot = 1").is_err(), "typos are errors");
        assert!(parse("root = \"/x\"\nthreads = 0").is_err());
        assert!(parse("root = \"/x\"\nthreads = \"many\"").is_err());
    }

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).into())
        }
    }

    #[test]
    fn export_locations() {
        let p = |e| export_path(e).unwrap();
        assert_eq!(
            p(env(&[("HOME", "/h")])),
            PathBuf::from("/h/.local/state/dume/dume.json")
        );
        assert_eq!(
            p(env(&[("HOME", "/h"), ("XDG_STATE_HOME", "/s")])),
            PathBuf::from("/s/dume/dume.json")
        );
        assert_eq!(
            p(env(&[("HOME", "/h"), ("XDG_STATE_HOME", "rel")])),
            PathBuf::from("/h/.local/state/dume/dume.json")
        );
        assert_eq!(
            p(env(&[("HOME", "/h"), ("DUME_JSON", "/f.json")])),
            PathBuf::from("/f.json")
        );
        assert!(export_path(env(&[])).is_err());
        assert_eq!(
            legacy_export(env(&[("HOME", "/h")])),
            Some("/h/dume.json".into())
        );
    }

    #[test]
    fn legacy_export_prefers_the_0_6_location() {
        let t = tempfile::tempdir().unwrap();
        let h = t.path().to_str().unwrap();
        let old = t.path().join(".local/state/dume/dume.json");
        assert_eq!(
            legacy_export(env(&[("HOME", h)])),
            Some(t.path().join("dume.json"))
        );
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, "[]").unwrap();
        assert_eq!(legacy_export(env(&[("HOME", h)])), Some(old));
        let j = t.path().join("j.json");
        std::fs::write(&j, "[]").unwrap();
        let js = j.to_str().unwrap();
        assert_eq!(
            legacy_export(env(&[("HOME", h), ("DUME_JSON", js)])),
            Some(j)
        );
    }

    #[test]
    fn db_locations() {
        let p = |e| db_path(e).unwrap();
        assert_eq!(
            p(env(&[("HOME", "/h")])),
            PathBuf::from("/h/.local/state/dume/dume.db")
        );
        assert_eq!(
            p(env(&[("HOME", "/h"), ("XDG_STATE_HOME", "/s")])),
            PathBuf::from("/s/dume/dume.db")
        );
        assert_eq!(
            p(env(&[("HOME", "/h"), ("DUME_DB", "/f.db")])),
            PathBuf::from("/f.db")
        );
        assert!(db_path(env(&[])).unwrap_err().contains("DUME_DB"));
    }

    #[test]
    fn load_checks_permissions_and_root() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let p = t.path().canonicalize().unwrap();
        let share = p.join("share/dume");
        std::fs::create_dir_all(&share).unwrap();
        let l = Layout {
            prefix: p.clone(),
            bin: p.join("bin"),
            share: share.clone(),
        };
        assert!(load(&l).unwrap_err().contains("config.toml.example"));
        let f = share.join("config.toml");
        std::fs::write(&f, format!("root = {:?}", p.display().to_string())).unwrap();
        assert_eq!(load(&l).unwrap().root, p);
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(load(&l).unwrap_err().contains("writable"));
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::write(&f, "root = \"/definitely/not/here\"").unwrap();
        assert!(load(&l).unwrap_err().contains("root"));
    }
}
