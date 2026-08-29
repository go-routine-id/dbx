//! Transient toast notifications, bottom-right, auto-dismiss after ~3s.
//! Contract (widgets agent):
//! - `push` adds a toast; newest at the bottom; max 3 visible (oldest dropped).
//! - `tick` ages toasts; expired ones are removed.
//! - `render` draws stacked rounded-border boxes anchored bottom-right of
//!   `area`; kind icon colored: success ✓ / warning ⚠ / error ✗ / info ℹ.
//! - Box width fits message (clamped), height 3.

use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::Rect;

use crate::theme::Theme;

#[derive(Clone, Copy, Debug)]
pub enum ToastKind {
    Success,
    Warning,
    Error,
    Info,
}

struct Toast {
    kind: ToastKind,
    message: String,
    created: Instant,
}

#[derive(Default)]
pub struct Toasts {
    items: Vec<Toast>,
}

impl Toasts {
    pub fn push(&mut self, kind: ToastKind, message: impl Into<String>) {
        let _ = message;
        todo!("widgets agent")
    }

    pub fn tick(&mut self) {
        todo!("widgets agent")
    }

    pub fn render(&self, f: &mut Frame, area: Rect, theme: &Theme) {
        let _ = (f, area, theme);
        todo!("widgets agent")
    }
}
