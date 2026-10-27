//! Issue #1913: a deck restart whose orchestration reuses the pane ids of an
//! orchestration tab this TUI already shows. The restarted deck's roles belong
//! in a tab of their own, as a freshly attached TUI would show them, not as
//! flat cards on the Dashboard.

use spec::spec;
use tokio::sync::RwLock;

use super::*;
use crate::daemon_client::Incarnation;
use crate::daemon_client::restart_fixture::RestartingDaemon;
use crate::embedded_pane::{EmbeddedPaneController, PaneLostReason};

fn named(process: &str) -> Incarnation {
    Incarnation::from(Some(process.to_string()))
}

/// The deck's listing of its one orchestration: `team`'s orchestrator in
/// `pane-1`, the pane and agent ids the restart fixture serves on every
/// process, under the per-run token `orchestration_id`.
fn listed(cwd: &str, orchestration_id: &str) -> Vec<crate::agent_pty::AgentRecord> {
    serde_json::from_value(serde_json::json!([
        { "id": "1", "pane_id_env": "pane-1", "rows": 24, "cols": 80, "tab_membership": {
            "kind": "orchestration", "name": "team", "role_index": 0,
            "role_name": "orchestrator", "is_start_role": true,
            "orchestration_cwd": cwd, "orchestration_id": orchestration_id } },
    ]))
    .unwrap()
}

/// One render-loop frame's drains, in the run loop's order.
fn frame(
    state: &SharedState,
    pc: &Arc<dyn PaneController>,
    tabs: &mut TabManager,
    ui: &mut UiState,
) {
    process_pending_orchestration_surfaces(state, pc, tabs, ui);
    process_pending_pane_closures(state, pc.as_ref(), tabs, ui);
    process_restart_drops(state, pc.as_ref(), tabs, ui);
}

/// Scenario: The TUI shows an orchestration in its own tab when the deck restarts and the restarted deck runs an orchestration whose orchestrator reuses the old one's pane id. The event stream reopens on the restarted deck and resyncs from its listing, with or without the old pane's reattach having noticed the restart first. The old tab goes with the restart notice, and the restarted deck's orchestration gets a tab of its own, attached on the restarted deck, rather than a flat card on the Dashboard (issue #1913).
// The restart fixture is a Unix-socket daemon.
#[cfg(unix)]
#[spec("lifecycle/handshake/014")]
#[test]
fn handshake_014_a_restart_reusing_an_orchestrations_pane_ids_gets_its_own_tab() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    for reattach_noticed in [false, true] {
        let daemon = runtime.block_on(async { RestartingDaemon::spawn("a") });
        let controller = Arc::new(
            EmbeddedPaneController::new(daemon.path.clone(), runtime.handle().clone())
                .with_protocol_gate(),
        );
        let pc: Arc<dyn PaneController> = controller.clone();
        let mut tabs = TabManager::new(pc.clone());
        let cwd = tempfile::tempdir().unwrap();
        let cwd = cwd.path().to_string_lossy().into_owned();
        let state: SharedState = Arc::new(RwLock::new(AppState::default()));
        let mut ui = UiState::default();

        // Before the restart: the stream on `a` lists the orchestration, and
        // the render loop builds its tab.
        {
            let mut st = state.blocking_write();
            st.learn_stream_incarnation(named("a"));
            st.card_unseen_agents(&listed(&cwd, "run-1"));
        }
        frame(&state, &pc, &mut tabs, &mut ui);
        let before = tabs
            .tab_index_for_pane("pane-1")
            .expect("the orchestration's tab before the restart");
        assert!(tabs.orchestration_tab_is(before, &cwd, "team", Some("run-1")));
        assert!(
            pc.pane_incarnation("pane-1")
                .is_some_and(|p| p.same_process(&named("a"))),
            "the role was not attached on the first deck"
        );

        // The deck restarts as `b`, which runs a new orchestration in the
        // same pane id; the event subscriber resubscribes as it does after a
        // gap.
        daemon.restart_as("b");
        if reattach_noticed {
            controller.mark_pane_lost_for_test("pane-1", PaneLostReason::DeckRestarted);
        }
        {
            let records = listed(&cwd, "run-2");
            let mut st = state.blocking_write();
            let dropped = st.learn_stream_incarnation(named("b"));
            st.resync_after_event_gap(&records);
            st.seed_successor_cards(&dropped, &records);
            st.card_unseen_agents(&records);
        }
        frame(&state, &pc, &mut tabs, &mut ui);
        frame(&state, &pc, &mut tabs, &mut ui);

        let case = if reattach_noticed {
            "reattach noticed the restart"
        } else {
            "the stream noticed the restart"
        };
        assert!(
            ui.restart_notice.is_some(),
            "{case}: the restart notice was not shown"
        );
        let Some(after) = tabs.tab_index_for_pane("pane-1") else {
            panic!("{case}: the restarted deck's orchestration got no tab");
        };
        assert!(
            tabs.orchestration_tab_is(after, &cwd, "team", Some("run-2")),
            "{case}: pane-1 is in a tab that is not the restarted deck's orchestration"
        );
        assert_eq!(
            tabs.tab_count(),
            2,
            "{case}: expected the Dashboard and the restarted orchestration's tab"
        );
        assert!(
            pc.pane_incarnation("pane-1")
                .is_some_and(|p| p.same_process(&named("b"))),
            "{case}: the role is not attached on the restarted deck"
        );
        let st = state.blocking_read();
        let cards: Vec<_> = st
            .sessions
            .values()
            .filter(|card| card.pane_id.as_deref() == Some("pane-1"))
            .collect();
        assert_eq!(cards.len(), 1, "{case}: pane-1 has {} cards", cards.len());
        assert_eq!(
            st.pane_role_map.get("pane-1").map(String::as_str),
            Some("orchestrator"),
            "{case}: the restarted role is not placed"
        );
    }
}
