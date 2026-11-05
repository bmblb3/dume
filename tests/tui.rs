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

#[test]
fn going_up_from_launch_directory_stops_at_scan_root() {
    let e = env();
    let mut a = App::new(e.db.clone(), e.root.join("proj/case_1"), vec![], 2);
    key(&mut a, KeyCode::Char('h'));
    assert_eq!(a.root, e.root.join("proj"));
    // The child we came from is selected (case_1 is second largest).
    assert_eq!(a.selected(), 1);
    key(&mut a, KeyCode::Left);
    assert_eq!(a.root, e.root);
    key(&mut a, KeyCode::Char('h'));
    assert_eq!(a.root, e.root, "never above the scan root");
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

// ---- clean flow ----

use dumelib::recipe::{self, Recipe};
use dumelib::report::human;
use std::os::unix::fs::PermissionsExt;

/// Recipes from `(file, text)` pairs, loaded the way the app loads them. Scripts end in `.sh`.
fn recipes(e: &Env, files: &[(&str, &str)]) -> Vec<Recipe> {
    let dir = e.root.parent().unwrap().join("actions");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    for (name, text) in files {
        let p = dir.join(name);
        fs::write(&p, text).unwrap();
        let mode = if name.ends_with(".sh") { 0o755 } else { 0o644 };
        fs::set_permissions(&p, fs::Permissions::from_mode(mode)).unwrap();
    }
    let (r, skipped) = recipe::load(&dir).unwrap();
    assert!(skipped.is_empty(), "{skipped:?}");
    r
}

const BIG: (&str, &str) = (
    "big.sql",
    "-- dume-name: Time steps\nSELECT path FROM entries WHERE name REGEXP '^big$'",
);
const LOGS: (&str, &str) = (
    "logs.sql",
    "-- dume-name: Old logs\nSELECT path FROM entries WHERE name REGEXP '\\.log$'",
);
const NONE: (&str, &str) = (
    "none.sql",
    "-- dume-name: Core dumps\nSELECT path FROM entries WHERE name REGEXP '^core$'",
);

fn app_with(e: &Env, here: &str, r: Vec<Recipe>) -> App {
    let here = if here.is_empty() {
        e.root.clone()
    } else {
        e.root.join(here)
    };
    App::new(e.db.clone(), here, r, 2)
}

fn ctrl(a: &mut App, c: char) {
    a.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
}

/// Tick (and draw) until no worker is busy.
fn settle(a: &mut App) -> String {
    let t = Instant::now();
    loop {
        a.tick();
        let s = screen(a);
        if !a.busy() {
            return s;
        }
        assert!(t.elapsed() < Duration::from_secs(20), "stuck:\n{s}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// `c`, then wait for the menu.
fn menu(a: &mut App) -> String {
    key(a, KeyCode::Char('c'));
    assert_eq!(a.mode, Mode::Refreshing);
    let s = settle(a);
    assert_eq!(a.mode, Mode::Menu, "status: {}\n{s}", a.status);
    s
}

fn big_paths(e: &Env) -> Vec<PathBuf> {
    let mut v = vec![];
    for i in 1..=3 {
        for step in ["100", "200"] {
            v.push(e.root.join(format!("proj/case_{i}/{step}/big")));
        }
    }
    v
}

#[test]
fn c_rescans_then_shows_menu_with_counts() {
    let e = env();
    fs::write(e.root.join("proj/case_1/run.log"), vec![1u8; 8192]).unwrap();
    let mut a = app_with(&e, "proj", recipes(&e, &[BIG, LOGS]));
    let s = menu(&mut a);
    assert!(s.contains("Time steps") && s.contains("6 paths"), "{s}");
    assert!(s.contains("Old logs") && s.contains("1 path"), "{s}");
    assert!(s.contains(&human(6 * (64 << 10))), "{s}");
    // The database itself was refreshed, so the Where view sees the new file too.
    assert!(
        Db::open_ro(&e.db)
            .unwrap()
            .children(&e.root.join("proj/case_1"))
            .unwrap()
            .iter()
            .any(|c| c.name == b"run.log")
    );
    key(&mut a, KeyCode::Esc);
    assert_eq!(a.mode, Mode::List);
}

#[test]
fn menu_hides_recipes_with_nothing_to_clean() {
    let e = env();
    let mut a = app_with(&e, "proj", recipes(&e, &[BIG, NONE]));
    let s = menu(&mut a);
    assert!(s.contains("Time steps") && !s.contains("Core dumps"), "{s}");

    let mut a = app_with(&e, "proj", recipes(&e, &[NONE]));
    let s = menu(&mut a);
    assert!(s.contains("Nothing to clean here."), "{s}");
    key(&mut a, KeyCode::Enter);
    assert_eq!(a.mode, Mode::Menu, "nothing to choose");
}

#[test]
fn recipe_error_is_one_line_not_a_crash() {
    let e = env();
    let bad = ("bad.sql", "-- dume-name: Broken\nSELECT nope FROM nowhere");
    let mut a = app_with(&e, "proj", recipes(&e, &[BIG, bad]));
    let s = menu(&mut a);
    assert!(s.contains("recipe Broken failed:"), "{s}");
    assert!(s.contains("Time steps"), "{s}");
}

#[test]
fn paths_start_ticked_untick_and_ctrl_a() {
    let e = env();
    let mut a = app_with(&e, "proj", recipes(&e, &[BIG]));
    menu(&mut a);
    key(&mut a, KeyCode::Enter);
    assert_eq!(a.mode, Mode::Paths);
    let s = screen(&mut a);
    let k = 64 << 10;
    assert!(
        s.contains(&format!("6 of 6 paths ticked, {}", human(6 * k))),
        "{s}"
    );
    assert!(s.contains("case_1/100/big"), "{s}");
    key(&mut a, KeyCode::Char(' '));
    let s = screen(&mut a);
    assert!(
        s.contains(&format!("5 of 6 paths ticked, {}", human(5 * k))),
        "{s}"
    );
    ctrl(&mut a, 'a');
    assert!(screen(&mut a).contains("6 of 6 paths ticked"));
    ctrl(&mut a, 'a');
    assert!(screen(&mut a).contains("0 of 6 paths ticked"));
    key(&mut a, KeyCode::Enter);
    assert_eq!(a.mode, Mode::Paths, "needs a ticked path");
    assert!(screen(&mut a).contains("Tick at least one"));
    key(&mut a, KeyCode::Esc);
    assert_eq!(a.mode, Mode::Menu);
}

#[test]
fn confirm_names_directory_count_and_total_and_warns_at_scan_root() {
    let e = env();
    let mut a = app_with(&e, "proj", recipes(&e, &[BIG]));
    menu(&mut a);
    key(&mut a, KeyCode::Enter);
    key(&mut a, KeyCode::Enter);
    assert_eq!(a.mode, Mode::Confirm);
    let s = screen(&mut a);
    assert!(
        s.contains(&e.root.join("proj").display().to_string()),
        "{s}"
    );
    assert!(s.contains("Time steps"), "{s}");
    assert!(
        s.contains(&format!(
            "Remove 6 paths ({})? This cannot be undone.",
            human(6 * (64 << 10))
        )),
        "{s}"
    );
    assert!(!s.contains("whole scan"), "{s}");
    key(&mut a, KeyCode::Char('n'));
    assert_eq!(a.mode, Mode::Paths);
    assert!(big_paths(&e).iter().all(|p| p.exists()));

    let mut a = app_with(&e, "", recipes(&e, &[BIG]));
    menu(&mut a);
    key(&mut a, KeyCode::Enter);
    key(&mut a, KeyCode::Enter);
    let s = screen(&mut a);
    assert!(
        s.contains(
            "This is the top folder of the whole scan: everything ticked below it will be changed."
        ),
        "{s}"
    );
    assert!(!s.contains("scan root"), "no jargon: {s}");
    key(&mut a, KeyCode::Esc);
    assert_eq!(a.mode, Mode::Paths);
}

#[test]
fn only_ticked_paths_are_removed_and_db_updated() {
    let e = env();
    let mut a = app_with(&e, "proj", recipes(&e, &[BIG]));
    let before = Db::open_ro(&e.db)
        .unwrap()
        .total_under(&e.root.join("proj"))
        .unwrap();
    menu(&mut a);
    key(&mut a, KeyCode::Enter);
    key(&mut a, KeyCode::Char(' ')); // untick case_1/100/big
    key(&mut a, KeyCode::Enter);
    key(&mut a, KeyCode::Char('y'));
    assert_eq!(a.mode, Mode::Running);
    let s = settle(&mut a);
    assert_eq!(a.mode, Mode::Done, "{s}");
    let paths = big_paths(&e);
    assert!(paths[0].exists(), "unticked path survives");
    assert!(paths[1..].iter().all(|p| !p.exists()));
    assert!(
        s.contains(&format!("Freed {}", human(5 * (64 << 10)))),
        "{s}"
    );
    assert!(s.contains("5 done"), "{s}");

    let d = Db::open_ro(&e.db).unwrap();
    let has = |dir: &str| {
        d.children(&e.root.join(dir))
            .unwrap()
            .iter()
            .any(|c| c.name == b"big")
    };
    assert!(has("proj/case_1/100"));
    assert!(!has("proj/case_1/200") && !has("proj/case_2/100"));
    assert_eq!(
        d.total_under(&e.root.join("proj")).unwrap(),
        before - 5 * (64 << 10)
    );

    key(&mut a, KeyCode::Char('x'));
    assert_eq!(a.mode, Mode::List);
    assert_eq!(a.root, e.root.join("proj"));
}

#[test]
fn script_recipe_runs_per_ticked_file() {
    let e = env();
    let log = e.root.parent().unwrap().join("ran.txt");
    let sh = format!("#!/bin/sh\necho \"$1\" >> '{}'\n", log.display());
    let r = recipes(
        &e,
        &[
            ("touch.sh", &sh),
            (
                "s.sql",
                "-- dume-name: Compress\n-- dume-apply: touch.sh\nSELECT path FROM entries WHERE name REGEXP '^big$'",
            ),
        ],
    );
    let mut a = app_with(&e, "proj", r);
    let s = menu(&mut a);
    assert!(s.contains("up to"), "{s}");
    key(&mut a, KeyCode::Enter);
    assert!(screen(&mut a).contains("up to"));
    key(&mut a, KeyCode::Down);
    key(&mut a, KeyCode::Char(' ')); // untick case_1/200/big
    key(&mut a, KeyCode::Enter);
    let s = screen(&mut a);
    assert!(
        s.contains("Run touch.sh on 5 files? This cannot be undone."),
        "{s}"
    );
    key(&mut a, KeyCode::Char('y'));
    let s = settle(&mut a);
    assert_eq!(a.mode, Mode::Done, "{s}");
    let ran = fs::read_to_string(&log).unwrap();
    let mut want: Vec<_> = big_paths(&e)
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    want.remove(1);
    assert_eq!(ran.lines().collect::<Vec<_>>(), want);
    // The files are still there and the honest result says nothing shrank.
    assert!(big_paths(&e).iter().all(|p| p.exists()));
    assert!(s.contains("did not shrink"), "{s}");
}

#[test]
fn esc_during_refresh_cancels_and_keeps_db() {
    let e = env();
    let mut a = app_with(&e, "proj", recipes(&e, &[BIG]));
    fs::write(e.root.join("proj/late.txt"), b"late").unwrap();
    key(&mut a, KeyCode::Char('c'));
    assert_eq!(a.mode, Mode::Refreshing);
    key(&mut a, KeyCode::Esc);
    assert_eq!(a.mode, Mode::List);
    assert_eq!(a.root, e.root.join("proj"));
    assert!(screen(&mut a).contains("case_2/"));
    // The background refresh finishes and is applied silently, without leaving Where.
    let t = Instant::now();
    while !screen(&mut a).contains("late.txt") {
        assert!(
            t.elapsed() < Duration::from_secs(20),
            "refresh never applied"
        );
        a.tick();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(a.mode, Mode::List);
    assert!(big_paths(&e).iter().all(|p| p.exists()));
    assert!(screen(&mut a).contains("case_2/"));

    // Another writer holds the lock: a plain message, back in Where.
    let lock = db::lock_writer(&e.db).unwrap();
    key(&mut a, KeyCode::Char('c'));
    let s = settle(&mut a);
    assert_eq!(a.mode, Mode::List, "{s}");
    assert!(
        a.status.contains("another dume is updating this scan"),
        "{}",
        a.status
    );
    drop(lock);
    // A child forked meanwhile by another test thread can hold a copy of the lock's fd until
    // it execs; wait until the lock is really free.
    let t = Instant::now();
    while db::lock_writer(&e.db).is_err() {
        assert!(t.elapsed() < Duration::from_secs(20), "lock never released");
        std::thread::sleep(Duration::from_millis(5));
    }
    menu(&mut a);
}

#[test]
fn deleted_here_returns_to_where_with_message() {
    let e = env();
    let mut a = app_with(&e, "proj", recipes(&e, &[BIG]));
    key(&mut a, KeyCode::Char('G')); // notes.txt
    key(&mut a, KeyCode::Up); // case_3
    key(&mut a, KeyCode::Enter);
    assert_eq!(a.root, e.root.join("proj/case_3"));
    fs::remove_dir_all(e.root.join("proj/case_3")).unwrap();
    key(&mut a, KeyCode::Char('c'));
    let s = settle(&mut a);
    assert_eq!(a.mode, Mode::List, "{s}");
    assert_eq!(a.root, e.root.join("proj"));
    assert!(a.status.contains("no longer exists"), "{}", a.status);
    assert!(!s.contains("case_3/"), "db refreshed: {s}");

    // Replaced by a file: the same.
    key(&mut a, KeyCode::Home);
    key(&mut a, KeyCode::Enter);
    let here = a.root.clone();
    assert_ne!(here, e.root.join("proj"));
    fs::remove_dir_all(&here).unwrap();
    fs::write(&here, "now a file").unwrap();
    key(&mut a, KeyCode::Char('c'));
    settle(&mut a);
    assert_eq!(a.mode, Mode::List);
    assert_eq!(a.root, e.root.join("proj"));
    assert!(a.status.contains("no longer exists"), "{}", a.status);
}

#[test]
fn footer_shows_keys_for_each_mode() {
    let e = env();
    let mut a = app_with(&e, "proj", recipes(&e, &[BIG]));
    let s = screen(&mut a);
    for k in [
        "↑↓ move",
        "enter open",
        "← back",
        "c clean this folder",
        "N ncdu",
        "q quit",
    ] {
        assert!(s.contains(k), "Where footer lacks {k}: {s}");
    }
    key(&mut a, KeyCode::Char('c'));
    assert!(screen(&mut a).contains("esc back"));
    let s = settle(&mut a);
    assert!(s.contains("enter choose") && s.contains("esc back"), "{s}");
    key(&mut a, KeyCode::Enter);
    let s = screen(&mut a);
    for k in [
        "space tick/untick",
        "ctrl-a all/none",
        "enter continue",
        "esc back",
    ] {
        assert!(s.contains(k), "Paths footer lacks {k}: {s}");
    }
    key(&mut a, KeyCode::Enter);
    let s = screen(&mut a);
    assert!(
        s.contains("y yes, clean") && s.contains("n/esc back"),
        "{s}"
    );
    key(&mut a, KeyCode::Char('y'));
    assert!(screen(&mut a).contains("esc stop after the current path"));
    settle(&mut a);
    assert!(screen(&mut a).contains("any key"));
}

#[test]
fn huge_selection_draws_only_visible_paths() {
    const N: usize = 20_000;
    let e = env();
    let many = e.root.join("many");
    fs::create_dir(&many).unwrap();
    for i in 0..N {
        fs::write(many.join(format!("f_{i:06}")), b"x").unwrap();
    }
    let r = recipes(
        &e,
        &[(
            "f.sql",
            "-- dume-name: All f\nSELECT path FROM entries WHERE name REGEXP '^f_'",
        )],
    );
    let mut a = app_with(&e, "many", r);
    let s = menu(&mut a);
    assert!(s.contains(&format!("{N} paths")), "{s}");
    key(&mut a, KeyCode::Enter);
    let t = Instant::now();
    for _ in 0..50 {
        screen(&mut a);
    }
    assert!(
        t.elapsed() < Duration::from_secs(2),
        "paths view too slow: {:?}",
        t.elapsed()
    );
    key(&mut a, KeyCode::End);
    let s = screen(&mut a);
    assert!(s.contains("f_019999") && !s.contains("f_000000"), "{s}");
}

/// A recipe that counts to `n` in SQL and then returns nothing, so it takes a while.
fn slow(n: u64) -> String {
    format!(
        "-- dume-name: Zz slow\nWITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i < {n}) \
         SELECT CAST('none' AS BLOB) AS path FROM n WHERE i < 0"
    )
}

#[test]
fn cannot_start_a_clean_while_recipes_are_still_being_checked() {
    let e = env();
    // About 5 s unoptimised here; the menu shows "Time steps" within milliseconds of it starting,
    // and recipe results only change inside `tick`, so `busy()` below is exact.
    let sl = slow(6_000_000);
    let mut a = app_with(&e, "proj", recipes(&e, &[BIG, ("slow.sql", &sl)]));
    key(&mut a, KeyCode::Char('c'));
    // Wait for the first recipe while the slow one is still running.
    let t = Instant::now();
    while !(a.mode == Mode::Menu && screen(&mut a).contains("Time steps")) {
        assert!(t.elapsed() < Duration::from_secs(20), "no menu");
        a.tick();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(a.busy(), "the slow recipe finished too soon for this test");
    key(&mut a, KeyCode::Enter);
    assert_eq!(a.mode, Mode::Menu, "no choosing while recipes are checked");
    let s = screen(&mut a);
    assert!(s.contains("Still checking recipes — one moment…"), "{s}");
    settle(&mut a);
    assert_eq!(a.mode, Mode::Menu);
    key(&mut a, KeyCode::Enter);
    assert_eq!(a.mode, Mode::Paths);
    key(&mut a, KeyCode::Enter);
    key(&mut a, KeyCode::Char('y'));
    let s = settle(&mut a);
    assert_eq!(a.mode, Mode::Done, "{s}");
    assert!(
        s.contains(&format!("Freed {}", human(6 * (64 << 10)))),
        "{s}"
    );
    assert!(big_paths(&e).iter().all(|p| !p.exists()));
    let d = Db::open_ro(&e.db).unwrap();
    assert!(
        d.children(&e.root.join("proj/case_1/100"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn failed_rescan_after_clean_says_it_could_not_measure() {
    let e = env();
    let mut a = app_with(&e, "proj", recipes(&e, &[BIG]));
    menu(&mut a);
    key(&mut a, KeyCode::Enter);
    key(&mut a, KeyCode::Enter);
    let lock = db::lock_writer(&e.db).unwrap();
    key(&mut a, KeyCode::Char('y'));
    let s = settle(&mut a);
    drop(lock);
    assert_eq!(a.mode, Mode::Done, "{s}");
    assert!(
        big_paths(&e).iter().all(|p| !p.exists()),
        "files were removed"
    );
    assert!(
        s.contains("dume could not measure how much space was freed."),
        "{s}"
    );
    assert!(s.contains("another dume is updating this scan"), "{s}");
    assert!(s.contains("Press c again or rescan to refresh."), "{s}");
    assert!(!s.contains("did not shrink") && !s.contains("Freed"), "{s}");
}

#[test]
fn esc_stops_the_run_and_says_what_was_not_touched() {
    let e = env();
    let base = e.root.parent().unwrap().to_path_buf();
    let (started, gate) = (base.join("started"), base.join("gate"));
    // Each run says it started, then waits until the test opens the gate.
    let sh = format!(
        "#!/bin/sh\ntouch '{}'\nwhile [ ! -e '{}' ]; do sleep 0.01; done\n",
        started.display(),
        gate.display()
    );
    let r = recipes(
        &e,
        &[
            ("gate.sh", &sh),
            (
                "s.sql",
                "-- dume-name: Gate\n-- dume-apply: gate.sh\nSELECT path FROM entries WHERE name REGEXP '^big$'",
            ),
        ],
    );
    let mut a = app_with(&e, "proj", r);
    menu(&mut a);
    key(&mut a, KeyCode::Enter);
    key(&mut a, KeyCode::Enter);
    key(&mut a, KeyCode::Char('y'));
    // The first path is running (held at the gate) when esc is pressed.
    let t = Instant::now();
    while !started.exists() {
        assert!(
            t.elapsed() < Duration::from_secs(20),
            "first script never ran"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    key(&mut a, KeyCode::Esc);
    fs::write(&gate, "").unwrap();
    let s = settle(&mut a);
    assert_eq!(a.mode, Mode::Done, "{s}");
    assert!(
        s.contains("You stopped the run: 5 paths were not touched."),
        "{s}"
    );
    assert!(s.contains("1 done · 5 skipped"), "{s}");
    assert!(
        pos(&s, "You stopped the run") < pos(&s, "did not shrink"),
        "headline first: {s}"
    );
}
