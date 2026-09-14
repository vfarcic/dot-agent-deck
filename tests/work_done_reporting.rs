// Unix-only at the source level for the reason `idle_worker_detector.rs` states:
// every test here spawns a real PTY running a POSIX-shell stub (`stty -echo
// -icanon`, `printf`, `exec cat -u` under a pinned `SHELL=/bin/sh`), none of
// which exists on Windows. This file is FAST tier, so CI's Windows job compiles
// it — `#![cfg(unix)]` makes the crate empty there instead of failing to build.
#![cfg(unix)]
//! Fast-tier behavioral coverage for what the daemon TELLS ANOTHER PANE when a
//! unit reports `work-done`: ordinary worker-to-orchestrator feedback (issues
//! #448 and #433), plus a dispatched unit's retained return edge (PRD #220).
//!
//! These tests drive the real `AppState::handle_delegate` /
//! `AppState::handle_work_done` against daemon-owned PTYs, with the role maps
//! populated exactly as `StartAgent` would populate them. The orchestrator pane is
//! a raw, no-echo `cat`, so every byte the daemon submits into it appears exactly
//! once in its observable snapshot and nothing else does — which makes both
//! "this text was submitted" and "this text was NOT submitted" directly
//! observable. Worker panes are plain `cat`.
//!
//! The first three cases are the three things the old code could not tell apart:
//! a completion nobody commissioned (#448), a commissioned completion whose
//! summary file could not be written (#433), and a commissioned completion on a
//! project that has the idle detector switched OFF — which must still be reported
//! as the genuine completion it is.
//!
//! The fourth (`005`) guards the ledger's own failure mode rather than the old
//! code's: a delegate that never reached its worker must not leave a commission
//! standing, or the next uncommissioned completion spends it and #448 returns
//! through the mechanism added to prevent it.

use std::time::Duration;

use tempfile::TempDir;
use tokio::sync::broadcast;

use dot_agent_deck::agent_pty::{
    AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, GuardedSend, SpawnOptions, TabMembership,
};
use dot_agent_deck::dispatch_return::DispatchCaller;
use dot_agent_deck::event::{BroadcastMsg, DelegateSignal, WorkDoneSignal};
use dot_agent_deck::state::{AppState, OrchestrationIdentity};
use spec::spec;

mod common;

const ORCH_PANE: &str = "work-done-orchestrator-pane";
const ORCH_ROLE: &str = "orchestrator";
const WORKER_PANE: &str = "work-done-coder-pane";
const WORKER_ROLE: &str = "coder";
const ORCHESTRATION: &str = "work-done-test-orchestration";
const ORCHESTRATION_INSTANCE: &str = "work-done-test-orchestration-instance-1";

/// The daemon's unchanged happy-path pointer. Its ABSENCE is the assertion in two
/// of these tests: pointing an orchestrator at a file the daemon did not write is
/// exactly the #433 defect, and it is what the daemon used to do unconditionally.
const POINTER_NEEDLE: &str = "Read .dot-agent-deck/work-done-coder.md for their full report.";

/// The #448 label. Spelled out here rather than imported from `src/` so a silent
/// rewording of the daemon's own template fails these tests instead of following
/// them — the same discipline as `idle_worker_detector.rs`'s `IDLE_NEEDLE`.
const UNSOLICITED_NEEDLE: &str = "you have no outstanding delegation to that worker";

/// The #433 label, for a commissioned completion whose file could not be written.
const UNFILED_NEEDLE: &str = "could not write .dot-agent-deck/work-done-coder.md";

/// The daemon frames an inlined report as inert data, so matching the WRAPPED
/// opening marker — not just the report text — proves the text arrived through
/// the daemon's own template.
const REPORT_FRAME_NEEDLE: &str = "[UNTRUSTED-WORKER-REPORT:";

/// A previous delegation's report, already parked at the role-keyed path before
/// the test runs. This is the file #433 is about: when a write fails, THIS is what
/// an orchestrator following the pointer reads, and nothing in it says so.
const STALE_REPORT: &str = "Implemented the previous delegation. STALE-REPORT-BODY-7c41.";

/// A token unique to the report the worker sends *in* each test, so its appearance
/// in the orchestrator's pane proves the daemon inlined THIS report rather than
/// echoing anything else. Kept to `[a-z0-9-]` so it survives both the whitespace
/// collapse and the frame-breaking filter unchanged.
const FRESH_SENTINEL: &str = "fresh-report-body-a91f";

struct WorkDoneHarness {
    cwd: TempDir,
    registry: std::sync::Arc<AgentPtyRegistry>,
    state: AppState,
    event_tx: broadcast::Sender<BroadcastMsg>,
    orchestrator_agent_id: String,
}

impl WorkDoneHarness {
    /// One orchestrator pane plus one `coder` worker pane in a single
    /// orchestration, both in a fresh tempdir. `project_config` writes a
    /// `.dot-agent-deck.toml` into that directory when the test needs to move the
    /// detector seams; `None` leaves production defaults in force.
    async fn new(project_config: Option<&str>) -> Self {
        common::init_test_env();
        let cwd = common::race_safe_tempdir();
        if let Some(contents) = project_config {
            std::fs::write(cwd.path().join(".dot-agent-deck.toml"), contents)
                .expect("write project config");
        }
        let cwd_str = cwd.path().to_string_lossy().to_string();
        let registry = std::sync::Arc::new(AgentPtyRegistry::new());

        // Raw no-echo cat: one observable copy of every byte the daemon submits.
        // The readiness marker proves termios has already changed, so nothing the
        // test asserts on can be swallowed by the shell's own line discipline.
        let orchestrator_command =
            "stty -echo -icanon -icrnl -opost min 1 time 0 && printf ORCH-READY && exec cat -u";
        let orchestrator_agent_id = registry
            .spawn_agent(SpawnOptions {
                command: Some(orchestrator_command),
                cwd: Some(&cwd_str),
                env: vec![
                    (DOT_AGENT_DECK_PANE_ID.to_string(), ORCH_PANE.to_string()),
                    ("SHELL".to_string(), "/bin/sh".to_string()),
                ],
                tab_membership: Some(TabMembership::Orchestration {
                    name: ORCHESTRATION.to_string(),
                    role_index: 0,
                    role_name: ORCH_ROLE.to_string(),
                    is_start_role: true,
                    orchestration_cwd: Some(cwd_str.clone()),
                    display_title: None,
                    orchestration_id: Some(ORCHESTRATION_INSTANCE.to_string()),
                }),
                ..SpawnOptions::default()
            })
            .expect("spawn the orchestrator observer stub");
        registry
            .spawn_agent(SpawnOptions {
                command: Some("cat"),
                cwd: Some(&cwd_str),
                env: vec![
                    (DOT_AGENT_DECK_PANE_ID.to_string(), WORKER_PANE.to_string()),
                    ("SHELL".to_string(), "/bin/sh".to_string()),
                ],
                ..SpawnOptions::default()
            })
            .expect("spawn the coder worker stub");

        // PRD #140: the daemon's routing identity, in the `Instance` shape a
        // current client stamps.
        let orchestration = OrchestrationIdentity::Instance {
            id: ORCHESTRATION_INSTANCE.to_string(),
            name: ORCHESTRATION.to_string(),
        };
        let mut state = AppState::default();
        for (pane_id, role, is_orchestrator) in [
            (ORCH_PANE, ORCH_ROLE, true),
            (WORKER_PANE, WORKER_ROLE, false),
        ] {
            state.register_pane(pane_id.to_string());
            state
                .pane_role_map
                .insert(pane_id.to_string(), role.to_string());
            state
                .pane_orchestration_map
                .insert(pane_id.to_string(), orchestration.clone());
            state
                .pane_cwd_map
                .insert(pane_id.to_string(), cwd_str.clone());
            if is_orchestrator {
                state.orchestrator_pane_ids.insert(pane_id.to_string());
            }
        }

        let (event_tx, _event_rx) = broadcast::channel(64);
        let harness = Self {
            cwd,
            registry,
            state,
            event_tx,
            orchestrator_agent_id,
        };
        let ready = harness
            .wait_for_orchestrator(
                |snapshot| snapshot.contains("ORCH-READY"),
                Duration::from_secs(5),
            )
            .await;
        assert!(
            ready.contains("ORCH-READY"),
            "orchestrator raw-cat stub never became ready; snapshot = {ready:?}"
        );
        harness
    }

    /// The real delegate path: same signal `dot-agent-deck delegate --to coder`
    /// puts on the hook socket, handled by the real daemon-side handler, so the
    /// commission ledger is armed the way production arms it.
    async fn delegate(&self) {
        self.state
            .handle_delegate(
                DelegateSignal {
                    pane_id: ORCH_PANE.to_string(),
                    task: "Perform the delegated test task.".to_string(),
                    to: vec![WORKER_ROLE.to_string()],
                    timestamp: chrono::Utc::now(),
                },
                &self.registry,
                &self.event_tx,
            )
            .await;
    }

    /// The real work-done path: the signal `dot-agent-deck work-done --task-file`
    /// puts on the hook socket, from the WORKER's pane.
    async fn work_done(&self, summary: &str) {
        self.state
            .handle_work_done(
                WorkDoneSignal {
                    pane_id: WORKER_PANE.to_string(),
                    task: summary.to_string(),
                    done: false,
                    timestamp: chrono::Utc::now(),
                },
                &self.registry,
            )
            .await;
    }

    fn summary_path(&self) -> std::path::PathBuf {
        self.cwd.path().join(".dot-agent-deck/work-done-coder.md")
    }

    fn orchestrator_snapshot(&self) -> String {
        String::from_utf8_lossy(
            &self
                .registry
                .snapshot(&self.orchestrator_agent_id)
                .unwrap_or_default(),
        )
        .into_owned()
    }

    async fn wait_for_orchestrator(
        &self,
        predicate: impl Fn(&str) -> bool,
        timeout: Duration,
    ) -> String {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let snapshot = self.orchestrator_snapshot();
            if predicate(&snapshot) || tokio::time::Instant::now() >= deadline {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

impl Drop for WorkDoneHarness {
    fn drop(&mut self) {
        self.registry.shutdown_all();
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build multi-thread runtime")
}

/// Scenario: Park an earlier delegation's report at `.dot-agent-deck/work-done-coder.md`, then have the `coder` worker run `work-done` with NO delegation outstanding — the case of a human tasking a worker directly. The orchestrator pane must receive a report explicitly labelled as one it never commissioned, carrying the worker's text inline, and must NOT be told to read the summary file; the earlier report must still be on disk byte-for-byte.
#[spec("orchestration/work-done/001")]
#[test]
fn work_done_001_unsolicited_completion_is_labelled_and_clobbers_nothing() {
    runtime().block_on(async {
        let harness = WorkDoneHarness::new(None).await;
        std::fs::create_dir_all(harness.cwd.path().join(".dot-agent-deck"))
            .expect("create the coordination directory");
        std::fs::write(harness.summary_path(), STALE_REPORT).expect("park the earlier report");

        // No delegate: nothing was commissioned from this worker.
        harness
            .work_done(&format!("Did what a person asked me. {FRESH_SENTINEL}"))
            .await;

        let snapshot = harness
            .wait_for_orchestrator(
                |snapshot| snapshot.contains(UNSOLICITED_NEEDLE),
                Duration::from_secs(5),
            )
            .await;
        assert!(
            snapshot.contains(UNSOLICITED_NEEDLE),
            "an uncommissioned completion must be reported as such, not as delegated work coming \
             back; snapshot = {snapshot:?}"
        );
        assert!(
            !snapshot.contains(POINTER_NEEDLE),
            "the orchestrator must not be pointed at a file this completion did not write; \
             snapshot = {snapshot:?}"
        );
        assert!(
            snapshot.contains(REPORT_FRAME_NEEDLE) && snapshot.contains(FRESH_SENTINEL),
            "the report itself must still reach the orchestrator, framed as untrusted data; \
             snapshot = {snapshot:?}"
        );
        assert_eq!(
            std::fs::read_to_string(harness.summary_path()).expect("the earlier report survives"),
            STALE_REPORT,
            "an uncommissioned completion must not overwrite the last report the orchestrator DID \
             commission"
        );
    });
}

/// Scenario: On a project whose config sets `worker_response_timeout_minutes = 0` — the idle detector switched off, so neither delegation watch arms anything at all — delegate to `coder` and then have it report `work-done`. The orchestrator pane must receive the ordinary completion pointer, with no unsolicited label anywhere, and the summary file must hold the new report.
#[spec("orchestration/work-done/002")]
#[test]
fn work_done_002_disabled_idle_detector_still_reports_a_genuine_completion() {
    runtime().block_on(async {
        // The key sits above every table header on purpose: appended after one it
        // would silently become a key OF that table. The empty `roles` list keeps
        // `clear` unresolvable, so the delegate dispatches without respawning the
        // worker stub out from under the test.
        let harness = WorkDoneHarness::new(Some(
            "worker_response_timeout_minutes = 0\n\n[[orchestrations]]\nname = \"unused\"\nroles = []\n",
        ))
        .await;
        harness.delegate().await;
        harness
            .work_done(&format!("Finished the delegated task. {FRESH_SENTINEL}"))
            .await;

        let snapshot = harness
            .wait_for_orchestrator(
                |snapshot| snapshot.contains(POINTER_NEEDLE),
                Duration::from_secs(5),
            )
            .await;
        assert!(
            snapshot.contains(POINTER_NEEDLE),
            "a project with the idle detector OFF must still get its completion reported — \
             suppressing on 'no watch armed' would silently break every such project; \
             snapshot = {snapshot:?}"
        );
        assert!(
            !snapshot.contains(UNSOLICITED_NEEDLE),
            "a genuinely delegated completion must never be labelled unsolicited; \
             snapshot = {snapshot:?}"
        );
        assert!(
            !snapshot.contains(REPORT_FRAME_NEEDLE),
            "the happy path stays a short pointer; the report belongs in the file; \
             snapshot = {snapshot:?}"
        );
        assert!(
            std::fs::read_to_string(harness.summary_path())
                .expect("the summary file is written")
                .contains(FRESH_SENTINEL),
            "the file the orchestrator was pointed at must hold THIS report"
        );
    });
}

/// Scenario: Occupy `.dot-agent-deck` with a regular file so the daemon cannot create the directory or write the summary, then delegate to `coder` and have it report `work-done`. The orchestrator pane must be told the file could not be written and receive the report inline instead, and must never be pointed at the path the daemon failed to write.
#[spec("orchestration/work-done/003")]
#[test]
fn work_done_003_failed_summary_write_inlines_the_report_instead_of_pointing_at_it() {
    runtime().block_on(async {
        let harness = WorkDoneHarness::new(None).await;
        // A regular file where the coordination directory belongs: `create_dir_all`
        // and the write both fail with ENOTDIR/EEXIST, and they fail for uid 0 too,
        // so this holds in a container that runs the suite as root (a read-only
        // directory would not).
        std::fs::write(
            harness.cwd.path().join(".dot-agent-deck"),
            b"not a directory",
        )
        .expect("occupy the coordination path");

        harness.delegate().await;
        harness
            .work_done(&format!("Finished the delegated task. {FRESH_SENTINEL}"))
            .await;

        let snapshot = harness
            .wait_for_orchestrator(
                |snapshot| snapshot.contains(UNFILED_NEEDLE),
                Duration::from_secs(5),
            )
            .await;
        assert!(
            snapshot.contains(UNFILED_NEEDLE),
            "a summary the daemon could not write must be reported as missing, not vouched for; \
             snapshot = {snapshot:?}"
        );
        assert!(
            !snapshot.contains(POINTER_NEEDLE),
            "pointing at an unwritten path is the defect: whatever sits there belongs to an \
             earlier delegation; snapshot = {snapshot:?}"
        );
        assert!(
            snapshot.contains(REPORT_FRAME_NEEDLE) && snapshot.contains(FRESH_SENTINEL),
            "the report is still in memory when the write fails, so it must be inlined rather \
             than lost; snapshot = {snapshot:?}"
        );
    });
}

/// The daemon's own respawn-failure notice, and the test's synchronization edge:
/// `dispatch_one_owned` writes it into the orchestrator pane immediately before
/// the error return under audit, so observing it proves the dispatch task has
/// reached that arm and the test never has to guess at timing.
const RESPAWN_FAILED_NEEDLE: &str = "respawn failed for role 'coder'";

/// Scenario: On a project whose `coder` role sets `clear = true`, points at a binary that does not exist, and whose idle detector is switched off, delegate so the respawn kills the live worker and then fails to replace it, then have that same worker pane report `work-done` — the case of a person tasking it directly afterwards. The orchestrator pane must be told the respawn failed and must then report the completion as one it never commissioned, never pointing at a summary file.
#[spec("orchestration/work-done/005")]
#[test]
fn work_done_005_failed_respawn_does_not_leave_a_phantom_commission() {
    runtime().block_on(async {
        // Both detectors OFF (`worker_response_timeout_minutes = 0` disables the
        // idle watch and the silent-worker watch alike). That is the point of the
        // test as much as the respawn is: with no watch armed, the release under
        // audit is the ONLY thing that can discharge the commission, so a fix that
        // leaned on either detector would fail here.
        // The role command names a binary that does not exist, so the respawn
        // disposes of the live `cat` on the worker pane and then FAILS to bring
        // the replacement up — the production hazard this test is about, stated
        // literally. A single word with no shell metacharacters is exec'd
        // directly rather than through `$SHELL -c`, so the missing binary is an
        // `AgentPtyError::Spawn` from `spawn_agent` and not a shell exiting 127
        // (which would be a successful spawn of a child that then dies).
        //
        // Until issue #606 this test instead EVICTED the worker's agent and let
        // the respawn fail `NotFound`. That is no longer a failure: a
        // `clear = true` delegate to a pane whose agent is simply gone now
        // re-creates the worker rather than leaving the role unreachable, which
        // is exactly what #606 asked for. The commission-release behaviour under
        // audit here is unchanged; only the way the respawn is made to fail is.
        let harness = WorkDoneHarness::new(Some(&format!(
            "worker_response_timeout_minutes = 0\n\n\
             [[orchestrations]]\nname = \"{ORCHESTRATION}\"\n\n\
             [[orchestrations.roles]]\nname = \"{WORKER_ROLE}\"\n\
             command = \"/nonexistent-dot-agent-deck-respawn-target\"\nclear = true\n"
        )))
        .await;

        harness.delegate().await;
        let after_delegate = harness
            .wait_for_orchestrator(
                |snapshot| snapshot.contains(RESPAWN_FAILED_NEEDLE),
                Duration::from_secs(5),
            )
            .await;
        assert!(
            after_delegate.contains(RESPAWN_FAILED_NEEDLE),
            "the dispatch must reach the respawn-error arm for this test to be testing anything; \
             snapshot = {after_delegate:?}"
        );

        // The delegate never reached the worker, so a completion arriving now was
        // asked for by a person, not by the orchestrator.
        harness
            .work_done(&format!("A person asked me for this. {FRESH_SENTINEL}"))
            .await;

        let snapshot = harness
            .wait_for_orchestrator(
                |snapshot| snapshot.contains(UNSOLICITED_NEEDLE),
                Duration::from_secs(5),
            )
            .await;
        assert!(
            snapshot.contains(UNSOLICITED_NEEDLE),
            "a delegate that died on its respawn must release its commission — left standing, it \
             launders the next uncommissioned completion into a solicited one, which is #448 \
             through the very ledger added to prevent it; snapshot = {snapshot:?}"
        );
        assert!(
            !snapshot.contains(POINTER_NEEDLE),
            "and the laundered label brings the clobber with it: a solicited completion is \
             pointed at a summary file this one must never have written; snapshot = {snapshot:?}"
        );
        assert!(
            !harness.summary_path().exists(),
            "an uncommissioned completion writes no summary file at all"
        );
    });
}

/// A raw, no-echo `cat` successor for the orchestrator pane, carrying the same
/// registry `TabMembership` a legitimate in-place restart of that role would
/// carry — so the only thing separating it from its predecessor is the registry
/// agent id, which is exactly the input under test.
fn spawn_orchestrator_successor(
    registry: &std::sync::Arc<AgentPtyRegistry>,
    cwd: &str,
    marker: &str,
) -> String {
    let command =
        format!("stty -echo -icanon -icrnl -opost min 1 time 0 && printf {marker} && exec cat -u");
    registry
        .spawn_agent(SpawnOptions {
            command: Some(&command),
            cwd: Some(cwd),
            env: vec![
                (DOT_AGENT_DECK_PANE_ID.to_string(), ORCH_PANE.to_string()),
                ("SHELL".to_string(), "/bin/sh".to_string()),
            ],
            tab_membership: Some(TabMembership::Orchestration {
                name: ORCHESTRATION.to_string(),
                role_index: 0,
                role_name: ORCH_ROLE.to_string(),
                is_start_role: true,
                orchestration_cwd: Some(cwd.to_string()),
                display_title: None,
                orchestration_id: Some(ORCHESTRATION_INSTANCE.to_string()),
            }),
            ..SpawnOptions::default()
        })
        .expect("spawn the orchestrator successor stub")
}

/// The successor's readiness marker, and the barrier written into it after the
/// refusal. Distinct strings so neither can be mistaken for the other, and
/// neither is a substring of the feedback the test is proving absent.
const SUCCESSOR_READY: &str = "SUCCESSOR-ORCH-READY";
const SUCCESSOR_BARRIER: &str = "AUTHORIZED-WRITE-AFTER-THE-REFUSAL-4d02";

/// Scenario: Delegate to `coder` and let it report `work-done` once, proving this
/// fixture delivers the completion feedback into the orchestrator pane; then
/// delegate again and restart the orchestrator in place — the pane keeps its id,
/// role and orchestration, but a NEW agent owns it — before the worker reports.
/// The second completion must not be typed into that successor: its scrollback
/// must still hold its own readiness marker and a later authorized write, and
/// none of the feedback.
#[spec("orchestration/work-done/006")]
#[test]
fn work_done_006_feedback_is_refused_when_the_orchestrator_pane_changed_hands() {
    runtime().block_on(async {
        let harness = WorkDoneHarness::new(None).await;

        // --- Control. The same delegate → work-done pair this test then repeats
        // across a hand-over, so a later absence cannot be blamed on a fixture
        // that never delivered anything in the first place.
        harness.delegate().await;
        harness
            .work_done(&format!("Finished the first delegation. {FRESH_SENTINEL}"))
            .await;
        let delivered = harness
            .wait_for_orchestrator(
                |snapshot| snapshot.contains(POINTER_NEEDLE),
                Duration::from_secs(5),
            )
            .await;
        assert!(
            delivered.contains(POINTER_NEEDLE),
            "control: a commissioned completion must reach the orchestrator that commissioned \
             it, or the refusal asserted below is not a refusal of anything; snapshot = \
             {delivered:?}"
        );

        // --- The race. The commission is issued while the ORIGINAL orchestrator
        // owns the pane, so the feedback is bound to that agent...
        harness.delegate().await;

        // ...and then the orchestrator is restarted in place. `close_agent`
        // removes the record before the child dies, so the EOF sweep does not
        // run and the outstanding delegation survives the restart — which is
        // what leaves the identity gate as the only thing standing between the
        // completion and the new occupant.
        let cwd = harness.cwd.path().to_string_lossy().into_owned();
        harness
            .registry
            .close_agent(&harness.orchestrator_agent_id)
            .expect("close the commissioning orchestrator");
        let successor = spawn_orchestrator_successor(&harness.registry, &cwd, SUCCESSOR_READY);
        assert_ne!(
            successor, harness.orchestrator_agent_id,
            "the restart must produce a NEW registry agent id"
        );
        let snapshot_of = |agent_id: &str| {
            String::from_utf8_lossy(&harness.registry.snapshot(agent_id).unwrap_or_default())
                .into_owned()
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !snapshot_of(&successor).contains(SUCCESSOR_READY)
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            snapshot_of(&successor).contains(SUCCESSOR_READY),
            "precondition: the successor must be up and echoing, or the absence asserted below \
             proves only that its stub never started; snapshot = {:?}",
            snapshot_of(&successor)
        );

        harness
            .work_done(&format!("Finished the second delegation. {FRESH_SENTINEL}"))
            .await;

        // A barrier rather than a sleep: an authorized write that has
        // demonstrably arrived proves the successor's PTY has drained past the
        // point where leaked feedback would have landed.
        let barrier = harness
            .registry
            .write_and_submit_guarded(ORCH_PANE, SUCCESSOR_BARRIER, &successor, || async { true })
            .await
            .expect("the barrier write must reach the registry");
        assert_eq!(
            barrier,
            GuardedSend::Applied,
            "the successor owns the pane, so a write bound to IT must be applied — otherwise \
             this test proves nothing about the refusal"
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !snapshot_of(&successor).contains(SUCCESSOR_BARRIER)
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let snapshot = snapshot_of(&successor);
        assert!(
            snapshot.contains(SUCCESSOR_BARRIER),
            "the barrier write never reached the successor's PTY, so the absence below is \
             untested; snapshot = {snapshot:?}"
        );
        assert!(
            !snapshot.contains(POINTER_NEEDLE)
                && !snapshot.contains(UNSOLICITED_NEEDLE)
                && !snapshot.contains(REPORT_FRAME_NEEDLE),
            "a previous conversation's completion report was typed into — and submitted in — an \
             agent that merely inherited the orchestrator's pane id; snapshot = {snapshot:?}"
        );
    });
}

/// A shareable writer for the two dispatch-return tests' real tracing output.
#[derive(Clone)]
struct DispatchReturnLogWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for DispatchReturnLogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for DispatchReturnLogWriter {
    type Writer = DispatchReturnLogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn dispatch_return_log_buffer() -> &'static std::sync::Arc<std::sync::Mutex<Vec<u8>>> {
    static BUFFER: std::sync::OnceLock<std::sync::Arc<std::sync::Mutex<Vec<u8>>>> =
        std::sync::OnceLock::new();
    BUFFER.get_or_init(|| {
        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(DispatchReturnLogWriter(std::sync::Arc::clone(&buffer)))
            .with_max_level(tracing_subscriber::filter::LevelFilter::DEBUG)
            .with_ansi(false)
            .without_time()
            .finish();
        tracing::subscriber::set_global_default(subscriber)
            .expect("install the dispatch-return test log subscriber");
        buffer
    })
}

fn dispatch_return_logs() -> String {
    String::from_utf8(dispatch_return_log_buffer().lock().unwrap().clone())
        .expect("captured dispatch-return logs are UTF-8")
}

fn spawn_dispatch_return_observer(
    registry: &std::sync::Arc<AgentPtyRegistry>,
    cwd: &str,
    pane_id: &str,
    marker: &str,
) -> String {
    let command =
        format!("stty -echo -icanon -icrnl -opost min 1 time 0 && printf {marker} && exec cat -u");
    registry
        .spawn_agent(SpawnOptions {
            command: Some(&command),
            cwd: Some(cwd),
            env: vec![
                (DOT_AGENT_DECK_PANE_ID.to_string(), pane_id.to_string()),
                ("SHELL".to_string(), "/bin/sh".to_string()),
            ],
            ..SpawnOptions::default()
        })
        .unwrap_or_else(|error| panic!("spawn raw-cat observer on {pane_id}: {error}"))
}

fn dispatch_return_snapshot(registry: &AgentPtyRegistry, agent_id: &str) -> String {
    String::from_utf8_lossy(&registry.snapshot(agent_id).unwrap_or_default()).into_owned()
}

async fn wait_for_dispatch_return_snapshot(
    registry: &AgentPtyRegistry,
    agent_id: &str,
    needle: &str,
) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = dispatch_return_snapshot(registry, agent_id);
        if snapshot.contains(needle) || tokio::time::Instant::now() >= deadline {
            return snapshot;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn barrier_dispatch_return_observer(
    registry: &AgentPtyRegistry,
    pane_id: &str,
    agent_id: &str,
    barrier: &str,
) -> String {
    let outcome = registry
        .write_and_submit_guarded(pane_id, barrier, agent_id, || async { true })
        .await
        .expect("the barrier write must reach the registry");
    assert_eq!(
        outcome,
        GuardedSend::Applied,
        "the observer owns the pane, so its barrier write must be applied"
    );
    let snapshot = wait_for_dispatch_return_snapshot(registry, agent_id, barrier).await;
    assert!(
        snapshot.contains(barrier),
        "the barrier never reached the observer, so the absence under test is unproven; \
         snapshot = {snapshot:?}"
    );
    snapshot
}

struct DispatchReturnHarness {
    cwd: TempDir,
    registry: std::sync::Arc<AgentPtyRegistry>,
    state: AppState,
    caller_agent_id: String,
    unit_agent_id: String,
}

impl DispatchReturnHarness {
    async fn new(caller_pane: &str, caller_ready: &str, unit_pane: &str, unit_ready: &str) -> Self {
        common::init_test_env();
        let _ = dispatch_return_log_buffer();
        let cwd = common::race_safe_tempdir();
        let cwd_string = cwd.path().to_string_lossy().into_owned();
        let registry = std::sync::Arc::new(AgentPtyRegistry::new());
        let caller_agent_id =
            spawn_dispatch_return_observer(&registry, &cwd_string, caller_pane, caller_ready);
        let unit_agent_id =
            spawn_dispatch_return_observer(&registry, &cwd_string, unit_pane, unit_ready);

        for (label, agent_id, ready) in [
            ("caller", &caller_agent_id, caller_ready),
            ("unit", &unit_agent_id, unit_ready),
        ] {
            let snapshot = wait_for_dispatch_return_snapshot(&registry, agent_id, ready).await;
            assert!(
                snapshot.contains(ready),
                "precondition: the {label} observer did not become ready; snapshot = {snapshot:?}"
            );
        }

        Self {
            cwd,
            registry,
            state: AppState::default(),
            caller_agent_id,
            unit_agent_id,
        }
    }

    fn cwd_string(&self) -> String {
        self.cwd.path().to_string_lossy().into_owned()
    }

    fn register(&self, caller_pane: &str, unit_pane: &str, unit_name: &str) {
        self.registry.register_dispatch_return(
            unit_pane,
            &self.unit_agent_id,
            DispatchCaller {
                pane_id: caller_pane.to_string(),
                agent_id: self.caller_agent_id.clone(),
                unit_name: unit_name.to_string(),
            },
        );
        assert_eq!(
            self.registry.outstanding_dispatch_returns(),
            1,
            "precondition: registering the dispatched unit must retain exactly one return edge"
        );
    }

    async fn complete(&self, unit_pane: &str, report: &str) {
        self.state
            .handle_work_done(
                WorkDoneSignal {
                    pane_id: unit_pane.to_string(),
                    task: report.to_string(),
                    done: true,
                    timestamp: chrono::Utc::now(),
                },
                &self.registry,
            )
            .await;
    }
}

impl Drop for DispatchReturnHarness {
    fn drop(&mut self) {
        self.registry.shutdown_all();
    }
}

/// Scenario: Retain a dispatched unit's completion recipient, replace that caller with a different agent on the same pane id, and then complete the unit. The completion must be refused as the old caller's session, leave no bytes in the successor, and consume the return edge without retrying it.
#[spec("dispatch/return/004")]
#[test]
fn dispatch_return_004_completion_is_refused_when_the_caller_pane_changed_hands() {
    runtime().block_on(async {
        const CALLER_PANE: &str = "dispatch-return-004-caller";
        const UNIT_PANE: &str = "dispatch-return-004-unit";
        const UNIT: &str = "return-handover-unit-4f27";
        const REPORT: &str = "return-handover-report-must-not-leak-8ab1";
        const CALLER_READY: &str = "RETURN-004-CALLER-READY";
        const UNIT_READY: &str = "RETURN-004-UNIT-READY";
        const SUCCESSOR_READY: &str = "RETURN-004-SUCCESSOR-READY";
        const BARRIER: &str = "RETURN-004-AUTHORIZED-BARRIER";

        let harness =
            DispatchReturnHarness::new(CALLER_PANE, CALLER_READY, UNIT_PANE, UNIT_READY).await;
        harness.register(CALLER_PANE, UNIT_PANE, UNIT);

        harness
            .registry
            .close_agent(&harness.caller_agent_id)
            .expect("close the caller without the deliberate pane-close sweep");
        let successor = spawn_dispatch_return_observer(
            &harness.registry,
            &harness.cwd_string(),
            CALLER_PANE,
            SUCCESSOR_READY,
        );
        assert_ne!(
            successor, harness.caller_agent_id,
            "the pane hand-over must mint a different registry agent id"
        );
        let ready =
            wait_for_dispatch_return_snapshot(&harness.registry, &successor, SUCCESSOR_READY).await;
        assert!(
            ready.contains(SUCCESSOR_READY),
            "precondition: the successor did not become ready; snapshot = {ready:?}"
        );

        harness.complete(UNIT_PANE, REPORT).await;
        assert_eq!(
            harness.registry.outstanding_dispatch_returns(),
            0,
            "a refused completion is terminal and must consume its retained return edge"
        );

        let snapshot =
            barrier_dispatch_return_observer(&harness.registry, CALLER_PANE, &successor, BARRIER)
                .await;
        assert!(
            !snapshot.contains("dispatch:")
                && !snapshot.contains(UNIT)
                && !snapshot.contains(REPORT),
            "the completion was written into a different agent that merely inherited the caller's \
             pane id; successor snapshot = {snapshot:?}"
        );

        let log = dispatch_return_logs();
        assert!(
            log.lines().any(|line| {
                line.contains(CALLER_PANE)
                    && line.contains("dispatch: identity gate refused the result")
                    && line.contains("WrongSession")
                    && line.contains("nothing written")
            }),
            "the completion must be observably refused as WrongSession, with nothing written; \
             captured log = {log:?}"
        );
    });
}

/// Scenario: Retain a dispatched unit's return edge, deliberately close its caller pane, and then let the still-live unit complete while an unrelated pane is observable. The close must evict the edge, the late completion must take the logged unknown-pane drop path without panicking, and no report bytes may reach the unrelated pane.
#[spec("dispatch/return/005")]
#[test]
fn dispatch_return_005_completion_is_dropped_after_the_caller_pane_is_gone() {
    runtime().block_on(async {
        const CALLER_PANE: &str = "dispatch-return-005-caller";
        const UNIT_PANE: &str = "dispatch-return-005-unit";
        const ALTERNATE_PANE: &str = "dispatch-return-005-alternate";
        const UNIT: &str = "return-gone-unit-6c39";
        const REPORT: &str = "return-gone-report-must-not-reroute-5de2";
        const CALLER_READY: &str = "RETURN-005-CALLER-READY";
        const UNIT_READY: &str = "RETURN-005-UNIT-READY";
        const ALTERNATE_READY: &str = "RETURN-005-ALTERNATE-READY";
        const BARRIER: &str = "RETURN-005-AUTHORIZED-BARRIER";

        let harness =
            DispatchReturnHarness::new(CALLER_PANE, CALLER_READY, UNIT_PANE, UNIT_READY).await;
        let alternate = spawn_dispatch_return_observer(
            &harness.registry,
            &harness.cwd_string(),
            ALTERNATE_PANE,
            ALTERNATE_READY,
        );
        let ready =
            wait_for_dispatch_return_snapshot(&harness.registry, &alternate, ALTERNATE_READY).await;
        assert!(
            ready.contains(ALTERNATE_READY),
            "precondition: the alternate observer did not become ready; snapshot = {ready:?}"
        );
        harness.register(CALLER_PANE, UNIT_PANE, UNIT);

        drop(harness.registry.begin_pane_close(CALLER_PANE));
        assert_eq!(
            harness.registry.outstanding_dispatch_returns(),
            0,
            "begin_pane_close must evict every retained return edge whose caller is going away"
        );
        harness
            .registry
            .close_agent(&harness.caller_agent_id)
            .expect("close the caller after its pane-scoped sweep");
        drop(harness.registry.finish_pane_close(CALLER_PANE, true));

        harness.complete(UNIT_PANE, REPORT).await;
        assert_eq!(
            harness.registry.outstanding_dispatch_returns(),
            0,
            "the late completion must not recreate or reroute an evicted return edge"
        );

        let snapshot = barrier_dispatch_return_observer(
            &harness.registry,
            ALTERNATE_PANE,
            &alternate,
            BARRIER,
        )
        .await;
        assert!(
            !snapshot.contains("dispatch:")
                && !snapshot.contains(UNIT)
                && !snapshot.contains(REPORT),
            "a completion with no caller was rerouted into an unrelated pane; alternate snapshot = \
             {snapshot:?}"
        );

        let log = dispatch_return_logs();
        assert!(
            log.lines().any(|line| {
                line.contains(CALLER_PANE)
                    && line
                        .contains("pane close: dropped dispatch return entries touching this pane")
            }),
            "the caller close must log that it dropped the retained return edge; captured log = \
             {log:?}"
        );
        assert!(
            log.lines().any(|line| {
                line.contains(UNIT_PANE) && line.contains("work-done from unknown pane")
            }),
            "the late completion must be dropped on the existing logged unknown-pane path; \
             captured log = {log:?}"
        );
    });
}
