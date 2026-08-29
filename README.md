# dbx

Terminal database explorer — DataGrip-like UX, opencode-like UI.

> **Status: early development.** See roadmap below.

## What is this?

`dbx` is a TUI (terminal UI) database client. Browse schemas, run queries in
multi-tab consoles with contextual autocomplete, inspect DDL, and visualize
ERD — all without leaving your terminal. Works great over SSH.

## Features (planned)

- 🌳 Database explorer tree (schema → table → columns/indexes)
- 📝 Multi-tab query consoles with persistent scratch files
- 💡 Contextual autocomplete (keywords, tables, columns — alias-aware)
- 📊 Result grid with paging, client-side sort/filter, CSV export
- 📄 DDL / quick-documentation popup
- 🕸️ ERD view rendered in-terminal (powered by [flowmaid](https://github.com/go-routine-id/flowmaid))
- 🔌 Saved connections, MySQL first — architecture ready for multi-database

## Tech

Built in Rust with [ratatui](https://github.com/ratatui-org/ratatui),
[sqlx](https://github.com/launchbadge/sqlx), and
[flowmaid](https://github.com/go-routine-id/flowmaid).

## License

GPL-3.0-or-later — see [LICENSE](LICENSE).
