//! MySQL driver implementation using sqlx.
//! Translates information_schema metadata and SQL queries into model-agnostic structs.

use std::time::{Duration, Instant};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use sqlx::mysql::{MySqlConnectOptions, MySqlPool, MySqlPoolOptions, MySqlRow};
use sqlx::{Column, Row, TypeInfo, ValueRef, AssertSqlSafe};

use super::{
    Capabilities, Collection, CollectionMeta, CollectionRef, ColumnMeta, Driver, DriverInfo,
    ForeignKeyMeta, IndexMeta, Namespace, Page, QueryResult, Record, RecordPage, Value,
};
use crate::config::ConnectionConfig;

/// Safely escapes a MySQL identifier by replacing backticks with double-backticks
fn escape_ident(ident: &str) -> String {
    format!("`{}`", ident.replace('`', "``"))
}

pub struct MySqlDriver {
    pool: MySqlPool,
    info: DriverInfo,
    /// Dedicated connection held open while an interactive transaction is
    /// active — BEGIN/COMMIT/ROLLBACK and every statement between them must
    /// run on one connection, not whatever the pool hands out per call.
    tx_conn: tokio::sync::Mutex<Option<sqlx::pool::PoolConnection<sqlx::MySql>>>,
}

impl MySqlDriver {
    pub async fn connect(cfg: &ConnectionConfig) -> Result<Self> {
        let mut opts = MySqlConnectOptions::new();
        if let Some(sock) = &cfg.socket {
            opts = opts.socket(sock);
        } else {
            let port = cfg.port.unwrap_or(3306);
            opts = opts.host(&cfg.host).port(port);
        }

        // TLS enforcement. `None` = driver default (opportunistic TLS), so a
        // pre-existing config without ssl/ssl_mode keeps working.
        if let Some(mode) = cfg.effective_ssl_mode() {
            let ssl_mode = match mode {
                crate::config::SslMode::Disable => sqlx::mysql::MySqlSslMode::Disabled,
                crate::config::SslMode::Require => sqlx::mysql::MySqlSslMode::Required,
                crate::config::SslMode::Verify => sqlx::mysql::MySqlSslMode::VerifyIdentity,
            };
            opts = opts.ssl_mode(ssl_mode);
        }

        if let Some(user) = &cfg.user {
            opts = opts.username(user);
        }
        if let Some(pass) = cfg.resolve_password() {
            opts = opts.password(&pass);
        }
        if let Some(db) = &cfg.database {
            opts = opts.database(db);
        }

        let pool = MySqlPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(opts)
            .await
            .with_context(|| format!("failed to connect to MySQL ({})", cfg.display_url()))?;

        // Query server version
        let version_row: (String,) = sqlx::query_as("SELECT VERSION()")
            .fetch_one(&pool)
            .await
            .unwrap_or_else(|_| ("unknown".to_string(),));

        let info = DriverInfo {
            name: "MySQL".to_string(),
            server_version: version_row.0,
            query_language: "SQL".to_string(),
        };

        Ok(Self { pool, info, tx_conn: tokio::sync::Mutex::new(None) })
    }

    /// Shared helper for the "list object names in a schema" queries.
    async fn query_collection_names(
        &self,
        sql: &'static str,
        ns: &str,
        what: &str,
    ) -> Result<Vec<Collection>> {
        let rows = sqlx::query(sql)
            .bind(ns)
            .fetch_all(&self.pool)
            .await
            .with_context(|| format!("failed to list {what}"))?;
        Ok(rows
            .iter()
            .map(|r| Collection {
                name: r.get::<String, _>(0),
                estimated_row_count: None,
                estimated_size_bytes: None,
            })
            .collect())
    }
}

#[async_trait]
impl Driver for MySqlDriver {
    fn info(&self) -> DriverInfo {
        self.info.clone()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::BROWSE
            | Capabilities::QUERY_TEXT
            | Capabilities::DDL
            | Capabilities::ERD
            | Capabilities::EXPLAIN
            | Capabilities::PROCESS_LIST
            | Capabilities::EDIT_DATA
    }

    /// One catalog query for every FK in the schema.
    async fn schema_foreign_keys(
        &self,
        ns: &Namespace,
    ) -> Result<Vec<(String, ForeignKeyMeta)>> {
        let sql = "\
            SELECT TABLE_NAME, CONSTRAINT_NAME, COLUMN_NAME, \
                   REFERENCED_TABLE_SCHEMA, REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME \
            FROM information_schema.KEY_COLUMN_USAGE \
            WHERE TABLE_SCHEMA = ? AND REFERENCED_TABLE_NAME IS NOT NULL";
        let rows = sqlx::query(sql)
            .bind(&ns.0)
            .fetch_all(&self.pool)
            .await
            .with_context(|| format!("failed to list foreign keys in {}", ns.0))?;

        Ok(rows
            .iter()
            .map(|r| {
                let table: String = r.get(0);
                (
                    table,
                    ForeignKeyMeta {
                        name: r.get(1),
                        column: r.get(2),
                        ref_namespace: Namespace(r.get(3)),
                        ref_table: r.get(4),
                        ref_column: r.get(5),
                    },
                )
            })
            .collect())
    }

    /// Running threads from `information_schema.PROCESSLIST`, excluding this
    /// connection and sleeping sessions.
    async fn process_list(&self) -> Result<QueryResult> {
        let sql = "\
            SELECT ID AS pid, USER AS user, DB AS `database`, COMMAND AS state, \
                   TIME AS seconds, INFO AS query \
            FROM information_schema.PROCESSLIST \
            WHERE ID <> CONNECTION_ID() AND COMMAND <> 'Sleep' \
            ORDER BY TIME DESC";
        self.execute(&Namespace("information_schema".to_string()), sql)
            .await
    }

    async fn kill_process(&self, id: &str) -> Result<()> {
        let pid: u64 = id
            .trim()
            .parse()
            .with_context(|| format!("'{id}' is not a MySQL thread id"))?;
        // KILL QUERY ends the statement but leaves the connection alive.
        sqlx::query(AssertSqlSafe(format!("KILL QUERY {pid}")))
            .execute(&self.pool)
            .await
            .with_context(|| format!("failed to kill query on thread {pid}"))?;
        Ok(())
    }

    async fn ping(&self) -> Result<Duration> {
        let start = Instant::now();
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .context("MySQL ping failed")?;
        Ok(start.elapsed())
    }

    async fn namespaces(&self) -> Result<Vec<Namespace>> {
        let rows = sqlx::query("SHOW DATABASES")
            .fetch_all(&self.pool)
            .await
            .context("failed to list databases")?;

        let mut nss = Vec::new();
        for r in rows {
            let name: String = r.get(0);
            // Hide internal MySQL schemas by default
            if name != "information_schema" && name != "performance_schema" && name != "mysql" && name != "sys" {
                nss.push(Namespace(name));
            }
        }
        Ok(nss)
    }

    async fn collections(&self, ns: &Namespace) -> Result<Vec<Collection>> {
        // TABLE_ROWS is BIGINT UNSIGNED — cast to signed so sqlx can decode
        // it as i64 (the unsigned flag otherwise makes try_get::<i64> fail).
        let sql = "\
            SELECT TABLE_NAME, CAST(TABLE_ROWS AS SIGNED), (DATA_LENGTH + INDEX_LENGTH) AS size_bytes \
            FROM information_schema.TABLES \
            WHERE TABLE_SCHEMA = ? AND TABLE_TYPE = 'BASE TABLE' \
            ORDER BY TABLE_NAME";
        let rows = sqlx::query(sql)
            .bind(&ns.0)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| {
                eprintln!("[dbx] mysql collections query error: {e}");
                e
            })
            .with_context(|| format!("failed to list tables in schema {}", ns.0))?;

        let mut cols = Vec::new();
        for r in rows {
            let name: String = r.get(0);
            let row_est: Option<i64> = r.try_get(1).ok();
            let size = size_bytes_from_row(&r, 2);
            cols.push(Collection {
                name,
                estimated_row_count: row_est.map(|v| v.max(0) as u64),
                estimated_size_bytes: size,
            });
        }
        Ok(cols)
    }

    async fn collection_meta(&self, c: &CollectionRef) -> Result<CollectionMeta> {
        // 1. Fetch columns
        let col_sql = "\
            SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_KEY, EXTRA \
            FROM information_schema.COLUMNS \
            WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
            ORDER BY ORDINAL_POSITION";
        let col_rows = sqlx::query(col_sql)
            .bind(&c.namespace.0)
            .bind(&c.name)
            .fetch_all(&self.pool)
            .await
            .with_context(|| format!("failed to describe columns for {}", c))?;

        let mut columns = Vec::new();
        for r in col_rows {
            let name: String = r.get(0);
            let data_type: String = r.get(1);
            let is_nullable: String = r.get(2);
            let col_key: String = r.get(3);
            let extra: String = r.get(4);

            columns.push(ColumnMeta {
                name,
                data_type,
                is_nullable: is_nullable == "YES",
                is_primary_key: col_key == "PRI",
                is_unique: col_key == "UNI",
                is_foreign_key: col_key == "MUL",
                extra: if extra.is_empty() { None } else { Some(extra) },
            });
        }

        // 2. Fetch indexes
        let idx_sql = "\
            SELECT INDEX_NAME, COLUMN_NAME, NON_UNIQUE \
            FROM information_schema.STATISTICS \
            WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
            ORDER BY INDEX_NAME, SEQ_IN_INDEX";
        let idx_rows = sqlx::query(idx_sql)
            .bind(&c.namespace.0)
            .bind(&c.name)
            .fetch_all(&self.pool)
            .await
            .unwrap_or_default();

        let mut index_map: std::collections::BTreeMap<String, (bool, bool, Vec<String>)> = std::collections::BTreeMap::new();
        for r in idx_rows {
            let idx_name: String = r.get(0);
            let col_name: String = r.get(1);
            let non_unique: i32 = r.try_get(2).unwrap_or(1);
            let entry = index_map.entry(idx_name.clone()).or_insert_with(|| {
                let is_pri = idx_name == "PRIMARY";
                let is_uniq = non_unique == 0;
                (is_uniq, is_pri, Vec::new())
            });
            entry.2.push(col_name);
        }

        let indexes = index_map
            .into_iter()
            .map(|(name, (is_unique, is_primary, columns))| IndexMeta {
                name,
                columns,
                is_unique,
                is_primary,
            })
            .collect();

        // 3. Fetch foreign keys
        let fk_sql = "\
            SELECT CONSTRAINT_NAME, COLUMN_NAME, REFERENCED_TABLE_SCHEMA, REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME \
            FROM information_schema.KEY_COLUMN_USAGE \
            WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND REFERENCED_TABLE_NAME IS NOT NULL";
        let fk_rows = sqlx::query(fk_sql)
            .bind(&c.namespace.0)
            .bind(&c.name)
            .fetch_all(&self.pool)
            .await
            .unwrap_or_default();

        let mut foreign_keys = Vec::new();
        for r in fk_rows {
            let name: String = r.get(0);
            let col: String = r.get(1);
            let ref_ns: String = r.get(2);
            let ref_tbl: String = r.get(3);
            let ref_col: String = r.get(4);
            foreign_keys.push(ForeignKeyMeta {
                name,
                column: col,
                ref_namespace: Namespace(ref_ns),
                ref_table: ref_tbl,
                ref_column: ref_col,
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

        let count_sql = format!("SELECT COUNT(*) FROM {}.{}", ns_esc, name_esc);
        let total_records: Option<u64> = sqlx::query_as::<_, (i64,)>(AssertSqlSafe(count_sql.as_str()))
            .fetch_one(&self.pool)
            .await
            .ok()
            .map(|(count,)| count.max(0) as u64);

        let query = format!(
            "SELECT * FROM {}.{} LIMIT {} OFFSET {}",
            ns_esc, name_esc, page.limit, page.offset
        );

        let start = Instant::now();
        let rows = sqlx::query(AssertSqlSafe(query.as_str()))
            .fetch_all(&self.pool)
            .await
            .with_context(|| format!("failed to fetch records for {}", c))?;
        let _ = start.elapsed();

        let mut columns = Vec::new();
        let mut records = Vec::new();

        if let Some(first_row) = rows.first() {
            columns = first_row
                .columns()
                .iter()
                .map(|col| col.name().to_string())
                .collect();
        }

        for row in rows {
            records.push(convert_mysql_row(&row)?);
        }

        Ok(RecordPage {
            columns,
            records,
            page: page.offset / page.limit,
            page_size: page.limit,
            total_records,
        })
    }

    async fn execute(&self, ns: &Namespace, query: &str) -> Result<QueryResult> {
        // When an interactive transaction is open, run on its dedicated
        // connection; otherwise borrow a pooled connection for this call only.
        let mut tx_guard = self.tx_conn.lock().await;
        let mut pooled: sqlx::pool::PoolConnection<sqlx::MySql>;
        let conn: &mut sqlx::mysql::MySqlConnection = if let Some(c) = tx_guard.as_mut() {
            &mut **c
        } else {
            pooled = self.pool.acquire().await.context("failed to acquire connection from pool")?;
            &mut *pooled
        };
        let use_sql = format!("USE {}", escape_ident(&ns.0));
        let _ = sqlx::query(AssertSqlSafe(use_sql.as_str())).execute(&mut *conn).await;

        let start = Instant::now();
        let trimmed = query.trim_start();
        let is_select = super::starts_with_keyword(trimmed, "select")
            || super::starts_with_keyword(trimmed, "show")
            || super::starts_with_keyword(trimmed, "explain")
            || super::starts_with_keyword(trimmed, "describe")
            || super::starts_with_keyword(trimmed, "desc");

        if is_select {
            let rows = sqlx::query(AssertSqlSafe(query))
                .fetch_all(&mut *conn)
                .await
                .with_context(|| format!("query execution failed: {}", query))?;
            let elapsed = start.elapsed();

            let mut columns = Vec::new();
            let mut records = Vec::new();

            if let Some(first_row) = rows.first() {
                columns = first_row
                    .columns()
                    .iter()
                    .map(|col| col.name().to_string())
                    .collect();
            }

            for row in rows {
                records.push(convert_mysql_row(&row)?);
            }

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
                .with_context(|| format!("statement execution failed: {}", query))?;
            let elapsed = start.elapsed();

            Ok(QueryResult {
                columns: Vec::new(),
                records: Vec::new(),
                rows_affected: res.rows_affected(),
                execution_time: elapsed,
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

    async fn definition(&self, c: &CollectionRef) -> Result<String> {
        let sql = format!("SHOW CREATE TABLE {}.{}", escape_ident(&c.namespace.0), escape_ident(&c.name));
        let row = sqlx::query(AssertSqlSafe(sql.as_str()))
            .fetch_one(&self.pool)
            .await
            .with_context(|| format!("failed to get DDL for {}", c))?;
        let ddl: String = row.get(1);
        Ok(ddl)
    }

    async fn list_views(&self, ns: &Namespace) -> Result<Vec<Collection>> {
        self.query_collection_names(
            "SELECT TABLE_NAME FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = ? AND TABLE_TYPE = 'VIEW' ORDER BY TABLE_NAME",
            &ns.0,
            "views",
        )
        .await
    }

    async fn list_routines(&self, ns: &Namespace) -> Result<Vec<Collection>> {
        self.query_collection_names(
            "SELECT ROUTINE_NAME FROM information_schema.ROUTINES \
             WHERE ROUTINE_SCHEMA = ? GROUP BY ROUTINE_NAME ORDER BY ROUTINE_NAME",
            &ns.0,
            "routines",
        )
        .await
    }

    // list_sequences: MySQL has no first-class sequences — default impl
    // (empty) is used.

    async fn routine_definition(&self, c: &CollectionRef) -> Result<String> {
        let row = sqlx::query(
            "SELECT ROUTINE_DEFINITION FROM information_schema.ROUTINES \
             WHERE ROUTINE_SCHEMA = ? AND ROUTINE_NAME = ? \
             ORDER BY ROUTINE_TYPE LIMIT 1",
        )
        .bind(&c.namespace.0)
        .bind(&c.name)
        .fetch_optional(&self.pool)
        .await
        .context("failed to fetch routine definition")?;
        match row {
            Some(r) => match r.try_get::<String, _>(0) {
                // ROUTINE_DEFINITION is NULL for users without SHOW_ROUTINE /
                // global SELECT — treat as "unavailable" instead of panicking.
                Ok(def) if !def.is_empty() => Ok(def),
                _ => Err(anyhow!(
                    "routine definition unavailable (insufficient privileges): {}",
                    c
                )),
            },
            None => Err(anyhow!("routine not found: {}", c)),
        }
    }
}

/// Decode a size column that may come back as signed / unsigned / numeric or
/// string depending on the query — try each representation.
fn size_bytes_from_row(row: &MySqlRow, idx: usize) -> Option<u64> {
    if let Ok(v) = row.try_get::<i64, _>(idx) {
        return Some(v.max(0) as u64);
    }
    if let Ok(v) = row.try_get::<u64, _>(idx) {
        return Some(v);
    }
    if let Ok(v) = row.try_get::<f64, _>(idx) {
        return Some(v.max(0.0) as u64);
    }
    if let Ok(v) = row.try_get::<String, _>(idx) {
        return v.trim().parse().ok();
    }
    None
}

/// Converts a dynamic sqlx MySQL row into our generic `Record`
fn convert_mysql_row(row: &MySqlRow) -> Result<Record> {
    let mut values = Vec::with_capacity(row.len());

    for (i, col) in row.columns().iter().enumerate() {
        let raw_val = row.try_get_raw(i)?;
        if raw_val.is_null() {
            values.push(Value::Null);
            continue;
        }

        let type_name = col.type_info().name();
        let val = match type_name {
            "BOOLEAN" | "BOOL" | "TINYINT(1)" => {
                if let Ok(b) = row.try_get::<bool, _>(i) {
                    Value::Bool(b)
                } else if let Ok(n) = row.try_get::<i8, _>(i) {
                    Value::Bool(n != 0)
                } else {
                    Value::String(row.try_get::<String, _>(i).unwrap_or_default())
                }
            }
            "TINYINT" | "SMALLINT" | "INT" | "MEDIUMINT" => {
                if let Ok(n) = row.try_get::<i32, _>(i) {
                    Value::Int(n as i64)
                } else if let Ok(u) = row.try_get::<u32, _>(i) {
                    Value::UInt(u as u64)
                } else {
                    Value::Null
                }
            }
            "BIGINT" => {
                if let Ok(n) = row.try_get::<i64, _>(i) {
                    Value::Int(n)
                } else if let Ok(u) = row.try_get::<u64, _>(i) {
                    Value::UInt(u)
                } else {
                    Value::Null
                }
            }
            "FLOAT" | "DOUBLE" => {
                if let Ok(f) = row.try_get::<f64, _>(i) {
                    Value::Float(f)
                } else {
                    Value::Null
                }
            }
            "DECIMAL" | "NUMERIC" => {
                if let Ok(d) = row.try_get::<sqlx::types::BigDecimal, _>(i) {
                    Value::Decimal(d.to_string())
                } else if let Ok(s) = row.try_get::<String, _>(i) {
                    Value::Decimal(s)
                } else {
                    Value::Null
                }
            }
            "JSON" => {
                if let Ok(j) = row.try_get::<serde_json::Value, _>(i) {
                    Value::Json(j)
                } else {
                    Value::Null
                }
            }
            "BINARY" | "VARBINARY" | "BLOB" | "TINYBLOB" | "MEDIUMBLOB" | "LONGBLOB" => {
                if let Ok(bytes) = row.try_get::<Vec<u8>, _>(i) {
                    Value::Bytes(bytes)
                } else {
                    Value::Null
                }
            }
            "DATETIME" | "TIMESTAMP" | "DATE" | "TIME" => {
                if let Ok(dt) = row.try_get::<sqlx::types::chrono::NaiveDateTime, _>(i) {
                    Value::DateTime(dt.to_string())
                } else if let Ok(d) = row.try_get::<sqlx::types::chrono::NaiveDate, _>(i) {
                    Value::DateTime(d.to_string())
                } else if let Ok(s) = row.try_get::<String, _>(i) {
                    Value::DateTime(s)
                } else {
                    Value::Null
                }
            }
            _ => {
                if let Ok(s) = row.try_get::<String, _>(i) {
                    Value::String(s)
                } else {
                    Value::String(format!("<unsupported: {}>", type_name))
                }
            }
        };
        values.push(val);
    }

    Ok(Record { values })
}
