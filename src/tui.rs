//! Terminal UI: browse your scan database directory by directory.
//!
//! The state machine (`App::on_key` / `App::tick`) is separate from drawing so it can be tested.

use crate::apply::{self, Verdict};
use crate::db::{self, Child, Db};
use crate::recipe::{self, Apply, Hit, Recipe, Selection};
use crate::report::{human, pct};
use crate::scan::Progress;
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::Line;
use ratatui::widgets::{Block, Paragraph, Row as TRow, Table, TableState, Wrap};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
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
    /// `Q`: the steps queued with `a`, to review, drop or run.
    Queue,
    /// Run every queued step?
    QueueConfirm,
    /// Quitting with steps still queued.
    QuitAsk,
}

/// Recipe results for one folder, in arrival order: (recipe index, result).
type Picks = Vec<(usize, Result<Selection, String>)>;

/// One queued step: a recipe's ticked hits, in the folder they were picked in. Running it
/// goes through `apply::run` like any other run, so every path is checked again then.
#[derive(Clone)]
struct Queued {
    recipe: Recipe,
    here: PathBuf,
    hits: Vec<Hit>,
    size: u64,
}

impl Queued {
    fn new(recipe: Recipe, here: PathBuf, hits: Vec<Hit>) -> Queued {
        let size = hits.iter().map(|h| h.size).sum();
        Queued {
            recipe,
            here,
            hits,
            size,
        }
    }
}

/// `p` lies strictly inside one of `dirs` (paths relative to the scan root): one lookup per
/// parent folder of `p`, whatever the size of `dirs`.
fn inside_any(p: &[u8], dirs: &HashSet<&[u8]>) -> bool {
    !dirs.is_empty()
        && p.iter()
            .enumerate()
            .any(|(i, &b)| b == b'/' && dirs.contains(&p[..i]))
}

/// Paths being queued or cleaned, indexed for quick "is this one of them, or inside one".
struct Marked<'a> {
    all: HashSet<&'a [u8]>,
    dirs: HashSet<&'a [u8]>,
    hits: &'a [Hit],
}

impl<'a> Marked<'a> {
    fn new(hits: &'a [Hit]) -> Marked<'a> {
        Marked {
            all: hits.iter().map(|h| h.path.as_slice()).collect(),
            dirs: hits
                .iter()
                .filter(|h| h.is_dir)
                .map(|h| h.path.as_slice())
                .collect(),
            hits,
        }
    }
}

/// Take `m` out of a recipe's results: hits that are, or lie inside, a marked path go.
/// `shrink` (deletes): a folder hit holding marked paths loses their size. Set lookups
/// only, so this is instant for millions of paths.
fn take_out(sel: &mut Selection, m: &Marked, shrink: bool) {
    if m.hits.is_empty() {
        return;
    }
    let before = sel.hits.len();
    sel.hits
        .retain(|h| !m.all.contains(h.path.as_slice()) && !inside_any(&h.path, &m.dirs));
    let mut changed = sel.hits.len() != before;
    if shrink {
        // Hits never nest (see recipe::select), so a marked path lies in at most one.
        let cut: Vec<(usize, u64)> = {
            let at: HashMap<&[u8], usize> = sel
                .hits
                .iter()
                .enumerate()
                .filter(|(_, h)| h.is_dir)
                .map(|(i, h)| (h.path.as_slice(), i))
                .collect();
            m.hits
                .iter()
                .filter_map(|h| {
                    h.path
                        .iter()
                        .enumerate()
                        .filter(|&(_, &b)| b == b'/')
                        .find_map(|(j, _)| at.get(&h.path[..j]).map(|&k| (k, h.size)))
                })
                .collect()
        };
        for (k, size) in cut {
            sel.hits[k].size = sel.hits[k].size.saturating_sub(size);
            changed = true;
        }
    }
    if changed {
        sel.count = sel.hits.len() as u64;
        sel.size = sel.hits.iter().map(|h| h.size).sum();
    }
}

/// Keep the `hits` of `recipe` not queued yet. A queued delete covers a path and all inside
/// it; a queued script covers only its own repeats (its files stay). A new delete absorbs
/// queued paths inside the folders it removes. Cost: one pass over the queue plus a few
/// set lookups per path, so `a` stays instant with millions of queued paths.
fn merge_new(queue: &mut Vec<Queued>, hits: Vec<Hit>, recipe: &Recipe) -> Vec<Hit> {
    let deleting = |q: &Queued| matches!(q.recipe.apply, Apply::Delete);
    let script = matches!(recipe.apply, Apply::Script(_));
    let hits: Vec<Hit> = {
        let gone: HashSet<&[u8]> = queue
            .iter()
            .filter(|q| deleting(q))
            .flat_map(|q| &q.hits)
            .map(|h| h.path.as_slice())
            .collect();
        let gone_dirs: HashSet<&[u8]> = queue
            .iter()
            .filter(|q| deleting(q))
            .flat_map(|q| &q.hits)
            .filter(|h| h.is_dir)
            .map(|h| h.path.as_slice())
            .collect();
        let same: HashSet<&[u8]> = queue
            .iter()
            .filter(|q| script && q.recipe.file == recipe.file)
            .flat_map(|q| &q.hits)
            .map(|h| h.path.as_slice())
            .collect();
        hits.into_iter()
            .filter(|h| {
                let p = h.path.as_slice();
                !gone.contains(p) && !inside_any(p, &gone_dirs) && !same.contains(p)
            })
            .collect()
    };
    let new_dirs: HashSet<&[u8]> = hits
        .iter()
        .filter(|h| h.is_dir && !script)
        .map(|h| h.path.as_slice())
        .collect();
    if !new_dirs.is_empty() {
        for q in queue.iter_mut() {
            let before = q.hits.len();
            q.hits.retain(|h| !inside_any(&h.path, &new_dirs));
            if q.hits.len() != before {
                q.size = q.hits.iter().map(|h| h.size).sum();
            }
        }
        queue.retain(|q| !q.hits.is_empty());
    }
    hits
}

/// The folders to rescan after running `dirs`: each once, minus those inside another.
fn tops(mut dirs: Vec<PathBuf>) -> Vec<PathBuf> {
    dirs.sort();
    dirs.dedup();
    let mut out: Vec<PathBuf> = Vec::new();
    for d in dirs {
        if !out.iter().any(|t| d.starts_with(t)) {
            out.push(d);
        }
    }
    out
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
    /// `x` also sends how many items of other people were left inside the removed folder.
    Applied(
        usize,
        Vec<(Vec<u8>, Verdict)>,
        io::Result<db::Refreshed>,
        usize,
    ),
    /// A queue run was stopped: the steps it did not reach go back in the queue.
    Requeue(Vec<Queued>),
}

/// The result of a run, for the Done screen.
struct Outcome {
    done: usize,
    problems: Vec<(Vec<u8>, Verdict)>,
    rescan: Option<String>,
    before: u64,
    after: u64,
    /// `x`: how many items of other people were left inside the removed folder.
    left: usize,
    /// A queue run: sizes are for the whole scan, paths shown in full.
    queue: bool,
}

/// `x`: the highlighted row being removed. `hit` is filled from the database refreshed in
/// this same flow, so `apply::run` never sees a row from an older scan.
struct Del {
    target: PathBuf,
    hit: Option<Hit>,
    files: u64,
}

/// Items inside `dir` not owned by `me`, not looking inside them: what removing `dir` leaves.
/// Other filesystems (mount points) are skipped; `apply::delete` reports those itself.
#[doc(hidden)]
pub fn left_by_others(dir: &Path, me: u32) -> usize {
    dir.symlink_metadata()
        .map_or(0, |m| count_others(dir, me, m.dev()))
}

fn count_others(dir: &Path, me: u32, dev: u64) -> usize {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return 0;
    };
    rd.flatten()
        .map(|e| match e.path().symlink_metadata() {
            Ok(m) if m.dev() != dev => 0,
            Ok(m) if m.uid() != me => 1,
            Ok(m) if m.is_dir() => count_others(&e.path(), me, dev),
            _ => 0,
        })
        .sum()
}

/// The Done line of `x` for `path`, from the run's problems and [`left_by_others`].
#[doc(hidden)]
pub fn removed_text(path: &Path, problems: &[(Vec<u8>, Verdict)], left: usize) -> String {
    let path = path.display();
    let others = |n: usize| {
        format!(
            "{} that {} to other people",
            plural(n, "item"),
            if n == 1 { "belongs" } else { "belong" }
        )
    };
    match problems.first() {
        None | Some((_, Verdict::Done)) => format!("Removed {path}."),
        Some((_, Verdict::Failed(w))) if w.starts_with(OTHERS) => format!(
            "Removed everything of yours; left {}: {}",
            others(left.max(1)),
            &w[OTHERS.len()..]
        ),
        Some((_, Verdict::Failed(w))) if w.starts_with(MOUNT) => {
            let mut t = format!(
                "Removed everything of yours on this disk; left another disk mounted inside it: {}",
                &w[MOUNT.len()..]
            );
            if left > 0 {
                t += &format!("; also left {}.", others(left));
            }
            t
        }
        Some((_, Verdict::Failed(w) | Verdict::Skipped(w))) => {
            format!("Did not remove {path}: {w}")
        }
    }
}

/// How `apply::delete` starts the verdicts for what it had to leave in place.
const OTHERS: &str = "contains files owned by other people: ";
const MOUNT: &str = "contains a mount point: ";

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
    pub rescan_skip_secs: u64,
    pub prune: Vec<String>,
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
    rescan_skip_secs: u64,
    prune: Vec<String>,
    /// Directories refreshed this session and when (see [`is_fresh`]).
    fresh: Vec<(PathBuf, Instant)>,
    /// The directory the running refresh covers.
    refreshing: Option<PathBuf>,
    /// Set when `c` lists from the saved scan without rescanning: says how old it is.
    saved_note: Option<String>,
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
    picks: Picks,
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
    del: Option<Del>,
    /// Steps queued with `a`, in order; run with `Q`, then `r`.
    queue: Vec<Queued>,
    /// The folder `picks` belong to. Finished results of folders checked earlier this
    /// session wait in `cache`, kept up to date as paths are queued or cleaned.
    picks_dir: Option<PathBuf>,
    cache: HashMap<PathBuf, Picks>,
    /// A direct run's recipe and paths: those cleaned come out of the results when it ends.
    ran: Option<(Recipe, Vec<Hit>)>,
    /// Queue view cursor.
    qsel: usize,
    /// The running (or last) run is the queue; `queue_before` is the scan's size before it.
    queue_run: bool,
    queue_before: u64,
}

/// True if `dir` lies inside a directory refreshed less than `secs` seconds ago.
fn is_fresh(fresh: &[(PathBuf, Instant)], dir: &Path, secs: u64) -> bool {
    fresh
        .iter()
        .any(|(p, at)| dir.starts_with(p) && at.elapsed().as_secs() < secs)
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
            rescan_skip_secs: 300,
            prune: Vec::new(),
            fresh: Vec::new(),
            refreshing: None,
            saved_note: None,
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
            del: None,
            queue: Vec::new(),
            picks_dir: None,
            cache: HashMap::new(),
            ran: None,
            qsel: 0,
            queue_run: false,
            queue_before: 0,
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
        app.rescan_skip_secs = l.rescan_skip_secs;
        app.prune = l.prune;
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
        let (root, db, uid, threads, prune) = (
            self.scan_root.clone(),
            self.db_path.clone(),
            self.uid,
            self.threads,
            self.prune.clone(),
        );
        let progress = self.progress.clone();
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        std::thread::spawn(move || {
            let res = db
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| db::scan_to_db_pruned(&root, &db, uid, threads, progress, &prune));
            let _ = tx.send(Msg::Scanned(res));
        });
    }

    /// Remove the half-written scan file if we quit mid-scan.
    pub fn cleanup(&self) {
        if self.mode == Mode::Scanning {
            let tmp = db::tmp_path(&self.db_path);
            let _ = std::fs::remove_file(db::journal_path(&tmp));
            let _ = std::fs::remove_file(tmp);
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
        if c.pruned {
            self.status = format!(
                "{} is pruned: its size is counted but its contents are not indexed.",
                String::from_utf8_lossy(&c.name)
            );
            return;
        }
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
    /// `C` (`rescan`) does that; `c` runs the recipes on the database as it is, without a rescan.
    /// Keep the finished results of the folder last checked, for `c` there again.
    fn park(&mut self) {
        if let Some(d) = self.picks_dir.take() {
            let p = std::mem::take(&mut self.picks);
            if p.len() == self.recipes.len() {
                self.cache.insert(d, p);
            }
        }
    }

    /// Kept results of `dir`, and of folders above or below it, no longer match the scan.
    fn invalidate(&mut self, dir: &Path) {
        self.cache
            .retain(|k, _| !(k.starts_with(dir) || dir.starts_with(k)));
    }

    /// Drop every kept result: they may hide paths that are still there.
    fn forget_all(&mut self) {
        self.cache.clear();
        self.picks.clear();
        self.picks_dir = None;
    }

    /// `hits`, now queued or cleaned by `recipe`, come out of every result this session:
    /// all recipes' for a delete, only `recipe`'s own for a script (the files stay).
    fn forget(&mut self, recipe: &Recipe, hits: &[Hit]) {
        let m = Marked::new(hits);
        let script = matches!(recipe.apply, Apply::Script(_));
        let recipes = &self.recipes;
        for list in std::iter::once(&mut self.picks).chain(self.cache.values_mut()) {
            for (i, res) in list.iter_mut() {
                if let Ok(sel) = res {
                    if !script {
                        take_out(sel, &m, true);
                    } else if recipes[*i].file == recipe.file {
                        take_out(sel, &m, false);
                    }
                }
            }
        }
    }

    fn start_clean(&mut self, rescan: bool) {
        let here = self.root.clone();
        self.park();
        if rescan {
            self.invalidate(&here);
        } else if let Some(p) = self.cache.remove(&here) {
            // Checked earlier this session: show those results, kept up to date, at once.
            self.del = None;
            self.picks = p;
            self.picks_dir = Some(here);
            self.msel = 0;
            self.saved_note = Some(
                "Results from earlier in this session, minus what you marked since. C runs the checks again."
                    .into(),
            );
            self.mode = Mode::Menu;
            return;
        }
        self.picks_dir = Some(here);
        self.del = None;
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
        let fresh = is_fresh(&self.fresh, &here, self.rescan_skip_secs);
        let skip = !rescan || fresh;
        // Without a rescan, sizes and folder hits come from the saved scan: say how old.
        // (Files are re-checked at delete time; folders are removed as they are on disk.)
        self.saved_note = (!rescan && !fresh).then(|| {
            let age = self
                .db
                .as_ref()
                .and_then(|d| d.meta().ok())
                .map(|m| SystemTime::UNIX_EPOCH + Duration::from_secs(m.scanned))
                .and_then(|t| SystemTime::now().duration_since(t).ok());
            match age {
                Some(a) => {
                    let when = match age_text(a) {
                        t if a.as_secs() < 60 => t,
                        t => format!("{t} ago"),
                    };
                    format!(
                        "Listed from your saved scan (full scan {when}). Esc, then C, rescans this folder first."
                    )
                }
                None => "Listed from your saved scan. Esc, then C, rescans this folder first.".into(),
            }
        });
        // Only a real rescan marks the folder as fresh (see on_refreshed).
        self.refreshing = rescan.then(|| here.clone());
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        std::thread::spawn(move || {
            // Safe: file hits are re-checked by mtime at delete time (apply.rs check_as) and
            // folder removal deletes what is on disk, so a stale list only affects what is shown.
            let res = if skip {
                Ok(db::Refreshed {
                    files: 0,
                    errors: 0,
                })
            } else {
                db::refresh(&db, std::slice::from_ref(&here), threads, progress)
            };
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

    /// `x`: rescan the highlighted row (a file: the folder it is in), then ask.
    fn start_remove(&mut self) {
        let Some(c) = self.children.get(self.sel) else {
            self.status = "Nothing to remove here.".into();
            return;
        };
        let name = Path::new(OsStr::from_bytes(&c.name));
        let target = self.root.join(name);
        let plain = name.components().count() == 1
            && matches!(
                name.components().next(),
                Some(std::path::Component::Normal(_))
            );
        if !plain || !target.starts_with(&self.scan_root) || target == self.scan_root {
            self.status = format!(
                "dume can only remove things inside {}, not that folder itself.",
                self.scan_root.display()
            );
            return;
        }
        if target.symlink_metadata().is_ok_and(|m| m.is_symlink()) {
            self.status = format!(
                "{} is a symlink; dume does not remove symlinks.",
                target.display()
            );
            return;
        }
        let scan = if c.is_dir {
            target.clone()
        } else {
            self.root.clone()
        };
        self.del = Some(Del {
            target,
            hit: None,
            files: 0,
        });
        self.progress = Arc::default();
        self.scan_started = Instant::now();
        self.mode = Mode::Refreshing;
        self.refreshing = Some(scan.clone());
        let (db, threads, progress) = (self.db_path.clone(), self.threads, self.progress.clone());
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Refreshed(db::refresh(&db, &[scan], threads, progress)));
        });
    }

    /// `x`, after the rescan: build the one hit from the refreshed database and ask.
    fn ask_remove(&mut self, mut d: Del) {
        self.rx = None;
        self.mode = Mode::List;
        let Some(db) = &self.db else {
            self.status = "The scan could not be opened, so nothing was removed.".into();
            return;
        };
        match db.entry(&d.target) {
            Ok(Some(e)) => {
                d.files = e.files;
                d.hit = Some(Hit {
                    path: e.rel,
                    size: e.size,
                    is_dir: e.is_dir,
                    mtime: e.mtime.map(|m| m as u64),
                });
                self.del = Some(d);
                self.mode = Mode::Confirm;
            }
            Ok(None) if d.target.symlink_metadata().is_err() => {
                self.status = format!("{} no longer exists.", d.target.display());
            }
            Ok(None) => {
                self.status = format!(
                    "None of your files are in {} any more; nothing to remove.",
                    d.target.display()
                );
            }
            Err(e) => self.status = format!("{}: {e}", d.target.display()),
        }
    }

    fn on_refreshed(&mut self, res: io::Result<db::Refreshed>) {
        let del = self.del.take();
        let covered = self.refreshing.take();
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
                // A skipped refresh must not extend its own window.
                if let Some(p) =
                    covered.filter(|p| !is_fresh(&self.fresh, p, self.rescan_skip_secs))
                {
                    self.fresh.retain(|(q, _)| *q != p);
                    self.fresh.push((p, Instant::now()));
                }
                self.reopen();
                match del {
                    Some(d) => self.ask_remove(d),
                    None => self.mode = Mode::Menu,
                }
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
            KeyCode::Enter | KeyCode::Char('a') if self.ticked.0 == 0 => {
                self.status = "Tick at least one path to continue.".into();
            }
            KeyCode::Enter => self.mode = Mode::Confirm,
            KeyCode::Char('a') => self.enqueue(),
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Menu,
            _ => {}
        }
    }

    /// The recipe and ticked hits a run or a queue step is made of: the `x` removal, or the
    /// chosen recipe's ticked paths.
    fn current_step(&self) -> (Recipe, Vec<Hit>) {
        if let Some(d) = &self.del {
            // `x`: one hit with `here` = the folder shown, so the same guards apply.
            let name = d.target.file_name().unwrap_or_default().to_string_lossy();
            let r = Recipe {
                name: format!("Remove {name}"),
                apply: Apply::Delete,
                sql: String::new(),
                file: PathBuf::new(),
            };
            (r, d.hit.clone().into_iter().collect())
        } else {
            let (r, sel) = self.chosen();
            // Only the ticked hits, in order: `apply::run` is what keeps them inside `here`.
            let hits: Vec<_> = sel
                .hits
                .iter()
                .zip(&self.ticks)
                .filter(|(_, t)| **t)
                .map(|(h, _)| h.clone())
                .collect();
            (r.clone(), hits)
        }
    }

    fn start_run(&mut self) {
        let (recipe, hits) = self.current_step();
        self.ran = Some((recipe.clone(), hits.clone()));
        let here = self.root.clone();
        let del = self.del.as_ref().map(|d| d.target.clone());
        let job = Queued::new(recipe, here.clone(), hits);
        self.run_jobs(vec![job], vec![here], del, false);
    }

    /// `a`: queue the current step instead of running it. Paths already queued (or inside a
    /// queued folder) are not added twice; queued paths inside a newly queued folder move
    /// into it. Back to the menu to pick more (`x`: back to the folder).
    fn enqueue(&mut self) {
        let (recipe, hits) = self.current_step();
        let name = recipe.name.clone();
        let total = hits.len();
        let hits = merge_new(&mut self.queue, hits, &recipe);
        self.forget(&recipe, &hits);
        let added = hits.len();
        let size: u64 = hits.iter().map(|h| h.size).sum();
        if added > 0 {
            self.queue
                .push(Queued::new(recipe, self.root.clone(), hits));
        }
        let again = match total - added {
            0 => String::new(),
            n => format!(" ({} already queued)", n_paths(n)),
        };
        self.status = format!(
            "Queued {} ({}) from {name}{again}. Q in the folder view shows the queue.",
            n_paths(added),
            human(size)
        );
        if self.del.take().is_some() {
            self.mode = Mode::List;
        } else {
            self.mode = Mode::Menu;
        }
    }

    /// `r`, `y` in the queue: run every queued step in order, then rescan each folder once.
    fn start_queue(&mut self) {
        let jobs = std::mem::take(&mut self.queue);
        let dirs = tops(jobs.iter().map(|q| q.here.clone()).collect());
        self.queue_before = self
            .db
            .as_ref()
            .and_then(|d| d.total_under(&self.scan_root).ok())
            .unwrap_or(0);
        self.del = None;
        self.run_jobs(jobs, dirs, None, true);
    }

    /// After a run: a recipe run started from the menu goes back to it (with what it cleaned
    /// taken out); `x` and queue runs go back to the folder.
    fn done_to_menu(&self) -> bool {
        self.del.is_none()
            && !self.queue_run
            && self.picks_dir.as_deref() == Some(self.root.as_path())
            && self.picks.len() == self.recipes.len()
            && is_dir(&self.root)
    }

    /// `q` or ctrl-c: with steps still queued, ask first.
    fn request_quit(&mut self) {
        if self.queue.is_empty() || self.mode == Mode::QuitAsk {
            self.quit = true;
        } else {
            self.mode = Mode::QuitAsk;
        }
    }

    fn run_jobs(
        &mut self,
        jobs: Vec<Queued>,
        refresh: Vec<PathBuf>,
        del: Option<PathBuf>,
        queue: bool,
    ) {
        self.cancel = Arc::default();
        let total: usize = jobs.iter().map(|j| j.hits.len()).sum();
        self.step = (0, total, String::new());
        self.outcome = None;
        self.queue_run = queue;
        self.mode = Mode::Running;
        let (db, root, bin, threads) = (
            self.db_path.clone(),
            self.scan_root.clone(),
            self.bin.clone(),
            self.threads,
        );
        let cancel = self.cancel.clone();
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        std::thread::spawn(move || {
            let (mut done, mut problems, mut back) = (0, Vec::new(), Vec::new());
            let mut base = 0;
            for j in &jobs {
                let verdicts = apply::run(
                    &j.recipe,
                    &root,
                    &j.here,
                    &j.hits,
                    bin.as_deref(),
                    &cancel,
                    &mut |i, h, v| {
                        if *v == Verdict::Done {
                            done += 1;
                        } else {
                            problems.push((h.path.clone(), v.clone()));
                        }
                        let p = String::from_utf8_lossy(&h.path).into_owned();
                        let _ = tx.send(Msg::Step(base + i + 1, total, p));
                    },
                );
                // Stopped with esc: what was not reached goes back in the queue.
                let rest: Vec<Hit> = j
                    .hits
                    .iter()
                    .zip(&verdicts)
                    .filter(|(_, v)| matches!(v, Verdict::Skipped(w) if w == "cancelled"))
                    .map(|(h, _)| h.clone())
                    .collect();
                if !rest.is_empty() {
                    back.push(Queued::new(j.recipe.clone(), j.here.clone(), rest));
                }
                base += j.hits.len();
            }
            let kept = |v: &Verdict| matches!(v, Verdict::Failed(w) if w.starts_with(OTHERS) || w.starts_with(MOUNT));
            let left = match &del {
                Some(t) if problems.iter().any(|(_, v)| kept(v)) => {
                    left_by_others(t, rustix::process::getuid().as_raw())
                }
                _ => 0,
            };
            let res = db::refresh(&db, &refresh, threads, Arc::default());
            if queue && !back.is_empty() {
                let _ = tx.send(Msg::Requeue(back));
            }
            let _ = tx.send(Msg::Applied(done, problems, res, left));
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
            self.request_quit();
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
                    KeyCode::Char('q') | KeyCode::Esc => self.request_quit(),
                    KeyCode::Char('N') => self.want_ncdu = true,
                    KeyCode::Char('Q') if self.queue.is_empty() => {
                        self.status = "Nothing queued yet. In a recipe's path list, a queues instead of cleaning now.".into();
                    }
                    KeyCode::Char('Q') => {
                        self.qsel = 0;
                        self.mode = Mode::Queue;
                    }
                    KeyCode::Char('c') if self.db.is_some() => self.start_clean(false),
                    KeyCode::Char('C') if self.db.is_some() => self.start_clean(true),
                    KeyCode::Char('x') if self.db.is_some() => self.start_remove(),
                    KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => self.open_dir(),
                    _ => {}
                }
            }
            Mode::Refreshing => {
                if matches!(code, KeyCode::Esc | KeyCode::Char('q')) {
                    // The scan can't be interrupted; its result is applied when it ends.
                    self.del = None;
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
                KeyCode::Char('a') => self.enqueue(),
                KeyCode::Esc | KeyCode::Char('n' | 'q') if self.del.take().is_some() => {
                    self.mode = Mode::List;
                }
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
                let menu = self.done_to_menu();
                self.del = None;
                self.mode = if menu { Mode::Menu } else { Mode::List };
            }
            Mode::Queue => {
                self.status.clear();
                if let Some(n) = Self::step(self.qsel, self.queue.len(), code) {
                    self.qsel = n;
                    return;
                }
                match code {
                    KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace => {
                        if self.qsel < self.queue.len() {
                            let q = self.queue.remove(self.qsel);
                            self.forget_all();
                            self.status = format!("Dropped {} from the queue.", q.recipe.name);
                            self.qsel = self.qsel.min(self.queue.len().saturating_sub(1));
                        }
                        if self.queue.is_empty() {
                            self.mode = Mode::List;
                        }
                    }
                    KeyCode::Char('r') | KeyCode::Enter => self.mode = Mode::QueueConfirm,
                    KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::List,
                    _ => {}
                }
            }
            Mode::QueueConfirm => match code {
                KeyCode::Char('y' | 'Y') => self.start_queue(),
                KeyCode::Esc | KeyCode::Char('n' | 'q') => self.mode = Mode::Queue,
                _ => {}
            },
            Mode::QuitAsk => match code {
                KeyCode::Char('r') => {
                    self.qsel = 0;
                    self.mode = Mode::Queue;
                }
                KeyCode::Char('q') => self.quit = true,
                KeyCode::Esc | KeyCode::Char('n') => self.mode = Mode::List,
                _ => {}
            },
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
                            self.forget_all();
                            self.fresh.clear();
                            self.fresh.push((self.scan_root.clone(), Instant::now()));
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
                Msg::Picked(i, mut res) => {
                    // Already queued: not shown again (scripts: only in their own recipe).
                    if let Ok(sel) = &mut res {
                        for q in &self.queue {
                            let script = matches!(q.recipe.apply, Apply::Script(_));
                            if !script || self.recipes[i].file == q.recipe.file {
                                take_out(sel, &Marked::new(&q.hits), !script);
                            }
                        }
                    }
                    self.picks.push((i, res));
                }
                Msg::Step(done, total, path) => self.step = (done, total, path),
                Msg::Requeue(back) => {
                    self.queue.splice(0..0, back);
                }
                Msg::Applied(done, problems, res, left) => {
                    self.rx = None;
                    if let Some((r, hits)) = self.ran.take() {
                        let bad: HashSet<&[u8]> =
                            problems.iter().map(|(p, _)| p.as_slice()).collect();
                        let gone: Vec<Hit> = hits
                            .into_iter()
                            .filter(|h| !bad.contains(h.path.as_slice()))
                            .collect();
                        self.forget(&r, &gone);
                    } else if problems
                        .iter()
                        .any(|(_, v)| !matches!(v, Verdict::Skipped(w) if w == "cancelled"))
                    {
                        // Queued paths were taken out when queued; some are still there.
                        self.forget_all();
                    }
                    let before = if self.queue_run {
                        self.queue_before
                    } else {
                        self.total
                    };
                    self.reopen();
                    let after = if self.queue_run {
                        self.db
                            .as_ref()
                            .and_then(|d| d.total_under(&self.scan_root).ok())
                            .unwrap_or(0)
                    } else {
                        self.total
                    };
                    self.status.clear();
                    self.outcome = Some(Outcome {
                        done,
                        problems,
                        rescan: res.err().map(|e| e.to_string()),
                        before,
                        after,
                        left,
                        queue: self.queue_run,
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
        let queued = match self.queue.len() {
            0 => String::new(),
            n => format!(" · Q queue ({n})"),
        };
        let help = match self.mode {
            Mode::Ask => "answer with the keys shown · q quit".into(),
            Mode::Scanning => "scanning — q quits and discards this scan".into(),
            Mode::List => format!(
                "↑↓ move · enter open · ← back · c clean · C rescan & clean · x remove · N ncdu{queued} · q quit"
            ),
            Mode::Refreshing => "esc back (the update finishes in the background)".into(),
            Mode::Menu => "↑↓ move · enter choose · esc back".into(),
            Mode::Paths => {
                "↑↓ move · space tick/untick · ctrl-a all/none · enter continue · a queue · esc back"
                    .into()
            }
            Mode::Confirm if self.del.is_some() => {
                "y yes, remove · a queue instead · n/esc cancel".into()
            }
            Mode::Confirm => "y yes, clean · a queue instead · n/esc back".into(),
            Mode::Running => "esc stop after the current path".into(),
            Mode::Done if self.done_to_menu() => "any key back to what to clean here".into(),
            Mode::Done => "any key back to the folder".into(),
            Mode::Queue => "↑↓ move · d drop step · r run all · esc back".into(),
            Mode::QueueConfirm => "y yes, run them all · n/esc back".into(),
            Mode::QuitAsk => "r review and run · q quit, dropping the queue · esc back".into(),
        };
        f.render_widget(
            Paragraph::new(vec![
                Line::from(help.as_str()),
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
            Mode::Queue => self.draw_queue(f, body),
            Mode::QueueConfirm => self.draw_queue_confirm(f, body),
            Mode::QuitAsk => self.draw_quit_ask(f, body),
        }
    }

    /// Where a queued step was picked, relative to the scan root.
    fn folder_of(&self, here: &Path) -> String {
        match here.strip_prefix(&self.scan_root) {
            Ok(r) if r.as_os_str().is_empty() => "(top folder)".into(),
            Ok(r) => r.display().to_string(),
            Err(_) => here.display().to_string(),
        }
    }

    fn queue_total(&self) -> (usize, u64) {
        self.queue
            .iter()
            .fold((0, 0), |(n, s), q| (n + q.hits.len(), s + q.size))
    }

    fn draw_queue(&mut self, f: &mut Frame, area: Rect) {
        let (n, size) = self.queue_total();
        let title = format!(
            " Queue: {}, {}, {} ",
            plural(self.queue.len(), "step"),
            n_paths(n),
            human(size)
        );
        let rows: Vec<TRow> = self
            .queue
            .iter()
            .map(|q| {
                TRow::new([
                    q.recipe.name.clone(),
                    self.folder_of(&q.here),
                    n_paths(q.hits.len()),
                    amount(&q.recipe.apply, q.size),
                ])
            })
            .collect();
        let widths = [
            Constraint::Min(20),
            Constraint::Min(20),
            Constraint::Length(14),
            Constraint::Length(16),
        ];
        f.render_stateful_widget(
            Table::new(rows, widths)
                .block(Block::bordered().title(title))
                .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
                .highlight_symbol("▶ "),
            area,
            &mut TableState::default().with_selected(Some(self.qsel)),
        );
    }

    fn draw_queue_confirm(&self, f: &mut Frame, area: Rect) {
        let (n, size) = self.queue_total();
        let mut lines = vec![
            Line::from(format!(
                "Run all {}: {} ({})? This cannot be undone.",
                plural(self.queue.len(), "queued step"),
                n_paths(n),
                human(size)
            ))
            .bold(),
            Line::from("Every path is checked again just before it is touched."),
            Line::from(""),
        ];
        for q in &self.queue {
            lines.push(Line::from(format!(
                "  {} in {}: {}",
                q.recipe.name,
                self.folder_of(&q.here),
                n_paths(q.hits.len())
            )));
        }
        if self.queue.iter().any(|q| q.here == self.scan_root) {
            lines.push(Line::from(""));
            lines.push(
                Line::from("Some steps were picked in the top folder of the whole scan.")
                    .bold()
                    .red(),
            );
        }
        lines.push(Line::from(""));
        lines.push(Line::from("[y] yes, run them all   [n] no, go back"));
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(Block::bordered().title(" Are you sure? ")),
            area,
        );
    }

    fn draw_quit_ask(&self, f: &mut Frame, area: Rect) {
        let (n, size) = self.queue_total();
        let lines = vec![
            Line::from(format!(
                "You have {} that did not run: {} ({}).",
                plural(self.queue.len(), "queued step"),
                n_paths(n),
                human(size)
            ))
            .bold(),
            Line::from("The queue is not saved: quitting now drops it."),
            Line::from(""),
            Line::from("[r] review and run it   [q] quit anyway   [esc] go back"),
        ];
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(Block::bordered().title(" Quit? ")),
            area,
        );
    }

    fn spinner(&self) -> &'static str {
        SPIN[(self.scan_started.elapsed().as_millis() / 150) as usize % SPIN.len()]
    }

    fn draw_refreshing(&self, f: &mut Frame, area: Rect) {
        let (what, then, title) = match &self.del {
            Some(d) => (
                &d.target,
                "Then dume asks before removing anything.",
                " Remove ",
            ),
            None => (
                &self.root,
                "Then dume looks for what can be cleaned here.",
                " Clean this folder ",
            ),
        };
        let lines = vec![
            Line::from(format!(
                "{} Updating the scan of {} …",
                self.spinner(),
                what.display()
            ))
            .bold(),
            Line::from(format!(
                "{} of your files found · {}s",
                self.progress.files.load(Ordering::Relaxed),
                self.scan_started.elapsed().as_secs()
            )),
            Line::from(then),
        ];
        f.render_widget(
            Paragraph::new(lines).block(Block::bordered().title(title)),
            area,
        );
    }

    fn draw_menu(&mut self, f: &mut Frame, area: Rect) {
        let block = Block::bordered().title(" What to clean here ");
        let inner = block.inner(area);
        f.render_widget(block, area);
        let list = inner;
        // One row per recipe, in order, with how far its check is; only rows that found
        // something can be chosen, and the cursor skips the rest.
        let shown = self.shown();
        let busy = self.computing();
        let spin = self.spinner();
        let dim = Style::new().add_modifier(Modifier::DIM);
        let rows: Vec<TRow> = self
            .recipes
            .iter()
            .enumerate()
            .map(|(k, r)| match self.picks.get(k) {
                Some((_, Ok(s))) if !s.hits.is_empty() => TRow::new([
                    r.name.clone(),
                    n_paths(s.hits.len()),
                    amount(&r.apply, s.size),
                ]),
                Some((_, Ok(_))) => {
                    TRow::new([r.name.clone(), "none found".into(), String::new()]).style(dim)
                }
                Some((_, Err(_))) => {
                    TRow::new([r.name.clone(), "failed".into(), String::new()]).yellow()
                }
                None if busy && k == self.picks.len() => {
                    TRow::new([r.name.clone(), format!("{spin} looking…"), String::new()])
                }
                None if busy => {
                    TRow::new([r.name.clone(), "waiting".into(), String::new()]).style(dim)
                }
                None => TRow::new([r.name.clone(), "not checked".into(), String::new()]).style(dim),
            })
            .collect();
        // Failures are spelled out below the rows, one line each, and cannot be chosen.
        let mut notes: Vec<Line> = Vec::new();
        if let Some(n) = &self.saved_note {
            notes.push(Line::from(n.clone()).style(dim));
        }
        if shown.is_empty() && !busy {
            notes.push(Line::from("Nothing to clean here.").bold());
            notes.push(Line::from(
                "Go back (esc) and try another folder, or a folder above.",
            ));
        }
        notes.extend(self.picks.iter().filter_map(|(i, res)| {
            let e = res.as_ref().err()?;
            Some(Line::from(format!("recipe {} failed: {e}", self.recipes[*i].name)).yellow())
        }));
        let [list, errs] = Layout::vertical([
            Constraint::Length(rows.len().max(1) as u16),
            Constraint::Min(0),
        ])
        .areas(list);
        f.render_widget(Paragraph::new(notes).wrap(Wrap { trim: false }), errs);
        self.msel = self.msel.min(shown.len().saturating_sub(1));
        // The cursor row is the recipe's own row: picks arrive in recipe order.
        let cursor = shown.get(self.msel).map(|&i| self.picks[i].0);
        // shortcut: the menu is not windowed; there are only as many lines as recipes.
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
            &mut TableState::default().with_selected(cursor),
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
        if let Some(d) = &self.del {
            let size = human(d.hit.as_ref().map_or(0, |h| h.size));
            let dir = d.hit.as_ref().is_some_and(|h| h.is_dir);
            let mut lines = vec![Line::from(format!("Remove {}?", d.target.display())).bold()];
            if dir {
                lines.push(Line::from(format!(
                    "{} ({size}). Anything else of yours inside goes too.",
                    plural(d.files as usize, "file")
                )));
                lines.push(Line::from(
                    "Files owned by other people inside it are left alone.",
                ));
            } else {
                lines.push(Line::from(format!("1 file ({size}).")));
            }
            lines.push(Line::from(
                "This cannot be undone. Press y to remove, n or esc to cancel.",
            ));
            f.render_widget(
                Paragraph::new(lines)
                    .wrap(Wrap { trim: false })
                    .block(Block::bordered().title(" Are you sure? ")),
                area,
            );
            return;
        }
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
        if let Some(d) = &self.del {
            let line = Line::from(format!("Removing {} …", d.target.display())).bold();
            f.render_widget(
                Paragraph::new(line).block(Block::bordered().title(" Removing ")),
                area,
            );
            return;
        }
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
                Line::from(if self.del.is_some() {
                    "The list may still show removed items; press c to rescan this folder."
                } else {
                    "Press c again or rescan to refresh."
                }),
            ],
            None => vec![
                Line::from(match o.after.cmp(&o.before) {
                    std::cmp::Ordering::Less => format!(
                        "Freed {} {} (was {}, now {}).",
                        human(o.before - o.after),
                        if o.queue { "in total" } else { "here" },
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
        if let Some(d) = &self.del {
            lines.push(Line::from(removed_text(&d.target, &o.problems, o.left)));
            f.render_widget(
                Paragraph::new(lines)
                    .wrap(Wrap { trim: false })
                    .block(Block::bordered().title(" Done ")),
                area,
            );
            return;
        }
        let stopped = o
            .problems
            .iter()
            .filter(|(_, v)| matches!(v, Verdict::Skipped(w) if w == "cancelled"))
            .count();
        if stopped > 0 {
            lines.insert(
                0,
                Line::from(format!(
                    "You stopped the run: {} {} not touched{}.",
                    n_paths(stopped),
                    if stopped == 1 { "was" } else { "were" },
                    if o.queue {
                        " (they are back in the queue)"
                    } else {
                        ""
                    }
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
            let shown = if o.queue {
                String::from_utf8_lossy(p).into_owned()
            } else {
                self.rel_here(p)
            };
            lines.push(Line::from(format!("{what}: {shown}: {why}")));
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
                    if c.pruned {
                        format!("{name}/ (not indexed)")
                    } else if c.is_dir {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn h(p: &str, is_dir: bool) -> Hit {
        Hit {
            path: p.as_bytes().to_vec(),
            size: 10,
            is_dir,
            mtime: None,
        }
    }

    fn del() -> Recipe {
        Recipe {
            name: "r".into(),
            apply: Apply::Delete,
            sql: String::new(),
            file: PathBuf::from("del.sql"),
        }
    }

    fn ansa() -> Recipe {
        Recipe {
            name: "s".into(),
            apply: Apply::Script(PathBuf::from("ansa.sh")),
            sql: String::new(),
            file: PathBuf::from("ansa.sql"),
        }
    }

    fn step(hits: Vec<Hit>) -> Queued {
        Queued::new(del(), PathBuf::from("/x"), hits)
    }

    fn paths(v: &[Hit]) -> Vec<&str> {
        v.iter()
            .map(|h| std::str::from_utf8(&h.path).unwrap())
            .collect()
    }

    #[test]
    fn merge_new_skips_queued_and_moves_paths_into_a_new_folder() {
        let mut q = vec![step(vec![h("a/x", false), h("b", true), h("c/1", false)])];
        let kept = merge_new(
            &mut q,
            vec![
                h("a/x", false),      // queued
                h("b/deep/f", false), // inside a queued folder
                h("bb", false),       // a sibling with a common prefix: new
                h("c", true),         // a folder holding a queued path: absorbs it
                h("d", false),        // new
            ],
            &del(),
        );
        assert_eq!(paths(&kept), ["bb", "c", "d"]);
        assert_eq!(paths(&q[0].hits), ["a/x", "b"]);
        assert_eq!(q[0].size, 20);
        // A step whose paths all moved into a new folder disappears.
        let mut q = vec![step(vec![h("c/1", false)]), step(vec![h("z", false)])];
        merge_new(&mut q, vec![h("c", true)], &del());
        assert_eq!(q.len(), 1);
        assert_eq!(paths(&q[0].hits), ["z"]);
    }

    #[test]
    fn merge_new_treats_scripts_as_keeping_their_files() {
        let s = |hits| Queued::new(ansa(), PathBuf::from("/x"), hits);
        // Queued for the script: the same script again is a repeat, a delete is not.
        let mut q = vec![s(vec![h("m.ansa", false)])];
        assert!(merge_new(&mut q, vec![h("m.ansa", false)], &ansa()).is_empty());
        assert_eq!(
            paths(&merge_new(&mut q, vec![h("m.ansa", false)], &del())),
            ["m.ansa"]
        );
        // Queued for deletion: nothing more to do to it, script or not.
        let mut q = vec![step(vec![h("d", true)])];
        assert!(merge_new(&mut q, vec![h("d/m.ansa", false)], &ansa()).is_empty());
        // A script on a folder does not absorb queued paths inside it.
        let mut q = vec![step(vec![h("e/1", false)])];
        merge_new(&mut q, vec![h("e", true)], &ansa());
        assert_eq!(paths(&q[0].hits), ["e/1"]);
    }

    #[test]
    fn take_out_removes_marked_and_shrinks_folders_holding_them() {
        let sel = |hits: Vec<Hit>| Selection {
            count: hits.len() as u64,
            size: hits.iter().map(|h| h.size).sum(),
            hits,
        };
        let mut a = sel(vec![
            h("t/100", true),
            h("t/200", true),
            h("u/x.vtk", false),
        ]);
        a.hits[0].size = 100;
        let marked = vec![h("t/100/x.vtk", false), h("u", true)];
        let m = Marked::new(&marked);
        let mut b = a.clone();
        take_out(&mut a, &m, true);
        // u/x.vtk is inside the marked folder u; t/100 holds a marked 10-byte file.
        assert_eq!(paths(&a.hits), ["t/100", "t/200"]);
        assert_eq!((a.hits[0].size, a.count, a.size), (90, 2, 100));
        // Scripts keep their files: no shrinking.
        take_out(&mut b, &m, false);
        assert_eq!((b.hits[0].size, b.count, b.size), (100, 2, 110));
    }

    #[test]
    fn merge_new_is_fast_with_huge_queues() {
        // The old check compared every new path with every queued one: 10 billion pairs here.
        let n = 100_000;
        let mut q = vec![step(
            (0..n)
                .map(|i| h(&format!("p/d{}/f{i}", i % 1000), false))
                .collect(),
        )];
        let new: Vec<Hit> = (0..n)
            .map(|i| h(&format!("p/d{}/f{}", i % 1000, i + n / 2), false))
            .chain((0..10).map(|i| h(&format!("p/d{i}"), true)))
            .collect();
        let t = std::time::Instant::now();
        let kept = merge_new(&mut q, new, &del());
        assert!(t.elapsed().as_secs() < 10, "took {:?}", t.elapsed());
        // The 10 new folders are kept, and absorb the queued files inside them (1 in 100).
        let in_new_dirs = |p: &[u8]| (0..10).any(|i| p.starts_with(format!("p/d{i}/").as_bytes()));
        assert_eq!(kept.iter().filter(|h| h.is_dir).count(), 10);
        assert!(q[0].hits.iter().all(|h| !in_new_dirs(&h.path)));
        assert_eq!(q[0].hits.len(), n - n / 100);
        // Of the new files, the first half were queued already; the second half are new.
        assert_eq!(kept.iter().filter(|h| !h.is_dir).count(), n / 2);
    }

    #[test]
    fn fresh_covers_descendants_within_window() {
        let f = [(PathBuf::from("/a"), Instant::now())];
        assert!(is_fresh(&f, Path::new("/a/b"), 300));
        assert!(!is_fresh(&f, Path::new("/c"), 300));
        assert!(!is_fresh(&f, Path::new("/a/b"), 0));
    }
}
