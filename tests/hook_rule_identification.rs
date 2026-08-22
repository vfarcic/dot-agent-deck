//! Claude Code hook rules used to be identified by an
//! `.contains("dot-agent-deck")` substring check
//! ([`dot_agent_deck::hooks_manage`]), so a deck binary installed under any
//! other filename could not see the rule it had just written. Repeated
//! auto-installs then accumulated one rule per startup instead of replacing
//! the prior one. Rules are now identified by their command shape (the
//! `--agent claude-code` suffix) rather than by the binary's name, and a
//! specific binary is matched against an existing rule by its resolved
//! (canonicalized) on-disk identity, not a literal string comparison.
//!
//! These tests exercise `install_to` / `uninstall_from` — the same
//! explicit-settings-path seam `codex_hooks_safety.rs` uses for the sibling
//! Codex matcher — against a `tempfile` settings.json fixture. No `$HOME`
//! manipulation, no spawned processes.
//!
//! `_013` onwards widen the remit from *which rule is ours* to *what the deck
//! is allowed to do to a file it does not own* — the same family of defect seen
//! from the other end. `~/.claude/settings.json` belongs to the user and holds
//! their `model`, `env` and `permissions` alongside the deck's hooks, so every
//! one of these pins a property of the form "the user's own content is still
//! there afterwards": #516/#522 (a file the deck cannot parse is never
//! rewritten, on install *and* uninstall), #535 (a user command co-located in
//! one rule object with the deck's is never deleted along with it), and #534
//! (the publish is a same-directory temp + `rename`, so a concurrent reader
//! never sees a truncated file and a crash mid-write leaves the original
//! intact).

use std::path::{Path, PathBuf};

use dot_agent_deck::hooks_manage::{install_to, uninstall_from};
use serde_json::{Value, json};

#[path = "../src/test_temp.rs"]
mod test_temp;

fn settings_path() -> (tempfile::TempDir, PathBuf) {
    let dir = test_temp::tempdir().expect("create settings dir");
    let path = dir.path().join("settings.json");
    (dir, path)
}

fn write_settings(path: &Path, value: &Value) {
    std::fs::write(
        path,
        serde_json::to_vec_pretty(value).expect("serialize settings fixture"),
    )
    .expect("write settings fixture");
}

fn read_settings(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).expect("read settings fixture"))
        .expect("parse settings fixture")
}

/// A rule in the current `{"hooks": [{"type": "command", "command": ...}]}` shape.
fn user_rule(command: &str) -> Value {
    json!({
        "hooks": [{"type": "command", "command": command}]
    })
}

/// A rule in the legacy flat `{"command": ...}` shape — one of the two shapes
/// `rule_commands` (`src/hooks_manage.rs`) chains together when extracting a
/// rule's command strings.
fn old_format_rule(command: &str) -> Value {
    json!({"command": command})
}

/// Every command string carried by rules under `hook_type`, from either the
/// current nested shape or the legacy flat shape.
fn rule_commands(settings: &Value, hook_type: &str) -> Vec<String> {
    settings
        .get("hooks")
        .and_then(|h| h.get(hook_type))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .iter()
        .flat_map(|rule| {
            let nested = rule
                .get("hooks")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|hook| hook.get("command").and_then(Value::as_str))
                .map(str::to_string);
            let flat = rule
                .get("command")
                .and_then(Value::as_str)
                .map(str::to_string);
            nested.chain(flat)
        })
        .collect()
}

/// Total rule count across every event type, so a headline assertion does not
/// need to know the private `HOOK_TYPES` list.
fn total_rule_count(settings: &Value) -> usize {
    settings
        .get("hooks")
        .and_then(Value::as_object)
        .map(|hooks| {
            hooks
                .values()
                .filter_map(Value::as_array)
                .map(Vec::len)
                .sum()
        })
        .unwrap_or(0)
}

/// Scenario: Install deck hooks three times in a row under the same renamed binary path (`/opt/tools/worker-agent-deck`, never installed as plain `dot-agent-deck`). Each event type must end up with exactly one deck rule, matching the count after a single install — not one appended per install.
#[test]
fn hook_rule_identification_001_repeated_install_renamed_binary_stays_single_rule() {
    let (_dir, path) = settings_path();
    let binary = "/opt/tools/worker-agent-deck";

    install_to(&path, binary).expect("install");
    let after_one = total_rule_count(&read_settings(&path));

    install_to(&path, binary).expect("install");
    install_to(&path, binary).expect("install");
    let after_three = total_rule_count(&read_settings(&path));

    assert_eq!(
        after_three, after_one,
        "repeated install under a renamed binary must not accumulate rules: \
         after 1 install = {after_one}, after 3 installs = {after_three}"
    );

    let pre_tool_use = rule_commands(&read_settings(&path), "PreToolUse");
    assert_eq!(
        pre_tool_use,
        vec![format!("{binary} hook --agent claude-code")],
        "PreToolUse must hold exactly one rule for the renamed binary; got {pre_tool_use:?}"
    );
}

/// Scenario: Install deck hooks under a renamed binary path, then uninstall. No deck rule should remain, but the current substring predicate cannot recognise hooks written under an unfamiliar binary name.
#[test]
fn hook_rule_identification_002_uninstall_removes_rules_written_under_renamed_binary() {
    let (_dir, path) = settings_path();
    let binary = "/opt/tools/worker-agent-deck";

    install_to(&path, binary).expect("install");
    uninstall_from(&path).expect("uninstall");

    let settings = read_settings(&path);
    assert_eq!(
        total_rule_count(&settings),
        0,
        "uninstall must remove every rule written under a renamed binary; settings={settings:?}"
    );
}

/// Scenario: Install deck hooks from two genuinely different binary paths, then reinstall the first. Each distinct binary must keep its own rule throughout — the second install must not wipe the first's, and reinstalling the first must not add a third rule.
#[test]
fn hook_rule_identification_003_distinct_binaries_each_keep_their_own_rule() {
    let (_dir, path) = settings_path();

    install_to(&path, "/a/dot-agent-deck").expect("install");
    install_to(&path, "/b/other-deck-name").expect("install");

    let after_two = rule_commands(&read_settings(&path), "PreToolUse");
    assert_eq!(
        after_two.len(),
        2,
        "two genuinely different deck binaries must each keep their own rule; got {after_two:?}"
    );

    install_to(&path, "/a/dot-agent-deck").expect("install");
    let after_reinstall = rule_commands(&read_settings(&path), "PreToolUse");
    assert_eq!(
        after_reinstall.len(),
        2,
        "re-installing an already-known binary must replace its own rule, not add a third; got {after_reinstall:?}"
    );
}

/// Scenario: A user-authored hook whose command merely mentions dot-agent-deck as an argument (an audit-wrapper watching for it) must never be treated as deck-owned, across both install and uninstall.
#[test]
fn hook_rule_identification_004_user_hook_mentioning_name_is_never_deleted() {
    let (_dir, path) = settings_path();
    let user_command = "/usr/local/bin/audit-wrapper --watch dot-agent-deck";
    write_settings(
        &path,
        &json!({
            "hooks": {
                "PreToolUse": [user_rule(user_command)]
            }
        }),
    );

    install_to(&path, "/opt/tools/worker-agent-deck").expect("install");
    let after_install = rule_commands(&read_settings(&path), "PreToolUse");
    assert!(
        after_install.contains(&user_command.to_string()),
        "a user hook that merely mentions dot-agent-deck must survive install; got {after_install:?}"
    );

    uninstall_from(&path).expect("uninstall");
    let after_uninstall = rule_commands(&read_settings(&path), "PreToolUse");
    assert!(
        after_uninstall.contains(&user_command.to_string()),
        "a user hook that merely mentions dot-agent-deck must survive uninstall; got {after_uninstall:?}"
    );
}

/// Scenario: Unrelated user commands that happen to end in the literal word "hook" — never written by the deck — must never be mistaken for deck rules by install or uninstall. This guards against the specific hazard a naive command-suffix match would introduce.
#[test]
fn hook_rule_identification_005_unrelated_command_ending_in_hook_is_never_deleted() {
    let (_dir, path) = settings_path();
    let unrelated = ["mytool hook", "/usr/bin/git hook"];
    write_settings(
        &path,
        &json!({
            "hooks": {
                "PreToolUse": unrelated.iter().map(|c| user_rule(c)).collect::<Vec<_>>()
            }
        }),
    );

    install_to(&path, "/opt/tools/worker-agent-deck").expect("install");
    let after_install = rule_commands(&read_settings(&path), "PreToolUse");
    for command in unrelated {
        assert!(
            after_install.contains(&command.to_string()),
            "an unrelated command ending in \"hook\" must survive install; got {after_install:?}"
        );
    }

    uninstall_from(&path).expect("uninstall");
    let after_uninstall = rule_commands(&read_settings(&path), "PreToolUse");
    for command in unrelated {
        assert!(
            after_uninstall.contains(&command.to_string()),
            "an unrelated command ending in \"hook\" must survive uninstall; got {after_uninstall:?}"
        );
    }
}

/// Scenario: A hook rule written by an older install under the historical default
/// binary name (`dot-agent-deck`, pre-fix bare `<path> hook` form) must still be
/// recognised and replaced by a fresh install — even when the fresh install's own
/// binary name looks nothing like a deck binary at all. Recognition is keyed off
/// the LEGACY RULE's own executable name, never off the installer's name — a
/// predicate keyed off the installer's name instead cannot be satisfied alongside
/// `_003` (a genuinely different binary must NOT be swept up): the two cases would
/// differ only by a coincidence of which fragment a test's binary path happened to
/// contain, not by anything causal.
#[test]
fn hook_rule_identification_006_legacy_rule_is_recognised_and_replaced() {
    let (_dir, path) = settings_path();
    let legacy_command = "/usr/local/bin/dot-agent-deck hook";
    write_settings(
        &path,
        &json!({
            "hooks": {
                "PreToolUse": [user_rule(legacy_command)]
            }
        }),
    );

    // Deliberately a binary name that does not "look" deck-ish at all — proving
    // migration is driven by the legacy RULE's own name, not by whether the
    // installing binary resembles one.
    install_to(&path, "/usr/local/bin/foo-tool").expect("install");

    let rules = rule_commands(&read_settings(&path), "PreToolUse");
    assert_eq!(
        rules,
        vec!["/usr/local/bin/foo-tool hook --agent claude-code".to_string()],
        "a legacy rule under the historical default binary name must be replaced by \
         the fresh rule regardless of the installing binary's own name; got {rules:?}"
    );
}

/// Scenario: A deck rule written in the legacy flat `{"command": ...}` shape (predating the current `{"hooks": [...]}` wrapper) must still be recognised and removed by uninstall.
#[test]
fn hook_rule_identification_007_old_flat_format_rule_is_recognised() {
    let (_dir, path) = settings_path();
    let legacy_command = "/usr/local/bin/dot-agent-deck hook";
    write_settings(
        &path,
        &json!({
            "hooks": {
                "PreToolUse": [old_format_rule(legacy_command)]
            }
        }),
    );

    uninstall_from(&path).expect("uninstall");

    let remaining = read_settings(&path)["hooks"]["PreToolUse"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        remaining.is_empty(),
        "an old flat-format deck rule must be recognised and removed by uninstall; got {remaining:?}"
    );
}

/// Scenario: A binary installed at a path containing spaces must round-trip
/// through repeated installs without accumulating rules, must be recognised in
/// its quoted form, and must be fully removable by uninstall — mirroring
/// `devin_hooks_manage`'s `install_quotes_a_binary_path_with_spaces` precedent
/// (`src/devin_hooks_manage.rs:726-738`). The expected quoting form is
/// platform-specific — POSIX single quotes on Unix, `cmd.exe` double quotes on
/// Windows, per `shell_quote_if_needed`'s two `#[cfg]` arms
/// (`src/hooks_manage.rs:147-184`) — so this pins the actual mechanism per
/// platform rather than only the fact that quoting happened.
#[test]
fn hook_rule_identification_008_spaced_binary_path_round_trips() {
    let (_dir, path) = settings_path();
    let binary = "/Applications/My Deck/dot-agent-deck";
    #[cfg(unix)]
    let expected_command = "'/Applications/My Deck/dot-agent-deck' hook --agent claude-code";
    #[cfg(windows)]
    let expected_command = "\"/Applications/My Deck/dot-agent-deck\" hook --agent claude-code";

    install_to(&path, binary).expect("install");
    let after_one = rule_commands(&read_settings(&path), "PreToolUse");
    assert_eq!(
        after_one,
        vec![expected_command.to_string()],
        "a spaced binary path must be quoted so the command still parses to the \
         intended argv; got {after_one:?}"
    );

    install_to(&path, binary).expect("install");
    let after_two = rule_commands(&read_settings(&path), "PreToolUse");
    assert_eq!(
        after_two,
        vec![expected_command.to_string()],
        "a spaced binary path must not accumulate rules across repeated installs; \
         got {after_two:?}"
    );

    uninstall_from(&path).expect("uninstall");
    let remaining = total_rule_count(&read_settings(&path));
    assert_eq!(
        remaining, 0,
        "a spaced binary path's rules must be fully removable by uninstall; {remaining} remained"
    );
}

/// Scenario: A rule written by a HISTORICAL install under a spaced binary path,
/// in the pre-fix unquoted flat `<path> hook` form (no shell quoting, no
/// `--agent` suffix — the shape every rule had before this change), must still
/// be recognised and removed by uninstall. The path's own internal whitespace
/// means the executable cannot be recovered by counting whitespace-split tokens;
/// it must be recovered by parsing the command from the RIGHT (`strip_suffix`).
#[test]
fn hook_rule_identification_009_historical_unquoted_spaced_rule_is_still_recognised() {
    let (_dir, path) = settings_path();
    let legacy_command = "/Applications/My Deck/dot-agent-deck hook";
    write_settings(
        &path,
        &json!({
            "hooks": {
                "PreToolUse": [user_rule(legacy_command)]
            }
        }),
    );

    uninstall_from(&path).expect("uninstall");

    let remaining = rule_commands(&read_settings(&path), "PreToolUse");
    assert!(
        remaining.is_empty(),
        "a historical unquoted spaced-path rule must still be recognised and removed \
         by uninstall; got {remaining:?}"
    );
}

/// Scenario: `~/.local/bin/dot-agent-deck` symlinked to a real, differently-named
/// `~/.local/bin/worker-agent-deck` binary — the exact real-machine case a renamed
/// deck binary produces. Installing via each path in turn must resolve to the SAME
/// deployment and leave exactly one rule. Every fixture elsewhere in this file
/// is a fictional path, so `canonicalize` never succeeds against it and this
/// symlink-resolution branch has never actually executed under this suite
/// before now.
#[cfg(unix)]
#[test]
fn hook_rule_identification_010_symlinked_binary_collapses_to_one_rule() {
    let binary_dir = test_temp::tempdir().expect("create binary tempdir");
    let real_binary = binary_dir.path().join("worker-agent-deck");
    std::fs::write(&real_binary, b"#!/bin/sh\n").expect("write real binary");
    let symlink_path = binary_dir.path().join("dot-agent-deck");
    std::os::unix::fs::symlink(&real_binary, &symlink_path).expect("create symlink");

    let (_dir, path) = settings_path();
    install_to(&path, symlink_path.to_str().expect("symlink path is utf8")).expect("install");
    install_to(
        &path,
        real_binary.to_str().expect("real binary path is utf8"),
    )
    .expect("install");

    let pre_tool_use = rule_commands(&read_settings(&path), "PreToolUse");
    assert_eq!(
        pre_tool_use.len(),
        1,
        "a symlink and the real file it resolves to are the SAME deployment and \
         must collapse to one rule, not two; got {pre_tool_use:?}"
    );
}

/// Scenario: Two genuinely different on-disk binaries that happen to share the
/// literal basename `dot-agent-deck` (e.g. two separate local builds at
/// different paths) must be treated as distinct deployments — installing the
/// second must not collapse onto the first's rule, and reinstalling either must
/// not wipe the other's. Unlike `_003`'s fictional paths (where
/// `canonicalize` always fails), both paths here are real files, so this
/// exercises the canonicalize-success branch `_003` cannot reach.
#[test]
fn hook_rule_identification_011_distinct_builds_sharing_basename_do_not_collapse() {
    let build_a_dir = test_temp::tempdir().expect("build a tempdir");
    let build_a = build_a_dir.path().join("dot-agent-deck");
    std::fs::write(&build_a, b"#!/bin/sh\n").expect("write build a");

    let build_b_dir = test_temp::tempdir().expect("build b tempdir");
    let build_b = build_b_dir.path().join("dot-agent-deck");
    std::fs::write(&build_b, b"#!/bin/sh\n").expect("write build b");

    let (_dir, path) = settings_path();
    install_to(&path, build_a.to_str().expect("build a path is utf8")).expect("install");
    install_to(&path, build_b.to_str().expect("build b path is utf8")).expect("install");

    let after_two = rule_commands(&read_settings(&path), "PreToolUse");
    assert_eq!(
        after_two.len(),
        2,
        "two distinct on-disk builds sharing a basename must each keep their own \
         rule; got {after_two:?}"
    );

    install_to(&path, build_a.to_str().expect("build a path is utf8")).expect("install");
    let after_reinstall = rule_commands(&read_settings(&path), "PreToolUse");
    assert_eq!(
        after_reinstall.len(),
        2,
        "reinstalling one build must refresh its own rule, not wipe the other's; \
         got {after_reinstall:?}"
    );
}

/// Scenario: A user's own tool whose basename merely CONTAINS the substring
/// "deck" (looser than the exact historical binary name `dot-agent-deck`)
/// writes its own legacy-shaped `<path> hook` rule. Neither install nor
/// uninstall may ever treat it as deck-owned. This is a mutation guard: a
/// fragment-based predicate (matching any basename containing `"deck"`, or
/// containing `"agent-deck"`) passes every other test in this file, so this
/// case exists specifically to fail if identification is ever loosened to a
/// substring/fragment match instead of the exact compiled binary name.
#[test]
fn hook_rule_identification_012_fragment_match_mutation_guard() {
    let (_dir, path) = settings_path();
    let unrelated_command = "/usr/local/bin/my-deck-tool hook";
    write_settings(
        &path,
        &json!({
            "hooks": {
                "PreToolUse": [user_rule(unrelated_command)]
            }
        }),
    );

    install_to(&path, "/opt/tools/worker-agent-deck").expect("install");
    let after_install = rule_commands(&read_settings(&path), "PreToolUse");
    assert!(
        after_install.contains(&unrelated_command.to_string()),
        "a user tool whose basename merely contains \"deck\" must survive install \
         unless it is EXACTLY the historical default binary name; got {after_install:?}"
    );

    uninstall_from(&path).expect("uninstall");
    let after_uninstall = rule_commands(&read_settings(&path), "PreToolUse");
    assert!(
        after_uninstall.contains(&unrelated_command.to_string()),
        "a user tool whose basename merely contains \"deck\" must survive uninstall; \
         got {after_uninstall:?}"
    );
}

/// Scenario: Two deck-owned rules exist, written by two distinct on-disk
/// binaries sharing a basename (mirroring `_011`), alongside a coexisting
/// non-deck user hook whose command names a path that never existed. One
/// binary's file is then deleted from disk and install runs again via the
/// surviving binary. The now-dead binary's rule must be pruned, the surviving
/// binary's rule must remain, and the never-deck-owned user hook — whose
/// command also names a nonexistent path — must be left untouched throughout:
/// the prune applies only to rules already identified as deck-owned, never a
/// general "delete anything pointing at a missing file" sweep, which would
/// delete user hooks for tools not currently installed.
#[test]
fn hook_rule_identification_014_dead_binary_rule_is_pruned_on_install() {
    let build_a_dir = test_temp::tempdir().expect("build a tempdir");
    let build_a = build_a_dir.path().join("dot-agent-deck");
    std::fs::write(&build_a, b"#!/bin/sh\n").expect("write build a");
    let build_a_str = build_a.to_str().expect("build a path is utf8").to_string();

    let build_b_dir = test_temp::tempdir().expect("build b tempdir");
    let build_b = build_b_dir.path().join("dot-agent-deck");
    std::fs::write(&build_b, b"#!/bin/sh\n").expect("write build b");
    let build_b_str = build_b.to_str().expect("build b path is utf8").to_string();

    let (_dir, path) = settings_path();

    let user_command = "/usr/local/bin/nonexistent-tool --watch";
    write_settings(
        &path,
        &json!({
            "hooks": {
                "PreToolUse": [user_rule(user_command)]
            }
        }),
    );

    install_to(&path, &build_a_str).expect("install");
    // Total right after only build_a is installed: one deck rule per event
    // type plus the untouched user hook. Captured here (rather than hardcoding
    // the private HOOK_TYPES length) so the final assertion below can check
    // "pruning build_a's rules and keeping build_b's leaves the SAME total" —
    // an invariant that holds regardless of how many event types there are.
    let after_one_total = total_rule_count(&read_settings(&path));

    install_to(&path, &build_b_str).expect("install");

    let after_two = rule_commands(&read_settings(&path), "PreToolUse");
    assert_eq!(
        after_two.len(),
        3,
        "two distinct on-disk builds plus the coexisting user hook must all be \
         present before any file is deleted; got {after_two:?}"
    );

    std::fs::remove_file(&build_a).expect("delete build a from disk");

    install_to(&path, &build_b_str).expect("install");

    let settings = read_settings(&path);
    let pre_tool_use = rule_commands(&settings, "PreToolUse");
    let build_a_command = format!("{build_a_str} hook --agent claude-code");
    let build_b_command = format!("{build_b_str} hook --agent claude-code");

    assert!(
        !pre_tool_use.contains(&build_a_command),
        "a deck-owned rule whose executable no longer exists on disk must be \
         pruned on install; got {pre_tool_use:?}"
    );
    assert!(
        pre_tool_use.contains(&build_b_command),
        "a deck-owned rule whose executable still exists on disk must be kept; \
         got {pre_tool_use:?}"
    );
    assert!(
        pre_tool_use.contains(&user_command.to_string()),
        "a non-deck user hook whose command names a nonexistent path must \
         survive the prune — it was never deck-owned; got {pre_tool_use:?}"
    );
    assert_eq!(
        pre_tool_use.len(),
        2,
        "exactly one surviving deck rule plus the untouched user hook must \
         remain under PreToolUse; got {pre_tool_use:?}"
    );

    assert_eq!(
        total_rule_count(&settings),
        after_one_total,
        "pruning the dead binary's rule across every event type must leave \
         the same total rule count as when only one binary's rules existed \
         (one deck rule per event type plus the untouched user hook) — just \
         now owned by the surviving binary instead of the deleted one"
    );
}

/// Scenario: A binary path containing `%` or `!` — both `cmd.exe`-special for
/// variable expansion — must be double-quoted on Windows even though neither
/// character trips the POSIX safe-set check and `~` (also present here, but
/// NOT `cmd.exe`-special) does not force quoting on its own. Closes a gap:
/// nothing exercised this arm of `shell_quote_if_needed`'s Windows safe set
/// before now, and `cargo clippy` on a Unix box cannot even compile the
/// `#[cfg(windows)]` arm — CI's `build-windows` job is the only thing that
/// sees it.
#[cfg(windows)]
#[test]
fn hook_rule_identification_015_windows_percent_and_bang_are_quoted() {
    let (_dir, path) = settings_path();
    let binary = r"C:\Tools\RUNNER~1\100%!\dot-agent-deck.exe";
    let expected_command = format!("\"{binary}\" hook --agent claude-code");

    install_to(&path, binary).expect("install");
    let after_install = rule_commands(&read_settings(&path), "PreToolUse");
    assert_eq!(
        after_install,
        vec![expected_command.clone()],
        "a path containing '%' or '!' must be double-quoted on Windows because both \
         fall outside the safe set, even though '~' alone would not force quoting \
         — the quoting itself does not neutralise cmd.exe's expansion of '%VAR%' \
         or (under delayed expansion) '!VAR!', which the quotes do not prevent; \
         got {after_install:?}"
    );

    uninstall_from(&path).expect("uninstall");
    let remaining = total_rule_count(&read_settings(&path));
    assert_eq!(
        remaining, 0,
        "a Windows-quoted path containing '%'/'!' must still be fully removable \
         by uninstall; {remaining} remained"
    );
}

/// A hook rule written by a pre-fix Windows install, in the bare `<path> hook`
/// legacy shape carrying the platform's real `.exe` suffix
/// (`C:\Program Files\deck\dot-agent-deck.exe hook`), must still be recognised
/// as a legacy deck rule on Windows. `is_legacy_deck_rule`
/// (`src/hooks_manage.rs:417`) has to compare a rule's basename against the
/// literal `DEFAULT_BINARY_NAME` (`"dot-agent-deck"`, no `.exe`) in an
/// extension-aware way, since a Windows basename always carries the
/// extension and would otherwise never match. Without that, a fresh install
/// would append a second rule beside the unrecognised legacy one instead of
/// replacing it (the hook firing twice), and uninstall would never be able
/// to remove either — the same duplicate-rule / unremovable-rule symptom
/// this fix addresses generally, reintroduced specifically on the platform
/// it was meant to make safe.
///
/// Scenario: Seed a legacy Windows rule whose command is a real `.exe` path in
/// the historical bare `hook` form, install a fresh binary, and assert the
/// legacy rule is replaced (not duplicated) under `PreToolUse`; separately,
/// seed the same legacy rule and assert `uninstall_from` removes it entirely.
#[cfg(windows)]
#[test]
fn hook_rule_identification_016_windows_legacy_exe_rule_is_recognised_and_removed() {
    let legacy_command = r"C:\Program Files\deck\dot-agent-deck.exe hook";

    // Part 1: install must recognise and replace the legacy rule, not
    // duplicate a fresh one beside it.
    let (_dir, path) = settings_path();
    write_settings(
        &path,
        &json!({
            "hooks": {
                "PreToolUse": [user_rule(legacy_command)]
            }
        }),
    );

    install_to(&path, r"C:\Tools\worker-agent-deck.exe").expect("install");

    let pre_tool_use = rule_commands(&read_settings(&path), "PreToolUse");
    assert_eq!(
        pre_tool_use,
        vec![r"C:\Tools\worker-agent-deck.exe hook --agent claude-code".to_string()],
        "a pre-fix Windows legacy rule (with its real .exe suffix) must be \
         recognised and replaced by the fresh install, not left duplicated \
         beside a second rule; got {pre_tool_use:?}"
    );

    // Part 2: uninstall must remove a legacy .exe rule entirely.
    let (_dir2, path2) = settings_path();
    write_settings(
        &path2,
        &json!({
            "hooks": {
                "PreToolUse": [user_rule(legacy_command)]
            }
        }),
    );

    uninstall_from(&path2).expect("uninstall");

    let remaining = total_rule_count(&read_settings(&path2));
    assert_eq!(
        remaining, 0,
        "a pre-fix Windows legacy .exe rule must be fully removable by \
         uninstall; {remaining} remained"
    );
}

/// Scenario: A `settings.json` made invalid by a single trailing comma — while
/// still carrying the user's `model`, `env`, and `permissions` configuration —
/// must never be silently replaced. Audit's most serious finding: `read_settings`
/// maps ANY parse error to `{}`, so on the real machine this would silently
/// destroy 10 non-`hooks` keys including 123 `permissions.allow` entries and
/// leave only the deck's own hooks behind. This pins that `install_to` leaves a
/// file it cannot parse exactly as it found it, mirroring
/// `codex_hooks_manage::install_to`'s `ErrorKind::NotFound`-only-means-empty
/// contract (`src/codex_hooks_manage.rs:290-316`).
#[test]
fn hook_rule_identification_013_malformed_settings_json_is_never_clobbered() {
    let (_dir, path) = settings_path();
    let malformed = "{\n  \"model\": \"opus\",\n  \"env\": {\"FOO\": \"bar\"},\n  \
                      \"permissions\": {\"allow\": [\"Bash(git *)\"]},\n  \"hooks\": {},\n}\n";
    std::fs::write(&path, malformed).expect("write malformed settings fixture");

    let err = install_to(&path, "/opt/tools/worker-agent-deck")
        .expect_err("install must refuse a settings.json it cannot parse");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData, "got {err}");

    let after = std::fs::read_to_string(&path).expect("read settings after install attempt");
    assert_eq!(
        after, malformed,
        "a malformed settings.json must never be silently replaced by install — the \
         user's model/env/permissions would be destroyed and only deck hooks would \
         remain"
    );
}

/// A `settings.json` invalidated by exactly one trailing comma, carrying the
/// same real configuration as #516's measured reproduction: `model`, `env`, and
/// both halves of `permissions`.
fn malformed_settings_fixture() -> &'static str {
    "{\n  \"model\": \"opus\",\n  \"env\": {\"ANTHROPIC_LOG\": \"debug\"},\n  \
     \"permissions\": {\"allow\": [\"Bash(git status)\"], \"deny\": [\"Bash(rm -rf *)\", \
     \"Read(./.env)\"]},\n  \"hooks\": {},\n}\n"
}

/// Scenario: A `settings.json` made invalid by a single trailing comma — while
/// still carrying the user's `model`, `env` and `permissions` — is uninstalled
/// from. The file must be left byte-for-byte as it was found, with the user's
/// bytes additionally preserved at `settings.json.bak`. #506 fixed this for
/// install and deliberately left uninstall on the lenient reader, so uninstall
/// still mapped the unparseable file to `{}` and wrote that over it —
/// destroying `model`, `env` and every `permissions` entry (`deny` included, so
/// the user's own `Bash(rm -rf *)` and `Read(./.env)` guards go with it) while
/// reporting success and exiting 0. This is #522: the remainder of #516 on the
/// path #516's fix does not cover.
#[test]
fn hook_rule_identification_017_malformed_settings_json_is_never_clobbered_by_uninstall() {
    let (_dir, path) = settings_path();
    let malformed = malformed_settings_fixture();
    std::fs::write(&path, malformed).expect("write malformed settings fixture");

    let err =
        uninstall_from(&path).expect_err("uninstall must refuse a settings.json it cannot parse");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData, "got {err}");

    let after = std::fs::read_to_string(&path).expect("read settings after uninstall attempt");
    assert_eq!(
        after, malformed,
        "a malformed settings.json must never be truncated to {{}} by uninstall — the \
         user's model/env/permissions (including permissions.deny) would be destroyed"
    );

    // The refusal must also reach the shell, not just the file: the CLI exits
    // non-zero on this `Err` (`main.rs`'s `HooksAction::Uninstall` arm), where
    // `agent_registry::claude_uninstall` used to hardcode `Ok(())`.
    let backup = std::fs::read_to_string(path.with_extension("json.bak"))
        .expect("uninstall must preserve the user's bytes at settings.json.bak");
    assert_eq!(
        backup, malformed,
        "the backup must be a byte-for-byte copy of what the user had"
    );
}

/// Scenario: A perfectly VALID `settings.json` carrying `model`, `env`,
/// `permissions.allow` and `permissions.deny` alongside the deck's hooks goes
/// through a full install and then a full uninstall. Every one of those keys
/// must still be present and unchanged at both points. `hooks/install/001`
/// explicitly does not assert this ("Does not assert: other unrelated keys in
/// `settings.json`"), so the preservation property was unpinned on both paths.
///
/// This is the CONTROL for `_017`: it is the nearest thing that must keep
/// working, and it passes both before and after that fix. Its job is to show
/// that `_017`'s red is attributable to the file being *unparseable*, not to
/// install/uninstall dropping unrelated keys in general.
#[test]
fn hook_rule_identification_018_valid_settings_keep_model_env_and_permissions() {
    let (_dir, path) = settings_path();
    let user_keys = json!({
        "model": "opus",
        "env": {"ANTHROPIC_LOG": "debug"},
        "permissions": {
            "allow": ["Bash(git status)", "Bash(git diff)"],
            "deny": ["Bash(rm -rf *)", "Read(./.env)"]
        }
    });
    let mut fixture = user_keys.clone();
    fixture["hooks"] = json!({});
    write_settings(&path, &fixture);

    install_to(&path, "/opt/tools/worker-agent-deck").expect("install");
    let after_install = read_settings(&path);
    for key in ["model", "env", "permissions"] {
        assert_eq!(
            after_install.get(key),
            user_keys.get(key),
            "install must leave the user's {key} untouched; got {after_install:?}"
        );
    }

    uninstall_from(&path).expect("uninstall");
    let after_uninstall = read_settings(&path);
    for key in ["model", "env", "permissions"] {
        assert_eq!(
            after_uninstall.get(key),
            user_keys.get(key),
            "uninstall must leave the user's {key} untouched; got {after_uninstall:?}"
        );
    }
}

/// The fixture from #535's executed reproduction: the user's own audit hook and
/// the deck's, co-located as two commands inside ONE rule object under a shared
/// `"matcher": "Bash"`.
fn co_located_rule(deck_command: &str) -> Value {
    json!({
        "matcher": "Bash",
        "hooks": [
            {"type": "command", "command": USER_AUDIT_COMMAND},
            {"type": "command", "command": deck_command}
        ]
    })
}

const USER_AUDIT_COMMAND: &str = "/usr/local/bin/my-critical-audit.sh";

/// Scenario: The user put their own `my-critical-audit.sh` and the deck's hook
/// command in the SAME rule object — a normal thing to do, since one rule's
/// `hooks` array is a list of commands sharing a matcher — and then runs
/// `hooks uninstall`. Only the deck's command may be removed; the user's must
/// still be there, still under its `"matcher": "Bash"`.
///
/// This is #535, and it is a granularity defect rather than a matching one:
/// the predicate was an `any()` over every command in a rule and `retain` then
/// dropped the entire rule `Value`, so it made no difference which rule the
/// matcher identified — `strip_deck_commands` replaces both halves. The
/// reproduction in the issue ends `--> my-critical-audit.sh still present?
/// False`, with nothing said about it.
#[test]
fn hook_rule_identification_019_co_located_user_command_survives_uninstall() {
    let (_dir, path) = settings_path();
    let deck_command = "/usr/local/bin/dot-agent-deck hook --agent claude-code";
    write_settings(
        &path,
        &json!({
            "hooks": {
                "PreToolUse": [co_located_rule(deck_command)]
            }
        }),
    );

    uninstall_from(&path).expect("uninstall");

    let settings = read_settings(&path);
    let commands = rule_commands(&settings, "PreToolUse");
    assert!(
        commands.contains(&USER_AUDIT_COMMAND.to_string()),
        "the user's own hook co-located in the deck's rule object must survive \
         uninstall — only the deck's command may be removed; got {commands:?}"
    );
    assert!(
        !commands.contains(&deck_command.to_string()),
        "uninstall must still remove the deck's own command from the shared rule; \
         got {commands:?}"
    );
    let matchers: Vec<_> = settings["hooks"]["PreToolUse"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|rule| {
            rule.get("matcher")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    assert!(
        matchers.contains(&"Bash".to_string()),
        "the surviving rule must keep the matcher the user's hook was registered \
         under, not be rebuilt without it; got {matchers:?}"
    );
}

/// Scenario: The same co-located rule as `_019`, but the deck INSTALLS over it
/// instead of uninstalling — the path that matters more in practice, since
/// `auto_install` runs unattended at every dashboard startup. Install
/// normalizes the deck's own rules down to a single fresh rule per event type,
/// and it did so with the same whole-rule `retain`, so a user who moved the
/// deck's command into their own rule object lost their hook the next time they
/// merely *opened* the deck.
///
/// The co-located command here belongs to the binary that is installing —
/// that is what makes install claim the rule at all. `_003` pins the
/// complementary case (a rule belonging to a genuinely different deck binary is
/// not touched), which is why `_019`'s fixture does not reproduce this on the
/// install path.
#[test]
fn hook_rule_identification_020_co_located_user_command_survives_install() {
    let (_dir, path) = settings_path();
    let binary = "/opt/tools/worker-agent-deck";
    let deck_command = format!("{binary} hook --agent claude-code");
    write_settings(
        &path,
        &json!({
            "hooks": {
                "PreToolUse": [co_located_rule(&deck_command)]
            }
        }),
    );

    install_to(&path, binary).expect("install");

    let commands = rule_commands(&read_settings(&path), "PreToolUse");
    assert!(
        commands.contains(&USER_AUDIT_COMMAND.to_string()),
        "the user's own hook co-located in the deck's rule object must survive \
         install — auto-install runs at every startup; got {commands:?}"
    );
    assert_eq!(
        commands.iter().filter(|c| *c == &deck_command).count(),
        1,
        "the installing binary must end up with exactly one rule of its own, not \
         zero and not a duplicate beside the co-located one; got {commands:?}"
    );

    // Installing twice must converge, not accumulate: the second run sees the
    // user's rule with the deck's command already stripped out of it.
    install_to(&path, binary).expect("install");
    let after_two = rule_commands(&read_settings(&path), "PreToolUse");
    assert_eq!(
        after_two, commands,
        "a second install over a stripped co-located rule must be a no-op; got {after_two:?}"
    );
}

/// Scenario: A hard link is made to `settings.json` before the deck installs,
/// standing in for a concurrent reader (Claude Code itself) holding the file
/// the deck is about to rewrite. After the install, the hard link must still
/// hold the ORIGINAL bytes — proving the deck published by writing a temp file
/// and `rename`-ing it over the destination, rather than truncating the
/// original file object in place.
///
/// This is #534's mechanism, observed deterministically. `std::fs::write` opens
/// with `O_TRUNC` and writes: for a window between those two syscalls the file
/// every other process is looking at is zero bytes long, which is both the
/// torn read Claude Code can hit and the partial file that a crash mid-write
/// leaves behind — i.e. exactly the malformed `settings.json` that `_013` and
/// `_017` exist to refuse. A `rename(2)` publish has no such window: the
/// original inode is never modified, so the witness link still reads clean.
#[cfg(unix)]
#[test]
fn hook_rule_identification_021_settings_are_published_by_rename_not_truncated_in_place() {
    let (dir, path) = settings_path();
    let original = "{\n  \"model\": \"opus\",\n  \"hooks\": {}\n}\n";
    std::fs::write(&path, original).expect("write settings fixture");

    let witness = dir.path().join("witness.json");
    std::fs::hard_link(&path, &witness).expect("hard link the settings file");

    install_to(&path, "/opt/tools/worker-agent-deck").expect("install");

    let witnessed = std::fs::read_to_string(&witness).expect("read witness after install");
    assert_eq!(
        witnessed, original,
        "the file object a concurrent reader is holding must never be truncated or \
         rewritten in place — the deck must publish a new file with rename(2), \
         leaving the original intact until the instant it is replaced"
    );

    // The destination itself must, of course, have been updated.
    let commands = rule_commands(&read_settings(&path), "PreToolUse");
    assert_eq!(
        commands,
        vec!["/opt/tools/worker-agent-deck hook --agent claude-code".to_string()],
        "the destination path must carry the freshly installed rule; got {commands:?}"
    );
}

/// Scenario: Four threads install four distinct deck binaries concurrently over
/// one `settings.json` while a fifth reads it in a tight loop — two deck
/// processes and a human editor, compressed into one process. Every single read
/// must parse as JSON, and when the storm is over all four binaries' rules must
/// be there.
///
/// The read-modify-write had no serialization of any kind, and the two
/// assertions pin the two halves that fixes it. The reader catches the TORN
/// READ, which the temp-file+`rename` publish closes: `std::fs::write`'s
/// `O_TRUNC` leaves the file zero bytes long until the following `write`
/// lands. The four-rule assertion catches the LOST UPDATE, which only the
/// mutex closes: an atomic publish still lets two callers read the same
/// "before" state and have the second overwrite the first's rule with a stale
/// copy, and every install here preserves the other three binaries' rules
/// (`_003`, `_011`), so a missing rule at the end means exactly one such
/// interleaving. `codex_hooks_manage` closed both with a
/// `static INSTALL_LOCK: Mutex<()>` plus an atomic publish (its findings
/// #1/M-2); this pins the same pair for Claude's adapter.
///
/// Deterministic in the green direction, probabilistic in the red: with both in
/// place neither window exists, so this cannot flake to failure; without them
/// the reader catches the `O_TRUNC` gap well within a hundred writes and the
/// writers lose updates at the same rate.
#[test]
fn hook_rule_identification_022_concurrent_installs_never_tear_or_lose_updates() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let (_dir, path) = settings_path();
    write_settings(&path, &json!({"model": "opus", "hooks": {}}));

    // Distinct basenames, so no writer's install treats another's rule as a
    // stale sibling of its own and prunes it (`_014`'s dead-binary sweep is
    // keyed on the installing binary's basename).
    let binary = |i: usize| format!("/opt/build{i}/worker-agent-deck-{i}");

    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let path = path.clone();
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut torn = 0usize;
            let mut reads = 0usize;
            while !stop.load(Ordering::Relaxed) {
                reads += 1;
                match std::fs::read(&path) {
                    Ok(bytes) if serde_json::from_slice::<Value>(&bytes).is_ok() => {}
                    _ => torn += 1,
                }
                std::thread::yield_now();
            }
            (torn, reads)
        })
    };

    let writers: Vec<_> = (0..4)
        .map(|i| {
            let path = path.clone();
            std::thread::spawn(move || {
                for _ in 0..25 {
                    install_to(&path, &binary(i)).expect("install");
                }
            })
        })
        .collect();
    for writer in writers {
        writer.join().expect("writer thread panicked");
    }
    stop.store(true, Ordering::Relaxed);
    let (torn, reads) = reader.join().expect("reader thread panicked");

    assert_eq!(
        torn, 0,
        "a concurrent reader must never observe a truncated or half-written \
         settings.json; {torn} of {reads} reads did not parse as JSON"
    );

    let settings = read_settings(&path);
    let commands = rule_commands(&settings, "PreToolUse");
    for i in 0..4 {
        let expected = format!("{} hook --agent claude-code", binary(i));
        assert!(
            commands.contains(&expected),
            "every concurrent installer's rule must survive — a missing one is a \
             lost update, where two callers read the same state and the second \
             wrote its own rule over the first's; got {commands:?}"
        );
    }
    assert_eq!(
        settings.get("model"),
        Some(&json!("opus")),
        "the user's model key must survive concurrent installs; got {settings:?}"
    );
}

/// Scenario: A `settings.json` the user has deliberately kept private (mode
/// 0600) is installed into. Its mode must be exactly 0600 afterwards — the
/// deck must not widen a file it does not own.
///
/// This is the guard for the specific hazard a temp-file+rename publish
/// introduces and an in-place `std::fs::write` does not have: `File::create`
/// applies `0666 & !umask` — 0644 under a typical 022 umask, 0664 under 002 —
/// and the `rename` then replaces the destination with that wider file. #360
/// fixed it in the Devin adapter, where a real install ships the config at
/// 0600, and #382 fixed it in the Codex one and moved both onto a shared
/// helper; this pins it for Claude's adapter at the moment the publish switches
/// to a rename, so the fix for #534 cannot reintroduce that bug here.
#[cfg(unix)]
#[test]
fn hook_rule_identification_023_install_never_widens_settings_permissions() {
    use std::os::unix::fs::PermissionsExt as _;

    let (_dir, path) = settings_path();
    write_settings(&path, &json!({"model": "opus", "hooks": {}}));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .expect("restrict settings fixture to 0600");

    install_to(&path, "/opt/tools/worker-agent-deck").expect("install");

    let mode = std::fs::metadata(&path)
        .expect("stat settings after install")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode, 0o600,
        "install must preserve the destination's own mode; settings.json went from \
         0600 to {mode:o}, exposing the user's env/permissions to every local account"
    );
}

/// Scenario: `settings.json` is a symlink into a dotfiles checkout — a normal
/// `stow`/`chezmoi` arrangement. The deck must refuse rather than guess: the
/// symlink must still be a symlink afterwards and the file it points at must be
/// byte-for-byte unchanged.
///
/// There is no safe silent option here. A `rename(2)` publish onto the link
/// path replaces the symlink with a regular file, orphaning the dotfiles copy
/// (the user's edits and the deck's silently diverge from then on); resolving
/// the link instead means writing through it to a path outside the directory
/// the deck meant to touch, which is the write-anywhere hazard a same-directory
/// publish exists to close. Refusing is the only branch that destroys nothing,
/// so it fails loudly and leaves the arrangement alone.
#[cfg(unix)]
#[test]
fn hook_rule_identification_024_symlinked_settings_are_refused_not_replaced() {
    let (_dir, path) = settings_path();
    let dotfiles = test_temp::tempdir().expect("create dotfiles dir");
    let real = dotfiles.path().join("claude-settings.json");
    let original = "{\n  \"model\": \"opus\",\n  \"hooks\": {}\n}\n";
    std::fs::write(&real, original).expect("write dotfiles settings");
    std::os::unix::fs::symlink(&real, &path).expect("symlink settings.json into dotfiles");

    let err = install_to(&path, "/opt/tools/worker-agent-deck")
        .expect_err("install must refuse a symlinked settings.json");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "got {err}");

    assert!(
        std::fs::symlink_metadata(&path)
            .expect("stat settings path after install")
            .file_type()
            .is_symlink(),
        "a symlinked settings.json must still be a symlink after the deck refuses — \
         replacing it with a regular file orphans the user's dotfiles copy"
    );
    let after = std::fs::read_to_string(&real).expect("read dotfiles settings after install");
    assert_eq!(
        after, original,
        "the file a symlinked settings.json points at must be left byte-for-byte as \
         found — the deck must not write outside the directory it was pointed at"
    );
}
