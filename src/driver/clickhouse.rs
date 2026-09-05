//! ClickHouse driver over the HTTP interface (port 8123, or 8443 for TLS).
//!
//! ClickHouse has no native async client in our dependency tree, so every
//! call is a plain HTTP POST through `ureq` (blocking) wrapped in
//! `tokio::task::spawn_blocking`. Responses are requested in `FORMAT JSON`
//! via the `default_format` URL parameter — chosen over `JSONEachRow`
//! because the `meta` array carries column names *and* ClickHouse type
//! strings even for empty result sets, which drives the same type-directed
//! value mapping the sqlx drivers do.
//!
//! Metadata comes from the `system` database (`system.databases`,
//! `system.tables`, `system.columns`) rather than an information schema.
//!
//! Capabilities are read/query-focused: ClickHouse has no foreign keys (no
//! ERD) and no row-level UPDATE/DELETE semantics the generic editor could
//! target (no EDIT_DATA).

use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;

use super::{
    Capabilities, Collection, CollectionMeta, CollectionRef, ColumnMeta, Driver, DriverInfo,
    Namespace, Page, QueryResult, Record, RecordPage, Value,
};
use crate::config::{ConnectionConfig, SslMode};

/// ClickHouse's plain HTTP port. 8443 is the conventional HTTPS port; the
/// scheme is inferred from `ssl`/`ssl_mode`, falling back to TLS when the
/// port is 8443 and nothing explicit is configured.
const DEFAULT_PORT: u16 = 8123;
const TLS_PORT: u16 = 8443;

/// The built-in account ClickHouse ships with; used when the config leaves
/// `user` empty (mirrors the CLI's own default).
const DEFAULT_USER: &str = "default";

/// Quote an identifier ClickHouse-style (backticks, inner backtick doubled).
fn escape_ident(ident: &str) -> String {
    format!("`{}`", ident.replace('`', "``"))
}

/// Escape a string *literal*. ClickHouse uses backslash escapes in strings,
/// so a literal backslash must be doubled before quoting quotes.
fn escape_literal(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// The FORMAT JSON wire shape: `meta` (name+type per column) plus `data`.
#[derive(serde::Deserialize)]
struct JsonResponse {
    meta: Vec<JsonColumn>,
    data: Vec<serde_json::Map<String, serde_json::Value>>,
}

#[derive(serde::Deserialize)]
struct JsonColumn {
    name: String,
    #[serde(rename = "type")]
    ty: String,
}

/// Strip `Nullable(...)` / `LowCardinality(...)` wrappers, returning the base
/// type and whether the column can hold NULL.
fn unwrap_type(ty: &str) -> (&str, bool) {
    let mut t = ty;
    let mut nullable = false;
    loop {
        if let Some(inner) = t.strip_prefix("Nullable(").and_then(|s| s.strip_suffix(')')) {
            nullable = true;
            t = inner;
        } else if let Some(inner) =
            t.strip_prefix("LowCardinality(").and_then(|s| s.strip_suffix(')'))
        {
            t = inner;
        } else {
            return (t, nullable);
        }
    }
}

/// Map one JSON cell to a `Value`, directed by the ClickHouse type string —
/// the same role the sqlx `TypeInfo` lookup plays in the other drivers.
/// Int128/256, Decimal and Date/DateTime arrive as JSON strings/numbers that
/// need the declared type to interpret correctly.
fn json_to_value(ty: &str, v: &serde_json::Value) -> Value {
    if v.is_null() {
        return Value::Null;
    }
    let (base, _) = unwrap_type(ty);

    let num_str = |v: &serde_json::Value| -> String {
        match v {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        }
    };

    if base.starts_with("UInt") {
        // UInt64 arrives as an unquoted JSON number (we disable 64-bit
        // quoting); UInt128/256 are always strings.
        if let Some(u) = v.as_u64() {
            Value::UInt(u)
        } else if let Some(s) = v.as_str().and_then(|s| s.parse::<u64>().ok()) {
            Value::UInt(s)
        } else {
            Value::String(num_str(v))
        }
    } else if base.starts_with('I') && base.starts_with("Int") {
        if let Some(i) = v.as_i64() {
            Value::Int(i)
        } else if let Some(i) = v.as_str().and_then(|s| s.parse::<i64>().ok()) {
            Value::Int(i)
        } else {
            Value::String(num_str(v))
        }
    } else if base.starts_with("Float") {
        v.as_f64()
            .map(Value::Float)
            .unwrap_or_else(|| Value::String(num_str(v)))
    } else if base.starts_with("Decimal") {
        // Exact-precision decimal: keep the server's textual form.
        Value::Decimal(num_str(v))
    } else if base == "Bool" {
        // Newer servers emit JSON booleans; older ones emit 0/1.
        match v {
            serde_json::Value::Bool(b) => Value::Bool(*b),
            serde_json::Value::Number(n) => Value::Bool(n.as_i64().unwrap_or(0) != 0),
            serde_json::Value::String(s) => Value::Bool(s == "true" || s == "1"),
            _ => Value::Null,
        }
    } else if base.starts_with("Date") || base == "Time" || base.starts_with("Time64") {
        Value::DateTime(num_str(v))
    } else if base.starts_with("UUID")
        || base.starts_with("String")
        || base.starts_with("FixedString")
        || base.starts_with("Enum")
        || base.starts_with("IPv")
    {
        Value::String(num_str(v))
    } else if matches!(v, serde_json::Value::Array(_) | serde_json::Value::Object(_)) {
        // Array / Tuple / Map / Nested / JSON columns: keep the structure.
        Value::Json(v.clone())
    } else {
        // Unknown type: fall back to the JSON value's own shape.
        match v {
            serde_json::Value::Bool(b) => Value::Bool(*b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Value::Int(i)
                } else if let Some(u) = n.as_u64() {
                    Value::UInt(u)
                } else {
                    n.as_f64().map(Value::Float).unwrap_or(Value::Null)
                }
            }
            serde_json::Value::String(s) => Value::String(s.clone()),
            other => Value::Json(other.clone()),
        }
    }
}

/// Parse a `FORMAT JSON` body into column names + records. Column order comes
/// from `meta`, so records line up with `columns` even when the server
/// reorders keys inside each row object.
fn parse_json_response(body: &str) -> Result<(Vec<String>, Vec<Record>)> {
    let parsed: JsonResponse =
        serde_json::from_str(body).context("failed to parse ClickHouse JSON response")?;
    let names: Vec<String> = parsed.meta.iter().map(|c| c.name.clone()).collect();
    let types: Vec<String> = parsed.meta.iter().map(|c| c.ty.clone()).collect();
    let records = parsed
        .data
        .iter()
        .map(|row| Record {
            values: parsed
                .meta
                .iter()
                .enumerate()
                .map(|(i, col)| {
                    row.get(&col.name)
                        .map(|v| json_to_value(&types[i], v))
                        .unwrap_or(Value::Null)
                })
                .collect(),
        })
        .collect();
    Ok((names, records))
}

/// Does this query produce rows we can parse back? Only these get the JSON
/// treatment; everything else is sent raw (INSERT/ALTER/CREATE/...).
fn is_row_returning(query: &str) -> bool {
    let trimmed = query.trim_start();
    ["select", "show", "describe", "desc", "explain", "exists", "with"]
        .iter()
        .any(|kw| super::starts_with_keyword(trimmed, kw))
}

pub struct ClickHouseDriver {
    agent: ureq::Agent,
    /// `http(s)://host:port` — already resolved, no trailing slash.
    base_url: String,
    user: String,
    password: Option<String>,
    database: Option<String>,
    info: DriverInfo,
}

impl ClickHouseDriver {
    pub async fn connect(cfg: &ConnectionConfig) -> Result<Self> {
        let port = cfg.port.unwrap_or(DEFAULT_PORT);
        // TLS when explicitly required/verified; when unset, the 8443
        // convention decides. ureq always verifies certificates when TLS is
        // on (rustls + webpki roots), so Require and Verify behave alike.
        let use_tls = match cfg.effective_ssl_mode() {
            Some(SslMode::Disable) => false,
            Some(_) => true,
            None => port == TLS_PORT,
        };
        let scheme = if use_tls { "https" } else { "http" };
        let base_url = format!("{scheme}://{}:{port}", cfg.host);

        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            // No overall timeout: analytics queries legitimately run for
            // minutes and ureq's `timeout` would abort them mid-flight.
            .build();

        let mut driver = Self {
            agent,
            base_url,
            user: cfg
                .user
                .clone()
                .filter(|u| !u.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_USER.to_string()),
            password: cfg.resolve_password(),
            database: cfg.database.clone(),
            info: DriverInfo {
                name: "ClickHouse".to_string(),
                server_version: "unknown".to_string(),
                query_language: "SQL".to_string(),
            },
        };

        // Fail fast on connect, like the sqlx drivers' pool connect does.
        driver
            .ping()
            .await
            .with_context(|| format!("failed to connect to ClickHouse ({})", cfg.display_url()))?;

        let version = driver
            .run_json("SELECT version() AS v", None)
            .await
            .ok()
            .and_then(|(_, records)| records.into_iter().next())
            .and_then(|r| r.values.into_iter().next())
            .map(|v| v.display_str())
            .unwrap_or_else(|| "unknown".to_string());
        driver.info.server_version = version;

        Ok(driver)
    }

    /// The URL for one request, with per-request settings as query params.
    /// `default_format=JSON` applies only when the statement has no FORMAT
    /// clause of its own, so a user-specified format still wins.
    fn url(&self, database: Option<&str>) -> String {
        let db = database.or(self.database.as_deref());
        let mut url = format!(
            "{}/?default_format=JSON&output_format_json_quote_64bit_integers=0\
             &output_format_json_quote_denormals=0",
            self.base_url
        );
        if let Some(db) = db.filter(|d| !d.trim().is_empty()) {
            // Percent-encode the one character class that matters in a
            // database identifier (and never appears in practice anyway).
            url.push_str("&database=");
            for b in db.bytes() {
                match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                        url.push(b as char)
                    }
                    other => url.push_str(&format!("%{other:02X}")),
                }
            }
        }
        url
    }

    /// Blocking HTTP POST of `sql`; returns the response body. Runs inside
    /// `spawn_blocking` because ureq is synchronous.
    fn http_post(
        agent: ureq::Agent,
        url: String,
        user: String,
        password: Option<String>,
        sql: String,
    ) -> Result<String> {
        let mut req = agent
            .post(&url)
            .set("X-ClickHouse-User", &user)
            // Empty key header is what ClickHouse expects for a passwordless
            // account; omitting it entirely reads as "no credentials".
            .set("X-ClickHouse-Key", password.as_deref().unwrap_or(""));
        req = req.set("Content-Type", "text/plain; charset=utf-8");
        match req.send_string(&sql) {
            Ok(resp) => resp
                .into_string()
                .context("failed to read ClickHouse response body"),
            // ClickHouse reports query errors as non-2xx with a plain-text
            // body ("Code: 62. DB::Exception: ...") — surface it verbatim.
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                Err(anyhow!("ClickHouse HTTP {code}: {}", body.trim()))
            }
            Err(e) => Err(anyhow!("ClickHouse request failed: {e}")),
        }
    }

    /// POST `sql` and parse the FORMAT JSON response into columns + records.
    async fn run_json(&self, sql: &str, database: Option<&str>) -> Result<(Vec<String>, Vec<Record>)> {
        let agent = self.agent.clone();
        let url = self.url(database);
        let user = self.user.clone();
        let password = self.password.clone();
        let sql = sql.to_string();
        let body = tokio::task::spawn_blocking(move || Self::http_post(agent, url, user, password, sql))
            .await
            .context("ClickHouse HTTP task panicked")??;
        parse_json_response(&body)
    }

    /// POST `sql` discarding the body (DDL / INSERT / KILL).
    async fn run_statement(&self, sql: &str, database: Option<&str>) -> Result<()> {
        let agent = self.agent.clone();
        let url = self.url(database);
        let user = self.user.clone();
        let password = self.password.clone();
        let sql = sql.to_string();
        tokio::task::spawn_blocking(move || Self::http_post(agent, url, user, password, sql))
            .await
            .context("ClickHouse HTTP task panicked")??;
        Ok(())
    }

    /// `system.tables` rows for one database. ClickHouse stores views as
    /// tables with a *View engine, so the same query serves both
    /// `collections` and `list_views`.
    async fn tables_of_kind(&self, ns: &Namespace, want_views: bool) -> Result<Vec<Collection>> {
        let sql = format!(
            "SELECT name, total_rows, total_bytes, engine \
             FROM system.tables WHERE database = {} ORDER BY name",
            escape_literal(&ns.0)
        );
        let (_, records) = self.run_json(&sql, None).await?;
        let view_engines = ["View", "MaterializedView", "LiveView", "WindowView"];
        Ok(records
            .into_iter()
            .filter(|r| {
                let engine = r.values.get(3).map(|v| v.display_str()).unwrap_or_default();
                view_engines.contains(&engine.as_str()) == want_views
            })
            .map(|r| {
                let to_u64 = |i: usize| match r.values.get(i) {
                    Some(Value::UInt(u)) => Some(*u),
                    Some(Value::Int(i)) if *i >= 0 => Some(*i as u64),
                    _ => None,
                };
                Collection {
                    name: r.values[0].display_str(),
                    estimated_row_count: to_u64(1),
                    estimated_size_bytes: to_u64(2),
                }
            })
            .collect())
    }
}

#[async_trait]
impl Driver for ClickHouseDriver {
    fn info(&self) -> DriverInfo {
        self.info.clone()
    }

    fn capabilities(&self) -> Capabilities {
        // No ERD (ClickHouse has no foreign keys) and no EDIT_DATA (no
        // row-level UPDATE/DELETE; mutations are ALTER ... UPDATE batch ops
        // the generic editor cannot express safely).
        Capabilities::BROWSE
            | Capabilities::QUERY_TEXT
            | Capabilities::DDL
            | Capabilities::EXPLAIN
            | Capabilities::PROCESS_LIST
    }

    async fn ping(&self) -> Result<Duration> {
        let start = Instant::now();
        let agent = self.agent.clone();
        let url = format!("{}/ping", self.base_url);
        tokio::task::spawn_blocking(move || -> Result<()> {
            match agent.get(&url).call() {
                Ok(resp) => {
                    let _ = resp.into_string();
                    Ok(())
                }
                Err(ureq::Error::Status(code, resp)) => {
                    let body = resp.into_string().unwrap_or_default();
                    Err(anyhow!("ClickHouse ping failed (HTTP {code}): {}", body.trim()))
                }
                Err(e) => Err(anyhow!("ClickHouse ping failed: {e}")),
            }
        })
        .await
        .context("ClickHouse ping task panicked")??;
        Ok(start.elapsed())
    }

    async fn namespaces(&self) -> Result<Vec<Namespace>> {
        let (_, records) = self
            .run_json("SELECT name FROM system.databases ORDER BY name", None)
            .await
            .context("failed to list ClickHouse databases")?;
        Ok(records
            .into_iter()
            .map(|r| Namespace(r.values[0].display_str()))
            .collect())
    }

    async fn collections(&self, ns: &Namespace) -> Result<Vec<Collection>> {
        self.tables_of_kind(ns, false).await
    }

    async fn list_views(&self, ns: &Namespace) -> Result<Vec<Collection>> {
        self.tables_of_kind(ns, true).await
    }

    async fn collection_meta(&self, c: &CollectionRef) -> Result<CollectionMeta> {
        let col_sql = format!(
            "SELECT name, type, default_kind, default_expression \
             FROM system.columns WHERE database = {} AND table = {} ORDER BY position",
            escape_literal(&c.namespace.0),
            escape_literal(&c.name)
        );
        let (_, col_rows) = self
            .run_json(&col_sql, None)
            .await
            .with_context(|| format!("failed to fetch columns for {}", c))?;
        if col_rows.is_empty() {
            return Err(anyhow!("table '{}' not found in {}", c.name, c.namespace));
        }

        // The ordering/sorting key lives on the table, as a comma-separated
        // expression list; a bare column name in it is flagged as the PK.
        let pk_sql = format!(
            "SELECT primary_key FROM system.tables WHERE database = {} AND table = {}",
            escape_literal(&c.namespace.0),
            escape_literal(&c.name)
        );
        let pk_expr = self
            .run_json(&pk_sql, None)
            .await
            .ok()
            .and_then(|(_, r)| r.into_iter().next())
            .and_then(|r| r.values.into_iter().next())
            .map(|v| v.display_str())
            .unwrap_or_default();
        let pk_cols: Vec<String> = pk_expr
            .split(',')
            .map(|p| p.trim().trim_matches('`').to_string())
            .filter(|p| !p.is_empty())
            .collect();

        let columns = col_rows
            .into_iter()
            .map(|r| {
                let name = r.values[0].display_str();
                let ty = r.values[1].display_str();
                let (_, nullable) = unwrap_type(&ty);
                let default_kind = r.values.get(2).map(|v| v.display_str()).unwrap_or_default();
                let default_expr = r.values.get(3).map(|v| v.display_str()).unwrap_or_default();
                let extra = if default_kind.is_empty() {
                    None
                } else {
                    Some(format!("{default_kind} {default_expr}"))
                };
                ColumnMeta {
                    is_primary_key: pk_cols.iter().any(|p| *p == name),
                    name,
                    data_type: ty,
                    is_nullable: nullable,
                    // A MergeTree sorting key is not a uniqueness constraint.
                    is_unique: false,
                    is_foreign_key: false,
                    extra,
                }
            })
            .collect();

        Ok(CollectionMeta {
            reference: c.clone(),
            columns,
            // No secondary indexes / FKs to surface (data-skipping indexes
            // exist but have no row-level meaning for the UI).
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        })
    }

    async fn records(&self, c: &CollectionRef, page: Page) -> Result<RecordPage> {
        let table = format!("{}.{}", escape_ident(&c.namespace.0), escape_ident(&c.name));

        // count() on MergeTree is served from metadata — cheap enough to run
        // per page, unlike the OLTP drivers' full COUNT(*).
        let total_records = self
            .run_json(&format!("SELECT count() AS n FROM {table}"), None)
            .await
            .ok()
            .and_then(|(_, r)| r.into_iter().next())
            .and_then(|r| r.values.into_iter().next())
            .and_then(|v| match v {
                Value::UInt(u) => Some(u),
                Value::Int(i) if i >= 0 => Some(i as u64),
                _ => None,
            });

        let (columns, records) = self
            .run_json(
                &format!(
                    "SELECT * FROM {table} LIMIT {} OFFSET {}",
                    page.limit, page.offset
                ),
                None,
            )
            .await
            .with_context(|| format!("failed to fetch records for {}", c))?;

        Ok(RecordPage {
            columns,
            records,
            page: page.offset.checked_div(page.limit).unwrap_or(0),
            page_size: page.limit,
            total_records,
        })
    }

    async fn execute(&self, ns: &Namespace, query: &str) -> Result<QueryResult> {
        let start = Instant::now();

        if is_row_returning(query) {
            let agent = self.agent.clone();
            let url = self.url(Some(&ns.0));
            let user = self.user.clone();
            let password = self.password.clone();
            let sql = query.to_string();
            let body =
                tokio::task::spawn_blocking(move || Self::http_post(agent, url, user, password, sql))
                    .await
                    .context("ClickHouse HTTP task panicked")??;
            let elapsed = start.elapsed();

            match parse_json_response(&body) {
                Ok((columns, records)) => {
                    let count = records.len() as u64;
                    Ok(QueryResult {
                        columns,
                        records,
                        rows_affected: count,
                        execution_time: elapsed,
                    })
                }
                // The query carried its own FORMAT clause (default_format
                // yields to it) — the body is TSV/CSV/whatever; show it raw.
                Err(_) => Ok(QueryResult {
                    columns: vec!["result".to_string()],
                    records: body
                        .lines()
                        .filter(|l| !l.is_empty())
                        .map(|l| Record {
                            values: vec![Value::String(l.to_string())],
                        })
                        .collect(),
                    rows_affected: 0,
                    execution_time: elapsed,
                }),
            }
        } else {
            // INSERT / ALTER / CREATE / DROP / ... : the HTTP interface
            // reports success with an empty body, so there is no affected-row
            // count to return.
            self.run_statement(query, Some(&ns.0)).await?;
            Ok(QueryResult {
                columns: Vec::new(),
                records: Vec::new(),
                rows_affected: 0,
                execution_time: start.elapsed(),
            })
        }
    }

    /// ClickHouse keeps the original CREATE text in system.tables.
    async fn definition(&self, c: &CollectionRef) -> Result<String> {
        let sql = format!(
            "SHOW CREATE TABLE {}.{}",
            escape_ident(&c.namespace.0),
            escape_ident(&c.name)
        );
        let (_, records) = self
            .run_json(&sql, None)
            .await
            .with_context(|| format!("failed to fetch DDL for {}", c))?;
        records
            .into_iter()
            .next()
            .and_then(|r| r.values.into_iter().next())
            .map(|v| v.display_str())
            .ok_or_else(|| anyhow!("no DDL recorded for '{}'", c.name))
    }

    /// Running queries, longest-running first — the view an analyst actually
    /// wants when something is stuck.
    async fn process_list(&self) -> Result<QueryResult> {
        let start = Instant::now();
        let (columns, records) = self
            .run_json(
                "SELECT query_id, user, elapsed, read_rows, memory_usage, query \
                 FROM system.processes ORDER BY elapsed DESC",
                None,
            )
            .await
            .context("failed to list ClickHouse processes")?;
        Ok(QueryResult {
            columns,
            records,
            rows_affected: 0,
            execution_time: start.elapsed(),
        })
    }

    async fn kill_process(&self, id: &str) -> Result<()> {
        // KILL QUERY is asynchronous server-side: it marks the query for
        // cancellation and returns immediately.
        self.run_statement(
            &format!("KILL QUERY WHERE query_id = {}", escape_literal(id)),
            None,
        )
        .await
    }

    // Interactive transactions, routines and sequences keep the trait
    // defaults: ClickHouse has none of them over HTTP.
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a canned FORMAT JSON body the way a real server would send it.
    fn parse(body: &str) -> (Vec<String>, Vec<Record>) {
        parse_json_response(body).expect("canned response must parse")
    }

    #[test]
    fn test_parse_json_response_maps_types() {
        // 64-bit ints unquoted (we ask for that), Decimal as a string,
        // Nullable as null, Date/DateTime as strings, Array as JSON.
        let body = r#"{
            "meta": [
                {"name": "id", "type": "UInt64"},
                {"name": "delta", "type": "Int32"},
                {"name": "price", "type": "Decimal(10, 2)"},
                {"name": "note", "type": "Nullable(String)"},
                {"name": "day", "type": "Date"},
                {"name": "ts", "type": "DateTime64(3)"},
                {"name": "tags", "type": "Array(String)"},
                {"name": "active", "type": "Bool"},
                {"name": "uuid", "type": "UUID"}
            ],
            "data": [
                {"id": 18446744073709551615, "delta": -7, "price": "19.99",
                 "note": null, "day": "2024-05-01", "ts": "2024-05-01 10:20:30.123",
                 "tags": ["a", "b"], "active": true,
                 "uuid": "61f0c404-5cb3-11e7-907b-a6006ad3dba0"}
            ],
            "rows": 1
        }"#;
        let (cols, records) = parse(body);
        assert_eq!(cols.len(), 9);
        let v = &records[0].values;
        assert_eq!(v[0], Value::UInt(u64::MAX), "UInt64 must stay exact");
        assert_eq!(v[1], Value::Int(-7));
        assert_eq!(v[2], Value::Decimal("19.99".to_string()));
        assert_eq!(v[3], Value::Null);
        assert_eq!(v[4], Value::DateTime("2024-05-01".to_string()));
        assert_eq!(v[5], Value::DateTime("2024-05-01 10:20:30.123".to_string()));
        assert_eq!(
            v[6],
            Value::Json(serde_json::json!(["a", "b"])),
            "arrays keep their structure"
        );
        assert_eq!(v[7], Value::Bool(true));
        assert_eq!(
            v[8],
            Value::String("61f0c404-5cb3-11e7-907b-a6006ad3dba0".to_string())
        );
    }

    #[test]
    fn test_parse_empty_result_keeps_columns() {
        // The reason FORMAT JSON was chosen over JSONEachRow: with zero rows
        // the column headers still arrive via `meta`.
        let body = r#"{
            "meta": [{"name": "n", "type": "UInt8"}, {"name": "s", "type": "String"}],
            "data": [],
            "rows": 0
        }"#;
        let (cols, records) = parse(body);
        assert_eq!(cols, vec!["n".to_string(), "s".to_string()]);
        assert!(records.is_empty());
    }

    #[test]
    fn test_type_mapping_edges() {
        let s = |v: &str| serde_json::Value::String(v.to_string());
        // Int128/UInt256 are always quoted by the server; they fit into i64/u64
        // when small and fall back to string when not.
        assert_eq!(json_to_value("Int128", &s("-5")), Value::Int(-5));
        let big = "340282366920938463463374607431768211455";
        assert_eq!(
            json_to_value("UInt256", &s(big)),
            Value::String(big.to_string())
        );
        // LowCardinality(Nullable(...)) unwraps to the base type.
        assert_eq!(
            json_to_value("LowCardinality(Nullable(String))", &s("x")),
            Value::String("x".to_string())
        );
        assert_eq!(
            json_to_value("Nullable(UInt32)", &serde_json::Value::Null),
            Value::Null
        );
        // Old servers emit Bool as 0/1.
        assert_eq!(
            json_to_value("Bool", &serde_json::json!(0)),
            Value::Bool(false)
        );
        // Decimal arriving as a JSON number keeps its textual form.
        assert_eq!(
            json_to_value("Decimal(9,4)", &serde_json::json!(1.5)),
            Value::Decimal("1.5".to_string())
        );
        // Enum renders as its label string.
        assert_eq!(
            json_to_value("Enum8('a' = 1)", &s("a")),
            Value::String("a".to_string())
        );
    }

    #[test]
    fn test_unwrap_type() {
        assert_eq!(unwrap_type("String"), ("String", false));
        assert_eq!(unwrap_type("Nullable(Int32)"), ("Int32", true));
        assert_eq!(
            unwrap_type("LowCardinality(Nullable(String))"),
            ("String", true)
        );
    }

    #[test]
    fn test_row_returning_detection() {
        for q in ["SELECT 1", "  show tables", "EXPLAIN PLAN SELECT 1", "WITH x AS (SELECT 1) SELECT * FROM x", "desc t"] {
            assert!(is_row_returning(q), "{q}");
        }
        for q in ["INSERT INTO t VALUES (1)", "ALTER TABLE t DELETE WHERE 1", "CREATE TABLE t (x Int32)", "KILL QUERY WHERE 1", ""] {
            assert!(!is_row_returning(q), "{q}");
        }
        // Unicode before the keyword must not panic (see starts_with_keyword).
        assert!(!is_row_returning("éSELECT"));
    }

    #[test]
    fn test_escaping() {
        assert_eq!(escape_ident("order"), "`order`");
        assert_eq!(escape_ident("a`b"), "`a``b`");
        assert_eq!(escape_literal("o'clock"), "'o\\'clock'");
        assert_eq!(escape_literal("a\\b"), "'a\\\\b'");
    }
}
