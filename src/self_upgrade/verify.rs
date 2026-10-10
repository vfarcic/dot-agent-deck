//! Checking a downloaded release asset before anything is replaced.
//!
//! In order: the checksum manifest's build provenance (`gh attestation verify`,
//! when the plan said `gh` is installed and logged in), the asset's SHA-256
//! against that manifest (mandatory: a missing entry or a mismatch aborts),
//! for a CLI binary its own `--version`, and the same SHA-256 again right
//! before the file is handed to whatever installs it ([`rehash`]).
//! `docs/develop/self-upgrade.md` has the whole chain and what it does not
//! cover.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

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

/// Check `bytes` (the downloaded `asset`) against `manifest`, returning the
/// verified SHA-256 for the later re-checks ([`rehash`]).
pub fn verify_checksum(
    manifest: &str,
    manifest_name: &str,
    asset: &str,
    bytes: &[u8],
) -> Result<String, UpgradeError> {
    let expected = manifest_entry(manifest, manifest_name, asset)?;
    let actual = sha256_hex(bytes);
    if actual == expected {
        Ok(expected)
    } else {
        Err(UpgradeError::ChecksumMismatch {
            asset: asset.to_string(),
            manifest: manifest_name.to_string(),
            expected,
            actual,
        })
    }
}

/// The SHA-256 of the file at `path`, read without following a symlink in
/// its last component.
pub fn file_sha256(path: &Path) -> std::io::Result<String> {
    let mut file = super::execute::open_no_follow(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// The staged file at `path` must still hash to `expected`, the digest
/// verified against the manifest. Called immediately before the file is
/// handed to something that installs it (`pkexec`, `apt-get`, `hdiutil`, a
/// rename), so a file swapped after the check is refused rather than
/// installed. It narrows the window to the hand-off; it does not close it
/// (`docs/develop/self-upgrade.md`).
pub fn rehash(path: &Path, expected: &str) -> Result<(), UpgradeError> {
    let actual = file_sha256(path).map_err(|e| UpgradeError::StagedChanged {
        path: path.display().to_string(),
        expected: expected.to_string(),
        actual: format!("unreadable: {e}"),
    })?;
    if actual == expected {
        Ok(())
    } else {
        Err(UpgradeError::StagedChanged {
            path: path.display().to_string(),
            expected: expected.to_string(),
            actual,
        })
    }
}

/// Why build provenance will not be checked when `gh` is missing.
pub const GH_NOT_INSTALLED: &str = "the GitHub CLI (`gh`) is not installed";
/// Why, when `gh auth status` reports no usable login.
pub const GH_LOGGED_OUT: &str =
    "the GitHub CLI (`gh`) is not logged in, or its token is invalid (run `gh auth login`)";

/// Whether build provenance CAN be checked on this machine, decided before
/// the user confirms so the plan says which it will be
/// ([`super::plan::PlanOptions::provenance`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvenanceCheck {
    /// `gh` at this path is installed and logged in.
    Available { gh: PathBuf },
    /// It cannot be checked, and why, in words for the user.
    Unavailable { reason: String },
}

impl ProvenanceCheck {
    /// Look `gh` up on `host` and ask it whether it is logged in.
    pub fn detect(host: &dyn Host) -> Self {
        Self::with_gh(host, host.find_program("gh").as_deref())
    }

    /// Ask `gh` (when there is one) whether it is logged in. A known logged-out
    /// state and an unexpected failure (`gh` cannot be spawned, `gh auth
    /// status` failing for another reason, such as no network) each get their
    /// own reason.
    pub fn with_gh(host: &dyn Host, gh: Option<&Path>) -> Self {
        let Some(gh) = gh else {
            return Self::Unavailable {
                reason: GH_NOT_INSTALLED.to_string(),
            };
        };
        let output = match host.run(gh, &[OsStr::new("auth"), OsStr::new("status")]) {
            Ok(output) => output,
            Err(e) => {
                return Self::Unavailable {
                    reason: format!(
                        "the GitHub CLI (`gh`) at {} could not be run: {e}",
                        gh.display()
                    ),
                };
            }
        };
        if output.success {
            return Self::Available {
                gh: gh.to_path_buf(),
            };
        }
        let said = format!("{}\n{}", output.stderr, output.stdout);
        let lower = said.to_ascii_lowercase();
        if lower.contains("not logged in") || lower.contains("is invalid") {
            return Self::Unavailable {
                reason: GH_LOGGED_OUT.to_string(),
            };
        }
        // gh marks the failing account's line with `X`; the first line is often
        // just the host name.
        let lines = said.lines().map(str::trim).filter(|line| !line.is_empty());
        let detail = lines
            .clone()
            .find_map(|line| line.strip_prefix("X "))
            .or_else(|| lines.clone().next())
            .map(str::trim)
            .map_or_else(
                || match output.code {
                    Some(code) => format!("exit {code}"),
                    None => "killed by a signal".to_string(),
                },
                str::to_string,
            );
        Self::Unavailable {
            reason: format!("`gh auth status` failed: {detail}"),
        }
    }

    pub fn will_check(&self) -> bool {
        matches!(self, Self::Available { .. })
    }
}

/// Whether build provenance was checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provenance {
    Verified,
    /// Not checked, and why: the plan said so before the user confirmed.
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

/// Verify that `manifest_bytes`, written once to `manifest_path` for `gh` to
/// read, were produced by this repository's release workflow for the tag
/// `v<version>`.
///
/// The plan already said whether this would happen: [`ProvenanceCheck::
/// Unavailable`] is reported as skipped. [`ProvenanceCheck::Available`] is a
/// promise, so a `gh` that has since disappeared or logged out aborts rather
/// than downgrading silently, and so does a verification that runs and fails.
///
/// `gh` reads the file, while the checksums are parsed from the bytes in
/// memory. What ties the two together is the digest: the attestation `gh`
/// verified must name the SHA-256 of `manifest_bytes` as a subject, so a file
/// swapped under `gh` cannot vouch for different bytes.
pub fn verify_provenance(
    host: &dyn Host,
    check: &ProvenanceCheck,
    manifest_path: &Path,
    manifest_bytes: &[u8],
    version: &str,
) -> Result<Provenance, UpgradeError> {
    let manifest_name = manifest_path.file_name().map_or_else(
        || manifest_path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let gh = match check {
        ProvenanceCheck::Unavailable { reason } => {
            return Ok(Provenance::Skipped {
                reason: reason.clone(),
            });
        }
        ProvenanceCheck::Available { gh } => gh,
    };
    if let ProvenanceCheck::Unavailable { reason } = ProvenanceCheck::with_gh(host, Some(gh)) {
        return Err(UpgradeError::ProvenanceUnavailable { reason });
    }
    let failed = |detail: String| UpgradeError::ProvenanceFailed {
        manifest: manifest_name.clone(),
        detail,
    };
    let repo = crate::repo_identity::SLUG;
    let signer = format!("{repo}/.github/workflows/release.yml");
    let source_ref = format!(
        "refs/tags/v{}",
        version.strip_prefix('v').unwrap_or(version)
    );
    let output = host
        .run_within(
            gh,
            &[
                OsStr::new("attestation"),
                OsStr::new("verify"),
                manifest_path.as_os_str(),
                OsStr::new("--repo"),
                OsStr::new(repo),
                OsStr::new("--signer-workflow"),
                OsStr::new(&signer),
                OsStr::new("--source-ref"),
                OsStr::new(&source_ref),
                OsStr::new("--format"),
                OsStr::new("json"),
            ],
            super::VERIFY_TIMEOUT,
        )
        .map_err(|e| failed(e.to_string()))?;
    if !output.success {
        let detail = [output.stderr.trim(), output.stdout.trim()]
            .into_iter()
            .find(|text| !text.is_empty())
            .unwrap_or("it exited without saying why")
            .to_string();
        return Err(failed(detail));
    }
    let digest = sha256_hex(manifest_bytes);
    if attested_digests(&output.stdout).contains(&digest) {
        Ok(Provenance::Verified)
    } else {
        Err(failed(format!(
            "the attestation it verified does not cover the downloaded {manifest_name} (sha256 {digest})"
        )))
    }
}

/// Every subject SHA-256 in `gh attestation verify --format json` output.
fn attested_digests(json: &str) -> Vec<String> {
    let Ok(serde_json::Value::Array(results)) = serde_json::from_str::<serde_json::Value>(json)
    else {
        return Vec::new();
    };
    results
        .iter()
        .filter_map(|result| {
            result
                .pointer("/verificationResult/statement/subject")
                .and_then(serde_json::Value::as_array)
        })
        .flatten()
        .filter_map(|subject| subject.pointer("/digest/sha256")?.as_str())
        .map(str::to_ascii_lowercase)
        .collect()
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
            Ok(sha256_hex(bytes))
        );
        let binary_mode = format!("{} *{ASSET}\n", sha256_hex(bytes).to_uppercase());
        assert_eq!(
            verify_checksum(&binary_mode, "checksums.txt", ASSET, bytes),
            Ok(sha256_hex(bytes))
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

    const GH: &str = "/usr/bin/gh";
    const MANIFEST: &[u8] = b"the manifest bytes\n";

    fn verify_line(version: &str) -> String {
        format!(
            "{GH} attestation verify /s/checksums.txt --repo vfarcic/dot-agent-deck --signer-workflow vfarcic/dot-agent-deck/.github/workflows/release.yml --source-ref refs/tags/v{version} --format json"
        )
    }

    /// `gh attestation verify --format json` output whose attestation names
    /// `digests` as subjects.
    fn attestation_json(digests: &[String]) -> String {
        let subjects: Vec<_> = digests
            .iter()
            .map(|d| serde_json::json!({"name": "checksums.txt", "digest": {"sha256": d}}))
            .collect();
        serde_json::json!([{"verificationResult": {"statement": {"subject": subjects}}}])
            .to_string()
    }

    fn available() -> ProvenanceCheck {
        ProvenanceCheck::Available {
            gh: PathBuf::from(GH),
        }
    }

    #[test]
    fn verify_005_provenance_unavailable_without_gh() {
        let host = FakeHost::new();
        let check = ProvenanceCheck::detect(&host);
        assert_eq!(
            check,
            ProvenanceCheck::Unavailable {
                reason: GH_NOT_INSTALLED.into()
            }
        );
        let result = verify_provenance(
            &host,
            &check,
            Path::new("/s/checksums.txt"),
            MANIFEST,
            "0.46.0",
        )
        .unwrap();
        assert!(result.message().contains("not installed"));
        assert!(host.ran().is_empty());
    }

    // Unix `PATH` layout: native Windows is unsupported (#164).
    #[cfg(unix)]
    #[test]
    fn verify_006_logged_out_and_unexpected_failures_have_their_own_reasons() {
        let logged_out = FakeHost::new().exe(GH).on_path("/usr/bin").answer(
            "/usr/bin/gh auth status",
            fail("You are not logged into any GitHub hosts. To log in, run: gh auth login"),
        );
        assert_eq!(
            ProvenanceCheck::detect(&logged_out),
            ProvenanceCheck::Unavailable {
                reason: GH_LOGGED_OUT.into()
            }
        );

        let invalid = FakeHost::new().exe(GH).on_path("/usr/bin").answer(
            "/usr/bin/gh auth status",
            fail("github.com\n  X Failed to log in to github.com using token (GITHUB_TOKEN)\n  - The token in GITHUB_TOKEN is invalid.\n"),
        );
        assert_eq!(
            ProvenanceCheck::detect(&invalid),
            ProvenanceCheck::Unavailable {
                reason: GH_LOGGED_OUT.into()
            }
        );

        let offline = FakeHost::new().exe(GH).on_path("/usr/bin").answer(
            "/usr/bin/gh auth status",
            fail("github.com\n  X Timeout trying to log in to github.com account u (keyring)\n"),
        );
        let ProvenanceCheck::Unavailable { reason } = ProvenanceCheck::detect(&offline) else {
            panic!("an offline gh cannot check provenance");
        };
        assert_eq!(
            reason,
            "`gh auth status` failed: Timeout trying to log in to github.com account u (keyring)"
        );

        // On PATH by name, but not runnable: the spawn itself fails.
        let unrunnable = FakeHost::new()
            .on_path("/usr/bin")
            .link(GH, "/usr/bin/gh-real");
        let mut unrunnable = unrunnable;
        unrunnable
            .executables
            .insert(PathBuf::from("/usr/bin/gh-real"));
        let ProvenanceCheck::Unavailable { reason } = ProvenanceCheck::detect(&unrunnable) else {
            panic!("a gh that cannot be spawned cannot check provenance");
        };
        assert!(reason.contains("could not be run"), "{reason}");

        let logged_in = FakeHost::new()
            .exe(GH)
            .on_path("/usr/bin")
            .answer("/usr/bin/gh auth status", ok(""));
        assert_eq!(ProvenanceCheck::detect(&logged_in), available());
        assert!(available().will_check());
    }

    #[test]
    fn verify_007_provenance_verified_and_failed() {
        let good = attestation_json(&[sha256_hex(b"other"), sha256_hex(MANIFEST)]);
        let host = FakeHost::new()
            .exe(GH)
            .answer("/usr/bin/gh auth status", ok(""))
            .answer(&verify_line("0.46.0"), ok(&good));
        assert_eq!(
            verify_provenance(
                &host,
                &available(),
                Path::new("/s/checksums.txt"),
                MANIFEST,
                "v0.46.0"
            ),
            Ok(Provenance::Verified)
        );
        assert_eq!(
            host.bound_of("/usr/bin/gh attestation"),
            Some(crate::self_upgrade::VERIFY_TIMEOUT)
        );
        assert_eq!(
            host.bound_of("/usr/bin/gh auth"),
            Some(crate::self_upgrade::PROBE_TIMEOUT)
        );

        let host = FakeHost::new()
            .exe(GH)
            .answer("/usr/bin/gh auth status", ok(""))
            .answer(&verify_line("0.46.0"), fail("no attestations found"));
        let err = verify_provenance(
            &host,
            &available(),
            Path::new("/s/checksums.txt"),
            MANIFEST,
            "0.46.0",
        )
        .unwrap_err();
        assert!(err.to_string().contains("no attestations found"), "{err}");
    }

    #[test]
    fn verify_009_attestation_is_tied_to_the_release_tag() {
        // `--source-ref refs/tags/v<version>`: gh refuses an attestation made
        // for another tag, and that refusal aborts.
        let host = FakeHost::new()
            .exe(GH)
            .answer("/usr/bin/gh auth status", ok(""))
            .answer(
                &verify_line("0.47.0"),
                fail("Error: expected SourceRepositoryRef to be refs/tags/v0.47.0, got refs/tags/v0.46.0"),
            );
        let err = verify_provenance(
            &host,
            &available(),
            Path::new("/s/checksums.txt"),
            MANIFEST,
            "0.47.0",
        )
        .unwrap_err();
        assert!(
            matches!(err, UpgradeError::ProvenanceFailed { .. }),
            "{err:?}"
        );
        assert!(host.ran().contains(&verify_line("0.47.0")));
    }

    #[test]
    fn verify_010_attestation_must_cover_the_bytes_in_memory() {
        // gh verified SOME file, but not the bytes whose checksums are used.
        let elsewhere = attestation_json(&[sha256_hex(b"a swapped manifest")]);
        let host = FakeHost::new()
            .exe(GH)
            .answer("/usr/bin/gh auth status", ok(""))
            .answer(&verify_line("0.46.0"), ok(&elsewhere));
        let err = verify_provenance(
            &host,
            &available(),
            Path::new("/s/checksums.txt"),
            MANIFEST,
            "0.46.0",
        )
        .unwrap_err();
        assert!(err.to_string().contains("does not cover"), "{err}");

        let garbage = FakeHost::new()
            .exe(GH)
            .answer("/usr/bin/gh auth status", ok(""))
            .answer(&verify_line("0.46.0"), ok("Verification succeeded!"));
        assert!(
            verify_provenance(
                &garbage,
                &available(),
                Path::new("/s/checksums.txt"),
                MANIFEST,
                "0.46.0",
            )
            .is_err()
        );
    }

    #[test]
    fn verify_011_a_promised_check_that_cannot_run_aborts() {
        // The plan said provenance would be checked; by execution gh is logged
        // out. That aborts instead of silently skipping.
        let host = FakeHost::new().exe(GH).answer(
            "/usr/bin/gh auth status",
            fail("You are not logged into any GitHub hosts."),
        );
        let err = verify_provenance(
            &host,
            &available(),
            Path::new("/s/checksums.txt"),
            MANIFEST,
            "0.46.0",
        )
        .unwrap_err();
        assert_eq!(
            err,
            UpgradeError::ProvenanceUnavailable {
                reason: GH_LOGGED_OUT.into()
            }
        );
        assert!(err.to_string().contains("Nothing was changed"), "{err}");
        assert!(!host.ran().iter().any(|line| line.contains("attestation")));

        // And a plan that said it would not be checked skips with its reason.
        let skipped = verify_provenance(
            &FakeHost::new(),
            &ProvenanceCheck::Unavailable {
                reason: GH_LOGGED_OUT.into(),
            },
            Path::new("/s/checksums.txt"),
            MANIFEST,
            "0.46.0",
        )
        .unwrap();
        assert_eq!(
            skipped,
            Provenance::Skipped {
                reason: GH_LOGGED_OUT.into()
            }
        );
    }

    #[test]
    fn verify_012_rehash_refuses_a_file_swapped_after_verification() {
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("dot-agent-deck-linux-amd64");
        std::fs::write(&staged, b"verified build").unwrap();
        let digest = sha256_hex(b"verified build");
        assert_eq!(rehash(&staged, &digest), Ok(()));

        std::fs::remove_file(&staged).unwrap();
        std::fs::write(&staged, b"something else").unwrap();
        let err = rehash(&staged, &digest).unwrap_err();
        assert!(matches!(err, UpgradeError::StagedChanged { .. }), "{err:?}");
        assert!(err.to_string().contains("was not installed"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn verify_013_rehash_does_not_follow_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::write(&real, b"verified build").unwrap();
        let link = dir.path().join("staged");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(rehash(&link, &sha256_hex(b"verified build")).is_err());
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
