#![cfg(all(feature = "e2e", unix))]

//! Synthetic L2 coverage for history-only input delivery and visible feedback.

mod common;

use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::daemon_client::DaemonClient;
use dot_agent_deck::daemon_protocol::AttachRequest;
use dot_agent_deck::event::SendResult;
use serde_json::json;
use spec::spec;

#[cfg(unix)]
fn write_executable(path: &std::path::Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::write(path, contents).expect("write send-result recorder");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod send-result recorder");
}

/// Scenario: Launch a real dashboard with a long-lived synthetic pane, then
/// identify its Codex session as history-only through a synthetic hook event.
/// An unidentified atomic send must fail closed, while an identified send and
/// dashboard entry still report and visibly explain the history-only target.
#[spec("prompt/pane-input/004")]
#[test]
fn pane_input_004_history_only_send_reports_result_and_feedback() {
    let deck = TuiDeck::builder()
        .impersonating_pane_signals()
        .with_continue_session("history-codex", "cat")
        .launch_with_fixture("minimal");
    deck.wait_for_string("[Command Mode Ctrl+D]");
    deck.send_keys(b"\x04");

    let records = common::agent_records_on(deck.attach_socket_path());
    let record = records
        .iter()
        .find(|record| record.display_name.as_deref() == Some("history-codex"))
        .or_else(|| records.first())
        .expect("restored synthetic pane must have a daemon record");
    let pane_id = record
        .pane_id_env
        .clone()
        .expect("restored synthetic pane must have a daemon pane id");
    let agent_id = record.id.clone();
    let event = json!({
        "session_id": "history-codex-session",
        "agent_type": "codex",
        "event_type": "session_start",
        "timestamp": "2026-07-15T12:00:00Z",
        "pane_id": pane_id,
        "agent_id": agent_id,
        "live_target": {
            "kind": "process",
            "writable": "history-only"
        }
    });
    common::write_hook_line(deck.hook_socket_path(), &event.to_string())
        .expect("inject history-only Codex SessionStart");
    deck.wait_for_absence("No agent");

    let unidentified_response = common::attach_request_on(
        deck.attach_socket_path(),
        &AttachRequest::WriteAndSubmit {
            pane_id: pane_id.clone(),
            text: "this must not reach a history-only target".to_string(),
        },
    )
    .expect("send input request");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build identity-bearing send runtime");
    let identified_result = runtime
        .block_on(
            DaemonClient::new(deck.attach_socket_path().to_path_buf())
                .write_and_submit_with_identity(
                    &pane_id,
                    "this identified send must not reach a history-only target",
                    Some(&agent_id),
                    Some("history-codex-session"),
                    Some("history-only-identified-004"),
                ),
        )
        .expect("send identified input request");

    deck.send_keys(b"1");
    let feedback = "History-only session cannot accept live input";
    let feedback_visible = deck.wait_for_grid_string_within(feedback, Duration::from_secs(2));
    let grid = deck.snapshot_grid();

    assert_eq!(
        unidentified_response.send_result,
        Some(SendResult::NoLiveTarget),
        "an unidentified paned send must fail closed as no-live-target; feedback_visible={feedback_visible}\nFinal grid:\n{grid}"
    );
    assert_eq!(
        identified_result,
        SendResult::HistoryOnly,
        "an identified send must preserve the honest history-only result; feedback_visible={feedback_visible}\nFinal grid:\n{grid}"
    );
    assert!(
        feedback_visible,
        "the dashboard must surface `{feedback}` instead of entering PaneInput or silently dropping the send\nFinal grid:\n{grid}"
    );
    assert!(
        grid.contains("Codex"),
        "a rejected history-only send must not remove the dashboard card\nFinal grid:\n{grid}"
    );
}

/// Scenario: Keep a real dashboard focused in PaneInput while its attached
/// session transitions from live to history-only, then send a key and a paste
/// in separate runs. Each rejection must show feedback and leave PaneInput.
#[spec("prompt/pane-input/008")]
#[test]
fn pane_input_008_stream_rejection_surfaces_feedback_and_exits_input_mode() {
    let mut observations = Vec::new();
    for (input_kind, input) in [
        ("key", b"rejected-key".as_slice()),
        ("paste", b"\x1b[200~rejected-paste\x1b[201~".as_slice()),
    ] {
        let deck = TuiDeck::builder()
            .impersonating_pane_signals()
            .with_continue_session(format!("stream-rejection-{input_kind}"), "cat")
            .launch_with_fixture("minimal");
        deck.wait_for_string("[Command Mode Ctrl+D]");

        let record = common::agent_records_on(deck.attach_socket_path())
            .into_iter()
            .next()
            .expect("restored synthetic pane must have a daemon record");
        let pane_id = record
            .pane_id_env
            .expect("restored synthetic pane must have a daemon pane id");
        let event = json!({
            "session_id": format!("stream-rejection-{input_kind}-session"),
            "agent_type": "codex",
            "event_type": "session_start",
            "timestamp": "2026-07-15T12:00:00Z",
            "pane_id": pane_id,
            "agent_id": record.id,
            "live_target": {
                "kind": "process",
                "writable": "history-only"
            }
        });
        common::write_hook_line(deck.hook_socket_path(), &event.to_string())
            .expect("make focused synthetic session history-only");
        deck.wait_for_absence("No agent");

        deck.send_keys(input);
        let feedback = deck.wait_for_grid_string_within(
            "History-only session cannot accept live input",
            Duration::from_secs(2),
        );
        let grid = deck.snapshot_grid();
        observations.push((
            input_kind,
            feedback,
            !grid.contains("[Command Mode Ctrl+D]"),
            grid,
        ));
    }

    assert!(
        observations
            .iter()
            .all(|(_, feedback, exited, _)| *feedback && *exited),
        "key and paste rejection must both show feedback and exit PaneInput; observations={observations:#?}"
    );
}

/// Scenario: Open an orchestration whose start role declares itself
/// history-only, let the real spawn-time prompt action receive that non-applied
/// result, then transition the same role to live. The UI must show feedback,
/// avoid marking the role Working, retain the prompt, and deliver it after live.
#[spec("prompt/pane-input/007")]
#[test]
#[cfg(unix)]
fn pane_input_007_orchestrator_prompt_retries_after_non_applied_result() {
    const MARKER: &str = "ORCHESTRATORRESULTMARKER20";
    const DELIVERED_POINTER: &str = "Read .dot-agent-deck/orchestrator-context";
    let deck = TuiDeck::launch_with_fixture("send-result-orchestration");
    deck.wait_for_string("No active agents");
    let script = deck.workdir().join("orchestrator-send-result.sh");
    write_executable(
        &script,
        r#"#!/bin/sh
emit_target() {
    WRITABLE="$1" python3 - <<'PY'
import datetime
import json
import os
import socket

pane = os.environ["DOT_AGENT_DECK_PANE_ID"]
payload = {
    "session_id": "orchestrator-send-result-session",
    "agent_type": "codex",
    "event_type": "session_start",
    "timestamp": datetime.datetime.now(datetime.timezone.utc).isoformat(),
    "pane_id": pane,
    "agent_id": os.environ.get("DOT_AGENT_DECK_AGENT_ID"),
    "token": os.environ.get("DOT_AGENT_DECK_PANE_CAPABILITY"),
    "live_target": {
        "kind": "pty" if os.environ["WRITABLE"] == "live" else "process",
        "writable": os.environ["WRITABLE"],
    },
}
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(os.environ["DOT_AGENT_DECK_SOCKET"])
s.sendall((json.dumps(payload) + "\n").encode())
s.close()
PY
}

emit_target history-only
while [ ! -f allow-live-target ]; do sleep 0.05; done
emit_target live
while IFS= read -r line; do printf '%s\n' "$line" >> orchestrator-prompt.log; done
"#,
    );

    deck.send_keys(b"\x0e");
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" ");
    deck.wait_for_string("No mode");
    deck.send_keys(b"\x1b[C");
    deck.wait_for_absence("Command:");
    deck.send_keys(b"\r");
    deck.send_keys(b"\r");

    // 30s rather than 5s. This wait covers a chain that spawns the pane, runs
    // the stand-in through a `python3` interpreter, round-trips a
    // `session_start` over the daemon socket and repaints — and it fails as a
    // unit when the runner stalls. Measured 2026-09-07: this test took 20.9s on
    // a `ubuntu-latest` runner against 1.54s on the same tier's green run of
    // `main` (13.6x its own CI-normal), expiring this budget AND the delivery
    // budget below before asserting. That is NOT a slower runner: across the
    // 2653 tests the two reports share, the median CI/local per-test ratio is
    // 0.96x, and this test is faster on a healthy runner (1.54s) than locally
    // (1.94s). It is an intermittent stall, and 30s — the modal
    // `wait_for_grid_string_within` budget in this suite, 19 of 61 uses — rides
    // it out. The wait returns as soon as the string paints, so a healthy run
    // pays nothing for the headroom; only a genuine failure waits longer.
    let feedback = deck.wait_for_grid_string_within(
        "History-only session cannot accept live input",
        Duration::from_secs(30),
    );
    let marked_working = deck.snapshot_grid().contains("Working");
    std::fs::write(deck.workdir().join("allow-live-target"), "")
        .expect("allow synthetic role to become live");
    // 15s rather than 10s, for the same stall: the run above expired this
    // budget too, so widening only the feedback wait would not have saved it.
    // 15s is the top of the range this helper already uses across the suite
    // (10s in 13 places, 15s in 10).
    let delivered = common::wait_for_file_substr_count(
        &deck.workdir().join("orchestrator-prompt.log"),
        DELIVERED_POINTER,
        1,
        Duration::from_secs(15),
    );
    let context = std::fs::read_to_string(
        deck.workdir()
            .join(".dot-agent-deck/orchestrator-context.md"),
    )
    .expect("read generated orchestrator context");
    let grid = deck.snapshot_grid();

    assert!(
        feedback,
        "the orchestrator prompt's HistoryOnly result must surface visible feedback\nFinal grid:\n{grid}"
    );
    assert!(
        !marked_working,
        "a role whose prompt was not delivered must not be marked Working\nFinal grid:\n{grid}"
    );
    assert!(
        delivered,
        "the non-delivered orchestrator prompt must be retained and retried after the role becomes live\nFinal grid:\n{grid}"
    );
    assert!(
        context.contains(MARKER),
        "the delivered context pointer must reference the generated context containing the role prompt"
    );
}

/// Scenario: Launch a real dashboard with two panes — one whose program puts
/// its terminal in raw mode and then never reads, one plain `cat` — and send the
/// stuck pane a prompt far larger than its terminal will hold. The send must
/// come back as possibly delivered rather than hang, its card must show the
/// error, and a send to the other pane made while the stuck one is pending must
/// still go straight through.
#[cfg(target_os = "linux")]
#[spec("prompt/pane-input/046")]
#[test]
fn prompt_pane_input_046_a_pane_that_stops_reading_does_not_hang_its_send_or_stall_the_deck() {
    const SENTINEL: &str = "HEALTHY-PANE-SENTINEL-046";
    const HEALTHY_PANE: &str = "healthy-046";
    let deck = TuiDeck::builder()
        .impersonating_pane_signals()
        .with_continue_session(
            "wedged-046",
            "sh -c 'stty raw; printf WEDGE-READY; exec sleep 600'",
        )
        .launch_with_fixture("minimal");
    deck.wait_for_string("[Command Mode Ctrl+D]");
    assert!(
        common::wait_until(Duration::from_secs(15), || {
            common::agent_records_on(deck.attach_socket_path())
                .iter()
                .any(|record| record.display_name.as_deref() == Some("wedged-046"))
        }),
        "precondition: the stuck pane was restored"
    );
    let wedged = common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .find(|record| record.display_name.as_deref() == Some("wedged-046"))
        .expect("the stuck pane's daemon record");
    // A session on the stuck pane, so its card shows a status rather than
    // "No agent" — the error is reported as that status.
    let wedged_pane = wedged
        .pane_id_env
        .clone()
        .expect("the stuck pane's pane id");
    let session = json!({
        "session_id": "wedged-046-session",
        "agent_type": "claude_code",
        "event_type": "session_start",
        "timestamp": "2026-10-03T12:00:00Z",
        "pane_id": wedged_pane,
        "agent_id": wedged.id,
    });
    common::write_hook_line(deck.hook_socket_path(), &session.to_string())
        .expect("inject the stuck pane's SessionStart");
    deck.wait_until_grid("the stuck pane's card names its agent", |grid| {
        grid.contains("Claude")
    });
    // The second pane is started on the same daemon directly: it only has to
    // take input, and the dashboard restores one session per launch.
    let started_healthy = common::attach_request_on(
        deck.attach_socket_path(),
        &AttachRequest::StartAgent {
            command: Some("cat".to_string()),
            cwd: None,
            display_name: Some(HEALTHY_PANE.to_string()),
            rows: 24,
            cols: 80,
            env: vec![(
                dot_agent_deck::agent_pty::DOT_AGENT_DECK_PANE_ID.to_string(),
                HEALTHY_PANE.to_string(),
            )],
            tab_membership: None,
            agent_type: None,
            seed: None,
            authoring_kind: None,
            client_seeded_kind: None,
            remember_command: false,
        },
    )
    .expect("start the healthy pane");
    let healthy_id = started_healthy
        .id
        .clone()
        .unwrap_or_else(|| panic!("start-agent returned no id: {started_healthy:?}"));
    assert!(
        common::wait_until(Duration::from_secs(10), || {
            String::from_utf8_lossy(&common::pane_snapshot_on(
                deck.attach_socket_path(),
                &wedged.id,
            ))
            .contains("WEDGE-READY")
        }),
        "precondition: the stuck pane's program never put its terminal in raw mode"
    );
    assert!(
        !deck.snapshot_grid().contains("Error"),
        "precondition: no card shows an error yet\n{}",
        deck.snapshot_grid()
    );

    // A single line far larger than the terminal's input queue, so the write
    // stops part-way with the pane not reading.
    let flood = "x".repeat(200_000);
    let socket = deck.attach_socket_path().to_path_buf();
    let wedged_agent = wedged.id.clone();
    let started = std::time::Instant::now();
    let (stuck_tx, stuck_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build stuck-send runtime")
            .block_on(DaemonClient::new(socket).write_and_submit_with_identity(
                &wedged_pane,
                &flood,
                Some(&wedged_agent),
                Some("wedged-046-session"),
                Some("wedged-send-046"),
            ));
        let _ = stuck_tx.send((result, started.elapsed()));
    });

    // While that send is stuck on the wedged pane, the other pane still takes
    // input at once. The stuck pane's terminal echoes what it receives (raw
    // mode, echo left on), so its first bytes showing is the observable that
    // the send has reached the PTY — and the send cannot finish until the
    // queue behind them would take all 200 KB.
    assert!(
        common::wait_until(Duration::from_secs(10), || {
            String::from_utf8_lossy(&common::pane_snapshot_on(
                deck.attach_socket_path(),
                &wedged.id,
            ))
            .contains(&"x".repeat(256))
        }),
        "precondition: the stuck send never reached the stuck pane's PTY"
    );
    let healthy_started = std::time::Instant::now();
    let healthy_result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build healthy-send runtime")
        .block_on(async {
            tokio::time::timeout(
                Duration::from_secs(10),
                DaemonClient::new(deck.attach_socket_path().to_path_buf())
                    .write_and_submit_with_identity(
                        HEALTHY_PANE,
                        SENTINEL,
                        Some(&healthy_id),
                        None,
                        Some("healthy-send-046"),
                    ),
            )
            .await
        })
        .expect("the healthy pane's send did not come back within 10 s")
        .expect("send to the healthy pane");
    let healthy_took = healthy_started.elapsed();
    assert_eq!(healthy_result, SendResult::Applied);
    if let Ok(early) = stuck_rx.try_recv() {
        panic!(
            "precondition: the healthy send was made while the stuck one was still pending; it \
             had already returned {early:?}"
        );
    }
    assert!(
        healthy_took < Duration::from_secs(8),
        "the healthy pane's send waited {healthy_took:?} behind the stuck pane"
    );
    assert!(
        common::wait_until(Duration::from_secs(5), || {
            String::from_utf8_lossy(&common::pane_snapshot_on(
                deck.attach_socket_path(),
                &healthy_id,
            ))
            .contains(SENTINEL)
        }),
        "the healthy pane never showed its input"
    );

    let (stuck_result, stuck_took) = stuck_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the send into the pane that stopped reading never came back");
    assert_eq!(
        stuck_result.expect("the stuck send returns a result, not a transport error"),
        SendResult::Ambiguous,
        "part of the prompt went to a pane that stopped reading, so it may have been delivered \
         (it came back after {stuck_took:?})"
    );
    assert!(
        deck.wait_for_grid_string_within("Error", Duration::from_secs(10)),
        "the stuck pane's card never showed the error\n{}",
        deck.snapshot_grid()
    );
}
