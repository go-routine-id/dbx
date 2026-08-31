//! Update detection and self-update against the GitHub releases API.
//!
//! [`check_for_update`] runs once on startup (off the hot path); if a newer
//! release exists a toast is shown. Any network/parse error is silently
//! ignored — the check must never take down or slow the TUI.
//!
//! [`self_update`] backs `dbx --self-update`: it downloads the release asset
//! for the running platform and swaps it in for the running binary. Trust
//! model is HTTPS-to-GitHub — exactly what the documented `curl` upgrade does
//! by hand.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The current build's version, from `Cargo.toml` (e.g. `0.1.0`).
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

const RELEASES_LATEST: &str = "https://api.github.com/repos/go-routine-id/dbx/releases/latest";
/// Release assets are published under a stable `latest/download/<name>` path,
/// so the URL never has to be rebuilt per version.
const RELEASE_DOWNLOAD: &str = "https://github.com/go-routine-id/dbx/releases/latest/download";

/// Smallest plausible release binary (~10 MB today). Guards against saving an
/// HTML error page or a truncated download over the working binary.
const MIN_BINARY_BYTES: usize = 1024 * 1024;

/// Returns `Some(latest_version)` if a newer release than the current build
/// exists, or `None` when up-to-date / the check failed.
pub fn check_for_update() -> Option<String> {
    let body = ureq::get(RELEASES_LATEST)
        .set("User-Agent", "dbx-update-check")
        .set("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(5))
        .call()
        .ok()?
        .into_string()
        .ok()?;

    let value: serde_json::Value = serde_json::from_str(&body).ok()?;
    let tag = value
        .get("tag_name")?
        .as_str()?
        .trim_start_matches(['v', 'V'])
        .to_string();

    if is_newer(CURRENT_VERSION, &tag) {
        Some(tag)
    } else {
        None
    }
}

/// True if `latest` is strictly newer than `current`, comparing
/// `MAJOR[.MINOR[.PATCH]]` numerically. Tolerates a leading `v`/`V` and
/// pre-release/build suffixes (`-rc1`, `+build`), which are ignored for
/// ordering; missing trailing parts count as 0.
fn is_newer(current: &str, latest: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> {
        let s = s.trim_start_matches(['v', 'V']);
        let s = s.split(['-', '+']).next().unwrap_or("");
        s.split('.').filter_map(|p| p.parse::<u64>().ok()).collect()
    };
    let a = parse(current);
    let b = parse(latest);
    let n = a.len().max(b.len());
    for i in 0..n {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        if x != y {
            return y > x;
        }
    }
    false
}

/// Release asset name for a platform, or `None` when no build is published
/// for it. Kept parameterised so every supported target can be unit-tested
/// from any host.
fn asset_for(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("macos", "aarch64") => Some("dbx-macos-arm64"),
        ("macos", "x86_64") => Some("dbx-macos-x86_64"),
        ("linux", "x86_64") => Some("dbx-linux-x86_64"),
        ("windows", "x86_64") => Some("dbx-windows-x86_64.exe"),
        _ => None,
    }
}

/// Release asset name for the running platform.
fn asset_name() -> Option<&'static str> {
    asset_for(std::env::consts::OS, std::env::consts::ARCH)
}

/// Turn a permission error into an actionable message; pass anything else
/// through with context.
fn io_error_hint(e: &std::io::Error, dir: &Path) -> String {
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        format!(
            "no write permission for {} — re-run with `sudo dbx --self-update`",
            dir.display()
        )
    } else {
        format!("{e}")
    }
}

/// Download one release asset into memory.
fn download_asset(asset: &str) -> Result<Vec<u8>, String> {
    let url = format!("{RELEASE_DOWNLOAD}/{asset}");
    let resp = ureq::get(&url)
        .set("User-Agent", "dbx-self-update")
        // Binaries are ~10-14 MB; allow for a slow link.
        .timeout(Duration::from_secs(300))
        .call()
        .map_err(|e| format!("download failed: {e}"))?;

    let mut bytes = Vec::new();
    resp.into_reader()
        .read_to_end(&mut bytes)
        .map_err(|e| format!("failed to read download: {e}"))?;

    if bytes.len() < MIN_BINARY_BYTES {
        return Err(format!(
            "downloaded file is only {} bytes — refusing to install it",
            bytes.len()
        ));
    }
    Ok(bytes)
}

/// Path of the binary to replace, with symlinks resolved so we overwrite the
/// real file rather than the link pointing at it.
fn running_binary() -> Result<PathBuf, String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("cannot locate the running binary: {e}"))?;
    Ok(std::fs::canonicalize(&exe).unwrap_or(exe))
}

/// Swap `new_bytes` in for the running binary.
///
/// The staging file is written **into the same directory** as the target so
/// the final `rename` stays within one filesystem and is therefore atomic —
/// an interrupted update can never leave a half-written binary in place.
fn replace_binary(new_bytes: &[u8]) -> Result<(), String> {
    let exe = running_binary()?;
    let dir = exe
        .parent()
        .ok_or_else(|| "the running binary has no parent directory".to_string())?;

    let tmp = dir.join(format!(".dbx-update-{}.tmp", std::process::id()));
    std::fs::write(&tmp, new_bytes).map_err(|e| io_error_hint(&e, dir))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)) {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("failed to mark the new binary executable: {e}"));
        }
    }

    // On Windows a running executable cannot be overwritten, but it CAN be
    // renamed — move it aside and let the next self-update clean it up.
    #[cfg(windows)]
    let backup = {
        let backup = exe.with_extension("exe.old");
        let _ = std::fs::remove_file(&backup);
        if let Err(e) = std::fs::rename(&exe, &backup) {
            let _ = std::fs::remove_file(&tmp);
            return Err(io_error_hint(&e, dir));
        }
        backup
    };

    if let Err(e) = std::fs::rename(&tmp, &exe) {
        let _ = std::fs::remove_file(&tmp);
        // Put the old binary back so a failed update never leaves the user
        // without a working `dbx`.
        #[cfg(windows)]
        let _ = std::fs::rename(&backup, &exe);
        return Err(io_error_hint(&e, dir));
    }
    Ok(())
}

/// Download and install the latest release over the running binary.
///
/// Returns `Ok(None)` when already current, or `Ok(Some((from, to)))` after a
/// successful swap. Unlike [`check_for_update`] this reports errors loudly —
/// the user asked for it explicitly.
pub fn self_update() -> Result<Option<(String, String)>, String> {
    let Some(latest) = check_for_update() else {
        return Ok(None);
    };
    let asset = asset_name().ok_or_else(|| {
        format!(
            "no published build for {}/{} — install manually from {}",
            std::env::consts::OS,
            std::env::consts::ARCH,
            "https://github.com/go-routine-id/dbx/releases/latest"
        )
    })?;

    println!("downloading {asset} (v{latest})...");
    let bytes = download_asset(asset)?;
    replace_binary(&bytes)?;
    Ok(Some((CURRENT_VERSION.to_string(), latest)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_newer() {
        assert!(is_newer("0.1.0", "0.1.1"));
        assert!(is_newer("0.1.0", "0.2.0"));
        assert!(is_newer("0.1.0", "1.0.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.2.0", "0.1.9"));
        assert!(!is_newer("1.0.0", "0.9.9"));
        // Shorter-but-equal is not newer.
        assert!(!is_newer("0.1.0", "0.1"));
        // v/V prefix and pre-release suffixes are tolerated.
        assert!(is_newer("0.1.0", "v0.1.1"));
        assert!(is_newer("0.1.0", "V0.1.1"));
        assert!(is_newer("0.1.0", "0.1.1-rc1"));
        // Trailing zero parts are equal, not newer.
        assert!(!is_newer("0.1.0", "0.1.0.0"));
    }

    #[test]
    fn test_asset_for_every_published_target() {
        // These names are the release workflow's artifact names — they are a
        // contract between CI and self-update, so pin them.
        assert_eq!(asset_for("macos", "aarch64"), Some("dbx-macos-arm64"));
        assert_eq!(asset_for("macos", "x86_64"), Some("dbx-macos-x86_64"));
        assert_eq!(asset_for("linux", "x86_64"), Some("dbx-linux-x86_64"));
        assert_eq!(
            asset_for("windows", "x86_64"),
            Some("dbx-windows-x86_64.exe")
        );
        // Unpublished targets must be refused, not guessed at.
        assert_eq!(asset_for("linux", "aarch64"), None);
        assert_eq!(asset_for("freebsd", "x86_64"), None);
    }

    #[test]
    fn test_running_host_has_an_asset() {
        // Anything we can build and test on is a target we publish for, so a
        // missing mapping here means the release matrix and self-update have
        // drifted apart.
        assert!(
            asset_name().is_some(),
            "no release asset mapped for {}/{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
    }

    #[test]
    fn test_permission_error_suggests_sudo() {
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let msg = io_error_hint(&denied, Path::new("/usr/local/bin"));
        assert!(msg.contains("sudo dbx --self-update"), "got {msg}");
        // Other errors pass through without the misleading hint.
        let other = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert!(!io_error_hint(&other, Path::new("/tmp")).contains("sudo"));
    }
}
