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

### One-line install (macOS & Linux)

Detects your platform, downloads the right binary, and installs it:

```bash
curl -fsSL https://raw.githubusercontent.com/go-routine-id/dbx/main/install.sh | bash
```

### Manual install

Not sure which asset to grab? Check your platform first:

```bash
uname -s -m
# Darwin arm64  → dbx-macos-arm64
# Darwin x86_64 → dbx-macos-x86_64
# Linux  x86_64 → dbx-linux-x86_64
```

### macOS

```bash
# Apple Silicon (M1/M2/M3)
curl -L -o dbx https://github.com/go-routine-id/dbx/releases/latest/download/dbx-macos-arm64
chmod +x dbx && sudo mkdir -p /usr/local/bin && sudo mv dbx /usr/local/bin/

# Intel
curl -L -o dbx https://github.com/go-routine-id/dbx/releases/latest/download/dbx-macos-x86_64
chmod +x dbx && sudo mkdir -p /usr/local/bin && sudo mv dbx /usr/local/bin/
```

Gatekeeper may block the unsigned binary — allow it with `xattr -d com.apple.quarantine /usr/local/bin/dbx`.

### Linux (x86_64)

```bash
curl -L -o dbx https://github.com/go-routine-id/dbx/releases/latest/download/dbx-linux-x86_64
chmod +x dbx && sudo mkdir -p /usr/local/bin && sudo mv dbx /usr/local/bin/
```

### Windows (x64)

Download `dbx-windows-x86_64.exe` from the [latest release](https://github.com/go-routine-id/dbx/releases/latest), rename it to `dbx.exe`, and drop it into a folder on your `PATH`.

### Run

```bash
dbx
```

Press `a` to add a connection, `t` to test-ping it, then `Enter` to connect.

### Upgrade

```bash
dbx --self-update
```

Downloads the latest release for your platform and swaps it in place. If the
binary lives in a root-owned directory, re-run it with `sudo`. Your config
(`~/.config/dbx/config.toml`) is untouched.

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
| ✅ | Startup update check + in-place `dbx --self-update` |
| ✅ | Row detail view (`v`) + free-text cell search (`Ctrl+F` / `Ctrl+G`) |
| ✅ | **EXPLAIN plan tree** with the costliest node highlighted (`Ctrl+P`) |
| ✅ | Foreign-key navigation both ways — follow a key (`f`) or find what references a row (`F`) |
| ✅ | Schema diff against another saved connection, with migration SQL (`Alt+D`) |
| ✅ | Running-query monitor with cancel (`Ctrl+K`) |
| ✅ | Console watch mode — auto re-run every 1/5/15/60s (`Ctrl+W`) |
| ✅ | ERD export to SVG + Mermaid (`E`) |
| ✅ | SQL Server support (tiberius / TDS) |
| ✅ | SSH tunnel per connection (`[connections.ssh]`) |
| ✅ | Light theme (`theme = "light"`) |
| ✅ | Excel (xlsx) + SQL dump export formats |

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
| SQL Server | ✅ implemented (tiberius; 2012+ for OFFSET/FETCH paging) |
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

Mouse capture is enabled so you can click an ERD node to open its context
menu, click a workspace tab to switch to it, and click outside a popup to
dismiss it. Two-finger scrolling works in both axes: vertically it walks rows,
horizontally it walks columns (and pans the ERD). Consequence: **selecting/copying text in
the terminal requires holding `Shift`** (standard for TUI apps with mouse
support). Mouse capture is released automatically when the app exits.

## Configuration

Config lives in `config.toml` (`~/.config/dbx/config.toml`, or `$DBX_CONFIG` /
`--config`). Keys:

| Key | Default | Description |
|---|---|---|
| `connections` | `[]` | Saved connection entries |
| `page_size` | `50` | Rows fetched per page in the data grid |
| `theme` | `"dark"` | Colour palette: `dark` or `light` (for bright terminals) |

A connection can reach its database through an SSH bastion — add an
`[connections.ssh]` table to the entry (config file only; the add/edit form
preserves it). dbx spawns the system `ssh` with `-N -L` and points the driver
at the forwarded loopback port; authentication comes from your agent /
`~/.ssh/config` (BatchMode is on, so interactive prompts are never expected):

```toml
[[connections]]
name = "prod-pg"
driver = "postgres"
host = "db.internal"      # resolved on the bastion side
port = 5432
user = "app"
password = "$ENV:PROD_PG_PASS"
database = "appdb"

[connections.ssh]
host = "bastion.example.com"
# port = 22                     # optional, this is the default
# user = "deploy"               # optional, falls back to ssh's own default
# identity_file = "~/.ssh/id_ed25519"  # optional, ~ is expanded
# local_port = 15432            # optional, a free port is picked when 0/missing
```

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
