# dbx — Screen Inventory

All planned screens, their states, and how they connect. Visual style follows
[ui-ux.md](ui-ux.md): rounded thin borders, one accent color, quiet palette.

## Navigation map

```
                        +----------------------+
                        |  S1 Connection       |
                        |  picker              |
                        +----------+-----------+
                                   | enter
                                   v
+-------------------------------------------------------------+
|                     S2 Main workspace                        |
|                                                              |
|   S2a Explorer   S2b Console tabs   S2c Result grid          |
|        |                                  ^                  |
|        | enter on table                   | ctrl+enter       |
|        v                                  |                  |
|   S3 Data tab (per table) --------+-------+                  |
+--------|--------------------------|--------------------------+
         | F1                       | ctrl+e       | ctrl+o (explorer)
         v                          v              v
   P1 DDL popup            P3 History popup  P4 Context menu
                                   ^
                                   | ?           / (grid)
                             P2 Help popup   F1 Filter bar

  S4 ERD view  <- ctrl+g from workspace
  P5 New connection form  <- n from S1
  P6 Toast (overlay, any screen)
  S5 Error / disconnect states
```

---

## S1 — Connection picker

Entry point. Lists saved connections from `~/.config/dbx/config.toml`.

```
╭──────────────────────────────────────────────────────────────╮
│  ◆ dbx                                            v0.1.0     │
│                                                              │
│   Select connection                                          │
│                                                              │
│   ▸ local-mysql      mysql://root@127.0.0.1:3306      ● ok   │
│     dev-server       mysql://dev@10.0.1.5:3306               │
│     staging          mysql://app@staging.internal:3306       │
│     + New connection                                         │
│                                                              │
╰──────────────────────────────────────────────────────────────╯
  ↑↓ navigate   enter connect   t test   n new   e edit   del delete   q quit
```

States:

| State | Tampilan |
|---|---|
| Empty (no saved connections) | Placeholder dim: `no connections yet — press n to add one` |
| Testing connection | Spinner per row: `⠋ testing...` |
| Test failed | Row marker `✗` merah + toast error |
| Connecting | Full-row spinner, lalu masuk S2 |

---

## P5 — New/edit connection form (popup dari S1)

```
╭────────────── New connection ──────────────╮
│                                            │
│  Name      ▸ local-mysql▌                  │
│  Host        127.0.0.1                     │
│  Port        3306                          │
│  User        root                          │
│  Password    ••••••   (env: DBX_PASS_LOCAL)│
│  Database    (optional)                    │
│  SSL         [ ] require                   │
│                                            │
│         [ Test ]   [ Save ]   esc          │
╰────────────────────────────────────────────╯
```

- `tab` pindah field, password bisa refer env var (`$ENV:VAR_NAME`) — tidak
  pernah disimpan plaintext kalau user memilih env reference.
- `Test` menjalankan koneksi dengan spinner inline.

---

## S2 — Main workspace

Layout tiga area: explorer (kiri, collapsible), console tabs (atas), result
grid (bawah). Header berisi koneksi aktif + console tabs.

### S2a — Explorer

```
┌─────────────────┐
│ 🔎 filter...    │  <- muncul saat /
│ ▾ wacca_db      │
│   ▾ users       │
│     ▸ columns   │
│     ▸ indexes   │
│   ▸ orders      │
│   ▸ payments    │
│ ▸ analytics_db  │
└─────────────────┘
```

- `enter` pada tabel → buka S3 data tab
- `enter` pada tabel saat editor fokus (`ctrl+o` → Insert name) → sisipkan nama di cursor
- `/` filter tabel real-time
- `r` refresh metadata (re-cache untuk autocomplete)

### S2b — Console (editor)

```
│ console-1                                          ctrl+enter run │
│ ──────────────────────────────────────────────────────────────────│
│ 1  SELECT u.name, COUNT(o.id) as total                            │
│ 2  FROM users u                                                   │
│ 3  WHERE u.created_at > '2026-01-01'▌                             │
│ 4                                                                 │
```

States:

| State | Tampilan |
|---|---|
| Empty | Placeholder dim `-- write a query, ctrl+enter to run` |
| Dirty (unsaved) | Dot `●` di tab name — autosave ke scratch file |
| Running | Spinner + elapsed di divider, `esc` cancel |
| Multi-statement | Run statement di cursor (parse `;`), `ctrl+shift+enter` run all |

### S2c — Result grid

```
│ Result · 10 rows in 0.023s                    / filter   ⬇ csv      │
│ name                    │ total                                     │
│ ────────────────────────┼──────                                     │
│ ▸ ryan                  │ 42                                        │
│   budi                  │ 17                                        │
│ page 1/1                                             s sort col     │
```

States:

| State | Tampilan |
|---|---|
| Loading | Skeleton rows + spinner |
| Empty result | `0 rows — query ran in 0.012s` |
| Error | Panel merah: pesan error MySQL + line pointer kalau ada |
| Large value | Cell terpotong `…`, `enter` buka popup full value |

---

## S3 — Data tab (per tabel, read-only)

```
│ wacca_db › users · rows 1-50 of 12,140                [Data] [DDL]  │
│ filter: name LIKE 'ry%'▌                              ← → page      │
│ id   │ name   │ email            │ created_at                       │
│ ─────┼────────┼──────────────────┼──────────────                    │
│ 1    │ ryan   │ ryan@x.com       │ 2026-01-02 10:11:12              │
```

- `[DDL]` toggle ke definisi kolom/index/FK inline
- Filter bar = WHERE builder sederhana (text-based MVP)

---

## S4 — ERD view

```
╭────────────────── ERD: wacca_db ─────────────────────────────╮
│  ┌──────────────┐                   ┌───────────────┐        │
│  │ users        │ 1                 │ orders        │        │
│  │ id    bigint │──────o────────────│ user_id bigint│        │
│  └──────────────┘                   └───────┬───────┘        │
│                                             │ n              │
│                                      ┌──────┴────────┐       │
│                                      │ order_items   │       │
│                                      └───────────────┘       │
╰──────────────────────────────────────────────────────────────╯
 hjkl pan   +/- zoom   / find table   enter DDL   esc back
```

- Dibuka dengan `ctrl+g` dari workspace
- Node ter-select → `enter` buka P1 DDL popup tabel itu

---

## Popups & overlays

### P1 — DDL popup (`F1`, dari mana saja)

```
╭────────────── users · DDL ───────────────────────────────────╮
│ CREATE TABLE `users` (                                       │
│   `id` bigint NOT NULL AUTO_INCREMENT,                       │
│   `email` varchar(255) NOT NULL,                             │
│   PRIMARY KEY (`id`),                                        │
│   UNIQUE KEY `email` (`email`)                               │
│ ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;                     │
│                                                y copy  esc   │
╰──────────────────────────────────────────────────────────────╯
```

### P2 — Help popup (`?`, kontekstual per screen)

```
╭────────────── Help · console ──────────────╮
│ ctrl+enter   run statement at cursor       │
│ ctrl+space   autocomplete                  │
│ ctrl+e       history                       │
│ ...                                        │
╰────────────────────────────────────────────╯
```

### P3 — History popup (`ctrl+e`)

```
╭────────────── History · local-mysql ───────╮
│ ▸ SELECT u.name, COUNT(o.id) ...  2m ago   │
│   SHOW TABLES                     1h ago   │
│   ...                                      │
╰────────────────────────────────────────────╯
 enter: load ke console   ctrl+y: copy
```

### P4 — Context menu explorer (`ctrl+o` pada tabel)

```
╭────────────────────╮
│ ▸ Open data        │
│   Generate SELECT  │
│   Copy name        │
│   Show DDL         │
╰────────────────────╯
```

### P6 — Toast (overlay, pojok kanan bawah, auto-dismiss 3s)

```
              ╭───────────────────────────╮
              │ ✓ Connected to local-mysql│
              ╰───────────────────────────╯
```

Varian: `✓` success hijau, `⚠` warning kuning, `✗` error merah.

---

## S5 — Error & edge states

| Kondisi | Screen behavior |
|---|---|
| Koneksi gagal saat start | Toast error di S1 + row ditandai `✗`, tetap bisa pilih koneksi lain |
| Koneksi putus saat bekerja | Banner merah di header `● disconnected — r reconnect`, query diblok |
| Query error | Result panel jadi error panel merah, cursor di editor melompat ke line error (kalau MySQL kasih posisi) |
| MySQL versi tidak kompatibel | Toast warning, fitur metadata tertentu dimatikan graceful |
| Config file rusak | S1 tampilkan error panel dengan path file + line error TOML |
| Terminal terlalu kecil | Full-screen notice: `terminal too small (min 80x24), resize to continue` |

---

## Screen → milestone mapping

| Screen | Milestone |
|---|---|
| S1, P5 | M1 |
| S2a explorer | M1 |
| S3 data tab, P1 DDL | M1 |
| S2b console, S2c result, P3 history | M2 |
| P2 help, P6 toast, spinner states | M0 |
| Autocomplete popup (S2b) | M2 |
| P4 context menu | M2 |
| S4 ERD | M3 |
| S5 states | bertahap M0–M2 |
