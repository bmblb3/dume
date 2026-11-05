//! Terminal UI: browse your scan database directory by directory.
//!
//! The state machine (`App::on_key` / `App::tick`) is separate from drawing so it can be tested.

use crate::apply::{self, Verdict};
use crate::db::{self, Child, Db};
use crate::recipe::{self, Apply, Recipe, Selection};
use crate::report::{human, pct};
use crate::scan::Progress;
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::Line;
use ratatui::widgets::{Block, Paragraph, Row as TRow, Table, TableState, Wrap};
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant, SystemTime};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// A question at start-up (see [`Ask`]).
    Ask,
    Scanning,
    /// The Where view: the current directory's children, largest first.
    List,
    /// After `c`: rescanning the current directory before choosing what to clean.
    Refreshing,
    /// The recipes that found something here.
    Menu,
    /// The paths the chosen recipe found, to tick or untick.
    Paths,
    Confirm,
    Running,
    Done,
}

enum Msg {
    /// A full scan finished, with the number of files kept.
    Scanned(io::Result<u64>),
    /// The current directory was rescanned (`c`).
    Refreshed(io::Result<db::Refreshed>),
    /// Recipe `i` ran on the refreshed database.
    Picked(usize, Result<Selection, String>),
    /// `done` of `total` paths handled; `path` was the last one.
    Step(usize, usize, String),
    /// The run finished: paths done, those that were not (with why), and the rescan after.
    Applied(usize, Vec<(Vec<u8>, Verdict)>, io::Result<db::Refreshed>),
}

/// The result of a run, for the Done screen.
struct Outcome {
    done: usize,
    problems: Vec<(Vec<u8>, Verdict)>,
    rescan: Option<String>,
    before: u64,
    after: u64,
}

const SPIN: [&str; 4] = ["|", "/", "-", "\\"];

fn n_paths(n: usize) -> String {
    plural(n, "path")
}

fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

/// "X", or "up to X" when a script decides how much it frees.
fn amount(apply: &Apply, size: u64) -> String {
    match apply {
        Apply::Delete => human(size),
        Apply::Script(_) => format!("up to {}", human(size)),
    }
}

fn is_dir(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())
}

/// What the start-up screen asks the user.
#[derive(Clone, Debug, PartialEq)]
pub enum Ask {
    /// An export from an older version sits at this path (and there is no database).
    Migrate(PathBuf),
    /// No scan database yet.
    NoExport,
    /// The saved database can't be used, for this reason.
    Problem(String),
    /// A usable database exists.
    Existing { age: Duration },
    /// Confirm a full rescan that replaces the database.
    Rescan,
}

/// Decide what to ask on start-up. Pure apart from reading the database, so it can be tested.
pub fn startup(
    db: &Path,
    scan_root: &Path,
    uid: u32,
    legacy: Option<&Path>,
    now: SystemTime,
) -> Ask {
    if !db.exists() {
        return match legacy {
            Some(old) if old.is_file() => Ask::Migrate(old.to_path_buf()),
            _ => Ask::NoExport,
        };
    }
    let m = match Db::open_ro(db).and_then(|d| d.meta()) {
        Ok(m) => m,
        Err(e) => return Ask::Problem(format!("Your saved scan can't be read ({e}).")),
    };
    if m.uid != uid {
        return Ask::Problem(
            "Your saved scan was made by an older version or for another user.".into(),
        );
    }
    if m.root.canonicalize().ok().as_deref() != Some(scan_root) {
        return Ask::Problem(format!(
            "Your saved scan is for {}, but the team root is {}.",
            m.root.display(),
            scan_root.display()
        ));
    }
    let scanned = SystemTime::UNIX_EPOCH + Duration::from_secs(m.scanned);
    Ask::Existing {
        age: now.duration_since(scanned).unwrap_or_default(),
    }
}

/// "just now", "5 minutes", "3 hours", "2 days".
pub fn age_text(d: Duration) -> String {
    let (s, plural) = (d.as_secs(), |n: u64, u: &str| {
        format!("{n} {u}{}", if n == 1 { "" } else { "s" })
    });
    match s {
        0..60 => "just now".into(),
        60..3600 => plural(s / 60, "minute"),
        3600..86400 => plural(s / 3600, "hour"),
        _ => plural(s / 86400, "day"),
    }
}

/// First visible row so that `sel` is on screen, moving `off` as little as possible.
fn window(off: usize, sel: usize, height: usize, len: usize) -> usize {
    if height == 0 {
        return 0;
    }
    let off = off.min(len.saturating_sub(1));
    if sel < off {
        sel
    } else if sel >= off + height {
        sel + 1 - height
    } else {
        off
    }
}

/// Everything the wrapper needs to start.
pub struct Launch {
    pub view: PathBuf,
    pub scan_root: PathBuf,
    pub db: PathBuf,
    pub legacy_export: Option<PathBuf>,
    pub recipes: Vec<Recipe>,
    pub bin: Option<PathBuf>,
    pub threads: usize,
    pub stale_days: u64,
    pub uid: u32,
}

pub struct App {
    pub mode: Mode,
    pub quit: bool,
    pub status: String,
    /// Set by `N`: the binary should suspend the UI and run ncdu on the database.
    pub want_ncdu: bool,
    /// The directory the Where view shows.
    pub root: PathBuf,
    db_path: PathBuf,
    /// Reopened after every scan: a scan renames a new file over the old one.
    db: Option<Db>,
    recipes: Vec<Recipe>,
    bin: Option<PathBuf>,
    threads: usize,
    scan_root: PathBuf,
    uid: u32,
    legacy: Option<PathBuf>,
    stale_days: u64,
    ask: Ask,
    progress: Arc<Progress>,
    scan_started: Instant,
    /// Children of `root`, largest first, and the total under it.
    children: Vec<Child>,
    total: u64,
    sel: usize,
    /// First visible row of the list.
    offset: usize,
    /// Directories we came from (innermost last), with the selection to return to.
    stack: Vec<(PathBuf, usize)>,
    rx: Option<Receiver<Msg>>,
    /// Clean flow: recipe results in recipe order, from the database refreshed for `root`.
    picks: Vec<(usize, Result<Selection, String>)>,
    /// Menu cursor, an index into [`App::shown`].
    msel: usize,
    /// The chosen entry of `picks`, and which of its hits are ticked.
    pick: usize,
    ticks: Vec<bool>,
    /// Count and size of the ticked hits, kept in step with `ticks`.
    ticked: (usize, u64),
    psel: usize,
    poff: usize,
    cancel: Arc<AtomicBool>,
    /// Running: (done, total, last path).
    step: (usize, usize, String),
    outcome: Option<Outcome>,
}

impl App {
    fn blank(db: PathBuf, root: PathBuf, recipes: Vec<Recipe>, threads: usize) -> Self {
        Self {
            mode: Mode::List,
            quit: false,
            status: String::new(),
            want_ncdu: false,
            scan_root: root.clone(),
            root,
            db_path: db,
            db: None,
            recipes,
            bin: None,
            threads,
            uid: rustix::process::getuid().as_raw(),
            legacy: None,
            stale_days: 7,
            ask: Ask::NoExport,
            progress: Arc::default(),
            scan_started: Instant::now(),
            children: Vec::new(),
            total: 0,
            sel: 0,
            offset: 0,
            stack: Vec::new(),
            rx: None,
            picks: Vec::new(),
            msel: 0,
            pick: 0,
            ticks: Vec::new(),
            ticked: (0, 0),
            psel: 0,
            poff: 0,
            cancel: Arc::default(),
            step: (0, 0, String::new()),
            outcome: None,
        }
    }

    /// Browse the database at `db`, starting in `root`.
    pub fn new(db: PathBuf, root: PathBuf, recipes: Vec<Recipe>, threads: usize) -> Self {
        let mut app = Self::blank(db, root, recipes, threads);
        app.reopen();
        if let Some(m) = app.db.as_ref().and_then(|d| d.meta().ok()) {
            app.scan_root = m.root;
        }
        app
    }

    /// The wrapper: start from nothing and ask what to do about the saved scan.
    pub fn launch(l: Launch) -> Self {
        let mut app = Self::blank(l.db, l.view, l.recipes, l.threads);
        app.scan_root = l.scan_root;
        app.uid = l.uid;
        app.bin = l.bin;
        app.legacy = l.legacy_export;
        app.stale_days = l.stale_days;
        app.evaluate();
        app
    }

    /// The open database, if any (for `N`).
    pub fn db(&self) -> Option<&Db> {
        self.db.as_ref()
    }

    pub fn selected(&self) -> usize {
        self.sel
    }

    /// Open the database file afresh and reload the current directory.
    fn reopen(&mut self) {
        self.db = None;
        match Db::open_ro(&self.db_path) {
            Ok(d) => {
                self.db = Some(d);
                self.load();
            }
            Err(e) => self.status = format!("cannot read {}: {e}", self.db_path.display()),
        }
    }

    /// Reload `root`'s children and total; keeps the selection in range.
    fn load(&mut self) {
        let Some(db) = &self.db else { return };
        match db
            .children(&self.root)
            .and_then(|c| Ok((c, db.total_under(&self.root)?)))
        {
            Ok((c, t)) => (self.children, self.total) = (c, t),
            Err(e) => {
                (self.children, self.total) = (Vec::new(), 0);
                self.status = format!("{}: {e}", self.root.display());
            }
        }
        self.sel = self.sel.min(self.children.len().saturating_sub(1));
    }

    fn evaluate(&mut self) {
        self.ask = startup(
            &self.db_path,
            &self.scan_root,
            self.uid,
            self.legacy.as_deref(),
            SystemTime::now(),
        );
        self.mode = Mode::Ask;
    }

    fn start_scan(&mut self) {
        self.progress = Arc::default();
        self.scan_started = Instant::now();
        self.mode = Mode::Scanning;
        let (root, db, uid, threads) = (
            self.scan_root.clone(),
            self.db_path.clone(),
            self.uid,
            self.threads,
        );
        let progress = self.progress.clone();
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        std::thread::spawn(move || {
            let res = db
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| db::scan_to_db(&root, &db, uid, threads, progress));
            let _ = tx.send(Msg::Scanned(res));
        });
    }

    /// Remove the half-written scan file if we quit mid-scan.
    pub fn cleanup(&self) {
        if self.mode == Mode::Scanning {
            let _ = std::fs::remove_file(self.db_path.with_file_name(".dume.db.tmp"));
        }
    }

    /// Leave the start-up screen for the Where view.
    fn browse(&mut self) {
        self.reopen();
        match self.db {
            Some(_) => self.mode = Mode::List,
            None => {
                self.ask =
                    Ask::Problem(format!("Your saved scan can't be read ({}).", self.status));
                self.status.clear();
                self.mode = Mode::Ask;
            }
        }
    }

    fn on_key_ask(&mut self, code: KeyCode) {
        let yes = matches!(code, KeyCode::Char('y' | 'Y'));
        let quit = matches!(code, KeyCode::Char('q' | 'Q') | KeyCode::Esc);
        match self.ask.clone() {
            Ask::Migrate(old) => {
                if yes {
                    // shortcut: imports on the UI thread, so a huge export freezes the screen
                    // for a few seconds; move to the worker if that bites.
                    let res = self
                        .db_path
                        .parent()
                        .map_or(Ok(()), std::fs::create_dir_all)
                        .and_then(|()| db::import_json(&old, &self.db_path));
                    if let Err(e) = res {
                        self.legacy = None;
                        self.ask =
                            Ask::Problem(format!("Could not import {}: {e}.", old.display()));
                        return;
                    }
                    self.evaluate();
                } else if code == KeyCode::Char('n') {
                    self.legacy = None;
                    self.start_scan();
                } else if quit {
                    self.quit = true;
                }
            }
            Ask::NoExport | Ask::Problem(_) => {
                if yes {
                    self.start_scan();
                } else if quit || code == KeyCode::Char('n') {
                    self.quit = true;
                }
            }
            Ask::Existing { .. } => match code {
                KeyCode::Enter | KeyCode::Char('b') => self.browse(),
                KeyCode::Char('r') => self.ask = Ask::Rescan,
                _ if quit => self.quit = true,
                _ => {}
            },
            Ask::Rescan => {
                if yes {
                    self.start_scan();
                } else if quit || code == KeyCode::Char('n') {
                    self.evaluate();
                }
            }
        }
    }

    fn open_dir(&mut self) {
        let Some(c) = self.children.get(self.sel).filter(|c| c.is_dir) else {
            return;
        };
        let new_root = self.root.join(OsStr::from_bytes(&c.name));
        let old = std::mem::replace(&mut self.root, new_root);
        self.stack.push((old, self.sel));
        (self.sel, self.offset) = (0, 0);
        self.load();
    }

    fn go_up(&mut self) {
        if let Some((root, sel)) = self.stack.pop() {
            (self.root, self.sel, self.offset) = (root, sel, 0);
            self.load();
        } else if self.root != self.scan_root && self.root.starts_with(&self.scan_root) {
            // Above the launch directory: select the folder we came from.
            let from = self.root.file_name().map(|n| n.as_bytes().to_vec());
            self.root.pop();
            (self.sel, self.offset) = (0, 0);
            self.load();
            let from = from.unwrap_or_default();
            self.sel = self
                .children
                .iter()
                .position(|c| c.name == from)
                .unwrap_or(0);
        }
    }

    fn breadcrumb(&self) -> String {
        let base = self.scan_root.display().to_string();
        match self.root.strip_prefix(&self.scan_root) {
            Ok(rel) if !rel.as_os_str().is_empty() => {
                let parts: Vec<_> = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy())
                    .collect();
                format!("{base} › {}", parts.join(" › "))
            }
            _ => self.root.display().to_string(),
        }
    }

    /// A worker is showing progress, so the screen must keep redrawing.
    pub fn busy(&self) -> bool {
        match self.mode {
            Mode::Refreshing | Mode::Running => true,
            Mode::Menu => self.computing(),
            _ => false,
        }
    }

    fn computing(&self) -> bool {
        self.rx.is_some() && self.picks.len() < self.recipes.len()
    }

    /// `c`: rescan `root`, then run every recipe on the refreshed database, in one worker,
    /// so the hits can only come from a database refreshed for this directory just now.
    fn start_clean(&mut self) {
        self.progress = Arc::default();
        self.scan_started = Instant::now();
        self.picks.clear();
        self.msel = 0;
        self.mode = Mode::Refreshing;
        let (db, here, threads, progress) = (
            self.db_path.clone(),
            self.root.clone(),
            self.threads,
            self.progress.clone(),
        );
        let recipes = self.recipes.clone();
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        std::thread::spawn(move || {
            let res = db::refresh(&db, std::slice::from_ref(&here), threads, progress);
            let ok = res.is_ok() && is_dir(&here);
            if tx.send(Msg::Refreshed(res)).is_err() || !ok {
                return;
            }
            let d = match Db::open_ro(&db) {
                Ok(d) => d,
                Err(e) => {
                    for i in 0..recipes.len() {
                        let _ = tx.send(Msg::Picked(i, Err(e.to_string())));
                    }
                    return;
                }
            };
            for (i, r) in recipes.iter().enumerate() {
                // Stop once nobody listens (esc, or a run started).
                if tx
                    .send(Msg::Picked(i, recipe::select(&d, r, &here)))
                    .is_err()
                {
                    return;
                }
            }
        });
    }

    fn on_refreshed(&mut self, res: io::Result<db::Refreshed>) {
        if self.mode != Mode::Refreshing {
            // esc'd: apply the finished refresh silently.
            self.rx = None;
            self.reopen();
            return;
        }
        let here = self.root.clone();
        if !is_dir(&here) {
            while !is_dir(&self.root) {
                if let Some((r, sel)) = self.stack.pop() {
                    (self.root, self.sel) = (r, sel);
                } else if self.root != self.scan_root && self.root.pop() {
                    self.sel = 0;
                } else {
                    break;
                }
            }
            self.rx = None;
            self.offset = 0;
            self.mode = Mode::List;
            self.reopen();
            self.status = format!(
                "{} no longer exists; showing the folder above.",
                here.display()
            );
            return;
        }
        match res {
            Ok(_) => {
                self.reopen();
                self.mode = Mode::Menu;
            }
            Err(e) => {
                self.rx = None;
                self.mode = Mode::List;
                self.status = format!("Could not update the scan of this folder: {e}");
            }
        }
    }

    /// Indexes into `picks` of the recipes that can be chosen: those that found something.
    fn shown(&self) -> Vec<usize> {
        (0..self.picks.len())
            .filter(|&i| self.picks[i].1.as_ref().is_ok_and(|s| !s.hits.is_empty()))
            .collect()
    }

    /// The chosen recipe and its selection (Paths, Confirm).
    fn chosen(&self) -> (&Recipe, &Selection) {
        let (i, sel) = &self.picks[self.pick];
        (
            &self.recipes[*i],
            sel.as_ref().expect("only successful picks are chosen"),
        )
    }

    fn recount(&mut self) {
        let (_, sel) = self.chosen();
        let t = sel
            .hits
            .iter()
            .zip(&self.ticks)
            .filter(|(_, t)| **t)
            .fold((0, 0), |(n, s), (h, _)| (n + 1, s + h.size));
        self.ticked = t;
    }

    fn on_key_menu(&mut self, code: KeyCode) {
        let shown = self.shown();
        if let Some(n) = Self::step(self.msel, shown.len(), code) {
            self.msel = n;
            return;
        }
        match code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.rx = None;
                self.mode = Mode::List;
            }
            // A run's rescan must not wait on a recipe query still reading the database.
            KeyCode::Enter if self.computing() => {
                self.status = "Still checking recipes — one moment…".into();
            }
            KeyCode::Enter => {
                let Some(&i) = shown.get(self.msel) else {
                    return;
                };
                let Ok(sel) = &self.picks[i].1 else { return };
                self.ticks = vec![true; sel.hits.len()];
                (self.pick, self.psel, self.poff) = (i, 0, 0);
                self.recount();
                self.mode = Mode::Paths;
            }
            _ => {}
        }
    }

    fn on_key_paths(&mut self, key: KeyEvent) {
        let len = self.ticks.len();
        if let Some(n) = Self::step(self.psel, len, key.code) {
            self.psel = n;
            return;
        }
        match key.code {
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let all = self.ticked.0 < self.ticks.len();
                self.ticks.fill(all);
                self.recount();
            }
            KeyCode::Char(' ') => {
                if let Some(t) = self.ticks.get_mut(self.psel) {
                    *t = !*t;
                    let size = self.chosen().1.hits[self.psel].size;
                    self.ticked = if self.ticks[self.psel] {
                        (self.ticked.0 + 1, self.ticked.1 + size)
                    } else {
                        (self.ticked.0 - 1, self.ticked.1 - size)
                    };
                }
                self.psel = (self.psel + 1).min(len.saturating_sub(1));
            }
            KeyCode::Enter if self.ticked.0 == 0 => {
                self.status = "Tick at least one path to continue.".into();
            }
            KeyCode::Enter => self.mode = Mode::Confirm,
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Menu,
            _ => {}
        }
    }

    fn start_run(&mut self) {
        let (r, sel) = self.chosen();
        let recipe = r.clone();
        // Only the ticked hits, in order: `apply::run` is what keeps them inside `here`.
        let hits: Vec<_> = sel
            .hits
            .iter()
            .zip(&self.ticks)
            .filter(|(_, t)| **t)
            .map(|(h, _)| h.clone())
            .collect();
        self.cancel = Arc::default();
        self.step = (0, hits.len(), String::new());
        self.outcome = None;
        self.mode = Mode::Running;
        let (db, root, here, bin, threads) = (
            self.db_path.clone(),
            self.scan_root.clone(),
            self.root.clone(),
            self.bin.clone(),
            self.threads,
        );
        let cancel = self.cancel.clone();
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        std::thread::spawn(move || {
            let total = hits.len();
            let (mut done, mut problems) = (0, Vec::new());
            apply::run(
                &recipe,
                &root,
                &here,
                &hits,
                bin.as_deref(),
                &cancel,
                &mut |i, h, v| {
                    if *v == Verdict::Done {
                        done += 1;
                    } else {
                        problems.push((h.path.clone(), v.clone()));
                    }
                    let p = String::from_utf8_lossy(&h.path).into_owned();
                    let _ = tx.send(Msg::Step(i + 1, total, p));
                },
            );
            let res = db::refresh(&db, &[here], threads, Arc::default());
            let _ = tx.send(Msg::Applied(done, problems, res));
        });
    }

    fn step(cur: usize, len: usize, key: KeyCode) -> Option<usize> {
        let last = len.saturating_sub(1);
        Some(match key {
            KeyCode::Down | KeyCode::Char('j') => (cur + 1).min(last),
            KeyCode::Up | KeyCode::Char('k') => cur.saturating_sub(1),
            KeyCode::PageDown => (cur + 10).min(last),
            KeyCode::PageUp => cur.saturating_sub(10),
            KeyCode::Home | KeyCode::Char('g') => 0,
            KeyCode::End | KeyCode::Char('G') => last,
            _ => return None,
        })
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && key.code == KeyCode::Char('c')
            && self.mode != Mode::Running
        {
            self.quit = true;
            return;
        }
        let code = key.code;
        match self.mode {
            Mode::Ask => self.on_key_ask(code),
            Mode::Scanning => {
                if matches!(code, KeyCode::Char('q') | KeyCode::Esc) {
                    self.quit = true;
                }
            }
            Mode::List => {
                self.status.clear();
                if let Some(n) = Self::step(self.sel, self.children.len(), code) {
                    self.sel = n;
                    return;
                }
                match code {
                    KeyCode::Esc if !self.stack.is_empty() => self.go_up(),
                    KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h' | 'u') => self.go_up(),
                    KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
                    KeyCode::Char('N') => self.want_ncdu = true,
                    KeyCode::Char('c') if self.db.is_some() => self.start_clean(),
                    KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => self.open_dir(),
                    _ => {}
                }
            }
            Mode::Refreshing => {
                if matches!(code, KeyCode::Esc | KeyCode::Char('q')) {
                    // The scan can't be interrupted; its result is applied when it ends.
                    self.mode = Mode::List;
                }
            }
            Mode::Menu => {
                self.status.clear();
                self.on_key_menu(code);
            }
            Mode::Paths => {
                self.status.clear();
                self.on_key_paths(key);
            }
            Mode::Confirm => match code {
                KeyCode::Char('y' | 'Y') => self.start_run(),
                KeyCode::Esc | KeyCode::Char('n' | 'q') => self.mode = Mode::Paths,
                _ => {}
            },
            Mode::Running => {
                if code == KeyCode::Esc && !self.cancel.swap(true, Ordering::Relaxed) {
                    self.status = "Stopping after the current path…".into();
                }
            }
            Mode::Done => {
                self.status.clear();
                self.mode = Mode::List;
            }
        }
    }

    /// Drain progress from the background worker. Call regularly.
    pub fn tick(&mut self) {
        while let Some(msg) = self.rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            match msg {
                Msg::Scanned(res) => {
                    self.rx = None;
                    match res {
                        Ok(files) => {
                            self.browse();
                            self.status = format!(
                                "Scan finished: {files} files in {}s",
                                self.scan_started.elapsed().as_secs()
                            );
                        }
                        Err(e) => {
                            self.ask = Ask::Problem(format!("The scan failed: {e}."));
                            self.mode = Mode::Ask;
                        }
                    }
                }
                Msg::Refreshed(res) => self.on_refreshed(res),
                Msg::Picked(i, res) => self.picks.push((i, res)),
                Msg::Step(done, total, path) => self.step = (done, total, path),
                Msg::Applied(done, problems, res) => {
                    self.rx = None;
                    let before = self.total;
                    self.reopen();
                    self.status.clear();
                    self.outcome = Some(Outcome {
                        done,
                        problems,
                        rescan: res.err().map(|e| e.to_string()),
                        before,
                        after: self.total,
                    });
                    self.mode = Mode::Done;
                }
            }
        }
    }

    pub fn draw(&mut self, f: &mut Frame) {
        let [head, body, foot] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .areas(f.area());
        let lines = if matches!(self.mode, Mode::Ask | Mode::Scanning) {
            vec![Line::from(format!("dume — {}", self.scan_root.display())).bold()]
        } else {
            vec![
                Line::from(self.breadcrumb()).bold(),
                Line::from(format!("{} of your files here", human(self.total))),
            ]
        };
        f.render_widget(Paragraph::new(lines), head);
        let help = match self.mode {
            Mode::Ask => "answer with the keys shown · q quit",
            Mode::Scanning => "scanning — q quits and discards this scan",
            Mode::List => "↑↓ move · enter open · ← back · c clean this folder · N ncdu · q quit",
            Mode::Refreshing => "esc back (the update finishes in the background)",
            Mode::Menu => "↑↓ move · enter choose · esc back",
            Mode::Paths => {
                "↑↓ move · space tick/untick · ctrl-a all/none · enter continue · esc back"
            }
            Mode::Confirm => "y yes, clean · n/esc back",
            Mode::Running => "esc stop after the current path",
            Mode::Done => "any key back to the folder",
        };
        f.render_widget(
            Paragraph::new(vec![
                Line::from(help),
                Line::from(self.status.as_str()).yellow(),
            ]),
            foot,
        );
        match self.mode {
            Mode::Ask => self.draw_ask(f, body),
            Mode::Scanning => self.draw_scanning(f, body),
            Mode::List => self.draw_list(f, body),
            Mode::Refreshing => self.draw_refreshing(f, body),
            Mode::Menu => self.draw_menu(f, body),
            Mode::Paths => self.draw_paths(f, body),
            Mode::Confirm => self.draw_confirm(f, body),
            Mode::Running => self.draw_running(f, body),
            Mode::Done => self.draw_done(f, body),
        }
    }

    fn spinner(&self) -> &'static str {
        SPIN[(self.scan_started.elapsed().as_millis() / 150) as usize % SPIN.len()]
    }

    fn draw_refreshing(&self, f: &mut Frame, area: Rect) {
        let lines = vec![
            Line::from(format!(
                "{} Updating the scan of {} …",
                self.spinner(),
                self.root.display()
            ))
            .bold(),
            Line::from(format!(
                "{} of your files found · {}s",
                self.progress.files.load(Ordering::Relaxed),
                self.scan_started.elapsed().as_secs()
            )),
            Line::from("Then dume looks for what can be cleaned here."),
        ];
        f.render_widget(
            Paragraph::new(lines).block(Block::bordered().title(" Clean this folder ")),
            area,
        );
    }

    fn draw_menu(&mut self, f: &mut Frame, area: Rect) {
        let block = Block::bordered().title(" What to clean here ");
        let inner = block.inner(area);
        f.render_widget(block, area);
        let [list, note] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
        if self.computing() {
            f.render_widget(
                Line::from(format!(
                    "{} Checking recipe {} of {} …",
                    self.spinner(),
                    self.picks.len() + 1,
                    self.recipes.len()
                )),
                note,
            );
        }
        // Failures are listed below the choices, one line each, and cannot be chosen.
        let failed: Vec<Line> = self
            .picks
            .iter()
            .filter_map(|(i, res)| res.as_ref().err().map(|e| (i, e)))
            .map(|(i, e)| {
                Line::from(format!("recipe {} failed: {e}", self.recipes[*i].name)).yellow()
            })
            .collect();
        let shown = self.shown();
        let [list, errs] = Layout::vertical([
            Constraint::Length(shown.len().max(2) as u16),
            Constraint::Min(0),
        ])
        .areas(list);
        f.render_widget(Paragraph::new(failed).wrap(Wrap { trim: false }), errs);
        if shown.is_empty() {
            if !self.computing() {
                f.render_widget(
                    Paragraph::new(vec![
                        Line::from("Nothing to clean here.").bold(),
                        Line::from("Go back (esc) and try another folder, or a folder above."),
                    ]),
                    list,
                );
            }
            return;
        }
        self.msel = self.msel.min(shown.len() - 1);
        // shortcut: the menu is not windowed; there are only as many lines as recipes.
        let rows: Vec<TRow> = shown
            .iter()
            .map(|&i| {
                let r = &self.recipes[self.picks[i].0];
                let s = self.picks[i].1.as_ref().expect("shown picks are Ok");
                TRow::new([
                    r.name.clone(),
                    n_paths(s.hits.len()),
                    amount(&r.apply, s.size),
                ])
            })
            .collect();
        let widths = [
            Constraint::Min(20),
            Constraint::Length(14),
            Constraint::Length(16),
        ];
        f.render_stateful_widget(
            Table::new(rows, widths)
                .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
                .highlight_symbol("▶ "),
            list,
            &mut TableState::default().with_selected(Some(self.msel)),
        );
    }

    /// `p` (relative to the scan root) relative to `root`, for display.
    fn rel_here(&self, p: &[u8]) -> String {
        let here = self
            .root
            .strip_prefix(&self.scan_root)
            .unwrap_or(Path::new(""));
        let n = here.as_os_str().len();
        let p = if n == 0 {
            p
        } else {
            p.get(n + 1..).unwrap_or(p)
        };
        String::from_utf8_lossy(p).into_owned()
    }

    fn draw_paths(&mut self, f: &mut Frame, area: Rect) {
        let (r, sel) = self.chosen();
        let (n, size) = self.ticked;
        let title = format!(
            " {}: {n} of {} paths ticked, {} — in {} ",
            r.name,
            sel.hits.len(),
            amount(&r.apply, size),
            self.root.display()
        );
        let block = Block::bordered().title(title);
        let inner = block.inner(area);
        // Build only the rows on screen: a recipe can select hundreds of thousands of paths.
        let height = inner.height as usize;
        let off = window(self.poff, self.psel, height, sel.hits.len());
        let end = (off + height).min(sel.hits.len());
        let rows: Vec<TRow> = (off..end)
            .map(|i| {
                let h = &sel.hits[i];
                let name = self.rel_here(&h.path);
                TRow::new([
                    if self.ticks[i] { "[x]" } else { "[ ]" }.to_string(),
                    human(h.size),
                    if h.is_dir { format!("{name}/") } else { name },
                ])
            })
            .collect();
        let widths = [
            Constraint::Length(3),
            Constraint::Length(11),
            Constraint::Min(10),
        ];
        let sel_row = self.psel - off;
        f.render_stateful_widget(
            Table::new(rows, widths)
                .block(block)
                .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
                .highlight_symbol("▶ "),
            area,
            &mut TableState::default().with_selected(Some(sel_row)),
        );
        self.poff = off;
    }

    fn draw_confirm(&self, f: &mut Frame, area: Rect) {
        let (r, _) = self.chosen();
        let (n, size) = self.ticked;
        let ask = match &r.apply {
            Apply::Delete => format!("Remove {} ({})?", n_paths(n), human(size)),
            Apply::Script(s) => format!(
                "Run {} on {}?",
                s.file_name().unwrap_or_default().to_string_lossy(),
                plural(n, "file")
            ),
        };
        let mut lines = vec![
            Line::from(format!("{ask} This cannot be undone.")).bold(),
            Line::from(format!("Folder: {}", self.root.display())),
            Line::from(format!("Recipe: {}", r.name)),
        ];
        if self.root == self.scan_root {
            lines.push(Line::from(""));
            lines.push(
                Line::from(
                    "This is the top folder of the whole scan: everything ticked below it will be changed.",
                )
                .bold()
                .red(),
            );
        }
        lines.push(Line::from(""));
        lines.push(Line::from("[y] yes, clean   [n] no, go back"));
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(Block::bordered().title(" Are you sure? ")),
            area,
        );
    }

    fn draw_running(&self, f: &mut Frame, area: Rect) {
        let (done, total, path) = &self.step;
        let lines = vec![
            Line::from(format!("Cleaning: {done} of {total} paths")).bold(),
            Line::from(path.as_str()),
        ];
        f.render_widget(
            Paragraph::new(lines).block(Block::bordered().title(" Cleaning ")),
            area,
        );
    }

    fn draw_done(&self, f: &mut Frame, area: Rect) {
        let Some(o) = &self.outcome else { return };
        let mut lines = match &o.rescan {
            // The database was not updated, so `after` would be the old size: say so instead.
            Some(e) => vec![
                Line::from("dume could not measure how much space was freed.").bold(),
                Line::from(format!("The scan could not be updated: {e}")).yellow(),
                Line::from("Press c again or rescan to refresh."),
            ],
            None => vec![
                Line::from(match o.after.cmp(&o.before) {
                    std::cmp::Ordering::Less => format!(
                        "Freed {} here (was {}, now {}).",
                        human(o.before - o.after),
                        human(o.before),
                        human(o.after)
                    ),
                    std::cmp::Ordering::Equal => {
                        format!("This folder did not shrink: still {}.", human(o.after))
                    }
                    std::cmp::Ordering::Greater => format!(
                        "The folder is now {} (it was {}).",
                        human(o.after),
                        human(o.before)
                    ),
                })
                .bold(),
            ],
        };
        let stopped = o
            .problems
            .iter()
            .filter(|(_, v)| matches!(v, Verdict::Skipped(w) if w == "cancelled"))
            .count();
        if stopped > 0 {
            lines.insert(
                0,
                Line::from(format!(
                    "You stopped the run: {} {} not touched.",
                    n_paths(stopped),
                    if stopped == 1 { "was" } else { "were" }
                ))
                .bold(),
            );
        }
        let failed = o
            .problems
            .iter()
            .filter(|(_, v)| matches!(v, Verdict::Failed(_)))
            .count();
        lines.push(Line::from(format!(
            "{} done · {} skipped · {failed} failed",
            o.done,
            o.problems.len() - failed
        )));
        // Only as many reasons as fit.
        let room = (area.height as usize).saturating_sub(lines.len() + 3);
        if !o.problems.is_empty() {
            lines.push(Line::from(""));
        }
        for (p, v) in o.problems.iter().take(room) {
            let (what, why) = match v {
                Verdict::Failed(w) => ("failed", w),
                Verdict::Skipped(w) => ("skipped", w),
                Verdict::Done => continue,
            };
            lines.push(Line::from(format!("{what}: {}: {why}", self.rel_here(p))));
        }
        if o.problems.len() > room {
            lines.push(Line::from(format!(
                "… and {} more",
                o.problems.len() - room
            )));
        }
        f.render_widget(
            Paragraph::new(lines).block(Block::bordered().title(" Done ")),
            area,
        );
    }

    fn draw_ask(&self, f: &mut Frame, area: Rect) {
        let root = self.scan_root.display();
        let file = self.db_path.display();
        let lines: Vec<Line> = match &self.ask {
            Ask::Migrate(old) => vec![
                Line::from(format!(
                    "Found a scan from an earlier version: {}",
                    old.display()
                )),
                Line::from(format!("Import it into {file}? The old file is kept.")),
                Line::from(""),
                Line::from("[y] import it   [n] ignore it and scan fresh   [q] quit"),
            ],
            Ask::NoExport => vec![
                Line::from(format!("You have no scan of {root} yet.")),
                Line::from("Scan it now? This can take many minutes on a big disk."),
                Line::from(""),
                Line::from("[y] scan now   [q] quit"),
            ],
            Ask::Problem(why) => vec![
                Line::from(why.as_str()).yellow(),
                Line::from(format!("Do a full scan of {root} now? It replaces {file}.")),
                Line::from(""),
                Line::from("[y] scan now   [q] quit"),
            ],
            Ask::Existing { age } => {
                let stale = age.as_secs() > self.stale_days * 86400;
                let mut v = vec![Line::from(format!(
                    "Found your scan of {root}, made {} ago.",
                    age_text(*age)
                ))];
                if stale {
                    v.push(
                        Line::from(format!(
                            "It is more than {} days old; a rescan is recommended.",
                            self.stale_days
                        ))
                        .yellow(),
                    );
                }
                v.push(Line::from(""));
                v.push(Line::from(
                    "[enter] browse it   [r] rescan everything   [q] quit",
                ));
                v
            }
            Ask::Rescan => vec![
                Line::from(format!("Do a full scan of {root} and overwrite {file}?")).bold(),
                Line::from(
                    "This can take many minutes. Cleaning already updates the scan for you.",
                ),
                Line::from(""),
                Line::from("[y] yes, rescan   [n] no, go back"),
            ],
        };
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(Block::bordered().title(" dume ")),
            area,
        );
    }

    fn draw_scanning(&self, f: &mut Frame, area: Rect) {
        let p = &self.progress;
        let lines = vec![
            Line::from(format!("Scanning {} …", self.scan_root.display())).bold(),
            Line::from(format!(
                "{} directories read · {} of your files found · {}s",
                p.dirs.load(Ordering::Relaxed),
                p.files.load(Ordering::Relaxed),
                self.scan_started.elapsed().as_secs()
            )),
        ];
        f.render_widget(
            Paragraph::new(lines).block(Block::bordered().title(" Scanning ")),
            area,
        );
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect) {
        let len = self.children.len();
        if len == 0 {
            f.render_widget(Paragraph::new("None of your files are under here."), area);
            return;
        }
        // Build only the rows on screen: a directory can hold hundreds of thousands of entries.
        let height = area.height.saturating_sub(1) as usize; // minus the header row
        let off = window(self.offset, self.sel, height, len);
        self.offset = off;
        let end = (off + height).min(len);
        let rows: Vec<TRow> = self.children[off..end]
            .iter()
            .map(|c| {
                let name = String::from_utf8_lossy(&c.name);
                TRow::new([
                    human(c.size),
                    format!("{:.1}%", pct(c.size, self.total)),
                    if c.is_dir {
                        format!("{name}/")
                    } else {
                        name.into_owned()
                    },
                ])
            })
            .collect();
        let widths = [
            Constraint::Length(11),
            Constraint::Length(7),
            Constraint::Min(10),
        ];
        f.render_stateful_widget(
            Table::new(rows, widths)
                .header(
                    TRow::new(["SIZE", "%", "NAME"])
                        .style(Style::new().add_modifier(Modifier::BOLD)),
                )
                .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
                .highlight_symbol("▶ "),
            area,
            &mut TableState::default().with_selected(Some(self.sel - off)),
        );
    }
}
