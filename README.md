# dbx

Terminal database explorer — DataGrip-like UX, opencode-like UI.

> **Status: early development.** See [ROADMAP.md](ROADMAP.md) and
> [docs/ui-ux.md](docs/ui-ux.md) for the design concept.

## What is this?

`dbx` is a TUI (terminal UI) database client. Browse schemas, run queries in
multi-tab consoles, inspect DDL, visualize ERD, and edit table data inline
(insert / update / delete) — all without leaving your terminal. Works great
over SSH.

## Install

Prebuilt binaries ship on every [release](https://github.com/go-routine-id/dbx/releases/latest).

### macOS

```bash
# Apple Silicon (M1/M2/M3)
curl -L -o dbx https://github.com/go-routine-id/dbx/releases/latest/download/dbx-macos-arm64
chmod +x dbx && sudo mv dbx /usr/local/bin/

# Intel
curl -L -o dbx https://github.com/go-routine-id/dbx/releases/latest/download/dbx-macos-x86_64
chmod +x dbx && sudo mv dbx /usr/local/bin/
```

Gatekeeper may block the unsigned binary — allow it with `xattr -d com.apple.quarantine /usr/local/bin/dbx`.

### Linux (x86_64)

```bash
curl -L -o dbx https://github.com/go-routine-id/dbx/releases/latest/download/dbx-linux-x86_64
chmod +x dbx && sudo mv dbx /usr/local/bin/
```

### Windows (x64)

Download `dbx-windows-x86_64.exe` from the [latest release](https://github.com/go-routine-id/dbx/releases/latest), rename it to `dbx.exe`, and drop it into a folder on your `PATH`.

### Run

```bash
dbx
```

Press `a` to add a connection, `t` to test-ping it, then `Enter` to connect.

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
| ✅ | Contextual autocomplete (tables, columns, keywords) with `Tab` to accept |
| ✅ | Saved query collections + per-connection query history |
| ✅ | SQLite support — point a connection at a `.db` file, no server needed |
| 🚧 | SQL Server (architecture ready, driver not implemented) |

The ERD renderer uses [flowmaid](https://github.com/go-routine-id/flowmaid)
for automatic layout and crow's-foot geometry, painted into a
`ratatui::widgets::canvas` with the `Braille` marker so curves stay smooth
inside a terminal grid.

## Preview
<img width="1314" height="1044" alt="image" src="https://github.com/user-attachments/assets/a8b3a8e1-30b6-4998-aecf-864842dac0bd" />
<img width="1314" height="1044" alt="image" src="https://github.com/user-attachments/assets/c86ebb64-bd8e-43a3-a204-bd14fdeaacd5" />
<img width="1314" height="1044" alt="image" src="https://github.com/user-attachments/assets/30e780b7-43b4-42fe-b577-778f9cd4c9d3" />
<img width="1314" height="1044" alt="image" src="https://github.com/user-attachments/assets/a149d6d6-2b2d-4dff-9d6e-ff23470bd452" />




## Driver support

| Driver | Status |
|---|---|
| MySQL | ✅ implemented |
| PostgreSQL | ✅ implemented |
| SQL Server | 🚧 architecture ready (`DriverType` enum exists, no implementation) |
| SQLite | ✅ implemented (file-based — set the file path in the `database` field) |

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
| `PageUp` / `PageDown` (in ERD tab) | Scroll ERD viewport one page |
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

## Configuration

Config lives in `config.toml` (`~/.config/dbx/config.toml`, or `$DBX_CONFIG` /
`--config`). Keys:

| Key | Default | Description |
|---|---|---|
| `connections` | `[]` | Saved connection entries |
| `page_size` | `50` | Rows fetched per page in the data grid |

Environment variables:

| Env | Description |
|---|---|
| `DBX_CONFIG` | Config file path override (same as `--config`) |
| `DBX_DEFAULT_HOST` | Default host pre-filled in the "New Connection" form |
| `DBX_DEFAULT_PORT` | Default port pre-filled in the form |
| `DBX_DEFAULT_USER` | Default username pre-filled in the form |
| `DBX_DEFAULT_PASSWORD` | Default password pre-filled in the form |
| `DBX_DEFAULT_DATABASE` | Default database pre-filled in the form |

Passwords may reference environment variables directly: `password = "$ENV:MY_DB_PASS"`.

## Tech

Built in Rust with [ratatui](https://github.com/ratatui-org/ratatui),
[sqlx](https://github.com/launchbadge/sqlx), and
[flowmaid](https://github.com/go-routine-id/flowmaid).

## License

GPL-3.0-or-later — see [LICENSE](LICENSE).
