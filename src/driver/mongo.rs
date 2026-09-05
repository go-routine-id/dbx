//! MongoDB driver implementation using the official `mongodb` crate.
//!
//! MongoDB is **schemaless**: collections have no declared columns, and two
//! documents in the same collection may share no fields at all. The driver
//! therefore *synthesizes* a schema by sampling documents (see
//! [`SCHEMA_SAMPLE_LIMIT`]) and taking the union of their top-level fields.
//! The synthesized schema is a browsing hint, not a guarantee — `records`
//! still unions in fields that appear in the page but not in the sample, so
//! no data is silently dropped from the grid.
//!
//! No Mutex around the client: the `Driver` trait takes `&self`, and
//! `mongodb::Client` is cloneable and internally pooled, so it is shared
//! directly.
//!
//! Console query language (v1, read-only) — a single JSON object:
//!
//! ```json
//! { "collection": "users", "find":     { "filter": {...}, "sort": {...},
//!                                        "projection": {...}, "limit": 50, "skip": 0 } }
//! { "collection": "users", "aggregate": [ { "$group": ... }, ... ] }
//! ```
//!
//! Inside filters/sorts, plain JSON values map to their BSON counterparts;
//! two extended conveniences are recognised anywhere in the payload:
//! `{ "$oid": "<24-hex>" }` becomes an ObjectId and `{ "$date": "<rfc3339>"
//! }` (or epoch millis) becomes a BSON date. Everything else (insert /
//! update / delete / ...) is rejected with a usage error — see
//! [`parse_console_command`].
//!
//! TLS: honoured via the shared `ssl` / `ssl_mode` config. `Disable` maps to
//! `tls=false`; `Require` and `Verify` both map to `tls=true` (the rustls
//! backend always verifies certificates — Mongo's "encrypt but don't verify"
//! nuance is deliberately not offered in v1).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use mongodb::bson::{self, Bson, Document, doc};
use mongodb::Client;
use mongodb::options::ClientOptions;

use super::{
    Capabilities, Collection, CollectionMeta, CollectionRef, ColumnMeta, Driver, DriverInfo,
    IndexMeta, Namespace, Page, QueryResult, Record, RecordPage, Value,
};
use crate::config::{ConnectionConfig, SslMode};

/// Default MongoDB port when the connection config omits one.
const DEFAULT_PORT: u16 = 27017;

/// How many documents `collection_meta` samples to synthesize a schema.
const SCHEMA_SAMPLE_LIMIT: i64 = 100;

/// Console `find` fetches at most this many documents when the payload omits
/// `limit` — the console is for interactive browsing, not full scans.
const CONSOLE_DEFAULT_LIMIT: u64 = 100;
/// Hard cap on console result sizes (`find.limit` and aggregate output), so
/// a thoughtless `{}` filter cannot pull an entire collection into the TUI.
const CONSOLE_MAX_LIMIT: u64 = 1000;

/// Usage text baked into every console-parse error so the format teaches
/// itself from the error message.
const CONSOLE_USAGE: &str = "MongoDB console expects one JSON object:\n  \
    {\"collection\": \"<name>\", \"find\": {\"filter\": {...}, \"sort\": {...}, \
    \"projection\": {...}, \"limit\": 50, \"skip\": 0}}\n  \
    {\"collection\": \"<name>\", \"aggregate\": [<pipeline stages>]}\n\
    Use {\"$oid\": \"<24-hex>\"} for ObjectIds and {\"$date\": \"<rfc3339 or epoch millis>\"} \
    for dates. v1 is read-only: insert/update/delete are not supported.";

pub struct MongoDriver {
    client: Client,
    info: DriverInfo,
    /// Configured default database — the namespace the console falls back to
    /// and the authSource (see `build_uri`). Also the namespace fallback when
    /// the user lacks the privilege to list databases.
    default_db: Option<String>,
}

impl MongoDriver {
    pub async fn connect(cfg: &ConnectionConfig) -> Result<Self> {
        let uri = build_uri(cfg);
        // `uri` contains the resolved password — it is parsed in memory and
        // never logged or stored.
        let opts = ClientOptions::parse(&uri)
            .await
            .with_context(|| {
                format!("invalid MongoDB connection parameters ({})", cfg.display_url())
            })?;
        let client = Client::with_options(opts)
            .with_context(|| format!("failed to connect to MongoDB ({})", cfg.display_url()))?;

        // Client construction is lazy — force a round trip so a bad host or
        // wrong credentials fail at connect time, not on the first browse.
        client
            .database("admin")
            .run_command(doc! { "ping": 1 })
            .await
            .with_context(|| format!("failed to reach MongoDB ({})", cfg.display_url()))?;

        let version = client
            .database("admin")
            .run_command(doc! { "buildInfo": 1 })
            .await
            .ok()
            .and_then(|d| d.get_str("version").ok().map(str::to_string))
            .unwrap_or_else(|| "unknown".to_string());

        let default_db = cfg
            .database
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(str::to_string);

        Ok(Self {
            client,
            info: DriverInfo {
                name: "MongoDB".to_string(),
                server_version: version,
                query_language: "MQL (JSON)".to_string(),
            },
            default_db,
        })
    }
}

/// Drain a server cursor into a Vec using the driver's inherent
/// `advance`/`deserialize_current` — no extra `futures` dependency needed.
async fn drain(mut cursor: mongodb::Cursor<Document>) -> Result<Vec<Document>> {
    let mut docs = Vec::new();
    while cursor.advance().await? {
        docs.push(cursor.deserialize_current()?);
    }
    Ok(docs)
}

#[async_trait]
impl Driver for MongoDriver {
    fn info(&self) -> DriverInfo {
        self.info.clone()
    }

    fn capabilities(&self) -> Capabilities {
        // Schemaless documents have no fixed row shape, so there is no
        // inline EDIT_DATA; a document's nested JSON stays viewable through
        // the large-value popup (Value::Json cells).
        Capabilities::BROWSE | Capabilities::QUERY_TEXT
    }

    async fn ping(&self) -> Result<Duration> {
        let start = Instant::now();
        self.client
            .database("admin")
            .run_command(doc! { "ping": 1 })
            .await
            .context("MongoDB ping failed")?;
        Ok(start.elapsed())
    }

    async fn namespaces(&self) -> Result<Vec<Namespace>> {
        match self.client.list_database_names().await {
            Ok(mut names) => {
                names.sort();
                Ok(names.into_iter().map(Namespace).collect())
            }
            Err(e) => {
                // A low-privilege user cannot list databases; fall back to
                // the configured default database so the tree still opens.
                match &self.default_db {
                    Some(db) => Ok(vec![Namespace(db.clone())]),
                    None => Err(e).context("failed to list MongoDB databases"),
                }
            }
        }
    }

    async fn collections(&self, ns: &Namespace) -> Result<Vec<Collection>> {
        let db = self.client.database(&ns.0);
        let names = db
            .list_collection_names()
            .await
            .with_context(|| format!("failed to list collections in {}", ns.0))?;

        let mut out = Vec::with_capacity(names.len());
        for name in names {
            // `system.*` holds server internals (views, indexes) — noise in
            // an explorer tree.
            if name.starts_with("system.") {
                continue;
            }
            // Cheap metadata estimate; a per-collection failure (e.g. a view
            // without privileges) must not sink the whole listing.
            let count = db
                .collection::<Document>(&name)
                .estimated_document_count()
                .await
                .ok();
            out.push(Collection {
                name,
                estimated_row_count: count,
                estimated_size_bytes: None,
            });
        }
        Ok(out)
    }

    async fn collection_meta(&self, c: &CollectionRef) -> Result<CollectionMeta> {
        let coll = self
            .client
            .database(&c.namespace.0)
            .collection::<Document>(&c.name);

        // Schema inference: sample documents and union their top-level
        // fields. See the module docs — this is a synthesized hint.
        let sample = drain(
            coll.find(doc! {})
                .limit(SCHEMA_SAMPLE_LIMIT)
                .await
                .with_context(|| format!("failed to sample {}", c))?,
        )
        .await?;
        let columns = infer_schema(&sample)
            .into_iter()
            .map(|f| ColumnMeta {
                is_primary_key: f.name == "_id",
                is_unique: f.name == "_id",
                is_nullable: f.nullable,
                is_foreign_key: false,
                name: f.name,
                data_type: f.type_name,
                extra: None,
            })
            .collect();

        // Indexes, unlike fields, are real metadata — list them exactly.
        let mut indexes = Vec::new();
        let mut cursor = coll
            .list_indexes()
            .await
            .with_context(|| format!("failed to list indexes on {}", c))?;
        while cursor.advance().await? {
            let idx: mongodb::IndexModel = cursor.deserialize_current()?;
            let cols: Vec<String> = idx.keys.keys().cloned().collect();
            let (name, unique) = match idx.options {
                Some(o) => (o.name.unwrap_or_default(), o.unique.unwrap_or(false)),
                None => (String::new(), false),
            };
            indexes.push(IndexMeta {
                is_primary: cols == ["_id"],
                is_unique: unique,
                name,
                columns: cols,
            });
        }

        Ok(CollectionMeta {
            reference: c.clone(),
            columns,
            indexes,
            foreign_keys: Vec::new(),
        })
    }

    async fn records(&self, c: &CollectionRef, page: Page) -> Result<RecordPage> {
        let coll = self
            .client
            .database(&c.namespace.0)
            .collection::<Document>(&c.name);

        let total_records = coll.estimated_document_count().await.ok();

        // Column set comes from the same sampling used for metadata, so the
        // grid matches what the schema panel shows…
        let sample = drain(
            coll.find(doc! {})
                .limit(SCHEMA_SAMPLE_LIMIT)
                .await
                .with_context(|| format!("failed to sample {}", c))?,
        )
        .await?;
        let mut columns: Vec<String> = infer_schema(&sample)
            .into_iter()
            .map(|f| f.name)
            .collect();

        // Sort by _id for stable paging — natural order can shift under the
        // cursor between pages.
        let limit = page.limit.clamp(1, i64::MAX as u64) as i64;
        let docs = drain(
            coll.find(doc! {})
                .sort(doc! { "_id": 1 })
                .skip(page.offset)
                .limit(limit)
                .await
                .with_context(|| format!("failed to fetch documents from {}", c))?,
        )
        .await?;

        // …but the sample is only a hint: any field that shows up in this
        // page without being sampled is appended, so no data is hidden.
        for d in &docs {
            for k in d.keys() {
                if !columns.contains(k) {
                    columns.push(k.clone());
                }
            }
        }
        let records = docs.iter().map(|d| doc_to_record(d, &columns)).collect();

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
        let cmd = parse_console_command(query)?;
        let db = self.client.database(&ns.0);
        match cmd {
            ConsoleCommand::Find {
                collection,
                filter,
                sort,
                projection,
                skip,
                limit,
            } => {
                let coll = db.collection::<Document>(&collection);
                let mut find = coll.find(filter).skip(skip).limit(limit as i64);
                if let Some(sort) = sort {
                    find = find.sort(sort);
                }
                if let Some(projection) = projection {
                    find = find.projection(projection);
                }
                let docs = drain(
                    find.await
                        .with_context(|| format!("find failed on {}.{collection}", ns.0))?,
                )
                .await?;
                Ok(docs_to_query_result(docs, start.elapsed()))
            }
            ConsoleCommand::Aggregate {
                collection,
                pipeline,
            } => {
                let coll = db.collection::<Document>(&collection);
                let mut docs = drain(
                    coll.aggregate(pipeline)
                        .await
                        .with_context(|| format!("aggregate failed on {}.{collection}", ns.0))?,
                )
                .await?;
                docs.truncate(CONSOLE_MAX_LIMIT as usize);
                Ok(docs_to_query_result(docs, start.elapsed()))
            }
        }
    }

    /// Schemaless stores have no DDL — render the synthesized schema and the
    /// real index list as pretty JSON for the definition popup.
    async fn definition(&self, c: &CollectionRef) -> Result<String> {
        let meta = self.collection_meta(c).await?;
        let fields: Vec<serde_json::Value> = meta
            .columns
            .iter()
            .map(|col| {
                serde_json::json!({
                    "name": col.name,
                    "type": col.data_type,
                    "nullable": col.is_nullable,
                    "primary_key": col.is_primary_key,
                })
            })
            .collect();
        let indexes: Vec<serde_json::Value> = meta
            .indexes
            .iter()
            .map(|i| {
                serde_json::json!({
                    "name": i.name,
                    "keys": i.columns,
                    "unique": i.is_unique,
                })
            })
            .collect();
        Ok(serde_json::to_string_pretty(&serde_json::json!({
            "database": c.namespace.0,
            "collection": c.name,
            "note": format!(
                "MongoDB is schemaless — fields inferred from a sample of up to {SCHEMA_SAMPLE_LIMIT} documents"
            ),
            "fields": fields,
            "indexes": indexes,
        }))?)
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested without a server)
// ---------------------------------------------------------------------------

/// Percent-encode per RFC 3986 for URI userinfo/host segments — MongoDB
/// credentials and unix-socket paths can contain `@`, `:`, `/`, etc.
fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Build the `mongodb://` connection URI from the shared config shape.
///
/// * `database` doubles as the **authSource** (Mongo users are typically
///   created in the database they own); with no database configured the
///   authSource is `admin`.
/// * A unix socket rides in the host slot (percent-encoded), like Mongo's
///   own URI format allows.
/// * TLS comes from the shared `ssl`/`ssl_mode` config (see module docs).
fn build_uri(cfg: &ConnectionConfig) -> String {
    let auth = match &cfg.user {
        Some(u) if !u.trim().is_empty() => {
            let pass = cfg.resolve_password().unwrap_or_default();
            format!("{}:{}@", pct_encode(u.trim()), pct_encode(&pass))
        }
        _ => String::new(),
    };

    let host_part = match &cfg.socket {
        Some(sock) => pct_encode(sock),
        None => format!("{}:{}", cfg.host, cfg.port.unwrap_or(DEFAULT_PORT)),
    };

    let db_path = cfg
        .database
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .unwrap_or("");

    let mut params = Vec::new();
    if !auth.is_empty() {
        let auth_source = if db_path.is_empty() { "admin" } else { db_path };
        params.push(format!("authSource={}", pct_encode(auth_source)));
    }
    match cfg.effective_ssl_mode() {
        Some(SslMode::Disable) => params.push("tls=false".to_string()),
        Some(SslMode::Require) | Some(SslMode::Verify) => params.push("tls=true".to_string()),
        None => {}
    }

    let query = if params.is_empty() {
        String::new()
    } else {
        format!("?{}", params.join("&"))
    };
    format!("mongodb://{auth}{host_part}/{db_path}{query}")
}

/// The BSON type name MongoDB itself reports (`$type` aliases) — used as the
/// synthesized column's `data_type`.
fn bson_type_name(b: &Bson) -> &'static str {
    match b {
        Bson::Double(_) => "double",
        Bson::String(_) => "string",
        Bson::Array(_) => "array",
        Bson::Document(_) => "object",
        Bson::Boolean(_) => "bool",
        Bson::Null => "null",
        Bson::RegularExpression(_) => "regex",
        Bson::JavaScriptCode(_) => "javascript",
        Bson::JavaScriptCodeWithScope(_) => "javascriptWithScope",
        Bson::Int32(_) => "int",
        Bson::Int64(_) => "long",
        Bson::Timestamp(_) => "timestamp",
        Bson::Binary(_) => "binData",
        Bson::ObjectId(_) => "objectId",
        Bson::DateTime(_) => "date",
        Bson::Symbol(_) => "symbol",
        Bson::Decimal128(_) => "decimal",
        Bson::Undefined => "undefined",
        Bson::DbPointer(_) => "dbPointer",
        Bson::MinKey => "minKey",
        Bson::MaxKey => "maxKey",
    }
}

/// Map a BSON value to the model-agnostic `Value`. `_id` ObjectIds render as
/// their hex string; nested documents/arrays become `Value::Json` (the
/// large-value popup then shows the full JSON).
fn bson_to_value(b: &Bson) -> Value {
    match b {
        Bson::Null | Bson::Undefined => Value::Null,
        Bson::Boolean(v) => Value::Bool(*v),
        Bson::Int32(v) => Value::Int(i64::from(*v)),
        Bson::Int64(v) => Value::Int(*v),
        Bson::Double(v) => Value::Float(*v),
        Bson::Decimal128(v) => Value::Decimal(v.to_string()),
        Bson::String(s) => Value::String(s.clone()),
        Bson::ObjectId(oid) => Value::String(oid.to_hex()),
        Bson::DateTime(dt) => Value::DateTime(
            dt.try_to_rfc3339_string()
                .unwrap_or_else(|_| dt.timestamp_millis().to_string()),
        ),
        Bson::Binary(bin) => Value::Bytes(bin.bytes.clone()),
        Bson::Document(_) | Bson::Array(_) => Value::Json(bson_to_json(b)),
        Bson::RegularExpression(re) => Value::String(format!("/{}/{}", re.pattern, re.options)),
        Bson::JavaScriptCode(code) => Value::String(code.clone()),
        Bson::Symbol(s) => Value::String(s.clone()),
        Bson::Timestamp(ts) => Value::String(format!("Timestamp({}, {})", ts.time, ts.increment)),
        Bson::MinKey => Value::String("MinKey".to_string()),
        Bson::MaxKey => Value::String("MaxKey".to_string()),
        Bson::JavaScriptCodeWithScope(_) | Bson::DbPointer(_) => Value::Json(bson_to_json(b)),
    }
}

/// BSON → plain JSON, recursively. Mongo-specific scalars degrade to their
/// canonical string form (ObjectId hex, RFC 3339 date, decimal string), so
/// the popup shows readable values rather than extended-JSON wrappers.
fn bson_to_json(b: &Bson) -> serde_json::Value {
    match b {
        Bson::Null | Bson::Undefined => serde_json::Value::Null,
        Bson::Boolean(v) => serde_json::Value::Bool(*v),
        Bson::Int32(v) => serde_json::Value::from(*v),
        Bson::Int64(v) => serde_json::Value::from(*v),
        Bson::Double(v) => serde_json::Number::from_f64(*v)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Bson::Decimal128(v) => serde_json::Value::String(v.to_string()),
        Bson::String(s) => serde_json::Value::String(s.clone()),
        Bson::ObjectId(oid) => serde_json::Value::String(oid.to_hex()),
        Bson::DateTime(dt) => serde_json::Value::String(
            dt.try_to_rfc3339_string()
                .unwrap_or_else(|_| dt.timestamp_millis().to_string()),
        ),
        Bson::Binary(bin) => {
            serde_json::Value::String(format!("<binary {} bytes>", bin.bytes.len()))
        }
        Bson::Array(a) => serde_json::Value::Array(a.iter().map(bson_to_json).collect()),
        Bson::Document(d) => serde_json::Value::Object(
            d.iter().map(|(k, v)| (k.clone(), bson_to_json(v))).collect(),
        ),
        Bson::RegularExpression(re) => {
            serde_json::Value::String(format!("/{}/{}", re.pattern, re.options))
        }
        Bson::JavaScriptCode(code) => serde_json::Value::String(code.clone()),
        Bson::JavaScriptCodeWithScope(cws) => serde_json::json!({
            "code": cws.code,
            "scope": bson_to_json(&Bson::Document(cws.scope.clone())),
        }),
        Bson::Symbol(s) => serde_json::Value::String(s.clone()),
        Bson::Timestamp(ts) => {
            serde_json::Value::String(format!("Timestamp({}, {})", ts.time, ts.increment))
        }
        Bson::DbPointer(p) => serde_json::Value::String(format!("{p:?}")),
        Bson::MinKey => serde_json::Value::String("MinKey".to_string()),
        Bson::MaxKey => serde_json::Value::String("MaxKey".to_string()),
    }
}

/// One synthesized field of a sampled schema.
#[derive(Debug, PartialEq, Eq)]
struct InferredField {
    name: String,
    /// BSON type name, or `"mixed"` when the samples disagree.
    type_name: String,
    /// True when the field is absent from at least one sampled document.
    nullable: bool,
}

/// Union the top-level fields of `docs` into a stable column set: `_id`
/// first (it is every document's primary key), then the rest in first-seen
/// order. An empty sample still yields `_id`, the one field MongoDB adds to
/// every inserted document.
fn infer_schema(docs: &[Document]) -> Vec<InferredField> {
    if docs.is_empty() {
        return vec![InferredField {
            name: "_id".to_string(),
            type_name: "objectId".to_string(),
            nullable: false,
        }];
    }

    let mut order: Vec<String> = Vec::new();
    // None = mixed types seen across samples.
    let mut types: HashMap<String, Option<&'static str>> = HashMap::new();
    let mut present: HashMap<String, usize> = HashMap::new();

    for d in docs {
        for (k, v) in d {
            if !order.contains(k) {
                order.push(k.clone());
            }
            let tn = bson_type_name(v);
            types
                .entry(k.clone())
                .and_modify(|t| {
                    if *t != Some(tn) {
                        *t = None;
                    }
                })
                .or_insert(Some(tn));
            *present.entry(k.clone()).or_default() += 1;
        }
    }

    order.sort_by_key(|name| if name == "_id" { 0 } else { 1 });
    order
        .into_iter()
        .map(|name| InferredField {
            nullable: present[&name] < docs.len(),
            type_name: types[&name].unwrap_or("mixed").to_string(),
            name,
        })
        .collect()
}

/// Align one document onto the column set; absent fields read as NULL.
fn doc_to_record(doc: &Document, columns: &[String]) -> Record {
    Record {
        values: columns
            .iter()
            .map(|c| doc.get(c).map(bson_to_value).unwrap_or(Value::Null))
            .collect(),
    }
}

/// Result-set shaping shared by `find` and `aggregate`: columns are the
/// union of the returned documents' fields (`_id` first).
fn docs_to_query_result(docs: Vec<Document>, elapsed: Duration) -> QueryResult {
    let columns: Vec<String> = infer_schema(&docs).into_iter().map(|f| f.name).collect();
    let records: Vec<Record> = docs.iter().map(|d| doc_to_record(d, &columns)).collect();
    let rows_affected = records.len() as u64;
    QueryResult {
        columns,
        records,
        rows_affected,
        execution_time: elapsed,
    }
}

/// A parsed console command (see module docs for the format).
#[derive(Debug, PartialEq)]
enum ConsoleCommand {
    Find {
        collection: String,
        filter: Document,
        sort: Option<Document>,
        projection: Option<Document>,
        skip: u64,
        limit: u64,
    },
    Aggregate {
        collection: String,
        pipeline: Vec<Document>,
    },
}

/// JSON value → BSON, with `{ "$oid": "<hex>" }` and `{ "$date":
/// "<rfc3339 or epoch millis>" }` conveniences so console filters can target
/// ObjectIds and dates — plain JSON alone cannot express either.
fn json_to_bson(v: &serde_json::Value) -> Result<Bson> {
    Ok(match v {
        serde_json::Value::Null => Bson::Null,
        serde_json::Value::Bool(b) => Bson::Boolean(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Bson::Int64(i)
            } else if let Some(u) = n.as_u64() {
                // u64 above i64::MAX has no BSON int form; a double is the
                // same trade-off the mongo shell makes for large numbers.
                Bson::Double(u as f64)
            } else {
                Bson::Double(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        serde_json::Value::String(s) => Bson::String(s.clone()),
        serde_json::Value::Array(a) => {
            Bson::Array(a.iter().map(json_to_bson).collect::<Result<Vec<_>>>()?)
        }
        serde_json::Value::Object(o) => {
            if o.len() == 1 {
                if let Some(serde_json::Value::String(hex)) = o.get("$oid") {
                    return Ok(Bson::ObjectId(
                        bson::oid::ObjectId::parse_str(hex)
                            .with_context(|| format!("invalid $oid value {hex:?}"))?,
                    ));
                }
                if let Some(d) = o.get("$date") {
                    let dt = match d {
                        serde_json::Value::String(s) => {
                            bson::DateTime::parse_rfc3339_str(s)
                                .with_context(|| format!("invalid $date value {s:?}"))?
                        }
                        serde_json::Value::Number(n) => bson::DateTime::from_millis(
                            n.as_i64()
                                .ok_or_else(|| anyhow!("$date millis must be an integer"))?,
                        ),
                        _ => bail!("$date must be an RFC 3339 string or epoch millis"),
                    };
                    return Ok(Bson::DateTime(dt));
                }
            }
            let mut doc = Document::new();
            for (k, val) in o {
                doc.insert(k.clone(), json_to_bson(val)?);
            }
            Bson::Document(doc)
        }
    })
}

/// Parse the console's mini query format (see module docs). Every failure
/// path appends the usage text so the error message teaches the format.
fn parse_console_command(query: &str) -> Result<ConsoleCommand> {
    // The shared console splits scripts on ';' — tolerate the stray trailing
    // one it leaves on a single-statement JSON payload.
    let trimmed = query.trim().trim_end_matches(';').trim();
    let value: serde_json::Value = serde_json::from_str(trimmed)
        .map_err(|e| anyhow!("invalid JSON: {e}\n{CONSOLE_USAGE}"))?;
    let obj = value
        .as_object()
        .ok_or_else(|| anyhow!("top level must be a JSON object\n{CONSOLE_USAGE}"))?;

    let collection = obj
        .get("collection")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| {
            anyhow!("missing \"collection\": which collection to query\n{CONSOLE_USAGE}")
        })?;

    let json_doc = |v: &serde_json::Value, what: &str| -> Result<Document> {
        match json_to_bson(v)? {
            Bson::Document(d) => Ok(d),
            _ => bail!("\"{what}\" must be a JSON object\n{CONSOLE_USAGE}"),
        }
    };

    match (obj.get("find"), obj.get("aggregate")) {
        (Some(find), None) => {
            let p = find
                .as_object()
                .ok_or_else(|| anyhow!("\"find\" must be a JSON object\n{CONSOLE_USAGE}"))?;
            let filter = match p.get("filter") {
                Some(f) => json_doc(f, "filter")?,
                None => Document::new(),
            };
            let sort = p.get("sort").map(|s| json_doc(s, "sort")).transpose()?;
            let projection = p
                .get("projection")
                .map(|s| json_doc(s, "projection"))
                .transpose()?;
            let limit = match p.get("limit") {
                Some(v) => {
                    let n = v
                        .as_u64()
                        .filter(|n| *n >= 1)
                        .ok_or_else(|| anyhow!("\"limit\" must be a positive integer"))?;
                    n.min(CONSOLE_MAX_LIMIT)
                }
                None => CONSOLE_DEFAULT_LIMIT,
            };
            let skip = match p.get("skip") {
                Some(v) => v
                    .as_u64()
                    .ok_or_else(|| anyhow!("\"skip\" must be a non-negative integer"))?,
                None => 0,
            };
            Ok(ConsoleCommand::Find {
                collection,
                filter,
                sort,
                projection,
                skip,
                limit,
            })
        }
        (None, Some(agg)) => {
            let stages = agg
                .as_array()
                .ok_or_else(|| anyhow!("\"aggregate\" must be a JSON array of pipeline stages\n{CONSOLE_USAGE}"))?;
            if stages.is_empty() {
                bail!("\"aggregate\" pipeline is empty\n{CONSOLE_USAGE}");
            }
            let pipeline = stages
                .iter()
                .map(|s| json_doc(s, "pipeline stage"))
                .collect::<Result<Vec<_>>>()?;
            Ok(ConsoleCommand::Aggregate {
                collection,
                pipeline,
            })
        }
        (Some(_), Some(_)) => {
            bail!("specify only one of \"find\" / \"aggregate\"\n{CONSOLE_USAGE}")
        }
        (None, None) => {
            let others: Vec<&str> = obj
                .keys()
                .filter(|k| k.as_str() != "collection")
                .map(String::as_str)
                .collect();
            if others.is_empty() {
                bail!("missing command: \"find\" or \"aggregate\"\n{CONSOLE_USAGE}");
            }
            bail!(
                "unsupported command(s) {} — v1 is read-only: only \"find\" and \"aggregate\"\n{CONSOLE_USAGE}",
                others.join(", ")
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mongodb::bson::spec::BinarySubtype;
    use mongodb::bson::{Binary, DateTime, Regex, Timestamp, oid::ObjectId};
    use serde_json::json;

    fn oid() -> ObjectId {
        ObjectId::parse_str("64b7f5e2a9c1d2e3f4a5b6c7").unwrap()
    }

    // ---- URI building ----

    fn cfg(
        user: Option<&str>,
        pass: Option<&str>,
        db: Option<&str>,
        ssl: Option<SslMode>,
    ) -> ConnectionConfig {
        ConnectionConfig {
            name: "t".to_string(),
            driver: crate::config::DriverType::MongoDB,
            host: "db.local".to_string(),
            port: None,
            user: user.map(str::to_string),
            password: pass.map(str::to_string),
            database: db.map(str::to_string),
            socket: None,
            ssl: false,
            ssl_mode: ssl,
            ssl_ca: None,
            ssl_cert: None,
            ssl_key: None,
            ssh: None,
        }
    }

    #[test]
    fn test_pct_encode() {
        assert_eq!(pct_encode("plain-USER_1.0~x"), "plain-USER_1.0~x");
        assert_eq!(pct_encode("p@ss:w/rd"), "p%40ss%3Aw%2Frd");
    }

    #[test]
    fn test_build_uri_no_auth_default_port() {
        assert_eq!(
            build_uri(&cfg(None, None, None, None)),
            "mongodb://db.local:27017/"
        );
    }

    #[test]
    fn test_build_uri_auth_source_follows_database_else_admin() {
        // database set → authSource is that database.
        assert_eq!(
            build_uri(&cfg(Some("ada"), Some("s3cret"), Some("shop"), None)),
            "mongodb://ada:s3cret@db.local:27017/shop?authSource=shop"
        );
        // no database → authSource admin.
        assert_eq!(
            build_uri(&cfg(Some("ada"), Some("s3cret"), None, None)),
            "mongodb://ada:s3cret@db.local:27017/?authSource=admin"
        );
    }

    #[test]
    fn test_build_uri_tls_modes_and_special_chars() {
        assert_eq!(
            build_uri(&cfg(Some("a@b"), Some("p/w"), Some("d"), Some(SslMode::Verify))),
            "mongodb://a%40b:p%2Fw@db.local:27017/d?authSource=d&tls=true"
        );
        assert_eq!(
            build_uri(&cfg(None, None, None, Some(SslMode::Disable))),
            "mongodb://db.local:27017/?tls=false"
        );
        // No auth → no authSource param at all.
        assert_eq!(
            build_uri(&cfg(None, None, Some("shop"), None)),
            "mongodb://db.local:27017/shop"
        );
    }

    #[test]
    fn test_build_uri_unix_socket() {
        let mut c = cfg(None, None, None, None);
        c.socket = Some("/tmp/mongodb-27017.sock".to_string());
        assert_eq!(
            build_uri(&c),
            "mongodb://%2Ftmp%2Fmongodb-27017.sock/"
        );
    }

    // ---- BSON → Value mapping ----

    #[test]
    fn test_bson_to_value_scalars() {
        assert_eq!(bson_to_value(&Bson::Null), Value::Null);
        assert_eq!(bson_to_value(&Bson::Undefined), Value::Null);
        assert_eq!(bson_to_value(&Bson::Boolean(true)), Value::Bool(true));
        assert_eq!(bson_to_value(&Bson::Int32(7)), Value::Int(7));
        assert_eq!(bson_to_value(&Bson::Int64(9)), Value::Int(9));
        assert_eq!(bson_to_value(&Bson::Double(2.5)), Value::Float(2.5));
        assert_eq!(
            bson_to_value(&Bson::String("hi".into())),
            Value::String("hi".into())
        );
        assert_eq!(
            bson_to_value(&Bson::ObjectId(oid())),
            Value::String("64b7f5e2a9c1d2e3f4a5b6c7".into())
        );
        // Fractional-second rendering varies; assert the stable prefix.
        let Value::DateTime(dt) = bson_to_value(&Bson::DateTime(DateTime::from_millis(0))) else {
            panic!("date must map to Value::DateTime")
        };
        assert!(dt.starts_with("1970-01-01T00:00:00"), "got {dt}");
        assert_eq!(
            bson_to_value(&Bson::Binary(Binary {
                subtype: BinarySubtype::Generic,
                bytes: vec![1, 2, 3],
            })),
            Value::Bytes(vec![1, 2, 3])
        );
        assert_eq!(
            bson_to_value(&Bson::RegularExpression(Regex {
                pattern: "^a".into(),
                options: "i".into(),
            })),
            Value::String("/^a/i".into())
        );
        assert_eq!(
            bson_to_value(&Bson::Timestamp(Timestamp {
                time: 5,
                increment: 2,
            })),
            Value::String("Timestamp(5, 2)".into())
        );
        assert_eq!(
            bson_to_value(&Bson::Symbol("s".into())),
            Value::String("s".into())
        );
    }

    #[test]
    fn test_bson_to_value_nested_becomes_json() {
        let nested = doc! {
            "tags": ["a", "b"],
            "addr": { "city": "jakarta", "zip": 12345 },
            "ref": Bson::ObjectId(oid()),
        };
        let Value::Json(j) = bson_to_value(&Bson::Document(nested)) else {
            panic!("nested document must map to Value::Json")
        };
        assert_eq!(j["tags"], json!(["a", "b"]));
        assert_eq!(j["addr"], json!({ "city": "jakarta", "zip": 12345 }));
        // Mongo scalars degrade to readable strings inside JSON.
        assert_eq!(j["ref"], json!("64b7f5e2a9c1d2e3f4a5b6c7"));
    }

    // ---- Schema inference ----

    #[test]
    fn test_infer_schema_union_nullable_and_mixed() {
        let docs = vec![
            doc! { "_id": Bson::ObjectId(oid()), "name": "ada", "age": 36 },
            doc! { "_id": Bson::ObjectId(oid()), "age": "old" }, // name absent; age mixed
        ];
        let fields = infer_schema(&docs);
        assert_eq!(fields[0].name, "_id", "_id must come first");
        assert!(!fields[0].nullable);

        let name = fields.iter().find(|f| f.name == "name").unwrap();
        assert_eq!(name.type_name, "string");
        assert!(name.nullable, "absent in one sample → nullable");

        let age = fields.iter().find(|f| f.name == "age").unwrap();
        assert_eq!(age.type_name, "mixed", "int + string across samples");
        assert!(!age.nullable, "present in every sample");
    }

    #[test]
    fn test_infer_schema_empty_collection_yields_id_only() {
        let fields = infer_schema(&[]);
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].name, "_id");
    }

    #[test]
    fn test_doc_to_record_aligns_missing_fields_to_null() {
        let doc = doc! { "_id": Bson::ObjectId(oid()), "name": "ada" };
        let cols = vec!["_id".to_string(), "name".to_string(), "email".to_string()];
        let rec = doc_to_record(&doc, &cols);
        assert_eq!(
            rec.values,
            vec![
                Value::String("64b7f5e2a9c1d2e3f4a5b6c7".into()),
                Value::String("ada".into()),
                Value::Null,
            ]
        );
    }

    // ---- Console mini-format parsing ----

    #[test]
    fn test_parse_find_minimal_defaults() {
        let cmd = parse_console_command(r#"{ "collection": "users", "find": {} }"#).unwrap();
        assert_eq!(
            cmd,
            ConsoleCommand::Find {
                collection: "users".into(),
                filter: Document::new(),
                sort: None,
                projection: None,
                skip: 0,
                limit: CONSOLE_DEFAULT_LIMIT,
            }
        );
    }

    #[test]
    fn test_parse_find_full_with_extended_values() {
        let cmd = parse_console_command(
            r#"{
                "collection": "users",
                "find": {
                    "filter": { "_id": { "$oid": "64b7f5e2a9c1d2e3f4a5b6c7" },
                                "since": { "$date": "2024-01-02T03:04:05Z" } },
                    "sort": { "_id": -1 },
                    "projection": { "name": 1 },
                    "limit": 5,
                    "skip": 10
                }
            }"#,
        )
        .unwrap();
        let ConsoleCommand::Find {
            filter,
            sort,
            projection,
            skip,
            limit,
            ..
        } = cmd
        else {
            panic!("expected find")
        };
        assert_eq!(filter.get("_id"), Some(&Bson::ObjectId(oid())));
        assert_eq!(
            filter.get("since"),
            Some(&Bson::DateTime(
                DateTime::parse_rfc3339_str("2024-01-02T03:04:05Z").unwrap()
            ))
        );
        assert_eq!(sort.unwrap().get_i64("_id").unwrap(), -1);
        assert_eq!(projection.unwrap().get_i64("name").unwrap(), 1);
        assert_eq!((skip, limit), (10, 5));
    }

    #[test]
    fn test_parse_tolerates_trailing_semicolon() {
        // The shared console splitter leaves a trailing ';' on single-statement
        // payloads.
        let cmd = parse_console_command(r#"{ "collection": "c", "find": {} };"#).unwrap();
        assert!(matches!(cmd, ConsoleCommand::Find { .. }));
    }

    #[test]
    fn test_parse_limit_is_capped() {
        let cmd =
            parse_console_command(r#"{ "collection": "c", "find": { "limit": 999999 } }"#).unwrap();
        let ConsoleCommand::Find { limit, .. } = cmd else {
            panic!("expected find")
        };
        assert_eq!(limit, CONSOLE_MAX_LIMIT);
    }

    #[test]
    fn test_parse_aggregate() {
        let cmd = parse_console_command(
            r#"{ "collection": "orders", "aggregate": [ { "$group": { "_id": "$sku" } } ] }"#,
        )
        .unwrap();
        let ConsoleCommand::Aggregate { pipeline, .. } = cmd else {
            panic!("expected aggregate")
        };
        assert_eq!(pipeline.len(), 1);
        assert!(pipeline[0].contains_key("$group"));
    }

    #[test]
    fn test_parse_errors_teach_the_format() {
        for (input, needle) in [
            ("not json", "invalid JSON"),
            (r#"[1, 2]"#, "top level must be a JSON object"),
            (r#"{ "find": {} }"#, "missing \"collection\""),
            (r#"{ "collection": "c" }"#, "missing command"),
            (
                r#"{ "collection": "c", "insert": { "a": 1 } }"#,
                "unsupported command",
            ),
            (
                r#"{ "collection": "c", "find": {}, "aggregate": [] }"#,
                "only one of",
            ),
            (r#"{ "collection": "c", "aggregate": [] }"#, "pipeline is empty"),
        ] {
            let err = parse_console_command(input).unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains(needle), "{input:?} → {msg}");
            assert!(msg.contains("find"), "usage text missing from: {msg}");
        }
    }

    // ---- JSON → BSON conversion ----

    #[test]
    fn test_json_to_bson_plain_and_extended() {
        assert_eq!(json_to_bson(&json!(null)).unwrap(), Bson::Null);
        assert_eq!(json_to_bson(&json!(true)).unwrap(), Bson::Boolean(true));
        assert_eq!(json_to_bson(&json!(42)).unwrap(), Bson::Int64(42));
        assert_eq!(json_to_bson(&json!(2.5)).unwrap(), Bson::Double(2.5));
        assert_eq!(
            json_to_bson(&json!("s")).unwrap(),
            Bson::String("s".into())
        );
        // u64 beyond i64 range degrades to double rather than wrapping.
        assert_eq!(
            json_to_bson(&json!(u64::MAX)).unwrap(),
            Bson::Double(u64::MAX as f64)
        );
        assert_eq!(
            json_to_bson(&json!({ "$date": 0 })).unwrap(),
            Bson::DateTime(DateTime::from_millis(0))
        );
        // A $-prefixed key with siblings is a real field, not a convenience.
        assert_eq!(
            json_to_bson(&json!({ "$oid": "abc", "x": 1 })).unwrap(),
            Bson::Document(doc! { "$oid": "abc", "x": 1_i64 })
        );
        // Nested convenience inside an array.
        assert_eq!(
            json_to_bson(&json!([{ "$oid": "64b7f5e2a9c1d2e3f4a5b6c7" }])).unwrap(),
            Bson::Array(vec![Bson::ObjectId(oid())])
        );
    }

    #[test]
    fn test_json_to_bson_bad_extended_values_error() {
        assert!(json_to_bson(&json!({ "$oid": "zz" })).is_err());
        assert!(json_to_bson(&json!({ "$date": "not-a-date" })).is_err());
        assert!(json_to_bson(&json!({ "$date": true })).is_err());
    }
}
