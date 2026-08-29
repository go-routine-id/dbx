//! Help popup (`?`): centered cheat sheet of keybindings for the current
//! screen. Uses the generic popup container.
//! Contract (widgets agent):
//! - `render` dims background, then a popup titled `Help · {context}` listing
//!   `key  action` rows; key column accent, action text. Sized to content,
//!   clamped to area.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::theme::Theme;
use crate::ui::widgets::popup;

/// Column gap between the key and its action, plus padding inside the popup.
const COL_GAP: usize = 2;
const PAD_H: u16 = 2;

pub fn render(f: &mut Frame, area: Rect, context: &str, bindings: &[(&str, &str)], theme: &Theme) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    popup::dim_background(f, area, theme);

    let key_w = bindings
        .iter()
        .map(|(k, _)| UnicodeWidthStr::width(*k))
        .max()
        .unwrap_or(0);
    let action_w = bindings
        .iter()
        .map(|(_, a)| UnicodeWidthStr::width(*a))
        .max()
        .unwrap_or(0);

    let content_w = (key_w + COL_GAP + action_w) as u16;
    let width = content_w
        .saturating_add(2) // borders
        .saturating_add(PAD_H * 2)
        .max(10);
    let height = (bindings.len() as u16).saturating_add(2).max(3); // borders

    let title = format!("Help · {context}");
    let inner = popup::render_frame(f, area, Some(&title), width, height, theme);

    // Content area inside the popup, with horizontal padding.
    let rows = Rect {
        x: inner.x.saturating_add(PAD_H).min(inner.x + inner.width),
        y: inner.y,
        width: inner.width.saturating_sub(PAD_H * 2),
        height: inner.height,
    };
    if rows.width == 0 || rows.height == 0 {
        return;
    }

    let key_style = theme.panel().fg(theme.accent);
    let action_style = theme.panel();

    for (i, (key, action)) in bindings
        .iter()
        .take(rows.height as usize)
        .enumerate()
    {
        let pad = key_w - UnicodeWidthStr::width(*key) + COL_GAP;
        let line = Line::from(vec![
            Span::styled(key.to_string(), key_style),
            Span::styled(" ".repeat(pad), action_style),
            Span::styled(action.to_string(), action_style),
        ]);
        let row_area = Rect {
            y: rows.y + i as u16,
            height: 1,
            ..rows
        };
        f.render_widget(Paragraph::new(line), row_area);
    }
}
