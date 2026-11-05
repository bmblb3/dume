//! Terminal UI: browse the report, pick a directory pattern, run a cleaning action on it.
//!
//! The state machine (`App::on_key` / `App::tick`) is separate from drawing so it can be tested.

use crate::actions::{self, Action};
use crate::merge::{self, Export};
use crate::report::{self, Report, Row, human, pct};
use crate::scan::Progress;
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::Line;
use ratatui::widgets::{Block, Cell, Gauge, Paragraph, Row as TRow, Table, TableState, Tabs, Wrap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant, SystemTime};

/// Which kinds of pattern the "What" view lists.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Filter {
    All,
    Dirs,
    Files,
}

/// The "What" list entry `sel` (is_dir, index into dir_rows / file_rows) as a directory row.
fn dir_row<'a>(report: &'a Report, what: &[(bool, usize)], sel: usize) -> Option<&'a Row> {
    match what.get(sel) {
        Some(&(true, i)) => report.dir_rows.get(i),
        _ => None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// A question at start-up (see [`Ask`]).
    Ask,
    Scanning,
    List,
    Matches,
    Actions,
    Confirm,
    Running,
    Done,
}

enum Msg {
    Outcome(actions::Outcome),
    Finished(io::Result<(Report, Export)>),
    Scanned(io::Result<(Report, PathBuf, Export)>),
}

/// What the start-up screen asks the user.
#[derive(Clone, Debug, PartialEq)]
pub enum Ask {
    /// An export from an older version sits at this path.
    Migrate(PathBuf),
    NoExport,
    /// The saved export can't be used, for this reason.
    Problem(String),
    /// A usable export exists.
    Existing {
        age: Duration,
    },
    /// Confirm a full rescan that overwrites the export.
    Rescan,
}

/// Decide what to ask on start-up. Pure apart from reading the export, so it can be tested.
pub fn startup(
    export: &Path,
    scan_root: &Path,
    uid: u32,
    legacy: Option<&Path>,
    now: SystemTime,
) -> Ask {
    startup_with_doc(export, scan_root, uid, legacy, now).0
}

/// [`startup`], also handing back the parsed export when it is usable, so it isn't parsed again.
pub fn startup_with_doc(
    export: &Path,
    scan_root: &Path,
    uid: u32,
    legacy: Option<&Path>,
    now: SystemTime,
) -> (Ask, Option<Export>) {
    if !export.exists() {
        return match legacy {
            Some(old) if old.is_file() => (Ask::Migrate(old.to_path_buf()), None),
            _ => (Ask::NoExport, None),
        };
    }
    let problem = |m: String| (Ask::Problem(m), None);
    let e = match Export::load(export) {
        Ok(e) => e,
        Err(e) => return problem(format!("Your saved scan can't be read ({e}).")),
    };
    if e.uid().ok() != Some(uid) {
        return problem("Your saved scan was made by an older version or for another user.".into());
    }
    let named = e.root_name();
    if Path::new(&named).canonicalize().ok().as_deref() != Some(scan_root) {
        return problem(format!(
            "Your saved scan is for {named}, but the team root is {}.",
            scan_root.display()
        ));
    }
    let age = std::fs::metadata(export)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| now.duration_since(t).ok())
        .unwrap_or_default();
    (Ask::Existing { age }, Some(e))
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

/// Report for `view`, or for `scan_root` if `view` has nothing in the export. Returns the dir used.
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

pub fn load_view(export: &Path, view: &Path, scan_root: &Path) -> io::Result<(Report, PathBuf)> {
    view_of(&Export::load(export)?, view, scan_root)
}

fn view_of(doc: &Export, view: &Path, scan_root: &Path) -> io::Result<(Report, PathBuf)> {
    match report_of(doc, view) {
        Ok(r) => Ok((r, view.to_path_buf())),
        Err(_) if view != scan_root => Ok((report_of(doc, scan_root)?, scan_root.to_path_buf())),
        Err(e) => Err(e),
    }
}

/// Everything the wrapper needs to start.
pub struct Launch {
    pub view: PathBuf,
    pub scan_root: PathBuf,
    pub export: PathBuf,
    pub legacy_export: Option<PathBuf>,
    pub actions: Vec<Action>,
    pub bin: Option<PathBuf>,
    pub threads: usize,
    pub stale_days: u64,
    pub uid: u32,
}

/// Build a report for the directory `root` (canonical, inside the export at `export`).
pub fn load_report(export: &Path, root: &Path) -> io::Result<Report> {
    report_of(&Export::load(export)?, root)
}

fn report_of(doc: &Export, root: &Path) -> io::Result<Report> {
    let rel = doc.relative(root)?;
    Ok(report::analyse(doc.subtree(&rel)?, true))
}

pub struct App {
    pub report: Report,
    pub mode: Mode,
    pub quit: bool,
    pub status: String,
    pub failed: Vec<String>,
    /// Set by `N`: the binary should suspend the UI and run ncdu on the export.
    pub want_ncdu: bool,
    /// The directory every list is computed for.
    pub root: PathBuf,
    export: PathBuf,
    actions: Vec<Action>,
    threads: usize,
    scan_root: PathBuf,
    uid: u32,
    bin: Option<PathBuf>,
    legacy: Option<PathBuf>,
    stale_days: u64,
    ask: Ask,
    progress: Arc<Progress>,
    scan_started: Instant,
    /// 0 = Where (pick a directory), 1 = What (patterns inside it).
    view: usize,
    filter: Filter,
    /// The What list: `(is_dir, index)` into the report's dir / file rows, largest first.
    what: Vec<(bool, usize)>,
    sel: [usize; 2],
    scroll: usize,
    act_sel: usize,
    /// Matches ticked with tab; empty means every match.
    marked: std::collections::BTreeSet<usize>,
    show_all: bool,
    /// Roots we came from (innermost last), with the report and selection to return to.
    /// The report is dropped after an action, since its sizes are then out of date.
    stack: Vec<(PathBuf, Option<Report>, [usize; 2])>,
    /// First visible row of each list: Where, What, then the matches view.
    offsets: [usize; 3],
    /// The parsed export, kept so it is parsed once per session, not once per screen.
    doc: Option<Export>,
    rx: Option<Receiver<Msg>>,
    total: usize,
    done: usize,
    before: u64,
    after: Option<u64>,
    refresh_err: Option<String>,
    running: String,
}

impl App {
    pub fn new(
        report: Report,
        root: PathBuf,
        export: PathBuf,
        actions: Vec<Action>,
        threads: usize,
    ) -> Self {
        let mut app = Self {
            report,
            mode: Mode::List,
            quit: false,
            status: String::new(),
            failed: Vec::new(),
            want_ncdu: false,
            scan_root: root.clone(),
            root,
            export,
            actions,
            threads,
            uid: rustix::process::getuid().as_raw(),
            bin: None,
            legacy: None,
            stale_days: 7,
            ask: Ask::NoExport,
            progress: Arc::default(),
            scan_started: Instant::now(),
            view: 0,
            filter: Filter::All,
            what: Vec::new(),
            sel: [0; 2],
            scroll: 0,
            act_sel: 0,
            show_all: false,
            stack: Vec::new(),
            offsets: [0; 3],
            marked: Default::default(),
            doc: None,
            rx: None,
            total: 0,
            done: 0,
            before: 0,
            after: None,
            refresh_err: None,
            running: String::new(),
        };
        app.rebuild_what();
        app
    }

    /// The wrapper: start from nothing and ask what to do about the saved scan.
    pub fn launch(l: Launch) -> Self {
        let mut app = Self::new(Report::empty(), l.view, l.export, l.actions, l.threads);
        app.scan_root = l.scan_root;
        app.uid = l.uid;
        app.bin = l.bin;
        app.legacy = l.legacy_export;
        app.stale_days = l.stale_days;
        app.evaluate();
        app
    }

    fn evaluate(&mut self) {
        let (ask, doc) = startup_with_doc(
            &self.export,
            &self.scan_root,
            self.uid,
            self.legacy.as_deref(),
            SystemTime::now(),
        );
        (self.ask, self.doc) = (ask, doc);
        self.mode = Mode::Ask;
    }

    fn start_scan(&mut self) {
        self.progress = Arc::default();
        self.scan_started = Instant::now();
        self.mode = Mode::Scanning;
        let (scan_root, export, uid, threads) = (
            self.scan_root.clone(),
            self.export.clone(),
            self.uid,
            self.threads,
        );
        let (view, progress) = (self.root.clone(), self.progress.clone());
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        std::thread::spawn(move || {
            let res = (|| {
                merge::scan_to_file(&scan_root, &export, uid, threads, progress)?;
                let doc = Export::load(&export)?;
                let (report, view) = view_of(&doc, &view, &scan_root)?;
                Ok((report, view, doc))
            })();
            let _ = tx.send(Msg::Scanned(res));
        });
    }

    /// Remove the half-written scan file if we quit mid-scan.
    pub fn cleanup(&self) {
        if self.mode == Mode::Scanning {
            let name = self
                .export
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            let _ = std::fs::remove_file(self.export.with_file_name(format!(".{name}.tmp")));
        }
    }

    fn on_key_ask(&mut self, code: KeyCode) {
        let yes = matches!(code, KeyCode::Char('y' | 'Y'));
        let quit = matches!(code, KeyCode::Char('q' | 'Q') | KeyCode::Esc);
        match self.ask.clone() {
            Ask::Migrate(old) => {
                if yes {
                    let moved = self
                        .export
                        .parent()
                        .map_or(Ok(()), std::fs::create_dir_all)
                        .and_then(|()| std::fs::rename(&old, &self.export));
                    if let Err(e) = moved {
                        self.legacy = None;
                        self.ask = Ask::Problem(format!("Could not move {}: {e}.", old.display()));
                        return;
                    }
                    self.evaluate();
                } else if code == KeyCode::Char('n') {
                    self.legacy = None;
                    self.evaluate();
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
                KeyCode::Enter | KeyCode::Char('b') => {
                    let doc = self
                        .doc
                        .take()
                        .map_or_else(|| Export::load(&self.export), Ok);
                    match doc.and_then(|d| {
                        let v = view_of(&d, &self.root, &self.scan_root)?;
                        Ok((v, d))
                    }) {
                        Ok(((r, view), doc)) => {
                            self.doc = Some(doc);
                            self.root = view;
                            self.set_report(r);
                            self.mode = Mode::List;
                            self.clamp();
                        }
                        Err(e) => {
                            self.ask = Ask::Problem(format!("Your saved scan can't be read ({e})."))
                        }
                    }
                }
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

    fn pick_root(&mut self) {
        let Some((_, name)) = self.report.children.get(self.sel[0]) else {
            return;
        };
        let new_root = self.root.join(name);
        let doc = match self
            .doc
            .take()
            .map_or_else(|| Export::load(&self.export), Ok)
        {
            Ok(d) => d,
            Err(e) => {
                self.status = format!("cannot read {}: {e}", self.export.display());
                return;
            }
        };
        let res = report_of(&doc, &new_root);
        self.doc = Some(doc);
        match res {
            Ok(r) => {
                let old = std::mem::replace(&mut self.report, r);
                let old_root = std::mem::replace(&mut self.root, new_root);
                self.stack.push((old_root, Some(old), self.sel));
                self.sel = [0; 2];
                self.offsets = [0; 3];
                self.view = 0;
                self.rebuild_what();
            }
            Err(e) => self.status = e.to_string(),
        }
    }

    fn pop_root(&mut self) {
        let Some((root, report, sel)) = self.stack.pop() else {
            return;
        };
        let report = report.map_or_else(
            || {
                let doc = self
                    .doc
                    .take()
                    .map_or_else(|| Export::load(&self.export), Ok)?;
                let r = report_of(&doc, &root);
                self.doc = Some(doc);
                r
            },
            Ok,
        );
        match report {
            Ok(r) => {
                self.root = root;
                self.set_report(r);
                self.sel = sel;
                self.offsets = [0; 3];
                self.view = 0;
                self.clamp();
            }
            Err(e) => {
                self.stack.push((root, None, sel));
                self.status = e.to_string();
            }
        }
    }

    pub fn selected_in(&self, view: usize) -> usize {
        self.sel[view]
    }

    pub fn export_path(&self) -> &Path {
        &self.export
    }

    fn len(&self, view: usize) -> usize {
        if view == 0 {
            self.report.children.len()
        } else {
            self.what.len()
        }
    }

    /// Rebuild the What list for the current report and filter, largest first.
    fn rebuild_what(&mut self) {
        let (d, f) = (&self.report.dir_rows, &self.report.file_rows);
        let mut v: Vec<(bool, usize)> = Vec::new();
        if self.filter != Filter::Files {
            v.extend((0..d.len()).map(|i| (true, i)));
        }
        if self.filter != Filter::Dirs {
            v.extend((0..f.len()).map(|i| (false, i)));
        }
        let row = |&(is_dir, i): &(bool, usize)| if is_dir { &d[i] } else { &f[i] };
        v.sort_by(|a, b| {
            let (a, b) = (row(a), row(b));
            b.size.cmp(&a.size).then_with(|| a.pattern.cmp(&b.pattern))
        });
        self.what = v;
        self.clamp();
    }

    fn set_report(&mut self, r: Report) {
        self.report = r;
        self.rebuild_what();
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

    fn cur_row(&self) -> Option<&Row> {
        dir_row(&self.report, &self.what, self.sel[1])
    }

    /// Indices into `actions` offered for the current pattern: matching first.
    fn action_list(&self) -> Vec<usize> {
        let pat = self.cur_row().map_or("", |r| r.pattern.as_str());
        let (mut m, o): (Vec<_>, Vec<_>) =
            (0..self.actions.len()).partition(|&i| self.actions[i].applies_to(pat));
        if self.show_all {
            m.extend(o);
        }
        m
    }

    fn clamp(&mut self) {
        for v in 0..2 {
            self.sel[v] = self.sel[v].min(self.len(v).saturating_sub(1));
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
        let back = matches!(
            code,
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('h' | 'q')
        );
        match self.mode {
            Mode::Ask => self.on_key_ask(code),
            Mode::Scanning => {
                if matches!(code, KeyCode::Char('q') | KeyCode::Esc) {
                    self.quit = true;
                }
            }
            Mode::List => {
                self.status.clear();
                if let Some(n) = Self::step(self.sel[self.view], self.len(self.view), code) {
                    self.sel[self.view] = n;
                    return;
                }
                let open = matches!(code, KeyCode::Enter | KeyCode::Right | KeyCode::Char('l'));
                match code {
                    KeyCode::Esc if !self.stack.is_empty() => self.pop_root(),
                    KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h' | 'u') => {
                        self.pop_root()
                    }
                    KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
                    KeyCode::Char('N') => self.want_ncdu = true,
                    KeyCode::Tab | KeyCode::BackTab => self.view = 1 - self.view,
                    KeyCode::Char('1') => self.view = 0,
                    KeyCode::Char('2') => self.view = 1,
                    KeyCode::Char(c @ ('d' | 'f')) if self.view == 1 => {
                        let want = if c == 'd' {
                            Filter::Dirs
                        } else {
                            Filter::Files
                        };
                        self.filter = if self.filter == want {
                            Filter::All
                        } else {
                            want
                        };
                        self.sel[1] = 0;
                        self.offsets[1] = 0;
                        self.rebuild_what();
                    }
                    _ if open && self.view == 0 => self.pick_root(),
                    _ if open && self.cur_row().is_some() => {
                        self.mode = Mode::Matches;
                        self.scroll = 0;
                        self.marked.clear();
                    }
                    _ if open && !self.what.is_empty() => {
                        self.status =
                            "actions apply to directory patterns; press d to show only those"
                                .into();
                    }
                    _ => {}
                }
            }
            Mode::Matches => {
                let n = self.cur_row().map_or(0, |r| r.dirs.len());
                if key.modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('a') {
                    // fzf's select-all: toggles between everything and nothing.
                    self.marked = if self.marked.len() == n {
                        Default::default()
                    } else {
                        (0..n).collect()
                    };
                } else if matches!(code, KeyCode::Tab | KeyCode::Char(' ')) && n > 0 {
                    if !self.marked.remove(&self.scroll) {
                        self.marked.insert(self.scroll);
                    }
                    self.scroll = (self.scroll + 1).min(n - 1);
                } else if let Some(s) = Self::step(self.scroll, n, code) {
                    self.scroll = s;
                } else if back {
                    self.mode = Mode::List;
                } else if matches!(
                    code,
                    KeyCode::Enter | KeyCode::Right | KeyCode::Char('a' | 'l')
                ) {
                    self.mode = Mode::Actions;
                    self.act_sel = 0;
                }
            }
            Mode::Actions => {
                let n = self.action_list().len();
                if let Some(s) = Self::step(self.act_sel, n, code) {
                    self.act_sel = s;
                } else if back {
                    self.mode = Mode::Matches;
                } else if code == KeyCode::Char('A') {
                    self.show_all = !self.show_all;
                    self.act_sel = 0;
                } else if code == KeyCode::Enter && n > 0 {
                    self.mode = Mode::Confirm;
                    self.scroll = 0;
                }
            }
            Mode::Confirm => {
                let n = self.cur_row().map_or(0, |r| self.target_count(r));
                if let Some(s) = Self::step(self.scroll, n, code) {
                    self.scroll = s;
                } else if code == KeyCode::Char('y') {
                    self.start();
                } else if back || code == KeyCode::Char('n') {
                    self.mode = Mode::Actions;
                }
            }
            Mode::Running => {}
            Mode::Done => {
                self.mode = Mode::List;
                self.clamp();
            }
        }
    }

    /// The matches an action will run on: the ticked ones, or all when none are ticked.
    fn targets<'a>(&'a self, row: &'a Row) -> Box<dyn Iterator<Item = &'a (String, u64)> + 'a> {
        if self.marked.is_empty() {
            Box::new(row.dirs.iter())
        } else {
            Box::new(self.marked.iter().filter_map(|&i| row.dirs.get(i)))
        }
    }

    fn target_count(&self, row: &Row) -> usize {
        if self.marked.is_empty() {
            row.dirs.len()
        } else {
            self.marked.len()
        }
    }

    fn start(&mut self) {
        let list = self.action_list();
        let (Some(row), Some(&ai)) = (self.cur_row().cloned(), list.get(self.act_sel)) else {
            return;
        };
        let action = self.actions[ai].clone();
        let dirs: Vec<PathBuf> = self
            .targets(&row)
            .map(|(p, _)| self.root.join(p.trim_start_matches('/')))
            .collect();
        self.total = dirs.len();
        self.done = 0;
        self.failed.clear();
        self.before = self.report.total;
        self.after = None;
        self.refresh_err = None;
        self.running = format!("{} on {} ({} dirs)", action.name, row.pattern, dirs.len());
        self.mode = Mode::Running;

        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        let (root, export, threads) = (self.root.clone(), self.export.clone(), self.threads);
        let bin = self.bin.clone();
        let doc = self.doc.take();
        std::thread::spawn(move || {
            for d in &dirs {
                let _ = tx.send(Msg::Outcome(actions::run_on(
                    &action,
                    &row.pattern,
                    &root,
                    d,
                    bin.as_deref(),
                )));
            }
            // Rescan what the action touched so the report shows the new sizes.
            let res = (|| {
                let mut doc = doc.map_or_else(|| Export::load(&export), Ok)?;
                merge::refresh_export(&mut doc, &export, &dirs, None, threads, false)?;
                Ok((report_of(&doc, &root)?, doc))
            })();
            let _ = tx.send(Msg::Finished(res));
        });
    }

    /// Drain progress from the background worker. Call regularly.
    pub fn tick(&mut self) {
        while let Some(msg) = self.rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            match msg {
                Msg::Outcome(o) => {
                    self.done += 1;
                    if !o.ok {
                        self.failed.push(format!("{}: {}", o.dir.display(), o.msg));
                    }
                }
                Msg::Scanned(res) => {
                    self.rx = None;
                    match res {
                        Ok((r, view, doc)) => {
                            self.doc = Some(doc);
                            self.status = format!(
                                "Scan finished: {} files in {}s",
                                r.files,
                                self.scan_started.elapsed().as_secs()
                            );
                            self.root = view;
                            self.set_report(r);
                            self.mode = Mode::List;
                            self.clamp();
                        }
                        Err(e) => {
                            self.ask = Ask::Problem(format!("The scan failed: {e}."));
                            self.mode = Mode::Ask;
                        }
                    }
                }
                Msg::Finished(res) => {
                    match res {
                        Ok((r, doc)) => {
                            for s in &mut self.stack {
                                s.1 = None;
                            }
                            self.after = Some(r.total);
                            self.set_report(r);
                            self.doc = Some(doc);
                        }
                        Err(e) => self.refresh_err = Some(e.to_string()),
                    }
                    self.rx = None;
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
            let mut sub = format!(
                "{} in {} files, {} dirs",
                human(self.report.total),
                self.report.files,
                self.report.dirs
            );
            if self.report.loose > 0 {
                sub += &format!(" · {} in files directly here", human(self.report.loose));
            }
            vec![Line::from(self.breadcrumb()).bold(), Line::from(sub)]
        };
        f.render_widget(Paragraph::new(lines), head);
        let help = match self.mode {
            Mode::Ask => "answer with the keys shown · q quit",
            Mode::Scanning => "scanning — q quits and discards this scan",
            Mode::List if self.view == 0 => {
                "↑↓/jk move · enter open dir · backspace up · tab what's inside · N ncdu · q quit"
            }
            Mode::List => {
                "↑↓/jk move · enter open pattern · d dirs · f files · backspace up · tab where · N ncdu · q quit"
            }
            Mode::Matches => {
                "↑↓/jk move · tab/space select · ctrl-a all · enter/a choose action (none selected = all) · esc back"
            }
            Mode::Actions => "↑↓/jk select · enter choose · A show all actions · esc back",
            Mode::Confirm => "y run · n/esc cancel · ↑↓ scroll",
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
            Mode::Matches => self.draw_matches(f, body),
            Mode::Actions => self.draw_actions(f, body),
            Mode::Confirm => self.draw_confirm(f, body),
            Mode::Running => self.draw_running(f, body),
            Mode::Done => self.draw_done(f, body),
        }
    }

    fn draw_ask(&self, f: &mut Frame, area: Rect) {
        let root = self.scan_root.display();
        let file = self.export.display();
        let lines: Vec<Line> = match &self.ask {
            Ask::Migrate(old) => vec![
                Line::from(format!(
                    "Found a scan from an earlier version: {}",
                    old.display()
                )),
                Line::from(format!("Move it to {file}?")),
                Line::from(""),
                Line::from("[y] move it   [n] ignore it and scan fresh   [q] quit"),
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
                    "This can take many minutes. Cleaning actions already update the scan for you.",
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
        let [tabs, table] =
            Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(area);
        let what = match self.filter {
            Filter::All => " 2 What ".to_string(),
            Filter::Dirs => " 2 What (dirs) ".to_string(),
            Filter::Files => " 2 What (files) ".to_string(),
        };
        f.render_widget(
            Tabs::new([" 1 Where ".to_string(), what])
                .select(self.view)
                .highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
            tabs,
        );
        let total = self.report.total;
        let v = self.view;
        // Build only the rows on screen: lists can hold hundreds of thousands of entries.
        let height = table.height.saturating_sub(1) as usize; // minus the header row
        let (len, sel) = (self.len(v), self.sel[v]);
        if len == 0 {
            let msg = if v == 0 {
                "No subdirectories here. Press tab to see what is inside."
            } else {
                "Nothing repeats 3+ times in here."
            };
            f.render_widget(Paragraph::new(msg), table);
            return;
        }
        let off = window(self.offsets[v], sel, height, len);
        self.offsets[v] = off;
        let end = (off + height).min(len);
        let (rows, widths, header): (Vec<TRow>, Vec<Constraint>, TRow) = if v == 0 {
            (
                self.report.children[off..end]
                    .iter()
                    .map(|(s, name)| {
                        TRow::new([
                            human(*s),
                            format!("{:.1}%", pct(*s, total)),
                            format!("{name}/"),
                        ])
                    })
                    .collect(),
                vec![
                    Constraint::Length(11),
                    Constraint::Length(7),
                    Constraint::Min(10),
                ],
                TRow::new(["SIZE", "%", "DIRECTORY"]),
            )
        } else {
            (
                self.what[off..end]
                    .iter()
                    .map(|&(is_dir, i)| {
                        let r = if is_dir {
                            &self.report.dir_rows[i]
                        } else {
                            &self.report.file_rows[i]
                        };
                        TRow::new([
                            Cell::from(if is_dir { "dir" } else { "file" }),
                            r.pattern.clone().into(),
                            r.count.to_string().into(),
                            human(r.size).into(),
                            format!("{:.1}%", pct(r.size, total)).into(),
                            r.example.trim_start_matches('/').to_string().into(),
                        ])
                    })
                    .collect(),
                vec![
                    Constraint::Length(5),
                    Constraint::Length(24),
                    Constraint::Length(8),
                    Constraint::Length(11),
                    Constraint::Length(7),
                    Constraint::Min(10),
                ],
                TRow::new(["TYPE", "PATTERN", "COUNT", "SIZE", "%", "LARGEST"]),
            )
        };
        f.render_stateful_widget(
            Table::new(rows, widths)
                .header(header.style(Style::new().add_modifier(Modifier::BOLD)))
                .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
                .highlight_symbol("▶ "),
            table,
            &mut TableState::default().with_selected(Some(sel - off)),
        );
    }

    fn draw_matches(&mut self, f: &mut Frame, area: Rect) {
        let Some(row) = dir_row(&self.report, &self.what, self.sel[1]) else {
            return;
        };
        let height = area.height.saturating_sub(3) as usize; // borders and header-less table
        let off = window(self.offsets[2], self.scroll, height, row.dirs.len());
        self.offsets[2] = off;
        let end = (off + height).min(row.dirs.len());
        let rows: Vec<TRow> = row.dirs[off..end]
            .iter()
            .enumerate()
            .map(|(i, (p, s))| {
                let tick = if self.marked.contains(&(off + i)) {
                    "✓"
                } else {
                    " "
                };
                TRow::new([format!("{tick} {}", p.trim_start_matches('/')), human(*s)])
            })
            .collect();
        f.render_stateful_widget(
            Table::new(rows, [Constraint::Min(10), Constraint::Length(11)])
                .block(Block::bordered().title(format!(
                    " {} — {} dirs, {} · {} ",
                    row.pattern,
                    row.count,
                    human(row.size),
                    if self.marked.is_empty() {
                        "all will be used".to_string()
                    } else {
                        format!("{} selected", self.marked.len())
                    }
                )))
                .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
                .highlight_symbol("▶ "),
            area,
            &mut TableState::default()
                .with_selected((!row.dirs.is_empty()).then(|| self.scroll - off)),
        );
    }

    fn draw_actions(&self, f: &mut Frame, area: Rect) {
        let pat = self.cur_row().map_or("", |r| r.pattern.as_str());
        let list = self.action_list();
        let mut lines: Vec<Line> = Vec::new();
        if list.is_empty() {
            lines.push(Line::from(format!("No action for pattern `{pat}`.")));
            lines.push(Line::from(
                "Add an executable script with a `# dume-pattern: <pattern>` header line,",
            ));
            lines.push(Line::from("or press A to list every action."));
        }
        for (n, &i) in list.iter().enumerate() {
            let a = &self.actions[i];
            let tag = if a.applies_to(pat) {
                ""
            } else {
                "  (not for this pattern)"
            };
            let mut l = Line::from(format!(
                "{} {}  [{}]{tag}",
                if n == self.act_sel { "▶" } else { " " },
                a.name,
                a.patterns.join(", ")
            ));
            if n == self.act_sel {
                l = l.style(Style::new().add_modifier(Modifier::REVERSED));
            }
            lines.push(l);
        }
        f.render_widget(
            Paragraph::new(lines).block(Block::bordered().title(format!(" Actions for `{pat}` "))),
            area,
        );
    }

    fn draw_confirm(&self, f: &mut Frame, area: Rect) {
        let (Some(row), Some(&ai)) = (self.cur_row(), self.action_list().get(self.act_sel)) else {
            return;
        };
        let a = &self.actions[ai];
        let header = vec![
            Line::from(format!("Run `{}` ({})", a.name, a.path.display())).bold(),
            Line::from(format!(
                "in each of {} directories matching `{}` ({}), with the directory as working dir.",
                self.target_count(row),
                row.pattern,
                human(self.targets(row).map(|d| d.1).sum())
            )),
            Line::from("Changes are permanent. Press y to run, n to cancel.").red(),
            Line::from(""),
        ];
        // Only the lines on screen: a pattern can match hundreds of thousands of directories.
        let height = area.height.saturating_sub(2) as usize;
        let skip = self.scroll.min(header.len());
        let mut lines: Vec<Line> = header.into_iter().skip(skip).collect();
        lines.extend(
            self.targets(row)
                .skip(self.scroll.saturating_sub(4))
                .take(height)
                .map(|(p, s)| {
                    Line::from(format!(
                        "  {}  ({})",
                        self.root.join(p.trim_start_matches('/')).display(),
                        human(*s)
                    ))
                }),
        );
        lines.truncate(height);
        f.render_widget(
            Paragraph::new(lines).block(Block::bordered().title(" Confirm ")),
            area,
        );
    }

    fn draw_running(&self, f: &mut Frame, area: Rect) {
        let [g, rest] = Layout::vertical([Constraint::Length(3), Constraint::Min(1)]).areas(area);
        f.render_widget(
            Gauge::default()
                .block(Block::bordered().title(format!(" {} ", self.running)))
                .ratio(self.done as f64 / self.total.max(1) as f64)
                .label(format!("{}/{}", self.done, self.total)),
            g,
        );
        let lines: Vec<Line> = self
            .failed
            .iter()
            .map(|s| Line::from(s.as_str()).red())
            .collect();
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), rest);
    }

    fn draw_done(&self, f: &mut Frame, area: Rect) {
        let mut lines = vec![
            Line::from(format!(
                "{}: ran on {} directories, {} failed.",
                self.running,
                self.total,
                self.failed.len()
            ))
            .bold(),
        ];
        match (&self.refresh_err, self.after) {
            (Some(e), _) => lines
                .push(Line::from(format!("Could not update {}: {e}", self.export.display())).red()),
            (None, Some(after)) => lines.push(Line::from(format!(
                "Now {} (was {}): freed {}.",
                human(after),
                human(self.before),
                human(self.before.saturating_sub(after))
            ))),
            _ => {}
        }
        lines.extend(self.failed.iter().map(|s| Line::from(s.as_str()).red()));
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    }
}
