//! Redis driver implementation using redis-rs (async, tokio).
//!
//! Redis is a key-value store, not a relational engine, so the mapping onto
//! the explorer model is deliberately synthetic:
//!
//! * **Namespaces** are the logical databases `db0..dbN` (N from
//!   `CONFIG GET databases`, falling back to 16 when CONFIG is disabled or
//!   renamed on the server).
//! * **Collections** are *key prefixes*, not tables: a full `SCAN` groups
//!   keys by their first `:` segment (`user:42` → collection `user`); keys
//!   without a prefix land in a `(root)` collection.
//! * **Records** are keys in one prefix group: `key`, `type`, `ttl`
//!   (milliseconds, NULL = no expiry) and a bounded `value` preview.
//! * **execute** is a raw Redis command console: the text is tokenised
//!   shell-style (quotes/escapes honoured) into argv and sent as-is.
//!
//! Connection state is one `ConnectionManager` per logical database (a
//! manager pins a db index and multiplexes a single auto-reconnecting
//! connection), created lazily and cached.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;

use super::{
    Capabilities, Collection, CollectionMeta, CollectionRef, ColumnMeta, Driver, DriverInfo,
    Namespace, Page, QueryResult, Record, RecordPage, Value,
};
use crate::config::ConnectionConfig;

type ConnManager = ::redis::aio::ConnectionManager;

/// Name of the synthetic collection holding keys with no `:` prefix.
pub(crate) const ROOT_COLLECTION: &str = "(root)";
/// Fallback logical-database count when `CONFIG GET databases` fails
/// (the stock Redis default).
const DEFAULT_DB_COUNT: usize = 16;
/// SCAN batch size per round trip.
const SCAN_BATCH: u64 = 1000;
/// Safety cap on keys enumerated for one collection listing, so a fat db
/// can't hang the UI on a multi-million-key keyspace.
const MAX_ENUMERATED_KEYS: usize = 100_000;
/// Value previews are bounded so a giant string/hash never floods the grid.
/// The full value is one `GET <key>` away in the query console.
const PREVIEW_MAX_CHARS: usize = 200;
/// Items pulled per key when previewing a collection-typed value.
const PREVIEW_ITEMS: i64 = 5;

/// Which prefix group a key belongs to: the segment before the first `:`,
/// or `None` for keys with no (usable) prefix — the `(root)` collection.
fn key_group(key: &str) -> Option<&str> {
    key.split_once(':')
        .map(|(prefix, _)| prefix)
        .filter(|p| !p.is_empty())
}

/// Group scanned keys into `(collection_name, count)` pairs, sorted by name
/// (BTreeMap) so the tree order is stable across refreshes. Note `(root)`
/// sorts first — `(` precedes letters in byte order.
fn group_keys<'a>(keys: impl Iterator<Item = &'a str>) -> Vec<(String, u64)> {
    let mut groups: BTreeMap<String, u64> = BTreeMap::new();
    for key in keys {
        let name = key_group(key).unwrap_or(ROOT_COLLECTION).to_string();
        *groups.entry(name).or_default() += 1;
    }
    groups.into_iter().collect()
}

/// Escape Redis glob metacharacters so a key prefix can be embedded in a
/// `MATCH` pattern literally — a prefix containing `*` must not widen the
/// scan beyond its own group.
fn escape_match_pattern(literal: &str) -> String {
    let mut out = String::with_capacity(literal.len());
    for c in literal.chars() {
        if matches!(c, '*' | '?' | '[' | ']' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Tokenise a raw Redis command line into argv, shell-style: whitespace
/// splits, single quotes are literal, double quotes honour `\"` `\\` `\n`
/// `\t` escapes, and a backslash outside quotes escapes the next character.
/// Quotes are needed because Redis values routinely contain spaces.
pub(crate) fn parse_command_line(input: &str) -> Result<Vec<String>> {
    let mut argv = Vec::new();
    let mut cur = String::new();
    let mut in_token = false;
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            // Inside single quotes: everything literal until the close.
            '\'' => {
                in_token = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(ch) => cur.push(ch),
                        None => return Err(anyhow!("unterminated single quote in command")),
                    }
                }
            }
            // Inside double quotes: backslash escapes honoured.
            '"' => {
                in_token = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some('n') => cur.push('\n'),
                            Some('t') => cur.push('\t'),
                            Some('r') => cur.push('\r'),
                            Some(ch) => cur.push(ch),
                            None => return Err(anyhow!("unterminated escape in command")),
                        },
                        Some(ch) => cur.push(ch),
                        None => return Err(anyhow!("unterminated double quote in command")),
                    }
                }
            }
            '\\' => {
                in_token = true;
                match chars.next() {
                    Some(ch) => cur.push(ch),
                    None => return Err(anyhow!("dangling backslash at end of command")),
                }
            }
            c if c.is_whitespace() => {
                if in_token {
                    argv.push(std::mem::take(&mut cur));
                    in_token = false;
                }
            }
            c => {
                in_token = true;
                cur.push(c);
            }
        }
    }
    if in_token {
        argv.push(cur);
    }
    Ok(argv)
}

/// Map a Redis reply value onto the model-agnostic [`Value`]. Nested
/// containers (arrays, maps) are rendered inline — at the top level of a
/// command reply they become rows instead (see [`reply_to_result`]).
fn redis_value(v: &::redis::Value) -> Value {
    match v {
        ::redis::Value::Nil => Value::Null,
        ::redis::Value::Int(i) => Value::Int(*i),
        ::redis::Value::BulkString(bytes) => match String::from_utf8(bytes.clone()) {
            Ok(s) => Value::String(s),
            Err(_) => Value::Bytes(bytes.clone()),
        },
        ::redis::Value::SimpleString(s) => Value::String(s.clone()),
        ::redis::Value::Okay => Value::String("OK".to_string()),
        ::redis::Value::Double(d) => Value::Float(*d),
        ::redis::Value::Boolean(b) => Value::Bool(*b),
        ::redis::Value::BigNumber(n) => Value::Decimal(n.to_string()),
        ::redis::Value::VerbatimString { text, .. } => Value::String(text.clone()),
        ::redis::Value::Array(items) | ::redis::Value::Set(items) => {
            Value::String(inline_render_items(items.iter()))
        }
        ::redis::Value::Map(pairs) => {
            let inner = pairs
                .iter()
                .map(|(k, val)| format!("{}: {}", redis_value(k).display_str(), redis_value(val).display_str()))
                .collect::<Vec<_>>()
                .join(", ");
            Value::String(format!("{{{inner}}}"))
        }
        ::redis::Value::Attribute { data, .. } => redis_value(data),
        ::redis::Value::Push { data, .. } => Value::String(inline_render_items(data.iter())),
        // ServerError has no Display impl in redis 0.27 — Debug it is.
        ::redis::Value::ServerError(e) => Value::String(format!("{e:?}")),
    }
}

/// Render a sequence of reply values as `[a, b, c]` for one grid cell.
fn inline_render_items<'a>(items: impl Iterator<Item = &'a ::redis::Value>) -> String {
    let inner = items
        .map(|v| redis_value(v).display_str())
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{inner}]")
}

/// Truncate to a character boundary with an ellipsis note.
fn truncate_preview(s: &str) -> String {
    if s.chars().count() <= PREVIEW_MAX_CHARS {
        return s.to_string();
    }
    let cut: String = s.chars().take(PREVIEW_MAX_CHARS).collect();
    format!("{cut}… (truncated — full value via `GET` in the console)")
}

/// Build the `value` preview cell for one key from its type, element count
/// and sampled items (all fetched in one pipeline per page).
fn value_preview(kind: &str, len: Option<i64>, items: &::redis::Value) -> String {
    let len_part = len.map(|n| format!("len={n} ")).unwrap_or_default();
    let body = match kind {
        // Strings: the items slot carries the value itself.
        "string" => redis_value(items).display_str(),
        // Hash pairs arrive as a flat [field, value, field, value, …].
        "hash" => match items {
            ::redis::Value::Array(flat) => {
                let pairs = flat
                    .chunks(2)
                    .map(|kv| {
                        let f = redis_value(&kv[0]).display_str();
                        let v = kv.get(1).map(|v| redis_value(v).display_str()).unwrap_or_default();
                        format!("{f}: {v}")
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{{{pairs}}}")
            }
            other => redis_value(other).display_str(),
        },
        _ => redis_value(items).display_str(),
    };
    truncate_preview(&format!("{len_part}{body}"))
}

/// Parse the numeric db index out of a namespace name (`db3` → 3). A bare
/// number is accepted too so hand-built `CollectionRef`s keep working.
fn parse_db(ns: &Namespace) -> Result<i64> {
    let raw = ns.0.strip_prefix("db").unwrap_or(&ns.0);
    raw.parse::<i64>()
        .with_context(|| format!("'{ns}' is not a Redis namespace (expected dbN)"))
}

pub struct RedisDriver {
    /// Connection parameters with the db index zeroed; each cached manager
    /// gets a copy with its own db filled in.
    base: ::redis::ConnectionInfo,
    /// One manager per logical database, created on first touch.
    managers: tokio::sync::Mutex<HashMap<i64, ConnManager>>,
    info: DriverInfo,
}

impl RedisDriver {
    pub async fn connect(cfg: &ConnectionConfig) -> Result<Self> {
        let db: i64 = cfg
            .database
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .unwrap_or("0")
            .parse()
            .context("Redis 'database' field must be a numeric db index (e.g. \"0\")")?;

        let base = ::redis::ConnectionInfo {
            addr: ::redis::ConnectionAddr::Tcp(
                cfg.host.clone(),
                cfg.port.unwrap_or_else(|| cfg.driver.default_port()),
            ),
            redis: ::redis::RedisConnectionInfo {
                db,
                // `user` doubles as the ACL username (Redis 6+); empty means
                // the `default` user.
                username: cfg.user.clone().filter(|u| !u.is_empty()),
                password: cfg.resolve_password().filter(|p| !p.is_empty()),
                // RESP2: keeps replies in the Array/BulkString shapes this
                // driver parses, and every server since Redis 2 speaks it.
                protocol: ::redis::ProtocolVersion::RESP2,
            },
        };

        let manager = Self::open_manager(&base, db).await?;

        // Server version for the header; unknown is fine (INFO may be
        // renamed away on hardened servers).
        let server_version = match ::redis::cmd("INFO")
            .arg("server")
            .query_async(&mut manager.clone())
            .await
        {
            Ok(::redis::Value::BulkString(bytes)) => String::from_utf8_lossy(&bytes)
                .lines()
                .find_map(|l| l.strip_prefix("redis_version:"))
                .map(|v| v.trim().to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            _ => "unknown".to_string(),
        };

        let mut managers = HashMap::new();
        managers.insert(db, manager);

        Ok(Self {
            base,
            managers: tokio::sync::Mutex::new(managers),
            info: DriverInfo {
                name: "Redis".to_string(),
                server_version,
                query_language: "Redis command".to_string(),
            },
        })
    }

    async fn open_manager(base: &::redis::ConnectionInfo, db: i64) -> Result<ConnManager> {
        let mut info = base.clone();
        info.redis.db = db;
        let client = ::redis::Client::open(info)
            .context("invalid Redis connection parameters")?;
        ConnManager::new(client)
            .await
            .with_context(|| format!("failed to connect to Redis (db{db})"))
    }

    /// Manager for a logical database, opening and caching it on demand.
    async fn conn_for(&self, db: i64) -> Result<ConnManager> {
        let mut guard = self.managers.lock().await;
        if let Some(m) = guard.get(&db) {
            return Ok(m.clone());
        }
        let m = Self::open_manager(&self.base, db).await?;
        guard.insert(db, m.clone());
        Ok(m)
    }

    /// Full SCAN of one logical db (MATCH + COUNT batched), capped at
    /// [`MAX_ENUMERATED_KEYS`]. Returns the keys and whether the scan
    /// completed (false = truncated by the cap).
    async fn scan_keys(&self, db: i64, pattern: Option<&str>) -> Result<(Vec<String>, bool)> {
        let mut conn = self.conn_for(db).await?;
        let mut cursor: u64 = 0;
        let mut keys = Vec::new();
        loop {
            let mut cmd = ::redis::cmd("SCAN");
            cmd.arg(cursor).arg("COUNT").arg(SCAN_BATCH);
            if let Some(p) = pattern {
                cmd.arg("MATCH").arg(p);
            }
            let (next, batch): (u64, Vec<String>) = cmd
                .query_async(&mut conn)
                .await
                .context("SCAN failed")?;
            keys.extend(batch);
            cursor = next;
            if cursor == 0 || keys.len() >= MAX_ENUMERATED_KEYS {
                break;
            }
        }
        Ok((keys, cursor == 0))
    }

    /// Render one command reply into a `QueryResult` grid.
    fn reply_to_result(v: &::redis::Value, elapsed: Duration) -> QueryResult {
        let (columns, records) = match v {
            // Array of uniform 2-element arrays → key/value table (covers
            // CONFIG GET, HGETALL-style replies, XRANGE entries).
            ::redis::Value::Array(items)
                if !items.is_empty()
                    && items.iter().all(|i| matches!(i, ::redis::Value::Array(p) if p.len() == 2)) =>
            {
                let records = items
                    .iter()
                    .map(|i| match i {
                        ::redis::Value::Array(p) => Record {
                            values: vec![redis_value(&p[0]), redis_value(&p[1])],
                        },
                        _ => unreachable!("shape checked above"),
                    })
                    .collect::<Vec<_>>();
                (vec!["key".to_string(), "value".to_string()], records)
            }
            ::redis::Value::Array(items) | ::redis::Value::Set(items) => {
                let records = items
                    .iter()
                    .enumerate()
                    .map(|(i, item)| Record {
                        values: vec![Value::Int(i as i64), redis_value(item)],
                    })
                    .collect::<Vec<_>>();
                (vec!["idx".to_string(), "value".to_string()], records)
            }
            ::redis::Value::Map(pairs) => {
                let records = pairs
                    .iter()
                    .map(|(k, val)| Record {
                        values: vec![redis_value(k), redis_value(val)],
                    })
                    .collect::<Vec<_>>();
                (vec!["key".to_string(), "value".to_string()], records)
            }
            other => (
                vec!["result".to_string()],
                vec![Record {
                    values: vec![redis_value(other)],
                }],
            ),
        };
        let rows_affected = records.len() as u64;
        QueryResult {
            columns,
            records,
            rows_affected,
            execution_time: elapsed,
        }
    }

    /// `INFO` rendered as section/key/value rows — far more readable in the
    /// grid than the raw multi-kilobyte blob.
    fn info_to_result(text: &str, elapsed: Duration) -> QueryResult {
        let mut section = String::new();
        let mut records = Vec::new();
        for line in text.lines() {
            if let Some(name) = line.strip_prefix('#') {
                section = name.trim().to_string();
            } else if let Some((k, v)) = line.split_once(':') {
                records.push(Record {
                    values: vec![
                        Value::String(section.clone()),
                        Value::String(k.to_string()),
                        Value::String(v.to_string()),
                    ],
                });
            }
        }
        let rows_affected = records.len() as u64;
        QueryResult {
            columns: vec!["section".into(), "key".into(), "value".into()],
            records,
            rows_affected,
            execution_time: elapsed,
        }
    }

    /// `SLOWLOG GET` entries decoded into one row per slow command:
    /// `[id, timestamp, duration_us, argv, client_addr, client_name]`.
    fn slowlog_to_result(items: &[::redis::Value], elapsed: Duration) -> QueryResult {
        let mut records = Vec::new();
        for entry in items {
            let ::redis::Value::Array(fields) = entry else { continue };
            if fields.len() < 4 {
                continue;
            }
            let ts = match redis_value(&fields[1]) {
                Value::Int(secs) => Value::DateTime(
                    // Keep it dependency-free: epoch seconds + UTC marker.
                    format!("{secs} (unix)"),
                ),
                other => other,
            };
            records.push(Record {
                values: vec![
                    redis_value(&fields[0]),
                    ts,
                    redis_value(&fields[2]),
                    redis_value(&fields[3]),
                    fields.get(4).map(redis_value).unwrap_or(Value::Null),
                    fields.get(5).map(redis_value).unwrap_or(Value::Null),
                ],
            });
        }
        let rows_affected = records.len() as u64;
        QueryResult {
            columns: vec![
                "id".into(),
                "timestamp".into(),
                "duration_us".into(),
                "command".into(),
                "client_addr".into(),
                "client_name".into(),
            ],
            records,
            rows_affected,
            execution_time: elapsed,
        }
    }
}

#[async_trait]
impl Driver for RedisDriver {
    fn info(&self) -> DriverInfo {
        self.info.clone()
    }

    fn capabilities(&self) -> Capabilities {
        // No EDIT_DATA / DDL / ERD: writing is done through the command
        // console (execute), and key prefixes have no schema to diagram.
        Capabilities::BROWSE | Capabilities::QUERY_TEXT | Capabilities::PROCESS_LIST
    }

    async fn ping(&self) -> Result<Duration> {
        let start = Instant::now();
        let mut conn = self.conn_for(self.base.redis.db).await?;
        ::redis::cmd("PING")
            .query_async::<::redis::Value>(&mut conn)
            .await
            .context("Redis ping failed")?;
        Ok(start.elapsed())
    }

    /// Logical databases `db0..dbN`. N comes from `CONFIG GET databases`;
    /// when CONFIG is renamed/disabled (hardened servers) the stock default
    /// of 16 stands in so browsing still works.
    async fn namespaces(&self) -> Result<Vec<Namespace>> {
        let mut conn = self.conn_for(self.base.redis.db).await?;
        let reply = ::redis::cmd("CONFIG")
            .arg("GET")
            .arg("databases")
            .query_async::<::redis::Value>(&mut conn)
            .await;

        let count = reply
            .ok()
            .and_then(|v| match v {
                // RESP2: flat [name, value] pair.
                ::redis::Value::Array(pair) if pair.len() == 2 => {
                    match redis_value(&pair[1]) {
                        Value::String(s) => s.trim().parse::<usize>().ok(),
                        Value::Int(i) => usize::try_from(i).ok(),
                        _ => None,
                    }
                }
                // RESP3: a map.
                ::redis::Value::Map(pairs) => pairs.iter().find_map(|(k, v)| {
                    if redis_value(k).display_str().eq_ignore_ascii_case("databases") {
                        match redis_value(v) {
                            Value::String(s) => s.trim().parse::<usize>().ok(),
                            Value::Int(i) => usize::try_from(i).ok(),
                            _ => None,
                        }
                    } else {
                        None
                    }
                }),
                _ => None,
            })
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_DB_COUNT);

        Ok((0..count).map(|i| Namespace(format!("db{i}"))).collect())
    }

    /// Key-prefix groups of the db, computed from one full SCAN. Keys with
    /// no `:` prefix are reported under `(root)`.
    async fn collections(&self, ns: &Namespace) -> Result<Vec<Collection>> {
        let db = parse_db(ns)?;
        let (keys, complete) = self.scan_keys(db, None).await?;
        let groups = group_keys(keys.iter().map(String::as_str));
        Ok(groups
            .into_iter()
            .map(|(name, count)| Collection {
                name,
                // Truncated by the safety cap: counts are partial, so report
                // none rather than confidently-wrong numbers.
                estimated_row_count: complete.then_some(count),
                estimated_size_bytes: None,
            })
            .collect())
    }

    /// Every prefix collection shares the same synthetic shape.
    async fn collection_meta(&self, c: &CollectionRef) -> Result<CollectionMeta> {
        parse_db(&c.namespace)?;
        let col = |name: &str, data_type: &str, is_pk: bool, nullable: bool| ColumnMeta {
            name: name.to_string(),
            data_type: data_type.to_string(),
            is_nullable: nullable,
            is_primary_key: is_pk,
            is_unique: is_pk,
            is_foreign_key: false,
            extra: None,
        };
        Ok(CollectionMeta {
            reference: c.clone(),
            columns: vec![
                col("key", "String", true, false),
                col("type", "String", false, false),
                // PTTL in milliseconds; NULL = the key has no expiry.
                col("ttl", "Int (ms)", false, true),
                // Bounded preview; the full value is fetched via the console.
                col("value", "String (preview)", false, true),
            ],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        })
    }

    async fn records(&self, c: &CollectionRef, page: Page) -> Result<RecordPage> {
        let db = parse_db(&c.namespace)?;
        let pattern = if c.name == ROOT_COLLECTION {
            "*".to_string()
        } else {
            format!("{}:*", escape_match_pattern(&c.name))
        };
        let (mut keys, complete) = self.scan_keys(db, Some(&pattern)).await?;
        if c.name == ROOT_COLLECTION {
            keys.retain(|k| key_group(k).is_none());
        }
        // SCAN order is arbitrary; sort so paging is stable across refreshes.
        keys.sort();

        let total = complete.then_some(keys.len() as u64);
        let slice: Vec<String> = keys
            .into_iter()
            .skip(page.offset as usize)
            .take(page.limit as usize)
            .collect();

        let mut conn = self.conn_for(db).await?;

        // Pass 1: TYPE + PTTL for every key in the page (one round trip).
        let mut meta_pipe = ::redis::pipe();
        for k in &slice {
            meta_pipe.cmd("TYPE").arg(k).cmd("PTTL").arg(k);
        }
        let meta: Vec<::redis::Value> = if slice.is_empty() {
            Vec::new()
        } else {
            meta_pipe
                .query_async(&mut conn)
                .await
                .context("TYPE/PTTL pipeline failed")?
        };

        // Pass 2: length + sampled items per key (exactly two replies per
        // key, so chunking below stays aligned).
        let mut val_pipe = ::redis::pipe();
        let mut kinds: Vec<String> = Vec::with_capacity(slice.len());
        for (i, k) in slice.iter().enumerate() {
            let kind = match meta.get(2 * i) {
                Some(::redis::Value::SimpleString(s)) => s.clone(),
                _ => "unknown".to_string(),
            };
            kinds.push(kind.clone());
            match kind.as_str() {
                "string" => {
                    val_pipe.cmd("STRLEN").arg(k).cmd("GET").arg(k);
                }
                "list" => {
                    val_pipe.cmd("LLEN").arg(k).cmd("LRANGE").arg(k).arg(0).arg(PREVIEW_ITEMS - 1);
                }
                "set" => {
                    val_pipe.cmd("SCARD").arg(k).cmd("SRANDMEMBER").arg(k).arg(PREVIEW_ITEMS);
                }
                "zset" => {
                    val_pipe
                        .cmd("ZCARD")
                        .arg(k)
                        .cmd("ZRANGE")
                        .arg(k)
                        .arg(0)
                        .arg(PREVIEW_ITEMS - 1)
                        .arg("WITHSCORES");
                }
                "hash" => {
                    val_pipe.cmd("HLEN").arg(k).cmd("HGETALL").arg(k);
                }
                "stream" => {
                    val_pipe
                        .cmd("XLEN")
                        .arg(k)
                        .cmd("XRANGE")
                        .arg(k)
                        .arg("-")
                        .arg("+")
                        .arg("COUNT")
                        .arg(PREVIEW_ITEMS);
                }
                // Module/unknown type: OBJECT ENCODING works on any key, so
                // the preview still says something useful.
                _ => {
                    val_pipe.cmd("OBJECT").arg("ENCODING").arg(k);
                    val_pipe.cmd("OBJECT").arg("ENCODING").arg(k);
                }
            }
        }
        let vals: Vec<::redis::Value> = if slice.is_empty() {
            Vec::new()
        } else {
            val_pipe
                .query_async(&mut conn)
                .await
                .context("value preview pipeline failed")?
        };

        let mut records = Vec::with_capacity(slice.len());
        for (i, key) in slice.iter().enumerate() {
            let ttl = match meta.get(2 * i + 1) {
                // -1 = no expiry → NULL; -2 = key vanished mid-scan → NULL.
                Some(::redis::Value::Int(ms)) if *ms >= 0 => Value::Int(*ms),
                _ => Value::Null,
            };
            let len = match vals.get(2 * i) {
                Some(::redis::Value::Int(n)) => Some(*n),
                _ => None,
            };
            let items = vals.get(2 * i + 1).cloned().unwrap_or(::redis::Value::Nil);
            let preview = if kinds[i] == "unknown" {
                // The two OBJECT ENCODING replies are identical; show it.
                match redis_value(&items) {
                    Value::String(enc) => format!("(encoding: {enc})"),
                    _ => "(no preview)".to_string(),
                }
            } else {
                value_preview(&kinds[i], len, &items)
            };
            records.push(Record {
                values: vec![
                    Value::String(key.clone()),
                    Value::String(kinds[i].clone()),
                    ttl,
                    Value::String(preview),
                ],
            });
        }

        Ok(RecordPage {
            columns: vec!["key".into(), "type".into(), "ttl".into(), "value".into()],
            records,
            page: page.offset.checked_div(page.limit).unwrap_or(0),
            page_size: page.limit,
            total_records: total,
        })
    }

    /// Raw Redis command console. The text is tokenised shell-style into
    /// argv and forwarded verbatim — this is deliberately not SQL, and it
    /// doubles as the driver's CLI mode (INFO, SLOWLOG GET, GET, …).
    async fn execute(&self, ns: &Namespace, query: &str) -> Result<QueryResult> {
        let db = parse_db(ns)?;
        let argv = parse_command_line(query)?;
        if argv.is_empty() {
            return Err(anyhow!("empty command"));
        }
        let mut cmd = ::redis::cmd(&argv[0]);
        for arg in &argv[1..] {
            cmd.arg(arg);
        }
        let mut conn = self.conn_for(db).await?;
        let start = Instant::now();
        let reply: ::redis::Value = cmd
            .query_async(&mut conn)
            .await
            .with_context(|| format!("Redis command failed: {}", argv[0]))?;
        let elapsed = start.elapsed();

        let verb = argv[0].to_ascii_uppercase();
        let sub = argv.get(1).map(|s| s.to_ascii_uppercase());
        Ok(match (verb.as_str(), sub.as_deref(), &reply) {
            ("INFO", _, ::redis::Value::BulkString(bytes)) => {
                Self::info_to_result(&String::from_utf8_lossy(bytes), elapsed)
            }
            ("SLOWLOG", Some("GET"), ::redis::Value::Array(items)) => {
                Self::slowlog_to_result(items, elapsed)
            }
            _ => Self::reply_to_result(&reply, elapsed),
        })
    }

    /// Key prefixes have no DDL; this exists only to satisfy the trait (the
    /// UI gates the DDL action on the capability, which is off).
    async fn definition(&self, _c: &CollectionRef) -> Result<String> {
        anyhow::bail!("Redis key prefixes have no stored definition — browse or use the console")
    }

    /// `CLIENT LIST` doubles as the process list: one row per connected
    /// client. The `id` field is renamed to `pid` so the generic kill flow
    /// (which looks for a `pid` column) works unchanged.
    async fn process_list(&self) -> Result<QueryResult> {
        let mut conn = self.conn_for(self.base.redis.db).await?;
        let start = Instant::now();
        let reply = ::redis::cmd("CLIENT")
            .arg("LIST")
            .query_async::<::redis::Value>(&mut conn)
            .await
            .context("CLIENT LIST failed")?;
        let elapsed = start.elapsed();
        let ::redis::Value::BulkString(bytes) = reply else {
            anyhow::bail!("unexpected CLIENT LIST reply");
        };
        let text = String::from_utf8_lossy(&bytes);

        // First pass: union of field names, in first-seen order.
        let mut columns: Vec<String> = Vec::new();
        let mut rows: Vec<Vec<(String, String)>> = Vec::new();
        for line in text.lines() {
            let mut fields = Vec::new();
            for kv in line.split(' ') {
                if let Some((k, v)) = kv.split_once('=') {
                    let k = if k == "id" { "pid" } else { k }.to_string();
                    if !columns.contains(&k) {
                        columns.push(k.clone());
                    }
                    fields.push((k, v.to_string()));
                }
            }
            rows.push(fields);
        }

        let records = rows
            .into_iter()
            .map(|fields| Record {
                values: columns
                    .iter()
                    .map(|c| {
                        fields
                            .iter()
                            .find(|(k, _)| k == c)
                            .map(|(_, v)| Value::String(v.clone()))
                            .unwrap_or(Value::Null)
                    })
                    .collect(),
            })
            .collect::<Vec<_>>();
        let rows_affected = records.len() as u64;
        Ok(QueryResult {
            columns,
            records,
            rows_affected,
            execution_time: elapsed,
        })
    }

    /// `CLIENT KILL ID <id>` — the `pid` column carries the numeric id.
    async fn kill_process(&self, id: &str) -> Result<()> {
        let client_id: u64 = id
            .trim()
            .parse()
            .with_context(|| format!("'{id}' is not a Redis client id"))?;
        let mut conn = self.conn_for(self.base.redis.db).await?;
        let killed: i64 = ::redis::cmd("CLIENT")
            .arg("KILL")
            .arg("ID")
            .arg(client_id)
            .query_async(&mut conn)
            .await
            .with_context(|| format!("failed to kill Redis client {client_id}"))?;
        if killed == 0 {
            anyhow::bail!("no client with id {client_id}");
        }
        Ok(())
    }

    // Transactions, views, routines, sequences: trait defaults (unsupported /
    // empty). Redis MULTI is stateful per-connection and the console already
    // covers the scripting escape hatches (EVAL) directly.
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- key grouping -------------------------------------------------

    #[test]
    fn test_key_group() {
        assert_eq!(key_group("user:42"), Some("user"));
        assert_eq!(key_group("a:b:c"), Some("a"), "first segment wins");
        assert_eq!(key_group("plain"), None);
        assert_eq!(key_group(":orphan"), None, "empty prefix is root");
    }

    #[test]
    fn test_group_keys() {
        let keys = ["user:1", "user:2", "order:9", "plain", ":x", "sess:a:b"];
        let groups = group_keys(keys.into_iter());
        assert_eq!(
            groups,
            vec![
                (ROOT_COLLECTION.to_string(), 2),
                ("order".to_string(), 1),
                ("sess".to_string(), 1),
                ("user".to_string(), 2),
            ],
            "BTreeMap order: sorted by name; '(' precedes letters"
        );
    }

    // --- glob escaping -------------------------------------------------

    #[test]
    fn test_escape_match_pattern() {
        assert_eq!(escape_match_pattern("user"), "user");
        assert_eq!(escape_match_pattern("a*b?c[d]e\\f"), "a\\*b\\?c\\[d\\]e\\\\f");
    }

    // --- command-line tokenising ---------------------------------------

    #[test]
    fn test_parse_command_line_plain() {
        assert_eq!(
            parse_command_line("GET mykey").unwrap(),
            vec!["GET".to_string(), "mykey".to_string()]
        );
        assert!(parse_command_line("   ").unwrap().is_empty());
    }

    #[test]
    fn test_parse_command_line_quotes() {
        assert_eq!(
            parse_command_line("SET greeting \"hello world\"").unwrap(),
            vec!["SET".to_string(), "greeting".to_string(), "hello world".to_string()]
        );
        assert_eq!(
            parse_command_line("GET 'it''s'").unwrap(),
            vec!["GET".to_string(), "its".to_string()],
            "adjacent quoted chunks concatenate, like a shell"
        );
        assert_eq!(
            parse_command_line("SET k \"line\\nbreak\\t\\\"q\\\"\"").unwrap(),
            vec!["SET".to_string(), "k".to_string(), "line\nbreak\t\"q\"".to_string()]
        );
        // Backslash escapes a space outside quotes.
        assert_eq!(
            parse_command_line("GET my\\ key").unwrap(),
            vec!["GET".to_string(), "my key".to_string()]
        );
    }

    #[test]
    fn test_parse_command_line_errors() {
        assert!(parse_command_line("SET k \"unterminated").is_err());
        assert!(parse_command_line("SET k 'unterminated").is_err());
        assert!(parse_command_line("GET \\").is_err());
    }

    // --- reply → Value mapping ------------------------------------------

    #[test]
    fn test_redis_value_mapping() {
        use ::redis::Value as R;
        assert_eq!(redis_value(&R::Nil), Value::Null);
        assert_eq!(redis_value(&R::Int(42)), Value::Int(42));
        assert_eq!(
            redis_value(&R::BulkString(b"hello".to_vec())),
            Value::String("hello".to_string())
        );
        assert_eq!(
            redis_value(&R::BulkString(vec![0xff, 0x00])),
            Value::Bytes(vec![0xff, 0x00]),
            "non-UTF8 stays bytes"
        );
        assert_eq!(redis_value(&R::Okay), Value::String("OK".to_string()));
        assert_eq!(redis_value(&R::Double(1.5)), Value::Float(1.5));
        assert_eq!(redis_value(&R::Boolean(true)), Value::Bool(true));
        assert_eq!(
            redis_value(&R::Array(vec![R::Int(1), R::SimpleString("x".into())])),
            Value::String("[1, x]".to_string()),
            "nested containers render inline"
        );
        assert_eq!(
            redis_value(&R::Map(vec![(R::SimpleString("a".into()), R::Int(2))])),
            Value::String("{a: 2}".to_string())
        );
    }

    // --- reply → QueryResult grid ---------------------------------------

    #[test]
    fn test_reply_to_result_scalar() {
        let r = RedisDriver::reply_to_result(&::redis::Value::Int(7), Duration::ZERO);
        assert_eq!(r.columns, vec!["result"]);
        assert_eq!(r.records[0].values[0], Value::Int(7));
        assert_eq!(r.rows_affected, 1);
    }

    #[test]
    fn test_reply_to_result_array_and_pairs() {
        use ::redis::Value as R;
        let r = RedisDriver::reply_to_result(
            &R::Array(vec![R::BulkString(b"a".to_vec()), R::Int(1)]),
            Duration::ZERO,
        );
        assert_eq!(r.columns, vec!["idx", "value"]);
        assert_eq!(r.records.len(), 2);
        assert_eq!(r.records[1].values, vec![Value::Int(1), Value::Int(1)]);

        // Uniform 2-element arrays become a key/value table (CONFIG GET).
        let r = RedisDriver::reply_to_result(
            &R::Array(vec![
                R::Array(vec![R::BulkString(b"databases".to_vec()), R::BulkString(b"16".to_vec())]),
                R::Array(vec![R::BulkString(b"maxmemory".to_vec()), R::BulkString(b"0".to_vec())]),
            ]),
            Duration::ZERO,
        );
        assert_eq!(r.columns, vec!["key", "value"]);
        assert_eq!(r.records[0].values[0], Value::String("databases".to_string()));
        assert_eq!(r.records[1].values[1], Value::String("0".to_string()));
    }

    // --- INFO / SLOWLOG renderings --------------------------------------

    #[test]
    fn test_info_to_result() {
        let text = "# Server\r\nredis_version:7.2.0\r\nuptime_in_seconds:5\r\n# Clients\r\nconnected_clients:3\r\n";
        let r = RedisDriver::info_to_result(text, Duration::ZERO);
        assert_eq!(r.columns, vec!["section", "key", "value"]);
        assert_eq!(
            r.records[0].values,
            vec![
                Value::String("Server".into()),
                Value::String("redis_version".into()),
                Value::String("7.2.0".into())
            ]
        );
        assert_eq!(r.records[2].values[0], Value::String("Clients".into()));
    }

    #[test]
    fn test_slowlog_to_result() {
        use ::redis::Value as R;
        let items = vec![R::Array(vec![
            R::Int(12),
            R::Int(1_700_000_000),
            R::Int(321),
            R::Array(vec![R::BulkString(b"GET".to_vec()), R::BulkString(b"k".to_vec())]),
        ])];
        let r = RedisDriver::slowlog_to_result(&items, Duration::ZERO);
        assert_eq!(r.columns[0], "id");
        assert_eq!(r.records[0].values[0], Value::Int(12));
        assert_eq!(r.records[0].values[2], Value::Int(321));
        assert_eq!(r.records[0].values[3], Value::String("[GET, k]".into()));
    }

    // --- previews --------------------------------------------------------

    #[test]
    fn test_value_preview_shapes() {
        use ::redis::Value as R;
        let items = R::Array(vec![R::BulkString(b"a".to_vec()), R::BulkString(b"b".to_vec())]);
        assert_eq!(value_preview("list", Some(7), &items), "len=7 [a, b]");

        let flat = R::Array(vec![
            R::BulkString(b"f1".to_vec()),
            R::BulkString(b"v1".to_vec()),
            R::BulkString(b"f2".to_vec()),
            R::BulkString(b"v2".to_vec()),
        ]);
        assert_eq!(value_preview("hash", Some(2), &flat), "len=2 {f1: v1, f2: v2}");

        let s = R::BulkString(b"plain".to_vec());
        assert_eq!(value_preview("string", Some(5), &s), "len=5 plain");
    }

    #[test]
    fn test_truncate_preview_bounds_long_values() {
        let long = "x".repeat(PREVIEW_MAX_CHARS * 3);
        let out = truncate_preview(&long);
        assert!(out.len() > PREVIEW_MAX_CHARS);
        assert!(out.contains('…'), "truncation marker present");
        // Multibyte safety: truncation must cut on a char boundary.
        let wide = "é".repeat(PREVIEW_MAX_CHARS * 2);
        let out = truncate_preview(&wide);
        assert!(out.starts_with(&"é".repeat(PREVIEW_MAX_CHARS)));
    }

    #[test]
    fn test_parse_db() {
        assert_eq!(parse_db(&Namespace("db3".into())).unwrap(), 3);
        assert_eq!(parse_db(&Namespace("db0".into())).unwrap(), 0);
        assert_eq!(parse_db(&Namespace("7".into())).unwrap(), 7);
        assert!(parse_db(&Namespace("main".into())).is_err());
    }
}
