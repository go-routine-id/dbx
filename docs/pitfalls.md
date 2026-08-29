# dbx — Common Pitfalls & How We Avoid Them

Collected from real-world DB client tools (DataGrip, DBeaver, mycli, pgcli,
harlequin) and general DB client engineering. Each pitfall maps to a
mitigation in [requirements.md](requirements.md) / [ROADMAP.md](../ROADMAP.md).

Legend: 🔴 = would be fatal for dbx, 🟡 = quality/UX damage.

---

## 1. Memory & large result sets

| # | Pitfall | Who hit it | Our mitigation |
|---|---------|-----------|----------------|
| 1.1 🔴 | Fetching entire result sets into memory → freeze/OOM on big tables | Classic DBeaver/DataGrip complaint class | Paged fetch mandatory (FR-2.3/FR-2.4); grid renders only visible rows; `Record` streaming per page |
| 1.2 🔴 | Rendering the whole grid instead of the viewport | Most hand-rolled TUI grids | Render only visible window; NFR-3 forbids hot-path allocation |
| 1.3 🟡 | CSV export loads everything into RAM first | Many tools | Export streams row batches → file |
| 1.4 🟡 | `OFFSET` paging gets slow deep into huge tables | Known MySQL behavior | Accepted for MVP; keyset pagination in backlog |

## 2. UI responsiveness

| # | Pitfall | Mitigation |
|---|---------|-----------|
| 2.1 🔴 | Query runs on the UI thread → app freezes | All driver calls on tokio tasks (architecture doc); spinner + elapsed (FR-3.4) |
| 2.2 🔴 | No real cancellation — "cancel" only drops the client side | Admin connection + `KILL QUERY <id>` with `CONNECTION_ID()` (task 2.3) — verified by test 2.9 |
| 2.3 🟡 | Metadata queries block typing/autocomplete | Metadata cache (task 1.6); refresh in background |

## 3. SQL parsing

| # | Pitfall | Mitigation |
|---|---------|-----------|
| 3.1 🔴 | Splitting statements on naive `;` — breaks on strings, comments, `DELIMITER //` procedures | Tokenizer-based split (task 2.2 → 2.3 dependency is explicit) |
| 3.2 🟡 | Completion breaks after certain keywords / fuzzy edge cases | mycli changelog is full of these → tokenizer + tested heuristics (2.9); tier-2 uses `sqlparser-rs`, never hand-rolled parser |
| 3.3 🟡 | Copy-pasted SQL with weird terminators (`\G`, stray chars) misbehaves | Normalizer in tokenizer; `\G` explicitly unsupported in v0.1 (documented) |

## 4. Data types & encoding

| # | Pitfall | Mitigation |
|---|---------|-----------|
| 4.1 🔴 | Unsigned `BIGINT` > i64::MAX overflows / wraps | Type-safe `Record` values; integration test 1.10 covers it |
| 4.2 🔴 | `DECIMAL` converted via f64 → precision loss (money!) | Keep DECIMAL as string/decimal type, never f64 |
| 4.3 🟡 | `DATETIME`/`TIMESTAMP` timezone surprises | Display raw server values; no implicit conversion in v0.1 |
| 4.4 🟡 | `BLOB`/binary printed raw → terminal garbage | Render as `<blob N bytes>` / hex preview |
| 4.5 🟡 | `NULL` vs empty string indistinguishable | NULL shown as dim `NULL`, empty as `""` |
| 4.6 🟡 | Legacy charsets (latin1) mojibake | Respect connection charset; test matrix includes utf8mb4 |
| 4.7 🟡 | CJK/emoji break table alignment | `unicode-width` everywhere (already added from this review) |

## 5. Metadata performance

| # | Pitfall | Mitigation |
|---|---------|-----------|
| 5.1 🔴 | N+1 `information_schema` queries — one per table per column → explorer takes minutes on big schemas | Batch metadata queries; cache per connection (1.6) |
| 5.2 🟡 | Trusting `information_schema.TABLES.ROWS` as exact count | It's an **estimate** for InnoDB — label it `~12k rows` |

## 6. Connection management

| # | Pitfall | Mitigation |
|---|---------|-----------|
| 6.1 🔴 | Stale connection after laptop sleep / network drop → cryptic failure | `ping` before use, auto-reconnect, disconnect banner with `r` (S5) |
| 6.2 🔴 | Transactions left open → InnoDB metadata locks block everyone's DDL | All our queries autocommit; never hold tx open across UI frames |
| 6.3 🟡 | Connection leak per query / per tab | One pool per connection + one admin conn; closed on tab close |
| 6.4 🟡 | Socket-only MySQL (`-S`) ignored without port | mycli bug class → support `socket` field in connection config |

## 7. Security

| # | Pitfall | Mitigation |
|---|---------|-----------|
| 7.1 🔴 | Passwords in plaintext config / logged / in query history | `$ENV:` refs or prompt (FR-1.2); history stores query text only; logs redact connection strings |
| 7.2 🔴 | SQL injection via identifiers (table name from tree interpolated raw) | Identifiers always backtick-quoted with escaping |
| 7.3 🟡 | Destructive statements (DROP/TRUNCATE/DELETE without WHERE) run silently | mycli-style `destructive_warning` equivalent: confirm dialog (post-v0.1 when write ops land; read-only MVP limits exposure) |

## 8. Terminal/TUI specifics

| # | Pitfall | Mitigation |
|---|---------|-----------|
| 8.1 🔴 | Panic leaves terminal in raw mode → user's shell looks broken | Panic hook restores terminal (NFR-9) — first thing built in M0 |
| 8.2 🟡 | External pager clobbers scrollback | mycli `less -S` bug class → we never shell out to a pager; all scroll in-app |
| 8.3 🟡 | Mouse/colors glitch over SSH/tmux | Core flows keyboard-only; mouse optional (FR-5.7); 256-color fallback |
| 8.4 🟡 | Tiny terminal renders garbage | Min-size notice (FR-5.5) |

## 9. Config & files

| # | Pitfall | Mitigation |
|---|---------|-----------|
| 9.1 🟡 | Config parser quirks corrupt values (mycli: commas → list) | Strict TOML typed schema; unknown keys rejected |
| 9.2 🔴 | Crash while writing config/scratch → corrupted file | Atomic writes: tmp + rename (NFR-9) |
| 9.3 🟡 | Corrupt config bricks the whole app | Parse-error screen with file path + line (S5), app still opens picker |

## 10. Compatibility assumptions

| # | Pitfall | Mitigation |
|---|---------|-----------|
| 10.1 🟡 | Assuming MySQL 8 `information_schema` — breaks on 5.7 | Test matrix 5.7 & 8.x (1.10); version-gated queries |
| 10.2 🟡 | pgcli class: works locally, fails on remote (TLS, auth plugin) | Test against a remote-shaped MySQL (caching_sha2_password) in CI |

---

## Meta-lesson

The recurring theme across all mature tools: **the boring parts (types,
encoding, cancellation, config writes, terminal restore) are where DB clients
actually fail** — not the fancy features. M0–M2 ordering already front-loads
most of these; this doc is the checklist we revisit at each milestone's
"done when".

## Sources

- [Database development mistakes (Stack Overflow)](https://stackoverflow.com/questions/621884/database-development-mistakes-made-by-application-developers)
- [Five common database performance mistakes (Ted Spence)](https://tedspence.com/five-common-database-performance-mistakes-in-api-development-06d99c001bb2)
- [pgcli connection issue #1254](https://github.com/dbcli/pgcli/issues/1254)
- [mycli changelog](https://github.com/dbcli/mycli/blob/main/changelog.md)
- [mycli FAQ](https://www.mycli.net/faq)
