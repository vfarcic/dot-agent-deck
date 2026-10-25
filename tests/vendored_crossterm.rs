//! The deck builds against the patched crossterm in `vendor/crossterm`, not the
//! registry crate (`vendor/crossterm/VENDORED.md`).
//!
//! `[patch.crates-io]` applies only while the version it supplies satisfies
//! what the dependents ask for. A crossterm bump that outgrows it, such as a
//! Renovate PR, makes cargo resolve the registry crate with only a warning,
//! and keys typed after a resize strand again. The lock file records which
//! one cargo chose, so this reads it.

use std::path::Path;

fn read(path: &str) -> toml::Table {
    let full = Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
    let text =
        std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("read {}: {e}", full.display()));
    text.parse()
        .unwrap_or_else(|e| panic!("parse {}: {e}", full.display()))
}

#[test]
fn the_lock_resolves_crossterm_to_the_vendored_copy_alone() {
    let manifest = read("Cargo.toml");
    let patch = manifest["patch"]["crates-io"]["crossterm"]["path"].as_str();
    assert_eq!(
        patch,
        Some("vendor/crossterm"),
        "the root Cargo.toml's [patch.crates-io] entry moved"
    );

    let vendored = read("vendor/crossterm/Cargo.toml");
    let vendored_version = vendored["package"]["version"].as_str().unwrap();

    let lock = read("Cargo.lock");
    let entries: Vec<&toml::Table> = lock["package"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p.as_table())
        .filter(|p| p["name"].as_str() == Some("crossterm"))
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "Cargo.lock has {} crossterm entries; with the patch applied, every dependent \
         shares the vendored copy: {entries:?}",
        entries.len()
    );
    let entry = entries[0];
    assert!(
        !entry.contains_key("source"),
        "Cargo.lock's crossterm comes from {:?}, not vendor/crossterm: the \
         [patch.crates-io] entry no longer applies. Re-apply the patch to the new \
         version or drop it, as vendor/crossterm/VENDORED.md says.",
        entry["source"]
    );
    assert_eq!(entry["version"].as_str(), Some(vendored_version));
}
