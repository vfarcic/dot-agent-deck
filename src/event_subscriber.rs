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
        let subscribed = if resync {
            resubscribe(&client, &state).await
        } else {
            client.subscribe_events().await.map_err(|e| {
                tracing::debug!(error = %e, "subscribe_events: subscribe failed, retrying");
            })
        };
        // On `Err` the failure was logged where it happened. Qodo on #1553: a
        // resync that could not read the daemon's agents leaves this state
        // unreconciled, so it does not settle on the new stream as if it were;
        // the subscriber resubscribes after the backoff, which resynchronizes
        // again, and the gap stays recorded meanwhile.
        if let Ok(mut sub) = subscribed {
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
        tokio::time::sleep(delay).await;
        delay = std::cmp::min(delay * 2, config.max_delay);
    }
}

/// Bound on the requests a resync makes before it has a reconciled stream,
/// matching the TUI's startup hydration bound in spirit: a wedged daemon must not
/// stall the subscriber, and an opened stream is not being read while they run.
const RESYNC_LIST_TIMEOUT: Duration = Duration::from_secs(5);

/// Issue #1520: the subscriber is resubscribing after a gap. Open the new
/// stream, re-read the daemon's agents and reconcile this state with them
/// ([`crate::state::AppState::resync_after_event_gap`]) BEFORE the stream's
/// first event is applied, so the snapshot lands under everything newer.
///
/// Issue #1555: where the daemon offers it, the stream and the snapshot come
/// from one `SubscribeEventsWithSnapshot`, which the daemon opens at the instant
/// it reads the snapshot, so no event on the stream is one the snapshot already
/// includes. From a daemon that does not, it is `SubscribeEvents` then
/// `ListAgents`, as before: events broadcast between the two are queued on the
/// stream AND in the snapshot, and are applied over it — the residual
/// `resync_after_event_gap` documents.
///
/// `Err(())` when either the stream or the daemon's agents could not be had;
/// the failure is logged here, and the caller resubscribes and tries again
/// rather than settling on a stream it never reconciled with. Where the snapshot
/// was the part that failed, the gap is recorded again, so a delivery written
/// across it stops rather than trusting a history that is now known to be
/// incomplete.
async fn resubscribe(
    client: &DaemonClient,
    state: &SharedState,
) -> Result<crate::daemon_client::EventSubscription, ()> {
    match subscribe_with_snapshot(client).await {
        Ok((sub, records)) => {
            state.write().await.resync_after_event_gap(&records);
            Ok(sub)
        }
        Err(ResyncFailure::Subscribe(e)) => {
            tracing::debug!(error = %e, "subscribe_events: subscribe failed, retrying");
            Err(())
        }
        Err(ResyncFailure::Snapshot(e)) => {
            tracing::warn!(
                error = %e,
                "subscribe_events: resync after reconnect could not list agents, retrying"
            );
            state.write().await.note_event_stream_gap();
            Err(())
        }
        Err(ResyncFailure::TimedOut) => {
            tracing::warn!(
                timeout_ms = RESYNC_LIST_TIMEOUT.as_millis() as u64,
                "subscribe_events: resync after reconnect timed out listing agents, retrying"
            );
            state.write().await.note_event_stream_gap();
            Err(())
        }
    }
}

/// Why [`subscribe_with_snapshot`] could not hand back a stream and a snapshot.
enum ResyncFailure {
    /// No subscription could be opened: the daemon could not be reached for
    /// the ordered request, or the fallback's plain subscribe failed.
    Subscribe(crate::daemon_client::ClientError),
    /// The snapshot could not be read: a reachable daemon refused or garbled
    /// the ordered request, or the fallback's `ListAgents` failed.
    Snapshot(crate::daemon_client::ClientError),
    /// Either of those took longer than [`RESYNC_LIST_TIMEOUT`].
    TimedOut,
}

/// The stream and the snapshot a resync starts from: one
/// `SubscribeEventsWithSnapshot` where the daemon offers it, `SubscribeEvents`
/// then `ListAgents` where it does not. See [`resubscribe`].
async fn subscribe_with_snapshot(
    client: &DaemonClient,
) -> Result<
    (
        crate::daemon_client::EventSubscription,
        Vec<crate::agent_pty::AgentRecord>,
    ),
    ResyncFailure,
> {
    use crate::daemon_client::{ClientError, GatedQuery};
    match tokio::time::timeout(RESYNC_LIST_TIMEOUT, client.subscribe_events_with_snapshot()).await {
        Ok(Ok(GatedQuery::Answered(answer))) => Ok(answer),
        Ok(Ok(GatedQuery::Unsupported)) => {
            let sub = client
                .subscribe_events()
                .await
                .map_err(ResyncFailure::Subscribe)?;
            match tokio::time::timeout(RESYNC_LIST_TIMEOUT, client.list_agents()).await {
                Ok(Ok(records)) => Ok((sub, records)),
                Ok(Err(e)) => Err(ResyncFailure::Snapshot(e)),
                Err(_) => Err(ResyncFailure::TimedOut),
            }
        }
        // Qodo on #1577: a daemon that cannot be reached — down, or restarting —
        // fails the request's `Hello` or connect, and that is a failed
        // subscribe, as it was before the ordered request existed: logged
        // quietly, with no further gap recorded on every backoff.
        Ok(Err(e @ (ClientError::Io(_) | ClientError::SocketMissing(_)))) => {
            Err(ResyncFailure::Subscribe(e))
        }
        Ok(Err(e)) => Err(ResyncFailure::Snapshot(e)),
        Err(_) => Err(ResyncFailure::TimedOut),
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
        PROTOCOL_VERSION, read_frame, write_frame,
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

    /// A daemon reduced to the requests the subscriber makes. The FIRST
    /// subscription carries `first_stream` and is then torn down with
    /// `KIND_STREAM_END "lagged"` — the daemon's own reaction to a receiver that
    /// fell behind its broadcast. Every later one carries `queued` (plain
    /// `SubscribeEvents` only) and then `after_snapshot`, and stays open.
    /// `ListAgents` and the snapshot of a `SubscribeEventsWithSnapshot` are
    /// answered from `daemon`, joined exactly the way the real handler joins it
    /// ([`AppState::attach_live_sessions`]), except that the first
    /// `failing_lists` of them are refused.
    ///
    /// `queued` is what the real daemon broadcasts between a plain resubscribe
    /// and the `ListAgents` that follows it, so `daemon` must already include
    /// it; `after_snapshot` is broadcast after the snapshot, so `daemon` must
    /// not. A `SubscribeEventsWithSnapshot` (issue #1555) opens its receiver and
    /// reads the snapshot at one instant, so nothing reaches it that the
    /// snapshot includes: it carries `after_snapshot` alone. `ordered` is
    /// whether this daemon advertises that request; one that does not is a
    /// daemon from before it.
    struct ScriptedDaemon {
        first_stream: Vec<AgentEvent>,
        queued: Vec<AgentEvent>,
        after_snapshot: Vec<AgentEvent>,
        daemon: AppState,
        agents: Vec<(&'static str, &'static str)>,
        failing_lists: usize,
        ordered: bool,
    }

    impl ScriptedDaemon {
        /// No `queued` or `after_snapshot` events, and a daemon from before
        /// issue #1555 unless `ordered` is set.
        fn new(
            first_stream: Vec<AgentEvent>,
            daemon: AppState,
            agents: Vec<(&'static str, &'static str)>,
            failing_lists: usize,
            ordered: bool,
        ) -> Self {
            Self {
                first_stream,
                queued: Vec::new(),
                after_snapshot: Vec::new(),
                daemon,
                agents,
                failing_lists,
                ordered,
            }
        }

        /// The listing reply, or `None` for a scripted refusal.
        fn listing(&self, n: &Counters) -> Option<Vec<AgentRecord>> {
            if n.lists.fetch_add(1, Ordering::SeqCst) < self.failing_lists {
                return None;
            }
            let mut records: Vec<AgentRecord> = self
                .agents
                .iter()
                .map(|(agent, pane)| {
                    serde_json::from_value(serde_json::json!({ "id": agent, "pane_id_env": pane }))
                        .unwrap()
                })
                .collect();
            self.daemon.attach_live_sessions(&mut records);
            Some(records)
        }
    }

    struct Counters {
        subscriptions: AtomicUsize,
        lists: AtomicUsize,
        /// How many `SubscribeEventsWithSnapshot` requests arrived.
        ordered_subscriptions: AtomicUsize,
    }

    async fn write_events(wr: &mut tokio::net::unix::OwnedWriteHalf, events: &[AgentEvent]) {
        for ev in events {
            let msg = serde_json::to_vec(&BroadcastMsg::Event(ev.clone())).unwrap();
            write_frame(wr, KIND_EVENT, &msg).await.unwrap();
        }
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
                    AttachRequest::Hello { .. } => {
                        let mut resp = AttachResponse::hello(PROTOCOL_VERSION);
                        if script.ordered {
                            resp = resp.with_capabilities();
                        }
                        let resp = serde_json::to_vec(&resp).unwrap();
                        write_frame(&mut wr, KIND_RESP, &resp).await.unwrap();
                    }
                    AttachRequest::SubscribeEvents => {
                        let ok = serde_json::to_vec(&AttachResponse::ok()).unwrap();
                        write_frame(&mut wr, KIND_RESP, &ok).await.unwrap();
                        if n.subscriptions.fetch_add(1, Ordering::SeqCst) == 0 {
                            write_events(&mut wr, &script.first_stream).await;
                            write_frame(&mut wr, KIND_STREAM_END, b"lagged")
                                .await
                                .unwrap();
                        } else {
                            write_events(&mut wr, &script.queued).await;
                            write_events(&mut wr, &script.after_snapshot).await;
                            // Held open until the client goes away.
                            let _ = read_frame(&mut rd).await;
                        }
                    }
                    AttachRequest::SubscribeEventsWithSnapshot => {
                        assert!(script.ordered, "sent to a daemon that never advertised it");
                        n.subscriptions.fetch_add(1, Ordering::SeqCst);
                        n.ordered_subscriptions.fetch_add(1, Ordering::SeqCst);
                        let resp = match script.listing(&n) {
                            Some(records) => AttachResponse::agent_records(records),
                            None => AttachResponse::err("scripted failure"),
                        };
                        let refused = !resp.ok;
                        let resp = serde_json::to_vec(&resp).unwrap();
                        write_frame(&mut wr, KIND_RESP, &resp).await.unwrap();
                        if refused {
                            return;
                        }
                        write_events(&mut wr, &script.after_snapshot).await;
                        let _ = read_frame(&mut rd).await;
                    }
                    AttachRequest::ListAgents => {
                        let resp = match script.listing(&n) {
                            Some(records) => AttachResponse::agent_records(records),
                            None => AttachResponse::err("scripted failure"),
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
        run_until_seen(script, panes, |seen| seen == expected).await
    }

    /// [`run_until`], stopping at the first view `done` accepts.
    async fn run_until_seen(
        script: ScriptedDaemon,
        panes: &[&str],
        done: impl Fn(&View) -> bool,
    ) -> (View, Arc<Counters>) {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("attach.sock");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind the scripted daemon");
        let counters = Arc::new(Counters {
            subscriptions: AtomicUsize::new(0),
            lists: AtomicUsize::new(0),
            ordered_subscriptions: AtomicUsize::new(0),
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
        while !done(&seen) && tokio::time::Instant::now() < deadline {
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

    /// Both kinds of daemon a reconnect can meet: one from before issue #1555,
    /// which the subscriber resynchronizes from with `SubscribeEvents` plus
    /// `ListAgents`, and one that answers `SubscribeEventsWithSnapshot`.
    const DAEMONS: [bool; 2] = [false, true];

    /// Scenario: Run the TUI's event subscriber against a daemon that sends one conversation's start and then tears the stream down as `lagged`, while that conversation ends and a second one starts and gets to work. After the subscriber reconnects, the TUI's state must name the second conversation as the pane's, show the card working, and count the conversation that ended while it was away.
    #[spec("session/live/018")]
    #[tokio::test]
    async fn live_018_a_reconnect_brings_the_client_state_back_to_the_daemons() {
        for ordered in DAEMONS {
            let expected = (Some("gen-b".to_string()), Some(SessionStatus::Working), 1);
            let (seen, _) = run_until(
                ScriptedDaemon::new(
                    vec![event("gen-a", EventType::SessionStart, 1)],
                    rolled_over_daemon(),
                    vec![(AGENT, PANE)],
                    0,
                    ordered,
                ),
                &[PANE],
                &expected,
            )
            .await;
            assert_eq!(
                seen, expected,
                "after resubscribing (ordered snapshot: {ordered}), the TUI must agree with the \
                 daemon about the pane's conversation (gen-b), the card's status (Working) and \
                 the one conversation that ended while it was disconnected — not carry on from \
                 the stream it lost (generation, status, closures)"
            );
        }
    }

    /// Issue #1520 (Qodo on #1553): a `ListAgents` that fails right after the
    /// resubscribe must not leave the TUI on the stream it never reconciled
    /// with. The subscriber resubscribes and resynchronizes again. Issue #1555:
    /// the same for a refused `SubscribeEventsWithSnapshot`.
    #[tokio::test]
    async fn a_snapshot_that_could_not_be_read_is_retried_on_a_new_subscription() {
        for ordered in DAEMONS {
            let expected = (Some("gen-b".to_string()), Some(SessionStatus::Working), 1);
            let (seen, counters) = run_until(
                ScriptedDaemon::new(
                    vec![event("gen-a", EventType::SessionStart, 1)],
                    rolled_over_daemon(),
                    vec![(AGENT, PANE)],
                    1,
                    ordered,
                ),
                &[PANE],
                &expected,
            )
            .await;
            assert_eq!(
                seen, expected,
                "a failed snapshot (ordered: {ordered}) must be retried, not taken as an empty \
                 one (generation, status, closures)"
            );
            assert!(
                counters.lists.load(Ordering::SeqCst) >= 2
                    && counters.subscriptions.load(Ordering::SeqCst) >= 3,
                "the retry (ordered: {ordered}) must come from a fresh subscription and a \
                 second snapshot"
            );
            assert_eq!(
                counters.ordered_subscriptions.load(Ordering::SeqCst) >= 2,
                ordered,
                "a daemon that advertises the ordered subscription is resynchronized through \
                 it, and only such a daemon is"
            );
        }
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

        for ordered in DAEMONS {
            let expected = (None, Some(SessionStatus::Idle), 1);
            let (seen, _) = run_until(
                ScriptedDaemon::new(
                    vec![tool_start("gen-a", 1)],
                    ended_daemon(),
                    vec![(AGENT, PANE), (OTHER_AGENT, OTHER_PANE)],
                    0,
                    ordered,
                ),
                &[PANE, OTHER_PANE],
                &expected,
            )
            .await;
            assert_eq!(
                seen, expected,
                "the conversation that ended unseen (ordered: {ordered}) must leave the pane, \
                 idle, counted as closed (generation, status, closures)"
            );

            // Control: the same daemon, but only the pane without a generation
            // is listed, so nothing in the reply shows the field is supported.
            let unchanged = (Some("gen-a".to_string()), Some(SessionStatus::Idle), 0);
            let (seen, _) = run_until(
                ScriptedDaemon::new(
                    vec![tool_start("gen-a", 1)],
                    ended_daemon(),
                    vec![(AGENT, PANE)],
                    0,
                    ordered,
                ),
                &[PANE, OTHER_PANE],
                &unchanged,
            )
            .await;
            assert_eq!(
                seen, unchanged,
                "control (ordered: {ordered}): a reply with no generation anywhere cannot tell \
                 an ended conversation from an older daemon, so the TUI's generation must be \
                 left alone"
            );
        }
    }

    /// Issue #1555: a conversation rolls over in the window between the
    /// resubscribe and the snapshot. `gen-a` ended while the stream was down;
    /// then `gen-b` starts and ends and `gen-c` starts, all before the snapshot
    /// is read, and `gen-c` gets to work after it.
    ///
    /// A daemon from before the fix can only be resynchronized with
    /// `SubscribeEvents` then `ListAgents`, and the window's events are queued
    /// on the new subscription although the listing already includes them, so
    /// the TUI replays them over the snapshot: `gen-b` comes back and each
    /// change is counted again. A daemon that answers
    /// `SubscribeEventsWithSnapshot` opens the subscription at the snapshot, so
    /// only `gen-c`'s work follows it, and the TUI counts the one closure the
    /// snapshot shows.
    #[tokio::test]
    async fn events_the_snapshot_already_includes_are_not_replayed_over_it() {
        let window = vec![
            event("gen-b", EventType::SessionStart, 3),
            event("gen-b", EventType::SessionEnd, 4),
            event("gen-c", EventType::SessionStart, 5),
        ];
        let after_snapshot = vec![tool_start("gen-c", 6)];
        let snapshot_daemon = || {
            let mut daemon = AppState::default();
            daemon.register_pane(PANE.to_string());
            daemon.apply_event(event("gen-a", EventType::SessionStart, 1));
            daemon.apply_event(event("gen-a", EventType::SessionEnd, 2));
            for ev in &window {
                daemon.apply_event(ev.clone());
            }
            assert_eq!(daemon.pane_hook_session_id(PANE).as_deref(), Some("gen-c"));
            daemon
        };
        let script = |ordered| ScriptedDaemon {
            queued: window.clone(),
            after_snapshot: after_snapshot.clone(),
            ..ScriptedDaemon::new(
                vec![event("gen-a", EventType::SessionStart, 1)],
                snapshot_daemon(),
                vec![(AGENT, PANE)],
                0,
                ordered,
            )
        };

        let expected = (Some("gen-c".to_string()), Some(SessionStatus::Working), 1);
        let (seen, counters) = run_until(script(true), &[PANE], &expected).await;
        assert_eq!(
            seen, expected,
            "events the snapshot already includes must not be applied over it: the pane is \
             on gen-c, working, and the snapshot shows one conversation (gen-a) ended \
             (generation, status, closures)"
        );
        assert!(
            counters.ordered_subscriptions.load(Ordering::SeqCst) >= 1,
            "the subscriber must resynchronize through the ordered subscription when the \
             daemon advertises it"
        );

        // Control: the same window against a daemon from before the fix, which
        // the subscriber can only resynchronize unordered. This is the replay
        // the test exists to catch, so it must show here.
        let (seen, _) = run_until_seen(script(false), &[PANE], |seen| {
            seen.1 == Some(SessionStatus::Working)
        })
        .await;
        assert!(
            seen.2 > 1,
            "control: without an ordered snapshot the queued events replay over it and the \
             closures are counted again, which is what makes the case above meaningful; \
             saw {seen:?}"
        );
    }

    /// Qodo on #1577: a daemon that is down while the subscriber retries fails
    /// every attempt at the connect, before any snapshot is asked for. That is
    /// a failed subscribe, not a failed snapshot: the stream's break recorded
    /// the gap once, and the retries record no more of them.
    #[tokio::test]
    async fn retries_against_a_daemon_that_is_down_record_no_further_gap() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("attach.sock");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind the scripted daemon");
        let server = tokio::spawn(async move {
            // One subscription, ended as `lagged`; then the daemon goes away.
            let (stream, _) = listener.accept().await.expect("the subscriber connects");
            let (mut rd, mut wr) = stream.into_split();
            let _ = read_frame(&mut rd).await;
            let ok = serde_json::to_vec(&AttachResponse::ok()).unwrap();
            write_frame(&mut wr, KIND_RESP, &ok).await.unwrap();
            write_frame(&mut wr, KIND_STREAM_END, b"lagged")
                .await
                .unwrap();
        });

        let state: SharedState = Arc::new(RwLock::new(AppState::default()));
        let config = SubscriberConfig {
            initial_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(20),
            drop_event: None,
            break_on_event: None,
        };
        let subscriber = tokio::spawn(run(
            DaemonClient::new(socket.clone()),
            Arc::clone(&state),
            config,
        ));
        server
            .await
            .expect("the scripted daemon served its one stream");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while state.read().await.event_stream_gaps() == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the stream's end was never recorded as a gap"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::fs::remove_file(&socket)
            .await
            .expect("the daemon's socket goes with it");
        // A dozen or more retries at this backoff.
        tokio::time::sleep(Duration::from_millis(400)).await;
        let gaps = state.read().await.event_stream_gaps();
        subscriber.abort();
        assert_eq!(
            gaps, 1,
            "retries that cannot reach the daemon must not each record another gap"
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
