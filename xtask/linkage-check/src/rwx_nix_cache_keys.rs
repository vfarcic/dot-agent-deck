//! Issue #2529: a tool cache in `.rwx/ci.yml` holds what its task changed on
//! top of the layers it ran on. The `nix` and `devbox` tasks run on the
//! `nix-install` layer, so their caches hold Nix store paths and a Nix
//! database written over one Nix release. Under a name that did not change
//! with `NIX_VERSION` (`ci-devbox-v1`), the 2026-11-26 cron rewrote the cache
//! over Nix 2.36.0, PR #2491's head on a 2.35.6 base restored it, and
//! `devbox run` exited 126 on a store path that was not there.
//!
//! So those caches carry the Nix release in their names, and renovate.json's
//! NixOS/nix manager reads that version out of the names as well as out of
//! `NIX_VERSION`, so one Renovate PR moves all of them. These tests hold the
//! three together:
//!
//! - every tool cache of a task that uses `nix-install` names the
//!   `NIX_VERSION` that task installs;
//! - the manager's matchStrings extract exactly the `NIX_VERSION` line and
//!   those tool-cache lines, so a renamed cache that the manager would stop
//!   reading goes red here rather than on the next bump.

use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;
use yaml_rust2::{Yaml, YamlLoader};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("xtask/linkage-check sits two levels below the workspace root")
        .to_path_buf()
}

const CI: &str = ".rwx/ci.yml";

/// A task's `use:`, which RWX accepts as one key or a list of keys.
fn uses(task: &Yaml) -> Vec<&str> {
    match &task["use"] {
        Yaml::String(s) => vec![s.as_str()],
        Yaml::Array(a) => a.iter().filter_map(Yaml::as_str).collect(),
        _ => Vec::new(),
    }
}

/// What the RWX definition `text` says: the `NIX_VERSION` the `nix-install`
/// task sets, and the tool-cache name of every task that uses `nix-install`,
/// as `(task key, cache name)`.
fn nix_pins(text: &str) -> (String, Vec<(String, String)>) {
    let docs = YamlLoader::load_from_str(text).expect("the RWX definition is valid YAML");
    let tasks = docs[0]["tasks"].as_vec().expect("a `tasks:` list");
    let key = |t: &Yaml| t["key"].as_str().unwrap_or_default().to_string();
    let installs: Vec<&Yaml> = tasks.iter().filter(|t| key(t) == "nix-install").collect();
    let [install] = installs[..] else {
        panic!("exactly one `nix-install` task, found {}", installs.len());
    };
    let version = install["env"]["NIX_VERSION"]
        .as_str()
        .expect("`nix-install` sets NIX_VERSION as an X.Y.Z string")
        .to_string();
    let caches = tasks
        .iter()
        .filter(|t| uses(t).contains(&"nix-install"))
        .filter_map(|t| t["tool-cache"].as_str().map(|c| (key(t), c.to_string())))
        .collect();
    (version, caches)
}

/// The tool caches in `text` that do not name its `NIX_VERSION`, as messages.
fn stale_caches(text: &str) -> Vec<String> {
    let (version, caches) = nix_pins(text);
    let needle = format!("nix-{version}-");
    caches
        .into_iter()
        .filter(|(_, cache)| !cache.contains(&needle))
        .map(|(task, cache)| {
            format!("task `{task}` uses nix-install at {version}, but its tool cache `{cache}` does not name `{needle}`")
        })
        .collect()
}

#[test]
fn every_nix_task_cache_names_the_installed_nix_release() {
    let text = fs::read_to_string(repo_root().join(CI)).expect("read .rwx/ci.yml");
    let (_, caches) = nix_pins(&text);
    let tasks: Vec<&str> = caches.iter().map(|(t, _)| t.as_str()).collect();
    for expected in ["nix", "devbox"] {
        assert!(
            tasks.contains(&expected),
            "`{expected}` must keep a tool cache on the nix-install layer, or this test no \
             longer covers it; found {tasks:?}"
        );
    }
    let stale = stale_caches(&text);
    assert!(
        stale.is_empty(),
        "{CI}: a tool cache written over one Nix release must not be restored over another \
         (issue #2529); rename it with the version:\n{}",
        stale.join("\n")
    );
}

const FIXTURE: &str = "
tasks:
  - key: nix-install
    env:
      NIX_VERSION: 2.36.0
  - key: nix
    use: [code, nix-install]
    tool-cache: ci-nix-2.36.0-v1
  - key: devbox
    use: nix-install
    tool-cache: CACHE
  - key: other
    use: [code]
    tool-cache: ci-other-v1
";

#[test]
fn a_matching_name_passes_and_a_task_off_the_nix_layer_is_ignored() {
    assert_eq!(
        stale_caches(&FIXTURE.replace("CACHE", "ci-devbox-nix-2.36.0-v1")),
        Vec::<String>::new()
    );
}

#[test]
fn a_name_without_the_version_fails() {
    let stale = stale_caches(&FIXTURE.replace("CACHE", "ci-devbox-v1"));
    assert_eq!(stale.len(), 1, "{stale:?}");
    assert!(stale[0].contains("`devbox`"), "{stale:?}");
}

#[test]
fn a_name_left_on_the_previous_release_fails() {
    let stale = stale_caches(&FIXTURE.replace("CACHE", "ci-devbox-nix-2.35.6-v1"));
    assert_eq!(stale.len(), 1, "{stale:?}");
    assert!(stale[0].contains("ci-devbox-nix-2.35.6-v1"), "{stale:?}");
}

/// renovate.json's NixOS/nix manager, as its compiled file patterns and
/// matchStrings.
fn nix_manager() -> (Vec<Regex>, Vec<Regex>) {
    let text = fs::read_to_string(repo_root().join("renovate.json")).expect("read renovate.json");
    let config: serde_json::Value = serde_json::from_str(&text).expect("renovate.json is JSON");
    let managers: Vec<&serde_json::Value> = config["customManagers"]
        .as_array()
        .expect("customManagers")
        .iter()
        .filter(|m| m["depNameTemplate"] == "NixOS/nix")
        .collect();
    let [manager] = managers[..] else {
        panic!(
            "renovate.json must carry exactly one NixOS/nix customManager, found {}",
            managers.len()
        );
    };
    let regexes = |key: &str, strip_slashes: bool| -> Vec<Regex> {
        manager[key]
            .as_array()
            .unwrap_or_else(|| panic!("the NixOS/nix manager has a {key} array"))
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
        regexes("managerFilePatterns", true),
        regexes("matchStrings", false),
    )
}

#[test]
fn renovate_reads_the_nix_pin_and_every_nix_cache_name() {
    let root = repo_root();
    let (files, matchers) = nix_manager();
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
                let line = text[..value.start()].matches('\n').count() + 1;
                found.push(format!("{rel}:{line} {}", value.as_str()));
            }
        }
    }
    found.sort();

    let text = fs::read_to_string(root.join(CI)).expect("read .rwx/ci.yml");
    let (version, caches) = nix_pins(&text);
    let line_of = |wanted: &dyn Fn(&str) -> bool, what: &str| -> usize {
        let lines: Vec<usize> = text
            .lines()
            .enumerate()
            .filter(|(_, l)| wanted(l.trim()))
            .map(|(n, _)| n + 1)
            .collect();
        let [line] = lines[..] else {
            panic!("{CI} must have exactly one {what} line, found {lines:?}");
        };
        line
    };
    let mut expected = vec![format!(
        "{CI}:{} {version}",
        line_of(&|l| l.starts_with("NIX_VERSION:"), "`NIX_VERSION:`")
    )];
    for (_, cache) in &caches {
        let line = line_of(
            &|l| l == format!("tool-cache: {cache}"),
            &format!("`tool-cache: {cache}`"),
        );
        expected.push(format!("{CI}:{line} {version}"));
    }
    expected.sort();
    assert_eq!(
        found, expected,
        "renovate.json's NixOS/nix manager must read exactly `NIX_VERSION` and the tool-cache \
         name of every task on the nix-install layer, all at {version}, so a Nix bump renames \
         those caches in the same PR (issue #2529)"
    );
}
