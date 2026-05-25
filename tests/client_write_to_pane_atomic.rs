//! PRD #100 M3.1 — regression test for the client-initiated send-prompt
//! atomicity contract.
//!
//! Before the fix (PRD #100 Phase 2), the TUI's
//! `EmbeddedPaneController::write_to_pane` queued two STREAM_IN frames
//! for the per-pane I/O task — the payload, then `\r` after a 150 ms
//! client-side `std::thread::sleep`. The per-agent writer mutex on the
//! daemon side was released between those two frames, so a concurrent
//! daemon-initiated write (orchestration delegate / work-done feedback
//! / respawn notice) could land its own payload + CR in the 150 ms gap.
//! Master byte order in that case:
//!   `[user payload][daemon payload][daemon \r][user \r]`
//! After ICRNL on input and canonical line buffering, the slave saw
//! one fused line `user-payload + daemon-payload\n` plus a trailing
//! empty line. Cat then emitted a single combined `<fused>\r\n` on its
//! stdout — exactly the "Enter inserted a newline" bug surface.
//!
//! After the fix, `EmbeddedPaneController::write_to_pane` issues a
//! single `WriteAndSubmit` RPC. The daemon runs the full payload +
//! `SUBMIT_DELAY` + CR sequence under the per-agent writer mutex —
//! the same atomic contract orchestration dispatch already had (PRD
//! #93 round-8). Concurrent daemon-initiated writes block on the
//! same mutex; the slave receives each canonical line cleanly and
//! cat emits two distinct `<payload>\r\n` substrings.
//!
//! Assertion mirrors `write_to_pane_and_submit_serializes_concurrent_writes_per_pane`
//! in `tests/orchestration_delegate.rs`: each payload's `\r\n`-suffixed
//! form must appear in the scrollback as a contiguous substring. A
//! fused write at the PTY-master level would collapse both payloads
//! into one canonical line and neither `<payload>\r\n` would
//! individually appear. The PTY's local echo can still interleave
//! payload bytes on the master independently of cat's stdout writes;
//! anchoring on cat's `<payload>\r\n` writes is the only assertion
//! that's robust to that master-side rendering race.
//!
//! Verified by toggling: temporarily restoring the old
//! `EmbeddedPaneController::write_to_pane` body (queue STREAM_IN
//! payload, `std::thread::sleep(SUBMIT_DELAY)`, queue STREAM_IN `\r`)
//! makes this test fail with `USERMSG-PAYLOAD\r\n missing from
//! scrollback`. Restoring the RPC path makes it pass.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use tempfile::TempDir;
use tokio::task::JoinHandle;

use dot_agent_deck::agent_pty::{
    AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, TabMembership, is_valid_pane_id_env,
};
use dot_agent_deck::daemon_client::{DaemonClient, StartAgentOptions};
use dot_agent_deck::daemon_protocol::{bind_attach_listener, serve_attach};
use dot_agent_deck::embedded_pane::EmbeddedPaneController;
use dot_agent_deck::pane::PaneController;

static HARNESS_BIND_LOCK: Mutex<()> = Mutex::new(());

struct Server {
    _dir: TempDir,
    path: PathBuf,
    registry: Arc<AgentPtyRegistry>,
    handle: JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.handle.abort();
        self.registry.shutdown_all();
    }
}

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
    let (event_tx, _) = tokio::sync::broadcast::channel(16);
    let handle = tokio::spawn(async move {
        let _ = serve_attach(listener, registry_for_task, event_tx).await;
    });

    Server {
        _dir: dir,
        path,
        registry,
        handle,
    }
}

async fn start_cat_pane(server: &Server, pane_id: &str) -> String {
    assert!(is_valid_pane_id_env(pane_id));
    let client = DaemonClient::new(server.path.clone());
    let cwd = std::env::temp_dir().to_string_lossy().into_owned();
    client
        .start_agent(StartAgentOptions {
            command: Some("cat -u".to_string()),
            cwd: Some(cwd),
            display_name: Some(pane_id.to_string()),
            rows: 24,
            cols: 80,
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane_id.to_string())],
            tab_membership: Some(TabMembership::Orchestration {
                name: "atomic".to_string(),
                role_index: 0,
                role_name: pane_id.to_string(),
                is_start_role: false,
                orchestration_cwd: None,
            }),
            agent_type: None,
        })
        .await
        .expect("start_agent")
}

async fn wait_for_in_snapshot(
    registry: &AgentPtyRegistry,
    agent_id: &str,
    needle: &[u8],
    timeout: Duration,
) -> Option<Vec<u8>> {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if let Ok(snap) = registry.snapshot(agent_id)
            && snap.windows(needle.len()).any(|w| w == needle)
        {
            return Some(snap);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    None
}

/// Race `EmbeddedPaneController::write_to_pane` (the user-facing
/// send-prompt path) against a concurrent `AgentPtyRegistry::write_to_pane_and_submit`
/// scheduled to land in the user write's 150 ms `SUBMIT_DELAY` window.
/// With the pre-fix controller (two STREAM_IN frames + client-side
/// sleep) the daemon-initiated write lands between the user's payload
/// and CR; the slave sees one fused canonical line and cat emits a
/// single combined `<fused>\r\n`. With the post-fix controller
/// (single `WriteAndSubmit` RPC routed through the same per-agent
/// writer mutex orchestration dispatch uses) the user's payload+CR is
/// atomic; the daemon-initiated write waits its turn and cat emits
/// two distinct `<payload>\r\n` substrings.
///
/// Multi-thread runtime: the controller's `write_to_pane` is sync and
/// uses `runtime.block_on(...)` to drive the `WriteAndSubmit` RPC. On
/// a current-thread runtime that `block_on` would deadlock the test's
/// only worker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn controller_write_to_pane_is_atomic_against_concurrent_daemon_write() {
    let server = start_server().await;
    let agent_id = start_cat_pane(&server, "atomic-pane").await;

    let ctrl = Arc::new(EmbeddedPaneController::new(
        server.path.clone(),
        tokio::runtime::Handle::current(),
    ));

    // Register the daemon-started agent in the controller's pane map
    // via hydrate. Without this the pre-fix `write_to_pane` body —
    // which looks the pane up under the registry's `panes` mutex —
    // would error with "Pane not found" before the race could play
    // out and the toggle-verification step would fail trivially.
    // Hydrate reuses the agent's `pane_id_env` as the local pane id
    // when valid (so "atomic-pane" is the same on both sides), so
    // the test's `write_to_pane("atomic-pane", ...)` calls land on
    // the same agent that `registry.write_to_pane_and_submit` does.
    let ctrl_for_hydrate = ctrl.clone();
    let hydrated = tokio::task::spawn_blocking(move || ctrl_for_hydrate.hydrate_from_daemon())
        .await
        .unwrap();
    assert_eq!(
        hydrated.len(),
        1,
        "expected one hydrated pane; got {hydrated:?}"
    );
    assert_eq!(
        hydrated[0].pane_id, "atomic-pane",
        "hydration should reuse the agent's pane_id_env as the local pane id"
    );

    // Order matters for reproducing the PRD #100 race against the
    // *pre-fix* controller: the user write must reach the writer
    // mutex FIRST so its payload lands while the mutex is briefly
    // released; then the daemon-initiated write fires during the
    // user's 150 ms client-side sleep gap. With the *post-fix*
    // controller the user's call goes through a single RPC that
    // holds the mutex across payload + sleep + CR, so the
    // daemon-initiated write blocks on the mutex instead of
    // interleaving.
    let ctrl_for_user = ctrl.clone();
    let user_task = tokio::task::spawn_blocking(move || {
        ctrl_for_user.write_to_pane("atomic-pane", "USERMSG-PAYLOAD")
    });

    // Brief delay to nudge the daemon-initiated write into the
    // user's `SUBMIT_DELAY` window. The pre-fix controller's
    // STREAM_IN(payload) frame arrives ~immediately at the daemon,
    // releases the mutex, and starts sleeping; this delay puts the
    // daemon-initiated `write_to_pane_and_submit` squarely inside
    // that sleep.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let registry_for_task = server.registry.clone();
    let daemon_task = tokio::spawn(async move {
        registry_for_task
            .write_to_pane_and_submit("atomic-pane", "BGWRITER-MSG")
            .await
    });

    user_task.await.unwrap().expect("controller write");
    daemon_task.await.unwrap().unwrap();

    // Atomicity invariant: cat -u outputs each canonical line as one
    // atomic `<payload>\r\n` write. If the per-pane writer mutex
    // serialized the two writes correctly, the slave saw two
    // distinct canonical lines and cat emitted two distinct
    // `<payload>\r\n` substrings. A fused master-side byte stream
    // (PRD #100 bug surface — `user_payload + bg_payload\r`) would
    // collapse into ONE combined canonical line, and neither
    // `USERMSG-PAYLOAD\r\n` nor `BGWRITER-MSG\r\n` would appear on
    // its own.
    let user_needle: &[u8] = b"USERMSG-PAYLOAD\r\n";
    let bg_needle: &[u8] = b"BGWRITER-MSG\r\n";

    let snap = wait_for_in_snapshot(
        &server.registry,
        &agent_id,
        user_needle,
        Duration::from_secs(5),
    )
    .await
    .unwrap_or_else(|| {
        let last = server.registry.snapshot(&agent_id).unwrap_or_default();
        panic!(
            "PRD #100 regression: USERMSG-PAYLOAD\\r\\n missing from \
                 scrollback — the user's payload and CR were not atomic, \
                 so the slave fused them with BGWRITER-MSG into one \
                 canonical line. Last snapshot: {:?}",
            String::from_utf8_lossy(&last)
        )
    });
    assert!(
        snap.windows(bg_needle.len()).any(|w| w == bg_needle)
            || wait_for_in_snapshot(
                &server.registry,
                &agent_id,
                bg_needle,
                Duration::from_secs(5)
            )
            .await
            .is_some(),
        "PRD #100 regression: BGWRITER-MSG\\r\\n missing from scrollback \
         — the orchestration-side write was not atomic against the \
         concurrent user write. Last snapshot: {:?}",
        String::from_utf8_lossy(&snap)
    );

    drop(ctrl);
    let _ = server.registry.close_agent(&agent_id);
}

/// PRD #100 audit #6: race ≥2 parallel `EmbeddedPaneController::write_to_pane`
/// calls (each going through the new `WriteAndSubmit` RPC) against
/// the same pane. The original M3.1 test pinned client-vs-daemon
/// serialization through the writer mutex; this test pins
/// client-vs-client serialization through the SAME mutex. Asymmetry
/// between the two surfaces would mean the RPC handler unlocked
/// something it shouldn't have, so even though the mechanism is
/// shared (`AgentPtyRegistry::write_to_pane_and_submit`), the
/// RPC-only path deserves its own coverage.
///
/// Assertion: each of the three concurrent payloads must surface as
/// its own `<payload>\r\n` substring in cat's scrollback. Fused
/// writes at the PTY master would collapse two or more payloads
/// into a single canonical line and cat would emit one combined
/// `<a><b>\r\n` instead of two distinct `<a>\r\n` + `<b>\r\n`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_controller_write_to_pane_serializes_per_pane() {
    let server = start_server().await;
    let agent_id = start_cat_pane(&server, "multi-client-pane").await;

    let ctrl = Arc::new(EmbeddedPaneController::new(
        server.path.clone(),
        tokio::runtime::Handle::current(),
    ));

    let ctrl_for_hydrate = ctrl.clone();
    let hydrated = tokio::task::spawn_blocking(move || ctrl_for_hydrate.hydrate_from_daemon())
        .await
        .unwrap();
    assert_eq!(hydrated.len(), 1);
    assert_eq!(hydrated[0].pane_id, "multi-client-pane");

    let payloads = ["CLIENTA-MSG", "CLIENTB-MSG", "CLIENTC-MSG"];
    let mut joins = Vec::new();
    for payload in payloads {
        let ctrl = ctrl.clone();
        joins.push(tokio::task::spawn_blocking(move || {
            ctrl.write_to_pane("multi-client-pane", payload)
        }));
    }
    for j in joins {
        j.await.unwrap().expect("controller write");
    }

    // Every payload must surface as its own `<payload>\r\n` chunk
    // from cat. Same atomicity-via-canonical-line shape as the
    // existing `write_to_pane_and_submit_serializes_concurrent_writes_per_pane`
    // in `tests/orchestration_delegate.rs`.
    for payload in payloads {
        let needle: Vec<u8> = format!("{payload}\r\n").into_bytes();
        let snap =
            wait_for_in_snapshot(&server.registry, &agent_id, &needle, Duration::from_secs(5))
                .await
                .unwrap_or_else(|| {
                    let last = server.registry.snapshot(&agent_id).unwrap_or_default();
                    panic!(
                        "concurrent-write regression: {payload}\\r\\n missing \
                     from scrollback — the RPC path fused writes from \
                     multiple clients into a single canonical line. \
                     Snapshot: {:?}",
                        String::from_utf8_lossy(&last)
                    )
                });
        assert!(
            snap.windows(needle.len()).any(|w| w == needle.as_slice()),
            "expected {payload}\\r\\n in scrollback: {:?}",
            String::from_utf8_lossy(&snap)
        );
    }

    drop(ctrl);
    let _ = server.registry.close_agent(&agent_id);
}

/// PRD #100 reviewer 'minor gap': mirror of the headline atomicity
/// test, but the concurrent daemon-initiated writer is a
/// `write_to_pane_notice` (LF terminator, no submit) instead of a
/// `write_to_pane_and_submit` (CR terminator). Both go through
/// `write_to_pane_internal` and the same per-agent writer mutex, so
/// the serialization invariant holds — but the LF-terminator path is
/// what hypothesis #2 of the original PRD specifically called out as
/// the bug surface, and it's worth pinning that the notice path also
/// can't sneak its LF in between the user's payload and CR.
///
/// Assertion: the user's `USERMSG-PAYLOAD\r\n` survives as a contiguous
/// substring even with a concurrent notice landing during the user
/// write's `SUBMIT_DELAY` window. A pre-fix-style notice landing in
/// the gap would let the slave see `user-payload + notice\n` followed
/// by the user's CR, collapsing them into one canonical line —
/// `USERMSG-PAYLOAD\r\n` would not appear.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn controller_write_to_pane_is_atomic_against_concurrent_notice() {
    let server = start_server().await;
    let agent_id = start_cat_pane(&server, "notice-race-pane").await;

    let ctrl = Arc::new(EmbeddedPaneController::new(
        server.path.clone(),
        tokio::runtime::Handle::current(),
    ));

    let ctrl_for_hydrate = ctrl.clone();
    let hydrated = tokio::task::spawn_blocking(move || ctrl_for_hydrate.hydrate_from_daemon())
        .await
        .unwrap();
    assert_eq!(hydrated.len(), 1);
    assert_eq!(hydrated[0].pane_id, "notice-race-pane");

    let ctrl_for_user = ctrl.clone();
    let user_task = tokio::task::spawn_blocking(move || {
        ctrl_for_user.write_to_pane("notice-race-pane", "USERMSG-PAYLOAD")
    });

    // 50 ms head start on the user write puts the notice squarely
    // inside the user write's `SUBMIT_DELAY` window — the original
    // bug scenario for hypothesis #2.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let registry_for_task = server.registry.clone();
    let notice_task = tokio::spawn(async move {
        registry_for_task
            .write_to_pane_notice("notice-race-pane", "NOTICE-MARKER")
            .await
    });

    user_task.await.unwrap().expect("controller write");
    notice_task.await.unwrap().unwrap();

    // The user's payload must surface as `USERMSG-PAYLOAD\r\n` — its
    // own canonical line, not fused with the notice. The notice
    // terminator is LF (no CR), so the notice text itself doesn't
    // close a canonical line on its own; what we're guarding against
    // is the notice's bytes landing between the user's payload and
    // the user's CR.
    let user_needle: &[u8] = b"USERMSG-PAYLOAD\r\n";
    wait_for_in_snapshot(
        &server.registry,
        &agent_id,
        user_needle,
        Duration::from_secs(5),
    )
    .await
    .unwrap_or_else(|| {
        let last = server.registry.snapshot(&agent_id).unwrap_or_default();
        panic!(
            "notice-race regression: USERMSG-PAYLOAD\\r\\n missing — \
                 the notice's LF landed between the user's payload and \
                 the user's CR, fusing them into a single canonical \
                 line. Snapshot: {:?}",
            String::from_utf8_lossy(&last)
        )
    });

    // The notice text must surface too (PTY echo gets it; cat won't
    // emit it as a separate stdout line because the LF terminator
    // doesn't close a *new* canonical line from cat's perspective
    // unless the buffer is already empty — but the echo path always
    // surfaces it).
    let notice_needle: &[u8] = b"NOTICE-MARKER";
    wait_for_in_snapshot(
        &server.registry,
        &agent_id,
        notice_needle,
        Duration::from_secs(5),
    )
    .await
    .expect("notice text must surface in scrollback");

    drop(ctrl);
    let _ = server.registry.close_agent(&agent_id);
}
