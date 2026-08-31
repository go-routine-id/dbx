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

/// Smallest useful editor: borders plus a few lines of SQL.
const MIN_EDITOR_H: u16 = 6;

/// Width of the editor's line-number gutter (`"  1 │ "`).
const GUTTER_W: usize = 6;

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
    /// Index of the first visible data row (vertical scroll) — mirrors the
    /// window the result table renders, so mouse clicks map to the right row.
    pub result_scroll_y: usize,
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
    /// X start of each visible result column, computed at draw time.
    pub result_col_starts: Vec<u16>,
    /// Auto re-run interval. `None` = off. Set with `Ctrl+W`; the event loop
    /// re-executes the query whenever `last_run` is older than this.
    pub watch_interval: Option<std::time::Duration>,
    /// When the watched query last executed.
    pub last_run: Option<std::time::Instant>,
    /// First editor line drawn. The editor pane is only a few rows tall, so a
    /// query of any real length needs the view to follow the cursor.
    pub editor_scroll: usize,
    /// First editor column drawn, for lines wider than the pane.
    pub editor_scroll_x: usize,
}

/// Interval cycled through by `Ctrl+W`, in seconds. `None` (off) is the
/// entry and exit of the cycle so watching is never left on by accident.
pub const WATCH_INTERVALS: [u64; 4] = [1, 5, 15, 60];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsoleSubpane {
    Editor,
    Result,
}

/// A single entry in the console picker.
#[derive(Clone, Debug)]
pub struct ConsolePopupItem {
    /// Display text (for collections this already includes a `[collection]`
    /// badge prefix).
    pub label: String,
    /// SQL loaded into the editor on Enter.
    pub payload: String,
    /// `(collection, name)` used to delete this entry; `None` for history.
    pub delete_key: Option<(String, String)>,
}

/// What kind of list the picker shows (drives hints + whether `d` deletes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsolePopupMode {
    History,
    Collections,
}

/// A searchable picker shown over the console. `all_items` is the full list;
/// `items` is the live-search-filtered view actually rendered / navigated.
#[derive(Clone, Debug)]
pub struct ConsolePopup {
    pub title: String,
    pub all_items: Vec<ConsolePopupItem>,
    pub items: Vec<ConsolePopupItem>,
    pub selected: usize,
    /// Live search filter (empty = show everything).
    pub filter: String,
    pub mode: ConsolePopupMode,
}

impl ConsolePopup {
    pub fn new(title: String, items: Vec<ConsolePopupItem>, mode: ConsolePopupMode) -> Self {
        let mut popup = Self {
            title,
            all_items: items.clone(),
            items,
            selected: 0,
            filter: String::new(),
            mode,
        };
        popup.rebuild();
        popup
    }

    /// Re-filter `items` from `all_items` using `filter`, and clamp `selected`.
    pub fn rebuild(&mut self) {
        let f = self.filter.to_lowercase();
        self.items = if f.is_empty() {
            self.all_items.clone()
        } else {
            self.all_items
                .iter()
                .filter(|i| i.label.to_lowercase().contains(&f))
                .cloned()
                .collect()
        };
        if self.selected >= self.items.len() {
            self.selected = self.items.len().saturating_sub(1);
        }
    }

    pub fn push_filter(&mut self, ch: char) {
        self.filter.push(ch);
        self.rebuild();
    }

    pub fn pop_filter(&mut self) {
        self.filter.pop();
        self.rebuild();
    }
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
            result_scroll_y: 0,
            focused_subpane: ConsoleSubpane::Editor,
            popup: None,
            autocomplete: Vec::new(),
            autocomplete_selected: 0,
            result_hit_area: None,
            result_col_starts: Vec::new(),
            watch_interval: None,
            last_run: None,
            editor_scroll: 0,
            editor_scroll_x: 0,
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
        // Stale suggestions would otherwise linger after the text is replaced.
        self.autocomplete.clear();
        self.autocomplete_selected = 0;
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

    /// Forward delete: removes the character the cursor sits before, and
    /// joins the next line when already at the end of one.
    pub fn delete_forward(&mut self) {
        let Some(line) = self.lines.get(self.cursor_row) else {
            return;
        };
        let len = line.chars().count();
        if self.cursor_col < len {
            let mut chars: Vec<char> = line.chars().collect();
            chars.remove(self.cursor_col);
            self.lines[self.cursor_row] = chars.into_iter().collect();
        } else if self.cursor_row + 1 < self.lines.len() {
            let next = self.lines.remove(self.cursor_row + 1);
            self.lines[self.cursor_row].push_str(&next);
        }
    }

    pub fn move_line_start(&mut self) {
        self.cursor_col = 0;
    }

    pub fn move_line_end(&mut self) {
        self.cursor_col = self
            .lines
            .get(self.cursor_row)
            .map(|l| l.chars().count())
            .unwrap_or(0);
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

/// If `s[i..]` begins a PostgreSQL dollar-quoted string (`$$…$$` or
/// `$tag$…$tag$`), return the byte index just past its closing delimiter.
/// Otherwise return `None` (the `$` is a normal character, e.g. a parameter).
pub fn dollar_quote_end(s: &str, i: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    if bytes.get(i) != Some(&b'$') {
        return None;
    }
    // Parse the opening delimiter: `$$` (empty tag) or `$ident$`.
    let mut j = i + 1;
    if bytes.get(j) == Some(&b'$') {
        j += 1;
    } else {
        let first = *bytes.get(j)?;
        if !(first.is_ascii_alphabetic() || first == b'_') {
            return None;
        }
        j += 1;
        while let Some(&b) = bytes.get(j) {
            if b.is_ascii_alphanumeric() || b == b'_' {
                j += 1;
            } else {
                break;
            }
        }
        if bytes.get(j) != Some(&b'$') {
            return None;
        }
        j += 1;
    }
    let tag = &s[i..j];
    let close = s[j..].find(tag)?;
    Some(j + close + tag.len())
}

/// Pretty-print a SQL statement: each main clause keyword starts a new line.
/// Deliberately minimal (no parser) — but it DOES respect string literals
/// (incl. backslash-escaped quotes), backtick identifiers, `--` / `/* */`
/// comments and `$tag$` dollar-quotes, and matches keywords exactly (so
/// `SELECTED` / `GROUP_CONCAT` aren't mistaken for clauses).
pub fn format_sql(sql: &str) -> String {
    const CLAUSE_STARTS: &[&str] = &[
        "SELECT", "FROM", "WHERE", "GROUP", "ORDER", "HAVING", "LIMIT", "UNION",
        "ON", "VALUES", "SET", "INTO",
    ];

    let mut out = String::new();
    let mut token = String::new();
    let mut in_string: Option<char> = None;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = sql[i..].chars().next().unwrap();

        if in_line_comment {
            // Preserve the rest of the line verbatim; only the newline ends it.
            if c == '\n' {
                in_line_comment = false;
                flush_sql_token(&mut token, &mut out, CLAUSE_STARTS);
            } else {
                out.push(c);
            }
            i += c.len_utf8();
            continue;
        }
        if in_block_comment {
            out.push(c);
            if c == '*' && bytes.get(i + 1) == Some(&b'/') {
                out.push('/');
                in_block_comment = false;
                i += 2;
            } else {
                i += c.len_utf8();
            }
            continue;
        }
        if let Some(q) = in_string {
            out.push(c);
            // Backslash escape (MySQL `\'`) — consume the next char too.
            if c == '\\' {
                if let Some(nc) = sql[i + 1..].chars().next() {
                    out.push(nc);
                    i += 1 + nc.len_utf8();
                } else {
                    i += 1;
                }
                continue;
            }
            if c == q {
                in_string = None;
            }
            i += c.len_utf8();
            continue;
        }
        if c == '\'' || c == '"' || c == '`' {
            flush_sql_token(&mut token, &mut out, CLAUSE_STARTS);
            in_string = Some(c);
            out.push(c);
            i += 1;
            continue;
        }
        if c == '$' && let Some(end) = dollar_quote_end(sql, i) {
            flush_sql_token(&mut token, &mut out, CLAUSE_STARTS);
            out.push_str(&sql[i..end]);
            i = end;
            continue;
        }
        if c == '-' && bytes.get(i + 1) == Some(&b'-') {
            let after = sql[i + 2..].chars().next();
            let is_comment = after
                .map(|n| n.is_whitespace() || n.is_control())
                .unwrap_or(true);
            if is_comment {
                flush_sql_token(&mut token, &mut out, CLAUSE_STARTS);
                out.push('-');
                out.push('-');
                in_line_comment = true;
                i += 2;
                continue;
            }
        }
        if c == '/' && bytes.get(i + 1) == Some(&b'*') {
            flush_sql_token(&mut token, &mut out, CLAUSE_STARTS);
            out.push('/');
            out.push('*');
            in_block_comment = true;
            i += 2;
            continue;
        }
        if c.is_whitespace() {
            flush_sql_token(&mut token, &mut out, CLAUSE_STARTS);
            i += c.len_utf8();
            continue;
        }
        token.push(c);
        i += c.len_utf8();
    }
    flush_sql_token(&mut token, &mut out, CLAUSE_STARTS);
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
/// the SQL standard / MySQL), `/* */` block comments, and PostgreSQL
/// `$tag$` dollar-quoted bodies. Returns trimmed, non-empty statements.
pub fn split_statements(sql: &str) -> Vec<String> {
    let mut stmts = Vec::new();
    let mut cur = String::new();
    let mut in_string: Option<char> = None;
    let mut in_line_comment = false;
    let mut in_block_comment = false;

    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = sql[i..].chars().next().unwrap();
        if in_line_comment {
            cur.push(c);
            if c == '\n' {
                in_line_comment = false;
            }
            i += c.len_utf8();
            continue;
        }
        if in_block_comment {
            cur.push(c);
            if c == '*' && bytes.get(i + 1) == Some(&b'/') {
                cur.push('/');
                in_block_comment = false;
                i += 2;
            } else {
                i += c.len_utf8();
            }
            continue;
        }
        if let Some(q) = in_string {
            cur.push(c);
            // Backslash escape keeps the next char from closing the string
            // (MySQL `\'`). Consume it so it isn't re-scanned.
            if c == '\\' {
                if let Some(nc) = sql[i + 1..].chars().next() {
                    cur.push(nc);
                    i += 1 + nc.len_utf8();
                } else {
                    i += 1;
                }
                continue;
            }
            if c == q {
                in_string = None;
            }
            i += c.len_utf8();
            continue;
        }
        if c == '\'' || c == '"' || c == '`' {
            in_string = Some(c);
            cur.push(c);
            i += 1;
            continue;
        }
        // PostgreSQL dollar-quote: a `;` inside `$$…$$` / `$tag$…$tag$` is
        // part of the body and must not split the statement.
        if c == '$' && let Some(end) = dollar_quote_end(sql, i) {
            cur.push_str(&sql[i..end]);
            i = end;
            continue;
        }
        // `--` starts a comment only when followed by whitespace/control
        // (SQL standard; MySQL allows `a--b` as a - (-b)).
        if c == '-' && bytes.get(i + 1) == Some(&b'-') {
            let after = sql[i + 2..].chars().next();
            let is_comment = after
                .map(|n| n.is_whitespace() || n.is_control())
                .unwrap_or(true);
            if is_comment {
                in_line_comment = true;
                cur.push('-');
                cur.push('-');
                i += 2;
                continue;
            }
        }
        if c == '/' && bytes.get(i + 1) == Some(&b'*') {
            in_block_comment = true;
            cur.push('/');
            cur.push('*');
            i += 2;
            continue;
        }
        if c == ';' {
            let s = cur.trim();
            if !s.is_empty() {
                stmts.push(s.to_string());
            }
            cur.clear();
            i += 1;
            continue;
        }
        cur.push(c);
        i += c.len_utf8();
    }
    let tail = cur.trim();
    if !tail.is_empty() {
        stmts.push(tail.to_string());
    }
    stmts
}

/// Is `stmt` a pure comment — nothing but `--` line / `/* */` block comments
/// and whitespace? Such statements are skipped at execution. A statement like
/// `-- note\nSELECT 1` is NOT comment-only (the `SELECT` must still run).
pub fn is_comment_only(stmt: &str) -> bool {
    strip_comments(stmt).trim().is_empty()
}

/// Remove `--` line comments and `/* */` block comments, leaving string
/// literals, backtick identifiers and `$tag$` dollar-quotes untouched so a
/// `'--'` value or `$body$ -- x $body$` isn't mistaken for a comment.
fn strip_comments(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut in_string: Option<char> = None;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = sql[i..].chars().next().unwrap();
        if in_line_comment {
            if c == '\n' {
                in_line_comment = false;
                out.push(c);
            }
            i += c.len_utf8();
            continue;
        }
        if in_block_comment {
            if c == '*' && bytes.get(i + 1) == Some(&b'/') {
                in_block_comment = false;
                i += 2;
            } else {
                i += c.len_utf8();
            }
            continue;
        }
        if let Some(q) = in_string {
            out.push(c);
            if c == '\\' {
                if let Some(nc) = sql[i + 1..].chars().next() {
                    out.push(nc);
                    i += 1 + nc.len_utf8();
                } else {
                    i += 1;
                }
                continue;
            }
            if c == q {
                in_string = None;
            }
            i += c.len_utf8();
            continue;
        }
        if c == '\'' || c == '"' || c == '`' {
            in_string = Some(c);
            out.push(c);
            i += 1;
            continue;
        }
        if c == '$' && let Some(end) = dollar_quote_end(sql, i) {
            out.push_str(&sql[i..end]);
            i = end;
            continue;
        }
        if c == '-' && bytes.get(i + 1) == Some(&b'-') {
            let after = sql[i + 2..].chars().next();
            let is_comment = after
                .map(|n| n.is_whitespace() || n.is_control())
                .unwrap_or(true);
            if is_comment {
                in_line_comment = true;
                i += 2;
                continue;
            }
        }
        if c == '/' && bytes.get(i + 1) == Some(&b'*') {
            in_block_comment = true;
            i += 2;
            continue;
        }
        out.push(c);
        i += c.len_utf8();
    }
    out
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
    // The editor grows with the query instead of sitting at a fixed height: a
    // one-liner should not eat the result pane, and a 30-line query should not
    // be squeezed into 8 rows. Always leaves room for the results.
    let wanted = console.lines.len().saturating_add(2) as u16;
    let max_editor = (area.height * 3 / 5).max(MIN_EDITOR_H);
    let editor_h = wanted.clamp(MIN_EDITOR_H, max_editor);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(editor_h), // Editor Area (Top)
            Constraint::Min(5),           // Result Grid / Info Area (Bottom)
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

/// Centered searchable picker overlay for history / saved queries.
fn render_console_popup(f: &mut Frame, area: Rect, popup: &ConsolePopup, theme: &Theme) {
    let width = 78.min(area.width.saturating_sub(4));
    let height = 18.min(area.height.saturating_sub(2));
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

    // Search bar (top), list (middle), hint pinned to the bottom row.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let search = Line::from(vec![
        Span::styled("/ ", theme.dim()),
        Span::styled(popup.filter.clone(), theme.base()),
        Span::styled("█", theme.accent()),
    ]);
    f.render_widget(Paragraph::new(search), chunks[0]);

    let max_rows = chunks[1].height as usize;
    let sel = popup.selected.min(popup.items.len().saturating_sub(1));
    let start = sel.saturating_sub(max_rows / 2);
    let mut lines = Vec::new();
    for (i, item) in popup.items.iter().skip(start).take(max_rows).enumerate() {
        let is_sel = start + i == sel;
        let style = if is_sel {
            theme.selected()
        } else {
            theme.base()
        };
        lines.push(Line::from(vec![
            Span::styled(if is_sel { "▶ " } else { "  " }, theme.accent()),
            Span::styled(item.label.clone(), style),
        ]));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled("  no matches", theme.dim())));
    }
    f.render_widget(Paragraph::new(lines), chunks[1]);

    let hint = match popup.mode {
        ConsolePopupMode::Collections => " ↑/↓ navigate · type search · Enter load · Ctrl+D delete · Esc close ",
        ConsolePopupMode::History => " ↑/↓ navigate · type search · Enter load · Esc close ",
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(hint, theme.dim()))),
        chunks[2],
    );
}

fn render_editor(
    f: &mut Frame,
    area: Rect,
    console: &mut QueryConsole,
    is_tab_focused: bool,
    theme: &Theme,
) {
    let is_editor_focused = is_tab_focused && console.focused_subpane == ConsoleSubpane::Editor;
    let border_style = if is_editor_focused {
        theme.accent()
    } else {
        theme.border()
    };

    let title = format!(" SQL Editor: {} [F5 / Ctrl+Enter / Alt+Enter to run] ", console.title);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .style(theme.base())
        .title(title);

    let inner = block.inner(area);
    f.render_widget(block, area);

    // Keep the cursor inside the visible window, vertically and horizontally,
    // before drawing it. A long SELECT would otherwise run off the right edge
    // and take the cursor with it.
    let visible = inner.height as usize;
    let mut top = console.editor_scroll.min(console.cursor_row);
    if console.cursor_row >= top + visible {
        top = console.cursor_row + 1 - visible;
    }
    console.editor_scroll = top;

    let text_w = (inner.width as usize).saturating_sub(GUTTER_W) .max(1);
    let mut left = console.editor_scroll_x.min(console.cursor_col);
    if console.cursor_col >= left + text_w {
        left = console.cursor_col + 1 - text_w;
    }
    console.editor_scroll_x = left;

    /// Characters `[left, left + text_w)` of a line, as a String.
    fn window(line: &str, left: usize, text_w: usize) -> String {
        line.chars().skip(left).take(text_w).collect()
    }

    let mut lines = Vec::new();
    for (r_idx, line_str) in console
        .lines
        .iter()
        .enumerate()
        .skip(top)
        .take(visible)
    {
        let line_num_str = format!("{:>3} │ ", r_idx + 1);
        let mut spans = vec![Span::styled(line_num_str, theme.dim())];
        let shown = window(line_str, left, text_w);

        if is_editor_focused && r_idx == console.cursor_row {
            // The cursor block sits on the char LEFT of the insertion point —
            // the one backspace removes — so what's highlighted is exactly
            // what the user can delete.
            let chars: Vec<char> = shown.chars().collect();
            // Cursor position translated into the visible window.
            let cur = console.cursor_col.saturating_sub(left).min(chars.len());
            if cur > 0 {
                let before: String = chars[..cur - 1].iter().collect();
                let cursor_char: String = chars[cur - 1].to_string();
                let after: String = chars[cur..].iter().collect();
                spans.extend(highlight_sql_line(&before, theme));
                spans.push(Span::styled(
                    cursor_char,
                    theme.selected().add_modifier(Modifier::REVERSED | Modifier::BOLD),
                ));
                spans.extend(highlight_sql_line(&after, theme));
            } else {
                // Cursor at column 0 (or empty line): render a block cursor
                // before the first character so the caret stays visible.
                spans.push(Span::styled(
                    " ",
                    theme.selected().add_modifier(Modifier::REVERSED | Modifier::BOLD),
                ));
                spans.extend(highlight_sql_line(&shown, theme));
            }
        } else {
            spans.extend(highlight_sql_line(&shown, theme));
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
        let watch = match console.watch_interval {
            Some(d) => format!(" [watch {}s]", d.as_secs()),
            None => String::new(),
        };
        let multi = if console.results.len() > 1 {
            format!(" [result {}/{}]", console.active_result + 1, console.results.len())
        } else {
            String::new()
        };
        format!(
            " Query Result{multi}{watch} ({} rows affected, {:.2?}) ",
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

            // Record rendered x-starts so mouse clicks map to actual widths.
            // Columns are `Constraint::Min(16)`: when they fit they share the
            // width evenly, and when they overflow each is 16 wide (clipped) —
            // so the per-column stride is at least 16, never `width/visible`.
            if let Some(inner) = console.result_hit_area {
                let num_visible = num_cols.saturating_sub(col_offset).max(1);
                let col_w = (inner.width / num_visible as u16).max(16);
                console.result_col_starts = (0..num_visible)
                    .map(|i| inner.x + (i as u16 * col_w))
                    .collect();
            }

            // Vertical scroll: keep the selected row inside the visible window
            // and slice `records` to that window so the mouse handler can map a
            // click with the same offset (header + bottom margin = 2 rows).
            let inner_height = console.result_hit_area.map(|r| r.height).unwrap_or(0) as usize;
            let visible_rows = inner_height.saturating_sub(2).max(1);
            let total_rows = res.records.len();
            if console.result_selected_row < console.result_scroll_y {
                console.result_scroll_y = console.result_selected_row;
            }
            if console.result_selected_row >= console.result_scroll_y + visible_rows {
                console.result_scroll_y = console.result_selected_row - visible_rows + 1;
            }
            if console.result_scroll_y > total_rows.saturating_sub(visible_rows) {
                console.result_scroll_y = total_rows.saturating_sub(visible_rows);
            }

            let header_cells = res
                .columns
                .iter()
                .skip(col_offset)
                .map(|col| Cell::from(Span::styled(col, theme.accent().add_modifier(Modifier::BOLD))));
            let header = TableRow::new(header_cells).height(1).bottom_margin(1);

            let rows: Vec<TableRow> = res
                .records
                .iter()
                .skip(console.result_scroll_y)
                .take(visible_rows)
                .enumerate()
                .map(|(rel_idx, record)| {
                    let r_idx = console.result_scroll_y + rel_idx;
                    // Same rule as the grid: the cursor stays visible when
                    // the result pane is not focused, just quieter.
                    let is_row_sel = r_idx == console.result_selected_row;
                    let cells = record.values.iter().skip(col_offset).enumerate().map(|(i, val)| {
                        let abs_col = col_offset + i;
                        let cell_str = val.display_str();
                        let is_cell_sel = is_row_sel && abs_col == console.result_selected_col;
                        let sel = if is_result_focused {
                            theme.selected()
                        } else {
                            theme.selected_inactive()
                        };
                        let cell_style = if is_cell_sel {
                            sel.add_modifier(Modifier::BOLD)
                        } else if is_row_sel {
                            sel
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
            state.select(Some(
                console.result_selected_row.saturating_sub(console.result_scroll_y),
            ));

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
    fn test_delete_forward_and_line_motions() {
        let mut c = QueryConsole::new("t.sql".to_string(), Some("SELECT 1\nFROM t"));
        c.cursor_row = 0;
        c.cursor_col = 0;

        // End / Home walk the current line, not the buffer.
        c.move_line_end();
        assert_eq!(c.cursor_col, "SELECT 1".chars().count());
        c.move_line_start();
        assert_eq!(c.cursor_col, 0);

        // Forward delete removes the char under the cursor.
        c.delete_forward();
        assert_eq!(c.text(), "ELECT 1\nFROM t");

        // At end of line it joins the next one (and never panics at the very end).
        c.move_line_end();
        c.delete_forward();
        assert_eq!(c.text(), "ELECT 1FROM t");
        c.cursor_row = 0;
        c.move_line_end();
        c.delete_forward();
        assert_eq!(c.text(), "ELECT 1FROM t", "delete at the very end is a no-op");
    }

    #[test]
    fn test_editor_horizontal_window_keeps_the_cursor_visible() {
        // A long single-line SELECT used to run off the right edge, taking the
        // cursor with it — there was no horizontal scrolling at all.
        let long = format!("SELECT {} FROM t", (1..=60).map(|i| format!("col{i}")).collect::<Vec<_>>().join(", "));
        let mut c = QueryConsole::new("t.sql".to_string(), Some(&long));
        c.move_line_end();
        let width = 40usize; // text columns available after the gutter

        // Mirrors the renderer's horizontal window calculation.
        let mut left = c.editor_scroll_x.min(c.cursor_col);
        if c.cursor_col >= left + width {
            left = c.cursor_col + 1 - width;
        }
        c.editor_scroll_x = left;

        assert!(c.cursor_col > width, "test needs a line wider than the pane");
        assert!(
            c.cursor_col >= c.editor_scroll_x && c.cursor_col < c.editor_scroll_x + width,
            "cursor {} outside window {}..{}",
            c.cursor_col,
            c.editor_scroll_x,
            c.editor_scroll_x + width
        );

        // Back at the start of the line the window must return to column 0.
        c.move_line_start();
        let left = c.editor_scroll_x.min(c.cursor_col);
        assert_eq!(left, 0);
    }

    #[test]
    fn test_editor_scroll_follows_the_cursor() {
        // The editor pane is a few rows tall; a long query must not scroll the
        // cursor out of sight (it used to render only the first lines).
        let sql = (1..=30).map(|i| format!("-- line {i}")).collect::<Vec<_>>().join("\n");
        let mut c = QueryConsole::new("t.sql".to_string(), Some(&sql));
        assert_eq!(c.lines.len(), 30);
        // `new` puts the cursor on the last line.
        assert_eq!(c.cursor_row, 29);

        // Mirrors the renderer's window calculation for an 8-row pane.
        let visible = 8usize;
        let mut top = c.editor_scroll.min(c.cursor_row);
        if c.cursor_row >= top + visible {
            top = c.cursor_row + 1 - visible;
        }
        c.editor_scroll = top;
        assert!(
            c.cursor_row >= c.editor_scroll && c.cursor_row < c.editor_scroll + visible,
            "cursor {} outside window {}..{}",
            c.cursor_row,
            c.editor_scroll,
            c.editor_scroll + visible
        );
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
    fn test_split_statements_dollar_quote() {
        // A `;` inside a PostgreSQL `$$…$$` / `$body$…$body$` must not split.
        let stmts = split_statements("CREATE FUNCTION f() RETURNS void AS $$ BEGIN; END; $$ LANGUAGE plpgsql; SELECT 2");
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains("$$ BEGIN; END; $$"));

        let stmts = split_statements("SELECT $tag$ a; b $tag$; SELECT 2");
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0], "SELECT $tag$ a; b $tag$");

        // A bare `$` (not a dollar-quote) is left alone.
        let stmts = split_statements("SELECT $1; SELECT 2");
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0], "SELECT $1");
    }

    #[test]
    fn test_is_comment_only() {
        // Pure comments are skipped.
        assert!(is_comment_only("-- just a note"));
        assert!(is_comment_only("/* whole block */"));
        assert!(is_comment_only("  -- leading ws\n-- more"));
        // A statement after a leading comment still runs.
        assert!(!is_comment_only("-- note\nSELECT 1"));
        assert!(!is_comment_only("/* lead */ SELECT 1"));
        assert!(!is_comment_only("SELECT 1"));
        assert!(!is_comment_only("'-- not a comment'"));
    }

    #[test]
    fn test_format_sql_ignores_keywords_in_literals_comments() {
        // Clause keywords inside backticks / dollar-quotes / comments must not
        // trigger a line break.
        let f = format_sql("SELECT `FROM` FROM t -- WHERE note\nORDER BY `order`");
        assert!(!f.contains("SELECT \nFROM") || f.starts_with("SELECT `FROM`"));
        // `FROM` inside the backtick is literal.
        assert!(f.starts_with("SELECT `FROM`\nFROM t"));
        // `$x$ FROM $x$` is a dollar-quoted literal.
        let f = format_sql("SELECT $x$ FROM $x$ FROM t");
        assert!(f.starts_with("SELECT $x$ FROM $x$\nFROM t"));
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

