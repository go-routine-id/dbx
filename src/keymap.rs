//! Keybinding tables: the status-bar hints and the `?` help overlay.
//!
//! Kept apart from `app.rs` so adding a binding is a one-file edit, and so the
//! list stays readable as a list rather than buried in the event loop.

pub const PICKER_HINTS: [(&str, &str); 7] = [
    ("Enter", "connect"),
    ("a", "add"),
    ("e", "edit"),
    ("d", "delete"),
    ("t", "test"),
    ("q", "quit"),
    ("?", "help"),
];

/// Which surface the status-bar hints describe. Computed at render time in
/// app.rs from the focused pane and the active workspace tab, so the hint
/// line always matches the keys that are actually live there.
pub enum HintContext {
    Tree,
    Table,
    Console,
    Erd,
}

/// Contextual status-bar hints for the explorer screen. The old static list
/// advertised console-only bindings on a table tab (and `c` as "new console"
/// in grids where it copies a cell); each context now lists only keys that
/// work there, with a universal tail. `g erd` appears only when the driver
/// can lay out an ERD — otherwise the key would just toast a warning.
pub fn explorer_status_hints(
    ctx: HintContext,
    erd_capable: bool,
) -> Vec<(&'static str, &'static str)> {
    let mut hints: Vec<(&'static str, &'static str)> = Vec::new();
    match ctx {
        HintContext::Tree => {
            hints.push(("Enter/Space", "open"));
            hints.push(("c", "console"));
            if erd_capable {
                hints.push(("g", "erd"));
            }
            hints.push(("Tab", "pane"));
        }
        HintContext::Table => {
            hints.push(("e", "edit cell"));
            hints.push(("x", "delete"));
            hints.push(("i", "insert"));
            hints.push(("Ctrl+E", "export"));
            if erd_capable {
                hints.push(("g", "erd"));
            }
            hints.push(("Tab", "pane"));
        }
        HintContext::Console => {
            hints.push(("Ctrl+Enter", "run"));
            hints.push(("F5", "run all"));
            hints.push(("Tab", "subpane"));
        }
        HintContext::Erd => {
            hints.push(("E", "export svg"));
            hints.push(("+/-", "zoom"));
            hints.push(("hjkl", "pan"));
            hints.push(("0", "reset view"));
            hints.push(("Tab", "pane"));
        }
    }
    // Universal tail — true in every context.
    hints.push(("w", "close tab"));
    hints.push(("Esc", "back"));
    hints.push(("?", "help"));
    hints
}

pub const PICKER_HELP_BINDINGS: [(&str, &str); 7] = [
    ("Enter", "connect to selected database"),
    ("a", "add new connection"),
    ("e", "edit selected connection"),
    ("d", "delete selected connection"),
    ("t", "test connection ping"),
    ("q", "quit"),
    ("Esc", "close popup / back"),
];

pub const EXPLORER_HELP_BINDINGS: [(&str, &str); 62] = [
    ("Tab", "toggle focus between Explorer tree & Workspace / subpane"),
    ("c", "open new SQL Query Console tab"),
    ("g", "open In-Terminal ERD diagram for selected database"),
    ("Ctrl+T", "search all objects / jump to a table"),
    ("Ctrl+Enter / Alt+Enter", "run the statement under the cursor"),
    ("F5", "run every statement in the console"),
    ("Home / End (in editor)", "jump to start / end of line (also Ctrl+A / Ctrl+E)"),
    ("s / S (in table tab)", "add a sort column (asc/desc/off) / clear all sorting"),
    ("Ctrl+B", "collapse / restore the explorer tree"),
    ("Ctrl+Space (in editor)", "ask for autocomplete suggestions"),
    ("Enter / Tab / Esc (suggestions open)", "accept the highlighted suggestion / dismiss the list"),
    ("Esc (while running)", "cancel the query in flight"),
    ("Ctrl+R", "reconnect after a dropped connection"),
    ("Ctrl+Shift+I", "import rows from a CSV file into the active table"),
    ("Alt+H", "open query history for this connection"),
    ("Alt+F", "open saved query collections"),
    ("Ctrl+S", "save current query to a collection"),
    ("Ctrl+F", "pretty-print SQL in the editor"),
    ("Ctrl+Z / Ctrl+Y (in editor)", "undo / redo the last edit"),
    ("Ctrl+Shift+C (in editor)", "copy the whole query buffer to the clipboard"),
    ("Ctrl+← / Ctrl+→ (in editor)", "move one word left / right"),
    ("Alt+Backspace (in editor)", "delete the word before the cursor"),
    ("PageUp / PageDown, Ctrl+Home / Ctrl+End (in editor)", "page, or jump to buffer start / end"),
    ("Ctrl+/ (or Ctrl+7) · Ctrl+D · Ctrl+Shift+K (in editor)", "toggle comment · duplicate line · delete line"),
    ("v · Ctrl+F / Ctrl+G (in console result)", "expand row · search cells / next match"),
    ("[ / ]", "switch workspace tab (or result set in console)"),
    ("j / Down", "move cursor / selection down"),
    ("k / Up", "move cursor / selection up"),
    ("h / Left", "move cursor / column selection left"),
    ("l / Right", "move cursor / column selection right"),
    ("Space", "expand / collapse database node in tree"),
    ("Enter", "open table in workspace grid"),
    ("s", "sort data grid by active column (asc → desc → off)"),
    ("< / > (in table tab)", "shrink / grow the focused column's width (or Alt+drag a header separator)"),
    ("/", "filter data grid rows (col op value, e.g. status = paid)"),
    ("y / c", "copy active cell value to system clipboard"),
    ("Y / Ctrl+Y", "copy active row (Y: JSON · Ctrl+Y: TSV)"),
    ("y / c (in row detail)", "copy the highlighted column's value"),
    ("Y / Ctrl+Y (in row detail)", "copy the expanded row (JSON / TSV)"),
    ("Ctrl+E", "open export dialog (CSV, JSON, SQL INSERT, SQL dump, xlsx)"),
    ("e / Enter", "edit active cell value (shows safe SQL confirmation)"),
    ("e (on tree table)", "edit table schema (ALTER: drop/add column, rename)"),
    ("a (in tree)", "create schema / view / type / function — table opens a column form"),
    ("N (on a database node)", "create a new database (with SQL preview)"),
    ("Ctrl+O (on tree table)", "context menu: view DDL / open rows / edit schema / delete table"),
    ("x", "delete selected row (shows safe SQL confirmation)"),
    ("v (in table tab)", "expand the selected row vertically (wide tables)"),
    ("Ctrl+F / Ctrl+G", "search all cells / jump to the next match"),
    ("E (in ERD tab)", "export the diagram as ~/dbx_erd_<schema>.svg + .mmd"),
    ("Ctrl+W (in console)", "cycle auto re-run: off / 1s / 5s / 15s / 60s"),
    ("Ctrl+T (in console)", "toggle autocommit — off = each run opens a transaction"),
    ("F6 / F7 (in console)", "commit / roll back the open transaction"),
    ("Ctrl+P (in console)", "EXPLAIN the query and show the plan tree"),
    ("f (on an FK cell)", "open the row this foreign key references"),
    ("F (on any cell)", "find every row in the schema that references it"),
    ("Ctrl+K", "list running queries (x cancels, r refreshes)"),
    ("Alt+D", "compare this schema with another saved connection"),
    ("i", "open INSERT-row modal — fill fields, server applies DEFAULT for skipped"),
    ("F1", "view table DDL schema popup"),
    ("y (in DDL popup)", "copy the shown DDL to the clipboard"),
    ("n / p", "next / previous page in data grid"),
    ("w", "close active workspace tab"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_hints_follow_context() {
        let tree = explorer_status_hints(HintContext::Tree, true);
        assert!(tree.contains(&("c", "console")));
        assert!(!tree.contains(&("Ctrl+Enter", "run")));

        let table = explorer_status_hints(HintContext::Table, true);
        assert!(table.contains(&("Ctrl+E", "export")));
        assert!(!table.contains(&("Ctrl+Enter", "run")));

        let console = explorer_status_hints(HintContext::Console, true);
        assert!(console.contains(&("Ctrl+Enter", "run")));
        assert!(!console.contains(&("Ctrl+E", "export")));

        let erd = explorer_status_hints(HintContext::Erd, true);
        assert!(erd.contains(&("E", "export svg")));
    }

    #[test]
    fn status_hints_erd_gate() {
        let without = explorer_status_hints(HintContext::Tree, false);
        assert!(!without.iter().any(|(k, _)| *k == "g"));
        let with = explorer_status_hints(HintContext::Tree, true);
        assert!(with.contains(&("g", "erd")));
    }

    #[test]
    fn status_hints_always_end_with_universal_keys() {
        for ctx in [
            HintContext::Tree,
            HintContext::Table,
            HintContext::Console,
            HintContext::Erd,
        ] {
            let hints = explorer_status_hints(ctx, true);
            assert_eq!(hints.last(), Some(&("?", "help")));
            assert!(hints.contains(&("w", "close tab")));
            assert!(hints.contains(&("Esc", "back")));
        }
    }
}
