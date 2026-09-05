//! Saved connections configuration and management.
//! See FR-1 & NFR-11/15: permission 0600, atomic writes, env-var resolution ($ENV:VAR).

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

/// Type of database connection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DriverType {
    MySql,
    Postgres,
    SqlServer,
    Sqlite,
    Redis, // redis
}

impl DriverType {
    pub fn default_port(&self) -> u16 {
        match self {
            DriverType::MySql => 3306,
            DriverType::Postgres => 5432,
            DriverType::SqlServer => 1433,
            DriverType::Sqlite => 0,
            DriverType::Redis => 6379, // redis
        }
    }
}

/// SSL enforcement level for a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SslMode {
    Disable,
    Require,
    /// Require TLS + verify the server certificate.
    Verify,
}

/// Tolerant deserializer for `ssl_mode`: unknown spellings (libpq/sqlx
/// variants like "verify-full", "prefer", "verify_ca") fall back to `None`
/// (driver default) instead of failing the whole config parse — a bad value
/// must never empty the connection list or enable data-loss overwrites.
fn de_ssl_mode_opt<'de, D>(d: D) -> Result<Option<SslMode>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = Option::<String>::deserialize(d)?;
    Ok(s.and_then(|v| match v.to_lowercase().as_str() {
        "require" | "required" => Some(SslMode::Require),
        "verify" | "verify-full" | "verify_ca" | "verify_identity" | "verify_full" => {
            Some(SslMode::Verify)
        }
        _ => None,
    }))
}

/// A saved connection entry in config.toml.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConnectionConfig {
    pub name: String,
    pub driver: DriverType,
    #[serde(default = "default_host")]
    pub host: String,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub password: Option<String>,
    pub database: Option<String>,
    pub socket: Option<String>,
    #[serde(default)]
    pub ssl: bool,
    /// TLS enforcement. Missing/unknown → driver default (opportunistic TLS).
    /// The legacy `ssl: true` boolean is honoured when `ssl_mode` is unset.
    #[serde(default, deserialize_with = "de_ssl_mode_opt")]
    pub ssl_mode: Option<SslMode>,
    /// Optional SSH bastion: when present, the driver connects through a
    /// local port forwarded by a spawned `ssh -L` process instead of
    /// reaching `host` directly. Config file only (not in the form modal);
    /// edit sessions preserve the section untouched.
    #[serde(default)]
    pub ssh: Option<SshConfig>,
}

/// SSH tunnel settings — the `[connections.ssh]` table in config.toml.
/// Authentication (agent, keys, `~/.ssh/config`) is delegated to the system
/// `ssh` binary; dbx never handles SSH credentials itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshConfig {
    /// Bastion host (may also be an alias from `~/.ssh/config`).
    pub host: String,
    /// SSH port. Missing → 22.
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    /// SSH user. Missing → ssh's own default (config file or login name).
    pub user: Option<String>,
    /// Private key passed as `ssh -i` (`~` is expanded). Missing → ssh's own
    /// key lookup (agent, `~/.ssh/config` IdentityFile).
    pub identity_file: Option<String>,
    /// Loopback port the forward listens on. `0`/missing → a free port is
    /// picked at connect time.
    #[serde(default)]
    pub local_port: u16,
}

// By hand so the port matches the serde default (22): a derived Default
// would give port 0, and `ssh -p 0` is nonsense.
impl Default for SshConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: default_ssh_port(),
            user: None,
            identity_file: None,
            local_port: 0,
        }
    }
}

fn default_ssh_port() -> u16 {
    22
}

impl ConnectionConfig {
    /// Effective SSL mode: an explicit `ssl_mode` wins; otherwise the legacy
    /// `ssl: true` boolean maps to `Require`; otherwise `None` (driver default).
    pub fn effective_ssl_mode(&self) -> Option<SslMode> {
        self.ssl_mode.or_else(|| self.ssl.then_some(SslMode::Require))
    }
}

/// Default connection host when a config entry omits it. Local dev
/// databases conventionally live on loopback; override per-machine with
/// `DBX_DEFAULT_HOST`.
pub const DEFAULT_HOST: &str = "127.0.0.1";

fn default_host() -> String {
    std::env::var("DBX_DEFAULT_HOST").unwrap_or_else(|_| DEFAULT_HOST.to_string())
}

impl ConnectionConfig {
    /// Resolves password (expands `$ENV:VAR_NAME` if specified)
    pub fn resolve_password(&self) -> Option<String> {
        let raw = self.password.as_ref()?;
        if let Some(var_name) = raw.strip_prefix("$ENV:") {
            std::env::var(var_name).ok()
        } else {
            Some(raw.clone())
        }
    }

    /// Display string for connection list (redacting password).
    pub fn display_url(&self) -> String {
        let user_part = self.user.as_deref().unwrap_or("root");
        let base = if let Some(sock) = &self.socket {
            format!("{}://{}@unix({})", self.driver_str(), user_part, sock)
        } else {
            let port = self.port.unwrap_or_else(|| self.driver.default_port());
            format!("{}://{}@{}:{}", self.driver_str(), user_part, self.host, port)
        };
        match &self.ssh {
            Some(ssh) if !ssh.host.trim().is_empty() => {
                format!("{base} via ssh:{}", ssh.host.trim())
            }
            _ => base,
        }
    }

    fn driver_str(&self) -> &'static str {
        match self.driver {
            DriverType::MySql => "mysql",
            DriverType::Postgres => "postgres",
            DriverType::SqlServer => "sqlserver",
            DriverType::Sqlite => "sqlite",
            DriverType::Redis => "redis", // redis
        }
    }
}

/// Root structure of ~/.config/dbx/config.toml.
/// Default rows per page when a table tab is opened. Configurable via the
/// `page_size` key in config.toml.
const DEFAULT_PAGE_SIZE: u64 = 50;

fn default_page_size() -> u64 {
    DEFAULT_PAGE_SIZE
}

/// A single saved query within a [`QueryCollection`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SavedQuery {
    pub name: String,
    pub sql: String,
    /// Optional description (not yet surfaced in the UI — reserved).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

/// A named collection of saved queries (e.g. "reporting", "migrations").
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct QueryCollection {
    pub name: String,
    #[serde(default)]
    pub queries: Vec<SavedQuery>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub connections: Vec<ConnectionConfig>,
    #[serde(default)]
    pub settings: HashMap<String, String>,
    /// Rows fetched per page in the data grid. `0` (or missing) falls back
    /// to [`DEFAULT_PAGE_SIZE`].
    #[serde(default = "default_page_size")]
    pub page_size: u64,
    /// Organized saved queries: named collections → list of saved queries.
    #[serde(default)]
    pub query_collections: Vec<QueryCollection>,
    /// Legacy flat favorites. Kept only to migrate pre-collections configs on
    /// load; emptied by the migration and never re-serialized once empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub favorites: Vec<(String, String)>,
    /// Last executed queries per connection name (most recent first).
    #[serde(default)]
    pub query_history: HashMap<String, Vec<String>>,
    /// Colour palette: `dark` (default) or `light` for bright terminals.
    #[serde(default)]
    pub theme: crate::theme::ThemeName,
}

impl AppConfig {
    /// Record a successfully-executed query in `conn`'s history (dedup, cap).
    pub fn push_history(&mut self, conn: &str, query: &str) {
        let q = query.trim();
        if q.is_empty() {
            return;
        }
        let history = self.query_history.entry(conn.to_string()).or_default();
        // Most recent first; drop the previous occurrence of the same query.
        history.retain(|h| h != q);
        history.insert(0, q.to_string());
        history.truncate(50);
    }

    /// Move legacy flat `favorites` into a "Default" collection (one-time,
    /// on load). Clears `favorites` so it isn't re-serialized afterwards.
    pub fn migrate_legacy_favorites(&mut self) {
        let legacy = std::mem::take(&mut self.favorites);
        for (name, sql) in legacy {
            // Reuses `save_query` so names are de-duplicated against anything
            // already present in the "Default" collection.
            self.save_query("Default", &name, &sql);
        }
    }

    /// Save a query into the named collection (creating it if needed), with
    /// name de-duplication within that collection. Returns the final
    /// `(collection, name)`.
    pub fn save_query(
        &mut self,
        collection: &str,
        name: &str,
        sql: &str,
    ) -> (String, String) {
        let col = if let Some(c) = self
            .query_collections
            .iter_mut()
            .find(|c| c.name == collection)
        {
            c
        } else {
            self.query_collections.push(QueryCollection {
                name: collection.to_string(),
                queries: Vec::new(),
            });
            self.query_collections.last_mut().expect("just pushed")
        };
        // Disambiguate duplicates so a later save never overwrites an earlier.
        let mut final_name = name.to_string();
        let mut i = 2;
        while col.queries.iter().any(|q| q.name == final_name) {
            final_name = format!("{name} ({i})");
            i += 1;
        }
        col.queries.push(SavedQuery {
            name: final_name.clone(),
            sql: sql.to_string(),
            description: String::new(),
        });
        (collection.to_string(), final_name)
    }

    /// Remove a query by `(collection, name)`, dropping its collection when it
    /// becomes empty. Returns `true` if a query was removed.
    pub fn delete_query(&mut self, collection: &str, name: &str) -> bool {
        let mut removed = false;
        if let Some(col) = self
            .query_collections
            .iter_mut()
            .find(|c| c.name == collection)
        {
            let before = col.queries.len();
            col.queries.retain(|q| q.name != name);
            removed = col.queries.len() != before;
        }
        if removed {
            self.query_collections.retain(|c| !c.queries.is_empty());
        }
        removed
    }
}

impl AppConfig {
    /// Resolve the configured page size, treating an absent/zero value as
    /// the default (a `0` would otherwise mean "fetch nothing").
    pub fn effective_page_size(&self) -> u64 {
        if self.page_size == 0 {
            DEFAULT_PAGE_SIZE
        } else {
            self.page_size
        }
    }

    /// Determines the config file path following XDG precedence:
    /// 1. `--config <path>` override
    /// 2. `$DBX_CONFIG` env var
    /// 3. `$XDG_CONFIG_HOME/dbx/config.toml`
    /// 4. `~/.config/dbx/config.toml`
    pub fn default_path(cli_override: Option<&Path>) -> PathBuf {
        if let Some(p) = cli_override {
            return p.to_path_buf();
        }
        if let Ok(env_path) = std::env::var("DBX_CONFIG") {
            return PathBuf::from(env_path);
        }
        if let Some(config_dir) = dirs::config_dir() {
            return config_dir.join("dbx").join("config.toml");
        }
        PathBuf::from(".config/dbx/config.toml")
    }

    /// Loads configuration from file or returns an empty default if not found.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read config from {}", path.display()))?;
        let mut config: AppConfig = toml::from_str(&content)
            .with_context(|| format!("failed to parse TOML config at {}", path.display()))?;
        config.migrate_legacy_favorites();
        Ok(config)
    }

    /// Atomically saves configuration to disk with 0600 permissions.
    pub fn save(&self, path: &Path) -> Result<()> {
        let parent = path.parent().ok_or_else(|| anyhow!("invalid config path"))?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create config directory {}", parent.display()))?;

        let toml_str = toml::to_string_pretty(self)
            .context("failed to serialize config to TOML")?;

        let tmp_path = parent.join(format!(".config.tmp.{}", std::process::id()));

        // Write with 0600 permissions (read/write only by owner)
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp_path)
                .with_context(|| format!("failed to open temp config {}", tmp_path.display()))?;
            file.write_all(toml_str.as_bytes())?;
            file.sync_all()?;
        }

        #[cfg(not(unix))]
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp_path)?;
            file.write_all(toml_str.as_bytes())?;
            file.sync_all()?;
        }

        fs::rename(&tmp_path, path)
            .with_context(|| format!("failed to atomically replace config at {}", path.display()))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_push_history_dedup_and_cap() {
        let mut cfg = AppConfig::default();
        cfg.push_history("dev", "SELECT 1");
        cfg.push_history("dev", "SELECT 2");
        cfg.push_history("dev", "SELECT 1"); // moves to front, dedups
        let h = cfg.query_history.get("dev").unwrap();
        assert_eq!(h, &vec!["SELECT 1".to_string(), "SELECT 2".to_string()]);

        // Empty queries are ignored.
        cfg.push_history("dev", "   ");
        assert_eq!(cfg.query_history.get("dev").unwrap().len(), 2);

        // Different connections are isolated.
        cfg.push_history("prod", "SELECT * FROM big");
        assert_eq!(cfg.query_history.get("prod").unwrap().len(), 1);
        assert_eq!(cfg.query_history.get("dev").unwrap().len(), 2);
    }

    #[test]
    fn test_migrate_legacy_favorites() {
        let mut cfg = AppConfig::default();
        cfg.favorites = vec![
            ("users".to_string(), "SELECT * FROM users".to_string()),
            ("orders".to_string(), "SELECT * FROM orders".to_string()),
        ];
        cfg.migrate_legacy_favorites();
        // Legacy field is cleared so it isn't re-serialized.
        assert!(cfg.favorites.is_empty());
        assert_eq!(cfg.query_collections.len(), 1);
        assert_eq!(cfg.query_collections[0].name, "Default");
        assert_eq!(cfg.query_collections[0].queries.len(), 2);
    }

    #[test]
    fn test_save_query_dedup_and_delete() {
        let mut cfg = AppConfig::default();
        let (col, name) = cfg.save_query("Default", "users", "SELECT 1");
        assert_eq!((col.as_str(), name.as_str()), ("Default", "users"));
        // Duplicate name within the collection is disambiguated.
        let (_, name2) = cfg.save_query("Default", "users", "SELECT 2");
        assert_eq!(name2, "users (2)");
        // A different collection is created on demand.
        cfg.save_query("reporting", "daily", "SELECT 3");
        assert_eq!(cfg.query_collections.len(), 2);

        // Delete removes the query; deleting the last query drops the collection.
        assert!(cfg.delete_query("Default", "users"));
        assert!(!cfg.delete_query("Default", "users")); // already gone
        assert!(cfg.delete_query("Default", "users (2)"));
        assert_eq!(
            cfg.query_collections.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["reporting"]
        );
    }
}
