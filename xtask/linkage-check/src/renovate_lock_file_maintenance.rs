//! The weekly lock file maintenance PR automerges, and the one setting that
//! keeps a bad refresh off `main` is `platformAutomerge: false` (PR #1488).
//! With it, Renovate merges only when every check run on the head is green;
//! without it, GitHub's auto-merge waits for the five required checks alone,
//! and the two jobs that reject a bad lock refresh — `desktop-web` (a pnpm
//! entry under pnpm's 24-hour floor, #813) and `desktop-driver` (a Tauri crate
//! and npm package on different releases, #1358) — are both unrequired.
//!
//! Nothing else would notice that setting going away: the config validator
//! accepts either value, and the failure it allows is a red `main` some weeks
//! later. So this pins it, and also fails on a later rule that could match a
//! lock file maintenance update and re-enable platform automerge, since later
//! packageRules override earlier ones.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("xtask/linkage-check sits two levels below the workspace root")
        .to_path_buf()
}

fn real_config() -> Value {
    serde_json::from_str(
        &fs::read_to_string(repo_root().join("renovate.json")).expect("read renovate.json"),
    )
    .expect("parse renovate.json")
}

/// Matchers a lock file maintenance update cannot satisfy, because it carries
/// no package, dependency type or version. A rule using one of them never
/// reaches that update, so it cannot override the merge settings for it.
const PACKAGE_MATCHERS: &[&str] = &[
    "matchPackageNames",
    "matchDepNames",
    "matchDepTypes",
    "matchDatasources",
    "matchCurrentVersion",
    "matchCurrentValue",
    "matchNewValue",
    "matchSourceUrls",
];

fn could_match_lock_file_maintenance(rule: &Value) -> bool {
    let update_types_allow = match rule.get("matchUpdateTypes") {
        None => true,
        Some(types) => types
            .as_array()
            .is_some_and(|types| types.iter().any(|t| t == "lockFileMaintenance")),
    };
    update_types_allow && PACKAGE_MATCHERS.iter().all(|key| rule.get(*key).is_none())
}

/// Is the rule dedicated to lock file maintenance alone — matching on the
/// update type and nothing else?
fn is_dedicated_rule(rule: &Value) -> bool {
    let match_keys: Vec<&String> = rule
        .as_object()
        .map(|rule| rule.keys().filter(|key| key.starts_with("match")).collect())
        .unwrap_or_default();
    match_keys.len() == 1 && rule["matchUpdateTypes"] == serde_json::json!(["lockFileMaintenance"])
}

fn check(config: &Value) -> Result<(), String> {
    let rules = config["packageRules"]
        .as_array()
        .ok_or("renovate.json has no packageRules array")?;
    let dedicated = rules
        .iter()
        .rposition(is_dedicated_rule)
        .ok_or("no packageRule matches `lockFileMaintenance` alone")?;
    let rule = &rules[dedicated];
    if rule["automerge"] != true || rule["automergeType"] != "pr" {
        return Err(
            "the lock file maintenance rule must set `automerge: true` with \
                    `automergeType: \"pr\"`"
                .into(),
        );
    }
    if rule["platformAutomerge"] != false {
        return Err(
            "the lock file maintenance rule must set `platformAutomerge: false`, \
                    or GitHub merges it on the required checks alone while `desktop-web` \
                    or `desktop-driver` is red"
                .into(),
        );
    }
    if rule["minimumReleaseAgeBehaviour"] != "timestamp-optional" {
        return Err(
            "the lock file maintenance rule must set `minimumReleaseAgeBehaviour: \
                    \"timestamp-optional\"`, or `renovate/stability-days` stays pending \
                    for ever and the automerge never fires"
                .into(),
        );
    }
    for (index, later) in rules.iter().enumerate().skip(dedicated + 1) {
        if could_match_lock_file_maintenance(later)
            && later
                .get("platformAutomerge")
                .is_some_and(|value| value != false)
        {
            return Err(format!(
                "packageRules[{index}] can match a lock file maintenance update and sets \
                 `platformAutomerge` after the rule that turns it off"
            ));
        }
    }
    Ok(())
}

/// Scenario: The real renovate.json automerges lock file maintenance through
/// Renovate's own all-checks merge rather than GitHub's required-checks one.
#[test]
fn lock_file_maintenance_automerges_only_on_every_check() {
    check(&real_config()).unwrap_or_else(|err| panic!("{err}"));
}

/// Scenario: Each edit that would let a lock refresh merge past a red
/// unrequired check is rejected, so the guard above cannot pass vacuously.
#[test]
fn each_unsafe_edit_is_rejected() {
    let dedicated = |config: &Value| -> usize {
        config["packageRules"]
            .as_array()
            .unwrap()
            .iter()
            .rposition(is_dedicated_rule)
            .expect("the real config has the dedicated rule")
    };

    let mut platform = real_config();
    let at = dedicated(&platform);
    platform["packageRules"][at]["platformAutomerge"] = Value::Bool(true);
    assert!(
        check(&platform).is_err(),
        "platformAutomerge: true must fail"
    );

    let mut dropped = real_config();
    let at = dedicated(&dropped);
    dropped["packageRules"][at]
        .as_object_mut()
        .unwrap()
        .remove("platformAutomerge");
    assert!(
        check(&dropped).is_err(),
        "a missing platformAutomerge must fail"
    );

    let mut pending = real_config();
    let at = dedicated(&pending);
    pending["packageRules"][at]
        .as_object_mut()
        .unwrap()
        .remove("minimumReleaseAgeBehaviour");
    assert!(
        check(&pending).is_err(),
        "the default timestamp-required must fail"
    );

    let mut overridden = real_config();
    overridden["packageRules"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({ "matchManagers": ["npm"], "platformAutomerge": true }));
    assert!(
        check(&overridden).is_err(),
        "a later manager-wide rule re-enabling platform automerge must fail"
    );

    let mut unrelated = real_config();
    unrelated["packageRules"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "matchPackageNames": ["some-crate"],
            "platformAutomerge": true
        }));
    assert!(
        check(&unrelated).is_ok(),
        "a later package-scoped rule cannot reach lock file maintenance and must pass"
    );
}
