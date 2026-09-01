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

    /// Selection in a pane that does not have focus.
    ///
    /// Keeps the same accent colour as [`Self::selected`] so a row selected by
    /// mouse looks like one selected by the arrow keys — only the bold weight
    /// is dropped, which is enough to tell the focused pane apart without the
    /// selection changing colour under the user.
    pub fn selected_inactive(&self) -> Style {
        Style::default().bg(self.panel).fg(self.accent)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A row selected by mouse (pane unfocused) must not change colour versus
    /// one selected by the arrow keys — only its weight. The first attempt
    /// dimmed it to grey, which read as a different kind of selection.
    #[test]
    fn test_selection_keeps_its_colour_when_the_pane_loses_focus() {
        let t = Theme::dark();
        assert_eq!(
            t.selected().fg,
            t.selected_inactive().fg,
            "focused and unfocused selection must share the accent colour"
        );
        assert_eq!(t.selected().bg, t.selected_inactive().bg);
        // The focused one is the louder of the two.
        assert!(t.selected().add_modifier.contains(Modifier::BOLD));
        assert!(!t.selected_inactive().add_modifier.contains(Modifier::BOLD));
    }
}
