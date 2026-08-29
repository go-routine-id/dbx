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
use ratatui::style::Modifier;
use ratatui::text::Span;
use ratatui::widgets::{Block, BorderType, Borders};

use crate::theme::Theme;

/// Margin kept between the popup and the edges of `area` when clamping.
const MARGIN_H: u16 = 4;
const MARGIN_V: u16 = 2;

pub fn dim_background(f: &mut Frame, area: Rect, theme: &Theme) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    // Patch every existing cell: dim foreground + DIM modifier, background
    // untouched. Content behind the popup reads as inactive.
    f.buffer_mut()
        .set_style(area, theme.dim().add_modifier(Modifier::DIM));
}

pub fn render_frame(f: &mut Frame, area: Rect, title: Option<&str>, width: u16, height: u16, theme: &Theme) -> Rect {
    if area.width == 0 || area.height == 0 {
        return Rect {
            x: area.x,
            y: area.y,
            width: 0,
            height: 0,
        };
    }

    let max_w = area.width.saturating_sub(MARGIN_H).max(1).min(area.width);
    let max_h = area.height.saturating_sub(MARGIN_V).max(1).min(area.height);
    let w = width.clamp(1, max_w);
    let h = height.clamp(1, max_h);

    let x = area.x + (area.width - w) / 2;
    let y = area.y + (area.height - h) / 2;
    let popup_area = Rect {
        x,
        y,
        width: w,
        height: h,
    };

    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme.border())
        .style(theme.panel());
    if let Some(t) = title {
        block = block.title(Span::styled(format!(" {t} "), theme.accent()));
    }

    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);
    inner
}
