//! Screen S3 / Component: SQL Query Console & Editor.
//! Multi-line SQL text editor with cursor positioning, syntax highlighting, and execution state.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Paragraph, Row as TableRow, Table, TableState,
};

use crate::driver::QueryResult;
use crate::theme::Theme;

/// SQL keywords to highlight in the editor.
const SQL_KEYWORDS: &[&str] = &[
    "SELECT", "FROM", "WHERE", "INSERT", "INTO", "UPDATE", "SET", "DELETE",
    "JOIN", "INNER", "LEFT", "RIGHT", "OUTER", "CROSS", "ON", "GROUP", "BY",
    "ORDER", "ASC", "DESC", "HAVING", "LIMIT", "OFFSET", "UNION", "ALL",
    "CREATE", "TABLE", "DROP", "ALTER", "ADD", "COLUMN", "INDEX", "PRIMARY",
    "KEY", "FOREIGN", "REFERENCES", "NULL", "NOT", "DEFAULT", "AUTO_INCREMENT",
    "SHOW", "DATABASES", "TABLES", "COLUMNS", "DESCRIBE", "EXPLAIN", "USE",
    "AND", "OR", "IN", "IS", "LIKE", "BETWEEN", "EXISTS", "CASE", "WHEN",
    "THEN", "ELSE", "END", "AS", "DISTINCT", "COUNT", "SUM", "AVG", "MIN", "MAX",
];

#[derive(Clone, Debug)]
pub struct QueryConsole {
    pub title: String,
    pub lines: Vec<String>,
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub is_executing: bool,
    pub last_result: Option<QueryResult>,
    pub execution_error: Option<String>,
    pub result_selected_row: usize,
    pub result_selected_col: usize,
    pub result_scroll_x: usize,
    pub focused_subpane: ConsoleSubpane,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsoleSubpane {
    Editor,
    Result,
}

impl QueryConsole {
    pub fn new(title: String, initial_query: Option<&str>) -> Self {
        let lines = if let Some(q) = initial_query {
            let split: Vec<String> = q.lines().map(|s| s.to_string()).collect();
            if split.is_empty() {
                vec![String::new()]
            } else {
                split
            }
        } else {
            vec!["SELECT * FROM ".to_string()]
        };

        let cursor_row = lines.len().saturating_sub(1);
        let cursor_col = lines.last().map(|l| l.chars().count()).unwrap_or(0);

        Self {
            title,
            lines,
            cursor_row,
            cursor_col,
            is_executing: false,
            last_result: None,
            execution_error: None,
            result_selected_row: 0,
            result_selected_col: 0,
            result_scroll_x: 0,
            focused_subpane: ConsoleSubpane::Editor,
        }
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn insert_char(&mut self, c: char) {
        if self.cursor_row >= self.lines.len() {
            self.lines.push(String::new());
            self.cursor_row = self.lines.len() - 1;
        }

        let line = &mut self.lines[self.cursor_row];
        let mut chars: Vec<char> = line.chars().collect();
        if self.cursor_col >= chars.len() {
            chars.push(c);
            self.cursor_col = chars.len();
        } else {
            chars.insert(self.cursor_col, c);
            self.cursor_col += 1;
        }
        *line = chars.into_iter().collect();
    }

    pub fn insert_newline(&mut self) {
        if self.cursor_row >= self.lines.len() {
            self.lines.push(String::new());
            self.cursor_row = self.lines.len() - 1;
            self.cursor_col = 0;
            return;
        }

        let current_line = &self.lines[self.cursor_row];
        let chars: Vec<char> = current_line.chars().collect();
        if self.cursor_col >= chars.len() {
            self.lines.insert(self.cursor_row + 1, String::new());
        } else {
            let left: String = chars[..self.cursor_col].iter().collect();
            let right: String = chars[self.cursor_col..].iter().collect();
            self.lines[self.cursor_row] = left;
            self.lines.insert(self.cursor_row + 1, right);
        }
        self.cursor_row += 1;
        self.cursor_col = 0;
    }

    pub fn backspace(&mut self) {
        if self.cursor_row >= self.lines.len() {
            return;
        }

        if self.cursor_col > 0 {
            let line = &mut self.lines[self.cursor_row];
            let mut chars: Vec<char> = line.chars().collect();
            if self.cursor_col <= chars.len() {
                chars.remove(self.cursor_col - 1);
                self.cursor_col -= 1;
                *line = chars.into_iter().collect();
            }
        } else if self.cursor_row > 0 {
            // Merge with previous line
            let current_line = self.lines.remove(self.cursor_row);
            self.cursor_row -= 1;
            let prev_char_count = self.lines[self.cursor_row].chars().count();
            self.lines[self.cursor_row].push_str(&current_line);
            self.cursor_col = prev_char_count;
        }
    }

    pub fn move_cursor_left(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.cursor_col = self.lines[self.cursor_row].chars().count();
        }
    }

    pub fn move_cursor_right(&mut self) {
        if self.cursor_row < self.lines.len() {
            let line_len = self.lines[self.cursor_row].chars().count();
            if self.cursor_col < line_len {
                self.cursor_col += 1;
            } else if self.cursor_row + 1 < self.lines.len() {
                self.cursor_row += 1;
                self.cursor_col = 0;
            }
        }
    }

    pub fn move_cursor_up(&mut self) {
        if self.cursor_row > 0 {
            self.cursor_row -= 1;
            let line_len = self.lines[self.cursor_row].chars().count();
            if self.cursor_col > line_len {
                self.cursor_col = line_len;
            }
        }
    }

    pub fn move_cursor_down(&mut self) {
        if self.cursor_row + 1 < self.lines.len() {
            self.cursor_row += 1;
            let line_len = self.lines[self.cursor_row].chars().count();
            if self.cursor_col > line_len {
                self.cursor_col = line_len;
            }
        }
    }
}

/// Tokenizes a single SQL line and returns highlighted Spans (owned Strings).
pub fn highlight_sql_line(line: &str, theme: &Theme) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut current_word = String::new();
    let mut chars = line.char_indices().peekable();

    while let Some((idx, ch)) = chars.next() {
        if ch.is_alphanumeric() || ch == '_' {
            current_word.push(ch);
        } else {
            if !current_word.is_empty() {
                let upper = current_word.to_uppercase();
                if SQL_KEYWORDS.contains(&upper.as_str()) {
                    spans.push(Span::styled(
                        current_word.clone(),
                        theme.accent().add_modifier(Modifier::BOLD),
                    ));
                } else if current_word.chars().all(|c| c.is_ascii_digit()) {
                    spans.push(Span::styled(
                        current_word.clone(),
                        Style::default().fg(Color::Rgb(240, 140, 80)),
                    ));
                } else {
                    spans.push(Span::styled(current_word.clone(), theme.base()));
                }
                current_word.clear();
            }

            if ch == '\'' || ch == '"' || ch == '`' {
                // String or identifier literal
                let quote_char = ch;
                let mut literal = String::from(quote_char);
                while let Some((_, next_ch)) = chars.peek() {
                    let next_ch = *next_ch;
                    literal.push(next_ch);
                    chars.next();
                    if next_ch == quote_char {
                        break;
                    }
                }
                let style = if quote_char == '`' {
                    Style::default().fg(Color::Rgb(120, 200, 120))
                } else {
                    Style::default().fg(Color::Rgb(230, 200, 100))
                };
                spans.push(Span::styled(literal, style));
            } else if ch == '-' && chars.peek().map(|(_, c)| *c) == Some('-') {
                // Single-line comment
                let comment_text = line[idx..].to_string();
                spans.push(Span::styled(comment_text, theme.dim()));
                break;
            } else {
                spans.push(Span::styled(ch.to_string(), theme.dim()));
            }
        }
    }

    if !current_word.is_empty() {
        let upper = current_word.to_uppercase();
        if SQL_KEYWORDS.contains(&upper.as_str()) {
            spans.push(Span::styled(
                current_word,
                theme.accent().add_modifier(Modifier::BOLD),
            ));
        } else if current_word.chars().all(|c| c.is_ascii_digit()) {
            spans.push(Span::styled(
                current_word,
                Style::default().fg(Color::Rgb(240, 140, 80)),
            ));
        } else {
            spans.push(Span::styled(current_word, theme.base()));
        }
    }

    spans
}

pub fn render_query_console(
    f: &mut Frame,
    area: Rect,
    console: &QueryConsole,
    is_tab_focused: bool,
    theme: &Theme,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(10), // Editor Area (Top)
            Constraint::Min(8),     // Result Grid / Info Area (Bottom)
        ])
        .split(area);

    render_editor(f, chunks[0], console, is_tab_focused, theme);
    render_result(f, chunks[1], console, is_tab_focused, theme);
}

fn render_editor(
    f: &mut Frame,
    area: Rect,
    console: &QueryConsole,
    is_tab_focused: bool,
    theme: &Theme,
) {
    let is_editor_focused = is_tab_focused && console.focused_subpane == ConsoleSubpane::Editor;
    let border_style = if is_editor_focused {
        theme.accent()
    } else {
        theme.border()
    };

    let title = format!(" SQL Editor: {} [Ctrl+Enter/F5 to execute] ", console.title);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .style(theme.base())
        .title(title);

    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = Vec::new();
    for (r_idx, line_str) in console.lines.iter().enumerate() {
        let line_num_str = format!("{:>3} │ ", r_idx + 1);
        let mut spans = vec![Span::styled(line_num_str, theme.dim())];

        if is_editor_focused && r_idx == console.cursor_row {
            // Highlight cursor on active line using char indexing
            let chars: Vec<char> = line_str.chars().collect();
            let char_count = chars.len();
            let col = console.cursor_col.min(char_count);
            let before: String = chars[..col].iter().collect();
            let cursor_char: String = if col < char_count {
                chars[col].to_string()
            } else {
                " ".to_string()
            };
            let after: String = if col + 1 <= char_count {
                chars[col + 1..].iter().collect()
            } else {
                String::new()
            };

            spans.extend(highlight_sql_line(&before, theme));
            spans.push(Span::styled(
                cursor_char,
                theme.selected().add_modifier(Modifier::REVERSED | Modifier::BOLD),
            ));
            spans.extend(highlight_sql_line(&after, theme));
        } else {
            spans.extend(highlight_sql_line(line_str, theme));
        }

        lines.push(Line::from(spans));
    }

    let p = Paragraph::new(lines);
    f.render_widget(p, inner);
}

fn render_result(
    f: &mut Frame,
    area: Rect,
    console: &QueryConsole,
    is_tab_focused: bool,
    theme: &Theme,
) {
    let is_result_focused = is_tab_focused && console.focused_subpane == ConsoleSubpane::Result;
    let border_style = if is_result_focused {
        theme.accent()
    } else {
        theme.border()
    };

    let title = if console.is_executing {
        " Query Result (Executing...) ".to_string()
    } else if let Some(res) = &console.last_result {
        format!(
            " Query Result ({} rows affected, {:.2?}) ",
            res.rows_affected, res.execution_time
        )
    } else if console.execution_error.is_some() {
        " Query Error ".to_string()
    } else {
        " Query Result ".to_string()
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .style(theme.base())
        .title(title);

    if let Some(err) = &console.execution_error {
        let inner = block.inner(area);
        f.render_widget(block, area);

        let err_lines = vec![
            Line::from(Span::styled("Execution failed with error:", theme.error())),
            Line::from(Span::styled(err, theme.base())),
        ];
        let p = Paragraph::new(err_lines).style(theme.base());
        f.render_widget(p, inner);
        return;
    }

    if let Some(res) = &console.last_result {
        if !res.columns.is_empty() {
            if res.records.is_empty() {
                let inner = block.inner(area);
                f.render_widget(block, area);
                crate::ui::widgets::empty::render(
                    f,
                    inner,
                    "Query executed successfully (0 rows returned)",
                    Some("Use [Ctrl+Enter] to re-run or edit SQL above"),
                    theme,
                );
                return;
            }

            let num_cols = res.columns.len();
            let col_offset = console.result_scroll_x.min(num_cols.saturating_sub(1));

            let header_cells = res
                .columns
                .iter()
                .skip(col_offset)
                .map(|col| Cell::from(Span::styled(col, theme.accent().add_modifier(Modifier::BOLD))));
            let header = TableRow::new(header_cells).height(1).bottom_margin(1);

            let rows: Vec<TableRow> = res
                .records
                .iter()
                .enumerate()
                .map(|(r_idx, record)| {
                    let is_row_sel = r_idx == console.result_selected_row && is_result_focused;
                    let cells = record.values.iter().skip(col_offset).enumerate().map(|(rel_idx, val)| {
                        let abs_col = col_offset + rel_idx;
                        let cell_str = val.display_str();
                        let is_cell_sel = is_row_sel && abs_col == console.result_selected_col;
                        let cell_style = if is_cell_sel {
                            theme.selected().add_modifier(Modifier::BOLD)
                        } else if is_row_sel {
                            theme.selected()
                        } else {
                            theme.base()
                        };
                        Cell::from(Span::styled(cell_str, cell_style))
                    });
                    TableRow::new(cells).height(1)
                })
                .collect();

            let widths: Vec<Constraint> = res
                .columns
                .iter()
                .skip(col_offset)
                .map(|_| Constraint::Min(16))
                .collect();

            let table = Table::new(rows, widths)
                .header(header)
                .block(block)
                .style(theme.base());

            let mut state = TableState::default();
            state.select(Some(console.result_selected_row));

            f.render_stateful_widget(table, area, &mut state);
            return;
        } else {
            let inner = block.inner(area);
            f.render_widget(block, area);
            let msg = format!("Query OK, {} rows affected ({:.2?})", res.rows_affected, res.execution_time);
            let p = Paragraph::new(Span::styled(msg, theme.success())).alignment(Alignment::Center);
            f.render_widget(p, inner);
            return;
        }
    }

    let inner = block.inner(area);
    f.render_widget(block, area);
    let empty_text = vec![
        Line::from(Span::styled("No query executed yet.", theme.dim())),
        Line::from(Span::styled(
            "Write a SQL statement above and press [Ctrl+Enter] or [F5] to run.",
            theme.accent(),
        )),
    ];
    let p = Paragraph::new(empty_text).alignment(Alignment::Center);
    f.render_widget(p, inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_query_console_text_and_mutations() {
        let mut console = QueryConsole::new("test.sql".to_string(), Some("SELECT 1;"));
        assert_eq!(console.text(), "SELECT 1;");

        // Insert newline & characters
        console.insert_newline();
        console.insert_char('W');
        console.insert_char('H');
        console.insert_char('E');
        console.insert_char('R');
        console.insert_char('E');
        console.insert_char(' ');
        console.insert_char('1');
        assert_eq!(console.text(), "SELECT 1;\nWHERE 1");

        // Backspace
        console.backspace();
        assert_eq!(console.text(), "SELECT 1;\nWHERE ");
    }

    #[test]
    fn test_query_console_multibyte_utf8() {
        let mut console = QueryConsole::new("utf8.sql".to_string(), Some("SELECT '👋';"));
        console.move_cursor_left();
        console.move_cursor_left();
        console.insert_char('🌍');
        assert_eq!(console.text(), "SELECT '👋🌍';");

        console.backspace();
        assert_eq!(console.text(), "SELECT '👋';");
    }

    #[test]
    fn test_sql_tokenizer() {
        let theme = Theme::dark();
        let spans = highlight_sql_line("SELECT id, `name` FROM users WHERE id = 42 -- comment", &theme);
        assert!(!spans.is_empty());
    }
}

