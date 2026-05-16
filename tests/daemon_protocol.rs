//! Round-trip tests for the M1.2 streaming attach protocol. Each test spins
//! up an in-process attach server bound to a tempdir socket, drives it with
//! a UnixStream client, and verifies every message kind round-trips
//! correctly — including concurrent attach-stream subscribers.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::task::JoinHandle;

use dot_agent_deck::agent_pty::AgentPtyRegistry;
use dot_agent_deck::daemon_protocol::{
    AttachRequest, AttachResponse, KIND_DETACH, KIND_REQ, KIND_RESP, KIND_STREAM_END,
    KIND_STREAM_IN, KIND_STREAM_OUT, TabMembership, bind_attach_listener, serve_attach,
};

// ---------------------------------------------------------------------------
// Test harness
// ---------------------------------------------------------------------------

struct Server {
    _dir: TempDir,
    path: PathBuf,
    registry: Arc<AgentPtyRegistry>,
    handle: JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

// Serializes the tempdir-creation + bind sequence across parallel tests.
// `bind_attach_listener` -> `bind_socket` flips the process-global `umask` to
// 0o177 for the duration of `bind(2)`. If another test creates its tempdir
// inside that window, the new dir inherits 0o600 (no execute bit) and any
// later bind beneath it fails with EACCES. Holding this lock around both
// operations keeps the umask narrowing invisible to other tests' tempdir
// calls.
static HARNESS_BIND_LOCK: Mutex<()> = Mutex::new(());

async fn start_server() -> Server {
    let registry = Arc::new(AgentPtyRegistry::new());

    let (dir, path, listener) = {
        let _g = HARNESS_BIND_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("attach.sock");
        let listener = bind_attach_listener(&path).expect("bind attach listener");
        (dir, path, listener)
    };

    let registry_for_task = registry.clone();
    let handle = tokio::spawn(async move {
        let _ = serve_attach(listener, registry_for_task).await;
    });

    Server {
        _dir: dir,
        path,
        registry,
        handle,
    }
}

async fn write_frame(s: &mut UnixStream, kind: u8, payload: &[u8]) {
    let mut hdr = [0u8; 5];
    hdr[0] = kind;
    hdr[1..5].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    s.write_all(&hdr).await.unwrap();
    if !payload.is_empty() {
        s.write_all(payload).await.unwrap();
    }
}

async fn read_frame(s: &mut UnixStream) -> Option<(u8, Vec<u8>)> {
    let mut hdr = [0u8; 5];
    s.read_exact(&mut hdr).await.ok()?;
    let len = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
    let mut payload = vec![0u8; len];
    if len > 0 {
        s.read_exact(&mut payload).await.ok()?;
    }
    Some((hdr[0], payload))
}

async fn write_request(s: &mut UnixStream, req: &AttachRequest) {
    let payload = serde_json::to_vec(req).unwrap();
    write_frame(s, KIND_REQ, &payload).await;
}

async fn read_response(s: &mut UnixStream) -> AttachResponse {
    let (kind, payload) = read_frame(s).await.expect("expected RESP frame");
    assert_eq!(kind, KIND_RESP, "expected RESP, got 0x{kind:02x}");
    serde_json::from_slice(&payload).expect("RESP payload must be valid JSON")
}

async fn start_agent(server: &Server, command: &str) -> String {
    let mut s = UnixStream::connect(&server.path).await.unwrap();
    write_request(
        &mut s,
        &AttachRequest::StartAgent {
            command: Some(command.into()),
            cwd: None,
            display_name: None,
            rows: 24,
            cols: 80,
            env: vec![],
            tab_membership: None,
        },
    )
    .await;
    let resp = read_response(&mut s).await;
    assert!(resp.ok, "start-agent failed: {:?}", resp.error);
    resp.id.expect("start-agent response missing id")
}

async fn connect_attach(server: &Server, id: &str) -> UnixStream {
    let mut s = UnixStream::connect(&server.path).await.unwrap();
    write_request(&mut s, &AttachRequest::AttachStream { id: id.into() }).await;
    let resp = read_response(&mut s).await;
    assert!(resp.ok, "attach-stream failed: {:?}", resp.error);
    s
}

/// Read STREAM_OUT frames from `s` until the accumulated bytes contain
/// `marker`, or the timeout fires. Returns the accumulated bytes (only
/// useful for diagnostics on failure).
async fn read_until_contains(s: &mut UnixStream, marker: &[u8]) -> Vec<u8> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut acc: Vec<u8> = Vec::new();
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        match tokio::time::timeout(remaining, read_frame(s)).await {
            Ok(Some((KIND_STREAM_OUT, bytes))) => {
                acc.extend_from_slice(&bytes);
                if acc.windows(marker.len()).any(|w| w == marker) {
                    return acc;
                }
            }
            Ok(Some((KIND_STREAM_END, _))) => {
                panic!(
                    "stream ended before marker {:?} appeared; got {:?}",
                    String::from_utf8_lossy(marker),
                    String::from_utf8_lossy(&acc)
                );
            }
            Ok(Some(_)) => continue,
            Ok(None) => {
                panic!(
                    "connection closed before marker {:?}; got {:?}",
                    String::from_utf8_lossy(marker),
                    String::from_utf8_lossy(&acc)
                );
            }
            Err(_) => break,
        }
    }
    panic!(
        "timeout waiting for marker {:?}; got {:?}",
        String::from_utf8_lossy(marker),
        String::from_utf8_lossy(&acc)
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_agents_returns_empty_initially() {
    let server = start_server().await;
    let mut s = UnixStream::connect(&server.path).await.unwrap();
    write_request(&mut s, &AttachRequest::ListAgents).await;
    let resp = read_response(&mut s).await;
    assert!(resp.ok);
    assert_eq!(resp.agents.as_deref(), Some(&[][..] as &[String]));
}

// PRD #76 M2.12: `tab_membership` round-trips through the StartAgent →
// list_agents wire path so the TUI can rebuild mode/orchestration tabs
// on reconnect instead of stranding every hydrated pane on the
// dashboard. Two end-to-end paths: Mode tab and Orchestration tab.

async fn start_agent_with_membership(server: &Server, membership: TabMembership) -> String {
    let mut s = UnixStream::connect(&server.path).await.unwrap();
    write_request(
        &mut s,
        &AttachRequest::StartAgent {
            command: Some("sh -c 'sleep 30'".into()),
            cwd: Some("/tmp".into()),
            display_name: Some("auditor".into()),
            rows: 24,
            cols: 80,
            env: vec![],
            tab_membership: Some(membership),
        },
    )
    .await;
    let resp = read_response(&mut s).await;
    assert!(resp.ok, "start-agent failed: {:?}", resp.error);
    resp.id.expect("start-agent response missing id")
}

#[tokio::test]
async fn start_agent_with_mode_membership_round_trip() {
    let server = start_server().await;

    let id = start_agent_with_membership(
        &server,
        TabMembership::Mode {
            name: "k8s-ops".into(),
        },
    )
    .await;

    let records = server.registry.agent_records();
    let rec = records
        .iter()
        .find(|r| r.id == id)
        .expect("agent missing from list_agents");
    assert_eq!(
        rec.tab_membership,
        Some(TabMembership::Mode {
            name: "k8s-ops".into()
        })
    );
    server.registry.shutdown_all();
}

#[tokio::test]
async fn start_agent_with_orchestration_membership_round_trip() {
    let server = start_server().await;

    let id = start_agent_with_membership(
        &server,
        TabMembership::Orchestration {
            name: "tdd-cycle".into(),
            role_index: 2,
        },
    )
    .await;

    let records = server.registry.agent_records();
    let rec = records
        .iter()
        .find(|r| r.id == id)
        .expect("agent missing from list_agents");
    assert_eq!(
        rec.tab_membership,
        Some(TabMembership::Orchestration {
            name: "tdd-cycle".into(),
            role_index: 2,
        })
    );
    server.registry.shutdown_all();
}

// PRD #76 M2.15: spawn-time rows/cols are no longer the hardcoded 24/80
// VT100 default — the TUI computes the value from its real viewport.
// Pin the wire-format round-trip for explicit non-default values so a
// regression that re-hardcodes the field, or quietly substitutes the
// `default_rows`/`default_cols` defaults on the encode path, fails the
// build. The fields are old (M2.12 already used them implicitly via the
// 24/80 defaults), so the only thing changing in M2.15 is that callers
// finally pass real values — and those real values must survive
// `serde_json` round-trip on the daemon side.
#[tokio::test]
async fn start_agent_round_trips_explicit_rows_cols() {
    let server = start_server().await;

    let req = AttachRequest::StartAgent {
        command: Some("sh -c 'sleep 30'".into()),
        cwd: None,
        display_name: None,
        // Pick values that are obviously NOT the 24/80 defaults so a regression
        // that silently substitutes defaults shows up immediately. Choosing
        // 50/200 also exercises a cols value > 127 (catches accidental
        // narrowing to `i8` / `u7` on the wire).
        rows: 50,
        cols: 200,
        env: vec![],
        tab_membership: None,
    };

    // Wire round-trip: encode + decode via the same serde path the daemon
    // uses, and verify the values come back identical. This is the spec's
    // primary assertion — the daemon's handler reads these fields off the
    // decoded value, so if the round-trip is lossy the daemon would see
    // 24/80 (the serde defaults) and open the PTY at the wrong size.
    let json = serde_json::to_string(&req).unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["rows"], 50);
    assert_eq!(v["cols"], 200);
    let back: AttachRequest = serde_json::from_str(&json).unwrap();
    match back {
        AttachRequest::StartAgent { rows, cols, .. } => {
            assert_eq!(rows, 50);
            assert_eq!(cols, 200);
        }
        _ => panic!("wrong variant"),
    }

    // End-to-end sanity: the daemon accepts the non-default values
    // without error and the agent appears in `list_agents`. This catches
    // a regression where the server-side handler validates rows/cols
    // against a narrower range than the wire schema allows.
    let mut s = UnixStream::connect(&server.path).await.unwrap();
    write_request(&mut s, &req).await;
    let resp = read_response(&mut s).await;
    assert!(resp.ok, "start-agent failed: {:?}", resp.error);
    let id = resp.id.expect("start-agent response missing id");
    assert!(server.registry.agent_ids().contains(&id));
    server.registry.shutdown_all();
}

// PRD #76 M2.15 fixup F4 — forward-compat coverage for the older-client
// wire shape. The fields `rows` / `cols` carry `#[serde(default = ...)]`
// so an older client that omits them deserializes to the 24/80 VT100
// defaults rather than failing the decode. Without this test, an
// accidental removal of those serde attributes (or a rename of the
// `default_rows` / `default_cols` functions) would silently break the
// older-client → newer-daemon path: every reconnect from a stale binary
// would crash with a `missing field` decode error, even though the
// daemon's intent is to gracefully accept the legacy shape.
#[test]
fn start_agent_deserializes_old_client_shape_without_rows_cols() {
    // Hand-crafted JSON literal that mirrors what an older client (one
    // built before M2.15 added the rows/cols TUI plumbing) would have
    // serialized: the type tag, the legacy fields, no rows / no cols.
    // The literal is deliberately not built from a Rust struct so a
    // regression that drops `#[serde(default = ...)]` from the type
    // shows up here even if the encoding side is updated in lockstep.
    let json = r#"{
        "op": "start-agent",
        "command": "sh -c 'sleep 30'",
        "cwd": null,
        "display_name": null,
        "env": []
    }"#;

    let decoded: AttachRequest = serde_json::from_str(json).expect(
        "older client shape must deserialize via #[serde(default)] on rows/cols/tab_membership",
    );
    match decoded {
        AttachRequest::StartAgent {
            rows,
            cols,
            tab_membership,
            ..
        } => {
            assert_eq!(rows, 24, "missing `rows` must default to the VT100 24");
            assert_eq!(cols, 80, "missing `cols` must default to the VT100 80");
            assert!(
                tab_membership.is_none(),
                "missing `tab_membership` must default to None (dashboard pane)"
            );
        }
        other => panic!("expected StartAgent variant, got {other:?}"),
    }
}

#[tokio::test]
async fn start_agent_with_invalid_membership_name_is_rejected() {
    // M2.12 fixup reviewer #2: an invalid `tab_membership.name` (empty,
    // over-128-byte, or any of the control bytes the dashboard render
    // path renders as a span) must *reject* the spawn rather than
    // silently dropping the membership to `None`. Silent drop hid bad
    // spawn metadata behind a pane that looked dashboard-resident on
    // reconnect. Covers all five edge cases from `is_valid_display_name`.
    let server = start_server().await;

    let cases: &[(&str, String)] = &[
        ("empty", String::new()),
        ("129-byte", "a".repeat(129)),
        ("embedded \\n", "ok\nname".into()),
        ("embedded \\r", "ok\rname".into()),
        ("embedded \\0", "ok\0name".into()),
    ];

    for (label, name) in cases {
        let mut s = UnixStream::connect(&server.path).await.unwrap();
        write_request(
            &mut s,
            &AttachRequest::StartAgent {
                command: Some("sh -c 'sleep 30'".into()),
                cwd: Some("/tmp".into()),
                display_name: Some("auditor".into()),
                rows: 24,
                cols: 80,
                env: vec![],
                tab_membership: Some(TabMembership::Mode { name: name.clone() }),
            },
        )
        .await;
        let resp = read_response(&mut s).await;
        assert!(
            !resp.ok,
            "case {label}: spawn must fail for invalid tab_membership name, got ok response"
        );
        assert!(
            resp.id.is_none(),
            "case {label}: rejected spawn must not return an id"
        );
        assert!(
            resp.error
                .as_deref()
                .is_some_and(|e| e.contains("tab_membership")),
            "case {label}: error must mention tab_membership, got {:?}",
            resp.error
        );
    }

    // And the daemon must not have spawned anything across all five
    // cases — invalid metadata, no agent.
    assert!(
        server.registry.is_empty(),
        "no agents should be spawned when tab_membership validation rejects"
    );
    server.registry.shutdown_all();
}

#[tokio::test]
async fn start_list_stop_round_trip() {
    let server = start_server().await;

    let id = start_agent(&server, "/bin/sh").await;

    // list-agents sees the new id.
    let mut s = UnixStream::connect(&server.path).await.unwrap();
    write_request(&mut s, &AttachRequest::ListAgents).await;
    let resp = read_response(&mut s).await;
    assert_eq!(resp.agents, Some(vec![id.clone()]));

    // stop-agent succeeds and the registry is empty afterwards.
    let mut s = UnixStream::connect(&server.path).await.unwrap();
    write_request(&mut s, &AttachRequest::StopAgent { id: id.clone() }).await;
    let resp = read_response(&mut s).await;
    assert!(resp.ok);

    // Verify via the in-process registry handle.
    assert!(server.registry.is_empty());
}

#[tokio::test]
async fn stop_unknown_agent_returns_err() {
    let server = start_server().await;
    let mut s = UnixStream::connect(&server.path).await.unwrap();
    write_request(
        &mut s,
        &AttachRequest::StopAgent {
            id: "does-not-exist".into(),
        },
    )
    .await;
    let resp = read_response(&mut s).await;
    assert!(!resp.ok);
    assert!(resp.error.is_some());
}

#[tokio::test]
async fn malformed_request_returns_err() {
    let server = start_server().await;
    let mut s = UnixStream::connect(&server.path).await.unwrap();
    // Send a REQ frame with non-JSON payload.
    write_frame(&mut s, KIND_REQ, b"not json").await;
    let resp = read_response(&mut s).await;
    assert!(!resp.ok);
    assert!(
        resp.error
            .as_deref()
            .is_some_and(|e| e.contains("malformed request"))
    );
}

#[tokio::test]
async fn wrong_first_frame_kind_returns_err() {
    let server = start_server().await;
    let mut s = UnixStream::connect(&server.path).await.unwrap();
    // Server expects KIND_REQ as the first frame; sending STREAM_IN should
    // produce an error response.
    write_frame(&mut s, KIND_STREAM_IN, b"oops").await;
    let resp = read_response(&mut s).await;
    assert!(!resp.ok);
}

#[tokio::test]
async fn attach_stream_forwards_keystrokes_and_output() {
    let server = start_server().await;
    let id = start_agent(&server, "/bin/sh").await;

    let mut a = connect_attach(&server, &id).await;

    // Send a command and verify its output flows back as STREAM_OUT.
    write_frame(&mut a, KIND_STREAM_IN, b"echo HELLO-MARKER\n").await;
    read_until_contains(&mut a, b"HELLO-MARKER").await;

    // Cleanup: kill the agent.
    server.registry.close_agent(&id).unwrap();
}

#[tokio::test]
async fn two_attach_clients_both_receive_output() {
    let server = start_server().await;
    let id = start_agent(&server, "/bin/sh").await;

    let mut a = connect_attach(&server, &id).await;
    let mut b = connect_attach(&server, &id).await;

    // Client A drives the keystroke; both A and B must see the resulting
    // PTY output. This is the concurrent-attach property from PRD line 199.
    write_frame(&mut a, KIND_STREAM_IN, b"echo SHARED-MARKER\n").await;

    let (got_a, got_b) = tokio::join!(
        read_until_contains(&mut a, b"SHARED-MARKER"),
        read_until_contains(&mut b, b"SHARED-MARKER"),
    );
    assert!(!got_a.is_empty());
    assert!(!got_b.is_empty());

    server.registry.close_agent(&id).unwrap();
}

#[tokio::test]
async fn snapshot_returns_scrollback() {
    let server = start_server().await;
    let id = start_agent(&server, "/bin/sh").await;

    // Drive some output into scrollback via an attach session, then close
    // that session.
    {
        let mut a = connect_attach(&server, &id).await;
        write_frame(&mut a, KIND_STREAM_IN, b"echo SCROLL-MARKER\n").await;
        read_until_contains(&mut a, b"SCROLL-MARKER").await;
        write_frame(&mut a, KIND_DETACH, &[]).await;
    }

    // Give the daemon a moment to process the detach.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Snapshot should still contain the marker.
    let mut s = UnixStream::connect(&server.path).await.unwrap();
    write_request(&mut s, &AttachRequest::Snapshot { id: id.clone() }).await;
    let resp = read_response(&mut s).await;
    assert!(resp.ok);

    let mut snap: Vec<u8> = Vec::new();
    loop {
        match read_frame(&mut s).await {
            Some((KIND_STREAM_OUT, b)) => snap.extend_from_slice(&b),
            Some((KIND_STREAM_END, _)) => break,
            Some(_) => continue,
            None => break,
        }
    }
    assert!(
        String::from_utf8_lossy(&snap).contains("SCROLL-MARKER"),
        "snapshot should contain SCROLL-MARKER; got: {:?}",
        String::from_utf8_lossy(&snap)
    );

    server.registry.close_agent(&id).unwrap();
}

#[tokio::test]
async fn snapshot_unknown_agent_returns_err() {
    let server = start_server().await;
    let mut s = UnixStream::connect(&server.path).await.unwrap();
    write_request(&mut s, &AttachRequest::Snapshot { id: "nope".into() }).await;
    let resp = read_response(&mut s).await;
    assert!(!resp.ok);
}

#[tokio::test]
async fn detach_does_not_kill_agent() {
    let server = start_server().await;
    let id = start_agent(&server, "/bin/sh").await;

    let mut a = connect_attach(&server, &id).await;
    write_frame(&mut a, KIND_DETACH, &[]).await;
    drop(a);

    // The agent must still be in the registry.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut s = UnixStream::connect(&server.path).await.unwrap();
    write_request(&mut s, &AttachRequest::ListAgents).await;
    let resp = read_response(&mut s).await;
    assert_eq!(resp.agents, Some(vec![id.clone()]));

    server.registry.close_agent(&id).unwrap();
}

#[tokio::test]
async fn killing_agent_emits_stream_end_to_attached_clients() {
    let server = start_server().await;
    let id = start_agent(&server, "/bin/sh").await;

    let mut a = connect_attach(&server, &id).await;

    // Stop the agent via the protocol from a separate connection.
    let mut s = UnixStream::connect(&server.path).await.unwrap();
    write_request(&mut s, &AttachRequest::StopAgent { id: id.clone() }).await;
    let resp = read_response(&mut s).await;
    assert!(resp.ok);

    // The attached client must see STREAM_END (possibly preceded by some
    // residual STREAM_OUT bytes) within a reasonable timeout.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut saw_end = false;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        match tokio::time::timeout(remaining, read_frame(&mut a)).await {
            Ok(Some((KIND_STREAM_END, _))) => {
                saw_end = true;
                break;
            }
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(_) => break,
        }
    }
    assert!(
        saw_end,
        "attached client should observe STREAM_END after agent stop"
    );
}

#[tokio::test]
async fn attach_unknown_agent_returns_err() {
    let server = start_server().await;
    let mut s = UnixStream::connect(&server.path).await.unwrap();
    write_request(
        &mut s,
        &AttachRequest::AttachStream {
            id: "missing".into(),
        },
    )
    .await;
    let resp = read_response(&mut s).await;
    assert!(!resp.ok);
}

#[tokio::test]
async fn start_agent_rejects_blank_command() {
    // Whitespace-only `command` is treated as a client bug, not an attack
    // — see the trust-boundary doc on AttachRequest::StartAgent. The server
    // returns an error response without spawning anything.
    let server = start_server().await;
    let mut s = UnixStream::connect(&server.path).await.unwrap();
    write_request(
        &mut s,
        &AttachRequest::StartAgent {
            command: Some("   ".into()),
            cwd: None,
            display_name: None,
            rows: 24,
            cols: 80,
            env: vec![],
            tab_membership: None,
        },
    )
    .await;
    let resp = read_response(&mut s).await;
    assert!(!resp.ok);
    assert!(
        resp.error
            .as_deref()
            .is_some_and(|e| e.contains("empty") || e.contains("whitespace"))
    );
    assert!(server.registry.is_empty());
}

#[tokio::test]
async fn unknown_kind_during_stream_closes_connection() {
    let server = start_server().await;
    let id = start_agent(&server, "/bin/sh").await;
    let mut a = connect_attach(&server, &id).await;

    // Send a frame with an unknown kind. The server logs and closes the
    // connection — the output task is aborted as part of teardown, so the
    // client should observe EOF (read_frame returns None) within a bounded
    // time. We don't assert on STREAM_END here because the server's choice
    // is "close the connection", not "send STREAM_END" for protocol
    // violations.
    write_frame(&mut a, 0xEE, b"junk").await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut closed = false;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        match tokio::time::timeout(remaining, read_frame(&mut a)).await {
            Ok(None) => {
                closed = true;
                break;
            }
            Ok(Some(_)) => continue,
            Err(_) => break,
        }
    }
    assert!(
        closed,
        "connection should close after unknown frame kind on stream"
    );

    server.registry.close_agent(&id).unwrap();
}

#[tokio::test]
async fn concurrent_stream_in_writers_both_reach_pty() {
    // Two clients send STREAM_IN keystrokes concurrently. The shared PTY
    // writer is under a tokio Mutex, so individual `write_all` calls don't
    // interleave at sub-call granularity. Both inputs must reach the PTY
    // — verified by writing two distinct markers and observing both in the
    // PTY's echoed output (a third reader client receives the broadcast).
    let server = start_server().await;
    let id = start_agent(&server, "/bin/sh").await;

    let mut writer_a = connect_attach(&server, &id).await;
    let mut writer_b = connect_attach(&server, &id).await;
    let mut reader = connect_attach(&server, &id).await;

    // Drive both writers in parallel. Each sends an `echo` followed by a
    // newline so the shell evaluates and echoes the marker back.
    let send_a = async {
        write_frame(&mut writer_a, KIND_STREAM_IN, b"echo MARKER-AAA\n").await;
    };
    let send_b = async {
        write_frame(&mut writer_b, KIND_STREAM_IN, b"echo MARKER-BBB\n").await;
    };
    tokio::join!(send_a, send_b);

    // Both markers must appear in the reader's STREAM_OUT within a
    // reasonable timeout. read_until_contains polls until each marker
    // shows up; ordering between the two is not guaranteed.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut acc: Vec<u8> = Vec::new();
    let mut saw_a = false;
    let mut saw_b = false;
    while tokio::time::Instant::now() < deadline && !(saw_a && saw_b) {
        let remaining = deadline - tokio::time::Instant::now();
        match tokio::time::timeout(remaining, read_frame(&mut reader)).await {
            Ok(Some((KIND_STREAM_OUT, bytes))) => {
                acc.extend_from_slice(&bytes);
                if acc.windows(b"MARKER-AAA".len()).any(|w| w == b"MARKER-AAA") {
                    saw_a = true;
                }
                if acc.windows(b"MARKER-BBB".len()).any(|w| w == b"MARKER-BBB") {
                    saw_b = true;
                }
            }
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(_) => break,
        }
    }
    assert!(
        saw_a && saw_b,
        "both concurrent STREAM_IN writers must reach the PTY; saw_a={saw_a} saw_b={saw_b}, output: {:?}",
        String::from_utf8_lossy(&acc)
    );

    server.registry.close_agent(&id).unwrap();
}

/// Poll until the agent's broadcast subscriber count falls back to
/// `target` (typically 0), or panic after `budget`.
///
/// Used by the slow-client bounded-disconnect tests in place of reading
/// frames off the wedged socket. Reading would drain the kernel recv
/// buffer and let the server-side write make progress — which means the
/// test could pass via the broadcast-lag fallback path even when the
/// per-write timeout is missing. Observing receiver-count from the
/// server side instead pins the assertion strictly to "the wedged
/// client's attach handler dropped its `Receiver` within bounded time",
/// which only happens when the per-write timeout actually fires
/// (snapshot path) or the output-task per-write timeout fires
/// (live-output path).
async fn assert_receiver_count_reaches(
    registry: &AgentPtyRegistry,
    id: &str,
    target: usize,
    budget: Duration,
    what: &str,
) {
    let deadline = tokio::time::Instant::now() + budget;
    while tokio::time::Instant::now() < deadline {
        match registry.receiver_count(id) {
            Some(c) if c <= target => return,
            // Agent has been removed from the registry — there is no bus
            // anymore, so by definition no subscriber can be holding a
            // receiver. Treat as success.
            None => return,
            Some(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    let final_count = registry.receiver_count(id);
    panic!(
        "{what}: receiver_count did not drop to {target} within {budget:?} (final: {final_count:?})"
    );
}

#[tokio::test]
async fn slow_client_dropped_within_bounded_time() {
    // A wedged client (one that stops draining its socket) must be
    // disconnected by the daemon within bounded time, otherwise the
    // output task would pin forever on `write_all` and lag detection
    // could never fire. Two stages are exercised back-to-back in the
    // same test:
    //
    // 1. Live-output stage. Attach early; the live STREAM_OUT writes
    //    eventually back-pressure on the wedged client's recv buffer
    //    and hit `CLIENT_WRITE_TIMEOUT` in the output task.
    //
    // 2. Initial-snapshot stage. Attach after scrollback has filled to
    //    its cap, so the *first* STREAM_OUT carrying the snapshot is
    //    already large enough to overflow the kernel send buffer and
    //    must hit `CLIENT_WRITE_TIMEOUT` in the snapshot path itself.
    //
    // Both stages assert via the server-side broadcast receiver count
    // rather than by reading from the wedged client. Reading would
    // drain the kernel buffer and unblock the server's write, allowing
    // the test to pass via the broadcast-lag close path even if the
    // per-write timeout had been removed. Receiver-count observation
    // is strictly tied to the attach handler's `Receiver` being
    // dropped, which only happens when the bounded write actually
    // times out and the handler returns / the output task breaks.
    let server = start_server().await;
    let id = start_agent(&server, "yes AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA").await;

    // Stage 1: live-output. The slow client never reads after the
    // attach OK response — the kernel recv buffer fills as live
    // STREAM_OUT frames arrive, eventually wedging the output task's
    // bounded write.
    let _slow = connect_attach(&server, &id).await;
    assert_eq!(
        server.registry.receiver_count(&id),
        Some(1),
        "live-stage attach should register a subscriber"
    );
    // Budget is large enough to absorb CLIENT_WRITE_TIMEOUT (5s) plus
    // the time to fill the kernel send buffer at PTY-output rate.
    assert_receiver_count_reaches(
        &server.registry,
        &id,
        0,
        Duration::from_secs(20),
        "wedged live-stream slow client",
    )
    .await;

    // Stage 2: initial-snapshot. By now `yes` has filled scrollback to
    // its 1 MiB cap (well above any Unix-socket send buffer), so the
    // snapshot write itself is large enough to back-pressure on a
    // non-reading client. Open the connection raw so we can read the
    // attach OK response without then draining STREAM_OUT.
    let mut slow2 = UnixStream::connect(&server.path).await.unwrap();
    write_request(&mut slow2, &AttachRequest::AttachStream { id: id.clone() }).await;
    let resp = read_response(&mut slow2).await;
    assert!(resp.ok, "attach-stream failed: {:?}", resp.error);
    assert_eq!(
        server.registry.receiver_count(&id),
        Some(1),
        "snapshot-stage attach should register a subscriber"
    );
    assert_receiver_count_reaches(
        &server.registry,
        &id,
        0,
        Duration::from_secs(20),
        "wedged snapshot-stage slow client",
    )
    .await;

    server.registry.close_agent(&id).unwrap();
}

// ---------------------------------------------------------------------------
// M4.1 — disconnect/reconnect and partial-frame reassembly
// ---------------------------------------------------------------------------

#[tokio::test]
async fn abrupt_disconnect_keeps_agent_alive() {
    // Laptop-sleep / link-loss case: the client's socket dies without
    // sending KIND_DETACH. The daemon must distinguish EOF from explicit
    // detach, keep the agent alive, and serve scrollback to a fresh
    // connection.
    let server = start_server().await;
    let id = start_agent(&server, "/bin/sh").await;

    {
        let mut a = connect_attach(&server, &id).await;
        write_frame(&mut a, KIND_STREAM_IN, b"echo PRE-DROP\n").await;
        read_until_contains(&mut a, b"PRE-DROP").await;
        // Drop without DETACH — simulates abrupt socket loss.
    }

    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut s = UnixStream::connect(&server.path).await.unwrap();
    write_request(&mut s, &AttachRequest::ListAgents).await;
    let resp = read_response(&mut s).await;
    assert_eq!(
        resp.agents,
        Some(vec![id.clone()]),
        "agent must survive abrupt disconnect"
    );

    let mut s = UnixStream::connect(&server.path).await.unwrap();
    write_request(&mut s, &AttachRequest::Snapshot { id: id.clone() }).await;
    let resp = read_response(&mut s).await;
    assert!(resp.ok);

    let mut snap: Vec<u8> = Vec::new();
    loop {
        match read_frame(&mut s).await {
            Some((KIND_STREAM_OUT, b)) => snap.extend_from_slice(&b),
            Some((KIND_STREAM_END, _)) => break,
            Some(_) => continue,
            None => break,
        }
    }
    assert!(
        String::from_utf8_lossy(&snap).contains("PRE-DROP"),
        "snapshot after abrupt disconnect must contain PRE-DROP; got: {:?}",
        String::from_utf8_lossy(&snap)
    );

    server.registry.close_agent(&id).unwrap();
}

#[tokio::test]
async fn reattach_after_detach_sees_scrollback() {
    // Orchestrator-reconnect proof: scrollback survives an explicit
    // DETACH and is replayed as the initial-snapshot prelude on a
    // subsequent AttachStream.
    let server = start_server().await;
    let id = start_agent(&server, "/bin/sh").await;

    {
        let mut a = connect_attach(&server, &id).await;
        write_frame(&mut a, KIND_STREAM_IN, b"echo SCROLL-A\n").await;
        read_until_contains(&mut a, b"SCROLL-A").await;
        write_frame(&mut a, KIND_DETACH, &[]).await;
    }

    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut a = connect_attach(&server, &id).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut acc: Vec<u8> = Vec::new();
    let mut saw = false;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        match tokio::time::timeout(remaining, read_frame(&mut a)).await {
            Ok(Some((KIND_STREAM_OUT, bytes))) => {
                acc.extend_from_slice(&bytes);
                if acc.windows(b"SCROLL-A".len()).any(|w| w == b"SCROLL-A") {
                    saw = true;
                    break;
                }
            }
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(_) => break,
        }
    }
    assert!(
        saw,
        "reattach must replay SCROLL-A as snapshot prelude; got: {:?}",
        String::from_utf8_lossy(&acc)
    );

    server.registry.close_agent(&id).unwrap();
}

#[tokio::test]
async fn partial_frame_writes_reassemble() {
    // The wire format is `1-byte kind + 4-byte BE len + payload`. The
    // server must reassemble a frame whose bytes arrive split across
    // multiple writes (read_exact, not a single read). Fragment the
    // STREAM_IN frame into header / length / payload chunks with sleeps
    // between, and verify the PTY still receives the input.
    let server = start_server().await;
    let id = start_agent(&server, "/bin/sh").await;
    let mut a = connect_attach(&server, &id).await;

    let payload = b"echo PARTIAL-FRAME\n";
    let len_be = (payload.len() as u32).to_be_bytes();

    a.write_all(&[KIND_STREAM_IN]).await.unwrap();
    a.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    a.write_all(&len_be).await.unwrap();
    a.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    a.write_all(payload).await.unwrap();
    a.flush().await.unwrap();

    read_until_contains(&mut a, b"PARTIAL-FRAME").await;

    server.registry.close_agent(&id).unwrap();
}

#[tokio::test]
async fn resize_propagates_to_pty() {
    // Spawn at 24x80 (the harness default), resize to 50x200, then ask the
    // shell for its terminal size via `stty size`. The kernel sets that via
    // TIOCGWINSZ on the slave side, which is exactly what `MasterPty::resize`
    // updates on the master side — so a passing assertion proves the
    // protocol op reached the daemon's PTY.
    let server = start_server().await;
    let id = start_agent(&server, "/bin/sh").await;

    let mut s = UnixStream::connect(&server.path).await.unwrap();
    write_request(
        &mut s,
        &AttachRequest::Resize {
            id: id.clone(),
            rows: 50,
            cols: 200,
        },
    )
    .await;
    let resp = read_response(&mut s).await;
    assert!(resp.ok, "resize failed: {:?}", resp.error);

    // Use a command-substitution marker so the value `50 200` only appears
    // in the shell's *output*, not in the line echoed back from the input
    // (which contains the literal `$(stty size)` text). Reading until we
    // see `DIM=50 200` therefore proves the resize ioctl actually
    // propagated to the slave side of the PTY.
    let mut a = connect_attach(&server, &id).await;
    write_frame(&mut a, KIND_STREAM_IN, b"echo \"DIM=$(stty size)\"\n").await;
    let acc = read_until_contains(&mut a, b"DIM=50 200").await;
    let out = String::from_utf8_lossy(&acc);
    assert!(
        out.contains("DIM=50 200"),
        "expected 'DIM=50 200' from `stty size` after resize; got: {out:?}"
    );

    server.registry.close_agent(&id).unwrap();
}

#[tokio::test]
async fn resize_unknown_agent_returns_err() {
    let server = start_server().await;
    let mut s = UnixStream::connect(&server.path).await.unwrap();
    write_request(
        &mut s,
        &AttachRequest::Resize {
            id: "does-not-exist".into(),
            rows: 50,
            cols: 200,
        },
    )
    .await;
    let resp = read_response(&mut s).await;
    assert!(!resp.ok);
    assert!(resp.error.is_some());
}

#[tokio::test]
async fn resize_zero_rows_or_cols_returns_err() {
    // Guard against a buggy caller producing a 0x0 PTY: the daemon must
    // refuse zero rows/cols up front rather than silently passing them to
    // TIOCSWINSZ.
    let server = start_server().await;
    let id = start_agent(&server, "/bin/sh").await;

    for (rows, cols) in [(0u16, 80u16), (24u16, 0u16), (0u16, 0u16)] {
        let mut s = UnixStream::connect(&server.path).await.unwrap();
        write_request(
            &mut s,
            &AttachRequest::Resize {
                id: id.clone(),
                rows,
                cols,
            },
        )
        .await;
        let resp = read_response(&mut s).await;
        assert!(
            !resp.ok,
            "resize {rows}x{cols} should be rejected; got ok response"
        );
    }

    server.registry.close_agent(&id).unwrap();
}

/// Send an oversized resize and assert the kernel-visible size is clamped to
/// `PTY_RESIZE_DIM_MAX` rather than the requested value. Mirrors the
/// `resize_propagates_to_pty` shell-output proof — `stty size` reports the
/// slave-side TIOCGWINSZ, which is what `MasterPty::resize` updates.
async fn assert_resize_clamps(rows: u16, cols: u16, expected_rows: u16, expected_cols: u16) {
    let server = start_server().await;
    let id = start_agent(&server, "/bin/sh").await;

    let mut s = UnixStream::connect(&server.path).await.unwrap();
    write_request(
        &mut s,
        &AttachRequest::Resize {
            id: id.clone(),
            rows,
            cols,
        },
    )
    .await;
    let resp = read_response(&mut s).await;
    assert!(
        resp.ok,
        "oversized resize {rows}x{cols} must succeed (clamped); got error: {:?}",
        resp.error
    );

    let mut a = connect_attach(&server, &id).await;
    write_frame(&mut a, KIND_STREAM_IN, b"echo \"DIM=$(stty size)\"\n").await;
    let marker = format!("DIM={expected_rows} {expected_cols}");
    let acc = read_until_contains(&mut a, marker.as_bytes()).await;
    let out = String::from_utf8_lossy(&acc);
    assert!(
        out.contains(&marker),
        "expected clamp '{marker}' from `stty size` after resize {rows}x{cols}; got: {out:?}"
    );

    server.registry.close_agent(&id).unwrap();
}

#[tokio::test]
async fn registry_resize_clamps_oversized_rows() {
    assert_resize_clamps(10_000, 80, 4096, 80).await;
}

#[tokio::test]
async fn registry_resize_clamps_oversized_cols() {
    assert_resize_clamps(80, 10_000, 80, 4096).await;
}

#[tokio::test]
async fn registry_resize_clamps_both() {
    assert_resize_clamps(u16::MAX, u16::MAX, 4096, 4096).await;
}
