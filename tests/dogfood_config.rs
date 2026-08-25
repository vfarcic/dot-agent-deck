//! This repository's own `.dot-agent-deck.toml` — the config its maintainers
//! actually run the team from — parses, validates, and keeps its three
//! provider variants in step (issues #704, #705).
//!
//! Until now that file had no automated coverage at all: it is read by the
//! product rather than by the test suite, so a mistake in it surfaced when
//! somebody tried to open an orchestration, not when they ran `cargo test-fast`.
//! That was tolerable while it held one orchestration. With three, and with the
//! two variants inheriting from the first, the file has a *contract* — and the
//! answer to "what stops the three drifting apart" has to be something a machine
//! checks. `extends` does the sharing; this file proves the sharing took.

use std::path::{Path, PathBuf};

use dot_agent_deck::config_validation::{Severity, validate_config};
use dot_agent_deck::project_config::{
    OrchestrationConfig, ProjectConfig, default_orchestration, load_project_config,
};

/// The workspace root — where the config under test lives. Integration tests get
/// the crate root in `CARGO_MANIFEST_DIR`, which for this single-root workspace
/// is the same directory.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn dogfood_config() -> ProjectConfig {
    load_project_config(&repo_root())
        .expect("this repo's own .dot-agent-deck.toml must parse")
        .expect("this repo's own .dot-agent-deck.toml must exist")
}

fn orchestration<'a>(config: &'a ProjectConfig, name: &str) -> &'a OrchestrationConfig {
    config
        .orchestrations
        .iter()
        .find(|o| o.name == name)
        .unwrap_or_else(|| {
            panic!(
                "no orchestration named '{name}'; defined: {}",
                config
                    .orchestrations
                    .iter()
                    .map(|o| o.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

/// The file `dot-agent-deck validate` would accept — checked here so a broken
/// dogfood config fails the per-task gate rather than the next person to press
/// `Ctrl+N`.
#[test]
fn the_repo_config_validates_without_errors() {
    let issues = validate_config(&dogfood_config());
    let errors: Vec<String> = issues
        .iter()
        .filter(|i| i.severity == Severity::Error)
        .map(|i| i.to_string())
        .collect();
    assert!(
        errors.is_empty(),
        "this repo's own config must be one `dot-agent-deck validate` accepts:\n{}",
        errors.join("\n")
    );
}

/// The three provider variants exist and `mixed` is the DECLARED default, not
/// merely the first one written down (issue #704).
#[test]
fn the_repo_config_declares_mixed_as_its_default_orchestration() {
    let config = dogfood_config();
    for name in ["mixed", "anthropic", "GPT"] {
        assert!(
            !orchestration(&config, name).roles.is_empty(),
            "'{name}' must be spawnable — a roleless block is not offered as a dispatch target"
        );
    }

    let chosen = default_orchestration(&config, &repo_root())
        .expect("this repo defines spawnable orchestrations");
    assert_eq!(chosen.name, "mixed");
    assert_eq!(
        chosen.diagnostic(),
        None,
        "a repo that declares its default must produce no ambiguity diagnostic — if this fires, \
         the declaration was lost and every default run is back to depending on file order"
    );
}

/// **The sync guarantee.** `anthropic` and `GPT` must differ from `mixed` in
/// nothing but each role's `command`.
///
/// This is the mechanical form of the promise #705 makes to a contributor: pick
/// whichever provider you have credentials for and get *the same process*, not a
/// second-class one. Because both variants inherit through `extends`, a change to
/// `mixed`'s workflow reaches them automatically — so what this test really
/// guards is somebody replacing that inheritance with a copy, which is the exact
/// failure #304 was closed over.
#[test]
fn the_provider_variants_differ_from_mixed_only_in_their_commands() {
    let config = dogfood_config();
    let mixed = orchestration(&config, "mixed");

    for variant_name in ["anthropic", "GPT"] {
        let variant = orchestration(&config, variant_name);
        assert_eq!(
            variant.roles.len(),
            mixed.roles.len(),
            "'{variant_name}' must carry every role 'mixed' does"
        );
        for (theirs, ours) in variant.roles.iter().zip(&mixed.roles) {
            assert_eq!(
                theirs.name, ours.name,
                "role ORDER must match 'mixed' — a role's index is what the tab layout and the \
                 delegate path key panes on, so a reordered variant opens with its columns \
                 shuffled"
            );
            assert_eq!(
                (
                    theirs.start,
                    theirs.clear,
                    theirs.description.as_deref(),
                    theirs.prompt_template.as_deref()
                ),
                (
                    ours.start,
                    ours.clear,
                    ours.description.as_deref(),
                    ours.prompt_template.as_deref()
                ),
                "'{variant_name}' role '{}' diverged from 'mixed' in something other than its \
                 command — the whole point of the variants is that only the launcher differs",
                ours.name
            );
        }
    }
}

/// What each `devbox run <script>` in the config actually launches. Panics if
/// `devbox.json` does not define the script, which is the more urgent failure:
/// a role naming a script that does not exist opens a pane and dies with
/// `devbox: script not found`, which reads as an agent crash.
fn devbox_script_bodies() -> serde_json::Map<String, serde_json::Value> {
    let devbox: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(repo_root().join("devbox.json")).unwrap())
            .expect("devbox.json must parse");
    devbox
        .get("shell")
        .and_then(|s| s.get("scripts"))
        .and_then(|s| s.as_object())
        .cloned()
        .expect("devbox.json must define shell.scripts")
}

/// What one role's `command` ends up running, as a flat string. `None` for a
/// command that is not a `devbox run <script>` indirection.
fn launched_program(
    scripts: &serde_json::Map<String, serde_json::Value>,
    command: &str,
) -> Option<String> {
    let script = command.strip_prefix("devbox run ")?.trim();
    let body = scripts.get(script).unwrap_or_else(|| {
        panic!("a role runs `{command}`, but devbox.json defines no script named `{script}`")
    });
    Some(match body {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(lines) => lines
            .iter()
            .filter_map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join(" "),
        other => panic!("devbox script `{script}` is neither a string nor an array: {other}"),
    })
}

/// Every role command resolves to a devbox script that exists.
#[test]
fn every_role_command_names_a_devbox_script_that_exists() {
    let scripts = devbox_script_bodies();
    for orch in &dogfood_config().orchestrations {
        for role in &orch.roles {
            // The panic lives in the helper, which names the script; calling it
            // is the assertion.
            let _ = launched_program(&scripts, &role.command);
        }
    }
}

/// **The provider guarantee.** `anthropic` launches Claude for every role and
/// `GPT` launches OpenCode for every role — which is the entire reason those two
/// exist. A contributor picks one because it is the provider they have
/// credentials for; a single role that quietly runs the other stalls the run on
/// the credential they do not have, at whatever point that role is first
/// delegated to.
///
/// Checked through `devbox.json` rather than by reading the role's `command`,
/// because the command is an indirection (`devbox run agent-coder`) whose name
/// says nothing about what it launches — which is exactly how one could drift.
#[test]
fn each_single_provider_variant_launches_only_that_provider() {
    let scripts = devbox_script_bodies();
    let config = dogfood_config();
    for (variant, program) in [("anthropic", "claude"), ("GPT", "opencode")] {
        for role in &orchestration(&config, variant).roles {
            let launched = launched_program(&scripts, &role.command).unwrap_or_else(|| {
                panic!(
                    "'{variant}' role '{}' must go through devbox so its \
                                           provider is knowable from the repo",
                    role.name
                )
            });
            assert!(
                launched.split_whitespace().next() == Some(program),
                "'{variant}' role '{}' runs `{}` -> `{launched}`, which is not {program}. A \
                 single-provider orchestration with one foreign role is worse than no variant at \
                 all: it fails partway through a run, on the credential the contributor picked \
                 this variant to avoid needing",
                role.name,
                role.command
            );
        }
    }
}

/// And `mixed` earns its name: it is not silently one provider.
#[test]
fn mixed_actually_spans_more_than_one_launcher() {
    let scripts = devbox_script_bodies();
    let config = dogfood_config();
    let programs: std::collections::BTreeSet<String> = orchestration(&config, "mixed")
        .roles
        .iter()
        .filter_map(|r| launched_program(&scripts, &r.command))
        .filter_map(|p| p.split_whitespace().next().map(str::to_string))
        .collect();
    assert!(
        programs.len() > 1,
        "'mixed' resolved to a single launcher ({programs:?}) — then it duplicates one of the \
         single-provider variants and the three-way choice is a two-way one"
    );
}

/// The `[[modes]]` half of the same file, kept honest for the same reason: it is
/// what `dot-agent-deck` opens for a `dev` pane on this repo.
#[test]
fn the_repo_config_still_defines_its_dev_mode() {
    let config = dogfood_config();
    assert!(
        config.modes.iter().any(|m| m.name == "dev"),
        "the `dev` mode is what a pane opened on this repo uses; losing it is silent"
    );
}

/// A guard on the file's own most-documented trap: a top-level key written below
/// the first table header is silently absorbed into that table, and `validate`
/// still reports the config as valid. `worker_response_timeout_minutes` sits
/// above `[[modes]]` for exactly this reason — assert it survived, since the
/// symptom of losing it is only that a silent worker is never reported.
#[test]
fn the_top_level_timeout_key_is_still_above_the_first_table_header() {
    let text = std::fs::read_to_string(repo_root().join(".dot-agent-deck.toml")).unwrap();
    let key = text
        .find("worker_response_timeout_minutes")
        .expect("the key must still be in the file");
    let first_table = text
        .find("\n[[")
        .expect("the file defines at least one table");
    assert!(
        key < first_table,
        "`worker_response_timeout_minutes` has fallen below the first `[[table]]` header, so TOML \
         now reads it as a key of that table and it does nothing — and `dot-agent-deck validate` \
         still says `Config is valid.`"
    );
    assert_eq!(
        dogfood_config().worker_response_timeout_minutes,
        120,
        "and it must still parse as the top-level value it looks like"
    );
}

/// Nothing above should be readable as a claim about arbitrary user configs.
/// Pin the path so a future move of the file fails here rather than degrading
/// every test in this module into a vacuous pass.
#[test]
fn the_config_under_test_is_the_repo_root_one() {
    assert!(
        Path::new(&repo_root().join(".dot-agent-deck.toml")).is_file(),
        "these tests assert on THIS repo's config; if it moved, they are asserting on nothing"
    );
}
