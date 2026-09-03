#![cfg(unix)]

//! L1 close-path tests backed by a synthetic Unix-socket daemon.
//!
//! These exercise the real `EmbeddedPaneController::close_pane` path without
//! spawning the dot-agent-deck binary. The fake daemon accepts StartAgent and
//! AttachStream, then returns a controlled StopAgent error so the observable
//! local pane registry can prove whether teardown completed or was retained.

// Issue #322. Fast-tier, does NOT link `tests/common/mod.rs` — the synthetic
// daemon below binds a `daemon.sock` in its scratch dir, so live `/tmp`
// sampling during a recorded `cargo test-e2e` caught five of this file's
// directories holding a LISTENing socket. `#[path]`-including the ~40-line
// crate-internal resolver costs one module and two extra test executions,
// rather than the harness's ~530. See `docs/develop/e2e-temp-dirs.md`.
#[path = "../src/test_temp.rs"]
mod test_temp;

use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use dot_agent_deck::agent_pty::AgentRecord;
use dot_agent_deck::daemon_protocol::{
    AttachRequest, AttachResponse, KIND_REQ, read_frame, write_resp,
};
use dot_agent_deck::embedded_pane::EmbeddedPaneController;
use dot_agent_deck::pane::{PaneController, PaneDirection, PaneError, PaneInfo, RenameOutcome};
use dot_agent_deck::project_config::{OrchestrationConfig, OrchestrationRoleConfig};
use dot_agent_deck::tab::TabManager;
use spec::spec;
use tempfile::TempDir;
use tokio::net::{UnixListener, UnixStream};

struct StopErrorDaemon {
    socket_path: PathBuf,
    stop_requests: Arc<Mutex<Vec<String>>>,
    list_requests: Arc<AtomicUsize>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    _dir: TempDir,
}

#[derive(Clone, Copy)]
enum StopPlan {
    Error(&'static str),
    NotFound(ListPlan),
    ChainedHandover,
    EndlessChurn { stop_reply_delay: Duration },
    ReplacementError(&'static str),
    ReplacementTimeout,
}

#[derive(Clone, Copy)]
enum ListPlan {
    Empty,
    Replacement(&'static str),
    DelayedReplacement {
        replacement_id: &'static str,
        delay: Duration,
    },
    Error(&'static str),
    Timeout,
}

impl StopErrorDaemon {
    fn spawn(stop_error: &'static str) -> Self {
        Self::spawn_with_plan(StopPlan::Error(stop_error))
    }

    fn spawn_not_found(list_plan: ListPlan) -> Self {
        Self::spawn_with_plan(StopPlan::NotFound(list_plan))
    }

    fn spawn_with_replacement(replacement_id: &'static str) -> Self {
        Self::spawn_not_found(ListPlan::Replacement(replacement_id))
    }

    fn spawn_chained_handover() -> Self {
        Self::spawn_with_plan(StopPlan::ChainedHandover)
    }

    fn spawn_endless_churn(stop_reply_delay: Duration) -> Self {
        Self::spawn_with_plan(StopPlan::EndlessChurn { stop_reply_delay })
    }

    fn spawn_replacement_error(error: &'static str) -> Self {
        Self::spawn_with_plan(StopPlan::ReplacementError(error))
    }

    fn spawn_replacement_timeout() -> Self {
        Self::spawn_with_plan(StopPlan::ReplacementTimeout)
    }

    fn spawn_with_plan(stop_plan: StopPlan) -> Self {
        let dir = test_temp::tempdir().expect("synthetic daemon tempdir");
        let socket_path = dir.path().join("daemon.sock");
        let listener = StdUnixListener::bind(&socket_path).expect("bind synthetic daemon socket");
        listener
            .set_nonblocking(true)
            .expect("set synthetic daemon listener nonblocking");

        let stop_requests = Arc::new(Mutex::new(Vec::new()));
        let list_requests = Arc::new(AtomicUsize::new(0));
        let slot_owner = Arc::new(AtomicUsize::new(2));
        let pane_id_env = Arc::new(Mutex::new(None));
        let first_stop_at = Arc::new(Mutex::new(None));
        let shutdown = Arc::new(AtomicBool::new(false));
        let requests_for_thread = Arc::clone(&stop_requests);
        let lists_for_thread = Arc::clone(&list_requests);
        let slot_owner_for_thread = Arc::clone(&slot_owner);
        let pane_id_for_thread = Arc::clone(&pane_id_env);
        let first_stop_for_thread = Arc::clone(&first_stop_at);
        let shutdown_for_thread = Arc::clone(&shutdown);
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("synthetic daemon runtime");
            runtime.block_on(async move {
                let listener = UnixListener::from_std(listener)
                    .expect("convert synthetic daemon listener to tokio");
                while !shutdown_for_thread.load(Ordering::SeqCst) {
                    tokio::select! {
                        accepted = listener.accept() => {
                            let (stream, _) = accepted.expect("accept synthetic daemon client");
                            let stop_requests = Arc::clone(&requests_for_thread);
                            let list_requests = Arc::clone(&lists_for_thread);
                            let slot_owner = Arc::clone(&slot_owner_for_thread);
                            let pane_id_env = Arc::clone(&pane_id_for_thread);
                            let first_stop_at = Arc::clone(&first_stop_for_thread);
                            tokio::spawn(async move {
                                handle_connection(
                                    stream,
                                    stop_plan,
                                    stop_requests,
                                    list_requests,
                                    slot_owner,
                                    pane_id_env,
                                    first_stop_at,
                                ).await;
                            });
                        }
                        _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                    }
                }
            });
        });

        Self {
            socket_path,
            stop_requests,
            list_requests,
            shutdown,
            thread: Some(thread),
            _dir: dir,
        }
    }

    fn path(&self) -> &Path {
        &self.socket_path
    }

    fn stop_requests(&self) -> Vec<String> {
        self.stop_requests.lock().unwrap().clone()
    }

    fn list_requests(&self) -> usize {
        self.list_requests.load(Ordering::SeqCst)
    }
}

impl Drop for StopErrorDaemon {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            thread.join().expect("join synthetic daemon thread");
        }
    }
}

async fn handle_connection(
    mut stream: UnixStream,
    stop_plan: StopPlan,
    stop_requests: Arc<Mutex<Vec<String>>>,
    list_requests: Arc<AtomicUsize>,
    slot_owner: Arc<AtomicUsize>,
    pane_id_env: Arc<Mutex<Option<String>>>,
    first_stop_at: Arc<Mutex<Option<Instant>>>,
) {
    let Some((kind, payload)) = read_frame(&mut stream)
        .await
        .expect("read synthetic daemon request")
    else {
        return;
    };
    assert_eq!(kind, KIND_REQ, "controller must send a request frame");
    let request: AttachRequest =
        serde_json::from_slice(&payload).expect("decode synthetic daemon request");

    match request {
        AttachRequest::StartAgent { env, .. } => {
            *pane_id_env.lock().unwrap() = env
                .into_iter()
                .find_map(|(key, value)| (key == "DOT_AGENT_DECK_PANE_ID").then_some(value));
            write_resp(&mut stream, &AttachResponse::with_id("agent-1".to_string()))
                .await
                .expect("reply to StartAgent");
        }
        AttachRequest::AttachStream { .. } => {
            write_resp(&mut stream, &AttachResponse::ok())
                .await
                .expect("reply to AttachStream");
            if matches!(
                stop_plan,
                StopPlan::NotFound(ListPlan::DelayedReplacement { .. })
            ) {
                // End the old agent's stream so the pane I/O task enters its
                // real replacement-lookup state before close begins.
                return;
            }
            // Keep the long-lived attach connection open until the controller
            // tears down the pane and drops its stream task.
            while read_frame(&mut stream).await.ok().flatten().is_some() {}
        }
        AttachRequest::StopAgent { id } => {
            stop_requests.lock().unwrap().push(id.clone());
            let response = match stop_plan {
                StopPlan::Error(stop_error) => AttachResponse::err(stop_error),
                StopPlan::NotFound(list_plan) => {
                    let replacement_id = match list_plan {
                        ListPlan::Replacement(id)
                        | ListPlan::DelayedReplacement {
                            replacement_id: id, ..
                        } => Some(id),
                        ListPlan::Empty | ListPlan::Error(_) | ListPlan::Timeout => None,
                    };
                    if replacement_id == Some(id.as_str()) {
                        AttachResponse::ok()
                    } else {
                        first_stop_at
                            .lock()
                            .unwrap()
                            .get_or_insert_with(Instant::now);
                        AttachResponse::err(format!("Agent {id} not found"))
                    }
                }
                StopPlan::ChainedHandover => match id.as_str() {
                    "agent-2" => {
                        slot_owner.store(3, Ordering::SeqCst);
                        AttachResponse::err("Agent agent-2 not found")
                    }
                    "agent-3" => {
                        slot_owner.store(0, Ordering::SeqCst);
                        AttachResponse::ok()
                    }
                    _ => AttachResponse::err(format!("Agent {id} not found")),
                },
                StopPlan::EndlessChurn { stop_reply_delay } => {
                    let owner = format!("agent-{}", slot_owner.load(Ordering::SeqCst));
                    if id == owner {
                        tokio::time::sleep(stop_reply_delay).await;
                        slot_owner.fetch_add(1, Ordering::SeqCst);
                    }
                    AttachResponse::err(format!("Agent {id} not found"))
                }
                StopPlan::ReplacementError(error) => {
                    if id == "agent-2" {
                        AttachResponse::err(error)
                    } else {
                        AttachResponse::err(format!("Agent {id} not found"))
                    }
                }
                StopPlan::ReplacementTimeout => {
                    if id == "agent-2" {
                        std::future::pending::<()>().await;
                    }
                    AttachResponse::err(format!("Agent {id} not found"))
                }
            };
            write_resp(&mut stream, &response)
                .await
                .expect("reply to StopAgent");
        }
        AttachRequest::ListAgents => {
            list_requests.fetch_add(1, Ordering::SeqCst);
            let list_plan = match stop_plan {
                StopPlan::Error(_) | StopPlan::NotFound(ListPlan::Empty) => Some(ListPlan::Empty),
                StopPlan::NotFound(list_plan) => Some(list_plan),
                StopPlan::ChainedHandover
                | StopPlan::EndlessChurn { .. }
                | StopPlan::ReplacementError(_)
                | StopPlan::ReplacementTimeout => None,
            };
            if let Some(ListPlan::Error(error)) = list_plan {
                write_resp(&mut stream, &AttachResponse::err(error))
                    .await
                    .expect("reply to ListAgents with error");
                return;
            }
            if matches!(list_plan, Some(ListPlan::Timeout)) {
                std::future::pending::<()>().await;
            }
            let replacement_id = match list_plan {
                Some(ListPlan::Replacement(id)) => Some(id.to_string()),
                Some(ListPlan::DelayedReplacement {
                    replacement_id,
                    delay,
                }) => first_stop_at
                    .lock()
                    .unwrap()
                    .as_ref()
                    .filter(|started| started.elapsed() >= delay)
                    .map(|_| replacement_id.to_string()),
                Some(ListPlan::Empty | ListPlan::Error(_) | ListPlan::Timeout) => None,
                None => Some(format!("agent-{}", slot_owner.load(Ordering::SeqCst))),
            };
            let records = replacement_id
                .map(|replacement_id| {
                    vec![AgentRecord {
                        id: replacement_id,
                        pane_id_env: pane_id_env.lock().unwrap().clone(),
                        display_name: None,
                        cwd: None,
                        tab_membership: None,
                        agent_type: None,
                        rows: 24,
                        cols: 80,
                        live: None,
                        spawned_at_ms: None,
                    }]
                })
                .unwrap_or_default();
            write_resp(&mut stream, &AttachResponse::agent_records(records))
                .await
                .expect("reply to ListAgents");
        }
        other => panic!("unexpected synthetic daemon request: {other:?}"),
    }
}

fn controller_for(
    daemon: &StopErrorDaemon,
) -> (tokio::runtime::Runtime, EmbeddedPaneController, String) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("controller runtime");
    let controller =
        EmbeddedPaneController::new(daemon.path().to_path_buf(), runtime.handle().clone());
    let pane_id = controller
        .create_pane(Some("cat"), Some("/tmp"))
        .expect("synthetic daemon should create and attach the pane");
    assert_eq!(controller.pane_ids(), vec![pane_id.clone()]);
    (runtime, controller, pane_id)
}

fn assert_unverified_warning(warnings: Vec<String>, pane_id: &str, reason: &str) {
    assert_eq!(
        warnings.len(),
        1,
        "an unverified close must produce exactly one warning: {warnings:?}"
    );
    let warning = &warnings[0];
    assert!(
        warning.contains(&format!("Closed pane {pane_id}")),
        "the warning must say that the requested pane was closed: {warning}"
    );
    assert!(
        warning.contains("could not query the daemon"),
        "the warning must identify the close as unverified: {warning}"
    );
    assert!(
        warning.contains("may still be running unattended"),
        "the warning must name the unattended-agent risk: {warning}"
    );
    assert!(
        warning.contains(reason),
        "the warning must explain why verification failed: {warning}"
    );
}

fn assert_slot_churn_warning(warnings: Vec<String>, pane_id: &str) {
    assert_eq!(
        warnings.len(),
        1,
        "bounded pane-slot churn must produce exactly one warning: {warnings:?}"
    );
    let warning = &warnings[0];
    assert!(
        warning.contains(&format!("Closed pane {pane_id}")),
        "the warning must say that the requested pane was closed: {warning}"
    );
    assert!(
        warning.contains("pane slot kept changing owners"),
        "the warning must identify repeated slot handovers: {warning}"
    );
    assert!(
        warning.contains("close could not be verified"),
        "the warning must identify the close as unverified: {warning}"
    );
    assert!(
        warning.contains("may still be running unattended"),
        "the warning must name the unattended-agent risk: {warning}"
    );
}

type BoundedClose = (
    tokio::runtime::Runtime,
    EmbeddedPaneController,
    String,
    Result<(), PaneError>,
    Duration,
);

fn close_with_deadline(
    runtime: tokio::runtime::Runtime,
    controller: EmbeddedPaneController,
    pane_id: String,
    deadline: Duration,
) -> BoundedClose {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let started = Instant::now();
        let result = controller.close_pane(&pane_id);
        let elapsed = started.elapsed();
        sender
            .send((runtime, controller, pane_id, result, elapsed))
            .expect("return bounded close result to test thread");
    });

    let closed = receiver
        .recv_timeout(deadline)
        .unwrap_or_else(|error| panic!("close_pane did not return within {deadline:?}: {error}"));
    worker.join().expect("join bounded close test thread");
    closed
}

/// Scenario: Create and attach a pane through a synthetic daemon, then have both StopAgent attempts report that the agent is not found and ListAgents prove its stable slot empty. Closing must remove the ghost card successfully without producing an unverified-close warning.
#[spec("lifecycle/stop/005")]
#[test]
fn stop_005_not_found_completes_teardown() {
    let daemon = StopErrorDaemon::spawn("Agent agent-1 not found");
    let (_runtime, controller, pane_id) = controller_for(&daemon);

    let result = controller.close_pane(&pane_id);

    assert!(
        result.is_ok(),
        "an already-gone daemon agent must count as a successful close, got {result:?}"
    );
    assert!(
        controller.pane_ids().is_empty(),
        "a successful already-stopped close must not re-insert the pane"
    );
    assert_eq!(
        daemon.stop_requests(),
        vec!["agent-1", "agent-1"],
        "the existing reattach-race retry remains before NotFound is classified as already stopped"
    );
    assert!(
        daemon.list_requests() > 0,
        "the test must exercise the real pane-slot resolution after both NotFound replies"
    );
    assert!(
        controller.take_close_warnings().is_empty(),
        "a daemon-verified empty slot must not produce an unverified-close warning"
    );
}

/// Scenario: Create and attach a pane through a synthetic daemon, then make StopAgent fail with a genuine server error. Closing must surface the error and retain the pane in the local registry so the user can see it and retry.
#[spec("lifecycle/stop/006")]
#[test]
fn stop_006_genuine_error_retains_pane() {
    let daemon = StopErrorDaemon::spawn("permission denied while stopping agent");
    let (_runtime, controller, pane_id) = controller_for(&daemon);

    let error = controller
        .close_pane(&pane_id)
        .expect_err("a genuine StopAgent error must remain visible");

    assert!(
        error.to_string().contains("permission denied"),
        "the surfaced close error must preserve the daemon's reason: {error}"
    );
    assert_eq!(
        controller.pane_ids(),
        vec![pane_id],
        "a genuine stop failure must re-insert the pane for retry"
    );
    assert_eq!(
        daemon.stop_requests(),
        vec!["agent-1"],
        "only NotFound receives the reattach-race retry"
    );
}

/// Scenario: Close live panes against daemon errors that contain `not found` but are not the exact requested-agent error. Every case must surface the original error, retain the pane for retry, and avoid the NotFound retry/slot-resolution path.
#[spec("lifecycle/stop/007")]
#[test]
fn stop_007_unrelated_not_found_errors_retain_live_pane() {
    for stop_error in [
        "pane not found",
        "session not found",
        "Agent agent-2 not found",
        "stop failed: Agent agent-1 not found",
    ] {
        let daemon = StopErrorDaemon::spawn(stop_error);
        let (_runtime, controller, pane_id) = controller_for(&daemon);

        let error = controller
            .close_pane(&pane_id)
            .expect_err("a merely-containing not-found error must remain visible");

        assert!(
            error.to_string().contains(stop_error),
            "the surfaced error must retain the daemon message {stop_error:?}: {error}"
        );
        assert_eq!(
            controller.pane_ids(),
            vec![pane_id],
            "a live pane must be retained for {stop_error:?}"
        );
        assert_eq!(
            daemon.stop_requests(),
            vec!["agent-1"],
            "an unrelated not-found message must not enter the exact-agent retry path"
        );
        assert_eq!(
            daemon.list_requests(),
            0,
            "slot resolution is reserved for exact id-scoped Agent not found replies"
        );
    }
}

/// Scenario: Create a pane whose original agent disappears while a replacement now owns the same stable pane slot. Close must discover and stop the replacement before teardown, without producing an unverified-close warning.
#[spec("lifecycle/stop/008")]
#[test]
fn stop_008_replacement_slot_owner_is_stopped() {
    let daemon = StopErrorDaemon::spawn_with_replacement("agent-2");
    let (_runtime, controller, pane_id) = controller_for(&daemon);

    let result = controller.close_pane(&pane_id);

    assert!(result.is_ok(), "replacement-aware close failed: {result:?}");
    assert!(
        controller.pane_ids().is_empty(),
        "the pane may be removed only after its replacement agent is stopped"
    );
    assert_eq!(
        daemon.stop_requests(),
        vec!["agent-1", "agent-1", "agent-2"],
        "the close path must stop the replacement discovered in the pane slot"
    );
    assert!(
        daemon.list_requests() > 0,
        "the replacement must be discovered through the real ListAgents path"
    );
    assert!(
        controller.take_close_warnings().is_empty(),
        "a replacement that was found and stopped must not produce an unverified-close warning"
    );
}

/// Scenario: End a pane's attach stream so it starts looking for a respawn, then begin closing while the daemon reports the slot empty. A replacement appearing about five seconds later must still be discovered and stopped before the pane is removed.
#[spec("lifecycle/stop/009")]
#[test]
fn stop_009_late_replacement_is_stopped() {
    let daemon = StopErrorDaemon::spawn_not_found(ListPlan::DelayedReplacement {
        replacement_id: "agent-2",
        delay: Duration::from_millis(4_800),
    });
    let (_runtime, controller, pane_id) = controller_for(&daemon);

    let reattach_deadline = Instant::now() + Duration::from_secs(1);
    while daemon.list_requests() == 0 && Instant::now() < reattach_deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        daemon.list_requests() > 0,
        "the ended attach stream must put the pane I/O task into its real reattaching state"
    );

    let result = controller.close_pane(&pane_id);

    assert!(result.is_ok(), "late-replacement close failed: {result:?}");
    assert!(
        controller.pane_ids().is_empty(),
        "the pane may be removed only after its late replacement is stopped"
    );
    assert_eq!(
        daemon.stop_requests(),
        vec!["agent-1", "agent-1", "agent-2"],
        "a replacement appearing near the respawn worst case must not be orphaned"
    );
    assert!(
        controller.take_close_warnings().is_empty(),
        "a late replacement that was found and stopped is still a verified close"
    );
}

/// Scenario: After both StopAgent attempts report exact NotFound, make ListAgents return a daemon error. Closing must remove the pane successfully and queue exactly one warning that explains the unverified close and unattended-agent risk.
#[spec("lifecycle/stop/010")]
#[test]
fn stop_010_list_failure_completes_with_warning() {
    let daemon = StopErrorDaemon::spawn_not_found(ListPlan::Error("registry unavailable"));
    let (_runtime, controller, pane_id) = controller_for(&daemon);

    let result = controller.close_pane(&pane_id);

    assert!(result.is_ok(), "unverifiable close failed: {result:?}");
    assert!(
        controller.pane_ids().is_empty(),
        "a ListAgents error must not restore the ghost card"
    );
    assert_unverified_warning(
        controller.take_close_warnings(),
        &pane_id,
        "registry unavailable",
    );
    assert!(
        controller.take_close_warnings().is_empty(),
        "the warning queue must drain exactly once"
    );
}

/// Scenario: After both StopAgent attempts report exact NotFound, leave ListAgents wedged past its timeout. Closing must remove the pane successfully and queue exactly one warning that explains the timed-out verification and unattended-agent risk.
#[spec("lifecycle/stop/011")]
#[test]
fn stop_011_list_timeout_completes_with_warning() {
    let daemon = StopErrorDaemon::spawn_not_found(ListPlan::Timeout);
    let (_runtime, controller, pane_id) = controller_for(&daemon);

    let result = controller.close_pane(&pane_id);

    assert!(result.is_ok(), "timed-out verification failed: {result:?}");
    assert!(
        controller.pane_ids().is_empty(),
        "a ListAgents timeout must not restore the ghost card"
    );
    assert_unverified_warning(controller.take_close_warnings(), &pane_id, "timed out");
    assert!(
        controller.take_close_warnings().is_empty(),
        "the timeout warning queue must drain exactly once"
    );
}

/// Scenario: Make the original pane agent disappear, then have replacement B disappear while its StopAgent request is in flight because replacement C takes over the same slot. Closing must chase the handover through C, stop that last owner, remove the pane, and emit no warning for the verified close.
#[spec("lifecycle/stop/012")]
#[test]
fn stop_012_chained_handover_stops_last_slot_owner() {
    let daemon = StopErrorDaemon::spawn_chained_handover();
    let (_runtime, controller, pane_id) = controller_for(&daemon);

    let result = controller.close_pane(&pane_id);

    assert!(result.is_ok(), "chained-handover close failed: {result:?}");
    assert!(
        daemon.stop_requests().iter().any(|id| id == "agent-3"),
        "the close must send StopAgent to C, the last owner of the pane slot: {:?}",
        daemon.stop_requests()
    );
    assert!(
        controller.pane_ids().is_empty(),
        "the pane must be removed after the last slot owner is stopped"
    );
    assert!(
        controller.take_close_warnings().is_empty(),
        "a close verified after chasing a handover must not be announced as unverified"
    );
}

/// Scenario: Make every StopAgent request lose a race to a fresh owner of the same pane slot, with immediate synthetic replies. Closing must return quickly at its replacement-round bound, remove the pane, and queue exactly one warning about the unverified close and unattended-agent risk.
#[spec("lifecycle/stop/013")]
#[test]
fn stop_013_fast_slot_churn_is_bounded_and_announced() {
    let daemon = StopErrorDaemon::spawn_endless_churn(Duration::ZERO);
    let (runtime, controller, pane_id) = controller_for(&daemon);

    let (_runtime, controller, pane_id, result, elapsed) =
        close_with_deadline(runtime, controller, pane_id, Duration::from_secs(13));

    assert!(result.is_ok(), "bounded churn close failed: {result:?}");
    assert!(
        elapsed < Duration::from_secs(3),
        "immediate churn must hit the replacement-round cap instead of waiting for the total budget: {elapsed:?}"
    );
    let replacement_stops = daemon
        .stop_requests()
        .into_iter()
        .filter(|id| id != "agent-1")
        .count();
    assert_eq!(
        replacement_stops, 3,
        "the synthetic fast-churn path must exercise the three-round cap"
    );
    assert!(
        controller.pane_ids().is_empty(),
        "bound exhaustion completes teardown instead of restoring a ghost card"
    );
    assert_slot_churn_warning(controller.take_close_warnings(), &pane_id);
    assert!(
        controller.take_close_warnings().is_empty(),
        "the churn warning queue must drain exactly once"
    );
}

/// Scenario: Make each attempted replacement stop take four seconds and then lose to another owner of the pane slot. Closing must return before a third slow reply can finish, proving the total wall-clock budget is distinct from the round cap, while removing the pane and announcing the unverified close once.
#[spec("lifecycle/stop/014")]
#[test]
fn stop_014_slow_slot_churn_hits_total_budget() {
    let daemon = StopErrorDaemon::spawn_endless_churn(Duration::from_secs(4));
    let (runtime, controller, pane_id) = controller_for(&daemon);

    let (_runtime, controller, pane_id, result, elapsed) =
        close_with_deadline(runtime, controller, pane_id, Duration::from_secs(13));

    assert!(
        result.is_ok(),
        "budget-bounded churn close failed: {result:?}"
    );
    assert!(
        elapsed >= Duration::from_millis(7_500),
        "the slow variant must wait for two delayed replacement replies: {elapsed:?}"
    );
    let replacement_stops = daemon
        .stop_requests()
        .into_iter()
        .filter(|id| id != "agent-1")
        .count();
    assert_eq!(
        replacement_stops, 2,
        "the total budget must stop slow churn before the three-round cap"
    );
    assert!(
        controller.pane_ids().is_empty(),
        "total-budget exhaustion completes teardown instead of restoring a ghost card"
    );
    assert_slot_churn_warning(controller.take_close_warnings(), &pane_id);
    assert!(
        controller.take_close_warnings().is_empty(),
        "the churn warning queue must drain exactly once"
    );
}

/// Scenario: Let replacement B take over after the original agent disappears, then make B's StopAgent request return a genuine server error. Closing must surface that error, retain the pane for retry, and never absorb the failure into the slot-churn warning path.
#[spec("lifecycle/stop/015")]
#[test]
fn stop_015_replacement_error_retains_pane() {
    let daemon = StopErrorDaemon::spawn_replacement_error(
        "permission denied while stopping replacement agent",
    );
    let (_runtime, controller, pane_id) = controller_for(&daemon);

    let error = controller
        .close_pane(&pane_id)
        .expect_err("a replacement StopAgent server error must remain visible");

    assert!(
        error.to_string().contains("permission denied"),
        "the surfaced close error must preserve the replacement stop failure: {error}"
    );
    assert!(
        daemon.stop_requests().iter().any(|id| id == "agent-2"),
        "the failure must come from stopping the replacement slot owner"
    );
    assert_eq!(
        controller.pane_ids(),
        vec![pane_id],
        "a replacement stop failure must retain the pane for retry"
    );
    assert!(
        controller.take_close_warnings().is_empty(),
        "a genuine stop failure must not be degraded into an unverified-close warning"
    );
}

/// Scenario: Let replacement B take over after the original agent disappears, then leave B's StopAgent request unanswered past the close timeout. Closing must return a timeout error, retain the pane for retry, and never absorb the timeout into the slot-churn warning path.
#[spec("lifecycle/stop/016")]
#[test]
fn stop_016_replacement_timeout_retains_pane() {
    let daemon = StopErrorDaemon::spawn_replacement_timeout();
    let (runtime, controller, pane_id) = controller_for(&daemon);

    let (_runtime, controller, pane_id, result, elapsed) =
        close_with_deadline(runtime, controller, pane_id, Duration::from_secs(7));
    let error = result.expect_err("a replacement StopAgent timeout must remain visible");

    assert!(
        elapsed >= Duration::from_millis(4_500),
        "the test must exercise the real replacement StopAgent timeout: {elapsed:?}"
    );
    assert!(
        error.to_string().contains("timed out"),
        "the surfaced close error must identify the replacement stop timeout: {error}"
    );
    assert!(
        daemon.stop_requests().iter().any(|id| id == "agent-2"),
        "the timeout must come from stopping the replacement slot owner"
    );
    assert_eq!(
        controller.pane_ids(),
        vec![pane_id],
        "a replacement stop timeout must retain the pane for retry"
    );
    assert!(
        controller.take_close_warnings().is_empty(),
        "a stop timeout must not be degraded into an unverified-close warning"
    );
}

struct DelayedCloseController {
    delays: std::collections::HashMap<String, Duration>,
}

impl PaneController for DelayedCloseController {
    fn focus_pane(&self, _pane_id: &str) -> Result<(), PaneError> {
        Ok(())
    }

    fn close_pane(&self, pane_id: &str) -> Result<(), PaneError> {
        let delay = self
            .delays
            .get(pane_id)
            .copied()
            .expect("every orchestration pane has a scripted close delay");
        std::thread::sleep(delay);
        Ok(())
    }

    fn create_pane(&self, _command: Option<&str>, _cwd: Option<&str>) -> Result<String, PaneError> {
        Err(PaneError::NotAvailable)
    }

    fn list_panes(&self) -> Result<Vec<PaneInfo>, PaneError> {
        Ok(Vec::new())
    }

    fn resize_pane(
        &self,
        _pane_id: &str,
        _direction: PaneDirection,
        _amount: u16,
    ) -> Result<(), PaneError> {
        Ok(())
    }

    fn rename_pane(&self, _pane_id: &str, name: &str) -> Result<RenameOutcome, PaneError> {
        Ok(RenameOutcome::applied(name))
    }

    fn toggle_layout(&self) -> Result<(), PaneError> {
        Ok(())
    }

    fn write_to_pane(&self, _pane_id: &str, _text: &str) -> Result<(), PaneError> {
        Ok(())
    }

    fn name(&self) -> &str {
        "delayed-close"
    }

    fn is_available(&self) -> bool {
        true
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

fn six_role_orchestration() -> OrchestrationConfig {
    OrchestrationConfig {
        default: false,
        name: "six-role-close".to_string(),
        roles: (0..6)
            .map(|index| OrchestrationRoleConfig {
                agent: None,
                name: format!("role-{index}"),
                command: "cat".to_string(),
                start: index == 0,
                description: None,
                prompt_template: None,
                clear: false,
            })
            .collect(),
    }
}

/// Scenario: Close a hydrated six-role orchestration whose panes each sleep for a distinct known delay before succeeding. The whole tab must finish well below the sequential sum, and the returned closed ids must remain in role order rather than completion order.
#[spec("lifecycle/stop/019")]
#[test]
fn stop_019_tab_close_is_concurrent_and_keeps_pane_order() {
    let pane_ids: Vec<String> = (0..6).map(|index| format!("pane-{index}")).collect();
    let scripted_delays = [400, 350, 300, 250, 200, 150]
        .into_iter()
        .map(Duration::from_millis);
    let delays = pane_ids
        .iter()
        .cloned()
        .zip(scripted_delays)
        .collect::<std::collections::HashMap<_, _>>();
    let controller = Arc::new(DelayedCloseController { delays });
    let mut tabs = TabManager::new(controller);
    let (tab_index, _) = tabs
        .open_orchestration_tab_with_existing_role_panes(
            &six_role_orchestration(),
            "/work",
            pane_ids.iter().cloned().map(Some).collect(),
            None,
        )
        .expect("open hydrated six-role orchestration");

    let started = Instant::now();
    let outcome = tabs.close_tab(tab_index).expect("close orchestration tab");
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(1),
        "six closes totaling 1.65s sequentially must fan out concurrently under the 1.0s ceiling; elapsed {elapsed:?}"
    );
    assert_eq!(
        outcome.closed, pane_ids,
        "close results must retain role/pane input order despite staggered completion"
    );
    assert!(outcome.failed.is_empty());
    assert_eq!(tabs.tab_count(), 1, "a clean close removes the whole tab");
    eprintln!("six-pane concurrent tab close elapsed {elapsed:?} (ceiling 1s)");
}
