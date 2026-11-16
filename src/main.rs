use clap::Parser;
use dumelib::{bench, db, recipe, report, scan};
use std::ffi::OsStr;
use std::io::{BufWriter, IsTerminal, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Export only one user's regular files as ncdu JSON.
///
/// Usage: dume-scan /data > dume.json && ncdu -f dume.json
#[derive(Parser)]
#[command(name = "dume-scan", version, max_term_width = 100)]
struct Cli {
    /// Directory to scan (never crosses into other filesystems or follows symlinks)
    #[arg(required_unless_present_any = ["json", "find"])]
    dir: Option<PathBuf>,
    /// Number of concurrent threads
    #[arg(short = 'j', long, default_value_t = 32, value_parser = clap::value_parser!(u16).range(1..))]
    threads: u16,
    /// Owner uid to keep (default: your own uid)
    #[arg(short, long, conflicts_with_all = ["merge", "report", "json", "find"])]
    uid: Option<u32>,
    /// Scan DIR into the database FILE instead of printing JSON
    #[arg(short, long, value_name = "FILE", conflicts_with_all = ["merge", "report", "json"])]
    output: Option<PathBuf>,
    /// Print the database FILE as ncdu JSON (DIR not needed)
    #[arg(long, value_name = "FILE", conflicts_with_all = ["merge", "report"])]
    json: Option<PathBuf>,
    /// Rescan DIR (a directory inside the database FILE) and update FILE in place
    #[arg(short, long, value_name = "FILE")]
    merge: Option<PathBuf>,
    /// Don't scan: print what's taking space under DIR (a directory inside the database FILE)
    #[arg(short, long, value_name = "FILE", conflicts_with = "merge")]
    report: Option<PathBuf>,
    /// Don't scan: time a scan of DIR and the recipes in --actions, using the database FILE
    #[arg(long, value_name = "FILE", conflicts_with_all = ["merge", "report", "json", "output"])]
    bench: Option<PathBuf>,
    /// With --bench: the recipes directory to time (*.sql)
    #[arg(long, value_name = "DIR", requires = "bench")]
    actions: Option<PathBuf>,
    /// With --bench: recipes evaluated at once in the parallel pass
    #[arg(long, default_value_t = 4, requires = "bench", value_parser = clap::value_parser!(u16).range(1..))]
    parallel: u16,
    /// Don't scan: list the files under DIR [default: .] whose name matches REGEX, newest first
    #[arg(long, num_args = 2, value_names = ["REGEX", "FILE"],
          conflicts_with_all = ["merge", "report", "json", "output", "bench"])]
    find: Vec<String>,
    /// Rows per section in --report
    #[arg(short = 'n', long, default_value_t = 15, requires = "report")]
    top: usize,
    /// With -o: directory name glob to size but not store, e.g. '.git' (repeatable)
    #[arg(long, value_name = "GLOB", requires = "output")]
    prune: Vec<String>,
    /// Print each unreadable path to stderr (default: only a count)
    #[arg(short, long)]
    verbose: bool,
}

fn main() -> ExitCode {
    // clap exits with 2 on usage errors.
    let cli = Cli::parse();

    if cli.merge.is_none()
        && cli.bench.is_none()
        && cli.report.is_none()
        && cli.output.is_none()
        && cli.find.is_empty()
        && std::io::stdout().is_terminal()
    {
        eprintln!(
            "dume-scan: refusing to write JSON to a terminal; pipe it, e.g. `dume-scan DIR | ncdu -f-` or `dume-scan DIR > dume.json`"
        );
        return ExitCode::from(2);
    }

    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::BrokenPipe {
                eprintln!("dume-scan: {e}");
            }
            ExitCode::from(1)
        }
    }
}

fn run(cli: &Cli) -> std::io::Result<()> {
    if let Some(file) = &cli.json {
        let mut out = BufWriter::with_capacity(1 << 20, std::io::stdout().lock());
        return db::Db::open_ro(file)?.write_json(&mut out);
    }
    let cwd = PathBuf::from(".");
    let given = match &cli.dir {
        Some(d) => d,
        None if !cli.find.is_empty() => &cwd,
        None => unreachable!("clap requires DIR without --json or --find"),
    };
    let fail =
        |e: std::io::Error| std::io::Error::new(e.kind(), format!("{}: {e}", given.display()));
    // dume never follows symlinks, DIR itself included: name the real directory instead.
    if given.symlink_metadata().map_err(fail)?.is_symlink() {
        let real = given.canonicalize().map_err(fail)?;
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "{} is a symlink; dume does not follow symlinks. Give the real directory: {}",
                given.display(),
                real.display()
            ),
        ));
    }
    // Absolute and resolved, so a database made with a relative DIR is found again by
    // `-m`/`-r` from anywhere. (Symlinks above DIR were always followed by the open.)
    let dir = &given.canonicalize().map_err(fail)?;
    let progress: std::sync::Arc<scan::Progress> = Default::default();
    if let Some(file) = &cli.report {
        let d = db::Db::open_ro(file)?;
        let r = report::analyse_db(&d, dir, false)?;
        return r.write_text(
            &given.to_string_lossy(),
            cli.top,
            &mut std::io::stdout().lock(),
        );
    }
    if let [re, file] = &cli.find[..] {
        let d = db::Db::open_ro(Path::new(file))?;
        let mut out = BufWriter::new(std::io::stdout().lock());
        return find(&d, dir, re, &mut out);
    }
    if let Some(file) = &cli.bench {
        return bench::run(
            file,
            dir,
            cli.actions.as_deref(),
            cli.parallel as usize,
            cli.threads as usize,
            &mut std::io::stdout().lock(),
        );
    }
    if let Some(file) = &cli.merge {
        let uid = db::Db::open_ro(file)?.meta()?.uid;
        let r = db::refresh(
            file,
            std::slice::from_ref(dir),
            cli.threads as usize,
            progress,
        )?;
        return summary(r.files, r.errors, uid, false);
    }
    let uid = cli
        .uid
        .unwrap_or_else(|| rustix::process::getuid().as_raw());
    if let Some(file) = &cli.output {
        let n = db::scan_to_db_pruned(
            dir,
            file,
            uid,
            cli.threads as usize,
            progress.clone(),
            &cli.prune,
        )
        .map_err(fail)?;
        return summary(n, progress_errors(&progress), uid, false);
    }
    let (scan, dev) = scan::scan(dir, uid, cli.threads as usize, cli.verbose).map_err(fail)?;
    let mut out = BufWriter::with_capacity(1 << 20, std::io::stdout().lock());
    // ncdu shows this at the top: the directory as the user named it.
    let root = given.to_string_lossy();
    scan.write(&mut out, &root, dev, uid, env!("CARGO_PKG_VERSION"))?;
    summary(scan.files, scan.errors, uid, true)
}

/// `--find`: files under `dir` whose name matches `re`, newest first (unknown mtime last).
/// Runs as a recipe, so it gets the same read-only, scoped, time-limited query.
fn find(d: &db::Db, dir: &Path, re: &str, out: &mut impl Write) -> std::io::Result<()> {
    regex::bytes::Regex::new(re).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("--find: {e}"))
    })?;
    let r = recipe::Recipe {
        name: "--find".into(),
        apply: recipe::Apply::Delete,
        sql: format!(
            "SELECT path FROM entries WHERE NOT is_dir AND name REGEXP '{}'",
            re.replace('\'', "''")
        ),
        file: PathBuf::new(),
    };
    let mut hits = recipe::select(d, &r, dir)
        .map_err(|e| std::io::Error::other(format!("--find: {e}")))?
        .hits;
    hits.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| a.path.cmp(&b.path)));
    let root = d.meta()?.root;
    let mut total = 0;
    for h in &hits {
        total += h.size;
        let when = h.mtime.map_or_else(|| "-".into(), utc);
        write!(out, "{when:<16}  {:>10}  ", report::human(h.size))?;
        out.write_all(root.join(OsStr::from_bytes(&h.path)).as_os_str().as_bytes())?;
        out.write_all(b"\n")?;
    }
    out.flush()?;
    let n = hits.len();
    eprintln!(
        "dume-scan: {n} {}, {} (times in UTC)",
        if n == 1 { "file" } else { "files" },
        report::human(total)
    );
    Ok(())
}

/// `secs` since the epoch as `YYYY-MM-DD HH:MM` in UTC (civil_from_days, H. Hinnant).
fn utc(secs: u64) -> String {
    let (days, rem) = ((secs / 86_400) as i64 + 719_468, secs % 86_400);
    let (era, doe) = (days.div_euclid(146_097), days.rem_euclid(146_097));
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60
    )
}

fn progress_errors(p: &scan::Progress) -> u64 {
    p.errors.load(std::sync::atomic::Ordering::Relaxed)
}

fn summary(files: u64, errors: u64, uid: u32, hint: bool) -> std::io::Result<()> {
    eprintln!("dume-scan: {files} files for uid {uid}");
    if errors > 0 {
        let h = if hint { " (use -v to list)" } else { "" };
        eprintln!("dume-scan: warning: {errors} entries could not be read{h}");
    }
    Ok(())
}
