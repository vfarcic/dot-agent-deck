#![cfg(unix)]

//! Fast safety coverage for Codex hook installation and wrapper trust scoping.

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::process::{Command, Output, Stdio};

use dot_agent_deck::codex_hooks_manage::install_to;
use serde_json::{Value, json};
use spec::spec;

const DECK_BINARY: &str = "/opt/dot-agent-deck/bin/dot-agent-deck";
const TRUST_BYPASS_FLAG: &str = "--dangerously-bypass-hook-trust";

fn hooks_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join("hooks.json")
}

fn write_hooks(home: &std::path::Path, value: &Value) {
    std::fs::write(
        hooks_path(home),
        serde_json::to_vec_pretty(value).expect("serialize hooks fixture"),
    )
    .expect("write hooks fixture");
}

fn read_hooks(home: &std::path::Path) -> Value {
    serde_json::from_slice(&std::fs::read(hooks_path(home)).expect("read hooks fixture"))
        .expect("parse hooks fixture")
}

fn assert_incompatible_config_is_untouched(value: Value) {
    let home = tempfile::tempdir().expect("create Codex home");
    write_hooks(home.path(), &value);
    let original = std::fs::read(hooks_path(home.path())).expect("read original hooks");

    let result = install_to(home.path(), DECK_BINARY);

    assert!(
        result.is_err(),
        "structurally incompatible hooks.json must return an error instead of being replaced"
    );
    assert_eq!(
        std::fs::read(hooks_path(home.path())).expect("read hooks after rejected install"),
        original,
        "structurally incompatible hooks.json must remain byte-for-byte unchanged"
    );
}

fn write_fake_codex(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("codex");
    std::fs::write(
        &path,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$CODEX_ARGS_RECORD\"\n",
    )
    .expect("write fake codex");
    let mut permissions = std::fs::metadata(&path)
        .expect("stat fake codex")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).expect("make fake codex executable");
    path
}

fn run_fake_codex(
    agent: &str,
    codex_home: &std::path::Path,
    fixture_dir: &std::path::Path,
) -> (Output, Vec<String>) {
    let fake_codex = write_fake_codex(fixture_dir);
    let args_record = fixture_dir.join("args.txt");
    let output = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args(["wrap", "--agent", agent, "--"])
        .arg(fake_codex)
        .env("CODEX_HOME", codex_home)
        .env("CODEX_ARGS_RECORD", &args_record)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run wrapper with fake codex");
    let args = std::fs::read_to_string(args_record)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    (output, args)
}

/// Scenario: Install deck hooks into a Codex home containing an unrelated user hook whose command mentions dot-agent-deck as an audit argument. The user rule must remain present while exactly one deck-owned rule is installed.
#[test]
fn codex_hooks_install_001_substring_match_does_not_delete_user_hook() {
    let home = tempfile::tempdir().expect("create Codex home");
    let user_command = "/usr/local/bin/audit-wrapper --watch dot-agent-deck";
    write_hooks(
        home.path(),
        &json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{"type": "command", "command": user_command}]
                }]
            }
        }),
    );

    install_to(home.path(), DECK_BINARY).expect("install deck hooks");

    let rules = read_hooks(home.path())["hooks"]["PreToolUse"]
        .as_array()
        .expect("PreToolUse rules")
        .clone();
    assert!(
        rules
            .iter()
            .any(|rule| rule["hooks"][0]["command"] == user_command),
        "a user hook that merely mentions dot-agent-deck must be preserved; rules={rules:?}"
    );
}

/// Scenario: Attempt installation over malformed hooks.json. Installation must either reject the file without changing it or preserve the original bytes in a backup before writing replacement content.
#[test]
fn codex_hooks_install_002_malformed_json_is_never_discarded() {
    let home = tempfile::tempdir().expect("create Codex home");
    let original = b"{\n  \"hooks\": [this is user data\n";
    std::fs::write(hooks_path(home.path()), original).expect("write malformed hooks fixture");

    let result = install_to(home.path(), DECK_BINARY);
    let current = std::fs::read(hooks_path(home.path())).expect("read hooks after install");
    let backup_preserved = std::fs::read_dir(home.path())
        .expect("list Codex home")
        .filter_map(Result::ok)
        .filter(|entry| entry.path() != hooks_path(home.path()))
        .any(|entry| std::fs::read(entry.path()).is_ok_and(|bytes| bytes == original));

    let safely_rejected = result.is_err() && current == original;
    assert!(
        safely_rejected || backup_preserved,
        "malformed hooks.json was discarded: result={result:?} current={:?}",
        String::from_utf8_lossy(&current)
    );
}

/// Scenario: Attempt installation when hooks.json has a non-object root. Installation must fail and leave the incompatible user file unchanged.
#[test]
fn codex_hooks_install_003_non_object_root_is_untouched() {
    assert_incompatible_config_is_untouched(json!(["user-hook-config"]));
}

/// Scenario: Attempt installation when the hooks field is not an object. Installation must fail and leave the incompatible user file unchanged.
#[test]
fn codex_hooks_install_004_non_object_hooks_field_is_untouched() {
    assert_incompatible_config_is_untouched(json!({"hooks": ["user-hook-config"]}));
}

/// Scenario: Attempt installation when an existing event value is not an array. Installation must fail and leave the incompatible user file unchanged.
#[test]
fn codex_hooks_install_005_non_array_event_is_untouched() {
    assert_incompatible_config_is_untouched(json!({
        "hooks": {"PreToolUse": {"user": "hook-config"}}
    }));
}

/// Scenario: Reinstall deck hooks over an existing valid file. The installer must publish through a same-directory replacement rather than truncating the destination inode in place.
#[test]
fn codex_hooks_install_006_write_is_atomic_replacement() {
    let home = tempfile::tempdir().expect("create Codex home");
    write_hooks(
        home.path(),
        &json!({
            "hooks": {
                "PreToolUse": [{
                    "hooks": [{"type": "command", "command": "/user/own-hook"}]
                }]
            }
        }),
    );
    let before_inode = std::fs::metadata(hooks_path(home.path()))
        .expect("stat hooks before install")
        .ino();

    install_to(home.path(), DECK_BINARY).expect("install deck hooks");

    let after_inode = std::fs::metadata(hooks_path(home.path()))
        .expect("stat hooks after install")
        .ino();
    assert_ne!(
        after_inode, before_inode,
        "hooks.json must be atomically replaced via temp file and rename, not truncated in place"
    );
}

/// Scenario: Wrap a direct executable named codex while explicitly declaring the agent as Claude. The wrapper must neither install Codex hooks nor inject Codex's hook-trust bypass flag.
#[test]
fn codex_hooks_trust_001_direct_codex_requires_codex_identity() {
    let fixture = tempfile::tempdir().expect("create wrapper fixture");
    let home = tempfile::tempdir().expect("create Codex home");

    let (output, args) = run_fake_codex("claude", home.path(), fixture.path());

    assert!(output.status.success(), "wrapper failed: {output:?}");
    assert!(
        !hooks_path(home.path()).exists(),
        "non-Codex identity must not install Codex hooks"
    );
    assert!(
        !args.iter().any(|arg| arg == TRUST_BYPASS_FLAG),
        "non-Codex identity trusted hooks globally: args={args:?}"
    );
}

/// Scenario: Launch a deck-managed Codex with an unrelated third-party hook already present in CODEX_HOME. The wrapper may install its own hook, but must not use an invocation-global flag that also trusts the third-party hook.
#[test]
fn codex_hooks_trust_002_third_party_hooks_are_not_globally_trusted() {
    let fixture = tempfile::tempdir().expect("create wrapper fixture");
    let home = tempfile::tempdir().expect("create Codex home");
    let third_party_command = "/usr/local/bin/untrusted-third-party-hook";
    write_hooks(
        home.path(),
        &json!({
            "hooks": {
                "PreToolUse": [{
                    "hooks": [{"type": "command", "command": third_party_command}]
                }]
            }
        }),
    );

    let (output, args) = run_fake_codex("codex", home.path(), fixture.path());

    assert!(output.status.success(), "wrapper failed: {output:?}");
    let installed = read_hooks(home.path());
    assert!(
        installed["hooks"]["PreToolUse"]
            .as_array()
            .is_some_and(|rules| rules
                .iter()
                .any(|rule| rule["hooks"][0]["command"] == third_party_command)),
        "third-party hook must remain user-controlled: {installed}"
    );
    assert!(
        !args.iter().any(|arg| arg == TRUST_BYPASS_FLAG),
        "invocation-global trust bypass would trust the third-party hook too: args={args:?}"
    );
}

/// Scenario: Launch an executable named codex that redirects CODEX_HOME to an uninspected home containing a foreign hook before invoking Codex. The deck must not let the redirected hook set receive its invocation-global trust bypass.
#[spec("codex/trust/001")]
#[test]
fn codex_trust_001_vetted_home_cannot_be_swapped_under_bypass() {
    let fixture = tempfile::tempdir().expect("create Codex launcher fixture");
    let vetted_home = tempfile::tempdir().expect("create vetted Codex home");
    let swapped_home = tempfile::tempdir().expect("create swapped Codex home");
    write_hooks(
        swapped_home.path(),
        &json!({
            "hooks": {
                "PreToolUse": [{
                    "hooks": [{"type": "command", "command": "/uninspected/foreign-hook"}]
                }]
            }
        }),
    );
    let launcher = fixture.path().join("codex");
    let launch_record = fixture.path().join("launch.txt");
    std::fs::write(
        &launcher,
        "#!/bin/sh\nexport CODEX_HOME=\"$SWAPPED_CODEX_HOME\"\nprintf 'home=%s\\n' \"$CODEX_HOME\" > \"$CODEX_LAUNCH_RECORD\"\nprintf 'arg=%s\\n' \"$@\" >> \"$CODEX_LAUNCH_RECORD\"\n",
    )
    .expect("write CODEX_HOME-swapping launcher");
    let mut permissions = std::fs::metadata(&launcher)
        .expect("stat CODEX_HOME-swapping launcher")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&launcher, permissions)
        .expect("make CODEX_HOME-swapping launcher executable");

    let output = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args(["wrap", "--agent", "codex", "--"])
        .arg(&launcher)
        .env("CODEX_HOME", vetted_home.path())
        .env("SWAPPED_CODEX_HOME", swapped_home.path())
        .env("CODEX_LAUNCH_RECORD", &launch_record)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run CODEX_HOME-swapping launcher");
    let observed = std::fs::read_to_string(&launch_record).expect("read Codex launch record");
    let bypass_reached_swapped_home = observed
        .lines()
        .any(|line| line == format!("arg={TRUST_BYPASS_FLAG}"))
        && observed.contains(&format!("home={}", swapped_home.path().display()));

    assert!(output.status.success(), "wrapper failed: {output:?}");
    assert!(
        !bypass_reached_swapped_home,
        "an uninspected CODEX_HOME must never receive the global trust bypass; launch={observed:?}"
    );
}
