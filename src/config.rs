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
}

impl DriverType {
    pub fn default_port(&self) -> u16 {
        match self {
            DriverType::MySql => 3306,
            DriverType::Postgres => 5432,
            DriverType::SqlServer => 1433,
            DriverType::Sqlite => 0,
        }
    }
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
        if let Some(sock) = &self.socket {
            format!("{}://{}@unix({})", self.driver_str(), user_part, sock)
        } else {
            let port = self.port.unwrap_or_else(|| self.driver.default_port());
            format!("{}://{}@{}:{}", self.driver_str(), user_part, self.host, port)
        }
    }

    fn driver_str(&self) -> &'static str {
        match self.driver {
            DriverType::MySql => "mysql",
            DriverType::Postgres => "postgres",
            DriverType::SqlServer => "sqlserver",
            DriverType::Sqlite => "sqlite",
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
        let config: AppConfig = toml::from_str(&content)
            .with_context(|| format!("failed to parse TOML config at {}", path.display()))?;
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
