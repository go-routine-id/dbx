# dbx — Requirements

Full requirement breakdown before implementation. Priorities use MoSCoW
(**M**ust / **S**hould / **C**ould / **W**on't-for-now). Milestone refs point
to [ROADMAP.md](../ROADMAP.md).

---

## 1. Functional requirements

### FR-1 Connections (M1)

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-1.1 | Save named connections in `~/.config/dbx/config.toml` | M |
| FR-1.2 | Secrets via `$ENV:VAR` reference or interactive prompt — never hardcoded | M |
| FR-1.3 | Test connection from picker (S1), spinner + ✓/✗ marker | M |
| FR-1.4 | Add/edit/delete connection via form (P5) | M |
| FR-1.5 | `--config <path>` CLI override | S |
| FR-1.6 | In-app connection switch (`ctrl+d`) | C (backlog) |
| FR-1.7 | Unix socket connections (`socket` field) — must work without host/port | M |

### FR-2 Browsing (M1)

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-2.1 | Explorer tree: namespace → collection → columns/indexes (S2a) | M |
| FR-2.2 | Real-time table filter (`/`), collapse/expand, refresh (`r`) | M |
| FR-2.3 | Data tab per table (S3): paged, read-only grid | M |
| FR-2.4 | Paging via `LIMIT/OFFSET`; smooth at 100k+ rows (page fetch < 300ms) | M |
| FR-2.5 | DDL popup `F1` (P1) + copy to clipboard | M |
| FR-2.6 | Exotic MySQL types display sanely (DECIMAL as exact string — never f64; unsigned BIGINT; JSON; GEOMETRY) | M |
| FR-2.7 | `NULL` rendered as dim `NULL`, distinct from empty string `""` | M |
| FR-2.8 | `BLOB`/binary rendered as `<blob N bytes>` + hex preview popup — never raw bytes to terminal | M |
| FR-2.9 | Row counts from `information_schema` shown as estimate (`~12k rows`) — InnoDB stats are approximate | M |
| FR-2.10 | Raw server datetime values shown as-is; no implicit timezone conversion | M |

### FR-3 Query (M2)

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-3.1 | Multi-tab consoles, persistent scratch files, dirty marker | M |
| FR-3.2 | Multi-line editor + SQL highlighting (tokenizer-based) | M |
| FR-3.3 | Run statement at cursor (`ctrl+enter`); split via tokenizer, not naive `;` | M |
| FR-3.4 | Async run: spinner + elapsed; `esc` cancels server-side via admin conn + `KILL QUERY` | M |
| FR-3.5 | Result grid (S2c): paging, column sort, client-side filter, large-value popup | M |
| FR-3.6 | CSV export (`ctrl+s`) — **streams** row batches to file, never buffers full result in RAM | M |
| FR-3.7 | Error panel: MySQL message + jump to error line when position known | M |
| FR-3.8 | Per-connection query history, `ctrl+e` popup (P3) — stores query text only, never connection strings/credentials | M |
| FR-3.9 | Autocomplete tier 1: keywords + table/column names, `.`-trigger heuristic | M |
| FR-3.10 | Autocomplete tier 2: alias-aware via `sqlparser-rs` | C (M2.5) |
| FR-3.11 | Explorer context menu (P4): open data / generate SELECT / copy name / DDL | M |
| FR-3.12 | `EXPLAIN` viewer | C (backlog) |
| FR-3.13 | **Destructive statement guard**: `DROP` / `TRUNCATE` / `DELETE`-without-`WHERE` (tokenizer-detected) → confirm dialog before execution. Console runs arbitrary SQL, so this ships in M2 — not deferred to write-ops phase | M |
| FR-3.14 | All driver-generated identifiers backtick-quoted with escaping (no raw interpolation from tree/config into SQL) | M |

### FR-4 ERD (M3)

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-4.1 | FK graph from `information_schema` → Mermaid `erDiagram` | M |
| FR-4.2 | Junction-table (M:N) detection | S |
| FR-4.3 | In-terminal render via flowmaid scene + own ratatui painter | M |
| FR-4.4 | Pan (hjkl/arrows), zoom (+/-), find table (`/`), select → DDL | M |
| FR-4.5 | Hidden (not broken) when driver lacks `ERD` capability | M |
| FR-4.6 | Edge re-route on node move (`route()`) | C |

### FR-5 Platform/UX shell (M0, M4)

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-5.1 | Centralized theme (9 tokens), truecolor + 256 fallback | M |
| FR-5.2 | Tick loop (~60ms) driving spinner/toast animations | M |
| FR-5.3 | Status bar hints per screen; help popup `?` (P2) | M |
| FR-5.4 | Toasts, empty states, error states, disconnect banner (S5) | M |
| FR-5.5 | Terminal-too-small notice (min 80×24) | M |
| FR-5.6 | Light theme | S (M4) |
| FR-5.7 | Mouse support | C (backlog) |
| FR-5.8 | TTY check & graceful non-TTY refusal (`is_terminal()`) on stdin/stdout | M |
| FR-5.9 | `TERM=dumb` detection with clear informative exit | M |
| FR-5.10 | `NO_COLOR` / `DBX_NO_COLOR` / `--no-color` support with monochrome theme fallback | S (M4) |
| FR-5.11 | Signal handling (`SIGTERM`, `SIGHUP`, `SIGINT`) restoring terminal on abnormal exit | M |
| FR-5.12 | Help examples & rich usage instructions in CLI (`after_help`) | M |
| FR-5.13 | File-only logging via `DBX_LOG=<path>` (zero terminal pollution during TUI) | M |
| FR-5.14 | Auto crash-log writing on panic (`~/.local/state/dbx/crash.log`) | M |

---

## 2. Non-functional requirements

| ID | Requirement | Target |
|----|-------------|--------|
| NFR-1 | Cold startup | < 100ms to first paint |
| NFR-2 | Idle memory | < 50MB resident |
| NFR-3 | Render loop | no per-frame allocation in hot paths; 60fps capable |
| NFR-4 | Single static binary | no runtime driver/plugin installs |
| NFR-5 | Works over SSH | no X11/clipboard dependency for core flows (clipboard degrades gracefully) |
| NFR-6 | Terminal support | iTerm2, kitty, WezTerm, Alacritty, tmux; truecolor with 256 fallback |
| NFR-7 | Platforms (v0.1) | macOS arm64/x64, Linux x64 |
| NFR-8 | MySQL compat | 5.7+ and 8.x (`information_schema` differences handled) |
| NFR-9 | Crash safety | panic hook restores terminal; never corrupt config/scratch files (atomic write: tmp + rename) |
| NFR-10 | License | GPL-3.0-or-later (flowmaid dependency) |
| NFR-11 | Credential hygiene | logs/toasts never contain passwords or full connection strings (host only); config file mode `0600` |
| NFR-12 | Transaction hygiene | all queries autocommit; never hold a transaction open across UI frames (InnoDB metadata locks block DDL server-wide) |
| NFR-13 | Connection lifecycle | one pool per connection + one admin conn (for `KILL QUERY`); stale connections detected via `ping`, auto-reconnect; pools closed on tab close |
| NFR-14 | Exit code contract | `0` = normal exit, `1` = runtime/DB error, `2` = CLI/config usage error |
| NFR-15 | XDG path precedence | `--config <path>` > `$DBX_CONFIG` > `$XDG_CONFIG_HOME/dbx/` > `~/.config/dbx/` |

---

## 3. Crate dependencies

Versions verified against crates.io on 2026-08-30.

| Crate | Version | Purpose | Milestone |
|-------|---------|---------|-----------|
| `ratatui` | 0.30.2 | TUI framework | M0 |
| `crossterm` | 0.29.0 | Terminal backend (events, raw mode) | M0 |
| `tokio` | 1.53 | Async runtime (`rt-multi-thread`, `macros`) | M0 |
| `clap` | 4.6 | CLI args (derive) | M0 |
| `anyhow` | 1.0 | Error handling | M0 |
| `serde` | 1.0 | (de)serialization | M0 |
| `toml` | 1.1 | Config file | M1 |
| `dirs` | 6.0 | Config dir lookup | M1 |
| `sqlx` | 0.9 | MySQL wire protocol (`runtime-tokio`, `mysql`) | M1 |
| `async-trait` | 0.1 | `Driver` trait | M1 |
| `serde_json` | 1.0 | Dynamic `Record` values | M1 |
| `unicode-width` | 0.2 | Correct cell widths (CJK/emoji) | M1 |
| `arboard` | 3.6 | Clipboard (degrade gracefully headless) | M1 |
| `sqlparser` | 0.62 | Autocomplete tier 2 | M2.5 (optional) |
| `flowmaid` | 0.25 | ER layout engine — `er::scene()` yields geometry for a terminal painter; `render()` emits **SVG** (not used) | M0 spike / M3 |
| `testcontainers` | 0.28 | MySQL integration tests | M1 (dev-dep) |

Rules: no new crate without a line in this table; prefer std/existing deps;
every crate must justify its compile-time cost.

---

## 4. Dev & test environment

| Need | Detail |
|---|---|
| Toolchain | stable Rust (rustup), edition 2024 |
| Test MySQL | Docker (testcontainers) or local compose; versions 5.7 & 8.x matrix for metadata tests |
| CI | GitHub Actions (M4): fmt, clippy (`-D warnings`), tests incl. integration, build matrix |
| Quality gates | `cargo fmt --check`, `cargo clippy`, unit + integration tests green before push |

---

## 5. Explicitly out of scope

- Runtime-downloadable drivers / plugin system
- ERD visual editing, schema designer/migrations
- Git integration
- Windows support (v0.1; revisit after)
- Multi-cursor editing, hover tooltips (terminal limitation)
