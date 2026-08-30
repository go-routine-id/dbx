//! App runtime: event loop, tick-driven animation, terminal lifecycle.

use std::io::{self};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::config::AppConfig;
use crate::driver::{Driver, Page};
use crate::clipboard::ClipboardManager;
use crate::export::{ExportFormat, Exporter};
use crate::theme::Theme;
use crate::ui::layout::{self, MIN_HEIGHT, MIN_WIDTH};
use crate::ui::screens::explorer::{
    self, ExportModalState, ExplorerState, FocusedPane, TreeNodeKind, WorkspaceTab,
};
use crate::ui::screens::picker::{self, ConfirmDeleteModal, ConnectionForm, FormField};
use crate::ui::screens::query::{ConsoleSubpane, QueryConsole};
use crate::ui::widgets::{help, spinner::Spinner, statusbar, toast::ToastKind, toast::Toasts};

/// How long the event poll waits before a tick fires (spinner/toast cadence).
const TICK_CAP: Duration = Duration::from_millis(60);

/// Sentinel string stored in `CellEditModalState::text_buffer` when the user
/// chose to set the cell to NULL via `Ctrl+N`. The SQL builder detects this
/// sentinel and emits `SET col = NULL` (no quotes) instead of `SET col = '...'`.
/// Kept internal — never shown to the user as-is; the modal renders it as
/// the bold-italic word "NULL".
pub const NULL_SENTINEL: &str = "__DBX_NULL__";

/// Helper: extract the printable character from a `KeyCode::Char(_)` event,
/// or `None` if it's anything else (function keys, arrows, etc.). Centralizes
/// the `Char` → `char` mapping for the cell-edit modal's text input.
fn extract_char_payload(code: KeyCode, is_ctrl: bool) -> Option<char> {
    if is_ctrl {
        return None;
    }
    match code {
        KeyCode::Char(c) => Some(c),
        _ => None,
    }
}

/// Phase-1 plan for cell-edit modal key handling. Captures everything the
/// handler needs from immutable borrows (`exp.cell_edit_modal` as ref +
/// `exp.active_tab()` as ref) so phase 2 can take a mutable borrow of
/// `exp.cell_edit_modal` without conflicting.
struct EditKeyPlan {
    is_ctrl: bool,
    key: KeyCode,
    char_payload: Option<char>,
    is_sentinel: bool,
    is_nullable: bool,
    original_value: Option<String>,
}

/// Phase-1 plan for the INSERT-row modal. Mirrors `EditKeyPlan` for the
/// multi-field case. `is_skip` is true when the focused field is in the
/// "skip / use DEFAULT" state (buffer = None), distinct from `is_sentinel`
/// which is the NULL state.
struct InsertKeyPlan {
    is_ctrl: bool,
    key: KeyCode,
    char_payload: Option<char>,
    is_sentinel: bool,
    is_nullable: bool,
    is_skip: bool,
    focused: usize,
    n: usize,
}

// ---------------------------------------------------------------------------
// Generic SQL building helpers (multi-driver safe)
// ---------------------------------------------------------------------------
//
// These are intentionally **driver-agnostic** in their inputs: they take raw
// strings and build SQL that works against any backend. The only driver
// specifics that leak through are:
//   1. Identifier quoting style (PG uses `"..."`, MySQL uses `` `...` ``).
//   2. The `LIMIT 1` suffix that MySQL requires on a `DELETE` statement
//      without an implied unique key (PostgreSQL rejects it).
//   3. NULL literal rendering — same `NULL` keyword on every SQL backend,
//      no quoting.
//
// All other concerns (string value escaping, NULL sentinel, skip columns,
// dialect-agnostic UPDATE/INSERT syntax) are uniform. The driver crate
// remains the source of truth for the connection dialect — we sniff
// `DriverInfo::name` here purely for the two style differences above.

/// Quoting style for a SQL dialect. Currently we only need to distinguish
/// PostgreSQL (double-quote identifiers) from MySQL/SQLite (backtick) and
/// SQL Server (square brackets). When SQL Server / SQLite land, extend the
/// match in `quote_ident_for` — everything downstream stays the same.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QuoteStyle {
    /// PostgreSQL: `"identifier"` (case-sensitive).
    Double,
    /// MySQL / MariaDB: `` `identifier` `` (case-insensitive on default
    /// lower_case_table_names). Also what SQLite accepts for compatibility.
    Backtick,
    /// SQL Server: `[identifier]` or `"identifier"` (the latter is
    /// non-standard and conflicts with string literals in some contexts).
    Bracket,
}

/// Map a driver-info name to its identifier quoting style. Defaults to
/// double-quote (PG) for any unknown driver — safest for a SQL-syntax
/// reference, and the connection itself will reject the SQL if the dialect
/// is wildly different.
fn quote_style_for(driver_name: &str) -> QuoteStyle {
    let lower = driver_name.to_lowercase();
    if lower.contains("postgres") || lower.contains("pg") {
        QuoteStyle::Double
    } else if lower.contains("mysql") || lower.contains("maria") {
        QuoteStyle::Backtick
    } else if lower.contains("sql server") || lower.contains("mssql") {
        QuoteStyle::Bracket
    } else if lower.contains("sqlite") {
        // SQLite accepts both. Backtick is the most ergonomic for
        // round-tripped queries.
        QuoteStyle::Backtick
    } else {
        QuoteStyle::Double
    }
}

/// Quote a SQL identifier (table / column / schema name) according to the
/// given style. Doubles up the inner quote character so the identifier
/// itself can contain the quote without breaking out — same defensive
/// pattern that PostgreSQL/MySQL driver crates already apply internally.
fn quote_ident_with(ident: &str, style: QuoteStyle) -> String {
    match style {
        QuoteStyle::Double => format!("\"{}\"", ident.replace('"', "\"\"")),
        QuoteStyle::Backtick => format!("`{}`", ident.replace('`', "``")),
        QuoteStyle::Bracket => format!("[{}]", ident.replace(']', "]]")),
    }
}

/// Convenience wrapper that pulls the style from a `DriverInfo`-style name.
/// Kept as a single entry point so call sites never have to think about the
/// underlying style enum.
fn quote_ident(ident: &str, driver_name: &str) -> String {
    quote_ident_with(ident, quote_style_for(driver_name))
}

/// MySQL requires `LIMIT 1` at the end of a single-row DELETE (and UPDATE)
/// when the WHERE clause doesn't target a unique key. PostgreSQL rejects
/// the `LIMIT` clause in DELETE/UPDATE. Other dialects vary. This returns
/// the dialect-appropriate suffix to append after the WHERE clause.
fn single_row_suffix(driver_name: &str) -> &'static str {
    let lower = driver_name.to_lowercase();
    if lower.contains("mysql") || lower.contains("maria") {
        " LIMIT 1"
    } else {
        // PostgreSQL, SQL Server, SQLite: the WHERE clause is enough.
        ""
    }
}

/// Escape a user-supplied value for safe inclusion as a SQL string literal.
/// Doubles single quotes (the standard SQL escape). `None` (skip) is
/// handled by callers before reaching here. The empty string becomes
/// `''` which is a valid empty literal on every backend.
fn escape_string_literal(s: &str) -> String {
    s.replace('\'', "''")
}

/// Render a single `Value` as the right-hand side of `col = ...` or as a
/// WHERE-clause comparison value. NULL is the keyword `NULL` (never
/// quoted, never escaped — this is the standard SQL semantics shared by
/// every backend). All other values are emitted as quoted string
/// literals; the server will coerce them to the column's actual type.
/// This matches what every DB GUI does — it's the only way to stay
/// driver-agnostic without dragging in type-aware formatting for INT,
/// BOOL, TIMESTAMP, NUMERIC, etc. that differs subtly per dialect.
fn render_value_sql(val: &crate::driver::Value) -> String {
    match val {
        crate::driver::Value::Null => "NULL".to_string(),
        other => format!("'{}'", escape_string_literal(&other.display_str())),
    }
}

/// Render the user-edited buffer that lives in a modal (`CellEditModalState`
/// or `InsertRowModalState`). The `__DBX_NULL__` sentinel from `Ctrl+N` is
/// translated to the bare `NULL` keyword; everything else is quoted. Empty
/// string is intentionally rendered as `''` (a valid empty literal) so
/// the user can blank out a non-nullable text column when desired.
fn render_buffer_sql(buf: &str) -> String {
    if buf == NULL_SENTINEL {
        "NULL".to_string()
    } else {
        format!("'{}'", escape_string_literal(buf))
    }
}

/// Build a WHERE clause that targets a single row from a table page,
/// preferring primary-key columns (the safe, narrow target) and falling
/// back to a full-row match when no PK is defined. Returns the WHERE
/// fragment without the leading `WHERE ` keyword. Returns `None` when
/// no columns could be matched at all (e.g. zero-column table) so callers
/// can bail out with a toast instead of running a destructive statement
/// with an always-true WHERE.
fn build_where_for_row(
    columns: &[String],
    row: &crate::driver::Record,
    pk_cols: &[String],
    driver_name: &str,
) -> Option<String> {
    let mut where_clauses: Vec<String> = Vec::new();

    if !pk_cols.is_empty() {
        for pk in pk_cols {
            if let Some(pos) = columns.iter().position(|c| c == pk)
                && let Some(val) = row.values.get(pos)
            {
                let q_col = quote_ident(pk, driver_name);
                if matches!(val, crate::driver::Value::Null) {
                    where_clauses.push(format!("{q_col} IS NULL"));
                } else {
                    where_clauses.push(format!("{q_col} = {}", render_value_sql(val)));
                }
            }
        }
    } else {
        // No PK: match on every column. Less safe (could match multiple
        // rows if duplicates exist) but the only way to target a row in
        // tables without a defined primary key. The dialect-specific
        // `LIMIT 1` suffix appended by the caller narrows the blast radius
        // on backends that allow it.
        for (i, c) in columns.iter().enumerate() {
            if let Some(val) = row.values.get(i) {
                let q_col = quote_ident(c, driver_name);
                if matches!(val, crate::driver::Value::Null) {
                    where_clauses.push(format!("{q_col} IS NULL"));
                } else {
                    where_clauses.push(format!("{q_col} = {}", render_value_sql(val)));
                }
            }
        }
    }

    if where_clauses.is_empty() {
        None
    } else {
        Some(where_clauses.join(" AND "))
    }
}

/// Build a generic `INSERT INTO ns.tbl (col, col) VALUES (val, val)` from
/// a list of `(column_name, Option<buffer>)` pairs. `None` buffers are
/// skipped — the column is omitted from both the column list and the
/// values list, so the server applies the column's DEFAULT (or NULL when
/// nullable and no default, or rejects the row when NOT NULL with no
/// default). `Some(NULL_SENTINEL)` is rendered as the bare `NULL` keyword.
/// The output syntax is the standard SQL form that PostgreSQL, MySQL,
/// SQL Server, and SQLite all accept. Driver differences (PG `RETURNING`,
/// MySQL `LAST_INSERT_ID()`, etc.) are intentionally **not** baked in —
/// the caller can ignore rows_affected if it needs richer feedback.
fn build_insert_sql(
    cref: &crate::driver::CollectionRef,
    fields: &[(String, Option<String>)],
    driver_name: &str,
) -> Option<String> {
    // Strip out the skipped (None) columns. Order is preserved as supplied.
    let provided: Vec<(&String, &String)> = fields
        .iter()
        .filter_map(|(name, buf)| buf.as_ref().map(|b| (name, b)))
        .collect();
    if provided.is_empty() {
        return None;
    }
    let q_ns = quote_ident(&cref.namespace.0, driver_name);
    let q_tbl = quote_ident(&cref.name, driver_name);
    let col_list = provided
        .iter()
        .map(|(name, _)| quote_ident(name, driver_name))
        .collect::<Vec<_>>()
        .join(", ");
    let value_list = provided
        .iter()
        .map(|(_, buf)| render_buffer_sql(buf))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "INSERT INTO {q_ns}.{q_tbl} ({col_list}) VALUES ({value_list});"
    ))
}

/// Build a single-row `INSERT` for copy-to-clipboard (row-as-INSERT), using
/// the dialect-aware identifier quoting + value rendering. Unlike
/// `Exporter::format_sql_insert` (which hardcodes backticks for the export
/// file), this is safe on PostgreSQL too.
fn build_insert_row_sql(
    table: &str,
    columns: &[String],
    row: &crate::driver::Record,
    driver_name: &str,
) -> String {
    let q_tbl = quote_ident(table, driver_name);
    let col_list = columns
        .iter()
        .map(|c| quote_ident(c, driver_name))
        .collect::<Vec<_>>()
        .join(", ");
    let value_list = row
        .values
        .iter()
        .map(render_value_sql)
        .collect::<Vec<_>>()
        .join(", ");
    format!("INSERT INTO {q_tbl} ({col_list}) VALUES ({value_list});")
}

/// Expand a leading `~/` to the user's home directory.
fn expand_tilde(path: &str) -> std::path::PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return std::path::PathBuf::from(home).join(rest);
        }
    }
    std::path::PathBuf::from(path)
}

/// Recompute the console editor's autocomplete suggestions from the text
/// before the cursor.
fn refresh_autocomplete(
    c: &mut crate::ui::screens::query::QueryConsole,
    tables: &[String],
    columns: &std::collections::HashMap<String, Vec<String>>,
) {
    let before = c
        .lines
        .get(c.cursor_row)
        .map(|l| l.chars().take(c.cursor_col).collect::<String>())
        .unwrap_or_default();
    c.autocomplete = crate::ui::screens::query::suggest(&before, tables, columns);
    c.autocomplete_selected = 0;
}

/// Open a collection (table OR view) as a data tab: focus it if already
/// open, otherwise fetch column metadata + first page and push a `DataTab`.
/// Shared by the tree Enter handler (Table & View nodes) and the object
/// search — one construction path for every tab.
async fn open_collection_tab(
    exp: &mut crate::ui::screens::explorer::ExplorerState,
    drv: &Arc<dyn crate::driver::Driver>,
    cref: crate::driver::CollectionRef,
    page_size: u64,
    read_only: bool,
) -> Result<(), String> {
    if let Some(existing_idx) = exp.tabs.iter().position(|t| match t {
        WorkspaceTab::Table(tab) => tab.collection == cref,
        _ => false,
    }) {
        exp.active_tab_index = existing_idx;
        exp.focused_pane = FocusedPane::Workspace;
        return Ok(());
    }
    let meta_res = drv.collection_meta(&cref).await;
    let rec_res = drv
        .records(&cref, Page { offset: 0, limit: page_size })
        .await
        .map_err(|e| format!("{e:#}"))?;
    let column_meta = meta_res.map(|m| m.columns).unwrap_or_default();
    // Feed the console autocomplete with this table's column names.
    exp.column_cache.insert(
        format!("{}.{}", cref.namespace, cref.name),
        column_meta.iter().map(|c| c.name.clone()).collect(),
    );
    exp.tabs.push(WorkspaceTab::Table(crate::ui::screens::explorer::DataTab {
        collection: cref,
        page: rec_res,
        selected_row: 0,
        selected_col: 0,
        scroll_offset_x: 0,
        column_meta,
        sort_col: None,
        sort_dir: crate::ui::screens::explorer::SortDir::Asc,
        filter: None,
        filter_editing: false,
        filter_buffer: String::new(),
        read_only,
        grid_hit_area: None,
        grid_col_starts: Vec::new(),
    }));
    exp.active_tab_index = exp.tabs.len().saturating_sub(1);
    exp.focused_pane = FocusedPane::Workspace;
    Ok(())
}

/// Build an INSERT where every value is a quoted string literal — used by
/// CSV import so a cell whose text happens to equal the `__DBX_NULL__`
/// sentinel is inserted as that literal, never as SQL NULL.
fn build_insert_literal_sql(
    table: &str,
    columns: &[String],
    values: &[String],
    driver_name: &str,
) -> String {
    let q_tbl = quote_ident(table, driver_name);
    let col_list = columns
        .iter()
        .map(|c| quote_ident(c, driver_name))
        .collect::<Vec<_>>()
        .join(", ");
    let val_list = values
        .iter()
        .map(|v| format!("'{}'", escape_string_literal(v)))
        .collect::<Vec<_>>()
        .join(", ");
    format!("INSERT INTO {q_tbl} ({col_list}) VALUES ({val_list});")
}

/// Re-fetch the active table tab's current page (shared by insert-row,
/// sql-confirm and CSV-import so page-offset semantics stay consistent).
async fn refresh_table_page(
    exp: &mut crate::ui::screens::explorer::ExplorerState,
    drv: &Arc<dyn crate::driver::Driver>,
    cref: &crate::driver::CollectionRef,
) {
    if let Some(WorkspaceTab::Table(t)) = exp.active_tab_mut() {
        if t.collection == *cref {
            let cur_page = Page {
                offset: t.page.page * t.page.page_size,
                limit: t.page.page_size,
            };
            if let Ok(refreshed) = drv.records(cref, cur_page).await {
                t.page = refreshed;
            }
        }
    }
}

/// Roadmap M2.10: detect statements that can destroy data before the query
/// console runs them, so the user gets an explicit confirm dialog.
///
/// This is a deliberate heuristic (keyword/prefix scan), NOT a full SQL
/// parser. False positives are preferred over false negatives — an extra
/// confirm on a benign query is a small annoyance; a DROP that executes
/// without confirmation is a data-loss incident.
fn is_destructive_statement(query: &str) -> bool {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return false;
    }
    // Use the SAME splitter the executor uses (`split_statements`), so a
    // DROP hidden behind a `-- comment; more` can't slip past the guard
    // while the executor still runs it.
    crate::ui::screens::query::split_statements(query)
        .iter()
        .any(|s| is_destructive_stmt(strip_leading_comments(s)))
}

/// Is a single statement destructive (DROP / TRUNCATE / DELETE without WHERE
/// / ALTER that drops something)?
fn is_destructive_stmt(stmt: &str) -> bool {
    let first = stmt
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_uppercase();
    match first.as_str() {
        "DROP" | "TRUNCATE" => true,
        "DELETE" => !has_where_clause(stmt),
        "ALTER" => stmt.to_uppercase().contains(" DROP "),
        _ => false,
    }
}

/// Does a statement contain a `WHERE` keyword? Used to allow `DELETE … WHERE`
/// (targeted) through the guard while blocking `DELETE FROM t` (full table).
fn has_where_clause(stmt: &str) -> bool {
    stmt.split_whitespace()
        .any(|tok| tok.to_uppercase() == "WHERE")
}

/// Skip leading whitespace and leading `--` / `/* */` comments so the first
/// real keyword of a statement can be inspected.
fn strip_leading_comments(mut s: &str) -> &str {
    loop {
        s = s.trim_start();
        if s.starts_with("--") {
            s = s.lines().nth(1).unwrap_or("");
            continue;
        }
        if s.starts_with("/*") {
            if let Some(end) = s.find("*/") {
                s = &s[end + 2..];
                continue;
            }
        }
        break;
    }
    s
}

const PICKER_HINTS: [(&str, &str); 7] = [
    ("Enter", "connect"),
    ("a", "add"),
    ("e", "edit"),
    ("d", "delete"),
    ("t", "test"),
    ("q", "quit"),
    ("?", "help"),
];

const EXPLORER_HINTS: [(&str, &str); 12] = [
    ("Tab", "pane"),
    ("c", "new console"),
    ("g", "erd"),
    ("Ctrl+Enter", "run SQL"),
    ("Enter/Space", "open/expand"),
    ("y/Y", "copy cell/row"),
    ("Ctrl+E", "export"),
    ("e", "edit cell"),
    ("Backspace", "delete row"),
    ("i", "insert row"),
    ("w", "close tab"),
    ("Esc", "picker"),
];

const PICKER_HELP_BINDINGS: [(&str, &str); 7] = [
    ("Enter", "connect to selected database"),
    ("a", "add new connection"),
    ("e", "edit selected connection"),
    ("d", "delete selected connection"),
    ("t", "test connection ping"),
    ("q", "quit"),
    ("Esc", "close popup / back"),
];

const EXPLORER_HELP_BINDINGS: [(&str, &str); 29] = [
    ("Tab", "toggle focus between Explorer tree & Workspace / subpane"),
    ("c", "open new SQL Query Console tab"),
    ("g", "open In-Terminal ERD diagram for selected database"),
    ("Ctrl+T", "search all objects / jump to a table"),
    ("Ctrl+Enter / F5", "execute SQL query in active console"),
    ("Ctrl+R", "reconnect after a dropped connection"),
    ("Ctrl+Shift+I", "import rows from a CSV file into the active table"),
    ("Alt+H", "open query history for this connection"),
    ("Alt+F", "open saved favorite queries"),
    ("Ctrl+S", "save current query as favorite"),
    ("Ctrl+F", "pretty-print SQL in the editor"),
    ("[ / ] (in result)", "previous / next result set"),
    ("j / Down", "move cursor / selection down"),
    ("k / Up", "move cursor / selection up"),
    ("h / Left", "move cursor / column selection left"),
    ("l / Right", "move cursor / column selection right"),
    ("Space", "expand / collapse database node in tree"),
    ("Enter", "open table in workspace grid"),
    ("s", "sort data grid by active column (asc → desc → off)"),
    ("/", "filter data grid rows (col op value, e.g. status = paid)"),
    ("y / c", "copy active cell value to system clipboard"),
    ("Y / Ctrl+Y", "copy active row as formatted JSON to clipboard"),
    ("Ctrl+E", "open export dialog (CSV, JSON, SQL INSERT) for current dataset"),
    ("e / Enter", "edit active cell value (shows safe SQL confirmation)"),
    ("Backspace", "delete selected row (shows safe SQL confirmation)"),
    ("i", "open INSERT-row modal — fill fields, server applies DEFAULT for skipped"),
    ("F1", "view table DDL schema popup"),
    ("n / p", "next / previous page in data grid"),
    ("w", "close active workspace tab"),
];

pub enum ScreenMode {
    Picker,
    Connected,
}

pub struct App {
    config: AppConfig,
    config_path: PathBuf,
    theme: Theme,
    toasts: Toasts,
    help_open: bool,
    should_quit: bool,

    // Screen S1 / P5 state
    selected_connection: usize,
    form_modal: Option<ConnectionForm>,
    // In-flight test ping for the form modal. None = idle.
    form_test_rx: Option<tokio::sync::mpsc::Receiver<Result<std::time::Duration, String>>>,
    /// Confirmation dialog for destructive delete on a saved connection.
    /// `Some` means a confirmation popup is open; user must press Enter to
    /// actually delete, or Esc to cancel. Stores the original index so the
    /// delete targets the right connection even if the picker selection has
    /// moved (which it can't while this modal is open, but defensive).
    confirm_delete_modal: Option<ConfirmDeleteModal>,

    // Driver & Screen S2 state
    mode: ScreenMode,
    active_driver: Option<Arc<dyn Driver>>,
    explorer_state: Option<ExplorerState>,
    connecting: bool,
    /// Name of the connection currently connected — key for query history.
    active_connection_name: Option<String>,
    /// Full config of the active connection — used for reconnect.
    active_connection: Option<crate::config::ConnectionConfig>,
    /// Picker-pane area from the last draw — maps a mouse click to a connection.
    picker_hit_area: Option<Rect>,
}

impl App {
    pub fn new(config: AppConfig, config_path: PathBuf) -> Self {
        Self {
            config,
            config_path,
            theme: Theme::dark(),
            toasts: Toasts::default(),
            help_open: false,
            should_quit: false,
            selected_connection: 0,
            form_modal: None,
            form_test_rx: None,
            confirm_delete_modal: None,
            mode: ScreenMode::Picker,
            active_driver: None,
            explorer_state: None,
            connecting: false,
            active_connection_name: None,
            active_connection: None,
            picker_hit_area: None,
        }
    }

    /// Handle a mouse event. Currently only left-clicking an ERD node opens
    /// its DDL (hit-test in scene space); every other mouse gesture is a
    /// no-op. Keyboard navigation remains the primary interaction model.
    async fn handle_mouse(&mut self, mouse: MouseEvent) -> anyhow::Result<()> {
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return Ok(());
        }

        // S1: click a connection row to select it (Enter still connects).
        if matches!(self.mode, ScreenMode::Picker) {
            if let Some(area) = self.picker_hit_area {
                if mouse.row >= area.y && mouse.row < area.y + area.height {
                    let idx = (mouse.row - area.y).saturating_sub(1) as usize;
                    if idx < self.config.connections.len() {
                        self.selected_connection = idx;
                    }
                }
            }
            return Ok(());
        }

        let (Some(drv), Some(exp)) = (&self.active_driver, &mut self.explorer_state) else {
            return Ok(());
        };
        // An overlay modal owns the click — don't let it fall through to the
        // diagram behind it. The DDL popup dismisses on a click OUTSIDE its
        // painted rect (standard "click-away" behaviour); every other modal
        // swallows the click entirely.
        if exp.ddl_popup.is_some() {
            let inside = match exp.ddl_popup_area {
                Some(rect) => {
                    mouse.column >= rect.x
                        && mouse.column < rect.x + rect.width
                        && mouse.row >= rect.y
                        && mouse.row < rect.y + rect.height
                }
                None => false,
            };
            if !inside {
                exp.ddl_popup = None;
                exp.ddl_popup_area = None;
            }
            return Ok(());
        }
        if exp.export_modal.is_some()
            || exp.cell_edit_modal.is_some()
            || exp.insert_row_modal.is_some()
            || exp.sql_confirm_modal.is_some()
            || exp.object_search.is_some()
            || exp.import_csv_modal.is_some()
        {
            return Ok(());
        }

        // S2 tree: click a node to select it (Enter/Space still open).
        // `tree_hit_area` is border-excluded and nodes start at its top with
        // NO header row, so the row maps 1:1 (plus scroll). Column is checked
        // so a click in the workspace pane isn't swallowed by the tree.
        if let Some(area) = exp.tree_hit_area {
            if mouse.column >= area.x
                && mouse.column < area.x + area.width
                && mouse.row >= area.y
                && mouse.row < area.y + area.height
            {
                let idx = (mouse.row - area.y) as usize + exp.tree_scroll;
                if idx < exp.tree_nodes.len() {
                    exp.selected_tree_index = idx;
                    exp.focused_pane = FocusedPane::Tree;
                    return Ok(());
                }
            }
        }

        // S2 workspace: click a cell / row / ERD node.
        let mut focus_workspace = false;
        let Some(tab) = exp.active_tab_mut() else {
            return Ok(());
        };
        match tab {
            WorkspaceTab::Table(t) => {
                let Some(area) = t.grid_hit_area else { return Ok(()) };
                // Column check so a click on empty sidebar space below the
                // tree doesn't fall through to the grid.
                if mouse.column >= area.x
                    && mouse.column < area.x + area.width
                    && mouse.row >= area.y
                    && mouse.row < area.y + area.height
                {
                    let rel_row = mouse.row - area.y;
                    // Table header (1) + bottom_margin (1) = 2 rows before data.
                    if rel_row >= 2 {
                        let data_row = (rel_row - 2) as usize;
                        // Bound against the *displayed* (filtered) rows.
                        let visible = t
                            .filter
                            .as_ref()
                            .map(|f| {
                                t.page
                                    .records
                                    .iter()
                                    .filter(|r| crate::ui::screens::explorer::record_matches_filter(r, f))
                                    .count()
                            })
                            .unwrap_or(t.page.records.len());
                        if data_row < visible {
                            t.selected_row = data_row;
                        }
                    }
                    // Column hit-test against the exact x-starts recorded at
                    // render time. Clicking the header row selects the column.
                    let mut col_visible = 0usize;
                    for (i, s) in t.grid_col_starts.iter().enumerate() {
                        if mouse.column >= *s {
                            col_visible = i;
                        } else {
                            break;
                        }
                    }
                    t.selected_col = (col_visible + t.scroll_offset_x)
                        .min(t.page.columns.len().saturating_sub(1));
                    focus_workspace = true;
                }
            }
            WorkspaceTab::Console(c) => {
                let Some(area) = c.result_hit_area else { return Ok(()) };
                if mouse.column >= area.x
                    && mouse.column < area.x + area.width
                    && mouse.row >= area.y
                    && mouse.row < area.y + area.height
                {
                    if let Some(res) = &c.last_result {
                        let rel_row = mouse.row - area.y;
                        // Table header (1) + bottom_margin (1) = 2 rows.
                        if rel_row >= 2 {
                            let data_row = (rel_row - 2) as usize;
                            if data_row < res.records.len() {
                                c.result_selected_row = data_row;
                            }
                        }
                        let mut col_visible = 0usize;
                        for (i, s) in c.result_col_starts.iter().enumerate() {
                            if mouse.column >= *s {
                                col_visible = i;
                            } else {
                                break;
                            }
                        }
                        c.result_selected_col = (col_visible + c.result_scroll_x)
                            .min(res.columns.len().saturating_sub(1));
                        c.focused_subpane = ConsoleSubpane::Result;
                        focus_workspace = true;
                    }
                }
            }
            WorkspaceTab::Erd(erd) => {
                let Some(idx) = erd.node_at_mouse(mouse.column, mouse.row) else {
                    return Ok(());
                };
                erd.selected_node = Some(idx);
                let cref = {
                    let Some(scene) = &erd.scene else { return Ok(()) };
                    let node = &scene.scene.nodes[idx];
                    crate::driver::CollectionRef {
                        namespace: erd.namespace.clone(),
                        name: node.id.clone(),
                    }
                };
                // Borrow of `erd` ends here; NLL lets us touch `exp` again.
                match drv.definition(&cref).await {
                    Ok(ddl) => {
                        exp.ddl_popup = Some((cref, ddl));
                    }
                    Err(e) => {
                        self.toasts
                            .push(ToastKind::Error, format!("failed to fetch DDL: {e:#}"));
                    }
                }
            }
        }
        if focus_workspace {
            exp.focused_pane = FocusedPane::Workspace;
        }
        Ok(())
    }

    fn handle_key(&mut self, key: KeyEvent) {
        // Universal exit: in raw mode Ctrl+C arrives as a key event, not SIGINT.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }

        // If help is open, Esc or ? closes it
        if self.help_open {
            if key.code == KeyCode::Esc || key.code == KeyCode::Char('?') {
                self.help_open = false;
            }
            return;
        }

        // Screen S2 (Explorer) Handlers
        if matches!(self.mode, ScreenMode::Connected) {
            if let Some(exp) = &mut self.explorer_state {
                // If DDL popup is open, Esc closes it
                if exp.ddl_popup.is_some() {
                    if key.code == KeyCode::Esc {
                        exp.ddl_popup = None;
                    }
                    return;
                }

                // If Export modal is open, route keys
                if let Some(modal) = &mut exp.export_modal {
                    match key.code {
                        KeyCode::Esc => {
                            exp.export_modal = None;
                        }
                        KeyCode::Tab => {
                            modal.active_field = (modal.active_field + 1) % 2;
                        }
                        KeyCode::Left | KeyCode::Right => {
                            if modal.active_field == 0 {
                                let formats = ExportFormat::ALL;
                                let curr_idx = formats.iter().position(|f| *f == modal.format).unwrap_or(0);
                                let next_idx = if key.code == KeyCode::Right {
                                    (curr_idx + 1) % formats.len()
                                } else {
                                    (curr_idx + formats.len() - 1) % formats.len()
                                };
                                modal.format = formats[next_idx];
                                let default_ext = modal.format.extension();
                                modal.target_path = format!("~/dbx_export_{}.{}", modal.default_table_name, default_ext);
                                modal.confirm_overwrite = false; // new path → re-confirm
                            }
                        }
                        KeyCode::Backspace => {
                            if modal.active_field == 1 {
                                modal.target_path.pop();
                                modal.confirm_overwrite = false; // path changed
                            }
                        }
                        KeyCode::Char(c) => {
                            if modal.active_field == 1 && !key.modifiers.contains(KeyModifiers::CONTROL) {
                                modal.target_path.push(c);
                                modal.confirm_overwrite = false; // path changed
                            }
                        }
                        _ => {}
                    }
                    return;
                }

                // If Cell edit modal is open, route keys.
                // We split work into two phases to satisfy the borrow checker:
                //   1) read-only phase: extract row/col index and (if the buffer holds
                //      the NULL sentinel) the cell's original value, then drop the
                //      immutable borrow of `exp`.
                //   2) mutation phase: take `&mut exp.cell_edit_modal` and apply the
                //      decision from phase 1. This avoids holding both borrows at once.
                if exp.cell_edit_modal.is_some() {
                    let is_ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

                    // Phase 1: gather everything we need from immutable borrows.
                    let plan: EditKeyPlan = {
                        let edit = exp.cell_edit_modal.as_ref().unwrap();
                        let is_sentinel = edit.text_buffer == NULL_SENTINEL;
                        let original_value: Option<String> = if is_sentinel {
                            exp.active_tab().and_then(|tab| match tab {
                                WorkspaceTab::Table(t) => t
                                    .page
                                    .records
                                    .get(edit.row_idx)
                                    .and_then(|row| row.values.get(edit.col_idx))
                                    .map(|v| v.display_str()),
                                _ => None,
                            })
                        } else {
                            None
                        };
                        EditKeyPlan {
                            is_ctrl,
                            key: key.code,
                            char_payload: extract_char_payload(key.code, is_ctrl),
                            is_sentinel,
                            is_nullable: edit.is_nullable,
                            original_value,
                        }
                    };

                    // Phase 2: apply. Only one path actually mutates.
                    match plan.key {
                        KeyCode::Esc => {
                            exp.cell_edit_modal = None;
                        }
                        // Ctrl+N: set the cell to NULL. Only enabled on nullable columns.
                        // Stores the NULL_SENTINEL in the buffer; the SQL builder translates
                        // it to a bare `= NULL` clause. Idempotent — pressing it again is a
                        // no-op while the sentinel is already set.
                        KeyCode::Char('n') | KeyCode::Char('N') if plan.is_ctrl => {
                            if plan.is_nullable {
                                if !plan.is_sentinel {
                                    let edit = exp.cell_edit_modal.as_mut().unwrap();
                                    edit.text_buffer = NULL_SENTINEL.to_string();
                                    self.toasts.push(
                                        ToastKind::Info,
                                        "cell will be set to NULL (press [Enter] to review SQL)"
                                            .to_string(),
                                    );
                                }
                            } else {
                                self.toasts.push(
                                    ToastKind::Warning,
                                    "this column is NOT NULL — cannot set to NULL".to_string(),
                                );
                            }
                        }
                        // Ctrl+G: clear the NULL sentinel and restore the current cell's
                        // original value, so the user can back out of the NULL choice
                        // without closing the whole modal.
                        KeyCode::Char('g') | KeyCode::Char('G') if plan.is_ctrl => {
                            if let Some(orig) = plan.original_value {
                                let edit = exp.cell_edit_modal.as_mut().unwrap();
                                edit.text_buffer = orig;
                            }
                        }
                        KeyCode::Backspace => {
                            // Don't let Backspace corrupt the NULL sentinel — just clear it
                            // and return to the original value, like Ctrl+G.
                            if let Some(orig) = plan.original_value {
                                let edit = exp.cell_edit_modal.as_mut().unwrap();
                                edit.text_buffer = orig;
                            } else {
                                let edit = exp.cell_edit_modal.as_mut().unwrap();
                                edit.text_buffer.pop();
                            }
                        }
                        KeyCode::Char(_) if !plan.is_ctrl => {
                            if let Some(c) = plan.char_payload {
                                let edit = exp.cell_edit_modal.as_mut().unwrap();
                                // Typing any character while the NULL sentinel is set
                                // clears it first (overwrite semantics), so the user
                                // can't accidentally mix literal text with the sentinel.
                                if plan.is_sentinel {
                                    edit.text_buffer = String::new();
                                }
                                edit.text_buffer.push(c);
                            }
                        }
                        _ => {
                            // Anything else (function keys, ctrl-modified chars, etc.) is
                            // ignored inside the cell-edit modal — the user must Esc out
                            // to use global bindings.
                        }
                    }
                    return;
                }

                // If INSERT-row modal is open, route keys. Same 2-phase pattern
                // as the cell-edit modal: gather everything from immutable
                // borrows first, then mutate in phase 2. Insert modal doesn't
                // need a SQL-preview confirm step — the user has already
                // reviewed their values field by field, and the statement is
                // additive (new row) rather than destructive.
                if exp.insert_row_modal.is_some() {
                    let is_ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                    let plan: InsertKeyPlan = {
                        let m = exp.insert_row_modal.as_ref().unwrap();
                        let n = m.column_meta.len();
                        let focused = m.focused_field;
                        let is_sentinel = m
                            .field_buffers
                            .get(focused)
                            .and_then(|x| x.as_ref())
                            .map(|s| s == NULL_SENTINEL)
                            .unwrap_or(false);
                        let is_nullable = m
                            .column_meta
                            .get(focused)
                            .map(|c| c.is_nullable)
                            .unwrap_or(false);
                        let is_skip = m
                            .field_buffers
                            .get(focused)
                            .map(|x| x.is_none())
                            .unwrap_or(true);
                        InsertKeyPlan {
                            is_ctrl,
                            key: key.code,
                            char_payload: extract_char_payload(key.code, is_ctrl),
                            is_sentinel,
                            is_nullable,
                            is_skip,
                            focused,
                            n,
                        }
                    };

                    // Phase 2: apply the plan to the mutable modal state.
                    match plan.key {
                        KeyCode::Esc => {
                            // Esc on an empty field → flip back to "skip" state
                            // (handy for "I changed my mind, let the server
                            // apply DEFAULT"). Esc on a populated field or on
                            // the sentinel just cancels the modal entirely.
                            if plan.is_skip {
                                exp.insert_row_modal = None;
                                self.toasts.push(
                                    ToastKind::Info,
                                    "insert cancelled".to_string(),
                                );
                            } else {
                                let m = exp.insert_row_modal.as_mut().unwrap();
                                m.field_buffers[plan.focused] = None;
                            }
                        }
                        KeyCode::Tab | KeyCode::Down => {
                            if plan.n > 0 {
                                let m = exp.insert_row_modal.as_mut().unwrap();
                                m.focused_field = (plan.focused + 1) % plan.n;
                            }
                        }
                        KeyCode::BackTab | KeyCode::Up => {
                            if plan.n > 0 {
                                let m = exp.insert_row_modal.as_mut().unwrap();
                                m.focused_field = if plan.focused == 0 {
                                    plan.n - 1
                                } else {
                                    plan.focused - 1
                                };
                            }
                        }
                        // Ctrl+N: mark the focused field as NULL. Only valid
                        // on nullable columns; non-nullable columns get a
                        // warning toast (the row would be rejected anyway).
                        KeyCode::Char('n') | KeyCode::Char('N') if plan.is_ctrl => {
                            if plan.is_nullable {
                                if !plan.is_sentinel {
                                    let m = exp.insert_row_modal.as_mut().unwrap();
                                    m.field_buffers[plan.focused] =
                                        Some(NULL_SENTINEL.to_string());
                                }
                            } else {
                                self.toasts.push(
                                    ToastKind::Warning,
                                    "this column is NOT NULL — cannot set to NULL".to_string(),
                                );
                            }
                        }
                        // Backspace: drop the last char of the focused buffer.
                        // If the buffer holds the NULL sentinel, Backspace
                        // flips it back to the "skip" state instead of
                        // corrupting the literal sentinel string.
                        KeyCode::Backspace => {
                            let m = exp.insert_row_modal.as_mut().unwrap();
                            match m.field_buffers[plan.focused].as_mut() {
                                None => {}
                                Some(s) if s == NULL_SENTINEL => {
                                    m.field_buffers[plan.focused] = None;
                                }
                                Some(s) => {
                                    s.pop();
                                    if s.is_empty() {
                                        // Empty string is the same as
                                        // "skip" — collapse it so the UI
                                        // shows <skip> instead of an
                                        // empty bar. Saves a keystroke.
                                        m.field_buffers[plan.focused] = None;
                                    }
                                }
                            }
                        }
                        KeyCode::Char(_) if !plan.is_ctrl => {
                            if let Some(c) = plan.char_payload {
                                let m = exp.insert_row_modal.as_mut().unwrap();
                                let buf = m.field_buffers[plan.focused].get_or_insert_with(String::new);
                                // If the buffer is the NULL sentinel, the
                                // first typed char starts a fresh value
                                // (overwrite semantics) — same as cell-edit.
                                if buf == NULL_SENTINEL {
                                    m.field_buffers[plan.focused] = Some(c.to_string());
                                } else {
                                    buf.push(c);
                                }
                            }
                        }
                        // Enter (submit) is intentionally handled in the async
                        // event loop, not here. Building the SQL + executing
                        // it against the driver + refreshing the page all need
                        // `.await` and access to `drv` / `app`, which are
                        // out of scope in this sync `handle_key`. The
                        // cell-edit modal uses the same split: char/backspace
                        // routing happens here, but Enter opens the
                        // SQL-confirm modal in the event loop.
                        KeyCode::Enter => {}
                        _ => {}
                    }
                    return;
                }

                // If SQL confirm modal is open, Esc cancels
                if exp.sql_confirm_modal.is_some() {
                    if key.code == KeyCode::Esc {
                        exp.sql_confirm_modal = None;
                    }
                    return;
                }

                // Object search overlay owns all keys while it's open.
                if let Some(s) = &mut exp.object_search {
                    match key.code {
                        KeyCode::Esc => exp.object_search = None,
                        KeyCode::Backspace => {
                            s.query.pop();
                        }
                        KeyCode::Up | KeyCode::Char('k') => s.selected = s.selected.saturating_sub(1),
                        KeyCode::Down | KeyCode::Char('j') => {
                            let n = s
                                .results
                                .iter()
                                .filter(|(r, _)| {
                                    r.name.contains(&s.query) || r.namespace.0.contains(&s.query)
                                })
                                .count();
                            if n > 0 {
                                s.selected = (s.selected + 1).min(n - 1);
                            }
                        }
                        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => s.query.push(c),
                        _ => {}
                    }
                    return;
                }

                // CSV-import modal: path typing + read-on-Enter. (Enter again
                // once parsed is handled in the async event loop — it needs
                // `drv.execute`.)
                if let Some(imp) = &mut exp.import_csv_modal {
                    match key.code {
                        KeyCode::Esc => exp.import_csv_modal = None,
                        KeyCode::Backspace if !imp.parsed => {
                            imp.path.pop();
                        }
                        KeyCode::Char(c)
                            if !imp.parsed && !key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            imp.path.push(c);
                        }
                        KeyCode::Enter => {
                            if !imp.parsed {
                                let resolved = expand_tilde(&imp.path);
                                match std::fs::read_to_string(&resolved) {
                                    Ok(content) => {
                                        imp.rows = crate::export::parse_csv(&content);
                                        imp.parsed = true;
                                    }
                                    Err(e) => {
                                        self.toasts.push(
                                            ToastKind::Error,
                                            format!("failed to read CSV: {e:#}"),
                                        );
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                    return;
                }

                match key.code {
                    KeyCode::Esc => {
                        // Overlays with their own Esc handler close first, so
                        // Esc still dismisses them (don't hijack into
                        // back-one-level or the picker).
                        if let Some(tab) = exp.active_tab_mut() {
                            match tab {
                                WorkspaceTab::Table(t) if t.filter_editing => {
                                    t.filter_editing = false;
                                    return;
                                }
                                WorkspaceTab::Console(c) if c.popup.is_some() => {
                                    c.popup = None;
                                    return;
                                }
                                _ => {}
                            }
                        }
                        // Back one level at a time instead of jumping straight
                        // to the connection picker: an active workspace tab
                        // first returns focus to the tree, then Esc again (on
                        // the tree) disconnects back to the picker. Modals are
                        // already dismissed by their own handlers above.
                        if exp.focused_pane == FocusedPane::Workspace {
                            exp.focused_pane = FocusedPane::Tree;
                            return;
                        }
                        self.mode = ScreenMode::Picker;
                        self.active_driver = None;
                        self.explorer_state = None;
                        // Teardown must mirror connect: drop the retained
                        // connection config (and its password) too.
                        self.active_connection = None;
                        self.active_connection_name = None;
                        return;
                    }
                    KeyCode::Tab => {
                        // Autocomplete takes priority over pane-switching:
                        // Tab inserts the highlighted suggestion.
                        if exp.focused_pane == FocusedPane::Workspace
                            && let Some(WorkspaceTab::Console(c)) = exp.active_tab_mut()
                            && c.focused_subpane == ConsoleSubpane::Editor
                            && !c.autocomplete.is_empty()
                        {
                            c.accept_autocomplete();
                            return;
                        }
                        if exp.focused_pane == FocusedPane::Tree {
                            exp.focused_pane = FocusedPane::Workspace;
                        } else {
                            let mut switch_to_tree = false;
                            if let Some(WorkspaceTab::Console(c)) = exp.active_tab_mut() {
                                if c.focused_subpane == ConsoleSubpane::Editor {
                                    c.focused_subpane = ConsoleSubpane::Result;
                                } else {
                                    c.focused_subpane = ConsoleSubpane::Editor;
                                    switch_to_tree = true;
                                }
                            } else {
                                switch_to_tree = true;
                            }

                            if switch_to_tree {
                                exp.focused_pane = FocusedPane::Tree;
                            }
                        }
                    }
                    KeyCode::Char('?') => {
                        self.help_open = true;
                    }
                    KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        if let Some(tab) = exp.active_tab() {
                            match tab {
                                WorkspaceTab::Table(t) => {
                                    let default_fmt = ExportFormat::Csv;
                                    let path = format!("~/dbx_export_{}.{}", t.collection.name, default_fmt.extension());
                                    exp.export_modal = Some(ExportModalState {
                                        format: default_fmt,
                                        target_path: path,
                                        active_field: 0,
                                        default_table_name: t.collection.name.clone(),
                                        confirm_overwrite: false,
                                    });
                                }
                                WorkspaceTab::Console(c) => {
                                    if let Some(res) = &c.last_result && !res.records.is_empty() {
                                        let default_fmt = ExportFormat::Csv;
                                        let path = format!("~/dbx_export_query_result.{}", default_fmt.extension());
                                        exp.export_modal = Some(ExportModalState {
                                            format: default_fmt,
                                            target_path: path,
                                            active_field: 0,
                                            default_table_name: "query_result".to_string(),
                                            confirm_overwrite: false,
                                        });
                                    } else {
                                        self.toasts.push(ToastKind::Info, "no query result to export".to_string());
                                    }
                                }
                                WorkspaceTab::Erd(_) => {}
                            }
                        }
                    }
                    KeyCode::Char('q') => {
                        self.should_quit = true;
                    }
                    _ => {}
                }

                match exp.focused_pane {
                    FocusedPane::Tree => match key.code {
                        KeyCode::Up | KeyCode::Char('k') => {
                            if exp.selected_tree_index > 0 {
                                exp.selected_tree_index -= 1;
                            }
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            if !exp.tree_nodes.is_empty()
                                && exp.selected_tree_index < exp.tree_nodes.len() - 1
                            {
                                exp.selected_tree_index += 1;
                            }
                        }
                        _ => {}
                    },
                    FocusedPane::Workspace => {
                        let can_edit = exp.driver_capabilities.contains(crate::driver::Capabilities::EDIT_DATA);
                        // Snapshot for console autocomplete (borrowed before
                        // `active_tab_mut` takes &mut on `exp`).
                        let (ac_tables, ac_columns) = (
                            exp.tables
                                .values()
                                .flatten()
                                .map(|c| c.name.clone())
                                .collect::<Vec<String>>(),
                            exp.column_cache.clone(),
                        );
                        if let Some(tab) = exp.active_tab_mut() {
                            match tab {
                                WorkspaceTab::Table(t) => {
                                    if t.filter_editing {
                                        // Filter input mode: every key feeds the
                                        // filter buffer until Enter/Esc.
                                        match key.code {
                                            KeyCode::Esc => t.filter_editing = false,
                                            KeyCode::Enter => {
                                                t.filter =
                                                    crate::ui::screens::explorer::parse_filter(
                                                        &t.filter_buffer,
                                                        &t.page.columns,
                                                    );
                                                t.filter_editing = false;
                                            }
                                            KeyCode::Backspace => {
                                                t.filter_buffer.pop();
                                            }
                                            KeyCode::Char(c)
                                                if !key.modifiers
                                                    .contains(KeyModifiers::CONTROL) =>
                                            {
                                                t.filter_buffer.push(c);
                                            }
                                            _ => {}
                                        }
                                    } else {
                                        match key.code {
                                    KeyCode::Char('i')
                                        if key.modifiers.contains(KeyModifiers::CONTROL)
                                            && key.modifiers.contains(KeyModifiers::SHIFT) =>
                                    {
                                        // Ctrl+Shift+I: import rows from a CSV file.
                                        exp.import_csv_modal = Some(
                                            crate::ui::screens::explorer::ImportCsvModalState {
                                                path: String::new(),
                                                rows: Vec::new(),
                                                parsed: false,
                                            },
                                        );
                                    }
                                    KeyCode::Char('i') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                        // Copy the selected row as an INSERT statement.
                                        if let Some(row) = t.page.records.get(t.selected_row) {
                                            let driver_name = self
                                                .active_driver
                                                .as_ref()
                                                .map(|d| d.info().name.clone())
                                                .unwrap_or_default();
                                            let sql = build_insert_row_sql(
                                                &t.collection.name,
                                                &t.page.columns,
                                                row,
                                                &driver_name,
                                            );
                                            match ClipboardManager::set_text(&sql) {
                                                Ok(_) => self.toasts.push(
                                                    ToastKind::Success,
                                                    "copied row as INSERT to clipboard".to_string(),
                                                ),
                                                Err(e) => self.toasts.push(ToastKind::Error, e),
                                            }
                                        }
                                    }
                                    KeyCode::Char('s') => {
                                        // Client-side sort on the selected column:
                                        // off → asc → desc → off.
                                        let col = t.selected_col;
                                        t.sort_col = match t.sort_col {
                                            Some(c) if c == col => match t.sort_dir {
                                                crate::ui::screens::explorer::SortDir::Asc => {
                                                    t.sort_dir = crate::ui::screens::explorer::SortDir::Desc;
                                                    Some(col)
                                                }
                                                crate::ui::screens::explorer::SortDir::Desc => None,
                                            },
                                            _ => {
                                                t.sort_dir = crate::ui::screens::explorer::SortDir::Asc;
                                                Some(col)
                                            }
                                        };
                                    }
                                    KeyCode::Up | KeyCode::Char('k') => {
                                        if t.selected_row > 0 {
                                            t.selected_row -= 1;
                                        }
                                    }
                                    KeyCode::Down | KeyCode::Char('j') => {
                                        if !t.page.records.is_empty() && t.selected_row < t.page.records.len() - 1 {
                                            t.selected_row += 1;
                                        }
                                    }
                                    KeyCode::Left | KeyCode::Char('h') => {
                                        if t.selected_col > 0 {
                                            t.selected_col -= 1;
                                            if t.selected_col < t.scroll_offset_x {
                                                t.scroll_offset_x = t.selected_col;
                                            }
                                        }
                                    }
                                    KeyCode::Right | KeyCode::Char('l') => {
                                        if !t.page.columns.is_empty() && t.selected_col < t.page.columns.len() - 1 {
                                            t.selected_col += 1;
                                            if t.selected_col >= t.scroll_offset_x + 6 {
                                                t.scroll_offset_x = t.selected_col.saturating_sub(5);
                                            }
                                        }
                                    }
                                    KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                        if let Some(row) = t.page.records.get(t.selected_row) {
                                            match ClipboardManager::copy_row_tsv(row) {
                                                Ok(_) => self.toasts.push(ToastKind::Success, "copied row as TSV (spreadsheet) to clipboard".to_string()),
                                                Err(e) => self.toasts.push(ToastKind::Error, e),
                                            }
                                        }
                                    }
                                    KeyCode::Char('y') => {
                                        if let Some(row) = t.page.records.get(t.selected_row) {
                                            if let Some(val) = row.values.get(t.selected_col) {
                                                match ClipboardManager::copy_cell(val) {
                                                    Ok(_) => self.toasts.push(ToastKind::Success, "copied cell to clipboard".to_string()),
                                                    Err(e) => self.toasts.push(ToastKind::Error, e),
                                                }
                                            }
                                        }
                                    }
                                    KeyCode::Char('c') => {
                                        if !key.modifiers.contains(KeyModifiers::CONTROL) {
                                            if let Some(row) = t.page.records.get(t.selected_row) {
                                                if let Some(val) = row.values.get(t.selected_col) {
                                                    match ClipboardManager::copy_cell(val) {
                                                        Ok(_) => self.toasts.push(ToastKind::Success, "copied cell to clipboard".to_string()),
                                                        Err(e) => self.toasts.push(ToastKind::Error, e),
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    KeyCode::Char('Y') => {
                                        if let Some(row) = t.page.records.get(t.selected_row) {
                                            match ClipboardManager::copy_row_json(&t.page.columns, row) {
                                                Ok(_) => self.toasts.push(ToastKind::Success, "copied row as JSON to clipboard".to_string()),
                                                Err(e) => self.toasts.push(ToastKind::Error, e),
                                            }
                                        }
                                    }
                                    KeyCode::Char('e') | KeyCode::Enter => {
                                        if t.read_only {
                                            self.toasts.push(ToastKind::Warning, "this view is read-only".to_string());
                                        } else if !can_edit {
                                            self.toasts.push(ToastKind::Warning, "active driver does not support editing data");
                                        } else if let Some(row) = t.page.records.get(t.selected_row)
                                            && let Some(col_name) = t.page.columns.get(t.selected_col)
                                            && let Some(val) = row.values.get(t.selected_col)
                                        {
                                            // Look up the column's nullable flag from the metadata that
                                            // was fetched when the tab was opened. If metadata is missing
                                            // (e.g. legacy tab or query console), default to true so the
                                            // user still has the option; the UPDATE will fail at the DB
                                            // level if the column is actually NOT NULL.
                                            let is_nullable = t
                                                .column_meta
                                                .iter()
                                                .find(|m| m.name == *col_name)
                                                .map(|m| m.is_nullable)
                                                .unwrap_or(true);
                                            exp.cell_edit_modal = Some(crate::ui::screens::explorer::CellEditModalState {
                                                collection: t.collection.clone(),
                                                column_name: col_name.clone(),
                                                row_idx: t.selected_row,
                                                col_idx: t.selected_col,
                                                text_buffer: val.display_str(),
                                                is_nullable,
                                            });
                                        }
                                    }
                                    KeyCode::Char('/') => {
                                        // Enter filter-editing mode; pre-fill the
                                        // current filter so it can be tweaked.
                                        t.filter_editing = true;
                                        t.filter_buffer = t
                                            .filter
                                            .as_ref()
                                            .map(|f| f.display())
                                            .unwrap_or_default();
                                    }
                                    _ => {}
                                }
                                    }
                                },
                                WorkspaceTab::Console(c) => {
                                    // History / favorites picker owns all keys.
                                    if let Some(popup) = &mut c.popup {
                                        match key.code {
                                            KeyCode::Esc => c.popup = None,
                                            KeyCode::Up | KeyCode::Char('k') => {
                                                popup.selected = popup.selected.saturating_sub(1);
                                            }
                                            KeyCode::Down | KeyCode::Char('j') => {
                                                popup.selected = (popup.selected + 1)
                                                    .min(popup.items.len().saturating_sub(1));
                                            }
                                            KeyCode::Enter => {
                                                let payload = popup
                                                    .items
                                                    .get(popup.selected)
                                                    .map(|(_, p)| p.clone());
                                                c.popup = None;
                                                if let Some(sql) = payload {
                                                    c.set_text(sql);
                                                }
                                            }
                                            _ => {}
                                        }
                                        return;
                                    }
                                    match c.focused_subpane {
                                    ConsoleSubpane::Editor => match key.code {
                                        KeyCode::Up => {
                                            c.move_cursor_up();
                                            c.autocomplete.clear();
                                        }
                                        KeyCode::Down => {
                                            c.move_cursor_down();
                                            c.autocomplete.clear();
                                        }
                                        KeyCode::Left => {
                                            c.move_cursor_left();
                                            c.autocomplete.clear();
                                        }
                                        KeyCode::Right => {
                                            c.move_cursor_right();
                                            c.autocomplete.clear();
                                        }
                                        KeyCode::Backspace => {
                                            c.backspace();
                                            refresh_autocomplete(c, &ac_tables, &ac_columns);
                                        }
                                        KeyCode::Enter => {
                                            c.insert_newline();
                                            c.autocomplete.clear();
                                        }
                                        // Alt+H: open this connection's query history.
                                        // (Not Ctrl+H — on many terminals Ctrl+H is the
                                        // Backspace erase byte, so it must stay free.)
                                        KeyCode::Char('h') if key.modifiers.contains(KeyModifiers::ALT) => {
                                            let conn = self.active_connection_name.clone().unwrap_or_default();
                                            let items = self
                                                .config
                                                .query_history
                                                .get(&conn)
                                                .cloned()
                                                .unwrap_or_default()
                                                .into_iter()
                                                .map(|q| (q.clone(), q))
                                                .collect();
                                            c.popup = Some(crate::ui::screens::query::ConsolePopup {
                                                title: format!("Query History ({conn})"),
                                                items,
                                                selected: 0,
                                            });
                                        }
                                        // Alt+F: open saved favorite queries.
                                        // (Ctrl+Shift+F is unreliable: crossterm drops
                                        // the SHIFT modifier on most terminals.)
                                        KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::ALT) => {
                                            c.popup = Some(crate::ui::screens::query::ConsolePopup {
                                                title: "Saved Queries".to_string(),
                                                items: self.config.favorites.clone(),
                                                selected: 0,
                                            });
                                        }
                                        // Ctrl+S: save the current query as a favorite.
                                        KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                            let sql = c.text();
                                            let name = c
                                                .lines
                                                .first()
                                                .cloned()
                                                .unwrap_or_default()
                                                .trim()
                                                .to_string();
                                            let base = if name.is_empty() {
                                                "untitled".to_string()
                                            } else {
                                                name
                                            };
                                            // Disambiguate duplicates so a later favorite
                                            // never silently overwrites an earlier one.
                                            let mut name = base.clone();
                                            let mut i = 2;
                                            while self.config.favorites.iter().any(|(n, _)| n == &name) {
                                                name = format!("{base} ({i})");
                                                i += 1;
                                            }
                                            self.config.favorites.push((name.clone(), sql));
                                            match self.config.save(&self.config_path) {
                                                Ok(_) => self.toasts.push(
                                                    ToastKind::Success,
                                                    format!("saved favorite '{name}'"),
                                                ),
                                                Err(e) => self.toasts.push(
                                                    ToastKind::Error,
                                                    format!("failed to save favorite: {e:#}"),
                                                ),
                                            }
                                        }
                                        // Ctrl+F: pretty-print the SQL.
                                        KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                            let formatted = crate::ui::screens::query::format_sql(&c.text());
                                            c.set_text(formatted);
                                        }
                                        KeyCode::Char(ch) => {
                                            if !key.modifiers.contains(KeyModifiers::CONTROL) {
                                                c.insert_char(ch);
                                                refresh_autocomplete(c, &ac_tables, &ac_columns);
                                            }
                                        }
                                        _ => {}
                                    },
                                    ConsoleSubpane::Result => match key.code {
                                        // Switch between multiple result sets.
                                        KeyCode::Char('[') => {
                                            if c.active_result > 0 {
                                                c.active_result -= 1;
                                                c.last_result = c.results.get(c.active_result).cloned();
                                                c.result_selected_row = 0;
                                                c.result_selected_col = 0;
                                                c.result_scroll_x = 0;
                                            }
                                        }
                                        KeyCode::Char(']') => {
                                            if c.active_result + 1 < c.results.len() {
                                                c.active_result += 1;
                                                c.last_result = c.results.get(c.active_result).cloned();
                                                c.result_selected_row = 0;
                                                c.result_selected_col = 0;
                                                c.result_scroll_x = 0;
                                            }
                                        }
                                        KeyCode::Up | KeyCode::Char('k') => {
                                            if c.result_selected_row > 0 {
                                                c.result_selected_row -= 1;
                                            }
                                        }
                                        KeyCode::Down | KeyCode::Char('j') => {
                                            if let Some(res) = &c.last_result {
                                                if !res.records.is_empty() && c.result_selected_row < res.records.len() - 1 {
                                                    c.result_selected_row += 1;
                                                }
                                            }
                                        }
                                        KeyCode::Left | KeyCode::Char('h') => {
                                            if c.result_selected_col > 0 {
                                                c.result_selected_col -= 1;
                                                if c.result_selected_col < c.result_scroll_x {
                                                    c.result_scroll_x = c.result_selected_col;
                                                }
                                            }
                                        }
                                        KeyCode::Right | KeyCode::Char('l') => {
                                            if let Some(res) = &c.last_result {
                                                if !res.columns.is_empty() && c.result_selected_col < res.columns.len() - 1 {
                                                    c.result_selected_col += 1;
                                                    if c.result_selected_col >= c.result_scroll_x + 6 {
                                                        c.result_scroll_x = c.result_selected_col.saturating_sub(5);
                                                    }
                                                }
                                            }
                                        }
                                        KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                            if let Some(res) = &c.last_result {
                                                if let Some(row) = res.records.get(c.result_selected_row) {
                                                    match ClipboardManager::copy_row_tsv(row) {
                                                        Ok(_) => self.toasts.push(ToastKind::Success, "copied row as TSV (spreadsheet) to clipboard".to_string()),
                                                        Err(e) => self.toasts.push(ToastKind::Error, e),
                                                    }
                                                }
                                            }
                                        }
                                        KeyCode::Char('y') => {
                                            if let Some(res) = &c.last_result {
                                                if let Some(row) = res.records.get(c.result_selected_row) {
                                                    if let Some(val) = row.values.get(c.result_selected_col) {
                                                        match ClipboardManager::copy_cell(val) {
                                                            Ok(_) => self.toasts.push(ToastKind::Success, "copied cell to clipboard".to_string()),
                                                            Err(e) => self.toasts.push(ToastKind::Error, e),
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        KeyCode::Char('c') => {
                                            if !key.modifiers.contains(KeyModifiers::CONTROL) {
                                                if let Some(res) = &c.last_result {
                                                    if let Some(row) = res.records.get(c.result_selected_row) {
                                                        if let Some(val) = row.values.get(c.result_selected_col) {
                                                            match ClipboardManager::copy_cell(val) {
                                                                Ok(_) => self.toasts.push(ToastKind::Success, "copied cell to clipboard".to_string()),
                                                                Err(e) => self.toasts.push(ToastKind::Error, e),
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        KeyCode::Char('Y') => {
                                            if let Some(res) = &c.last_result {
                                                if let Some(row) = res.records.get(c.result_selected_row) {
                                                    match ClipboardManager::copy_row_json(&res.columns, row) {
                                                        Ok(_) => self.toasts.push(ToastKind::Success, "copied row as JSON to clipboard".to_string()),
                                                        Err(e) => self.toasts.push(ToastKind::Error, e),
                                                    }
                                                }
                                            }
                                        }
                                        _ => {}
                                    },
                                    }
                                },
                                WorkspaceTab::Erd(e) => match key.code {
                                    KeyCode::Up | KeyCode::Char('k') => e.scroll_up(),
                                    KeyCode::Down | KeyCode::Char('j') => e.scroll_down(),
                                    KeyCode::Left | KeyCode::Char('h') => e.scroll_left(),
                                    KeyCode::Right | KeyCode::Char('l') => e.scroll_right(),
                                    KeyCode::Char('0') => e.reset_view(),
                                    // Keyboard node selection (`.`/`,` next/prev);
                                    // Enter → DDL is handled in the event loop
                                    // (it needs `drv.definition().await`).
                                    KeyCode::Char('.') | KeyCode::Char('>') => e.select_next(),
                                    KeyCode::Char(',') | KeyCode::Char('<') => e.select_prev(),
                                    KeyCode::Char('+') | KeyCode::Char('=') => e.zoom_in(),
                                    KeyCode::Char('-') | KeyCode::Char('_') => e.zoom_out(),
                                    _ => {}
                                },
                            }
                        }

                        if key.code == KeyCode::Char('w') && !exp.tabs.is_empty() {
                            exp.tabs.remove(exp.active_tab_index);
                            if exp.active_tab_index >= exp.tabs.len() && exp.active_tab_index > 0 {
                                exp.active_tab_index -= 1;
                            }
                            // If the last tab was just closed, the workspace pane is now
                            // empty and there's nothing to interact with there. Bounce
                            // focus back to the tree so the user can pick another table
                            // without manually pressing Tab/h. We only do this when the
                            // tab list is fully empty — if other tabs remain, the user
                            // probably wants to keep working in the workspace.
                            if exp.tabs.is_empty() {
                                exp.focused_pane = FocusedPane::Tree;
                            }
                        }
                    }
                }
            }
            return;
        }

        // If form modal P5 is open, route typing to form
        if let Some(form) = &mut self.form_modal {
            match key.code {
                KeyCode::Esc => {
                    self.form_modal = None;
                }
                KeyCode::Tab | KeyCode::Down => {
                    form.focused_field = form.focused_field.next();
                }
                KeyCode::BackTab | KeyCode::Up => {
                    form.focused_field = form.focused_field.prev();
                }
                KeyCode::Enter => {
                    let new_cfg = form.to_connection_config();
                    if form.is_editing && let Some(orig) = &form.original_name {
                        if let Some(pos) = self.config.connections.iter().position(|c| &c.name == orig) {
                            self.config.connections[pos] = new_cfg;
                        }
                    } else {
                        self.config.connections.push(new_cfg);
                        self.selected_connection = self.config.connections.len().saturating_sub(1);
                    }

                    if let Err(e) = self.config.save(&self.config_path) {
                        self.toasts.push(ToastKind::Error, format!("failed to save config: {e:#}"));
                    } else {
                        self.toasts.push(ToastKind::Success, "connection saved".to_string());
                    }
                    self.form_modal = None;
                }
                KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if form.focused_field == FormField::Driver => {
                    form.driver = match form.driver {
                        crate::config::DriverType::MySql => {
                            if form.port == "3306" {
                                form.port = "5432".to_string();
                            }
                            crate::config::DriverType::Postgres
                        }
                        crate::config::DriverType::Postgres => {
                            if form.port == "5432" {
                                form.port = "3306".to_string();
                            }
                            crate::config::DriverType::MySql
                        }
                        _ => crate::config::DriverType::MySql,
                    };
                }
                KeyCode::Backspace => {
                    let target_str = match form.focused_field {
                        FormField::Name => &mut form.name,
                        FormField::Driver => return,
                        FormField::Host => &mut form.host,
                        FormField::Port => &mut form.port,
                        FormField::User => &mut form.user,
                        FormField::Password => &mut form.password,
                        FormField::Database => &mut form.database,
                    };
                    target_str.pop();
                }
                KeyCode::Char(c) => {
                    if !key.modifiers.contains(KeyModifiers::CONTROL) {
                        let target_str = match form.focused_field {
                            FormField::Name => &mut form.name,
                            FormField::Driver => return,
                            FormField::Host => &mut form.host,
                            FormField::Port => {
                                if c.is_ascii_digit() {
                                    &mut form.port
                                } else {
                                    return;
                                }
                            }
                            FormField::User => &mut form.user,
                            FormField::Password => &mut form.password,
                            FormField::Database => &mut form.database,
                        };
                        target_str.push(c);
                    }
                }
                _ => {}
            }
            return;
        }

        // Screen S1 (Picker) Keybindings
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('?') => self.help_open = true,
            KeyCode::Up | KeyCode::Char('k') => {
                if self.selected_connection > 0 {
                    self.selected_connection -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.config.connections.is_empty()
                    && self.selected_connection < self.config.connections.len() - 1
                {
                    self.selected_connection += 1;
                }
            }
            KeyCode::Char('a') => {
                self.form_modal = Some(ConnectionForm::new_empty());
            }
            KeyCode::Char('e') => {
                if let Some(cfg) = self.config.connections.get(self.selected_connection) {
                    self.form_modal = Some(ConnectionForm::from_config(cfg));
                }
            }
            KeyCode::Char('d') => {
                // Open the confirm-delete modal instead of deleting immediately.
                // Destructive operations need a confirmation step so a stray
                // `d` keystroke can't wipe a saved credential by accident.
                if !self.config.connections.is_empty()
                    && let Some(cfg) = self.config.connections.get(self.selected_connection).cloned()
                {
                    self.confirm_delete_modal = Some(ConfirmDeleteModal {
                        connection_name: cfg.name,
                        connection_index: self.selected_connection,
                    });
                }
            }
            _ => {}
        }
    }

    /// Execute the pending delete. Called from the confirm-delete modal's
    /// `Enter` handler in `run()`. Pulled out so the actual destructive logic
    /// (remove from vec, save config, adjust selection) lives in one place
    /// and can be unit-tested or reused if other entry points ever need it.
    fn execute_pending_delete(&mut self) {
        let Some(modal) = self.confirm_delete_modal.take() else {
            return;
        };
        // Defensive: re-bound the index in case the vec shrank from elsewhere.
        // (No other code path currently mutates the list while this modal is
        // open, but bounding protects against future refactors.)
        if modal.connection_index >= self.config.connections.len() {
            self.toasts.push(
                ToastKind::Error,
                format!("connection '{}' no longer exists", modal.connection_name),
            );
            return;
        }
        let removed = self.config.connections.remove(modal.connection_index);
        // Keep the cursor on a valid index after removal.
        if self.selected_connection >= self.config.connections.len()
            && self.selected_connection > 0
        {
            self.selected_connection -= 1;
        }
        match self.config.save(&self.config_path) {
            Ok(_) => {
                self.toasts.push(
                    ToastKind::Info,
                    format!("deleted '{}'", removed.name),
                );
            }
            Err(e) => {
                self.toasts.push(
                    ToastKind::Error,
                    format!("failed to delete: {e:#}"),
                );
            }
        }
    }

    fn draw(&mut self, f: &mut ratatui::Frame, spinner: &Spinner) {
        let area = f.area();
        let theme = &self.theme;

        // Paint background base. When a blocking form-modal test is in flight, dim
        // the entire frame so the rest of the UI reads as inactive.
        let base_style = if self.form_test_rx.is_some() {
            theme.dimmed()
        } else {
            theme.base()
        };
        f.render_widget(Block::default().style(base_style), area);

        if layout::too_small(area) {
            let notice = format!(
                "terminal too small (min {MIN_WIDTH}x{MIN_HEIGHT}), resize to continue"
            );
            let line_area = Rect {
                x: area.x,
                y: area.y + area.height / 2,
                width: area.width,
                height: 1,
            };
            f.render_widget(
                Paragraph::new(notice)
                    .style(theme.dim())
                    .alignment(Alignment::Center),
                line_area,
            );
            return;
        }

        let layout = layout::compute(area);

        // Header
        let config_str = format!("  [{}]", self.config_path.display());
        let header = Line::from(vec![
            Span::styled("◆ dbx", theme.accent()),
            Span::styled(format!("  v{}", env!("CARGO_PKG_VERSION")), theme.dim()),
            Span::styled(config_str, theme.dim()),
        ]);
        f.render_widget(Paragraph::new(header).style(theme.base()), layout.header);

        // Body
        match self.mode {
            ScreenMode::Picker => {
                self.picker_hit_area = Some(layout.body);
                picker::render_picker(
                    f,
                    layout.body,
                    &self.config.connections,
                    self.selected_connection,
                    theme,
                );
                let context_text = if self.connecting {
                    "Connecting..."
                } else {
                    "S1: Connection Picker"
                };
                statusbar::render(f, layout.status, context_text, &PICKER_HINTS, theme);
            }
            ScreenMode::Connected => {
                if let Some(exp) = &mut self.explorer_state {
                    explorer::render_explorer(f, layout.body, exp, theme);
                }
                statusbar::render(f, layout.status, "S2: Explorer", &EXPLORER_HINTS, theme);
            }
        }

        // Overlays
        if let Some(form) = &self.form_modal {
            // Draw a dim "scrim" over the whole frame BEFORE the modal so everything
            // outside the modal reads as inactive. The modal then sits on top with
            // its own style, drawing the eye to the in-flight test.
            if self.form_test_rx.is_some() {
                f.render_widget(Clear, area);
                f.render_widget(
                    Block::default().style(theme.dimmed()),
                    area,
                );
            }
            picker::render_form_modal(f, area, form, self.form_test_rx.is_some(), spinner, theme);
        }

        // Confirm-delete popup (destructive action). Drawn after form modal so
        // a stacked confirm-on-form case is unambiguous, but before toasts
        // so any "delete cancelled" / "deleted ..." toast overlays cleanly.
        if let Some(confirm) = &self.confirm_delete_modal {
            picker::render_confirm_delete_modal(f, area, confirm, theme);
        }

        if self.connecting {
            let spin_area = Rect {
                x: area.x + 2,
                y: area.y + 1,
                width: 30,
                height: 1,
            };
            spinner.render(f, spin_area, "Connecting to MySQL...", theme);
        }

        self.toasts.render(f, area, theme);

        if self.help_open {
            let (title, bindings) = match self.mode {
                ScreenMode::Picker => ("Connection Picker", &PICKER_HELP_BINDINGS[..]),
                ScreenMode::Connected => ("Database Explorer", &EXPLORER_HELP_BINDINGS[..]),
            };
            help::render(f, area, title, bindings, theme);
        }
    }
}

/// Restores the terminal on drop so every exit path leaves the shell usable.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let guard = Self;
        // Mouse capture lets the user click an ERD node (hit-tested in scene
        // space) to open its DDL. `EnableMouseCapture` + `EnterAlternateScreen`
        // in one execute keeps the two terminal modes atomic.
        execute!(io::stdout(), EnableMouseCapture, EnterAlternateScreen)?;
        Ok(guard)
    }

    fn restore() {
        let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        Self::restore();
    }
}

pub async fn run(cli_config: Option<PathBuf>) -> anyhow::Result<()> {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        TerminalGuard::restore();
        default_hook(info);
    }));

    let _guard = TerminalGuard::enter().context("failed to initialize terminal")?;

    tokio::spawn(async {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut sigterm) = signal(SignalKind::terminate()) {
            sigterm.recv().await;
            TerminalGuard::restore();
            std::process::exit(130);
        }
    });

    let config_path = AppConfig::default_path(cli_config.as_deref());
    let config = match AppConfig::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            // Never fail silently: a parse error that empties the connection
            // list can lead to a later save() overwriting the user's config.
            eprintln!("warning: failed to load config ({}): {e:#}", config_path.display());
            AppConfig::default()
        }
    };

    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))
        .context("failed to create terminal backend")?;

    let mut app = App::new(config, config_path);
    let mut spinner = Spinner::new();
    let mut last_tick = Instant::now();

    while !app.should_quit {
        terminal
            .draw(|f| app.draw(f, &spinner))
            .context("failed to draw frame")?;

        if event::poll(TICK_CAP).context("failed to poll terminal events")? {
            match event::read().context("failed to read terminal event")? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
            // P5: while a form-modal test is in flight, ignore ALL key input so the user
            // can't edit fields, change driver, or save mid-ping. The spinner + toast
            // communicate progress; result delivered via tick poll.
            if app.form_test_rx.is_some() {
                continue;
            }
            // Confirm-delete modal intercepts ONLY Enter / Esc. Any other key
            // is ignored so the user can't accidentally fire picker shortcuts
            // (j/k navigation, `a` add, `d` re-open modal, etc.) while the
            // destructive dialog is up. Enter executes, Esc cancels.
            if app.confirm_delete_modal.is_some() {
                match key.code {
                    KeyCode::Enter => {
                        app.execute_pending_delete();
                    }
                    KeyCode::Esc => {
                        app.confirm_delete_modal = None;
                        app.toasts.push(
                            ToastKind::Info,
                            "delete cancelled".to_string(),
                        );
                    }
                    _ => {}
                }
                continue;
            }
            // P5: Ctrl+T inside Connection Form Modal — handle BEFORE handle_key so the
            // sync form-modal handler doesn't swallow the key as plain 't' typing.
            // Non-blocking: spawn task, store receiver, return immediately. UI stays
            // responsive; spinner shows in the modal; result delivered via tick poll.
            if !app.help_open
                && app.form_modal.is_some()
                && app.form_test_rx.is_none()
                && key.modifiers.contains(KeyModifiers::CONTROL)
                && key.code == KeyCode::Char('t')
            {
                let cfg = app.form_modal.as_ref().map(|f| f.to_connection_config());
                if let Some(cfg) = cfg {
                    // Don't push an info "testing..." toast here — the in-modal spinner
                    // is the live feedback. Only the final success/error toast is shown,
                    // so the user sees a clear, single result instead of two quick
                    // back-to-back toasts.
                    if let Some(form) = app.form_modal.as_mut() {
                        form.last_test_result = None;
                    }
                    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Duration, String>>(1);
                    tokio::spawn(async move {
                        // Hard 60s ceiling: even if the network/server hangs forever
                        // (e.g. packet drop, firewall blackhole), the user gets a
                        // clear timeout toast instead of a permanently spinning modal.
                        let res = match tokio::time::timeout(
                            Duration::from_secs(60),
                            async {
                                match crate::driver::connect_driver(&cfg).await {
                                    Ok(driver) => driver.ping().await
                                        .map_err(|e| format!("ping failed: {e:#}")),
                                    Err(e) => Err(format!("connect failed: {e:#}")),
                                }
                            },
                        ).await {
                            Ok(inner) => inner,
                            Err(_) => Err("ping timed out after 60s".to_string()),
                        };
                        let _ = tx.send(res).await;
                    });
                    app.form_test_rx = Some(rx);
                }
                continue;
            }
            if matches!(app.mode, ScreenMode::Connected) {
                // Reconnect (Ctrl+R): re-establish the active connection when
                // it was dropped. Handled before the driver/explorer borrow so
                // we can replace `active_driver` freely.
                if app.active_connection.is_some()
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.code == KeyCode::Char('r')
                {
                    // Gate: don't fire while a modal owns the keyboard, or when
                    // the connection is already healthy (Ctrl+R should only
                    // rescue a dropped connection, not abort in-flight work).
                    let busy = app
                        .explorer_state
                        .as_ref()
                        .map(|e| {
                            e.export_modal.is_some()
                                || e.cell_edit_modal.is_some()
                                || e.insert_row_modal.is_some()
                                || e.sql_confirm_modal.is_some()
                                || e.object_search.is_some()
                                || e.import_csv_modal.is_some()
                                || e.ddl_popup.is_some()
                        })
                        .unwrap_or(false);
                    if busy {
                        app.toasts.push(ToastKind::Warning, "close the current popup before reconnecting".to_string());
                        continue;
                    }
                    if let Some(drv) = &app.active_driver
                        && drv.ping().await.is_ok()
                    {
                        app.toasts.push(ToastKind::Info, "connection is healthy — nothing to reconnect".to_string());
                        continue;
                    }

                    let cfg = app.active_connection.clone().unwrap();
                    app.toasts.push(ToastKind::Info, "reconnecting...".to_string());
                    // Timeout so a hung network can't freeze the TUI forever.
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        crate::driver::connect_driver(&cfg),
                    )
                    .await
                    {
                        Ok(Ok(new_drv)) => {
                            let info = new_drv.info();
                            // Refresh namespaces/tree but PRESERVE open tabs and
                            // buffers (don't rebuild ExplorerState from scratch).
                            let ns_result = new_drv.namespaces().await;
                            if let Some(exp) = &mut app.explorer_state {
                                match ns_result {
                                    Ok(ns) => {
                                        exp.namespaces = ns;
                                        exp.tables.clear();
                                        exp.rebuild_tree_nodes();
                                        exp.selected_tree_index = 0;
                                    }
                                    Err(e) => {
                                        app.toasts.push(
                                            ToastKind::Error,
                                            format!("reconnect: failed to list namespaces: {e:#}"),
                                        );
                                    }
                                }
                            }
                            app.active_driver = Some(new_drv);
                            app.toasts.push(
                                ToastKind::Success,
                                format!("reconnected: {} {}", info.name, info.server_version),
                            );
                        }
                        Ok(Err(e)) => {
                            app.toasts.push(ToastKind::Error, format!("reconnect failed: {e:#}"));
                        }
                        Err(_) => {
                            app.toasts.push(ToastKind::Error, "reconnect timed out after 10s".to_string());
                        }
                    }
                    continue;
                }
                if let (Some(drv), Some(exp)) = (&app.active_driver, &mut app.explorer_state) {
                    // 1. Export modal execution
                    if let Some(modal) = &exp.export_modal {
                        if key.code == KeyCode::Enter {
                            let format = modal.format;
                            let target_path = modal.target_path.clone();
                            let table_name = modal.default_table_name.clone();
                            let confirm_overwrite = modal.confirm_overwrite;

                            // Overwrite guard: if the target file already exists,
                            // ask the user to confirm before clobbering it. The
                            // first Enter flips the flag + warns; the second one
                            // (with the flag set) proceeds.
                            if !confirm_overwrite
                                && std::path::Path::new(&target_path).exists()
                            {
                                if let Some(m) = exp.export_modal.as_mut() {
                                    m.confirm_overwrite = true;
                                }
                                app.toasts.push(
                                    ToastKind::Warning,
                                    format!("{} already exists — press Enter again to overwrite", target_path),
                                );
                                continue;
                            }

                            // Extract current active dataset
                            let export_data: Option<(Vec<String>, Vec<crate::driver::Record>)> = if let Some(tab) = exp.active_tab() {
                                match tab {
                                    WorkspaceTab::Table(t) => Some((t.page.columns.clone(), t.page.records.clone())),
                                    WorkspaceTab::Console(c) => c.last_result.as_ref().map(|res| (res.columns.clone(), res.records.clone())),
                                    WorkspaceTab::Erd(_) => None,
                                }
                            } else {
                                None
                            };

                            if let Some((cols, recs)) = export_data {
                                let content = match format {
                                    ExportFormat::Csv => Ok(Exporter::format_csv(&cols, &recs)),
                                    ExportFormat::Json => Exporter::format_json(&cols, &recs)
                                        .map_err(|e| format!("JSON export error: {e:#}")),
                                    ExportFormat::SqlInsert => Ok(Exporter::format_sql_insert(&table_name, &cols, &recs)),
                                };

                                match content {
                                    Ok(text) => match Exporter::save_to_file(&target_path, &text) {
                                        Ok(saved_path) => {
                                            app.toasts.push(
                                                ToastKind::Success,
                                                format!("exported {} rows to {}", recs.len(), saved_path.display()),
                                            );
                                            exp.export_modal = None;
                                        }
                                        Err(e) => {
                                            app.toasts.push(ToastKind::Error, format!("failed to save export file: {e:#}"));
                                        }
                                    },
                                    Err(e) => {
                                        app.toasts.push(ToastKind::Error, e);
                                    }
                                }
                            } else {
                                app.toasts.push(ToastKind::Error, "no dataset available for export".to_string());
                                exp.export_modal = None;
                            }
                            continue;
                        }
                    }

                    // 2. Cell edit modal submit -> Generate SQL Preview & open confirmation modal
                    if let Some(edit) = &exp.cell_edit_modal {
                        if key.code == KeyCode::Enter {
                            let cref = edit.collection.clone();
                            let col_name = edit.column_name.clone();
                            let new_val_str = edit.text_buffer.clone();
                            let row_idx = edit.row_idx;

                            // Fetch table metadata to discover primary keys for exact targeting
                            let drv_clone = drv.clone();
                            let meta = drv_clone.collection_meta(&cref).await.ok();
                            let pk_cols: Vec<String> = meta
                                .as_ref()
                                .map(|m| {
                                    m.columns
                                        .iter()
                                        .filter(|c| c.is_primary_key)
                                        .map(|c| c.name.clone())
                                        .collect()
                                })
                                .unwrap_or_default();

                            // Build the WHERE clause via the shared helper so UPDATE / DELETE
                            // / future row-targeting statements all agree on NULL handling,
                            // identifier quoting, and PK-preferring semantics.
                            let driver_name = drv.info().name.clone();
                            let where_sql = if let Some(WorkspaceTab::Table(t)) = exp.active_tab() {
                                t.page
                                    .records
                                    .get(row_idx)
                                    .and_then(|row| {
                                        build_where_for_row(&t.page.columns, row, &pk_cols, &driver_name)
                                    })
                                    .unwrap_or_else(|| "1 = 1".to_string())
                            } else {
                                "1 = 1".to_string()
                            };

                            let q_ns = quote_ident(&cref.namespace.0, &driver_name);
                            let q_tbl = quote_ident(&cref.name, &driver_name);
                            let q_col = quote_ident(&col_name, &driver_name);

                            // The sentinel is the only way the buffer can carry
                            // "set to NULL" intent; literal user-typed text "NULL"
                            // stays a regular string.
                            let assignment = render_buffer_sql(&new_val_str);

                            // MySQL needs `LIMIT 1` on single-row UPDATE when the WHERE
                            // doesn't include a unique key. Other backends don't.
                            let suffix = single_row_suffix(&driver_name);

                            let sql = format!(
                                "UPDATE {q_ns}.{q_tbl} SET {q_col} = {assignment} WHERE {where_sql}{suffix};"
                            );

                            exp.cell_edit_modal = None;
                            exp.sql_confirm_modal = Some(crate::ui::screens::explorer::SqlConfirmModalState {
                                collection: cref,
                                sql_query: sql,
                                row_idx,
                            });
                            continue;
                        }
                    }

                    // 2b. INSERT-row modal submit. Char / Tab / Esc / Ctrl+N
                    //     are all routed in `handle_key` (sync), but the
                    //     submit step needs `drv.execute().await` and a page
                    //     refresh, so it lives here in the async event loop.
                    //     We don't go through a SQL-confirm modal because the
                    //     statement is additive (new row) and the user has
                    //     already reviewed every value field by field.
                    if let Some(m) = &exp.insert_row_modal {
                        if key.code == KeyCode::Enter {
                            // Snapshot the modal state into a plain Vec so
                            // the borrow on `exp.insert_row_modal` ends
                            // before we touch `drv` and `exp.active_tab_mut`.
                            let mut fields: Vec<(String, Option<String>)> =
                                Vec::with_capacity(m.column_meta.len());
                            for (i, col) in m.column_meta.iter().enumerate() {
                                let buf = m.field_buffers.get(i).cloned().flatten();
                                fields.push((col.name.clone(), buf));
                            }
                            let cref = m.collection.clone();

                            let driver_name = drv.info().name.clone();
                            match build_insert_sql(&cref, &fields, &driver_name) {
                                Some(sql) => {
                                    app.toasts.push(
                                        ToastKind::Info,
                                        "executing INSERT...".to_string(),
                                    );
                                    let drv_clone = drv.clone();
                                    let cref_clone = cref.clone();
                                    match drv_clone.execute(&cref.namespace, &sql).await {
                                        Ok(res) => {
                                            app.toasts.push(
                                                ToastKind::Success,
                                                format!(
                                                    "inserted 1 row (rows affected: {})",
                                                    res.rows_affected.max(1)
                                                ),
                                            );
                                            exp.insert_row_modal = None;
                                            // Refresh the current page so the
                                            // new row shows up immediately if
                                            // it falls within the visible
                                            // window. If it doesn't (e.g.
                                            // sorted descending), the user can
                                            // navigate manually.
                                            refresh_table_page(exp, drv, &cref_clone).await;
                                        }
                                        Err(e) => {
                                            app.toasts.push(
                                                ToastKind::Error,
                                                format!("INSERT failed: {e:#}"),
                                            );
                                            // Keep modal open so the user can
                                            // fix the offending field and
                                            // retry instead of starting over.
                                        }
                                    }
                                }
                                None => {
                                    app.toasts.push(
                                        ToastKind::Error,
                                        "no values provided — fill at least one field before inserting"
                                            .to_string(),
                                    );
                                }
                            }
                            continue;
                        }
                    }

                    // 3. SQL Confirmation modal -> Execute UPDATE and refresh data grid
                    if let Some(confirm) = &exp.sql_confirm_modal {
                        if key.code == KeyCode::Enter {
                            let cref = confirm.collection.clone();
                            let sql = confirm.sql_query.clone();
                            let drv_clone = drv.clone();

                            app.toasts.push(ToastKind::Info, "executing confirmed statement...".to_string());
                            // Execute each split statement (same as the console),
                            // so a confirmed destructive script runs all of it.
                            let statements = crate::ui::screens::query::split_statements(&sql);
                            let mut affected = 0u64;
                            let mut exec_err: Option<String> = None;
                            for stmt in &statements {
                                if crate::ui::screens::query::is_comment_only(stmt) {
                                    continue;
                                }
                                match drv_clone.execute(&cref.namespace, stmt).await {
                                    Ok(res) => affected += res.rows_affected,
                                    Err(e) => {
                                        exec_err = Some(format!("{e:#}"));
                                        break;
                                    }
                                }
                            }
                            match exec_err {
                                None => {
                                    app.toasts.push(
                                        ToastKind::Success,
                                        format!("executed successfully (rows affected: {affected})"),
                                    );
                                    // Confirmed statements (destructive guard / row
                                    // delete) count as history too.
                                    if let Some(conn) = &app.active_connection_name {
                                        app.config.push_history(conn, &sql);
                                    }
                                    exp.sql_confirm_modal = None;

                                    // Refresh active table tab
                                    refresh_table_page(exp, drv, &cref).await;
                                }
                                Some(e) => {
                                    app.toasts.push(ToastKind::Error, format!("UPDATE failed: {e}"));
                                    exp.sql_confirm_modal = None;
                                }
                            }
                            continue;
                        }
                    }

                    // Object search (Ctrl+T): open the overlay and fetch every
                    // collection across all namespaces into the result list.
                    if exp.object_search.is_none()
                        && exp.ddl_popup.is_none()
                        && exp.export_modal.is_none()
                        && exp.cell_edit_modal.is_none()
                        && exp.insert_row_modal.is_none()
                        && exp.sql_confirm_modal.is_none()
                        && exp.import_csv_modal.is_none()
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('t')
                    {
                        let mut results = Vec::new();
                        for ns in exp.namespaces.clone() {
                            // Fetch all four object types in parallel per schema.
                            let (tables, views, routines, seqs) = tokio::join!(
                                drv.collections(&ns),
                                drv.list_views(&ns),
                                drv.list_routines(&ns),
                                drv.list_sequences(&ns),
                            );
                            let mut push = |list: Result<Vec<crate::driver::Collection>, _>,
                                            kind: crate::ui::screens::explorer::SearchKind| {
                                if let Ok(objs) = list {
                                    for o in objs {
                                        results.push((
                                            crate::driver::CollectionRef {
                                                namespace: ns.clone(),
                                                name: o.name,
                                            },
                                            kind,
                                        ));
                                    }
                                }
                            };
                            push(tables, crate::ui::screens::explorer::SearchKind::Table);
                            push(views, crate::ui::screens::explorer::SearchKind::View);
                            push(routines, crate::ui::screens::explorer::SearchKind::Routine);
                            push(seqs, crate::ui::screens::explorer::SearchKind::Sequence);
                        }
                        exp.object_search = Some(
                            crate::ui::screens::explorer::ObjectSearchState {
                                query: String::new(),
                                results,
                                selected: 0,
                            },
                        );
                        continue;
                    }

                    // Enter in the object search → open the highlighted object
                    // according to its kind.
                    if exp.object_search.is_some() && key.code == KeyCode::Enter {
                        let hit = {
                            let s = exp.object_search.as_ref().unwrap();
                            let filtered: Vec<&(
                                crate::driver::CollectionRef,
                                crate::ui::screens::explorer::SearchKind,
                            )> = s
                                .results
                                .iter()
                                .filter(|(r, _)| {
                                    r.name.contains(&s.query)
                                        || r.namespace.0.contains(&s.query)
                                })
                                .collect();
                            filtered.get(s.selected).map(|(r, k)| (r.clone(), *k))
                        };
                        exp.object_search = None;
                        if let Some((cref, kind)) = hit {
                            use crate::ui::screens::explorer::SearchKind;
                            match kind {
                                SearchKind::Table | SearchKind::View => {
                                    if let Err(e) = open_collection_tab(
                                        exp,
                                        drv,
                                        cref,
                                        app.config.effective_page_size(),
                                        kind == SearchKind::View,
                                    )
                                    .await
                                    {
                                        app.toasts.push(ToastKind::Error, format!("failed to open: {e}"));
                                    }
                                }
                                SearchKind::Routine => {
                                    match drv.routine_definition(&cref).await {
                                        Ok(ddl) => exp.ddl_popup = Some((cref, ddl)),
                                        Err(e) => app.toasts.push(
                                            ToastKind::Error,
                                            format!("failed to fetch routine: {e:#}"),
                                        ),
                                    }
                                }
                                SearchKind::Sequence => {
                                    let stub = format!("SEQUENCE {}.{}", cref.namespace, cref.name);
                                    exp.ddl_popup = Some((cref, stub));
                                }
                            }
                        }
                        continue;
                    }

                    // CSV import: Enter on a PARSED file inserts the rows. The
                    // first Enter (read the file) must fall through to
                    // handle_key — gating on `parsed` keeps them distinct.
                    if exp
                        .import_csv_modal
                        .as_ref()
                        .is_some_and(|m| m.parsed)
                        && key.code == KeyCode::Enter
                    {
                        let snapshot = {
                            let m = exp.import_csv_modal.as_ref().unwrap();
                            Some((
                                m.rows.clone(),
                                exp.active_tab().and_then(|t| match t {
                                        WorkspaceTab::Table(t) => Some(t.collection.clone()),
                                        _ => None,
                                    }),
                                ))
                        };
                        if let Some((rows, Some(cref))) = snapshot {
                            // Need a header row + at least one data row.
                            if rows.len() < 2 || rows[0].is_empty() {
                                app.toasts.push(
                                    ToastKind::Error,
                                    "CSV has no data rows (need a header + at least one row)".to_string(),
                                );
                                exp.import_csv_modal = None;
                                continue;
                            }
                            let drv_clone = drv.clone();
                            let driver_name = drv.info().name.clone();
                            let headers = rows.first().cloned().unwrap_or_default();
                            let mut inserted = 0u64;
                            let mut fail: Option<String> = None;
                            for row in rows.iter().skip(1) {
                                if row.len() != headers.len() {
                                    fail = Some(format!(
                                        "column count mismatch ({} vs {}) at row {}",
                                        row.len(),
                                        headers.len(),
                                        inserted + 2
                                    ));
                                    break;
                                }
                                let sql = build_insert_literal_sql(
                                    &cref.name,
                                    &headers,
                                    row,
                                    &driver_name,
                                );
                                match drv_clone.execute(&cref.namespace, &sql).await {
                                    Ok(_) => inserted += 1,
                                    Err(e) => {
                                        fail = Some(format!("{e:#}"));
                                        break;
                                    }
                                }
                            }
                            exp.import_csv_modal = None;
                            match fail {
                                Some(e) => {
                                    app.toasts.push(
                                        ToastKind::Error,
                                        format!("import failed after {inserted} rows: {e}"),
                                    );
                                }
                                None => {
                                    app.toasts.push(
                                        ToastKind::Success,
                                        format!("imported {inserted} rows"),
                                    );
                                    // Refresh the table tab.
                                    refresh_table_page(exp, drv, &cref).await;
                                }
                            }
                        } else {
                            exp.import_csv_modal = None;
                        }
                        continue;
                    }

                    // Enter on an ERD tab with a keyboard-selected node
                    // (`.`/`,` moves the selection) opens its DDL — the same
                    // path as a mouse click on the node.
                    if exp.ddl_popup.is_none()
                        && exp.focused_pane == FocusedPane::Workspace
                        && key.code == KeyCode::Enter
                    {
                        let cref: Option<crate::driver::CollectionRef> = (|| {
                            let WorkspaceTab::Erd(erd) = exp.active_tab()? else {
                                return None;
                            };
                            let idx = erd.selected_node?;
                            let node = &erd.scene.as_ref()?.scene.nodes[idx];
                            Some(crate::driver::CollectionRef {
                                namespace: erd.namespace.clone(),
                                name: node.id.clone(),
                            })
                        })();
                        if let Some(cref) = cref {
                            let drv_clone = drv.clone();
                            match drv_clone.definition(&cref).await {
                                Ok(ddl) => {
                                    exp.ddl_popup = Some((cref, ddl));
                                }
                                Err(e) => {
                                    app.toasts.push(
                                        ToastKind::Error,
                                        format!("failed to fetch DDL: {e:#}"),
                                    );
                                }
                            }
                            continue;
                        }
                    }

                    if exp.ddl_popup.is_none() && exp.focused_pane == FocusedPane::Tree {
                        match key.code {
                            KeyCode::Char(' ') | KeyCode::Enter => {
                                if let Some(node) = exp.selected_node() {
                                    match &node.kind {
                                        TreeNodeKind::Database(ns) => {
                                            let ns_clone = ns.clone();
                                            let is_expanded = node.is_expanded;
                                            // Fetch every object type the first time this
                                            // schema is expanded — and retry on re-expand
                                            // (no permanent gate on `tables`), so a transient
                                            // view/routine listing failure is re-attempted.
                                            if !is_expanded {
                                                if let Some(n) = exp.tree_nodes.iter_mut().find(|n| match &n.kind {
                                                    TreeNodeKind::Database(d) => d == &ns_clone,
                                                    _ => false,
                                                }) {
                                                    n.is_loading = true;
                                                }
                                                let drv_clone = drv.clone();
                                                // 4 round trips in parallel instead of serial.
                                                let (tables, views, routines, seqs) = tokio::join!(
                                                    drv_clone.collections(&ns_clone),
                                                    drv_clone.list_views(&ns_clone),
                                                    drv_clone.list_routines(&ns_clone),
                                                    drv_clone.list_sequences(&ns_clone),
                                                );
                                                if let Ok(t) = tables {
                                                    exp.tables.insert(ns_clone.0.clone(), t);
                                                }
                                                if let Ok(v) = views {
                                                    exp.views.insert(ns_clone.0.clone(), v);
                                                }
                                                if let Ok(r) = routines {
                                                    exp.routines.insert(ns_clone.0.clone(), r);
                                                }
                                                if let Ok(s) = seqs {
                                                    exp.sequences.insert(ns_clone.0.clone(), s);
                                                }
                                                if let Some(n) = exp.tree_nodes.iter_mut().find(|n| match &n.kind {
                                                    TreeNodeKind::Database(d) => d == &ns_clone,
                                                    _ => false,
                                                }) {
                                                    n.is_loading = false;
                                                }
                                            }
                                            if let Some(n) = exp.tree_nodes.iter_mut().find(|n| match &n.kind {
                                                TreeNodeKind::Database(d) => d == &ns_clone,
                                                _ => false,
                                            }) {
                                                n.is_expanded = !is_expanded;
                                            }
                                            exp.rebuild_tree_nodes();

                                            // Restore selection index to the toggled database item
                                            if let Some(new_idx) = exp.tree_nodes.iter().position(|n| match &n.kind {
                                                TreeNodeKind::Database(d) => d == &ns_clone,
                                                _ => false,
                                            }) {
                                                exp.selected_tree_index = new_idx;
                                            }
                                        }
                                        TreeNodeKind::View(cref) => {
                                            // A view opens like a table (SELECT *).
                                            if let Err(e) = open_collection_tab(
                                                exp,
                                                drv,
                                                cref.clone(),
                                                app.config.effective_page_size(),
                                                true, // views are read-only
                                            )
                                            .await
                                            {
                                                app.toasts.push(ToastKind::Error, format!("failed to load view: {e}"));
                                            }
                                        }
                                        TreeNodeKind::Routine(cref) => {
                                            let cref_clone = cref.clone();
                                            let drv_clone = drv.clone();
                                            match drv_clone.routine_definition(&cref_clone).await {
                                                Ok(ddl) => {
                                                    exp.ddl_popup = Some((cref_clone, ddl));
                                                }
                                                Err(e) => {
                                                    app.toasts.push(ToastKind::Error, format!("failed to fetch routine: {e:#}"));
                                                }
                                            }
                                        }
                                        TreeNodeKind::Sequence(cref) => {
                                            // Sequences have no body to render — show a stub so the
                                            // user at least sees the object resolved.
                                            exp.ddl_popup = Some((
                                                cref.clone(),
                                                format!("SEQUENCE {}.{}", cref.namespace, cref.name),
                                            ));
                                        }
                                        TreeNodeKind::Table(cref, _) => {
                                            if let Err(e) = open_collection_tab(
                                                exp,
                                                drv,
                                                cref.clone(),
                                                app.config.effective_page_size(),
                                                false,
                                            )
                                            .await
                                            {
                                                app.toasts.push(ToastKind::Error, format!("failed to load table: {e}"));
                                            }
                                        }
                                    }
                                }
                            }
                            KeyCode::F(1) => {
                                if let Some(node) = exp.selected_node() {
                                    let cref = match &node.kind {
                                        TreeNodeKind::Table(cref, _)
                                        | TreeNodeKind::View(cref)
                                        | TreeNodeKind::Routine(cref)
                                        | TreeNodeKind::Sequence(cref) => cref.clone(),
                                        TreeNodeKind::Database(_) => continue,
                                    };
                                    let drv_clone = drv.clone();
                                    let def: Result<String, String> = match &node.kind {
                                        TreeNodeKind::Routine(_) => drv_clone
                                            .routine_definition(&cref)
                                            .await
                                            .map_err(|e| format!("{e:#}")),
                                        TreeNodeKind::Sequence(_) => Ok(format!(
                                            "SEQUENCE {}.{}",
                                            cref.namespace, cref.name
                                        )),
                                        // Table → CREATE TABLE; View → synthesised
                                        // from metadata (driver doesn't expose
                                        // CREATE VIEW yet).
                                        _ => drv_clone
                                            .definition(&cref)
                                            .await
                                            .map_err(|e| format!("{e:#}")),
                                    };
                                    match def {
                                        Ok(ddl) => exp.ddl_popup = Some((cref, ddl)),
                                        Err(e) => {
                                            app.toasts.push(ToastKind::Error, format!("failed to fetch DDL: {e}"));
                                        }
                                    }
                                }
                            }
                            KeyCode::Char('g') | KeyCode::Char('G') => {
                                // Roadmap M3.6: ERD entry is capability-gated.
                                // If the active driver can't lay out an ERD,
                                // tell the user instead of opening an empty tab.
                                if !exp
                                    .driver_capabilities
                                    .contains(crate::driver::Capabilities::ERD)
                                {
                                    app.toasts.push(
                                        ToastKind::Warning,
                                        "this driver does not support ERD diagrams".to_string(),
                                    );
                                    continue;
                                }
                                if let Some(node) = exp.selected_node() {
                                    let target_ns = match &node.kind {
                                        TreeNodeKind::Database(ns) => ns.clone(),
                                        TreeNodeKind::Table(cref, _) => cref.namespace.clone(),
                                        TreeNodeKind::View(cref) => cref.namespace.clone(),
                                        TreeNodeKind::Routine(cref) => cref.namespace.clone(),
                                        TreeNodeKind::Sequence(cref) => cref.namespace.clone(),
                                    };

                                    if let Some(existing_idx) = exp.tabs.iter().position(|t| match t {
                                        WorkspaceTab::Erd(erd) => erd.namespace == target_ns,
                                        _ => false,
                                    }) {
                                        exp.active_tab_index = existing_idx;
                                        exp.focused_pane = FocusedPane::Workspace;
                                    } else {
                                        app.toasts.push(ToastKind::Info, format!("generating ERD for '{}'...", target_ns.0));
                                        let drv_clone = drv.clone();
                                        let mut erd_tab = crate::ui::screens::erd::ErdTab::new(target_ns.clone());
                                        if let Ok(tbls) = drv_clone.collections(&target_ns).await {
                                            let mut metas = Vec::new();
                                            for tbl in tbls {
                                                let cref = crate::driver::CollectionRef {
                                                    namespace: target_ns.clone(),
                                                    name: tbl.name,
                                                };
                                                if let Ok(meta) = drv_clone.collection_meta(&cref).await {
                                                    metas.push(meta);
                                                }
                                            }
                                            erd_tab.generate_from_meta(&metas);
                                            exp.tabs.push(WorkspaceTab::Erd(erd_tab));
                                            exp.active_tab_index = exp.tabs.len().saturating_sub(1);
                                            exp.focused_pane = FocusedPane::Workspace;
                                            app.toasts.push(ToastKind::Success, "ERD generated".to_string());
                                        }
                                    }
                                }
                            }
                            _ => {
                                app.handle_key(key);
                            }
                        }
                    } else if exp.ddl_popup.is_none() && exp.focused_pane == FocusedPane::Workspace {
                        let is_ctrl_enter = key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Enter;
                        let is_f5 = key.code == KeyCode::F(5);

                        // Don't run the query under a history/favorites popup —
                        // Ctrl+Enter is muscle memory for "run" but the popup
                        // owns the keystroke until Esc/Enter dismisses it.
                        if (is_ctrl_enter || is_f5)
                            && exp.active_tab().is_some_and(|t| match t {
                                WorkspaceTab::Console(c) => c.popup.is_none(),
                                _ => true,
                            })
                        {
                            // Find active database from selected tree node or fallback to first namespace
                            let active_ns = if let Some(node) = exp.selected_node() {
                                match &node.kind {
                                    TreeNodeKind::Database(ns) => ns.clone(),
                                    TreeNodeKind::Table(cref, _) => cref.namespace.clone(),
                                    TreeNodeKind::View(cref) => cref.namespace.clone(),
                                    TreeNodeKind::Routine(cref) => cref.namespace.clone(),
                                    TreeNodeKind::Sequence(cref) => cref.namespace.clone(),
                                }
                            } else {
                                exp.namespaces.first().cloned().unwrap_or(crate::driver::Namespace("mysql".to_string()))
                            };

                            // Snapshot the console text before mutating state so
                            // we can decide whether the statement is destructive
                            // (M2.10 guard) without holding a borrow on the tab.
                            let query_text = if let Some(WorkspaceTab::Console(c)) = exp.active_tab() {
                                c.text()
                            } else {
                                continue;
                            };

                            // Destructive statement guard: DROP / TRUNCATE /
                            // DELETE-without-WHERE must be confirmed before they
                            // run. Reuses the SQL-confirm modal (Enter = execute,
                            // Esc = cancel) with a placeholder collection — the
                            // confirm path only needs the namespace.
                            if is_destructive_statement(&query_text) {
                                app.toasts.push(
                                    ToastKind::Warning,
                                    "destructive statement detected — confirm to execute".to_string(),
                                );
                                exp.sql_confirm_modal = Some(
                                    crate::ui::screens::explorer::SqlConfirmModalState {
                                        collection: crate::driver::CollectionRef {
                                            namespace: active_ns,
                                            name: "(console)".to_string(),
                                        },
                                        sql_query: query_text,
                                        row_idx: 0,
                                    },
                                );
                                continue;
                            }

                            if let Some(WorkspaceTab::Console(console)) = exp.active_tab_mut() {
                                let drv_clone = drv.clone();
                                console.is_executing = true;
                                console.execution_error = None;

                                // Split on `;` and run each statement, collecting
                                // every result set (multi-statement support).
                                let statements = crate::ui::screens::query::split_statements(&query_text);
                                // Empty input is an error, not a silent success.
                                if statements.is_empty() {
                                    console.is_executing = false;
                                    console.execution_error = Some("empty query — nothing to execute".to_string());
                                    console.results = Vec::new();
                                    console.last_result = None;
                                    console.active_result = 0;
                                    app.toasts.push(ToastKind::Warning, "empty query — nothing to execute".to_string());
                                    continue;
                                }
                                // Stateful scripts (SET @x, transactions) run on
                                // separate pooled connections, so session state
                                // doesn't persist — warn up front.
                                if statements.len() > 1 {
                                    app.toasts.push(
                                        ToastKind::Warning,
                                        "multi-statement: SET @x / BEGIN..COMMIT won't persist between statements".to_string(),
                                    );
                                }
                                let mut results = Vec::new();
                                let mut failed: Option<String> = None;
                                for stmt in &statements {
                                    if crate::ui::screens::query::is_comment_only(stmt) {
                                        continue;
                                    }
                                    match drv_clone.execute(&active_ns, stmt).await {
                                        Ok(r) => results.push(r),
                                        Err(e) => {
                                            failed = Some(format!("{e:#}"));
                                            break;
                                        }
                                    }
                                }
                                console.is_executing = false;
                                match failed {
                                    None => {
                                        console.results = results.clone();
                                        console.active_result = 0;
                                        console.last_result = results.into_iter().next();
                                        console.execution_error = None;
                                        console.result_selected_row = 0;
                                        console.result_selected_col = 0;
                                        console.result_scroll_x = 0;
                                        // Record to this connection's query history.
                                        // Not persisted per-query (re-serializing the
                                        // whole config on every run is too costly);
                                        // saved once when the app exits.
                                        if let Some(conn) = &app.active_connection_name {
                                            app.config.push_history(conn, &query_text);
                                        }
                                        app.toasts.push(ToastKind::Success, "query executed".to_string());
                                    }
                                    Some(e) => {
                                        // Don't leave stale results from the previous
                                        // run visible under the error.
                                        console.execution_error = Some(e);
                                        console.results = Vec::new();
                                        console.last_result = None;
                                        console.active_result = 0;
                                        app.toasts.push(ToastKind::Error, "query failed".to_string());
                                    }
                                }
                            }
                        } else {
                            match key.code {
                                // Backspace on a selected row → confirm + DELETE the row.
                                // We need the active driver for the driver-name sniff
                                // (`quote_ident` + `single_row_suffix`) so this lives
                                // here in the async event loop, not in `handle_key`.
                                KeyCode::Backspace => {
                                    let can_edit = exp.driver_capabilities.contains(crate::driver::Capabilities::EDIT_DATA);
                                    if !can_edit {
                                        app.toasts.push(ToastKind::Warning, "active driver does not support editing data".to_string());
                                        continue;
                                    }
                                    let read_only = exp
                                        .active_tab()
                                        .map(|t| match t {
                                            WorkspaceTab::Table(t) => t.read_only,
                                            _ => false,
                                        })
                                        .unwrap_or(false);
                                    if read_only {
                                        app.toasts.push(ToastKind::Warning, "this view is read-only".to_string());
                                        continue;
                                    }
                                    // Build WHERE from PK (preferred) or all columns.
                                    // Pull everything we need from immutable borrows,
                                    // then drop them before opening the confirm modal
                                    // mutably.
                                    let delete_plan: Option<(crate::driver::CollectionRef, String)> = (|| {
                                        let tab = exp.active_tab()?;
                                        let WorkspaceTab::Table(t) = tab else { return None; };
                                        let pk_cols: Vec<String> = t
                                            .column_meta
                                            .iter()
                                            .filter(|c| c.is_primary_key)
                                            .map(|c| c.name.clone())
                                            .collect();
                                        let row = t.page.records.get(t.selected_row)?;
                                        let driver_name = drv.info().name.clone();
                                        let where_sql = build_where_for_row(
                                            &t.page.columns,
                                            row,
                                            &pk_cols,
                                            &driver_name,
                                        )?;
                                        let q_ns = quote_ident(&t.collection.namespace.0, &driver_name);
                                        let q_tbl = quote_ident(&t.collection.name, &driver_name);
                                        let suffix = single_row_suffix(&driver_name);
                                        let sql = format!(
                                            "DELETE FROM {q_ns}.{q_tbl} WHERE {where_sql}{suffix};"
                                        );
                                        Some((t.collection.clone(), sql))
                                    })();
                                    match delete_plan {
                                        Some((cref, sql)) => {
                                            exp.sql_confirm_modal = Some(
                                                crate::ui::screens::explorer::SqlConfirmModalState {
                                                    collection: cref,
                                                    sql_query: sql,
                                                    row_idx: exp
                                                        .active_tab()
                                                        .and_then(|t| match t {
                                                            WorkspaceTab::Table(tt) => Some(tt.selected_row),
                                                            _ => None,
                                                        })
                                                        .unwrap_or(0),
                                                },
                                            );
                                        }
                                        None => {
                                            app.toasts.push(
                                                ToastKind::Error,
                                                "cannot delete: no row selected or no columns to identify it".to_string(),
                                            );
                                        }
                                    }
                                }
                                // `i` → open the INSERT-row modal. All fields start
                                // in the "skip" state so the user can opt in to
                                // providing values one column at a time.
                                KeyCode::Char('i') | KeyCode::Char('I')
                                    if !key.modifiers.contains(KeyModifiers::CONTROL) =>
                                {
                                    let can_edit = exp.driver_capabilities.contains(crate::driver::Capabilities::EDIT_DATA);
                                    if !can_edit {
                                        app.toasts.push(ToastKind::Warning, "active driver does not support editing data".to_string());
                                        continue;
                                    }
                                    let read_only = exp
                                        .active_tab()
                                        .map(|t| match t {
                                            WorkspaceTab::Table(t) => t.read_only,
                                            _ => false,
                                        })
                                        .unwrap_or(false);
                                    if read_only {
                                        app.toasts.push(ToastKind::Warning, "this view is read-only".to_string());
                                        continue;
                                    }
                                    // Snapshot the active table's column metadata.
                                    let open: Option<crate::ui::screens::explorer::InsertRowModalState> = exp
                                        .active_tab()
                                        .and_then(|tab| match tab {
                                            WorkspaceTab::Table(t) => {
                                                if t.column_meta.is_empty() {
                                                    None
                                                } else {
                                                    let n = t.column_meta.len();
                                                    Some(crate::ui::screens::explorer::InsertRowModalState {
                                                        collection: t.collection.clone(),
                                                        field_buffers: vec![None; n],
                                                        column_meta: t.column_meta.clone(),
                                                        focused_field: 0,
                                                    })
                                                }
                                            }
                                            _ => None,
                                        });
                                    match open {
                                        Some(m) => {
                                            exp.insert_row_modal = Some(m);
                                        }
                                        None => {
                                            app.toasts.push(
                                                ToastKind::Error,
                                                "no column metadata available for this table".to_string(),
                                            );
                                        }
                                    }
                                }
                                KeyCode::Char('c') => {
                                    let count = exp.tabs.iter().filter(|t| matches!(t, WorkspaceTab::Console(_))).count() + 1;
                                    let console_title = format!("console_{count}.sql");
                                    exp.tabs.push(WorkspaceTab::Console(QueryConsole::new(console_title, None)));
                                    exp.active_tab_index = exp.tabs.len().saturating_sub(1);
                                    exp.focused_pane = FocusedPane::Workspace;
                                    app.toasts.push(ToastKind::Info, "opened new SQL console".to_string());
                                }
                                KeyCode::Char('n') => {
                                    if let Some(WorkspaceTab::Table(tab)) = exp.active_tab_mut() {
                                        let mut next_page = Page::default();
                                        next_page.offset = (tab.page.page + 1) * tab.page.page_size;
                                        let drv_clone = drv.clone();
                                        let cref_clone = tab.collection.clone();
                                        if let Ok(new_rec_page) = drv_clone.records(&cref_clone, next_page).await {
                                            if !new_rec_page.records.is_empty() {
                                                tab.page = new_rec_page;
                                                tab.selected_row = 0;
                                            }
                                        }
                                    }
                                }
                                KeyCode::Char('p') => {
                                    if let Some(WorkspaceTab::Table(tab)) = exp.active_tab_mut() && tab.page.page > 0 {
                                        let mut prev_page = Page::default();
                                        prev_page.offset = (tab.page.page - 1) * tab.page.page_size;
                                        let drv_clone = drv.clone();
                                        let cref_clone = tab.collection.clone();
                                        if let Ok(new_rec_page) = drv_clone.records(&cref_clone, prev_page).await {
                                            tab.page = new_rec_page;
                                            tab.selected_row = 0;
                                        }
                                    }
                                }
                                _ => {
                                    app.handle_key(key);
                                }
                            }
                        }
                    } else {
                        app.handle_key(key);
                    }
                }
            } else {
                // Test connection 't' key handled with async tokio spawn
                if !app.help_open && app.form_modal.is_none() && key.code == KeyCode::Char('t') {
                    if let Some(cfg) = app.config.connections.get(app.selected_connection).cloned() {
                        app.toasts.push(ToastKind::Info, format!("testing ping to '{}'...", cfg.name));
                        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<Duration, String>>(1);
                        tokio::spawn(async move {
                            match crate::driver::connect_driver(&cfg).await {
                                Ok(driver) => match driver.ping().await {
                                    Ok(dur) => { let _ = tx.send(Ok(dur)).await; }
                                    Err(e) => { let _ = tx.send(Err(format!("ping failed: {e:#}"))).await; }
                                },
                                Err(e) => { let _ = tx.send(Err(format!("connect failed: {e:#}"))).await; }
                            }
                        });

                        match tokio::time::timeout(Duration::from_millis(3000), rx.recv()).await {
                            Ok(Some(res)) => match res {
                                Ok(dur) => app.toasts.push(ToastKind::Success, format!("ping ok ({:.2?})", dur)),
                                Err(err) => app.toasts.push(ToastKind::Error, err),
                            },
                            Ok(None) => {
                                app.toasts.push(ToastKind::Error, "ping task ended unexpectedly".to_string());
                            }
                            Err(_) => {
                                app.toasts.push(ToastKind::Error, "ping timed out (3s)".to_string());
                            }
                        }
                    }
                } else if !app.help_open && app.form_modal.is_none() && key.code == KeyCode::Enter {
                    if let Some(cfg) = app.config.connections.get(app.selected_connection).cloned() {
                        app.connecting = true;
                        app.toasts.push(ToastKind::Info, format!("connecting to '{}'...", cfg.name));
                        match crate::driver::connect_driver(&cfg).await {
                            Ok(drv) => {
                                let info = drv.info();
                                let capabilities = drv.capabilities();
                                let namespaces = drv.namespaces().await.unwrap_or_default();
                                app.explorer_state = Some(ExplorerState::new(namespaces, capabilities));
                                app.active_driver = Some(drv);
                                app.active_connection_name = Some(cfg.name.clone());
                                app.active_connection = Some(cfg.clone());
                                app.mode = ScreenMode::Connected;
                                app.toasts.push(ToastKind::Success, format!("connected: {} {}", info.name, info.server_version));
                            }
                            Err(e) => {
                                app.toasts.push(ToastKind::Error, format!("connection failed: {e:#}"));
                            }
                        }
                        app.connecting = false;
                    }
                } else {
                    app.handle_key(key);
                }
            }
            }
                Event::Mouse(mouse) => {
                    app.handle_mouse(mouse).await?;
                }
                _ => {}
            }
        }

        if last_tick.elapsed() >= TICK_CAP {
            spinner.tick();
            app.toasts.tick();
            // Poll form-modal test result non-blockingly. Push toast AND set the
            // form's last_test_result so the outcome shows inside the modal
            // (right where the user is editing) — most reliable place to be seen.
            if let Some(rx) = &mut app.form_test_rx {
                match rx.try_recv() {
                    Ok(res) => {
                        let (success, message) = match res {
                            Ok(dur) => {
                                let msg = format!("ping ok ({:.2?})", dur);
                                app.toasts.push(ToastKind::Success, msg.clone());
                                (true, msg)
                            }
                            Err(err) => {
                                app.toasts.push(ToastKind::Error, err.clone());
                                (false, err)
                            }
                        };
                        if let Some(form) = app.form_modal.as_mut() {
                            form.last_test_result =
                                Some(crate::ui::screens::picker::TestResult { success, message });
                        }
                        app.form_test_rx = None;
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                        // Still pending; keep waiting.
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        let msg = "ping task ended unexpectedly".to_string();
                        app.toasts.push(ToastKind::Error, msg.clone());
                        if let Some(form) = app.form_modal.as_mut() {
                            form.last_test_result = Some(crate::ui::screens::picker::TestResult {
                                success: false,
                                message: msg,
                            });
                        }
                        app.form_test_rx = None;
                    }
                }
            }
            last_tick = Instant::now();
        }
    }

    // Persist query history / favorites accumulated during the session.
    if let Err(e) = app.config.save(&app.config_path) {
        eprintln!("warning: failed to save config on exit: {e:#}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::{CollectionRef, Namespace, Record, Value};

    fn cref(ns: &str, tbl: &str) -> CollectionRef {
        CollectionRef {
            namespace: Namespace(ns.to_string()),
            name: tbl.to_string(),
        }
    }

    fn row(values: Vec<Value>) -> Record {
        Record { values }
    }

    #[test]
    fn test_quote_ident_by_dialect() {
        // PostgreSQL → double quotes
        assert_eq!(quote_ident("users", "PostgreSQL 15.3"), "\"users\"");
        assert_eq!(quote_ident("order_items", "postgres"), "\"order_items\"");
        // MySQL → backticks
        assert_eq!(quote_ident("users", "MySQL 8.0"), "`users`");
        assert_eq!(quote_ident("order items", "mysql"), "`order items`");
        // SQL Server → brackets
        assert_eq!(quote_ident("users", "Microsoft SQL Server 2019"), "[users]");
        // SQLite → backticks (accepted by SQLite)
        assert_eq!(quote_ident("users", "SQLite 3.45"), "`users`");
        // Unknown driver defaults to PG double-quote
        assert_eq!(quote_ident("users", "MongoDB"), "\"users\"");
    }

    #[test]
    fn test_quote_ident_escapes_inner_quote() {
        // An identifier containing the quote char must double it up.
        assert_eq!(quote_ident("a\"b", "postgres"), "\"a\"\"b\"");
        assert_eq!(quote_ident("a`b", "mysql"), "`a``b`");
        assert_eq!(quote_ident("a]b", "sql server"), "[a]]b]");
    }

    #[test]
    fn test_single_row_suffix_by_dialect() {
        assert_eq!(single_row_suffix("MySQL 8.0"), " LIMIT 1");
        assert_eq!(single_row_suffix("MariaDB 10.11"), " LIMIT 1");
        assert_eq!(single_row_suffix("PostgreSQL 15.3"), "");
        assert_eq!(single_row_suffix("Microsoft SQL Server 2019"), "");
        assert_eq!(single_row_suffix("SQLite 3.45"), "");
    }

    #[test]
    fn test_escape_string_literal_doubles_quotes() {
        assert_eq!(escape_string_literal("plain"), "plain");
        assert_eq!(escape_string_literal("O'Brien"), "O''Brien");
        assert_eq!(escape_string_literal("it's ''quoted''"), "it''s ''''quoted''''");
    }

    #[test]
    fn test_render_buffer_sql_sentinel_and_quotes() {
        assert_eq!(render_buffer_sql(NULL_SENTINEL), "NULL");
        assert_eq!(render_buffer_sql("hello"), "'hello'");
        assert_eq!(render_buffer_sql(""), "''");
        assert_eq!(render_buffer_sql("O'Brien"), "'O''Brien'");
    }

    #[test]
    fn test_build_where_pk_prefer_and_quote() {
        let cols = vec!["id".to_string(), "name".to_string()];
        let rec = row(vec![Value::Int(42), Value::String("ada".to_string())]);
        let pk = vec!["id".to_string()];

        // PostgreSQL: PK column double-quoted, int rendered as quoted literal.
        let sql = build_where_for_row(&cols, &rec, &pk, "PostgreSQL 15.3").unwrap();
        assert_eq!(sql, "\"id\" = '42'");

        // MySQL: backtick.
        let sql = build_where_for_row(&cols, &rec, &pk, "MySQL 8.0").unwrap();
        assert_eq!(sql, "`id` = '42'");
    }

    #[test]
    fn test_build_where_null_pk_uses_is_null() {
        let cols = vec!["id".to_string(), "name".to_string()];
        let rec = row(vec![Value::Null, Value::String("ada".to_string())]);
        let pk = vec!["id".to_string()];
        let sql = build_where_for_row(&cols, &rec, &pk, "postgres").unwrap();
        assert_eq!(sql, "\"id\" IS NULL");
    }

    #[test]
    fn test_build_where_falls_back_to_all_columns() {
        let cols = vec!["a".to_string(), "b".to_string()];
        let rec = row(vec![Value::Int(1), Value::Null]);
        // No PK → every column participates; NULL → IS NULL.
        let sql = build_where_for_row(&cols, &rec, &[], "postgres").unwrap();
        assert_eq!(sql, "\"a\" = '1' AND \"b\" IS NULL");
    }

    #[test]
    fn test_build_where_empty_returns_none() {
        // Zero-column table → no WHERE at all → None (caller bails out).
        assert_eq!(build_where_for_row(&[], &row(vec![]), &[], "postgres"), None);
    }

    #[test]
    fn test_build_insert_skips_none_and_quotes_by_dialect() {
        let c = cref("shop", "orders");
        let fields = vec![
            ("id".to_string(), Some("5".to_string())),
            ("user_id".to_string(), Some(NULL_SENTINEL.to_string())),
            ("note".to_string(), None), // skip → server DEFAULT
        ];

        let pg = build_insert_sql(&c, &fields, "PostgreSQL 15.3").unwrap();
        assert_eq!(
            pg,
            "INSERT INTO \"shop\".\"orders\" (\"id\", \"user_id\") VALUES ('5', NULL);"
        );

        let my = build_insert_sql(&c, &fields, "MySQL 8.0").unwrap();
        assert_eq!(
            my,
            "INSERT INTO `shop`.`orders` (`id`, `user_id`) VALUES ('5', NULL);"
        );
    }

    #[test]
    fn test_build_insert_all_skipped_returns_none() {
        let c = cref("shop", "orders");
        let fields = vec![("id".to_string(), None)];
        assert_eq!(build_insert_sql(&c, &fields, "postgres"), None);
    }

    #[test]
    fn test_build_insert_row_sql_dialect() {
        let columns = vec!["id".to_string(), "name".to_string(), "note".to_string()];
        let row = Record {
            values: vec![
                Value::Int(7),
                Value::String("O'Brien".to_string()),
                Value::Null,
            ],
        };
        let pg = build_insert_row_sql("users", &columns, &row, "PostgreSQL 15.3");
        assert_eq!(
            pg,
            "INSERT INTO \"users\" (\"id\", \"name\", \"note\") VALUES ('7', 'O''Brien', NULL);"
        );
        let my = build_insert_row_sql("users", &columns, &row, "MySQL 8.0");
        assert_eq!(
            my,
            "INSERT INTO `users` (`id`, `name`, `note`) VALUES ('7', 'O''Brien', NULL);"
        );
    }

    #[test]
    fn test_is_destructive_statement() {
        // DROP / TRUNCATE always trip the guard.
        assert!(is_destructive_statement("DROP TABLE users;"));
        assert!(is_destructive_statement("drop database app;"));
        assert!(is_destructive_statement("TRUNCATE TABLE audit_log;"));
        // DELETE without WHERE is destructive.
        assert!(is_destructive_statement("DELETE FROM users;"));
        assert!(is_destructive_statement("DELETE FROM users"));
        // DELETE with WHERE is allowed through.
        assert!(!is_destructive_statement("DELETE FROM users WHERE id = 5;"));
        // ALTER that DROPs a column is destructive.
        assert!(is_destructive_statement("ALTER TABLE users DROP COLUMN email;"));
        // ALTER that ADDs is safe.
        assert!(!is_destructive_statement("ALTER TABLE users ADD COLUMN bio TEXT;"));
        // A benign SELECT is safe even if a later chunk is destructive.
        assert!(is_destructive_statement("SELECT * FROM users; DROP TABLE users;"));
        // Case-insensitive.
        assert!(is_destructive_statement("  drop  table  users  "));
        // Guard bypass: DROP hidden after a comment containing ';' (the old
        // naive split() let this through; split_statements + strip does not).
        assert!(is_destructive_statement("SELECT 1; -- note; more\nDROP TABLE users"));
        assert!(is_destructive_statement("-- note\nDROP TABLE users"));
        assert!(is_destructive_statement("/* hi */ DELETE FROM users"));
        // Safe DELETE with WHERE stays safe even behind a comment.
        assert!(!is_destructive_statement("-- safe\nDELETE FROM users WHERE id = 1"));
        // Empty / whitespace → safe.
        assert!(!is_destructive_statement(""));
        assert!(!is_destructive_statement("   "));
    }
}
