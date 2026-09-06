//! Screen S3 / Component: SQL Query Console & Editor.
//! Multi-line SQL text editor with cursor positioning, syntax highlighting, and execution state.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, Paragraph, Row as TableRow, Table, TableState,
    Wrap,
};

use unicode_width::UnicodeWidthStr;

use crate::console::dollar_quote_end;
use crate::driver::QueryResult;
use crate::theme::Theme;

/// Smallest useful editor: borders plus a few lines of SQL.
const MIN_EDITOR_H: u16 = 6;

/// Width of the editor's line-number gutter (`"  1 │ "`).
const GUTTER_W: usize = 6;
/// Keywords are only suggested from this many typed characters on.
const MIN_KEYWORD_PREFIX: usize = 2;
/// Total popup height, borders included — four suggestions at a time.
const AC_POPUP_H: u16 = 6;
/// Title of the suggestion box; also drives its minimum width.
const AC_TITLE: &str = " Complete (Tab) ";
/// Columns the box adds around a suggestion: two borders plus the two-cell
/// "▶ " selection marker, and one trailing space so text never touches the
/// right border.
const AC_CHROME_W: u16 = 5;

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
    /// Editor text area from the last draw, so PageUp/PageDown move by the
    /// number of rows actually on screen.
    pub editor_hit_area: Option<Rect>,
    /// `v` expands the selected result row vertically, the same way a table
    /// tab does — a query returning a wide row was unreadable otherwise.
    pub row_detail: bool,
    pub row_detail_scroll: usize,
    /// Free-text search across every cell of the result, mirroring the table
    /// grid's `Ctrl+F` / `Ctrl+G`. Empty = no search active.
    pub search_query: String,
    /// True while the query is being typed; the pane owns every key then.
    pub search_editing: bool,
    pub search_buffer: String,
    /// Caret position in screen cells, recorded by `render_editor` (the only
    /// place that knows the block, gutter and scroll offsets). The suggestion
    /// box anchors to it rather than re-deriving the same arithmetic.
    pub caret_screen: Option<(u16, u16)>,
    /// The suggestion box's inner rect and the index of its first visible
    /// entry, recorded while it is drawn so a click can land on a suggestion
    /// instead of the result grid underneath.
    pub autocomplete_hit: Option<(Rect, usize)>,
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
    /// How long the in-flight query has been running, refreshed each tick so
    /// the result pane can show a live counter.
    pub exec_elapsed: std::time::Duration,
    /// Editor snapshots for undo, oldest first. Whole-buffer copies: a query
    /// console holds a screenful of text, so the simplicity is worth more
    /// than the bytes.
    undo_stack: Vec<EditSnapshot>,
    /// Snapshots undone and available to redo, most recent last. Cleared by
    /// any new edit — the usual linear-history rule.
    redo_stack: Vec<EditSnapshot>,
    /// What the last edit was, so a run of typed characters collapses into a
    /// single undo step instead of one per keystroke.
    last_edit: EditKind,
}

/// A point-in-time copy of the editor: text plus caret.
#[derive(Clone, Debug, PartialEq, Eq)]
struct EditSnapshot {
    lines: Vec<String>,
    cursor_row: usize,
    cursor_col: usize,
}

/// Edit categories for undo coalescing. Consecutive `Typing` (or
/// `Deleting`) edits share one snapshot; anything else starts a new step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditKind {
    /// No edit yet, or the last one closed the current step.
    None,
    Typing,
    Deleting,
}

/// Is `c` part of a word, for word-wise motion? Identifiers keep `_`, so
/// `user_id` is one word rather than three.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Cap on the undo history — deep enough for a session's worth of editing,
/// bounded so a long-lived tab cannot grow without limit.
const UNDO_DEPTH: usize = 200;

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
            editor_hit_area: None,
            row_detail: false,
            row_detail_scroll: 0,
            search_query: String::new(),
            search_editing: false,
            search_buffer: String::new(),
            caret_screen: None,
            autocomplete_hit: None,
            result_col_starts: Vec::new(),
            watch_interval: None,
            last_run: None,
            editor_scroll: 0,
            editor_scroll_x: 0,
            exec_elapsed: std::time::Duration::ZERO,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            last_edit: EditKind::None,
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
        self.checkpoint(EditKind::None);
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
        // Replacing the whole buffer (format, load from history or favorites)
        // is exactly the edit a user most wants back.
        self.checkpoint(EditKind::None);
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

    /// Snapshot the editor before an edit of `kind`.
    ///
    /// Consecutive edits of the same kind share one snapshot, so a typed word
    /// is one undo step, not one per letter. Any edit invalidates the redo
    /// history — the usual linear-undo rule.
    fn checkpoint(&mut self, kind: EditKind) {
        self.redo_stack.clear();
        if kind != EditKind::None && self.last_edit == kind {
            return;
        }
        self.last_edit = kind;
        self.undo_stack.push(self.snapshot());
        if self.undo_stack.len() > UNDO_DEPTH {
            self.undo_stack.remove(0);
        }
    }

    fn snapshot(&self) -> EditSnapshot {
        EditSnapshot {
            lines: self.lines.clone(),
            cursor_row: self.cursor_row,
            cursor_col: self.cursor_col,
        }
    }

    fn restore(&mut self, snap: EditSnapshot) {
        self.lines = snap.lines;
        self.cursor_row = self.cursor_row.min(self.lines.len().saturating_sub(1));
        self.cursor_row = snap.cursor_row.min(self.lines.len().saturating_sub(1));
        self.cursor_col = snap
            .cursor_col
            .min(self.lines.get(self.cursor_row).map_or(0, |l| l.chars().count()));
        self.autocomplete.clear();
        self.autocomplete_selected = 0;
        // The next edit opens a fresh step: undoing then typing must not fold
        // into whatever run was interrupted.
        self.last_edit = EditKind::None;
    }

    /// Step back one edit. Returns false when there is nothing to undo.
    pub fn undo(&mut self) -> bool {
        let Some(prev) = self.undo_stack.pop() else {
            return false;
        };
        let current = self.snapshot();
        self.restore(prev);
        self.redo_stack.push(current);
        true
    }

    /// Step forward again after an undo. Returns false when there is nothing
    /// to redo.
    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo_stack.pop() else {
            return false;
        };
        let current = self.snapshot();
        self.restore(next);
        self.undo_stack.push(current);
        true
    }

    pub fn insert_char(&mut self, c: char) {
        self.checkpoint(EditKind::Typing);
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
        // A line break closes the typing run: undo should step back to the
        // start of this line, not swallow the previous one too.
        self.checkpoint(EditKind::None);
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
        self.checkpoint(EditKind::Deleting);
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
        self.checkpoint(EditKind::Deleting);
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

    /// Byte-agnostic word scan: the column where the word before the caret
    /// starts. Whitespace before the caret is skipped first, so pressing it
    /// twice at the start of a word moves over the previous one.
    fn word_start(&self) -> usize {
        let Some(line) = self.lines.get(self.cursor_row) else {
            return 0;
        };
        let chars: Vec<char> = line.chars().collect();
        let mut i = self.cursor_col.min(chars.len());
        while i > 0 && !is_word_char(chars[i - 1]) {
            i -= 1;
        }
        while i > 0 && is_word_char(chars[i - 1]) {
            i -= 1;
        }
        i
    }

    /// The column just past the word after the caret.
    fn word_end(&self) -> usize {
        let Some(line) = self.lines.get(self.cursor_row) else {
            return 0;
        };
        let chars: Vec<char> = line.chars().collect();
        let mut i = self.cursor_col.min(chars.len());
        while i < chars.len() && !is_word_char(chars[i]) {
            i += 1;
        }
        while i < chars.len() && is_word_char(chars[i]) {
            i += 1;
        }
        i
    }

    /// Ctrl+Left: jump to the start of the previous word, crossing to the end
    /// of the line above when already at column 0.
    pub fn move_word_left(&mut self) {
        if self.cursor_col == 0 {
            self.move_cursor_left();
            return;
        }
        self.cursor_col = self.word_start();
    }

    /// Ctrl+Right: jump past the end of the next word.
    pub fn move_word_right(&mut self) {
        let len = self
            .lines
            .get(self.cursor_row)
            .map_or(0, |l| l.chars().count());
        if self.cursor_col >= len {
            self.move_cursor_right();
            return;
        }
        self.cursor_col = self.word_end();
    }

    /// Alt+Backspace / Ctrl+W: delete the word before the caret. At the start
    /// of a line it falls back to joining with the line above.
    pub fn delete_word_left(&mut self) {
        if self.cursor_col == 0 {
            self.backspace();
            return;
        }
        self.checkpoint(EditKind::None);
        let start = self.word_start();
        let line = &mut self.lines[self.cursor_row];
        let chars: Vec<char> = line.chars().collect();
        let head: String = chars[..start].iter().collect();
        let tail: String = chars[self.cursor_col.min(chars.len())..].iter().collect();
        *line = format!("{head}{tail}");
        self.cursor_col = start;
    }

    /// Toggle `-- ` on every line of the buffer touched by the caret. Adds
    /// the marker when the line is not commented, removes it when it is.
    pub fn toggle_comment(&mut self) {
        self.checkpoint(EditKind::None);
        let Some(line) = self.lines.get_mut(self.cursor_row) else {
            return;
        };
        let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
        let rest = &line[indent.len()..];
        if let Some(uncommented) = rest.strip_prefix("-- ") {
            *line = format!("{indent}{uncommented}");
            self.cursor_col = self.cursor_col.saturating_sub(3);
        } else if let Some(uncommented) = rest.strip_prefix("--") {
            *line = format!("{indent}{uncommented}");
            self.cursor_col = self.cursor_col.saturating_sub(2);
        } else {
            *line = format!("{indent}-- {rest}");
            self.cursor_col += 3;
        }
    }

    /// Copy the caret's line below itself, caret following the copy.
    pub fn duplicate_line(&mut self) {
        self.checkpoint(EditKind::None);
        let Some(line) = self.lines.get(self.cursor_row).cloned() else {
            return;
        };
        self.lines.insert(self.cursor_row + 1, line);
        self.cursor_row += 1;
    }

    /// Delete the caret's line. The buffer always keeps at least one line.
    pub fn delete_line(&mut self) {
        self.checkpoint(EditKind::None);
        if self.lines.len() <= 1 {
            self.lines = vec![String::new()];
            self.cursor_row = 0;
            self.cursor_col = 0;
            return;
        }
        self.lines.remove(self.cursor_row);
        self.cursor_row = self.cursor_row.min(self.lines.len() - 1);
        self.cursor_col = self
            .cursor_col
            .min(self.lines[self.cursor_row].chars().count());
    }

    /// Insert pasted text at the caret, newlines included — one edit, one
    /// undo step, and no autocomplete storm from a per-character replay.
    pub fn insert_text(&mut self, text: &str) {
        self.checkpoint(EditKind::None);
        for (i, part) in text.replace("\r\n", "\n").split('\n').enumerate() {
            if i > 0 {
                // `insert_newline` would checkpoint again; splice directly.
                let line = self.lines[self.cursor_row].clone();
                let chars: Vec<char> = line.chars().collect();
                let head: String = chars[..self.cursor_col.min(chars.len())].iter().collect();
                let tail: String = chars[self.cursor_col.min(chars.len())..].iter().collect();
                self.lines[self.cursor_row] = head;
                self.lines.insert(self.cursor_row + 1, tail);
                self.cursor_row += 1;
                self.cursor_col = 0;
            }
            if part.is_empty() {
                continue;
            }
            let line = &mut self.lines[self.cursor_row];
            let chars: Vec<char> = line.chars().collect();
            let at = self.cursor_col.min(chars.len());
            let head: String = chars[..at].iter().collect();
            let tail: String = chars[at..].iter().collect();
            *line = format!("{head}{part}{tail}");
            self.cursor_col = at + part.chars().count();
        }
        self.last_edit = EditKind::None;
    }

    /// Move the caret one screenful up / down, staying inside the buffer.
    pub fn move_page(&mut self, rows: usize, down: bool) {
        let last = self.lines.len().saturating_sub(1);
        self.cursor_row = if down {
            (self.cursor_row + rows).min(last)
        } else {
            self.cursor_row.saturating_sub(rows)
        };
        self.cursor_col = self
            .cursor_col
            .min(self.lines[self.cursor_row].chars().count());
    }

    /// Jump to the very start / end of the buffer (Ctrl+Home / Ctrl+End).
    pub fn move_to_buffer_edge(&mut self, end: bool) {
        if end {
            self.cursor_row = self.lines.len().saturating_sub(1);
            self.cursor_col = self.lines[self.cursor_row].chars().count();
        } else {
            self.cursor_row = 0;
            self.cursor_col = 0;
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

/// Tier-1 autocomplete for the text before the cursor:
/// - after FROM/JOIN/INTO/UPDATE → table names
/// - `table.` prefix → column names (from `column_cache` keyed `ns.table`)
/// - otherwise → SQL keywords
pub fn suggest(
    line_before_cursor: &str,
    tables: &[String],
    column_cache: &std::collections::HashMap<String, Vec<String>>,
) -> Vec<String> {
    suggest_inner(line_before_cursor, tables, column_cache, MIN_KEYWORD_PREFIX)
}

/// Suggestions for an explicit request (Ctrl+Space): the two-character
/// threshold drops to one, so a single letter is answered.
///
/// It does NOT drop to zero. With no token at all, `starts_with("")` matches
/// every keyword and the cap keeps the first twenty in declaration order —
/// `select`, `from`, `where`… — which can never offer the `AND` / `OR` /
/// `LIKE` a bare `WHERE ` actually wants. Table and column context (after
/// `FROM`, after `table.`) is handled before this point and still answers on
/// an empty prefix.
pub fn suggest_forced(
    line_before_cursor: &str,
    tables: &[String],
    column_cache: &std::collections::HashMap<String, Vec<String>>,
) -> Vec<String> {
    suggest_inner(line_before_cursor, tables, column_cache, 1)
}

fn suggest_inner(
    line_before_cursor: &str,
    tables: &[String],
    column_cache: &std::collections::HashMap<String, Vec<String>>,
    min_keyword_prefix: usize,
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
        return drop_noop(t, &current);
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
        return drop_noop(cols, col_prefix);
    }

    // Otherwise keywords — but only past a prefix long enough to mean
    // something. One letter matched up to eight keywords, so the popup was
    // open almost permanently while typing.
    if current.chars().count() < min_keyword_prefix {
        return Vec::new();
    }
    let upper = current.to_uppercase();
    // Follow the case the user is typing in: completing `select` into
    // `SELECT` silently rewrites their style.
    let shout = current.chars().any(|c| c.is_uppercase());
    let hits: Vec<String> = SQL_KEYWORDS
        .iter()
        .filter(|k| k.starts_with(&upper))
        .map(|k| {
            if shout {
                k.to_string()
            } else {
                k.to_lowercase()
            }
        })
        .take(20)
        .collect();
    drop_noop(hits, &current)
}

/// Drop a suggestion list that only offers what is already typed — otherwise
/// the popup sits open on every finished word (`SELECT` suggests `SELECT`).
fn drop_noop(hits: Vec<String>, current: &str) -> Vec<String> {
    if hits.len() == 1 && hits[0].eq_ignore_ascii_case(current) {
        return Vec::new();
    }
    hits
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
                        Style::default().fg(theme.syntax_number),
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
                // Backticked identifiers read as names, plain quotes as
                // string values — different meanings, different tokens.
                let style = if quote_char == '`' {
                    Style::default().fg(theme.syntax_ident)
                } else {
                    Style::default().fg(theme.syntax_string)
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
                Style::default().fg(theme.syntax_number),
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

    if console.row_detail
        && let Some(res) = &console.last_result
        && let Some(record) = res.records.get(console.result_selected_row)
    {
        crate::ui::screens::explorer::render_record_detail(
            f,
            area,
            &format!(
                " query result — row {}/{} ",
                console.result_selected_row + 1,
                res.records.len()
            ),
            &res.columns,
            record,
            console.result_selected_col,
            console.row_detail_scroll,
            // A query result has no key metadata to mark.
            &|_| "  ",
            theme,
        );
    } else if let Some(popup) = &console.popup {
        render_console_popup(f, area, popup, theme);
    } else if is_tab_focused
        && console.focused_subpane == ConsoleSubpane::Editor
        && !console.autocomplete.is_empty()
        && let Some((caret_x, _)) = console.caret_screen
    {
        render_autocomplete(f, area, chunks[0], caret_x, console, theme);
    } else {
        console.autocomplete_hit = None;
    }
}

/// Where the suggestion box goes: directly under the **editor pane**,
/// horizontally aligned with the caret.
///
/// "Under the caret" is not enough — the editor is six rows at its smallest,
/// so a box one row below the caret still buries the rest of the query. This
/// places it in the result pane's space instead, where it hides results (a
/// transient loss) rather than the text being written.
///
/// `None` when there is not enough room below the editor to draw a box.
///
/// Split out of the renderer so the placement rules are testable.
fn autocomplete_rect(
    area: Rect,
    editor: Rect,
    caret_x: u16,
    items: u16,
    widest: u16,
) -> Option<Rect> {
    let below = editor.y.saturating_add(editor.height);
    let room = (area.y + area.height).saturating_sub(below);
    let height = (items + 2).min(AC_POPUP_H).min(room);
    if height < 3 {
        return None;
    }
    // Width follows the longest suggestion, not the pane: a list of short
    // keywords should not draw a box across the whole screen.
    let title_w = UnicodeWidthStr::width(AC_TITLE) as u16 + 2;
    let width = (widest + AC_CHROME_W).max(title_w).min(area.width);
    let x = caret_x
        .max(area.x)
        .min((area.x + area.width).saturating_sub(width));
    Some(Rect {
        x,
        y: below,
        width,
        height,
    })
}

/// Every (row, column) whose cell matches the console's search, in reading
/// order. Empty when no search is active.
pub fn result_search_matches(console: &QueryConsole) -> Vec<(usize, usize)> {
    if console.search_query.is_empty() {
        return Vec::new();
    }
    let Some(res) = &console.last_result else {
        return Vec::new();
    };
    res.records
        .iter()
        .enumerate()
        .flat_map(|(r, rec)| {
            rec.values
                .iter()
                .enumerate()
                .filter(|(_, v)| {
                    crate::ui::screens::explorer::cell_matches_search(v, &console.search_query)
                })
                .map(move |(c, _)| (r, c))
        })
        .collect()
}

/// Suggestion list, floating just under the editor pane. It overlaps the
/// result grid on purpose — a transient popup over results is far cheaper
/// than one over the query being written.
fn render_autocomplete(
    f: &mut Frame,
    area: Rect,
    editor: Rect,
    caret_x: u16,
    console: &mut QueryConsole,
    theme: &Theme,
) {
    let widest = console
        .autocomplete
        .iter()
        .map(|s| UnicodeWidthStr::width(s.as_str()))
        .max()
        .unwrap_or(0) as u16;
    let Some(popup_area) = autocomplete_rect(
        area,
        editor,
        caret_x,
        console.autocomplete.len() as u16,
        widest,
    ) else {
        console.autocomplete_hit = None;
        return;
    };
    let inner = crate::ui::widgets::popup::render_frame_in(f, popup_area, Some(AC_TITLE), theme);

    let visible = inner.height as usize;
    let sel = console
        .autocomplete_selected
        .min(console.autocomplete.len().saturating_sub(1));
    // Keep the highlighted entry in view when the list is longer than the box.
    let start = sel.saturating_sub(visible.saturating_sub(1));
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
    console.autocomplete_hit = Some((inner, start));
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

    console.editor_hit_area = Some(inner);
    // Record where the caret actually landed, for the suggestion box to
    // anchor to. The `-1` mirrors the block cursor drawn above, which sits on
    // the character LEFT of the insertion point.
    console.caret_screen = is_editor_focused.then(|| {
        let col = inner.x
            + GUTTER_W as u16
            + (console.cursor_col.saturating_sub(console.editor_scroll_x)) as u16;
        let row = inner.y + (console.cursor_row.saturating_sub(console.editor_scroll)) as u16;
        (col.saturating_sub(1).max(inner.x), row.min(inner.bottom().saturating_sub(1)))
    });
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
        // A live counter (and the spinner below) is the difference between
        // "working" and "hung" for a query that takes a while.
        format!(
            " Query Result — running {:.1}s (Esc to cancel) ",
            console.exec_elapsed.as_secs_f64()
        )
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
        let search = if console.search_editing {
            format!(" [find: {}_]", console.search_buffer)
        } else if console.search_query.is_empty() {
            String::new()
        } else {
            format!(
                " [find \"{}\": {} hits, Ctrl+G next]",
                console.search_query,
                result_search_matches(console).len()
            )
        };
        format!(
            " Query Result{multi}{watch}{search} ({} rows affected, {:.2?}) ",
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

    // While running, the pane itself says so — an animated frame plus the
    // elapsed time, so a slow query never reads as a hang.
    if console.is_executing {
        let inner = block.inner(area);
        f.render_widget(block, area);
        const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let frame = FRAMES[(console.exec_elapsed.as_millis() / 80) as usize % FRAMES.len()];
        let body = vec![
            Line::from(Span::styled(
                format!("{frame} Running query... {:.1}s", console.exec_elapsed.as_secs_f64()),
                theme.accent(),
            )),
            Line::from(Span::styled("Esc to cancel", theme.dim())),
        ];
        f.render_widget(
            Paragraph::new(body).alignment(Alignment::Center),
            inner,
        );
        return;
    }

    if let Some(err) = &console.execution_error {
        let inner = block.inner(area);
        f.render_widget(block, area);

        let err_lines = vec![
            Line::from(Span::styled("Execution failed with error:", theme.error())),
            Line::from(Span::styled(err, theme.base())),
        ];
        // Wrapped: a server's error text routinely runs past the pane width,
        // and the useful half ("syntax error at or near …") is at the end.
        let p = Paragraph::new(err_lines)
            .style(theme.base())
            .wrap(Wrap { trim: false });
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

            let search_query = console.search_query.clone();
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
                        // A search hit is marked even when it is not the
                        // cursor, so the eye finds the others.
                        let is_hit = !search_query.is_empty()
                            && crate::ui::screens::explorer::cell_matches_search(val, &search_query);
                        let cell_style = if is_cell_sel {
                            sel.add_modifier(Modifier::BOLD)
                        } else if is_row_sel {
                            sel
                        } else if is_hit {
                            theme.accent().add_modifier(Modifier::BOLD)
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

        // Otherwise keywords — matching the case the user is typing in.
        let s = suggest("SEL", &tables, &cache);
        assert!(s.contains(&"SELECT".to_string()));
        let s = suggest("sel", &tables, &cache);
        assert!(s.contains(&"select".to_string()), "got {s:?}");
        assert!(!s.contains(&"SELECT".to_string()), "case was rewritten");

        // Trailing space after FROM → suggest all tables.
        let s = suggest("SELECT * FROM ", &tables, &cache);
        assert!(s.contains(&"users".to_string()));
        assert!(s.contains(&"orders".to_string()));

        // Keyword context with empty prefix → no keyword noise.
        assert!(suggest("SELECT ", &tables, &cache).is_empty());
    }

    #[test]
    fn test_suggest_stays_quiet_until_it_has_something_to_say() {
        let cache = std::collections::HashMap::new();
        let tables = vec!["users".to_string(), "orders".to_string()];

        // One typed character matched up to eight keywords; the popup was
        // open from the first keystroke of nearly every word.
        assert!(suggest("s", &tables, &cache).is_empty());
        assert!(suggest("SELECT * FROM users WHERE a", &tables, &cache).is_empty());
        assert!(!suggest("se", &tables, &cache).is_empty());

        // A finished word suggests only itself — that is not a suggestion.
        assert!(suggest("select", &tables, &cache).is_empty());
        assert!(suggest("SELECT", &tables, &cache).is_empty());
        // …but a word that is still a prefix of others keeps suggesting.
        assert!(!suggest("in", &tables, &cache).is_empty());

        // Same rule for tables and columns.
        assert!(suggest("SELECT * FROM users", &tables, &cache).is_empty());
        assert!(!suggest("SELECT * FROM o", &tables, &cache).is_empty());
    }

    #[test]
    fn test_ctrl_space_lowers_the_threshold_but_still_needs_a_word() {
        let cache = std::collections::HashMap::new();
        let tables = vec!["users".to_string()];
        // Typing `s` stays quiet, but asking explicitly answers.
        assert!(suggest("s", &tables, &cache).is_empty());
        assert!(!suggest_forced("s", &tables, &cache).is_empty());

        // With no word at all, a keyword dump would be the first twenty
        // entries in declaration order — never the AND/OR/LIKE that a bare
        // `WHERE ` wants. Say nothing instead.
        assert!(suggest_forced("SELECT * FROM users WHERE ", &tables, &cache).is_empty());

        // Table context is real context, so it still answers on an empty word.
        assert_eq!(
            suggest_forced("SELECT * FROM ", &tables, &cache),
            vec!["users".to_string()]
        );
    }    #[test]
    fn test_autocomplete_box_never_covers_editor_text() {
        // Console pane rows 0..24, editor occupying its first 6 (the minimum,
        // i.e. the case the old placement got wrong).
        let area = Rect::new(0, 0, 80, 24);
        let editor = Rect::new(0, 0, 80, 6);

        let r = autocomplete_rect(area, editor, 10, 4, 6).expect("fits");
        assert!(
            r.y >= editor.y + editor.height,
            "box overlaps the editor: {r:?}"
        );
        assert_eq!(r.x, 10, "box aligns with the caret column");

        // True wherever the caret sits, including the editor's last line —
        // "below the caret" used to bury the rest of a short query.
        for caret_row in 1..6 {
            let r = autocomplete_rect(area, editor, 0, 4, 6).expect("fits");
            assert!(r.y > caret_row, "caret row {caret_row} covered by {r:?}");
        }

        // Width follows the content, not the pane.
        let r = autocomplete_rect(area, editor, 0, 2, 6).expect("fits");
        assert!(r.width < area.width, "box spans the pane: {r:?}");

        // A caret at the right edge pulls the box back inside the pane.
        let r = autocomplete_rect(area, editor, 79, 4, 20).expect("fits");
        assert!(r.x + r.width <= 80, "box runs off the right edge: {r:?}");

        // No room under the editor → no box, rather than one drawn over the
        // query or off the buffer.
        assert!(autocomplete_rect(Rect::new(0, 0, 80, 8), Rect::new(0, 0, 80, 6), 0, 4, 6).is_none());
    }

    #[test]
    fn test_autocomplete_box_height_follows_the_list() {
        let area = Rect::new(0, 0, 80, 24);
        let editor = Rect::new(0, 0, 80, 6);
        // One suggestion → borders + one row.
        assert_eq!(autocomplete_rect(area, editor, 0, 1, 6).unwrap().height, 3);
        // A long list is capped instead of filling the pane.
        assert_eq!(
            autocomplete_rect(area, editor, 0, 40, 6).unwrap().height,
            AC_POPUP_H
        );
    }

    #[test]
    fn test_autocomplete_box_fits_wide_characters() {
        // Terminal columns, not `chars().count()`: a CJK name is twice as
        // wide as it is long and used to be clipped by the right border.
        let area = Rect::new(0, 0, 80, 24);
        let editor = Rect::new(0, 0, 80, 6);
        let name = "顧客マスタ";
        let w = UnicodeWidthStr::width(name) as u16;
        assert_eq!(w, 10, "fixture assumption");
        let r = autocomplete_rect(area, editor, 0, 1, w).expect("fits");
        assert!(
            r.width >= w + 4,
            "box too narrow for a wide-character entry: {r:?}"
        );
    }
    #[test]
    fn test_undo_groups_a_typed_word_into_one_step() {
        let mut c = QueryConsole::new("t".to_string(), Some(""));
        for ch in "SELECT".chars() {
            c.insert_char(ch);
        }
        assert_eq!(c.text(), "SELECT");
        // One step back clears the whole run, not one letter.
        assert!(c.undo());
        assert_eq!(c.text(), "");
        assert!(!c.undo(), "nothing left to undo");
    }

    #[test]
    fn test_undo_separates_typing_from_deleting() {
        let mut c = QueryConsole::new("t".to_string(), Some(""));
        for ch in "abc".chars() {
            c.insert_char(ch);
        }
        c.backspace();
        c.backspace();
        assert_eq!(c.text(), "a");
        assert!(c.undo());
        assert_eq!(c.text(), "abc", "the deletions are one step");
        assert!(c.undo());
        assert_eq!(c.text(), "", "the typing is another");
    }

    #[test]
    fn test_undo_restores_a_buffer_replaced_wholesale() {
        // set_text is how formatting and loading a saved query work — the
        // edit a user most often wants back.
        let mut c = QueryConsole::new("t".to_string(), Some(""));
        for ch in "select 1".chars() {
            c.insert_char(ch);
        }
        c.set_text("SELECT\n  1".to_string());
        assert_eq!(c.text(), "SELECT\n  1");
        assert!(c.undo());
        assert_eq!(c.text(), "select 1");
    }

    #[test]
    fn test_redo_replays_and_is_dropped_by_a_new_edit() {
        let mut c = QueryConsole::new("t".to_string(), Some(""));
        for ch in "ab".chars() {
            c.insert_char(ch);
        }
        assert!(c.undo());
        assert_eq!(c.text(), "");
        assert!(c.redo());
        assert_eq!(c.text(), "ab");
        assert!(!c.redo(), "nothing left to redo");

        // Undo, then type: the redo branch is gone (linear history).
        assert!(c.undo());
        c.insert_char('x');
        assert!(!c.redo());
        assert_eq!(c.text(), "x");
    }

    #[test]
    fn test_undo_restores_the_caret_and_survives_a_shorter_buffer() {
        let mut c = QueryConsole::new("t".to_string(), Some(""));
        for ch in "abc".chars() {
            c.insert_char(ch);
        }
        c.insert_newline();
        for ch in "defgh".chars() {
            c.insert_char(ch);
        }
        assert_eq!((c.cursor_row, c.cursor_col), (1, 5));
        assert!(c.undo());
        // Caret must land somewhere valid for the restored text.
        assert!(c.cursor_row < c.lines.len());
        assert!(c.cursor_col <= c.lines[c.cursor_row].chars().count());
    }

    #[test]
    fn test_undo_history_is_bounded() {
        let mut c = QueryConsole::new("t".to_string(), Some(""));
        // Each set_text is its own step; go well past the cap.
        for i in 0..(UNDO_DEPTH + 50) {
            c.set_text(format!("q{i}"));
        }
        assert!(c.undo_stack.len() <= UNDO_DEPTH, "history grew unbounded");
    }
    fn console(text: &str) -> QueryConsole {
        QueryConsole::new("t".to_string(), Some(text))
    }

    #[test]
    fn test_word_motion_treats_identifiers_as_one_word() {
        let mut c = console("SELECT user_id FROM t");
        c.cursor_row = 0;
        c.cursor_col = 21;
        c.move_word_left();
        assert_eq!(c.cursor_col, 20, "start of `t`");
        c.move_word_left();
        assert_eq!(&"SELECT user_id FROM t"[c.cursor_col..c.cursor_col + 4], "FROM");
        c.move_word_left();
        assert_eq!(
            &"SELECT user_id FROM t"[c.cursor_col..c.cursor_col + 7],
            "user_id",
            "`user_id` is one word, not three"
        );
        c.move_word_right();
        assert_eq!(c.cursor_col, 14, "just past `user_id`");
    }

    #[test]
    fn test_delete_word_left_removes_one_identifier() {
        let mut c = console("SELECT user_id");
        c.cursor_row = 0;
        c.cursor_col = 14;
        c.delete_word_left();
        assert_eq!(c.text(), "SELECT ");
        // And it is a single undo step.
        assert!(c.undo());
        assert_eq!(c.text(), "SELECT user_id");
    }

    #[test]
    fn test_toggle_comment_round_trips_and_keeps_indent() {
        let mut c = console("  SELECT 1");
        c.cursor_row = 0;
        c.cursor_col = 4;
        c.toggle_comment();
        assert_eq!(c.text(), "  -- SELECT 1");
        c.toggle_comment();
        assert_eq!(c.text(), "  SELECT 1");
        // A marker without the space is removed too.
        let mut c = console("--SELECT 1");
        c.toggle_comment();
        assert_eq!(c.text(), "SELECT 1");
    }

    #[test]
    fn test_duplicate_and_delete_line() {
        let mut c = console("a\nb");
        c.cursor_row = 0;
        c.cursor_col = 1;
        c.duplicate_line();
        assert_eq!(c.text(), "a\na\nb");
        assert_eq!(c.cursor_row, 1, "caret follows the copy");
        c.delete_line();
        assert_eq!(c.text(), "a\nb");

        // The buffer never becomes line-less.
        let mut c = console("only");
        c.delete_line();
        assert_eq!(c.lines, vec![String::new()]);
    }

    #[test]
    fn test_insert_text_pastes_multiple_lines_as_one_edit() {
        let mut c = console("SELECT ");
        c.cursor_row = 0;
        c.cursor_col = 7;
        c.insert_text("1,\n  2\n");
        assert_eq!(c.text(), "SELECT 1,\n  2\n");
        assert_eq!(c.cursor_row, 2);
        // One undo takes the whole paste back.
        assert!(c.undo());
        assert_eq!(c.text(), "SELECT ");

        // CRLF from a Windows clipboard does not leave stray carriage returns.
        let mut c = console("");
        c.insert_text("a\r\nb");
        assert_eq!(c.text(), "a\nb");
    }

    #[test]
    fn test_paging_and_buffer_edges_stay_in_bounds() {
        let mut c = console("1\n2\n3\n4\n5");
        // `new` parks the caret at the end of the buffer.
        c.move_to_buffer_edge(false);
        c.move_page(2, true);
        assert_eq!(c.cursor_row, 2);
        c.move_page(99, true);
        assert_eq!(c.cursor_row, 4, "clamped to the last line");
        c.move_page(99, false);
        assert_eq!(c.cursor_row, 0);
        c.move_to_buffer_edge(true);
        assert_eq!((c.cursor_row, c.cursor_col), (4, 1));
        c.move_to_buffer_edge(false);
        assert_eq!((c.cursor_row, c.cursor_col), (0, 0));
    }
}
