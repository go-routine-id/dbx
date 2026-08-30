//! Best-effort update check against the GitHub releases API.
//!
//! Runs once on startup (off the hot path); if a newer release exists a toast
//! is shown. Any network/parse error is silently ignored — the check must
//! never take down or slow the TUI.

use std::time::Duration;

/// The current build's version, from `Cargo.toml` (e.g. `0.1.0`).
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

const RELEASES_LATEST: &str = "https://api.github.com/repos/go-routine-id/dbx/releases/latest";

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
}
