//! App runtime: event loop, tick-driven animation, terminal lifecycle.
//! Contract for M0 (app-runtime agent):
//!
//! `run` must:
//! 1. Install a panic hook that restores the terminal BEFORE printing the
//!    panic (NFR-9 — a panic must never leave the user's shell in raw mode).
//! 2. Enter raw mode + alternate screen; restore both on every exit path
//!    (normal, error, panic).
//! 3. Run the loop: draw → poll crossterm events with ~60ms cap → on tick,
//!    advance spinner/toasts → on key, dispatch.
//! 4. Keys for the M0 demo: `q` quit, `?` toggle help popup, `t` push a demo
//!    toast (cycles kinds), any other key ignored.
//! 5. If terminal < 80x24, render only the too-small notice (layout::too_small).
//!
//! Demo screen: header `◆ dbx  v{version}` (accent logo), body shows an
//! empty-state + a live spinner ("Connecting... (demo)"), status bar with the
//! demo hints, toasts overlay, help popup when open.

use std::path::PathBuf;

use crate::theme::Theme;
use crate::ui::widgets::toast::Toasts;

pub struct App {
    theme: Theme,
    toasts: Toasts,
    help_open: bool,
    should_quit: bool,
}

impl App {
    pub fn new() -> Self {
        todo!("app-runtime agent")
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

pub async fn run(_config: Option<PathBuf>) -> anyhow::Result<()> {
    todo!("app-runtime agent")
}
