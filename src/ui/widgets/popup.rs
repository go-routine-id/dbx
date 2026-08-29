//! Generic floating popup container: dims the background, centered rounded
//! thin border box, panel background.
//! Contract (widgets agent):
//! - `dim_background` fills `area` with a dim overlay so content behind the
//!   popup reads as inactive.
//! - `render_frame` draws a centered box of `width`/`height` (clamped to
//!   `area` with margin), rounded thin border in `theme.border()`, background
//!   `theme.panel()`, optional title in `theme.accent()`. Returns the inner
//!   content Rect so callers render into it.

use ratatui::Frame;
use ratatui::layout::Rect;

use crate::theme::Theme;

pub fn dim_background(f: &mut Frame, area: Rect, theme: &Theme) {
    let _ = (f, area, theme);
    todo!("widgets agent")
}

pub fn render_frame(f: &mut Frame, area: Rect, title: Option<&str>, width: u16, height: u16, theme: &Theme) -> Rect {
    let _ = (f, area, title, width, height, theme);
    todo!("widgets agent")
}
