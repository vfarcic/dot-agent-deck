//! The `.rwx/` tasks that download a release tarball check it against a pinned
//! SHA-256, and a Renovate bump that moves the version without the checksum
//! leaves the task failing at `sha256sum -c`. That happened twice:
//!
//! - PR #2523: `.rwx/ci.yml`'s `devbox` task. Renovate's 0.22.0 -> 0.23.0 bump
//!   moved `DEVBOX_USE_VERSION` and left the checksum at 0.22.0's.
//! - Issue #2566: the `nextest` task in `.rwx/ci.yml`, `.rwx/release-lib.yml`
//!   and `.rwx/sandbox.yml`, whose manager read only the version.
//!
//! renovate.json's jetify-com/devbox and RWX cargo-nextest managers now read
//! the version and the checksum in one match (`currentValue` and
//! `currentDigest`), and their `github-release-attachments` datasource maps
//! the digest to the new release's checksum asset. These tests run those
//! matchStrings over the real `.rwx/` files and require one match per pin,
//! carrying the version and the checksum the task checks. A task rewritten so
//! that the regex no longer spans both goes red here, rather than on the next
//! bump, when Renovate would move only the version again or neither.

use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("xtask/linkage-check sits two levels below the workspace root")
        .to_path_buf()
}

const CI: &str = ".rwx/ci.yml";

/// The file pattern renovate.json's RWX customManagers carry.
const RWX_FILE_PATTERN: &str = "/^\\.rwx/[^/]+\\.ya?ml$/";

/// The only datasource that updates a `currentDigest` from a release's
/// checksum asset; under `github-releases` or `crate` a bump moves the version
/// alone (PR #2523, issue #2566).
const DIGEST_DATASOURCE: &str = "github-release-attachments";

/// One renovate.json customManager, as these tests read it: the raw entry, its
/// compiled file patterns and its compiled matchStrings. Renovate writes named
/// groups as `(?<name>…)`, which the `regex` crate accepts as it is.
struct Manager {
    raw: serde_json::Value,
    files: Vec<Regex>,
    matchers: Vec<Regex>,
}

/// The one renovate.json customManager with depName `dep_name` that `keep`
/// selects. `what` names it in failures.
fn manager(dep_name: &str, what: &str, keep: impl Fn(&serde_json::Value) -> bool) -> Manager {
    let text = fs::read_to_string(repo_root().join("renovate.json")).expect("read renovate.json");
    let config: serde_json::Value = serde_json::from_str(&text).expect("renovate.json is JSON");
    let managers: Vec<&serde_json::Value> = config["customManagers"]
        .as_array()
        .expect("customManagers")
        .iter()
        .filter(|m| m["depNameTemplate"] == dep_name)
        .filter(|m| keep(m))
        .collect();
    let [manager] = managers[..] else {
        panic!(
            "renovate.json must carry exactly one {what} customManager, found {}",
            managers.len()
        );
    };
    let regexes = |key: &str, strip_slashes: bool| -> Vec<Regex> {
        manager[key]
            .as_array()
            .unwrap_or_else(|| panic!("the {what} manager has a {key} array"))
            .iter()
            .map(|v| {
                let p = v.as_str().expect("a string");
                let p = if strip_slashes {
                    p.strip_prefix('/')
                        .and_then(|p| p.strip_suffix('/'))
                        .expect("a /regex/ file pattern")
                } else {
                    p
                };
                Regex::new(p).unwrap_or_else(|e| panic!("{key} entry {p:?}: {e}"))
            })
            .collect()
    };
    Manager {
        raw: manager.clone(),
        files: regexes("managerFilePatterns", true),
        matchers: regexes("matchStrings", false),
    }
}

impl Manager {
    fn template(&self, key: &str) -> &str {
        self.raw[key]
            .as_str()
            .unwrap_or_else(|| panic!("the manager has a {key}"))
    }

    /// Every `(file, currentValue, currentDigest)` this manager's matchStrings
    /// extract from the top level of `.rwx/`, in file order.
    fn extract(&self) -> Vec<(String, String, Option<String>)> {
        let root = repo_root();
        let mut found = Vec::new();
        for rel in rwx_files() {
            if !self.files.iter().any(|f| f.is_match(&rel)) {
                continue;
            }
            let text = fs::read_to_string(root.join(&rel)).expect("read an .rwx file");
            for matcher in &self.matchers {
                for caps in matcher.captures_iter(&text) {
                    let value = caps.name("currentValue").expect("captures currentValue");
                    let digest = caps.name("currentDigest").map(|d| d.as_str().to_string());
                    found.push((rel.clone(), value.as_str().to_string(), digest));
                }
            }
        }
        found
    }
}

/// The top-level `.rwx/` entries, as repo-relative paths, sorted.
fn rwx_files() -> Vec<String> {
    let mut rels: Vec<String> = fs::read_dir(repo_root().join(".rwx"))
        .expect("read .rwx/")
        .map(|e| {
            format!(
                ".rwx/{}",
                e.expect("an entry").file_name().to_string_lossy()
            )
        })
        .collect();
    rels.sort();
    rels
}

/// The values `pick` reads off the lines of `text`, trimmed.
fn picked(text: &str, pick: impl Fn(&str) -> Option<String>) -> Vec<String> {
    text.lines().filter_map(|l| pick(l.trim())).collect()
}

/// The value of the one line in `.rwx/ci.yml` that `pick` reads.
fn the_one(text: &str, what: &str, pick: impl Fn(&str) -> Option<String>) -> String {
    let found = picked(text, pick);
    let [value] = &found[..] else {
        panic!("{CI} must have exactly one {what}, found {found:?}");
    };
    value.clone()
}

/// The checksum of an `echo "<sha256>  <path>" | sha256sum -c -` line for
/// `path`.
fn checksum_for(path: &str) -> impl Fn(&str) -> Option<String> + '_ {
    move |l: &str| {
        l.strip_prefix("echo \"")
            .and_then(|l| l.strip_suffix(&format!("  {path}\" | sha256sum -c -")))
            .map(str::to_string)
    }
}

/// Scenario: read the devbox version and tarball checksum `.rwx/ci.yml`'s
/// `devbox` task pins, run renovate.json's jetify-com/devbox matchString over
/// every `.rwx/` file it applies to, and require exactly one match capturing
/// that version and that checksum, under a datasource that updates digests.
#[test]
fn renovate_reads_the_devbox_pin_and_its_checksum() {
    let manager = manager("jetify-com/devbox", "jetify-com/devbox", |_| true);
    assert_eq!(
        manager.template("datasourceTemplate"),
        DIGEST_DATASOURCE,
        "only {DIGEST_DATASOURCE} updates a currentDigest from the release's \
         checksums.txt; under github-releases a devbox bump moves the version alone (PR #2523)"
    );

    let text = fs::read_to_string(repo_root().join(CI)).expect("read .rwx/ci.yml");
    let version = the_one(&text, "`DEVBOX_USE_VERSION:` line", |l| {
        l.strip_prefix("DEVBOX_USE_VERSION:")
            .map(|v| v.trim().to_string())
    });
    let checksum = the_one(
        &text,
        "`sha256sum -c` line for /tmp/devbox.tar.gz",
        checksum_for("/tmp/devbox.tar.gz"),
    );
    assert_eq!(
        manager.extract(),
        vec![(CI.to_string(), version.clone(), Some(checksum.clone()))],
        "renovate.json's jetify-com/devbox manager must read `DEVBOX_USE_VERSION` ({version}) \
         and the tarball's SHA-256 ({checksum}) in exactly one match, so a devbox bump moves \
         both in one PR (PR #2523)"
    );
}

/// The RWX cargo-nextest manager: the one with depName `cargo-nextest` that
/// reads only `.rwx/` (the workflows' nextest manager shares the depName).
fn nextest_manager() -> Manager {
    manager("cargo-nextest", "RWX cargo-nextest", |m| {
        m["managerFilePatterns"]
            .as_array()
            .is_some_and(|p| p.len() == 1 && p[0] == RWX_FILE_PATTERN)
    })
}

/// Scenario: in every `.rwx/` file, read the nextest release tag each `nextest`
/// task pins (`CARGO_NEXTEST_RELEASE:`) and the tarball checksum it checks, run
/// renovate.json's RWX cargo-nextest matchString over those files, and require
/// one match per task capturing that tag and that checksum, under a datasource
/// that updates digests, for the release the task's download URL fetches.
#[test]
fn renovate_reads_each_nextest_pin_and_its_checksum() {
    let manager = nextest_manager();
    assert_eq!(
        manager.template("datasourceTemplate"),
        DIGEST_DATASOURCE,
        "only {DIGEST_DATASOURCE} updates a currentDigest from the release's `.sha256` \
         assets; under `crate` a nextest bump moves the version alone (issue #2566)"
    );
    assert_eq!(
        manager.template("packageNameTemplate"),
        "nextest-rs/nextest",
        "the digest is read from the nextest-rs/nextest GitHub release the task downloads"
    );

    let root = repo_root();
    let mut expected = Vec::new();
    for rel in rwx_files() {
        let text = fs::read_to_string(root.join(&rel)).expect("read an .rwx file");
        let tags = picked(&text, |l| {
            l.strip_prefix("CARGO_NEXTEST_RELEASE:")
                .map(|v| v.trim().to_string())
        });
        let checksums = picked(&text, checksum_for("/tmp/cargo-nextest.tar.gz"));
        assert_eq!(
            tags.len(),
            checksums.len(),
            "{rel} has {} `CARGO_NEXTEST_RELEASE:` line(s) and {} `sha256sum -c` line(s) for \
             /tmp/cargo-nextest.tar.gz; each nextest task pins one of each",
            tags.len(),
            checksums.len()
        );
        if !tags.is_empty() {
            assert!(
                text.contains(
                    "releases/download/${CARGO_NEXTEST_RELEASE}/${CARGO_NEXTEST_RELEASE}-x86_64-unknown-linux-gnu.tar.gz"
                ),
                "{rel} pins `CARGO_NEXTEST_RELEASE` but does not download that release's \
                 tarball, so the pin Renovate moves is not the one the task installs"
            );
        }
        for (tag, checksum) in tags.into_iter().zip(checksums) {
            expected.push((rel.clone(), tag, Some(checksum)));
        }
    }
    assert!(
        !expected.is_empty(),
        "no `.rwx/` file pins `CARGO_NEXTEST_RELEASE:`, so this comparison would pass vacuously"
    );
    assert_eq!(
        manager.extract(),
        expected,
        "renovate.json's RWX cargo-nextest manager must read each nextest task's \
         `CARGO_NEXTEST_RELEASE` and its tarball's SHA-256 in one match, so a nextest bump \
         moves both in one PR (issue #2566)"
    );
}

/// Scenario: read the RWX cargo-nextest manager's `regex:` versioning and
/// check it against nextest's tag names: it reads X.Y.Z out of a
/// `cargo-nextest-X.Y.Z` release and refuses a pre-release and another crate's
/// release from the same repository, so Renovate offers neither.
#[test]
fn renovate_reads_nextest_release_tags_and_nothing_else() {
    let manager = nextest_manager();
    let template = manager.template("versioningTemplate");
    let versioning = Regex::new(
        template
            .strip_prefix("regex:")
            .unwrap_or_else(|| panic!("versioningTemplate {template:?} is a regex: versioning")),
    )
    .expect("the versioning regex compiles");

    let parts = versioning
        .captures("cargo-nextest-0.9.152")
        .expect("a cargo-nextest release tag is a version");
    assert_eq!(
        (&parts["major"], &parts["minor"], &parts["patch"]),
        ("0", "9", "152")
    );
    // Tag names from nextest-rs/nextest's releases (checked 2026-11-28).
    for other in [
        "cargo-nextest-0.9.152-b.1",
        "nextest-runner-0.130.0",
        "quick-junit-0.3.5",
        "0.9.152",
    ] {
        assert!(
            !versioning.is_match(other),
            "{other:?} must not read as a cargo-nextest release"
        );
    }
}
