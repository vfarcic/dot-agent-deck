//! PRD #1666 part 3, round 5: the single entry a worker's `work-done` reaches
//! the daemon through.
//!
//! [`complete`] is called by the hook socket's `work_done` arm, and later by
//! the gated `work_done_task` verb and the fast-tier seam. The connection
//! writes its one line from the [`WorkDoneDisposition`] it returns, never
//! before.
//!
//! At this commit it is an adapter over the path the arm ran inline: hold
//! room for the report, mark a dispatched unit complete, compose the report
//! under the state read guard and post it. Its answers are the arm's: a
//! `deck_full` refusal when the deck has no room for the report, and an
//! acceptance otherwise. The task-scoped transaction replaces the body
//! (`.dot-agent-deck/coder-p3-r5-plan.md` §2); its callers keep calling this.

use tokio::sync::RwLock;
use tracing::warn;

use crate::agent_pty::AgentPtyRegistry;
use crate::config_validation::escape_id_for_log;
use crate::event::{SignalAck, WorkDoneSignal};
use crate::hook_provenance::Provenance;
use crate::state::AppState;

/// Test pause point: after the report's provenance was classified and before
/// anything is read or reserved for it.
pub(crate) const PAUSE_ATTESTED: &str = "work-done-attested";
/// Test pause point, on the connection: the result is in hand and no byte of
/// the answer is written yet.
pub(crate) const PAUSE_COMMITTED: &str = "work-done-committed";
/// Test pause point, on the connection: the answer's line is written.
pub(crate) const PAUSE_ACKED: &str = "work-done-acked";

/// What a `work-done` is answered with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkDoneDisposition {
    /// The report was taken.
    Accepted,
    /// Nothing was reported; `code` and `message` say why.
    Refused { code: &'static str, message: String },
}

impl WorkDoneDisposition {
    /// The line the hook socket answers a legacy `work_done` with.
    pub(crate) fn to_signal_ack(&self) -> SignalAck {
        match self {
            Self::Accepted => SignalAck::accepted(),
            Self::Refused { code, message } => SignalAck::refused(code, message),
        }
    }
}

/// Take one `work-done` report: see the module documentation.
pub(crate) async fn complete(
    state: &RwLock<AppState>,
    registry: &AgentPtyRegistry,
    signal: WorkDoneSignal,
    provenance: &Provenance,
) -> WorkDoneDisposition {
    registry.pause_point(PAUSE_ATTESTED).await;
    // PR #1735 fifth review (N3): room for the report is held BEFORE anything
    // is taken on — the completion mark, the delegation, a dispatched unit's
    // return edge — so a report the worker is told was accepted cannot then be
    // refused by its orchestrator's full inbox or a deck that filled
    // meanwhile. With no room now the work-done is refused, visibly, and
    // nothing it would have consumed is touched.
    let room = match registry.inbox().reserve_reply() {
        Ok(room) => room,
        Err(error) => {
            warn!(
                pane_id = %escape_id_for_log(&signal.pane_id),
                %error,
                "hook socket: refused a work-done; the deck has no room for its report"
            );
            return WorkDoneDisposition::Refused {
                code: "deck_full",
                message: format!(
                    "{error}, so it could not take this work-done report; nothing was \
                     reported, so run work-done again once the deck has room"
                ),
            };
        }
    };
    // PRD #1589 D5: the completion mark `close` reads, bound to the attested
    // sender, before the return route's own handling.
    crate::daemon::mark_dispatched_unit_completion(registry, &signal, provenance);
    // The report is posted AFTER the read guard is released:
    // `prepare_work_done` runs under it and `deliver` after it. PRD #1666 S5:
    // `deliver` is an inbox post and waits on nothing.
    let delivery = state.read().await.prepare_work_done(signal, registry).await;
    delivery.deliver(registry, room).await;
    WorkDoneDisposition::Accepted
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::agent_pty::TestFault;

    /// The synchronous pause blocks only its own thread, reports when it is
    /// reached, and returns once released.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pause_point_sync_blocks_its_thread_until_released() {
        let registry = Arc::new(AgentPtyRegistry::new());
        let (reached, release) = registry.pause_at_sync_for_test("window");
        let paused = {
            let registry = Arc::clone(&registry);
            tokio::task::spawn_blocking(move || {
                registry.pause_point_sync("window");
                "past the pause"
            })
        };
        reached
            .recv_timeout(Duration::from_secs(5))
            .expect("the pause is reached");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!paused.is_finished(), "the pause holds its thread");
        release.send(()).expect("release the pause");
        assert_eq!(paused.await.expect("the paused thread"), "past the pause");
        // Armed once: a second visit passes straight through.
        registry.pause_point_sync("window");
    }

    /// An injected fault fires once: a `Fail` asks its step to fail, a `Panic`
    /// panics with its payload, and an unarmed point does nothing.
    #[test]
    fn fault_point_fires_what_was_injected_once() {
        let registry = AgentPtyRegistry::new();
        assert!(!registry.fault_point("step"));
        registry.inject_fault_for_test("step", TestFault::Fail);
        assert!(registry.fault_point("step"));
        assert!(!registry.fault_point("step"));
        registry.inject_fault_for_test("step", TestFault::Panic("injected-7d31"));
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            registry.fault_point("step");
        }))
        .expect_err("the injected panic fires");
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"injected-7d31"));
        assert!(!registry.fault_point("step"));
    }

    /// The adapter answers what the inline arm answered: an acceptance, and
    /// `deck_full` with nothing marked when the deck has no room.
    #[tokio::test]
    async fn complete_answers_as_the_inline_arm_did() {
        let state = RwLock::new(AppState::default());
        let registry = AgentPtyRegistry::new();
        let signal = || WorkDoneSignal {
            pane_id: "task-completion-seam-pane".to_string(),
            task: "seam report".to_string(),
            done: false,
            timestamp: chrono::Utc::now(),
            token: None,
        };
        assert_eq!(
            complete(&state, &registry, signal(), &Provenance::Unattested).await,
            WorkDoneDisposition::Accepted
        );

        let mut held = Vec::new();
        while let Ok(room) = registry.inbox().reserve_reply() {
            held.push(room);
        }
        match complete(&state, &registry, signal(), &Provenance::Unattested).await {
            WorkDoneDisposition::Refused { code, message } => {
                assert_eq!(code, "deck_full");
                assert!(message.contains("nothing was reported"), "{message}");
            }
            other => panic!("expected deck_full, got {other:?}"),
        }
        assert_eq!(
            serde_json::to_value(WorkDoneDisposition::Accepted.to_signal_ack()).unwrap(),
            serde_json::to_value(SignalAck::accepted()).unwrap()
        );
    }
}
