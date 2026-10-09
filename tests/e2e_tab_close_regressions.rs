#![cfg(all(feature = "e2e", unix))]

//! PTY-attached regression coverage for whole-tab close planning and teardown.
//!
//! The real binary talks to a protocol-faithful synthetic daemon so each test
//! can control StopAgent outcomes while observing the production dialog, tab
//! strip, status line, and daemon registry.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener as StdUnixListener, UnixStream as StdUnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use common::TuiDeck;
use dot_agent_deck::agent_pty::{AgentRecord, TabMembership};
use dot_agent_deck::daemon_protocol::{
    AttachRequest, AttachResponse, KIND_REQ, PROTOCOL_VERSION, RunningAgentsSummary, read_frame,
    write_resp,
};
use spec::spec;
use tempfile::TempDir;
use tokio::net::{UnixListener, UnixStream};

#[derive(Clone)]
enum StopScript {
    FailOnce { agent_id: String },
    AlreadyGone,
    DoneUnverified,
}

struct ScriptedDaemon {
    socket_path: PathBuf,
    records: Arc<Mutex<Vec<AgentRecord>>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    _dir: TempDir,
}

impl ScriptedDaemon {
    fn spawn(records: Vec<AgentRecord>, stop_script: StopScript) -> Self {
        let dir = common::harness_tempdir().expect("scripted daemon tempdir");
        let socket_path = dir.path().join("daemon.sock");
        let listener = StdUnixListener::bind(&socket_path).expect("bind scripted daemon socket");
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))
            .expect("make scripted daemon socket owner-only");
        listener
            .set_nonblocking(true)
            .expect("set scripted daemon listener nonblocking");

        let records = Arc::new(Mutex::new(records));
        let stop_requests = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let next_spawn = Arc::new(AtomicUsize::new(1));
        let fail_pending = Arc::new(AtomicBool::new(true));
        let records_for_thread = Arc::clone(&records);
        let stops_for_thread = Arc::clone(&stop_requests);
        let shutdown_for_thread = Arc::clone(&shutdown);
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("scripted daemon runtime");
            runtime.block_on(async move {
                let listener =
                    UnixListener::from_std(listener).expect("convert scripted listener to tokio");
                loop {
                    let (stream, _) = listener
                        .accept()
                        .await
                        .expect("accept scripted daemon client");
                    if shutdown_for_thread.load(Ordering::SeqCst) {
                        break;
                    }
                    let records = Arc::clone(&records_for_thread);
                    let stop_requests = Arc::clone(&stops_for_thread);
                    let next_spawn = Arc::clone(&next_spawn);
                    let fail_pending = Arc::clone(&fail_pending);
                    let stop_script = stop_script.clone();
                    tokio::spawn(async move {
                        handle_connection(
                            stream,
                            records,
                            stop_requests,
                            next_spawn,
                            fail_pending,
                            stop_script,
                        )
                        .await;
                    });
                }
            });
        });

        Self {
            socket_path,
            records,
            shutdown,
            thread: Some(thread),
            _dir: dir,
        }
    }

    fn path(&self) -> &Path {
        &self.socket_path
    }

    fn records(&self) -> Vec<AgentRecord> {
        self.records.lock().unwrap().clone()
    }

    fn wait_for_record_count(&self, expected: usize, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.records.lock().unwrap().len() == expected {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::yield_now();
        }
    }
}

impl Drop for ScriptedDaemon {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = StdUnixStream::connect(&self.socket_path);
        if let Some(thread) = self.thread.take() {
            thread.join().expect("join scripted daemon thread");
        }
    }
}

async fn handle_connection(
    mut stream: UnixStream,
    records: Arc<Mutex<Vec<AgentRecord>>>,
    stop_requests: Arc<Mutex<Vec<String>>>,
    next_spawn: Arc<AtomicUsize>,
    fail_pending: Arc<AtomicBool>,
    stop_script: StopScript,
) {
    let Some((kind, payload)) = read_frame(&mut stream)
        .await
        .expect("read scripted daemon request")
    else {
        // The startup liveness probe connects and deliberately sends nothing.
        return;
    };
    assert_eq!(kind, KIND_REQ, "scripted daemon expects request frames");
    let request: AttachRequest =
        serde_json::from_slice(&payload).expect("decode scripted daemon request");

    match request {
        AttachRequest::Hello { .. } => {
            let summary = RunningAgentsSummary::from_records(&records.lock().unwrap());
            let response = AttachResponse::hello(PROTOCOL_VERSION)
                .with_running_agents(summary)
                .with_guarded_send();
            write_resp(&mut stream, &response)
                .await
                .expect("reply to Hello");
        }
        AttachRequest::ListAgents => {
            if matches!(stop_script, StopScript::DoneUnverified)
                && !stop_requests.lock().unwrap().is_empty()
            {
                write_resp(
                    &mut stream,
                    &AttachResponse::err("registry unavailable during close verification"),
                )
                .await
                .expect("reply with scripted ListAgents failure");
            } else {
                let snapshot = records.lock().unwrap().clone();
                write_resp(&mut stream, &AttachResponse::agent_records(snapshot))
                    .await
                    .expect("reply to ListAgents");
            }
        }
        AttachRequest::StartAgent {
            cwd,
            display_name,
            rows,
            cols,
            env,
            tab_membership,
            agent_type,
            ..
        } => {
            let ordinal = next_spawn.fetch_add(1, Ordering::SeqCst);
            let id = format!("spawned-{ordinal}");
            let pane_id_env = env
                .into_iter()
                .find_map(|(key, value)| (key == "DOT_AGENT_DECK_PANE_ID").then_some(value));
            records.lock().unwrap().push(AgentRecord {
                id: id.clone(),
                pane_id_env,
                display_name,
                cwd,
                tab_membership,
                agent_type,
                rows,
                cols,
                live: None,
                spawned_at_ms: None,
                cli_name: None,
                prompt_keys: None,
                crashed: None,
                orchestrator_context_path: None,
                authoring_kind: None,
            });
            write_resp(&mut stream, &AttachResponse::with_id(id))
                .await
                .expect("reply to StartAgent");
        }
        AttachRequest::AttachStream { id, .. } => {
            let present = records.lock().unwrap().iter().any(|record| record.id == id);
            let response = if present {
                AttachResponse::ok()
            } else {
                AttachResponse::err(format!("Agent {id} not found"))
            };
            write_resp(&mut stream, &response)
                .await
                .expect("reply to AttachStream");
            if present {
                while read_frame(&mut stream).await.ok().flatten().is_some() {}
            }
        }
        AttachRequest::SubscribeEvents => {
            write_resp(&mut stream, &AttachResponse::ok())
                .await
                .expect("reply to SubscribeEvents");
            while read_frame(&mut stream).await.ok().flatten().is_some() {}
        }
        AttachRequest::StopAgent { id } => {
            stop_requests.lock().unwrap().push(id.clone());
            let fail_this_attempt = matches!(
                &stop_script,
                StopScript::FailOnce { agent_id } if agent_id == &id
            ) && fail_pending.swap(false, Ordering::SeqCst);
            let response = if fail_this_attempt {
                AttachResponse::err(format!("permission denied while stopping {id}"))
            } else if matches!(
                stop_script,
                StopScript::AlreadyGone | StopScript::DoneUnverified
            ) {
                records.lock().unwrap().retain(|record| record.id != id);
                AttachResponse::err(format!("Agent {id} not found"))
            } else {
                records.lock().unwrap().retain(|record| record.id != id);
                AttachResponse::ok()
            };
            write_resp(&mut stream, &response)
                .await
                .expect("reply to StopAgent");
        }
        AttachRequest::Resize { .. } | AttachRequest::SetAgentLabel { .. } => {
            write_resp(&mut stream, &AttachResponse::ok())
                .await
                .expect("reply to pane metadata request");
        }
        other => panic!("unexpected scripted daemon request: {other:?}"),
    }
}

fn fixture_path(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

fn orchestration_record(
    fixture: &str,
    orchestration: &str,
    role_index: usize,
    agent_id: &str,
    pane_id: &str,
) -> AgentRecord {
    let role_name = if role_index == 0 {
        "orchestrator"
    } else {
        "worker"
    };
    AgentRecord {
        id: agent_id.to_string(),
        pane_id_env: Some(pane_id.to_string()),
        display_name: Some(format!("{orchestration}-{role_name}")),
        cwd: Some(fixture_path(fixture)),
        tab_membership: Some(TabMembership::Orchestration {
            name: orchestration.to_string(),
            role_index,
            role_name: role_name.to_string(),
            is_start_role: role_index == 0,
            orchestration_cwd: Some(fixture_path(fixture)),
            display_title: Some(orchestration.to_string()),
            orchestration_id: Some(format!("{orchestration}-scripted")),
        }),
        agent_type: None,
        rows: 24,
        cols: 80,
        live: None,
        spawned_at_ms: None,
        cli_name: None,
        prompt_keys: None,
        crashed: None,
        orchestrator_context_path: None,
        authoring_kind: None,
    }
}

fn launch_against(daemon: &ScriptedDaemon, fixture: &str) -> TuiDeck {
    TuiDeck::builder()
        .with_pty_size(240, 40)
        .with_env(
            "DOT_AGENT_DECK_ATTACH_SOCKET",
            daemon.path().to_string_lossy(),
        )
        .launch_with_fixture(fixture)
}

fn confirm_close(deck: &TuiDeck) {
    deck.send_bytes(b"\x1b[B");
    deck.send_bytes(b"\r");
}

fn request_close_alpha_tab(deck: &TuiDeck) {
    let (col, row) = deck.wait_for_in_grid("alpha [×]");
    deck.click(col + "alpha [".len() as u16, row);
}

/// Scenario: Hydrate a two-role orchestration whose worker refuses its first stop, then confirm its close. The tab and failed role must remain with a retry status while the successful role disappears; a second close removes the retained tab.
#[spec("lifecycle/stop/017")]
#[test]
fn stop_017_partial_tab_close_is_retained_and_retryable() {
    let daemon = ScriptedDaemon::spawn(
        vec![
            orchestration_record("tab-close-targets", "alpha", 0, "orch-agent", "100"),
            orchestration_record("tab-close-targets", "alpha", 1, "spawned-1", "101"),
        ],
        StopScript::FailOnce {
            agent_id: "spawned-1".to_string(),
        },
    );
    let deck = launch_against(&daemon, "tab-close-targets");
    deck.wait_for_string("alpha-orchestrator");
    assert!(daemon.wait_for_record_count(2, Duration::from_secs(5)));

    request_close_alpha_tab(&deck);
    deck.wait_for_string("Close this tab and all its agents?");
    let first_started = Instant::now();
    confirm_close(&deck);
    deck.wait_for_string("tab is kept");
    let first_elapsed = first_started.elapsed();

    let retained = daemon.records();
    assert_eq!(
        retained.len(),
        1,
        "only the failed pane may remain: {retained:?}"
    );
    assert_eq!(retained[0].id, "spawned-1");
    assert!(
        deck.snapshot_grid().contains('×'),
        "the retained tab must keep a close affordance for retry"
    );

    // Retry on the retained orchestration tab after the one-shot failure clears.
    let retry_started = Instant::now();
    request_close_alpha_tab(&deck);
    deck.wait_for_string("Close this tab and all its agents?");
    confirm_close(&deck);
    deck.wait_for_absence("×");
    let retry_elapsed = retry_started.elapsed();
    assert!(
        daemon.wait_for_record_count(0, Duration::from_secs(5)),
        "retry must close the failed pane and remove the tab: {:?}",
        daemon.records()
    );

    eprintln!(
        "partial close retained tab after {first_elapsed:?}; retry removed it after {retry_elapsed:?}"
    );
}

/// Scenario: Close a hydrated orchestration after exact id-scoped NotFound proves its start role already gone, then repeat with daemon verification unavailable. Both tabs must be removed; the proven-gone case stays warning-free, while the unverified case renders one unattended-agent warning.
#[spec("lifecycle/stop/018")]
#[test]
fn stop_018_already_gone_and_unverified_success_remove_tab() {
    {
        let daemon = ScriptedDaemon::spawn(
            vec![orchestration_record(
                "tab-close-targets",
                "alpha",
                0,
                "ghost-agent",
                "200",
            )],
            StopScript::AlreadyGone,
        );
        let deck = launch_against(&daemon, "tab-close-targets");
        deck.wait_for_string("alpha-orchestrator");
        request_close_alpha_tab(&deck);
        deck.wait_for_string("Close this tab and all its agents?");
        confirm_close(&deck);
        deck.wait_for_absence("×");
        deck.wait_for_string("Closed tab");
        let grid = deck.snapshot_grid();
        assert!(!grid.contains("unattended"), "{grid}");
        assert!(daemon.records().is_empty());
    }

    {
        let daemon = ScriptedDaemon::spawn(
            vec![orchestration_record(
                "tab-close-targets",
                "alpha",
                0,
                "unverified-agent",
                "300",
            )],
            StopScript::DoneUnverified,
        );
        let deck = launch_against(&daemon, "tab-close-targets");
        deck.wait_for_string("alpha-orchestrator");
        request_close_alpha_tab(&deck);
        deck.wait_for_string("Close this tab and all its agents?");
        confirm_close(&deck);
        deck.wait_for_absence("×");
        deck.wait_for_string("may still be running unattended");
        let grid = deck.snapshot_grid();
        assert_eq!(
            grid.matches("may still be running unattended").count(),
            1,
            "the DoneUnverified warning must reach the status line exactly once\n{grid}"
        );
        assert!(daemon.records().is_empty());
    }
}
