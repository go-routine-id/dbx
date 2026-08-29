//! Screen S4 / Component: In-Terminal ERD Diagram Workspace using `flowmaid`.
//! Renders database schema relationships with interactive pan & zoom.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

use crate::driver::{CollectionMeta, Namespace};
use crate::theme::Theme;

#[derive(Clone, Debug)]
pub struct ErdTab {
    pub namespace: Namespace,
    pub mermaid_code: String,
    pub rendered_lines: Vec<String>,
    pub scroll_offset_y: usize,
    pub scroll_offset_x: usize,
    pub is_loading: bool,
}

impl ErdTab {
    pub fn new(namespace: Namespace) -> Self {
        Self {
            namespace,
            mermaid_code: String::new(),
            rendered_lines: Vec::new(),
            scroll_offset_y: 0,
            scroll_offset_x: 0,
            is_loading: true,
        }
    }

    pub fn generate_from_meta(&mut self, collections: &[CollectionMeta]) {
        let mut mermaid = String::from("erDiagram\n");

        for meta in collections {
            let tbl_name = &meta.reference.name;
            mermaid.push_str(&format!("    {tbl_name} {{\n"));
            for col in &meta.columns {
                let dt = col.data_type.split('(').next().unwrap_or(&col.data_type);
                let pk = if col.is_primary_key { " PK" } else if col.is_foreign_key { " FK" } else { "" };
                mermaid.push_str(&format!("        {} {}{}\n", dt, col.name, pk));
            }
            mermaid.push_str("    }\n");

            for fk in &meta.foreign_keys {
                // Relationship: referenced_table ||--o{ current_table : references
                mermaid.push_str(&format!(
                    "    {} ||--o{{ {} : \"{}\"\n",
                    fk.ref_table, tbl_name, fk.column
                ));
            }
        }

        self.mermaid_code = mermaid;
        self.render_diagram();
        self.is_loading = false;
    }

    pub fn render_diagram(&mut self) {
        if self.mermaid_code.is_empty() {
            self.rendered_lines = vec!["No tables found to generate ERD.".to_string()];
            return;
        }

        match flowmaid::parser::parse(&self.mermaid_code) {
            Ok(graph) => {
                let art = flowmaid::render::render(&graph);
                self.rendered_lines = art.lines().map(|s| s.to_string()).collect();
            }
            Err(e) => {
                self.rendered_lines = vec![
                    "Failed to parse & render ERD diagram:".to_string(),
                    format!("{e}"),
                    "".to_string(),
                    "Mermaid Source:".to_string(),
                ];
                self.rendered_lines.extend(self.mermaid_code.lines().map(|s| s.to_string()));
            }
        }
    }

    pub fn scroll_up(&mut self) {
        if self.scroll_offset_y > 0 {
            self.scroll_offset_y -= 1;
        }
    }

    pub fn scroll_down(&mut self) {
        if self.scroll_offset_y < self.rendered_lines.len().saturating_sub(1) {
            self.scroll_offset_y += 1;
        }
    }

    pub fn scroll_left(&mut self) {
        if self.scroll_offset_x > 0 {
            self.scroll_offset_x -= 1;
        }
    }

    pub fn scroll_right(&mut self) {
        self.scroll_offset_x += 1;
    }
}

pub fn render_erd(
    f: &mut Frame,
    area: Rect,
    erd: &ErdTab,
    is_focused: bool,
    theme: &Theme,
) {
    let border_style = if is_focused { theme.accent() } else { theme.border() };
    let title = format!(" ERD Diagram: {} [h/j/k/l or Arrows to pan] ", erd.namespace.0);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .style(theme.base())
        .title(title);

    let inner = block.inner(area);
    f.render_widget(block, area);

    if erd.is_loading {
        let p = Paragraph::new(Span::styled("Generating ERD diagram...", theme.dim()))
            .alignment(Alignment::Center);
        f.render_widget(p, inner);
        return;
    }

    let mut lines = Vec::new();
    for raw_line in erd.rendered_lines.iter().skip(erd.scroll_offset_y) {
        let chars: Vec<char> = raw_line.chars().collect();
        let display_str: String = if erd.scroll_offset_x < chars.len() {
            chars[erd.scroll_offset_x..].iter().collect()
        } else {
            String::new()
        };
        lines.push(Line::from(Span::styled(display_str, theme.base())));
    }

    let p = Paragraph::new(lines);
    f.render_widget(p, inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::{CollectionRef, ColumnMeta, ForeignKeyMeta};

    #[test]
    fn test_erd_generator() {
        let mut erd = ErdTab::new(Namespace("shop".to_string()));
        let meta = vec![
            CollectionMeta {
                reference: CollectionRef {
                    namespace: Namespace("shop".to_string()),
                    name: "users".to_string(),
                },
                columns: vec![
                    ColumnMeta {
                        name: "id".to_string(),
                        data_type: "int".to_string(),
                        is_nullable: false,
                        is_primary_key: true,
                        is_unique: true,
                        is_foreign_key: false,
                        extra: None,
                    },
                ],
                indexes: vec![],
                foreign_keys: vec![],
            },
            CollectionMeta {
                reference: CollectionRef {
                    namespace: Namespace("shop".to_string()),
                    name: "orders".to_string(),
                },
                columns: vec![
                    ColumnMeta {
                        name: "id".to_string(),
                        data_type: "int".to_string(),
                        is_nullable: false,
                        is_primary_key: true,
                        is_unique: true,
                        is_foreign_key: false,
                        extra: None,
                    },
                    ColumnMeta {
                        name: "user_id".to_string(),
                        data_type: "int".to_string(),
                        is_nullable: false,
                        is_primary_key: false,
                        is_unique: false,
                        is_foreign_key: true,
                        extra: None,
                    },
                ],
                indexes: vec![],
                foreign_keys: vec![
                    ForeignKeyMeta {
                        name: "fk_orders_users".to_string(),
                        column: "user_id".to_string(),
                        ref_namespace: Namespace("shop".to_string()),
                        ref_table: "users".to_string(),
                        ref_column: "id".to_string(),
                    },
                ],
            },
        ];

        erd.generate_from_meta(&meta);
        assert!(!erd.mermaid_code.is_empty());
        assert!(erd.mermaid_code.contains("erDiagram"));
        assert!(erd.mermaid_code.contains("users"));
        assert!(erd.mermaid_code.contains("orders"));
    }
}
