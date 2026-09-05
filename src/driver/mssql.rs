//! Microsoft SQL Server driver using tiberius (the TDS protocol).
//! Translates sys.* / INFORMATION_SCHEMA metadata and T-SQL queries into the
//! model-agnostic structs shared with the sqlx drivers.

use std::time::{Duration, Instant};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use tiberius::{AuthMethod, Client, ColumnType, Config, EncryptionLevel, Row};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use super::{
    Capabilities, Collection, CollectionMeta, CollectionRef, ColumnMeta, Driver, DriverInfo,
    ForeignKeyMeta, IndexMeta, Namespace, Page, QueryResult, Record, RecordPage, Value,
};
use crate::config::{ConnectionConfig, SslMode};

/// tiberius has no connection pool — a `Client` is one connection.
type MssqlClient = Client<Compat<TcpStream>>;

/// Safely escapes a T-SQL identifier with brackets (`]` → `]]`).
fn escape_ident(ident: &str) -> String {
    format!("[{}]", ident.replace(']', "]]"))
}

pub struct MssqlDriver {
    /// The working connection. Behind a mutex because `Client` is a single
    /// connection and every query needs `&mut`.
    client: tokio::sync::Mutex<MssqlClient>,
    /// Kept so an interactive transaction can open a second, dedicated
    /// connection — mirroring the sqlx drivers' pool-acquired tx_conn.
    cfg: ConnectionConfig,
    info: DriverInfo,
    /// Dedicated connection held open while an interactive transaction is
    /// active — BEGIN/COMMIT/ROLLBACK and every statement between them must
    /// run on one connection.
    tx_client: tokio::sync::Mutex<Option<MssqlClient>>,
}

/// Translates our TLS settings into tiberius terms. SQL Server's default is
/// already `EncryptionLevel::Required`; the nuance is certificate trust.
///
/// mTLS limitation: tiberius 0.12 has no client-certificate API — its TLS
/// streams (both the native-tls and rustls backends) hard-code
/// `with_no_client_auth()`, and `Config` exposes only server-trust knobs
/// (`trust_cert`, `trust_cert_ca`). `ConnectionConfig`'s
/// `ssl_ca`/`ssl_cert`/`ssl_key` are therefore ignored for SQL Server;
/// TLS client-certificate authentication against SQL Server is not
/// possible with this driver version.
fn tiberius_config(cfg: &ConnectionConfig) -> Config {
    let mut c = Config::new();
    c.host(&cfg.host);
    c.port(cfg.port.unwrap_or_else(|| cfg.driver.default_port()));
    if let Some(db) = &cfg.database {
        c.database(db);
    }
    c.authentication(AuthMethod::sql_server(
        cfg.user.clone().unwrap_or_default(),
        cfg.resolve_password().unwrap_or_default(),
    ));
    match cfg.effective_ssl_mode() {
        Some(SslMode::Disable) => c.encryption(EncryptionLevel::NotSupported),
        Some(SslMode::Require) => {
            c.encryption(EncryptionLevel::Required);
            // Encryption without identity verification: on-prem SQL Server
            // almost always presents a self-signed certificate.
            c.trust_cert();
        }
        // Verify: required + validate against the system roots (no trust_cert).
        Some(SslMode::Verify) => c.encryption(EncryptionLevel::Required),
        // Driver default: encrypt, but tolerate self-signed certificates —
        // the opportunistic-TLS stance the other drivers take.
        None => c.trust_cert(),
    }
    c
}

async fn open_client(cfg: &ConnectionConfig) -> Result<MssqlClient> {
    let config = tiberius_config(cfg);
    let addr = config.get_addr();
    let tcp = TcpStream::connect(&addr)
        .await
        .with_context(|| format!("failed to reach SQL Server at {addr}"))?;
    let _ = tcp.set_nodelay(true);
    Client::connect(config, tcp.compat_write())
        .await
        .with_context(|| format!("failed to connect to SQL Server ({})", cfg.display_url()))
}

impl MssqlDriver {
    pub async fn connect(cfg: &ConnectionConfig) -> Result<Self> {
        let mut client = open_client(cfg).await?;

        let server_version = match client.simple_query("SELECT @@VERSION").await {
            Ok(s) => s
                .into_row()
                .await
                .ok()
                .flatten()
                .and_then(|row| row.get::<&str, usize>(0).map(|s| s.to_string()))
                // @@VERSION is multi-line; the first line carries the edition.
                .map(|v| v.lines().next().unwrap_or(&v).to_string())
                .unwrap_or_else(|| "SQL Server (unknown version)".to_string()),
            Err(_) => "SQL Server (unknown version)".to_string(),
        };

        let info = DriverInfo {
            name: "SQL Server".to_string(),
            server_version,
            query_language: "T-SQL".to_string(),
        };

        Ok(Self {
            client: tokio::sync::Mutex::new(client),
            cfg: cfg.clone(),
            info,
            tx_client: tokio::sync::Mutex::new(None),
        })
    }

    /// Shared helper for the "list object names in a schema" catalog queries
    /// (views / routines / sequences). Same consolidation as the postgres
    /// driver's `query_collection_names`.
    async fn query_collection_names(
        &self,
        sql: &'static str,
        ns: &str,
        what: &str,
    ) -> Result<Vec<Collection>> {
        let mut client = self.client.lock().await;
        let rows = client
            .query(sql, &[&ns])
            .await
            .with_context(|| format!("failed to list {what}"))?
            .into_first_result()
            .await
            .with_context(|| format!("failed to read {what}"))?;
        Ok(rows
            .iter()
            .filter_map(|r| r.get::<&str, usize>(0))
            .map(|name| Collection {
                name: name.to_string(),
                estimated_row_count: None,
                estimated_size_bytes: None,
            })
            .collect())
    }
}

/// Drains a statement that returns nothing of interest (BEGIN / COMMIT /
/// ROLLBACK / KILL) so the connection is clean for the next command.
async fn run_utility(client: &mut MssqlClient, sql: &str) -> Result<()> {
    client
        .simple_query(sql)
        .await
        .with_context(|| format!("statement failed: {sql}"))?
        .into_results()
        .await
        .with_context(|| format!("statement failed: {sql}"))?;
    Ok(())
}

/// Runs a SELECT-shaped query on `client` and shapes the rows into a
/// `QueryResult`. Shared by `execute` and by `process_list` (which runs on a
/// dedicated connection so a runaway query can't block the monitor).
async fn run_select(client: &mut MssqlClient, sql: &str, start: Instant) -> Result<QueryResult> {
    let mut stream = client
        .query(sql, &[])
        .await
        .with_context(|| format!("query execution failed: {}", sql))?;
    let columns: Vec<String> = stream
        .columns()
        .await
        .ok()
        .flatten()
        .map(|cols| cols.iter().map(|c| c.name().to_string()).collect())
        .unwrap_or_default();
    let rows = stream
        .into_first_result()
        .await
        .with_context(|| format!("query execution failed: {}", sql))?;
    let elapsed = start.elapsed();

    let mut records = Vec::new();
    for row in &rows {
        records.push(convert_mssql_row(row)?);
    }
    let count = records.len() as u64;

    Ok(QueryResult {
        columns,
        records,
        rows_affected: count,
        execution_time: elapsed,
    })
}

#[async_trait]
impl Driver for MssqlDriver {
    fn info(&self) -> DriverInfo {
        self.info.clone()
    }

    fn capabilities(&self) -> Capabilities {
        // No EXPLAIN: SQL Server's SHOWPLAN_XML is a session mode, not a
        // statement prefix, and the plan is XML — it doesn't fit the
        // row-oriented plan tree the UI renders.
        Capabilities::BROWSE
            | Capabilities::QUERY_TEXT
            | Capabilities::DDL
            | Capabilities::ERD
            | Capabilities::EDIT_DATA
            | Capabilities::PROCESS_LIST
    }

    async fn ping(&self) -> Result<Duration> {
        let start = Instant::now();
        let mut client = self.client.lock().await;
        client
            .simple_query("SELECT 1")
            .await
            .context("SQL Server ping failed")?
            .into_results()
            .await
            .context("SQL Server ping failed")?;
        Ok(start.elapsed())
    }

    async fn namespaces(&self) -> Result<Vec<Namespace>> {
        // Schemas, like the postgres driver. The engine's own schemas are
        // noise for browsing (sys is huge), so they are filtered out.
        let sql = "\
            SELECT name FROM sys.schemas \
            WHERE name NOT IN ('sys', 'INFORMATION_SCHEMA', 'guest') \
              AND name NOT LIKE 'db!_%' ESCAPE '!' \
            ORDER BY name";
        let mut client = self.client.lock().await;
        let rows = client
            .query(sql, &[])
            .await
            .context("failed to list SQL Server schemas")?
            .into_first_result()
            .await
            .context("failed to list SQL Server schemas")?;

        let mut nss: Vec<Namespace> = rows
            .iter()
            .filter_map(|r| r.get::<&str, usize>(0))
            .map(|s| Namespace(s.to_string()))
            .collect();
        // Every SQL Server database has a dbo schema; keep it visible even if
        // the filter above somehow left nothing.
        if nss.is_empty() {
            nss.push(Namespace("dbo".to_string()));
        }
        Ok(nss)
    }

    async fn collections(&self, ns: &Namespace) -> Result<Vec<Collection>> {
        // Row counts come from sys.partitions, which honours ordinary metadata
        // visibility. sys.dm_db_partition_stats would require VIEW DATABASE
        // STATE — a plain db_datareader account (the typical read-only
        // persona for a DB explorer) doesn't have it, and hard-failing there
        // would leave the whole schema tree empty.
        let sql = "\
            SELECT t.name, SUM(p.rows) AS est_rows \
            FROM sys.tables t \
            JOIN sys.schemas sch ON sch.schema_id = t.schema_id \
            JOIN sys.partitions p \
              ON p.object_id = t.object_id AND p.index_id IN (0, 1) \
            WHERE sch.name = @P1 \
            GROUP BY t.name ORDER BY t.name";
        let mut client = self.client.lock().await;
        let rows = client
            .query(sql, &[&ns.0])
            .await
            .with_context(|| format!("failed to list tables in schema {}", ns.0))?
            .into_first_result()
            .await
            .with_context(|| format!("failed to list tables in schema {}", ns.0))?;

        let mut out: Vec<Collection> = rows
            .iter()
            .filter_map(|r| {
                let name = r.get::<&str, usize>(0)?.to_string();
                let row_est: Option<i64> = r.try_get::<i64, usize>(1).ok().flatten();
                Some(Collection {
                    name,
                    estimated_row_count: row_est.map(|v| v.max(0) as u64),
                    estimated_size_bytes: None,
                })
            })
            .collect();

        // On-disk size is best-effort: the DMV needs VIEW DATABASE STATE, so
        // a permission error degrades to "size unknown" rather than sinking
        // the table list. No index_id filter here — the struct contract is
        // "table + indexes", and heap/clustered alone would under-report
        // tables with large nonclustered indexes.
        let size_sql = "\
            SELECT t.name, SUM(s.used_page_count) * 8192 AS size_bytes \
            FROM sys.tables t \
            JOIN sys.schemas sch ON sch.schema_id = t.schema_id \
            JOIN sys.dm_db_partition_stats s ON s.object_id = t.object_id \
            WHERE sch.name = @P1 \
            GROUP BY t.name";
        let size_res = match client.query(size_sql, &[&ns.0]).await {
            Ok(stream) => stream.into_first_result().await,
            Err(e) => Err(e),
        };
        if let Ok(size_rows) = size_res {
            let sizes: std::collections::HashMap<String, u64> = size_rows
                .iter()
                .filter_map(|r| {
                    let name = r.get::<&str, usize>(0)?.to_string();
                    let size: Option<i64> = r.try_get::<i64, usize>(1).ok().flatten();
                    Some((name, size.unwrap_or(0).max(0) as u64))
                })
                .collect();
            for c in &mut out {
                c.estimated_size_bytes = sizes.get(&c.name).copied();
            }
        }

        Ok(out)
    }

    async fn collection_meta(&self, c: &CollectionRef) -> Result<CollectionMeta> {
        // Bracket both parts: OBJECT_ID parses a raw "dbo.a.b" as a THREE-part
        // name and returns NULL, which would silently yield empty metadata for
        // any table whose name contains a dot (or needs quoting at all).
        let two_part = format!("{}.{}", escape_ident(&c.namespace.0), escape_ident(&c.name));

        // 1. Columns, with the type rendered the way SQL Server does
        //    (nvarchar(50), decimal(18,2), varchar(max)).
        let col_sql = "\
            SELECT col.name, \
                   CASE \
                     WHEN ty.name IN ('nvarchar','nchar') THEN ty.name + '(' + CASE WHEN col.max_length = -1 THEN 'max' ELSE CAST(col.max_length / 2 AS varchar(10)) END + ')' \
                     WHEN ty.name IN ('varchar','char','varbinary','binary') THEN ty.name + '(' + CASE WHEN col.max_length = -1 THEN 'max' ELSE CAST(col.max_length AS varchar(10)) END + ')' \
                     WHEN ty.name IN ('decimal','numeric') THEN ty.name + '(' + CAST(col.precision AS varchar(10)) + ',' + CAST(col.scale AS varchar(10)) + ')' \
                     ELSE ty.name \
                   END AS data_type, \
                   col.is_nullable, \
                   OBJECT_DEFINITION(col.default_object_id) AS default_expr \
            FROM sys.columns col \
            JOIN sys.types ty ON ty.user_type_id = col.user_type_id \
            WHERE col.object_id = OBJECT_ID(@P1) \
            ORDER BY col.column_id";
        let mut client = self.client.lock().await;
        let col_rows = client
            .query(col_sql, &[&two_part])
            .await
            .with_context(|| format!("failed to describe columns for {}", c))?
            .into_first_result()
            .await
            .with_context(|| format!("failed to describe columns for {}", c))?;

        // 2. Primary key columns.
        let pk_sql = "\
            SELECT c2.name \
            FROM sys.key_constraints kc \
            JOIN sys.tables t ON t.object_id = kc.parent_object_id \
            JOIN sys.schemas s ON s.schema_id = t.schema_id \
            JOIN sys.index_columns ic ON ic.object_id = t.object_id AND ic.index_id = kc.unique_index_id \
            JOIN sys.columns c2 ON c2.object_id = t.object_id AND c2.column_id = ic.column_id \
            WHERE kc.type = 'PK' AND s.name = @P1 AND t.name = @P2";
        let pk_rows = match client.query(pk_sql, &[&c.namespace.0, &c.name]).await {
            Ok(s) => s.into_first_result().await.unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        let pk_set: std::collections::HashSet<String> = pk_rows
            .iter()
            .filter_map(|r| r.get::<&str, usize>(0).map(|s| s.to_string()))
            .collect();

        // 3. Unique (non-PK) index columns.
        let uq_sql = "\
            SELECT DISTINCT c2.name \
            FROM sys.indexes i \
            JOIN sys.tables t ON t.object_id = i.object_id \
            JOIN sys.schemas s ON s.schema_id = t.schema_id \
            JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
            JOIN sys.columns c2 ON c2.object_id = i.object_id AND c2.column_id = ic.column_id \
            WHERE i.is_unique = 1 AND i.is_primary_key = 0 \
              AND s.name = @P1 AND t.name = @P2";
        let uq_rows = match client.query(uq_sql, &[&c.namespace.0, &c.name]).await {
            Ok(s) => s.into_first_result().await.unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        let uq_set: std::collections::HashSet<String> = uq_rows
            .iter()
            .filter_map(|r| r.get::<&str, usize>(0).map(|s| s.to_string()))
            .collect();

        let mut columns = Vec::new();
        for r in &col_rows {
            let name: String = r.get::<&str, usize>(0).unwrap_or_default().to_string();
            let data_type: String = r.get::<&str, usize>(1).unwrap_or_default().to_string();
            let is_nullable: bool = r.get::<bool, usize>(2).unwrap_or(true);
            let default_expr: Option<String> =
                r.try_get::<&str, usize>(3).ok().flatten().map(|s| s.to_string());

            columns.push(ColumnMeta {
                is_primary_key: pk_set.contains(&name),
                is_unique: uq_set.contains(&name),
                is_nullable,
                name,
                data_type,
                is_foreign_key: false, // resolved below
                extra: default_expr,
            });
        }

        // 4. Indexes, one row per (index, column) — grouped in Rust so the
        //    query stays free of FOR XML string aggregation.
        let idx_sql = "\
            SELECT i.name, c2.name, i.is_unique, i.is_primary_key \
            FROM sys.indexes i \
            JOIN sys.tables t ON t.object_id = i.object_id \
            JOIN sys.schemas s ON s.schema_id = t.schema_id \
            JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
            JOIN sys.columns c2 ON c2.object_id = i.object_id AND c2.column_id = ic.column_id \
            WHERE s.name = @P1 AND t.name = @P2 \
              AND i.name IS NOT NULL AND i.is_hypothetical = 0 \
            ORDER BY i.name, ic.key_ordinal";
        let idx_rows = match client.query(idx_sql, &[&c.namespace.0, &c.name]).await {
            Ok(s) => s.into_first_result().await.unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        let mut indexes: Vec<IndexMeta> = Vec::new();
        for r in &idx_rows {
            let idx_name: String = r.get::<&str, usize>(0).unwrap_or_default().to_string();
            let col_name: String = r.get::<&str, usize>(1).unwrap_or_default().to_string();
            let is_unique = r.get::<bool, usize>(2).unwrap_or(false);
            let is_primary = r.get::<bool, usize>(3).unwrap_or(false);
            if let Some(ix) = indexes.iter_mut().find(|ix| ix.name == idx_name) {
                ix.columns.push(col_name);
            } else {
                indexes.push(IndexMeta {
                    name: idx_name,
                    columns: vec![col_name],
                    is_unique,
                    is_primary,
                });
            }
        }

        // 5. Foreign keys.
        let fk_sql = "\
            SELECT fk.name, pc.name, rs.name, rt.name, rc.name \
            FROM sys.foreign_keys fk \
            JOIN sys.tables pt ON pt.object_id = fk.parent_object_id \
            JOIN sys.schemas ps ON ps.schema_id = pt.schema_id \
            JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
            JOIN sys.columns pc ON pc.object_id = pt.object_id AND pc.column_id = fkc.parent_column_id \
            JOIN sys.tables rt ON rt.object_id = fk.referenced_object_id \
            JOIN sys.schemas rs ON rs.schema_id = rt.schema_id \
            JOIN sys.columns rc ON rc.object_id = rt.object_id AND rc.column_id = fkc.referenced_column_id \
            WHERE ps.name = @P1 AND pt.name = @P2";
        let fk_rows = match client.query(fk_sql, &[&c.namespace.0, &c.name]).await {
            Ok(s) => s.into_first_result().await.unwrap_or_default(),
            Err(_) => Vec::new(),
        };

        let mut foreign_keys = Vec::new();
        let mut fk_col_names = std::collections::HashSet::new();
        for r in &fk_rows {
            let name: String = r.get::<&str, usize>(0).unwrap_or_default().to_string();
            let col: String = r.get::<&str, usize>(1).unwrap_or_default().to_string();
            let ref_ns: String = r.get::<&str, usize>(2).unwrap_or_default().to_string();
            let ref_tbl: String = r.get::<&str, usize>(3).unwrap_or_default().to_string();
            let ref_col: String = r.get::<&str, usize>(4).unwrap_or_default().to_string();
            fk_col_names.insert(col.clone());
            foreign_keys.push(ForeignKeyMeta {
                name,
                column: col,
                ref_namespace: Namespace(ref_ns),
                ref_table: ref_tbl,
                ref_column: ref_col,
            });
        }
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
        let table = format!("{}.{}", escape_ident(&c.namespace.0), escape_ident(&c.name));

        let count_sql = format!("SELECT COUNT(*) FROM {table}");
        let mut client = self.client.lock().await;
        let count_row = match client.query(count_sql.as_str(), &[]).await {
            Ok(s) => s.into_row().await.ok().flatten(),
            Err(_) => None,
        };
        let total_records: Option<u64> = count_row
            .and_then(|r| {
                r.try_get::<i64, usize>(0)
                    .ok()
                    .flatten()
                    .or_else(|| r.try_get::<i32, usize>(0).ok().flatten().map(|v| v as i64))
            })
            .map(|v| v.max(0) as u64);

        // OFFSET/FETCH needs an ORDER BY in T-SQL; "(SELECT NULL)" is the
        // accepted no-op ordering for "I just want paging". Page numbers are
        // u64 from our own Page struct, so inlining them is injection-free.
        let query = format!(
            "SELECT * FROM {table} ORDER BY (SELECT NULL) OFFSET {} ROWS FETCH NEXT {} ROWS ONLY",
            page.offset, page.limit
        );

        let start = Instant::now();
        let mut stream = client
            .query(query.as_str(), &[])
            .await
            .with_context(|| format!("failed to fetch records for {}", c))?;
        // Column names come from the stream metadata, so an empty page still
        // renders its header row.
        let columns: Vec<String> = stream
            .columns()
            .await
            .ok()
            .flatten()
            .map(|cols| cols.iter().map(|c| c.name().to_string()).collect())
            .unwrap_or_default();
        let rows = stream
            .into_first_result()
            .await
            .with_context(|| format!("failed to fetch records for {}", c))?;
        let _ = start.elapsed();

        let mut records = Vec::new();
        for row in &rows {
            records.push(convert_mssql_row(row)?);
        }

        Ok(RecordPage {
            columns,
            records,
            page: page.offset / page.limit,
            page_size: page.limit,
            total_records,
        })
    }

    async fn execute(&self, _ns: &Namespace, query: &str) -> Result<QueryResult> {
        // When an interactive transaction is open, run on its dedicated
        // connection; otherwise use the main client — and DROP the tx lock
        // first, so begin_tx/in_tx aren't stuck behind an unrelated
        // long-running query. Lock order when both are held stays
        // tx_client → client, so no deadlock.
        let mut tx_guard = self.tx_client.lock().await;
        let mut main_guard;
        let client: &mut MssqlClient = if let Some(c) = tx_guard.as_mut() {
            c
        } else {
            drop(tx_guard);
            main_guard = self.client.lock().await;
            &mut main_guard
        };

        let start = Instant::now();
        let trimmed = query.trim_start();
        let is_select = super::starts_with_keyword(trimmed, "select")
            || super::starts_with_keyword(trimmed, "with");

        if is_select {
            run_select(client, query, start).await
        } else {
            let res = client
                .execute(query, &[])
                .await
                .with_context(|| format!("statement execution failed: {}", query))?;
            let elapsed = start.elapsed();

            Ok(QueryResult {
                columns: Vec::new(),
                records: Vec::new(),
                rows_affected: res.total(),
                execution_time: elapsed,
            })
        }
    }

    async fn begin_tx(&self) -> Result<()> {
        let mut guard = self.tx_client.lock().await;
        if guard.is_some() {
            anyhow::bail!("a transaction is already open");
        }
        let mut conn = open_client(&self.cfg)
            .await
            .context("failed to open a connection for the transaction")?;
        run_utility(&mut conn, "BEGIN TRANSACTION").await?;
        *guard = Some(conn);
        Ok(())
    }

    async fn commit_tx(&self) -> Result<()> {
        let mut conn = self
            .tx_client
            .lock()
            .await
            .take()
            .context("no transaction is open")?;
        run_utility(&mut conn, "COMMIT TRANSACTION").await?;
        Ok(())
    }

    async fn rollback_tx(&self) -> Result<()> {
        let mut conn = self
            .tx_client
            .lock()
            .await
            .take()
            .context("no transaction is open")?;
        run_utility(&mut conn, "ROLLBACK TRANSACTION").await?;
        Ok(())
    }

    async fn in_tx(&self) -> bool {
        self.tx_client.lock().await.is_some()
    }

    async fn definition(&self, c: &CollectionRef) -> Result<String> {
        // SQL Server has no SHOW CREATE TABLE; reconstruct from catalog meta
        // the same way the postgres driver does.
        let meta = self.collection_meta(c).await?;
        let mut ddl = format!(
            "CREATE TABLE {}.{} (\n",
            escape_ident(&c.namespace.0),
            escape_ident(&c.name)
        );

        let mut lines = Vec::new();
        for col in &meta.columns {
            let mut line = format!("    {} {}", escape_ident(&col.name), col.data_type);
            if !col.is_nullable {
                line.push_str(" NOT NULL");
            }
            if let Some(def) = &col.extra {
                line.push_str(&format!(" DEFAULT {def}"));
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
        self.query_collection_names(
            "SELECT v.name FROM sys.views v \
             JOIN sys.schemas s ON s.schema_id = v.schema_id \
             WHERE s.name = @P1 ORDER BY v.name",
            &ns.0,
            "views",
        )
        .await
    }

    async fn list_routines(&self, ns: &Namespace) -> Result<Vec<Collection>> {
        // P = SQL stored proc; FN/IF/TF = scalar/inline/table-valued functions.
        self.query_collection_names(
            "SELECT o.name FROM sys.objects o \
             JOIN sys.schemas s ON s.schema_id = o.schema_id \
             WHERE s.name = @P1 AND o.type IN ('P', 'FN', 'IF', 'TF') \
             ORDER BY o.name",
            &ns.0,
            "routines",
        )
        .await
    }

    async fn list_sequences(&self, ns: &Namespace) -> Result<Vec<Collection>> {
        self.query_collection_names(
            "SELECT sq.name FROM sys.sequences sq \
             JOIN sys.schemas s ON s.schema_id = sq.schema_id \
             WHERE s.name = @P1 ORDER BY sq.name",
            &ns.0,
            "sequences",
        )
        .await
    }

    async fn routine_definition(&self, c: &CollectionRef) -> Result<String> {
        // Bracketed like collection_meta: dots/spaces in routine names must
        // not turn the OBJECT_ID argument into a multi-part parse.
        let two_part = format!("{}.{}", escape_ident(&c.namespace.0), escape_ident(&c.name));
        let mut client = self.client.lock().await;
        let row = client
            .query("SELECT OBJECT_DEFINITION(OBJECT_ID(@P1))", &[&two_part])
            .await
            .context("failed to fetch routine definition")?
            .into_row()
            .await
            .context("failed to fetch routine definition")?;
        row.and_then(|r| r.get::<&str, usize>(0).map(|s| s.to_string()))
            .ok_or_else(|| anyhow!("routine not found: {}", c))
    }

    /// One catalog query for every FK in the schema, instead of the default's
    /// round trip per table.
    async fn schema_foreign_keys(
        &self,
        ns: &Namespace,
    ) -> Result<Vec<(String, ForeignKeyMeta)>> {
        let sql = "\
            SELECT pt.name, fk.name, pc.name, rs.name, rt.name, rc.name \
            FROM sys.foreign_keys fk \
            JOIN sys.tables pt ON pt.object_id = fk.parent_object_id \
            JOIN sys.schemas ps ON ps.schema_id = pt.schema_id \
            JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
            JOIN sys.columns pc ON pc.object_id = pt.object_id AND pc.column_id = fkc.parent_column_id \
            JOIN sys.tables rt ON rt.object_id = fk.referenced_object_id \
            JOIN sys.schemas rs ON rs.schema_id = rt.schema_id \
            JOIN sys.columns rc ON rc.object_id = rt.object_id AND rc.column_id = fkc.referenced_column_id \
            WHERE ps.name = @P1";
        let mut client = self.client.lock().await;
        let rows = client
            .query(sql, &[&ns.0])
            .await
            .with_context(|| format!("failed to list foreign keys in {}", ns.0))?
            .into_first_result()
            .await
            .with_context(|| format!("failed to list foreign keys in {}", ns.0))?;

        Ok(rows
            .iter()
            .map(|r| {
                let table: String = r.get::<&str, usize>(0).unwrap_or_default().to_string();
                (
                    table,
                    ForeignKeyMeta {
                        name: r.get::<&str, usize>(1).unwrap_or_default().to_string(),
                        column: r.get::<&str, usize>(2).unwrap_or_default().to_string(),
                        ref_namespace: Namespace(
                            r.get::<&str, usize>(3).unwrap_or_default().to_string(),
                        ),
                        ref_table: r.get::<&str, usize>(4).unwrap_or_default().to_string(),
                        ref_column: r.get::<&str, usize>(5).unwrap_or_default().to_string(),
                    },
                )
            })
            .collect())
    }

    /// Active requests from the DMVs, excluding this session. `sp_who2`-style
    /// idle connections are noise — only work actually in flight is shown.
    ///
    /// Runs on a dedicated connection like `kill_process`: routed through
    /// `execute` it would queue behind the very runaway query the user opened
    /// the monitor to cancel.
    async fn process_list(&self) -> Result<QueryResult> {
        let sql = "\
            SELECT r.session_id AS spid, s.login_name AS [user], \
                   DB_NAME(s.database_id) AS [database], s.status AS state, \
                   DATEDIFF(second, r.start_time, GETDATE()) AS seconds, \
                   t.text AS query \
            FROM sys.dm_exec_requests r \
            JOIN sys.dm_exec_sessions s ON s.session_id = r.session_id \
            CROSS APPLY sys.dm_exec_sql_text(r.sql_handle) t \
            WHERE r.session_id <> @@SPID \
            ORDER BY r.start_time DESC";
        let mut client = open_client(&self.cfg)
            .await
            .context("failed to open a connection for the process list")?;
        run_select(&mut client, sql, Instant::now()).await
    }

    async fn kill_process(&self, id: &str) -> Result<()> {
        let spid: i32 = id
            .trim()
            .parse()
            .with_context(|| format!("'{id}' is not a SQL Server session id"))?;
        // KILL ends the session, not just the query — SQL Server has no
        // gentler "cancel only" statement, so the UI's own confirmation is
        // the guardrail.
        //
        // Run on a dedicated connection, NOT the main client: the main mutex
        // is held for the whole duration of any running query, so a KILL
        // issued there would queue behind the exact runaway query it is
        // meant to stop (postgres cancels on a pooled connection instead).
        let mut client = open_client(&self.cfg)
            .await
            .context("failed to open a connection for KILL")?;
        run_utility(&mut client, &format!("KILL {spid}"))
            .await
            .with_context(|| format!("failed to kill session {spid}"))
    }
}

/// Converts a dynamic tiberius row into our generic `Record`.
fn convert_mssql_row(row: &Row) -> Result<Record> {
    let mut values = Vec::with_capacity(row.len());

    for (i, col) in row.columns().iter().enumerate() {
        let val = match col.column_type() {
            ColumnType::Bit | ColumnType::Bitn => {
                opt(row.try_get::<bool, _>(i), Value::Bool)
            }
            ColumnType::Int1 => opt(row.try_get::<u8, _>(i), |v| Value::Int(v as i64)),
            ColumnType::Int2 => opt(row.try_get::<i16, _>(i), |v| Value::Int(v as i64)),
            ColumnType::Int4 => opt(row.try_get::<i32, _>(i), |v| Value::Int(v as i64)),
            ColumnType::Int8 => opt(row.try_get::<i64, _>(i), Value::Int),
            // Intn carries 1/2/4/8-byte ints on the wire; try widest first.
            ColumnType::Intn => {
                if let Some(v) = row.try_get::<i64, _>(i).ok().flatten() {
                    Value::Int(v)
                } else if let Some(v) = row.try_get::<i32, _>(i).ok().flatten() {
                    Value::Int(v as i64)
                } else if let Some(v) = row.try_get::<i16, _>(i).ok().flatten() {
                    Value::Int(v as i64)
                } else {
                    opt(row.try_get::<u8, _>(i), |v| Value::Int(v as i64))
                }
            }
            ColumnType::Float4 => opt(row.try_get::<f32, _>(i), |v| Value::Float(v as f64)),
            ColumnType::Float8 => opt(row.try_get::<f64, _>(i), Value::Float),
            ColumnType::Floatn => {
                if let Some(v) = row.try_get::<f64, _>(i).ok().flatten() {
                    Value::Float(v)
                } else {
                    opt(row.try_get::<f32, _>(i), |v| Value::Float(v as f64))
                }
            }
            ColumnType::Money | ColumnType::Money4 => {
                // tiberius decodes BOTH money types to ColumnData::F64
                // (smallmoney is i32/1e4 widened), so an f32 read never
                // matches and would silently surface smallmoney as NULL.
                opt(row.try_get::<f64, _>(i), money_to_value)
            }
            ColumnType::Decimaln | ColumnType::Numericn => opt(
                row.try_get::<tiberius::numeric::Numeric, _>(i),
                |n| Value::Decimal(format_numeric(n)),
            ),
            ColumnType::Guid => opt(row.try_get::<tiberius::Uuid, _>(i), |u| {
                Value::String(u.to_string())
            }),
            ColumnType::Datetime
            | ColumnType::Datetime4
            | ColumnType::Datetimen
            | ColumnType::Datetime2 => opt(
                row.try_get::<sqlx::types::chrono::NaiveDateTime, _>(i),
                |dt| Value::DateTime(dt.to_string()),
            ),
            ColumnType::Daten => opt(row.try_get::<sqlx::types::chrono::NaiveDate, _>(i), |d| {
                Value::DateTime(d.to_string())
            }),
            ColumnType::Timen => opt(row.try_get::<sqlx::types::chrono::NaiveTime, _>(i), |t| {
                Value::DateTime(t.to_string())
            }),
            ColumnType::DatetimeOffsetn => opt(
                // FixedOffset keeps the stored offset — decoding to Utc would
                // silently shift the displayed wall-clock time.
                row.try_get::<sqlx::types::chrono::DateTime<sqlx::types::chrono::FixedOffset>, _>(i),
                |dt| Value::DateTime(dt.to_rfc3339()),
            ),
            ColumnType::BigVarBin | ColumnType::BigBinary | ColumnType::Image => {
                opt(row.try_get::<&[u8], _>(i), |b| Value::Bytes(b.to_vec()))
            }
            ColumnType::Xml => opt(
                row.try_get::<&tiberius::xml::XmlData, _>(i),
                |x| Value::String(x.to_string()),
            ),
            _ => {
                // Textual types (varchar/nvarchar/char/nchar/text/ntext) and
                // anything else that can decode to &str.
                if let Ok(Some(s)) = row.try_get::<&str, _>(i) {
                    Value::String(s.to_string())
                } else {
                    Value::String(format!("<mssql:{:?}>", col.column_type()))
                }
            }
        };
        values.push(val);
    }

    Ok(Record { values })
}

/// Decodes an optional tiberius value: `Ok(Some(v))` maps through `f`,
/// SQL NULL (`Ok(None)`) and decode mismatches become `Value::Null` — the
/// same fallback policy the sqlx drivers use.
fn opt<T>(r: tiberius::Result<Option<T>>, f: impl FnOnce(T) -> Value) -> Value {
    match r {
        Ok(Some(v)) => f(v),
        _ => Value::Null,
    }
}

/// Formats a decimal/numeric exactly, sign included. tiberius' own Display
/// delegates to a Debug impl that writes `{int_part}.{dec_part}` — and BOTH
/// parts are negative for negative values, so `-12.34` would render as
/// `-12.-34`. Format from the signed scaled integer instead.
fn format_numeric(n: tiberius::numeric::Numeric) -> String {
    let scale = n.scale() as usize;
    let value = n.value();
    let negative = value < 0;
    let digits = value.unsigned_abs().to_string();
    let body = if scale == 0 {
        digits
    } else if digits.len() <= scale {
        // |value| < 1: zero-pad the fraction ("5" at scale 2 → "0.05").
        format!("0.{:0>width$}", digits, width = scale)
    } else {
        let (int_part, dec_part) = digits.split_at(digits.len() - scale);
        format!("{int_part}.{dec_part}")
    };
    if negative {
        format!("-{body}")
    } else {
        body
    }
}

/// money/smallmoney are fixed-point with scale 4, but tiberius decodes them
/// to f64 (dividing by 1e4). Re-rendering with exactly 4 decimals recovers
/// the intended decimal text for any value within f64's ~15 significant
/// decimal digits (|v| < ~9e11 at scale 4); beyond that the precision was
/// already lost at decode time and can't be recovered. At least 2 decimals
/// are kept so values still read as currency.
fn money_to_value(v: f64) -> Value {
    let s = format!("{v:.4}");
    let (int_part, frac) = s.split_once('.').unwrap_or((s.as_str(), ""));
    let frac = frac.trim_end_matches('0');
    if frac.len() <= 2 {
        Value::Decimal(format!("{int_part}.{frac:0<2}"))
    } else {
        Value::Decimal(format!("{int_part}.{frac}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escape_ident() {
        assert_eq!(escape_ident("users"), "[users]");
        assert_eq!(escape_ident("user]table"), "[user]]table]");
    }

    #[test]
    fn test_format_numeric_keeps_the_sign() {
        use tiberius::numeric::Numeric;
        assert_eq!(format_numeric(Numeric::new_with_scale(1234, 2)), "12.34");
        assert_eq!(format_numeric(Numeric::new_with_scale(-1234, 2)), "-12.34");
        assert_eq!(format_numeric(Numeric::new_with_scale(-5, 1)), "-0.5");
        assert_eq!(format_numeric(Numeric::new_with_scale(5, 2)), "0.05");
        assert_eq!(format_numeric(Numeric::new_with_scale(-42, 0)), "-42");
        assert_eq!(format_numeric(Numeric::new_with_scale(0, 4)), "0.0000");
    }

    #[test]
    fn test_money_to_value_recovers_the_decimal_text() {
        assert_eq!(money_to_value(12.34), Value::Decimal("12.34".to_string()));
        assert_eq!(money_to_value(100.0), Value::Decimal("100.00".to_string()));
        assert_eq!(money_to_value(0.5), Value::Decimal("0.50".to_string()));
        assert_eq!(
            money_to_value(-1234.5678),
            Value::Decimal("-1234.5678".to_string())
        );
        assert_eq!(money_to_value(0.0001), Value::Decimal("0.0001".to_string()));
    }
}
