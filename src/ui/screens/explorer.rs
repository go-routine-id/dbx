//! Screen S2: Database Explorer Tree & Tabbed Data Grid Workspace.
//! Keyboard-first DataGrip-like navigation: Tree on the left, Data Grid / Tabs on the right.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, Paragraph, Row as TableRow, Table, TableState,
};

use crate::driver::{Collection, CollectionRef, ColumnMeta, Namespace, RecordPage};
use crate::export::ExportFormat;
use crate::theme::Theme;
use crate::ui::screens::erd::{self, ErdTab};
use crate::ui::screens::query::{self, QueryConsole};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusedPane {
    Tree,
    Workspace,
}

#[derive(Clone, Debug)]
pub struct ExportModalState {
    pub format: ExportFormat,
    pub target_path: String,
    pub active_field: usize, // 0: Format selector, 1: Path input
    pub default_table_name: String,
    /// Set when the user pressed Enter on an existing path and we asked them
    /// to confirm overwriting. Cleared when the modal is (re)opened.
    pub confirm_overwrite: bool,
}

#[derive(Clone, Debug)]
pub struct CellEditModalState {
    pub collection: CollectionRef,
    pub column_name: String,
    pub row_idx: usize,
    pub col_idx: usize,
    pub text_buffer: String,
    /// Whether the column allows NULL. When true, the user can press `Ctrl+N`
    /// inside the modal to set the cell to NULL. The SQL preview then emits
    /// `SET col = NULL` instead of `SET col = '<value>'`.
    pub is_nullable: bool,
}

#[derive(Clone, Debug)]
pub struct SqlConfirmModalState {
    pub collection: CollectionRef,
    pub sql_query: String,
    pub row_idx: usize,
}

/// State for the multi-field INSERT-row modal. One `Option<String>` per
/// column from the table's metadata. `None` means "skip this column" — the
/// generated `INSERT` statement omits it, so the server applies the column
/// DEFAULT (or NULL if no default + nullable, or error if NOT NULL with no
/// default). A non-None buffer is the literal value, or `crate::app::NULL_SENTINEL`
/// to set the column to NULL (only valid for nullable columns).
#[derive(Clone, Debug)]
pub struct InsertRowModalState {
    pub collection: CollectionRef,
    /// Per-column buffers parallel to the table's column order.
    pub field_buffers: Vec<Option<String>>,
    pub column_meta: Vec<ColumnMeta>,
    pub focused_field: usize,
}

#[derive(Clone, Debug)]
pub enum TreeNodeKind {
    Database(Namespace),
    Table(CollectionRef, Option<u64>),
}

#[derive(Clone, Debug)]
pub struct TreeNode {
    pub kind: TreeNodeKind,
    pub is_expanded: bool,
    pub is_loading: bool,
}

#[derive(Clone, Debug)]
pub enum WorkspaceTab {
    Table(DataTab),
    Console(QueryConsole),
    Erd(ErdTab),
}

impl WorkspaceTab {
    pub fn title(&self) -> String {
        match self {
            WorkspaceTab::Table(t) => t.collection.name.clone(),
            WorkspaceTab::Console(c) => c.title.clone(),
            WorkspaceTab::Erd(e) => format!("ERD: {}", e.namespace.0),
        }
    }
}

#[derive(Clone, Debug)]
pub struct DataTab {
    pub collection: CollectionRef,
    pub page: RecordPage,
    pub selected_row: usize,
    pub selected_col: usize,
    pub scroll_offset_x: usize,
    /// Column metadata (nullable, type, PK, etc.) parallel to `page.columns`.
    /// Populated when the tab is opened or its page is refreshed. Used by the
    /// cell-edit modal to know whether the user can set the cell to NULL.
    /// Empty for tabs opened before this field existed or for query-console
    /// tabs that don't bind to a single table.
    pub column_meta: Vec<ColumnMeta>,
}

#[derive(Clone, Debug)]
pub struct ExplorerState {
    pub focused_pane: FocusedPane,
    pub namespaces: Vec<Namespace>,
    pub tables: std::collections::HashMap<String, Vec<Collection>>,
    pub tree_nodes: Vec<TreeNode>,
    pub selected_tree_index: usize,
    /// First tree-node row currently visible in the tree pane. Adjusted on
    /// every draw so `selected_tree_index` stays in view when the list is
    /// taller than the viewport.
    pub tree_scroll: usize,

    // Right Workspace Tabs (Tables & Query Consoles)
    pub tabs: Vec<WorkspaceTab>,
    pub active_tab_index: usize,

    // Modals
    pub ddl_popup: Option<(CollectionRef, String)>,
    /// Rect of the DDL popup as last painted, used to dismiss it on a click
    /// outside the popup. Only meaningful while `ddl_popup` is `Some`.
    pub ddl_popup_area: Option<Rect>,
    pub export_modal: Option<ExportModalState>,
    pub cell_edit_modal: Option<CellEditModalState>,
    pub sql_confirm_modal: Option<SqlConfirmModalState>,
    pub insert_row_modal: Option<InsertRowModalState>,
    pub driver_capabilities: crate::driver::Capabilities,
}

impl ExplorerState {
    pub fn new(namespaces: Vec<Namespace>, driver_capabilities: crate::driver::Capabilities) -> Self {
        let mut tree_nodes = Vec::new();
        for ns in &namespaces {
            tree_nodes.push(TreeNode {
                kind: TreeNodeKind::Database(ns.clone()),
                is_expanded: false,
                is_loading: false,
            });
        }

        Self {
            focused_pane: FocusedPane::Tree,
            namespaces,
            tables: std::collections::HashMap::new(),
            tree_nodes,
            selected_tree_index: 0,
            tree_scroll: 0,
            tabs: Vec::new(),
            active_tab_index: 0,
            ddl_popup: None,
            ddl_popup_area: None,
            export_modal: None,
            cell_edit_modal: None,
            sql_confirm_modal: None,
            insert_row_modal: None,
            driver_capabilities,
        }
    }

    pub fn rebuild_tree_nodes(&mut self) {
        let mut nodes = Vec::new();
        for ns in &self.namespaces {
            let is_expanded = self
                .tree_nodes
                .iter()
                .find(|n| match &n.kind {
                    TreeNodeKind::Database(d) => d == ns,
                    _ => false,
                })
                .map(|n| n.is_expanded)
                .unwrap_or(false);

            nodes.push(TreeNode {
                kind: TreeNodeKind::Database(ns.clone()),
                is_expanded,
                is_loading: false,
            });

            if is_expanded && let Some(tbls) = self.tables.get(&ns.0) {
                for tbl in tbls {
                    nodes.push(TreeNode {
                        kind: TreeNodeKind::Table(
                            CollectionRef {
                                namespace: ns.clone(),
                                name: tbl.name.clone(),
                            },
                            tbl.estimated_row_count,
                        ),
                        is_expanded: false,
                        is_loading: false,
                    });
                }
            }
        }
        self.tree_nodes = nodes;
        if self.selected_tree_index >= self.tree_nodes.len() {
            self.selected_tree_index = self.tree_nodes.len().saturating_sub(1);
        }
    }

    pub fn selected_node(&self) -> Option<&TreeNode> {
        self.tree_nodes.get(self.selected_tree_index)
    }

    pub fn active_tab(&self) -> Option<&WorkspaceTab> {
        self.tabs.get(self.active_tab_index)
    }

    pub fn active_tab_mut(&mut self) -> Option<&mut WorkspaceTab> {
        self.tabs.get_mut(self.active_tab_index)
    }
}

pub fn render_explorer(
    f: &mut Frame,
    area: Rect,
    state: &mut ExplorerState,
    theme: &Theme,
) {
    let main_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(32), // Explorer tree width
            Constraint::Min(48),   // Workspace / Table grid
        ])
        .split(area);

    render_tree(f, main_chunks[0], state, theme);
    render_workspace(f, main_chunks[1], state, theme);

    if let Some((cref, ddl)) = state.ddl_popup.clone() {
        state.ddl_popup_area = Some(render_ddl_popup(f, area, &cref, &ddl, theme));
    }

    if let Some(export_modal) = &state.export_modal {
        render_export_modal(f, area, export_modal, theme);
    }

    if let Some(edit_modal) = &state.cell_edit_modal {
        render_cell_edit_modal(f, area, edit_modal, theme);
    }

    if let Some(insert_modal) = &state.insert_row_modal {
        render_insert_row_modal(f, area, insert_modal, theme);
    }

    if let Some(confirm_modal) = &state.sql_confirm_modal {
        render_sql_confirm_modal(f, area, confirm_modal, theme);
    }
}

fn render_tree(f: &mut Frame, area: Rect, state: &mut ExplorerState, theme: &Theme) {
    let is_focused = state.focused_pane == FocusedPane::Tree;
    let border_style = if is_focused { theme.accent() } else { theme.border() };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .style(theme.base())
        .title(" Databases ");

    let inner = block.inner(area);
    f.render_widget(block, area);

    if state.tree_nodes.is_empty() {
        let p = Paragraph::new(Span::styled("No databases found", theme.dim()))
            .alignment(Alignment::Center);
        f.render_widget(p, inner);
        return;
    }

    // Follow-scroll: the tree can be taller than the pane. We know the
    // viewport height here, so lazily nudge `tree_scroll` so the selected
    // row stays visible after every navigation / expansion. This is
    // idempotent, so drawing every frame is harmless.
    let visible = inner.height as usize;
    let len = state.tree_nodes.len();
    let sel = state.selected_tree_index;
    if state.tree_scroll > sel {
        state.tree_scroll = sel;
    }
    let max_scroll = len.saturating_sub(visible);
    if state.tree_scroll > max_scroll {
        state.tree_scroll = max_scroll;
    }
    if sel >= state.tree_scroll.saturating_add(visible) {
        state.tree_scroll = sel.saturating_add(1).saturating_sub(visible);
    }

    let end = (state.tree_scroll + visible).min(len);
    let mut lines = Vec::new();
    for i in state.tree_scroll..end {
        let node = &state.tree_nodes[i];
        let is_sel = i == sel && is_focused;
        let line = match &node.kind {
            TreeNodeKind::Database(ns) => {
                let prefix = if node.is_loading {
                    "⏳ "
                } else if node.is_expanded {
                    "▼ "
                } else {
                    "▶ "
                };
                let style = if is_sel {
                    theme.selected().add_modifier(Modifier::BOLD)
                } else {
                    theme.base().add_modifier(Modifier::BOLD)
                };
                Line::from(vec![
                    Span::styled(prefix, theme.accent()),
                    Span::styled(format!("📁 {}", ns.0), style),
                ])
            }
            TreeNodeKind::Table(cref, count) => {
                // Row count comes from the information schema / planner
                // estimate, so it's approximate — mark it with `~`.
                let count_str = count
                    .map(|c| format!(" (~{})", c))
                    .unwrap_or_default();
                let style = if is_sel {
                    theme.selected()
                } else {
                    theme.base()
                };
                Line::from(vec![
                    Span::styled("   📄 ", theme.dim()),
                    Span::styled(&cref.name, style),
                    Span::styled(count_str, theme.dim()),
                ])
            }
        };
        lines.push(line);
    }

    let p = Paragraph::new(lines);
    f.render_widget(p, inner);
}

fn render_workspace(f: &mut Frame, area: Rect, state: &mut ExplorerState, theme: &Theme) {
    let is_focused = state.focused_pane == FocusedPane::Workspace;
    let border_style = if is_focused { theme.accent() } else { theme.border() };

    if state.tabs.is_empty() {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(border_style)
            .style(theme.base())
            .title(" Workspace ");

        let text = vec![
            Line::from(Span::styled("No tab opened in workspace", theme.dim())),
            Line::from(Span::styled(
                "Select a table and press [Enter], or press [c] to open a new SQL Console",
                theme.accent(),
            )),
        ];
        let p = Paragraph::new(text)
            .block(block)
            .alignment(Alignment::Center);
        f.render_widget(p, area);
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // Tabs Header
            Constraint::Min(10),   // Active Tab Body (Grid or Console)
            Constraint::Length(1), // Footer info
        ])
        .split(area);

    // 1. Tab headers
    let mut tab_spans = Vec::new();
    for (i, tab) in state.tabs.iter().enumerate() {
        let is_active = i == state.active_tab_index;
        let style = if is_active {
            theme.accent().add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            theme.dim()
        };
        let icon = match tab {
            WorkspaceTab::Table(_) => "📄",
            WorkspaceTab::Console(_) => "⚡",
            WorkspaceTab::Erd(_) => "🗺️",
        };
        tab_spans.push(Span::styled(format!(" [ {icon} {} ] ", tab.title()), style));
        tab_spans.push(Span::raw(" "));
    }
    f.render_widget(Paragraph::new(Line::from(tab_spans)), chunks[0]);

    // 2. Active Tab Body
    if let Some(tab) = state.active_tab_mut() {
        match tab {
            WorkspaceTab::Table(data_tab) => {
                render_grid(f, chunks[1], data_tab, is_focused, theme);

                // 3. Pagination Footer
                let total_str = data_tab
                    .page
                    .total_records
                    .map(|t| format!(" of {} total", t))
                    .unwrap_or_default();
                let footer_text = format!(
                    " Page {} (showing {} rows{}) | [n] Next Page  [p] Prev Page  [w] Close Tab",
                    data_tab.page.page + 1,
                    data_tab.page.records.len(),
                    total_str
                );
                let p = Paragraph::new(Span::styled(footer_text, theme.dim()));
                f.render_widget(p, chunks[2]);
            }
            WorkspaceTab::Console(console) => {
                query::render_query_console(f, chunks[1], console, is_focused, theme);

                let footer_text = " [Ctrl+Enter/F5] Run Query | [Tab] Switch Subpane | [w] Close Console Tab";
                let p = Paragraph::new(Span::styled(footer_text, theme.dim()));
                f.render_widget(p, chunks[2]);
            }
            WorkspaceTab::Erd(erd_tab) => {
                erd::render_erd(f, chunks[1], erd_tab, is_focused, theme);

                let footer_text = " [h/j/k/l or Arrows] Pan Diagram | [w] Close ERD Tab";
                let p = Paragraph::new(Span::styled(footer_text, theme.dim()));
                f.render_widget(p, chunks[2]);
            }
        }
    }
}

fn render_grid(f: &mut Frame, area: Rect, tab: &DataTab, is_focused: bool, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(if is_focused { theme.accent() } else { theme.border() })
        .style(theme.base());

    if tab.page.records.is_empty() {
        let inner = block.inner(area);
        f.render_widget(block, area);
        crate::ui::widgets::empty::render(
            f,
            inner,
            "Table has 0 records or page is empty",
            Some("Press [n]/[p] for pages or [w] to close tab"),
            theme,
        );
        return;
    }

    let num_cols = tab.page.columns.len();
    let col_offset = tab.scroll_offset_x.min(num_cols.saturating_sub(1));

    // Column highlight: when focused, every cell in the active column gets a
    // dim "ruler" background so the user can track the cursor across rows.
    // The active cell (row ∩ col) gets the full accent highlight.
    let col_highlight_bg = theme.panel;
    let active_cell_style = Style::default()
        .bg(theme.accent)
        .fg(theme.background)
        .add_modifier(Modifier::BOLD);
    let col_highlight_style = Style::default()
        .bg(col_highlight_bg)
        .fg(theme.accent);
    let row_highlight_style = theme.selected();

    let header_cells = tab
        .page
        .columns
        .iter()
        .skip(col_offset)
        .enumerate()
        .map(|(rel_idx, col)| {
            let abs_col = col_offset + rel_idx;
            let is_active_col = is_focused && abs_col == tab.selected_col;
            let style = if is_active_col {
                Style::default()
                    .bg(theme.accent)
                    .fg(theme.background)
                    .add_modifier(Modifier::BOLD)
            } else {
                theme.accent().add_modifier(Modifier::BOLD)
            };
            Cell::from(Span::styled(col, style))
        });
    let header = TableRow::new(header_cells).height(1).bottom_margin(1);

    let rows: Vec<TableRow> = tab
        .page
        .records
        .iter()
        .enumerate()
        .map(|(r_idx, record)| {
            let is_row_sel = r_idx == tab.selected_row && is_focused;
            let cells = record.values.iter().skip(col_offset).enumerate().map(|(rel_idx, val)| {
                let abs_col = col_offset + rel_idx;
                let cell_str = val.display_str();
                let is_cell_sel = is_row_sel && abs_col == tab.selected_col;
                let is_col_sel = is_focused && abs_col == tab.selected_col;
                let cell_style = if is_cell_sel {
                    active_cell_style
                } else if is_row_sel {
                    row_highlight_style
                } else if is_col_sel {
                    col_highlight_style
                } else {
                    theme.base()
                };
                Cell::from(Span::styled(cell_str, cell_style))
            });
            TableRow::new(cells).height(1)
        })
        .collect();

    let widths: Vec<Constraint> = tab
        .page
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
    state.select(Some(tab.selected_row));

    f.render_stateful_widget(table, area, &mut state);
}

/// Renders the DDL popup and returns its `Rect` so callers can hit-test a
/// mouse click (click outside → dismiss).
fn render_ddl_popup(
    f: &mut Frame,
    area: Rect,
    cref: &CollectionRef,
    ddl: &str,
    theme: &Theme,
) -> Rect {
    let width = 75.min(area.width.saturating_sub(4));
    let height = 24.min(area.height.saturating_sub(2));

    let popup_area = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    f.render_widget(Clear, popup_area);

    let title = format!(" DDL Schema: {} [Esc to close] ", cref);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme.accent())
        .style(theme.panel())
        .title(title);

    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let p = Paragraph::new(ddl).style(theme.base());
    f.render_widget(p, inner);

    popup_area
}

fn render_export_modal(
    f: &mut Frame,
    area: Rect,
    modal: &ExportModalState,
    theme: &Theme,
) {
    let width = 60.min(area.width.saturating_sub(4));
    let height = 10.min(area.height.saturating_sub(2));

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
        .title(" Export Dataset [Ctrl+E] ");

    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // Format selector
            Constraint::Length(1), // Spacing
            Constraint::Length(3), // Path input box
            Constraint::Length(1), // Help hints
        ])
        .split(inner);

    // 1. Format selector
    let mut format_spans = vec![Span::styled(" Format: ", theme.dim())];
    for fmt in &ExportFormat::ALL {
        let is_selected = modal.format == *fmt;
        let is_field_active = modal.active_field == 0;
        let style = if is_selected && is_field_active {
            theme.accent().add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else if is_selected {
            theme.accent().add_modifier(Modifier::BOLD)
        } else {
            theme.dim()
        };
        let radio = if is_selected { "[● " } else { "[○ " };
        format_spans.push(Span::styled(format!("{}{}] ", radio, fmt.name()), style));
    }
    f.render_widget(Paragraph::new(Line::from(format_spans)), chunks[0]);

    // 2. Path input box
    let path_border = if modal.active_field == 1 { theme.accent() } else { theme.border() };
    let path_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(path_border)
        .title(" Destination File Path ");
    let path_inner = path_block.inner(chunks[2]);
    f.render_widget(path_block, chunks[2]);
    f.render_widget(Paragraph::new(modal.target_path.as_str()).style(theme.base()), path_inner);

    // 3. Hints
    let hint = "[Tab] Switch Field | [←/→] Change Format | [Enter] Export | [Esc] Cancel";
    f.render_widget(Paragraph::new(Span::styled(hint, theme.dim())).alignment(Alignment::Center), chunks[3]);
}

fn render_cell_edit_modal(
    f: &mut Frame,
    area: Rect,
    modal: &CellEditModalState,
    theme: &Theme,
) {
    let width = 64.min(area.width.saturating_sub(4));
    let height = 8.min(area.height.saturating_sub(2));

    let popup_area = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    f.render_widget(Clear, popup_area);

    // Title carries the NULLABLE badge so the user knows up-front whether
    // they can set this cell to NULL (only on nullable columns).
    let title = if modal.is_nullable {
        format!(
            " Edit Value: {}.{} (Row #{}, Col #{}) [NULLABLE] ",
            modal.collection.name,
            modal.column_name,
            modal.row_idx + 1,
            modal.col_idx + 1
        )
    } else {
        format!(
            " Edit Value: {}.{} (Row #{}, Col #{}) ",
            modal.collection.name,
            modal.column_name,
            modal.row_idx + 1,
            modal.col_idx + 1
        )
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme.accent())
        .style(theme.panel())
        .title(title);

    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Input box
            Constraint::Length(1), // NULLABLE-specific hint or blank
            Constraint::Length(1), // Common hint
        ])
        .split(inner);

    let input_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme.accent());
    let input_inner = input_block.inner(chunks[0]);
    f.render_widget(input_block, chunks[0]);

    // Render the buffer. The `__DBX_NULL__` sentinel means the user picked
    // "set to NULL" via Ctrl+N. Display it as a bold-italic `NULL` so the
    // user sees an unambiguous preview before reviewing the SQL.
    if modal.text_buffer == crate::app::NULL_SENTINEL {
        let null_line = Line::from(vec![
            Span::styled(
                "NULL",
                Style::default()
                    .fg(theme.warning)
                    .add_modifier(Modifier::BOLD | Modifier::ITALIC),
            ),
        ]);
        f.render_widget(Paragraph::new(null_line), input_inner);
    } else {
        f.render_widget(Paragraph::new(modal.text_buffer.as_str()).style(theme.base()), input_inner);
    }

    // NULLABLE-specific shortcut hint (only when column accepts NULL).
    if modal.is_nullable {
        let set_null_hint = Line::from(vec![
            Span::styled("[Ctrl+N] ", theme.accent().add_modifier(Modifier::BOLD)),
            Span::styled("Set to NULL  ", theme.dim()),
            Span::styled("  |  ", theme.dim()),
            Span::styled("[Esc] ", theme.accent()),
            Span::styled("clear NULL", theme.dim()),
        ]);
        f.render_widget(Paragraph::new(set_null_hint).alignment(Alignment::Center), chunks[1]);
    }

    let hint = "[Enter] Review SQL Mutation | [Esc] Cancel";
    f.render_widget(Paragraph::new(Span::styled(hint, theme.dim())).alignment(Alignment::Center), chunks[2]);
}

fn render_sql_confirm_modal(
    f: &mut Frame,
    area: Rect,
    modal: &SqlConfirmModalState,
    theme: &Theme,
) {
    let width = 70.min(area.width.saturating_sub(4));
    let height = 9.min(area.height.saturating_sub(2));

    let popup_area = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    f.render_widget(Clear, popup_area);

    let title = format!(
        " Confirm Safe SQL Mutation (Row #{}) ",
        modal.row_idx + 1
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme.error())
        .style(theme.panel())
        .title(title);

    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // Warning
            Constraint::Length(3), // SQL snippet box
            Constraint::Length(1), // Action hints
        ])
        .split(inner);

    let warning = Line::from(vec![
        Span::styled("⚠️  Ready to execute UPDATE statement:", theme.warning().add_modifier(Modifier::BOLD)),
    ]);
    f.render_widget(Paragraph::new(warning), chunks[0]);

    let sql_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme.border());
    let sql_inner = sql_block.inner(chunks[1]);
    f.render_widget(sql_block, chunks[1]);
    f.render_widget(Paragraph::new(modal.sql_query.as_str()).style(theme.base()), sql_inner);

    let hints = Line::from(vec![
        Span::styled("[Enter] Execute UPDATE", theme.accent().add_modifier(Modifier::BOLD)),
        Span::styled("  |  ", theme.dim()),
        Span::styled("[Esc] Cancel", theme.dim()),
    ]);
    f.render_widget(Paragraph::new(hints).alignment(Alignment::Center), chunks[2]);
}

/// Renders the multi-field INSERT-row modal. One row per column from
/// `modal.column_meta`. The current focused field is highlighted; the
/// empty/sentinel state is rendered distinctly so the user can tell at a
/// glance which columns will be skipped vs. explicitly set to NULL vs.
/// carrying a typed value.
///
/// Layout: popup is centered, height = min(22, 18+visible_fields). Inside:
///   - header (1 line): table identifier
///   - scrolling field list (one row per column: 1 line)
///   - 1 blank
///   - hint bar (1 line)
///   - 1 blank
///   - action hint (1 line)
fn render_insert_row_modal(
    f: &mut Frame,
    area: Rect,
    modal: &InsertRowModalState,
    theme: &Theme,
) {
    // Generous width so long type names + values fit. Falls back to the
    // terminal width minus a 2-col margin if the terminal is narrow.
    let width: u16 = 78.min(area.width.saturating_sub(4));
    let n_cols = modal.column_meta.len();
    // Reserve room for header + spacer + 2 hint lines + 2 inner margins (top
    // + bottom of Block) + 1 border. Up to ~18 column rows fit in a normal
    // terminal before we start clipping. The clip is intentional: the
    // modal is meant for a quick "fill the obvious fields, skip the rest"
    // flow, not for editing tables with hundreds of columns.
    let visible_fields: usize = n_cols.min(18);
    let body_height: u16 = 1 + visible_fields as u16 + 1 + 1 + 1;
    let height: u16 = (body_height + 2).min(area.height.saturating_sub(2));

    let popup_area = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    f.render_widget(Clear, popup_area);

    let title = format!(
        " Insert New Row: {}.{} ({} columns) ",
        modal.collection.namespace.0,
        modal.collection.name,
        n_cols
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme.accent())
        .style(theme.panel())
        .title(title);

    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    // Vertical layout: header / fields / spacer / per-field hint / spacer / action hint.
    // We can't use a dynamic constraint count in a single .split call without
    // building the array up front. Build the full list once.
    let mut constraints = Vec::with_capacity(n_cols + 4);
    constraints.push(Constraint::Length(1)); // header
    for _ in 0..n_cols {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Length(1)); // spacer
    constraints.push(Constraint::Length(1)); // per-field hint
    constraints.push(Constraint::Length(1)); // spacer
    constraints.push(Constraint::Length(1)); // action hint
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints(constraints)
        .split(inner);

    // Header line — explains the "None = skip / sentinel = NULL / text = value"
    // semantic so the user doesn't have to guess.
    let header_line = Line::from(vec![
        Span::styled("Column", theme.accent().add_modifier(Modifier::BOLD)),
        Span::styled("                  ", theme.dim()),
        Span::styled("Type", theme.dim()),
        Span::styled("    ", theme.dim()),
        Span::styled("Value", theme.accent().add_modifier(Modifier::BOLD)),
        Span::styled("  (empty = skip / Ctrl+N = NULL)", theme.dim()),
    ]);
    f.render_widget(Paragraph::new(header_line), chunks[0]);

    // One row per column.
    for (i, col) in modal.column_meta.iter().enumerate() {
        let is_focused = i == modal.focused_field;
        let buf = modal.field_buffers.get(i).and_then(|x| x.as_ref());

        // Column name (left, fixed width) + type dim (middle) + value (right).
        let label_style = if is_focused {
            theme.accent().add_modifier(Modifier::BOLD)
        } else {
            theme.base().add_modifier(Modifier::BOLD)
        };
        let type_str = format!("{:>20}", col.data_type);
        let nullable_tag = if col.is_nullable { " NULL" } else { " NOT NULL" };
        let null_style = if col.is_nullable { theme.dim() } else { theme.warning() };

        let (value_str, value_style) = match buf {
            None => (
                "<skip>".to_string(),
                if is_focused {
                    theme.dim()
                } else {
                    theme.dim().add_modifier(Modifier::DIM)
                },
            ),
            Some(s) if s == crate::app::NULL_SENTINEL => (
                "NULL".to_string(),
                Style::default()
                    .fg(theme.warning)
                    .add_modifier(Modifier::BOLD | Modifier::ITALIC),
            ),
            Some(s) => (
                s.clone(),
                if is_focused {
                    theme.base().add_modifier(Modifier::BOLD)
                } else {
                    theme.base()
                },
            ),
        };

        let mut spans = vec![
            Span::styled(format!("{:<22}", col.name), label_style),
            Span::styled(type_str, theme.dim()),
            Span::styled(nullable_tag, null_style),
            Span::styled("  ", theme.dim()),
        ];
        spans.push(Span::styled(value_str, value_style));
        if is_focused {
            spans.push(Span::styled("█", theme.accent()));
        }
        let line = Line::from(spans);
        f.render_widget(Paragraph::new(line), chunks[1 + i]);
    }

    // Per-field hint (only meaningful on the focused row).
    let hint_idx = 2 + n_cols;
    let action_idx = 4 + n_cols;

    if let Some(col) = modal.column_meta.get(modal.focused_field) {
        let per_field_hint = if col.is_nullable {
            Line::from(vec![
                Span::styled("Selected: ", theme.dim()),
                Span::styled(&col.name, theme.accent().add_modifier(Modifier::BOLD)),
                Span::styled("  |  ", theme.dim()),
                Span::styled("[Ctrl+N] ", theme.accent()),
                Span::styled("set NULL  ", theme.dim()),
                Span::styled("[Esc on empty] ", theme.accent()),
                Span::styled("back to skip", theme.dim()),
            ])
        } else {
            Line::from(vec![
                Span::styled("Selected: ", theme.dim()),
                Span::styled(&col.name, theme.accent().add_modifier(Modifier::BOLD)),
                Span::styled("  |  ", theme.dim()),
                Span::styled("[Esc on empty] ", theme.accent()),
                Span::styled("back to skip", theme.dim()),
            ])
        };
        f.render_widget(Paragraph::new(per_field_hint).alignment(Alignment::Center), chunks[hint_idx]);
    }

    let action = Line::from(vec![
        Span::styled("[Tab/↓] Next field  ", theme.dim()),
        Span::styled("[Shift+Tab/↑] Prev  ", theme.dim()),
        Span::styled("[Enter] Insert  ", theme.accent().add_modifier(Modifier::BOLD)),
        Span::styled("[Esc] Cancel", theme.dim()),
    ]);
    f.render_widget(Paragraph::new(action).alignment(Alignment::Center), chunks[action_idx]);
}

