//! Screen S2: Database Explorer Tree & Tabbed Data Grid Workspace.
//! Keyboard-first DataGrip-like navigation: Tree on the left, Data Grid / Tabs on the right.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, Paragraph, Row as TableRow, Table, TableState,
};

use crate::driver::{Collection, CollectionRef, ColumnMeta, Namespace, Record, RecordPage, Value};
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
    /// Source table when exporting from a table tab — a SQL dump uses it to
    /// fetch the DDL. None for console-result exports.
    pub collection: Option<CollectionRef>,
    /// Set when the user pressed Enter on an existing path and we asked them
    /// to confirm overwriting. Cleared when the modal is (re)opened.
    pub confirm_overwrite: bool,
}

#[derive(Clone, Debug)]
pub struct CellEditModalState {
    pub collection: CollectionRef,
    pub column_name: String,
    /// The column's SQL data type (e.g. VARCHAR(64), TIMESTAMP) — shown so
    /// the user knows what kind of value is expected.
    pub data_type: String,
    pub row_idx: usize,
    pub col_idx: usize,
    pub text_buffer: String,
    /// Whether the column allows NULL. When true, the user can press `Ctrl+N`
    /// inside the modal to set the cell to NULL. The SQL preview then emits
    /// `SET col = NULL` instead of `SET col = '<value>'`.
    pub is_nullable: bool,
    /// `true` when the column's type is boolean — the modal renders a
    /// true/false(/NULL) dropdown instead of a free-text input.
    pub is_boolean: bool,
    /// Active dropdown option: 0=true, 1=false, 2=NULL (only when nullable).
    pub bool_selection: usize,
}

#[derive(Clone, Debug)]
pub struct SqlConfirmModalState {
    pub collection: CollectionRef,
    pub sql_query: String,
    pub row_idx: usize,
}

/// Pick which saved connection to compare the current schema against.
#[derive(Clone, Debug)]
pub struct DiffPickerState {
    /// Names of the other saved connections, in config order.
    pub connections: Vec<String>,
    pub selected: usize,
}

/// Result of comparing this schema against another connection's.
#[derive(Clone, Debug)]
pub struct SchemaDiffState {
    pub against: String,
    pub namespace: String,
    pub diffs: Vec<crate::schema_diff::Difference>,
    /// DDL that would bring the other side in line with this one.
    pub migration: String,
    pub scroll: usize,
}

/// Snapshot of the server's running queries.
#[derive(Clone, Debug)]
pub struct ProcessListState {
    pub result: crate::driver::QueryResult,
    pub selected: usize,
}

impl ProcessListState {
    /// Server-side id of the highlighted row, used to cancel it.
    pub fn selected_pid(&self) -> Option<String> {
        let idx = self
            .result
            .columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case("pid"))?;
        Some(self.result.records.get(self.selected)?.values.get(idx)?.display_str())
    }
}

/// A parsed query plan, shown as a tree over the console.
#[derive(Clone, Debug)]
pub struct ExplainPlanState {
    pub nodes: Vec<crate::explain::PlanNode>,
    /// Index of the costliest node, highlighted as the bottleneck.
    pub hotspot: Option<usize>,
    pub scroll: usize,
}

/// Context menu shown when clicking/selecting an ERD node — a short list of
/// actions to run against that table.
#[derive(Clone, Debug)]
pub struct ErdMenuState {
    pub collection: CollectionRef,
    pub selected: usize,
    /// Screen cell to anchor the menu near (the mouse click position);
    /// `None` = centred (keyboard-triggered).
    pub menu_at: Option<(u16, u16)>,
}

/// Labels for the ERD node context menu, indexed by `ErdMenuState.selected`.
pub const ERD_MENU_OPTIONS: [&str; 4] = [
    "View DDL",
    "Open table (rows)",
    "Edit schema",
    "Delete table",
];

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

/// State for the CSV-import modal (`Ctrl+Shift+I` on a table tab). Enter
/// parses the file; a second Enter (once `parsed`) inserts the rows.
#[derive(Clone, Debug)]
pub struct ImportCsvModalState {
    pub path: String,
    pub rows: Vec<Vec<String>>,
    pub parsed: bool,
}

/// What kind of object to create with the create-object modal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreateKind {
    Schema,
    Table,
    View,
    Type,
    Function,
}

impl CreateKind {
    pub const ALL: [CreateKind; 5] = [CreateKind::Schema, CreateKind::Table, CreateKind::View, CreateKind::Type, CreateKind::Function];

    pub fn label(self) -> &'static str {
        match self {
            CreateKind::Schema => "schema",
            CreateKind::Table => "table",
            CreateKind::View => "view",
            CreateKind::Type => "data type",
            CreateKind::Function => "function",
        }
    }

    /// Move to the previous/next kind (wraps around).
    pub fn cycle(self, dir: isize) -> CreateKind {
        let idx = Self::ALL.iter().position(|k| *k == self).unwrap_or(0);
        let n = Self::ALL.len() as isize;
        Self::ALL[((idx as isize + dir + n) % n) as usize]
    }
}

/// State for the create-object modal (`a` in the tree). Pick a kind and type
/// a name; Enter generates a CREATE statement shown in the SQL-confirm modal.
#[derive(Clone, Debug)]
pub struct CreateObjectModalState {
    /// Schema context for the new object (empty for CREATE SCHEMA).
    pub namespace: Namespace,
    pub kind: CreateKind,
    pub name: String,
}

/// A schema-edit text input in progress.
#[derive(Clone, Debug)]
pub enum SchemaInput {
    /// Adding a column: stage 0 = typing the name, stage 1 = the type.
    AddColumn { name: String, ty: String, stage: usize },
    /// Renaming the table — typing the new name.
    RenameTable { value: String },
    /// Changing a column's type — typing the new type.
    ChangeType { column: String, value: String },
}

/// State for the schema-edit modal (`e` on a table in the tree). Builds an
/// ALTER TABLE from a small set of operations: drop column, add column,
/// rename table, change column type.
#[derive(Clone, Debug)]
pub struct SchemaEditModalState {
    pub collection: CollectionRef,
    pub columns: Vec<ColumnMeta>,
    pub selected: usize,
    pub drop_cols: Vec<String>,
    pub add_cols: Vec<(String, String)>,
    pub type_changes: Vec<(String, String)>,
    pub rename_table: Option<String>,
    pub input: Option<SchemaInput>,
}

/// Kind of object in the object-search results — drives the icon and how
/// Enter opens it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchKind {
    Table,
    View,
    Routine,
    Sequence,
}

/// State for the global object-search overlay (`Ctrl+T`). `results` holds
/// every object across all namespaces (fetched once when the overlay opens);
/// the visible list is filtered by `query` client-side.
#[derive(Clone, Debug)]
pub struct ObjectSearchState {
    pub query: String,
    pub results: Vec<(CollectionRef, SearchKind)>,
    pub selected: usize,
}

#[derive(Clone, Debug)]
pub enum TreeNodeKind {
    Database(Namespace),
    Table(CollectionRef, Option<u64>, Option<u64>), // (ref, est. rows, est. size bytes)
    View(CollectionRef),
    Routine(CollectionRef),
    Sequence(CollectionRef),
    /// A non-selectable divider that groups the objects under a schema
    /// ("Tables", "Views", ...). Grouping without another level of expanding
    /// keeps everything one keypress away while still being scannable.
    Section(&'static str, usize),
}

impl TreeNodeKind {
    /// Section headers are labels, not destinations: navigation skips them so
    /// arrow keys never land on a row that cannot be opened.
    pub fn is_selectable(&self) -> bool {
        !matches!(self, TreeNodeKind::Section(..))
    }

    /// Schema this node belongs to; `None` for a section divider.
    pub fn namespace(&self) -> Option<&Namespace> {
        match self {
            TreeNodeKind::Database(ns) => Some(ns),
            TreeNodeKind::Table(c, _, _)
            | TreeNodeKind::View(c)
            | TreeNodeKind::Routine(c)
            | TreeNodeKind::Sequence(c) => Some(&c.namespace),
            TreeNodeKind::Section(..) => None,
        }
    }
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
    /// Foreign keys of this table, kept so a cell can jump to the row it
    /// references (`f`). Same source as `column_meta`.
    pub foreign_keys: Vec<crate::driver::ForeignKeyMeta>,
    /// Active client-side sort (column index into `page.columns`) + direction.
    /// `None` = natural order.
    /// Active sort keys in priority order: the first decides, later ones only
    /// break ties. Empty = natural order.
    pub sort_keys: Vec<(usize, SortDir)>,
    /// Active client-side filter, applied on top of the sort.
    pub filter: Option<FilterExpr>,
    /// `true` while the user is typing a filter expression in the footer.
    pub filter_editing: bool,
    /// Text buffer for the filter being typed.
    pub filter_buffer: String,
    /// `true` for views (may be read-only / not updatable). Disables the
    /// cell-edit / insert / delete shortcuts so the UI never offers an
    /// operation that would fail at the DB (or silently mutate base rows).
    pub read_only: bool,
    /// Grid inner area from the last draw — maps a mouse click to a cell.
    pub grid_hit_area: Option<Rect>,
    /// X start (terminal column) of each visible column, computed at draw
    /// time so mouse hit-testing matches the actual rendered widths exactly.
    pub grid_col_starts: Vec<u16>,
    /// `true` while the selected row is shown expanded (one column per line),
    /// which is the only readable way to look at a very wide table.
    pub row_detail: bool,
    /// First column shown in the row-detail panel (it scrolls independently
    /// of the grid).
    pub row_detail_scroll: usize,
    /// Free-text search across every cell. Unlike `filter` (which is a
    /// column expression and hides rows) this only highlights matches and
    /// jumps between them, so the row set never changes underneath you.
    pub search_query: String,
    /// `true` while the user is typing a search term in the footer.
    pub search_editing: bool,
    pub search_buffer: String,
}

/// Rows of `tab` as they are actually displayed: client-side filter applied,
/// then the client-side sort. Row *references* are returned so
/// `page.records` keeps its natural order for pagination.
///
/// Every place that maps a `selected_row` back to a record must go through
/// this, or the selection silently points at the wrong row whenever a filter
/// or sort is active.
pub fn visible_records(tab: &DataTab) -> Vec<&Record> {
    let mut rows: Vec<&Record> = tab
        .page
        .records
        .iter()
        .filter(|r| {
            tab.filter
                .as_ref()
                .map(|f| record_matches_filter(r, f))
                .unwrap_or(true)
        })
        .collect();
    if !tab.sort_keys.is_empty() {
        // Stable so equal rows keep the order the server sent them in.
        rows.sort_by(|a, b| compare_by_keys(a, b, &tab.sort_keys));
    }
    rows
}

/// Does a cell contain `query`? Case-insensitive, on the displayed text so
/// what the user searches for is what they can see.
pub fn cell_matches_search(value: &Value, query: &str) -> bool {
    if query.is_empty() {
        return false;
    }
    value
        .display_str()
        .to_lowercase()
        .contains(&query.to_lowercase())
}

/// `(row, col)` of every cell matching the active search, in display order.
pub fn search_matches(tab: &DataTab) -> Vec<(usize, usize)> {
    if tab.search_query.is_empty() {
        return Vec::new();
    }
    visible_records(tab)
        .iter()
        .enumerate()
        .flat_map(|(r, rec)| {
            rec.values
                .iter()
                .enumerate()
                .filter(|(_, v)| cell_matches_search(v, &tab.search_query))
                .map(move |(c, _)| (r, c))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Sort direction for the data grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortDir {
    Asc,
    Desc,
}

/// Human-readable byte size (KB / MB / GB).
pub fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Comparison operator for a client-side row filter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterOp {
    Eq,
    Ne,
    Gt,
    Lt,
    Contains,
}

/// A parsed client-side filter: keep rows where `col op value` holds.
#[derive(Clone, Debug)]
pub struct FilterExpr {
    pub col: usize,
    pub op: FilterOp,
    pub value: String,
}

impl FilterExpr {
    /// Re-render the expression as a string (used to pre-fill the edit box).
    pub fn display(&self) -> String {
        let sym = match self.op {
            FilterOp::Eq => "=",
            FilterOp::Ne => "!=",
            FilterOp::Gt => ">",
            FilterOp::Lt => "<",
            FilterOp::Contains => "~",
        };
        format!("{} {sym} {}", self.col, self.value)
    }
}

/// Parse a footer filter expression of the form `col op value` (e.g.
/// `status = paid`, `amount > 100`, `name ~ ada`). The operator is detected
/// by scanning for the first known symbol; column matches by exact name.
pub fn parse_filter(buf: &str, columns: &[String]) -> Option<FilterExpr> {
    let buf = buf.trim();
    if buf.is_empty() {
        return None;
    }
    for (sym, op) in [
        ("!=", FilterOp::Ne),
        ("=", FilterOp::Eq),
        (">", FilterOp::Gt),
        ("<", FilterOp::Lt),
        ("~", FilterOp::Contains),
    ] {
        if let Some(pos) = buf.find(sym) {
            let col_name = buf[..pos].trim();
            let value = buf[pos + sym.len()..].trim().to_string();
            if value.is_empty() {
                return None;
            }
            let col = columns.iter().position(|c| c == col_name)?;
            return Some(FilterExpr { col, op, value });
        }
    }
    None
}

/// Does `record` satisfy the filter? Non-numeric `Gt`/`Lt` fall back to
/// string comparison; `Eq`/`Ne`/`Contains` always use the display string.
pub fn record_matches_filter(record: &Record, f: &FilterExpr) -> bool {
    let Some(val) = record.values.get(f.col) else {
        return false;
    };
    let cell = val.display_str();
    match f.op {
        FilterOp::Eq => cell == f.value,
        FilterOp::Ne => cell != f.value,
        FilterOp::Contains => cell.contains(&f.value),
        FilterOp::Gt => match numeric_pair(&cell, &f.value) {
            Some((a, b)) => a > b,
            None => cell > f.value,
        },
        FilterOp::Lt => match numeric_pair(&cell, &f.value) {
            Some((a, b)) => a < b,
            None => cell < f.value,
        },
    }
}

/// Parse two strings as numbers; `Some((a, b))` when both parse.
fn numeric_pair(a: &str, b: &str) -> Option<(f64, f64)> {
    Some((a.parse::<f64>().ok()?, b.parse::<f64>().ok()?))
}

#[derive(Clone, Debug)]
pub struct ExplorerState {
    pub focused_pane: FocusedPane,
    pub namespaces: Vec<Namespace>,
    pub tables: std::collections::HashMap<String, Vec<Collection>>,
    /// Non-table objects per namespace (views / routines / sequences), loaded
    /// lazily when a database node is expanded.
    pub views: std::collections::HashMap<String, Vec<Collection>>,
    pub routines: std::collections::HashMap<String, Vec<Collection>>,
    pub sequences: std::collections::HashMap<String, Vec<Collection>>,
    /// Column names per `ns.table` — feeds the console autocomplete.
    pub column_cache: std::collections::HashMap<String, Vec<String>>,
    pub tree_nodes: Vec<TreeNode>,
    pub selected_tree_index: usize,
    /// First tree-node row currently visible in the tree pane. Adjusted on
    /// every draw so `selected_tree_index` stays in view when the list is
    /// taller than the viewport.
    pub tree_scroll: usize,
    /// Tree-pane inner area from the last draw — maps a mouse click to a node.
    pub tree_hit_area: Option<Rect>,

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
    pub object_search: Option<ObjectSearchState>,
    pub import_csv_modal: Option<ImportCsvModalState>,
    pub schema_edit_modal: Option<SchemaEditModalState>,
    pub create_object_modal: Option<CreateObjectModalState>,
    /// Explorer tree folded away to give the workspace the full width.
    pub tree_collapsed: bool,
    pub erd_menu: Option<ErdMenuState>,
    /// Same context menu, but opened from the explorer tree (`Ctrl+O` on a
    /// table node) instead of the ERD canvas.
    pub tree_menu: Option<ErdMenuState>,
    /// Query-plan overlay (`Ctrl+P` in a console).
    pub explain_plan: Option<ExplainPlanState>,
    /// Running-query monitor (`Ctrl+K`).
    pub process_list: Option<ProcessListState>,
    /// Schema-diff flow (`Alt+D`): first a connection picker, then the result.
    pub diff_picker: Option<DiffPickerState>,
    pub schema_diff: Option<SchemaDiffState>,
    /// Rect of the ERD context menu as last painted — lets a mouse click pick
    /// the item under the cursor.
    pub erd_menu_area: Option<Rect>,
    /// Painted rect of the tree context menu, for mouse hit-testing.
    pub tree_menu_area: Option<Rect>,
    pub driver_capabilities: crate::driver::Capabilities,
    /// X start of each workspace tab (terminal columns), recorded at draw
    /// time so a mouse click maps to the right tab.
    pub tab_starts: Vec<u16>,
    /// Area of the tab header row from the last draw.
    pub tab_bar_area: Option<Rect>,
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
            views: std::collections::HashMap::new(),
            routines: std::collections::HashMap::new(),
            sequences: std::collections::HashMap::new(),
            column_cache: std::collections::HashMap::new(),
            tree_nodes,
            selected_tree_index: 0,
            tree_scroll: 0,
            tree_hit_area: None,
            tabs: Vec::new(),
            active_tab_index: 0,
            ddl_popup: None,
            ddl_popup_area: None,
            export_modal: None,
            cell_edit_modal: None,
            sql_confirm_modal: None,
            insert_row_modal: None,
            object_search: None,
            import_csv_modal: None,
            schema_edit_modal: None,
            create_object_modal: None,
            tree_collapsed: false,
            erd_menu: None,
            tree_menu: None,
            erd_menu_area: None,
            tree_menu_area: None,
            explain_plan: None,
            process_list: None,
            diff_picker: None,
            schema_diff: None,
            driver_capabilities,
            tab_starts: Vec::new(),
            tab_bar_area: None,
        }
    }

    /// Give the tree focus, revealing it first if it was folded away.
    ///
    /// Focus must never land on a pane that is not drawn — the keys would go
    /// somewhere the user cannot see.
    pub fn focus_tree(&mut self) {
        self.tree_collapsed = false;
        self.focused_pane = FocusedPane::Tree;
    }

    /// Next selectable row from `from` in `dir` (+1 / -1), skipping section
    /// dividers. Returns `from` when there is nothing selectable that way, so
    /// the selection never lands on a label or runs off the list.
    pub fn next_selectable(&self, from: usize, dir: isize) -> usize {
        let n = self.tree_nodes.len();
        if n == 0 {
            return 0;
        }
        let mut i = from as isize;
        loop {
            i += dir;
            if i < 0 || i as usize >= n {
                return from;
            }
            if self.tree_nodes[i as usize].kind.is_selectable() {
                return i as usize;
            }
        }
    }

    /// Nearest selectable row at or after `from`, used after a rebuild or a
    /// click so the selection is never parked on a divider.
    pub fn snap_to_selectable(&self, from: usize) -> usize {
        if self
            .tree_nodes
            .get(from)
            .map(|n| n.kind.is_selectable())
            .unwrap_or(false)
        {
            return from;
        }
        let forward = self.next_selectable(from, 1);
        if forward != from {
            return forward;
        }
        self.next_selectable(from, -1)
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

            if is_expanded {
                let tables = self.tables.get(&ns.0).map(|v| v.len()).unwrap_or(0);
                if tables > 0 {
                    nodes.push(TreeNode {
                        kind: TreeNodeKind::Section("Tables", tables),
                        is_expanded: false,
                        is_loading: false,
                    });
                }
                if let Some(tbls) = self.tables.get(&ns.0) {
                    for tbl in tbls {
                        nodes.push(TreeNode {
                            kind: TreeNodeKind::Table(
                                CollectionRef {
                                    namespace: ns.clone(),
                                    name: tbl.name.clone(),
                                },
                                tbl.estimated_row_count,
                                tbl.estimated_size_bytes,
                            ),
                            is_expanded: false,
                            is_loading: false,
                        });
                    }
                }
                let push_objects = |nodes: &mut Vec<TreeNode>, list: &[Collection], kind: fn(CollectionRef) -> TreeNodeKind| {
                    for obj in list {
                        nodes.push(TreeNode {
                            kind: kind(CollectionRef {
                                namespace: ns.clone(),
                                name: obj.name.clone(),
                            }),
                            is_expanded: false,
                            is_loading: false,
                        });
                    }
                };
                // Each group is announced by a divider only when it has
                // members, so an empty section never costs a row.
                let section = |nodes: &mut Vec<TreeNode>, label: &'static str, n: usize| {
                    if n > 0 {
                        nodes.push(TreeNode {
                            kind: TreeNodeKind::Section(label, n),
                            is_expanded: false,
                            is_loading: false,
                        });
                    }
                };
                if let Some(views) = self.views.get(&ns.0) {
                    section(&mut nodes, "Views", views.len());
                    push_objects(&mut nodes, views, TreeNodeKind::View);
                }
                if let Some(routines) = self.routines.get(&ns.0) {
                    section(&mut nodes, "Routines", routines.len());
                    push_objects(&mut nodes, routines, TreeNodeKind::Routine);
                }
                if let Some(seqs) = self.sequences.get(&ns.0) {
                    section(&mut nodes, "Sequences", seqs.len());
                    push_objects(&mut nodes, seqs, TreeNodeKind::Sequence);
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
    // The console is the centre of gravity, so the tree can be folded away to
    // give it the full width (Ctrl+B).
    let tree_w = if state.tree_collapsed { 0 } else { 32 };
    let main_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(tree_w), // Explorer tree width
            Constraint::Min(48),        // Workspace / Table grid
        ])
        .split(area);

    if !state.tree_collapsed {
        render_tree(f, main_chunks[0], state, theme);
    } else {
        state.tree_hit_area = None;
    }
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

    if let Some(search) = &state.object_search {
        render_object_search(f, area, search, theme);
    }

    if let Some(import) = &state.import_csv_modal {
        render_import_csv_modal(f, area, import, theme);
    }

    if let Some(schema) = &state.schema_edit_modal {
        render_schema_edit_modal(f, area, schema, theme);
    }

    if let Some(create) = &state.create_object_modal {
        render_create_object_modal(f, area, create, theme);
    }

    // Cloned so the popup rect can be recorded back into `state` (the hit
    // area a mouse click tests against) while rendering.
    // Row detail rides on the active tab rather than a modal field: the data
    // is already in the tab, so there is nothing to keep in sync.
    if let Some(WorkspaceTab::Table(tab)) = state.active_tab()
        && tab.row_detail
    {
        render_row_detail(f, area, tab, theme);
    }

    if let Some(picker) = &state.diff_picker {
        render_diff_picker(f, area, picker, theme);
    }

    if let Some(diff) = &state.schema_diff {
        render_schema_diff(f, area, diff, theme);
    }

    if let Some(procs) = &state.process_list {
        render_process_list(f, area, procs, theme);
    }

    if let Some(plan) = &state.explain_plan {
        render_explain_plan(f, area, plan, theme);
    }

    if let Some(menu) = state.erd_menu.clone() {
        let rect = erd_menu_rect(area, &menu);
        state.erd_menu_area = Some(rect);
        render_erd_menu(f, rect, &menu, theme);
    } else {
        state.erd_menu_area = None;
    }

    if let Some(menu) = state.tree_menu.clone() {
        let rect = erd_menu_rect(area, &menu);
        state.tree_menu_area = Some(rect);
        render_erd_menu(f, rect, &menu, theme);
    } else {
        state.tree_menu_area = None;
    }
}

/// One row shown vertically — `column : value` per line — which is the only
/// readable way to inspect a table too wide to fit on screen.
fn render_row_detail(f: &mut Frame, area: Rect, tab: &DataTab, theme: &Theme) {
    let rows = visible_records(tab);
    let Some(record) = rows.get(tab.selected_row) else {
        return;
    };

    let width = 72.min(area.width.saturating_sub(4));
    let height = 20.min(area.height.saturating_sub(2));
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
        .title(format!(
            " {} — row {}/{} ",
            tab.collection.name,
            tab.selected_row + 1,
            rows.len()
        ));
    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    // Align the values into a column so long tables stay scannable.
    let name_w = tab
        .page
        .columns
        .iter()
        .map(|c| c.chars().count())
        .max()
        .unwrap_or(8)
        .min(24);

    let visible = chunks[0].height as usize;
    let mut lines = Vec::new();
    for (i, col) in tab
        .page
        .columns
        .iter()
        .enumerate()
        .skip(tab.row_detail_scroll)
        .take(visible)
    {
        let value = record
            .values
            .get(i)
            .map(|v| v.display_str())
            .unwrap_or_default();
        // Mark the key columns so the row reads like the table's own shape.
        let flag = tab
            .column_meta
            .iter()
            .find(|m| &m.name == col)
            .map(|m| {
                if m.is_primary_key {
                    "🔑"
                } else if m.is_foreign_key {
                    "🔗"
                } else {
                    "  "
                }
            })
            .unwrap_or("  ");
        let is_sel = i == tab.selected_col;
        lines.push(Line::from(vec![
            Span::styled(format!("{flag} "), theme.dim()),
            Span::styled(
                format!("{:<name_w$} : ", col.chars().take(name_w).collect::<String>()),
                if is_sel { theme.accent() } else { theme.dim() },
            ),
            Span::styled(
                value,
                if is_sel {
                    theme.selected()
                } else {
                    theme.base()
                },
            ),
        ]));
    }
    f.render_widget(Paragraph::new(lines), chunks[0]);

    let more = tab.page.columns.len().saturating_sub(tab.row_detail_scroll + visible);
    let hint = if more > 0 {
        format!(" ↑/↓ scroll ({more} more) · ←/→ row · v/Esc close ")
    } else {
        " ↑/↓ scroll · ←/→ row · v/Esc close ".to_string()
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(hint, theme.dim()))),
        chunks[1],
    );
}

/// Choose the connection to diff against.
fn render_diff_picker(f: &mut Frame, area: Rect, picker: &DiffPickerState, theme: &Theme) {
    let width = 56.min(area.width.saturating_sub(4));
    let height = (picker.connections.len() as u16 + 4).min(area.height.saturating_sub(2));
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
        .title(" Compare schema against ");
    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let mut lines = Vec::new();
    if picker.connections.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no other saved connection to compare with",
            theme.dim(),
        )));
    }
    for (i, name) in picker.connections.iter().enumerate() {
        let is_sel = i == picker.selected;
        lines.push(Line::from(vec![
            Span::styled(if is_sel { "▶ " } else { "  " }, theme.accent()),
            Span::styled(
                name.clone(),
                if is_sel { theme.selected() } else { theme.base() },
            ),
        ]));
    }
    f.render_widget(Paragraph::new(lines), chunks[0]);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " ↑/↓ select · Enter compare · Esc cancel ",
            theme.dim(),
        ))),
        chunks[1],
    );
}

/// The differences, plus the migration DDL they imply.
fn render_schema_diff(f: &mut Frame, area: Rect, diff: &SchemaDiffState, theme: &Theme) {
    let width = 96.min(area.width.saturating_sub(4));
    let height = 22.min(area.height.saturating_sub(2));
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
        .title(format!(
            " Schema diff: {} vs {} ({} differences) ",
            diff.namespace,
            diff.against,
            diff.diffs.len()
        ));
    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let visible = chunks[0].height as usize;
    let mut lines = Vec::new();
    if diff.diffs.is_empty() {
        lines.push(Line::from(Span::styled(
            "  schemas are identical",
            theme.success(),
        )));
    }
    for d in diff.diffs.iter().skip(diff.scroll).take(visible) {
        let text = d.describe();
        // Colour by direction so drift reads at a glance.
        let style = match text.chars().next() {
            Some('-') => theme.error(),
            Some('+') => theme.warning(),
            _ => theme.base(),
        };
        lines.push(Line::from(Span::styled(format!("  {text}"), style)));
    }
    f.render_widget(Paragraph::new(lines), chunks[0]);

    let more = diff.diffs.len().saturating_sub(diff.scroll + visible);
    let hint = if more > 0 {
        format!(" ↑/↓ scroll ({more} more) · y copy migration SQL · Esc close ")
    } else {
        " ↑/↓ scroll · y copy migration SQL · Esc close ".to_string()
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(hint, theme.dim()))),
        chunks[1],
    );
}

/// Running queries, one per line, with the longest-running highlighted.
fn render_process_list(f: &mut Frame, area: Rect, procs: &ProcessListState, theme: &Theme) {
    let width = 100.min(area.width.saturating_sub(4));
    let height = 20.min(area.height.saturating_sub(2));
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
        .title(format!(
            " Running Queries ({}) ",
            procs.result.records.len()
        ));
    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    if procs.result.records.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled(
                "  nothing running right now",
                theme.dim(),
            )),
            chunks[0],
        );
    } else {
        let col = |name: &str| {
            procs
                .result
                .columns
                .iter()
                .position(|c| c.eq_ignore_ascii_case(name))
        };
        let (pid_i, user_i, secs_i, query_i) =
            (col("pid"), col("user"), col("seconds"), col("query"));

        let visible = chunks[0].height as usize;
        let start = procs.selected.saturating_sub(visible / 2);
        let mut lines = Vec::new();
        for (i, rec) in procs
            .result
            .records
            .iter()
            .enumerate()
            .skip(start)
            .take(visible)
        {
            let get = |idx: Option<usize>| {
                idx.and_then(|i| rec.values.get(i))
                    .map(|v| v.display_str())
                    .unwrap_or_default()
            };
            let secs = get(secs_i);
            // A query running for a while is the one worth looking at.
            let is_slow = secs.parse::<f64>().map(|s| s >= 5.0).unwrap_or(false);
            let is_sel = i == procs.selected;
            let style = if is_sel {
                theme.selected()
            } else if is_slow {
                theme.warning()
            } else {
                theme.base()
            };
            let query = get(query_i).replace(['\n', '\r'], " ");
            lines.push(Line::from(vec![
                Span::styled(if is_sel { "▶ " } else { "  " }, theme.accent()),
                Span::styled(format!("{:>8} ", get(pid_i)), theme.dim()),
                Span::styled(format!("{:>5}s ", secs), if is_slow { theme.warning() } else { theme.dim() }),
                Span::styled(format!("{:<12} ", get(user_i)), theme.dim()),
                Span::styled(query, style),
            ]));
        }
        f.render_widget(Paragraph::new(lines), chunks[0]);
    }

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " ↑/↓ select · x cancel query · r refresh · Esc close ",
            theme.dim(),
        ))),
        chunks[1],
    );
}

/// The query plan as an indented tree.
///
/// Plans are deep and narrow, so an indented tree with box-drawing connectors
/// reads far better in a terminal than a box-and-arrow graph would — and the
/// costliest node is coloured so the bottleneck is the first thing seen.
fn render_explain_plan(f: &mut Frame, area: Rect, plan: &ExplainPlanState, theme: &Theme) {
    let width = 96.min(area.width.saturating_sub(4));
    let height = 22.min(area.height.saturating_sub(2));
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
        .title(format!(" Query Plan ({} steps) ", plan.nodes.len()));
    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let visible = chunks[0].height as usize;
    let mut lines = Vec::new();
    for (i, node) in plan
        .nodes
        .iter()
        .enumerate()
        .skip(plan.scroll)
        .take(visible)
    {
        // Last child of its depth gets the closing connector.
        let is_last = plan
            .nodes
            .iter()
            .skip(i + 1)
            .take_while(|n| n.depth >= node.depth)
            .all(|n| n.depth > node.depth);
        let connector = if node.depth == 0 {
            String::new()
        } else {
            format!("{}{}", "  ".repeat(node.depth - 1), if is_last { "└─ " } else { "├─ " })
        };

        let is_hot = plan.hotspot == Some(i);
        let label_style = if is_hot {
            theme.error().add_modifier(Modifier::BOLD)
        } else if node.cost.is_some() || node.rows.is_some() {
            theme.base()
        } else {
            // Detail lines ("Filter: ...") are context, not plan steps.
            theme.dim()
        };

        let mut spans = vec![
            Span::styled(connector, theme.dim()),
            Span::styled(node.label.clone(), label_style),
        ];
        let mut meta = String::new();
        if let Some(c) = node.cost {
            meta.push_str(&format!("  cost {c:.2}"));
        }
        if let Some(r) = node.rows {
            meta.push_str(&format!("  rows {r:.0}"));
        }
        if is_hot {
            meta.push_str("  ← hotspot");
        }
        if !meta.is_empty() {
            spans.push(Span::styled(
                meta,
                if is_hot { theme.error() } else { theme.dim() },
            ));
        }
        lines.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(lines), chunks[0]);

    let more = plan.nodes.len().saturating_sub(plan.scroll + visible);
    let hint = if more > 0 {
        format!(" ↑/↓ scroll ({more} more) · Esc close ")
    } else {
        " ↑/↓ scroll · Esc close ".to_string()
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(hint, theme.dim()))),
        chunks[1],
    );
}

/// Where the ERD context menu is painted: anchored near the click (clamped
/// on-screen), or centred when it was opened from the keyboard.
pub fn erd_menu_rect(area: Rect, menu: &ErdMenuState) -> Rect {
    let width = 30.min(area.width.saturating_sub(4));
    let height = (ERD_MENU_OPTIONS.len() as u16 + 2).min(area.height.saturating_sub(2));
    match menu.menu_at {
        Some((mx, my)) => {
            let max_x = area.x + area.width.saturating_sub(width);
            let max_y = area.y + area.height.saturating_sub(height);
            let x = mx.saturating_sub(2).clamp(area.x, max_x);
            let y = my.saturating_sub(1).clamp(area.y, max_y);
            Rect { x, y, width, height }
        }
        None => Rect {
            x: area.x + (area.width.saturating_sub(width)) / 2,
            y: area.y + (area.height.saturating_sub(height)) / 2,
            width,
            height,
        },
    }
}

/// Small context menu for an ERD node (View DDL / Open rows / Edit schema /
/// Delete table). `Enter` or a mouse click runs the highlighted action
/// (handled in the event loop / mouse handler); `Esc` closes.
fn render_erd_menu(f: &mut Frame, popup_area: Rect, menu: &ErdMenuState, theme: &Theme) {
    let options = ERD_MENU_OPTIONS;
    f.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme.accent())
        .style(theme.panel())
        .title(format!(" {} ", menu.collection.name));
    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let mut lines = Vec::new();
    for (i, label) in options.iter().enumerate() {
        let is_sel = i == menu.selected;
        let style = if is_sel {
            theme.selected()
        } else {
            theme.base()
        };
        lines.push(Line::from(vec![
            Span::styled(if is_sel { "▶ " } else { "  " }, theme.accent()),
            Span::styled(*label, style),
        ]));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// Overlay for creating a schema / table / view / type / function.
fn render_create_object_modal(
    f: &mut Frame,
    area: Rect,
    modal: &CreateObjectModalState,
    theme: &Theme,
) {
    let width = 56.min(area.width.saturating_sub(4));
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
        .title(" Create Object ");
    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Length(1)])
        .split(inner);

    // Kind picker.
    let mut kind_line = String::new();
    for (i, k) in CreateKind::ALL.iter().enumerate() {
        let mark = if *k == modal.kind { "▶" } else { " " };
        kind_line.push_str(&format!(" {mark}{} ", k.label()));
        if i < CreateKind::ALL.len() - 1 {
            kind_line.push('|');
        }
    }
    f.render_widget(
        Paragraph::new(Span::styled(kind_line, theme.accent())),
        chunks[0],
    );

    let name_line = Line::from(vec![
        Span::styled(
            format!("name> "),
            theme.accent().add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("{}█", modal.name), theme.base()),
    ]);
    f.render_widget(Paragraph::new(name_line), chunks[1]);

    let hint = Line::from(Span::styled(
        " ←/→ or ↑/↓ pick kind · type name · Enter review · Esc cancel",
        theme.dim(),
    ));
    f.render_widget(Paragraph::new(hint).alignment(Alignment::Center), chunks[2]);
}

/// Overlay for editing a table's schema (build an ALTER TABLE).
fn render_schema_edit_modal(
    f: &mut Frame,
    area: Rect,
    modal: &SchemaEditModalState,
    theme: &Theme,
) {
    let width = 64.min(area.width.saturating_sub(4));
    let height = 20.min(area.height.saturating_sub(2));
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
        .title(format!(
            " Edit Schema: {}.{} ",
            modal.collection.namespace.0, modal.collection.name
        ));
    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([Constraint::Min(1), Constraint::Length(2)])
        .split(inner);

    // Follow-scroll: keep the selected column in view when the column list is
    // taller than the modal (same approach as the tree pane).
    let visible = (chunks[0].height as usize).max(1);
    let sel = modal.selected.min(modal.columns.len().saturating_sub(1));
    let start = sel.saturating_sub(visible / 2);
    let end = (start + visible).min(modal.columns.len() + modal.add_cols.len());

    let mut lines = Vec::new();
    for i in start..end {
        if i < modal.columns.len() {
            let col = &modal.columns[i];
            let dropped = modal.drop_cols.contains(&col.name);
            let is_sel = i == sel;
            let marker = if dropped {
                "✗"
            } else if is_sel {
                "▶"
            } else {
                " "
            };
            let style = if dropped {
                theme.dim()
            } else if is_sel {
                theme.selected()
            } else {
                theme.base()
            };
            let pk = if col.is_primary_key { " PK" } else { "" };
            let type_change = modal
                .type_changes
                .iter()
                .find(|(c, _)| c == &col.name)
                .map(|(_, t)| format!(" → {t}"))
                .unwrap_or_default();
            lines.push(Line::from(Span::styled(
                format!(
                    " {marker} {} {}{type_change}{pk}",
                    col.name, col.data_type
                ),
                style,
            )));
        } else {
            let j = i - modal.columns.len();
            if let Some((name, ty)) = modal.add_cols.get(j) {
                lines.push(Line::from(Span::styled(
                    format!(" + +{name} {ty}"),
                    theme.success(),
                )));
            }
        }
    }
    f.render_widget(Paragraph::new(lines), chunks[0]);

    let hint = if let Some(input) = &modal.input {
        match input {
            SchemaInput::AddColumn { name, ty, stage } => {
                if *stage == 0 {
                    format!("add column name> {name}█")
                } else {
                    format!("add column {name} type> {ty}█")
                }
            }
            SchemaInput::RenameTable { value } => format!("rename table to> {value}█"),
            SchemaInput::ChangeType { column, value } => {
                format!("change type of {column} to> {value}█")
            }
        }
    } else {
        let rename = modal
            .rename_table
            .as_ref()
            .map(|n| format!(" · rename→{n}"))
            .unwrap_or_default();
        format!(" ↑/↓ nav · d drop · a add · c type · r rename{rename} · Enter apply · Esc close")
    };
    f.render_widget(
        Paragraph::new(Span::styled(hint, theme.dim())).alignment(Alignment::Center),
        chunks[1],
    );
}

/// Centered overlay for CSV import: path input + preview of parsed rows.
fn render_import_csv_modal(f: &mut Frame, area: Rect, modal: &ImportCsvModalState, theme: &Theme) {
    let width = 72.min(area.width.saturating_sub(4));
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
        .title(" Import CSV ");
    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1), // path
            Constraint::Length(1), // status
            Constraint::Min(1),    // preview
            Constraint::Length(1), // hint
        ])
        .split(inner);

    let path_line = Line::from(vec![
        Span::styled("file> ", theme.accent().add_modifier(Modifier::BOLD)),
        Span::styled(format!("{}█", modal.path), theme.base()),
    ]);
    f.render_widget(Paragraph::new(path_line), chunks[0]);

    if modal.parsed {
        let status = format!("{} rows, {} columns", modal.rows.len().saturating_sub(1), modal.rows.first().map(|r| r.len()).unwrap_or(0));
        f.render_widget(Paragraph::new(Span::styled(status, theme.dim())), chunks[1]);

        // Preview up to 5 data rows (skip header).
        let mut lines = Vec::new();
        for row in modal.rows.iter().skip(1).take(5) {
            lines.push(Line::from(Span::styled(
                row.join(" | "),
                theme.base(),
            )));
        }
        f.render_widget(Paragraph::new(lines), chunks[2]);
    } else {
        f.render_widget(Paragraph::new(Span::styled("(press Enter to read the file)", theme.dim())), chunks[1]);
    }

    let hint = Line::from(vec![
        Span::styled("[Enter] ", theme.accent()),
        Span::styled(if modal.parsed { "Insert rows  " } else { "Read file  " }, theme.base()),
        Span::styled("[Esc] Cancel", theme.dim()),
    ]);
    f.render_widget(Paragraph::new(hint).alignment(Alignment::Center), chunks[3]);
}

/// Centered overlay for the object search (`Ctrl+T`): a text input at the
/// top, live-filtered list of matching objects below. `Enter` opens the
/// highlighted object (handled in the event loop).
fn render_object_search(f: &mut Frame, area: Rect, state: &ObjectSearchState, theme: &Theme) {
    let width = 60.min(area.width.saturating_sub(4));
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
        .title(" Search Objects (Ctrl+T) ");
    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1), // input
            Constraint::Length(1), // match count
            Constraint::Min(1),    // list
            Constraint::Length(1), // footer
        ])
        .split(inner);

    let input_line = Line::from(vec![
        Span::styled("> ", theme.accent().add_modifier(Modifier::BOLD)),
        Span::styled(format!("{}█", state.query), theme.base()),
    ]);
    f.render_widget(Paragraph::new(input_line), chunks[0]);

    let filtered: Vec<&(CollectionRef, SearchKind)> = state
        .results
        .iter()
        .filter(|(r, _)| {
            r.name.contains(&state.query) || r.namespace.0.contains(&state.query)
        })
        .collect();

    let match_line = Line::from(Span::styled(
        format!("{} match(es) | ↑/↓ navigate · Enter open · Esc cancel", filtered.len()),
        theme.dim(),
    ));
    f.render_widget(Paragraph::new(match_line), chunks[1]);

    let mut lines = Vec::new();
    let max_rows = (inner.height.saturating_sub(4)) as usize;
    let sel = state.selected.min(filtered.len().saturating_sub(1));
    let start = sel.saturating_sub(max_rows / 2);
    for (i, (r, kind)) in filtered.iter().skip(start).take(max_rows).enumerate() {
        let is_sel = start + i == sel;
        let style = if is_sel {
            theme.selected()
        } else {
            theme.base()
        };
        let icon = match kind {
            SearchKind::Table => "📄",
            SearchKind::View => "👁️",
            SearchKind::Routine => "⚙️",
            SearchKind::Sequence => "🔢",
        };
        lines.push(Line::from(vec![
            Span::styled(if is_sel { "▶ " } else { "  " }, theme.accent()),
            Span::styled(format!("{} {}.{}", icon, r.namespace.0, r.name), style),
        ]));
    }
    f.render_widget(Paragraph::new(lines), chunks[2]);

    let footer = Line::from(vec![
        Span::styled("[↑/↓] Navigate  ", theme.dim()),
        Span::styled("[Enter] Open  ", theme.accent()),
        Span::styled("[Esc] Cancel", theme.dim()),
    ]);
    f.render_widget(Paragraph::new(footer).alignment(Alignment::Center), chunks[3]);
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
    state.tree_hit_area = Some(inner);

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
        // The selection stays marked even when the tree loses focus — opening a
        // table moves focus to the workspace, and losing your place in the
        // list at that moment is disorienting. Focus only changes how loud the
        // marker is.
        let is_sel = i == sel;
        let sel_style = if is_focused {
            theme.selected()
        } else {
            theme.selected_inactive()
        };
        let line = match &node.kind {
            // A rule that names the group and how many are in it, so the list
            // stays scannable without another level of expanding.
            TreeNodeKind::Section(label, count) => {
                let head = format!("  {label} ({count}) ");
                let rule_w = (area.width as usize)
                    .saturating_sub(head.chars().count() + 3);
                Line::from(vec![
                    Span::styled(head, theme.accent().add_modifier(Modifier::BOLD)),
                    Span::styled("─".repeat(rule_w), theme.dim()),
                ])
            }
            TreeNodeKind::Database(ns) => {
                let prefix = if node.is_loading {
                    "⏳ "
                } else if node.is_expanded {
                    "▼ "
                } else {
                    "▶ "
                };
                let style = if is_sel {
                    sel_style.add_modifier(Modifier::BOLD)
                } else {
                    theme.base().add_modifier(Modifier::BOLD)
                };
                Line::from(vec![
                    Span::styled(prefix, theme.accent()),
                    Span::styled(format!("📁 {}", ns.0), style),
                ])
            }
            TreeNodeKind::Table(cref, _count, size) => {
                // Size is the only metadata shown in the tree — no fallback
                // to the row count (that estimate is less useful and the pane
                // is narrow).
                let meta = if let Some(s) = size {
                    format!(" (~{})", format_size(*s))
                } else {
                    String::new()
                };
                let style = if is_sel { sel_style } else { theme.base() };
                Line::from(vec![
                    Span::styled("   📄 ", theme.dim()),
                    Span::styled(&cref.name, style),
                    Span::styled(meta, theme.dim()),
                ])
            }
            TreeNodeKind::View(cref) => {
                let style = if is_sel { sel_style } else { theme.base() };
                Line::from(vec![
                    Span::styled("   👁️ ", theme.dim()),
                    Span::styled(&cref.name, style),
                ])
            }
            TreeNodeKind::Routine(cref) => {
                let style = if is_sel { sel_style } else { theme.base() };
                Line::from(vec![
                    Span::styled("   ⚙️ ", theme.dim()),
                    Span::styled(&cref.name, style),
                ])
            }
            TreeNodeKind::Sequence(cref) => {
                let style = if is_sel { sel_style } else { theme.base() };
                Line::from(vec![
                    Span::styled("   🔢 ", theme.dim()),
                    Span::styled(&cref.name, style),
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

    // 1. Tab headers — record each tab's x-start so a click can switch tabs.
    let mut tab_spans = Vec::new();
    let mut tab_starts = Vec::new();
    let mut x = chunks[0].x;
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
        let label = format!(" [ {icon} {} ] ", tab.title());
        tab_starts.push(x);
        x = x.saturating_add(unicode_width::UnicodeWidthStr::width(label.as_str()) as u16 + 1);
        tab_spans.push(Span::styled(label, style));
        tab_spans.push(Span::raw(" "));
    }
    state.tab_starts = tab_starts;
    state.tab_bar_area = Some(chunks[0]);
    f.render_widget(Paragraph::new(Line::from(tab_spans)), chunks[0]);

    // 2. Active Tab Body
    if let Some(tab) = state.active_tab_mut() {
        match tab {
            WorkspaceTab::Table(data_tab) => {
                render_grid(f, chunks[1], data_tab, is_focused, theme);

                // 3. Pagination Footer. While the user is typing a filter, the
                // footer becomes the filter input line.
                let p = if data_tab.search_editing {
                    let input = format!(
                        "[search] {}_ [Enter] find  [Esc] cancel",
                        data_tab.search_buffer
                    );
                    Paragraph::new(Span::styled(input, theme.accent()))
                } else if data_tab.filter_editing {
                    let input = format!("[filter] {}_ [Enter] apply  [Esc] cancel", data_tab.filter_buffer);
                    Paragraph::new(Span::styled(input, theme.accent()))
                } else {
                    let total_str = data_tab
                        .page
                        .total_records
                        .map(|t| format!(" of {} total", t))
                        .unwrap_or_default();
                    let filter_badge = data_tab.filter.as_ref().map(|f| {
                        let shown = data_tab
                            .page
                            .records
                            .iter()
                            .filter(|r| record_matches_filter(r, f))
                            .count();
                        format!(" | filter {} ({}/{} shown)", f.display(), shown, data_tab.page.records.len())
                    });
                    let ro = if data_tab.read_only { " [read-only view]" } else { "" };
                    let sort_badge = if data_tab.sort_keys.len() > 1 {
                        format!(" | sorted by {} columns", data_tab.sort_keys.len())
                    } else {
                        String::new()
                    };
                    let search_badge = if data_tab.search_query.is_empty() {
                        String::new()
                    } else {
                        format!(
                            " | search '{}' ({} hits, Ctrl+G next)",
                            data_tab.search_query,
                            search_matches(data_tab).len()
                        )
                    };
                    let footer_text = format!(
                        " Page {} (showing {} rows{}){}{}{} | [v] row  [s/S] sort/clear  [/] filter  [Ctrl+F] search  [n]/[p] page  [w] Close",
                        data_tab.page.page + 1,
                        data_tab.page.records.len(),
                        total_str,
                        ro,
                        filter_badge.unwrap_or_default(),
                        search_badge
                    ) + &sort_badge;
                    Paragraph::new(Span::styled(footer_text, theme.dim()))
                };
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

fn render_grid(f: &mut Frame, area: Rect, tab: &mut DataTab, is_focused: bool, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(if is_focused { theme.accent() } else { theme.border() })
        .style(theme.base());
    tab.grid_hit_area = Some(block.inner(area));

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

    // Horizontal scroll: columns keep a fixed minimum width (16) and only the
    // ones that fit the pane are rendered — the rest are reached via h/l (or
    // the col_starts the mouse handler uses). If everything fits, they still
    // grow to fill the pane.
    let inner_w = tab.grid_hit_area.map(|r| r.width).unwrap_or(80);
    let max_visible = (inner_w / 16).max(1) as usize;
    let num_visible = num_cols.saturating_sub(col_offset).max(1);
    let show_all = num_visible <= max_visible;
    let take_n = if show_all { num_visible } else { max_visible };

    // Record the rendered x-start of each visible column so mouse clicks map
    // to the exact same widths the Table widget computed.
    if let Some(inner) = tab.grid_hit_area {
        let col_w = if show_all {
            (inner.width / num_visible as u16).max(1)
        } else {
            16
        };
        tab.grid_col_starts = (0..take_n)
            .map(|i| inner.x + (i as u16 * col_w))
            .collect();
    }

    // Column highlight: when focused, every cell in the active column gets a
    // dim "ruler" background so the user can track the cursor across rows.
    // The active cell (row ∩ col) gets the full accent highlight.
    let col_highlight_bg = theme.panel;
    let active_cell_style = if is_focused {
        Style::default()
            .bg(theme.accent)
            .fg(theme.background)
            .add_modifier(Modifier::BOLD)
    } else {
        theme.selected_inactive().add_modifier(Modifier::BOLD)
    };
    let col_highlight_style = Style::default()
        .bg(col_highlight_bg)
        .fg(theme.accent);
    let row_highlight_style = if is_focused {
        theme.selected()
    } else {
        theme.selected_inactive()
    };

    // Sort indicator: direction plus the key's position, so a multi-column
    // sort shows which column decides and which only breaks ties.
    let sort_indicator = |abs_col: usize| -> String {
        match tab.sort_keys.iter().position(|(c, _)| *c == abs_col) {
            Some(i) => {
                let arrow = match tab.sort_keys[i].1 {
                    SortDir::Asc => "↑",
                    SortDir::Desc => "↓",
                };
                // Rank is only worth showing once more than one key is active.
                if tab.sort_keys.len() > 1 {
                    format!(" {arrow}{}", i + 1)
                } else {
                    format!(" {arrow}")
                }
            }
            None => String::new(),
        }
    };

    let header_cells = tab
        .page
        .columns
        .iter()
        .skip(col_offset)
        .take(take_n)
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
            Cell::from(Span::styled(format!("{col}{}", sort_indicator(abs_col)), style))
        });
    let header = TableRow::new(header_cells).height(1).bottom_margin(1);

    let rec_refs = visible_records(tab);

    let rows: Vec<TableRow> = rec_refs
        .iter()
        .enumerate()
        .map(|(r_idx, record)| {
            // Keep the cursor visible when the grid is not the focused pane
            // (e.g. after Tab to the tree): only its intensity changes.
            let is_row_sel = r_idx == tab.selected_row;
            let cells = record.values.iter().skip(col_offset).take(take_n).enumerate().map(|(rel_idx, val)| {
                let abs_col = col_offset + rel_idx;
                let cell_str = val.display_str();
                let is_cell_sel = is_row_sel && abs_col == tab.selected_col;
                let is_col_sel = abs_col == tab.selected_col;
                // A search hit outranks the column highlight (but not the
                // cursor) so matches stay findable while navigating.
                let is_hit = cell_matches_search(val, &tab.search_query);
                let cell_style = if is_cell_sel {
                    active_cell_style
                } else if is_hit {
                    theme.warning().add_modifier(Modifier::BOLD)
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
        .take(take_n)
        .map(|_| {
            if show_all {
                Constraint::Min(16)
            } else {
                Constraint::Length(16)
            }
        })
        .collect();

    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .style(theme.base());

    let mut state = TableState::default();
    state.select(Some(tab.selected_row));

    f.render_stateful_widget(table, area, &mut state);
}

/// Compare two records on the given column, applying the sort direction.
/// Compare two rows across every sort key, in priority order: the first key
/// that separates them decides, the rest only break ties.
pub fn compare_by_keys(a: &Record, b: &Record, keys: &[(usize, SortDir)]) -> std::cmp::Ordering {
    for &(col, dir) in keys {
        let ord = compare_records(a, b, col, dir);
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    std::cmp::Ordering::Equal
}

pub fn compare_records(a: &Record, b: &Record, col: usize, dir: SortDir) -> std::cmp::Ordering {
    let va = a.values.get(col).unwrap_or(&crate::driver::Value::Null);
    let vb = b.values.get(col).unwrap_or(&crate::driver::Value::Null);
    let ord = compare_values(va, vb);
    match dir {
        SortDir::Asc => ord,
        SortDir::Desc => ord.reverse(),
    }
}

/// Order two cell values for client-side sorting. NULLs sort first
/// (ascending); numeric-like values compare numerically; everything else
/// falls back to the display string.
fn compare_values(a: &crate::driver::Value, b: &crate::driver::Value) -> std::cmp::Ordering {
    use crate::driver::Value;
    match (a, b) {
        (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
        (Value::Null, _) => std::cmp::Ordering::Less,
        (_, Value::Null) => std::cmp::Ordering::Greater,
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        _ => {
            if let (Some(x), Some(y)) = (value_as_number(a), value_as_number(b)) {
                x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal)
            } else {
                a.display_str().cmp(&b.display_str())
            }
        }
    }
}

/// Interpret a cell value as a number for sorting, if it's numeric-like.
fn value_as_number(v: &crate::driver::Value) -> Option<f64> {
    use crate::driver::Value;
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::UInt(u) => Some(*u as f64),
        Value::Float(f) => Some(*f),
        Value::Decimal(s) => s.parse::<f64>().ok(),
        _ => None,
    }
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
    let nullable_tag = if modal.is_nullable { " [NULLABLE]" } else { "" };
    let title = format!(
        " Edit Value: {}.{} ({}) (Row #{}, Col #{}){nullable_tag} ",
        modal.collection.name,
        modal.column_name,
        modal.data_type,
        modal.row_idx + 1,
        modal.col_idx + 1
    );
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

    if modal.is_boolean {
        // Boolean dropdown: true / false / NULL (only when nullable).
        let options: [(&str, usize); 3] = [("true", 0), ("false", 1), ("NULL", 2)];
        let mut lines = Vec::new();
        for (label, idx) in options.iter() {
            if *idx > 1 && !modal.is_nullable {
                continue;
            }
            let is_sel = modal.bool_selection == *idx;
            let style = if is_sel {
                theme.selected()
            } else {
                theme.base()
            };
            lines.push(Line::from(Span::styled(
                format!("  {} {label}", if is_sel { "▶" } else { " " }),
                style,
            )));
        }
        f.render_widget(Paragraph::new(lines), chunks[0]);

        let hint_line = Line::from(vec![
            Span::styled("[↑/↓] ", theme.accent().add_modifier(Modifier::BOLD)),
            Span::styled("choose  ", theme.dim()),
            Span::styled("[Enter] ", theme.accent()),
            Span::styled("Review SQL  ", theme.dim()),
            Span::styled("[Esc] ", theme.accent()),
            Span::styled("Cancel", theme.dim()),
        ]);
        f.render_widget(Paragraph::new(hint_line).alignment(Alignment::Center), chunks[1]);
        return;
    }

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


#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::Value;

    fn record(values: Vec<Value>) -> Record {
        Record { values }
    }

    /// Minimal `DataTab` over `columns`/`records` for the display-order and
    /// search helpers.
    fn tab_with(columns: &[&str], records: Vec<Record>) -> DataTab {
        DataTab {
            collection: CollectionRef {
                namespace: Namespace("main".to_string()),
                name: "t".to_string(),
            },
            page: RecordPage {
                columns: columns.iter().map(|c| c.to_string()).collect(),
                records,
                page: 0,
                page_size: 50,
                total_records: None,
            },
            selected_row: 0,
            selected_col: 0,
            scroll_offset_x: 0,
            column_meta: Vec::new(),
            foreign_keys: Vec::new(),
            sort_keys: Vec::new(),
            filter: None,
            filter_editing: false,
            filter_buffer: String::new(),
            read_only: false,
            grid_hit_area: None,
            grid_col_starts: Vec::new(),
            row_detail: false,
            row_detail_scroll: 0,
            search_query: String::new(),
            search_editing: false,
            search_buffer: String::new(),
        }
    }

    #[test]
    fn test_cell_matches_search_is_case_insensitive_on_displayed_text() {
        assert!(cell_matches_search(&Value::String("Ada".into()), "ada"));
        assert!(cell_matches_search(&Value::String("ada".into()), "AD"));
        assert!(cell_matches_search(&Value::Int(1234), "23"));
        // NULL is searchable by what the grid actually shows.
        assert!(cell_matches_search(&Value::Null, "null"));
        assert!(!cell_matches_search(&Value::String("bob".into()), "ada"));
        // An empty query must never match, or every cell would light up.
        assert!(!cell_matches_search(&Value::String("bob".into()), ""));
    }

    #[test]
    fn test_search_matches_uses_display_order_not_storage_order() {
        let mut tab = tab_with(
            &["id", "name"],
            vec![
                record(vec![Value::Int(1), Value::String("zoe".into())]),
                record(vec![Value::Int(2), Value::String("ada".into())]),
            ],
        );
        tab.search_query = "ada".to_string();
        // Natural order: "ada" is the second row.
        assert_eq!(search_matches(&tab), vec![(1, 1)]);

        // Sorted by name, "ada" moves to the top — the match must follow the
        // rows the user can actually see.
        tab.sort_keys = vec![(1, SortDir::Asc)];
        assert_eq!(search_matches(&tab), vec![(0, 1)]);

        // No query -> no matches at all.
        tab.search_query.clear();
        assert!(search_matches(&tab).is_empty());
    }

    /// `selected_row` indexes the DISPLAYED rows. Row actions (copy, edit and
    /// especially DELETE) used to read `page.records` in natural order, so with
    /// a sort or filter active they targeted a different row than the one the
    /// user had highlighted.
    #[test]
    fn test_selected_row_resolves_through_the_displayed_order() {
        let mut tab = tab_with(
            &["id"],
            vec![
                record(vec![Value::Int(30)]),
                record(vec![Value::Int(10)]),
                record(vec![Value::Int(20)]),
            ],
        );
        // Sorted ascending the display order is 10, 20, 30.
        tab.sort_keys = vec![(0, SortDir::Asc)];
        tab.selected_row = 0;

        let displayed = visible_records(&tab);
        assert_eq!(displayed[tab.selected_row].values[0].display_str(), "10");
        // The natural-order read that used to back DELETE points elsewhere.
        assert_eq!(tab.page.records[tab.selected_row].values[0].display_str(), "30");
    }

    #[test]
    fn test_filter_shrinks_the_selectable_row_range() {
        let mut tab = tab_with(
            &["id"],
            vec![
                record(vec![Value::Int(1)]),
                record(vec![Value::Int(2)]),
                record(vec![Value::Int(3)]),
            ],
        );
        tab.filter = parse_filter("id = 2", &tab.page.columns);
        // Only one row is displayed, so only index 0 is a valid selection even
        // though `page.records` still holds three.
        assert_eq!(visible_records(&tab).len(), 1);
        assert_eq!(tab.page.records.len(), 3);
    }

    #[test]
    fn test_multi_column_sort_uses_later_keys_only_to_break_ties() {
        // (status, priority): sorting by status first must group the statuses,
        // with priority deciding only inside each group.
        let mut tab = tab_with(
            &["status", "priority"],
            vec![
                record(vec![Value::String("open".into()), Value::Int(2)]),
                record(vec![Value::String("done".into()), Value::Int(9)]),
                record(vec![Value::String("open".into()), Value::Int(1)]),
                record(vec![Value::String("done".into()), Value::Int(3)]),
            ],
        );
        tab.sort_keys = vec![(0, SortDir::Asc), (1, SortDir::Asc)];

        let got: Vec<String> = visible_records(&tab)
            .iter()
            .map(|r| format!("{}/{}", r.values[0].display_str(), r.values[1].display_str()))
            .collect();
        assert_eq!(got, vec!["done/3", "done/9", "open/1", "open/2"]);

        // Flipping only the secondary key reorders within each group, not across.
        tab.sort_keys = vec![(0, SortDir::Asc), (1, SortDir::Desc)];
        let got: Vec<String> = visible_records(&tab)
            .iter()
            .map(|r| format!("{}/{}", r.values[0].display_str(), r.values[1].display_str()))
            .collect();
        assert_eq!(got, vec!["done/9", "done/3", "open/2", "open/1"]);
    }

    #[test]
    fn test_sort_key_order_decides_priority() {
        let rows = vec![
            record(vec![Value::String("b".into()), Value::Int(1)]),
            record(vec![Value::String("a".into()), Value::Int(2)]),
        ];
        let mut tab = tab_with(&["name", "n"], rows);

        // Primary = name -> "a" first.
        tab.sort_keys = vec![(0, SortDir::Asc), (1, SortDir::Asc)];
        assert_eq!(visible_records(&tab)[0].values[0].display_str(), "a");

        // Swap the priority -> the numeric column decides instead.
        tab.sort_keys = vec![(1, SortDir::Asc), (0, SortDir::Asc)];
        assert_eq!(visible_records(&tab)[0].values[0].display_str(), "b");
    }

    #[test]
    fn test_compare_by_keys_is_equal_when_no_key_separates() {
        let a = record(vec![Value::Int(1), Value::String("x".into())]);
        let b = record(vec![Value::Int(1), Value::String("x".into())]);
        assert_eq!(
            compare_by_keys(&a, &b, &[(0, SortDir::Asc), (1, SortDir::Asc)]),
            std::cmp::Ordering::Equal
        );
        // No keys at all leaves everything equal, i.e. natural order.
        assert_eq!(compare_by_keys(&a, &b, &[]), std::cmp::Ordering::Equal);
    }

    #[test]
    fn test_visible_records_applies_filter_then_sort() {
        let mut tab = tab_with(
            &["id"],
            vec![
                record(vec![Value::Int(3)]),
                record(vec![Value::Int(1)]),
                record(vec![Value::Int(2)]),
            ],
        );
        tab.sort_keys = vec![(0, SortDir::Asc)];
        let ids: Vec<String> = visible_records(&tab)
            .iter()
            .map(|r| r.values[0].display_str())
            .collect();
        assert_eq!(ids, vec!["1", "2", "3"]);

        tab.sort_keys = vec![(0, SortDir::Desc)];
        let ids: Vec<String> = visible_records(&tab)
            .iter()
            .map(|r| r.values[0].display_str())
            .collect();
        assert_eq!(ids, vec!["3", "2", "1"]);
    }

    fn coll(name: &str) -> Collection {
        Collection {
            name: name.to_string(),
            estimated_row_count: None,
            estimated_size_bytes: None,
        }
    }

    /// Explorer with one expanded schema holding one of each object type.
    fn expanded_state() -> ExplorerState {
        let ns = Namespace("shop".to_string());
        let mut st = ExplorerState::new(vec![ns.clone()], crate::driver::Capabilities::all());
        st.tables.insert(ns.0.clone(), vec![coll("users"), coll("orders")]);
        st.views.insert(ns.0.clone(), vec![coll("active_users")]);
        st.routines.insert(ns.0.clone(), vec![coll("calc")]);
        st.tree_nodes[0].is_expanded = true;
        st.rebuild_tree_nodes();
        st
    }

    #[test]
    fn test_focusing_the_tree_reveals_it_when_collapsed() {
        // Ctrl+B hides the tree; anything that later hands focus back must
        // bring it on screen, or keys would go to a pane nobody can see.
        let mut st = expanded_state();
        st.tree_collapsed = true;
        st.focused_pane = FocusedPane::Workspace;

        st.focus_tree();
        assert!(!st.tree_collapsed, "tree must be revealed when focused");
        assert_eq!(st.focused_pane, FocusedPane::Tree);
    }

    #[test]
    fn test_tree_groups_objects_under_section_dividers() {
        let st = expanded_state();
        let kinds: Vec<String> = st
            .tree_nodes
            .iter()
            .map(|n| match &n.kind {
                TreeNodeKind::Database(d) => format!("db:{}", d.0),
                TreeNodeKind::Section(l, c) => format!("== {l} ({c})"),
                TreeNodeKind::Table(c, _, _) => format!("table:{}", c.name),
                TreeNodeKind::View(c) => format!("view:{}", c.name),
                TreeNodeKind::Routine(c) => format!("routine:{}", c.name),
                TreeNodeKind::Sequence(c) => format!("seq:{}", c.name),
            })
            .collect();

        assert_eq!(
            kinds,
            vec![
                "db:shop",
                "== Tables (2)",
                "table:users",
                "table:orders",
                "== Views (1)",
                "view:active_users",
                "== Routines (1)",
                "routine:calc",
            ]
        );
        // Sequences are empty here, so no divider is spent on them.
        assert!(!kinds.iter().any(|k| k.contains("Sequences")));
    }

    #[test]
    fn test_navigation_steps_over_section_dividers() {
        let st = expanded_state();
        // Walking down from the schema must land on a table, never a label.
        let mut i = 0;
        let mut visited = Vec::new();
        for _ in 0..8 {
            let next = st.next_selectable(i, 1);
            if next == i {
                break;
            }
            i = next;
            visited.push(i);
            assert!(
                st.tree_nodes[i].kind.is_selectable(),
                "navigation landed on a divider at {i}"
            );
        }
        // Every real object is reachable (2 tables + 1 view + 1 routine).
        assert_eq!(visited.len(), 4);

        // And back up again, still skipping labels.
        while st.next_selectable(i, -1) != i {
            i = st.next_selectable(i, -1);
            assert!(st.tree_nodes[i].kind.is_selectable());
        }
        assert_eq!(i, 0, "should walk all the way back to the schema row");
    }

    #[test]
    fn test_snap_to_selectable_never_parks_on_a_divider() {
        let st = expanded_state();
        // Index 1 is the "Tables" divider; a click or page jump there must
        // resolve to a real row.
        assert!(matches!(st.tree_nodes[1].kind, TreeNodeKind::Section(..)));
        let snapped = st.snap_to_selectable(1);
        assert!(st.tree_nodes[snapped].kind.is_selectable());
        // A row that is already fine is left alone.
        assert_eq!(st.snap_to_selectable(2), 2);
    }

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(500), "500 B");
        assert_eq!(format_size(2048), "2.0 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(format_size(2 * 1024 * 1024 * 1024), "2.0 GB");
        assert_eq!(format_size(0), "0 B");
    }

    #[test]
    fn test_compare_values_numeric() {
        use std::cmp::Ordering;
        // 10 < 2 numerically (not lexicographically).
        assert_eq!(
            compare_values(&Value::Int(10), &Value::Int(2)),
            Ordering::Greater
        );
        // Int vs Decimal cross-type.
        assert_eq!(
            compare_values(&Value::Int(2), &Value::Decimal("10.5".to_string())),
            Ordering::Less
        );
        // Float.
        assert_eq!(
            compare_values(&Value::Float(1.5), &Value::Float(1.25)),
            Ordering::Greater
        );
    }

    #[test]
    fn test_compare_values_null_sorts_first() {
        use std::cmp::Ordering;
        assert_eq!(compare_values(&Value::Null, &Value::Int(1)), Ordering::Less);
        assert_eq!(compare_values(&Value::Int(1), &Value::Null), Ordering::Greater);
        assert_eq!(compare_values(&Value::Null, &Value::Null), Ordering::Equal);
    }

    #[test]
    fn test_compare_records_respects_direction() {
        use crate::driver::Value;
        let a = record(vec![Value::Int(1)]);
        let b = record(vec![Value::Int(2)]);
        assert_eq!(compare_records(&a, &b, 0, SortDir::Asc), std::cmp::Ordering::Less);
        assert_eq!(compare_records(&a, &b, 0, SortDir::Desc), std::cmp::Ordering::Greater);
    }

    #[test]
    fn test_parse_filter() {
        let cols = vec!["status".to_string(), "amount".to_string(), "name".to_string()];
        let f = parse_filter("status = paid", &cols).expect("parse");
        assert_eq!(f.col, 0);
        assert_eq!(f.op, FilterOp::Eq);
        assert_eq!(f.value, "paid");

        let f = parse_filter("amount > 100", &cols).expect("parse");
        assert_eq!(f.col, 1);
        assert_eq!(f.op, FilterOp::Gt);

        let f = parse_filter("name ~ ada", &cols).expect("parse");
        assert_eq!(f.col, 2);
        assert_eq!(f.op, FilterOp::Contains);

        // Unknown column / malformed → None.
        assert!(parse_filter("nope = 1", &cols).is_none());
        assert!(parse_filter("status >", &cols).is_none());
        assert!(parse_filter("", &cols).is_none());
    }

    #[test]
    fn test_record_matches_filter() {
        use crate::driver::Value;
        let cols = vec!["status".to_string(), "amount".to_string(), "name".to_string()];
        let rec = record(vec![
            Value::String("paid".to_string()),
            Value::Decimal("150.50".to_string()),
            Value::String("ada lovelace".to_string()),
        ]);

        let eq = parse_filter("status = paid", &cols).unwrap();
        assert!(record_matches_filter(&rec, &eq));
        let ne = parse_filter("status != pending", &cols).unwrap();
        assert!(record_matches_filter(&rec, &ne));

        // Numeric comparison (not lexicographic).
        let gt = parse_filter("amount > 100", &cols).unwrap();
        assert!(record_matches_filter(&rec, &gt));
        let lt = parse_filter("amount < 100", &cols).unwrap();
        assert!(!record_matches_filter(&rec, &lt));

        // Contains (case-sensitive, like a LIKE without wildcards).
        let cont = parse_filter("name ~ lovelace", &cols).unwrap();
        assert!(record_matches_filter(&rec, &cont));
    }
}
