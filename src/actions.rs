//! Actions the UI triggers: opening tabs, running a console query, and the
//! ERD node menu.
//!
//! Split out of `app.rs` — each takes its state explicitly rather than through
//! `&mut App`, so they read (and test) independently of the event loop.

use std::sync::Arc;
use std::time::Instant;

use crate::config::AppConfig;
use crate::driver::Page;
use crate::sql::{is_destructive_statement, quote_ident};
use ratatui::layout::Rect;

use crate::ui::screens::explorer::{FocusedPane, TreeNodeKind, WorkspaceTab};
use crate::ui::widgets::toast::{ToastKind, Toasts};

/// Open a collection (table OR view) as a data tab: focus it if already
/// open, otherwise fetch column metadata + first page and push a `DataTab`.
/// Shared by the tree Enter handler (Table & View nodes) and the object
/// search — one construction path for every tab.
pub async fn open_collection_tab(
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
    let (column_meta, foreign_keys) = meta_res
        .map(|m| (m.columns, m.foreign_keys))
        .unwrap_or_default();
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
        foreign_keys,
        sort_keys: Vec::new(),
        filter: None,
        filter_editing: false,
        filter_buffer: String::new(),
        read_only,
        grid_hit_area: None,
        grid_col_starts: Vec::new(),
        row_detail: false,
        row_detail_scroll: 0,
        search_query: String::new(),
        search_editing: false,
        search_buffer: String::new(),
    }));
    exp.active_tab_index = exp.tabs.len().saturating_sub(1);
    exp.focused_pane = FocusedPane::Workspace;
    Ok(())
}

/// Index of the ERD context-menu item at screen cell `(col, row)`, or `None`
/// when the cell is outside the menu or on its border.
pub fn erd_menu_item_at(rect: Rect, col: u16, row: u16) -> Option<usize> {
    if col < rect.x || col >= rect.x + rect.width || row < rect.y || row >= rect.y + rect.height {
        return None;
    }
    // Row 0 is the top border; items start on the next line.
    let idx = row.checked_sub(rect.y + 1)? as usize;
    (idx < crate::ui::screens::explorer::ERD_MENU_OPTIONS.len()).then_some(idx)
}

/// An in-flight console query. Execution runs on its own task so the event
/// loop keeps drawing (spinner, elapsed time) and stays able to accept the
/// keystroke that cancels it.
pub struct QueryRun {
    pub rx: tokio::sync::mpsc::Receiver<Result<Vec<crate::driver::QueryResult>, String>>,
    pub handle: tokio::task::JoinHandle<()>,
    /// Tab the results belong to — the user may switch tabs while it runs.
    pub tab: usize,
    pub started: Instant,
    pub query_text: String,
    /// Namespace the run executes against, captured at start. The retry after
    /// a reconnect must reuse it: reconnecting rebuilds the tree with the
    /// selection reset, so re-deriving from the tree would silently run the
    /// query in the first database instead of the user's.
    pub active_ns: crate::driver::Namespace,
    /// True when this run is itself the single automatic retry after a
    /// reconnect — a retry that fails must NOT trigger another reconnect,
    /// or a dead server would reconnect-loop forever.
    pub is_retry: bool,
}

/// Validate the active console's SQL and start executing it in the background.
///
/// Everything that needs the UI (the destructive-statement guard, empty input,
/// the multi-statement warning) happens here, synchronously; only the actual
/// round trips move to a task. `run` receives the handle to poll.
pub fn start_console_query(
    exp: &mut crate::ui::screens::explorer::ExplorerState,
    drv: &Arc<dyn crate::driver::Driver>,
    toasts: &mut Toasts,
    run: &mut Option<QueryRun>,
    use_tx: bool,
) {
    // Only one query at a time: starting a second would orphan the first,
    // leaving its console spinning forever with no way to cancel it.
    if run.is_some() {
        toasts.push(
            ToastKind::Warning,
            "a query is already running — press Esc to cancel it".to_string(),
        );
        return;
    }
    // Find the active database from the selected tree node, else the first.
    let active_ns = if let Some(ns) = exp.selected_node().and_then(|n| n.kind.namespace()) {
        ns.clone()
    } else {
        exp.namespaces
            .first()
            .cloned()
            .unwrap_or(crate::driver::Namespace("mysql".to_string()))
    };

    let query_text = if let Some(WorkspaceTab::Console(c)) = exp.active_tab() {
        c.text()
    } else {
        return;
    };

    // Destructive statement guard: DROP / TRUNCATE / DELETE-without-WHERE must
    // be confirmed first. Reuses the SQL-confirm modal with a placeholder
    // collection — that path only needs the namespace.
    if is_destructive_statement(&query_text) {
        toasts.push(
            ToastKind::Warning,
            "destructive statement detected — confirm to execute".to_string(),
        );
        exp.sql_confirm_modal = Some(crate::ui::screens::explorer::SqlConfirmModalState {
            collection: crate::driver::CollectionRef {
                namespace: active_ns,
                name: "(console)".to_string(),
            },
            sql_query: query_text,
            row_idx: 0,
        });
        return;
    }

    // Split on `;` and drop pure comments, so a script of only comments is
    // reported as empty rather than "succeeding" with nothing.
    let statements: Vec<String> = crate::ui::screens::query::split_statements(&query_text)
        .into_iter()
        .filter(|s| !crate::ui::screens::query::is_comment_only(s))
        .collect();

    let tab = exp.active_tab_index;
    let Some(WorkspaceTab::Console(console)) = exp.active_tab_mut() else {
        return;
    };
    if statements.is_empty() {
        console.is_executing = false;
        console.execution_error = Some("empty query — nothing to execute".to_string());
        console.results = Vec::new();
        console.last_result = None;
        console.active_result = 0;
        toasts.push(
            ToastKind::Warning,
            "empty query — nothing to execute".to_string(),
        );
        return;
    }
    // Stateful scripts (SET @x, manual BEGIN..COMMIT) run on separate pooled
    // connections, so session state doesn't persist — warn up front. Inside an
    // interactive transaction every statement shares one connection, so the
    // warning doesn't apply there.
    if statements.len() > 1 && !use_tx {
        toasts.push(
            ToastKind::Warning,
            "multi-statement: SET @x / BEGIN..COMMIT won't persist between statements".to_string(),
        );
    }
    console.is_executing = true;
    console.execution_error = None;

    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let drv = drv.clone();
    let run_ns = active_ns.clone();
    let handle = tokio::spawn(async move {
        // Autocommit off → the whole run lives in a lazily-opened transaction
        // on a dedicated connection, until the user commits or rolls back.
        // A transaction may already be open from a previous run — that is the
        // point of the mode — so only BEGIN when none is.
        if use_tx
            && !drv.in_tx().await
            && let Err(e) = drv.begin_tx().await
        {
            let _ = tx
                .send(Err(format!("failed to begin transaction: {e:#}")))
                .await;
            return;
        }
        let mut results = Vec::new();
        let mut outcome = Ok(Vec::new());
        for stmt in &statements {
            match drv.execute(&run_ns, stmt).await {
                Ok(r) => results.push(r),
                Err(e) => {
                    outcome = Err(format!("{e:#}"));
                    break;
                }
            }
        }
        if outcome.is_ok() {
            outcome = Ok(results);
        }
        let _ = tx.send(outcome).await;
    });

    *run = Some(QueryRun {
        rx,
        handle,
        tab,
        started: Instant::now(),
        query_text,
        active_ns,
        is_retry: false,
    });
}

/// Re-run a console query that failed on a dropped connection, after
/// `try_reconnect` put a fresh driver in place. The new run is flagged
/// `is_retry` so a second failure is reported, not reconnected again.
///
/// `active_ns` comes from the failed run's `QueryRun` — never re-derived from
/// the tree, which `try_reconnect` has just rebuilt with the selection reset.
pub fn retry_console_query(
    exp: &mut crate::ui::screens::explorer::ExplorerState,
    drv: &Arc<dyn crate::driver::Driver>,
    run: &mut Option<QueryRun>,
    tab: usize,
    query_text: &str,
    active_ns: crate::driver::Namespace,
    use_tx: bool,
) {
    let statements: Vec<String> = crate::ui::screens::query::split_statements(query_text)
        .into_iter()
        .filter(|s| !crate::ui::screens::query::is_comment_only(s))
        .collect();
    if statements.is_empty() {
        return;
    }
    // The console's error state was set by the failed run; clear it so the
    // tab goes back to "executing" while the retry is in flight.
    if let Some(WorkspaceTab::Console(console)) = exp.tabs.get_mut(tab) {
        console.is_executing = true;
        console.execution_error = None;
    }

    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let drv = drv.clone();
    let run_ns = active_ns.clone();
    let handle = tokio::spawn(async move {
        if use_tx
            && !drv.in_tx().await
            && let Err(e) = drv.begin_tx().await
        {
            let _ = tx
                .send(Err(format!("failed to begin transaction: {e:#}")))
                .await;
            return;
        }
        let mut results = Vec::new();
        let mut outcome = Ok(Vec::new());
        for stmt in &statements {
            match drv.execute(&run_ns, stmt).await {
                Ok(r) => results.push(r),
                Err(e) => {
                    outcome = Err(format!("{e:#}"));
                    break;
                }
            }
        }
        if outcome.is_ok() {
            outcome = Ok(results);
        }
        let _ = tx.send(outcome).await;
    });

    *run = Some(QueryRun {
        rx,
        handle,
        tab,
        started: Instant::now(),
        query_text: query_text.to_string(),
        active_ns,
        is_retry: true,
    });
}

/// Apply a finished query to the console it was started from.
pub fn finish_console_query(
    exp: &mut crate::ui::screens::explorer::ExplorerState,
    toasts: &mut Toasts,
    config: &mut AppConfig,
    conn_name: Option<&String>,
    tab: usize,
    query_text: &str,
    outcome: Result<Vec<crate::driver::QueryResult>, String>,
) {
    // Tabs can be closed while a query runs, which shifts every later index —
    // so the slot alone is not proof it is the same console. Requiring it to
    // still be marked executing stops results landing in an unrelated tab.
    let stale = !matches!(
        exp.tabs.get(tab),
        Some(WorkspaceTab::Console(c)) if c.is_executing
    );
    if stale {
        toasts.push(
            ToastKind::Info,
            "query finished, but its console tab is gone".to_string(),
        );
        return;
    }
    let Some(WorkspaceTab::Console(console)) = exp.tabs.get_mut(tab) else {
        return;
    };
    console.is_executing = false;
    match outcome {
        Ok(results) => {
            console.last_result = results.first().cloned();
            console.active_result = 0;
            console.results = results;
            console.execution_error = None;
            console.result_selected_row = 0;
            console.result_selected_col = 0;
            console.result_scroll_x = 0;
            console.result_scroll_y = 0;
            // Recorded to history here rather than on disk per query —
            // re-serializing the whole config every run is too costly; it is
            // saved once on exit.
            if let Some(conn) = conn_name {
                config.push_history(conn, query_text);
            }
            toasts.push(ToastKind::Success, "query executed".to_string());
        }
        Err(e) => {
            // Don't leave stale results visible under the error.
            console.execution_error = Some(e);
            console.results = Vec::new();
            console.last_result = None;
            console.active_result = 0;
            toasts.push(ToastKind::Error, "query failed".to_string());
        }
    }
}

/// Move a grid's column selection one step and keep it inside the rendered
/// window. Shared by the h/l keys and horizontal scrolling so the viewport
/// never desyncs from the selection.
pub fn step_column(
    selected: &mut usize,
    scroll_x: &mut usize,
    n_columns: usize,
    area: Option<Rect>,
    right: bool,
) {
    if n_columns == 0 {
        return;
    }
    if right {
        if *selected + 1 < n_columns {
            *selected += 1;
        }
    } else {
        *selected = selected.saturating_sub(1);
    }
    // Columns render at a 16-cell minimum, which is what decides how many fit.
    let max_visible = area.map(|r| (r.width / 16).max(1) as usize).unwrap_or(6);
    if *selected < *scroll_x {
        *scroll_x = *selected;
    } else if *selected >= *scroll_x + max_visible {
        *scroll_x = selected.saturating_sub(max_visible - 1);
    }
}

/// Open (or toggle) whatever the tree has selected: expand a schema, open a
/// table/view as a data tab, or show a routine/sequence definition.
///
/// Shared by Enter/Space and by a mouse click, so clicking a table does the
/// same thing as selecting it and pressing Enter.
///
/// Returns the error message when opening a table/view failed — the caller
/// decides whether to toast it or reconnect-and-retry. Other node kinds keep
/// reporting through toasts internally.
pub async fn open_tree_node(
    exp: &mut crate::ui::screens::explorer::ExplorerState,
    drv: &Arc<dyn crate::driver::Driver>,
    toasts: &mut Toasts,
    page_size: u64,
) -> Option<String> {
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
                    page_size,
                    true, // views are read-only
                )
                .await
                {
                    return Some(format!("failed to load view: {e}"));
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
                        toasts.push(ToastKind::Error, format!("failed to fetch routine: {e:#}"));
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
            // A section divider is a label; there
            // is nothing to open.
            TreeNodeKind::Section(..) => {}
            TreeNodeKind::Table(cref, _, _) => {
                if let Err(e) = open_collection_tab(
                    exp,
                    drv,
                    cref.clone(),
                    page_size,
                    false,
                )
                .await
                {
                    return Some(format!("failed to load table: {e}"));
                }
            }
        }
    }
    None
}

/// Heuristic: does this error mean the connection died (as opposed to a
/// query being wrong)? Matched on message text because sqlx surfaces
/// disconnects as opaque io/driver errors. Used to decide whether an
/// automatic reconnect-and-retry is worth attempting.
pub fn looks_like_disconnect(err: &str) -> bool {
    let e = err.to_lowercase();
    [
        "connection closed",
        "closed the connection",
        "connection reset",
        "broken pipe",
        "connection refused",
        "terminating connection",
        "server closed",
        "not connected",
        "lost connection",
        "unexpected eof",
    ]
    .iter()
    .any(|p| e.contains(p))
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_looks_like_disconnect() {
        for (msg, want) in [
            ("error communicating with server: Connection closed", true),
            ("pool timed out while waiting for an open connection", false),
            ("db error: ERROR: syntax error at or near \"SELCT\"", false),
            ("io error: Broken pipe (os error 32)", true),
            ("Server closed the connection unexpectedly", true),
            ("Lost connection to MySQL server during query", true),
            ("failed to BEGIN", false),
        ] {
            assert_eq!(super::looks_like_disconnect(msg), want, "for: {msg}");
        }
    }
}
/// Read every table's structure in `ns`. Errors on individual tables are
/// skipped rather than failing the whole comparison — a diff over the tables
/// we could read is more useful than no diff at all.
pub async fn collect_schema(
    drv: &Arc<dyn crate::driver::Driver>,
    ns: &crate::driver::Namespace,
) -> Vec<crate::driver::CollectionMeta> {
    let Ok(tables) = drv.collections(ns).await else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for t in tables {
        let cref = crate::driver::CollectionRef {
            namespace: ns.clone(),
            name: t.name,
        };
        if let Ok(meta) = drv.collection_meta(&cref).await {
            out.push(meta);
        }
    }
    out
}

/// Run one action from the ERD node context menu. Shared by the keyboard
/// (`Enter`) and mouse (click) paths so both behave identically.
pub async fn run_erd_menu_action(
    exp: &mut crate::ui::screens::explorer::ExplorerState,
    drv: &Arc<dyn crate::driver::Driver>,
    toasts: &mut Toasts,
    page_size: u64,
    cref: crate::driver::CollectionRef,
    selected: usize,
) {
    match selected {
        // View DDL
        0 => match drv.definition(&cref).await {
            Ok(ddl) => exp.ddl_popup = Some((cref, ddl)),
            Err(e) => toasts.push(ToastKind::Error, format!("failed to fetch DDL: {e:#}")),
        },
        // Open table (list rows)
        1 => {
            if let Err(e) = open_collection_tab(exp, drv, cref, page_size, false).await {
                toasts.push(ToastKind::Error, format!("failed to open table: {e}"));
            }
        }
        // Edit schema
        2 => {
            if !exp
                .driver_capabilities
                .contains(crate::driver::Capabilities::DDL)
            {
                toasts.push(
                    ToastKind::Warning,
                    "this driver does not support editing schema".to_string(),
                );
                return;
            }
            match drv.collection_meta(&cref).await {
                Ok(meta) => {
                    exp.schema_edit_modal =
                        Some(crate::ui::screens::explorer::SchemaEditModalState {
                            collection: cref,
                            columns: meta.columns,
                            selected: 0,
                            drop_cols: Vec::new(),
                            add_cols: Vec::new(),
                            type_changes: Vec::new(),
                            rename_table: None,
                            input: None,
                        });
                }
                Err(e) => toasts.push(
                    ToastKind::Error,
                    format!("failed to fetch table schema: {e:#}"),
                ),
            }
        }
        // Delete table (DROP TABLE via the SQL-confirm modal)
        3 => {
            if !exp
                .driver_capabilities
                .contains(crate::driver::Capabilities::DDL)
            {
                toasts.push(
                    ToastKind::Warning,
                    "this driver does not support dropping tables".to_string(),
                );
                return;
            }
            let driver_name = drv.info().name.clone();
            let q_ns = quote_ident(&cref.namespace.0, &driver_name);
            let q_tbl = quote_ident(&cref.name, &driver_name);
            exp.sql_confirm_modal = Some(crate::ui::screens::explorer::SqlConfirmModalState {
                collection: cref,
                sql_query: format!("DROP TABLE {q_ns}.{q_tbl};"),
                row_idx: 0,
            });
        }
        _ => {}
    }
}
/// Re-fetch the active table tab's current page (shared by insert-row,
/// sql-confirm and CSV-import so page-offset semantics stay consistent).
pub async fn refresh_table_page(
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
