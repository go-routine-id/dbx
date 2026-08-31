//! dbx — terminal database explorer. M0: foundation shell (no DB yet).

mod app;
mod clipboard;
mod config;
mod driver;
mod explain;
mod export;
mod theme;
mod ui;
mod update;

use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

const EXAMPLES: &str = "\
Examples:
  dbx                      # Open connection picker using default config (~/.config/dbx/config.toml)
  dbx --config ./my.toml   # Use custom config file
  dbx --self-update        # Upgrade to the latest release in place
";

#[derive(Parser, Debug)]
#[command(
    name = "dbx",
    version,
    about = "Terminal database explorer — DataGrip UX in your terminal",
    after_help = EXAMPLES
)]
struct Cli {
    /// Path to config file (default: ~/.config/dbx/config.toml or $XDG_CONFIG_HOME/dbx/config.toml)
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Download the latest release and replace this binary in place
    #[arg(long)]
    self_update: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    // 1. Parse CLI arguments first (allows --help and --version to work in pipes/non-TTY)
    let cli = Cli::parse();

    // 2. Self-update runs before the TTY check: it prints plain text, so it
    //    has to keep working from scripts and pipes.
    if cli.self_update {
        return match update::self_update() {
            Ok(Some((from, to))) => {
                println!("updated dbx {from} -> {to}");
                println!("run `dbx --version` to confirm");
                ExitCode::SUCCESS
            }
            Ok(None) => {
                println!("dbx {} is already the latest version", update::CURRENT_VERSION);
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("self-update failed: {e}");
                ExitCode::from(1)
            }
        };
    }

    // 3. TTY check: ensure both stdin and stdout are interactive terminals for TUI execution
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        eprintln!("Error: dbx is an interactive TUI tool and requires a TTY (terminal).");
        eprintln!("If running via SSH, make sure to allocate a pseudo-terminal (ssh -t ...).");
        return ExitCode::from(1);
    }

    // 4. TERM check: reject dumb terminals incapable of ANSI cursor/screen controls
    if let Ok(term) = std::env::var("TERM")
        && term == "dumb"
    {
        eprintln!("Error: terminal ($TERM=dumb) does not support required cursor/screen capabilities.");
        return ExitCode::from(1);
    }

    match app::run(cli.config).await {
        Ok(_) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("Error: {err:#}");
            ExitCode::from(1)
        }
    }
}

