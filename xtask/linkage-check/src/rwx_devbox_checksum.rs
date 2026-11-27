//! PR #2523: `.rwx/ci.yml`'s `devbox` task downloads the devbox release
//! tarball for `DEVBOX_USE_VERSION` and checks it against a pinned SHA-256.
//! Renovate's 0.22.0 -> 0.23.0 bump moved the version and left the checksum
//! at 0.22.0's, so the task failed at `sha256sum -c`.
//!
//! renovate.json's jetify-com/devbox manager now reads the version and the
//! checksum in one match (`currentValue` and `currentDigest`), and its
//! `github-release-attachments` datasource maps the digest to the new
//! release's `checksums.txt`. This test runs that matchString over the real
//! `.rwx/` files and requires exactly one match, carrying the
//! `DEVBOX_USE_VERSION` and the checksum the task checks. A task rewritten so
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

/// renovate.json's jetify-com/devbox manager: its datasource, compiled file
/// patterns and matchStrings. Renovate writes named groups as `(?<name>…)`,
/// which the `regex` crate accepts as it is.
fn devbox_manager() -> (String, Vec<Regex>, Vec<Regex>) {
    let text = fs::read_to_string(repo_root().join("renovate.json")).expect("read renovate.json");
    let config: serde_json::Value = serde_json::from_str(&text).expect("renovate.json is JSON");
    let managers: Vec<&serde_json::Value> = config["customManagers"]
        .as_array()
        .expect("customManagers")
        .iter()
        .filter(|m| m["depNameTemplate"] == "jetify-com/devbox")
        .collect();
    let [manager] = managers[..] else {
        panic!(
            "renovate.json must carry exactly one jetify-com/devbox customManager, found {}",
            managers.len()
        );
    };
    let regexes = |key: &str, strip_slashes: bool| -> Vec<Regex> {
        manager[key]
            .as_array()
            .unwrap_or_else(|| panic!("the jetify-com/devbox manager has a {key} array"))
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
    (
        manager["datasourceTemplate"]
            .as_str()
            .expect("the jetify-com/devbox manager has a datasourceTemplate")
            .to_string(),
        regexes("managerFilePatterns", true),
        regexes("matchStrings", false),
    )
}

/// The value of the one `<key>: <value>` line in `text`, or of the one
/// `echo "<sha256>  /tmp/devbox.tar.gz" | sha256sum -c -` line.
fn the_one(text: &str, what: &str, pick: impl Fn(&str) -> Option<String>) -> String {
    let found: Vec<String> = text.lines().filter_map(|l| pick(l.trim())).collect();
    let [value] = &found[..] else {
        panic!("{CI} must have exactly one {what}, found {found:?}");
    };
    value.clone()
}

/// Scenario: read the devbox version and tarball checksum `.rwx/ci.yml`'s
/// `devbox` task pins, run renovate.json's jetify-com/devbox matchString over
/// every `.rwx/` file it applies to, and require exactly one match capturing
/// that version and that checksum, under a datasource that updates digests.
#[test]
fn renovate_reads_the_devbox_pin_and_its_checksum() {
    let root = repo_root();
    let (datasource, files, matchers) = devbox_manager();
    assert_eq!(
        datasource, "github-release-attachments",
        "only github-release-attachments updates a currentDigest from the release's \
         checksums.txt; under github-releases a devbox bump moves the version alone (PR #2523)"
    );

    let mut rels: Vec<String> = fs::read_dir(root.join(".rwx"))
        .expect("read .rwx/")
        .map(|e| {
            format!(
                ".rwx/{}",
                e.expect("an entry").file_name().to_string_lossy()
            )
        })
        .filter(|rel| files.iter().any(|f| f.is_match(rel)))
        .collect();
    rels.sort();
    let mut found = Vec::new();
    for rel in &rels {
        let text = fs::read_to_string(root.join(rel)).expect("read an .rwx file");
        for matcher in &matchers {
            for caps in matcher.captures_iter(&text) {
                let value = caps.name("currentValue").expect("captures currentValue");
                let digest = caps.name("currentDigest").map(|d| d.as_str().to_string());
                found.push((rel.clone(), value.as_str().to_string(), digest));
            }
        }
    }

    let text = fs::read_to_string(root.join(CI)).expect("read .rwx/ci.yml");
    let version = the_one(&text, "`DEVBOX_USE_VERSION:` line", |l| {
        l.strip_prefix("DEVBOX_USE_VERSION:")
            .map(|v| v.trim().to_string())
    });
    let checksum = the_one(&text, "`sha256sum -c` line for /tmp/devbox.tar.gz", |l| {
        l.strip_prefix("echo \"")
            .and_then(|l| l.strip_suffix("  /tmp/devbox.tar.gz\" | sha256sum -c -"))
            .map(str::to_string)
    });
    assert_eq!(
        found,
        vec![(CI.to_string(), version.clone(), Some(checksum.clone()))],
        "renovate.json's jetify-com/devbox manager must read `DEVBOX_USE_VERSION` ({version}) \
         and the tarball's SHA-256 ({checksum}) in exactly one match, so a devbox bump moves \
         both in one PR (PR #2523)"
    );
}
