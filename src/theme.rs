//! Centralized theme tokens — see docs/ui-ux.md.
//! Widgets must NEVER hardcode colors; always go through `Theme`.

use ratatui::style::{Color, Modifier, Style};

#[derive(Clone, Debug)]
pub struct Theme {
    pub background: Color,
    pub panel: Color,
    pub border: Color,
    pub accent: Color,
    pub text: Color,
    pub text_dim: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
}

impl Theme {
    /// Default dark theme (opencode-style). Truecolor.
    pub fn dark() -> Self {
        Self {
            background: Color::from_u32(0x0d1117),
            panel: Color::from_u32(0x161b22),
            border: Color::from_u32(0x30363d),
            accent: Color::from_u32(0x7c83ff),
            text: Color::from_u32(0xe6edf3),
            text_dim: Color::from_u32(0x7d8590),
            success: Color::from_u32(0x3fb950),
            warning: Color::from_u32(0xd29922),
            error: Color::from_u32(0xf85149),
        }
    }

    pub fn base(&self) -> Style {
        Style::default().bg(self.background).fg(self.text)
    }

    pub fn panel(&self) -> Style {
        Style::default().bg(self.panel).fg(self.text)
    }

    pub fn border(&self) -> Style {
        Style::default().fg(self.border)
    }

    pub fn accent(&self) -> Style {
        Style::default().fg(self.accent)
    }

    pub fn selected(&self) -> Style {
        Style::default()
            .bg(self.panel)
            .fg(self.accent)
            .add_modifier(Modifier::BOLD)
    }

    pub fn dim(&self) -> Style {
        Style::default().fg(self.text_dim)
    }

    /// Selection in a pane that does not have focus. Still clearly marked —
    /// losing your place when focus moves elsewhere is disorienting — but
    /// quiet enough that the focused pane still reads as the active one.
    pub fn selected_inactive(&self) -> Style {
        Style::default().bg(self.panel).fg(self.text_dim)
    }

    /// Full-area dim style used to fade out the rest of the UI when a blocking
    /// operation (e.g. in-flight connection test) is in progress.
    pub fn dimmed(&self) -> Style {
        Style::default()
            .bg(self.background)
            .fg(self.text_dim)
            .add_modifier(Modifier::DIM)
    }

    pub fn success(&self) -> Style {
        Style::default().fg(self.success)
    }

    pub fn warning(&self) -> Style {
        Style::default().fg(self.warning)
    }

    pub fn error(&self) -> Style {
        Style::default().fg(self.error)
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}
