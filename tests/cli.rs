use serde_json::Value;
use std::fs;
use std::os::unix::fs::{MetadataExt, symlink};
use std::process::{Command, Stdio};

fn dume() -> Command {
    Command::new(env!("CARGO_BIN_EXE_dume-scan"))
}

fn my_uid() -> u32 {
    fs::metadata(env!("CARGO_MANIFEST_DIR")).unwrap().uid()
}

fn run(args: &[&str]) -> (i32, String, String) {
    let o = dume().args(args).stdin(Stdio::null()).output().unwrap();
    (
        o.status.code().unwrap(),
        String::from_utf8(o.stdout).unwrap(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    )
}

/// Names of all files in an ncdu document, as slash-joined paths below the root.
fn paths(v: &Value) -> Vec<String> {
    fn walk(dir: &[Value], prefix: &str, out: &mut Vec<String>) {
        for item in &dir[1..] {
            match item {
                Value::Array(a) => {
                    let n = a[0]["name"].as_str().unwrap();
                    walk(a, &format!("{prefix}{n}/"), out);
                }
                o => out.push(format!("{prefix}{}", o["name"].as_str().unwrap())),
            }
        }
    }
    let mut out = Vec::new();
    walk(v[3].as_array().unwrap(), "", &mut out);
    out.sort();
    out
}

fn parse(s: &str) -> Value {
    serde_json::from_str(s).expect("valid JSON")
}

fn tree() -> tempfile::TempDir {
    let t = tempfile::tempdir().unwrap();
    let p = t.path();
    fs::create_dir_all(p.join("a/b")).unwrap();
    fs::create_dir_all(p.join("empty/deeper")).unwrap();
    fs::write(p.join("top.txt"), b"hello").unwrap();
    fs::write(p.join("a/one"), vec![0u8; 5000]).unwrap();
    fs::write(p.join("a/b/two \"quoted\"\n"), b"x").unwrap();
    symlink(p.join("top.txt"), p.join("link")).unwrap();
    symlink(p.join("a"), p.join("dirlink")).unwrap();
    fs::hard_link(p.join("top.txt"), p.join("a/hard")).unwrap();
    t
}

#[test]
fn lists_own_regular_files_only() {
    let t = tree();
    let (code, out, _) = run(&[t.path().to_str().unwrap(), "-j", "4"]);
    assert_eq!(code, 0);
    let v = parse(&out);
    assert_eq!(v[0], 1);
    assert_eq!(v[1], 2);
    assert_eq!(v[2]["progname"], "dume");
    assert_eq!(
        paths(&v),
        ["a/b/two \"quoted\"\n", "a/hard", "a/one", "top.txt"],
        "symlinks skipped, empty dirs pruned"
    );
    assert_eq!(v[3][0]["name"], t.path().to_str().unwrap());
}

#[test]
fn sizes_and_hardlinks() {
    let t = tree();
    let (_, out, _) = run(&[t.path().to_str().unwrap()]);
    let v = parse(&out);
    let a = v[3]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x.is_array())
        .unwrap();
    let one = a
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["name"] == "one")
        .unwrap();
    assert_eq!(one["asize"], 5000);
    assert!(one["dsize"].as_u64().unwrap() >= 5000);
    let hard = a
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["name"] == "hard")
        .unwrap();
    assert_eq!(hard["nlink"], 2);
    assert_eq!(hard["hlnkc"], true);
    assert!(one.get("hlnkc").is_none());
}

#[test]
fn other_uid_gets_nothing() {
    let t = tree();
    let other = (my_uid() + 12345).to_string();
    let (code, out, _) = run(&[t.path().to_str().unwrap(), "-u", &other]);
    assert_eq!(code, 0);
    let v = parse(&out);
    assert!(paths(&v).is_empty());
    assert_eq!(v[3].as_array().unwrap().len(), 1, "root only");
}

#[test]
fn explicit_uid_matches_default() {
    let t = tree();
    let d = t.path().to_str().unwrap();
    let u = my_uid().to_string();
    let a = paths(&parse(&run(&[d]).1));
    let b = paths(&parse(&run(&[d, "--uid", &u]).1));
    assert_eq!(a, b);
    assert!(!a.is_empty());
}

#[test]
fn non_utf8_names_stay_valid_json() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let t = tempfile::tempdir().unwrap();
    fs::write(t.path().join(OsStr::from_bytes(b"bad\xff\xfename")), b"x").unwrap();
    let v = parse(&run(&[t.path().to_str().unwrap()]).1);
    assert_eq!(paths(&v), ["bad\u{fffd}\u{fffd}name"]);
}

#[test]
fn big_directory_uses_parallel_path() {
    let t = tempfile::tempdir().unwrap();
    for i in 0..2000 {
        fs::write(t.path().join(format!("f{i}")), b"").unwrap();
    }
    fs::create_dir(t.path().join("sub")).unwrap();
    fs::write(t.path().join("sub/x"), b"y").unwrap();
    let (code, out, err) = run(&[t.path().to_str().unwrap(), "-j", "8"]);
    assert_eq!(code, 0);
    assert_eq!(paths(&parse(&out)).len(), 2001);
    assert!(err.contains("2001 files"), "{err}");
}

#[test]
fn unreadable_subdir_is_warning_not_failure() {
    use std::os::unix::fs::PermissionsExt;
    let t = tree();
    let locked = t.path().join("a/b");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let (code, out, err) = run(&[t.path().to_str().unwrap()]);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    parse(&out);
    if my_uid() != 0 {
        assert_eq!(code, 0);
        assert!(err.contains("could not be read"), "{err}");
    }
}

#[test]
fn does_not_follow_root_symlink() {
    let t = tree();
    let l = t.path().join("dirlink");
    let (code, _, err) = run(&[l.to_str().unwrap()]);
    assert_eq!(code, 1, "{err}");
}

#[test]
fn missing_dir_is_runtime_error() {
    let (code, _, err) = run(&["/nonexistent/dume/path"]);
    assert_eq!(code, 1);
    assert!(err.contains("nonexistent"));
}

#[test]
fn file_as_root_is_runtime_error() {
    let t = tree();
    let (code, ..) = run(&[t.path().join("top.txt").to_str().unwrap()]);
    assert_eq!(code, 1);
}

#[test]
fn usage_errors_exit_2() {
    assert_eq!(run(&[]).0, 2, "DIR is mandatory");
    assert_eq!(run(&["/tmp", "-j", "0"]).0, 2);
    assert_eq!(run(&["/tmp", "-j", "abc"]).0, 2);
    assert_eq!(run(&["/tmp", "--bogus"]).0, 2);
    assert_eq!(run(&["/tmp", "-u", "-1"]).0, 2);
}

#[test]
fn help_and_version_exit_0() {
    assert_eq!(run(&["--help"]).0, 0);
    assert_eq!(run(&["--version"]).0, 0);
}

#[test]
fn refuses_terminal_stdout() {
    // Give the child a pty as stdout via `script`; skip if unavailable.
    let Ok(sh) = Command::new("script").arg("--version").output() else {
        return;
    };
    if !sh.status.success() {
        return;
    }
    let t = tree();
    let cmd = format!("{} {}", env!("CARGO_BIN_EXE_dume-scan"), t.path().display());
    let o = Command::new("script")
        .args(["-qec", &format!("{cmd}; echo EXIT=$?"), "/dev/null"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&o.stdout);
    assert!(s.contains("refusing to write JSON to a terminal"), "{s}");
    assert!(s.contains("EXIT=2"), "{s}");
    assert!(!s.contains("progname"), "{s}");
}

#[test]
fn ncdu_round_trip() {
    let have = Command::new("ncdu").arg("--version").output().is_ok();
    if !have {
        assert!(
            std::env::var_os("DUME_REQUIRE_NCDU").is_none(),
            "ncdu required but not installed"
        );
        eprintln!("ncdu not installed; skipping");
        return;
    }
    let t = tree();
    let (_, json, _) = run(&[t.path().to_str().unwrap()]);
    let dir = tempfile::tempdir().unwrap();
    let inp = dir.path().join("me.json");
    let outp = dir.path().join("re.json");
    fs::write(&inp, &json).unwrap();
    let o = Command::new("ncdu")
        .arg("-f")
        .arg(&inp)
        .arg("-o")
        .arg(&outp)
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "ncdu rejected export: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    // ncdu's re-export must contain the same files.
    assert_eq!(
        paths(&parse(&fs::read_to_string(&outp).unwrap())),
        paths(&parse(&json))
    );

    // And piping via stdin works too.
    let mut child = dume().arg(t.path()).stdout(Stdio::piped()).spawn().unwrap();
    let o = Command::new("ncdu")
        .args(["-f-", "-o"])
        .arg(&outp)
        .stdin(child.stdout.take().unwrap())
        .output()
        .unwrap();
    assert!(child.wait().unwrap().success());
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
}

// ---- --merge ----

fn merge(dir: &std::path::Path, file: &std::path::Path, extra: &[&str]) -> (i32, String) {
    let mut a = vec![dir.to_str().unwrap(), "-m", file.to_str().unwrap()];
    a.extend_from_slice(extra);
    let (c, _, e) = run(&a);
    (c, e)
}

fn export(dir: &std::path::Path, file: &std::path::Path) {
    let (c, _, e) = run(&[dir.to_str().unwrap(), "-o", file.to_str().unwrap()]);
    assert_eq!(c, 0, "{e}");
}

/// The database's ncdu JSON, parsed.
fn db_json(file: &std::path::Path) -> Value {
    let (c, out, e) = run(&["--json", file.to_str().unwrap()]);
    assert_eq!(c, 0, "{e}");
    parse(&out)
}

#[test]
fn merge_picks_up_adds_changes_and_deletes() {
    let t = tree();
    let out = tempfile::tempdir().unwrap();
    let f = out.path().join("me.db");
    export(t.path(), &f);

    fs::write(t.path().join("a/b/new"), vec![0u8; 9000]).unwrap();
    fs::remove_file(t.path().join("a/one")).unwrap();
    fs::create_dir_all(t.path().join("a/fresh/deep")).unwrap();
    fs::write(t.path().join("a/fresh/deep/f"), b"z").unwrap();
    fs::write(t.path().join("top.txt"), b"changed outside the merge").unwrap();

    let (c, e) = merge(&t.path().join("a"), &f, &["-j", "2"]);
    assert_eq!(c, 0, "{e}");
    let v = db_json(&f);
    assert_eq!(
        paths(&v),
        [
            "a/b/new",
            "a/b/two \"quoted\"\n",
            "a/fresh/deep/f",
            "a/hard",
            "top.txt"
        ]
    );
    // top.txt is outside the merged subtree, so it keeps its old size.
    let top = v[3]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["name"] == "top.txt")
        .unwrap();
    assert_eq!(top["asize"], 5);
    assert_eq!(v[2]["progname"], "dume");
}

#[test]
fn merge_prunes_emptied_dirs_and_equals_fresh_scan() {
    let t = tree();
    let out = tempfile::tempdir().unwrap();
    let f = out.path().join("me.db");
    export(t.path(), &f);
    fs::remove_file(t.path().join("a/b/two \"quoted\"\n")).unwrap();
    assert_eq!(merge(&t.path().join("a/b"), &f, &[]).0, 0);
    let v = db_json(&f);
    assert_eq!(paths(&v), ["a/hard", "a/one", "top.txt"]);
    // Same result as scanning from scratch.
    assert_eq!(
        paths(&v),
        paths(&parse(&run(&[t.path().to_str().unwrap()]).1))
    );
}

#[test]
fn merge_at_root_and_ncdu_accepts_result() {
    let t = tree();
    let out = tempfile::tempdir().unwrap();
    let f = out.path().join("me.db");
    export(t.path(), &f);
    fs::remove_dir_all(t.path().join("a")).unwrap();
    assert_eq!(merge(t.path(), &f, &[]).0, 0);
    let v = db_json(&f);
    assert_eq!(paths(&v), ["top.txt"]);
    assert_eq!(v[3][0]["name"], t.path().to_str().unwrap());
    if Command::new("ncdu").arg("--version").output().is_ok() {
        let o = Command::new("ncdu")
            .args(["-f"])
            .arg(&f)
            .args(["-o", "/dev/null"])
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }
}

#[test]
fn merge_rejects_bad_inputs_and_leaves_file_alone() {
    let t = tree();
    let other = tree();
    let out = tempfile::tempdir().unwrap();
    let f = out.path().join("me.db");
    export(t.path(), &f);
    let before = fs::read(&f).unwrap();

    // outside the export's root
    assert_eq!(merge(other.path(), &f, &[]).0, 1);
    // not a database
    let junk = out.path().join("junk.json");
    fs::write(&junk, "nope").unwrap();
    assert_eq!(merge(t.path(), &junk, &[]).0, 1);
    // missing export
    assert_eq!(merge(t.path(), &out.path().join("none.json"), &[]).0, 1);

    assert_eq!(fs::read(&f).unwrap(), before);
    assert!(!out.path().join(".me.db.tmp").exists());
}

#[test]
fn merge_works_when_stdout_is_a_terminal_free_pipe() {
    // --merge never writes JSON to stdout.
    let t = tree();
    let out = tempfile::tempdir().unwrap();
    let f = out.path().join("me.db");
    export(t.path(), &f);
    let (_, stdout, _) = run(&[t.path().to_str().unwrap(), "-m", f.to_str().unwrap()]);
    assert!(stdout.is_empty());
}

// ---- --report ----

fn report_tree() -> (tempfile::TempDir, tempfile::TempDir, std::path::PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let p = t.path();
    for i in 0..5 {
        let d = p.join(format!("proj/run_{i:04}"));
        fs::create_dir_all(d.join("node_modules/node_modules")).unwrap();
        fs::write(d.join("out.log"), vec![0u8; 8192]).unwrap();
        fs::write(d.join("node_modules/a.js"), vec![0u8; 4096]).unwrap();
        fs::write(d.join("node_modules/node_modules/b.js"), vec![0u8; 4096]).unwrap();
    }
    fs::create_dir_all(p.join("other")).unwrap();
    fs::write(p.join("other/big.bin"), vec![1u8; 1 << 20]).unwrap();
    fs::hard_link(p.join("other/big.bin"), p.join("other/big2.bin")).unwrap();
    let out = tempfile::tempdir().unwrap();
    let f = out.path().join("dume.db");
    export(p, &f);
    (t, out, f)
}

fn report(dir: &std::path::Path, f: &std::path::Path, extra: &[&str]) -> (i32, String, String) {
    let mut a = vec![dir.to_str().unwrap(), "-r", f.to_str().unwrap()];
    a.extend_from_slice(extra);
    run(&a)
}

/// The row for `pattern` in the report, split on whitespace.
fn row<'a>(out: &'a str, pattern: &str) -> Vec<&'a str> {
    out.lines()
        .find(|l| l.trim_start().starts_with(pattern))
        .unwrap_or_else(|| panic!("no row {pattern}:\n{out}"))
        .split_whitespace()
        .collect()
}

#[test]
fn report_groups_derived_patterns() {
    let (t, _o, f) = report_tree();
    let (code, out, err) = report(t.path(), &f, &[]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("hardlinks once"), "{out}");
    // 5 run dirs collapse into one derived pattern.
    assert_eq!(row(&out, "run_#")[1], "5", "{out}");
    // Nested node_modules is only counted at the outermost level: 5, not 10.
    assert_eq!(row(&out, "node_modules")[1], "5", "{out}");
    // File patterns come from extensions.
    assert_eq!(row(&out, "*.log")[1], "5", "{out}");
    assert_eq!(row(&out, "*.js")[1], "10", "{out}");
    // Hardlinked 1 MiB counted once: total is well under 2 MiB.
    assert!(out.lines().next().unwrap().contains("1."), "{out}");
    assert!(out.contains("BIGGEST DIRECTORIES"), "{out}");
}

#[test]
fn report_on_subdir_and_top_limit() {
    let (t, _o, f) = report_tree();
    let (code, out, _) = report(&t.path().join("proj"), &f, &["-n", "1"]);
    assert_eq!(code, 0);
    assert!(!out.contains("big.bin"), "only proj is analysed:\n{out}");
    let dirs_section: Vec<_> = out
        .lines()
        .skip_while(|l| !l.starts_with("DIRECTORY"))
        .skip(2)
        .take_while(|l| !l.is_empty())
        .collect();
    assert_eq!(dirs_section.len(), 1, "-n 1 limits rows:\n{out}");
}

#[test]
fn report_ignores_rare_names_and_errors_cleanly() {
    let (t, _o, f) = report_tree();
    let (_, out, _) = report(t.path(), &f, &[]);
    assert!(
        !out.contains("other  "),
        "single dirs are not patterns:\n{out}"
    );
    // dir that exists on disk but not in the export
    fs::create_dir(t.path().join("unlisted")).unwrap();
    assert_eq!(report(&t.path().join("unlisted"), &f, &[]).0, 1);
    // outside the export root
    let other = tempfile::tempdir().unwrap();
    assert_eq!(report(other.path(), &f, &[]).0, 1);
    // flags that make no sense together
    let (c, ..) = run(&[
        t.path().to_str().unwrap(),
        "-r",
        f.to_str().unwrap(),
        "-m",
        f.to_str().unwrap(),
    ]);
    assert_eq!(c, 2);
    assert_eq!(
        run(&[t.path().to_str().unwrap(), "-n", "3"]).0,
        2,
        "-n needs --report"
    );
}

// ---- dume (wrapper) ----

#[test]
fn wrapper_needs_a_terminal() {
    let o = Command::new(env!("CARGO_BIN_EXE_dume"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&o.stderr).contains("needs a terminal"));
}

#[test]
fn wrapper_explains_a_bad_install_on_a_terminal() {
    let Ok(sh) = Command::new("script").arg("--version").output() else {
        return;
    };
    if !sh.status.success() {
        return;
    }
    // This test binary is not installed as PREFIX/bin/dume, so the layout check must say so.
    let cmd = env!("CARGO_BIN_EXE_dume");
    let o = Command::new("script")
        .args(["-qec", &format!("{cmd}; echo EXIT=$?"), "/dev/null"])
        .env_remove("DUME_COMMON")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&o.stdout);
    assert!(
        s.contains("PREFIX/bin/dume") && s.contains("DUME_COMMON"),
        "{s}"
    );
    assert!(s.contains("EXIT=1"), "{s}");
}

// ---- database output ----

fn ncdu_or_skip() -> bool {
    if Command::new("ncdu").arg("--version").output().is_ok() {
        return true;
    }
    assert!(
        std::env::var_os("DUME_REQUIRE_NCDU").is_none(),
        "ncdu required but not installed"
    );
    eprintln!("ncdu not installed; skipping");
    false
}

#[test]
fn json_from_db_is_accepted_by_ncdu() {
    if !ncdu_or_skip() {
        return;
    }
    let t = tree();
    let d = tempfile::tempdir().unwrap();
    let (f, inp, outp) = (
        d.path().join("x.db"),
        d.path().join("a.json"),
        d.path().join("b.json"),
    );
    export(t.path(), &f);
    fs::write(&inp, run(&["--json", f.to_str().unwrap()]).1).unwrap();
    let o = Command::new("ncdu")
        .arg("-f")
        .arg(&inp)
        .arg("-o")
        .arg(&outp)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(
        paths(&parse(&fs::read_to_string(&outp).unwrap())),
        paths(&db_json(&f))
    );
}

/// Sum of dsize over files, counting each hlnkc inode once, as ncdu does.
fn total(v: &Value) -> u64 {
    fn walk(dir: &[Value], seen: &mut std::collections::HashSet<u64>) -> u64 {
        dir[1..]
            .iter()
            .map(|i| match i.as_array() {
                Some(a) => walk(a, seen),
                None if i["hlnkc"] == true && !seen.insert(i["ino"].as_u64().unwrap()) => 0,
                None => i["dsize"].as_u64().unwrap(),
            })
            .sum()
    }
    walk(v[3].as_array().unwrap(), &mut Default::default())
}

#[test]
fn json_from_db_matches_direct_scan_totals() {
    let t = tree();
    let d = tempfile::tempdir().unwrap();
    let f = d.path().join("x.db");
    export(t.path(), &f);
    let from_db = db_json(&f);
    let direct = parse(&run(&[t.path().to_str().unwrap()]).1);
    assert_eq!(total(&from_db), total(&direct));
    assert_eq!(paths(&from_db), paths(&direct));
    assert_eq!(from_db[2]["progname"], "dume");
    assert_eq!(from_db[2]["uid"], my_uid());
    assert_eq!(from_db[3][0]["name"], t.path().to_str().unwrap());
    let hard = from_db[3]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["name"] == "top.txt")
        .unwrap();
    assert_eq!(hard["hlnkc"], true);
    assert_eq!(hard["nlink"], 2);
    assert!(hard["mtime"].is_i64());
}

#[test]
fn import_json_then_children_match() {
    use dumelib::db::{Db, import_json};
    let t = tree();
    let d = tempfile::tempdir().unwrap();
    let (j, a, b) = (
        d.path().join("x.json"),
        d.path().join("a.db"),
        d.path().join("b.db"),
    );
    fs::write(&j, run(&[t.path().to_str().unwrap()]).1).unwrap();
    assert_eq!(import_json(&j, &a).unwrap(), 4);
    export(t.path(), &b);
    let (da, db) = (Db::open_ro(&a).unwrap(), Db::open_ro(&b).unwrap());
    assert_eq!(da.meta().unwrap().uid, my_uid());
    assert_eq!(da.meta().unwrap().root, t.path());
    for sub in ["", "a", "a/b"] {
        let p = t.path().join(sub);
        let key = |db: &Db| -> Vec<_> {
            db.children(&p)
                .unwrap()
                .into_iter()
                .map(|c| (c.name, c.size, c.is_dir))
                .collect()
        };
        assert_eq!(key(&da), key(&db), "{sub}");
    }
    assert_eq!(
        da.total_under(t.path()).unwrap(),
        db.total_under(t.path()).unwrap()
    );
    assert_eq!(fs::metadata(&a).unwrap().mode() & 0o777, 0o600);
}

#[test]
fn scan_o_then_merge_then_report() {
    let (t, _o, f) = report_tree();
    fs::write(t.path().join("proj/run_0000/new.log"), vec![0u8; 100_000]).unwrap();
    let (c, _, e) = run(&[
        t.path().join("proj").to_str().unwrap(),
        "-m",
        f.to_str().unwrap(),
    ]);
    assert_eq!(c, 0, "{e}");
    let (c, out, e) = report(t.path(), &f, &[]);
    assert_eq!(c, 0, "{e}");
    assert_eq!(row(&out, "*.log")[1], "6", "{out}");
    assert!(
        run(&[t.path().to_str().unwrap(), "-o", f.to_str().unwrap()])
            .2
            .contains("files for uid")
    );
}

#[test]
fn stdout_mode_unchanged_and_terminal_refused() {
    let t = tree();
    let (c, out, _) = run(&[t.path().to_str().unwrap()]);
    assert_eq!(c, 0);
    assert_eq!(parse(&out)[2]["progname"], "dume");
    assert_eq!(run(&[]).0, 2, "DIR is required without --json");
    assert_eq!(run(&["-m", "x.db"]).0, 2);
    let Ok(sh) = Command::new("script").arg("--version").output() else {
        return;
    };
    if !sh.status.success() {
        return;
    }
    let d = tempfile::tempdir().unwrap();
    let f = d.path().join("x.db");
    export(t.path(), &f);
    let cmd = format!("{} --json {}", env!("CARGO_BIN_EXE_dume-scan"), f.display());
    let o = Command::new("script")
        .args(["-qec", &format!("{cmd}; echo EXIT=$?"), "/dev/null"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&o.stdout);
    assert!(s.contains("refusing to write JSON to a terminal"), "{s}");
    assert!(s.contains("EXIT=2"), "{s}");
}

#[test]
fn empty_database_gives_empty_json_and_report() {
    let t = tempfile::tempdir().unwrap();
    fs::create_dir(t.path().join("sub")).unwrap();
    let d = tempfile::tempdir().unwrap();
    let f = d.path().join("x.db");
    export(t.path(), &f);
    let v = db_json(&f);
    assert_eq!(v[3].as_array().unwrap().len(), 1);
    assert_eq!(v[3][0]["name"], t.path().to_str().unwrap());
    if ncdu_or_skip() {
        let j = d.path().join("e.json");
        fs::write(&j, run(&["--json", f.to_str().unwrap()]).1).unwrap();
        let o = Command::new("ncdu")
            .arg("-f")
            .arg(&j)
            .args(["-o", "/dev/null"])
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }
    let (c, _, e) = report(t.path(), &f, &[]);
    assert_eq!(c, 0, "{e}");
    // a missing non-root path is still an error
    assert_eq!(report(&t.path().join("sub"), &f, &[]).0, 1);

    // importing an empty export reads back the same way
    let j = d.path().join("old.json");
    fs::write(&j, run(&[t.path().to_str().unwrap()]).1).unwrap();
    let g = d.path().join("y.db");
    assert_eq!(dumelib::db::import_json(&j, &g).unwrap(), 0);
    assert_eq!(db_json(&g)[3].as_array().unwrap().len(), 1);
    assert_eq!(report(t.path(), &g, &[]).0, 0);
}

#[test]
fn import_rejects_missing_names_and_uid_conflicts() {
    let d = tempfile::tempdir().unwrap();
    let j = d.path().join("bad.json");
    fs::write(
        &j,
        r#"[1,2,{"progname":"dume","uid":1},[{"name":"/r"},{"asize":1,"dsize":1,"ino":1}]]"#,
    )
    .unwrap();
    let e = dumelib::db::import_json(&j, &d.path().join("x.db")).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
    assert!(e.to_string().contains("name"), "{e}");
    let f = d.path().join("x.db");
    for flag in ["-m", "-r", "--json"] {
        let mut a = vec!["/tmp", "-u", "1", flag, f.to_str().unwrap()];
        if flag == "--json" {
            a.remove(0);
        }
        assert_eq!(run(&a).0, 2, "{flag}");
    }
}
