//! Checking a downloaded release asset before anything is replaced.
//!
//! In order: the checksum manifest's build provenance (`gh attestation verify`,
//! when `gh` is installed and logged in), the asset's SHA-256 against that
//! manifest (mandatory: a missing entry or a mismatch aborts), and for a CLI
//! binary its own `--version`.

use std::ffi::OsStr;
use std::path::Path;

use sha2::{Digest, Sha256};

use super::{Host, UpgradeError, reported_version};

/// The SHA-256 the manifest lists for `asset`. The manifest is `shasum -a 256`
/// output: `<64 hex digits>  <name>`, or `<hex> *<name>` in binary mode.
pub fn manifest_entry(
    manifest: &str,
    manifest_name: &str,
    asset: &str,
) -> Result<String, UpgradeError> {
    let mut found: Option<String> = None;
    for line in manifest.lines() {
        let mut parts = line.split_whitespace();
        let (Some(hash), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let name = name.strip_prefix('*').unwrap_or(name);
        if name != asset || hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let hash = hash.to_ascii_lowercase();
        match &found {
            Some(previous) if *previous != hash => {
                return Err(UpgradeError::ChecksumAmbiguous {
                    asset: asset.to_string(),
                    manifest: manifest_name.to_string(),
                });
            }
            _ => found = Some(hash),
        }
    }
    found.ok_or_else(|| UpgradeError::ChecksumMissing {
        asset: asset.to_string(),
        manifest: manifest_name.to_string(),
    })
}

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Check `bytes` (the downloaded `asset`) against `manifest`.
pub fn verify_checksum(
    manifest: &str,
    manifest_name: &str,
    asset: &str,
    bytes: &[u8],
) -> Result<(), UpgradeError> {
    let expected = manifest_entry(manifest, manifest_name, asset)?;
    let actual = sha256_hex(bytes);
    if actual == expected {
        Ok(())
    } else {
        Err(UpgradeError::ChecksumMismatch {
            asset: asset.to_string(),
            manifest: manifest_name.to_string(),
            expected,
            actual,
        })
    }
}

/// Whether build provenance was checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provenance {
    Verified,
    /// Not checked, and why — reported to the user, not fatal.
    Skipped {
        reason: String,
    },
}

impl Provenance {
    pub fn message(&self) -> String {
        match self {
            Self::Verified => "Build provenance verified with `gh attestation verify`.".to_string(),
            Self::Skipped { reason } => format!("Build provenance was not checked: {reason}."),
        }
    }
}

/// Verify that `manifest` (a file on disk) was produced by this repository's
/// release workflow. No `gh`, or a `gh` that is not logged in (it cannot reach
/// the attestation API then), is reported as skipped; a verification that runs
/// and fails aborts.
pub fn verify_provenance(
    host: &dyn Host,
    gh: Option<&Path>,
    manifest: &Path,
) -> Result<Provenance, UpgradeError> {
    let Some(gh) = gh else {
        return Ok(Provenance::Skipped {
            reason: "the GitHub CLI (`gh`) is not installed".to_string(),
        });
    };
    let logged_in = host
        .run(gh, &[OsStr::new("auth"), OsStr::new("status")])
        .is_ok_and(|out| out.success);
    if !logged_in {
        return Ok(Provenance::Skipped {
            reason: "the GitHub CLI (`gh`) is not logged in (run `gh auth login`)".to_string(),
        });
    }
    let repo = crate::repo_identity::SLUG;
    let signer = format!("{repo}/.github/workflows/release.yml");
    let output = host
        .run(
            gh,
            &[
                OsStr::new("attestation"),
                OsStr::new("verify"),
                manifest.as_os_str(),
                OsStr::new("--repo"),
                OsStr::new(repo),
                OsStr::new("--signer-workflow"),
                OsStr::new(&signer),
            ],
        )
        .map_err(|e| UpgradeError::ProvenanceFailed {
            manifest: manifest.display().to_string(),
            detail: e.to_string(),
        })?;
    if output.success {
        Ok(Provenance::Verified)
    } else {
        let detail = [output.stderr.trim(), output.stdout.trim()]
            .into_iter()
            .find(|text| !text.is_empty())
            .unwrap_or("it exited without saying why")
            .to_string();
        Err(UpgradeError::ProvenanceFailed {
            manifest: manifest.file_name().map_or_else(
                || manifest.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            ),
            detail,
        })
    }
}

/// The binary at `path` must answer `--version` as dot-agent-deck `expected`.
pub fn verify_binary_version(
    host: &dyn Host,
    path: &Path,
    expected: &str,
) -> Result<(), UpgradeError> {
    let expected = expected.strip_prefix('v').unwrap_or(expected);
    match reported_version(host, path) {
        Some(actual) if actual == expected => Ok(()),
        actual => Err(UpgradeError::VersionMismatch {
            expected: expected.to_string(),
            actual: actual.map_or_else(
                || "no dot-agent-deck version".to_string(),
                |v| format!("v{v}"),
            ),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::self_upgrade::test_host::{FakeHost, fail, ok};

    const ASSET: &str = "dot-agent-deck-linux-amd64";

    fn manifest_for(bytes: &[u8]) -> String {
        format!(
            "{}  dot-agent-deck-darwin-arm64\n{}  {ASSET}\n",
            "0".repeat(64),
            sha256_hex(bytes)
        )
    }

    #[test]
    fn verify_001_checksum_match() {
        let bytes = b"the new binary";
        assert_eq!(
            verify_checksum(&manifest_for(bytes), "checksums.txt", ASSET, bytes),
            Ok(())
        );
        let binary_mode = format!("{} *{ASSET}\n", sha256_hex(bytes).to_uppercase());
        assert_eq!(
            verify_checksum(&binary_mode, "checksums.txt", ASSET, bytes),
            Ok(())
        );
    }

    #[test]
    fn verify_002_checksum_mismatch_aborts() {
        let manifest = manifest_for(b"what the release built");
        let err =
            verify_checksum(&manifest, "checksums.txt", ASSET, b"something else").unwrap_err();
        assert!(
            matches!(err, UpgradeError::ChecksumMismatch { .. }),
            "{err:?}"
        );
        assert!(err.to_string().contains("Nothing was changed"));
    }

    #[test]
    fn verify_003_checksum_missing_entry_aborts() {
        let manifest = manifest_for(b"x");
        let err = verify_checksum(
            &manifest,
            "checksums.txt",
            "dot-agent-deck-linux-arm64",
            b"x",
        )
        .unwrap_err();
        assert_eq!(
            err,
            UpgradeError::ChecksumMissing {
                asset: "dot-agent-deck-linux-arm64".into(),
                manifest: "checksums.txt".into(),
            }
        );
        // A prefix of the name is not the name.
        assert!(manifest_entry(&manifest, "checksums.txt", "dot-agent-deck-linux").is_err());
    }

    #[test]
    fn verify_004_conflicting_entries_abort() {
        let manifest = format!("{}  {ASSET}\n{}  {ASSET}\n", "a".repeat(64), "b".repeat(64));
        assert!(matches!(
            manifest_entry(&manifest, "checksums.txt", ASSET),
            Err(UpgradeError::ChecksumAmbiguous { .. })
        ));
    }

    #[test]
    fn verify_005_provenance_skipped_without_gh() {
        let host = FakeHost::new();
        let result = verify_provenance(&host, None, Path::new("/s/checksums.txt")).unwrap();
        assert!(matches!(result, Provenance::Skipped { .. }));
        assert!(result.message().contains("not installed"));
        assert!(host.ran().is_empty());
    }

    #[test]
    fn verify_006_provenance_skipped_when_gh_logged_out() {
        let host = FakeHost::new()
            .exe("/usr/bin/gh")
            .answer("/usr/bin/gh auth status", fail("not logged in"));
        let result = verify_provenance(
            &host,
            Some(Path::new("/usr/bin/gh")),
            Path::new("/s/checksums.txt"),
        );
        assert!(matches!(result, Ok(Provenance::Skipped { .. })));
    }

    #[test]
    fn verify_007_provenance_verified_and_failed() {
        let verify = "/usr/bin/gh attestation verify /s/checksums.txt --repo vfarcic/dot-agent-deck --signer-workflow vfarcic/dot-agent-deck/.github/workflows/release.yml";
        let host = FakeHost::new()
            .exe("/usr/bin/gh")
            .answer("/usr/bin/gh auth status", ok(""))
            .answer(verify, ok("Verification succeeded!"));
        assert_eq!(
            verify_provenance(
                &host,
                Some(Path::new("/usr/bin/gh")),
                Path::new("/s/checksums.txt")
            ),
            Ok(Provenance::Verified)
        );

        let host = FakeHost::new()
            .exe("/usr/bin/gh")
            .answer("/usr/bin/gh auth status", ok(""))
            .answer(verify, fail("no attestations found"));
        let err = verify_provenance(
            &host,
            Some(Path::new("/usr/bin/gh")),
            Path::new("/s/checksums.txt"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("no attestations found"), "{err}");
    }

    #[test]
    fn verify_008_binary_must_report_expected_version() {
        let host = FakeHost::new()
            .deck("/t/new", "0.46.0")
            .deck("/t/old", "0.45.0")
            .exe("/t/other")
            .answer("/t/other --version", ok("something 1.0\n"));
        assert_eq!(
            verify_binary_version(&host, Path::new("/t/new"), "v0.46.0"),
            Ok(())
        );
        assert!(matches!(
            verify_binary_version(&host, Path::new("/t/old"), "0.46.0"),
            Err(UpgradeError::VersionMismatch { .. })
        ));
        assert!(verify_binary_version(&host, Path::new("/t/other"), "0.46.0").is_err());
    }
}
