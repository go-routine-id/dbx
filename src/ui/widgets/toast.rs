//! Transient toast notifications, bottom-right, auto-dismiss after ~3s.
//! Contract (widgets agent):
//! - `push` adds a toast; newest at the bottom; max 3 visible (oldest dropped).
//! - `tick` ages toasts; expired ones are removed.
//! - `render` draws stacked rounded-border boxes anchored bottom-right of
//!   `area`; kind icon colored: success ✓ / warning ⚠ / error ✗ / info ℹ.
//! - Box width fits message (clamped), height 3.

use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::theme::Theme;

/// How long a normal toast stays visible before `tick` removes it.
pub const TOAST_TTL: Duration = Duration::from_secs(3);
/// Errors carry more detail (SQL messages, paths, causes), so give them
/// longer to read than an info/success blip.
pub const ERROR_TTL: Duration = Duration::from_secs(7);
/// Maximum number of toasts visible at once.
pub const MAX_VISIBLE: usize = 3;
/// Fixed box height (1 content line + top/bottom border).
const TOAST_HEIGHT: u16 = 3;

#[derive(Clone, Copy, Debug)]
pub enum ToastKind {
    Success,
    Warning,
    Error,
    Info,
}

impl ToastKind {
    /// Longer-lived errors vs. short-lived info/success.
    fn ttl(self) -> Duration {
        match self {
            ToastKind::Error => ERROR_TTL,
            _ => TOAST_TTL,
        }
    }
}

struct Toast {
    kind: ToastKind,
    message: String,
    created: Instant,
    ttl: Duration,
}

#[derive(Default)]
pub struct Toasts {
    items: Vec<Toast>,
}

impl Toasts {
    pub fn push(&mut self, kind: ToastKind, message: impl Into<String>) {
        if self.items.len() >= MAX_VISIBLE {
            // Drop the oldest so the newest always fits.
            self.items.remove(0);
        }
        self.items.push(Toast {
            kind,
            message: message.into(),
            created: Instant::now(),
            ttl: kind.ttl(),
        });
    }

    pub fn tick(&mut self) {
        self.items.retain(|t| t.created.elapsed() < t.ttl);
    }

    pub fn render(&self, f: &mut Frame, area: Rect, theme: &Theme) {
        if area.width < 4 || area.height < TOAST_HEIGHT {
            return;
        }

        // Newest at the bottom: iterate from the last item upward.
        for (i, toast) in self.items.iter().rev().enumerate() {
            let i = i as u16;
            let offset_from_bottom = (i + 1).saturating_mul(TOAST_HEIGHT);
            if offset_from_bottom > area.height {
                break; // no room for more boxes above
            }
            let y = area.y + area.height - offset_from_bottom;

            let (icon, icon_style) = match toast.kind {
                ToastKind::Success => ("✓", theme.panel().fg(theme.success)),
                ToastKind::Warning => ("⚠", theme.panel().fg(theme.warning)),
                ToastKind::Error => ("✗", theme.panel().fg(theme.error)),
                ToastKind::Info => ("ℹ", theme.panel().fg(theme.accent)),
            };

            // icon + space + message + left/right borders.
            let content_w = 2u16
                .saturating_add(UnicodeWidthStr::width(toast.message.as_str()) as u16);
            let width = content_w
                .saturating_add(2)
                .clamp(4, area.width);
            let x = area.x + area.width - width;
            let box_area = Rect {
                x,
                y,
                width,
                height: TOAST_HEIGHT,
            };

            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(theme.border())
                .style(theme.panel());
            let inner = block.inner(box_area);
            f.render_widget(block, box_area);

            if inner.width > 0 && inner.height > 0 {
                let line = Line::from(vec![
                    Span::styled(icon, icon_style),
                    Span::styled(" ", theme.panel()),
                    Span::styled(toast.message.clone(), theme.panel()),
                ]);
                f.render_widget(Paragraph::new(line), inner);
            }
        }
    }
}
