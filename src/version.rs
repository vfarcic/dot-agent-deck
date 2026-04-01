use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const CACHE_TTL_HOURS: i64 = 24;
const GITHUB_RELEASES_URL: &str =
    "https://api.github.com/repos/vfarcic/dot-agent-deck/releases/latest";

#[derive(Serialize, Deserialize)]
struct VersionCache {
    latest_version: String,
    checked_at: DateTime<Utc>,
}

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
}

fn current_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION")).expect("CARGO_PKG_VERSION is valid semver")
}

fn cache_path() -> PathBuf {
    crate::config::dirs_home().join(".config/dot-agent-deck/version-check.json")
}

fn read_cache_from(path: &Path) -> Option<VersionCache> {
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

fn write_cache_to(path: &Path, cache: &VersionCache) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, serde_json::to_string(cache).unwrap_or_default());
}

fn is_cache_fresh(cache: &VersionCache) -> bool {
    Utc::now()
        .signed_duration_since(cache.checked_at)
        .num_hours()
        < CACHE_TTL_HOURS
}

fn should_notify(current: &semver::Version, latest_tag: &str) -> Option<String> {
    let stripped = latest_tag.strip_prefix('v').or(latest_tag.strip_prefix('V')).unwrap_or(latest_tag);
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
        .get(GITHUB_RELEASES_URL)
        .header("User-Agent", concat!("dot-agent-deck/", env!("CARGO_PKG_VERSION")))
        .send()
        .await
        .ok()?;

    let release: GitHubRelease = resp.json().await.ok()?;
    Some(release.tag_name)
}

/// Returns the latest version string if a newer release exists, `None` otherwise.
/// All errors are silently swallowed — this must never block or crash the app.
pub async fn check_for_update() -> Option<String> {
    let path = cache_path();
    let current = current_version();

    // Try cached result first
    if let Some(cache) = read_cache_from(&path)
        && is_cache_fresh(&cache)
    {
        return should_notify(&current, &cache.latest_version);
    }

    // Cache missing or expired — fetch from GitHub
    let tag = fetch_latest_version().await?;

    write_cache_to(
        &path,
        &VersionCache {
            latest_version: tag.clone(),
            checked_at: Utc::now(),
        },
    );

    should_notify(&current, &tag)
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

    #[test]
    fn test_cache_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("version-check.json");

        let cache = VersionCache {
            latest_version: "1.2.3".into(),
            checked_at: Utc::now(),
        };
        write_cache_to(&path, &cache);

        let loaded = read_cache_from(&path).unwrap();
        assert_eq!(loaded.latest_version, "1.2.3");
    }

    #[test]
    fn test_cache_fresh() {
        let cache = VersionCache {
            latest_version: "1.0.0".into(),
            checked_at: Utc::now() - chrono::Duration::hours(1),
        };
        assert!(is_cache_fresh(&cache));
    }

    #[test]
    fn test_cache_expired() {
        let cache = VersionCache {
            latest_version: "1.0.0".into(),
            checked_at: Utc::now() - chrono::Duration::hours(25),
        };
        assert!(!is_cache_fresh(&cache));
    }

    #[test]
    fn test_invalid_cache_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("version-check.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(read_cache_from(&path).is_none());
    }
}
