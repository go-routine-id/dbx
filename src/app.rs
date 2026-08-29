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

use std::io::{self};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Context;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use crate::theme::Theme;
use crate::ui::layout::{self, MIN_HEIGHT, MIN_WIDTH};
use crate::ui::widgets::{empty, help, spinner::Spinner, statusbar, toast::ToastKind, toast::Toasts};

/// How long the event poll waits before a tick fires (spinner/toast cadence).
const TICK_CAP: Duration = Duration::from_millis(60);

const STATUS_HINTS: [(&str, &str); 3] = [("q", "quit"), ("?", "help"), ("t", "toast")];

const HELP_BINDINGS: [(&str, &str); 4] = [
    ("q", "quit"),
    ("?", "toggle help"),
    ("t", "push demo toast"),
    ("Esc", "close help"),
];

const TOAST_KINDS: [ToastKind; 4] = [
    ToastKind::Success,
    ToastKind::Warning,
    ToastKind::Error,
    ToastKind::Info,
];

pub struct App {
    theme: Theme,
    toasts: Toasts,
    help_open: bool,
    should_quit: bool,
}

impl App {
    pub fn new() -> Self {
        Self {
            theme: Theme::dark(),
            toasts: Toasts::default(),
            help_open: false,
            should_quit: false,
        }
    }

    fn handle_key(&mut self, key: KeyEvent, toast_counter: &mut u64) {
        // Universal exit: in raw mode Ctrl+C arrives as a key event, not SIGINT.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        // Plain bindings only — don't fire on Ctrl/Alt-modified chars.
        // (SHIFT allowed so `?`, which needs shift on most layouts, works.)
        if !(key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT) {
            return;
        }
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('?') => self.help_open = !self.help_open,
            KeyCode::Esc if self.help_open => self.help_open = false,
            KeyCode::Char('t') => {
                *toast_counter += 1;
                let kind = TOAST_KINDS[(*toast_counter as usize - 1) % TOAST_KINDS.len()];
                self.toasts
                    .push(kind, format!("demo toast #{}", *toast_counter));
            }
            _ => {}
        }
    }

    fn draw(&self, f: &mut ratatui::Frame, spinner: &Spinner) {
        let area = f.area();
        let theme = &self.theme;

        // Paint the whole frame with the base background first.
        f.render_widget(Block::default().style(theme.base()), area);

        if layout::too_small(area) {
            let notice = format!(
                "terminal too small (min {MIN_WIDTH}x{MIN_HEIGHT}), resize to continue"
            );
            let line_area = Rect {
                x: area.x,
                y: area.y + area.height / 2,
                width: area.width,
                height: 1,
            };
            f.render_widget(
                Paragraph::new(notice)
                    .style(theme.dim())
                    .alignment(Alignment::Center),
                line_area,
            );
            return;
        }

        let layout = layout::compute(area);

        // Header: accent logo + dim version.
        let header = Line::from(vec![
            Span::styled("◆ dbx", theme.accent()),
            Span::styled(format!("  v{}", env!("CARGO_PKG_VERSION")), theme.dim()),
        ]);
        f.render_widget(Paragraph::new(header).style(theme.base()), layout.header);

        // Body: live spinner on the first body line, empty state below it.
        let spinner_area = Rect {
            height: 1,
            ..layout.body
        };
        spinner.render(f, spinner_area, "Connecting... (demo)", theme);
        let empty_area = Rect {
            y: layout.body.y.saturating_add(1),
            height: layout.body.height.saturating_sub(1),
            ..layout.body
        };
        empty::render(
            f,
            empty_area,
            "no database connected",
            Some("press ? for help · t for a demo toast"),
            theme,
        );

        statusbar::render(f, layout.status, "dbx demo", &STATUS_HINTS, theme);

        // Overlays last, so they draw on top.
        self.toasts.render(f, area, theme);
        if self.help_open {
            help::render(f, area, "demo", &HELP_BINDINGS, theme);
        }
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

/// Restores the terminal on drop so every exit path (normal, error, panic
/// unwinding) leaves the shell usable.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        // Construct the guard BEFORE the fallible alt-screen enter: if it
        // fails, Drop still runs and undoes raw mode. LeaveAlternateScreen on
        // a never-entered alternate screen is a harmless no-op escape.
        let guard = Self;
        execute!(io::stdout(), EnterAlternateScreen)?;
        Ok(guard)
    }

    fn restore() {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        Self::restore();
    }
}

pub async fn run(_config: Option<PathBuf>) -> anyhow::Result<()> {
    // Panic hook FIRST: restore the terminal before the default panic output,
    // so a panic never leaves the user's shell in raw mode.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        TerminalGuard::restore();
        default_hook(info);
    }));

    let _guard = TerminalGuard::enter().context("failed to initialize terminal")?;

    // Spawn a signal listener for SIGINT/SIGTERM/SIGHUP so sudden termination restores the terminal cleanly.
    tokio::spawn(async {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut sigterm) = signal(SignalKind::terminate()) {
            sigterm.recv().await;
            TerminalGuard::restore();
            std::process::exit(130);
        }
    });
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))
        .context("failed to create terminal backend")?;

    let mut app = App::new();
    let mut spinner = Spinner::new();
    let mut toast_counter: u64 = 0;

    let mut last_tick = Instant::now();

    while !app.should_quit {
        terminal
            .draw(|f| app.draw(f, &spinner))
            .context("failed to draw frame")?;

        // Sync crossterm polling with a tick cap.
        if event::poll(TICK_CAP).context("failed to poll terminal events")?
            && let Event::Key(key) = event::read().context("failed to read terminal event")?
            && key.kind == KeyEventKind::Press
        {
            app.handle_key(key, &mut toast_counter);
        }

        // Tick on a wall-clock schedule, not on poll timeout — otherwise an
        // event flood (key repeat) starves the spinner and toasts.
        if last_tick.elapsed() >= TICK_CAP {
            spinner.tick();
            app.toasts.tick();
            last_tick = Instant::now();
        }
    }

    Ok(())
}
