# dbx UI/UX Concept

Two pillars:

- **UX follows DataGrip** — keyboard-first interactions: explorer tree,
  multi-tab query consoles, contextual autocomplete, result grid, `F1` DDL.
- **UI follows opencode** — muted dark palette, one accent color, rounded
  thin borders, animated loading states, transient toasts, quiet empty states.

---

## Theme

Truecolor palette with 256-color fallback. Theme is centralized in one struct
— widgets never hardcode colors.

| Token        | Value     | Usage                                   |
|--------------|-----------|-----------------------------------------|
| `background` | `#0d1117` | App background (dark, not pure black)   |
| `panel`      | `#161b22` | Panels, popups (slightly lighter)       |
| `border`     | `#30363d` | Thin, quiet borders and separators      |
| `accent`     | `#7c83ff` | Focus/selection only — the one loud color |
| `text`       | `#e6edf3` | Primary text                            |
| `text_dim`   | `#7d8590` | Hints, placeholders, inactive items     |
| `success`    | `#3fb950` | Query OK, connected                     |
| `warning`    | `#d29922` | Warnings                                |
| `error`      | `#f85149` | Errors                                  |

A light theme reuses the same tokens with different values (M4).

## Visual language (opencode-style)

- **No heavy/double borders** — thin muted borders, sometimes only a separator
  line. Rounded corners (`╭─╮`).
- **Accent discipline** — only the focused element carries the accent color;
  everything else stays gray.
- **Living loading states** — animated spinner + elapsed time while a query
  runs (`⠋ Running query... 1.2s`), never a frozen UI.
- **Toasts** — transient messages bottom-right (`✓ Connected to local-mysql`),
  auto-dismiss.
- **Informative empty states** — an empty console shows a dim placeholder:
  `-- write a query, ctrl+enter to run`.
- **Status bar always teaches** — every screen shows its available actions, so
  keybindings never need memorizing.
- Animation runs on a ~60ms tick loop; overhead is negligible.

---

## Screens

### 1. Connection picker (on start)

```
╭──────────────────────────────────────────────────────────────╮
│  ◆ dbx                                            v0.1.0     │
│                                                              │
│   Select connection                                          │
│                                                              │
│   ▸ local-mysql      mysql://root@127.0.0.1:3306             │
│     dev-server       mysql://dev@10.0.1.5:3306               │
│     staging          mysql://app@staging.internal:3306       │
│     + New connection                                         │
│                                                              │
╰──────────────────────────────────────────────────────────────╯
  ↑↓ navigate   enter connect   n new   q quit
```

### 2. Main screen — query console (center of gravity)

The console is the center of the app, like DataGrip. The explorer tree can be
collapsed to give the console room.

```
╭──────────────────────────────────────────────────────────────────────╮
│ ◆ dbx   local-mysql · wacca_db        [console-1] [console-2]  +     │
├─────────────────┬────────────────────────────────────────────────────┤
│ ▾ wacca_db      │ SELECT u.name, COUNT(o.id) as total                │
│   ▸ orders      │ FROM users u                                       │
│   ▸ payments    │ WHERE u.▌      ╭───────────────╮                   │
│   ▾ users       │              │ ▸ id            │ ← autocomplete    │
│     columns     │              │   email         │   popup           │
│     indexes     │              │   name          │                   │
│                 │              ╰───────────────╯                    │
│                 ├────────────────────────────────────────────────────┤
│                 │ ⠋ Running query... 1.2s            esc cancel      │
│                 ├────────────────────────────────────────────────────┤
│                 │ Result · 10 rows in 0.023s        / filter  ⬇ csv  │
│                 │ name                    │ total                    │
│                 │ ────────────────────────┼──────                    │
│                 │ ryan                    │ 42                       │
│                 │ budi                    │ 17                       │
╰─────────────────┴────────────────────────────────────────────────────╯
 ● connected · utf8mb4        ctrl+enter run   tab explorer   ? help
```

Pressing `enter` on a table in the explorer opens its **data tab**
(read-only grid) — like double-clicking a table in DataGrip:

```
│ wacca_db › users · rows 1-50 of 12,140               [Data] [DDL]    │
│ filter: name LIKE 'ry%'▌                                             │
│ id   │ name   │ email            │ created_at                        │
│ 1    │ ryan   │ ryan@x.com       │ 2026-01-02                        │
```

### 3. ERD view

Generated from `information_schema` foreign keys, laid out by flowmaid,
painted with unicode box/line drawing.

```
╭────────────────── ERD: wacca_db ─────────────────────────────╮
│                                                              │
│  ┌──────────────┐                   ┌───────────────┐        │
│  │ users        │ 1                 │ orders        │        │
│  │──────────────│                   │───────────────│        │
│  │ id    bigint │──────o────────────│ user_id bigint│        │
│  │ email varchar│      "places"     │ total  decimal│        │
│  └──────────────┘                   └───────┬───────┘        │
│                                             │ n              │
│                                      ┌──────┴────────┐       │
│                                      │ order_items   │       │
│                                      └───────────────┘       │
╰──────────────────────────────────────────────────────────────╯
 hjkl pan   +/- zoom   enter DDL   esc back
```

---

## Keybindings

Global:

| Key           | Action                                   |
|---------------|------------------------------------------|
| `tab`         | Toggle focus explorer ↔ main panel       |
| `ctrl+b`      | Collapse/expand explorer                 |
| `ctrl+enter`  | Run statement at cursor                  |
| `esc`         | Cancel running query / close popup       |
| `F1`          | DDL / quick documentation popup          |
| `ctrl+n`      | New console tab                          |
| `ctrl+w`      | Close current tab                        |
| `ctrl+e`      | Query history popup                      |
| `ctrl+d`      | Disconnect / switch connection           |
| `?`           | Help popup                               |
| `q`           | Quit / back                              |

Editor:

| Key           | Action                                   |
|---------------|------------------------------------------|
| `ctrl+space`  | Trigger autocomplete manually            |
| `↑↓` `tab`    | Navigate/accept autocomplete popup       |

Result grid:

| Key           | Action                                   |
|---------------|------------------------------------------|
| `/`           | Client-side filter                       |
| `s`           | Sort by focused column                   |
| `←` `→`       | Previous/next page                       |
| `ctrl+s`      | Export CSV                               |

Explorer:

| Key           | Action                                   |
|---------------|------------------------------------------|
| `j/k` `↑↓`    | Navigate tree                            |
| `enter`       | Open table data tab                      |
| `/`           | Filter tables                            |
| `ctrl+o`      | Context menu (Open data / Copy name / Show DDL / Generate SELECT) |

---

## DataGrip UX mapping

Replicated fully: keyboard-only operation, `ctrl+enter` run-at-cursor,
contextual autocomplete (alias-aware), execute/console history popups,
multi-tab consoles with persistent scratch files, result grid with
paging/sort/filter, `F1` quick documentation, async notifications,
cancellable queries (`KILL QUERY`).

Adapted to terminal: context menus become keyboard popups (`ctrl+o`),
drag & drop becomes `enter`-to-insert table name, toolbar icons become
status-bar hints.

Not possible in a terminal (accepted trade-offs): multi-cursor editing,
hover tooltips, rich iconography, pixel-smooth animations (grid snaps per
cell).

Where dbx beats DataGrip: <100ms startup, tens of MB RAM, works over plain
SSH, optional vim-mode editing (backlog).

---

## Popups & overlays

- All popups are floating, dim the background, rounded thin border, `esc`
  closes.
- Autocomplete popup: appears next to cursor, no thick border, panel
  background, max ~8 visible rows.
- Toast: bottom-right, auto-dismiss ~3s, success/error colored icon.
- Help (`?`): centered cheat-sheet of the current screen's keybindings.
