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
  everything else stays gray. *Exception:* a pane that loses focus keeps its
  selection in the accent colour, dropping only the bold weight — recolouring
  it grey read as a different kind of selection and made you lose your place
  when opening a table moved focus to the workspace.
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

Objects under a schema are grouped by kind with a labelled rule
(`Tables (12) ─────`) rather than another level of expanding, so everything
stays one keypress away while remaining scannable. The dividers are not
selectable — navigation steps over them.

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

> The bindings below are the ones actually shipped. `?` in the app is the
> authoritative list; this table is kept in step with it.

Global:

| Key                            | Action                                   |
|--------------------------------|------------------------------------------|
| `tab`                          | Toggle focus explorer ↔ main panel       |
| `ctrl+b`                       | Collapse/restore explorer                |
| `ctrl+enter` / `alt+enter` / `F5` | Run the console's SQL                 |
| `esc`                          | Cancel running query / close popup / back |
| `F1`                           | DDL popup for the selected object        |
| `c`                            | New console tab                          |
| `w`                            | Close current tab                        |
| `[` / `]`                      | Previous / next tab (also click a tab)   |
| `alt+h`                        | Query history popup                      |
| `alt+f` / `ctrl+s`             | Saved query collections / save current   |
| `ctrl+t`                       | Search all objects                       |
| `ctrl+r`                       | Reconnect after a dropped connection     |
| `ctrl+e`                       | Export the active result                 |
| `?`                            | Help popup (scrolls; two columns if wide) |
| `q`                            | Quit / back                              |

`ctrl+n` / `ctrl+w` / `ctrl+d` from the original concept were reassigned:
`ctrl+n` sets a cell to NULL, `ctrl+w` cycles the console watch interval, and
`ctrl+d` deletes a saved query. Tab management uses the unmodified `c` / `w`
because the console editor never needs those letters.

Editor:

| Key           | Action                                   |
|---------------|------------------------------------------|
| `ctrl+space`  | Trigger autocomplete manually            |
| `↑↓` `tab`    | Navigate/accept autocomplete popup       |
| `home`/`end`, `ctrl+a`/`ctrl+e` | Start / end of line    |
| `ctrl+f`      | Pretty-print the SQL                     |
| `ctrl+p`      | EXPLAIN the query, shown as a plan tree  |
| `ctrl+w`      | Cycle auto re-run: off / 1s / 5s / 15s / 60s |

Result grid / data tab:

| Key           | Action                                   |
|---------------|------------------------------------------|
| `/`           | Client-side filter                       |
| `s` / `S`     | Add a sort column (asc/desc/off) / clear all |
| `ctrl+f` / `ctrl+g` | Search every cell / jump to next match |
| `v`           | Expand the selected row vertically       |
| `n` / `p`     | Next / previous page                     |
| `←` `→`       | Move the column selection (scroll horizontally) |
| `f` / `F`     | Follow a foreign key / find what references this row |
| `e` / `i` / `x` | Edit cell / insert row / delete row     |
| `ctrl+e`      | Export (CSV / JSON / SQL INSERT)         |

Explorer:

| Key           | Action                                   |
|---------------|------------------------------------------|
| `j/k` `↑↓`    | Navigate tree (skips section dividers)   |
| `enter` / click | Open table data tab                    |
| `a` / `e`     | Create object / edit schema              |
| `g`           | Generate the ERD                         |
| `ctrl+t`      | Search all objects (stands in for a tree filter) |
| `ctrl+k`      | List running queries (cancel with `x`)   |
| `alt+d`       | Compare this schema with another connection |

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
