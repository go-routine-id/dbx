//! Animated braille spinner with a label and optional elapsed time.
//! Contract (widgets agent):
//! - `new()` starts at frame 0.
//! - `tick()` advances the frame; called every ~60ms by the app tick loop.
//! - `render` draws `⠋ label 1.2s` style line inside `area`, spinner glyph in
//!   `theme.accent()`, label in `theme.dim()`. Elapsed shown only when > 0.5s.

use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::Rect;

use crate::theme::Theme;

pub struct Spinner {
    frame: usize,
    started: Instant,
}

impl Default for Spinner {
    fn default() -> Self {
        Self::new()
    }
}

impl Spinner {
    pub fn new() -> Self {
        todo!("widgets agent")
    }

    /// Advance animation frame (call on every app tick).
    pub fn tick(&mut self) {
        todo!("widgets agent")
    }

    pub fn render(&self, f: &mut Frame, area: Rect, label: &str, theme: &Theme) {
        let _ = (f, area, label, theme);
        todo!("widgets agent")
    }
}
