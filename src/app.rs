//! App runtime: event loop, tick-driven animation, terminal lifecycle.

use std::io::{self};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, KeyboardEnhancementFlags, MouseButton, MouseEvent, MouseEventKind,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
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
use crate::ui::screens::query::{
    ConsolePopup, ConsolePopupItem, ConsolePopupMode, ConsoleSubpane, QueryConsole,
};
use crate::ui::widgets::{help, spinner::Spinner, statusbar, toast::ToastKind, toast::Toasts};

/// How long the event poll waits before a tick fires (spinner/toast cadence).
const TICK_CAP: Duration = Duration::from_millis(60);

/// Sentinel string stored in `CellEditModalState::text_buffer` when the user
/// chose to set the cell to NULL via `Ctrl+N`. The SQL builder detects this
/// sentinel and emits `SET col = NULL` (no quotes) instead of `SET col = '...'`.
/// Kept internal — never shown to the user as-is; the modal renders it as
/// the bold-italic word "NULL".
pub use crate::sql::NULL_SENTINEL;
use crate::keymap::{
    EXPLORER_HELP_BINDINGS, EXPLORER_HINTS, PICKER_HELP_BINDINGS, PICKER_HINTS,
};
use crate::actions::{
    QueryRun, collect_schema, erd_menu_item_at, finish_console_query, open_collection_tab,
    retry_console_query,
    open_tree_node, refresh_table_page, run_erd_menu_action, start_console_query, step_column,
};
use crate::sql::{
    build_insert_literal_sql, build_insert_row_sql, build_insert_sql, build_where_for_row,
    generate_alter_sql, generate_create_sql, quote_ident,
    render_buffer_sql, render_value_sql, single_row_suffix,
};

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

/// Expand a leading `~/` to the user's home directory.
fn expand_tilde(path: &str) -> std::path::PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return std::path::PathBuf::from(home).join(rest);
        }
    }
    std::path::PathBuf::from(path)
}

/// Move a selection one step within `n` items (clamped, no wrap).
fn step_selection(sel: &mut usize, n: usize, up: bool) {
    if n > 0 {
        if up {
            *sel = sel.saturating_sub(1);
        } else {
            *sel = (*sel + 1).min(n - 1);
        }
    }
}

/// Move to the next/previous workspace tab (wraps around). No-op with < 2 tabs.
fn switch_tab(exp: &mut crate::ui::screens::explorer::ExplorerState, delta: isize) {
    let n = exp.tabs.len();
    if n < 2 {
        return;
    }
    let cur = exp.active_tab_index as isize;
    let next = (cur + delta + n as isize).rem_euclid(n as isize);
    exp.active_tab_index = next as usize;
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
    /// First binding shown in the help popup; the list is longer than a normal
    /// terminal can display.
    help_scroll: usize,
    /// Full frame area from the last draw — the help popup needs it to know
    /// how many bindings actually fit before clamping the scroll.
    last_frame_area: Option<Rect>,
    should_quit: bool,

    // Screen S1 / P5 state
    selected_connection: usize,
    form_modal: Option<ConnectionForm>,
    // In-flight test ping for the form modal. None = idle.
    form_test_rx: Option<tokio::sync::mpsc::Receiver<Result<std::time::Duration, String>>>,
    // In-flight test ping from the picker's `t` key. Polled on tick like
    // form_test_rx — awaiting it inline would freeze the event loop (an SSH
    // tunnel alone is allowed 10s to come up).
    picker_test_rx: Option<tokio::sync::mpsc::Receiver<Result<std::time::Duration, String>>>,
    /// One-shot startup update check. `Some(latest)` means a newer release.
    update_check_rx: Option<tokio::sync::mpsc::Receiver<Option<String>>>,
    /// Console query running in the background, if any.
    query_run: Option<QueryRun>,
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
    /// Live SSH forward for the active connection (from its `[ssh]` section).
    /// Dropping the guard kills the `ssh -L` process, so it never outlives
    /// the driver it serves.
    ssh_tunnel: Option<crate::tunnel::SshTunnel>,
    /// Picker-pane area from the last draw — maps a mouse click to a connection.
    picker_hit_area: Option<Rect>,
    /// In-flight ERD drag-to-pan state (mouse gesture).
    erd_drag: Option<ErdDrag>,
    /// Autocommit mode for the query console (`Ctrl+T`). When off, every
    /// console run opens an interactive transaction on a dedicated connection
    /// that stays open until F6 (commit) or F7 (rollback).
    autocommit: bool,
    /// Mirror of `driver.in_tx()` for the status bar — refreshed after every
    /// console run / commit / rollback (the real state lives in the driver).
    tx_active: bool,
}

/// Mouse drag state for panning the ERD view. `start` anchors the
/// click-vs-drag threshold; `last` accumulates pan deltas.
#[derive(Clone, Copy, Debug, Default)]
struct ErdDrag {
    start_x: u16,
    start_y: u16,
    last_x: u16,
    last_y: u16,
    moved: bool,
}

impl App {
    pub fn new(config: AppConfig, config_path: PathBuf) -> Self {
        let theme = config.theme.theme();
        Self {
            config,
            config_path,
            theme,
            toasts: Toasts::default(),
            help_open: false,
            help_scroll: 0,
            last_frame_area: None,
            should_quit: false,
            selected_connection: 0,
            form_modal: None,
            form_test_rx: None,
            picker_test_rx: None,
            update_check_rx: None,
            query_run: None,
            confirm_delete_modal: None,
            mode: ScreenMode::Picker,
            active_driver: None,
            explorer_state: None,
            connecting: false,
            active_connection_name: None,
            active_connection: None,
            ssh_tunnel: None,
            picker_hit_area: None,
            erd_drag: None,
            autocommit: true,
            tx_active: false,
        }
    }

    /// If `err` looks like a dropped connection, rebuild the driver from the
    /// stored connection config. Returns true when a fresh connection is in
    /// Establish a driver for `cfg`, honouring its optional `[ssh]` section:
    /// spawns the port forward first and re-points the config at the loopback
    /// end. The tunnel guard is stored on `self` so it lives exactly as long
    /// as the driver.
    ///
    /// A previous tunnel is kept alive until the new one is proven: a failed
    /// reconnect must not kill the working forward the user still has. The one
    /// case where the old tunnel must go first is a fixed `local_port` — the
    /// new forward can't bind it while the old one holds it — so on
    /// establish-failure the old tunnel is dropped and the establish retried
    /// once. On a driver error the just-started tunnel is dropped (killed)
    /// before returning.
    async fn connect_with_tunnel(
        &mut self,
        cfg: &crate::config::ConnectionConfig,
    ) -> anyhow::Result<Arc<dyn Driver>> {
        let (eff, tunnel) = match crate::tunnel::SshTunnel::establish(cfg).await {
            Ok(pair) => pair,
            Err(first_err) => {
                if self.ssh_tunnel.is_none() {
                    return Err(first_err);
                }
                self.ssh_tunnel = None; // dropping frees a fixed local_port
                crate::tunnel::SshTunnel::establish(cfg).await?
            }
        };
        let drv = crate::driver::connect_driver(&eff).await?;
        self.ssh_tunnel = tunnel;
        Ok(drv)
    }

    /// place and the failed operation is worth retrying once.
    ///
    /// Mirrors the manual Ctrl+R path (10s timeout, namespace refresh) so an
    /// automatic rescue behaves exactly like the manual one.
    async fn try_reconnect(&mut self, err: &str) -> bool {
        if !crate::actions::looks_like_disconnect(err) {
            return false;
        }
        let Some(cfg) = self.active_connection.clone() else {
            return false;
        };
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            self.connect_with_tunnel(&cfg),
        )
        .await;
        match result {
            Ok(Ok(new_drv)) => {
                if let Ok(ns) = new_drv.namespaces().await
                    && let Some(exp) = &mut self.explorer_state
                {
                    exp.namespaces = ns;
                    exp.tables.clear();
                    exp.rebuild_tree_nodes();
                    exp.selected_tree_index = 0;
                }
                self.active_driver = Some(new_drv);
                // The old connection is gone, so any open transaction went
                // with it — the server rolled it back on disconnect.
                if self.tx_active {
                    self.tx_active = false;
                    self.toasts.push(
                        ToastKind::Warning,
                        "the open transaction was rolled back by the disconnect".to_string(),
                    );
                }
                self.toasts.push(
                    ToastKind::Success,
                    "connection dropped — reconnected".to_string(),
                );
                true
            }
            Ok(Err(e)) => {
                self.toasts
                    .push(ToastKind::Error, format!("reconnect failed: {e:#}"));
                false
            }
            Err(_) => {
                self.toasts.push(
                    ToastKind::Error,
                    "reconnect timed out after 10s".to_string(),
                );
                false
            }
        }
    }

    /// Handle a mouse event. Currently only left-clicking an ERD node opens
    /// its DDL (hit-test in scene space); every other mouse gesture is a
    /// no-op. Keyboard navigation remains the primary interaction model.
    async fn handle_mouse(&mut self, mouse: MouseEvent) -> anyhow::Result<()> {
        // Scroll: two-finger scroll / wheel pans the ERD (all directions) or
        // moves the selection one step in lists (picker / tree / grid /
        // console result — vertical only).
        if matches!(mouse.kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollDown | MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight) {
            let up = matches!(mouse.kind, MouseEventKind::ScrollUp);
            let is_vertical = matches!(mouse.kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollDown);
            if matches!(self.mode, ScreenMode::Picker) {
                if let Some(area) = self.picker_hit_area
                    && is_vertical
                    && mouse.row >= area.y
                    && mouse.row < area.y + area.height
                {
                    step_selection(
                        &mut self.selected_connection,
                        self.config.connections.len(),
                        up,
                    );
                }
                return Ok(());
            }
            // Don't scroll a list behind an open modal.
            let modal_open = self
                .explorer_state
                .as_ref()
                .map(|e| {
                    e.ddl_popup.is_some()
                        || e.export_modal.is_some()
                        || e.cell_edit_modal.is_some()
                        || e.insert_row_modal.is_some()
                        || e.sql_confirm_modal.is_some()
                        || e.object_search.is_some()
                        || e.import_csv_modal.is_some()
                        || e.schema_edit_modal.is_some()
                        || e.create_object_modal.is_some()
                        || e.erd_menu.is_some()
                        || e.tree_menu.is_some()
                        || e.explain_plan.is_some()
                        || e.process_list.is_some()
                        || e.schema_diff.is_some()
                        || e.diff_picker.is_some()
                })
                .unwrap_or(false);
            if modal_open {
                return Ok(());
            }
            if let Some(exp) = &mut self.explorer_state {
                // Tree pane.
                if let Some(area) = exp.tree_hit_area {
                    if is_vertical
                        && mouse.column >= area.x
                        && mouse.column < area.x + area.width
                        && mouse.row >= area.y
                        && mouse.row < area.y + area.height
                    {
                        exp.selected_tree_index =
                            exp.next_selectable(exp.selected_tree_index, if up { -1 } else { 1 });
                        return Ok(());
                    }
                }
                // Workspace: grid / result rows.
                let mut focus_workspace = false;
                if let Some(tab) = exp.active_tab_mut() {
                    match tab {
                        WorkspaceTab::Table(t) => {
                            // Horizontal scroll (two-finger swipe) walks the
                            // columns, mirroring how vertical scroll walks rows.
                            if let Some(area) = t.grid_hit_area
                                && !is_vertical
                                && mouse.column >= area.x
                                && mouse.column < area.x + area.width
                                && mouse.row >= area.y
                                && mouse.row < area.y + area.height
                            {
                                step_column(
                                    &mut t.selected_col,
                                    &mut t.scroll_offset_x,
                                    t.page.columns.len(),
                                    t.grid_hit_area,
                                    matches!(mouse.kind, MouseEventKind::ScrollRight),
                                );
                                focus_workspace = true;
                            }
                            if let Some(area) = t.grid_hit_area
                                && is_vertical
                                && mouse.column >= area.x
                                && mouse.column < area.x + area.width
                                && mouse.row >= area.y
                                && mouse.row < area.y + area.height
                            {
                                // Bound against the *displayed* (filtered) rows.
                                let visible = t
                                    .filter
                                    .as_ref()
                                    .map(|f| {
                                        t.page
                                            .records
                                            .iter()
                                            .filter(|r| {
                                                crate::ui::screens::explorer::record_matches_filter(
                                                    r, f,
                                                )
                                            })
                                            .count()
                                    })
                                    .unwrap_or(t.page.records.len());
                                step_selection(&mut t.selected_row, visible, up);
                                focus_workspace = true;
                            }
                        }
                        WorkspaceTab::Console(c) => {
                            if let Some(area) = c.result_hit_area
                                && !is_vertical
                                && mouse.column >= area.x
                                && mouse.column < area.x + area.width
                                && mouse.row >= area.y
                                && mouse.row < area.y + area.height
                            {
                                let n = c.last_result.as_ref().map(|r| r.columns.len()).unwrap_or(0);
                                step_column(
                                    &mut c.result_selected_col,
                                    &mut c.result_scroll_x,
                                    n,
                                    c.result_hit_area,
                                    matches!(mouse.kind, MouseEventKind::ScrollRight),
                                );
                                c.focused_subpane = ConsoleSubpane::Result;
                                focus_workspace = true;
                            }
                            if let Some(area) = c.result_hit_area
                                && is_vertical
                                && mouse.column >= area.x
                                && mouse.column < area.x + area.width
                                && mouse.row >= area.y
                                && mouse.row < area.y + area.height
                            {
                                if let Some(res) = &c.last_result {
                                    step_selection(&mut c.result_selected_row, res.records.len(), up);
                                    c.focused_subpane = ConsoleSubpane::Result;
                                }
                                focus_workspace = true;
                            }
                        }
                        // Two-finger scroll / wheel pans the ERD in all
                        // directions (zoom stays on the `+` / `-` keys).
                        WorkspaceTab::Erd(erd) => {
                            if let Some(area) = erd.last_canvas_area
                                && mouse.column >= area.x
                                && mouse.column < area.x + area.width
                                && mouse.row >= area.y
                                && mouse.row < area.y + area.height
                            {
                                match mouse.kind {
                                    MouseEventKind::ScrollUp => erd.scroll_up(),
                                    MouseEventKind::ScrollDown => erd.scroll_down(),
                                    MouseEventKind::ScrollLeft => erd.scroll_left(),
                                    MouseEventKind::ScrollRight => erd.scroll_right(),
                                    _ => {}
                                }
                            }
                        }
                    }
                }
                if focus_workspace {
                    exp.focused_pane = FocusedPane::Workspace;
                }
            }
            return Ok(());
        }

        // Figma-like mouse: drag pans the ERD, release after a click opens DDL.
        if matches!(mouse.kind, MouseEventKind::Drag(MouseButton::Left)) {
            if let Some(drag) = self.erd_drag {
                let dx = i32::from(mouse.column) - i32::from(drag.last_x);
                let dy = i32::from(mouse.row) - i32::from(drag.last_y);
                // A drag only counts as such past a small threshold, so a
                // one-cell wobble on a press doesn't kill the click.
                let moved = drag.moved
                    || (i32::from(mouse.column) - i32::from(drag.start_x)).abs() > 1
                    || (i32::from(mouse.row) - i32::from(drag.start_y)).abs() > 1;
                self.erd_drag = Some(ErdDrag {
                    start_x: drag.start_x,
                    start_y: drag.start_y,
                    last_x: mouse.column,
                    last_y: mouse.row,
                    moved,
                });
                if let Some(exp) = &mut self.explorer_state
                    && let Some(WorkspaceTab::Erd(erd)) = exp.active_tab_mut()
                {
                    erd.pan_by_cells(dx, dy);
                }
            }
            return Ok(());
        }

        if matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left)) {
            let was_click = self
                .erd_drag
                .as_ref()
                .map(|d| !d.moved)
                .unwrap_or(false);
            self.erd_drag = None;
            if was_click {
                // A click (press + release without drag) on an ERD node opens
                // its context menu. Only when no other modal is open.
                let (Some(_drv), Some(exp)) = (&self.active_driver, &mut self.explorer_state)
                else {
                    return Ok(());
                };
                if exp.ddl_popup.is_some()
                    || exp.export_modal.is_some()
                    || exp.cell_edit_modal.is_some()
                    || exp.insert_row_modal.is_some()
                    || exp.sql_confirm_modal.is_some()
                    || exp.object_search.is_some()
                    || exp.import_csv_modal.is_some()
                    || exp.schema_edit_modal.is_some()
                    || exp.create_object_modal.is_some()
                {
                    return Ok(());
                }
                if let Some(WorkspaceTab::Erd(erd)) = exp.active_tab_mut() {
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
                    exp.erd_menu = Some(crate::ui::screens::explorer::ErdMenuState {
                        collection: cref,
                        selected: 0,
                        menu_at: Some((mouse.column, mouse.row)),
                    });
                }
            }
            return Ok(());
        }

        // Moving the cursor over the ERD context menu highlights the item
        // under it, so the mouse and keyboard share one selection.
        if matches!(mouse.kind, MouseEventKind::Moved)
            && let Some(exp) = &mut self.explorer_state
            && let Some(rect) = exp.erd_menu_area
            && let Some(menu) = &mut exp.erd_menu
            && let Some(idx) = erd_menu_item_at(rect, mouse.column, mouse.row)
        {
            menu.selected = idx;
            return Ok(());
        }

        // Same hover-highlight for the tree context menu.
        if matches!(mouse.kind, MouseEventKind::Moved)
            && let Some(exp) = &mut self.explorer_state
            && let Some(rect) = exp.tree_menu_area
            && let Some(menu) = &mut exp.tree_menu
            && let Some(idx) = erd_menu_item_at(rect, mouse.column, mouse.row)
        {
            menu.selected = idx;
            return Ok(());
        }

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
        // ERD context menu: clicking an item runs it, clicking anywhere else
        // dismisses the menu (standard context-menu behaviour).
        if exp.erd_menu.is_some() {
            let hit = exp
                .erd_menu_area
                .and_then(|rect| erd_menu_item_at(rect, mouse.column, mouse.row));
            let cref = exp.erd_menu.as_ref().map(|m| m.collection.clone());
            exp.erd_menu = None;
            exp.erd_menu_area = None;
            if let (Some(idx), Some(cref)) = (hit, cref) {
                let drv = drv.clone();
                let page_size = self.config.effective_page_size();
                run_erd_menu_action(exp, &drv, &mut self.toasts, page_size, cref, idx).await;
            }
            return Ok(());
        }
        // Tree context menu: identical click behaviour.
        if exp.tree_menu.is_some() {
            let hit = exp
                .tree_menu_area
                .and_then(|rect| erd_menu_item_at(rect, mouse.column, mouse.row));
            let cref = exp.tree_menu.as_ref().map(|m| m.collection.clone());
            exp.tree_menu = None;
            exp.tree_menu_area = None;
            if let (Some(idx), Some(cref)) = (hit, cref) {
                let drv = drv.clone();
                let page_size = self.config.effective_page_size();
                run_erd_menu_action(exp, &drv, &mut self.toasts, page_size, cref, idx).await;
            }
            return Ok(());
        }
        if exp.export_modal.is_some()
            || exp.cell_edit_modal.is_some()
            || exp.insert_row_modal.is_some()
            || exp.sql_confirm_modal.is_some()
            || exp.object_search.is_some()
            || exp.import_csv_modal.is_some()
            || exp.schema_edit_modal.is_some()
            || exp.create_object_modal.is_some()
        {
            return Ok(());
        }

        // Click a workspace tab header to switch to that tab.
        if let Some(bar) = exp.tab_bar_area
            && mouse.column >= bar.x
            && mouse.column < bar.x + bar.width
            && mouse.row >= bar.y
            && mouse.row < bar.y + bar.height
        {
            if let Some(idx) = exp.tab_starts.iter().rposition(|&s| mouse.column >= s) {
                exp.active_tab_index = idx;
                exp.focused_pane = FocusedPane::Workspace;
            }
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
                    // Clicking a section divider only moves the selection to a
                    // real row; it must not open whatever happens to sit next
                    // to the label.
                    let clicked_openable = exp
                        .tree_nodes
                        .get(idx)
                        .map(|n| n.kind.is_selectable())
                        .unwrap_or(false);
                    exp.selected_tree_index = exp.snap_to_selectable(idx);
                    exp.focused_pane = FocusedPane::Tree;
                    // A click opens the node outright — selecting it and then
                    // asking for a second keypress is a step nobody wants.
                    if clicked_openable
                        && let Some(drv) = self.active_driver.clone()
                    {
                        let page_size = self.config.effective_page_size();
                        if let Some(err) = open_tree_node(exp, &drv, &mut self.toasts, page_size).await {
                            // A dead connection gets one reconnect; anything
                            // else is just reported. No automatic retry here:
                            // the reconnect rebuilt the tree with the selection
                            // reset, so re-running would open the FIRST node,
                            // not the one the user clicked.
                            if self.try_reconnect(&err).await {
                                self.toasts.push(
                                    ToastKind::Info,
                                    "reconnected — open the node again to retry".to_string(),
                                );
                            } else {
                                self.toasts.push(ToastKind::Error, err);
                            }
                        }
                    }
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
                        // Table header (1) + bottom_margin (1) = 2 rows. The
                        // rendered window is sliced by `result_scroll_y`, so the
                        // click's absolute row is offset + (rel_row - 2).
                        if rel_row >= 2 {
                            let data_row = c.result_scroll_y + (rel_row - 2) as usize;
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
                // Press on the ERD starts a potential drag-to-pan; DDL opens
                // on release if no drag occurred (handled in the Up branch).
                // Only gestures starting on the canvas count — pressing on
                // the border / status bar does nothing.
                let in_canvas = erd
                    .last_canvas_area
                    .map(|r| {
                        mouse.column >= r.x
                            && mouse.column < r.x + r.width
                            && mouse.row >= r.y
                            && mouse.row < r.y + r.height
                    })
                    .unwrap_or(false);
                if in_canvas {
                    self.erd_drag = Some(ErdDrag {
                        start_x: mouse.column,
                        start_y: mouse.row,
                        last_x: mouse.column,
                        last_y: mouse.row,
                        moved: false,
                    });
                }
                return Ok(());
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

        // Help owns every key while open: Esc/? closes, the rest scrolls the
        // (longer than one screen) binding list.
        if self.help_open {
            let bindings: &[(&str, &str)] = if matches!(self.mode, ScreenMode::Connected) {
                &EXPLORER_HELP_BINDINGS
            } else {
                &PICKER_HELP_BINDINGS
            };
            let area = self.last_frame_area.unwrap_or_default();
            let per_screen = crate::ui::widgets::help::visible_rows(area, bindings);
            let max_scroll = bindings.len().saturating_sub(per_screen);
            match key.code {
                KeyCode::Esc | KeyCode::Char('?') => {
                    self.help_open = false;
                    self.help_scroll = 0;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.help_scroll = (self.help_scroll + 1).min(max_scroll);
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.help_scroll = self.help_scroll.saturating_sub(1);
                }
                KeyCode::PageDown => {
                    self.help_scroll = (self.help_scroll + per_screen).min(max_scroll);
                }
                KeyCode::PageUp => {
                    self.help_scroll = self.help_scroll.saturating_sub(per_screen);
                }
                _ => {}
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

                    // Boolean column → dropdown navigation instead of free-text
                    // editing. Enter (SQL confirm) is handled in the event loop.
                    if exp.cell_edit_modal.as_ref().unwrap().is_boolean {
                        match key.code {
                            KeyCode::Up | KeyCode::Char('k') | KeyCode::Down | KeyCode::Char('j') => {
                                let edit = exp.cell_edit_modal.as_mut().unwrap();
                                let max = if edit.is_nullable { 2 } else { 1 };
                                let delta = if matches!(
                                    key.code,
                                    KeyCode::Up | KeyCode::Char('k')
                                ) {
                                    max
                                } else {
                                    1
                                };
                                edit.bool_selection = (edit.bool_selection + delta) % (max + 1);
                                edit.text_buffer = match edit.bool_selection {
                                    0 => "true".to_string(),
                                    1 => "false".to_string(),
                                    _ => NULL_SENTINEL.to_string(),
                                };
                                return;
                            }
                            // Ctrl+N still sets NULL on a nullable boolean.
                            KeyCode::Char('n') | KeyCode::Char('N') if plan.is_ctrl => {
                                if plan.is_nullable {
                                    let edit = exp.cell_edit_modal.as_mut().unwrap();
                                    edit.bool_selection = 2;
                                    edit.text_buffer = NULL_SENTINEL.to_string();
                                } else {
                                    self.toasts.push(
                                        ToastKind::Warning,
                                        "this column is NOT NULL — cannot set to NULL".to_string(),
                                    );
                                }
                                return;
                            }
                            // Esc falls through to the phase-2 Esc arm (close).
                            KeyCode::Esc => {}
                            // All other keys are ignored for booleans.
                            _ => return,
                        }
                    }

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

                // Query-plan overlay owns Esc + scrolling while it is open.
                if let Some(plan) = &mut exp.explain_plan {
                    match key.code {
                        KeyCode::Esc => exp.explain_plan = None,
                        KeyCode::Down | KeyCode::Char('j') => {
                            let last = plan.nodes.len().saturating_sub(1);
                            if plan.scroll < last {
                                plan.scroll += 1;
                            }
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            plan.scroll = plan.scroll.saturating_sub(1);
                        }
                        _ => {}
                    }
                    return;
                }

                // ERD node context menu: Up/Down navigate, Esc closes, Enter
                // runs the highlighted action (handled in the async loop).
                if let Some(menu) = &mut exp.erd_menu {
                    match key.code {
                        KeyCode::Esc => exp.erd_menu = None,
                        KeyCode::Up | KeyCode::Char('k') => {
                            menu.selected = menu.selected.saturating_sub(1);
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            menu.selected = (menu.selected + 1)
                                .min(crate::ui::screens::explorer::ERD_MENU_OPTIONS.len() - 1);
                        }
                        KeyCode::Enter => {} // handled in the event loop
                        _ => {}
                    }
                    return;
                }

                // Tree context menu: same navigation as the ERD menu.
                if let Some(menu) = &mut exp.tree_menu {
                    match key.code {
                        KeyCode::Esc => exp.tree_menu = None,
                        KeyCode::Up | KeyCode::Char('k') => {
                            menu.selected = menu.selected.saturating_sub(1);
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            menu.selected = (menu.selected + 1)
                                .min(crate::ui::screens::explorer::ERD_MENU_OPTIONS.len() - 1);
                        }
                        KeyCode::Enter => {} // handled in the event loop
                        _ => {}
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

                // Schema-edit modal (browse columns / type input).
                if exp.schema_edit_modal.is_some() {
                    use crate::ui::screens::explorer::SchemaInput;
                    let mut close_input = false;
                    let mut pending_add: Option<(String, String)> = None;
                    let mut pending_rename: Option<Option<String>> = None;
                    let mut pending_type_change: Option<(String, String)> = None;
                    if let Some(input) = &mut exp.schema_edit_modal.as_mut().unwrap().input {
                        match input {
                            SchemaInput::AddColumn { name, ty, stage } => match key.code {
                                KeyCode::Esc => close_input = true,
                                KeyCode::Backspace => {
                                    if *stage == 0 {
                                        name.pop();
                                    } else {
                                        ty.pop();
                                    }
                                }
                                KeyCode::Char(c)
                                    if !key.modifiers.contains(KeyModifiers::CONTROL) =>
                                {
                                    if *stage == 0 {
                                        name.push(c);
                                    } else {
                                        ty.push(c);
                                    }
                                }
                                KeyCode::Enter => {
                                    if *stage == 0 {
                                        *stage = 1;
                                    } else {
                                        let n = name.trim().to_string();
                                        let t = ty.trim().to_string();
                                        if !n.is_empty() && !t.is_empty() {
                                            pending_add = Some((n, t));
                                        }
                                        close_input = true;
                                    }
                                }
                                _ => {}
                            },
                            SchemaInput::RenameTable { value } => match key.code {
                                KeyCode::Esc => close_input = true,
                                KeyCode::Backspace => {
                                    value.pop();
                                }
                                KeyCode::Char(c)
                                    if !key.modifiers.contains(KeyModifiers::CONTROL) =>
                                {
                                    value.push(c);
                                }
                                KeyCode::Enter => {
                                    let v = value.trim().to_string();
                                    pending_rename = Some(if v.is_empty() {
                                        None
                                    } else {
                                        Some(v)
                                    });
                                    close_input = true;
                                }
                                _ => {}
                            },
                            SchemaInput::ChangeType { column, value } => match key.code {
                                KeyCode::Esc => close_input = true,
                                KeyCode::Backspace => {
                                    value.pop();
                                }
                                KeyCode::Char(c)
                                    if !key.modifiers.contains(KeyModifiers::CONTROL) =>
                                {
                                    value.push(c);
                                }
                                KeyCode::Enter => {
                                    let v = value.trim().to_string();
                                    if !v.is_empty() {
                                        pending_type_change =
                                            Some((column.clone(), v));
                                    }
                                    close_input = true;
                                }
                                _ => {}
                            },
                        }
                    } else {
                        if key.code != KeyCode::Enter {
                            let s = exp.schema_edit_modal.as_mut().unwrap();
                            match key.code {
                                KeyCode::Up | KeyCode::Char('k') => {
                                    s.selected = s.selected.saturating_sub(1);
                                }
                                KeyCode::Down | KeyCode::Char('j') => {
                                    s.selected =
                                        (s.selected + 1).min(s.columns.len().saturating_sub(1));
                                }
                                KeyCode::Char('d') => {
                                    if let Some(col) = s.columns.get(s.selected) {
                                        let n = col.name.clone();
                                        if s.drop_cols.contains(&n) {
                                            s.drop_cols.retain(|c| c != &n);
                                        } else {
                                            s.drop_cols.push(n);
                                        }
                                    }
                                }
                                KeyCode::Char('a') => {
                                    s.input = Some(SchemaInput::AddColumn {
                                        name: String::new(),
                                        ty: String::new(),
                                        stage: 0,
                                    });
                                }
                                KeyCode::Char('r') => {
                                    s.input = Some(SchemaInput::RenameTable {
                                        value: s.rename_table.clone().unwrap_or_default(),
                                    });
                                }
                                KeyCode::Char('c') => {
                                    if let Some(col) = s.columns.get(s.selected) {
                                        s.input = Some(SchemaInput::ChangeType {
                                            column: col.name.clone(),
                                            value: col.data_type.clone(),
                                        });
                                    }
                                }
                                KeyCode::Esc => exp.schema_edit_modal = None,
                                _ => {}
                            }
                            return;
                        }
                        // Enter (browse) → fall through; the event loop applies.
                    }
                    if close_input {
                        exp.schema_edit_modal.as_mut().unwrap().input = None;
                    }
                    let s = exp.schema_edit_modal.as_mut().unwrap();
                    if let Some((n, t)) = pending_add {
                        s.add_cols.push((n, t));
                    }
                    if let Some(rename) = pending_rename {
                        s.rename_table = rename;
                    }
                    if let Some((col, ty)) = pending_type_change {
                        s.type_changes.retain(|(c, _)| c != &col);
                        s.type_changes.push((col, ty));
                    }
                    return;
                }

                // Create-object modal: pick kind + type name. Enter falls
                // through to the event loop, which generates the CREATE.
                if let Some(c) = &mut exp.create_object_modal {
                    if key.code != KeyCode::Enter {
                        match key.code {
                            KeyCode::Esc => exp.create_object_modal = None,
                            KeyCode::Left | KeyCode::Up => {
                                c.kind = c.kind.cycle(-1);
                            }
                            KeyCode::Right | KeyCode::Down => {
                                c.kind = c.kind.cycle(1);
                            }
                            KeyCode::Backspace => {
                                c.name.pop();
                            }
                            KeyCode::Char(ch)
                                if !key.modifiers.contains(KeyModifiers::CONTROL) =>
                            {
                                c.name.push(ch);
                            }
                            _ => {}
                        }
                        return;
                    }
                    // Enter → generate CREATE in the event loop.
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
                            exp.focus_tree();
                            return;
                        }
                        self.mode = ScreenMode::Picker;
                        self.active_driver = None;
                        self.explorer_state = None;
                        // Teardown must mirror connect: drop the retained
                        // connection config (and its password) too. The tunnel
                        // guard going away kills the ssh forwarder.
                        self.active_connection = None;
                        self.active_connection_name = None;
                        self.ssh_tunnel = None;
                        return;
                    }
                    // Ctrl+B folds the explorer away so the console gets the
                    // full width, and back again.
                    KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        exp.tree_collapsed = !exp.tree_collapsed;
                        // A hidden tree must not keep focus, or keys would go
                        // to a pane that is not on screen.
                        if exp.tree_collapsed {
                            exp.focused_pane = FocusedPane::Workspace;
                        }
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

                            // A collapsed tree is not on screen; focusing it
                            // would send keys to a pane the user cannot see.
                            if switch_to_tree && !exp.tree_collapsed {
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
                                        collection: Some(t.collection.clone()),
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
                                            collection: None,
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
                        // [ / ] switch workspace tabs (free here in the tree).
                        KeyCode::Char('[') => switch_tab(exp, -1),
                        KeyCode::Char(']') => switch_tab(exp, 1),
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
                        // Fn+Up/Down (PageUp/PageDown) scroll one viewport.
                        KeyCode::PageDown => {
                            if !exp.tree_nodes.is_empty() {
                                let page_rows = exp
                                    .tree_hit_area
                                    .map(|r| (r.height as usize).saturating_sub(1))
                                    .unwrap_or(10)
                                    .max(1);
                                let target = (exp.selected_tree_index + page_rows)
                                    .min(exp.tree_nodes.len() - 1);
                                exp.selected_tree_index = exp.snap_to_selectable(target);
                            }
                        }
                        KeyCode::PageUp => {
                            let page_rows = exp
                                .tree_hit_area
                                .map(|r| (r.height as usize).saturating_sub(1))
                                .unwrap_or(10)
                                .max(1);
                            let target = exp.selected_tree_index.saturating_sub(page_rows);
                            exp.selected_tree_index = exp.snap_to_selectable(target);
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
                                    // Search input mode owns every key until
                                    // Enter/Esc, same shape as the filter.
                                    if t.search_editing {
                                        match key.code {
                                            KeyCode::Esc => {
                                                t.search_editing = false;
                                                t.search_buffer.clear();
                                            }
                                            KeyCode::Enter => {
                                                t.search_query = t.search_buffer.clone();
                                                t.search_editing = false;
                                                // Jump to the first hit so the
                                                // search does something visible.
                                                if let Some(&(r, c)) =
                                                    crate::ui::screens::explorer::search_matches(t)
                                                        .first()
                                                {
                                                    t.selected_row = r;
                                                    t.selected_col = c;
                                                }
                                            }
                                            KeyCode::Backspace => {
                                                t.search_buffer.pop();
                                            }
                                            KeyCode::Char(c)
                                                if !key.modifiers
                                                    .contains(KeyModifiers::CONTROL) =>
                                            {
                                                t.search_buffer.push(c);
                                            }
                                            _ => {}
                                        }
                                    } else if t.row_detail {
                                        // Row-detail overlay: scroll columns,
                                        // step rows, close.
                                        match key.code {
                                            KeyCode::Esc | KeyCode::Char('v') | KeyCode::Char('V') => {
                                                t.row_detail = false;
                                            }
                                            KeyCode::Down | KeyCode::Char('j') => {
                                                let last = t.page.columns.len().saturating_sub(1);
                                                if t.row_detail_scroll < last {
                                                    t.row_detail_scroll += 1;
                                                }
                                            }
                                            KeyCode::Up | KeyCode::Char('k') => {
                                                t.row_detail_scroll =
                                                    t.row_detail_scroll.saturating_sub(1);
                                            }
                                            // ←/→ walk rows without leaving the
                                            // overlay, so scanning is fast.
                                            KeyCode::Right | KeyCode::Char('l') => {
                                                let n = crate::ui::screens::explorer::visible_records(t).len();
                                                if t.selected_row + 1 < n {
                                                    t.selected_row += 1;
                                                    t.row_detail_scroll = 0;
                                                }
                                            }
                                            KeyCode::Left | KeyCode::Char('h')
                                                if t.selected_row > 0 =>
                                            {
                                                t.selected_row -= 1;
                                                t.row_detail_scroll = 0;
                                            }
                                            _ => {}
                                        }
                                    } else if t.filter_editing {
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
                                        // [ / ] switch workspace tabs (free in a table grid).
                                        KeyCode::Char('[') => switch_tab(exp, -1),
                                        KeyCode::Char(']') => switch_tab(exp, 1),
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
                                        if let Some(row) = crate::ui::screens::explorer::visible_records(t).get(t.selected_row).copied() {
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
                                    // `v` expands the selected row vertically —
                                    // the readable way to inspect a wide table.
                                    KeyCode::Char('v') | KeyCode::Char('V') => {
                                        t.row_detail = true;
                                        t.row_detail_scroll = 0;
                                    }
                                    // Ctrl+F: free-text search across all cells
                                    // (complements `/`, which filters by column).
                                    KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                        t.search_editing = true;
                                        t.search_buffer = t.search_query.clone();
                                    }
                                    // Ctrl+G: jump to the next match, wrapping.
                                    KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                        let hits = crate::ui::screens::explorer::search_matches(t);
                                        if hits.is_empty() {
                                            self.toasts.push(
                                                ToastKind::Info,
                                                "no matches — press Ctrl+F to search".to_string(),
                                            );
                                        } else {
                                            let cur = (t.selected_row, t.selected_col);
                                            let next = hits
                                                .iter()
                                                .find(|&&h| h > cur)
                                                .copied()
                                                .unwrap_or(hits[0]);
                                            t.selected_row = next.0;
                                            t.selected_col = next.1;
                                            // Keep the jumped-to column on screen.
                                            let max_visible = t
                                                .grid_hit_area
                                                .map(|r| (r.width / 16).max(1) as usize)
                                                .unwrap_or(6);
                                            if t.selected_col < t.scroll_offset_x {
                                                t.scroll_offset_x = t.selected_col;
                                            } else if t.selected_col >= t.scroll_offset_x + max_visible {
                                                t.scroll_offset_x =
                                                    t.selected_col.saturating_sub(max_visible - 1);
                                            }
                                        }
                                    }
                                    KeyCode::Char('s') => {
                                        // Each column cycles asc → desc → off,
                                        // and columns accumulate: sorting by a
                                        // second column keeps the first as the
                                        // primary key and uses the new one to
                                        // break ties.
                                        use crate::ui::screens::explorer::SortDir;
                                        let col = t.selected_col;
                                        match t.sort_keys.iter().position(|(c, _)| *c == col) {
                                            None => t.sort_keys.push((col, SortDir::Asc)),
                                            Some(i) => match t.sort_keys[i].1 {
                                                SortDir::Asc => t.sort_keys[i].1 = SortDir::Desc,
                                                SortDir::Desc => {
                                                    t.sort_keys.remove(i);
                                                }
                                            },
                                        }
                                    }
                                    // `S` drops every sort key at once —
                                    // unwinding them one column at a time is
                                    // tedious once a few are stacked up.
                                    KeyCode::Char('S') => {
                                        if t.sort_keys.is_empty() {
                                            self.toasts.push(
                                                ToastKind::Info,
                                                "no sort to clear".to_string(),
                                            );
                                        } else {
                                            t.sort_keys.clear();
                                            self.toasts.push(
                                                ToastKind::Info,
                                                "sort cleared".to_string(),
                                            );
                                        }
                                    }
                                    KeyCode::Up | KeyCode::Char('k') => {
                                        if t.selected_row > 0 {
                                            t.selected_row -= 1;
                                        }
                                    }
                                    KeyCode::Down | KeyCode::Char('j') => {
                                        // Bound by the DISPLAYED rows: with a
                                        // filter active the natural count is
                                        // larger, which would let the cursor
                                        // walk past the last visible row.
                                        let visible =
                                            crate::ui::screens::explorer::visible_records(t).len();
                                        if t.selected_row + 1 < visible {
                                            t.selected_row += 1;
                                        }
                                    }
                                    // PageUp/PageDown scroll one viewport (rows visible
                                    // in the grid) at a time.
                                    KeyCode::PageDown => {
                                        let visible = t
                                            .filter
                                            .as_ref()
                                            .map(|f| {
                                                t.page
                                                    .records
                                                    .iter()
                                                    .filter(|r| {
                                                        crate::ui::screens::explorer::record_matches_filter(
                                                            r, f,
                                                        )
                                                    })
                                                    .count()
                                            })
                                            .unwrap_or(t.page.records.len());
                                        if visible > 0 {
                                            let page_rows = t
                                                .grid_hit_area
                                                .map(|r| (r.height as usize).saturating_sub(2))
                                                .unwrap_or(10)
                                                .max(1);
                                            t.selected_row =
                                                (t.selected_row + page_rows).min(visible - 1);
                                        }
                                    }
                                    KeyCode::PageUp => {
                                        let page_rows = t
                                            .grid_hit_area
                                            .map(|r| (r.height as usize).saturating_sub(2))
                                            .unwrap_or(10)
                                            .max(1);
                                        t.selected_row = t.selected_row.saturating_sub(page_rows);
                                    }
                                    KeyCode::Left | KeyCode::Char('h') => {
                                        step_column(
                                            &mut t.selected_col,
                                            &mut t.scroll_offset_x,
                                            t.page.columns.len(),
                                            t.grid_hit_area,
                                            false,
                                        );
                                    }
                                    KeyCode::Right | KeyCode::Char('l') => {
                                        step_column(
                                            &mut t.selected_col,
                                            &mut t.scroll_offset_x,
                                            t.page.columns.len(),
                                            t.grid_hit_area,
                                            true,
                                        );
                                    }
                                    KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                        if let Some(row) = crate::ui::screens::explorer::visible_records(t).get(t.selected_row).copied() {
                                            match ClipboardManager::copy_row_tsv(row) {
                                                Ok(_) => self.toasts.push(ToastKind::Success, "copied row as TSV (spreadsheet) to clipboard".to_string()),
                                                Err(e) => self.toasts.push(ToastKind::Error, e),
                                            }
                                        }
                                    }
                                    KeyCode::Char('y') => {
                                        if let Some(row) = crate::ui::screens::explorer::visible_records(t).get(t.selected_row).copied() {
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
                                            if let Some(row) = crate::ui::screens::explorer::visible_records(t).get(t.selected_row).copied() {
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
                                        if let Some(row) = crate::ui::screens::explorer::visible_records(t).get(t.selected_row).copied() {
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
                                        } else if let Some(row) = crate::ui::screens::explorer::visible_records(t).get(t.selected_row).copied()
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
                                            let is_boolean = t
                                                .column_meta
                                                .iter()
                                                .find(|m| m.name == *col_name)
                                                .map(|m| m.data_type.to_uppercase().contains("BOOL"))
                                                .unwrap_or(false);
                                            let bool_selection = match val {
                                                crate::driver::Value::Bool(true) => 0,
                                                crate::driver::Value::Bool(false) => 1,
                                                _ => 2,
                                            };
                                            let data_type = t
                                                .column_meta
                                                .iter()
                                                .find(|m| m.name == *col_name)
                                                .map(|m| m.data_type.clone())
                                                .unwrap_or_default();
                                            exp.cell_edit_modal = Some(crate::ui::screens::explorer::CellEditModalState {
                                                collection: t.collection.clone(),
                                                column_name: col_name.clone(),
                                                data_type,
                                                row_idx: t.selected_row,
                                                col_idx: t.selected_col,
                                                text_buffer: val.display_str(),
                                                is_nullable,
                                                is_boolean,
                                                bool_selection,
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
                                            KeyCode::Esc => {
                                                c.popup = None;
                                                c.autocomplete.clear();
                                            }
                                            KeyCode::Up => {
                                                popup.selected = popup.selected.saturating_sub(1);
                                            }
                                            KeyCode::Down => {
                                                popup.selected = (popup.selected + 1)
                                                    .min(popup.items.len().saturating_sub(1));
                                            }
                                            KeyCode::Enter => {
                                                let payload = popup
                                                    .items
                                                    .get(popup.selected)
                                                    .map(|i| i.payload.clone());
                                                c.popup = None;
                                                if let Some(sql) = payload {
                                                    c.set_text(sql);
                                                }
                                            }
                                            KeyCode::Backspace => {
                                                popup.pop_filter();
                                            }
                                            // Ctrl+D deletes the highlighted saved query.
                                            // (Modifier key, so it never clashes with the
                                            // plain-letter search filter.)
                                            KeyCode::Char('d')
                                                if popup.mode == ConsolePopupMode::Collections
                                                    && key
                                                        .modifiers
                                                        .contains(KeyModifiers::CONTROL) =>
                                            {
                                                let key = popup
                                                    .items
                                                    .get(popup.selected)
                                                    .and_then(|i| i.delete_key.clone());
                                                if let Some((collection, name)) = key
                                                    && self.config.delete_query(&collection, &name)
                                                {
                                                    let _ = self.config.save(&self.config_path);
                                                    self.toasts.push(
                                                        ToastKind::Success,
                                                        format!("deleted '{name}' from '{collection}'"),
                                                    );
                                                    popup.all_items.retain(|i| {
                                                        match &i.delete_key {
                                                            Some((c, n)) => {
                                                                !(c == &collection && n == &name)
                                                            }
                                                            None => true,
                                                        }
                                                    });
                                                    popup.rebuild();
                                                }
                                            }
                                            KeyCode::Char(ch)
                                                if !key.modifiers.contains(KeyModifiers::CONTROL) =>
                                            {
                                                popup.push_filter(ch);
                                            }
                                            _ => {}
                                        }
                                        return;
                                    }
                                    match c.focused_subpane {
                                    ConsoleSubpane::Editor => match key.code {
                                        KeyCode::Up => {
                                            // With suggestions showing, Up/Down cycles the
                                            // highlighted suggestion (wrap-around); otherwise
                                            // they move the text cursor.
                                            if c.autocomplete.is_empty() {
                                                c.move_cursor_up();
                                            } else {
                                                let n = c.autocomplete.len();
                                                c.autocomplete_selected =
                                                    (c.autocomplete_selected + n - 1) % n;
                                            }
                                        }
                                        KeyCode::Down => {
                                            if c.autocomplete.is_empty() {
                                                c.move_cursor_down();
                                            } else {
                                                let n = c.autocomplete.len();
                                                c.autocomplete_selected =
                                                    (c.autocomplete_selected + 1) % n;
                                            }
                                        }
                                        KeyCode::Left => {
                                            c.move_cursor_left();
                                            c.autocomplete.clear();
                                        }
                                        KeyCode::Right => {
                                            c.move_cursor_right();
                                            c.autocomplete.clear();
                                        }
                                        // Ctrl+Space asks for suggestions without
                                        // having to type another character.
                                        KeyCode::Char(' ')
                                            if key.modifiers.contains(KeyModifiers::CONTROL) =>
                                        {
                                            refresh_autocomplete(c, &ac_tables, &ac_columns);
                                            if c.autocomplete.is_empty() {
                                                self.toasts.push(
                                                    ToastKind::Info,
                                                    "no suggestions here".to_string(),
                                                );
                                            }
                                        }
                                        KeyCode::Backspace => {
                                            c.backspace();
                                            refresh_autocomplete(c, &ac_tables, &ac_columns);
                                        }
                                        KeyCode::Delete => {
                                            c.delete_forward();
                                            refresh_autocomplete(c, &ac_tables, &ac_columns);
                                        }
                                        // Home/End and their emacs equivalents,
                                        // which terminals deliver more reliably.
                                        KeyCode::Home => {
                                            c.move_line_start();
                                            c.autocomplete.clear();
                                        }
                                        KeyCode::End => {
                                            c.move_line_end();
                                            c.autocomplete.clear();
                                        }
                                        KeyCode::Char('a')
                                            if key.modifiers.contains(KeyModifiers::CONTROL) =>
                                        {
                                            c.move_line_start();
                                            c.autocomplete.clear();
                                        }
                                        KeyCode::Char('e')
                                            if key.modifiers.contains(KeyModifiers::CONTROL) =>
                                        {
                                            c.move_line_end();
                                            c.autocomplete.clear();
                                        }
                                        // A run chord is handled by the event
                                        // loop; inserting a newline here too
                                        // would leave a stray blank line.
                                        KeyCode::Enter
                                            if key.modifiers.contains(KeyModifiers::CONTROL)
                                                || key.modifiers.contains(KeyModifiers::ALT) => {}
                                        KeyCode::Enter => {
                                            c.insert_newline();
                                            c.autocomplete.clear();
                                        }
                                        // Alt+H: open this connection's query history.
                                        // (Not Ctrl+H — on many terminals Ctrl+H is the
                                        // Backspace erase byte, so it must stay free.)
                                        KeyCode::Char('h') if key.modifiers.contains(KeyModifiers::ALT) => {
                                            let conn = self.active_connection_name.clone().unwrap_or_default();
                                            let items: Vec<ConsolePopupItem> = self
                                                .config
                                                .query_history
                                                .get(&conn)
                                                .cloned()
                                                .unwrap_or_default()
                                                .into_iter()
                                                .map(|q| ConsolePopupItem {
                                                    // Single-line label so a multi-line query
                                                    // doesn't break the list layout.
                                                    label: q.lines().next().unwrap_or("").to_string(),
                                                    payload: q,
                                                    delete_key: None,
                                                })
                                                .collect();
                                            c.autocomplete.clear();
                                            c.popup = Some(ConsolePopup::new(
                                                format!("Query History ({conn})"),
                                                items,
                                                ConsolePopupMode::History,
                                            ));
                                        }
                                        // Alt+F: open the saved-query collections.
                                        // (Ctrl+Shift+F is unreliable: crossterm drops
                                        // the SHIFT modifier on most terminals.)
                                        KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::ALT) => {
                                            let mut items: Vec<ConsolePopupItem> = Vec::new();
                                            for col in &self.config.query_collections {
                                                for q in &col.queries {
                                                    items.push(ConsolePopupItem {
                                                        label: format!("[{}] {}", col.name, q.name),
                                                        payload: q.sql.clone(),
                                                        delete_key: Some((col.name.clone(), q.name.clone())),
                                                    });
                                                }
                                            }
                                            items.sort_by(|a, b| a.label.cmp(&b.label));
                                            c.autocomplete.clear();
                                            c.popup = Some(ConsolePopup::new(
                                                "Saved Queries".to_string(),
                                                items,
                                                ConsolePopupMode::Collections,
                                            ));
                                        }
                                        // Ctrl+S: save the current query into a collection.
                                        KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                            let sql = c.text();
                                            // Don't save a blank editor.
                                            if sql.trim().is_empty() {
                                                self.toasts.push(
                                                    ToastKind::Warning,
                                                    "nothing to save — editor is empty".to_string(),
                                                );
                                                return;
                                            }
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
                                            // Save into the "Default" collection (dedup
                                            // handled inside `save_query`).
                                            let (collection, name) =
                                                self.config.save_query("Default", &base, &sql);
                                            match self.config.save(&self.config_path) {
                                                Ok(_) => self.toasts.push(
                                                    ToastKind::Success,
                                                    format!("saved '{name}' to '{collection}'"),
                                                ),
                                                Err(e) => self.toasts.push(
                                                    ToastKind::Error,
                                                    format!("failed to save query: {e:#}"),
                                                ),
                                            }
                                        }
                                        // Ctrl+W: cycle the auto re-run interval
                                        // (off -> 1s -> 5s -> 15s -> 60s -> off).
                                        KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                            use crate::ui::screens::query::WATCH_INTERVALS;
                                            let secs = c.watch_interval.map(|d| d.as_secs());
                                            let next = match secs {
                                                None => Some(WATCH_INTERVALS[0]),
                                                Some(cur) => WATCH_INTERVALS
                                                    .iter()
                                                    .position(|&s| s == cur)
                                                    .and_then(|i| WATCH_INTERVALS.get(i + 1).copied()),
                                            };
                                            c.watch_interval = next.map(Duration::from_secs);
                                            c.last_run = Some(Instant::now());
                                            match next {
                                                Some(s) => self.toasts.push(
                                                    ToastKind::Info,
                                                    format!("watch: re-running every {s}s"),
                                                ),
                                                None => self
                                                    .toasts
                                                    .push(ToastKind::Info, "watch off".to_string()),
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
                                                c.result_scroll_y = 0;
                                            } else {
                                                switch_tab(exp, -1);
                                            }
                                        }
                                        KeyCode::Char(']') => {
                                            if c.active_result + 1 < c.results.len() {
                                                c.active_result += 1;
                                                c.last_result = c.results.get(c.active_result).cloned();
                                                c.result_selected_row = 0;
                                                c.result_selected_col = 0;
                                                c.result_scroll_x = 0;
                                                c.result_scroll_y = 0;
                                            } else {
                                                switch_tab(exp, 1);
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
                                        // Fn+Up/Down (PageUp/PageDown) scroll one viewport.
                                        KeyCode::PageDown => {
                                            if let Some(res) = &c.last_result
                                                && !res.records.is_empty()
                                            {
                                                let page_rows = c
                                                    .result_hit_area
                                                    .map(|r| (r.height as usize).saturating_sub(2))
                                                    .unwrap_or(10)
                                                    .max(1);
                                                c.result_selected_row = (c.result_selected_row
                                                    + page_rows)
                                                    .min(res.records.len() - 1);
                                            }
                                        }
                                        KeyCode::PageUp => {
                                            let page_rows = c
                                                .result_hit_area
                                                .map(|r| (r.height as usize).saturating_sub(2))
                                                .unwrap_or(10)
                                                .max(1);
                                            c.result_selected_row = c
                                                .result_selected_row
                                                .saturating_sub(page_rows);
                                        }
                                        KeyCode::Left | KeyCode::Char('h') => {
                                            let n = c
                                                .last_result
                                                .as_ref()
                                                .map(|r| r.columns.len())
                                                .unwrap_or(0);
                                            step_column(
                                                &mut c.result_selected_col,
                                                &mut c.result_scroll_x,
                                                n,
                                                c.result_hit_area,
                                                false,
                                            );
                                        }
                                        KeyCode::Right | KeyCode::Char('l') => {
                                            let n = c
                                                .last_result
                                                .as_ref()
                                                .map(|r| r.columns.len())
                                                .unwrap_or(0);
                                            step_column(
                                                &mut c.result_selected_col,
                                                &mut c.result_scroll_x,
                                                n,
                                                c.result_hit_area,
                                                true,
                                            );
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
                                    // `E` writes the diagram out as SVG (from
                                    // flowmaid's own renderer) plus the Mermaid
                                    // source, so it can go straight into docs.
                                    KeyCode::Char('E') => {
                                        match e.export_files() {
                                            Ok(paths) => self.toasts.push(
                                                ToastKind::Success,
                                                format!("exported {paths}"),
                                            ),
                                            Err(err) => self
                                                .toasts
                                                .push(ToastKind::Error, format!("ERD export failed: {err}")),
                                        }
                                    }
                                    // [ / ] switch workspace tabs.
                                    KeyCode::Char('[') => switch_tab(exp, -1),
                                    KeyCode::Char(']') => switch_tab(exp, 1),
                                    KeyCode::Up | KeyCode::Char('k') => e.scroll_up(),
                                    KeyCode::Down | KeyCode::Char('j') => e.scroll_down(),
                                    KeyCode::Left | KeyCode::Char('h') => e.scroll_left(),
                                    KeyCode::Right | KeyCode::Char('l') => e.scroll_right(),
                                    KeyCode::PageDown => e.page_down(),
                                    KeyCode::PageUp => e.page_up(),
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
                                // Nothing left in the workspace, so the tree
                                // has to come back even if it was folded.
                                exp.focus_tree();
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
                    use crate::config::DriverType;
                    // Every implemented driver is offered in the cycle.
                    const CYCLE: [DriverType; 5] = [
                        DriverType::MySql,
                        DriverType::Postgres,
                        DriverType::Sqlite,
                        DriverType::SqlServer,
                        DriverType::Redis, // redis
                    ];
                    let cur = CYCLE.iter().position(|d| *d == form.driver).unwrap_or(0);
                    let delta = if key.code == KeyCode::Left {
                        CYCLE.len() - 1
                    } else {
                        1
                    };
                    let next = CYCLE[(cur + delta) % CYCLE.len()].clone();
                    // Keep a port the user typed themselves; only replace one
                    // that's still a driver default (or blank).
                    let is_default_port = form.port.is_empty()
                        || CYCLE.iter().any(|d| {
                            d.default_port() != 0 && form.port == d.default_port().to_string()
                        });
                    if is_default_port {
                        // SQLite is a file path — it has no port.
                        form.port = match next.default_port() {
                            0 => String::new(),
                            p => p.to_string(),
                        };
                    }
                    form.driver = next;
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
        self.last_frame_area = Some(area);
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

        // Header: identity, then where you are. The config path used to sit
        // here, but "which connection and database am I in" is what actually
        // needs answering while switching tabs.
        let mut header_spans = vec![
            Span::styled("◆ dbx", theme.accent()),
            Span::styled(format!("  v{}", env!("CARGO_PKG_VERSION")), theme.dim()),
        ];
        if let Some(conn) = &self.active_connection_name {
            let ns = self
                .explorer_state
                .as_ref()
                .and_then(|e| e.selected_node().and_then(|n| n.kind.namespace()).cloned())
                .or_else(|| {
                    self.explorer_state
                        .as_ref()
                        .and_then(|e| e.namespaces.first().cloned())
                });
            header_spans.push(Span::styled("   ", theme.dim()));
            header_spans.push(Span::styled(conn.clone(), theme.base()));
            if let Some(ns) = ns {
                header_spans.push(Span::styled(format!(" · {}", ns.0), theme.dim()));
            }
        } else {
            header_spans.push(Span::styled(
                format!("  [{}]", self.config_path.display()),
                theme.dim(),
            ));
        }
        let header = Line::from(header_spans);
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
                    "Select a connection"
                };
                statusbar::render(f, layout.status, context_text, &PICKER_HINTS, theme);
            }
            ScreenMode::Connected => {
                if let Some(exp) = &mut self.explorer_state {
                    explorer::render_explorer(f, layout.body, exp, theme);
                }
                // "S2: Explorer" was an internal screen id from the spec; the
                // status bar should report the connection instead.
                let status = match (&self.active_connection_name, &self.active_driver) {
                    (Some(name), Some(drv)) => {
                        let info = drv.info();
                        let tx = if self.tx_active {
                            " · [TX open — F6 commit / F7 rollback]"
                        } else if !self.autocommit {
                            " · [autocommit off]"
                        } else {
                            ""
                        };
                        format!("● {name} · {} {}{tx}", info.name, info.server_version)
                    }
                    _ => "● not connected".to_string(),
                };
                statusbar::render(f, layout.status, &status, &EXPLORER_HINTS, theme);
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
            help::render(f, area, title, bindings, self.help_scroll, theme);
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

        // Without the enhanced keyboard protocol a terminal cannot report
        // Ctrl+Enter at all — it arrives as a plain Enter, so the console's
        // "run query" chord silently inserts a newline instead. Ask for it
        // where the terminal supports it (kitty, Ghostty, WezTerm, foot);
        // elsewhere F5 and Alt+Enter remain the way to run.
        if matches!(crossterm::terminal::supports_keyboard_enhancement(), Ok(true)) {
            let _ = execute!(
                io::stdout(),
                PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                )
            );
        }
        Ok(guard)
    }

    fn restore() {
        // Popping is harmless when nothing was pushed, and leaving the flags
        // set would confuse the shell we hand the terminal back to.
        let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
        let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        Self::restore();
    }
}

/// Set by the termination-signal handler; the event loop turns it into a
/// normal quit. Never `std::process::exit` from the handler: exit skips
/// destructors, so the `App`'s SshTunnel guard would leak a live ssh
/// forwarder (orphaned, port still bound) on SIGTERM.
static TERMINATION_REQUESTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub async fn run(cli_config: Option<PathBuf>) -> anyhow::Result<()> {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        TerminalGuard::restore();
        default_hook(info);
    }));

    let _guard = TerminalGuard::enter().context("failed to initialize terminal")?;

    tokio::spawn(async {
        // Graceful shutdown on termination: flag the event loop to quit
        // through its normal path — dropping App kills the SSH tunnel and
        // dropping TerminalGuard restores the terminal (raw mode + alternate
        // screen) on the way out. Unix listens for SIGTERM (Ctrl+C already
        // arrives as a key event in raw mode); Windows has no SIGTERM, so it
        // listens for Ctrl+C instead.
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            if let Ok(mut sigterm) = signal(SignalKind::terminate()) {
                sigterm.recv().await;
                TERMINATION_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        #[cfg(windows)]
        {
            if tokio::signal::ctrl_c().await.is_ok() {
                TERMINATION_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst);
            }
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

    // Kick off a one-shot update check off the hot path. The result is polled
    // in the tick below and surfaced as a toast (best-effort, silent on error).
    let (update_tx, update_rx) = tokio::sync::mpsc::channel::<Option<String>>(1);
    tokio::spawn(async move {
        let result = tokio::task::spawn_blocking(crate::update::check_for_update)
            .await
            .ok()
            .flatten();
        let _ = update_tx.send(result).await;
    });
    app.update_check_rx = Some(update_rx);

    while !app.should_quit {
        // SIGTERM arrives as a flag, not a key event — quit through the
        // normal path so App (SSH tunnel) and TerminalGuard drop cleanly.
        if TERMINATION_REQUESTED.load(std::sync::atomic::Ordering::SeqCst) {
            app.should_quit = true;
            continue;
        }
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
                                // The tunnel guard lives only for this test:
                                // it drops (killing ssh) when the block ends.
                                let (eff, _tunnel) = crate::tunnel::SshTunnel::establish(&cfg)
                                    .await
                                    .map_err(|e| format!("tunnel failed: {e:#}"))?;
                                match crate::driver::connect_driver(&eff).await {
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
                                || e.schema_edit_modal.is_some()
                                || e.create_object_modal.is_some()
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
                        app.connect_with_tunnel(&cfg),
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
                            // The old connection took any open transaction
                            // with it — the server rolled it back on drop.
                            if app.tx_active {
                                app.tx_active = false;
                                app.toasts.push(
                                    ToastKind::Warning,
                                    "the open transaction was rolled back by the reconnect".to_string(),
                                );
                            }
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
                            let collection = modal.collection.clone();
                            let confirm_overwrite = modal.confirm_overwrite;

                            // Overwrite guard: if the target file already exists,
                            // ask the user to confirm before clobbering it. The
                            // first Enter flips the flag + warns; the second one
                            // (with the flag set) proceeds. Existence is checked
                            // on the RESOLVED path — the write goes through
                            // resolve_path (which expands `~`), and a literal
                            // "~/..." relative path never exists, so the raw
                            // string would silently bypass this guard.
                            let target_exists = crate::export::resolve_path(&target_path)
                                .map(|p| p.exists())
                                .unwrap_or(false);
                            if !confirm_overwrite && target_exists {
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
                                // A dump includes the table's DDL; fetch it
                                // now — a console result has no source table,
                                // and a failed lookup degrades to INSERTs only.
                                let ddl: Option<String> = if format == ExportFormat::SqlDump {
                                    match &collection {
                                        Some(cref) => match drv.definition(cref).await {
                                            Ok(d) => Some(d),
                                            Err(e) => {
                                                app.toasts.push(
                                                    ToastKind::Warning,
                                                    format!("could not fetch DDL — exporting INSERTs only: {e:#}"),
                                                );
                                                None
                                            }
                                        },
                                        None => None,
                                    }
                                } else {
                                    None
                                };

                                let content: Result<Vec<u8>, String> = match format {
                                    ExportFormat::Csv => {
                                        Ok(Exporter::format_csv(&cols, &recs).into_bytes())
                                    }
                                    ExportFormat::Json => Exporter::format_json(&cols, &recs)
                                        .map(String::into_bytes)
                                        .map_err(|e| format!("JSON export error: {e:#}")),
                                    ExportFormat::SqlInsert => Ok(Exporter::format_sql_insert(
                                        &table_name,
                                        &cols,
                                        &recs,
                                    )
                                    .into_bytes()),
                                    ExportFormat::SqlDump => Ok(Exporter::format_sql_dump(
                                        &table_name,
                                        ddl.as_deref(),
                                        &cols,
                                        &recs,
                                    )
                                    .into_bytes()),
                                    ExportFormat::Xlsx => Exporter::format_xlsx(&cols, &recs)
                                        .map_err(|e| format!("xlsx export error: {e:#}")),
                                };

                                match content {
                                    Ok(bytes) => {
                                        match Exporter::save_bytes_to_file(&target_path, &bytes) {
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
                                        }
                                    }
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
                            let col_idx = edit.col_idx;

                            // No change → skip the UPDATE entirely (don't even
                            // open the SQL confirm). Compare against the ORIGINAL
                            // cell VALUE (not its display string, so a literal
                            // "NULL" string ≠ SQL NULL), and read the row as it
                            // appears in the filtered/sorted view — row_idx
                            // indexes the displayed rows, not page.records in
                            // natural order.
                            let unchanged = exp
                                .active_tab()
                                .map(|tab| match tab {
                                    WorkspaceTab::Table(t) => {
                                        // Same display order the grid shows,
                                        // so `row_idx` means the same thing here.
                                        crate::ui::screens::explorer::visible_records(t)
                                            .get(row_idx)
                                            .and_then(|r| r.values.get(col_idx))
                                            .map(|v| match v {
                                                crate::driver::Value::Null => {
                                                    new_val_str == NULL_SENTINEL
                                                }
                                                other => {
                                                    other.display_str() == new_val_str
                                                        && new_val_str != NULL_SENTINEL
                                                }
                                            })
                                            .unwrap_or(false)
                                    }
                                    _ => false,
                                })
                                .unwrap_or(false);
                            if unchanged {
                                exp.cell_edit_modal = None;
                                app.toasts.push(
                                    ToastKind::Info,
                                    "value unchanged — no update needed".to_string(),
                                );
                                continue;
                            }

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
                            // stays a regular string. Boolean columns emit the
                            // bare `true`/`false` keyword (PG + MySQL accept it),
                            // not a quoted string.
                            let assignment = if edit.is_boolean && new_val_str != NULL_SENTINEL {
                                new_val_str.clone()
                            } else {
                                render_buffer_sql(&new_val_str)
                            };

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
                        && exp.schema_edit_modal.is_none()
                        && exp.create_object_modal.is_none()
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

                    // Schema edit: Enter applies the ALTER via the SQL-confirm
                    // modal (which splits + executes each statement).
                    // Enter only applies the ALTER when NOT typing in an input
                    // (add/rename/change-type) — handle_key advances those.
                    if exp
                        .schema_edit_modal
                        .as_ref()
                        .is_some_and(|m| m.input.is_none())
                        && key.code == KeyCode::Enter
                    {
                        let sql = {
                            let s = exp.schema_edit_modal.as_ref().unwrap();
                            generate_alter_sql(
                                &s.collection,
                                &s.drop_cols,
                                &s.add_cols,
                                &s.type_changes,
                                s.rename_table.as_deref(),
                                &drv.info().name,
                            )
                        };
                        match sql {
                            Some(sql) => {
                                let s = exp.schema_edit_modal.as_ref().unwrap();
                                exp.sql_confirm_modal = Some(
                                    crate::ui::screens::explorer::SqlConfirmModalState {
                                        collection: s.collection.clone(),
                                        sql_query: sql,
                                        row_idx: 0,
                                    },
                                );
                            }
                            None => {
                                app.toasts.push(
                                    ToastKind::Warning,
                                    "no schema changes to apply".to_string(),
                                );
                            }
                        }
                        exp.schema_edit_modal = None;
                        continue;
                    }

                    // Create object: Enter generates a CREATE statement into the
                    // SQL-confirm modal (name must be non-empty, and the kind
                    // must be supported by this driver).
                    if exp.create_object_modal.is_some() && key.code == KeyCode::Enter {
                        let sql = {
                            let c = exp.create_object_modal.as_ref().unwrap();
                            if c.name.trim().is_empty() {
                                None
                            } else {
                                generate_create_sql(
                                    &c.namespace,
                                    c.kind,
                                    &c.name,
                                    &drv.info().name,
                                )
                            }
                        };
                        match sql {
                            Some(sql) => {
                                let c = exp.create_object_modal.as_ref().unwrap();
                                exp.sql_confirm_modal = Some(
                                    crate::ui::screens::explorer::SqlConfirmModalState {
                                        collection: crate::driver::CollectionRef {
                                            namespace: c.namespace.clone(),
                                            name: c.name.clone(),
                                        },
                                        sql_query: sql,
                                        row_idx: 0,
                                    },
                                );
                            }
                            None => {
                                app.toasts.push(
                                    ToastKind::Warning,
                                    "enter a name (or this kind isn't supported by the driver)".to_string(),
                                );
                            }
                        }
                        exp.create_object_modal = None;
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

                    // Esc while a query runs cancels it. Checked before every
                    // other Esc handler so the running query wins.
                    if key.code == KeyCode::Esc
                        && let Some(run) = app.query_run.take()
                    {
                        run.handle.abort();
                        if let Some(WorkspaceTab::Console(c)) = exp.tabs.get_mut(run.tab) {
                            c.is_executing = false;
                            c.execution_error = Some(
                                "cancelled — the server may still be finishing this statement"
                                    .to_string(),
                            );
                        }
                        // Refresh the tx mirror from the REAL driver state:
                        // the aborted task may have already opened a
                        // transaction, and a stale `false` here would brick
                        // the console (new runs fail "already open", F6/F7
                        // refuse "no transaction") until a reconnect.
                        if let Some(drv) = &app.active_driver {
                            app.tx_active = drv.in_tx().await;
                        }
                        app.toasts
                            .push(ToastKind::Info, "query cancelled".to_string());
                        continue;
                    }

                    // Schema diff overlay: scroll / copy / close.
                    if exp.schema_diff.is_some() {
                        match key.code {
                            KeyCode::Esc => exp.schema_diff = None,
                            KeyCode::Down | KeyCode::Char('j') => {
                                if let Some(d) = exp.schema_diff.as_mut() {
                                    let last = d.diffs.len().saturating_sub(1);
                                    if d.scroll < last {
                                        d.scroll += 1;
                                    }
                                }
                            }
                            KeyCode::Up | KeyCode::Char('k') => {
                                if let Some(d) = exp.schema_diff.as_mut() {
                                    d.scroll = d.scroll.saturating_sub(1);
                                }
                            }
                            KeyCode::Char('y') | KeyCode::Char('Y') => {
                                let sql = exp
                                    .schema_diff
                                    .as_ref()
                                    .map(|d| d.migration.clone())
                                    .unwrap_or_default();
                                if sql.trim().is_empty() {
                                    app.toasts.push(
                                        ToastKind::Info,
                                        "no migration needed".to_string(),
                                    );
                                } else {
                                    match ClipboardManager::set_text(&sql) {
                                        Ok(_) => app.toasts.push(
                                            ToastKind::Success,
                                            "migration SQL copied to clipboard".to_string(),
                                        ),
                                        Err(e) => app.toasts.push(ToastKind::Error, e),
                                    }
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // Schema-diff connection picker: Enter connects to the
                    // chosen connection just long enough to read its schema.
                    if exp.diff_picker.is_some() {
                        match key.code {
                            KeyCode::Esc => exp.diff_picker = None,
                            KeyCode::Down | KeyCode::Char('j') => {
                                if let Some(p) = exp.diff_picker.as_mut() {
                                    p.selected = (p.selected + 1)
                                        .min(p.connections.len().saturating_sub(1));
                                }
                            }
                            KeyCode::Up | KeyCode::Char('k') => {
                                if let Some(p) = exp.diff_picker.as_mut() {
                                    p.selected = p.selected.saturating_sub(1);
                                }
                            }
                            KeyCode::Enter => {
                                let target_name = exp
                                    .diff_picker
                                    .as_ref()
                                    .and_then(|p| p.connections.get(p.selected).cloned());
                                exp.diff_picker = None;
                                let Some(target_name) = target_name else {
                                    continue;
                                };
                                let Some(cfg) = app
                                    .config
                                    .connections
                                    .iter()
                                    .find(|c| c.name == target_name)
                                    .cloned()
                                else {
                                    continue;
                                };
                                let Some(ns) = exp
                                    .selected_node()
                                    .and_then(|n| n.kind.namespace().cloned())
                                    .or_else(|| exp.namespaces.first().cloned())
                                else {
                                    app.toasts.push(
                                        ToastKind::Warning,
                                        "no schema selected to compare".to_string(),
                                    );
                                    continue;
                                };

                                app.toasts.push(
                                    ToastKind::Info,
                                    format!("comparing {} with '{target_name}'...", ns.0),
                                );
                                // The second connection is transient: it lives
                                // only for this comparison, so the app keeps
                                // its single-active-driver model. Its tunnel
                                // (if any) dies with the comparison as well.
                                let (eff_cfg, _other_tunnel) =
                                    match crate::tunnel::SshTunnel::establish(&cfg).await {
                                        Ok(pair) => pair,
                                        Err(e) => {
                                            app.toasts.push(
                                                ToastKind::Error,
                                                format!("tunnel to '{target_name}' failed: {e:#}"),
                                            );
                                            continue;
                                        }
                                    };
                                let other = match crate::driver::connect_driver(&eff_cfg).await {
                                    Ok(d) => d,
                                    Err(e) => {
                                        app.toasts.push(
                                            ToastKind::Error,
                                            format!("could not connect to '{target_name}': {e:#}"),
                                        );
                                        continue;
                                    }
                                };
                                let source = collect_schema(drv, &ns).await;
                                let target = collect_schema(&other, &ns).await;
                                let diffs = crate::schema_diff::diff_schemas(&source, &target);
                                let migration = crate::schema_diff::migration_sql(
                                    &diffs,
                                    &ns.0,
                                    &drv.info().name,
                                );
                                exp.schema_diff =
                                    Some(crate::ui::screens::explorer::SchemaDiffState {
                                        against: target_name,
                                        namespace: ns.0.clone(),
                                        scroll: 0,
                                        diffs,
                                        migration,
                                    });
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // Alt+D opens the schema-diff connection picker.
                    if key.modifiers.contains(KeyModifiers::ALT)
                        && matches!(key.code, KeyCode::Char('d') | KeyCode::Char('D'))
                    {
                        let others: Vec<String> = app
                            .config
                            .connections
                            .iter()
                            .map(|c| c.name.clone())
                            .filter(|n| Some(n) != app.active_connection_name.as_ref())
                            .collect();
                        if others.is_empty() {
                            app.toasts.push(
                                ToastKind::Warning,
                                "add another saved connection to compare against".to_string(),
                            );
                        } else {
                            exp.diff_picker =
                                Some(crate::ui::screens::explorer::DiffPickerState {
                                    connections: others,
                                    selected: 0,
                                });
                        }
                        continue;
                    }

                    // Running-query monitor: open with Ctrl+K, then x cancels
                    // the highlighted query and r refreshes the snapshot.
                    if exp.process_list.is_some() {
                        match key.code {
                            KeyCode::Esc => {
                                exp.process_list = None;
                                continue;
                            }
                            KeyCode::Up | KeyCode::Char('k') => {
                                if let Some(p) = exp.process_list.as_mut() {
                                    p.selected = p.selected.saturating_sub(1);
                                }
                                continue;
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                if let Some(p) = exp.process_list.as_mut() {
                                    p.selected = (p.selected + 1)
                                        .min(p.result.records.len().saturating_sub(1));
                                }
                                continue;
                            }
                            KeyCode::Char('x') | KeyCode::Char('X') => {
                                let pid = exp.process_list.as_ref().and_then(|p| p.selected_pid());
                                match pid {
                                    Some(pid) => match drv.kill_process(&pid).await {
                                        Ok(_) => {
                                            app.toasts.push(
                                                ToastKind::Success,
                                                format!("cancelled query {pid}"),
                                            );
                                            // Re-read so the list reflects it.
                                            if let Ok(res) = drv.process_list().await {
                                                exp.process_list = Some(
                                                    crate::ui::screens::explorer::ProcessListState {
                                                        selected: 0,
                                                        result: res,
                                                    },
                                                );
                                            }
                                        }
                                        Err(e) => app
                                            .toasts
                                            .push(ToastKind::Error, format!("cancel failed: {e:#}")),
                                    },
                                    None => app.toasts.push(
                                        ToastKind::Warning,
                                        "no query selected".to_string(),
                                    ),
                                }
                                continue;
                            }
                            KeyCode::Char('r') | KeyCode::Char('R') => {
                                match drv.process_list().await {
                                    Ok(res) => {
                                        exp.process_list = Some(
                                            crate::ui::screens::explorer::ProcessListState {
                                                selected: 0,
                                                result: res,
                                            },
                                        )
                                    }
                                    Err(e) => app
                                        .toasts
                                        .push(ToastKind::Error, format!("refresh failed: {e:#}")),
                                }
                                continue;
                            }
                            _ => continue,
                        }
                    }
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('k')
                    {
                        if !exp
                            .driver_capabilities
                            .contains(crate::driver::Capabilities::PROCESS_LIST)
                        {
                            app.toasts.push(
                                ToastKind::Warning,
                                "this driver has no server-side query list".to_string(),
                            );
                            continue;
                        }
                        match drv.process_list().await {
                            Ok(res) => {
                                exp.process_list =
                                    Some(crate::ui::screens::explorer::ProcessListState {
                                        selected: 0,
                                        result: res,
                                    })
                            }
                            Err(e) => app.toasts.push(
                                ToastKind::Error,
                                format!("failed to list running queries: {e:#}"),
                            ),
                        }
                        continue;
                    }

                    // Ctrl+P: EXPLAIN the console's query and show the plan.
                    if exp.explain_plan.is_none()
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('p')
                        && let Some(WorkspaceTab::Console(c)) = exp.active_tab()
                        && c.popup.is_none()
                    {
                        let query = c.text();
                        if query.trim().is_empty() {
                            app.toasts
                                .push(ToastKind::Warning, "nothing to explain".to_string());
                            continue;
                        }
                        if !exp
                            .driver_capabilities
                            .contains(crate::driver::Capabilities::EXPLAIN)
                        {
                            app.toasts.push(
                                ToastKind::Warning,
                                "this driver does not support EXPLAIN".to_string(),
                            );
                            continue;
                        }
                        let ns = exp
                            .selected_node()
                            .and_then(|n| n.kind.namespace().cloned())
                            .or_else(|| exp.namespaces.first().cloned());
                        let Some(ns) = ns else {
                            app.toasts
                                .push(ToastKind::Warning, "no database selected".to_string());
                            continue;
                        };
                        let sql = crate::explain::explain_sql(&drv.info().name, &query);
                        match drv.execute(&ns, &sql).await {
                            Ok(res) => {
                                let nodes = crate::explain::parse_plan(&drv.info().name, &res);
                                if nodes.is_empty() {
                                    app.toasts.push(
                                        ToastKind::Info,
                                        "EXPLAIN returned no plan rows".to_string(),
                                    );
                                } else {
                                    exp.explain_plan = Some(
                                        crate::ui::screens::explorer::ExplainPlanState {
                                            hotspot: crate::explain::hotspot(&nodes),
                                            nodes,
                                            scroll: 0,
                                        },
                                    );
                                }
                            }
                            Err(e) => app
                                .toasts
                                .push(ToastKind::Error, format!("EXPLAIN failed: {e:#}")),
                        }
                        continue;
                    }

                    // ERD node context menu: Enter runs the highlighted action.
                    if exp.erd_menu.is_some() && key.code == KeyCode::Enter {
                        let (cref, selected) = {
                            let m = exp.erd_menu.as_ref().unwrap();
                            (m.collection.clone(), m.selected)
                        };
                        exp.erd_menu = None;
                        let page_size = app.config.effective_page_size();
                        run_erd_menu_action(
                            exp,
                            drv,
                            &mut app.toasts,
                            page_size,
                            cref,
                            selected,
                        )
                        .await;
                        continue;
                    }

                    // Tree context menu: Enter runs the highlighted action.
                    if exp.tree_menu.is_some() && key.code == KeyCode::Enter {
                        let (cref, selected) = {
                            let m = exp.tree_menu.as_ref().unwrap();
                            (m.collection.clone(), m.selected)
                        };
                        exp.tree_menu = None;
                        let page_size = app.config.effective_page_size();
                        run_erd_menu_action(
                            exp,
                            drv,
                            &mut app.toasts,
                            page_size,
                            cref,
                            selected,
                        )
                        .await;
                        continue;
                    }

                    // Enter on an ERD tab with a keyboard-selected node
                    // (`.`/`,` moves the selection) opens its context menu —
                    // the same path as a mouse click on the node.
                    if exp.ddl_popup.is_none()
                        && exp.erd_menu.is_none()
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
                            exp.erd_menu = Some(crate::ui::screens::explorer::ErdMenuState {
                                collection: cref,
                                selected: 0,
                                menu_at: None,
                            });
                            continue;
                        }
                    }

                    if exp.ddl_popup.is_none()
                        && exp.schema_edit_modal.is_none()
                        && exp.create_object_modal.is_none()
                        && exp.sql_confirm_modal.is_none()
                        && exp.object_search.is_none()
                        // A context menu must capture the keyboard: without
                        // this gate, e/a/g/Space would reach the tree handler
                        // BEHIND the open menu, stacking overlays and firing
                        // actions the user never aimed at.
                        && exp.tree_menu.is_none()
                        && exp.erd_menu.is_none()
                        && exp.focused_pane == FocusedPane::Tree
                    {
                        match key.code {
                            KeyCode::Char('a') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                                // Create an object: on a schema node the default
                                // is a new schema; on a table node, an object in
                                // that schema.
                                if let Some(node) = exp.selected_node() {
                                    match &node.kind {
                                        TreeNodeKind::Database(ns) => {
                                            exp.create_object_modal = Some(
                                                crate::ui::screens::explorer::CreateObjectModalState {
                                                    namespace: ns.clone(),
                                                    kind: crate::ui::screens::explorer::CreateKind::Schema,
                                                    name: String::new(),
                                                },
                                            );
                                        }
                                        TreeNodeKind::Table(cref, _, _) => {
                                            exp.create_object_modal = Some(
                                                crate::ui::screens::explorer::CreateObjectModalState {
                                                    namespace: cref.namespace.clone(),
                                                    kind: crate::ui::screens::explorer::CreateKind::Table,
                                                    name: String::new(),
                                                },
                                            );
                                        }
                                        _ => {}
                                    }
                                }
                                continue;
                            }
                            KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                // Ctrl+O: context menu for the selected table —
                                // same actions as clicking a node in the ERD.
                                if let Some(node) = exp.selected_node() {
                                    if let TreeNodeKind::Table(cref, _, _) = &node.kind {
                                        exp.tree_menu = Some(
                                            crate::ui::screens::explorer::ErdMenuState {
                                                collection: cref.clone(),
                                                selected: 0,
                                                menu_at: None,
                                            },
                                        );
                                    }
                                }
                                continue;
                            }
                            KeyCode::Char(' ') | KeyCode::Enter => {
                                let page_size = app.config.effective_page_size();
                                let err =
                                    open_tree_node(exp, drv, &mut app.toasts, page_size).await;
                                // A dead connection gets one reconnect;
                                // anything else is reported. No automatic
                                // retry: the reconnect rebuilt the tree with
                                // the selection reset, so re-running would
                                // open the FIRST node, not the user's.
                                if let Some(err) = err {
                                    if app.try_reconnect(&err).await {
                                        app.toasts.push(
                                            ToastKind::Info,
                                            "reconnected — open the node again to retry"
                                                .to_string(),
                                        );
                                    } else {
                                        app.toasts.push(ToastKind::Error, err);
                                    }
                                }
                            }
                            KeyCode::Char('e') => {
                                // Edit schema (ALTER). Gated on the DDL
                                // capability so a NoSQL driver without
                                // structured schema just explains instead of
                                // offering a broken ALTER flow.
                                if let Some(node) = exp.selected_node() {
                                    let cref = match &node.kind {
                                        TreeNodeKind::Table(cref, _, _) => cref.clone(),
                                        _ => continue,
                                    };
                                    if !exp
                                        .driver_capabilities
                                        .contains(crate::driver::Capabilities::DDL)
                                    {
                                        app.toasts.push(
                                            ToastKind::Warning,
                                            "this driver does not support editing schema".to_string(),
                                        );
                                        continue;
                                    }
                                    let drv_clone = drv.clone();
                                    if let Ok(meta) = drv_clone.collection_meta(&cref).await {
                                        exp.schema_edit_modal = Some(
                                            crate::ui::screens::explorer::SchemaEditModalState {
                                                collection: cref,
                                                columns: meta.columns,
                                                selected: 0,
                                                drop_cols: Vec::new(),
                                                add_cols: Vec::new(),
                                                type_changes: Vec::new(),
                                                rename_table: None,
                                                input: None,
                                            },
                                        );
                                    }
                                }
                                continue;
                            }
                            KeyCode::F(1) => {
                                if let Some(node) = exp.selected_node() {
                                    let cref = match &node.kind {
                                        TreeNodeKind::Table(cref, _, _)
                                        | TreeNodeKind::View(cref)
                                        | TreeNodeKind::Routine(cref)
                                        | TreeNodeKind::Sequence(cref) => cref.clone(),
                                        TreeNodeKind::Database(_) | TreeNodeKind::Section(..) => {
                                            continue;
                                        }
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
                                    let Some(target_ns) = node.kind.namespace().cloned() else {
                                        continue;
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
                        // Ctrl+Enter only reaches us on terminals that speak
                        // the enhanced keyboard protocol, so Alt+Enter (a plain
                        // ESC-prefixed CR) and F5 are kept as equals rather than
                        // as second-class fallbacks.
                        let is_run_chord = key.code == KeyCode::Enter
                            && (key.modifiers.contains(KeyModifiers::CONTROL)
                                || key.modifiers.contains(KeyModifiers::ALT));
                        let is_ctrl_enter = is_run_chord;
                        let is_f5 = key.code == KeyCode::F(5);

                        let tab_is_console = exp
                            .active_tab()
                            .is_some_and(|t| matches!(t, WorkspaceTab::Console(_)));

                        // Ctrl+T toggles autocommit. Refused while a
                        // transaction is open — commit or roll it back first,
                        // so the toggle can never silently decide the fate of
                        // pending writes.
                        if tab_is_console
                            && key.code == KeyCode::Char('t')
                            && key.modifiers.contains(KeyModifiers::CONTROL)
                        {
                            // Gate on the real driver state, not the mirror —
                            // the mirror can go stale (e.g. a cancelled run
                            // that had already opened a transaction).
                            app.tx_active = drv.in_tx().await;
                            if app.tx_active {
                                app.toasts.push(
                                    ToastKind::Warning,
                                    "a transaction is open — F6 to commit, F7 to roll back".to_string(),
                                );
                            } else {
                                app.autocommit = !app.autocommit;
                                let state = if app.autocommit {
                                    "on".to_string()
                                } else {
                                    "off — console queries now open a transaction".to_string()
                                };
                                app.toasts.push(ToastKind::Info, format!("autocommit {state}"));
                            }
                            continue;
                        }

                        // F6 commits / F7 rolls back the open transaction.
                        if tab_is_console && matches!(key.code, KeyCode::F(6) | KeyCode::F(7)) {
                            // Same reason as Ctrl+T: trust the driver, not the
                            // mirror, so a stale mirror can't brick the tx.
                            app.tx_active = drv.in_tx().await;
                            if !app.tx_active {
                                app.toasts.push(
                                    ToastKind::Info,
                                    "no transaction is open".to_string(),
                                );
                            } else {
                                let commit = key.code == KeyCode::F(6);
                                let result = if commit {
                                    drv.commit_tx().await
                                } else {
                                    drv.rollback_tx().await
                                };
                                match result {
                                    Ok(()) => app.toasts.push(
                                        ToastKind::Success,
                                        if commit {
                                            "transaction committed".to_string()
                                        } else {
                                            "transaction rolled back".to_string()
                                        },
                                    ),
                                    Err(e) => app
                                        .toasts
                                        .push(ToastKind::Error, format!("{e:#}")),
                                }
                                app.tx_active = drv.in_tx().await;
                            }
                            continue;
                        }

                        // Don't run the query under a history/favorites popup —
                        // Ctrl+Enter is muscle memory for "run" but the popup
                        // owns the keystroke until Esc/Enter dismisses it.
                        if (is_ctrl_enter || is_f5)
                            && exp.active_tab().is_some_and(|t| match t {
                                WorkspaceTab::Console(c) => c.popup.is_none(),
                                _ => true,
                            })
                        {
                            start_console_query(exp, drv, &mut app.toasts, &mut app.query_run, !app.autocommit);
                            if let Some(WorkspaceTab::Console(c)) = exp.active_tab_mut() {
                                c.last_run = Some(Instant::now());
                            }
                        } else {
                            // Workspace-level edit/navigation keys. They only
                            // make sense on a Table tab — inside a console
                            // editor those letters are ordinary text — so route
                            // them to `handle_key` when the active tab isn't a
                            // Table (or, for `c`, when the console owns the key).
                            let active_is_table = exp
                                .active_tab()
                                .is_some_and(|t| matches!(t, WorkspaceTab::Table(_)));
                            let active_is_console = exp
                                .active_tab()
                                .is_some_and(|t| matches!(t, WorkspaceTab::Console(_)));
                            // While a table tab is in one of its own input
                            // modes, every key belongs to that mode: letters
                            // must reach the search/filter buffer instead of
                            // firing insert / delete / paging, and the
                            // row-detail overlay must not sit over a modal it
                            // did not open. Searching for "index" would
                            // otherwise insert a row, page forward and open the
                            // delete dialog.
                            let table_input_mode = exp.active_tab().is_some_and(|t| match t {
                                WorkspaceTab::Table(t) => {
                                    t.search_editing || t.filter_editing || t.row_detail
                                }
                                _ => false,
                            });
                            if table_input_mode {
                                app.handle_key(key);
                                continue;
                            }
                            match key.code {
                                // `x` on a selected row → confirm + DELETE the row.
                                // We need the active driver for the driver-name sniff
                                // (`quote_ident` + `single_row_suffix`) so this lives
                                // here in the async event loop, not in `handle_key`.
                                KeyCode::Char('x') | KeyCode::Char('X') if active_is_table => {
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
                                        // `selected_row` indexes the DISPLAYED
                                        // rows; reading page.records directly
                                        // would target a different row whenever
                                        // a filter or sort is active — and this
                                        // one builds a DELETE.
                                        let row = crate::ui::screens::explorer::visible_records(t)
                                            .get(t.selected_row)
                                            .copied()?;
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
                                // `f` on a foreign-key cell opens the row it
                                // references. A console tab (rather than a
                                // filtered table tab) is used so the parent row
                                // is found even when it sits on a later page.
                                KeyCode::Char('f')
                                    if active_is_table
                                        && !key.modifiers.contains(KeyModifiers::CONTROL) =>
                                {
                                    let plan: Option<(String, String)> = (|| {
                                        let WorkspaceTab::Table(t) = exp.active_tab()? else {
                                            return None;
                                        };
                                        let col = t.page.columns.get(t.selected_col)?;
                                        let fk = t.foreign_keys.iter().find(|f| &f.column == col)?;
                                        let row = crate::ui::screens::explorer::visible_records(t)
                                            .get(t.selected_row)
                                            .copied()?;
                                        let value = row.values.get(t.selected_col)?;
                                        // A NULL foreign key references nothing.
                                        if matches!(value, crate::driver::Value::Null) {
                                            return None;
                                        }
                                        let driver_name = drv.info().name.clone();
                                        let q_ns = quote_ident(&fk.ref_namespace.0, &driver_name);
                                        let q_tbl = quote_ident(&fk.ref_table, &driver_name);
                                        let q_col = quote_ident(&fk.ref_column, &driver_name);
                                        let literal = render_value_sql(value);
                                        Some((
                                            format!("{}.{}", fk.ref_table, fk.ref_column),
                                            format!(
                                                "SELECT * FROM {q_ns}.{q_tbl} WHERE {q_col} = {literal};"
                                            ),
                                        ))
                                    })();
                                    match plan {
                                        Some((target, sql)) => {
                                            let title = format!("→ {target}");
                                            exp.tabs.push(WorkspaceTab::Console(
                                                QueryConsole::new(title, Some(&sql)),
                                            ));
                                            exp.active_tab_index = exp.tabs.len().saturating_sub(1);
                                            exp.focused_pane = FocusedPane::Workspace;
                                            start_console_query(
                                                exp,
                                                drv,
                                                &mut app.toasts,
                                                &mut app.query_run,
                                                !app.autocommit,
                                            );
                                        }
                                        None => app.toasts.push(
                                            ToastKind::Info,
                                            "not a foreign-key cell (or the value is NULL)".to_string(),
                                        ),
                                    }
                                }
                                // `F` is the reverse of `f`: find every row in
                                // this schema that points AT the selected one.
                                // Referencing tables are unknown up front, so
                                // the whole schema's metadata is scanned once.
                                KeyCode::Char('F')
                                    if active_is_table
                                        && !key.modifiers.contains(KeyModifiers::CONTROL) =>
                                {
                                    let here: Option<(crate::driver::CollectionRef, String, String)> =
                                        (|| {
                                            let WorkspaceTab::Table(t) = exp.active_tab()? else {
                                                return None;
                                            };
                                            let col = t.page.columns.get(t.selected_col)?.clone();
                                            let row =
                                                crate::ui::screens::explorer::visible_records(t)
                                                    .get(t.selected_row)
                                                    .copied()?;
                                            let value = row.values.get(t.selected_col)?;
                                            // NULL is referenced by nothing.
                                            if matches!(value, crate::driver::Value::Null) {
                                                return None;
                                            }
                                            Some((
                                                t.collection.clone(),
                                                col,
                                                render_value_sql(value),
                                            ))
                                        })();
                                    let Some((cref, col, literal)) = here else {
                                        app.toasts.push(
                                            ToastKind::Info,
                                            "select a non-NULL cell to find references to it"
                                                .to_string(),
                                        );
                                        continue;
                                    };

                                    app.toasts.push(
                                        ToastKind::Info,
                                        format!("scanning {} for references...", cref.namespace.0),
                                    );
                                    let driver_name = drv.info().name.clone();
                                    // One catalog query on the server-backed
                                    // drivers; the trait default only falls back
                                    // to per-table walking where there is no
                                    // catalog (SQLite, a local file).
                                    let all_fks = match drv
                                        .schema_foreign_keys(&cref.namespace)
                                        .await
                                    {
                                        Ok(v) => v,
                                        Err(e) => {
                                            app.toasts.push(
                                                ToastKind::Error,
                                                format!("could not read foreign keys: {e:#}"),
                                            );
                                            continue;
                                        }
                                    };
                                    // Every (child table, child column) whose FK
                                    // targets the cell we're standing on.
                                    let refs: Vec<(String, String)> = all_fks
                                        .into_iter()
                                        .filter(|(_, fk)| {
                                            fk.ref_table == cref.name && fk.ref_column == col
                                        })
                                        .map(|(table, fk)| (table, fk.column))
                                        .collect();

                                    if refs.is_empty() {
                                        app.toasts.push(
                                            ToastKind::Info,
                                            format!("nothing references {}.{col}", cref.name),
                                        );
                                        continue;
                                    }

                                    // One statement per referencing table: the
                                    // console already shows multi-statement runs
                                    // as separate result sets ([ / ] to switch).
                                    let q_ns = quote_ident(&cref.namespace.0, &driver_name);
                                    let sql = refs
                                        .iter()
                                        .map(|(table, fk_col)| {
                                            format!(
                                                "SELECT * FROM {q_ns}.{} WHERE {} = {literal};",
                                                quote_ident(table, &driver_name),
                                                quote_ident(fk_col, &driver_name)
                                            )
                                        })
                                        .collect::<Vec<_>>()
                                        .join("\n");

                                    let title = format!("← refs to {}.{col}", cref.name);
                                    exp.tabs.push(WorkspaceTab::Console(QueryConsole::new(
                                        title,
                                        Some(&sql),
                                    )));
                                    exp.active_tab_index = exp.tabs.len().saturating_sub(1);
                                    exp.focused_pane = FocusedPane::Workspace;
                                    start_console_query(
                                        exp,
                                        drv,
                                        &mut app.toasts,
                                        &mut app.query_run,
                                        !app.autocommit,
                                    );
                                }
                                // `i` → open the INSERT-row modal. All fields start
                                // in the "skip" state so the user can opt in to
                                // providing values one column at a time.
                                KeyCode::Char('i') | KeyCode::Char('I')
                                    if active_is_table
                                        && !key.modifiers.contains(KeyModifiers::CONTROL) =>
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
                                KeyCode::Char('c') if !active_is_console => {
                                    let count = exp.tabs.iter().filter(|t| matches!(t, WorkspaceTab::Console(_))).count() + 1;
                                    let console_title = format!("console_{count}.sql");
                                    exp.tabs.push(WorkspaceTab::Console(QueryConsole::new(console_title, None)));
                                    exp.active_tab_index = exp.tabs.len().saturating_sub(1);
                                    exp.focused_pane = FocusedPane::Workspace;
                                    app.toasts.push(ToastKind::Info, "opened new SQL console".to_string());
                                }
                                KeyCode::Char('n') if active_is_table => {
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
                                KeyCode::Char('p') if active_is_table => {
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
                // Test connection 't' key: spawn and poll on tick — awaiting
                // inline would freeze the event loop, and the spawned task
                // needs its own ceiling because an SSH tunnel alone may take
                // 10s to come up.
                if !app.help_open && app.form_modal.is_none() && key.code == KeyCode::Char('t') {
                    if app.picker_test_rx.is_some() {
                        app.toasts.push(ToastKind::Info, "a test is already running".to_string());
                    } else if let Some(cfg) = app.config.connections.get(app.selected_connection).cloned() {
                        app.toasts.push(ToastKind::Info, format!("testing ping to '{}'...", cfg.name));
                        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Duration, String>>(1);
                        tokio::spawn(async move {
                            let test = async {
                                // Transient tunnel: established for the test
                                // and killed when the closure ends.
                                let (eff, _tunnel) = crate::tunnel::SshTunnel::establish(&cfg)
                                    .await
                                    .map_err(|e| format!("tunnel failed: {e:#}"))?;
                                let driver = crate::driver::connect_driver(&eff)
                                    .await
                                    .map_err(|e| format!("connect failed: {e:#}"))?;
                                driver
                                    .ping()
                                    .await
                                    .map_err(|e| format!("ping failed: {e:#}"))
                            };
                            // Hard ceiling so a hung server can't hold the
                            // forwarder (or the channel) forever.
                            let res = match tokio::time::timeout(Duration::from_secs(30), test).await {
                                Ok(res) => res,
                                Err(_) => Err("ping timed out (30s)".to_string()),
                            };
                            let _ = tx.send(res).await;
                        });
                        app.picker_test_rx = Some(rx);
                    }
                } else if !app.help_open && app.form_modal.is_none() && key.code == KeyCode::Enter {
                    if let Some(cfg) = app.config.connections.get(app.selected_connection).cloned() {
                        app.connecting = true;
                        app.toasts.push(ToastKind::Info, format!("connecting to '{}'...", cfg.name));
                        match app.connect_with_tunnel(&cfg).await {
                            Ok(drv) => {
                                let info = drv.info();
                                let capabilities = drv.capabilities();
                                let namespaces = drv.namespaces().await.unwrap_or_default();
                                app.explorer_state = Some(ExplorerState::new(namespaces, capabilities));
                                app.active_driver = Some(drv);
                                app.active_connection_name = Some(cfg.name.clone());
                                app.active_connection = Some(cfg.clone());
                                // A fresh driver has no transaction state — reset
                                // the console's tx mode along with the connection.
                                app.autocommit = true;
                                app.tx_active = false;
                                app.mode = ScreenMode::Connected;
                                let via = app
                                    .ssh_tunnel
                                    .as_ref()
                                    .map(|t| format!(" (ssh tunnel :{})", t.local_port))
                                    .unwrap_or_default();
                                app.toasts.push(ToastKind::Success, format!("connected: {} {}{via}", info.name, info.server_version));
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

            // A finished background query lands here rather than blocking the
            // loop, so the UI kept drawing (and accepting Esc) while it ran.
            if let Some(run) = &mut app.query_run {
                match run.rx.try_recv() {
                    Ok(outcome) => {
                        let (tab, text, is_retry, active_ns) = (
                            run.tab,
                            run.query_text.clone(),
                            run.is_retry,
                            run.active_ns.clone(),
                        );
                        app.query_run = None;
                        // A dropped connection gets one automatic
                        // reconnect-and-retry; a retry that fails (or any
                        // other error) is just reported. Only single-statement
                        // runs are retried: re-running a script would re-apply
                        // the statements that already committed before the
                        // connection dropped.
                        let single_statement = crate::ui::screens::query::split_statements(&text)
                            .into_iter()
                            .filter(|s| !crate::ui::screens::query::is_comment_only(s))
                            .count()
                            == 1;
                        let retried = match &outcome {
                            Err(err) if !is_retry && single_statement && app.try_reconnect(err).await => {
                                if let (Some(exp), Some(drv)) =
                                    (&mut app.explorer_state, &app.active_driver)
                                {
                                    retry_console_query(
                                        exp,
                                        drv,
                                        &mut app.query_run,
                                        tab,
                                        &text,
                                        active_ns,
                                        !app.autocommit,
                                    );
                                }
                                true
                            }
                            _ => false,
                        };
                        if !retried {
                            if let Some(exp) = &mut app.explorer_state {
                                finish_console_query(
                                    exp,
                                    &mut app.toasts,
                                    &mut app.config,
                                    app.active_connection_name.as_ref(),
                                    tab,
                                    &text,
                                    outcome,
                                );
                            }
                        }
                        // Refresh the status-bar mirror: a tx-mode run may have
                        // opened (or, on a begin failure, failed to open) a tx.
                        if let Some(drv) = &app.active_driver {
                            app.tx_active = drv.in_tx().await;
                        }
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        // The task died without reporting — don't leave the
                        // console stuck showing "executing".
                        let tab = run.tab;
                        app.query_run = None;
                        if let Some(exp) = &mut app.explorer_state
                            && let Some(WorkspaceTab::Console(c)) = exp.tabs.get_mut(tab)
                        {
                            c.is_executing = false;
                            c.execution_error =
                                Some("query task ended unexpectedly".to_string());
                        }
                    }
                }
            }

            // Watch mode: re-run the active console's query once its interval
            // has elapsed. Uses the same path as Ctrl+Enter so results, errors
            // and history behave identically.
            // A watched re-run must not fight a dialog: it could re-open the
            // destructive-statement confirmation on every interval, leaving it
            // impossible to dismiss.
            let overlay_open = app
                .explorer_state
                .as_ref()
                .map(|e| {
                    e.ddl_popup.is_some()
                        || e.export_modal.is_some()
                        || e.cell_edit_modal.is_some()
                        || e.insert_row_modal.is_some()
                        || e.sql_confirm_modal.is_some()
                        || e.object_search.is_some()
                        || e.import_csv_modal.is_some()
                        || e.schema_edit_modal.is_some()
                        || e.create_object_modal.is_some()
                        || e.erd_menu.is_some()
                        || e.tree_menu.is_some()
                        || e.explain_plan.is_some()
                        || e.process_list.is_some()
                        || e.schema_diff.is_some()
                        || e.diff_picker.is_some()
                })
                .unwrap_or(false)
                || app.help_open
                || app.form_modal.is_some();
            // Feed the running query's elapsed time to the console so its
            // pane can show a live counter rather than a frozen label.
            if let Some(run) = &app.query_run {
                let elapsed = run.started.elapsed();
                let tab = run.tab;
                if let Some(exp) = &mut app.explorer_state
                    && let Some(WorkspaceTab::Console(c)) = exp.tabs.get_mut(tab)
                {
                    c.exec_elapsed = elapsed;
                }
            }

            let watch_due = !overlay_open
                && app
                .explorer_state
                .as_ref()
                .and_then(|e| e.active_tab())
                .and_then(|t| match t {
                    WorkspaceTab::Console(c) => c.watch_interval.map(|iv| {
                        c.last_run.map(|t| t.elapsed() >= iv).unwrap_or(true)
                            && !c.is_executing
                    }),
                    _ => None,
                })
                .unwrap_or(false);
            if watch_due
                && let (Some(drv), Some(exp)) = (&app.active_driver.clone(), &mut app.explorer_state)
            {
                start_console_query(exp, drv, &mut app.toasts, &mut app.query_run, !app.autocommit);
                if let Some(WorkspaceTab::Console(c)) = exp.active_tab_mut() {
                    c.last_run = Some(Instant::now());
                }
            }
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
            // Poll the picker 't' test result — toast only, there is no modal
            // to surface it in.
            if let Some(rx) = &mut app.picker_test_rx {
                match rx.try_recv() {
                    Ok(res) => {
                        match res {
                            Ok(dur) => app
                                .toasts
                                .push(ToastKind::Success, format!("ping ok ({:.2?})", dur)),
                            Err(err) => app.toasts.push(ToastKind::Error, err),
                        }
                        app.picker_test_rx = None;
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        app.toasts
                            .push(ToastKind::Error, "ping task ended unexpectedly".to_string());
                        app.picker_test_rx = None;
                    }
                }
            }
            // Poll the startup update check. Only surface a toast when a
            // newer version actually exists — up-to-date / failed checks stay
            // silent so they don't nag on every launch.
            if let Some(rx) = &mut app.update_check_rx {
                match rx.try_recv() {
                    Ok(Some(latest)) => {
                        app.toasts.push(
                            ToastKind::Info,
                            format!(
                                "update available: v{latest} (you're on v{}) — quit and run `dbx --self-update`",
                                crate::update::CURRENT_VERSION
                            ),
                        );
                        app.update_check_rx = None;
                    }
                    Ok(None) => {
                        app.update_check_rx = None;
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        app.update_check_rx = None;
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

    #[test]
    fn test_step_column_keeps_selection_inside_the_window() {
        // Grid 80 cells wide -> 5 columns of 16 fit at a time.
        let area = Some(Rect::new(0, 0, 80, 20));
        let (mut sel, mut scroll) = (0usize, 0usize);

        // Walking right past the window edge drags the viewport along.
        for _ in 0..6 {
            step_column(&mut sel, &mut scroll, 12, area, true);
        }
        assert_eq!(sel, 6);
        assert!(
            sel >= scroll && sel < scroll + 5,
            "selection {sel} outside window {scroll}..{}",
            scroll + 5
        );

        // Walking back left pulls it back.
        for _ in 0..6 {
            step_column(&mut sel, &mut scroll, 12, area, false);
        }
        assert_eq!((sel, scroll), (0, 0));
    }

    #[test]
    fn test_step_column_clamps_at_both_ends() {
        let area = Some(Rect::new(0, 0, 80, 20));
        let (mut sel, mut scroll) = (0usize, 0usize);

        // Left at column 0 stays put rather than underflowing.
        step_column(&mut sel, &mut scroll, 3, area, false);
        assert_eq!(sel, 0);

        // Right stops on the last column.
        for _ in 0..10 {
            step_column(&mut sel, &mut scroll, 3, area, true);
        }
        assert_eq!(sel, 2);

        // No columns at all is a no-op, not a panic.
        let (mut sel, mut scroll) = (0usize, 0usize);
        step_column(&mut sel, &mut scroll, 0, area, true);
        assert_eq!((sel, scroll), (0, 0));
    }
}
