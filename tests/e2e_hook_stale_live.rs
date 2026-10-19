#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]

//! A genuine interactive Claude turn with hooks behaving like an old binary.
mod common;
#[path = "support/hook_stale.rs"]
mod hook_stale;

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::agent_pty::DOT_AGENT_DECK_PANE_ID;
use dot_agent_deck::daemon_client::{DaemonClient, StartAgentOptions};
use dot_agent_deck::event::{AgentType, EventType};
use dot_agent_deck::hook_binary::HookBinaryReason;
use hook_stale::{StaleHome, notices};
use spec::spec;

/// Scenario: A real interactive Haiku Claude lists a uniquely named sentinel
/// while its pinned wrapper forwards genuine hooks with version stamps removed.
/// The attached dashboard must show Working then Idle and the stale notice in
/// the same frame, with the sentinel visible in the agent's terminal.
/// Zooming hides the notice, and restoring the dashboard shows it again.
#[spec("hooks/stale/004")]
#[test]
fn hooks_stale_004_real_interactive_claude_works_with_a_visible_notice() {
    skip_unless!(common::check_claude_available());
    let fixture = StaleHome::new();
    let forwarded = fixture.home.join("forwarded-hook-types.txt");
    // The wrapper runs the genuine hook CLI behind a proxy of its hook socket
    // that removes only the two version keys from producer (status) lines,
    // preserving the genuine pane capability and all event data. This models an
    // old binary without adding a production stamp-disable seam.
    //
    // PRD #1666 MC-1: the proxy is concurrent and bidirectional. The inbox
    // hook's `UserPromptSubmit` sends an acknowledging `InboxFetch` and waits
    // for its reply, which a one-way, after-the-fact forwarder could neither
    // answer nor return — the fetch timed out, and the forwarded request then
    // acknowledged a task whose body never reached Claude. Every connection the
    // CLI opens is relayed while it runs, replies included, and each side's EOF
    // is passed on as a half-close, so connection lifetime is the CLI's own. The
    // CLI inherits stdin and stdout, so a hook's context output and a
    // long-lived waiter behave as with a direct call, with no timeout of ours.
    let script = format!(
        r#"#!/usr/bin/env python3
import json, os, socket, subprocess, sys, tempfile, threading
if sys.argv[1:] == ['--version']:
    print('dot-agent-deck 0.0.1')
    sys.exit(0)
destination = os.environ['DOT_AGENT_DECK_SOCKET']
audit_lock = threading.Lock()

def rewrite(line):
    # Producer lines are bare AgentEvent JSON; requests carry `message_type`
    # and pass through untouched.
    try:
        event = json.loads(line)
    except ValueError:
        return line
    if not isinstance(event, dict) or 'message_type' in event:
        return line
    event.pop('deck_build', None)
    event.pop('deck_exe', None)
    with audit_lock, open({forwarded}, 'a') as audit:
        audit.write(event.get('event_type', 'unknown') + '\n')
    return json.dumps(event).encode()

def half_close(sock):
    try:
        sock.shutdown(socket.SHUT_WR)
    except OSError:
        pass

def upstream(incoming, outgoing):
    pending = b''
    try:
        while True:
            chunk = incoming.recv(65536)
            if not chunk:
                break
            pending += chunk
            while b'\n' in pending:
                line, pending = pending.split(b'\n', 1)
                outgoing.sendall(rewrite(line) + b'\n')
        if pending:
            outgoing.sendall(rewrite(pending))
    except OSError:
        pass
    half_close(outgoing)

def downstream(outgoing, incoming):
    try:
        while True:
            chunk = outgoing.recv(65536)
            if not chunk:
                break
            incoming.sendall(chunk)
    except OSError:
        pass
    half_close(incoming)

def relay(incoming):
    with incoming, socket.socket(socket.AF_UNIX) as outgoing:
        outgoing.connect(destination)
        back = threading.Thread(target=downstream, args=(outgoing, incoming), daemon=True)
        back.start()
        upstream(incoming, outgoing)
        back.join()

with tempfile.TemporaryDirectory(prefix='old-hook-') as root:
    path = root + '/proxy.sock'
    with socket.socket(socket.AF_UNIX) as server:
        server.bind(path)
        server.listen(16)
        server.settimeout(0.1)
        done = threading.Event()
        relays = []
        def accept_loop():
            while True:
                try:
                    connection, _ = server.accept()
                except socket.timeout:
                    if done.is_set():
                        return
                    continue
                except OSError:
                    return
                connection.settimeout(None)
                worker = threading.Thread(target=relay, args=(connection,), daemon=True)
                worker.start()
                relays.append(worker)
        acceptor = threading.Thread(target=accept_loop, daemon=True)
        acceptor.start()
        env = dict(os.environ, DOT_AGENT_DECK_SOCKET=path)
        code = subprocess.call([{binary}] + sys.argv[1:], env=env)
        done.set()
        acceptor.join()
        for worker in relays:
            worker.join(5)
sys.exit(code)
"#,
        binary = serde_json::to_string(common::contained_deck_binary()).unwrap(),
        forwarded = serde_json::to_string(&forwarded).unwrap()
    );
    std::fs::write(&fixture.pin, script).unwrap();
    std::fs::set_permissions(&fixture.pin, std::fs::Permissions::from_mode(0o755)).unwrap();
    // The builder imports the same credential into its own HOME to register
    // recording redactions, while the overridden HOME is the one Claude uses.
    common::register_diagnostic_redactions(
        common::import_claude_credentials(&fixture.home).unwrap(),
    );
    // Credential import intentionally strips host hooks and replaces settings.
    // Restore the scenario's old pin after it, before either process starts.
    fixture.seed_hooks();
    let deck = TuiDeck::builder()
        .with_pty_size(300, 32)
        .with_imported_claude_credentials()
        .with_claude_trust_workdir()
        .with_env("HOME", fixture.home.to_string_lossy())
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");
    deck.wait_for_string("0.0.1");
    let initial = notices(deck.attach_socket_path());
    assert!(
        initial
            .iter()
            .any(|n| n.binary == fixture.pin.to_string_lossy()
                && n.reason == HookBinaryReason::Older),
        "startup pin: {initial:?}"
    );
    let cwd = deck.workdir().canonicalize().unwrap();
    common::register_diagnostic_redactions(
        common::seed_claude_project_trust(
            &fixture.home,
            &[
                cwd.to_string_lossy().into_owned(),
                deck.workdir().to_string_lossy().into_owned(),
            ],
        )
        .unwrap(),
    );
    const SENTINEL: &str = "hook_proof_1637_a81f.txt";
    const PANE: &str = "stale-live-pane";
    std::fs::write(cwd.join(SENTINEL), "hook proof fixture\n").unwrap();
    let events = deck.subscribe_events();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let client = DaemonClient::new(deck.attach_socket_path().to_path_buf());
    let id = runtime
        .block_on(client.start_agent(StartAgentOptions {
            command: Some(format!(
                "claude --model claude-haiku-4-5-20251001 --allowedTools Bash --settings '{}'",
                fixture.home.join(".claude/settings.json").display()
            )),
            cwd: Some(cwd.to_string_lossy().into_owned()),
            display_name: Some("old-hook-claude".into()),
            agent_type: Some(AgentType::ClaudeCode),
            env: vec![
                ("HOME".into(), fixture.home.to_string_lossy().into_owned()),
                (DOT_AGENT_DECK_PANE_ID.into(), PANE.into()),
                (
                    "DOT_AGENT_DECK_BIN".into(),
                    fixture.pin.to_string_lossy().into_owned(),
                ),
            ],
            rows: 22,
            cols: 140,
            ..Default::default()
        }))
        .expect("spawn interactive Claude");
    events.wait_for_session_start_on_pane(PANE, &id, Duration::from_secs(90));
    deck.wait_for_string("old-hook-claude");
    assert!(
        common::wait_until_panes_settled(
            deck.attach_socket_path(),
            std::slice::from_ref(&id),
            Duration::from_millis(1000),
            Duration::from_secs(5),
            Duration::from_secs(60)
        ),
        "Claude did not finish booting"
    );
    // PRD #1666: the task is posted to the agent's inbox, as the deck hands
    // an agent any task; nothing is typed into its pane.
    common::post_message_on(
        deck.attach_socket_path(),
        PANE,
        &id,
        "Use Bash now to run pwd, then sleep 3; ls -a in exactly the directory it prints. Print every filename verbatim, one per line. Do not ask questions or delegate; this is the complete task.",
    )
    .expect("post the real task");
    events.wait_for(
        |e| e.pane_id.as_deref() == Some(PANE) && e.event_type == EventType::ToolStart,
        Duration::from_secs(90),
    );
    let forwarded =
        std::fs::read_to_string(&forwarded).expect("the pinned wrapper forwarded real hooks");
    assert!(
        forwarded.contains("tool_start"),
        "wrapper did not forward Bash: {forwarded}"
    );
    let current = notices(deck.attach_socket_path());
    assert!(
        current
            .iter()
            .any(|n| n.binary == fixture.pin.to_string_lossy()
                && n.reason == HookBinaryReason::Unreported),
        "real hook notice in Hello: {current:?}"
    );
    assert!(
        deck.wait_for_grid_predicate_within(Duration::from_secs(30), |grid| grid
            .contains("Working")
            && grid.contains("predates version reporting")),
        "Working card and notice must share a frame:\n{}",
        deck.snapshot_grid()
    );
    events.wait_for(
        |e| e.pane_id.as_deref() == Some(PANE) && e.event_type == EventType::Idle,
        Duration::from_secs(90),
    );
    assert!(
        deck.wait_for_grid_predicate_within(Duration::from_secs(30), |grid| grid.contains("Idle")
            && grid.contains("predates version reporting")),
        "Idle card and notice must share a frame:\n{}",
        deck.snapshot_grid()
    );
    deck.send_keys(b"1");
    assert!(
        deck.wait_for_grid_or_pane_text_within(
            deck.attach_socket_path(),
            &id,
            SENTINEL,
            Duration::from_secs(30)
        ),
        "real agent did not list the fixture sentinel"
    );
    assert!(
        deck.wait_for_grid_string_within(SENTINEL, Duration::from_secs(30)),
        "the real agent's sentinel must be visible in its pane:\n{}",
        deck.snapshot_grid()
    );
    deck.send_keys(b"\x04"); // Ctrl+D: leave pane input for command mode.
    deck.send_keys(b"\x1a"); // Ctrl+Z: zoom the focused pane.
    assert!(
        deck.wait_for_grid_predicate_within(Duration::from_secs(10), |grid| {
            grid.contains("[Z]") && !grid.contains("predates version reporting")
        }),
        "the zoomed pane must show [Z] and hide the dashboard notice:\n{}",
        deck.snapshot_grid()
    );
    deck.send_keys(b"\x1a"); // Ctrl+Z again restores the dashboard layout.
    assert!(
        deck.wait_for_grid_predicate_within(Duration::from_secs(10), |grid| {
            !grid.contains("[Z]") && grid.contains("predates version reporting")
        }),
        "unzooming must restore the dashboard notice:\n{}",
        deck.snapshot_grid()
    );
    assert!(
        notices(deck.attach_socket_path())
            .iter()
            .any(|n| n.binary == fixture.pin.to_string_lossy()
                && n.reason == HookBinaryReason::Unreported)
    );
}
