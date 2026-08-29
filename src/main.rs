//! dbx — terminal database explorer. M0: foundation shell (no DB yet).

mod app;
mod theme;
mod ui;

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "dbx", version, about = "Terminal database explorer")]
struct Cli {
    /// Path to config file (default: ~/.config/dbx/config.toml)
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    app::run(cli.config).await
}
