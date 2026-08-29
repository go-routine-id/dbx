# dbx Roadmap

Design north star: **DataGrip UX** (keyboard-first interactions) with
**opencode UI** (muted palette, single accent color, rounded borders, animated
loading states, toasts).

Architecture north star: everything goes through a `DatabaseDriver` async
trait — MySQL first, multi-database ready by design.

## M0 — Foundation

Goal: runnable shell with the opencode look, no DB features yet.

- [ ] `cargo init`, dependency setup (ratatui, crossterm, sqlx, tokio, clap, serde/toml, anyhow, dirs)
- [ ] Theme system (centralized palette, truecolor with 256-color fallback)
- [ ] App shell: layout manager, tick loop (~60ms) for animations
- [ ] Visual primitives: spinner, toast notifications, empty states, help popup (`?`)

## M1 — Read-only core

Goal: connect to MySQL and browse it.

- [ ] `DatabaseDriver` trait: `list_databases`, `list_tables`, `describe_table`, `fetch_rows`, `execute_query`
- [ ] MySQL driver implementation (sqlx)
- [ ] Saved connections (`~/.config/dbx/config.toml`; secrets via env-var reference or prompt, never hardcoded)
- [ ] Connection picker screen
- [ ] Explorer tree: schema → table → columns/indexes, collapsible
- [ ] Data tab per table: paged grid (read-only)
- [ ] DDL / quick-documentation popup (`F1`)

## M2 — Query experience

Goal: the DataGrip feel.

- [ ] Query consoles: multi-tab, persistent scratch files
- [ ] Result grid: paging, column sort, client-side filter (`/`), CSV export
- [ ] `ctrl+enter` run statement at cursor; async execution with spinner + elapsed time; `esc` cancel (`KILL QUERY`)
- [ ] Query history per connection (`ctrl+e` popup)
- [ ] Contextual autocomplete: keywords + table/column from metadata cache, alias-aware (`u.` → `users` columns), auto-popup + `ctrl+space`
- [ ] SQL syntax highlighting in editor

## M3 — ERD

Goal: in-terminal ER diagram via [flowmaid](https://github.com/go-routine-id/flowmaid).

- [ ] Generate Mermaid `erDiagram` text from `information_schema` (FK graph, junction-table detection)
- [ ] ratatui painter for flowmaid `Scene` (unicode box/line drawing; reference: flowcli)
- [ ] Pan (`hjkl` / arrows), zoom (`+`/`-`), node select (`hit_test`) → DDL popup
- [ ] Re-route edges on node move (flowmaid `route()`)

## M4 — Polish & v0.1

- [ ] Full opencode-style pass: empty states, toasts, transitions, status-bar hints on every screen
- [ ] Light theme
- [ ] README: screenshots/demo GIF, install instructions (`cargo install`)
- [ ] crates.io publish, GitHub release binaries (macOS/Linux)

## Post-v0.1 (backlog)

| Area | Items |
|---|---|
| Write ops | Inline cell edit, add/delete row with DataGrip-style "pending changes + submit" dialog |
| Query analysis | `EXPLAIN` viewer (text), multi-statement results |
| Databases | PostgreSQL, SQLite drivers (already trait-ready) |
| Input | Mouse support (click, scroll, drag splitter), optional vim-mode editing |
| Productivity | Saved/favorite queries, connection switcher in-app (`ctrl+d`), table data filter bar (WHERE builder) |

## Non-goals (for now)

- ERD visual editing / schema migration designer
- Git integration, diff viewer
- Plugin system
