use clap::Parser;
use dumelib::{config, layout, recipe, tui};
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use std::io::IsTerminal;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Find what is taking space in your files and clean it up. Just run `dume`.
///
/// Settings and cleaning scripts come from the shared PREFIX/share/dume next to this program.
#[derive(Parser)]
#[command(version, max_term_width = 100)]
struct Cli {
    /// Directory inside the team root to look at [default: the current directory, if inside it]
    dir: Option<PathBuf>,
    /// Where your scan database is kept [default: ~/.local/state/dume/dume.db]
    #[arg(short, long, env = "DUME_DB")]
    file: Option<PathBuf>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if !std::io::stdout().is_terminal() {
        eprintln!("dume: needs a terminal; use `dume-scan` for scripting");
        return ExitCode::from(2);
    }
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("dume: {e}");
            ExitCode::from(1)
        }
    }
}

fn run(cli: &Cli) -> Result<(), String> {
    let layout = layout::locate()?;
    let cfg = config::load(&layout)?;
    let env = |k: &str| std::env::var_os(k);
    let db = match &cli.file {
        Some(f) => f.clone(),
        None => config::db_path(env)?,
    };
    let (recipes, skipped) =
        recipe::load(&layout.share.join("actions")).map_err(|e| e.to_string())?;

    let view = match &cli.dir {
        Some(d) => {
            let d = d
                .canonicalize()
                .map_err(|e| format!("{}: {e}", d.display()))?;
            if !d.starts_with(&cfg.root) {
                return Err(format!(
                    "{} is not inside the team root {}",
                    d.display(),
                    cfg.root.display()
                ));
            }
            d
        }
        None => std::env::current_dir()
            .and_then(|d| d.canonicalize())
            .ok()
            .filter(|d| d.starts_with(&cfg.root))
            .unwrap_or_else(|| cfg.root.clone()),
    };

    let mut app = tui::App::launch(tui::Launch {
        view,
        scan_root: cfg.root.clone(),
        db: db.clone(),
        legacy_export: config::legacy_export(env),
        recipes,
        bin: Some(layout.bin.clone()),
        threads: cfg.threads,
        stale_days: cfg.stale_days,
        rescan_skip_secs: cfg.rescan_skip_secs,
        prune: cfg.prune.clone(),
        uid: rustix::process::getuid().as_raw(),
    });
    if !skipped.is_empty() {
        app.status = format!("skipped recipes: {}", skipped.join("; "));
    }

    let mut term = ratatui::init();
    let res = (|| -> std::io::Result<()> {
        let mut redraw = true;
        while !app.quit {
            // Draw only when something changed (key, resize) or a background job is showing progress.
            if redraw || app.busy() || app.mode == tui::Mode::Scanning {
                term.draw(|f| app.draw(f))?;
            }
            redraw = false;
            if event::poll(Duration::from_millis(100))? {
                match event::read()? {
                    Event::Key(k) if k.kind == KeyEventKind::Press => {
                        app.on_key(k);
                        redraw = true;
                    }
                    Event::Resize(..) => redraw = true,
                    _ => {}
                }
            }
            let before = app.mode;
            app.tick();
            redraw |= app.mode != before;
            if std::mem::take(&mut app.want_ncdu) {
                let json = db.with_file_name(format!("dume.ncdu.{}.json", std::process::id()));
                ratatui::restore();
                let res = ncdu(&app, &layout.bin, &json);
                let _ = std::fs::remove_file(&json);
                term = ratatui::init();
                if let Err(e) = res {
                    app.status = format!("could not run ncdu: {e}");
                }
            }
        }
        Ok(())
    })();
    ratatui::restore();
    app.cleanup();
    res.map_err(|e| e.to_string())
}

/// Write the database as an ncdu export to the private file `json`, then run ncdu on it.
fn ncdu(app: &tui::App, bin: &Path, json: &Path) -> std::io::Result<()> {
    let db = app
        .db()
        .ok_or_else(|| std::io::Error::other("no scan to show"))?;
    let _ = std::fs::remove_file(json);
    let f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(json)?;
    db.write_json(&mut std::io::BufWriter::new(f))?;
    let ncdu = bin.join("ncdu");
    let ncdu = if ncdu.exists() {
        ncdu
    } else {
        PathBuf::from("ncdu")
    };
    Command::new(ncdu).arg("-f").arg(json).status()?;
    Ok(())
}
