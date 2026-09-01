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

    // --- Syntax highlighting (SQL editor) ---
    pub syntax_string: Color,
    pub syntax_number: Color,
    /// Quoted identifiers (`backticks`).
    pub syntax_ident: Color,

    /// Border colours cycled over ERD entities so neighbouring tables stay
    /// distinguishable. Muted on purpose: the raw terminal colours they
    /// replaced made the diagram read as a rainbow against a palette that is
    /// otherwise deliberately quiet.
    pub entity_accents: [Color; 6],
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

            syntax_string: Color::from_u32(0xa5d6ff),
            syntax_number: Color::from_u32(0xf0883e),
            syntax_ident: Color::from_u32(0x7ee787),

            // None of these is the warning amber, which marks the *selected*
            // entity — a selection must never be mistaken for one more table.
            entity_accents: [
                Color::from_u32(0x7c83ff), // indigo (the accent)
                Color::from_u32(0x39c5cf), // cyan
                Color::from_u32(0x56d364), // green
                Color::from_u32(0xdb61a2), // pink
                Color::from_u32(0xa371f7), // purple
                Color::from_u32(0xf0883e), // orange
            ],
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

    /// The design's rule is "widgets never hardcode colors". This walks the
    /// actual source so the rule is enforced, not just documented — the ERD
    /// alone had drifted to six raw terminal colours.
    #[test]
    fn test_no_widget_hardcodes_a_colour() {
        let mut offenders = Vec::new();
        for file in [
            "src/app.rs",
            "src/ui/screens/erd.rs",
            "src/ui/screens/query.rs",
            "src/ui/screens/explorer.rs",
            "src/ui/screens/picker.rs",
        ] {
            let src = std::fs::read_to_string(file).unwrap_or_default();
            for (i, line) in src.lines().enumerate() {
                if line.contains("Color::") {
                    offenders.push(format!("{file}:{}: {}", i + 1, line.trim()));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "colours must come from Theme, found:\n{}",
            offenders.join("\n")
        );
    }

    #[test]
    fn test_entity_accents_never_collide_with_the_selection_colour() {
        let t = Theme::dark();
        // The selected ERD entity is drawn in `warning`; if a table happened to
        // cycle onto the same colour the selection would be invisible.
        assert!(
            !t.entity_accents.contains(&t.warning),
            "an entity accent matches the selection colour"
        );
        // And they must all differ from each other, or two neighbouring tables
        // would look like one.
        for (i, a) in t.entity_accents.iter().enumerate() {
            for b in &t.entity_accents[i + 1..] {
                assert_ne!(a, b, "duplicate entity accent");
            }
        }
    }

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
