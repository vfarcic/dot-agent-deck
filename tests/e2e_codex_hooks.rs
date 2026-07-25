#![cfg(feature = "e2e")]

//! PTY-attached real-Codex native-hook parity coverage for PRD #20 W1.

mod common;

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::event::{AgentType, EventType};
use serde_json::{Value, json};
use spec::spec;

const HOOK_SENTINEL_NAME: &str = "codex_hooks_sentinel_f42e71.txt";
const HOOK_SENTINEL_CONTENT: &str = "CODEX_HOOKS_OK";
const TRUST_BYPASS_FLAG: &str = "--dangerously-bypass-hook-trust";
const DECK_HOOK_EVENTS: &[(&str, &str)] = &[
    ("SessionStart", "session_start"),
    ("UserPromptSubmit", "user_prompt_submit"),
    ("PreToolUse", "pre_tool_use"),
    ("PostToolUse", "post_tool_use"),
    ("PermissionRequest", "permission_request"),
    ("Stop", "stop"),
    ("PreCompact", "pre_compact"),
    ("PostCompact", "post_compact"),
    ("SubagentStart", "subagent_start"),
    ("SubagentStop", "subagent_stop"),
];

fn path_with_binary_dir() -> String {
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let bin_dir = std::path::Path::new(bin)
        .parent()
        .expect("test binary has a parent dir")
        .to_str()
        .expect("binary directory is UTF-8");
    format!("{bin_dir}:{}", std::env::var("PATH").unwrap_or_default())
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

fn deck_hook_response() -> String {
    hook_list_response(
        DECK_HOOK_EVENTS
            .iter()
            .enumerate()
            .map(|(index, (_event, event_key))| {
                json!({
                    "key": format!("__CODEX_HOME__/hooks.json:{event_key}:0:0"),
                    "eventName": event_key,
                    "handlerType": "command",
                    "matcher": null,
                    "command": "/opt/dot-agent-deck hook --agent codex",
                    "timeoutSec": 600,
                    "statusMessage": null,
                    "sourcePath": "__CODEX_HOME__/hooks.json",
                    "source": "user",
                    "pluginId": null,
                    "displayOrder": index,
                    "enabled": true,
                    "isManaged": false,
                    "currentHash": format!("sha256:deck-{index}"),
                    "trustStatus": "untrusted"
                })
            })
            .collect(),
    )
}

#[cfg(unix)]
fn write_executable(path: &Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::write(path, contents).expect("write executable fixture");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("make fixture executable");
}

fn trust_state_keys(home: &Path) -> Vec<String> {
    let config_path = home.join("config.toml");
    let contents = std::fs::read_to_string(&config_path).unwrap_or_else(|e| {
        panic!(
            "script-launched Codex trust config was not written at {}: {e}",
            config_path.display()
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

/// Scenario: Wrap a launcher script explicitly identified as Codex while a deterministic app-server stand-in reports the deck's hooks. The launcher child must inherit the pinned home without a global bypass, and config.toml must trust exactly the ten deck hook keys.
#[spec("codex/hooks/002")]
#[test]
#[cfg(unix)]
fn codex_hooks_002_script_launch_installs_exact_scoped_trust() {
    let fixture = tempfile::tempdir().expect("create script launch fixture");
    let home = tempfile::tempdir().expect("create isolated Codex home");
    let bin_dir = fixture.path().join("bin");
    std::fs::create_dir(&bin_dir).expect("create fixture bin");
    let child_record = fixture.path().join("child.txt");
    write_executable(
        &bin_dir.join("codex"),
        "#!/bin/sh\nif [ \"${1:-}\" = app-server ]; then\n    IFS= read -r _initialize\n    printf '%s\\n' '{\"id\":1,\"result\":{\"userAgent\":\"hooks-e2e\",\"codexHome\":\"test\",\"platformFamily\":\"unix\",\"platformOs\":\"linux\"}}'\n    IFS= read -r _list\n    printf '%s\\n' \"$CODEX_HOOK_LIST_RESPONSE\" | sed \"s|__CODEX_HOME__|$CODEX_HOME|g\"\n    exit 0\nfi\nprintf 'home=%s\\n' \"$CODEX_HOME\" > \"$CODEX_CHILD_RECORD\"\nprintf 'arg=%s\\n' \"$@\" >> \"$CODEX_CHILD_RECORD\"\n",
    );
    let launcher = fixture.path().join("launcher.sh");
    write_executable(&launcher, "#!/bin/sh\nexec codex \"$@\"\n");
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let output = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args(["wrap", "--agent", "codex", "--"])
        .arg(&launcher)
        .env("PATH", path)
        .env("CODEX_HOME", home.path())
        .env("CODEX_CHILD_RECORD", &child_record)
        .env("CODEX_HOOK_LIST_RESPONSE", deck_hook_response())
        .env("DOT_AGENT_DECK_PANE_ID", "script-codex-pane")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run wrapped Codex launcher");

    assert!(
        output.status.success(),
        "wrapped launcher failed: {output:?}"
    );
    assert!(
        home.path().join("hooks.json").exists(),
        "hooks.json was not installed"
    );
    let child = std::fs::read_to_string(&child_record).expect("read launcher child record");
    assert!(
        child.contains(&format!("home={}", home.path().display())),
        "launcher child did not inherit the pinned CODEX_HOME: {child:?}"
    );
    assert!(
        !child.contains(&format!("arg={TRUST_BYPASS_FLAG}")),
        "launcher child received the invocation-global bypass: {child:?}"
    );
    let mut expected = DECK_HOOK_EVENTS
        .iter()
        .map(|(_, event_key)| format!("{}/hooks.json:{event_key}:0:0", home.path().display()))
        .collect::<Vec<_>>();
    expected.sort();
    assert_eq!(
        trust_state_keys(home.path()),
        expected,
        "script launch must trust exactly the deck-authored hook keys"
    );
}

/// Scenario: Start the real deck with a restored non-Codex-basename launcher while a Codex stand-in is available on PATH. Command-agnostic startup installation and scoped trust must let the launcher emit a Codex prompt event that visibly creates a Codex card and Thinking status.
#[spec("codex/hooks/003")]
#[test]
#[cfg(unix)]
fn codex_hooks_003_non_codex_launcher_gets_startup_integration() {
    let fixture_bin = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-synthetic");
    let path = format!("{}:{}", fixture_bin.display(), path_with_binary_dir());
    let deck = TuiDeck::builder()
        .with_pty_size(180, 45)
        .with_env("PATH", path)
        .with_env("CODEX_HOOK_LIST_RESPONSE", deck_hook_response())
        .with_continue_session("launcher-codex", "/bin/sh startup-parity-launcher.sh")
        .launch_with_fixture("codex-synthetic");

    deck.wait_for_string("[Command Mode Ctrl+D]");
    deck.send_bytes(b"\x04");
    deck.wait_for_string("Dir:");
    assert!(
        deck.wait_for_grid_string_within("Codex ·", Duration::from_secs(15)),
        "the non-codex launcher never produced a Codex card after startup install+trust:\n{}",
        deck.snapshot_grid()
    );
    assert!(
        deck.wait_for_grid_string_within("Thinking", Duration::from_secs(15))
            && deck.wait_for_grid_string_within("STARTUP_CODEX_PARITY", Duration::from_secs(15)),
        "the launcher-delivered Codex prompt event was not visible on the card:\n{}",
        deck.snapshot_grid()
    );
}

/// Scenario: Launch a real cheap-model Codex through the normal wrapped pane
/// seam, submit a directive that runs one shell command and creates a unique
/// sentinel, then detach without exiting Codex. Native hooks installed in the
/// isolated Codex home must make the dashboard show the prompt, shell tool name
/// and command detail, and finally Idle while the Codex pane remains alive.
#[spec("codex/hooks/001")]
#[test]
fn codex_hooks_001_real_interactive_turn_reaches_idle_without_exit() {
    skip_unless!(common::check_codex_available());

    let prompt = format!(
        "Run this exact command with the shell tool: printf {HOOK_SENTINEL_CONTENT} > {HOOK_SENTINEL_NAME}. Do not use apply_patch. After reporting completion, stay open and wait for another prompt."
    );
    let command = format!(
        "codex --model {} --sandbox workspace-write --ask-for-approval never -c 'sandbox_workspace_write.network_access=true' -c 'model_reasoning_effort=\"low\"'",
        common::CODEX_TEST_MODEL,
    );
    let config_dir = tempfile::tempdir().expect("Codex hooks new-pane config");
    let config_path = config_dir.path().join("config.toml");
    std::fs::write(&config_path, format!("default_command = {command:?}\n"))
        .expect("write bare Codex hooks command");
    let deck = TuiDeck::builder()
        .with_pty_size(180, 45)
        .with_env("PATH", path_with_binary_dir())
        .with_env("DOT_AGENT_DECK_CONFIG", config_path.to_string_lossy())
        .with_imported_codex_credentials()
        .launch_with_fixture("codex-live");

    deck.wait_for_string("No active sessions");
    let events = deck.subscribe_events();
    deck.send_keys(b"\x0e");
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" ");
    deck.wait_for_string("Tab: switch");
    deck.send_keys(b"\r");
    deck.send_keys(b"\r");
    deck.send_keys(b"\r");
    deck.wait_for_string("[Command Mode Ctrl+D]");
    assert!(
        deck.wait_for_grid_string_within(common::CODEX_TEST_MODEL, Duration::from_secs(30)),
        "the wrapped interactive Codex UI never became ready:\n{}",
        deck.snapshot_grid()
    );

    deck.send_keys(prompt.as_bytes());
    deck.wait_for_string(HOOK_SENTINEL_NAME);
    deck.send_keys(b"\r");
    deck.send_bytes(b"\x04");
    deck.wait_for_string("Dir:");
    assert!(
        deck.wait_for_grid_string_within("Thinking", Duration::from_secs(60)),
        "the dashboard card never showed Thinking after Codex prompt submission:\n{}",
        deck.snapshot_grid()
    );

    let prompt_event = events.wait_for(
        |event| {
            event.agent_type == AgentType::Codex
                && event.event_type == EventType::Thinking
                && event.user_prompt.as_deref() == Some(prompt.as_str())
        },
        Duration::from_secs(120),
    );
    assert_eq!(prompt_event.agent_type, AgentType::Codex);
    assert!(
        deck.wait_for_grid_string_within(HOOK_SENTINEL_NAME, Duration::from_secs(30)),
        "the Codex UserPromptSubmit detail never appeared on the dashboard card:\n{}",
        deck.snapshot_grid()
    );

    let tool_start = events.wait_for(
        |event| {
            event.agent_type == AgentType::Codex
                && event.event_type == EventType::ToolStart
                && event.tool_name.as_deref() == Some("Bash")
                && event
                    .tool_detail
                    .as_deref()
                    .is_some_and(|value| value.contains(HOOK_SENTINEL_NAME))
        },
        Duration::from_secs(120),
    );
    assert_eq!(tool_start.tool_name.as_deref(), Some("Bash"));
    assert!(
        deck.wait_for_grid_string_within("Bash", Duration::from_secs(30))
            && deck.wait_for_grid_string_within(HOOK_SENTINEL_NAME, Duration::from_secs(30)),
        "the Codex Bash tool name and command detail never appeared on the dashboard card:\n{}",
        deck.snapshot_grid()
    );

    let tool_end = events.wait_for(
        |event| {
            event.agent_type == AgentType::Codex
                && event.event_type == EventType::ToolEnd
                && event.tool_name.as_deref() == Some("Bash")
                && event
                    .tool_detail
                    .as_deref()
                    .is_some_and(|value| value.contains(HOOK_SENTINEL_NAME))
        },
        Duration::from_secs(120),
    );
    assert!(
        tool_end
            .tool_detail
            .as_deref()
            .is_some_and(|value| value.contains(HOOK_SENTINEL_NAME)),
        "Codex PostToolUse lost the Bash command detail: {tool_end:?}"
    );

    let idle = events.wait_for(
        |event| event.agent_type == AgentType::Codex && event.event_type == EventType::Idle,
        Duration::from_secs(120),
    );
    assert_eq!(idle.agent_type, AgentType::Codex);
    assert!(
        deck.wait_for_grid_string_within("Idle", Duration::from_secs(30)),
        "the Codex card did not return to Idle at Stop-hook turn end:\n{}",
        deck.snapshot_grid()
    );

    let sentinel = deck.workdir().join(HOOK_SENTINEL_NAME);
    let sentinel_content = std::fs::read_to_string(&sentinel)
        .expect("real Codex did not create the requested hook sentinel");
    assert_eq!(
        sentinel_content, HOOK_SENTINEL_CONTENT,
        "real Codex did not complete the requested shell work"
    );
    assert!(
        common::agent_records_on(deck.attach_socket_path())
            .iter()
            .any(|record| record.agent_type == Some(AgentType::Codex)),
        "Stop-hook Idle was observed only after Codex exited; the pane must still be live"
    );
}
