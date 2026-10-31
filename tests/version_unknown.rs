//! Issue #326 — a build that cannot determine its own version reads as
//! "version unknown", not as the ancient `0.1.0` release.
//!
//! `build.rs` falls back to the `CARGO_PKG_VERSION` placeholder when nothing
//! was injected and git has no tag, and marks it with the SemVer build
//! metadata `unknown` (`0.1.0+unknown`). These tests pin the agreement between
//! the build script's value and the runtime predicate, and the daemon upgrade
//! offer that both clients read. The `connect` nudge, `remote add` /
//! `--no-install` and the self-upgrade plan are private seams tested in their
//! own modules (`src/connect.rs`, `src/remote.rs`,
//! `src/self_upgrade/plan.rs`).

// Only the resolver and its mark are used here; `tests/build_version.rs`
// covers the rest of the shared file.
#[allow(dead_code)]
#[path = "../build_version_resolve.rs"]
mod build_version_resolve;

use build_version_resolve::{VersionSource, resolve_version};
use dot_agent_deck::daemon_upgrade::{UpgradeOffer, upgrade_offer_against};
use dot_agent_deck::version::{UNKNOWN_VERSION_METADATA, is_unknown_version, own_version_known};

#[test]
fn the_build_scripts_placeholder_is_what_the_runtime_reads_as_unknown() {
    assert_eq!(
        build_version_resolve::UNKNOWN_VERSION_METADATA,
        UNKNOWN_VERSION_METADATA,
        "build.rs and src/version.rs must agree on the mark"
    );
    let placeholder = resolve_version(None, None, "0.1.0");
    assert_eq!(placeholder.source, VersionSource::Placeholder);
    assert!(
        is_unknown_version(&placeholder.value),
        "{}",
        placeholder.value
    );
    // Still valid SemVer: `src/version.rs` `.expect()`s that it parses.
    semver::Version::parse(&placeholder.value).expect("the placeholder parses");

    // A version that was resolved is never unknown — the real v0.1.0 tag
    // included, which is why the bare value could not carry the meaning.
    for resolved in [
        resolve_version(None, Some("v0.1.0"), "0.1.0"),
        resolve_version(Some("0.1.0"), None, "0.1.0"),
        resolve_version(None, Some("v0.49.3"), "0.1.0"),
    ] {
        assert!(!is_unknown_version(&resolved.value), "{}", resolved.value);
    }
}

#[test]
fn this_builds_own_version_is_unknown_only_when_it_says_so() {
    assert_eq!(
        own_version_known(),
        !env!("DAD_VERSION").ends_with("+unknown"),
        "DAD_VERSION is {}",
        env!("DAD_VERSION")
    );
}

#[test]
fn only_the_unknown_mark_reads_as_unknown() {
    for unknown in ["0.1.0+unknown", "v0.1.0+unknown", " 0.1.0+unknown\n"] {
        assert!(is_unknown_version(unknown), "{unknown:?}");
    }
    for known in [
        "0.1.0",
        "0.49.3",
        "0.50.0-beta.1",
        "1.2.3+meta",
        "0.1.0+unknown.1",
        "0.1.0-unknown",
        "garbage",
        "",
    ] {
        assert!(!is_unknown_version(known), "{known:?}");
    }
}

/// The daemon upgrade offer — what the desktop's daemon notice and every
/// other client read — does not treat an unknown version on either side as
/// the oldest release.
#[test]
fn an_unknown_version_on_either_side_is_offered_no_upgrade() {
    assert_eq!(
        upgrade_offer_against("0.49.3", Some("0.1.0+unknown")),
        UpgradeOffer::Unknown,
        "a daemon that does not know its version is not an old one"
    );
    assert_eq!(
        upgrade_offer_against("0.1.0+unknown", Some("0.49.3")),
        UpgradeOffer::Unknown,
        "a client that does not know its version cannot call the daemon newer"
    );
    assert_eq!(
        upgrade_offer_against("0.1.0+unknown", Some("0.1.0+unknown")),
        UpgradeOffer::Unknown
    );
    // The real v0.1.0 release is still older than a current client.
    assert_eq!(
        upgrade_offer_against("0.49.3", Some("0.1.0")),
        UpgradeOffer::Offered {
            from: "0.1.0".into(),
            to: "0.49.3".into(),
        }
    );
}
