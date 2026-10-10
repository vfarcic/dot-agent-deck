#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]

//! Interactive Haiku work across a remote upgrade. SSH and the downloaded
//! release are owned stand-ins; the daemon, agent, attached TUI and CLI are real.

mod common;
#[path = "support/remote_upgrade.rs"]
mod upgrade_fixture;

use std::fs;
use std::process::Command;
use std::time::Duration;

use dot_agent_deck::daemon_protocol::AttachRequest;
use dot_agent_deck::event::{AgentType, SendResult};
use spec::spec;
use upgrade_fixture::{DECK, NEW_BUILD, OLD_BUILD, Remote, pid_file, quoted, script};

const PANE: &str = "upgrade-live-haiku";
const LABEL: &str = "upgrade proof Haiku";
const AGENT_WAIT: Duration = Duration::from_secs(120);
/// How long Haiku gets to read a fixture and report it. A model round trip on
/// a loaded box outlasts [`AGENT_WAIT`]: measured 2026-10-10, this step timed
/// out at 120s under a load average of 53-71 and took 24s for the whole test
/// alone. Matches `REPORT_WAIT` in `e2e_new_agent_live.rs`.
const REPORT_WAIT: Duration = Duration::from_secs(240);

/// A per-run hex token, so a sentinel can only reach the pane by being read.
fn unique_token() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("OS randomness for a fixture sentinel");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn report_file(
    remote: &Remote,
    deck: &common::TuiDeck,
    agent: &str,
    session: &str,
    file: &str,
    expected: &str,
) {
    let prompt = format!(
        "Use the Read tool to read {file} in the current directory and report its exact contents. This is the complete task; do not ask questions or modify files."
    );
    // The expected contents are absent from the submitted prompt. Only actual
    // fixture inspection can put them on the live pane and the recorded grid.
    assert!(!prompt.contains(expected));
    let sent = common::write_and_submit_with_identity_on(
        &remote.attach,
        PANE,
        &prompt,
        agent,
        Some(session),
    )
    .expect("submit the fixture-reading task");
    assert_eq!(sent.send_result, Some(SendResult::Applied));
    assert!(
        common::wait_for_pane_text_on(&remote.attach, agent, expected, REPORT_WAIT),
        "interactive Haiku must read {file}; pane: {}",
        common::pane_search_key_on(&remote.attach, agent)
    );
    deck.wait_for_string(expected);
}

/// Scenario: Watch interactive Haiku read a unique fixture sentinel in a daemon-owned pane, then upgrade that remote over a PTY and choose Keep at the question naming Haiku.
/// The same agent reads a second sentinel after Keep; a second upgrade with Restart now stops that named agent and an attached deck renders the new daemon's empty dashboard.
#[spec("remote/upgrade/008")]
#[test]
fn remote_upgrade_008_live_interactive_haiku_keeps_work_then_restarts() {
    skip_unless!(common::check_claude_available());
    // Outlives nextest's 840s allowance for this test (`.config/nextest.toml`),
    // so a slow phase fails on its own wait, not on a daemon that reached its
    // lifetime first. 900 is the most `MAX_PINNED_ORPHAN_CAP_SECS` allows.
    let mut remote = Remote::with_lifetime(900);
    let home = remote.dir.path().join("remote-home");
    let workspace = remote.dir.path().join("haiku-workspace");
    fs::create_dir_all(&workspace).unwrap();
    let cwd = workspace.to_string_lossy().into_owned();
    common::seed_claude_worker_home(&home, std::slice::from_ref(&cwd))
        .expect("import Claude auth and seed onboarding plus per-folder trust");

    let retained = remote.dir.path().join("retained-build");
    let installed = Command::new(&retained)
        .args(["hooks", "install", "--agent", "claude-code"])
        .env_clear()
        .env("HOME", &home)
        .env("PATH", std::env::var("PATH").unwrap())
        .output()
        .expect("install hooks in the owned Claude HOME");
    assert!(
        installed.status.success(),
        "isolated hook installation must succeed"
    );

    let pid_path = remote.dir.path().join("haiku.pid");
    let launcher = remote.dir.path().join("interactive-haiku.sh");
    script(
        &launcher,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$$\" > {}\nexec claude --model claude-haiku-4-5-20251001 --allowedTools Read Bash\n",
            quoted(&pid_path),
        ),
    );
    let mut env = vec![("DOT_AGENT_DECK_PANE_ID".into(), PANE.into())];
    // Match the harness's explicit credential pass-through. Nothing is printed.
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        env.push(("ANTHROPIC_API_KEY".into(), key));
    }
    // Open before the start: the guarded submit binds to Haiku's real
    // `SessionStart` generation, and nothing broadcast earlier is replayed.
    let events = common::subscribe_events_on(&remote.attach);
    let start = common::attach_request_on(
        &remote.attach,
        &AttachRequest::StartAgent {
            command: Some(format!("/bin/sh {}", quoted(&launcher))),
            cwd: Some(cwd),
            rows: 36,
            cols: 150,
            env,
            display_name: Some(LABEL.into()),
            tab_membership: None,
            agent_type: Some(AgentType::ClaudeCode),
            seed: None,
            authoring_kind: None,
            client_seeded_kind: None,
            remember_command: false,
        },
    )
    .expect("start interactive Haiku on the old daemon");
    assert!(start.ok, "start Haiku: {:?}", start.error);
    let agent = start.id.unwrap();
    assert!(common::wait_until(AGENT_WAIT, || pid_file(&pid_path).is_some()));
    remote
        .agents
        .push((agent.clone(), pid_file(&pid_path).unwrap()));
    let session = events.wait_for_session_start_on_pane(PANE, &agent, AGENT_WAIT);

    // A normal attached TUI watches the old remote daemon's private endpoint.
    // Its build override matches that daemon so this viewer needs no consent.
    // TuiDeck supplies credential-redacted casts and provenance for the reel.
    let deck = common::TuiDeck::builder()
        .with_pty_size(180, 45)
        .with_imported_claude_credentials()
        .with_env(
            "DOT_AGENT_DECK_ATTACH_SOCKET",
            remote.attach.to_string_lossy(),
        )
        .with_env(
            "DOT_AGENT_DECK_SOCKET",
            remote.dir.path().join("remote-hook.sock").to_string_lossy(),
        )
        .with_env(
            "DOT_AGENT_DECK_STATE_DIR",
            remote.dir.path().join("viewer-state").to_string_lossy(),
        )
        .with_env("DOT_AGENT_DECK_BUILD_ID_OVERRIDE", OLD_BUILD)
        .launch_with_fixture("minimal");
    deck.wait_for_absence("No active agents");
    deck.send_keys(b"1");
    assert!(
        common::wait_until_panes_settled(
            &remote.attach,
            std::slice::from_ref(&agent),
            Duration::from_millis(1500),
            Duration::from_secs(8),
            AGENT_WAIT,
        ),
        "interactive Haiku must finish onboarding before submission"
    );
    deck.wait_for_string("Haiku");

    let first = format!("UPGRADE_BEFORE_{}", unique_token());
    fs::write(workspace.join("upgrade-before-proof.txt"), &first).unwrap();
    report_file(
        &remote,
        &deck,
        &agent,
        &session,
        "upgrade-before-proof.txt",
        &first,
    );

    let mut keep = remote.tty(&["remote", "upgrade", DECK]);
    keep.wait("Restart now?");
    assert!(keep.output().contains(LABEL) && keep.output().contains(PANE));
    keep.send("\n");
    keep.success();
    remote.assert_installed();
    remote.assert_kept();

    let second = format!("UPGRADE_AFTER_KEEP_{}", unique_token());
    fs::write(workspace.join("upgrade-after-keep-proof.txt"), &second).unwrap();
    report_file(
        &remote,
        &deck,
        &agent,
        &session,
        "upgrade-after-keep-proof.txt",
        &second,
    );
    remote.assert_kept();

    let mut restart = remote.tty(&["remote", "upgrade", DECK]);
    restart.wait("Restart now?");
    assert!(restart.output().contains(LABEL) && restart.output().contains(PANE));
    // Exactly the disclosed live agent is at stake in this owned daemon.
    assert_eq!(remote.inventory().agent_records.unwrap().len(), 1);
    restart.send("r\n");
    restart.success();
    remote.assert_restarted();
    assert_eq!(
        remote.hello().unwrap().build_version.as_deref(),
        Some(NEW_BUILD)
    );
    // The installed client and successor use the same fixture build stamp.
    remote
        .env
        .push(("DOT_AGENT_DECK_BUILD_ID_OVERRIDE".into(), NEW_BUILD.into()));
    let mut connected = remote.tty(&["connect", DECK]);
    connected.wait("No active agents");
    assert!(connected.screen().contains("[New Agent Ctrl+N]"));
}
