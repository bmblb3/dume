use clap::Parser;
use dumelib::{db, report, scan};
use std::io::{BufWriter, IsTerminal};
use std::path::PathBuf;
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
    #[arg(required_unless_present = "json")]
    dir: Option<PathBuf>,
    /// Number of concurrent threads
    #[arg(short = 'j', long, default_value_t = 32, value_parser = clap::value_parser!(u16).range(1..))]
    threads: u16,
    /// Owner uid to keep (default: your own uid)
    #[arg(short, long)]
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
    /// Rows per section in --report
    #[arg(short = 'n', long, default_value_t = 15, requires = "report")]
    top: usize,
    /// Print each unreadable path to stderr (default: only a count)
    #[arg(short, long)]
    verbose: bool,
}

fn main() -> ExitCode {
    // clap exits with 2 on usage errors.
    let cli = Cli::parse();

    if cli.merge.is_none()
        && cli.report.is_none()
        && cli.output.is_none()
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
    let dir = cli.dir.as_ref().expect("clap requires DIR without --json");
    let progress: std::sync::Arc<scan::Progress> = Default::default();
    if let Some(file) = &cli.report {
        let d = db::Db::open_ro(file)?;
        let r = report::analyse_db(&d, &dir.canonicalize()?, false)?;
        return r.write_text(
            &dir.to_string_lossy(),
            cli.top,
            &mut std::io::stdout().lock(),
        );
    }
    if let Some(file) = &cli.merge {
        let uid = db::Db::open_ro(file)?.meta()?.uid;
        let r = db::refresh(file, &[dir.canonicalize()?], cli.threads as usize, progress)?;
        return summary(r.files, r.errors, uid);
    }
    let uid = cli
        .uid
        .unwrap_or_else(|| rustix::process::getuid().as_raw());
    let wrap = |e: std::io::Error| std::io::Error::new(e.kind(), format!("{}: {e}", dir.display()));
    if let Some(file) = &cli.output {
        let n =
            db::scan_to_db(dir, file, uid, cli.threads as usize, progress.clone()).map_err(wrap)?;
        return summary(n, progress_errors(&progress), uid);
    }
    let (scan, dev) = scan::scan(dir, uid, cli.threads as usize, cli.verbose).map_err(wrap)?;
    let mut out = BufWriter::with_capacity(1 << 20, std::io::stdout().lock());
    let root = dir.to_string_lossy();
    scan.write(&mut out, &root, dev, uid, env!("CARGO_PKG_VERSION"))?;
    summary(scan.files, scan.errors, uid)
}

fn progress_errors(p: &scan::Progress) -> u64 {
    p.errors.load(std::sync::atomic::Ordering::Relaxed)
}

fn summary(files: u64, errors: u64, uid: u32) -> std::io::Result<()> {
    eprintln!("dume-scan: {files} files for uid {uid}");
    if errors > 0 {
        eprintln!("dume-scan: warning: {errors} entries could not be read (use -v to list)");
    }
    Ok(())
}
