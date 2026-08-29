//! Informative empty-state placeholder: centered dim message, optionally with
//! a hint line. See docs/screens.md "empty state" states.
//! Contract (widgets agent):
//! - `render` centers `message` (dim) vertically and horizontally in `area`,
//!   with optional `hint` one line below (dimmer/accent key parts left to
//!   caller as plain text).

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::theme::Theme;

pub fn render(f: &mut Frame, area: Rect, message: &str, hint: Option<&str>, theme: &Theme) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let mut lines = vec![Line::from(Span::styled(message.to_string(), theme.dim()))];
    if let Some(h) = hint {
        lines.push(Line::from(Span::styled(h.to_string(), theme.dim())));
    }

    let text_height = lines.len() as u16;
    if text_height > area.height {
        // Not enough room even for the lines themselves; render at the top.
        let p = Paragraph::new(lines).alignment(Alignment::Center);
        f.render_widget(p, area);
        return;
    }

    let y = area.y + (area.height - text_height) / 2;
    let centered = Rect {
        x: area.x,
        y,
        width: area.width,
        height: text_height,
    };

    let p = Paragraph::new(lines).alignment(Alignment::Center);
    f.render_widget(p, centered);
}
