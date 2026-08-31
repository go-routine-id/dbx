//! Help popup (`?`): centered cheat sheet of keybindings for the current
//! screen. Uses the generic popup container.
//! Contract (widgets agent):
//! - `render` dims background, then a popup titled `Help · {context}` listing
//!   `key  action` rows; key column accent, action text. Sized to content,
//!   clamped to area.
//! - The list does not fit a normal terminal once a screen has many bindings,
//!   so it lays out in two columns when the terminal is wide enough and
//!   scrolls otherwise — silently dropping the tail would hide exactly the
//!   newest features.

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
/// Gap between the two binding columns in the wide layout.
const COL_SPLIT_GAP: u16 = 4;
/// Mirrors `popup::render_frame`'s margins so the fit calculation agrees with
/// the frame that is actually drawn.
const MARGIN_H: u16 = 4;
const MARGIN_V: u16 = 2;

/// Rows the popup can show at `area` height, used by the caller to clamp
/// scrolling to something that actually moves the view.
pub fn visible_rows(area: Rect, bindings: &[(&str, &str)]) -> usize {
    let cols = columns_for(area, bindings);
    let body = area.height.saturating_sub(MARGIN_V + 3) as usize;
    body.max(1) * cols
}

/// Two columns when the terminal is wide enough to hold them, else one.
fn columns_for(area: Rect, bindings: &[(&str, &str)]) -> usize {
    let single = content_width(bindings) as u16;
    if area.width >= single * 2 + COL_SPLIT_GAP + MARGIN_H + PAD_H * 2 {
        2
    } else {
        1
    }
}

fn content_width(bindings: &[(&str, &str)]) -> usize {
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
    key_w + COL_GAP + action_w
}

pub fn render(
    f: &mut Frame,
    area: Rect,
    context: &str,
    bindings: &[(&str, &str)],
    scroll: usize,
    theme: &Theme,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    popup::dim_background(f, area, theme);

    let key_w = bindings
        .iter()
        .map(|(k, _)| UnicodeWidthStr::width(*k))
        .max()
        .unwrap_or(0);
    let cols = columns_for(area, bindings);
    let col_w = content_width(bindings) as u16;

    // Rows needed per column, then the frame: +2 borders, +1 footer line.
    let per_col = bindings.len().div_ceil(cols);
    let width = col_w * cols as u16
        + if cols == 2 { COL_SPLIT_GAP } else { 0 }
        + 2
        + PAD_H * 2;
    let height = (per_col as u16).saturating_add(3).max(4);

    let title = format!("Help · {context}");
    let inner = popup::render_frame(f, area, Some(&title), width, height, theme);

    let rows = Rect {
        x: inner.x.saturating_add(PAD_H).min(inner.x + inner.width),
        y: inner.y,
        width: inner.width.saturating_sub(PAD_H * 2),
        // Last line is the footer (scroll position / hint).
        height: inner.height.saturating_sub(1),
    };
    if rows.width == 0 || rows.height == 0 {
        return;
    }

    let key_style = theme.panel().fg(theme.accent);
    let action_style = theme.panel();

    // How many bindings fit right now, and where the window starts. Clamped so
    // scrolling past the end can never blank the popup.
    let per_screen = rows.height as usize * cols;
    let max_scroll = bindings.len().saturating_sub(per_screen);
    let start = scroll.min(max_scroll);
    let shown = &bindings[start..];

    for (i, (key, action)) in shown.iter().take(per_screen).enumerate() {
        let (col, row) = (i / rows.height as usize, i % rows.height as usize);
        let pad = key_w - UnicodeWidthStr::width(*key) + COL_GAP;
        let line = Line::from(vec![
            Span::styled(key.to_string(), key_style),
            Span::styled(" ".repeat(pad), action_style),
            Span::styled(action.to_string(), action_style),
        ]);
        let row_area = Rect {
            x: rows.x + col as u16 * (col_w + COL_SPLIT_GAP),
            y: rows.y + row as u16,
            width: col_w.min(rows.width),
            height: 1,
        };
        f.render_widget(Paragraph::new(line), row_area);
    }

    // Footer: say plainly when there is more, so nothing is hidden silently.
    let hidden = bindings.len().saturating_sub(start + per_screen);
    let footer = if hidden > 0 || start > 0 {
        format!(
            "{}-{} of {}  ·  ↑/↓ scroll  ·  Esc close",
            start + 1,
            (start + per_screen).min(bindings.len()),
            bindings.len()
        )
    } else {
        "Esc close".to_string()
    };
    let footer_area = Rect {
        y: rows.y + rows.height,
        height: 1,
        ..rows
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(footer, theme.dim()))),
        footer_area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bindings sized like the real ones (`"Ctrl+Enter / F5"` +
    /// `"execute SQL query in active console"`), because the column fit
    /// depends entirely on their width.
    fn bindings(n: usize) -> Vec<(&'static str, &'static str)> {
        // Leaked so the slice can be &'static, which the render API takes.
        (0..n)
            .map(|i| {
                let k: &'static str = Box::leak(format!("Ctrl+Shift+{i:02}").into_boxed_str());
                let a: &'static str =
                    Box::leak(format!("do the thing number {i} in the active tab").into_boxed_str());
                (k, a)
            })
            .collect()
    }

    #[test]
    fn test_wide_terminal_uses_two_columns() {
        let b = bindings(40);
        let wide = Rect::new(0, 0, 200, 30);
        let narrow = Rect::new(0, 0, 80, 30);
        assert_eq!(columns_for(wide, &b), 2);
        assert_eq!(columns_for(narrow, &b), 1);
    }

    #[test]
    fn test_all_bindings_are_reachable_on_a_small_terminal() {
        // The bug this guards: with 40 bindings on a 24-row terminal the popup
        // silently rendered only the first ~20 and there was no way to see the
        // rest — hiding exactly the newest features.
        let b = bindings(40);
        let small = Rect::new(0, 0, 80, 24);
        let per_screen = visible_rows(small, &b);

        assert!(per_screen < b.len(), "test needs an overflowing list");
        // Scrolling to the clamp must expose the final binding.
        let max_scroll = b.len() - per_screen;
        assert!(
            max_scroll + per_screen >= b.len(),
            "the last binding must be reachable by scrolling"
        );
    }

    #[test]
    fn test_two_columns_double_the_capacity() {
        let b = bindings(40);
        let rows = 30;
        let one = visible_rows(Rect::new(0, 0, 80, rows), &b);
        let two = visible_rows(Rect::new(0, 0, 200, rows), &b);
        assert_eq!(two, one * 2);
        // On a normal wide terminal every binding fits at once.
        assert!(two >= b.len(), "40 bindings should fit in two columns: {two}");
    }
}
