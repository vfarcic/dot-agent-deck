//! PRD #89 M2.2 — daemon-vs-snapshot restore precedence (in-process).
//!
//! Auto-restore on startup tries daemon hydration first; if hydration produced
//! any panes the daemon state wins and the disk snapshot is skipped, otherwise
//! the snapshot is applied. The decision is a pure, structural check on
//! `AppState.managed_pane_ids` exposed as `ui::should_apply_snapshot`, so it can
//! be pinned in-process here without the cross-deck PTY hydration primitive an
//! end-to-end L2 would need (see the tester handoff for `session/restore/005`).
//!
//! This is an in-crate integration test (style of `tests/rehydration.rs`): it
//! runs in the fast tier (`cargo test-fast`), not the `e2e` PTY suite.

use dot_agent_deck::state::AppState;
use dot_agent_deck::ui::should_apply_snapshot;
use spec::spec;

/// Scenario: Drive the `should_apply_snapshot` precedence seam directly. With a
/// fresh `AppState` that has no hydrated managed panes (the daemon-empty case),
/// it must return `true` so the disk snapshot is applied. After registering a
/// single hydrated managed pane id (simulating one pane produced by daemon
/// hydration), it must return `false` so the snapshot restore is skipped and the
/// daemon state wins — pinning the M2.2 precedence without double-restoring.
#[spec("session/restore/005")]
#[test]
fn restore_005_daemon_hydration_wins_over_snapshot() {
    // Zero hydrated managed panes → daemon empty → apply the disk snapshot.
    let mut state = AppState::default();
    assert!(
        should_apply_snapshot(&state),
        "PRD #89 M2.2: with no hydrated managed panes the daemon is empty, so the disk \
         snapshot must be applied (should_apply_snapshot == true)"
    );

    // At least one hydrated managed pane id present → daemon owns the workspace
    // → snapshot restore is skipped so panes are not double-restored.
    state.register_pane("agent-hydrated-1".to_string());
    assert!(
        !should_apply_snapshot(&state),
        "PRD #89 M2.2: once daemon hydration registered a managed pane the daemon state \
         wins, so the snapshot must be skipped (should_apply_snapshot == false)"
    );

    // Additional hydrated panes do not flip the decision back.
    state.register_pane("agent-hydrated-2".to_string());
    assert!(
        !should_apply_snapshot(&state),
        "PRD #89 M2.2: multiple hydrated managed panes must still skip the snapshot"
    );
}

use dot_agent_deck::config::SavedPane;
use dot_agent_deck::features::{self, Features};
use dot_agent_deck::interrupted_work::{InterruptedList, ItemAgent, RestoreReason, RestoreVerdict};
use dot_agent_deck::launch_records::LaunchRecord;
use dot_agent_deck::ui::should_defer_saved_agent_restore;
use dot_agent_deck::work_journal::{AttemptId, WorkId};
use serde_json::json;

static SAVED_AGENT_FLAG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct SavedAgentFlag(Features);
impl Drop for SavedAgentFlag {
    fn drop(&mut self) {
        features::set_for_test(self.0);
    }
}

/// Scenario: On a fresh TUI startup with no hydrated panes, a saved pane matching a resumable agent's recorded directory and exact command is left for the restore question instead of starting blank, with the experimental flag pinned off. Changing either match field, or having only a Start-over verdict keeps the ordinary saved-pane start, and an unrelated launch record cannot suppress it.
#[spec("journal/restore/018")]
#[test]
fn journal_restore_018_saved_panes_defer_only_an_exact_restorable_launch() {
    let _lock = SAVED_AGENT_FLAG_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _flag = SavedAgentFlag(features::current());
    let dir = "/home/me/code/restore-app";
    let command = "exec ./start-agent.sh --model small";
    let work = "w-abcdefgh-0000000000000018";
    let attempt = "restore-saved/18";
    let saved = SavedPane {
        dir: dir.into(),
        name: "saved-name-can-change".into(),
        command: command.into(),
        mode: None,
        orchestration: None,
    };
    let launch = LaunchRecord {
        v: dot_agent_deck::launch_records::SCHEMA_VERSION,
        work: WorkId::parse(work).unwrap(),
        attempt: AttemptId::parse(attempt).unwrap(),
        command: Some(command.into()),
        shell: None,
        agent_type: Some(dot_agent_deck::event::AgentType::ClaudeCode),
        cwd: Some(dir.into()),
        cwd_moved_from: None,
        name: Some("original-name".into()),
        role: None,
        orchestration: None,
        orchestration_id: None,
        ts_ms: 1795400000000,
        pid: Some(u32::MAX),
        pid_start: Some(1),
        launch_nonce: None,
    };
    let mut interrupted: InterruptedList = serde_json::from_value(json!({
        "items":[{
            "id":work, "kind":"agent", "name":"original-name", "worktree":dir,
            "last_entry":{"kind":"attempt_ended", "ts_ms":1795400000000_u64},
            "agents":[]
        }],
        "incomplete":false, "omitted":0
    }))
    .unwrap();
    interrupted.items[0].agents = vec![ItemAgent {
        attempt: launch.attempt.clone(),
        name: launch.name.clone(),
        agent_type: launch.agent_type.clone(),
        role: None,
        folder: launch.cwd.clone(),
        restore: RestoreVerdict::Resume,
    }];

    // Exercise the same startup branch as SavedSession::load on a fresh daemon.
    assert!(should_apply_snapshot(&AppState::default()));
    features::set_for_test(Features::test_with(false));
    assert!(
        should_defer_saved_agent_restore(&saved, &interrupted, std::slice::from_ref(&launch)),
        "the matching command must not start blank before the user answers Restore, \
         with the experimental flag off"
    );
    for verdict in [
        RestoreVerdict::StartOver {
            reason: RestoreReason::AgentCannotResume,
        },
        RestoreVerdict::StartOver {
            reason: RestoreReason::NotResumed,
        },
        RestoreVerdict::NotRestorable {
            reason: RestoreReason::OrchestrationLater,
        },
        RestoreVerdict::Unknown,
    ] {
        let mut unavailable = interrupted.clone();
        unavailable.items[0].agents[0].restore = verdict;
        assert!(
            !should_defer_saved_agent_restore(&saved, &unavailable, std::slice::from_ref(&launch)),
            "only a Resume verdict defers saved-workspace startup: {verdict:?}"
        );
    }
    for changed in [
        SavedPane {
            dir: format!("{dir}-other"),
            ..saved.clone()
        },
        SavedPane {
            command: format!("{command} --verbose"),
            ..saved.clone()
        },
    ] {
        assert!(
            !should_defer_saved_agent_restore(
                &changed,
                &interrupted,
                std::slice::from_ref(&launch)
            ),
            "both directory and verbatim original command must match: {changed:?}"
        );
    }
    assert!(!should_defer_saved_agent_restore(
        &saved,
        &InterruptedList::default(),
        std::slice::from_ref(&launch)
    ));
    assert!(!should_defer_saved_agent_restore(&saved, &interrupted, &[]));
    let mut unrelated = launch.clone();
    unrelated.attempt = AttemptId::parse("restore-saved/19").unwrap();
    assert!(
        !should_defer_saved_agent_restore(&saved, &interrupted, &[unrelated]),
        "a same-command launch must belong to the listed resume attempt"
    );
    let mut wrong_work = launch;
    wrong_work.work = WorkId::parse("w-abcdefgh-0000000000000019").unwrap();
    assert!(
        !should_defer_saved_agent_restore(&saved, &interrupted, &[wrong_work]),
        "matching an attempt under the wrong work must not hide a saved pane"
    );
}
