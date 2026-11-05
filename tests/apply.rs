use dumelib::apply::{Verdict, check, check_as, delete, run};
use dumelib::recipe::{Apply, Hit, Recipe};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

fn recipe(apply: Apply) -> Recipe {
    Recipe {
        name: "t".into(),
        apply,
        sql: String::new(),
        file: PathBuf::new(),
    }
}

/// Canonical scan root `r` and the directory the user is in, `r/h`.
fn tree() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().canonicalize().unwrap().join("r");
    let here = root.join("h");
    fs::create_dir_all(&here).unwrap();
    (t, root, here)
}

fn put(p: &Path) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, "x").unwrap();
}

/// A hit as the scan would have recorded it now.
fn hit(root: &Path, rel: &str) -> Hit {
    let md = fs::symlink_metadata(root.join(rel)).unwrap();
    Hit {
        path: rel.as_bytes().to_vec(),
        size: md.len(),
        is_dir: md.is_dir(),
        mtime: Some(md.mtime() as u64),
    }
}

fn raw(path: &[u8], is_dir: bool) -> Hit {
    Hit {
        path: path.to_vec(),
        size: 0,
        is_dir,
        mtime: None,
    }
}

fn go(r: &Recipe, root: &Path, here: &Path, hits: &[Hit], bin: Option<&Path>) -> Vec<Verdict> {
    run(
        r,
        root,
        here,
        hits,
        bin,
        &AtomicBool::new(false),
        &mut |_, _, _| {},
    )
}

fn skipped(v: &Verdict) -> &str {
    match v {
        Verdict::Skipped(s) => s,
        v => panic!("expected Skipped, got {v:?}"),
    }
}

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    fs::write(&p, format!("#!/bin/sh\n{body}")).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    p
}

#[test]
fn deletes_files_and_dirs_and_reports_done() {
    let (t, root, here) = tree();
    let outside = t.path().join("outside");
    put(&outside.join("keep"));
    put(&here.join("f"));
    put(&here.join("d/a/b"));
    symlink(&outside, here.join("d/link")).unwrap();
    put(&here.join("other"));
    let hits = [hit(&root, "h/f"), hit(&root, "h/d")];
    let mut seen = Vec::new();
    let v = run(
        &recipe(Apply::Delete),
        &root,
        &here,
        &hits,
        None,
        &AtomicBool::new(false),
        &mut |i, h, v| seen.push((i, h.path.clone(), v.clone())),
    );
    assert_eq!(v, [Verdict::Done, Verdict::Done]);
    assert_eq!(
        seen,
        [
            (0, b"h/f".to_vec(), Verdict::Done),
            (1, b"h/d".to_vec(), Verdict::Done)
        ]
    );
    assert!(!here.join("f").exists());
    assert!(fs::symlink_metadata(here.join("d")).is_err());
    assert!(here.join("other").exists(), "unticked path removed");
    assert!(outside.join("keep").exists(), "followed a symlink");
}

#[test]
fn skips_vanished_changed_and_symlink_swapped_paths() {
    let (t, root, here) = tree();
    let outside = t.path().join("outside");
    put(&outside.join("keep"));
    put(&outside.join("x"));
    put(&outside.join("file"));
    for f in ["gone", "changed", "kind/x", "swap/x", "p/x", "flink"] {
        put(&here.join(f));
    }
    fs::create_dir(here.join("nowdir")).unwrap();
    let mut hits = vec![
        hit(&root, "h/gone"),
        hit(&root, "h/changed"),
        hit(&root, "h/kind"),
        hit(&root, "h/swap"),
        hit(&root, "h/p/x"),
        hit(&root, "h/flink"),
    ];
    hits.push(Hit {
        is_dir: false,
        ..hit(&root, "h/nowdir")
    });
    // What changes after the scan:
    fs::remove_file(here.join("gone")).unwrap();
    let f = fs::File::options()
        .write(true)
        .open(here.join("changed"))
        .unwrap();
    f.set_modified(SystemTime::now() + Duration::from_secs(1000))
        .unwrap();
    fs::remove_dir_all(here.join("kind")).unwrap();
    put(&here.join("kind"));
    fs::remove_dir_all(here.join("swap")).unwrap();
    symlink(&outside, here.join("swap")).unwrap();
    fs::remove_dir_all(here.join("p")).unwrap();
    symlink(&outside, here.join("p")).unwrap();
    fs::remove_file(here.join("flink")).unwrap();
    symlink(outside.join("file"), here.join("flink")).unwrap();

    let v = go(&recipe(Apply::Delete), &root, &here, &hits, None);
    let why: Vec<&str> = v.iter().map(skipped).collect();
    assert_eq!(why[0], "no longer exists");
    assert_eq!(why[1], "changed since the scan");
    assert_eq!(why[2], "is now a file");
    assert_eq!(why[3], "is now a symlink");
    assert!(why[4].contains("symlink"), "{}", why[4]);
    assert_eq!(why[5], "is now a symlink");
    assert_eq!(why[6], "is now a directory");
    for f in ["keep", "x", "file"] {
        assert!(outside.join(f).exists(), "outside {f} touched");
    }
    assert!(here.join("changed").exists());
    assert!(here.join("kind").exists());
    assert!(here.join("nowdir").exists());
    assert!(fs::symlink_metadata(here.join("swap")).is_ok());
}

#[test]
fn refuses_paths_outside_here() {
    let (t, root, here) = tree();
    let abs = t.path().canonicalize().unwrap().join("abs");
    put(&abs);
    put(&root.join("x"));
    put(&root.join("sib/f"));
    put(&here.join("f"));
    let hits = [
        raw(b"../x", false),
        raw(b"h/../x", false),
        raw(b"h/../../abs", false),
        raw(abs.as_os_str().as_encoded_bytes(), false),
        raw(b"", true),
        raw(b"h", true),
        raw(b"h/", true),
        raw(b"sib/f", false),
        raw(b"sib", true),
    ];
    let v = go(&recipe(Apply::Delete), &root, &here, &hits, None);
    assert_eq!(v.len(), hits.len());
    for (h, v) in hits.iter().zip(&v) {
        assert!(
            matches!(v, Verdict::Skipped(_)),
            "{:?}: {v:?}",
            String::from_utf8_lossy(&h.path)
        );
    }
    for p in [&abs, &root.join("x"), &root.join("sib/f"), &here.join("f")] {
        assert!(p.exists(), "{} removed", p.display());
    }
}

#[test]
fn script_gets_path_cwd_and_shared_bin_first() {
    let (t, root, here) = tree();
    let tools = t.path().join("tools");
    let bin = tools.join("bin");
    fs::create_dir_all(&bin).unwrap();
    script(&bin, "mytool", "echo shared\n");
    let log = t.path().join("log");
    let s = script(
        &tools,
        "s.sh",
        &format!(
            "printf '%s|%s|%s|%s|%s|%s\\n' \"$1\" \"$PWD\" \"$DUME_HERE\" \"$DUME_PATH\" \"$(mytool)\" \"${{PATH%%:*}}\" >> {}\n",
            log.display()
        ),
    );
    put(&here.join("d/f"));
    put(&here.join("g"));
    let hits = [hit(&root, "h/d"), hit(&root, "h/g")];
    let v = go(&recipe(Apply::Script(s)), &root, &here, &hits, Some(&bin));
    assert_eq!(v, [Verdict::Done, Verdict::Done]);
    let (d, g, h, b) = (
        here.join("d"),
        here.join("g"),
        here.display().to_string(),
        bin.display().to_string(),
    );
    let (d, g) = (d.display(), g.display());
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        format!("{d}|{h}|{h}|{d}|shared|{b}\n{g}|{h}|{h}|{g}|shared|{b}\n")
    );
    assert!(here.join("d/f").exists(), "script mode must not delete");
}

#[test]
fn script_failure_is_reported_and_run_continues() {
    let (t, root, here) = tree();
    let s = script(
        t.path(),
        "s.sh",
        "case \"$1\" in *bad) echo first >&2; echo boom >&2; echo >&2; exit 3;; esac\n\
         case \"$1\" in *quiet) exit 4;; esac\ntouch \"$1.ok\"\n",
    );
    for f in ["bad", "quiet", "good"] {
        put(&here.join(f));
    }
    let hits = [
        hit(&root, "h/bad"),
        hit(&root, "h/quiet"),
        hit(&root, "h/good"),
    ];
    let v = go(&recipe(Apply::Script(s.clone())), &root, &here, &hits, None);
    match &v[0] {
        Verdict::Failed(m) => assert!(m.ends_with("boom"), "{m}"),
        v => panic!("{v:?}"),
    }
    match &v[1] {
        Verdict::Failed(m) => assert!(m.contains('4'), "{m}"),
        v => panic!("{v:?}"),
    }
    assert_eq!(v[2], Verdict::Done);
    assert!(here.join("good.ok").exists());

    // The script became group-writable after loading: refused, never run.
    fs::set_permissions(&s, fs::Permissions::from_mode(0o775)).unwrap();
    let v = go(&recipe(Apply::Script(s)), &root, &here, &hits[2..], None);
    assert!(
        matches!(&v[0], Verdict::Failed(m) if m.contains("writable")),
        "{v:?}"
    );
    // A script that cannot be spawned.
    let v = go(
        &recipe(Apply::Script(t.path().join("missing.sh"))),
        &root,
        &here,
        &hits[2..],
        None,
    );
    assert!(matches!(&v[0], Verdict::Failed(_)), "{v:?}");
}

#[test]
fn cancel_stops_after_current_path() {
    let (_t, root, here) = tree();
    for f in ["a", "b", "c"] {
        put(&here.join(f));
    }
    let hits = [hit(&root, "h/a"), hit(&root, "h/b"), hit(&root, "h/c")];
    let cancel = AtomicBool::new(false);
    let mut calls = 0;
    let v = run(
        &recipe(Apply::Delete),
        &root,
        &here,
        &hits,
        None,
        &cancel,
        &mut |_, _, _| {
            calls += 1;
            cancel.store(true, Ordering::Relaxed);
        },
    );
    assert_eq!(
        v,
        [
            Verdict::Done,
            Verdict::Skipped("cancelled".into()),
            Verdict::Skipped("cancelled".into())
        ]
    );
    assert_eq!(calls, 3);
    assert!(!here.join("a").exists());
    assert!(here.join("b").exists() && here.join("c").exists());
}

/// The guard passes, then a parent is swapped for a symlink to an outside directory before
/// the delete: the delete must act on the directory that was checked, never through the link.
#[test]
fn parent_swapped_for_symlink_after_guard_does_not_delete_outside() {
    let (t, root, here) = tree();
    let outside = t.path().join("outside");
    put(&outside.join("x"));
    put(&outside.join("d/y"));
    put(&here.join("p/x"));
    put(&here.join("p/d/y"));
    let fx = check(&root, &here, &hit(&root, "h/p/x")).unwrap();
    let dd = check(&root, &here, &hit(&root, "h/p/d")).unwrap();
    fs::rename(here.join("p"), here.join("p.old")).unwrap();
    symlink(&outside, here.join("p")).unwrap();
    assert_eq!(delete(&fx, &|_, _| {}), Verdict::Done);
    assert_eq!(delete(&dd, &|_, _| {}), Verdict::Done);
    assert!(
        outside.join("x").exists(),
        "deleted through a swapped parent"
    );
    assert!(
        outside.join("d/y").exists(),
        "deleted through a swapped parent"
    );
    // What was checked is what got deleted.
    assert!(!here.join("p.old/x").exists());
    assert!(!here.join("p.old/d").exists());
}

/// Tests can't mount, so the device of each entry is overridden: `mnt` pretends to be
/// another filesystem. It must be neither entered nor removed, and the rest still goes.
#[test]
fn mount_point_or_other_device_is_not_crossed() {
    let (_t, root, here) = tree();
    put(&here.join("d/a/f"));
    put(&here.join("d/mnt/inside"));
    put(&here.join("d/g"));
    let c = check(&root, &here, &hit(&root, "h/d")).unwrap();
    let v = delete(&c, &|name, st| {
        if name == "mnt" {
            st.st_dev += 1
        }
    });
    assert_eq!(v, Verdict::Failed("contains a mount point: mnt".into()));
    assert!(
        here.join("d/mnt/inside").exists(),
        "crossed into another device"
    );
    assert!(!here.join("d/a").exists() && !here.join("d/g").exists());
}

/// Tests can't chown, so the owner of each entry is overridden: `theirs` (a file) and
/// `shared` (a folder) belong to someone else. They and their parents stay; the rest goes.
#[test]
fn other_peoples_files_are_left_alone() {
    let (_t, root, here) = tree();
    put(&here.join("d/a/theirs"));
    put(&here.join("d/a/mine"));
    put(&here.join("d/shared/x"));
    put(&here.join("d/g"));
    let c = check(&root, &here, &hit(&root, "h/d")).unwrap();
    let v = delete(&c, &|name, st| {
        if name == "theirs" || name == "shared" {
            st.st_uid += 1
        }
    });
    assert_eq!(
        v,
        Verdict::Failed("contains files owned by other people: a/theirs".into())
    );
    assert!(here.join("d/a/theirs").exists() && here.join("d/shared/x").exists());
    assert!(!here.join("d/a/mine").exists() && !here.join("d/g").exists());
}

#[test]
fn hit_owned_by_someone_else_is_skipped() {
    let (_t, root, here) = tree();
    put(&here.join("f"));
    let me = rustix::process::getuid().as_raw();
    let why = check_as(&root, &here, &hit(&root, "h/f"), me + 1)
        .err()
        .unwrap();
    assert_eq!(why, "is owned by someone else");
    assert!(check_as(&root, &here, &hit(&root, "h/f"), me).is_ok());
}

#[test]
fn too_deep_a_tree_fails_plainly() {
    let (_t, root, here) = tree();
    let deep: PathBuf = std::iter::repeat_n("a", 300).collect();
    put(&here.join("d").join(&deep).join("f"));
    let c = check(&root, &here, &hit(&root, "h/d")).unwrap();
    assert_eq!(
        delete(&c, &|_, _| {}),
        Verdict::Failed("folder is nested too deeply".into())
    );
    assert!(here.join("d").exists());
}

#[test]
fn prefix_sibling_is_not_touched() {
    let (_t, root, here) = tree();
    put(&here.join("case1/f"));
    put(&here.join("case10/f"));
    put(&root.join("h0/f"));
    let hits = [hit(&root, "h/case1"), raw(b"h0/f", false)];
    let v = go(&recipe(Apply::Delete), &root, &here, &hits, None);
    assert_eq!(v[0], Verdict::Done);
    assert!(matches!(v[1], Verdict::Skipped(_)), "{v:?}");
    assert!(!here.join("case1").exists());
    assert!(here.join("case10/f").exists() && root.join("h0/f").exists());
}

#[test]
fn non_utf8_name_is_deleted() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let (_t, root, here) = tree();
    put(&here.join(OsStr::from_bytes(b"caf\xe9")));
    put(&here.join(OsStr::from_bytes(b"d\xff/f")));
    let mut f = hit(&root, "h");
    f.path = b"h/caf\xe9".to_vec();
    f.is_dir = false;
    f.mtime = None;
    let hits = [f, raw(b"h/d\xff", true)];
    let v = go(&recipe(Apply::Delete), &root, &here, &hits, None);
    assert_eq!(v, [Verdict::Done, Verdict::Done]);
    assert_eq!(fs::read_dir(&here).unwrap().count(), 0);
}

#[test]
fn double_slash_in_hit_path_is_normalised_or_refused() {
    let (_t, root, here) = tree();
    put(&here.join("f"));
    put(&here.join("g"));
    put(&root.join("x"));
    let hits = [
        raw(b"h//f", false),
        raw(b"h/./g", false),
        raw(b"h//../x", false),
    ];
    let v = go(&recipe(Apply::Delete), &root, &here, &hits, None);
    // `Path` normalises `//` and an inner `.`, so these name the real files below `here`.
    assert_eq!(v[0], Verdict::Done);
    assert_eq!(v[1], Verdict::Done);
    assert!(matches!(v[2], Verdict::Skipped(_)), "{v:?}");
    assert!(!here.join("f").exists() && !here.join("g").exists());
    assert!(root.join("x").exists());
}
