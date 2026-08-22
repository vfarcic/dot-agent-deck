// Unix-only at the source level for the reason `idle_worker_detector.rs` states:
// every test here spawns a real PTY running a POSIX-shell stub (`stty -echo
// -icanon`, `printf`, `exec cat -u` under a pinned `SHELL=/bin/sh`), none of
// which exists on Windows. This file is FAST tier, so CI's Windows job compiles
// it — `#![cfg(unix)]` makes the crate empty there instead of failing to build.
#![cfg(unix)]
//! Fast-tier behavioral coverage for what the daemon TELLS THE ORCHESTRATOR when
//! one of its workers reports `work-done` (issues #448 and #433).
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
    AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, SpawnOptions, TabMembership,
};
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
