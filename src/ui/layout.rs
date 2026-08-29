//! Layout manager: header / body / status bar + min-size guard.
//! Contract for M0. Implemented by app-runtime agent.

use ratatui::layout::{Constraint, Rect};

pub const MIN_WIDTH: u16 = 80;
pub const MIN_HEIGHT: u16 = 24;

/// Vertical split of the full terminal area.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub header: Rect,
    pub body: Rect,
    pub status: Rect,
}

/// Split `area` into header (1 line), status bar (1 line), and the rest as body.
pub fn compute(area: Rect) -> Layout {
    let [header, body, status] = ratatui::layout::Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);
    Layout {
        header,
        body,
        status,
    }
}

/// True when the terminal is too small to render dbx.
pub fn too_small(area: Rect) -> bool {
    area.width < MIN_WIDTH || area.height < MIN_HEIGHT
}
