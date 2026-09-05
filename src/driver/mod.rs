//! Database driver abstraction and generic data model.
//! See docs/architecture.md for capability model and architectural design.

pub mod clickhouse; // clickhouse
pub mod mongo; // mongo
pub mod mssql;
pub mod mysql;
pub mod postgres;
pub mod redis; // redis
pub mod sqlite;

use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::config::{ConnectionConfig, DriverType};

bitflags::bitflags! {
    /// Capabilities reported by drivers so the UI can enable/disable features gracefully.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
    pub struct Capabilities: u32 {
        const BROWSE     = 1 << 0;
        const QUERY_TEXT = 1 << 1;
        const DDL        = 1 << 2;
        const ERD        = 1 << 3;
        const EDIT_DATA  = 1 << 4;
        const EXPLAIN    = 1 << 5;
        /// Can list (and cancel) the server's currently running queries.
        const PROCESS_LIST = 1 << 6;
    }
}

/// Metadata about the active driver and server version.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DriverInfo {
    pub name: String,
    pub server_version: String,
    pub query_language: String,
}

/// A generic namespace (e.g. database / schema).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Namespace(pub String);

impl fmt::Display for Namespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A generic collection reference (e.g. table / document collection).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CollectionRef {
    pub namespace: Namespace,
    pub name: String,
}

impl fmt::Display for CollectionRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.namespace, self.name)
    }
}

/// Summary of a collection for the explorer tree.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Collection {
    pub name: String,
    pub estimated_row_count: Option<u64>,
    /// On-disk size in bytes (table + indexes), an estimate from the planner
    /// / information schema. Driver-specific: PostgreSQL includes TOAST/FSM/VM
    /// and sums partition children; MySQL is data+index page counts.
    #[serde(default)]
    pub estimated_size_bytes: Option<u64>,
}

/// Column or field definition.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ColumnMeta {
    pub name: String,
    pub data_type: String,
    pub is_nullable: bool,
    pub is_primary_key: bool,
    pub is_unique: bool,
    pub is_foreign_key: bool,
    pub extra: Option<String>,
}

/// Index definition.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexMeta {
    pub name: String,
    pub columns: Vec<String>,
    pub is_unique: bool,
    pub is_primary: bool,
}

/// Foreign key constraint definition.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForeignKeyMeta {
    pub name: String,
    pub column: String,
    pub ref_namespace: Namespace,
    pub ref_table: String,
    pub ref_column: String,
}

/// Complete structural metadata for a collection.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CollectionMeta {
    pub reference: CollectionRef,
    pub columns: Vec<ColumnMeta>,
    pub indexes: Vec<IndexMeta>,
    pub foreign_keys: Vec<ForeignKeyMeta>,
}

/// Dynamic cell value, model-agnostic (SQL & NoSQL ready).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    Decimal(String), // Preserves exact decimal precision without float rounding
    String(String),
    Bytes(Vec<u8>),  // Rendered as <blob N bytes> or hex
    Json(serde_json::Value),
    DateTime(String), // Raw server datetime string
}

impl Value {
    /// Formats the value for display in table cells.
    pub fn display_str(&self) -> String {
        match self {
            Value::Null => "NULL".to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Int(i) => i.to_string(),
            Value::UInt(u) => u.to_string(),
            Value::Float(f) => f.to_string(),
            Value::Decimal(s) => s.clone(),
            Value::String(s) => s.clone(),
            Value::Bytes(b) => format!("<blob {} bytes>", b.len()),
            Value::Json(j) => j.to_string(),
            Value::DateTime(dt) => dt.clone(),
        }
    }
}

/// A row/record of values.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub values: Vec<Value>,
}

/// Paged record results.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordPage {
    pub columns: Vec<String>,
    pub records: Vec<Record>,
    pub page: u64,
    pub page_size: u64,
    pub total_records: Option<u64>,
}

/// Query result (from arbitrary SQL/MQL execution).
#[derive(Clone, Debug)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub records: Vec<Record>,
    pub rows_affected: u64,
    pub execution_time: Duration,
}

/// Pagination request parameters.
#[derive(Clone, Copy, Debug)]
pub struct Page {
    pub offset: u64,
    pub limit: u64,
}

impl Default for Page {
    fn default() -> Self {
        Self {
            offset: 0,
            limit: 50,
        }
    }
}

/// Asynchronous, dyn-safe database driver trait.
#[async_trait]
pub trait Driver: Send + Sync {
    /// Identity and capabilities
    fn info(&self) -> DriverInfo;
    fn capabilities(&self) -> Capabilities;

    /// Health check
    async fn ping(&self) -> Result<Duration>;

    /// BROWSE capability
    async fn namespaces(&self) -> Result<Vec<Namespace>>;
    async fn collections(&self, ns: &Namespace) -> Result<Vec<Collection>>;
    async fn collection_meta(&self, c: &CollectionRef) -> Result<CollectionMeta>;
    async fn records(&self, c: &CollectionRef, page: Page) -> Result<RecordPage>;

    /// QUERY_TEXT capability
    async fn execute(&self, ns: &Namespace, query: &str) -> Result<QueryResult>;

    // ---- Interactive transactions. A driver that can hold one dedicated
    // connection open overrides these; the defaults report "unsupported" so
    // a connectionless driver stays valid. ----

    /// Open a transaction on a dedicated connection. While one is open,
    /// `execute` runs inside it instead of borrowing from the pool.
    async fn begin_tx(&self) -> Result<()> {
        anyhow::bail!("this driver does not support interactive transactions")
    }
    /// Commit and close the open transaction.
    async fn commit_tx(&self) -> Result<()> {
        anyhow::bail!("this driver does not support interactive transactions")
    }
    /// Roll back and close the open transaction.
    async fn rollback_tx(&self) -> Result<()> {
        anyhow::bail!("this driver does not support interactive transactions")
    }
    /// Whether a transaction is currently open.
    async fn in_tx(&self) -> bool {
        false
    }

    /// DDL capability
    async fn definition(&self, c: &CollectionRef) -> Result<String>;

    // ---- Non-table objects (views / routines / sequences). Default impls
    // return empty so a driver that doesn't surface them is still valid. ----

    /// Views in a namespace (openable like a table).
    async fn list_views(&self, _ns: &Namespace) -> Result<Vec<Collection>> {
        Ok(Vec::new())
    }
    /// Stored procedures & functions in a namespace.
    async fn list_routines(&self, _ns: &Namespace) -> Result<Vec<Collection>> {
        Ok(Vec::new())
    }
    /// Sequences in a namespace.
    async fn list_sequences(&self, _ns: &Namespace) -> Result<Vec<Collection>> {
        Ok(Vec::new())
    }
    /// Source / DDL of a stored routine.
    /// Every foreign key in `ns`, as `(child_table, fk)`.
    ///
    /// Used by the reverse foreign-key lookup ("what references this row?"),
    /// which needs the whole schema's relationships at once. The default walks
    /// each table — correct everywhere, but one round trip per table — so
    /// catalog-backed drivers override it with a single query.
    async fn schema_foreign_keys(&self, ns: &Namespace) -> Result<Vec<(String, ForeignKeyMeta)>> {
        let mut out = Vec::new();
        for t in self.collections(ns).await? {
            let cref = CollectionRef {
                namespace: ns.clone(),
                name: t.name.clone(),
            };
            // A single unreadable table must not sink the whole lookup.
            if let Ok(meta) = self.collection_meta(&cref).await {
                for fk in meta.foreign_keys {
                    out.push((t.name.clone(), fk));
                }
            }
        }
        Ok(out)
    }

    /// Sessions/queries currently running on the server, newest first.
    ///
    /// Defaults to empty so a driver without the notion (SQLite is a local
    /// file — there is no server to inspect) needs no implementation.
    async fn process_list(&self) -> Result<QueryResult> {
        Ok(QueryResult {
            columns: Vec::new(),
            records: Vec::new(),
            rows_affected: 0,
            execution_time: Duration::ZERO,
        })
    }

    /// Cancel one running query by its server-side id.
    async fn kill_process(&self, _id: &str) -> Result<()> {
        anyhow::bail!("this driver cannot cancel running queries")
    }

    async fn routine_definition(&self, _c: &CollectionRef) -> Result<String> {
        anyhow::bail!("this driver does not expose routine definitions")
    }
}

/// Factory function to instantiate and connect to a driver based on `DriverType`.
pub async fn connect_driver(cfg: &ConnectionConfig) -> Result<Arc<dyn Driver>> {
    match cfg.driver {
        DriverType::MySql => {
            let drv = mysql::MySqlDriver::connect(cfg).await?;
            Ok(Arc::new(drv))
        }
        DriverType::Postgres => {
            let drv = postgres::PostgresDriver::connect(cfg).await?;
            Ok(Arc::new(drv))
        }
        DriverType::Sqlite => {
            let drv = sqlite::SqliteDriver::connect(cfg).await?;
            Ok(Arc::new(drv))
        }
        DriverType::SqlServer => {
            let drv = mssql::MssqlDriver::connect(cfg).await?;
            Ok(Arc::new(drv))
        }
        DriverType::ClickHouse => {
            // clickhouse
            let drv = clickhouse::ClickHouseDriver::connect(cfg).await?;
            Ok(Arc::new(drv))
        }
        // mongo
        DriverType::MongoDB => {
            let drv = mongo::MongoDriver::connect(cfg).await?;
            Ok(Arc::new(drv))
        }
        // redis
        DriverType::Redis => {
            let drv = redis::RedisDriver::connect(cfg).await?;
            Ok(Arc::new(drv))
        }
    }
}

/// Case-insensitive "does this SQL start with keyword `kw`" test. Byte-slicing
/// at a fixed index (`trimmed[..6]`) panics when the cut lands inside a
/// multi-byte char — a console query can start with arbitrary Unicode — so
/// the prefix is taken with `get`, which yields None at a non-boundary.
pub(crate) fn starts_with_keyword(trimmed: &str, kw: &str) -> bool {
    trimmed
        .get(..kw.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(kw))
}

