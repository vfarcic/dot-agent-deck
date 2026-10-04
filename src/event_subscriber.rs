//! PRD #76 M2.17 (hook events) / M2.19 (delegate signals): the TUI's long-lived
//! `SubscribeEvents` connection, routing each [`BroadcastMsg::Event`] into its
//! `AppState` via `apply_event`.
//!
//! PRD #93 round-5: the delegate / work-done variants used to ride this
//! channel too — the daemon couldn't dispatch them locally and the TUI
//! re-ran the role-validation guards. The daemon now owns dispatch end
//! to end (writes the prompt directly into the target pane's PTY), so
//! only hook events flow through here.
//!
//! Reconnects with a small backoff on transport errors so a daemon
//! restart or a `KIND_STREAM_END "lagged"` tear-down recovers
//! automatically.
//!
//! Lives in the library rather than in `main.rs` so a test can drive it against
//! a scripted daemon; `main.rs` spawns [`run`].

use std::time::Duration;

use crate::daemon_client::DaemonClient;
use crate::event::{AgentEvent, BroadcastMsg};
use crate::state::SharedState;

/// How [`run`] paces its reconnects, and the e2e seam that makes it miss events.
#[derive(Clone, Copy)]
pub struct SubscriberConfig {
    /// The first reconnect delay, and the value the backoff resets to after a
    /// successful subscribe.
    pub initial_delay: Duration,
    /// The cap the doubling backoff stops at.
    pub max_delay: Duration,
    /// Issue #621 e2e seam: an event this returns `true` for is discarded
    /// instead of applied. `None` in a shipped binary — `main.rs` passes one only
    /// under the `e2e` feature.
    pub drop_event: Option<fn(&AgentEvent) -> bool>,
    /// Issue #1520 e2e seam: an event this returns `true` for is discarded AND
    /// the stream is torn down, the way a `KIND_STREAM_END "lagged"` loses the
    /// events it never forwarded. `None` in a shipped binary, like
    /// [`Self::drop_event`].
    pub break_on_event: Option<fn(&AgentEvent) -> bool>,
}

impl Default for SubscriberConfig {
    /// Backoff parameters tuned for "daemon briefly unavailable" rather
    /// than long outages: a fresh-daemon ready window is sub-second, so
    /// a 500ms initial delay catches most transient cases, and we cap
    /// at 5s so a stuck daemon doesn't burn CPU on reconnect attempts.
    fn default() -> Self {
        Self {
            initial_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(5),
            drop_event: None,
            break_on_event: None,
        }
    }
}

/// Subscribe, apply, and reconnect forever. Never returns; spawn it.
pub async fn run(client: DaemonClient, state: SharedState, config: SubscriberConfig) {
    let mut delay = config.initial_delay;
    // Issue #1520: every subscribe after the first attempt follows a stretch in
    // which nothing was listening — a stream that ended, or attempts that
    // failed — so it must resynchronize before applying anything new.
    let mut first_attempt = true;
    loop {
        let resync = !std::mem::replace(&mut first_attempt, false);
        match client.subscribe_events().await {
            Ok(_) if resync && !resync_after_gap(&client, &state).await => {
                // Qodo on #1553: a snapshot that could not be read leaves this
                // state unreconciled, so do not settle on the new stream as if
                // it were. Drop it and resubscribe after the backoff, which
                // resynchronizes again; the gap stays recorded meanwhile.
            }
            Ok(mut sub) => {
                // Reset backoff on a successful subscribe (and resync).
                delay = config.initial_delay;
                loop {
                    match sub.next_event().await {
                        Ok(Some(BroadcastMsg::Event(event))) => {
                            if config.drop_event.is_some_and(|drop| drop(&event)) {
                                continue;
                            }
                            if config.break_on_event.is_some_and(|brk| brk(&event)) {
                                break;
                            }
                            state.write().await.apply_event(event);
                        }
                        // PRD #120: a daemon-spawned orchestration (issue
                        // dispatch). Queue it for the render loop, which owns
                        // the TabManager + pane controller and builds the
                        // live tab. The subscriber task can't touch those.
                        Ok(Some(BroadcastMsg::OrchestrationSurface(surface))) => {
                            state.write().await.queue_orchestration_surface(surface);
                        }
                        // Issue #717: a close left a dispatched worktree on
                        // disk. Queue it for the render loop for the same
                        // reason as the surface above — the status line is
                        // `UiState`, which this task cannot touch.
                        Ok(Some(BroadcastMsg::WorktreeKept(kept))) => {
                            state.write().await.queue_worktree_kept(kept);
                        }
                        // PRD #741 M8 (issue #801 item 3): a `kind` tag this
                        // build does not know, from a newer daemon. Ignored
                        // rather than escalated — there is no payload to act
                        // on, and the TUI's own state is rebuilt from
                        // `list_agents` at hydration and reconciled by the
                        // ordinary event flow, so a message it cannot read
                        // costs it nothing it can name.
                        //
                        // What the variant buys is the line above this one:
                        // before it, such a frame failed its whole decode
                        // and arrived at the `Err` arm below, which breaks
                        // the loop and reconnects. A daemon pushing the new
                        // variant regularly therefore took the TUI's event
                        // stream down every time it did.
                        Ok(Some(BroadcastMsg::Unknown)) => {
                            tracing::debug!(
                                "subscribe_events: ignoring a broadcast kind this build does \
                                 not know"
                            );
                        }
                        Ok(None) => break,
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "subscribe_events: stream error, reconnecting"
                            );
                            break;
                        }
                    }
                }
                // Issue #1520: from here until the resubscribe, broadcasts are
                // lost. Say so now, so a delivery that relied on this stream
                // stops instead of acting on its silence while we back off.
                state.write().await.note_event_stream_gap();
            }
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "subscribe_events: subscribe failed, retrying"
                );
            }
        }
        tokio::time::sleep(delay).await;
        delay = std::cmp::min(delay * 2, config.max_delay);
    }
}

/// Bound on the `ListAgents` call a resync makes, matching the TUI's startup
/// hydration bound in spirit: a wedged daemon must not stall the subscriber, and
/// the stream it just reopened is not being read while this runs.
const RESYNC_LIST_TIMEOUT: Duration = Duration::from_secs(5);

/// Issue #1520: the subscriber has just resubscribed after a gap. Re-read the
/// daemon's agents and reconcile this state with them
/// ([`crate::state::AppState::resync_after_event_gap`]) BEFORE the new stream's
/// first event is applied, so the snapshot lands under everything newer.
///
/// Returns `false` when the daemon's agents could not be read. The gap is still
/// recorded then, so a delivery written across it stops rather than trusting a
/// history that is now known to be incomplete, and the caller resubscribes and
/// tries again rather than settling on a stream it never reconciled with.
async fn resync_after_gap(client: &DaemonClient, state: &SharedState) -> bool {
    match tokio::time::timeout(RESYNC_LIST_TIMEOUT, client.list_agents()).await {
        Ok(Ok(records)) => {
            state.write().await.resync_after_event_gap(&records);
            true
        }
        Ok(Err(e)) => {
            tracing::warn!(
                error = %e,
                "subscribe_events: resync after reconnect could not list agents, retrying"
            );
            state.write().await.note_event_stream_gap();
            false
        }
        Err(_) => {
            tracing::warn!(
                timeout_ms = RESYNC_LIST_TIMEOUT.as_millis() as u64,
                "subscribe_events: resync after reconnect timed out listing agents, retrying"
            );
            state.write().await.note_event_stream_gap();
            false
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use chrono::{DateTime, Utc};
    use tokio::sync::RwLock;

    use spec::spec;

    use super::*;
    use crate::agent_pty::AgentRecord;
    use crate::daemon_protocol::{
        AttachRequest, AttachResponse, KIND_EVENT, KIND_REQ, KIND_RESP, KIND_STREAM_END,
        read_frame, write_frame,
    };
    use crate::event::{AgentType, EventType};
    use crate::state::{AppState, SessionStatus};

    const PANE: &str = "pane-1";
    const AGENT: &str = "agent-1";
    /// A second agent on its own pane, alive throughout. Its only job is to put
    /// a `hook_generation` in the `ListAgents` reply, which is how the resync
    /// learns this daemon reports generations at all.
    const OTHER_PANE: &str = "pane-2";
    const OTHER_AGENT: &str = "agent-2";

    fn event_on(
        pane: &str,
        agent: &str,
        session: &str,
        event_type: EventType,
        secs: i64,
    ) -> AgentEvent {
        AgentEvent {
            session_id: session.to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type,
            tool_name: None,
            tool_detail: None,
            cwd: None,
            timestamp: DateTime::<Utc>::UNIX_EPOCH + chrono::TimeDelta::seconds(secs),
            user_prompt: None,
            metadata: Default::default(),
            pane_id: Some(pane.into()),
            agent_id: Some(agent.into()),
            agent_version: None,
            schema_version: None,
            live_target: None,
        }
    }

    fn event(session: &str, event_type: EventType, secs: i64) -> AgentEvent {
        event_on(PANE, AGENT, session, event_type, secs)
    }

    fn tool_start(session: &str, secs: i64) -> AgentEvent {
        let mut ev = event(session, EventType::ToolStart, secs);
        ev.tool_name = Some("Bash".into());
        ev
    }

    /// A daemon reduced to the two requests the subscriber makes. The FIRST
    /// subscription carries `first_stream` and is then torn down with
    /// `KIND_STREAM_END "lagged"` — the daemon's own reaction to a receiver that
    /// fell behind its broadcast. Every later one stays open and silent.
    /// `ListAgents` is answered from `daemon`, joined exactly the way the real
    /// handler joins it ([`AppState::attach_live_sessions`]), except that the
    /// first `failing_lists` of them are refused.
    struct ScriptedDaemon {
        first_stream: Vec<AgentEvent>,
        daemon: AppState,
        agents: Vec<(&'static str, &'static str)>,
        failing_lists: usize,
    }

    struct Counters {
        subscriptions: AtomicUsize,
        lists: AtomicUsize,
    }

    async fn serve(
        listener: tokio::net::UnixListener,
        script: Arc<ScriptedDaemon>,
        n: Arc<Counters>,
    ) {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let script = Arc::clone(&script);
            let n = Arc::clone(&n);
            tokio::spawn(async move {
                let (mut rd, mut wr) = stream.into_split();
                let Ok(Some((KIND_REQ, payload))) = read_frame(&mut rd).await else {
                    return;
                };
                let request: AttachRequest =
                    serde_json::from_slice(&payload).expect("a request the daemon can read");
                match request {
                    AttachRequest::SubscribeEvents => {
                        let ok = serde_json::to_vec(&AttachResponse::ok()).unwrap();
                        write_frame(&mut wr, KIND_RESP, &ok).await.unwrap();
                        if n.subscriptions.fetch_add(1, Ordering::SeqCst) == 0 {
                            for ev in &script.first_stream {
                                let msg =
                                    serde_json::to_vec(&BroadcastMsg::Event(ev.clone())).unwrap();
                                write_frame(&mut wr, KIND_EVENT, &msg).await.unwrap();
                            }
                            write_frame(&mut wr, KIND_STREAM_END, b"lagged")
                                .await
                                .unwrap();
                        } else {
                            // Held open until the client goes away.
                            let _ = read_frame(&mut rd).await;
                        }
                    }
                    AttachRequest::ListAgents => {
                        let resp = if n.lists.fetch_add(1, Ordering::SeqCst) < script.failing_lists
                        {
                            AttachResponse::err("scripted failure")
                        } else {
                            let mut records: Vec<AgentRecord> = script
                                .agents
                                .iter()
                                .map(|(agent, pane)| {
                                    serde_json::from_value(
                                        serde_json::json!({ "id": agent, "pane_id_env": pane }),
                                    )
                                    .unwrap()
                                })
                                .collect();
                            script.daemon.attach_live_sessions(&mut records);
                            let mut resp = AttachResponse::ok();
                            resp.agent_records = Some(records);
                            resp
                        };
                        let resp = serde_json::to_vec(&resp).unwrap();
                        write_frame(&mut wr, KIND_RESP, &resp).await.unwrap();
                    }
                    other => panic!("the scripted daemon does not answer {other:?}"),
                }
            });
        }
    }

    /// Issue #1520: what the TUI's `AppState` reads for `PANE` — its
    /// generation, the card's status, and the closure count.
    type View = (Option<String>, Option<SessionStatus>, u64);

    async fn tui_view(state: &SharedState) -> View {
        let st = state.read().await;
        let status = st
            .sessions
            .values()
            .find(|s| s.pane_id.as_deref() == Some(PANE))
            .map(|s| s.status.clone());
        (
            st.pane_hook_session_id(PANE),
            status,
            st.pane_generation_closures(PANE),
        )
    }

    /// Run the production subscriber against `script` until the TUI's view of
    /// `PANE` equals `expected` or 5 s pass, and return the last view seen and
    /// the request counts.
    async fn run_until(
        script: ScriptedDaemon,
        panes: &[&str],
        expected: &View,
    ) -> (View, Arc<Counters>) {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("attach.sock");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind the scripted daemon");
        let counters = Arc::new(Counters {
            subscriptions: AtomicUsize::new(0),
            lists: AtomicUsize::new(0),
        });
        let server = tokio::spawn(serve(listener, Arc::new(script), Arc::clone(&counters)));

        let mut tui = AppState::default();
        for pane in panes {
            tui.register_pane(pane.to_string());
        }
        let state: SharedState = Arc::new(RwLock::new(tui));
        let config = SubscriberConfig {
            initial_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(50),
            drop_event: None,
            break_on_event: None,
        };
        let subscriber = tokio::spawn(run(DaemonClient::new(socket), Arc::clone(&state), config));

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut seen = tui_view(&state).await;
        while seen != *expected && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
            seen = tui_view(&state).await;
        }
        subscriber.abort();
        server.abort();
        assert!(
            counters.subscriptions.load(Ordering::SeqCst) >= 2,
            "the subscriber never reconnected, so this did not test a reconnect"
        );
        (seen, counters)
    }

    /// The daemon of `live_018`: `gen-a` started, ended and was succeeded by
    /// `gen-b`, which is working — everything, in order.
    fn rolled_over_daemon() -> AppState {
        let mut daemon = AppState::default();
        daemon.register_pane(PANE.to_string());
        daemon.apply_event(event("gen-a", EventType::SessionStart, 1));
        daemon.apply_event(event("gen-a", EventType::SessionEnd, 2));
        daemon.apply_event(event("gen-b", EventType::SessionStart, 3));
        daemon.apply_event(tool_start("gen-b", 4));
        assert_eq!(daemon.pane_hook_session_id(PANE).as_deref(), Some("gen-b"));
        daemon
    }

    /// Scenario: Run the TUI's event subscriber against a daemon that sends one conversation's start and then tears the stream down as `lagged`, while that conversation ends and a second one starts and gets to work. After the subscriber reconnects, the TUI's state must name the second conversation as the pane's, show the card working, and count the conversation that ended while it was away.
    #[spec("session/live/018")]
    #[tokio::test]
    async fn live_018_a_reconnect_brings_the_client_state_back_to_the_daemons() {
        let expected = (Some("gen-b".to_string()), Some(SessionStatus::Working), 1);
        let (seen, _) = run_until(
            ScriptedDaemon {
                first_stream: vec![event("gen-a", EventType::SessionStart, 1)],
                daemon: rolled_over_daemon(),
                agents: vec![(AGENT, PANE)],
                failing_lists: 0,
            },
            &[PANE],
            &expected,
        )
        .await;
        assert_eq!(
            seen, expected,
            "after resubscribing, the TUI must agree with the daemon about the pane's \
             conversation (gen-b), the card's status (Working) and the one conversation that \
             ended while it was disconnected — not carry on from the stream it lost \
             (generation, status, closures)"
        );
    }

    /// Issue #1520 (Qodo on #1553): a `ListAgents` that fails right after the
    /// resubscribe must not leave the TUI on the stream it never reconciled
    /// with. The subscriber resubscribes and resynchronizes again.
    #[tokio::test]
    async fn a_snapshot_that_could_not_be_read_is_retried_on_a_new_subscription() {
        let expected = (Some("gen-b".to_string()), Some(SessionStatus::Working), 1);
        let (seen, counters) = run_until(
            ScriptedDaemon {
                first_stream: vec![event("gen-a", EventType::SessionStart, 1)],
                daemon: rolled_over_daemon(),
                agents: vec![(AGENT, PANE)],
                failing_lists: 1,
            },
            &[PANE],
            &expected,
        )
        .await;
        assert_eq!(
            seen, expected,
            "a failed snapshot must be retried, not taken as an empty one \
             (generation, status, closures)"
        );
        assert!(
            counters.lists.load(Ordering::SeqCst) >= 2
                && counters.subscriptions.load(Ordering::SeqCst) >= 3,
            "the retry must come from a fresh subscription and a second ListAgents"
        );
    }

    /// Issue #1520 (Qodo on #1553): a conversation that ended while the stream
    /// was down, with nothing succeeding it. The daemon's reply carries no
    /// generation for the pane, and a second agent's generation in the same
    /// reply shows that is an answer rather than an older daemon's silence, so
    /// the TUI drops `gen-a` and counts it as closed. Control: with no record
    /// that carries a generation — what a daemon from before the field sends —
    /// the TUI's generation is left alone, since there absence proves nothing.
    #[tokio::test]
    async fn a_conversation_that_ended_during_the_gap_leaves_the_pane_without_one() {
        let ended_daemon = || {
            let mut daemon = AppState::default();
            daemon.register_pane(PANE.to_string());
            daemon.register_pane(OTHER_PANE.to_string());
            daemon.apply_event(event("gen-a", EventType::SessionStart, 1));
            daemon.apply_event(event("gen-a", EventType::SessionEnd, 2));
            daemon.apply_event(event_on(
                OTHER_PANE,
                OTHER_AGENT,
                "gen-x",
                EventType::SessionStart,
                3,
            ));
            assert_eq!(daemon.pane_hook_session_id(PANE), None);
            daemon
        };

        let expected = (None, Some(SessionStatus::Idle), 1);
        let (seen, _) = run_until(
            ScriptedDaemon {
                first_stream: vec![tool_start("gen-a", 1)],
                daemon: ended_daemon(),
                agents: vec![(AGENT, PANE), (OTHER_AGENT, OTHER_PANE)],
                failing_lists: 0,
            },
            &[PANE, OTHER_PANE],
            &expected,
        )
        .await;
        assert_eq!(
            seen, expected,
            "the conversation that ended unseen must leave the pane, idle, counted as \
             closed (generation, status, closures)"
        );

        // Control: the same daemon, but only the pane without a generation is
        // listed, so nothing in the reply shows the field is supported.
        let unchanged = (Some("gen-a".to_string()), Some(SessionStatus::Idle), 0);
        let (seen, _) = run_until(
            ScriptedDaemon {
                first_stream: vec![tool_start("gen-a", 1)],
                daemon: ended_daemon(),
                agents: vec![(AGENT, PANE)],
                failing_lists: 0,
            },
            &[PANE, OTHER_PANE],
            &unchanged,
        )
        .await;
        assert_eq!(
            seen, unchanged,
            "control: a reply with no generation anywhere cannot tell an ended conversation \
             from an older daemon, so the TUI's generation must be left alone"
        );
    }

    /// Issue #1520 (Greptile on #1553): the resync takes the snapshot's card
    /// fields even when its stamp is not newer than the card's — absent, as from
    /// a daemon before `last_activity_ms`, or equal after millisecond
    /// truncation. The reply was built after everything the card holds, so it is
    /// the newer account whatever its stamp says.
    #[test]
    fn a_resync_refreshes_the_card_without_a_newer_stamp() {
        let mut tui = AppState::default();
        tui.register_pane(PANE.to_string());
        tui.apply_event(event("gen-a", EventType::SessionStart, 1));
        tui.apply_event(tool_start("gen-a", 2));
        let mut daemon = tui.clone();
        daemon.apply_event(event("gen-a", EventType::Idle, 3));

        let mut records: Vec<AgentRecord> = vec![
            serde_json::from_value(serde_json::json!({ "id": AGENT, "pane_id_env": PANE }))
                .unwrap(),
        ];
        daemon.attach_live_sessions(&mut records);
        let snap = records[0]
            .live
            .as_mut()
            .expect("the daemon has a live session");
        assert_eq!(snap.status, SessionStatus::Idle);
        snap.last_activity_ms = None;

        tui.resync_after_event_gap(&records);
        let status = tui
            .sessions
            .values()
            .find(|s| s.pane_id.as_deref() == Some(PANE))
            .map(|s| s.status.clone());
        assert_eq!(
            status,
            Some(SessionStatus::Idle),
            "the card must show the daemon's status after a resync, stamp or no stamp"
        );
    }
}
