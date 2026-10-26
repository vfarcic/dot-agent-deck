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

use crate::daemon_client::{ClientError, DaemonClient};
use crate::event::{AgentEvent, BroadcastMsg};
use crate::state::{ProtocolRefusal, SharedState};

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
    /// Reviewer R9T-05 e2e seam: told of each event once it is applied to the
    /// state, so a PTY test can wait for an event to be in the TUI's state,
    /// not only on its socket. `None` in a shipped binary, like
    /// [`Self::drop_event`].
    pub applied: Option<fn(&AgentEvent)>,
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
            applied: None,
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
            subscribe(&client, &state).await
        };
        // PRD #1666 part 2 (`lifecycle/handshake/011`): every subscription, the
        // first included, is opened only after issue #405's protocol check
        // passed: the daemon answering now may have replaced the one the
        // startup handshake checked. On the TUI's gated client
        // (`DaemonClient::with_protocol_gate`) that check is the `Hello` on the
        // very connection the subscription then rides. A daemon on another
        // protocol is sent nothing more; the TUI shows the refusal and keeps
        // checking at the backoff, so a daemon on this binary's protocol is
        // taken up again once one answers.
        match &subscribed {
            Ok(_) => {
                if state.write().await.protocol_refusal.take().is_some() {
                    tracing::info!(
                        "subscribe_events: the daemon speaks this binary's attach protocol again"
                    );
                }
            }
            Err(Some(refusal)) => {
                let mut state = state.write().await;
                if state.protocol_refusal != Some(*refusal) {
                    tracing::warn!(
                        daemon_protocol = ?refusal.daemon,
                        local_protocol = refusal.local,
                        "subscribe_events: the daemon speaks another attach protocol; not subscribing"
                    );
                }
                state.protocol_refusal = Some(*refusal);
            }
            Err(None) => {}
        }
        // On `Err` the failure was logged where it happened. Qodo on #1553: a
        // resync that could not read the daemon's agents leaves this state
        // unreconciled, so it does not settle on the new stream as if it were;
        // the subscriber resubscribes after the backoff, which resynchronizes
        // again, and the gap stays recorded meanwhile.
        if let Ok(mut sub) = subscribed {
            // Reset backoff on a successful subscribe (and resync).
            delay = config.initial_delay;
            seed_hook_binary_notices(&client, &state).await;
            loop {
                match sub.next_event().await {
                    Ok(Some(BroadcastMsg::Event(event))) => {
                        if config.drop_event.is_some_and(|drop| drop(&event)) {
                            continue;
                        }
                        if config.break_on_event.is_some_and(|brk| brk(&event)) {
                            break;
                        }
                        let applied = config.applied.map(|applied| (applied, event.clone()));
                        state.write().await.apply_event(event);
                        if let Some((applied, event)) = applied {
                            applied(&event);
                        }
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
                    // PRD #1666 §2.8: an inbox message changed state; the
                    // card's `✉N` / `delivered` line follows it live.
                    Ok(Some(BroadcastMsg::Inbox(notice))) => {
                        state.write().await.apply_inbox_notice(&notice);
                    }
                    // PRD #1666 §2.8: a new generation took a pane; the card's
                    // counts follow it from here.
                    Ok(Some(BroadcastMsg::InboxHandover(handover))) => {
                        state.write().await.apply_inbox_handover(&handover);
                    }
                    // Issue #1637: the daemon's hook-binary notices changed.
                    // The whole list replaces what the dashboard footer shows.
                    Ok(Some(BroadcastMsg::HookBinaryNotice(payload))) => {
                        state.write().await.hook_binary_notices = payload.notices;
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
            // PRD #1666 part 2 (G-fix6): and no stream speaks for a card
            // created meanwhile.
            let mut st = state.write().await;
            st.note_event_stream_gap();
            st.stream_incarnation = crate::daemon_client::Incarnation::UNNAMED;
        }
        tokio::time::sleep(delay).await;
        delay = std::cmp::min(delay * 2, config.max_delay);
        #[cfg(feature = "e2e")]
        e2e_hold_stream_down().await;
    }
}

/// Issue #1800 e2e seam: while the file `DOT_AGENT_DECK_E2E_HOLD_STREAM_DOWN`
/// names exists, do not resubscribe, so a PTY test can start something on the
/// daemon while this subscriber is certainly not listening, which the backoff
/// alone cannot be timed against. Writes `<that path>.held` once it holds, for
/// the test to wait on. Compiled only into the `e2e` build, like the seams
/// `main.rs` passes in [`SubscriberConfig`], so a shipped binary has no switch
/// that keeps its stream down.
#[cfg(feature = "e2e")]
async fn e2e_hold_stream_down() {
    let Some(path) = std::env::var_os("DOT_AGENT_DECK_E2E_HOLD_STREAM_DOWN") else {
        return;
    };
    let path = std::path::PathBuf::from(path);
    let mut held = path.clone().into_os_string();
    held.push(".held");
    let exists = || tokio::fs::try_exists(&path);
    if exists().await.unwrap_or(false) {
        let _ = tokio::fs::write(&held, b"").await;
    }
    while exists().await.unwrap_or(false) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The first subscription. PRD #1666 §2.8: where the daemon offers it, the
/// stream and the daemon's agents come from one `SubscribeEventsWithSnapshot`,
/// and the agents' inboxes become this state's account of every pane's inbox
/// ([`crate::state::AppState::take_inbox_snapshot`]) before the stream's first
/// notice is applied — the panes the TUI's startup hydration registers
/// included, whose own `ListAgents` reply is not ordered with the stream and
/// so is not read for their inboxes. Only the inboxes: nothing was missed
/// before the first subscription, so there is no gap to reconcile. From a
/// daemon that does not offer it, a plain `SubscribeEvents`, as before: the
/// capability shipped before the inbox did (issue #1555), so such a daemon
/// sends no inbox notices to order.
async fn subscribe(
    client: &DaemonClient,
    state: &SharedState,
) -> Result<crate::daemon_client::EventSubscription, Option<ProtocolRefusal>> {
    use crate::daemon_client::GatedQuery;
    match tokio::time::timeout(RESYNC_LIST_TIMEOUT, client.subscribe_events_with_snapshot()).await {
        Ok(Ok(GatedQuery::Answered((sub, records)))) => {
            let mut state = state.write().await;
            state.learn_stream_incarnation(learned_from(&sub));
            state.take_inbox_snapshot(&records);
            Ok(sub)
        }
        Ok(Ok(GatedQuery::Unsupported)) => match client.subscribe_events().await {
            Ok(sub) => {
                state
                    .write()
                    .await
                    .learn_stream_incarnation(learned_from(&sub));
                Ok(sub)
            }
            Err(ClientError::ProtocolMismatch { daemon, local }) => {
                Err(Some(ProtocolRefusal { daemon, local }))
            }
            Err(e) => {
                tracing::debug!(error = %e, "subscribe_events: subscribe failed, retrying");
                Err(None)
            }
        },
        Ok(Err(ClientError::ProtocolMismatch { daemon, local })) => {
            Err(Some(ProtocolRefusal { daemon, local }))
        }
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "subscribe_events: subscribe failed, retrying");
            Err(None)
        }
        Err(_) => {
            tracing::warn!(
                timeout_ms = RESYNC_LIST_TIMEOUT.as_millis() as u64,
                "subscribe_events: the first subscription timed out, retrying"
            );
            Err(None)
        }
    }
}

/// PRD #1666 part 2 (G-fix6): the daemon process `sub` comes from, which every
/// card and surface learned from it is stamped with. Unnamed from an ungated
/// client, which learns none.
fn learned_from(
    sub: &crate::daemon_client::EventSubscription,
) -> crate::daemon_client::Incarnation {
    sub.incarnation().cloned().unwrap_or_default()
}

/// Bound on the requests a resync makes before it has a reconciled stream,
/// matching the TUI's startup hydration bound in spirit: a wedged daemon must not
/// stall the subscriber, and an opened stream is not being read while they run.
const RESYNC_LIST_TIMEOUT: Duration = Duration::from_secs(5);

/// How long [`seed_hook_binary_notices`] waits for the daemon's `Hello`.
const HOOK_NOTICE_SEED_TIMEOUT: Duration = Duration::from_secs(2);

/// Issue #1637: read the daemon's hook-binary notices once a subscription is
/// open, so any change after this read is on the stream and is applied after
/// it. Whatever the read gets replaces what the state holds: a daemon that
/// omits the field (one that predates the notices, or has none) and a `Hello`
/// that fails or does not answer in time both leave no notice, since the list
/// held may be a previous daemon's.
async fn seed_hook_binary_notices(client: &DaemonClient, state: &SharedState) {
    let notices =
        match tokio::time::timeout(HOOK_NOTICE_SEED_TIMEOUT, client.hook_binary_notices()).await {
            Ok(Ok(notices)) => notices.unwrap_or_default(),
            Ok(Err(_)) | Err(_) => Vec::new(),
        };
    state.write().await.hook_binary_notices = notices;
}

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
) -> Result<crate::daemon_client::EventSubscription, Option<ProtocolRefusal>> {
    match subscribe_with_snapshot(client).await {
        Ok((sub, records)) => {
            let mut state = state.write().await;
            // PRD #1666 part 2 (G-fix6): before the snapshot, which repaints
            // only the cards learned from the process that answered. G-fix7c:
            // a card of another process is dropped here, for the render loop
            // to name in the restart notice, and the agent the answering
            // process runs on its pane gets a card of its own.
            let dropped = state.learn_stream_incarnation(learned_from(&sub));
            state.resync_after_event_gap(&records);
            state.seed_successor_cards(&dropped, &records);
            // G-fix7f: and an agent whose card-surfacing start was broadcast
            // while no stream was open gets the card it missed.
            state.card_unseen_agents(&records);
            Ok(sub)
        }
        Err(ResyncFailure::Protocol(refusal)) => Err(Some(refusal)),
        Err(ResyncFailure::Subscribe(e)) => {
            tracing::debug!(error = %e, "subscribe_events: subscribe failed, retrying");
            Err(None)
        }
        Err(ResyncFailure::Snapshot(e)) => {
            tracing::warn!(
                error = %e,
                "subscribe_events: resync after reconnect could not list agents, retrying"
            );
            state.write().await.note_event_stream_gap();
            Err(None)
        }
        Err(ResyncFailure::TimedOut) => {
            tracing::warn!(
                timeout_ms = RESYNC_LIST_TIMEOUT.as_millis() as u64,
                "subscribe_events: resync after reconnect timed out listing agents, retrying"
            );
            state.write().await.note_event_stream_gap();
            Err(None)
        }
    }
}

/// Why [`subscribe_with_snapshot`] could not hand back a stream and a snapshot.
enum ResyncFailure {
    /// PRD #1666 part 2: the daemon speaks another attach protocol, so nothing
    /// was opened or asked of it past its `Hello`.
    Protocol(ProtocolRefusal),
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
            // Two requests on two connections, so a daemon replaced between
            // them — the same build included, which the protocol check admits —
            // could answer the listing. PRD #1666 part 2 (G-fix7): the listing
            // is bound to the process that opened the stream, and is taken
            // only when it names that process too; any other listing is a
            // snapshot that could not be read, and the resync is tried again.
            let sub = client.subscribe_events().await.map_err(|e| match e {
                ClientError::ProtocolMismatch { daemon, local } => {
                    ResyncFailure::Protocol(ProtocolRefusal { daemon, local })
                }
                e => ResyncFailure::Subscribe(e),
            })?;
            let lister = match sub.incarnation() {
                Some(process) if !process.is_unnamed() => client.bound_to(process.clone()),
                _ => client.clone(),
            };
            match tokio::time::timeout(RESYNC_LIST_TIMEOUT, lister.list_agents_detailed()).await {
                Ok(Ok(listing)) if listing.incarnation.as_ref() == sub.incarnation() => {
                    Ok((sub, listing.records))
                }
                Ok(Ok(_)) => Err(ResyncFailure::Snapshot(ClientError::DaemonRestarted)),
                Ok(Err(ClientError::ProtocolMismatch { daemon, local })) => {
                    Err(ResyncFailure::Protocol(ProtocolRefusal { daemon, local }))
                }
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
        Ok(Err(ClientError::ProtocolMismatch { daemon, local })) => {
            Err(ResyncFailure::Protocol(ProtocolRefusal { daemon, local }))
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
            background: None,
            background_revision: None,
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
    /// subscription — a `SubscribeEventsWithSnapshot` where `ordered`, whose
    /// snapshot lists `agents` with their `inboxes` and counts as no
    /// `ListAgents` — carries `first_notices` and `first_stream` and is then
    /// torn down with
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
        /// The first subscription stays open after `first_stream` instead of
        /// being torn down `lagged`.
        first_stream_open: bool,
        /// Inbox notices the first subscription carries before `first_stream`.
        first_notices: Vec<crate::event::InboxNotice>,
        /// The inbox summary each listed pane's record carries.
        inboxes: Vec<(&'static str, crate::inbox_hub::InboxSummary)>,
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
                first_stream_open: false,
                first_notices: Vec::new(),
                inboxes: Vec::new(),
            }
        }

        /// The listing reply, or `None` for a scripted refusal.
        fn listing(&self, n: &Counters) -> Option<Vec<AgentRecord>> {
            if n.lists.fetch_add(1, Ordering::SeqCst) < self.failing_lists {
                return None;
            }
            Some(self.records())
        }

        /// The daemon's agents, as a listing reports them.
        fn records(&self) -> Vec<AgentRecord> {
            let mut records: Vec<AgentRecord> = self
                .agents
                .iter()
                .map(|(agent, pane)| {
                    serde_json::from_value(serde_json::json!({ "id": agent, "pane_id_env": pane }))
                        .unwrap()
                })
                .collect();
            self.daemon.attach_live_sessions(&mut records);
            for record in &mut records {
                record.inbox = self
                    .inboxes
                    .iter()
                    .find(|(pane, _)| record.pane_id_env.as_deref() == Some(*pane))
                    .map(|(_, summary)| summary.clone());
            }
            records
        }

        /// The first subscription's frames after its reply: `first_notices`,
        /// `first_stream`, then the `lagged` tear-down unless
        /// `first_stream_open`, in which case it is held open.
        async fn first_frames(
            &self,
            rd: &mut tokio::net::unix::OwnedReadHalf,
            wr: &mut tokio::net::unix::OwnedWriteHalf,
        ) {
            for notice in &self.first_notices {
                let msg = serde_json::to_vec(&BroadcastMsg::Inbox(notice.clone())).unwrap();
                write_frame(wr, KIND_EVENT, &msg).await.unwrap();
            }
            write_events(wr, &self.first_stream).await;
            if self.first_stream_open {
                let _ = read_frame(rd).await;
                return;
            }
            write_frame(wr, KIND_STREAM_END, b"lagged").await.unwrap();
        }
    }

    struct Counters {
        subscriptions: AtomicUsize,
        lists: AtomicUsize,
        /// How many `SubscribeEventsWithSnapshot` requests arrived to
        /// resynchronize, the first subscription's not counted.
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
                            script.first_frames(&mut rd, &mut wr).await;
                        } else {
                            write_events(&mut wr, &script.queued).await;
                            write_events(&mut wr, &script.after_snapshot).await;
                            // Held open until the client goes away.
                            let _ = read_frame(&mut rd).await;
                        }
                    }
                    AttachRequest::SubscribeEventsWithSnapshot => {
                        assert!(script.ordered, "sent to a daemon that never advertised it");
                        if n.subscriptions.fetch_add(1, Ordering::SeqCst) == 0 {
                            let resp = AttachResponse::agent_records(script.records());
                            let resp = serde_json::to_vec(&resp).unwrap();
                            write_frame(&mut wr, KIND_RESP, &resp).await.unwrap();
                            script.first_frames(&mut rd, &mut wr).await;
                            return;
                        }
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
            applied: None,
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

    /// PRD #1666 §2.8: the first subscription opens with the daemon's agents
    /// read at the same instant, and their inboxes become the TUI's account of
    /// each pane before any notice is applied — the pane the TUI's startup
    /// hydration registers afterwards included. A notice after the snapshot
    /// counts on top of it, and one for a pane the snapshot does not list is
    /// dropped. Hydration registers the pane and draws its card, and leaves
    /// its inbox to the ordered account.
    #[tokio::test]
    async fn the_first_subscription_takes_every_panes_inbox_from_its_snapshot() {
        use crate::inbox_display::IntegrationNotice::RefreshFailedPi;
        use crate::inbox_hub::{InboxCardNotices, InboxSummary, WakePath};
        let summary = |queued, integration| InboxSummary {
            queued,
            fetched: 0,
            claimed: 0,
            undeliverable: 0,
            last_acked_at: None,
            wake: WakePath::Doorbell,
            component: None,
            wake_stalled: false,
            wake_hold: None,
            notices: InboxCardNotices {
                integration,
                ..Default::default()
            },
        };
        let queued = |pane: &str, id: &str| crate::event::InboxNotice {
            pane_id: pane.into(),
            agent_id: Some(AGENT.into()),
            message_id: id.into(),
            subject: "subject".into(),
            from_label: "the orchestrator".into(),
            state: crate::inbox::StateName::Queued,
            reason: None,
            returned: false,
            wake_stalled: false,
            wake_hold: None,
            wake_update: false,
            attention_update: false,
            requeued: false,
            notices: Default::default(),
            provenance: None,
        };

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("attach.sock");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind the scripted daemon");
        let counters = Arc::new(Counters {
            subscriptions: AtomicUsize::new(0),
            lists: AtomicUsize::new(0),
            ordered_subscriptions: AtomicUsize::new(0),
        });
        let mut script = ScriptedDaemon::new(
            Vec::new(),
            AppState::default(),
            vec![(AGENT, PANE)],
            0,
            true,
        );
        script.first_stream_open = true;
        script.inboxes = vec![(PANE, summary(1, Some(RefreshFailedPi)))];
        script.first_notices = vec![queued(PANE, "m-2"), queued("pane-unlisted", "m-3")];
        let server = tokio::spawn(serve(listener, Arc::new(script), Arc::clone(&counters)));
        let state: SharedState = Arc::new(RwLock::new(AppState::default()));
        let config = SubscriberConfig {
            initial_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(50),
            drop_event: None,
            break_on_event: None,
            applied: None,
        };
        let subscriber = tokio::spawn(run(DaemonClient::new(socket), Arc::clone(&state), config));

        let pending = |st: &AppState| {
            st.sessions
                .values()
                .find(|s| s.pane_id.as_deref() == Some(PANE))
                .and_then(|s| s.card_inbox())
                .map(|inbox| (inbox.pending, inbox.integration))
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut seen = None;
        while seen != Some((2, Some(RefreshFailedPi))) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let mut st = state.write().await;
            if st
                .sessions
                .values()
                .all(|s| s.pane_id.as_deref() != Some(PANE))
            {
                // The startup hydration: the pane is registered and its card
                // drawn; its inbox is the ordered account's.
                st.register_pane(PANE.to_string());
                st.insert_placeholder_session(PANE.to_string(), None, None, Some(AGENT.into()));
            }
            seen = pending(&st);
        }
        subscriber.abort();
        server.abort();
        assert_eq!(
            seen,
            Some((2, Some(RefreshFailedPi))),
            "the snapshot's queued message and warning, and the message queued after it"
        );
        assert_eq!(
            counters.subscriptions.load(Ordering::SeqCst),
            1,
            "one subscription"
        );
        assert_eq!(counters.lists.load(Ordering::SeqCst), 0, "no other listing");
        let st = state.read().await;
        assert!(
            st.inbox_attention("pane-unlisted").is_none(),
            "a notice for a pane no account lists is dropped"
        );
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

    /// Scenario (issue #1637, Qodo on #1656): the TUI holds a previous
    /// daemon's hook-binary notice and reconnects. A daemon whose `Hello`
    /// carries notices replaces it; one whose `Hello` omits the field, and a
    /// `Hello` that fails, both leave the TUI with no notice rather than the
    /// previous daemon's.
    #[tokio::test]
    async fn a_reconnect_replaces_or_clears_the_hook_binary_notices() {
        use crate::hook_binary::{HookBinaryNotice, HookBinaryReason};
        let stale = HookBinaryNotice {
            binary: "/opt/old/dot-agent-deck".into(),
            agents: vec!["Codex".into()],
            version: Some("0.45.0".into()),
            daemon_version: "0.46.0".into(),
            reason: HookBinaryReason::Older,
            remedy: crate::hook_binary::REMEDY_UPGRADE_OR_REINSTALL.into(),
            command: None,
        };
        let fresh = HookBinaryNotice {
            binary: "/opt/other/dot-agent-deck".into(),
            ..stale.clone()
        };
        let dir = tempfile::tempdir().unwrap();
        for (name, reply, expected) in [
            ("with", Some(Some(vec![fresh.clone()])), vec![fresh.clone()]),
            ("omitted", Some(None), Vec::new()),
            ("failed", None, Vec::new()),
        ] {
            let socket = dir.path().join(format!("{name}.sock"));
            let server = match reply {
                Some(notices) => {
                    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
                    Some(tokio::spawn(async move {
                        let (stream, _) = listener.accept().await.unwrap();
                        let (mut rd, mut wr) = stream.into_split();
                        let _ = read_frame(&mut rd).await;
                        let mut resp = AttachResponse::hello(PROTOCOL_VERSION);
                        resp.hook_binary_notices = notices;
                        let resp = serde_json::to_vec(&resp).unwrap();
                        write_frame(&mut wr, KIND_RESP, &resp).await.unwrap();
                    }))
                }
                // Nothing listens: the `Hello` fails at the connect.
                None => None,
            };
            let state: SharedState = Arc::new(RwLock::new(AppState::default()));
            state.write().await.hook_binary_notices = vec![stale.clone()];
            seed_hook_binary_notices(&DaemonClient::new(socket), &state).await;
            assert_eq!(state.read().await.hook_binary_notices, expected, "{name}");
            if let Some(server) = server {
                server.await.unwrap();
            }
        }
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
            // The first subscription asks first whether the ordered one is
            // offered; this daemon offers nothing.
            let (stream, _) = listener.accept().await.expect("the subscriber connects");
            let (mut rd, mut wr) = stream.into_split();
            let _ = read_frame(&mut rd).await;
            let hello = serde_json::to_vec(&AttachResponse::hello(PROTOCOL_VERSION)).unwrap();
            write_frame(&mut wr, KIND_RESP, &hello).await.unwrap();
            let (stream, _) = listener.accept().await.expect("the subscriber subscribes");
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
            applied: None,
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

    /// Scenario: the TUI's event subscriber opens its stream on the daemon process `a` and a hook event paints a card for agent `1` on `pane-1`; the daemon restarts as the same build, the subscriber resubscribes on `b`, and `b` sends a Thinking frame for its own agent `1` on `pane-1` under the card's key. The old card is dropped and queued for the restart notice, and `b`'s agent has a card of its own from the resync's snapshot, bound to `b`, which the frame turns to Thinking; nothing of `a` is updated. Without the restart the same frame updates the card in place (control).
    #[spec("lifecycle/handshake/013")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handshake_013_a_new_processs_frame_never_updates_an_old_card() {
        use crate::daemon_client::Incarnation;
        use crate::daemon_client::restart_fixture::RestartingDaemon;
        let named = |process: &str| Incarnation::from(Some(process.to_string()));
        for restarted in [false, true] {
            let daemon = RestartingDaemon::spawn("a");
            let client = DaemonClient::new(daemon.path.clone()).with_protocol_gate();
            let state: SharedState = Arc::new(RwLock::new(AppState::default()));

            let first = subscribe(&client, &state).await.expect("the first stream");
            assert_eq!(state.read().await.stream_incarnation, named("a"));
            state.write().await.apply_event(event_on(
                "pane-1",
                "1",
                "card",
                EventType::SessionStart,
                1,
            ));
            drop(first);

            if restarted {
                daemon.restart_as("b");
            }
            let _second = resubscribe(&client, &state)
                .await
                .expect("the resubscription");
            if restarted {
                // From the restarted deck's snapshot, before it reports.
                let state = state.read().await;
                let seeded: Vec<_> = state
                    .sessions
                    .values()
                    .filter(|session| session.pane_id.as_deref() == Some("pane-1"))
                    .map(|session| session.incarnation.clone())
                    .collect();
                assert_eq!(
                    seeded,
                    vec![named("b")],
                    "the restarted deck's agent has no card"
                );
            }
            state.write().await.apply_event(event_on(
                "pane-1",
                "1",
                "card",
                EventType::Thinking,
                2,
            ));
            let state = state.read().await;
            if !restarted {
                let card = state.sessions.get("card").expect("the painted card");
                assert!(state.pending_restart_drops.is_empty(), "control");
                assert_eq!(card.status, SessionStatus::Thinking, "control");
                assert_eq!(
                    card.bound_pane(),
                    Some(crate::pane::BoundPane::new("pane-1", named("a"))),
                    "control"
                );
                continue;
            }
            assert_eq!(state.stream_incarnation, named("b"));
            let dropped: Vec<_> = state
                .pending_restart_drops
                .iter()
                .map(|card| {
                    (
                        card.key.as_str(),
                        card.pane_id.as_deref(),
                        &card.incarnation,
                    )
                })
                .collect();
            assert_eq!(
                dropped,
                vec![("card", Some("pane-1"), &named("a"))],
                "the old card was not dropped for the notice"
            );
            assert!(
                state
                    .sessions
                    .values()
                    .all(|card| !card.incarnation.same_process(&named("a"))),
                "a card of `a` was kept"
            );
            let successors: Vec<_> = state
                .sessions
                .iter()
                .filter(|(_, session)| session.pane_id.as_deref() == Some("pane-1"))
                .collect();
            assert_eq!(successors.len(), 1, "one card of its own: {successors:?}");
            let (_, successor) = successors[0];
            assert_eq!(successor.status, SessionStatus::Thinking);
            assert_eq!(
                successor.bound_pane(),
                Some(crate::pane::BoundPane::new("pane-1", named("b")))
            );
        }
    }

    /// Scenario: the TUI's event stream opens on deck process `a` with no cards, then ends; while it is down, an agent is started on `pane-1` — the deck's card-surfacing start is broadcast to no one. When the stream comes back, on the restarted `b` or on `a` again, the resync's snapshot lists that agent and it has a card bound to the stream's process, rather than none until the TUI restarts.
    #[spec("lifecycle/handshake/013")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handshake_013_an_agent_started_while_the_stream_was_down_gets_a_card() {
        use crate::daemon_client::Incarnation;
        use crate::daemon_client::restart_fixture::RestartingDaemon;
        let named = |process: &str| Incarnation::from(Some(process.to_string()));
        for restarted in [false, true] {
            let daemon = RestartingDaemon::spawn("a");
            let client = DaemonClient::new(daemon.path.clone()).with_protocol_gate();
            let state: SharedState = Arc::new(RwLock::new(AppState::default()));
            let first = subscribe(&client, &state).await.expect("the first stream");
            assert!(state.read().await.sessions.is_empty());
            drop(first);
            state.write().await.note_event_stream_gap();
            let process = if restarted { "b" } else { "a" };
            if restarted {
                daemon.restart_as("b");
            }
            let _second = resubscribe(&client, &state)
                .await
                .expect("the resubscription");
            let state = state.read().await;
            let cards: Vec<_> = state
                .sessions
                .values()
                .filter(|card| card.pane_id.as_deref() == Some("pane-1"))
                .map(|card| (card.agent_id.clone(), card.incarnation.clone()))
                .collect();
            assert_eq!(
                cards,
                vec![(Some("1".to_string()), named(process))],
                "restarted={restarted}: the agent started while the stream was down has no card"
            );
        }
    }

    /// Scenario: after a gap, the deck's snapshot lists a named agent with no card, two roles of one orchestration with no card, a role whose membership carries no orchestration id, and an agent whose pane already has the card its start drew, with no agent id. Only the first is titled with its name; the two roles are queued as one surface, in role order, for the render loop to build their tab, and each gets the card that tab shows (issue #1800); the id-less role gets neither; and the carded pane keeps its one card and its name.
    #[spec("lifecycle/handshake/013")]
    #[test]
    fn handshake_013_unseen_agents_are_carded_once_and_roles_are_surfaced_for_their_tab() {
        use crate::daemon_client::Incarnation;
        use crate::event::OrchestrationSurfaceRole;
        let mut tui = AppState::default();
        tui.learn_stream_incarnation(Incarnation::from(Some("b".to_string())));
        tui.apply_event(event_on("carded", "3", "card", EventType::SessionStart, 1));
        // As the deck's card-surfacing start draws it: no agent id, its name.
        if let Some(card) = tui.sessions.get_mut("card") {
            card.agent_id = None;
            card.display_name = Some("surfaced".into());
        }
        let records: Vec<AgentRecord> = serde_json::from_value(serde_json::json!([
            { "id": "1", "pane_id_env": "named", "display_name": "held-successor" },
            { "id": "2", "pane_id_env": "worker", "cwd": "/work/o/sub", "tab_membership": {
                "kind": "orchestration", "name": "o", "role_index": 1,
                "role_name": "worker", "orchestration_cwd": "/work/o",
                "display_title": "o run", "orchestration_id": "o-1" } },
            { "id": "3", "pane_id_env": "carded" },
            { "id": "4", "pane_id_env": "lead", "tab_membership": {
                "kind": "orchestration", "name": "o", "role_index": 0,
                "role_name": "lead", "is_start_role": true,
                "orchestration_cwd": "/work/o", "orchestration_id": "o-1" } },
            { "id": "5", "pane_id_env": "tokenless", "cwd": "/work/t", "tab_membership": {
                "kind": "orchestration", "name": "t", "role_index": 0,
                "role_name": "lead", "is_start_role": true } },
        ]))
        .unwrap();
        tui.card_unseen_agents(&records);
        let on = |pane: &str| {
            tui.sessions
                .values()
                .filter(|card| card.pane_id.as_deref() == Some(pane))
                .map(|card| card.display_name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(on("named"), vec![Some("held-successor".to_string())]);
        for role in ["lead", "worker"] {
            assert_eq!(
                on(role),
                vec![None],
                "role {role}, whose tab is queued, has no card for it"
            );
        }
        assert_eq!(
            on("tokenless"),
            Vec::<Option<String>>::new(),
            "a role no tab is built for was carded on the dashboard"
        );
        assert_eq!(
            on("carded"),
            vec![Some("surfaced".to_string())],
            "a carded pane got a second card, or lost its own"
        );
        let role = |pane_id: &str, role_index: usize, role_name: &str, is_start_role: bool| {
            OrchestrationSurfaceRole {
                pane_id: pane_id.into(),
                role_index,
                role_name: role_name.into(),
                is_start_role,
            }
        };
        assert_eq!(
            tui.pending_orchestration_surfaces,
            vec![(
                crate::event::OrchestrationSurface {
                    name: "o".into(),
                    cwd: "/work/o".into(),
                    display_title: Some("o run".into()),
                    orchestration_id: Some("o-1".into()),
                    roles: vec![
                        role("lead", 0, "lead", true),
                        role("worker", 1, "worker", false)
                    ],
                    context_path: None,
                },
                Incarnation::from(Some("b".to_string())),
            )],
            "the unseen roles were not queued as their orchestration's one surface"
        );
    }

    /// Scenario: after a gap, the deck's snapshot lists three orchestrations. In `mixed`, one role is carded and placed in its tab and the other has no card; in `tabless`, the only role has a card but was never placed in a tab; in `shown`, the only role is carded and placed. `mixed` is queued with both roles, so the render loop places the missing one beside the one its tab holds, and only the uncarded role gets a new card; `tabless` is queued so its tab is built; `shown` is not queued (issue #1800).
    #[spec("lifecycle/handshake/013")]
    #[test]
    fn handshake_013_an_orchestration_with_any_unseen_role_is_surfaced_whole() {
        use crate::daemon_client::Incarnation;
        let mut tui = AppState::default();
        tui.learn_stream_incarnation(Incarnation::from(Some("b".to_string())));
        for (pane, agent) in [("m0", "1"), ("t0", "3"), ("s0", "4")] {
            tui.apply_event(event_on(pane, agent, pane, EventType::SessionStart, 1));
        }
        for (pane, role) in [("m0", "lead"), ("s0", "lead")] {
            tui.pane_role_map.insert(pane.into(), role.into());
        }
        let role = |id: &str, pane: &str, name: &str, index: usize| {
            serde_json::json!({ "id": id, "pane_id_env": pane, "tab_membership": {
                "kind": "orchestration", "name": name, "role_index": index,
                "role_name": format!("r{index}"), "is_start_role": index == 0,
                "orchestration_cwd": format!("/work/{name}"),
                "orchestration_id": format!("{name}-id") } })
        };
        let records: Vec<AgentRecord> = serde_json::from_value(serde_json::json!([
            role("1", "m0", "mixed", 0),
            role("2", "m1", "mixed", 1),
            role("3", "t0", "tabless", 0),
            role("4", "s0", "shown", 0),
        ]))
        .unwrap();
        tui.card_unseen_agents(&records);
        let queued: Vec<(String, Vec<String>)> = tui
            .pending_orchestration_surfaces
            .iter()
            .map(|(surface, _)| {
                (
                    surface.name.clone(),
                    surface.roles.iter().map(|r| r.pane_id.clone()).collect(),
                )
            })
            .collect();
        assert_eq!(
            queued,
            vec![
                (
                    "mixed".to_string(),
                    vec!["m0".to_string(), "m1".to_string()]
                ),
                ("tabless".to_string(), vec!["t0".to_string()]),
            ],
            "the orchestrations with an unseen role were not queued whole, or a shown one was"
        );
        let cards_on = |pane: &str| {
            tui.sessions
                .values()
                .filter(|card| card.pane_id.as_deref() == Some(pane))
                .count()
        };
        for (pane, cards) in [("m0", 1), ("m1", 1), ("t0", 1), ("s0", 1)] {
            assert_eq!(
                cards_on(pane),
                cards,
                "pane {pane} has the wrong number of cards"
            );
        }
    }

    /// Scenario: after a gap the deck's snapshot lists an idle agent with no card, and the TUI cards it. The agent then reports it is thinking, with no new session start, and its card says Thinking; a later resync whose snapshot says it waits for input moves the card there. A report applied while the stream comes from another deck process does not update the card: the card is dropped for the restart notice, and the report draws that process's own card (control).
    #[spec("lifecycle/handshake/013")]
    #[test]
    fn handshake_013_a_carded_unseen_agent_takes_its_later_reports() {
        use crate::daemon_client::Incarnation;
        let named = |process: &str| Incarnation::from(Some(process.to_string()));
        let listed = |status: &str| -> Vec<AgentRecord> {
            serde_json::from_value(serde_json::json!([{
                "id": "1",
                "pane_id_env": "unseen",
                "live": { "status": status, "tool_count": 0 },
            }]))
            .unwrap()
        };
        let on = |tui: &AppState| {
            tui.sessions
                .values()
                .filter(|card| card.pane_id.as_deref() == Some("unseen"))
                .map(|card| (card.status.clone(), card.incarnation.clone()))
                .collect::<Vec<_>>()
        };
        let now = Utc::now().timestamp();
        for replaced in [false, true] {
            let mut tui = AppState::default();
            tui.learn_stream_incarnation(named("b"));
            tui.card_unseen_agents(&listed("Idle"));
            assert_eq!(
                on(&tui),
                vec![(SessionStatus::Idle, named("b"))],
                "precondition"
            );
            if replaced {
                // Not through `learn_stream_incarnation`, which drops the card
                // itself: the report's own guard is what this pins.
                tui.stream_incarnation = named("c");
            }
            tui.apply_event(event_on(
                "unseen",
                "1",
                "turn",
                EventType::Thinking,
                now + 1,
            ));
            if replaced {
                assert_eq!(
                    on(&tui),
                    vec![(SessionStatus::Thinking, named("c"))],
                    "control: another process's report updated the card"
                );
                let dropped = tui.take_restart_drops();
                assert_eq!(dropped.len(), 1, "control: the card was not dropped");
                assert_eq!(dropped[0].incarnation, named("b"));
                continue;
            }
            assert_eq!(
                on(&tui),
                vec![(SessionStatus::Thinking, named("b"))],
                "the carded agent's report was refused"
            );
            tui.resync_after_event_gap(&listed("WaitingForInput"));
            assert_eq!(
                on(&tui),
                vec![(SessionStatus::WaitingForInput, named("b"))],
                "a later resync did not refresh the card"
            );
        }
    }

    /// Scenario: the TUI holds a card for agent `1` on `pane-1` learned from deck process `a`, its stream's process, and then gets the placeholder card for a pane it attached on `b` with the same pane and agent id — the deck restarted as the same build, and the stream has not noticed yet. The placeholder is not the old card and does not replace it: it waits, and the old card stays, until the stream itself moves to `b`; then the old card is dropped and queued for the restart notice, and the placeholder is drawn bound to `b`. A placeholder learned from `a` itself is the card already there (control).
    #[spec("lifecycle/handshake/013")]
    #[test]
    fn handshake_013_a_placeholder_never_adopts_another_processs_card() {
        use crate::daemon_client::Incarnation;
        let named = |process: &str| Incarnation::from(Some(process.to_string()));
        for restarted in [false, true] {
            let mut tui = AppState::default();
            tui.learn_stream_incarnation(named("a"));
            tui.apply_event(event_on("pane-1", "1", "card", EventType::SessionStart, 1));
            let attached_on = named(if restarted { "b" } else { "a" });

            let key = tui.insert_placeholder_session_learned(
                "pane-1".into(),
                None,
                None,
                Some("1".into()),
                attached_on.clone(),
            );

            if !restarted {
                assert_eq!(key, "card", "control: the card already there");
                assert!(tui.pending_restart_drops.is_empty());
                continue;
            }
            assert_ne!(
                key, "card",
                "the placeholder adopted the old process's card"
            );
            assert!(
                tui.sessions.contains_key("card"),
                "the placeholder dropped a card before the stream moved"
            );
            assert!(
                !tui.sessions.contains_key(&key),
                "drawn before the stream moved"
            );
            assert!(tui.pending_restart_drops.is_empty());

            tui.learn_stream_incarnation(attached_on.clone());
            assert!(!tui.sessions.contains_key("card"), "the old card was kept");
            assert_eq!(
                tui.pending_restart_drops
                    .iter()
                    .map(|card| card.key.as_str())
                    .collect::<Vec<_>>(),
                vec!["card"]
            );
            assert_eq!(
                tui.sessions[&key].bound_pane(),
                Some(crate::pane::BoundPane::new("pane-1", attached_on))
            );
        }
    }

    /// Scenario: the TUI's stream is on deck process `b` when an attach the stream never named, `a`, hands in a placeholder for `pane-1`. It is not drawn, so the next frame from `b` for `pane-1`, its agent's start, gives `b`'s agent the pane's only card; the held placeholder is queued for the restart notice once the stream next opens on a process other than `a`. A placeholder learned from `b` itself is updated by the frame (control).
    #[spec("lifecycle/handshake/013")]
    #[test]
    fn handshake_013_a_frame_drops_another_processs_card_on_its_pane() {
        use crate::daemon_client::Incarnation;
        let named = |process: &str| Incarnation::from(Some(process.to_string()));
        for restarted in [false, true] {
            let mut tui = AppState::default();
            tui.learn_stream_incarnation(named("b"));
            let key = tui.insert_placeholder_session_learned(
                "pane-1".into(),
                None,
                None,
                None,
                named(if restarted { "a" } else { "b" }),
            );
            tui.apply_event(event_on("pane-1", "1", "card", EventType::SessionStart, 1));
            let on_pane: Vec<_> = tui
                .sessions
                .values()
                .filter(|card| card.pane_id.as_deref() == Some("pane-1"))
                .map(|card| card.incarnation.clone())
                .collect();
            assert_eq!(on_pane, vec![named("b")], "one card of b's on the pane");
            if !restarted {
                assert!(tui.pending_restart_drops.is_empty(), "control");
                continue;
            }
            assert!(!tui.sessions.contains_key(&key), "a's card was drawn");
            assert!(
                tui.pending_restart_drops.is_empty(),
                "reported before the stream said"
            );
            tui.learn_stream_incarnation(named("b"));
            assert_eq!(
                tui.pending_restart_drops
                    .iter()
                    .map(|card| card.key.as_str())
                    .collect::<Vec<_>>(),
                vec![key.as_str()],
                "a's placeholder was not queued for the notice"
            );
            assert!(tui.sessions.contains_key("card"), "b's card was dropped");
        }
    }

    /// Scenario: a TUI resynchronizes against a deck that predates the ordered subscribe-with-snapshot request, so it subscribes and then lists on a second connection; the deck restarts as the same build between the two. From a stream on the named process `a`, the restarted process is sent no listing and the resync fails; from a stream on a deck that named no process, the restarted deck's listing names `b` and is not taken either. With no restart, the listing is taken in both cases (control).
    #[spec("lifecycle/handshake/013")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handshake_013_a_fallback_snapshot_comes_from_the_streams_own_process() {
        use crate::daemon_client::restart_fixture::RestartingDaemon;
        for first in ["a", "unnamed-a"] {
            for restarted in [false, true] {
                let daemon = RestartingDaemon::spawn(first);
                daemon.withhold(crate::daemon_protocol::CAP_SUBSCRIBE_EVENTS_WITH_SNAPSHOT);
                if restarted {
                    daemon.restart_after("subscribe-events", "b");
                }
                let client = DaemonClient::new(daemon.path.clone()).with_protocol_gate();

                let answer = subscribe_with_snapshot(&client).await;

                let case = format!("first={first} restarted={restarted}");
                let lister = if restarted { "b" } else { first };
                let listed = daemon
                    .received_by(lister)
                    .contains(&"list-agents".to_string());
                // A named stream binds the listing, so the successor is sent
                // none; an unnamed one cannot, and refuses what answers.
                assert_eq!(listed, !restarted || first != "a", "{case}");
                match answer {
                    Ok((sub, records)) => {
                        assert!(!restarted, "{case}: another process's listing was taken");
                        let process = sub.incarnation().and_then(|i| i.as_deref());
                        assert_eq!(process, (first == "a").then_some("a"), "{case}");
                        assert_eq!(records.len(), 1, "{case}");
                    }
                    Err(ResyncFailure::Snapshot(_)) => {
                        assert!(
                            restarted,
                            "{case}: the stream's own process's listing was refused"
                        );
                    }
                    Err(_) => panic!("{case}: not a snapshot failure"),
                }
            }
        }
    }

    /// Scenario: a card painted while the TUI's stream came from the daemon process `a` is managed by the TUI, and the daemon restarts as the same build; the resync after the reconnect brings a snapshot from `b` whose record has the same pane and agent id and a different status. The old card is not repainted as `b`'s agent and keeps `a`; resynchronizing from `a` itself repaints it (control).
    #[spec("lifecycle/handshake/013")]
    #[test]
    fn handshake_013_a_resync_from_a_restarted_daemon_repaints_no_old_card() {
        use crate::daemon_client::Incarnation;
        let named = |process: &str| Incarnation::from(Some(process.to_string()));
        for restarted in [false, true] {
            let mut tui = AppState::default();
            tui.stream_incarnation = named("a");
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

            if restarted {
                tui.stream_incarnation = named("b");
            }
            tui.resync_after_event_gap(&records);
            let card = tui.sessions.get("gen-a").expect("the card");
            assert_eq!(card.incarnation, named("a"));
            if restarted {
                assert_ne!(
                    card.status,
                    SessionStatus::Idle,
                    "the old card was repainted as the restarted daemon's agent"
                );
            } else {
                assert_eq!(card.status, SessionStatus::Idle, "control");
            }
        }
    }

    /// Scenario: a daemon-spawned orchestration's surface arrives while the TUI's stream comes from the daemon process `a`, and waits in the render loop's queue. It is queued with `a`, so its role panes are attached on `a` only, whichever daemon answers when the render loop gets to it.
    #[spec("lifecycle/handshake/013")]
    #[test]
    fn handshake_013_a_queued_surface_keeps_the_process_whose_stream_carried_it() {
        use crate::daemon_client::Incarnation;
        let named = |process: &str| Incarnation::from(Some(process.to_string()));
        let mut tui = AppState::default();
        tui.stream_incarnation = named("a");
        tui.queue_orchestration_surface(crate::event::OrchestrationSurface {
            name: "team".into(),
            cwd: "/work".into(),
            display_title: None,
            orchestration_id: None,
            roles: vec![crate::event::OrchestrationSurfaceRole {
                pane_id: "sched-team-0-r0".into(),
                role_index: 0,
                role_name: "orchestrator".into(),
                is_start_role: true,
            }],
            context_path: None,
        });
        tui.stream_incarnation = named("b");
        assert_eq!(tui.pending_orchestration_surfaces[0].1, named("a"));
    }
}
