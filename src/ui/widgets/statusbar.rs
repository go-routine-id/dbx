//! Status bar: left-aligned context info, right-aligned keybinding hints.
//! Contract (widgets agent):
//! - `render` draws one line: `left` text (dim), then hints as
//!   `key action` pairs right-aligned — key in accent, action in dim.
//! - If hints overflow, drop from the middle; never wrap or panic.

use ratatui::Frame;
use ratatui::layout::Rect;

use crate::theme::Theme;

pub fn render(f: &mut Frame, area: Rect, left: &str, hints: &[(&str, &str)], theme: &Theme) {
    let _ = (f, area, left, hints, theme);
    todo!("widgets agent")
}
