# dbx Roadmap

Design north star: **DataGrip UX** (keyboard-first interactions) with
**opencode UI** (muted palette, single accent color, rounded borders, animated
loading states, toasts).

Architecture north star: everything goes through a `Driver` async trait —
MySQL first, multi-database ready by design (SQL and NoSQL).
See [docs/architecture.md](docs/architecture.md).

Screen references (S1, S2a, …) point to [docs/screens.md](docs/screens.md).

---

## M0 — Foundation

**Goal:** runnable shell with the opencode look, no DB features yet.

**Deliverable:** `cargo run` opens a TUI with the real theme, demo screens,
working animations — everything mocked, no database.

| # | Task | Output |
|---|------|--------|
| 0.1 | `cargo init`, dependency setup | `Cargo.toml`: ratatui, crossterm, tokio, clap, serde, toml, anyhow, dirs (sqlx added in M1) |
| 0.2 | Theme system | `theme.rs`: 9-token palette, truecolor + 256 fallback, `Theme` struct global |
| 0.3 | App runtime | `app.rs`: state machine, event loop (~60ms tick), crossterm raw mode + alt screen, panic hook restoring terminal |
| 0.4 | Layout manager | Base layout: header / body / status bar; resize handling; min-size notice (S5) |
| 0.5 | Visual primitives | `widgets/`: spinner (animated), toast (auto-dismiss 3s), empty state, popup container (dim background, rounded border) |
| 0.6 | Status bar + help popup (P2) | Context-aware keybinding hints per screen |
| 0.7 | CLI entry | `clap`: `dbx` (open picker), `--config <path>`, `--version` |
| 0.8 | **flowmaid spike** | Hardcoded `erDiagram` → `scene()` → render to a ratatui canvas. Pure de-risk for M3: if the scene API doesn't fit a terminal painter, we learn it here — not in M3 |

**Done when:** app runs, all primitives render in a demo screen, spinner/toast
animate, `?` opens help, `q` quits cleanly with terminal restored — and the
flowmaid spike renders a diagram.

---

## M1 — Read-only core

**Goal:** connect to real MySQL and browse it.

**Deliverable:** `dbx` → pick saved connection → explorer tree of schemas →
open table data (paged) → `F1` shows DDL.

| # | Task | Output |
|---|------|--------|
| 1.1 | `Driver` trait + capability model | Per docs/architecture.md: `info`, `capabilities`, `ping`, `namespaces`, `collections`, `collection_meta`, `records`, `execute`, `definition`; `Record`/`Page`/`CollectionMeta` types |
| 1.2 | `MySqlDriver` | sqlx MySQL impl: metadata from `information_schema`, paged `SELECT` (`LIMIT/OFFSET` — see backlog for keyset pagination), type → `Record` conversion incl. exotic types (`DECIMAL`, unsigned `BIGINT`, `JSON`, `GEOMETRY`) |
| 1.3 | Config: saved connections | `~/.config/dbx/config.toml` load/save; secrets via `$ENV:` reference or prompt; parse-error screen (S5) |
| 1.4 | Connection picker (S1) + form (P5) | List, test connection (spinner per row), add/edit/delete |
| 1.5 | Explorer tree (S2a) | schema → table → columns/indexes; collapse, `/` filter, `r` refresh |
| 1.6 | Metadata cache | In-memory cache per connection feeding tree + (later) autocomplete |
| 1.7 | Data tab (S3) | Paged read-only grid, `←→` page nav, column meta header |
| 1.8 | DDL popup (P1) | `SHOW CREATE TABLE`, `y` copy to clipboard (adds `arboard` dep) |
| 1.9 | Error & disconnect states (S5) | Red banner + `r` reconnect; driver errors as error panel/toast |
| 1.10 | **Integration tests** | MySQL in Docker (testcontainers or compose): metadata queries, paged fetch, exotic type round-trip through `Record` |

**Done when:** against a real MySQL: connect, browse 2+ schemas, open a
100k-row table with smooth paging, view DDL, survive a killed connection
(reconnect flow works).

---

## M2 — Query experience

**Goal:** the DataGrip feel.

**Deliverable:** multi-tab consoles with autocomplete, async execution,
history, rich result grid.

| # | Task | Output |
|---|------|--------|
| 2.1 | Console tabs (S2b) | Multi-tab, `ctrl+n`/`ctrl+w`, persistent scratch files (`~/.config/dbx/consoles/`), dirty marker |
| 2.2 | SQL editor | Multi-line editing, syntax highlighting via **tokenizer** (keyword/string/comment/number), bracket match. ⚠️ Build this **before** 2.3 — the tokenizer is reused for statement splitting |
| 2.3 | Async execution | `ctrl+enter` run statement at cursor — splitting uses the 2.2 tokenizer (never naive `;` split: strings, comments, `DELIMITER //` procedures). Spinner + elapsed; `esc` cancel: **needs a second admin connection** — grab `SELECT CONNECTION_ID()` on the main one, then `KILL QUERY <id>` from the admin conn |
| 2.4 | Result grid (S2c) | Paging, `s` column sort, `/` client-side filter, large-value popup, `ctrl+s` CSV export |
| 2.5 | Error display | MySQL error panel with message + line jump in editor |
| 2.6 | Query history (P3) | Per-connection persisted history, `ctrl+e` popup, load-to-console |
| 2.7 | Autocomplete — **scoped in two tiers** | **Tier 1 (M2):** keywords + table/column names from metadata cache, trigger after `.` only when the left side is an unambiguous table/alias name (simple heuristic). **Tier 2 (stretch/M2.5):** true alias-aware resolution via [`sqlparser-rs`](https://github.com/apache/datafusion-sqlparser-rs) on partial queries. Don't hand-roll a SQL parser |
| 2.8 | Context menu (P4) | `ctrl+o` on table: Open data / Generate SELECT / Copy name / Show DDL |
| 2.9 | **Tests** | Tokenizer edge cases (strings, comments, delimiters); integration: run/cancel query against Docker MySQL, verify `KILL QUERY` actually frees the server |

**Done when:** write `SELECT u.email FROM users u WHERE u.` and the popup
suggests columns (tier 1 heuristic); run with `ctrl+enter`, spinner runs,
result grid is filterable/sortable/exportable; query appears in `ctrl+e`;
cancel kills a sleeping query **server-side** (verified in tests, 2.9).

---

## M3 — ERD

**Goal:** in-terminal ER diagram via [flowmaid](https://github.com/go-routine-id/flowmaid).

**Deliverable:** `ctrl+g` opens a pannable/zoomable ERD of the current
schema; selecting a table opens its DDL.

| # | Task | Output |
|---|------|--------|
| 3.1 | Relation graph in driver | `relation_graph()`: FK edges from `information_schema.KEY_COLUMN_USAGE`, junction-table (M:N) detection |
| 3.2 | Mermaid generator | `RelationGraph` → `erDiagram` text (entities + relationships + labels) |
| 3.3 | ratatui painter for flowmaid `Scene` | Render nodes as boxes + edges as unicode lines/beziers onto a canvas widget (reference: flowcli; **approach already validated by the M0 spike, 0.8**) |
| 3.4 | View interactions | `hjkl`/arrows pan, `+`/`-` zoom, `/` find table, node selection via flowmaid `hit_test` |
| 3.5 | Node actions | `enter` → DDL popup (P1); edge re-route on node move via flowmaid `route()` (stretch) |
| 3.6 | Capability gating | ERD entry hidden when driver lacks `ERD` capability |

**Done when:** a schema with 20+ tables + FKs renders a legible diagram,
panning/zooming is smooth (per-cell snap acceptable), selecting a table shows
its DDL; a connection without FKs shows a friendly empty state.

---

## M4 — Polish & v0.1

**Goal:** releasable quality.

**Deliverable:** published crate + binaries, documented, visually finished.

| # | Task | Output |
|---|------|--------|
| 4.1 | Visual pass | Every screen matches docs/screens.md: empty states, toasts, transitions, status-bar hints everywhere |
| 4.2 | Light theme | Same tokens, light palette, `--theme light` or config |
| 4.3 | Config completeness | All options documented; sane defaults; `--config` override |
| 4.4 | Performance pass | No allocation hotspots in render loop; large result sets stay responsive |
| 4.5 | Docs | README with screenshots/demo GIF, install & usage, keybinding cheat sheet |
| 4.6 | Release pipeline | GitHub Actions: build matrix (macOS arm64/x64, Linux), GitHub release binaries, `cargo publish` |
| 4.7 | Versioning | Semver, `v0.1.0` tag, changelog |

**Done when:** `cargo install dbx` (or release binary) on a clean machine
gives the full M0–M3 experience; CI builds green; README demo matches reality.

---

## Post-v0.1 (backlog)

| Area | Items |
|---|---|
| Write ops | Inline cell edit, add/delete row with DataGrip-style "pending changes + submit" dialog (`EDIT_DATA` capability) |
| Query analysis | `EXPLAIN` viewer (text/XML plan), multi-statement results |
| Databases | PostgreSQL, SQLite (sqlx — trait-ready); SQL Server + Azure SQL ([tiberius](https://github.com/prisma/tiberius), TDS protocol); then NoSQL (MongoDB, Redis) via the capability model — see [docs/architecture.md](docs/architecture.md) |
| Input | Mouse support (click, scroll, drag splitter), optional vim-mode editing |
| Productivity | Saved/favorite queries, connection switcher in-app (`ctrl+d`), table data filter bar (WHERE builder), keyset (cursor) pagination for very large tables |

## Non-goals (for now)

- ERD visual editing / schema migration designer
- Git integration, diff viewer
- Plugin system / runtime-downloadable drivers (drivers are compiled in)
