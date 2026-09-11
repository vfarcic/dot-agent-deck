#![cfg(unix)]

//! Fast safety coverage for Codex hook installation and wrapper trust scoping.

// Issue #322. Fast-tier, does NOT link `tests/common/mod.rs`; the ~40-line
// crate-internal resolver is `#[path]`-included instead, at two extra test
// executions rather than the harness's ~530. The 14 scratch dirs here are
// pinned Codex homes and wrapper fixtures — small, but they were landing in the
// OS temp dir, which on this project's dev box is the RAM-backed `/tmp` the
// issue is about. See `docs/develop/e2e-temp-dirs.md`.
#[path = "../src/test_temp.rs"]
mod test_temp;

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::process::{Command, Output, Stdio};

use dot_agent_deck::codex_hooks_manage::{
    CodexHookEntry, DeckCommandMatch, deck_owned_entries, expected_hook_command, install_to,
};
use serde_json::{Value, json};
use spec::spec;

const DECK_BINARY: &str = "/opt/dot-agent-deck/bin/dot-agent-deck";
const TRUST_BYPASS_FLAG: &str = "--dangerously-bypass-hook-trust";
/// The signature every deck-authored Codex hook command ends with. Spelled here
/// so a test can recognise a deck command without reproducing the writer.
const DECK_COMMAND_SUFFIX: &str = "hook --agent codex";

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
    let home = test_temp::tempdir().expect("create Codex home");
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
        "#!/bin/sh\nif [ \"${1:-}\" = app-server ]; then\n    IFS= read -r _initialize\n    printf '%s\\n' '{\"id\":1,\"result\":{\"userAgent\":\"trust-test\",\"codexHome\":\"test\",\"platformFamily\":\"unix\",\"platformOs\":\"linux\"}}'\n    IFS= read -r _list\n    printf '%s\\n' \"$CODEX_HOOK_LIST_RESPONSE\" | sed \"s|__CODEX_HOME__|$CODEX_HOME|g\"\n    exit 0\nfi\nprintf '%s\\n' \"$@\" > \"$CODEX_ARGS_RECORD\"\nprintf '%s\\n' \"$CODEX_HOME\" > \"$CODEX_HOME_RECORD\"\n",
    )
    .expect("write fake codex");
    let mut permissions = std::fs::metadata(&path)
        .expect("stat fake codex")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).expect("make fake codex executable");
    path
}

/// Seed a durable deck at `<home>/.local/bin/dot-agent-deck` and return its path.
///
/// PRD #381's resolver refuses to pin the `target/{debug,release}` binary it is
/// running from into another program's persistent config, so a test that drives
/// the real `hooks install` CLI has to hand it a durable candidate of its own.
/// Seeding one inside the fixture — instead of letting the resolver walk out to
/// whatever the developer has at `~/.local/bin/dot-agent-deck` — is what makes
/// the outcome the fixture's rather than the host's. That leak is why PR #733
/// passed locally and failed on all three CI runners at once.
///
/// The candidate is never executed, only stat'd, so a two-line script is
/// enough; the exec bit and the owner-only write mode are the parts the
/// resolver actually reads.
fn seed_durable_binary(home: &std::path::Path) -> std::path::PathBuf {
    let bin_dir = home.join(".local").join("bin");
    std::fs::create_dir_all(&bin_dir).expect("create durable bin dir");
    let durable = bin_dir.join(format!(
        "{}{}",
        dot_agent_deck::platform::paths::DEFAULT_BINARY_NAME,
        std::env::consts::EXE_SUFFIX
    ));
    std::fs::write(&durable, "#!/bin/sh\nexit 0\n").expect("write durable deck");
    let mut permissions = std::fs::metadata(&durable)
        .expect("stat durable deck")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&durable, permissions).expect("make durable deck executable");
    durable
}

/// Seed an executable regular file at `path`, creating its parents.
///
/// Stands for "a second, perfectly valid deck install": absolute, present,
/// executable and outside any `target/{debug,release}` directory, which is the
/// whole of what `platform::paths::pin_is_repairable` asks before deciding a pin
/// must be left alone. Never executed — only stat'd.
fn seed_executable(path: &std::path::Path) -> String {
    std::fs::create_dir_all(path.parent().expect("seeded binary has a parent"))
        .expect("create seeded binary dir");
    std::fs::write(path, "#!/bin/sh\nexit 0\n").expect("write seeded binary");
    let mut permissions = std::fs::metadata(path)
        .expect("stat seeded binary")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).expect("make seeded binary executable");
    path.to_str()
        .expect("seeded binary path is UTF-8")
        .to_string()
}

/// Every deck-owned command under `event`, in file order.
fn deck_commands_for(home: &std::path::Path, event: &str) -> Vec<String> {
    read_hooks(home)["hooks"][event]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .flat_map(|rule| {
            rule["hooks"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
        })
        .filter_map(|handler| handler["command"].as_str().map(str::to_string))
        .filter(|command| command.ends_with(DECK_COMMAND_SUFFIX))
        .collect()
}

fn write_fake_program(path: &std::path::Path) {
    std::fs::copy(path.parent().expect("program parent").join("codex"), path)
        .expect("copy fake program");
    let mut permissions = std::fs::metadata(path)
        .expect("stat fake program")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).expect("make fake program executable");
}

fn hook_entry(
    key: &str,
    command: &str,
    source_path: &str,
    current_hash: &str,
    is_managed: bool,
) -> Value {
    json!({
        "key": key,
        "eventName": "preToolUse",
        "handlerType": "command",
        "matcher": null,
        "command": command,
        "timeoutSec": 600,
        "statusMessage": null,
        "sourcePath": source_path,
        "source": "user",
        "pluginId": null,
        "displayOrder": 0,
        "enabled": true,
        "isManaged": is_managed,
        "currentHash": current_hash,
        "trustStatus": "untrusted"
    })
}

fn hook_list_response(entries: Vec<Value>) -> String {
    json!({
        "id": 2,
        "result": {
            "data": [{
                "cwd": "/workspace",
                "hooks": entries,
                "warnings": [],
                "errors": []
            }]
        }
    })
    .to_string()
}

/// Run the wrapper against a fake Codex, with the deck's own `$HOME` pinned at
/// `deck_home`.
///
/// **`deck_home` is load-bearing, not hygiene** (issue #730). Since the trust
/// predicate narrowed to the exact command the install just wrote, the value
/// `platform::paths::durable_binary_path` resolves decides which listed entry is
/// trust-eligible — and step 2a of that resolver hangs off `$HOME`. Inheriting
/// the developer's real one would let `~/.local/bin/dot-agent-deck` (present on
/// a dev box, absent on every CI runner) pick the outcome, which is the leak
/// `codex_hooks_004`'s comment already records costing PR #733 a full CI round.
/// Seed it with [`seed_durable_binary`] and spell the expected command from the
/// path it returns.
fn run_wrapped_program(
    program: &std::path::Path,
    codex_home: &std::path::Path,
    fixture_dir: &std::path::Path,
    deck_home: &std::path::Path,
    hook_response: &str,
) -> (Output, Vec<String>, String) {
    let args_record = fixture_dir.join("args.txt");
    let home_record = fixture_dir.join("home.txt");
    let path = format!(
        "{}:{}",
        fixture_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args(["wrap", "--agent", "codex", "--"])
        .arg(program)
        .env("PATH", path)
        .env("HOME", deck_home)
        .env("CODEX_HOME", codex_home)
        .env("CODEX_ARGS_RECORD", &args_record)
        .env("CODEX_HOME_RECORD", &home_record)
        .env("CODEX_HOOK_LIST_RESPONSE", hook_response)
        .env("DOT_AGENT_DECK_PANE_ID", "codex-trust-test-pane")
        // Pin the hook endpoint at a dead path inside the fixture. The wrapper
        // emits `SessionStart`/`Idle` as it runs and resolves this var at emit
        // time, so without it the events travel to whatever daemon the
        // developer's `XDG_RUNTIME_DIR` points at — landing in their live deck
        // as a card for a pane that does not exist. Nothing listens here, and
        // `hook::send_to_socket` treats an unreachable socket as a no-op.
        .env("DOT_AGENT_DECK_SOCKET", fixture_dir.join("nowhere.sock"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run wrapper with fake Codex program");
    let args = std::fs::read_to_string(args_record)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    let observed_home = std::fs::read_to_string(home_record)
        .unwrap_or_default()
        .trim()
        .to_string();
    (output, args, observed_home)
}

fn trust_state_keys(home: &std::path::Path) -> Vec<String> {
    let path = home.join("config.toml");
    let contents = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "scoped trust config was not written at {}: {e}",
            path.display()
        )
    });
    let root: toml::Value = toml::from_str(&contents).expect("parse Codex config.toml");
    let mut keys = root
        .get("hooks")
        .and_then(|value| value.get("state"))
        .and_then(toml::Value::as_table)
        .expect("config.toml has [hooks.state] entries")
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    keys.sort();
    keys
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
        .env("CODEX_HOME_RECORD", fixture_dir.join("home.txt"))
        .env("CODEX_HOOK_LIST_RESPONSE", hook_list_response(Vec::new()))
        // Same reason as `run_wrapped_program` above: the wrapper emits as it
        // runs and resolves its endpoint at emit time. This site inherits no
        // pane id of its own, so without the pin it tags events with the pane
        // of whatever deck is running the suite and writes status into that
        // real card.
        .env("DOT_AGENT_DECK_SOCKET", fixture_dir.join("nowhere.sock"))
        .env_remove("DOT_AGENT_DECK_PANE_ID")
        .env_remove("DOT_AGENT_DECK_AGENT_ID")
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
    let home = test_temp::tempdir().expect("create Codex home");
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
    let home = test_temp::tempdir().expect("create Codex home");
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
    let home = test_temp::tempdir().expect("create Codex home");
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

/// Scenario: Seed a Codex hooks.json whose single PreToolUse rule object carries both a deck command and the user's own audit handler under one matcher, then reinstall from that same deck path. The user's handler and the rule's matcher must survive with only the deck's command taken out of it, and the refreshed deck rule added beside it.
#[test]
fn codex_hooks_install_007_a_mixed_rule_keeps_the_users_sibling_handler() {
    let home = test_temp::tempdir().expect("create Codex home");
    let user_command = "/usr/local/bin/my-critical-audit.sh";
    let deck_command = expected_hook_command(DECK_BINARY);
    write_hooks(
        home.path(),
        &json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [
                        { "type": "command", "command": deck_command },
                        { "type": "command", "command": user_command },
                    ]
                }]
            }
        }),
    );

    install_to(home.path(), DECK_BINARY).expect("install deck hooks");

    let rules = read_hooks(home.path())["hooks"]["PreToolUse"]
        .as_array()
        .expect("PreToolUse rules")
        .clone();
    let shared = rules
        .iter()
        .find(|rule| rule["matcher"] == json!("Bash"))
        .unwrap_or_else(|| {
            panic!("the rule object the user shared with the deck was deleted: {rules:?}")
        });
    assert_eq!(
        shared["hooks"].as_array().map(Vec::len),
        Some(1),
        "only the deck's own command may be removed from a shared rule: {shared:?}"
    );
    assert_eq!(
        shared["hooks"][0]["command"],
        json!(user_command),
        "the user's sibling handler must survive the reinstall: {shared:?}"
    );
    assert_eq!(
        deck_commands_for(home.path(), "PreToolUse"),
        vec![deck_command],
        "the deck's own rule must be refreshed exactly once: {rules:?}"
    );
}

/// Scenario: Reinstall Codex hooks over a deck-owned rule pinned at a second, still-valid deck install, and separately over one whose binary is positively gone. The valid foreign pin must be left in place with the fresh rule added beside it; the dead one must be repaired away, leaving a single deck rule.
#[test]
fn codex_hooks_install_008_a_valid_foreign_pin_survives_and_a_dead_one_is_repaired() {
    let fixture = test_temp::tempdir().expect("create install fixture");
    let installing = seed_executable(&fixture.path().join("this-install").join("dot-agent-deck"));
    let installing_command = expected_hook_command(&installing);

    // Arm 1 — a DIFFERENT but still-valid install, deliberately sharing the
    // installing binary's own basename so the only thing standing between it and
    // deletion is `pin_is_repairable` saying the target is still there.
    let other = seed_executable(&fixture.path().join("other-install").join("dot-agent-deck"));
    let other_command = expected_hook_command(&other);
    let valid = test_temp::tempdir().expect("create Codex home");
    write_hooks(
        valid.path(),
        &json!({
            "hooks": {
                "SessionStart": [
                    { "hooks": [ { "type": "command", "command": other_command } ] }
                ]
            }
        }),
    );

    install_to(valid.path(), &installing).expect("install over a valid foreign pin");

    assert_eq!(
        deck_commands_for(valid.path(), "SessionStart"),
        vec![other_command.clone(), installing_command.clone()],
        "PRD #381 Open Question 3: a deck pin that still works is left alone and the fresh \
         rule is added ALONGSIDE it, never repointed"
    );

    // Arm 2 — the same shape, but the pin is positively gone. Without this arm
    // the first one proves nothing: a predicate that never prunes anything would
    // pass it.
    let dead = fixture
        .path()
        .join("pruned-worktree")
        .join("dot-agent-deck");
    assert!(!dead.exists(), "the dead path must genuinely not exist");
    let dead_command = expected_hook_command(dead.to_str().expect("dead path is UTF-8"));
    let gone = test_temp::tempdir().expect("create Codex home");
    write_hooks(
        gone.path(),
        &json!({
            "hooks": {
                "SessionStart": [
                    { "hooks": [ { "type": "command", "command": dead_command } ] }
                ]
            }
        }),
    );

    install_to(gone.path(), &installing).expect("install over a dead pin");

    assert_eq!(
        deck_commands_for(gone.path(), "SessionStart"),
        vec![installing_command],
        "a deck pin whose binary is positively missing must still be repaired away"
    );
}

/// Scenario: Build a Codex hooks/list reply carrying the exact command the deck installs beside a crafted look-alike that merely ends with the same `hook --agent codex` verb, both unmanaged and both sourced from the deck's own hooks.json. Only the exact command may be selected for a trust write, while the wider revocation predicate still claims both.
#[spec("codex/trust/004")]
#[test]
fn codex_trust_004_only_the_exact_generated_command_is_trust_eligible() {
    let home = test_temp::tempdir().expect("create Codex home");
    let source_path = home.path().join("hooks.json");
    let deck_command = expected_hook_command(DECK_BINARY);
    let entry = |key: &str, command: &str| CodexHookEntry {
        key: key.to_string(),
        command: command.to_string(),
        source_path: source_path.clone(),
        current_hash: format!("sha256:{key}"),
        trust_status: "untrusted".to_string(),
        is_managed: false,
    };
    // The class, not an exploit: the deck's verb is a convention anything able
    // to write into `hooks.json` can also end a command with, so under the old
    // suffix test a command naming some OTHER executable was indistinguishable
    // from a deck entry — and a `trusted_hash` record is Codex's permission to
    // run it. Tightening the suffix cannot fix that; comparing against the exact
    // command the deck emits for the path it validated can.
    let entries = vec![
        entry("deck", &deck_command),
        entry("crafted", "/usr/local/bin/not-the-deck hook --agent codex"),
        entry("mention", "/usr/local/bin/audit --watch dot-agent-deck"),
    ];

    let trustable: Vec<&str> = deck_owned_entries(
        &entries,
        home.path(),
        DeckCommandMatch::Exact(&deck_command),
    )
    .into_iter()
    .map(|entry| entry.key.as_str())
    .collect();
    assert_eq!(
        trustable,
        vec!["deck"],
        "only the exact installed deck command may be handed a trusted_hash"
    );

    // The revocation predicate is deliberately the wider one: an uninstall
    // deletes every deck-owned command from `hooks.json` whichever install wrote
    // it, so narrowing here would orphan that entry's trust record.
    let revocable: Vec<&str> =
        deck_owned_entries(&entries, home.path(), DeckCommandMatch::Signature)
            .into_iter()
            .map(|entry| entry.key.as_str())
            .collect();
    assert_eq!(
        revocable,
        vec!["deck", "crafted"],
        "untrust must still reach any deck-signature command in the deck's own hooks.json"
    );
}

/// Scenario: Wrap a direct executable named codex while explicitly declaring the agent as Claude. The wrapper must neither install Codex hooks nor inject Codex's hook-trust bypass flag.
#[test]
fn codex_hooks_trust_001_direct_codex_requires_codex_identity() {
    let fixture = test_temp::tempdir().expect("create wrapper fixture");
    let home = test_temp::tempdir().expect("create Codex home");

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

/// Scenario: Launch a bare Codex stand-in and a launcher with a mixed hooks/list response from the pinned home. Only the unmanaged entry whose command is exactly the one the deck just installed may receive a scoped trust record; foreign, differently sourced, managed, merely name-mentioning, and crafted same-suffix commands must all remain untrusted.
#[spec("codex/trust/002")]
#[test]
fn codex_trust_002_only_pinned_unmanaged_deck_entries_are_trusted() {
    for launcher in [false, true] {
        let fixture = test_temp::tempdir().expect("create wrapper fixture");
        let home = test_temp::tempdir().expect("create Codex home");
        let deck_home = test_temp::tempdir().expect("create isolated deck HOME");
        let deck_command = expected_hook_command(
            seed_durable_binary(deck_home.path())
                .to_str()
                .expect("durable path is UTF-8"),
        );
        let fake_codex = write_fake_codex(fixture.path());
        write_hooks(
            home.path(),
            &json!({
                "hooks": {
                    "PreToolUse": [{
                        "hooks": [{
                            "type": "command",
                            "command": "/usr/local/bin/foreign-hook"
                        }]
                    }]
                }
            }),
        );
        let program = if launcher {
            let path = fixture.path().join("launcher.sh");
            write_fake_program(&path);
            path
        } else {
            std::path::PathBuf::from("codex")
        };
        let good_key = "__CODEX_HOME__/hooks.json:pre_tool_use:0:0";
        let response = hook_list_response(vec![
            hook_entry(
                good_key,
                &deck_command,
                "__CODEX_HOME__/hooks.json",
                "sha256:deck",
                false,
            ),
            hook_entry(
                "__CODEX_HOME__/hooks.json:pre_tool_use:0:1",
                "/usr/local/bin/foreign-hook",
                "__CODEX_HOME__/hooks.json",
                "sha256:foreign",
                false,
            ),
            hook_entry(
                "/different/home/hooks.json:pre_tool_use:0:0",
                "/opt/dot-agent-deck hook --agent codex",
                "/different/home/hooks.json",
                "sha256:different-home",
                false,
            ),
            hook_entry(
                "__CODEX_HOME__/hooks.json:pre_tool_use:0:2",
                "/opt/dot-agent-deck hook --agent codex",
                "__CODEX_HOME__/hooks.json",
                "sha256:managed",
                true,
            ),
            hook_entry(
                "__CODEX_HOME__/hooks.json:pre_tool_use:0:3",
                "/usr/local/bin/audit --watch dot-agent-deck",
                "__CODEX_HOME__/hooks.json",
                "sha256:mention",
                false,
            ),
            // Issue #730 / auditor finding LOW-1: the deck's verb is a
            // CONVENTION, not a capability — anything able to write into the
            // deck's own hooks.json can end a command with it and be
            // indistinguishable from a deck entry under a suffix test. A trust
            // record is Codex's permission to RUN the command, so the predicate
            // compares against the exact command the install just wrote instead.
            hook_entry(
                "__CODEX_HOME__/hooks.json:pre_tool_use:0:4",
                "/usr/local/bin/not-the-deck hook --agent codex",
                "__CODEX_HOME__/hooks.json",
                "sha256:crafted",
                false,
            ),
        ]);

        let (output, args, _) = run_wrapped_program(
            &program,
            home.path(),
            fixture.path(),
            deck_home.path(),
            &response,
        );

        assert!(output.status.success(), "wrapper failed: {output:?}");
        assert!(
            !args.iter().any(|arg| arg == TRUST_BYPASS_FLAG),
            "the invocation-global bypass must never appear: args={args:?}"
        );
        assert_eq!(
            trust_state_keys(home.path()),
            vec![good_key.replace("__CODEX_HOME__", &home.path().display().to_string())],
            "only the pinned home's unmanaged entry carrying the exact installed deck command \
             may be trusted (launcher={launcher})"
        );
        drop(fake_codex);
    }
}

/// Scenario: Launch Codex identity through bare codex, an absolute codex path, a launcher script, and devbox. Every child must inherit the pinned home and no program form may receive the deleted invocation-global hook-trust bypass.
#[spec("codex/trust/001")]
#[test]
fn codex_trust_001_no_program_form_receives_global_bypass() {
    for program_name in ["codex", "absolute-codex", "launcher.sh", "devbox"] {
        let fixture = test_temp::tempdir().expect("create Codex launcher fixture");
        let home = test_temp::tempdir().expect("create pinned Codex home");
        let deck_home = test_temp::tempdir().expect("create isolated deck HOME");
        seed_durable_binary(deck_home.path());
        let fake_codex = write_fake_codex(fixture.path());
        let program = match program_name {
            "codex" => std::path::PathBuf::from("codex"),
            "absolute-codex" => fake_codex.clone(),
            name => {
                let path = fixture.path().join(name);
                write_fake_program(&path);
                if name == "devbox" {
                    std::path::PathBuf::from(name)
                } else {
                    path
                }
            }
        };

        let (output, args, observed_home) = run_wrapped_program(
            &program,
            home.path(),
            fixture.path(),
            deck_home.path(),
            &hook_list_response(Vec::new()),
        );

        assert!(
            output.status.success(),
            "wrapper failed for {program_name}: {output:?}"
        );
        assert_eq!(
            observed_home,
            home.path().display().to_string(),
            "{program_name} did not inherit the pinned CODEX_HOME"
        );
        assert!(
            !args.iter().any(|arg| arg == TRUST_BYPASS_FLAG),
            "{program_name} received the deleted invocation-global trust bypass: args={args:?}"
        );
    }
}

/// Scenario: Trust one deck hook in a Codex home with an existing commented config and a foreign trust record, repeat the write, then uninstall Codex hooks. Unrelated bytes and the foreign record must survive, the deck table must not duplicate, and uninstall must remove only the deck key.
#[spec("codex/trust/003")]
#[test]
fn codex_trust_003_config_edits_are_preserving_idempotent_and_scoped() {
    let fixture = test_temp::tempdir().expect("create wrapper fixture");
    let home = test_temp::tempdir().expect("create Codex home");
    let deck_home = test_temp::tempdir().expect("create isolated deck HOME");
    let deck_command = expected_hook_command(
        seed_durable_binary(deck_home.path())
            .to_str()
            .expect("durable path is UTF-8"),
    );
    write_fake_codex(fixture.path());
    let original = "# user comment must survive\nmodel = \"gpt-user-choice\"\n\n[hooks.state.\"foreign-key\"]\nenabled = true\ntrusted_hash = \"sha256:foreign\"\n";
    std::fs::write(home.path().join("config.toml"), original).expect("seed Codex config");
    let deck_key_template = "__CODEX_HOME__/hooks.json:pre_tool_use:0:0";
    let deck_key = deck_key_template.replace("__CODEX_HOME__", &home.path().display().to_string());
    let response = hook_list_response(vec![hook_entry(
        deck_key_template,
        &deck_command,
        "__CODEX_HOME__/hooks.json",
        "sha256:deck",
        false,
    )]);

    for _ in 0..2 {
        let (output, _, _) = run_wrapped_program(
            std::path::Path::new("codex"),
            home.path(),
            fixture.path(),
            deck_home.path(),
            &response,
        );
        assert!(output.status.success(), "wrapper failed: {output:?}");
    }

    let trusted = std::fs::read_to_string(home.path().join("config.toml"))
        .expect("read trusted Codex config");
    assert!(
        trusted.starts_with(original),
        "existing config bytes, comments, and model selection must remain verbatim:\n{trusted}"
    );
    assert_eq!(
        trusted
            .matches(&format!("[hooks.state.\"{deck_key}\"]"))
            .count(),
        1,
        "repeated trust writes must not duplicate the deck table:\n{trusted}"
    );
    assert_eq!(
        trust_state_keys(home.path()),
        vec![deck_key.clone(), "foreign-key".to_string()],
    );

    let path = format!(
        "{}:{}",
        fixture.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let uninstall = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args(["hooks", "uninstall", "--agent", "codex"])
        .env("PATH", path)
        .env("CODEX_HOME", home.path())
        .env("CODEX_HOOK_LIST_RESPONSE", &response)
        .output()
        .expect("uninstall Codex hooks");
    assert!(
        uninstall.status.success(),
        "Codex hook uninstall failed: {}",
        String::from_utf8_lossy(&uninstall.stderr)
    );
    assert_eq!(
        trust_state_keys(home.path()),
        vec!["foreign-key".to_string()],
        "untrust must remove only the deck-owned key"
    );
}

/// Scenario: Run the documented Codex hook installation command against an isolated home, an isolated `$HOME` holding a seeded durable deck, and a deterministic app-server stand-in. The command must succeed, materialize Codex hook definitions, and pin the seeded durable binary rather than anything the host happens to have installed.
#[spec("codex/hooks/004")]
#[test]
fn codex_hooks_004_cli_install_succeeds() {
    let fixture = test_temp::tempdir().expect("create CLI fixture");
    let home = test_temp::tempdir().expect("create Codex home");
    let deck_home = test_temp::tempdir().expect("create isolated deck HOME");
    write_fake_codex(fixture.path());
    let durable = seed_durable_binary(deck_home.path());
    // Both halves are load-bearing. `HOME` anchors the resolver's durable
    // candidate at the seeded one above; the pinned `PATH` carries only the
    // fake `codex` plus the system directories the stand-in's `sed` needs, so
    // no `dot-agent-deck` the host happens to have on `$PATH` can decide the
    // outcome. Inheriting `$PATH` here is exactly what let this test pass on a
    // machine with `~/.local/bin/dot-agent-deck` and fail on every CI runner.
    let path = format!("{}:/usr/bin:/bin", fixture.path().display());

    let output = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args(["hooks", "install", "--agent", "codex"])
        .env("PATH", path)
        .env("HOME", deck_home.path())
        .env("CODEX_HOME", home.path())
        .env("CODEX_HOOK_LIST_RESPONSE", hook_list_response(Vec::new()))
        .output()
        .expect("install Codex hooks from CLI");

    assert!(
        output.status.success(),
        "documented Codex hook install must succeed; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        hooks_path(home.path()).exists(),
        "successful Codex hook install did not create hooks.json"
    );
    let written = std::fs::read_to_string(hooks_path(home.path())).expect("read hooks.json");
    assert!(
        written.contains(durable.to_str().expect("durable path is UTF-8")),
        "the install pinned something other than the durable binary seeded in this fixture, so \
         the test's outcome is being decided by host state:\n{written}"
    );
}
