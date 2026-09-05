//! SQLite driver implementation using sqlx.
//!
//! SQLite differs from the server-based drivers in two ways the rest of the
//! app has to be shielded from:
//!
//! * There is no host/port/user/password — a connection is just a **file
//!   path**, taken from `ConnectionConfig::database` (`:memory:` works too).
//! * "Namespaces" are attached databases (`main`, `temp`, plus anything
//!   `ATTACH`ed), not schemas, so `PRAGMA database_list` stands in for
//!   `information_schema.schemata`.
//!
//! Metadata comes from `sqlite_master` plus the `table_info` /
//! `foreign_key_list` / `index_list` pragmas.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions, SqliteRow};
use sqlx::{AssertSqlSafe, Column, Row, TypeInfo, ValueRef};

use super::{
    Capabilities, Collection, CollectionMeta, CollectionRef, ColumnMeta, Driver, DriverInfo,
    ForeignKeyMeta, IndexMeta, Namespace, Page, QueryResult, Record, RecordPage, Value,
};
use crate::config::ConnectionConfig;

/// Quote an identifier the SQLite-native way (double quotes, inner quotes
/// doubled). Used for every interpolated table/schema name, including the
/// pragmas — which cannot take bind parameters.
fn escape_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Escape a string *literal* for the pragmas that take a quoted argument.
fn escape_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

pub struct SqliteDriver {
    pool: SqlitePool,
    info: DriverInfo,
    /// Dedicated connection held open while an interactive transaction is
    /// active — BEGIN/COMMIT/ROLLBACK and every statement between them must
    /// run on one connection, not whatever the pool hands out per call.
    tx_conn: tokio::sync::Mutex<Option<sqlx::pool::PoolConnection<sqlx::Sqlite>>>,
}

impl SqliteDriver {
    pub async fn connect(cfg: &ConnectionConfig) -> Result<Self> {
        // The `database` field carries the file path for this driver.
        let path = cfg
            .database
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .ok_or_else(|| {
                anyhow!("SQLite needs a database file path (set the 'database' field)")
            })?;

        // Never create a database by accident: a typo in the path should be
        // an error, not a silently-empty new file.
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(false);

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(opts)
            .await
            .with_context(|| format!("failed to open SQLite database at {path}"))?;

        let version: String = sqlx::query_scalar("SELECT sqlite_version()")
            .fetch_one(&pool)
            .await
            .unwrap_or_else(|_| "unknown".to_string());

        Ok(Self {
            pool,
            info: DriverInfo {
                name: "SQLite".to_string(),
                server_version: version,
                query_language: "SQL".to_string(),
            },
            tx_conn: tokio::sync::Mutex::new(None),
        })
    }

    /// Rows of a `PRAGMA <schema>.<pragma>(<arg>)` call.
    async fn pragma(&self, ns: &str, pragma: &str, arg: &str) -> Result<Vec<SqliteRow>> {
        let sql = format!(
            "PRAGMA {}.{}({})",
            escape_ident(ns),
            pragma,
            escape_literal(arg)
        );
        sqlx::query(AssertSqlSafe(sql.as_str()))
            .fetch_all(&self.pool)
            .await
            .with_context(|| format!("PRAGMA {pragma} failed for {arg}"))
    }

    /// Object names of one `sqlite_master` type (`table` / `view`).
    async fn objects_of_type(&self, ns: &Namespace, kind: &str) -> Result<Vec<Collection>> {
        let sql = format!(
            "SELECT name FROM {}.sqlite_master \
             WHERE type = {} AND name NOT LIKE 'sqlite_%' \
             ORDER BY name",
            escape_ident(&ns.0),
            escape_literal(kind)
        );
        let rows = sqlx::query(AssertSqlSafe(sql.as_str()))
            .fetch_all(&self.pool)
            .await
            .with_context(|| format!("failed to list {kind}s in {}", ns.0))?;

        Ok(rows
            .iter()
            .filter_map(|r| r.try_get::<String, _>("name").ok())
            .map(|name| Collection {
                name,
                // SQLite has no cheap row-count or size estimate (dbstat is
                // an optional compile-time module), and a COUNT(*) per table
                // on tree expansion would be far too slow. Report neither
                // rather than guessing.
                estimated_row_count: None,
                estimated_size_bytes: None,
            })
            .collect())
    }
}

#[async_trait]
impl Driver for SqliteDriver {
    fn info(&self) -> DriverInfo {
        self.info.clone()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::BROWSE
            | Capabilities::QUERY_TEXT
            | Capabilities::DDL
            | Capabilities::ERD
            | Capabilities::EXPLAIN
            | Capabilities::EDIT_DATA
    }

    async fn ping(&self) -> Result<Duration> {
        let start = Instant::now();
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .context("SQLite ping failed")?;
        Ok(start.elapsed())
    }

    /// Attached databases stand in for schemas. `temp` is skipped — it only
    /// ever holds this session's temporary objects.
    async fn namespaces(&self) -> Result<Vec<Namespace>> {
        let rows = sqlx::query("PRAGMA database_list")
            .fetch_all(&self.pool)
            .await
            .context("failed to list SQLite databases")?;

        let names: Vec<Namespace> = rows
            .iter()
            .filter_map(|r| r.try_get::<String, _>("name").ok())
            .filter(|n| n != "temp")
            .map(Namespace)
            .collect();

        // A pool connection always has `main`; fall back to it if the pragma
        // came back empty for any reason.
        if names.is_empty() {
            return Ok(vec![Namespace("main".to_string())]);
        }
        Ok(names)
    }

    async fn collections(&self, ns: &Namespace) -> Result<Vec<Collection>> {
        self.objects_of_type(ns, "table").await
    }

    async fn collection_meta(&self, c: &CollectionRef) -> Result<CollectionMeta> {
        let ns = &c.namespace.0;

        // --- Foreign keys: PRAGMA foreign_key_list ---
        // Columns: id, seq, table, from, to, on_update, on_delete, match.
        // `to` is NULL when the FK targets the parent's primary key.
        let fk_rows = self.pragma(ns, "foreign_key_list", &c.name).await?;
        let mut foreign_keys = Vec::new();
        for r in &fk_rows {
            let column: String = match r.try_get("from") {
                Ok(v) => v,
                Err(_) => continue,
            };
            let ref_table: String = r.try_get("table").unwrap_or_default();
            let ref_column: String = r
                .try_get::<Option<String>, _>("to")
                .ok()
                .flatten()
                .unwrap_or_else(|| "rowid".to_string());
            let id: i64 = r.try_get("id").unwrap_or_default();
            foreign_keys.push(ForeignKeyMeta {
                // SQLite doesn't name its FK constraints; synthesise a stable
                // one so the ERD and DDL views have something to show.
                name: format!("fk_{}_{}", c.name, id),
                column,
                ref_namespace: c.namespace.clone(),
                ref_table,
                ref_column,
            });
        }

        // --- Indexes: PRAGMA index_list + index_info per index ---
        let idx_rows = self.pragma(ns, "index_list", &c.name).await?;
        let mut indexes = Vec::new();
        for r in &idx_rows {
            let name: String = match r.try_get("name") {
                Ok(v) => v,
                Err(_) => continue,
            };
            let is_unique = r.try_get::<i64, _>("unique").unwrap_or(0) != 0;
            // `origin` is "pk" for the implicit primary-key index.
            let is_primary = r
                .try_get::<String, _>("origin")
                .map(|o| o == "pk")
                .unwrap_or(false);
            let cols = self
                .pragma(ns, "index_info", &name)
                .await
                .unwrap_or_default()
                .iter()
                .filter_map(|ir| ir.try_get::<Option<String>, _>("name").ok().flatten())
                .collect::<Vec<String>>();
            indexes.push(IndexMeta {
                name,
                columns: cols,
                is_unique,
                is_primary,
            });
        }

        // --- Columns: PRAGMA table_info ---
        // Columns: cid, name, type, notnull, dflt_value, pk.
        let col_rows = self.pragma(ns, "table_info", &c.name).await?;
        if col_rows.is_empty() {
            return Err(anyhow!("table '{}' not found in {}", c.name, ns));
        }
        let mut columns = Vec::new();
        for r in &col_rows {
            let name: String = match r.try_get("name") {
                Ok(v) => v,
                Err(_) => continue,
            };
            let declared: String = r.try_get("type").unwrap_or_default();
            let data_type = if declared.is_empty() {
                // A column with no declared type has BLOB affinity.
                "BLOB".to_string()
            } else {
                declared
            };
            let is_primary_key = r.try_get::<i64, _>("pk").unwrap_or(0) != 0;
            let is_nullable = r.try_get::<i64, _>("notnull").unwrap_or(0) == 0 && !is_primary_key;
            let is_unique = is_primary_key
                || indexes
                    .iter()
                    .any(|i| i.is_unique && i.columns == [name.clone()]);
            let default: Option<String> = r
                .try_get::<Option<String>, _>("dflt_value")
                .ok()
                .flatten();
            columns.push(ColumnMeta {
                name: name.clone(),
                data_type,
                is_nullable,
                is_primary_key,
                is_unique,
                is_foreign_key: foreign_keys.iter().any(|f| f.column == name),
                extra: default.map(|d| format!("DEFAULT {d}")),
            });
        }

        Ok(CollectionMeta {
            reference: c.clone(),
            columns,
            indexes,
            foreign_keys,
        })
    }

    async fn records(&self, c: &CollectionRef, page: Page) -> Result<RecordPage> {
        let ns_esc = escape_ident(&c.namespace.0);
        let name_esc = escape_ident(&c.name);

        let count_sql = format!("SELECT COUNT(*) FROM {ns_esc}.{name_esc}");
        let total_records: Option<u64> =
            sqlx::query_scalar::<_, i64>(AssertSqlSafe(count_sql.as_str()))
                .fetch_one(&self.pool)
                .await
                .ok()
                .map(|count| count.max(0) as u64);

        let query = format!(
            "SELECT * FROM {}.{} LIMIT {} OFFSET {}",
            ns_esc, name_esc, page.limit, page.offset
        );
        let rows = sqlx::query(AssertSqlSafe(query.as_str()))
            .fetch_all(&self.pool)
            .await
            .with_context(|| format!("failed to fetch records for {}", c))?;

        let columns = rows
            .first()
            .map(|r| r.columns().iter().map(|c| c.name().to_string()).collect())
            .unwrap_or_default();
        let records = rows
            .iter()
            .map(convert_sqlite_row)
            .collect::<Result<Vec<_>>>()?;

        Ok(RecordPage {
            columns,
            records,
            page: page.offset.checked_div(page.limit).unwrap_or(0),
            page_size: page.limit,
            total_records,
        })
    }

    /// SQLite has no `search_path`; the namespace is already part of every
    /// qualified name, so `ns` is unused here.
    async fn execute(&self, _ns: &Namespace, query: &str) -> Result<QueryResult> {
        // When an interactive transaction is open, run on its dedicated
        // connection; otherwise borrow a pooled connection for this call only.
        let mut tx_guard = self.tx_conn.lock().await;
        let mut pooled: sqlx::pool::PoolConnection<sqlx::Sqlite>;
        let conn: &mut sqlx::sqlite::SqliteConnection = if let Some(c) = tx_guard.as_mut() {
            &mut **c
        } else {
            pooled = self.pool.acquire().await.context("failed to acquire connection from pool")?;
            &mut *pooled
        };
        let start = Instant::now();
        let trimmed = query.trim_start();
        let starts_with = |kw: &str| super::starts_with_keyword(trimmed, kw);
        let returns_rows =
            starts_with("select") || starts_with("pragma") || starts_with("explain") || starts_with("with");

        if returns_rows {
            let rows = sqlx::query(AssertSqlSafe(query))
                .fetch_all(&mut *conn)
                .await
                .with_context(|| format!("query execution failed: {query}"))?;
            let elapsed = start.elapsed();

            let columns = rows
                .first()
                .map(|r| r.columns().iter().map(|c| c.name().to_string()).collect())
                .unwrap_or_default();
            let records = rows
                .iter()
                .map(convert_sqlite_row)
                .collect::<Result<Vec<_>>>()?;
            let count = records.len() as u64;

            Ok(QueryResult {
                columns,
                records,
                rows_affected: count,
                execution_time: elapsed,
            })
        } else {
            let res = sqlx::query(AssertSqlSafe(query))
                .execute(&mut *conn)
                .await
                .with_context(|| format!("statement execution failed: {query}"))?;
            Ok(QueryResult {
                columns: Vec::new(),
                records: Vec::new(),
                rows_affected: res.rows_affected(),
                execution_time: start.elapsed(),
            })
        }
    }

    async fn begin_tx(&self) -> Result<()> {
        let mut guard = self.tx_conn.lock().await;
        if guard.is_some() {
            anyhow::bail!("a transaction is already open");
        }
        let mut conn = self.pool.acquire().await.context("failed to acquire connection from pool")?;
        sqlx::query("BEGIN").execute(&mut *conn).await.context("failed to BEGIN")?;
        *guard = Some(conn);
        Ok(())
    }

    async fn commit_tx(&self) -> Result<()> {
        let mut conn = self.tx_conn.lock().await.take()
            .context("no transaction is open")?;
        sqlx::query("COMMIT").execute(&mut *conn).await.context("failed to COMMIT")?;
        Ok(())
    }

    async fn rollback_tx(&self) -> Result<()> {
        let mut conn = self.tx_conn.lock().await.take()
            .context("no transaction is open")?;
        sqlx::query("ROLLBACK").execute(&mut *conn).await.context("failed to ROLLBACK")?;
        Ok(())
    }

    async fn in_tx(&self) -> bool {
        self.tx_conn.lock().await.is_some()
    }

    /// SQLite stores the original `CREATE` text, so the DDL is exact rather
    /// than synthesised from metadata.
    async fn definition(&self, c: &CollectionRef) -> Result<String> {
        let sql = format!(
            "SELECT sql FROM {}.sqlite_master WHERE name = ?",
            escape_ident(&c.namespace.0)
        );
        let ddl: Option<String> = sqlx::query_scalar(AssertSqlSafe(sql.as_str()))
            .bind(&c.name)
            .fetch_optional(&self.pool)
            .await
            .with_context(|| format!("failed to fetch DDL for {}", c))?
            .flatten();

        ddl.ok_or_else(|| anyhow!("no DDL recorded for '{}'", c.name))
    }

    async fn list_views(&self, ns: &Namespace) -> Result<Vec<Collection>> {
        self.objects_of_type(ns, "view").await
    }

    // `list_routines` / `list_sequences` keep the trait defaults (empty):
    // SQLite has neither stored routines nor sequence objects.
}

/// Convert one row into model-agnostic `Value`s.
///
/// SQLite is dynamically typed — the *declared* column type is only an
/// affinity hint and a column can hold anything — so the decode is driven by
/// the value's runtime type, with the declared type used only to pick the
/// preferred interpretation (e.g. BOOLEAN over INTEGER).
fn convert_sqlite_row(row: &SqliteRow) -> Result<Record> {
    let mut values = Vec::with_capacity(row.len());

    for (i, col) in row.columns().iter().enumerate() {
        let raw = row.try_get_raw(i)?;
        if raw.is_null() {
            values.push(Value::Null);
            continue;
        }

        let declared = col.type_info().name().to_uppercase();
        let value = if declared.contains("BOOL") {
            row.try_get::<bool, _>(i)
                .map(Value::Bool)
                .or_else(|_| row.try_get::<i64, _>(i).map(|n| Value::Bool(n != 0)))
                .unwrap_or(Value::Null)
        } else if declared.contains("BLOB") {
            row.try_get::<Vec<u8>, _>(i)
                .map(Value::Bytes)
                .unwrap_or(Value::Null)
        } else if declared.contains("DATE") || declared.contains("TIME") {
            row.try_get::<String, _>(i)
                .map(Value::DateTime)
                .unwrap_or(Value::Null)
        } else {
            // Storage classes: INTEGER, REAL, TEXT, BLOB. Try each in the
            // order that preserves the most precision.
            row.try_get::<i64, _>(i)
                .map(Value::Int)
                .or_else(|_| row.try_get::<f64, _>(i).map(Value::Float))
                .or_else(|_| row.try_get::<String, _>(i).map(Value::String))
                .or_else(|_| row.try_get::<Vec<u8>, _>(i).map(Value::Bytes))
                .unwrap_or(Value::Null)
        };
        values.push(value);
    }

    Ok(Record { values })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DriverType;

    /// A throwaway on-disk database. SQLite needs no server, so the driver
    /// can be exercised end-to-end in a plain unit test.
    struct TempDb(std::path::PathBuf);

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    async fn seeded_db() -> TempDb {
        // Wall-clock nanoseconds alone can repeat for two tests starting on
        // the same tick (parallel test threads) — a per-process counter
        // guarantees a unique path regardless of clock granularity.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "dbx-sqlite-test-{}-{seq}-{:?}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let opts = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true);
        let pool = SqlitePool::connect_with(opts).await.unwrap();
        for stmt in [
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, email TEXT)",
            "CREATE UNIQUE INDEX idx_users_email ON users(email)",
            "CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER REFERENCES users(id), total REAL)",
            "CREATE VIEW active_users AS SELECT * FROM users",
            "INSERT INTO users (id, name, email) VALUES (1, 'ada', 'ada@example.com')",
            "INSERT INTO users (id, name, email) VALUES (2, 'bob', NULL)",
            "INSERT INTO orders (id, user_id, total) VALUES (10, 1, 25.5)",
        ] {
            sqlx::query(stmt).execute(&pool).await.unwrap();
        }
        pool.close().await;
        TempDb(path)
    }

    fn cfg_for(path: &std::path::Path) -> ConnectionConfig {
        ConnectionConfig {
            name: "test".to_string(),
            driver: DriverType::Sqlite,
            host: String::new(),
            port: None,
            user: None,
            password: None,
            // The file path rides in `database` for this driver.
            database: Some(path.to_string_lossy().into_owned()),
            socket: None,
            ssl: false,
            ssl_mode: None,
            ssl_ca: None,
            ssl_cert: None,
            ssl_key: None,
            ssh: None,
        }
    }

    #[tokio::test]
    async fn test_browse_metadata_and_rows() {
        let db = seeded_db().await;
        let drv = SqliteDriver::connect(&cfg_for(&db.0)).await.unwrap();

        // Attached databases stand in for schemas; `temp` is filtered out.
        let ns = drv.namespaces().await.unwrap();
        assert!(ns.iter().any(|n| n.0 == "main"), "got {ns:?}");
        assert!(!ns.iter().any(|n| n.0 == "temp"));
        let main = Namespace("main".to_string());

        // Tables only — views and internal sqlite_* objects are excluded.
        let tables: Vec<String> = drv
            .collections(&main)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(tables, vec!["orders".to_string(), "users".to_string()]);

        let views: Vec<String> = drv
            .list_views(&main)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(views, vec!["active_users".to_string()]);

        // Column metadata: PK, nullability and the UNIQUE index all resolve.
        let users = CollectionRef {
            namespace: main.clone(),
            name: "users".to_string(),
        };
        let meta = drv.collection_meta(&users).await.unwrap();
        let id = meta.columns.iter().find(|c| c.name == "id").unwrap();
        assert!(id.is_primary_key && !id.is_nullable);
        let name = meta.columns.iter().find(|c| c.name == "name").unwrap();
        assert!(!name.is_nullable, "NOT NULL column must not be nullable");
        let email = meta.columns.iter().find(|c| c.name == "email").unwrap();
        assert!(email.is_nullable && email.is_unique, "unique index not picked up");

        // Foreign keys come from PRAGMA foreign_key_list.
        let orders = CollectionRef {
            namespace: main.clone(),
            name: "orders".to_string(),
        };
        let ometa = drv.collection_meta(&orders).await.unwrap();
        assert_eq!(ometa.foreign_keys.len(), 1);
        let fk = &ometa.foreign_keys[0];
        assert_eq!((fk.column.as_str(), fk.ref_table.as_str()), ("user_id", "users"));
        assert!(
            ometa.columns.iter().find(|c| c.name == "user_id").unwrap().is_foreign_key,
            "FK column not flagged"
        );

        // Rows + paging, and NULL / REAL / TEXT decoding.
        let page = drv
            .records(&users, Page { offset: 0, limit: 10 })
            .await
            .unwrap();
        assert_eq!(page.total_records, Some(2));
        assert_eq!(page.columns, vec!["id", "name", "email"]);
        assert_eq!(page.records[0].values[1], Value::String("ada".to_string()));
        assert_eq!(page.records[1].values[2], Value::Null);

        let one = drv
            .records(&users, Page { offset: 1, limit: 1 })
            .await
            .unwrap();
        assert_eq!(one.records.len(), 1);
        assert_eq!(one.page, 1);

        let orow = drv
            .records(&orders, Page { offset: 0, limit: 10 })
            .await
            .unwrap();
        assert_eq!(orow.records[0].values[2], Value::Float(25.5));

        // DDL is the original CREATE text, not a synthesised one.
        let ddl = drv.definition(&users).await.unwrap();
        assert!(ddl.starts_with("CREATE TABLE users"), "got {ddl}");

        // Arbitrary SQL: SELECT returns rows, INSERT reports rows_affected.
        let res = drv.execute(&main, "SELECT COUNT(*) AS n FROM users").await.unwrap();
        assert_eq!(res.records[0].values[0], Value::Int(2));
        let res = drv
            .execute(&main, "INSERT INTO users (id, name) VALUES (3, 'cy')")
            .await
            .unwrap();
        assert_eq!(res.rows_affected, 1);
    }

    /// Reverse foreign-key lookup: given a table+column, find every table in
    /// the schema whose FK points at it. This is the metadata the `F` binding
    /// walks, so prove it over a real database rather than trusting the shape.
    #[tokio::test]
    async fn test_foreign_keys_resolve_for_reverse_lookup() {
        let db = seeded_db().await;
        let drv = SqliteDriver::connect(&cfg_for(&db.0)).await.unwrap();
        let main = Namespace("main".to_string());

        // A second child table so the lookup has to find more than one.
        let extra = SqliteConnectOptions::new().filename(&db.0);
        let pool = SqlitePool::connect_with(extra).await.unwrap();
        sqlx::query(
            "CREATE TABLE sessions (id INTEGER PRIMARY KEY, owner INTEGER REFERENCES users(id))",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        // Exactly what the reverse navigation runs.
        let mut referencing: Vec<(String, String)> = drv
            .schema_foreign_keys(&main)
            .await
            .unwrap()
            .into_iter()
            .filter(|(_, fk)| fk.ref_table == "users" && fk.ref_column == "id")
            .map(|(table, fk)| (table, fk.column))
            .collect();
        referencing.sort();

        assert_eq!(
            referencing,
            vec![
                ("orders".to_string(), "user_id".to_string()),
                ("sessions".to_string(), "owner".to_string()),
            ],
            "both child tables must be found"
        );
    }

    #[tokio::test]
    async fn test_connect_rejects_missing_file_and_empty_path() {
        // A typo'd path must fail rather than silently create a new database.
        let missing = std::env::temp_dir().join("dbx-sqlite-does-not-exist-xyz.db");
        let _ = std::fs::remove_file(&missing);
        assert!(SqliteDriver::connect(&cfg_for(&missing)).await.is_err());
        assert!(!missing.exists(), "connect must not create the file");

        // No path at all is a clear error, not a panic.
        let mut cfg = cfg_for(&missing);
        cfg.database = None;
        assert!(SqliteDriver::connect(&cfg).await.is_err());
    }

    #[tokio::test]
    async fn test_interactive_transaction_commit_and_rollback() {
        use crate::driver::Driver;

        async fn user_count(drv: &SqliteDriver, ns: &Namespace) -> crate::driver::Value {
            drv.execute(ns, "SELECT COUNT(*) AS n FROM users")
                .await
                .unwrap()
                .records[0]
                .values[0]
                .clone()
        }

        let db = seeded_db().await;
        let drv = SqliteDriver::connect(&cfg_for(&db.0)).await.unwrap();
        let main = Namespace("main".to_string());

        // Rollback path: the inserted row must vanish.
        drv.begin_tx().await.unwrap();
        assert!(drv.in_tx().await);
        drv.execute(&main, "INSERT INTO users (id, name) VALUES (9001, 'tx-rollback')")
            .await
            .unwrap();
        assert!(drv.begin_tx().await.is_err(), "nested BEGIN must fail");
        drv.rollback_tx().await.unwrap();
        assert!(!drv.in_tx().await);

        // Commit path: the row persists. (If execute had silently used a
        // pooled connection, the INSERT would have escaped the transaction
        // and the rollback above would have changed nothing.)
        drv.begin_tx().await.unwrap();
        drv.execute(&main, "INSERT INTO users (id, name) VALUES (9002, 'tx-commit')")
            .await
            .unwrap();
        drv.commit_tx().await.unwrap();
        assert!(!drv.in_tx().await);

        let baseline = user_count(&drv, &main).await;
        drv.begin_tx().await.unwrap();
        drv.execute(&main, "INSERT INTO users (id, name) VALUES (9003, 'tx-gone')")
            .await
            .unwrap();
        drv.rollback_tx().await.unwrap();
        assert_eq!(user_count(&drv, &main).await, baseline, "rolled-back row must not persist");

        // Commit/rollback without an open transaction are clear errors.
        assert!(drv.commit_tx().await.is_err());
        assert!(drv.rollback_tx().await.is_err());
    }
}
