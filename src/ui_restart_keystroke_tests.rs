//! PRD #1666 part 2 (G-fix7c): a keystroke that reaches a pane after its
//! reattach found the deck restarted. The next frame drops the pane and names
//! the restart in one notice, so the keystroke adds no sentence of its own.

use spec::spec;
use tokio::sync::RwLock;

use super::*;
use crate::daemon_client::restart_fixture::RestartingDaemon;
use crate::embedded_pane::{EmbeddedPaneController, PaneLostReason};

/// Scenario: a focused pane's agent connection ends while an Enter is still on its way to it, held back by the submit debounce. When the reattach found the deck restarted, the Enter fails and the status line stays empty, since the next frame drops the pane and names the restart in its one notice. When the agent is simply gone, the same Enter says so on the status line (control).
#[spec("lifecycle/handshake/013")]
#[test]
fn handshake_013_a_keystroke_into_a_pane_lost_to_a_restart_says_nothing_of_its_own() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    for reason in [PaneLostReason::DeckRestarted, PaneLostReason::AgentGone] {
        let daemon = runtime.block_on(async { RestartingDaemon::spawn("a") });
        let controller = Arc::new(
            EmbeddedPaneController::new(daemon.path.clone(), runtime.handle().clone())
                .with_protocol_gate(),
        );
        let pane = controller
            .create_pane(Some("cat"), None)
            .expect("this TUI's own pane on a");
        controller.focus_pane(&pane).expect("focus the pane");
        let pc: Arc<dyn PaneController> = controller.clone();
        let mut tab_manager = TabManager::new(pc.clone());
        let state: SharedState = Arc::new(RwLock::new(AppState::default()));
        let mut ui = UiState {
            mode: UiMode::PaneInput,
            ..UiState::default()
        };

        controller.end_pane_io_for_test(&pane, reason);
        let snapshot = state.blocking_read().clone();
        dispatch_action(
            Action::ForwardToPane(b"\r".to_vec()),
            &mut ui,
            &*pc,
            &state,
            &mut tab_manager,
            &snapshot,
            &[],
            None,
            Rect::new(0, 0, 80, 24),
        );

        let status = ui.status_message.as_ref().map(|(text, _)| text.as_str());
        match reason {
            PaneLostReason::DeckRestarted => assert_eq!(
                status, None,
                "a keystroke into a pane lost to a restart leaked a sentence of its own"
            ),
            _ => assert_eq!(
                status,
                Some(
                    crate::pane::PaneError::CommandFailed(reason.user_message().into())
                        .to_string()
                        .as_str()
                ),
                "control: a keystroke into a pane whose agent is gone says so"
            ),
        }
    }
}
