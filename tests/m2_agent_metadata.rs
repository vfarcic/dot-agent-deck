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

use tempfile::TempDir;
use tokio::net::UnixStream;
use tokio::task::JoinHandle;

use dot_agent_deck::agent_pty::{AgentPtyRegistry, AgentRecord, CWD_MAX_LEN, DISPLAY_NAME_MAX_LEN};
use dot_agent_deck::daemon_client::{DaemonClient, StartAgentOptions};
use dot_agent_deck::daemon_protocol::{
    AttachResponse, KIND_REQ, KIND_RESP, bind_attach_listener, read_frame, serve_attach,
    write_frame,
};

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

// Silence "unused" warnings: `AttachResponse` and `UnixStream` are
// re-exports the harness might use in future tests but doesn't here.
#[allow(dead_code)]
fn _shape_imports_alive() -> (Option<AttachResponse>, Option<UnixStream>) {
    (None, None)
}
