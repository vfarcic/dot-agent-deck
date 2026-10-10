use serde::Deserialize;

use crate::repo_identity;

#[derive(Debug, Clone, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

/// Which releases a copy is offered.
///
/// `Stable` copies are offered only stable releases: GitHub's
/// `releases/latest`, which never names a prerelease or a draft. `Prerelease`
/// copies — the `dot-agent-deck-beta` Homebrew formula, or any build whose own
/// version is a SemVer prerelease — are offered the highest version among every
/// published release, prereleases included, so a beta sees the next beta.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseChannel {
    Stable,
    Prerelease,
}

impl ReleaseChannel {
    /// The channel a copy reporting `version` (with or without a leading `v`)
    /// follows on its version alone: `Prerelease` when it carries a SemVer
    /// prerelease suffix (`0.47.0-beta.1`), `Stable` otherwise, unparseable
    /// included.
    pub fn of_version(version: &str) -> Self {
        let version = version.strip_prefix('v').unwrap_or(version);
        match semver::Version::parse(version) {
            Ok(parsed) if !parsed.pre.is_empty() => Self::Prerelease,
            _ => Self::Stable,
        }
    }
}

pub(crate) fn current_version() -> semver::Version {
    semver::Version::parse(env!("DAD_VERSION")).expect("DAD_VERSION is valid semver")
}

pub(crate) fn should_notify(current: &semver::Version, latest_tag: &str) -> Option<String> {
    let latest = parse_tag(latest_tag)?;
    if latest > *current {
        Some(latest.to_string())
    } else {
        None
    }
}

fn parse_tag(tag: &str) -> Option<semver::Version> {
    let stripped = tag
        .strip_prefix('v')
        .or_else(|| tag.strip_prefix('V'))
        .unwrap_or(tag);
    semver::Version::parse(stripped).ok()
}

async fn fetch_latest_version(channel: ReleaseChannel) -> Option<String> {
    fetch_release_tag(
        channel,
        repo_identity::RELEASES_API_URL,
        repo_identity::RELEASES_LIST_API_URL,
    )
    .await
    .ok()
}

/// The tag of the newest release on `channel`, or why it could not be read:
/// `latest_url` (a GitHub `releases/latest` endpoint) answers for `Stable`,
/// `list_url` (a GitHub `releases` list endpoint) for `Prerelease`. The
/// startup nudge uses it and ignores the error.
pub(crate) async fn fetch_release_tag(
    channel: ReleaseChannel,
    latest_url: &str,
    list_url: &str,
) -> Result<String, String> {
    fetch_release_tags(channel, latest_url, list_url, false)
        .await
        .map(|(latest, _)| latest)
}

/// The tag of the newest release on `channel`, as [`fetch_release_tag`]
/// reads it, and the tag of the newest prerelease ([`newest_prerelease_tag`]).
/// The prerelease channel reads the list anyway; the stable channel reads it
/// as well only with `with_prerelease`, and otherwise answers `None` for it.
/// `dot-agent-deck upgrade` and the desktop app (`crate::self_upgrade`) use it
/// and report the error.
pub(crate) async fn fetch_release_tags(
    channel: ReleaseChannel,
    latest_url: &str,
    list_url: &str,
    with_prerelease: bool,
) -> Result<(String, Option<String>), String> {
    match channel {
        ReleaseChannel::Stable => {
            let latest = stable_tag(get_json(latest_url).await?)?;
            let prerelease = if with_prerelease {
                newest_prerelease_tag(&get_json::<Vec<GitHubRelease>>(list_url).await?)
            } else {
                None
            };
            Ok((latest, prerelease))
        }
        ReleaseChannel::Prerelease => {
            let releases = get_json::<Vec<GitHubRelease>>(list_url).await?;
            Ok((newest_tag(&releases)?, newest_prerelease_tag(&releases)))
        }
    }
}

async fn get_json<T: serde::de::DeserializeOwned>(api_url: &str) -> Result<T, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;

    let resp = client
        .get(api_url)
        .header(
            "User-Agent",
            concat!("dot-agent-deck/", env!("DAD_VERSION")),
        )
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("{api_url} answered {}", resp.status()));
    }

    resp.json().await.map_err(|e| e.to_string())
}

/// `release`'s tag when it is a stable release. `releases/latest` never
/// answers with a prerelease or a draft; this holds the stable channel to that
/// in our own code rather than only in GitHub's.
fn stable_tag(release: GitHubRelease) -> Result<String, String> {
    let is_prerelease = parse_tag(&release.tag_name).is_some_and(|v| !v.pre.is_empty());
    if release.draft || release.prerelease || is_prerelease {
        return Err(format!("`{}` is not a stable release", release.tag_name));
    }
    Ok(release.tag_name)
}

/// The tag with the highest version among `releases`, prereleases included and
/// drafts skipped. A tag that is not a version is ignored.
fn newest_tag(releases: &[GitHubRelease]) -> Result<String, String> {
    releases
        .iter()
        .filter(|release| !release.draft)
        .filter_map(|release| Some((parse_tag(&release.tag_name)?, &release.tag_name)))
        .max_by(|(a, _), (b, _)| a.cmp(b))
        .map(|(_, tag)| tag.clone())
        .ok_or_else(|| "no published release names a version".to_string())
}

/// The tag with the highest version among `releases` that carries a SemVer
/// prerelease suffix, drafts skipped: what the `dot-agent-deck-beta` Homebrew
/// formula receives (`release.yml`'s "Detect channel" routes a version with a
/// `-` in it there). `None` when no such release is listed.
fn newest_prerelease_tag(releases: &[GitHubRelease]) -> Option<String> {
    releases
        .iter()
        .filter(|release| !release.draft)
        .filter_map(|release| Some((parse_tag(&release.tag_name)?, &release.tag_name)))
        .filter(|(version, _)| !version.pre.is_empty())
        .max_by(|(a, _), (b, _)| a.cmp(b))
        .map(|(_, tag)| tag.clone())
}

/// Returns the latest version string if a newer release exists, `None` otherwise.
/// All errors are silently swallowed — this must never block or crash the app.
///
/// An `e2e` build does not ask. The L2 harness drives the real binary on a
/// machine with network access, so whether the badge appears depended on
/// whether GitHub answered that runner's unauthenticated request; when it
/// did, the badge covered the right end of the footer that tests match on
/// (`visibility_001` on PR #1617's CI). Gated on the feature rather than an
/// env var for the reason `effective_current_exe` in `src/platform/paths.rs` gives: a
/// release build carries no switch to turn the check off.
pub async fn check_for_update() -> Option<String> {
    if cfg!(feature = "e2e") {
        return None;
    }
    let current = current_version();
    let tag = fetch_latest_version(ReleaseChannel::of_version(&current.to_string())).await?;
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

    fn release(tag: &str, draft: bool, prerelease: bool) -> GitHubRelease {
        GitHubRelease {
            tag_name: tag.into(),
            draft,
            prerelease,
        }
    }

    /// The shape of GitHub's `releases` list: newest first, with a draft, a
    /// newer prerelease than any stable, and the stable line below it.
    fn release_list() -> Vec<GitHubRelease> {
        serde_json::from_str(
            r#"[
                {"tag_name": "v0.48.0-beta.1", "draft": true, "prerelease": true},
                {"tag_name": "v0.47.0-beta.2", "draft": false, "prerelease": true},
                {"tag_name": "v0.47.0-beta.10", "draft": false, "prerelease": true},
                {"tag_name": "nightly", "draft": false, "prerelease": true},
                {"tag_name": "v0.46.0", "draft": false, "prerelease": false},
                {"tag_name": "v0.45.0", "draft": false, "prerelease": false}
            ]"#,
        )
        .unwrap()
    }

    #[test]
    fn test_channel_of_version() {
        assert_eq!(
            ReleaseChannel::of_version("0.47.0-beta.1"),
            ReleaseChannel::Prerelease
        );
        assert_eq!(
            ReleaseChannel::of_version("v0.25.0-alpha.0"),
            ReleaseChannel::Prerelease
        );
        assert_eq!(ReleaseChannel::of_version("0.46.0"), ReleaseChannel::Stable);
        assert_eq!(
            ReleaseChannel::of_version("garbage"),
            ReleaseChannel::Stable
        );
    }

    #[test]
    fn test_newest_tag_includes_prereleases_by_semver_and_skips_drafts() {
        assert_eq!(newest_tag(&release_list()).unwrap(), "v0.47.0-beta.10");
        let mut list = release_list();
        list.push(release("v0.47.0", false, false));
        assert_eq!(newest_tag(&list).unwrap(), "v0.47.0", "a newer stable wins");
        assert!(newest_tag(&[release("v0.49.0", true, false)]).is_err());
    }

    #[test]
    fn test_newest_prerelease_tag_ignores_stables_and_drafts() {
        assert_eq!(
            newest_prerelease_tag(&release_list()).as_deref(),
            Some("v0.47.0-beta.10")
        );
        let mut list = release_list();
        list.push(release("v0.47.0", false, false));
        assert_eq!(
            newest_prerelease_tag(&list).as_deref(),
            Some("v0.47.0-beta.10"),
            "a newer stable is not a prerelease"
        );
        assert_eq!(
            newest_prerelease_tag(&[
                release("v0.46.0", false, false),
                release("v0.48.0-beta.1", true, true)
            ]),
            None
        );
    }

    #[test]
    fn test_stable_channel_is_never_offered_a_prerelease() {
        assert_eq!(
            stable_tag(release("v0.46.0", false, false)).unwrap(),
            "v0.46.0"
        );
        assert!(stable_tag(release("v0.47.0-beta.2", false, false)).is_err());
        assert!(stable_tag(release("v0.47.0", false, true)).is_err());
        assert!(stable_tag(release("v0.47.0", true, false)).is_err());
    }

    #[test]
    fn test_invalid_version_returns_none() {
        let current = semver::Version::new(0, 1, 0);
        assert_eq!(should_notify(&current, "not-a-version"), None);
    }
}
