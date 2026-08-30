# dbx — Architecture

## Layers

```
+------------------------------------------------------------------+
| TUI layer (ratatui)                                              |
|   screens, popups, keybindings, theme                            |
|   → only talks to Driver, never to a concrete DB client          |
+----------------------------+-------------------------------------+
                             | trait Driver (async, dyn-safe)
+----------------------------v-------------------------------------+
| Driver layer                                                     |
|   capabilities + generic data model (see below)                  |
|                                                                  |
|   +-------------+  +-------------+  +-------------+              |
|   | MySqlDriver |  | PgDriver    |  | MongoDriver |  ... future  |
|   | (sqlx)      |  | (sqlx)      |  | (mongodb)   |              |
|   +-------------+  +-------------+  +-------------+              |
+------------------------------------------------------------------
                             |
                        wire protocol
                             v
                    MySQL / PostgreSQL / MongoDB / ...
```

Why a trait instead of "JDBC-style downloadable drivers": dbx compiles each
driver **statically into the binary**. Adding a database = implement the trait
+ rebuild. No runtime driver management for the user. (Dynamic plugins are an
explicit non-goal.)

## Generic data model — SQL and NoSQL ready

The trait never assumes "tables / rows / SQL". It uses neutral terms:

| Concept      | MySQL            | PostgreSQL       | MongoDB            | Redis           |
|--------------|------------------|------------------|--------------------|-----------------|
| `Server`     | instance         | instance         | cluster/instance   | instance        |
| `Namespace`  | database/schema  | database/schema  | database           | logical db (0..15) |
| `Collection` | table            | table            | collection         | key pattern     |
| `Record`     | row              | row              | document           | key + value     |
| `QueryLang`  | SQL              | SQL              | MQL (JSON)         | Redis commands  |

`Record` values are dynamic (`serde_json::Value`-like), so a document DB does
not need to be forced into flat rows. Relational drivers just happen to return
flat, typed records.

## Capabilities — the UI adapts to the driver

Every driver reports what it can do; screens/features activate per
capability instead of per database type:

| Capability        | Enables                          | MySQL | Mongo | Redis |
|-------------------|----------------------------------|:-----:|:-----:|:-----:|
| `BROWSE`          | explorer tree                    |  ✅   |  ✅   |  ✅   |
| `QUERY_TEXT`      | console (driver's native lang)   |  SQL  |  MQL  |  CLI  |
| `DDL`             | `F1` definition popup            |  ✅   | ~stats|  ❌   |
| `ERD`             | ERD view (needs FK graph)        |  ✅   |  ❌   |  ❌   |
| `EDIT_DATA`       | inline edit (post-v0.1)          |  ✅   |  ✅   |  ✅   |
| `EXPLAIN`         | plan viewer                      |  ✅   |  ✅   |  ❌   |

> **`EXPLAIN` status (Aug 2026)**: both MySQL and PostgreSQL drivers set this
> capability today, but there is **no plan-viewer UI yet** — `EXPLAIN`
> queries run through the console and return raw rows. The capability is
> reserved for the future "EXPLAIN viewer (text/XML plan)" feature so the UI
> can gate on it without a driver change later.

Example: opening ERD on a Redis connection → menu item is hidden, not broken.

## Trait sketch (stable contract first, methods per capability)

```rust
#[async_trait]
trait Driver: Send + Sync {
    // identity & capabilities
    fn info(&self) -> DriverInfo;              // name, version, query lang
    fn capabilities(&self) -> Capabilities;

    // lifecycle
    async fn ping(&self) -> Result<Duration>;

    // BROWSE
    async fn namespaces(&self) -> Result<Vec<Namespace>>;
    async fn collections(&self, ns: &Namespace) -> Result<Vec<Collection>>;
    async fn collection_meta(&self, c: &CollectionRef) -> Result<CollectionMeta>;
    async fn records(&self, c: &CollectionRef, page: Page)
        -> Result<RecordPage>;

    // QUERY_TEXT — string in the driver's native language
    async fn execute(&self, ns: &Namespace, query: &str)
        -> Result<QueryResult>;   // streaming variant later

    // DDL / ERD — only when capability is present
    async fn definition(&self, c: &CollectionRef) -> Result<String>;
    async fn relation_graph(&self, ns: &Namespace) -> Result<RelationGraph>;
}
```

Rules:

- **Dyn-safe & async** — the app holds `Box<dyn Driver>` picked at connect
  time.
- **Capability-gated methods** return `Err(Unsupported)` rather than panicking;
  the UI should already have hidden the entry point.
- **Cancellation** — long queries run on a tokio task with a cancellation
  token; `esc` triggers the driver's cancel (`KILL QUERY` for MySQL).
- **No UI types leak down** — drivers return plain data; widgets do rendering.
- **No DB types leak up** — the TUI never imports `sqlx::Row` etc.; the
  driver converts to `Record`.

## App runtime

```
+-----------+     key/tick events     +------------------+
| crossterm | ----------------------> | App (state)      |
| events    |                         |  screens, focus, |
+-----------+                         |  popups          |
      ^                               +--------+---------+
      | render (Frame)                         | driver calls (async)
+-----+------+                          +------v---------+
| ratatui    | <----------------------- | tokio runtime  |
| Terminal   |    draw current state    |  tasks, cancel |
+------------+                          +----------------+
```

- ~60ms tick loop drives spinner/toast animations even without input.
- Driver calls never block the render loop — results arrive via channel and
  update state.
- Screens subscribe to state; toasts/errors flow through a single event bus.

## Configuration

- `~/.config/dbx/config.toml` — saved connections. Secrets via env reference
  (`password = "$ENV:DBX_PASS_LOCAL"`) or interactive prompt; never hardcoded.
- `~/.config/dbx/consoles/` — persistent scratch files per connection.
- `~/.config/dbx/history/` — query history per connection.

## Roadmap anchors

| Milestone | Architectural work |
|---|---|
| M0 | App runtime, tick loop, theme, event bus |
| M1 | `Driver` trait + `MySqlDriver` (BROWSE, QUERY_TEXT, DDL) |
| M2 | Console/result UX on top of `execute`, metadata cache for autocomplete |
| M3 | `relation_graph` → flowmaid ERD (ERD capability) |
| Post-v0.1 | `EDIT_DATA` capability; new drivers (PostgreSQL, SQLite, MongoDB) proving the model is NoSQL-ready |
