use dumelib::actions::{self, Action};
use dumelib::scan;
use dumelib::tui::{self, App, Mode};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

struct Env {
    _t: tempfile::TempDir,
    root: PathBuf,
    export: PathBuf,
    actions: PathBuf,
}

fn script(dir: &Path, file: &str, body: &str) {
    let p = dir.join(file);
    fs::write(&p, body).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
}

fn env() -> Env {
    let t = tempfile::tempdir().unwrap();
    let base = t.path().canonicalize().unwrap();
    let root = base.join("data");
    for i in 1..=3 {
        let c = root.join(format!("proj/case_{i}"));
        fs::create_dir_all(c.join("system")).unwrap();
        fs::write(c.join("system/controlDict"), b"x").unwrap();
        for step in ["100", "200"] {
            fs::create_dir_all(c.join(step)).unwrap();
            fs::write(c.join(step).join("big"), vec![1u8; 64 << 10]).unwrap();
        }
    }
    let export = base.join("dume.json");
    let uid = rustix::process::getuid().as_raw();
    let (s, dev) = scan::scan(&root, uid, 2, false).unwrap();
    let mut f = fs::File::create(&export).unwrap();
    s.write(&mut f, root.to_str().unwrap(), dev, uid, "test")
        .unwrap();
    let actions = base.join("actions");
    fs::create_dir(&actions).unwrap();
    script(
        &actions,
        "foam.sh",
        "#!/bin/sh\n# dume-name: Clean OpenFOAM case\n# dume-pattern: case_#\nrm -rf ./[0-9]*\n",
    );
    script(
        &actions,
        "other.sh",
        "#!/bin/sh\n# dume-name: Unrelated\n# dume-pattern: run_#\ntouch SHOULD_NOT_RUN\n",
    );
    fs::write(actions.join("notes.txt"), "# dume-name: not executable\n").unwrap();
    Env {
        _t: t,
        root,
        export,
        actions,
    }
}

fn app(e: &Env) -> App {
    let report = tui::load_report(&e.export, &e.root).unwrap();
    App::new(
        report,
        e.root.clone(),
        e.export.clone(),
        actions::load(&e.actions).unwrap().0,
        2,
    )
}

fn key(a: &mut App, c: KeyCode) {
    a.on_key(KeyEvent::new(c, KeyModifiers::NONE));
}

fn screen(a: &mut App) -> String {
    let mut t = Terminal::new(TestBackend::new(120, 30)).unwrap();
    t.draw(|f| a.draw(f)).unwrap();
    t.backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect()
}

fn wait_done(a: &mut App) {
    let t = Instant::now();
    while a.mode == Mode::Running {
        assert!(
            t.elapsed() < Duration::from_secs(20),
            "action did not finish"
        );
        a.tick();
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn loads_only_executables_with_metadata() {
    let e = env();
    let (a, skipped) = actions::load(&e.actions).unwrap();
    assert!(skipped.is_empty());
    let names: Vec<_> = a.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["Clean OpenFOAM case", "Unrelated"]);
    assert!(
        actions::load(&e.actions.join("missing"))
            .unwrap()
            .0
            .is_empty()
    );
}

#[test]
fn browse_pick_confirm_run_and_refresh() {
    let e = env();
    let mut a = app(&e);
    let before = a.report.total;
    assert!(
        screen(&mut a).contains("proj/"),
        "starts in Where, listing subdirectories"
    );
    key(&mut a, KeyCode::Tab);
    assert!(screen(&mut a).contains("case_#"), "What lists the patterns");

    key(&mut a, KeyCode::Enter);
    assert_eq!(a.mode, Mode::Matches);
    assert!(screen(&mut a).contains("proj/case_1"));

    key(&mut a, KeyCode::Enter);
    assert_eq!(a.mode, Mode::Actions);
    let s = screen(&mut a);
    assert!(s.contains("Clean OpenFOAM case"), "{s}");
    assert!(!s.contains("Unrelated"), "non-matching hidden until A");
    key(&mut a, KeyCode::Char('A'));
    assert!(screen(&mut a).contains("Unrelated"));
    key(&mut a, KeyCode::Char('A'));

    key(&mut a, KeyCode::Enter);
    assert_eq!(a.mode, Mode::Confirm);
    assert!(screen(&mut a).contains("Changes are permanent"));

    // Cancelling touches nothing.
    key(&mut a, KeyCode::Char('n'));
    assert_eq!(a.mode, Mode::Actions);
    assert!(e.root.join("proj/case_1/100/big").exists());

    key(&mut a, KeyCode::Enter);
    key(&mut a, KeyCode::Char('y'));
    wait_done(&mut a);
    assert!(a.failed.is_empty(), "{:?}", a.failed);
    for i in 1..=3 {
        let c = e.root.join(format!("proj/case_{i}"));
        assert!(!c.join("100").exists() && !c.join("200").exists());
        assert!(
            c.join("system/controlDict").exists(),
            "only the script's targets are removed"
        );
        assert!(!c.join("SHOULD_NOT_RUN").exists());
    }
    assert!(a.report.total < before, "report refreshed");
    assert!(screen(&mut a).contains("freed"));

    // The export on disk was updated too.
    let again = tui::load_report(&e.export, &e.root).unwrap();
    assert_eq!(again.total, a.report.total);

    key(&mut a, KeyCode::Char('x'));
    assert_eq!(a.mode, Mode::List);
}

#[test]
fn file_patterns_have_no_actions_and_q_quits() {
    let e = env();
    let mut a = app(&e);
    key(&mut a, KeyCode::Tab);
    key(&mut a, KeyCode::Char('f'));
    key(&mut a, KeyCode::Enter);
    assert_eq!(a.mode, Mode::List);
    assert!(screen(&mut a).contains("directory patterns"));
    key(&mut a, KeyCode::Char('q'));
    assert!(a.quit);
}

#[test]
fn script_failures_are_reported_and_symlinks_refused() {
    let e = env();
    script(
        &e.actions,
        "bad.sh",
        "#!/bin/sh\n# dume-pattern: case_#\necho boom >&2\nexit 3\n",
    );
    let acts = actions::load(&e.actions).unwrap().0;
    let bad: &Action = acts.iter().find(|a| a.path.ends_with("bad.sh")).unwrap();
    let dir = e.root.join("proj/case_1");
    let o = actions::run_on(bad, "case_#", &e.root, &dir, None);
    assert!(!o.ok && o.msg.contains("boom"), "{}", o.msg);

    // A symlink swapped in for a scanned directory must not be followed.
    let victim = e.root.join("proj/victim");
    fs::create_dir(&victim).unwrap();
    fs::write(victim.join("keep"), b"x").unwrap();
    fs::remove_dir_all(e.root.join("proj/case_2")).unwrap();
    symlink(&victim, e.root.join("proj/case_2")).unwrap();
    let foam = acts.iter().find(|a| a.path.ends_with("foam.sh")).unwrap();
    let o = actions::run_on(foam, "case_#", &e.root, &e.root.join("proj/case_2"), None);
    assert!(!o.ok && o.msg.contains("skipped"), "{}", o.msg);
    // Neither the root itself nor paths outside it are allowed.
    assert!(!actions::run_on(foam, "x", &e.root, &e.root, None).ok);
    assert!(!actions::run_on(foam, "x", &e.root, e.root.parent().unwrap(), None).ok);
}

#[test]
fn vanished_directory_is_removed_from_export() {
    let e = env();
    script(
        &e.actions,
        "rmdir.sh",
        "#!/bin/sh\n# dume-pattern: case_#\ncd .. && rm -rf \"$1\"\n",
    );
    let mut a = App::new(
        tui::load_report(&e.export, &e.root).unwrap(),
        e.root.clone(),
        e.export.clone(),
        actions::load(&e.actions).unwrap().0,
        2,
    );
    key(&mut a, KeyCode::Tab);
    key(&mut a, KeyCode::Enter);
    key(&mut a, KeyCode::Enter);
    // Pick the rmdir action: it sorts after "Clean OpenFOAM case".
    key(&mut a, KeyCode::Char('j'));
    key(&mut a, KeyCode::Enter);
    key(&mut a, KeyCode::Char('y'));
    wait_done(&mut a);
    assert!(a.failed.is_empty(), "{:?}", a.failed);
    assert!(!e.root.join("proj/case_1").exists());
    assert_eq!(a.report.total, 0, "everything under the data root is gone");
}

// ---- shared-install behaviour ----

#[test]
fn unsafe_actions_are_refused() {
    let e = env();
    // A script others can write to is skipped with a warning, and refused again at run time.
    let evil = e.actions.join("evil.sh");
    script(
        &e.actions,
        "evil.sh",
        "#!/bin/sh\n# dume-pattern: *\ntrue\n",
    );
    fs::set_permissions(&evil, fs::Permissions::from_mode(0o775)).unwrap();
    let (a, skipped) = actions::load(&e.actions).unwrap();
    assert_eq!(a.len(), 2);
    assert!(
        skipped.len() == 1 && skipped[0].contains("evil.sh"),
        "{skipped:?}"
    );
    let listed = Action {
        name: "e".into(),
        patterns: vec![],
        path: evil,
    };
    let dir = e.root.join("proj/case_1");
    let o = actions::run_on(&listed, "x", &e.root, &dir, None);
    assert!(!o.ok && o.msg.contains("writable"), "{}", o.msg);
    // A directory others can write to is refused outright.
    fs::set_permissions(&e.actions, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(
        actions::load(&e.actions)
            .unwrap_err()
            .to_string()
            .contains("writable")
    );
}

#[test]
fn shared_bin_dir_is_first_on_path() {
    let e = env();
    let bin = e.root.parent().unwrap().join("bin");
    fs::create_dir(&bin).unwrap();
    script(
        &bin,
        "sharedtool",
        "#!/bin/sh\necho shared > \"$1/used-shared-tool\"\n",
    );
    script(
        &e.actions,
        "uses-tool.sh",
        "#!/bin/sh\n# dume-pattern: case_#\nsharedtool \"$1\"\n",
    );
    let acts = actions::load(&e.actions).unwrap().0;
    let a = acts
        .iter()
        .find(|a| a.path.ends_with("uses-tool.sh"))
        .unwrap();
    let dir = e.root.join("proj/case_1");
    let o = actions::run_on(a, "case_#", &e.root, &dir, Some(&bin));
    assert!(o.ok, "{}", o.msg);
    assert!(dir.join("used-shared-tool").exists());
    // Without the shared dir the tool isn't found.
    assert!(!actions::run_on(a, "case_#", &e.root, &e.root.join("proj/case_2"), None).ok);
}

#[test]
fn startup_decisions() {
    use dumelib::tui::{Ask, startup};
    use std::time::SystemTime;
    let e = env();
    let uid = rustix::process::getuid().as_raw();
    let now = SystemTime::now();
    let missing = e.root.parent().unwrap().join("state/dume.json");

    assert_eq!(startup(&missing, &e.root, uid, None, now), Ask::NoExport);
    // A legacy export is offered for migration only when the new one is absent.
    assert_eq!(
        startup(&missing, &e.root, uid, Some(&e.export), now),
        Ask::Migrate(e.export.clone())
    );
    assert!(matches!(
        startup(&e.export, &e.root, uid, Some(&e.export), now),
        Ask::Existing { .. }
    ));

    let Ask::Existing { age } = startup(
        &e.export,
        &e.root,
        uid,
        None,
        now + Duration::from_secs(3 * 86400),
    ) else {
        panic!()
    };
    assert_eq!(dumelib::tui::age_text(age), "3 days");

    // Wrong user, wrong root, unreadable file -> a problem, never a crash.
    assert!(
        matches!(startup(&e.export, &e.root, uid + 1, None, now), Ask::Problem(m) if m.contains("another user"))
    );
    let other = e.root.join("proj");
    assert!(
        matches!(startup(&e.export, &other, uid, None, now), Ask::Problem(m) if m.contains("team root"))
    );
    let junk = e.root.parent().unwrap().join("junk.json");
    fs::write(&junk, "nope").unwrap();
    assert!(
        matches!(startup(&junk, &e.root, uid, None, now), Ask::Problem(m) if m.contains("can't be read"))
    );
}

fn launch(e: &Env, export: &Path, legacy: Option<PathBuf>) -> App {
    App::launch(tui::Launch {
        view: e.root.clone(),
        scan_root: e.root.clone(),
        export: export.to_path_buf(),
        legacy_export: legacy,
        actions: actions::load(&e.actions).unwrap().0,
        bin: None,
        threads: 2,
        stale_days: 7,
        uid: rustix::process::getuid().as_raw(),
    })
}

fn wait_scan(a: &mut App) {
    let t = Instant::now();
    while a.mode == Mode::Scanning {
        assert!(t.elapsed() < Duration::from_secs(20), "scan did not finish");
        screen(a);
        a.tick();
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn wrapper_first_run_asks_then_scans() {
    let e = env();
    let state = e.root.parent().unwrap().join("state/dume/dume.json");
    let mut a = launch(&e, &state, None);
    assert_eq!(a.mode, Mode::Ask);
    assert!(screen(&mut a).contains("no scan"));

    key(&mut a, KeyCode::Char('y'));
    assert_eq!(a.mode, Mode::Scanning);
    assert!(screen(&mut a).contains("Scanning"));
    wait_scan(&mut a);
    assert_eq!(a.mode, Mode::List);
    assert!(state.exists(), "export written, parent dirs created");
    assert!(!state.with_file_name(".dume.json.tmp").exists());
    assert!(a.report.files > 0 && screen(&mut a).contains("proj/"));
}

#[test]
fn wrapper_existing_scan_browse_or_confirmed_rescan() {
    let e = env();
    let mut a = launch(&e, &e.export, None);
    let s = screen(&mut a);
    assert!(
        s.contains("made just now ago") || s.contains("just now"),
        "{s}"
    );

    // 'r' asks again; 'n' goes back without touching the file.
    let before = fs::read(&e.export).unwrap();
    key(&mut a, KeyCode::Char('r'));
    assert!(screen(&mut a).contains("overwrite"));
    key(&mut a, KeyCode::Char('n'));
    assert!(screen(&mut a).contains("browse it"));
    assert_eq!(fs::read(&e.export).unwrap(), before);

    // Confirmed rescan replaces the file atomically.
    fs::write(e.root.join("proj/case_1/new_file"), b"n").unwrap();
    key(&mut a, KeyCode::Char('r'));
    key(&mut a, KeyCode::Char('y'));
    wait_scan(&mut a);
    assert_ne!(fs::read(&e.export).unwrap(), before);
    assert!(
        String::from_utf8(fs::read(&e.export).unwrap())
            .unwrap()
            .contains("new_file")
    );
    assert_eq!(a.mode, Mode::List);
}

#[test]
fn wrapper_browse_existing_and_migrate_legacy() {
    let e = env();
    let mut a = launch(&e, &e.export, None);
    key(&mut a, KeyCode::Enter);
    assert_eq!(a.mode, Mode::List);
    assert!(screen(&mut a).contains("proj/"));

    // Legacy export gets moved into the new location when the user agrees.
    let new = e.root.parent().unwrap().join("moved/dume.json");
    let mut a = launch(&e, &new, Some(e.export.clone()));
    assert!(screen(&mut a).contains("earlier version"));
    key(&mut a, KeyCode::Char('y'));
    assert!(new.exists() && !e.export.exists());
    assert!(screen(&mut a).contains("browse it"), "now an existing scan");
}

#[test]
fn failed_scan_keeps_old_export_and_reports() {
    let e = env();
    let before = fs::read(&e.export).unwrap();
    let mut a = App::launch(tui::Launch {
        scan_root: e.root.join("does-not-exist"),
        ..launch_parts(&e)
    });
    // Different root than the export's -> a problem screen offering a scan, which then fails.
    key(&mut a, KeyCode::Char('y'));
    wait_scan(&mut a);
    assert_eq!(a.mode, Mode::Ask);
    assert!(screen(&mut a).contains("scan failed"));
    assert_eq!(fs::read(&e.export).unwrap(), before, "old export untouched");
}

fn launch_parts(e: &Env) -> tui::Launch {
    tui::Launch {
        view: e.root.clone(),
        scan_root: e.root.clone(),
        export: e.export.clone(),
        legacy_export: None,
        actions: vec![],
        bin: None,
        threads: 2,
        stale_days: 7,
        uid: rustix::process::getuid().as_raw(),
    }
}

// ---- performance regressions ----

#[test]
fn huge_lists_draw_only_what_is_visible() {
    use dumelib::report::{Report, Row};
    const N: u64 = 200_000;
    let dirs: Vec<(String, u64)> = (0..N)
        .map(|i| (format!("/proj/case_{i:06}"), 4096))
        .collect();
    let row = Row {
        pattern: "case_#".into(),
        count: N,
        size: N * 4096,
        example: "proj/case_000000".into(),
        dirs,
    };
    let report = Report {
        total: N * 4096,
        files: 0,
        dirs: N,
        dir_rows: vec![row],
        file_rows: vec![],
        biggest: vec![],
        children: (0..N).map(|i| (4096, format!("sub_{i:06}"))).collect(),
        loose: 0,
    };
    let acts = vec![Action {
        name: "noop".into(),
        patterns: vec!["case_#".into()],
        path: PathBuf::from("/bin/true"),
    }];
    let mut a = App::new(
        report,
        PathBuf::from("/data"),
        PathBuf::from("/x.json"),
        acts,
        2,
    );

    // The Where list (200k subdirectories) is windowed too.
    let t = Instant::now();
    for _ in 0..20 {
        screen(&mut a);
    }
    assert!(
        t.elapsed() < Duration::from_secs(2),
        "where view too slow: {:?}",
        t.elapsed()
    );
    key(&mut a, KeyCode::Char('G'));
    assert!(screen(&mut a).contains("sub_199999"));

    key(&mut a, KeyCode::Tab);
    key(&mut a, KeyCode::Enter);
    let t = Instant::now();
    for _ in 0..20 {
        screen(&mut a);
    }
    // Used to be ~100 ms per frame at this size; the real cost is a few hundred microseconds.
    assert!(
        t.elapsed() < Duration::from_secs(2),
        "matches view too slow: {:?}",
        t.elapsed()
    );

    // Still reaches the end of the list, with the selection scrolled into view.
    key(&mut a, KeyCode::Char('G'));
    let s = screen(&mut a);
    assert!(
        s.contains("case_199999") && !s.contains("case_000000"),
        "{s}"
    );
    key(&mut a, KeyCode::Char('g'));
    assert!(screen(&mut a).contains("case_000000"));

    // Same for the confirm screen.
    key(&mut a, KeyCode::Enter);
    key(&mut a, KeyCode::Enter);
    assert_eq!(a.mode, Mode::Confirm);
    let t = Instant::now();
    for _ in 0..20 {
        screen(&mut a);
    }
    assert!(
        t.elapsed() < Duration::from_secs(2),
        "confirm too slow: {:?}",
        t.elapsed()
    );
    key(&mut a, KeyCode::Char('G'));
    assert!(screen(&mut a).contains("case_199999"));
}

#[test]
fn actions_work_on_the_export_kept_in_memory() {
    let e = env();
    let mut a = launch(&e, &e.export, None);
    key(&mut a, KeyCode::Enter); // browse the existing scan (parsed once, at start-up)
    key(&mut a, KeyCode::Tab);
    key(&mut a, KeyCode::Enter);
    key(&mut a, KeyCode::Enter);
    key(&mut a, KeyCode::Enter);
    key(&mut a, KeyCode::Char('y'));
    wait_done(&mut a);
    assert!(a.failed.is_empty(), "{:?}", a.failed);
    assert!(!e.root.join("proj/case_1/100").exists());
    let on_disk = tui::load_report(&e.export, &e.root).unwrap();
    assert_eq!(on_disk.total, a.report.total, "file and screen agree");

    // A second action straight away reuses the refreshed in-memory export.
    key(&mut a, KeyCode::Char('x'));
    fs::create_dir_all(e.root.join("proj/case_1/300")).unwrap();
    fs::write(e.root.join("proj/case_1/300/f"), vec![1u8; 9000]).unwrap();
    key(&mut a, KeyCode::Enter);
    key(&mut a, KeyCode::Enter);
    key(&mut a, KeyCode::Enter);
    key(&mut a, KeyCode::Enter);
    key(&mut a, KeyCode::Char('y'));
    wait_done(&mut a);
    assert!(!e.root.join("proj/case_1/300").exists());
    assert_eq!(
        tui::load_report(&e.export, &e.root).unwrap().total,
        a.report.total
    );
}

// ---- Where / What ----

#[test]
fn where_drills_into_directories_and_what_is_computed_inside() {
    let e = env();
    let mut a = app(&e);
    let (total, files) = (a.report.total, a.report.files);
    assert_eq!(a.report.children.len(), 1, "data/ has one subdirectory");

    key(&mut a, KeyCode::Enter); // into proj
    assert_eq!(a.root, e.root.join("proj"));
    assert_eq!(a.report.total, total);
    assert!(screen(&mut a).contains("› proj"), "breadcrumb");
    assert!(screen(&mut a).contains("case_1/") && screen(&mut a).contains("case_3/"));

    key(&mut a, KeyCode::Enter); // into case_1
    assert_eq!(a.root, e.root.join("proj/case_1"));
    assert!(a.report.total < total && a.report.files < files);
    assert!(screen(&mut a).contains("› proj › case_1"));
    // Nothing repeats 3+ times inside a single case.
    key(&mut a, KeyCode::Tab);
    assert!(screen(&mut a).contains("Nothing repeats"));

    // Backspace goes back up, always to the Where view, restoring report and selection.
    key(&mut a, KeyCode::Backspace);
    assert_eq!(a.root, e.root.join("proj"));
    assert_eq!(a.selected_in(0), 0);
    assert!(screen(&mut a).contains("case_1/"));
    key(&mut a, KeyCode::Char('h'));
    assert_eq!(a.root, e.root);
    assert_eq!(a.report.total, total);
    key(&mut a, KeyCode::Backspace); // already at the top: no-op
    assert_eq!(a.root, e.root);

    // Esc goes up while there is somewhere to go, and only then quits.
    key(&mut a, KeyCode::Enter);
    key(&mut a, KeyCode::Esc);
    assert_eq!(a.root, e.root);
    assert!(!a.quit);
    key(&mut a, KeyCode::Esc);
    assert!(a.quit);
}

#[test]
fn actions_run_inside_the_chosen_root_and_going_up_is_fresh() {
    let e = env();
    let mut a = app(&e);
    key(&mut a, KeyCode::Enter); // into proj
    let inside = a.report.total;
    key(&mut a, KeyCode::Tab);
    assert!(
        a.report.dir_rows.iter().any(|r| r.pattern == "case_#"),
        "relative to proj"
    );

    key(&mut a, KeyCode::Char('d'));
    key(&mut a, KeyCode::Enter);
    key(&mut a, KeyCode::Enter);
    key(&mut a, KeyCode::Enter);
    key(&mut a, KeyCode::Char('y'));
    wait_done(&mut a);
    assert!(a.failed.is_empty(), "{:?}", a.failed);
    assert!(!e.root.join("proj/case_3/200").exists());
    assert!(a.report.total < inside);
    key(&mut a, KeyCode::Char('x'));

    // Going up afterwards shows fresh sizes, not the ones remembered from before the action.
    key(&mut a, KeyCode::Char('u'));
    assert_eq!(a.root, e.root);
    assert_eq!(
        a.report.total,
        tui::load_report(&e.export, &e.root).unwrap().total
    );
}

#[test]
fn what_lists_dirs_and_files_with_a_filter() {
    let e = env();
    let mut a = app(&e);
    key(&mut a, KeyCode::Char('2'));
    let s = screen(&mut a);
    assert!(
        s.contains("dir") && s.contains("file") && s.contains("case_#"),
        "{s}"
    );
    assert!(
        s.contains("controlDict"),
        "file patterns without extension use the name: {s}"
    );

    key(&mut a, KeyCode::Char('f'));
    let s = screen(&mut a);
    assert!(s.contains("What (files)") && !s.contains("case_#"), "{s}");
    key(&mut a, KeyCode::Char('f')); // toggles back
    assert!(screen(&mut a).contains("case_#"));
    key(&mut a, KeyCode::Char('d'));
    let s = screen(&mut a);
    assert!(
        s.contains("What (dirs)") && s.contains("case_#") && !s.contains("controlDict"),
        "{s}"
    );
}

#[test]
fn tab_selects_matches_and_only_those_are_cleaned() {
    let e = env();
    let mut a = app(&e);
    key(&mut a, KeyCode::Tab);
    key(&mut a, KeyCode::Enter);
    assert_eq!(a.mode, Mode::Matches);
    assert!(screen(&mut a).contains("all will be used"));

    // Tick the first and third match (tab ticks, then moves down).
    key(&mut a, KeyCode::Tab);
    key(&mut a, KeyCode::Down);
    key(&mut a, KeyCode::Tab);
    let s = screen(&mut a);
    assert!(s.contains("2 selected"), "{s}");

    key(&mut a, KeyCode::Char('a'));
    key(&mut a, KeyCode::Enter);
    assert_eq!(a.mode, Mode::Confirm);
    assert!(screen(&mut a).contains("2 directories"));
    key(&mut a, KeyCode::Char('y'));
    wait_done(&mut a);
    assert!(a.failed.is_empty(), "{:?}", a.failed);

    let cleaned = (1..=3)
        .filter(|i| !e.root.join(format!("proj/case_{i}/100")).exists())
        .count();
    assert_eq!(cleaned, 2, "exactly the ticked directories");
    assert!(
        e.root.join("proj/case_2/100").exists(),
        "the unticked one is untouched"
    );
}

#[test]
fn ctrl_a_selects_all_then_none() {
    let e = env();
    let mut a = app(&e);
    key(&mut a, KeyCode::Tab);
    key(&mut a, KeyCode::Enter);
    a.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
    assert!(screen(&mut a).contains("3 selected"));
    a.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
    assert!(screen(&mut a).contains("all will be used"));
    assert_eq!(a.mode, Mode::Matches, "ctrl-a is not the actions key");
}
