use clap::Parser;
use dumelib::{actions, config, layout, tui};
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use std::io::IsTerminal;
use std::path::PathBuf;
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
    /// Where your scan is kept [default: ~/.local/state/dume/dume.json]
    #[arg(short, long, env = "DUME_JSON")]
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
    let export = match &cli.file {
        Some(f) => f.clone(),
        None => config::export_path(env)?,
    };
    let (actions, skipped) =
        actions::load(&layout.share.join("actions")).map_err(|e| e.to_string())?;

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
        export,
        legacy_export: config::legacy_export(env),
        actions,
        bin: Some(layout.bin.clone()),
        threads: cfg.threads,
        stale_days: cfg.stale_days,
        uid: rustix::process::getuid().as_raw(),
    });
    if !skipped.is_empty() {
        app.status = format!("skipped unsafe actions: {}", skipped.join("; "));
    }

    let mut term = ratatui::init();
    let res = (|| -> std::io::Result<()> {
        let mut redraw = true;
        while !app.quit {
            // Draw only when something changed (key, resize) or a background job is showing progress.
            if redraw || matches!(app.mode, tui::Mode::Running | tui::Mode::Scanning) {
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
                ratatui::restore();
                let ncdu = layout.bin.join("ncdu");
                let ncdu = if ncdu.exists() {
                    ncdu
                } else {
                    PathBuf::from("ncdu")
                };
                let status = Command::new(ncdu).arg("-f").arg(app.export_path()).status();
                term = ratatui::init();
                if let Err(e) = status {
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
