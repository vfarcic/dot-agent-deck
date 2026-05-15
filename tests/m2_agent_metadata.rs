//! PRD #76 M2.11 — per-agent display_name and cwd persisted in the
//! daemon-side registry. Replaces the file-based persistence approach
//! from commit b3a2a0d. These tests pin three layers:
//!   - validation rules on the wire (oversize / control-char / null-byte
//!     display names get dropped to `None`; oversize cwds likewise);
//!   - round-trip of the metadata through `StartAgent` → `list_agents`
//!     and through `SetAgentLabel`;
//!   - forward compat with an older daemon that omits `display_name` /
//!     `cwd` from `AgentRecord` JSON.
//!
//! The harness reuses the in-process `AgentPtyRegistry` + `serve_attach`
//! pattern from `m2_rehydration.rs` so the wire shape is exercised end
//! to end via the real `daemon_client::DaemonClient`.

#![cfg(unix)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tempfile::TempDir;
use tokio::net::UnixStream;
use tokio::task::JoinHandle;

use dot_agent_deck::agent_pty::{AgentPtyRegistry, AgentRecord, CWD_MAX_LEN, DISPLAY_NAME_MAX_LEN};
use dot_agent_deck::daemon_client::{DaemonClient, StartAgentOptions};
use dot_agent_deck::daemon_protocol::{
    AttachResponse, KIND_REQ, KIND_RESP, bind_attach_listener, read_frame, serve_attach,
    write_frame,
};
use dot_agent_deck::embedded_pane::EmbeddedPaneController;
use dot_agent_deck::pane::PaneController;

/// `bind_attach_listener` flips the process-global umask while binding;
/// share a lock with the rest of the M-series tests so concurrent tempdir
/// creation can't inherit a 0o600 dir during that window.
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
    }
}

async fn start_real_server() -> Server {
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

fn find_record(records: &[AgentRecord], id: &str) -> AgentRecord {
    records
        .iter()
        .find(|r| r.id == id)
        .cloned()
        .unwrap_or_else(|| panic!("agent {id} missing from list_agents"))
}

// ---------------------------------------------------------------------------
// Round-trip: StartAgent → list_agents.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_agent_with_display_name_and_cwd_round_trips_through_list_agents() {
    let server = start_real_server().await;
    let client = DaemonClient::new(server.path.clone());

    let id = client
        .start_agent(StartAgentOptions {
            command: Some("sh -c 'sleep 30'".into()),
            cwd: Some("/tmp".into()),
            display_name: Some("auditor".into()),
            ..Default::default()
        })
        .await
        .expect("start_agent should succeed");

    let records = client.list_agents().await.expect("list_agents");
    let rec = find_record(&records, &id);
    assert_eq!(rec.display_name.as_deref(), Some("auditor"));
    assert_eq!(rec.cwd.as_deref(), Some("/tmp"));

    server.registry.shutdown_all();
}

// ---------------------------------------------------------------------------
// SetAgentLabel: update + clear.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_agent_label_updates_stored_fields() {
    let server = start_real_server().await;
    let client = DaemonClient::new(server.path.clone());

    let id = client
        .start_agent(StartAgentOptions {
            command: Some("sh -c 'sleep 30'".into()),
            display_name: Some("initial".into()),
            cwd: Some("/tmp".into()),
            ..Default::default()
        })
        .await
        .expect("start_agent");

    client
        .set_agent_label(&id, Some("renamed".into()), Some("/var/tmp".into()))
        .await
        .expect("set_agent_label");

    let records = client.list_agents().await.expect("list_agents");
    let rec = find_record(&records, &id);
    assert_eq!(rec.display_name.as_deref(), Some("renamed"));
    assert_eq!(rec.cwd.as_deref(), Some("/var/tmp"));

    server.registry.shutdown_all();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_agent_label_clears_fields_when_passed_none() {
    let server = start_real_server().await;
    let client = DaemonClient::new(server.path.clone());

    let id = client
        .start_agent(StartAgentOptions {
            command: Some("sh -c 'sleep 30'".into()),
            display_name: Some("initial".into()),
            cwd: Some("/tmp".into()),
            ..Default::default()
        })
        .await
        .expect("start_agent");

    client
        .set_agent_label(&id, None, None)
        .await
        .expect("set_agent_label");

    let records = client.list_agents().await.expect("list_agents");
    let rec = find_record(&records, &id);
    assert!(rec.display_name.is_none(), "display_name should be cleared");
    assert!(rec.cwd.is_none(), "cwd should be cleared");

    server.registry.shutdown_all();
}

// ---------------------------------------------------------------------------
// Validation: oversize, control chars, null bytes, oversize cwd.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn display_name_oversize_is_dropped_to_none() {
    let server = start_real_server().await;
    let client = DaemonClient::new(server.path.clone());

    // 200 bytes > DISPLAY_NAME_MAX_LEN (128).
    let oversize: String = "a".repeat(200);
    assert!(oversize.len() > DISPLAY_NAME_MAX_LEN);

    let id = client
        .start_agent(StartAgentOptions {
            command: Some("sh -c 'sleep 30'".into()),
            display_name: Some(oversize),
            ..Default::default()
        })
        .await
        .expect("start_agent");

    let rec = find_record(&client.list_agents().await.unwrap(), &id);
    assert!(rec.display_name.is_none(), "oversize must be rejected");

    server.registry.shutdown_all();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn display_name_with_control_chars_is_dropped_to_none() {
    let server = start_real_server().await;
    let client = DaemonClient::new(server.path.clone());

    // ANSI escape sequence — exact payload the auditor flagged.
    let id = client
        .start_agent(StartAgentOptions {
            command: Some("sh -c 'sleep 30'".into()),
            display_name: Some("\x1b[31moops".into()),
            ..Default::default()
        })
        .await
        .expect("start_agent");

    let rec = find_record(&client.list_agents().await.unwrap(), &id);
    assert!(
        rec.display_name.is_none(),
        "ANSI escape must be rejected, got {:?}",
        rec.display_name
    );

    // Confirm SetAgentLabel applies the same filter, not just spawn-time
    // capture — a same-uid peer could otherwise smuggle control bytes
    // after the fact.
    client
        .set_agent_label(&id, Some("\x00null".into()), None)
        .await
        .expect("set_agent_label call itself should succeed");
    let rec = find_record(&client.list_agents().await.unwrap(), &id);
    assert!(
        rec.display_name.is_none(),
        "null byte must be rejected, got {:?}",
        rec.display_name
    );

    server.registry.shutdown_all();
}

// Reviewer P1: a dashboard rename must reach the daemon through
// `SetAgentLabel` and show up in `list_agents`. The dashboard handler is
// awkward to drive at the TUI level (it depends on terminal state and
// the live `PaneController`), so we cover the wire pathway it now uses:
// `pane.rename_pane` ultimately invokes `DaemonClient::set_agent_label`,
// and the daemon must reflect the new name. A separate per-PaneController
// test (`rename_pane_propagates_display_name_change_for_stream_panes` in
// `src/embedded_pane.rs`) would require a live daemon socket harness;
// this integration test pins the wire shape end-to-end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dashboard_rename_propagates_through_set_agent_label() {
    let server = start_real_server().await;
    let client = DaemonClient::new(server.path.clone());

    let id = client
        .start_agent(StartAgentOptions {
            command: Some("sh -c 'sleep 30'".into()),
            display_name: Some("old-name".into()),
            cwd: Some("/tmp".into()),
            ..Default::default()
        })
        .await
        .expect("start_agent");

    // This is the exact call the rename-Enter handler now makes via
    // `pane.rename_pane`: new label, cwd echoed so "None means clear"
    // doesn't erase the spawn-time cwd.
    client
        .set_agent_label(
            &id,
            Some("renamed-in-dashboard".into()),
            Some("/tmp".into()),
        )
        .await
        .expect("set_agent_label");

    let rec = find_record(&client.list_agents().await.unwrap(), &id);
    assert_eq!(rec.display_name.as_deref(), Some("renamed-in-dashboard"));
    assert_eq!(
        rec.cwd.as_deref(),
        Some("/tmp"),
        "cwd must survive the rename — set_agent_label clears with None"
    );

    server.registry.shutdown_all();
}

// Reviewer P2: the new-pane form's Name must reach the daemon in the
// initial `StartAgent` RPC, not via a follow-up rename. We exercise the
// wire path the form now uses: `StartAgent.display_name = form_name`
// lands in `AgentRecord.display_name` directly, no command-based
// fallback window.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_pane_form_name_lands_in_start_agent_display_name() {
    let server = start_real_server().await;
    let client = DaemonClient::new(server.path.clone());

    // Form-name differs from command so a regression that fell back to
    // `command.unwrap_or("shell")` would be visible.
    let form_name = "my-orchestrator";
    let id = client
        .start_agent(StartAgentOptions {
            command: Some("sh -c 'sleep 30'".into()),
            display_name: Some(form_name.into()),
            cwd: Some("/tmp".into()),
            ..Default::default()
        })
        .await
        .expect("start_agent");

    let rec = find_record(&client.list_agents().await.unwrap(), &id);
    assert_eq!(
        rec.display_name.as_deref(),
        Some(form_name),
        "form Name must reach AgentRecord.display_name, not the command fallback"
    );
    // Negative side of the assertion — the previous code would have set
    // `display_name = Some("sh -c 'sleep 30'")` until a follow-up
    // SetAgentLabel landed.
    assert_ne!(rec.display_name.as_deref(), Some("sh -c 'sleep 30'"));

    server.registry.shutdown_all();
}

// Auditor MED: cwd must reject ASCII control characters so a hostile
// `SetAgentLabel` can't smuggle terminal escape sequences into the
// dashboard's basename render at `src/ui.rs:5069-5105`. Mirrors the
// existing `display_name_with_control_chars_is_dropped_to_none` test —
// cwd now uses the same filter.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cwd_with_control_chars_is_dropped_to_none() {
    let server = start_real_server().await;
    let client = DaemonClient::new(server.path.clone());

    let id = client
        .start_agent(StartAgentOptions {
            command: Some("sh -c 'sleep 30'".into()),
            display_name: Some("ok".into()),
            // Don't pass a control-char cwd through StartAgent — that
            // would also be the child's CWD and spawn would fail. We
            // exercise the registry filter via SetAgentLabel, which
            // doesn't touch the child.
            cwd: Some("/tmp".into()),
            ..Default::default()
        })
        .await
        .expect("start_agent");

    // ESC (0x1b) — the exact byte the auditor flagged, in the canonical
    // payload `/tmp/\x1b[31mpwn`.
    client
        .set_agent_label(&id, Some("ok".into()), Some("/tmp/\x1b[31mpwn".into()))
        .await
        .expect("set_agent_label call itself should succeed");
    let rec = find_record(&client.list_agents().await.unwrap(), &id);
    assert!(
        rec.cwd.is_none(),
        "ESC in cwd must be rejected, got {:?}",
        rec.cwd
    );

    // NUL (0x00).
    client
        .set_agent_label(&id, Some("ok".into()), Some("/tmp/\x00pwn".into()))
        .await
        .expect("set_agent_label call itself should succeed");
    let rec = find_record(&client.list_agents().await.unwrap(), &id);
    assert!(
        rec.cwd.is_none(),
        "NUL in cwd must be rejected, got {:?}",
        rec.cwd
    );

    // DEL (0x7f) — the upper bound the display-name filter rejects.
    client
        .set_agent_label(&id, Some("ok".into()), Some("/tmp/\x7fpwn".into()))
        .await
        .expect("set_agent_label call itself should succeed");
    let rec = find_record(&client.list_agents().await.unwrap(), &id);
    assert!(
        rec.cwd.is_none(),
        "DEL (0x7f) in cwd must be rejected, got {:?}",
        rec.cwd
    );

    server.registry.shutdown_all();
}

// Auditor MED (positive case): the control-char filter must NOT reject
// legitimate UTF-8 paths. Accented characters encode as bytes > 0x7F
// (e.g. 'é' → 0xc3 0xa9), which the filter explicitly allows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cwd_with_unicode_path_is_accepted() {
    let server = start_real_server().await;
    let client = DaemonClient::new(server.path.clone());

    let id = client
        .start_agent(StartAgentOptions {
            command: Some("sh -c 'sleep 30'".into()),
            display_name: Some("ok".into()),
            cwd: Some("/tmp".into()),
            ..Default::default()
        })
        .await
        .expect("start_agent");

    let unicode_cwd = "/home/usér/projet";
    client
        .set_agent_label(&id, Some("ok".into()), Some(unicode_cwd.into()))
        .await
        .expect("set_agent_label");

    let rec = find_record(&client.list_agents().await.unwrap(), &id);
    assert_eq!(
        rec.cwd.as_deref(),
        Some(unicode_cwd),
        "UTF-8 cwd must round-trip unchanged"
    );

    server.registry.shutdown_all();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cwd_oversize_is_dropped_to_none() {
    let server = start_real_server().await;
    let client = DaemonClient::new(server.path.clone());

    // 4097 bytes > CWD_MAX_LEN (4096).
    let oversize_cwd: String = "/".repeat(CWD_MAX_LEN + 1);
    assert!(oversize_cwd.len() > CWD_MAX_LEN);

    let id = client
        .start_agent(StartAgentOptions {
            command: Some("sh -c 'sleep 30'".into()),
            display_name: Some("ok".into()),
            // SpawnOptions::cwd is also the child's CWD; the oversize
            // value isn't a valid path so the child would fail to spawn.
            // We exercise the registry's filter via SetAgentLabel instead,
            // which doesn't touch the child.
            ..Default::default()
        })
        .await
        .expect("start_agent");

    client
        .set_agent_label(&id, Some("ok".into()), Some(oversize_cwd))
        .await
        .expect("set_agent_label call itself should succeed");

    let rec = find_record(&client.list_agents().await.unwrap(), &id);
    assert_eq!(rec.display_name.as_deref(), Some("ok"));
    assert!(rec.cwd.is_none(), "oversize cwd must be rejected");

    server.registry.shutdown_all();
}

// ---------------------------------------------------------------------------
// Forward compat: older daemon omits display_name/cwd from AgentRecord JSON.
// ---------------------------------------------------------------------------

/// Hand-rolled mock daemon that returns an `AgentRecord` shape predating
/// M2.11 — `id` + `pane_id_env` only, no `display_name`, no `cwd`. The
/// real `DaemonClient` must still deserialize this and surface `None`
/// for both fields, so a newer TUI keeps working against an older
/// daemon binary in the field.
async fn run_legacy_record_server(listener: tokio::net::UnixListener) {
    while let Ok((mut s, _)) = listener.accept().await {
        // Expect one REQ (ListAgents) per connection.
        match read_frame(&mut s).await {
            Ok(Some((KIND_REQ, _payload))) => {
                // Synthesize an AgentRecord *missing* both new fields.
                let legacy_json = serde_json::json!({
                    "ok": true,
                    "agent_records": [
                        { "id": "1", "pane_id_env": "pane-1" },
                        { "id": "2" },
                    ],
                });
                let bytes = serde_json::to_vec(&legacy_json).unwrap();
                let _ = write_frame(&mut s, KIND_RESP, &bytes).await;
            }
            _ => continue,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn older_daemon_record_shape_still_hydrates_with_none_metadata() {
    let (dir, path, listener) = {
        let _g = HARNESS_BIND_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("attach.sock");
        let listener_std = std::os::unix::net::UnixListener::bind(&path).expect("bind sock");
        listener_std.set_nonblocking(true).expect("set_nonblocking");
        let listener = tokio::net::UnixListener::from_std(listener_std).expect("tokio adopt");
        (dir, path, listener)
    };
    let handle = tokio::spawn(async move {
        run_legacy_record_server(listener).await;
    });

    let client = DaemonClient::new(path.clone());
    let records = client
        .list_agents()
        .await
        .expect("list_agents should decode legacy shape");
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].id, "1");
    assert_eq!(records[0].pane_id_env.as_deref(), Some("pane-1"));
    assert!(records[0].display_name.is_none());
    assert!(records[0].cwd.is_none());
    assert_eq!(records[1].id, "2");
    assert!(records[1].pane_id_env.is_none());
    assert!(records[1].display_name.is_none());
    assert!(records[1].cwd.is_none());

    drop(client);
    handle.abort();
    drop(dir);
}

// ---------------------------------------------------------------------------
// PRD #76 M2.11 reviewer P2.4 — pin the controller-level rename and
// create-pane paths against a real daemon. These cover what the direct
// `set_agent_label` and `start_agent` tests above can't: they would
// pass even if `EmbeddedPaneController::rename_pane` or
// `create_pane_with_display_name` never forwarded to the daemon. The
// tests below drive the controller (the same object `ui.rs` calls) so
// a regression in the controller wiring fails the suite, not just the
// RPC.
// ---------------------------------------------------------------------------

async fn poll_record<F>(client: &DaemonClient, agent_id: &str, mut pred: F) -> Option<AgentRecord>
where
    F: FnMut(&AgentRecord) -> bool,
{
    // `rename_pane` is fire-and-forget on the controller's runtime, so
    // the daemon update is observed via polling rather than awaiting
    // the spawn handle. 2s is generous — in practice the in-process
    // server replies in single-digit milliseconds.
    let start = tokio::time::Instant::now();
    while tokio::time::Instant::now() - start < Duration::from_secs(2) {
        if let Ok(records) = client.list_agents().await
            && let Some(rec) = records.into_iter().find(|r| r.id == agent_id)
            && pred(&rec)
        {
            return Some(rec);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    None
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rename_pane_with_empty_text_clears_daemon_display_name() {
    let server = start_real_server().await;
    let client = DaemonClient::new(server.path.clone());

    // Build a controller against the same in-process daemon — this is
    // the same construction the production `main` path uses for
    // RemoteDeckLocal mode.
    let ctrl = Arc::new(EmbeddedPaneController::with_remote_deck(
        server.path.clone(),
        tokio::runtime::Handle::current(),
    ));

    // Spawn an agent through the controller so rename_pane has a real
    // Stream backend (PaneBackend::Pty would skip the daemon path).
    // create_pane_with_display_name internally `block_on`s the daemon
    // client, so run it on a blocking thread.
    let pane_id = {
        let ctrl = ctrl.clone();
        tokio::task::spawn_blocking(move || {
            ctrl.create_pane_with_display_name(
                Some("sh -c 'sleep 30'"),
                Some("/tmp"),
                Some("initial"),
            )
        })
        .await
        .unwrap()
        .expect("create_pane_with_display_name")
    };

    // Find the agent id (assigned by the daemon).
    let records = client.list_agents().await.unwrap();
    let rec = records
        .iter()
        .find(|r| r.pane_id_env.as_deref() == Some(&pane_id))
        .expect("agent record for new pane");
    let agent_id = rec.id.clone();
    assert_eq!(rec.display_name.as_deref(), Some("initial"));

    // Empty-string rename — this is exactly what the Enter handler
    // produces when the user clears the rename buffer and presses
    // Enter (`rename_commit_value` returns `Some(String::new())`).
    let id_for_call = pane_id.clone();
    let ctrl_clone = ctrl.clone();
    tokio::task::spawn_blocking(move || ctrl_clone.rename_pane(&id_for_call, ""))
        .await
        .unwrap()
        .expect("rename_pane with empty text");

    // The daemon must observe a `display_name: None` clear. Without
    // the trim-to-None logic in `rename_pane` it would store `Some("")`
    // (a blank label) or — under the original P1 bug — keep the stale
    // pre-clear value ("initial").
    let rec = poll_record(&client, &agent_id, |r| r.display_name.is_none())
        .await
        .expect("display_name must clear within timeout");
    assert!(rec.display_name.is_none());
    // cwd must survive the clear — the controller echoes the cached
    // cwd back to `set_agent_label` so "None to clear" semantics don't
    // erase the spawn-time cwd as a side effect of the rename.
    assert_eq!(rec.cwd.as_deref(), Some("/tmp"));

    server.registry.shutdown_all();
    drop(ctrl);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rename_pane_with_whitespace_clears_daemon_display_name() {
    let server = start_real_server().await;
    let client = DaemonClient::new(server.path.clone());
    let ctrl = Arc::new(EmbeddedPaneController::with_remote_deck(
        server.path.clone(),
        tokio::runtime::Handle::current(),
    ));

    let pane_id = {
        let ctrl = ctrl.clone();
        tokio::task::spawn_blocking(move || {
            ctrl.create_pane_with_display_name(
                Some("sh -c 'sleep 30'"),
                Some("/tmp"),
                Some("initial"),
            )
        })
        .await
        .unwrap()
        .expect("create_pane_with_display_name")
    };

    let agent_id = client
        .list_agents()
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.pane_id_env.as_deref() == Some(&pane_id))
        .expect("record")
        .id;

    // Whitespace-only rename — also a "clear". Without the trim check
    // in `rename_pane`, the daemon would happily store "   " (it
    // passes `is_valid_display_name`) and the dashboard would render
    // a blank label after reconnect.
    let id_for_call = pane_id.clone();
    let ctrl_clone = ctrl.clone();
    tokio::task::spawn_blocking(move || ctrl_clone.rename_pane(&id_for_call, "   "))
        .await
        .unwrap()
        .expect("rename_pane with whitespace");

    let rec = poll_record(&client, &agent_id, |r| r.display_name.is_none())
        .await
        .expect("whitespace rename must clear display_name");
    assert!(rec.display_name.is_none());

    server.registry.shutdown_all();
    drop(ctrl);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_pane_with_whitespace_name_falls_back_via_stream_path() {
    let server = start_real_server().await;
    let client = DaemonClient::new(server.path.clone());
    let ctrl = Arc::new(EmbeddedPaneController::with_remote_deck(
        server.path.clone(),
        tokio::runtime::Handle::current(),
    ));

    // Whitespace-only Name flowing through the controller into
    // StartAgent must NOT land in AgentRecord.display_name. The
    // controller filter strips it to None at the call site, and
    // `create_stream_pane` falls back to the command string for the
    // daemon-side label.
    let _pane_id = {
        let ctrl = ctrl.clone();
        tokio::task::spawn_blocking(move || {
            ctrl.create_pane_with_display_name(Some("sh -c 'sleep 30'"), Some("/tmp"), Some("   "))
        })
        .await
        .unwrap()
        .expect("create_pane_with_display_name")
    };

    let records = client.list_agents().await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].display_name.as_deref(),
        Some("sh -c 'sleep 30'"),
        "whitespace-only Name must fall back to command, not be stored as '   '"
    );
    assert_ne!(records[0].display_name.as_deref(), Some("   "));

    server.registry.shutdown_all();
    drop(ctrl);
}

// Silence "unused" warnings: `AttachResponse` and `UnixStream` are
// re-exports the harness might use in future tests but doesn't here.
#[allow(dead_code)]
fn _shape_imports_alive() -> (Option<AttachResponse>, Option<UnixStream>) {
    (None, None)
}
