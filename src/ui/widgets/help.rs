//! Help popup (`?`): centered cheat sheet of keybindings for the current
//! screen. Uses the generic popup container.
//! Contract (widgets agent):
//! - `render` dims background, then a popup titled `Help · {context}` listing
//!   `key  action` rows; key column accent, action text. Sized to content,
//!   clamped to area.

use ratatui::Frame;
use ratatui::layout::Rect;

use crate::theme::Theme;

pub fn render(f: &mut Frame, area: Rect, context: &str, bindings: &[(&str, &str)], theme: &Theme) {
    let _ = (f, area, context, bindings, theme);
    todo!("widgets agent")
}
