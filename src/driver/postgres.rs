//! PostgreSQL driver implementation using sqlx.
//! Translates PostgreSQL information_schema, pg_catalog metadata, and SQL queries into model-agnostic structs.

use std::time::{Duration, Instant};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions, PgRow};
use sqlx::{Column, Row, TypeInfo, ValueRef, AssertSqlSafe};

use super::{
    Capabilities, Collection, CollectionMeta, CollectionRef, ColumnMeta, Driver, DriverInfo,
    ForeignKeyMeta, IndexMeta, Namespace, Page, QueryResult, Record, RecordPage, Value,
};
use crate::config::ConnectionConfig;

/// Safely escapes a PostgreSQL identifier by wrapping it in double quotes and escaping inner quotes.
fn escape_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

pub struct PostgresDriver {
    pool: PgPool,
    info: DriverInfo,
    /// Dedicated connection held open while an interactive transaction is
    /// active — BEGIN/COMMIT/ROLLBACK and every statement between them must
    /// run on one connection, not whatever the pool hands out per call.
    tx_conn: tokio::sync::Mutex<Option<sqlx::pool::PoolConnection<sqlx::Postgres>>>,
}

impl PostgresDriver {
    pub async fn connect(cfg: &ConnectionConfig) -> Result<Self> {
        let mut opts = PgConnectOptions::new();
        let port = cfg.port.unwrap_or(5432);
        opts = opts.host(&cfg.host).port(port);

        // TLS enforcement. `None` = driver default (opportunistic TLS), so a
        // pre-existing config without ssl/ssl_mode keeps working.
        if let Some(mode) = cfg.effective_ssl_mode() {
            let ssl_mode = match mode {
                crate::config::SslMode::Disable => sqlx::postgres::PgSslMode::Disable,
                crate::config::SslMode::Require => sqlx::postgres::PgSslMode::Require,
                crate::config::SslMode::Verify => sqlx::postgres::PgSslMode::VerifyFull,
            };
            opts = opts.ssl_mode(ssl_mode);
        }

        // mTLS: custom CA + client certificate (PEM paths). Absent fields
        // keep today's behaviour exactly — sqlx only sends what is set.
        if let Some(ca) = &cfg.ssl_ca {
            opts = opts.ssl_root_cert(ca);
        }
        if let Some((cert, key)) = cfg.ssl_client_identity()? {
            opts = opts.ssl_client_cert(cert).ssl_client_key(key);
        }

        if let Some(user) = &cfg.user {
            opts = opts.username(user);
        }
        if let Some(pass) = cfg.resolve_password() {
            opts = opts.password(&pass);
        }
        if let Some(db) = &cfg.database {
            opts = opts.database(db);
        } else {
            // Default PostgreSQL database
            opts = opts.database("postgres");
        }

        let pool = PgPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(opts)
            .await
            .with_context(|| format!("failed to connect to PostgreSQL ({})", cfg.display_url()))?;

        // Query server version
        let version_row: (String,) = sqlx::query_as("SELECT version()")
            .fetch_one(&pool)
            .await
            .unwrap_or_else(|_| ("PostgreSQL (unknown version)".to_string(),));

        let info = DriverInfo {
            name: "PostgreSQL".to_string(),
            server_version: version_row.0,
            query_language: "SQL".to_string(),
        };

        Ok(Self { pool, info, tx_conn: tokio::sync::Mutex::new(None) })
    }

    /// Shared helper for the "list object names in a schema" queries
    /// (views / routines / sequences). Keeps the row→Collection mapping in
    /// one place instead of five copy-paste sites.
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
impl Driver for PostgresDriver {
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
            | Capabilities::PROCESS_LIST
    }

    /// One catalog query for every FK in the schema, instead of the default's
    /// round trip per table.
    async fn schema_foreign_keys(
        &self,
        ns: &Namespace,
    ) -> Result<Vec<(String, ForeignKeyMeta)>> {
        let sql = "\
            SELECT tc.table_name, tc.constraint_name, kcu.column_name, \
                   ccu.table_schema, ccu.table_name, ccu.column_name \
            FROM information_schema.table_constraints AS tc \
            JOIN information_schema.key_column_usage AS kcu \
              ON tc.constraint_name = kcu.constraint_name \
              AND tc.table_schema = kcu.table_schema \
            JOIN information_schema.constraint_column_usage AS ccu \
              ON ccu.constraint_name = tc.constraint_name \
              AND ccu.table_schema = tc.table_schema \
            WHERE tc.constraint_type = 'FOREIGN KEY' AND tc.table_schema = $1";
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

    /// Active backends from `pg_stat_activity`, excluding this connection and
    /// idle sessions — only work actually in flight is worth showing. Newest
    /// first, as the trait contract promises.
    async fn process_list(&self) -> Result<QueryResult> {
        let sql = "\
            SELECT pid, usename AS user, datname AS database, state, \
                   EXTRACT(EPOCH FROM (now() - query_start))::bigint AS seconds, \
                   query \
            FROM pg_stat_activity \
            WHERE pid <> pg_backend_pid() AND state <> 'idle' \
            ORDER BY query_start DESC";
        self.execute(&Namespace("public".to_string()), sql).await
    }

    async fn kill_process(&self, id: &str) -> Result<()> {
        let pid: i32 = id
            .trim()
            .parse()
            .with_context(|| format!("'{id}' is not a PostgreSQL pid"))?;
        // Cancel (not terminate): stops the query but keeps the session, which
        // is the conservative choice for someone else's connection.
        sqlx::query("SELECT pg_cancel_backend($1)")
            .bind(pid)
            .execute(&self.pool)
            .await
            .with_context(|| format!("failed to cancel backend {pid}"))?;
        Ok(())
    }

    async fn ping(&self) -> Result<Duration> {
        let start = Instant::now();
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .context("PostgreSQL ping failed")?;
        Ok(start.elapsed())
    }

    async fn namespaces(&self) -> Result<Vec<Namespace>> {
        let sql = "\
            SELECT schema_name \
            FROM information_schema.schemata \
            WHERE schema_name NOT IN ('information_schema', 'pg_catalog', 'pg_toast') \
              AND schema_name NOT LIKE 'pg_temp_%' \
              AND schema_name NOT LIKE 'pg_toast_temp_%' \
            ORDER BY schema_name";
        let rows = sqlx::query(sql)
            .fetch_all(&self.pool)
            .await
            .context("failed to list PostgreSQL schemas")?;

        let mut nss = Vec::new();
        for r in rows {
            let name: String = r.get(0);
            nss.push(Namespace(name));
        }

        // If public or other schemas are empty, at least ensure "public" is visible if available
        if nss.is_empty() {
            nss.push(Namespace("public".to_string()));
        }

        Ok(nss)
    }

    async fn collections(&self, ns: &Namespace) -> Result<Vec<Collection>> {
        // Partitioned parents (relkind 'p') own no storage, so sum the sizes
        // of their `pg_inherits` children; plain tables use their own size.
        let sql = "\
            SELECT c.relname AS table_name, \
                   c.reltuples::bigint AS estimated_rows, \
                   COALESCE( \
                     (SELECT sum(pg_total_relation_size(inhrelid)) \
                      FROM pg_inherits WHERE inhparent = c.oid), \
                     pg_total_relation_size(c.oid) \
                   )::bigint AS size_bytes \
            FROM pg_class c \
            JOIN pg_namespace n ON n.oid = c.relnamespace \
            WHERE n.nspname = $1 AND c.relkind IN ('r', 'p') \
            ORDER BY c.relname";

        let rows = sqlx::query(sql)
            .bind(&ns.0)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| {
                eprintln!("[dbx] pg collections query error: {e}");
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
            SELECT column_name, udt_name, is_nullable, column_default \
            FROM information_schema.columns \
            WHERE table_schema = $1 AND table_name = $2 \
            ORDER BY ordinal_position";
        let col_rows = sqlx::query(col_sql)
            .bind(&c.namespace.0)
            .bind(&c.name)
            .fetch_all(&self.pool)
            .await
            .with_context(|| format!("failed to describe columns for {}", c))?;

        // 2. Fetch primary key column names
        let pk_sql = "\
            SELECT kcu.column_name \
            FROM information_schema.table_constraints tc \
            JOIN information_schema.key_column_usage kcu \
              ON tc.constraint_name = kcu.constraint_name \
             AND tc.table_schema = kcu.table_schema \
            WHERE tc.constraint_type = 'PRIMARY KEY' \
              AND tc.table_schema = $1 \
              AND tc.table_name = $2";
        let pk_rows = sqlx::query(pk_sql)
            .bind(&c.namespace.0)
            .bind(&c.name)
            .fetch_all(&self.pool)
            .await
            .unwrap_or_default();
        let pk_set: std::collections::HashSet<String> = pk_rows.into_iter().map(|r| r.get(0)).collect();

        // 3. Fetch unique constraint column names
        let uq_sql = "\
            SELECT kcu.column_name \
            FROM information_schema.table_constraints tc \
            JOIN information_schema.key_column_usage kcu \
              ON tc.constraint_name = kcu.constraint_name \
             AND tc.table_schema = kcu.table_schema \
            WHERE tc.constraint_type = 'UNIQUE' \
              AND tc.table_schema = $1 \
              AND tc.table_name = $2";
        let uq_rows = sqlx::query(uq_sql)
            .bind(&c.namespace.0)
            .bind(&c.name)
            .fetch_all(&self.pool)
            .await
            .unwrap_or_default();
        let uq_set: std::collections::HashSet<String> = uq_rows.into_iter().map(|r| r.get(0)).collect();

        let mut columns = Vec::new();
        for r in col_rows {
            let name: String = r.get(0);
            let data_type: String = r.get(1);
            let is_nullable: String = r.get(2);
            let col_default: Option<String> = r.try_get(3).ok();

            let is_pk = pk_set.contains(&name);
            let is_uq = uq_set.contains(&name);

            columns.push(ColumnMeta {
                name,
                data_type,
                is_nullable: is_nullable == "YES",
                is_primary_key: is_pk,
                is_unique: is_uq,
                is_foreign_key: false, // Will be resolved below
                extra: col_default,
            });
        }

        // 4. Fetch indexes
        let idx_sql = "\
            SELECT indexname, indexdef \
            FROM pg_indexes \
            WHERE schemaname = $1 AND tablename = $2 \
            ORDER BY indexname";
        let idx_rows = sqlx::query(idx_sql)
            .bind(&c.namespace.0)
            .bind(&c.name)
            .fetch_all(&self.pool)
            .await
            .unwrap_or_default();

        let mut indexes = Vec::new();
        for r in idx_rows {
            let idx_name: String = r.get(0);
            let idx_def: String = r.get(1);
            let is_unique = idx_def.to_uppercase().contains("UNIQUE INDEX");
            let is_pri = idx_name.ends_with("_pkey") || pk_set.iter().any(|pk| idx_name.contains(pk));

            // Extract column names roughly from index definition: "... USING btree (col1, col2)"
            let cols = if let Some(start) = idx_def.rfind('(') {
                if let Some(end) = idx_def.rfind(')') {
                    if start < end {
                        idx_def[start + 1..end]
                            .split(',')
                            .map(|s| s.trim().trim_matches('"').to_string())
                            .collect()
                    } else {
                        vec![]
                    }
                } else {
                    vec![]
                }
            } else {
                vec![]
            };

            indexes.push(IndexMeta {
                name: idx_name,
                columns: cols,
                is_unique,
                is_primary: is_pri,
            });
        }

        // 5. Fetch foreign keys
        let fk_sql = "\
            SELECT \
                tc.constraint_name, \
                kcu.column_name, \
                ccu.table_schema AS foreign_table_schema, \
                ccu.table_name AS foreign_table_name, \
                ccu.column_name AS foreign_column_name \
            FROM information_schema.table_constraints AS tc \
            JOIN information_schema.key_column_usage AS kcu \
              ON tc.constraint_name = kcu.constraint_name \
              AND tc.table_schema = kcu.table_schema \
            JOIN information_schema.constraint_column_usage AS ccu \
              ON ccu.constraint_name = tc.constraint_name \
              AND ccu.table_schema = tc.table_schema \
            WHERE tc.constraint_type = 'FOREIGN KEY' \
              AND tc.table_schema = $1 \
              AND tc.table_name = $2";

        let fk_rows = sqlx::query(fk_sql)
            .bind(&c.namespace.0)
            .bind(&c.name)
            .fetch_all(&self.pool)
            .await
            .unwrap_or_default();

        let mut foreign_keys = Vec::new();
        let mut fk_col_names = std::collections::HashSet::new();

        for r in fk_rows {
            let name: String = r.get(0);
            let col: String = r.get(1);
            let ref_ns: String = r.get(2);
            let ref_tbl: String = r.get(3);
            let ref_col: String = r.get(4);

            fk_col_names.insert(col.clone());

            foreign_keys.push(ForeignKeyMeta {
                name,
                column: col,
                ref_namespace: Namespace(ref_ns),
                ref_table: ref_tbl,
                ref_column: ref_col,
            });
        }

        // Update is_foreign_key on columns
        for col in &mut columns {
            if fk_col_names.contains(&col.name) {
                col.is_foreign_key = true;
            }
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
            records.push(convert_postgres_row(&row)?);
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
        let mut pooled: sqlx::pool::PoolConnection<sqlx::Postgres>;
        let conn: &mut sqlx::postgres::PgConnection = if let Some(c) = tx_guard.as_mut() {
            &mut **c
        } else {
            pooled = self.pool.acquire().await.context("failed to acquire connection from pool")?;
            &mut *pooled
        };
        let set_path_sql = format!("SET search_path TO {}, public", escape_ident(&ns.0));
        let _ = sqlx::query(AssertSqlSafe(set_path_sql.as_str())).execute(&mut *conn).await;

        let start = Instant::now();
        let trimmed = query.trim_start();
        let is_select = super::starts_with_keyword(trimmed, "select")
            || super::starts_with_keyword(trimmed, "show")
            || super::starts_with_keyword(trimmed, "explain")
            || super::starts_with_keyword(trimmed, "with");

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
                records.push(convert_postgres_row(&row)?);
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
        let meta = self.collection_meta(c).await?;
        let mut ddl = format!("CREATE TABLE {}.{} (\n", escape_ident(&c.namespace.0), escape_ident(&c.name));

        let mut lines = Vec::new();
        for col in &meta.columns {
            let mut line = format!("    {} {}", escape_ident(&col.name), col.data_type.to_uppercase());
            if !col.is_nullable {
                line.push_str(" NOT NULL");
            }
            if let Some(def) = &col.extra {
                line.push_str(&format!(" DEFAULT {}", def));
            }
            lines.push(line);
        }

        let pks: Vec<String> = meta
            .columns
            .iter()
            .filter(|col| col.is_primary_key)
            .map(|col| escape_ident(&col.name))
            .collect();
        if !pks.is_empty() {
            lines.push(format!("    PRIMARY KEY ({})", pks.join(", ")));
        }

        for fk in &meta.foreign_keys {
            lines.push(format!(
                "    CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {}.{} ({})",
                escape_ident(&fk.name),
                escape_ident(&fk.column),
                escape_ident(&fk.ref_namespace.0),
                escape_ident(&fk.ref_table),
                escape_ident(&fk.ref_column)
            ));
        }

        ddl.push_str(&lines.join(",\n"));
        ddl.push_str("\n);");
        Ok(ddl)
    }

    async fn list_views(&self, ns: &Namespace) -> Result<Vec<Collection>> {
        // pg_class relkind 'v' (view) + 'm' (materialized view) — everything
        // that isn't a plain/partitioned table. `collections()` only returns
        // 'r'/'p', so views appear exactly once, under the View node.
        self.query_collection_names(
            "SELECT c.relname FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relkind IN ('v', 'm') \
             ORDER BY c.relname",
            &ns.0,
            "views",
        )
        .await
    }

    async fn list_routines(&self, ns: &Namespace) -> Result<Vec<Collection>> {
        self.query_collection_names(
            "SELECT routine_name FROM information_schema.routines \
             WHERE routine_schema = $1 GROUP BY routine_name ORDER BY routine_name",
            &ns.0,
            "routines",
        )
        .await
    }

    async fn list_sequences(&self, ns: &Namespace) -> Result<Vec<Collection>> {
        self.query_collection_names(
            "SELECT sequence_name FROM information_schema.sequences \
             WHERE sequence_schema = $1 ORDER BY sequence_name",
            &ns.0,
            "sequences",
        )
        .await
    }

    async fn routine_definition(&self, c: &CollectionRef) -> Result<String> {
        let mut conn = self.pool.acquire().await.context("failed to acquire connection from pool")?;
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT pg_get_functiondef(p.oid) \
             FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
             WHERE n.nspname = $1 AND p.proname = $2 \
             ORDER BY p.oid LIMIT 1",
        )
        .bind(&c.namespace.0)
        .bind(&c.name)
        .fetch_optional(&mut *conn)
        .await
        .context("failed to fetch routine definition")?;
        row.map(|r| r.0)
            .ok_or_else(|| anyhow!("routine not found: {}", c))
    }
}

/// Decode a size column that may come back as `INT8` or `NUMERIC` (Postgres
/// has no unsigned int8, and `sum()` widens to numeric) or a string —
/// try each representation so the code stays driver-agnostic.
fn size_bytes_from_row(row: &PgRow, idx: usize) -> Option<u64> {
    if let Ok(v) = row.try_get::<i64, _>(idx) {
        return Some(v.max(0) as u64);
    }
    if let Ok(v) = row.try_get::<f64, _>(idx) {
        return Some(v.max(0.0) as u64);
    }
    if let Ok(v) = row.try_get::<String, _>(idx) {
        return v.trim().parse().ok();
    }
    None
}

/// Converts a dynamic sqlx PostgreSQL row into our generic `Record`
fn convert_postgres_row(row: &PgRow) -> Result<Record> {
    let mut values = Vec::with_capacity(row.len());

    for (i, col) in row.columns().iter().enumerate() {
        let raw_val = row.try_get_raw(i)?;
        if raw_val.is_null() {
            values.push(Value::Null);
            continue;
        }

        let type_name = col.type_info().name();
        let val = match type_name {
            "BOOL" | "BOOLEAN" => {
                if let Ok(b) = row.try_get::<bool, _>(i) {
                    Value::Bool(b)
                } else {
                    Value::Null
                }
            }
            "INT2" | "SMALLINT" | "SMALLSERIAL" => {
                if let Ok(n) = row.try_get::<i16, _>(i) {
                    Value::Int(n as i64)
                } else {
                    Value::Null
                }
            }
            "INT4" | "INT" | "INTEGER" | "SERIAL" => {
                if let Ok(n) = row.try_get::<i32, _>(i) {
                    Value::Int(n as i64)
                } else {
                    Value::Null
                }
            }
            "INT8" | "BIGINT" | "BIGSERIAL" | "OID" => {
                if let Ok(n) = row.try_get::<i64, _>(i) {
                    Value::Int(n)
                } else {
                    Value::Null
                }
            }
            "FLOAT4" | "REAL" => {
                if let Ok(f) = row.try_get::<f32, _>(i) {
                    Value::Float(f as f64)
                } else {
                    Value::Null
                }
            }
            "FLOAT8" | "DOUBLE PRECISION" => {
                if let Ok(f) = row.try_get::<f64, _>(i) {
                    Value::Float(f)
                } else {
                    Value::Null
                }
            }
            "NUMERIC" | "DECIMAL" => {
                if let Ok(d) = row.try_get::<sqlx::types::BigDecimal, _>(i) {
                    Value::Decimal(d.to_string())
                } else if let Ok(s) = row.try_get::<String, _>(i) {
                    Value::Decimal(s)
                } else {
                    Value::Null
                }
            }
            "JSON" | "JSONB" => {
                if let Ok(j) = row.try_get::<serde_json::Value, _>(i) {
                    Value::Json(j)
                } else {
                    Value::Null
                }
            }
            "BYTEA" => {
                if let Ok(bytes) = row.try_get::<Vec<u8>, _>(i) {
                    Value::Bytes(bytes)
                } else {
                    Value::Null
                }
            }
            "UUID" => {
                if let Ok(u) = row.try_get::<sqlx::types::Uuid, _>(i) {
                    Value::String(u.to_string())
                } else if let Ok(s) = row.try_get::<String, _>(i) {
                    Value::String(s)
                } else {
                    Value::Null
                }
            }
            "TIMESTAMP" | "TIMESTAMPTZ" | "DATE" | "TIME" | "TIMETZ" => {
                if let Ok(dt) = row.try_get::<sqlx::types::chrono::DateTime<sqlx::types::chrono::Utc>, _>(i) {
                    Value::DateTime(dt.to_rfc3339())
                } else if let Ok(ndt) = row.try_get::<sqlx::types::chrono::NaiveDateTime, _>(i) {
                    Value::DateTime(ndt.to_string())
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
                    Value::String(format!("<postgres:{}>", type_name))
                }
            }
        };
        values.push(val);
    }

    Ok(Record { values })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escape_ident() {
        assert_eq!(escape_ident("users"), "\"users\"");
        assert_eq!(escape_ident("user\"table"), "\"user\"\"table\"");
    }
}
