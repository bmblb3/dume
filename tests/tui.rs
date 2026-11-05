use dumelib::db::{self, Db};
use dumelib::scan;
use dumelib::tui::{self, App, Mode};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

struct Env {
    _t: tempfile::TempDir,
    root: PathBuf,
    db: PathBuf,
}

fn uid() -> u32 {
    rustix::process::getuid().as_raw()
}

/// data/proj/case_{1,2,3} with time steps; case_2 is the biggest; proj/notes.txt is a loose file.
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
    fs::write(root.join("proj/case_2/extra"), vec![1u8; 256 << 10]).unwrap();
    fs::write(root.join("proj/notes.txt"), b"n").unwrap();
    let db = base.join("dume.db");
    db::scan_to_db(&root, &db, uid(), 2, Default::default()).unwrap();
    Env { _t: t, root, db }
}

fn app(e: &Env) -> App {
    App::new(e.db.clone(), e.root.clone(), vec![], 2)
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

fn launch_parts(e: &Env) -> tui::Launch {
    tui::Launch {
        view: e.root.clone(),
        scan_root: e.root.clone(),
        db: e.db.clone(),
        legacy_export: None,
        recipes: vec![],
        bin: None,
        threads: 2,
        stale_days: 7,
        uid: uid(),
    }
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

fn pos(s: &str, needle: &str) -> usize {
    s.find(needle)
        .unwrap_or_else(|| panic!("{needle} not on screen:\n{s}"))
}

// ---- Where ----

#[test]
fn starts_in_where_listing_children_by_size() {
    let e = env();
    let mut a = App::new(e.db.clone(), e.root.join("proj"), vec![], 2);
    assert_eq!(a.mode, Mode::List);
    let s = screen(&mut a);
    // Largest first, ties by name; files are listed too, without a slash.
    assert!(pos(&s, "case_2/") < pos(&s, "case_1/"), "{s}");
    assert!(pos(&s, "case_1/") < pos(&s, "case_3/"), "{s}");
    assert!(pos(&s, "case_3/") < pos(&s, "notes.txt"), "{s}");
    assert!(!s.contains("notes.txt/"), "{s}");
    // Percent of the current directory's total.
    let d = Db::open_ro(&e.db).unwrap();
    let here = e.root.join("proj");
    let big = d.children(&here).unwrap()[0].size;
    let pct = format!(
        "{:.1}%",
        100.0 * big as f64 / d.total_under(&here).unwrap() as f64
    );
    assert!(s.contains(&pct), "{pct}: {s}");
    assert!(s.contains("› proj"), "breadcrumb: {s}");
    assert!(s.contains("N ncdu") && s.contains("q quit"), "footer: {s}");
}

#[test]
fn where_drills_in_and_up() {
    let e = env();
    let mut a = app(&e);
    assert!(screen(&mut a).contains("proj/"));
    key(&mut a, KeyCode::Enter); // into proj
    assert_eq!(a.root, e.root.join("proj"));
    key(&mut a, KeyCode::Char('j'));
    key(&mut a, KeyCode::Right); // into case_1 (second largest)
    assert_eq!(a.root, e.root.join("proj/case_1"));
    let s = screen(&mut a);
    assert!(
        s.contains("› proj › case_1") && s.contains("system/"),
        "{s}"
    );

    // Going up restores the selection.
    key(&mut a, KeyCode::Backspace);
    assert_eq!(a.root, e.root.join("proj"));
    assert_eq!(a.selected(), 1);
    // A file can't be opened.
    key(&mut a, KeyCode::Char('G'));
    key(&mut a, KeyCode::Char('l'));
    assert_eq!(a.root, e.root.join("proj"));
    key(&mut a, KeyCode::Left);
    assert_eq!(a.root, e.root);
    key(&mut a, KeyCode::Char('h')); // already at the top: no-op
    assert_eq!(a.root, e.root);

    // Esc goes up while there is somewhere to go, and only then quits.
    key(&mut a, KeyCode::Enter);
    key(&mut a, KeyCode::Esc);
    assert_eq!(a.root, e.root);
    assert!(!a.quit);
    key(&mut a, KeyCode::Esc);
    assert!(a.quit);
}

// ---- start-up ----

#[test]
fn startup_decisions() {
    use dumelib::tui::{Ask, startup};
    use std::time::SystemTime;
    let e = env();
    let now = SystemTime::now();
    let missing = e.root.parent().unwrap().join("state/dume.db");
    let legacy = e.root.parent().unwrap().join("dume.json");
    fs::write(&legacy, "[]").unwrap();

    assert_eq!(startup(&missing, &e.root, uid(), None, now), Ask::NoExport);
    // A legacy export is offered for import only when the database is absent.
    assert_eq!(
        startup(&missing, &e.root, uid(), Some(&legacy), now),
        Ask::Migrate(legacy.clone())
    );
    assert!(matches!(
        startup(&e.db, &e.root, uid(), Some(&legacy), now),
        Ask::Existing { .. }
    ));
    let Ask::Existing { age } = startup(
        &e.db,
        &e.root,
        uid(),
        None,
        now + Duration::from_secs(3 * 86400),
    ) else {
        panic!()
    };
    assert_eq!(tui::age_text(age), "3 days");

    // Wrong user, wrong root, unreadable file -> a problem, never a crash.
    assert!(
        matches!(startup(&e.db, &e.root, uid() + 1, None, now), Ask::Problem(m) if m.contains("another user"))
    );
    let other = e.root.join("proj");
    assert!(
        matches!(startup(&e.db, &other, uid(), None, now), Ask::Problem(m) if m.contains("team root"))
    );
    let junk = e.root.parent().unwrap().join("junk.db");
    fs::write(&junk, "nope").unwrap();
    assert!(
        matches!(startup(&junk, &e.root, uid(), None, now), Ask::Problem(m) if m.contains("can't be read"))
    );
}

#[test]
fn first_run_asks_then_scans_into_db() {
    let e = env();
    let state = e.root.parent().unwrap().join("state/dume/dume.db");
    let mut a = App::launch(tui::Launch {
        db: state.clone(),
        ..launch_parts(&e)
    });
    assert_eq!(a.mode, Mode::Ask);
    assert!(screen(&mut a).contains("no scan"));

    key(&mut a, KeyCode::Char('y'));
    assert_eq!(a.mode, Mode::Scanning);
    assert!(screen(&mut a).contains("Scanning"));
    wait_scan(&mut a);
    assert_eq!(a.mode, Mode::List);
    assert!(state.exists(), "database written, parent dirs created");
    assert!(!state.with_file_name(".dume.db.tmp").exists());
    assert!(screen(&mut a).contains("proj/"));
    assert_eq!(Db::open_ro(&state).unwrap().meta().unwrap().uid, uid());
}

#[test]
fn existing_scan_browse_or_confirmed_rescan() {
    let e = env();
    let mut a = App::launch(launch_parts(&e));
    assert!(screen(&mut a).contains("just now"));

    // 'r' asks again; 'n' goes back without touching the file.
    let before = fs::read(&e.db).unwrap();
    key(&mut a, KeyCode::Char('r'));
    assert!(screen(&mut a).contains("overwrite"));
    key(&mut a, KeyCode::Char('n'));
    assert!(screen(&mut a).contains("browse it"));
    assert_eq!(fs::read(&e.db).unwrap(), before);
    key(&mut a, KeyCode::Enter);
    assert_eq!(a.mode, Mode::List);
    let s = screen(&mut a);
    assert!(s.contains("proj/") && !s.contains("new_dir/"), "{s}");

    // A confirmed rescan replaces the database, and the app reads the new one.
    fs::create_dir(e.root.join("new_dir")).unwrap();
    fs::write(e.root.join("new_dir/f"), vec![1u8; 9000]).unwrap();
    let mut a = App::launch(launch_parts(&e));
    key(&mut a, KeyCode::Char('r'));
    key(&mut a, KeyCode::Char('y'));
    wait_scan(&mut a);
    assert_eq!(a.mode, Mode::List);
    assert_ne!(fs::read(&e.db).unwrap(), before);
    assert!(screen(&mut a).contains("new_dir/"));
}

#[test]
fn legacy_json_offers_import() {
    let e = env();
    let base = e.root.parent().unwrap();
    let legacy = base.join("dume.json");
    let (s, dev) = scan::scan(&e.root, uid(), 2, false).unwrap();
    let mut f = fs::File::create(&legacy).unwrap();
    s.write(&mut f, e.root.to_str().unwrap(), dev, uid(), "test")
        .unwrap();
    let new = base.join("state/dume/dume.db");
    let launch = |legacy| {
        App::launch(tui::Launch {
            db: new.clone(),
            legacy_export: legacy,
            ..launch_parts(&e)
        })
    };

    // 'n' scans fresh instead.
    let mut a = launch(Some(legacy.clone()));
    assert!(screen(&mut a).contains("earlier version"));
    key(&mut a, KeyCode::Char('n'));
    assert_eq!(a.mode, Mode::Scanning);
    wait_scan(&mut a);
    assert_eq!(a.mode, Mode::List);
    fs::remove_file(&new).unwrap();

    // 'y' imports it; the old file is kept.
    let mut a = launch(Some(legacy.clone()));
    key(&mut a, KeyCode::Char('y'));
    assert!(new.exists() && legacy.exists());
    assert!(screen(&mut a).contains("browse it"), "now an existing scan");
    key(&mut a, KeyCode::Enter);
    assert_eq!(a.mode, Mode::List);
    assert!(screen(&mut a).contains("proj/"));

    // An unreadable legacy file is a problem screen offering a scan.
    fs::remove_file(&new).unwrap();
    fs::write(&legacy, "nope").unwrap();
    let mut a = launch(Some(legacy.clone()));
    key(&mut a, KeyCode::Char('y'));
    assert_eq!(a.mode, Mode::Ask);
    assert!(screen(&mut a).contains("Could not import"));
    assert!(!new.exists());
}

#[test]
fn failed_scan_keeps_old_db() {
    let e = env();
    let before = fs::read(&e.db).unwrap();
    let mut a = App::launch(tui::Launch {
        scan_root: e.root.join("does-not-exist"),
        ..launch_parts(&e)
    });
    // Different root than the database's -> a problem screen offering a scan, which then fails.
    key(&mut a, KeyCode::Char('y'));
    wait_scan(&mut a);
    assert_eq!(a.mode, Mode::Ask);
    assert!(screen(&mut a).contains("scan failed"));
    assert_eq!(fs::read(&e.db).unwrap(), before, "old database untouched");
    assert!(!e.db.with_file_name(".dume.db.tmp").exists());
}

// ---- performance regressions ----

#[test]
fn huge_lists_draw_only_what_is_visible() {
    use std::fmt::Write;
    const N: u64 = 200_000;
    let t = tempfile::tempdir().unwrap();
    let json = t.path().join("big.json");
    let mut s = format!(
        r#"[1,2,{{"progname":"dume","uid":{}}},[{{"name":"/data"}}"#,
        uid()
    );
    for i in 0..N {
        write!(
            s,
            r#",[{{"name":"sub_{i:06}"}},{{"name":"f","asize":4096,"dsize":4096,"ino":{i}}}]"#
        )
        .unwrap();
    }
    s.push_str("]]");
    fs::write(&json, s).unwrap();
    let dbf = t.path().join("big.db");
    db::import_json(&json, &dbf).unwrap();
    let mut a = App::new(dbf, PathBuf::from("/data"), vec![], 2);

    let t = Instant::now();
    for _ in 0..20 {
        screen(&mut a);
    }
    assert!(
        t.elapsed() < Duration::from_secs(2),
        "where view too slow: {:?}",
        t.elapsed()
    );
    // Still reaches the end of the list, with the selection scrolled into view.
    key(&mut a, KeyCode::Char('G'));
    let s = screen(&mut a);
    assert!(s.contains("sub_199999") && !s.contains("sub_000000"), "{s}");
    key(&mut a, KeyCode::Char('g'));
    assert!(screen(&mut a).contains("sub_000000"));
}
