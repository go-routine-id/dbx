//! SSH port-forward support: spawns the system `ssh` in `-N -L` mode so a
//! driver can reach a database behind a bastion. No SSH library dependency —
//! authentication (agent, keys, `~/.ssh/config`) is delegated to the user's
//! own ssh setup, and dbx never sees SSH credentials.

use std::process::Stdio;
use anyhow::{Context, Result, anyhow};
use tokio::process::{Child, Command};

use crate::config::{ConnectionConfig, DEFAULT_HOST};

/// How long to wait for the forward's loopback listener to come up.
const TUNNEL_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// A live `ssh -N -L` forward. Spawned with `kill_on_drop`, so the tunnel's
/// lifetime is exactly the guard's: the connection that owns the guard
/// dropping it (reconnect, back to picker, app exit) kills the ssh process.
pub struct SshTunnel {
    // Held purely for `kill_on_drop`: the guard dropping kills the forwarder.
    _child: Child,
    /// Loopback port the forward listens on — the driver connects here.
    pub local_port: u16,
}

impl SshTunnel {
    /// If `cfg` has an `[ssh]` section, spawn the forward and return the
    /// guard plus an effective config re-pointed at the loopback end.
    /// Without an `[ssh]` section this is a pass-through: the returned config
    /// is a clone of `cfg` and the guard is `None`.
    ///
    /// On any error the half-started ssh process is killed before returning.
    pub async fn establish(cfg: &ConnectionConfig) -> Result<(ConnectionConfig, Option<SshTunnel>)> {
        let Some(ssh) = &cfg.ssh else {
            return Ok((cfg.clone(), None));
        };
        if ssh.host.trim().is_empty() {
            return Err(anyhow!(
                "connection '{}': [ssh] section is missing a host",
                cfg.name
            ));
        }
        // A leading '-' would make ssh parse the value as its own option
        // (e.g. "-oProxyCommand=…" in a hostile config → command execution).
        for (what, value) in [("host", ssh.host.trim()), ("user", ssh.user.as_deref().unwrap_or("").trim())] {
            if value.starts_with('-') {
                return Err(anyhow!(
                    "connection '{}': [ssh] {what} must not start with '-'",
                    cfg.name
                ));
            }
        }
        let db_port = cfg.port.unwrap_or_else(|| cfg.driver.default_port());
        if db_port == 0 {
            return Err(anyhow!(
                "connection '{}': an SSH tunnel needs a TCP target (unix sockets and sqlite files are local already)",
                cfg.name
            ));
        }

        let local_port = if ssh.local_port != 0 {
            // An explicitly configured port must be free — otherwise the
            // readiness probe below would connect to whatever FOREIGN service
            // already listens there and report the tunnel as up.
            match std::net::TcpListener::bind((DEFAULT_HOST, ssh.local_port)) {
                Ok(listener) => drop(listener),
                Err(e) => {
                    return Err(anyhow!(
                        "connection '{}': tunnel local_port {} is already in use ({e})",
                        cfg.name,
                        ssh.local_port,
                    ));
                }
            }
            ssh.local_port
        } else {
            pick_free_port()?
        };

        let mut cmd = Command::new("ssh");
        cmd.arg("-N") // forward only — no remote command, no shell
            // Fail fast instead of silently running without the forward.
            .arg("-o").arg("ExitOnForwardFailure=yes")
            // Never block on an interactive password/host-key prompt — the
            // TUI has no way to answer it. Keys must come from the agent or
            // be passphrase-free; host keys from known_hosts.
            .arg("-o").arg("BatchMode=yes")
            .arg("-o").arg("StrictHostKeyChecking=accept-new")
            // Keep NAT/stateful firewalls from silently dropping an idle tunnel.
            .arg("-o").arg("ServerAliveInterval=15")
            .arg("-p").arg(ssh.port.to_string())
            .arg("-L").arg(format!("{local_port}:{}:{db_port}", cfg.host));
        if let Some(key) = &ssh.identity_file {
            cmd.arg("-i").arg(expand_tilde(key));
        }
        let target = match &ssh.user {
            Some(u) if !u.trim().is_empty() => format!("{}@{}", u.trim(), ssh.host.trim()),
            _ => ssh.host.trim().to_string(),
        };
        cmd.arg(target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            // Captured so an early exit (bad auth, unknown host, port taken)
            // surfaces ssh's own error message instead of a bare exit code.
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .context("failed to spawn `ssh` — is an OpenSSH client installed?")?;

        // Wait for the forward to come up: ssh binds the loopback listener
        // only after authenticating, so a successful TCP connect to it means
        // the tunnel is usable end to end. The probe connection itself is
        // forwarded to the database and immediately dropped — the same thing
        // any TCP health check does.
        let deadline = std::time::Instant::now() + TUNNEL_READY_TIMEOUT;
        loop {
            if let Some(status) = child.try_wait().context("failed to poll the ssh process")? {
                let mut msg = String::new();
                if let Some(mut err) = child.stderr.take() {
                    use tokio::io::AsyncReadExt;
                    let _ = err.read_to_string(&mut msg).await;
                }
                let detail = msg.trim();
                return Err(anyhow!(
                    "ssh tunnel to '{}' exited ({status}){}",
                    ssh.host.trim(),
                    if detail.is_empty() { String::new() } else { format!(": {detail}") },
                ));
            }
            match tokio::net::TcpStream::connect((DEFAULT_HOST, local_port)).await {
                Ok(_) => break,
                Err(e) if std::time::Instant::now() >= deadline => {
                    return Err(anyhow!(
                        "ssh tunnel to '{}' did not come up within {}s: {e}",
                        ssh.host.trim(),
                        TUNNEL_READY_TIMEOUT.as_secs(),
                    ));
                }
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
            }
        }

        // The probe connected — confirm it was our forwarder and not a
        // listener that raced us onto the port while ssh died trying.
        if let Some(status) = child.try_wait().context("failed to poll the ssh process")? {
            let mut msg = String::new();
            if let Some(mut err) = child.stderr.take() {
                use tokio::io::AsyncReadExt;
                let _ = err.read_to_string(&mut msg).await;
            }
            let detail = msg.trim();
            return Err(anyhow!(
                "ssh tunnel to '{}' exited ({status}){}",
                ssh.host.trim(),
                if detail.is_empty() { String::new() } else { format!(": {detail}") },
            ));
        }

        let mut eff = cfg.clone();
        eff.host = DEFAULT_HOST.to_string();
        eff.port = Some(local_port);
        // A unix socket is always local — combined with [ssh] it would make
        // the driver ignore the tunnel entirely (MySQL prefers the socket),
        // so the tunnel wins and the socket is dropped from the effective
        // config.
        eff.socket = None;
        // TLS (ssl/ssl_mode) still applies end to end *inside* the tunnel, so
        // it is intentionally left untouched.
        Ok((eff, Some(SshTunnel { _child: child, local_port })))
    }
}

/// Ask the OS for a free loopback port. There is an inherent race between
/// releasing this listener and ssh binding the port, but
/// `ExitOnForwardFailure` turns a lost race into a clear error instead of a
/// silent misroute.
fn pick_free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind((DEFAULT_HOST, 0))
        .context("failed to allocate a local port for the SSH tunnel")?;
    Ok(listener.local_addr()?.port())
}

fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest).to_string_lossy().into_owned();
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DriverType, SshConfig};

    fn base_cfg() -> ConnectionConfig {
        ConnectionConfig {
            name: "prod".to_string(),
            driver: DriverType::Postgres,
            host: "db.internal".to_string(),
            port: Some(5432),
            user: Some("app".to_string()),
            password: None,
            database: Some("appdb".to_string()),
            socket: None,
            ssl: false,
            ssl_mode: None,
            ssh: None,
        }
    }

    #[tokio::test]
    async fn test_establish_without_ssh_is_a_passthrough() {
        let cfg = base_cfg();
        let (eff, tunnel) = SshTunnel::establish(&cfg).await.unwrap();
        assert!(tunnel.is_none());
        assert_eq!(eff.host, "db.internal");
        assert_eq!(eff.port, Some(5432));
    }

    #[tokio::test]
    async fn test_establish_rejects_an_empty_bastion_host() {
        let mut cfg = base_cfg();
        cfg.ssh = Some(SshConfig {
            host: "  ".to_string(),
            ..SshConfig::default()
        });
        let err = SshTunnel::establish(&cfg).await.map(|_| ()).unwrap_err();
        assert!(err.to_string().contains("missing a host"), "{err:#}");
    }

    #[tokio::test]
    async fn test_establish_rejects_a_non_tcp_target() {
        // sqlite has no port to forward to; tunnelling it would be nonsense.
        let mut cfg = base_cfg();
        cfg.driver = DriverType::Sqlite;
        cfg.port = None;
        cfg.ssh = Some(SshConfig {
            host: "bastion.example".to_string(),
            ..SshConfig::default()
        });
        let err = SshTunnel::establish(&cfg).await.map(|_| ()).unwrap_err();
        assert!(err.to_string().contains("TCP target"), "{err:#}");
    }

    #[tokio::test]
    async fn test_establish_rejects_values_that_look_like_ssh_options() {
        // A leading '-' would be parsed by ssh as its own flag — a hostile
        // config could smuggle "-oProxyCommand=…" into the command line.
        let mut cfg = base_cfg();
        cfg.ssh = Some(SshConfig {
            host: "-oProxyCommand=evil".to_string(),
            ..SshConfig::default()
        });
        let err = SshTunnel::establish(&cfg).await.map(|_| ()).unwrap_err();
        assert!(err.to_string().contains("must not start with '-'"), "{err:#}");

        let mut cfg = base_cfg();
        cfg.ssh = Some(SshConfig {
            host: "bastion.example".to_string(),
            user: Some("-F/evil/config".to_string()),
            ..SshConfig::default()
        });
        let err = SshTunnel::establish(&cfg).await.map(|_| ()).unwrap_err();
        assert!(err.to_string().contains("must not start with '-'"), "{err:#}");
    }

    #[test]
    fn test_pick_free_port_returns_a_bindable_port() {
        let port = pick_free_port().unwrap();
        assert!(port > 0);
        // The listener was released, so the port can be taken again.
        std::net::TcpListener::bind((DEFAULT_HOST, port)).unwrap();
    }

    #[test]
    fn test_expand_tilde() {
        let expanded = expand_tilde("~/.ssh/id_ed25519");
        assert!(!expanded.starts_with('~'));
        assert!(expanded.ends_with(".ssh/id_ed25519"));
        // Paths without a tilde pass through untouched.
        assert_eq!(expand_tilde("/etc/keys/id_rsa"), "/etc/keys/id_rsa");
    }
}
