mod json;
mod scan;

use clap::Parser;
use std::io::{BufWriter, IsTerminal};
use std::path::PathBuf;
use std::process::ExitCode;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Export only one user's regular files as ncdu JSON.
///
/// Usage: dume /data > me.json && ncdu -f me.json
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
    /// Print each unreadable path to stderr (default: only a count)
    #[arg(short, long)]
    verbose: bool,
}

fn main() -> ExitCode {
    // clap exits with 2 on usage errors.
    let cli = Cli::parse();

    if std::io::stdout().is_terminal() {
        eprintln!(
            "dume: refusing to write JSON to a terminal; pipe it, e.g. `dume DIR | ncdu -f-` or `dume DIR > me.json`"
        );
        return ExitCode::from(2);
    }

    let uid = cli
        .uid
        .unwrap_or_else(|| rustix::process::getuid().as_raw());
    let (scan, dev) = match scan::scan(&cli.dir, uid, cli.threads as usize, cli.verbose) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("dume: {}: {e}", cli.dir.display());
            return ExitCode::from(1);
        }
    };

    let stdout = std::io::stdout().lock();
    let mut out = BufWriter::with_capacity(1 << 20, stdout);
    let root = cli.dir.to_string_lossy();
    if let Err(e) = scan.write(&mut out, &root, dev, env!("CARGO_PKG_VERSION")) {
        if e.kind() != std::io::ErrorKind::BrokenPipe {
            eprintln!("dume: write error: {e}");
        }
        return ExitCode::from(1);
    }

    eprintln!("dume: {} files for uid {uid}", scan.files);
    if scan.errors > 0 {
        eprintln!(
            "dume: warning: {} entries could not be read (use -v to list)",
            scan.errors
        );
    }
    ExitCode::SUCCESS
}
