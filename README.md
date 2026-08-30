# dbx

Terminal database explorer — DataGrip-like UX, opencode-like UI.

> **Status: early development.** See [ROADMAP.md](ROADMAP.md) and
> [docs/ui-ux.md](docs/ui-ux.md) for the design concept.

## What is this?

`dbx` is a TUI (terminal UI) database client. Browse schemas, run queries in
multi-tab consoles, inspect DDL, visualize ERD, and edit table data inline
(insert / update / delete) — all without leaving your terminal. Works great
over SSH.

## Features

| | Feature |
|---|---|
| ✅ | Database explorer tree (schema → table → columns/indexes) — MySQL & PostgreSQL |
| ✅ | Multi-tab query consoles with persistent scratch files |
| ✅ | Result grid with paging + CSV / SQL export |
| ✅ | DDL popup (table definition, indexes, foreign keys) |
| ✅ | **ERD view rendered in-terminal** — braille canvas with crow's-foot notation, pannable |
| ✅ | In-place data editing: `Enter` to edit cell, `i` to insert row, `Backspace` to delete (with SQL preview) |
| ✅ | `NULL` assignment via `Ctrl+N` on nullable columns |
| ✅ | Confirm-before-delete for saved connections |
| 🚧 | Contextual autocomplete (planned) |
| 🚧 | SQL Server / SQLite (architecture ready, drivers not implemented) |

The ERD renderer uses [flowmaid](https://github.com/go-routine-id/flowmaid)
for automatic layout and crow's-foot geometry, painted into a
`ratatui::widgets::canvas` with the `Braille` marker so curves stay smooth
inside a terminal grid.

## Driver support

| Driver | Status |
|---|---|
| MySQL | ✅ implemented |
| PostgreSQL | ✅ implemented |
| SQL Server | 🚧 architecture ready (`DriverType` enum exists, no implementation) |
| SQLite | 🚧 architecture ready (`DriverType` enum exists, no implementation) |

All SQL is built through a generic helper layer (`quote_ident`,
`single_row_suffix`, `render_*`, `build_where_for_row`, `build_insert_sql`)
keyed off `DriverInfo::name`, so the same code path produces valid SQL for
every driver. New drivers only need to implement the `Driver` async trait
and the rest of the app just works.

## Keyboard shortcuts (Explorer)

| Key | Action |
|---|---|
| `Tab` | Toggle focus between tree and workspace |
| `c` | New SQL console tab |
| `g` | **Generate ERD** for the selected database |
| `0` (in ERD tab) | Reset ERD viewport (pan + zoom) |
| `h` / `j` / `k` / `l` (in ERD tab) | Pan ERD viewport |
| `+` / `-` (in ERD tab) | Zoom in / out |
| `.` / `,` (in ERD tab) | Select next / previous node |
| `Enter` (on ERD node) | Open that table's DDL |
| `Ctrl+Enter` / `F5` | Run SQL in active console |
| `Enter` / `Space` | Open / expand tree node |
| `i` (in table tab) | Insert a new row |
| `Enter` (on cell, in table tab) | Edit cell |
| `Backspace` (on row, in table tab) | Delete row (with SQL confirm) |
| `Ctrl+N` (in cell edit) | Set cell to `NULL` (nullable columns only) |
| `y` / `Y` | Copy cell / row |
| `w` (in workspace) | Close active tab |
| `?` | Help overlay |

### Mouse

Mouse capture is enabled so you can click an ERD node to open its DDL, and
click outside a popup to dismiss it. Consequence: **selecting/copying text in
the terminal requires holding `Shift`** (standard for TUI apps with mouse
support). Mouse capture is released automatically when the app exits.

## Tech

Built in Rust with [ratatui](https://github.com/ratatui-org/ratatui),
[sqlx](https://github.com/launchbadge/sqlx), and
[flowmaid](https://github.com/go-routine-id/flowmaid).

## License

GPL-3.0-or-later — see [LICENSE](LICENSE).
