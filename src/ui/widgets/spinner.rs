//! Animated braille spinner with a label and optional elapsed time.
//! Contract (widgets agent):
//! - `new()` starts at frame 0.
//! - `tick()` advances the frame; called every ~60ms by the app tick loop.
//! - `render` draws `⠋ label 1.2s` style line inside `area`, spinner glyph in
//!   `theme.accent()`, label in `theme.dim()`. Elapsed shown only when > 0.5s.

use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::theme::Theme;

const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

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
        Self {
            frame: 0,
            started: Instant::now(),
        }
    }

    /// Advance animation frame (call on every app tick).
    pub fn tick(&mut self) {
        self.frame = (self.frame + 1) % FRAMES.len();
    }

    pub fn render(&self, f: &mut Frame, area: Rect, label: &str, theme: &Theme) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let mut spans = vec![
            Span::styled(FRAMES[self.frame % FRAMES.len()], theme.accent()),
            Span::raw(" "),
            Span::styled(label.to_string(), theme.dim()),
        ];

        let elapsed = self.started.elapsed();
        if elapsed.as_secs_f64() > 0.5 {
            let text = if elapsed.as_secs() >= 60 {
                format!(" {}m{:02}s", elapsed.as_secs() / 60, elapsed.as_secs() % 60)
            } else {
                format!(" {:.1}s", elapsed.as_secs_f64())
            };
            spans.push(Span::styled(text, theme.dim()));
        }

        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }
}
