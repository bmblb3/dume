//! `dume-scan --bench`: where the time goes, on a real database.
//!
//! Read-only. Times a scan of the folder (nothing is written), then every recipe in `actions`
//! one after another and again on a pool, so the cost of `c` can be read off before anything
//! is optimised.

use crate::db::Db;
use crate::recipe::{self, Selection};
use crate::scan::{self, Prune};
use crate::tui::age_text;
use rayon::prelude::*;
use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

/// Long enough to learn the real time of a slow recipe (the app stops at 30 s).
const LIMIT: Duration = Duration::from_secs(600);

fn secs(d: Duration) -> String {
    format!("{:.2} s", d.as_secs_f64())
}

fn describe(r: &Result<Selection, String>) -> (String, String) {
    match r {
        Ok(s) => (format!("{} paths", s.hits.len()), "ok".into()),
        Err(e) if e.contains("took too long") => ("-".into(), "timed out".into()),
        Err(e) => ("-".into(), format!("failed: {e}")),
    }
}

/// Print timings for `dir` (inside the database `db`). `parallel` is the pool size for the
/// second recipe pass, `threads` the scan threads.
pub fn run(
    db: &Path,
    dir: &Path,
    actions: Option<&Path>,
    parallel: usize,
    threads: usize,
    out: &mut impl Write,
) -> io::Result<()> {
    let d = Db::open_ro(db)?;
    let m = d.meta()?;
    let scanned = SystemTime::UNIX_EPOCH + Duration::from_secs(m.scanned);
    let age = SystemTime::now()
        .duration_since(scanned)
        .unwrap_or_default();
    let when = match age_text(age) {
        t if t == "just now" => t,
        t => format!("{t} ago"),
    };
    writeln!(out, "database: {}  scanned {when}", m.root.display())?;
    writeln!(
        out,
        "folder:   {}  rows in view: {}",
        dir.display(),
        d.count_under(dir)?
    )?;

    let prune = Prune::new(&m.prune)
        .map_err(io::Error::other)?
        .with_root(dir != m.root);
    let t = Instant::now();
    let (s, _) = scan::scan_pruned(dir, m.uid, threads, false, Default::default(), true, prune)?;
    writeln!(
        out,
        "scan only (no write): {}, {} files, {} errors",
        secs(t.elapsed()),
        s.files,
        s.errors
    )?;

    let Some(actions) = actions else {
        return Ok(());
    };
    let (recipes, skipped) = recipe::load(actions)?;
    for s in skipped {
        writeln!(out, "skipped: {s}")?;
    }
    writeln!(
        out,
        "\n{:<34} {:>10}  {:>14}  status",
        "recipe", "time", "result"
    )?;
    let start = Instant::now();
    let mut first = Vec::new();
    for r in &recipes {
        let t = Instant::now();
        let res = recipe::select_with_timeout(&d, r, dir, LIMIT);
        let (n, status) = describe(&res);
        writeln!(
            out,
            "{:<34} {:>10}  {:>14}  {status}",
            r.name,
            secs(t.elapsed()),
            n
        )?;
        first.push(res);
    }
    writeln!(out, "sequential total: {}", secs(start.elapsed()))?;

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(parallel)
        .build()
        .map_err(io::Error::other)?;
    let start = Instant::now();
    let second: Vec<_> = pool.install(|| {
        recipes
            .par_iter()
            .map(|r| {
                let db = Db::open_ro(db).map_err(|e| e.to_string())?;
                recipe::select_with_timeout(&db, r, dir, LIMIT)
            })
            .collect()
    });
    writeln!(
        out,
        "parallel ({parallel} threads): wall {}",
        secs(start.elapsed())
    )?;
    for ((r, a), b) in recipes.iter().zip(&first).zip(&second) {
        let same = match (a, b) {
            (Ok(a), Ok(b)) => a.hits == b.hits,
            (Err(a), Err(b)) => a == b,
            _ => false,
        };
        if !same {
            writeln!(out, "MISMATCH: {} differs between the two passes", r.name)?;
        }
    }
    Ok(())
}
