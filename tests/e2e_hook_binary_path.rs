#![cfg(all(feature = "e2e", unix))]

//! L2 regression coverage for PRD #381 — hook installation must resolve a
//! DURABLE binary path before writing it into another program's persistent,
//! user-level configuration.
//!
//! Since PR #1029 it also carries `hooks/install/006`, which is not about
//! durability: it is issue #730's two config-file defects driven through the
//! same unattended startup install, because that is where both actually fired.
//! The file is shared because the seeding, launching and JSON-walking fixtures
//! are exactly the ones those defects need.
//!
//! Every test here drives the real `dot-agent-deck` binary through the PTY
//! harness, which under `cargo test-e2e` genuinely IS a `target/debug`
//! artifact. For the two durability tests that is precisely what makes them the
//! regression guard for PRD #381: the seam the existing hook-install tests
//! drive (`hooks_manage::auto_install_to`) hardcodes
//! `let binary_path = "dot-agent-deck".to_string();`, so no test ever executes
//! the `std::env::current_exe()` derivation that produced the field defect — a
//! Stop hook failing with `/bin/sh: 1: …/dot-agent-deck-pr-356/target/release/
//! dot-agent-deck: not found` after that worktree was pruned.
//!
//! Gated `unix` as well as `e2e` (like `e2e_signals.rs`) because the fixtures
//! depend on the executable bit and on the `~/.local/bin` convention that the
//! resolver's first durable candidate names.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use common::{TuiDeck, harness_tempdir};
use serde_json::{Value, json};
use spec::spec;

/// The exact signature every deck-authored Claude rule ends with — the literal
/// value of `hooks_manage::HOOK_COMMAND_SUFFIX`.
const CLAUDE_SUFFIX: &str = "hook --agent claude-code";

/// The Codex equivalent — `codex_hooks_manage::HOOK_COMMAND_SUFFIX`.
const CODEX_SUFFIX: &str = "hook --agent codex";

/// The two directory fragments that mark a cargo build artifact. PRD #381's
/// central invariant is that neither may ever reach a config file the deck does
/// not own.
const BUILD_ARTIFACT_MARKERS: &[&str] = &["target/debug", "target/release"];

/// A stand-in executable. Never run by these tests — the resolver's contract for
/// `~/.local/bin/dot-agent-deck` is "exists and is executable", which is a stat,
/// so the body only has to be a valid script.
const STUB_BODY: &str = "#!/bin/sh\nexit 0\n";

/// Write `body` at `path`, creating parents, with mode 0o755.
fn write_executable(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .unwrap_or_else(|e| panic!("create {}: {e}", parent.display()));
    }
    std::fs::write(path, body).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .unwrap_or_else(|e| panic!("chmod {}: {e}", path.display()));
}

/// `Some(contents)` when `path` exists and is readable, `None` when it is absent.
fn read_if_present(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// Every `"command"` string anywhere in a JSON hook document. Walking the whole
/// tree rather than the documented nesting keeps the assertion honest if the
/// rule shape moves: a build-artifact path smuggled into a differently-shaped
/// rule is still caught.
fn collect_commands(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if key == "command"
                    && let Some(text) = child.as_str()
                {
                    out.push(text.to_string());
                }
                collect_commands(child, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_commands(item, out);
            }
        }
        _ => {}
    }
}

/// The deck-owned commands in the JSON hook document at `path` — those ending
/// with `suffix`, which is how both `hooks_manage` and `codex_hooks_manage`
/// identify their own rules. An absent file yields none.
fn deck_commands(path: &Path, suffix: &str) -> Vec<String> {
    let Some(body) = read_if_present(path) else {
        return Vec::new();
    };
    let doc: Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("parse {} as JSON: {e}\n{body}", path.display()));
    let mut all = Vec::new();
    collect_commands(&doc, &mut all);
    all.retain(|command| command.trim_end().ends_with(suffix));
    all
}

/// The value of `const BINARY_PATH = "…";` in the generated OpenCode plugin.
/// The plugin is JavaScript, not JSON — the shape difference PRD #381 calls out
/// as the reason the third integration was the last to be noticed.
fn opencode_binary_path(js: &str) -> Option<String> {
    let rest = js.split_once("const BINARY_PATH = ")?.1;
    let literal = rest.split_once(";\n")?.0;
    serde_json::from_str::<String>(literal).ok()
}

/// Every directory on `path_value` that actually holds a `dot-agent-deck`. Used
/// as an explicit pre-flight so neither test can pass because the host's own
/// installed deck leaked in through an inherited `PATH`.
fn deck_on_path(path_value: &str) -> Vec<PathBuf> {
    std::env::split_paths(path_value)
        .map(|dir| dir.join("dot-agent-deck"))
        .filter(|candidate| candidate.exists())
        .collect()
}

/// Every way `contents` violates "no build artifact reaches persistent config".
///
/// Findings are collected rather than asserted one at a time so a single run
/// reports all three integrations at once. Claude, Codex and OpenCode each have
/// their own writer and the field defect hit all three simultaneously, so a test
/// that stops at the first one hides two thirds of the picture.
fn build_artifact_problems(label: &str, contents: &str) -> Vec<String> {
    BUILD_ARTIFACT_MARKERS
        .iter()
        .filter(|marker| contents.contains(**marker))
        .map(|marker| {
            format!(
                "{label} contains `{marker}` — a build artifact is gitignored, removed by \
                 `cargo clean`, and gone when its worktree is pruned, so it must never be \
                 written into persistent user config"
            )
        })
        .collect()
}

/// Every way `commands` fails to name the durable executable: a command that does
/// not start with `durable`, or one that is a bare command name (hooks run under
/// `/bin/sh` with an environment the deck does not control, so the written value
/// must be an absolute path to a file that exists).
fn durable_command_problems(label: &str, commands: &[String], durable: &str) -> Vec<String> {
    let mut problems = Vec::new();
    for command in commands {
        if !command.starts_with(durable) {
            problems.push(format!(
                "{label}: hook command `{command}` does not name the durable executable \
                 `{durable}` (resolution order step 2a)"
            ));
        }
        if command.starts_with("dot-agent-deck ") {
            problems.push(format!(
                "{label}: hook command `{command}` is a bare command name, not an absolute \
                 path to a file that exists"
            ));
        }
    }
    problems
}

/// Panic with every collected finding at once, or return quietly when there are
/// none.
fn assert_no_problems(problems: &[String]) {
    assert!(
        problems.is_empty(),
        "PRD #381 violations ({}):\n  - {}",
        problems.len(),
        problems.join("\n  - ")
    );
}

/// Seed the three agent config directories whose presence gates each installer,
/// plus a stub `codex` on `PATH` (Codex's startup installer self-guards on
/// `codex` being resolvable). Returns the `PATH` to hand the deck.
fn seed_agent_homes(home: &Path, stub_bin: &Path) -> String {
    for dir in [".claude", ".codex", ".opencode"] {
        std::fs::create_dir_all(home.join(dir))
            .unwrap_or_else(|e| panic!("seed {}: {e}", home.join(dir).display()));
    }
    write_executable(&stub_bin.join("codex"), STUB_BODY);
    format!("{}:/usr/bin:/bin", stub_bin.display())
}

/// Launch the real deck against the `minimal` fixture with `home` as its HOME
/// and `path_value` as its PATH, and wait until the dashboard has painted.
///
/// `main.rs` runs every `AgentSpec::startup_auto_install` synchronously BEFORE
/// `run_tui`, so the painted empty-state line is a sufficient barrier: all three
/// installers have already had their chance to write by the time it appears.
/// Deliberately not `wait_until_quiescent` — the dashboard's periodic redraw tick
/// never leaves a 50 ms idle window, so quiescence times out here (measured).
fn launch_with_home(home: &Path, path_value: &str) -> TuiDeck {
    let deck = TuiDeck::builder()
        .with_env("HOME", home.to_str().expect("HOME path is UTF-8"))
        .with_env(
            "XDG_CONFIG_HOME",
            home.join(".config")
                .to_str()
                .expect("XDG config path is UTF-8"),
        )
        .with_env("PATH", path_value)
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");
    deck
}

/// Scenario: Launch the real (`target/debug`) deck into an isolated HOME that has
/// `~/.claude/`, `~/.codex/` and `~/.opencode/` present and a durable executable
/// seeded at `$HOME/.local/bin/dot-agent-deck`, with no `dot-agent-deck` on the
/// child's PATH. Every hook command the deck writes into `~/.claude/settings.json`
/// and `~/.codex/hooks.json`, and the OpenCode plugin's `BINARY_PATH`, must name
/// that seeded durable path, and no written config may mention `target/debug` or
/// `target/release`.
#[spec("hooks/install/004")]
#[test]
fn install_004_durable_path_is_written_not_the_build_artifact() {
    let sandbox = harness_tempdir().expect("harness tempdir for the isolated HOME");
    let home = sandbox.path().join("home");
    let stub_bin = sandbox.path().join("stubbin");
    let path_value = seed_agent_homes(&home, &stub_bin);

    // The durable candidate the resolver must prefer (PRD #381 step 2a — the
    // same choice `remote.rs:1034` already makes).
    let durable_path = home.join(".local").join("bin").join("dot-agent-deck");
    write_executable(&durable_path, STUB_BODY);
    let durable = durable_path
        .to_str()
        .expect("durable path is UTF-8")
        .to_string();

    // Pinned rather than inherited: with no `dot-agent-deck` reachable through
    // PATH, a pass can only come from the seeded `~/.local/bin` candidate, never
    // from the host's own installed deck (this machine has one at that very
    // location in the REAL home).
    assert!(
        deck_on_path(&path_value).is_empty(),
        "PATH handed to the deck must contain no `dot-agent-deck`: {path_value}"
    );

    let _deck = launch_with_home(&home, &path_value);

    let mut problems: Vec<String> = Vec::new();

    let settings = home.join(".claude").join("settings.json");
    match read_if_present(&settings) {
        None => problems.push(format!(
            "~/.claude/settings.json: absent at {} — `~/.claude/` was seeded, so the \
             installer should have written",
            settings.display()
        )),
        Some(body) => {
            let commands = deck_commands(&settings, CLAUDE_SUFFIX);
            if commands.is_empty() {
                problems.push(format!(
                    "~/.claude/settings.json: no deck-owned rule\n{body}"
                ));
            }
            problems.extend(durable_command_problems(
                "~/.claude/settings.json",
                &commands,
                &durable,
            ));
            problems.extend(build_artifact_problems("~/.claude/settings.json", &body));
        }
    }

    let codex_hooks = home.join(".codex").join("hooks.json");
    match read_if_present(&codex_hooks) {
        None => problems.push(format!(
            "~/.codex/hooks.json: absent at {} — a stub `codex` is on PATH, so the startup \
             installer should have written",
            codex_hooks.display()
        )),
        Some(body) => {
            let commands = deck_commands(&codex_hooks, CODEX_SUFFIX);
            if commands.is_empty() {
                problems.push(format!("~/.codex/hooks.json: no deck-owned rule\n{body}"));
            }
            problems.extend(durable_command_problems(
                "~/.codex/hooks.json",
                &commands,
                &durable,
            ));
            problems.extend(build_artifact_problems("~/.codex/hooks.json", &body));
        }
    }

    // The OpenCode integration is a GENERATED JAVASCRIPT file, not JSON — the
    // shape difference PRD #381 blames for it being the last of the three to be
    // noticed, so it is checked through its own parse rather than the JSON one.
    let plugin = home
        .join(".opencode")
        .join("plugin")
        .join("dot-agent-deck.js");
    match read_if_present(&plugin) {
        None => problems.push(format!(
            "OpenCode plugin: absent at {} — `~/.opencode/` was seeded, so the installer \
             should have written",
            plugin.display()
        )),
        Some(body) => {
            match opencode_binary_path(&body) {
                Some(binary_path) if binary_path == durable => {}
                Some(binary_path) => problems.push(format!(
                    "OpenCode plugin: BINARY_PATH is `{binary_path}`, not the durable \
                     executable `{durable}`"
                )),
                None => problems.push(format!(
                    "OpenCode plugin: no `const BINARY_PATH = \"…\";` in {}",
                    plugin.display()
                )),
            }
            problems.extend(build_artifact_problems("OpenCode plugin", &body));
        }
    }

    assert_no_problems(&problems);
}

/// Scenario: Launch the real (`target/debug`) deck into an isolated HOME that has
/// `~/.claude/`, `~/.codex/` and `~/.opencode/` present but NO
/// `$HOME/.local/bin/dot-agent-deck` and no `dot-agent-deck` anywhere on the
/// child's PATH. With no durable path resolvable the deck must write no hook rule
/// at all for any of the three integrations, and the dashboard must still paint —
/// refusing is not fatal.
#[spec("hooks/install/005")]
#[test]
fn install_005_refuses_to_write_when_no_durable_path_exists() {
    let sandbox = harness_tempdir().expect("harness tempdir for the isolated HOME");
    let home = sandbox.path().join("home");
    let stub_bin = sandbox.path().join("stubbin");
    let path_value = seed_agent_homes(&home, &stub_bin);

    // Deliberately NOT seeded — this is the case with no durable path at all.
    assert!(
        !home
            .join(".local")
            .join("bin")
            .join("dot-agent-deck")
            .exists(),
        "the no-durable-path case must not have a ~/.local/bin/dot-agent-deck"
    );
    // If the host's installed deck leaked in through an inherited PATH the
    // resolver would legitimately find a durable path and this test would pass
    // for the wrong reason. Checked explicitly rather than assumed — the harness
    // passes the host PATH through by default, and this machine really does have
    // a `dot-agent-deck` on it.
    assert!(
        deck_on_path(&path_value).is_empty(),
        "PATH handed to the deck must contain no `dot-agent-deck`: {path_value}"
    );

    // The `wait_for_string` inside is the assertion that the TUI came up: a
    // refusal on the startup-install path must not be fatal.
    let _deck = launch_with_home(&home, &path_value);

    let mut problems: Vec<String> = Vec::new();

    let settings = home.join(".claude").join("settings.json");
    let claude_commands = deck_commands(&settings, CLAUDE_SUFFIX);
    if !claude_commands.is_empty() {
        problems.push(format!(
            "~/.claude/settings.json: {} deck-owned rule(s) written with no durable path \
             available: {claude_commands:?}",
            claude_commands.len()
        ));
    }

    let codex_hooks = home.join(".codex").join("hooks.json");
    let codex_commands = deck_commands(&codex_hooks, CODEX_SUFFIX);
    if !codex_commands.is_empty() {
        problems.push(format!(
            "~/.codex/hooks.json: {} deck-owned rule(s) written with no durable path \
             available: {codex_commands:?}",
            codex_commands.len()
        ));
    }

    let plugin = home
        .join(".opencode")
        .join("plugin")
        .join("dot-agent-deck.js");
    if let Some(body) = read_if_present(&plugin) {
        problems.push(format!(
            "OpenCode plugin: written at {} with no durable path available; BINARY_PATH = \
             {:?}",
            plugin.display(),
            opencode_binary_path(&body)
        ));
    }

    assert_no_problems(&problems);
}

/// Every `"command"` string anywhere inside one rule object.
fn commands_in(rule: &Value) -> Vec<String> {
    let mut out = Vec::new();
    collect_commands(rule, &mut out);
    out
}

/// The rules under `event` in the hook document at `path`, or an empty slice's
/// worth when the file, the `hooks` object, or the event key is absent.
fn event_rules(doc: &Value, event: &str) -> Vec<Value> {
    doc.get("hooks")
        .and_then(|hooks| hooks.get(event))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Read `path` as JSON, panicking with the raw bytes when it will not parse —
/// an unparseable `hooks.json` after an install is itself the interesting
/// failure, so it must not surface as a bare `None`.
fn read_json(path: &Path) -> Value {
    let body = read_if_present(path)
        .unwrap_or_else(|| panic!("{} is absent after the install", path.display()));
    serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("parse {} as JSON: {e}\n{body}", path.display()))
}

/// Scenario: Seed `~/.codex/hooks.json` before launch with, under one installed
/// event and one the deck no longer installs, a rule holding the deck's own
/// command next to a user handler that carries no string `command` under the
/// user's `matcher`, plus a second deck-owned rule pinning a different, still
/// valid, seeded `dot-agent-deck`. Launch the real deck so its unattended
/// startup install runs, and the user's handler, their `matcher` and the foreign
/// valid pin must all survive unchanged while the deck refreshes only its own
/// command.
#[spec("hooks/install/006")]
#[test]
fn install_006_startup_install_keeps_sibling_handlers_and_valid_foreign_pins() {
    let sandbox = harness_tempdir().expect("harness tempdir for the isolated HOME");
    let home = sandbox.path().join("home");
    let stub_bin = sandbox.path().join("stubbin");
    let path_value = seed_agent_homes(&home, &stub_bin);

    // The install this launch performs — the binary whose prior command may be
    // refreshed.
    let durable_path = home.join(".local").join("bin").join("dot-agent-deck");
    write_executable(&durable_path, STUB_BODY);
    let durable = durable_path.to_str().expect("durable path is UTF-8");
    let ours = format!("{durable} {CODEX_SUFFIX}");

    // A SECOND, genuinely different install: absolute, present, executable, and
    // not under `target/`, so `pin_is_repairable` says the deck could have
    // written it and `command_is_replaceable` must leave it alone. Sharing the
    // `dot-agent-deck` basename is the point — that is what makes it a sibling
    // rather than an unrelated user hook, and the pre-#730 code repointed it on
    // every launch.
    let foreign_path = home.join(".other").join("bin").join("dot-agent-deck");
    write_executable(&foreign_path, STUB_BODY);
    let foreign = format!(
        "{} {CODEX_SUFFIX}",
        foreign_path.to_str().expect("foreign path is UTF-8")
    );

    assert!(
        deck_on_path(&path_value).is_empty(),
        "PATH handed to the deck must contain no `dot-agent-deck`: {path_value}"
    );

    // A handler object with NO string `command`. Codex's structural validator
    // requires only that each event value be an ARRAY, so this is a shape a
    // user's file can legitimately hold — and it is invisible to the deck's
    // per-command strip, which is how the pre-#1029 emptiness test came to
    // delete it along with the `matcher` it shares.
    let sibling = json!({ "type": "audit", "script": "/usr/local/bin/my-critical-audit.sh" });
    let shared_rule = json!({
        "matcher": "Bash",
        "hooks": [ { "type": "command", "command": ours }, sibling ]
    });
    let foreign_rule = json!({
        "matcher": "Read",
        "hooks": [ { "type": "command", "command": foreign } ]
    });

    // `PreToolUse` is installed by the deck; `SessionEnd` is NOT in
    // `CODEX_HOOK_EVENTS` but is accepted and run by Codex 0.149.0, so it goes
    // through `install_impl`'s retired-event sweep — the second call site of the
    // same shared strip, which must behave identically.
    let codex_hooks = home.join(".codex").join("hooks.json");
    let seeded = json!({
        "hooks": {
            "PreToolUse": [ shared_rule, foreign_rule ],
            "SessionEnd": [ shared_rule, foreign_rule ],
        }
    });
    std::fs::write(
        &codex_hooks,
        serde_json::to_string_pretty(&seeded).expect("serialize seeded hooks.json"),
    )
    .unwrap_or_else(|e| panic!("seed {}: {e}", codex_hooks.display()));

    let _deck = launch_with_home(&home, &path_value);

    let doc = read_json(&codex_hooks);
    let mut problems: Vec<String> = Vec::new();

    // The install must actually have RUN, or every assertion below passes
    // because nothing was touched. Asked on an event the seed deliberately left
    // out: `ours` is seeded, so finding it anywhere proves nothing, while a
    // `SessionStart` rule can only have been written by this launch.
    let wrote = event_rules(&doc, "SessionStart")
        .iter()
        .any(|rule| commands_in(rule).contains(&ours));
    if !wrote {
        problems.push(format!(
            "~/.codex/hooks.json: no `SessionStart` rule names the durable binary `{durable}`, \
             so the startup install did not run and this test proves nothing\n{doc:#}"
        ));
    }
    // And it must have ADDED its refreshed rule beside the two seeded ones
    // rather than replacing the array: two survivors plus one fresh rule.
    let installed = event_rules(&doc, "PreToolUse");
    if installed.len() != 3 {
        problems.push(format!(
            "PreToolUse: expected the user's rule, the foreign pin's rule and one fresh deck \
             rule, got {}\n{installed:#?}",
            installed.len()
        ));
    }

    for event in ["PreToolUse", "SessionEnd"] {
        let rules = event_rules(&doc, event);

        // Issue #730 / Greptile P1 on #1029: the user's rule survives the strip
        // of the deck's command that shared it, carrying the handler and the
        // matcher the user wrote.
        let kept = rules
            .iter()
            .find(|rule| rule.get("matcher").and_then(Value::as_str) == Some("Bash"));
        match kept {
            None => problems.push(format!(
                "{event}: the rule the deck's command shared with a user handler was deleted, \
                 taking the handler and its `matcher` with it\n{doc:#}"
            )),
            Some(rule) => {
                let handlers = rule
                    .get("hooks")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                if !handlers.contains(&sibling) {
                    problems.push(format!(
                        "{event}: the user's command-less handler is gone from their own \
                         rule: {rule:#}"
                    ));
                }
                if handlers
                    .iter()
                    .any(|h| h.get("command").and_then(Value::as_str) == Some(ours.as_str()))
                {
                    problems.push(format!(
                        "{event}: the deck's stale command was left inside the user's rule \
                         rather than refreshed into its own: {rule:#}"
                    ));
                }
            }
        }

        // Issue #730's other half: a deck pin naming a DIFFERENT, still-usable
        // install is not the deck's to repoint, so it is left exactly as it was
        // and the launching deck's rule is added beside it.
        if !rules.contains(&foreign_rule) {
            problems.push(format!(
                "{event}: the deck-owned rule pinning the still-valid second install \
                 `{foreign}` was repointed or removed\n{doc:#}"
            ));
        }
    }

    // The retired event gets no fresh rule of the deck's — the sweep clears, it
    // does not install.
    let retired = event_rules(&doc, "SessionEnd");
    if retired.len() != 2 {
        problems.push(format!(
            "SessionEnd: expected exactly the two seeded rules to remain, got {}\n{retired:#?}",
            retired.len()
        ));
    }

    assert_no_problems(&problems);
}
