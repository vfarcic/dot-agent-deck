use serde::Deserialize;

use crate::repo_identity;

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
}

fn current_version() -> semver::Version {
    semver::Version::parse(env!("DAD_VERSION")).expect("DAD_VERSION is valid semver")
}

fn should_notify(current: &semver::Version, latest_tag: &str) -> Option<String> {
    let stripped = latest_tag
        .strip_prefix('v')
        .or_else(|| latest_tag.strip_prefix('V'))
        .unwrap_or(latest_tag);
    let latest = semver::Version::parse(stripped).ok()?;
    if latest > *current {
        Some(latest.to_string())
    } else {
        None
    }
}

async fn fetch_latest_version() -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .ok()?;

    let resp = client
        .get(repo_identity::RELEASES_API_URL)
        .header(
            "User-Agent",
            concat!("dot-agent-deck/", env!("DAD_VERSION")),
        )
        .send()
        .await
        .ok()?;

    let release: GitHubRelease = resp.json().await.ok()?;
    Some(release.tag_name)
}

/// Returns the latest version string if a newer release exists, `None` otherwise.
/// All errors are silently swallowed — this must never block or crash the app.
pub async fn check_for_update() -> Option<String> {
    let current = current_version();
    let tag = fetch_latest_version().await?;
    should_notify(&current, &tag)
}

/// Pull the version number out of `dot-agent-deck --version` output.
///
/// The one parser for that output (PRD #1487 merged the two copies `remote.rs`
/// and `connect.rs` each carried). Strict on purpose, because callers use the
/// parse to tell "this really is dot-agent-deck" from "some other binary sits
/// at the same path" — `connect`'s probe, `remote`'s install check, and the
/// daemon's verification of the build it is about to restart onto. Requires:
///
/// 1. The first whitespace token to be exactly `dot-agent-deck`.
/// 2. The second token to start with a digit (after an optional `v`) and
///    contain a `.` — a cheap-but-sufficient sanity check that catches
///    "hello world" while accepting both `0.24.5` and `v0.24.5-rc.1`.
///
/// Returns the version token verbatim; callers compare strings.
pub(crate) fn parse_version_output(stdout: &str) -> Option<String> {
    let mut parts = stdout.split_whitespace();
    let prog = parts.next()?;
    if prog != "dot-agent-deck" {
        return None;
    }
    let version = parts.next()?;
    let stripped = version.strip_prefix('v').unwrap_or(version);
    let first = stripped.chars().next()?;
    if !first.is_ascii_digit() {
        return None;
    }
    if !stripped.contains('.') {
        return None;
    }
    Some(version.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_current_version_parses() {
        let v = current_version();
        assert!(!v.to_string().is_empty());
    }

    #[test]
    fn test_should_notify_newer() {
        let current = semver::Version::new(0, 1, 0);
        assert_eq!(should_notify(&current, "0.2.0"), Some("0.2.0".into()));
    }

    #[test]
    fn test_should_notify_same() {
        let current = semver::Version::new(0, 1, 0);
        assert_eq!(should_notify(&current, "0.1.0"), None);
    }

    #[test]
    fn test_should_notify_older() {
        let current = semver::Version::new(0, 1, 0);
        assert_eq!(should_notify(&current, "0.0.9"), None);
    }

    #[test]
    fn test_v_prefix_stripped() {
        let current = semver::Version::new(0, 1, 0);
        assert_eq!(should_notify(&current, "v0.2.0"), Some("0.2.0".into()));
        assert_eq!(should_notify(&current, "V0.2.0"), Some("0.2.0".into()));
    }

    #[test]
    fn test_invalid_version_returns_none() {
        let current = semver::Version::new(0, 1, 0);
        assert_eq!(should_notify(&current, "not-a-version"), None);
    }
}
