//! Status bar: left-aligned context info, right-aligned keybinding hints.
//! Contract (widgets agent):
//! - `render` draws one line: `left` text (dim), then hints as
//!   `key action` pairs right-aligned — key in accent, action in dim.
//! - If hints overflow, drop from the middle; never wrap or panic.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::theme::Theme;

/// Gap between the left text and the hint cluster, and between hint pairs.
const SEP: &str = "  ";
const SEP_W: usize = 2;

pub fn render(f: &mut Frame, area: Rect, left: &str, hints: &[(&str, &str)], theme: &Theme) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let width = area.width as usize;
    let row = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: 1,
    };

    let hint_w = |h: &(&str, &str)| UnicodeWidthStr::width(h.0) + 1 + UnicodeWidthStr::width(h.1);
    let left_w = UnicodeWidthStr::width(left).min(width);

    // Which hints survive, in original order. Overflow drops from the middle.
    let mut kept: Vec<usize> = (0..hints.len()).collect();
    let mut dropped = false;
    let mut right_w: usize;
    loop {
        let n = kept.len();
        let items = n + usize::from(dropped); // "…" counts as one item
        let seps = if items > 1 { (items - 1) * SEP_W } else { 0 };
        let hints_w: usize = kept.iter().map(|&i| hint_w(&hints[i])).sum();
        right_w = hints_w + usize::from(dropped) + seps;

        let avail = width.saturating_sub(left_w).saturating_sub(SEP_W);
        if right_w <= avail || n == 0 {
            break;
        }
        if n == 1 {
            kept.clear();
            dropped = true;
            right_w = usize::from(dropped);
            break;
        }
        kept.remove(n / 2);
        dropped = true;
    }

    // Left text (dim), truncated by paragraph clipping if needed.
    let left_area_w = (width.saturating_sub(right_w).saturating_sub(SEP_W)) as u16;
    if left_area_w > 0 {
        let left_area = Rect {
            width: left_area_w,
            ..row
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(left.to_string(), theme.dim()))),
            left_area,
        );
    }

    if right_w == 0 {
        return;
    }

    // Right-aligned hint cluster.
    let mut spans: Vec<Span> = Vec::new();
    let ellipsis_at = if dropped { kept.len() / 2 } else { usize::MAX };
    let mut first = true;
    let push_sep = |spans: &mut Vec<Span>, first: &mut bool| {
        if !*first {
            spans.push(Span::styled(SEP, theme.dim()));
        }
        *first = false;
    };
    for (pos, &i) in kept.iter().enumerate() {
        if dropped && pos == ellipsis_at {
            push_sep(&mut spans, &mut first);
            spans.push(Span::styled("…", theme.dim()));
        }
        push_sep(&mut spans, &mut first);
        let (key, action) = hints[i];
        spans.push(Span::styled(key.to_string(), theme.accent()));
        spans.push(Span::styled(" ", theme.dim()));
        spans.push(Span::styled(action.to_string(), theme.dim()));
    }
    if dropped && kept.is_empty() {
        push_sep(&mut spans, &mut first);
        spans.push(Span::styled("…", theme.dim()));
    }

    let right_area = Rect {
        x: area.x + (area.width - right_w.min(width) as u16),
        width: right_w.min(width) as u16,
        ..row
    };
    f.render_widget(Paragraph::new(Line::from(spans)), right_area);
}
