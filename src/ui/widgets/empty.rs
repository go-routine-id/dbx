//! Informative empty-state placeholder: centered dim message, optionally with
//! a hint line. See docs/screens.md "empty state" states.
//! Contract (widgets agent):
//! - `render` centers `message` (dim) vertically and horizontally in `area`,
//!   with optional `hint` one line below (dimmer/accent key parts left to
//!   caller as plain text).

use ratatui::Frame;
use ratatui::layout::Rect;

use crate::theme::Theme;

pub fn render(f: &mut Frame, area: Rect, message: &str, hint: Option<&str>, theme: &Theme) {
    let _ = (f, area, message, hint, theme);
    todo!("widgets agent")
}
