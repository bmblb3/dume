//! Terminal UI: browse your scan database directory by directory.
//!
//! The state machine (`App::on_key` / `App::tick`) is separate from drawing so it can be tested.

use crate::db::{self, Child, Db};
use crate::recipe::Recipe;
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
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant, SystemTime};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// A question at start-up (see [`Ask`]).
    Ask,
    Scanning,
    /// The Where view: the current directory's children, largest first.
    List,
    Confirm,
    Running,
    Done,
}

enum Msg {
    /// A full scan finished, with the number of files kept.
    Scanned(io::Result<u64>),
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
    #[allow(dead_code)] // used by the clean flow (Task 9)
    recipes: Vec<Recipe>,
    #[allow(dead_code)] // used by the clean flow (Task 9)
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
        let Some((root, sel)) = self.stack.pop() else {
            return;
        };
        (self.root, self.sel, self.offset) = (root, sel, 0);
        self.load();
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
                    KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => self.open_dir(),
                    _ => {}
                }
            }
            Mode::Confirm => {
                if matches!(code, KeyCode::Esc | KeyCode::Char('n' | 'q')) {
                    self.mode = Mode::List;
                }
            }
            Mode::Running => {}
            Mode::Done => self.mode = Mode::List,
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
            Mode::List => "↑↓/jk move · enter open dir · backspace up · N ncdu · q quit",
            Mode::Confirm => "n/esc cancel",
            Mode::Running => "running — please wait",
            Mode::Done => "press any key",
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
            Mode::Confirm | Mode::Running | Mode::Done => {}
        }
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
