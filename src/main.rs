mod json;
mod merge;
mod scan;

use clap::Parser;
use std::io::{BufWriter, IsTerminal};
use std::path::PathBuf;
use std::process::ExitCode;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Export only one user's regular files as ncdu JSON.
///
/// Usage: dume /data > dume.json && ncdu -f dume.json
#[derive(Parser)]
#[command(version, max_term_width = 100)]
struct Cli {
    /// Directory to scan (never crosses into other filesystems or follows symlinks)
    dir: PathBuf,
    /// Number of concurrent threads
    #[arg(short = 'j', long, default_value_t = 32, value_parser = clap::value_parser!(u16).range(1..))]
    threads: u16,
    /// Owner uid to keep (default: your own uid)
    #[arg(short, long)]
    uid: Option<u32>,
    /// Rescan DIR (a directory inside an existing export) and splice it into FILE in place
    #[arg(short, long, value_name = "FILE")]
    merge: Option<PathBuf>,
    /// Print each unreadable path to stderr (default: only a count)
    #[arg(short, long)]
    verbose: bool,
}

fn main() -> ExitCode {
    // clap exits with 2 on usage errors.
    let cli = Cli::parse();

    if cli.merge.is_none() && std::io::stdout().is_terminal() {
        eprintln!(
            "dume: refusing to write JSON to a terminal; pipe it, e.g. `dume DIR | ncdu -f-` or `dume DIR > dume.json`"
        );
        return ExitCode::from(2);
    }

    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::BrokenPipe {
                eprintln!("dume: {e}");
            }
            ExitCode::from(1)
        }
    }
}

fn run(cli: &Cli) -> std::io::Result<()> {
    let err = |m: String| std::io::Error::new(std::io::ErrorKind::InvalidInput, m);
    let mut export = cli.merge.as_deref().map(merge::Export::load).transpose()?;
    let file_uid = export.as_ref().map(|e| e.uid()).transpose()?;
    if let (Some(a), Some(b)) = (cli.uid, file_uid)
        && a != b
    {
        return Err(err(format!("export is for uid {b}, not {a}")));
    }
    let uid = cli
        .uid
        .or(file_uid)
        .unwrap_or_else(|| rustix::process::getuid().as_raw());

    let (scan, dev) = scan::scan(&cli.dir, uid, cli.threads as usize, cli.verbose)
        .map_err(|e| std::io::Error::new(e.kind(), format!("{}: {e}", cli.dir.display())))?;

    if let (Some(export), Some(file)) = (export.as_mut(), cli.merge.as_deref()) {
        if export.dev().is_some_and(|d| d != dev) {
            return Err(err(
                "DIR is on a different filesystem than the export's root".into(),
            ));
        }
        let dir = cli.dir.canonicalize()?;
        let rel = export.relative(&dir)?;
        let name = rel.last().map_or("", |s| s.as_str());
        let mut tree = Vec::new();
        scan.write_tree(&mut tree, name, None)?;
        let tree = serde_json::from_slice(&tree).map_err(std::io::Error::other)?;
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        export.splice(&rel, tree, ts);
        // Write beside the target and rename so a crash never leaves a torn file.
        let tmp = file.with_extension(format!("tmp{}", std::process::id()));
        let bytes = export.to_bytes();
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, file).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
    } else {
        let mut out = BufWriter::with_capacity(1 << 20, std::io::stdout().lock());
        let root = cli.dir.to_string_lossy();
        scan.write(&mut out, &root, dev, uid, env!("CARGO_PKG_VERSION"))?;
    }

    eprintln!("dume: {} files for uid {uid}", scan.files);
    if scan.errors > 0 {
        eprintln!(
            "dume: warning: {} entries could not be read (use -v to list)",
            scan.errors
        );
    }
    Ok(())
}
