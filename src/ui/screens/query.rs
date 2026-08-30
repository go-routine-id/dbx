//! Screen S3 / Component: SQL Query Console & Editor.
//! Multi-line SQL text editor with cursor positioning, syntax highlighting, and execution state.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, Paragraph, Row as TableRow, Table, TableState,
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
    /// `last_result` is kept as "the active result" for backward compat with
    /// render/copy code; it always mirrors `results[active_result]`.
    pub last_result: Option<QueryResult>,
    /// All result sets from the last multi-statement execution.
    pub results: Vec<QueryResult>,
    /// Which result set is currently shown / navigated.
    pub active_result: usize,
    pub execution_error: Option<String>,
    pub result_selected_row: usize,
    pub result_selected_col: usize,
    pub result_scroll_x: usize,
    pub focused_subpane: ConsoleSubpane,
    /// Optional picker overlay (query history / saved favorites). When set,
    /// the console routes Up/Down/Enter/Esc to it.
    pub popup: Option<ConsolePopup>,
    /// Live autocomplete suggestions for the current editor position.
    /// Empty = nothing to offer.
    pub autocomplete: Vec<String>,
    pub autocomplete_selected: usize,
    /// Result-pane inner area from the last draw — maps a mouse click to a cell.
    pub result_hit_area: Option<Rect>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsoleSubpane {
    Editor,
    Result,
}

/// A simple picker shown over the console: `items` are `(label, payload)`
/// — for history both are the query text; for favorites label = name,
/// payload = SQL. Enter loads `payload` into the editor.
#[derive(Clone, Debug)]
pub struct ConsolePopup {
    pub title: String,
    pub items: Vec<(String, String)>,
    pub selected: usize,
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
            results: Vec::new(),
            active_result: 0,
            execution_error: None,
            result_selected_row: 0,
            result_selected_col: 0,
            result_scroll_x: 0,
            focused_subpane: ConsoleSubpane::Editor,
            popup: None,
            autocomplete: Vec::new(),
            autocomplete_selected: 0,
            result_hit_area: None,
        }
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// Replace the current completion token (after the last whitespace or
    /// `.`) with the highlighted suggestion.
    pub fn accept_autocomplete(&mut self) {
        let Some(s) = self.autocomplete.get(self.autocomplete_selected).cloned() else {
            return;
        };
        self.autocomplete.clear();
        self.autocomplete_selected = 0;
        let line = &mut self.lines[self.cursor_row];
        let chars: Vec<char> = line.chars().collect();
        let mut start = self.cursor_col;
        while start > 0 {
            let c = chars[start - 1];
            if c.is_whitespace() || c == '.' {
                break;
            }
            start -= 1;
        }
        let head: String = chars[..start].iter().collect();
        let tail: String = chars[self.cursor_col..].iter().collect();
        *line = format!("{head}{s}{tail}");
        self.cursor_col = head.chars().count() + s.chars().count();
    }

    /// Replace the whole editor buffer, resetting the cursor to the end.
    pub fn set_text(&mut self, text: String) {
        let lines: Vec<String> = if text.trim().is_empty() {
            vec![String::new()]
        } else {
            text.lines().map(|s| s.to_string()).collect()
        };
        self.lines = lines;
        self.cursor_row = self.lines.len().saturating_sub(1);
        self.cursor_col = self.lines.last().map(|l| l.chars().count()).unwrap_or(0);
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

/// Pretty-print a SQL statement: each main clause keyword starts a new line.
/// Deliberately minimal (no parser) — but it DOES respect string literals so
/// a `'a FROM b'` value isn't split, and matches keywords exactly (so
/// `SELECTED` / `GROUP_CONCAT` aren't mistaken for clauses).
pub fn format_sql(sql: &str) -> String {
    const CLAUSE_STARTS: &[&str] = &[
        "SELECT", "FROM", "WHERE", "GROUP", "ORDER", "HAVING", "LIMIT", "UNION",
        "ON", "VALUES", "SET", "INTO",
    ];

    // Tokenize word-by-word, keeping quoted literals as a single token so a
    // clause keyword inside a string never triggers a line break.
    let mut out = String::new();
    let mut token = String::new();
    let mut in_string: Option<char> = None;
    for ch in sql.chars() {
        if let Some(quote) = in_string {
            token.push(ch);
            if ch == quote {
                in_string = None;
            }
            continue;
        }
        if ch == '\'' || ch == '"' {
            token.push(ch);
            in_string = Some(ch);
            continue;
        }
        if ch.is_whitespace() {
            flush_sql_token(&mut token, &mut out, &CLAUSE_STARTS);
            continue;
        }
        token.push(ch);
    }
    flush_sql_token(&mut token, &mut out, &CLAUSE_STARTS);
    out.trim_end().to_string()
}

fn flush_sql_token(token: &mut String, out: &mut String, clauses: &[&str]) {
    if token.is_empty() {
        return;
    }
    let is_literal = token.starts_with('\'') || token.starts_with('"');
    if !is_literal && clauses.contains(&token.to_uppercase().as_str()) && !out.is_empty() {
        while out.ends_with(' ') {
            out.pop();
        }
        out.push('\n');
    }
    out.push_str(token);
    out.push(' ');
    token.clear();
}

/// Split a query into individual `;`-separated statements, ignoring `;`
/// inside string literals (incl. backslash-escaped quotes), backtick
/// identifiers, `--` line comments (only when followed by whitespace, per
/// the SQL standard / MySQL) and `/* */` block comments. Returns trimmed,
/// non-empty statements.
pub fn split_statements(sql: &str) -> Vec<String> {
    let mut stmts = Vec::new();
    let mut cur = String::new();
    let mut in_string: Option<char> = None;
    let mut in_line_comment = false;
    let mut in_block_comment = false;

    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        if in_line_comment {
            cur.push(c);
            if c == '\n' {
                in_line_comment = false;
            }
            continue;
        }
        if in_block_comment {
            cur.push(c);
            if c == '*' && chars.peek() == Some(&'/') {
                cur.push(chars.next().unwrap());
                in_block_comment = false;
            }
            continue;
        }
        if let Some(q) = in_string {
            cur.push(c);
            // Backslash escape keeps the next char from closing the string
            // (MySQL `\'`). Consume it so it isn't re-scanned.
            if c == '\\' {
                if let Some(next) = chars.next() {
                    cur.push(next);
                }
                continue;
            }
            if c == q {
                in_string = None;
            }
            continue;
        }
        if c == '\'' || c == '"' || c == '`' {
            in_string = Some(c);
            cur.push(c);
            continue;
        }
        // `--` starts a comment only when followed by whitespace/control
        // (SQL standard; MySQL allows `a--b` as a - (-b)).
        if c == '-' && chars.peek() == Some(&'-') {
            let after = chars.clone().nth(1);
            let is_comment = after
                .map(|n| n.is_whitespace() || n.is_control())
                .unwrap_or(true);
            if is_comment {
                in_line_comment = true;
                cur.push(c);
                cur.push(chars.next().unwrap()); // consume second '-'
                continue;
            }
        }
        if c == '/' && chars.peek() == Some(&'*') {
            in_block_comment = true;
            cur.push(c);
            cur.push(chars.next().unwrap());
            continue;
        }
        if c == ';' {
            let s = cur.trim();
            if !s.is_empty() {
                stmts.push(s.to_string());
            }
            cur.clear();
            continue;
        }
        cur.push(c);
    }
    let tail = cur.trim();
    if !tail.is_empty() {
        stmts.push(tail.to_string());
    }
    stmts
}

/// Is `stmt` a pure comment (nothing but a `--` line comment or a whole
/// `/* ... */` block)? Such statements are skipped at execution.
pub fn is_comment_only(stmt: &str) -> bool {
    let t = stmt.trim();
    t.starts_with("--") || (t.starts_with("/*") && t.ends_with("*/"))
}

/// Tier-1 autocomplete for the text before the cursor:
/// - after FROM/JOIN/INTO/UPDATE → table names
/// - `table.` prefix → column names (from `column_cache` keyed `ns.table`)
/// - otherwise → SQL keywords
pub fn suggest(
    line_before_cursor: &str,
    tables: &[String],
    column_cache: &std::collections::HashMap<String, Vec<String>>,
) -> Vec<String> {
    let trimmed_end = line_before_cursor.trim_end();
    let words: Vec<&str> = trimmed_end.split_whitespace().collect();
    // A trailing space means the current token is empty but the previous one
    // is the context (e.g. "FROM " should suggest all tables).
    let has_trailing_ws = line_before_cursor.len() > trimmed_end.len();
    let current = if has_trailing_ws {
        String::new()
    } else {
        words.last().map(|w| w.to_string()).unwrap_or_default()
    };
    let prev = if has_trailing_ws {
        words.last().copied().unwrap_or("").to_uppercase()
    } else {
        words.iter().rev().nth(1).copied().unwrap_or("").to_uppercase()
    };

    // After FROM/JOIN/INTO/UPDATE → table names.
    if matches!(prev.as_str(), "FROM" | "JOIN" | "INTO" | "UPDATE") {
        let mut t: Vec<String> = tables
            .iter()
            .filter(|t| t.starts_with(&current))
            .cloned()
            .collect();
        t.sort();
        t.truncate(20);
        return t;
    }

    // `table.col` → columns of that table (matched by bare name or ns.table).
    if current.contains('.') {
        let (table_part, col_prefix) = match current.rfind('.') {
            Some(i) => (&current[..i], &current[i + 1..]),
            None => return Vec::new(),
        };
        let mut cols: Vec<String> = column_cache
            .iter()
            .filter(|(key, _)| {
                key.ends_with(&format!(".{table_part}")) || key.as_str() == table_part
            })
            .flat_map(|(_, v)| v.iter().cloned())
            .collect();
        cols.sort();
        cols.dedup();
        cols.retain(|c| c.starts_with(col_prefix));
        cols.truncate(20);
        return cols;
    }

    // Otherwise keywords, but only once something is typed (avoids noise).
    if current.is_empty() {
        return Vec::new();
    }
    let upper = current.to_uppercase();
    SQL_KEYWORDS
        .iter()
        .filter(|k| k.starts_with(&upper))
        .map(|k| k.to_string())
        .take(20)
        .collect()
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
    console: &mut QueryConsole,
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

    if let Some(popup) = &console.popup {
        render_console_popup(f, area, popup, theme);
    } else if !console.autocomplete.is_empty() {
        render_autocomplete(f, chunks[0], console, theme);
    }
}

/// Small overlay at the bottom of the editor showing live autocomplete
/// suggestions. Tab inserts the highlighted one.
fn render_autocomplete(f: &mut Frame, area: Rect, console: &QueryConsole, theme: &Theme) {
    let height = 6.min(area.height.saturating_sub(1));
    let popup_area = Rect {
        x: area.x + 1,
        y: area.y + area.height.saturating_sub(height),
        width: area.width.saturating_sub(2),
        height,
    };
    if popup_area.height < 3 {
        return;
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme.accent())
        .style(theme.panel())
        .title(" Complete (Tab) ");
    let inner = block.inner(popup_area);
    f.render_widget(Clear, popup_area);
    f.render_widget(block, popup_area);

    let visible = inner.height as usize;
    let sel = console
        .autocomplete_selected
        .min(console.autocomplete.len().saturating_sub(1));
    let start = sel.saturating_sub(visible / 2);
    let mut lines = Vec::new();
    for (i, s) in console
        .autocomplete
        .iter()
        .skip(start)
        .take(visible)
        .enumerate()
    {
        let is_sel = start + i == sel;
        lines.push(Line::from(Span::styled(
            if is_sel { format!("▶ {s}") } else { format!("  {s}") },
            if is_sel {
                theme.selected()
            } else {
                theme.base()
            },
        )));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// Centered picker overlay for history / favorites.
fn render_console_popup(f: &mut Frame, area: Rect, popup: &ConsolePopup, theme: &Theme) {
    let width = 72.min(area.width.saturating_sub(4));
    let height = 16.min(area.height.saturating_sub(2));
    let popup_area = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    f.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme.accent())
        .style(theme.panel())
        .title(format!(" {} ", popup.title));
    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    // List on top, hint pinned to the bottom row (not overlaying the list).
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let max_rows = chunks[0].height as usize;
    let sel = popup.selected.min(popup.items.len().saturating_sub(1));
    let start = sel.saturating_sub(max_rows / 2);
    let mut lines = Vec::new();
    for (i, (label, _)) in popup.items.iter().skip(start).take(max_rows).enumerate() {
        let is_sel = start + i == sel;
        let style = if is_sel {
            theme.selected()
        } else {
            theme.base()
        };
        lines.push(Line::from(vec![
            Span::styled(if is_sel { "▶ " } else { "  " }, theme.accent()),
            Span::styled(label.clone(), style),
        ]));
    }
    f.render_widget(Paragraph::new(lines), chunks[0]);

    let hint = Line::from(Span::styled(
        " ↑/↓ navigate · Enter load · Esc close ",
        theme.dim(),
    ));
    f.render_widget(Paragraph::new(hint), chunks[1]);
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
    console: &mut QueryConsole,
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
        let multi = if console.results.len() > 1 {
            format!(" [result {}/{}]", console.active_result + 1, console.results.len())
        } else {
            String::new()
        };
        format!(
            " Query Result{multi} ({} rows affected, {:.2?}) ",
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
    console.result_hit_area = Some(block.inner(area));

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

    #[test]
    fn test_format_sql_breaks_on_clauses() {
        let f = format_sql("SELECT a, b FROM t WHERE a = 1 ORDER BY b");
        assert_eq!(f, "SELECT a, b\nFROM t\nWHERE a = 1\nORDER BY b");

        // Case-insensitive clause detection.
        let f = format_sql("select * from t");
        assert_eq!(f, "select *\nfrom t");

        // No trailing whitespace.
        assert!(!f.ends_with(' '));
    }

    #[test]
    fn test_format_sql_respects_string_literals() {
        // 'a FROM b' is a string value — the clause detector must not split it.
        let f = format_sql("SELECT 'a FROM b' FROM t");
        assert_eq!(f, "SELECT 'a FROM b'\nFROM t");
    }

    #[test]
    fn test_format_sql_exact_keyword_match() {
        // SELECTED and GROUP_CONCAT are not clause keywords.
        let f = format_sql("SELECT SELECTED FROM t WHERE GROUP_CONCAT(x) > 1");
        assert!(f.contains("SELECT SELECTED\nFROM t"));
        assert!(!f.contains("\nGROUP_CONCAT"));
    }

    #[test]
    fn test_split_statements_basic() {
        assert_eq!(
            split_statements("SELECT 1; SELECT 2;"),
            vec!["SELECT 1".to_string(), "SELECT 2".to_string()]
        );
        // Semicolons inside string literals / comments must not split.
        let stmts = split_statements("SELECT 'a;b'; SELECT 2 -- ; done");
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0], "SELECT 'a;b'");
        assert!(stmts[1].contains("SELECT 2"));
        // Block comments.
        let stmts = split_statements("SELECT 1 /* ; */; SELECT 3");
        assert_eq!(stmts, vec!["SELECT 1 /* ; */".to_string(), "SELECT 3".to_string()]);
        // Trailing semicolon ignored; empty input → no statements.
        assert!(split_statements(";;;").is_empty());
        assert!(split_statements("   ").is_empty());
    }

    #[test]
    fn test_split_statements_escaped_quotes() {
        // MySQL `\'` — the escaped quote must not close the string.
        let stmts = split_statements(r"SELECT 'it\'s'; SELECT 2");
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0], r"SELECT 'it\'s'");
    }

    #[test]
    fn test_split_statements_backtick_identifier() {
        // `a;b` is a MySQL identifier — the `;` inside must not split.
        let stmts = split_statements("SELECT `a;b` FROM t; SELECT 2");
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0], "SELECT `a;b` FROM t");
    }

    #[test]
    fn test_suggest() {
        let mut cache = std::collections::HashMap::new();
        cache.insert(
            "shop.users".to_string(),
            vec!["id".to_string(), "name".to_string(), "email".to_string()],
        );
        let tables = vec!["users".to_string(), "orders".to_string()];

        // After FROM → table names.
        let s = suggest("SELECT * FROM us", &tables, &cache);
        assert!(s.contains(&"users".to_string()));

        // `table.col` → column names of that table.
        let s = suggest("SELECT users.na", &tables, &cache);
        assert!(s.contains(&"name".to_string()));
        assert!(!s.contains(&"id".to_string()));

        // Otherwise keywords.
        let s = suggest("SEL", &tables, &cache);
        assert!(s.contains(&"SELECT".to_string()));

        // Trailing space after FROM → suggest all tables.
        let s = suggest("SELECT * FROM ", &tables, &cache);
        assert!(s.contains(&"users".to_string()));
        assert!(s.contains(&"orders".to_string()));

        // Keyword context with empty prefix → no keyword noise.
        assert!(suggest("SELECT ", &tables, &cache).is_empty());
    }

    #[test]
    fn test_split_statements_double_dash_needs_whitespace() {
        // `a--b` is valid MySQL arithmetic (a - (-b)), not a comment.
        let stmts = split_statements("SELECT a--b FROM t; SELECT 2");
        assert_eq!(stmts.len(), 2);
        // But `-- ` IS a comment.
        let stmts = split_statements("SELECT 1; -- done");
        assert_eq!(stmts, vec!["SELECT 1".to_string(), "-- done".to_string()]);
        assert!(is_comment_only("-- done"));
        assert!(!is_comment_only("SELECT 1"));
        assert!(is_comment_only("/* just a note */"));
        assert!(!is_comment_only("/* note */ SELECT 1"));
    }
}

