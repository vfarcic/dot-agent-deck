//! PRD #1497 M5 — reading mode's Rust half: from an agent's turn events to the
//! sentences the app speaks.
//!
//! Reading mode is turned on for the open agent's pane ("reading on"), and from
//! then on the app speaks a short summary of each turn that agent finishes, and
//! its permission prompts and errors as they happen. The webview owns the mode
//! — which agent, when it ends — and the speech queue; this module owns what
//! is said, and it runs Rust-side so **the agent's reply never enters the
//! webview**: the reply goes from the turn event to the summary request and
//! nowhere else, and only the finished sentence crosses the IPC boundary.
//!
//! # Where turn events come from: [`TurnEventSource`]
//!
//! Where turn events come from is the daemon's to say (PRD #1497 D5: the final
//! reply arrives in the daemon, from Claude Code's hooks, Codex's hooks and
//! session log, and the deck's OpenCode plugin, and a remote deck's files are
//! reachable only by its daemon). The shipped source is [`DaemonTurnEvents`]:
//! the daemon's `subscribe-turn-replies` stream for finished and failed turns,
//! and its status stream, filtered to the agent, for permission prompts, errors
//! and usage limits. A daemon too old to offer the reply stream is refused with
//! [`DAEMON_TOO_OLD`], so "reading on" says in plain words that reading is not
//! available rather than starting a mode that never speaks (CLAUDE.md rule 20).
//!
//! A failed turn usually reaches the app twice — as its final reply, marked
//! failed, and as the agent's status turning to Error or Blocked — on two
//! connections, in either order. [`coalesce`] holds one back for
//! [`FAILURE_COALESCE_WINDOW`] so such a turn is announced once.
//!
//! A source answers one subscription per [`ReadingTarget`] with a channel of
//! [`TurnEvent`]s for that agent alone, from the moment of subscribing — never
//! a backlog (D3). Dropping the receiver is the unsubscribe: a source's
//! forwarding task sees its `send` fail and stops.
//!
//! # What is said
//!
//! [`announce`] turns one event into one [`ReadingSentence`], always naming
//! the agent (D10). A finished or failed turn is summarised through
//! [`super::summary`], which never fails (a deterministic fallback stands in
//! for the model's sentence). A permission prompt and an error or quota block
//! are announced at once, with a fixed sentence and **no model call**: the user
//! has to act on them, and a sentence that names the agent and what it wants
//! needs no summary.
//!
//! # Consent is read before every event, and revoking it ends reading
//!
//! Reading's Settings opt-in (D4) is what allows an agent's reply to leave the
//! machine, so it is not only checked when "reading on" starts.
//! [`read_turns`] asks the summariser before announcing each event
//! ([`TurnSummariser::consented`]), and [`SettingsSummariser`] reads the
//! settings again immediately before each summary request, sending nothing
//! when the opt-in is off. Either way the session ends with a
//! [`ReadingSentenceKind::Ended`] sentence, which the webview takes as
//! "reading off": it clears the indicator and the queued speech. Saving the
//! settings with the opt-in off ends the session at once, without waiting for
//! a turn (`desktop_set_settings`).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use dot_agent_deck::daemon_client::{DaemonClient, GatedQuery};
use dot_agent_deck::daemon_protocol::FinalReply;
use dot_agent_deck::event::{AgentEvent, BroadcastMsg, EventType};
use serde::Serialize;
use tokio::sync::mpsc;

use crate::settings::{IntentSettings, ReadingConsent, VoiceSettings};

use super::summary::{Summary, SummaryTransport, TurnKind, TurnSummaryRequest, spoken_name};

/// Why "reading on" cannot start on a deck whose daemon predates the reply
/// stream (it does not advertise `turn-replies`). A fragment, rendered into
/// [`unavailable_sentence`].
pub const DAEMON_TOO_OLD: &str =
    "this deck's daemon is too old to report finished turns. Update dot-agent-deck on that machine";

/// Why "reading on" cannot start when the deck's daemon did not answer the
/// subscription. A fragment, rendered into [`unavailable_sentence`].
pub const DAEMON_UNREACHABLE: &str = "the deck did not answer";

/// Why "reading on" cannot start when the agent's deck left the app's decks
/// while the subscription was being confirmed. A fragment, rendered into
/// [`unavailable_sentence`].
pub const DECK_CHANGED: &str = "the deck changed while reading was starting. Say reading on again";

/// How long after [`coalesce`] announced one half of a failed turn it absorbs
/// the other, so the turn is announced once.
pub const FAILURE_COALESCE_WINDOW: Duration = Duration::from_secs(3);

/// How long [`coalesce`] holds a failed reply that arrived before its turn's
/// block status, so a usage limit can still replace it. Both halves leave the
/// daemon together — from one hook line (the reply is published just before
/// the status is broadcast) or one Codex session-log poll — so this only has to
/// cover two local connections' delivery, which takes milliseconds; one second
/// leaves a wide margin while keeping the announcement prompt.
pub const FAILED_REPLY_HOLD: Duration = Duration::from_secs(1);

/// What "reading on" says while the Settings opt-in is off (D4), spoken.
pub const READING_NOT_ENABLED: &str =
    "Reading is turned off in Settings. Turn on Read turns aloud in Settings, Voice, first.";

/// The longest description of what a permission prompt wants that is spoken,
/// in characters. A prompt names a tool and its input; the first words are
/// what the user needs to decide whether to look.
pub const MAX_WANTS_CHARS: usize = 120;

/// One thing that happened to the agent being read (PRD #1497 M5's seam).
///
/// What M2's daemon subscription delivers, one per event, for the subscribed
/// agent only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnEvent {
    /// The agent finished a turn. `reply` is its final reply for that turn —
    /// Claude Code's `last_assistant_message`, Codex's `last_agent_message` —
    /// and never terminal output (D1). Empty when the agent gave none, which is
    /// spoken as the fallback.
    Finished { reply: String },
    /// The turn ended in an error. `reply` is whatever final reply the agent
    /// left, possibly empty.
    Failed { reply: String },
    /// The agent is asking permission. `wants` is what it is asking to do, in
    /// one line — "run cargo publish", "Bash: cargo publish" — and may be empty
    /// when the agent did not say.
    Permission { wants: String },
    /// The agent stopped and cannot go on by itself.
    Blocked { cause: BlockCause },
}

/// Why an agent stopped (a [`TurnEvent::Blocked`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockCause {
    /// An error ended its work.
    Error,
    /// It hit a usage or rate limit.
    Quota,
}

/// The agent a subscription is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadingTarget {
    /// The deck (daemon) the agent runs on — the fleet's deck key.
    pub deck_id: String,
    /// The agent's id on that deck.
    pub agent_id: String,
    /// The agent's type as the daemon reports it (`claude_code`, `codex`, …),
    /// when the app observes the agent; `None` when it does not.
    pub agent_type: Option<String>,
}

/// The events of one subscription.
pub type TurnEvents = mpsc::Receiver<TurnEvent>;

/// The future a [`TurnEventSource`] returns: the subscription, or why reading
/// is not available, as a sentence fragment.
pub type SubscribeFuture<'a> =
    Pin<Box<dyn Future<Output = Result<TurnEvents, String>> + Send + 'a>>;

/// Where turn events come from — the seam PRD #1497 M2 fills with a daemon
/// subscription.
///
/// `subscribe` answers `Err` with a plain fragment ("this daemon does not
/// report …") when reading cannot run for `target`: a daemon without the
/// capability, or an agent whose type reports no turn ends. The fragment is
/// spoken, so it says what is missing in the user's terms.
pub trait TurnEventSource: Send + Sync {
    fn subscribe(&self, target: &ReadingTarget) -> SubscribeFuture<'_>;
}

/// The shipped source: one deck's daemon, through the client library.
///
/// A subscription opens two connections — the daemon's reply stream for the
/// agent (withheld by the client library unless the daemon advertises it) and
/// its status stream — and one task that merges them through [`coalesce`]. The
/// task ends, closing both, when the reading session drops its receiver or the
/// reply stream ends, which the daemon does when the agent exits; the session's
/// events then end, and [`read_turns`] ends reading.
pub struct DaemonTurnEvents {
    client: Arc<DaemonClient>,
}

impl DaemonTurnEvents {
    pub fn new(client: Arc<DaemonClient>) -> Self {
        Self { client }
    }
}

impl TurnEventSource for DaemonTurnEvents {
    fn subscribe(&self, target: &ReadingTarget) -> SubscribeFuture<'_> {
        let agent_id = target.agent_id.clone();
        Box::pin(async move {
            let mut replies = match self.client.subscribe_turn_replies(&agent_id).await {
                Ok(GatedQuery::Answered(replies)) => replies,
                Ok(GatedQuery::Unsupported) => return Err(DAEMON_TOO_OLD.to_string()),
                Err(_) => return Err(DAEMON_UNREACHABLE.to_string()),
            };
            let mut statuses = self
                .client
                .subscribe_events()
                .await
                .map_err(|_| DAEMON_UNREACHABLE.to_string())?;
            let (incoming_tx, incoming) = mpsc::channel(16);
            let (events_tx, events) = mpsc::channel(16);
            tauri::async_runtime::spawn(async move {
                let status_tx = incoming_tx.clone();
                let reply_agent = agent_id.clone();
                let read_replies = async move {
                    while let Ok(Some(turn)) = replies.next_reply().await {
                        if turn.agent_id == reply_agent
                            && incoming_tx.send(Incoming::Reply(turn.reply)).await.is_err()
                        {
                            return;
                        }
                    }
                };
                let read_statuses = async move {
                    while let Ok(Some(message)) = statuses.next_event().await {
                        let BroadcastMsg::Event(event) = message else {
                            continue;
                        };
                        if let Some(status) = status_turn_event(&event, &agent_id)
                            && status_tx.send(Incoming::Status(status)).await.is_err()
                        {
                            return;
                        }
                    }
                    // A status stream that ends (a lagged subscriber) costs the
                    // announcements, not the summaries.
                    std::future::pending::<()>().await;
                };
                let merging = coalesce(incoming, events_tx);
                tokio::pin!(merging);
                tokio::select! {
                    _ = &mut merging => return,
                    _ = read_replies => {}
                    _ = read_statuses => {}
                }
                // The reply stream ended: both readers are dropped, so the
                // merge sees its input close, says what it was holding, and
                // ends the session's events.
                merging.await;
            });
            Ok(events)
        })
    }
}

/// What [`coalesce`] merges: a finished turn's reply, or a status the agent
/// reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incoming {
    Reply(FinalReply),
    Status(TurnEvent),
}

/// The status event `event` means for reading the agent `agent_id`, or `None`.
///
/// Only events naming that agent, and not an outside agent's unproven report.
/// A permission prompt is a `PermissionRequest`, or a `WaitingForInput` that is
/// not some other kind of notification (Claude Code's is installed for
/// permission prompts only). An error is an `Error` that ends work: not a
/// failed tool call (an `Error` naming a tool, after which the agent carries
/// on) and not the deck's own delivery notice about a message it could not
/// hand the agent. A usage limit is a `QuotaBlocked`.
pub fn status_turn_event(event: &AgentEvent, agent_id: &str) -> Option<TurnEvent> {
    use dot_agent_deck::event::{DELIVERY_NOTICE_METADATA_KEY, UNPROVEN_METADATA_KEY};
    use dot_agent_deck::quota_block::NOTIFICATION_TYPE_METADATA_KEY;
    if event.agent_id.as_deref() != Some(agent_id)
        || event.metadata.contains_key(UNPROVEN_METADATA_KEY)
    {
        return None;
    }
    match event.event_type {
        EventType::PermissionRequest => Some(TurnEvent::Permission {
            wants: permission_wants(event),
        }),
        EventType::WaitingForInput => event
            .metadata
            .get(NOTIFICATION_TYPE_METADATA_KEY)
            .is_none_or(|kind| kind == "permission_prompt")
            .then(|| TurnEvent::Permission {
                wants: permission_wants(event),
            }),
        EventType::Error
            if event.tool_name.is_none()
                && !event.metadata.contains_key(DELIVERY_NOTICE_METADATA_KEY) =>
        {
            Some(TurnEvent::Blocked {
                cause: BlockCause::Error,
            })
        }
        EventType::QuotaBlocked => Some(TurnEvent::Blocked {
            cause: BlockCause::Quota,
        }),
        _ => None,
    }
}

/// What a permission prompt asks to do: "Bash: cargo publish" from the tool and
/// its detail, the prompt text an agent sent instead (OpenCode), or nothing.
fn permission_wants(event: &AgentEvent) -> String {
    match (event.tool_name.as_deref(), event.tool_detail.as_deref()) {
        (Some(tool), Some(detail)) if !detail.is_empty() => format!("{tool}: {detail}"),
        (Some(tool), _) => tool.to_string(),
        _ => event.user_prompt.clone().unwrap_or_default(),
    }
}

/// Merge `incoming` into the events a reading session announces, until either
/// side closes, announcing a failed turn once.
///
/// A failed turn reaches reading in two halves — the agent's failed reply and
/// a block status (an error or a usage limit) — and the daemon publishes the
/// reply first, so the reply is usually the first to arrive. What is announced
/// depends on which arrives first:
///
/// - **A block status first** is announced immediately, with no hold, and a
///   failed reply arriving within [`FAILURE_COALESCE_WINDOW`] after it is
///   absorbed: the turn was already announced.
/// - **A failed reply first** is held for [`FAILED_REPLY_HOLD`]. A usage-limit
///   status within the hold replaces it — the usage limit says what the user
///   has to do — while an error status is absorbed into the failed turn's
///   summary, which carries the agent's own words. With no status, the reply is
///   announced when the hold runs out, and an error status within
///   [`FAILURE_COALESCE_WINDOW`] after that is still absorbed.
///
/// Everything else passes through in arrival order, a held reply first.
pub async fn coalesce(mut incoming: mpsc::Receiver<Incoming>, out: mpsc::Sender<TurnEvent>) {
    use tokio::time::{Instant, sleep_until};
    // A failed reply held back, and until when.
    let mut held: Option<(String, Instant)> = None;
    // The failure last announced, and until when it absorbs the other half.
    let mut announced: Option<(Announced, Instant)> = None;
    loop {
        let next = match &held {
            Some((_, until)) => tokio::select! {
                message = incoming.recv() => Some(message),
                _ = sleep_until(*until) => None,
                _ = out.closed() => return,
            },
            None => tokio::select! {
                message = incoming.recv() => Some(message),
                _ = out.closed() => return,
            },
        };
        let now = Instant::now();
        let recent = announced
            .filter(|(_, until)| now < *until)
            .map(|(what, _)| what);
        let mut emit = Vec::new();
        let release_held =
            |held: &mut Option<(String, Instant)>,
             emit: &mut Vec<TurnEvent>,
             announced: &mut Option<(Announced, Instant)>| {
                if let Some((reply, _)) = held.take() {
                    emit.push(TurnEvent::Failed { reply });
                    *announced = Some((Announced::Reply, now + FAILURE_COALESCE_WINDOW));
                }
            };
        match next {
            // The hold ran out with no status: announce the failed reply.
            None => release_held(&mut held, &mut emit, &mut announced),
            Some(None) => {
                release_held(&mut held, &mut emit, &mut announced);
                for event in emit {
                    let _ = out.send(event).await;
                }
                return;
            }
            Some(Some(Incoming::Reply(reply))) if reply.failed => {
                if !matches!(recent, Some(Announced::Status(_))) {
                    release_held(&mut held, &mut emit, &mut announced);
                    held = Some((reply.text, now + FAILED_REPLY_HOLD));
                }
            }
            Some(Some(Incoming::Reply(reply))) => {
                release_held(&mut held, &mut emit, &mut announced);
                emit.push(TurnEvent::Finished { reply: reply.text });
            }
            Some(Some(Incoming::Status(TurnEvent::Blocked { cause }))) => {
                match (held.is_some(), cause, recent) {
                    // The usage limit replaces the held failed reply.
                    (true, BlockCause::Quota, _) => {
                        held = None;
                        emit.push(TurnEvent::Blocked { cause });
                        announced = Some((Announced::Status(cause), now + FAILURE_COALESCE_WINDOW));
                    }
                    // The error is the held failed reply's own turn.
                    (true, BlockCause::Error, _) => {
                        release_held(&mut held, &mut emit, &mut announced)
                    }
                    // Already said: an error after this turn's failed reply or
                    // after any block, a usage limit after a usage limit.
                    (false, BlockCause::Error, Some(_))
                    | (false, BlockCause::Quota, Some(Announced::Status(BlockCause::Quota))) => {}
                    (false, cause, _) => {
                        emit.push(TurnEvent::Blocked { cause });
                        announced = Some((Announced::Status(cause), now + FAILURE_COALESCE_WINDOW));
                    }
                }
            }
            Some(Some(Incoming::Status(status))) => {
                release_held(&mut held, &mut emit, &mut announced);
                emit.push(status);
            }
        }
        for event in emit {
            if out.send(event).await.is_err() {
                return;
            }
        }
    }
}

/// What [`coalesce`] last announced of a failed turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Announced {
    /// The failed reply's summary.
    Reply,
    /// A block status.
    Status(BlockCause),
}

/// Why an agent of `agent_type` cannot be read, as a fragment, or `None` when
/// its final reply reaches the daemon (CLAUDE.md rule 20).
///
/// Every agent type the deck runs today reports it: Claude Code and Codex in
/// their `Stop` hooks (and Codex a failed turn's in its session log), OpenCode
/// through the deck's plugin, Pi through the deck's extension (the last
/// assistant message's text on its settled report), and Devin, whose hooks go
/// through the same Claude-compatible path and name the same
/// `last_assistant_message` field. Kept as the one place a future agent type
/// without a reply channel is named, so "reading on" says so in plain words.
pub fn agent_gap(agent_type: &str) -> Option<String> {
    let _ = agent_type;
    None
}

/// What "reading on" says when reading cannot run, from a source's or
/// [`agent_gap`]'s fragment.
pub fn unavailable_sentence(reason: &str) -> String {
    let reason = reason.trim().trim_end_matches('.');
    format!("Reading is not available: {reason}.")
}

/// What kind of thing a [`ReadingSentence`] says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadingSentenceKind {
    /// A finished or failed turn's summary.
    Turn,
    /// A permission prompt, announced at once.
    Permission,
    /// An error or quota block, announced at once.
    Blocked,
    /// Reading ended on this side — its Settings opt-in was turned off, or the
    /// agent's reply stream ended (the agent exited). Not a sentence to queue:
    /// the webview ends reading mode on it, clearing the indicator and the
    /// queued speech, and says "Reading off."
    Ended,
}

/// The text of the [`ReadingSentenceKind::Ended`] sentence.
pub const READING_ENDED: &str = "Reading off.";

/// The sentence that ends a reading session on this side.
pub fn ended_sentence() -> ReadingSentence {
    ReadingSentence {
        kind: ReadingSentenceKind::Ended,
        text: READING_ENDED.to_string(),
    }
}

/// One sentence to speak, as the webview receives it — and the only thing
/// about a turn that reaches it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadingSentence {
    pub kind: ReadingSentenceKind,
    pub text: String,
}

/// The future a [`TurnSummariser`] returns: the summary, or `None` when
/// reading's opt-in was off immediately before the request, so nothing was
/// sent.
pub type SummaryFuture<'a> = Pin<Box<dyn Future<Output = Option<Summary>> + Send + 'a>>;

/// Summarises one turn, and says whether reading may go on. The real one is
/// [`SettingsSummariser`] over the settings and the Commands connection; tests
/// pass their own.
pub trait TurnSummariser: Send + Sync {
    /// Whether reading's Settings opt-in is on now. Asked before every event.
    fn consented(&self) -> bool;
    /// Summarise one turn — or, with the opt-in off by the time the request
    /// would go out, send nothing and answer `None`.
    fn summarise<'a>(&'a self, request: TurnSummaryRequest<'a>) -> SummaryFuture<'a>;
}

/// The settings document's voice section, read when asked.
pub type LoadVoiceSettings = Box<dyn Fn() -> VoiceSettings + Send + Sync>;

/// A summary transport for the Commands connection the settings name.
pub type ConnectTransport = Box<dyn Fn(&IntentSettings) -> Box<dyn SummaryTransport> + Send + Sync>;

/// The real [`TurnSummariser`]: the settings read per call — so a changed
/// connection applies to the next turn and a revoked opt-in stops the next
/// request — and a transport to the Commands connection they name.
pub struct SettingsSummariser {
    load: LoadVoiceSettings,
    connect: ConnectTransport,
}

impl SettingsSummariser {
    pub fn new(load: LoadVoiceSettings, connect: ConnectTransport) -> Self {
        Self { load, connect }
    }
}

impl TurnSummariser for SettingsSummariser {
    fn consented(&self) -> bool {
        (self.load)().reading == ReadingConsent::On
    }

    fn summarise<'a>(&'a self, request: TurnSummaryRequest<'a>) -> SummaryFuture<'a> {
        Box::pin(async move {
            // Immediately before the request: the opt-in may have been turned
            // off since this turn's event arrived.
            let settings = (self.load)();
            if settings.reading != ReadingConsent::On {
                return None;
            }
            let transport = (self.connect)(&settings.intent);
            Some(
                super::summary::summarise_over(&settings.intent, transport.as_ref(), request).await,
            )
        })
    }
}

/// The sentence for a permission prompt: "The coder is asking for permission:
/// run cargo publish." `wants` is cut to one line and [`MAX_WANTS_CHARS`].
pub fn permission_sentence(agent: &str, wants: &str) -> String {
    let name = spoken_name(agent);
    let wants = one_line(wants, MAX_WANTS_CHARS);
    let wants = wants.trim_end_matches(['.', ' ']);
    if wants.is_empty() {
        format!("{name} is asking for permission.")
    } else {
        format!("{name} is asking for permission: {wants}.")
    }
}

/// The sentence for a block: "The coder stopped with an error." or "The coder
/// hit a usage limit."
pub fn blocked_sentence(agent: &str, cause: BlockCause) -> String {
    let name = spoken_name(agent);
    match cause {
        BlockCause::Error => format!("{name} stopped with an error."),
        BlockCause::Quota => format!("{name} hit a usage limit and stopped."),
    }
}

/// One event, as the sentence to speak — or `None` when the summariser found
/// reading's opt-in off and sent nothing.
pub async fn announce(
    agent: &str,
    event: TurnEvent,
    summariser: &dyn TurnSummariser,
) -> Option<ReadingSentence> {
    let (kind, reply) = match event {
        TurnEvent::Permission { wants } => {
            return Some(ReadingSentence {
                kind: ReadingSentenceKind::Permission,
                text: permission_sentence(agent, &wants),
            });
        }
        TurnEvent::Blocked { cause } => {
            return Some(ReadingSentence {
                kind: ReadingSentenceKind::Blocked,
                text: blocked_sentence(agent, cause),
            });
        }
        TurnEvent::Finished { reply } => (TurnKind::Finished, reply),
        TurnEvent::Failed { reply } => (TurnKind::Failed, reply),
    };
    let summary = summariser
        .summarise(TurnSummaryRequest {
            agent,
            kind,
            reply: &reply,
        })
        .await?;
    Some(ReadingSentence {
        kind: ReadingSentenceKind::Turn,
        text: summary.text,
    })
}

/// Read `events` until they end or `sink` refuses a sentence.
///
/// One event at a time, in order: a summary is waited for before the next
/// event is announced, so what is spoken never arrives out of the order it
/// happened in. `sink` answers `false` when nobody is listening any more (the
/// webview's channel closed), which ends the loop and drops `events` — the
/// unsubscribe.
///
/// Before each event the summariser is asked whether reading's opt-in is
/// still on, and the summary request asks again; when it is off, the loop
/// sends [`ended_sentence`] and ends, so nothing more is announced or sent.
///
/// When `events` end on the source's side — the deck ended the agent's reply
/// stream because the agent exited (review RV-S2), or the deck went away — the
/// loop sends [`ended_sentence`] too: nothing more can be read, and the webview
/// ends reading mode and says so rather than staying on for an agent that is
/// gone.
pub async fn read_turns(
    agent: &str,
    mut events: TurnEvents,
    summariser: &dyn TurnSummariser,
    mut sink: impl FnMut(ReadingSentence) -> bool,
) {
    while let Some(event) = events.recv().await {
        let sentence = if summariser.consented() {
            announce(agent, event, summariser).await
        } else {
            None
        };
        let Some(sentence) = sentence else {
            sink(ended_sentence());
            return;
        };
        if !sink(sentence) {
            return;
        }
    }
    sink(ended_sentence());
}

/// `text` on one line with control characters dropped and whitespace
/// collapsed, at most `max` characters.
fn one_line(text: &str, max: usize) -> String {
    let collapsed = text
        .split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if collapsed.chars().count() <= max {
        return collapsed;
    }
    let cut: String = collapsed.chars().take(max).collect();
    match cut.rfind(' ') {
        Some(at) if at > 0 => cut[..at].to_string(),
        _ => cut,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A summariser that records what it was asked and answers with a fixed
    /// sentence built from the request.
    #[derive(Default)]
    struct Recording {
        asked: Mutex<Vec<(String, TurnKind, String)>>,
    }

    impl TurnSummariser for Recording {
        fn consented(&self) -> bool {
            true
        }

        fn summarise<'a>(&'a self, request: TurnSummaryRequest<'a>) -> SummaryFuture<'a> {
            self.asked.lock().unwrap().push((
                request.agent.to_string(),
                request.kind,
                request.reply.to_string(),
            ));
            let text = format!("summary of {}", request.reply);
            Box::pin(async move {
                Some(Summary {
                    text,
                    fallback: None,
                })
            })
        }
    }

    #[tokio::test]
    async fn voice_reading_a_finished_turn_is_summarised_from_its_reply() {
        let summariser = Recording::default();
        let sentence = announce(
            "tester",
            TurnEvent::Finished {
                reply: "All 42 tests pass.".to_string(),
            },
            &summariser,
        )
        .await
        .expect("announced");
        assert_eq!(sentence.kind, ReadingSentenceKind::Turn);
        assert_eq!(sentence.text, "summary of All 42 tests pass.");
        assert_eq!(
            *summariser.asked.lock().unwrap(),
            vec![(
                "tester".to_string(),
                TurnKind::Finished,
                "All 42 tests pass.".to_string()
            )]
        );
    }

    #[tokio::test]
    async fn voice_reading_a_failed_turn_is_summarised_as_failed() {
        let summariser = Recording::default();
        let sentence = announce(
            "coder",
            TurnEvent::Failed {
                reply: String::new(),
            },
            &summariser,
        )
        .await
        .expect("announced");
        assert_eq!(sentence.kind, ReadingSentenceKind::Turn);
        assert_eq!(summariser.asked.lock().unwrap()[0].1, TurnKind::Failed);
    }

    #[tokio::test]
    async fn voice_reading_permission_and_blocks_are_announced_without_a_model_call() {
        let summariser = Recording::default();
        let permission = announce(
            "coder",
            TurnEvent::Permission {
                wants: "run cargo publish".to_string(),
            },
            &summariser,
        )
        .await
        .expect("announced");
        assert_eq!(permission.kind, ReadingSentenceKind::Permission);
        assert_eq!(
            permission.text,
            "The coder is asking for permission: run cargo publish."
        );
        let quota = announce(
            "coder",
            TurnEvent::Blocked {
                cause: BlockCause::Quota,
            },
            &summariser,
        )
        .await
        .expect("announced");
        assert_eq!(quota.kind, ReadingSentenceKind::Blocked);
        assert_eq!(quota.text, "The coder hit a usage limit and stopped.");
        let error = announce(
            "the reviewer",
            TurnEvent::Blocked {
                cause: BlockCause::Error,
            },
            &summariser,
        )
        .await
        .expect("announced");
        assert_eq!(error.text, "The reviewer stopped with an error.");
        assert!(summariser.asked.lock().unwrap().is_empty());
    }

    #[test]
    fn voice_reading_permission_sentence_is_one_bounded_line() {
        assert_eq!(
            permission_sentence("coder", ""),
            "The coder is asking for permission."
        );
        assert_eq!(
            permission_sentence("coder", "Bash:\n  cargo   publish.\n"),
            "The coder is asking for permission: Bash: cargo publish."
        );
        let long = "word ".repeat(100);
        let sentence = permission_sentence("coder", &long);
        let wants = sentence
            .strip_prefix("The coder is asking for permission: ")
            .unwrap();
        assert!(wants.chars().count() <= MAX_WANTS_CHARS + 1, "{sentence}");
        assert!(!sentence.contains('\n'));
    }

    fn failed(text: &str) -> Incoming {
        Incoming::Reply(FinalReply {
            turn_id: None,
            text: text.to_string(),
            failed: true,
        })
    }

    fn blocked(cause: BlockCause) -> Incoming {
        Incoming::Status(TurnEvent::Blocked { cause })
    }

    /// Feed `script` into [`coalesce`], waiting `gap` of (paused) time after
    /// each step, and collect what it announces once the input closes.
    async fn coalesced(script: Vec<(Incoming, Duration)>) -> Vec<TurnEvent> {
        let (tx, incoming) = mpsc::channel(16);
        let (out, mut events) = mpsc::channel(16);
        let merging = tokio::spawn(coalesce(incoming, out));
        for (message, gap) in script {
            tx.send(message).await.unwrap();
            tokio::time::sleep(gap).await;
        }
        drop(tx);
        merging.await.unwrap();
        let mut heard = Vec::new();
        while let Some(event) = events.recv().await {
            heard.push(event);
        }
        heard
    }

    const SHORT: Duration = Duration::from_millis(100);
    const LONG: Duration = Duration::from_secs(10);

    /// Review RV-B2: a failed turn's two halves, in all four orders, are one
    /// announcement — and the usage limit is never lost to the reply that the
    /// daemon publishes first.
    #[tokio::test(start_paused = true)]
    async fn voice_reading_a_failed_turn_is_announced_once_in_every_order() {
        let failed_turn = vec![TurnEvent::Failed {
            reply: "I could not finish.".to_string(),
        }];
        let quota = vec![TurnEvent::Blocked {
            cause: BlockCause::Quota,
        }];
        let error = vec![TurnEvent::Blocked {
            cause: BlockCause::Error,
        }];
        // The failed reply first (the daemon's order), then an error status:
        // the error is absorbed into the failed turn's summary.
        assert_eq!(
            coalesced(vec![
                (failed("I could not finish."), SHORT),
                (blocked(BlockCause::Error), SHORT),
            ])
            .await,
            failed_turn
        );
        // The failed reply first, then a usage limit: the usage limit replaces
        // it (a Claude quota block arrives this way).
        assert_eq!(
            coalesced(vec![
                (failed("rate limited"), SHORT),
                (blocked(BlockCause::Quota), SHORT),
            ])
            .await,
            quota
        );
        // An error status first: announced as it came, the reply absorbed.
        assert_eq!(
            coalesced(vec![
                (blocked(BlockCause::Error), SHORT),
                (failed("I could not finish."), SHORT),
            ])
            .await,
            error
        );
        // A usage limit first: announced as it came, the reply absorbed.
        assert_eq!(
            coalesced(vec![
                (blocked(BlockCause::Quota), SHORT),
                (failed("rate limited"), SHORT),
            ])
            .await,
            quota
        );
        // An error status after the hold ran out is still this turn's.
        assert_eq!(
            coalesced(vec![
                (failed("I could not finish."), FAILED_REPLY_HOLD + SHORT),
                (blocked(BlockCause::Error), SHORT),
            ])
            .await,
            failed_turn
        );
    }

    /// Review RV-S1: a block status is announced the moment it arrives, with
    /// no hold, while a failed reply waits only [`FAILED_REPLY_HOLD`]. A later,
    /// unrelated turn is announced as usual, and two failed turns far apart are
    /// two announcements.
    #[tokio::test(start_paused = true)]
    async fn voice_reading_a_lone_status_is_announced_without_delay() {
        use tokio::time::Instant;
        let (tx, incoming) = mpsc::channel(16);
        let (out, mut events) = mpsc::channel(16);
        let merging = tokio::spawn(coalesce(incoming, out));
        for cause in [BlockCause::Error, BlockCause::Quota] {
            let start = Instant::now();
            tx.send(blocked(cause)).await.unwrap();
            assert_eq!(events.recv().await, Some(TurnEvent::Blocked { cause }));
            assert_eq!(start.elapsed(), Duration::ZERO, "{cause:?} was held");
            tokio::time::sleep(LONG).await;
        }

        let start = Instant::now();
        tx.send(failed("first")).await.unwrap();
        assert_eq!(
            events.recv().await,
            Some(TurnEvent::Failed {
                reply: "first".into()
            })
        );
        assert_eq!(start.elapsed(), FAILED_REPLY_HOLD);
        tokio::time::sleep(LONG).await;
        tx.send(failed("second")).await.unwrap();
        assert_eq!(
            events.recv().await,
            Some(TurnEvent::Failed {
                reply: "second".into()
            })
        );
        tokio::time::sleep(LONG).await;
        tx.send(Incoming::Reply(FinalReply {
            turn_id: None,
            text: "done".to_string(),
            failed: false,
        }))
        .await
        .unwrap();
        assert_eq!(
            events.recv().await,
            Some(TurnEvent::Finished {
                reply: "done".to_string()
            })
        );
        // Dropping the session's receiver ends the merge without more input.
        drop(events);
        merging.await.unwrap();
    }

    fn status(agent: &str, event_type: EventType) -> AgentEvent {
        AgentEvent {
            session_id: "s".to_string(),
            agent_type: dot_agent_deck::event::AgentType::ClaudeCode,
            event_type,
            tool_name: None,
            tool_detail: None,
            cwd: None,
            timestamp: chrono::Utc::now(),
            user_prompt: None,
            metadata: Default::default(),
            pane_id: Some("p".to_string()),
            agent_id: Some(agent.to_string()),
            agent_version: None,
            schema_version: None,
            live_target: None,
        }
    }

    #[test]
    fn voice_reading_status_events_are_filtered_to_the_agent_and_to_what_stops_it() {
        let mut permission = status("a", EventType::PermissionRequest);
        permission.tool_name = Some("Bash".to_string());
        permission.tool_detail = Some("cargo publish".to_string());
        assert_eq!(
            status_turn_event(&permission, "a"),
            Some(TurnEvent::Permission {
                wants: "Bash: cargo publish".to_string()
            })
        );
        assert_eq!(status_turn_event(&permission, "b"), None, "another agent's");
        let mut idle_prompt = status("a", EventType::WaitingForInput);
        idle_prompt.metadata.insert(
            dot_agent_deck::quota_block::NOTIFICATION_TYPE_METADATA_KEY.to_string(),
            "idle_prompt".to_string(),
        );
        assert_eq!(status_turn_event(&idle_prompt, "a"), None);
        assert_eq!(
            status_turn_event(&status("a", EventType::QuotaBlocked), "a"),
            Some(TurnEvent::Blocked {
                cause: BlockCause::Quota
            })
        );
        assert_eq!(
            status_turn_event(&status("a", EventType::Error), "a"),
            Some(TurnEvent::Blocked {
                cause: BlockCause::Error
            })
        );
        let mut tool_failure = status("a", EventType::Error);
        tool_failure.tool_name = Some("Bash".to_string());
        assert_eq!(
            status_turn_event(&tool_failure, "a"),
            None,
            "the agent carries on"
        );
        let mut notice = status("a", EventType::Error);
        notice.metadata.insert(
            dot_agent_deck::event::DELIVERY_NOTICE_METADATA_KEY.to_string(),
            "d-1".to_string(),
        );
        assert_eq!(status_turn_event(&notice, "a"), None);
        assert_eq!(status_turn_event(&status("a", EventType::Idle), "a"), None);
    }

    /// Scenario (rule 20): every agent type the deck runs — Pi included,
    /// through the deck's extension — reports its final reply, so none is
    /// refused as a gap.
    #[test]
    fn voice_reading_no_agent_type_is_a_named_gap() {
        for covered in ["claude_code", "codex", "open_code", "pi", "devin"] {
            assert_eq!(agent_gap(covered), None, "{covered}");
        }
    }

    /// A fake deck daemon on a Unix socket: answers `hello` with `capabilities`,
    /// confirms both subscriptions, and on each writes `frames` — the reply
    /// stream's and the status stream's — then holds the connection open, or,
    /// with `end_replies` set, ends the reply stream with that reason.
    #[cfg(unix)]
    async fn fake_deck(
        path: std::path::PathBuf,
        capabilities: Vec<String>,
        reply_frames: Vec<Vec<u8>>,
        status_frames: Vec<Vec<u8>>,
        end_replies: Option<&'static [u8]>,
    ) -> tokio::task::JoinHandle<()> {
        use dot_agent_deck::daemon_protocol::{
            AttachResponse, KIND_EVENT, KIND_STREAM_END, PROTOCOL_VERSION, read_frame, write_frame,
            write_resp,
        };
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let capabilities = capabilities.clone();
                let reply_frames = reply_frames.clone();
                let status_frames = status_frames.clone();
                tokio::spawn(async move {
                    let Ok(Some((_, bytes))) = read_frame(&mut stream).await else {
                        return;
                    };
                    let request: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                    let replies = request["op"] == "subscribe-turn-replies";
                    let frames = match request["op"].as_str() {
                        Some("hello") => {
                            let hello = AttachResponse {
                                capabilities: Some(capabilities),
                                ..AttachResponse::hello(PROTOCOL_VERSION)
                            };
                            let _ = write_resp(&mut stream, &hello).await;
                            return;
                        }
                        Some("subscribe-turn-replies") => reply_frames,
                        Some("subscribe-events") => status_frames,
                        _ => {
                            let _ = write_resp(&mut stream, &AttachResponse::err("unknown")).await;
                            return;
                        }
                    };
                    let _ = write_resp(&mut stream, &AttachResponse::ok()).await;
                    for frame in frames {
                        let _ = write_frame(&mut stream, KIND_EVENT, &frame).await;
                    }
                    if let Some(reason) = end_replies.filter(|_| replies) {
                        let _ = write_frame(&mut stream, KIND_STREAM_END, reason).await;
                        return;
                    }
                    std::future::pending::<()>().await;
                });
            }
        })
    }

    #[cfg(unix)]
    fn target() -> ReadingTarget {
        ReadingTarget {
            deck_id: "deck".to_string(),
            agent_id: "agent-7".to_string(),
            agent_type: Some("claude_code".to_string()),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn voice_reading_a_daemon_without_turn_replies_refuses_in_plain_words() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("attach.sock");
        let deck = fake_deck(
            path.clone(),
            vec!["focus-gained".to_string()],
            vec![],
            vec![],
            None,
        )
        .await;
        let source = DaemonTurnEvents::new(Arc::new(DaemonClient::new(path)));
        let refused = source.subscribe(&target()).await.unwrap_err();
        assert_eq!(
            unavailable_sentence(&refused),
            "Reading is not available: this deck's daemon is too old to report finished turns. \
             Update dot-agent-deck on that machine."
        );
        deck.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn voice_reading_the_daemon_source_delivers_the_agents_replies_and_statuses() {
        use dot_agent_deck::daemon_protocol::TurnReply;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("attach.sock");
        let reply = |agent: &str, sequence: u64, text: &str| {
            serde_json::to_vec(&TurnReply {
                agent_id: agent.to_string(),
                pane_id: "p".to_string(),
                sequence,
                reply: FinalReply {
                    turn_id: None,
                    text: text.to_string(),
                    failed: false,
                },
            })
            .unwrap()
        };
        let mut permission = status("agent-7", EventType::PermissionRequest);
        permission.tool_name = Some("Bash".to_string());
        let deck = fake_deck(
            path.clone(),
            vec![dot_agent_deck::daemon_protocol::CAP_TURN_REPLIES.to_string()],
            vec![
                reply("someone-else", 1, "not ours"),
                reply("agent-7", 2, "All tests pass."),
            ],
            vec![
                serde_json::to_vec(&BroadcastMsg::Event(status(
                    "someone-else",
                    EventType::QuotaBlocked,
                )))
                .unwrap(),
                serde_json::to_vec(&BroadcastMsg::Event(permission)).unwrap(),
            ],
            None,
        )
        .await;
        let source = DaemonTurnEvents::new(Arc::new(DaemonClient::new(path)));
        let mut events = source.subscribe(&target()).await.unwrap();
        let mut heard = Vec::new();
        for _ in 0..2 {
            heard.push(
                tokio::time::timeout(Duration::from_secs(10), events.recv())
                    .await
                    .expect("delivered promptly")
                    .expect("stream live"),
            );
        }
        heard.sort_by_key(|event| format!("{event:?}"));
        assert_eq!(
            heard,
            vec![
                TurnEvent::Finished {
                    reply: "All tests pass.".to_string()
                },
                TurnEvent::Permission {
                    wants: "Bash".to_string()
                },
            ]
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(300), events.recv())
                .await
                .is_err(),
            "nothing of another agent's"
        );
        deck.abort();
    }

    /// Review RV-S2: the deck ends the agent's reply stream when the agent
    /// exits. The source's events then end after what was already delivered,
    /// and reading ends with the "Reading off." sentence rather than staying on
    /// for an agent that is gone.
    #[cfg(unix)]
    #[tokio::test]
    async fn voice_reading_ends_when_the_deck_ends_the_agents_reply_stream() {
        use dot_agent_deck::daemon_protocol::{TURN_REPLIES_END_AGENT_EXITED, TurnReply};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("attach.sock");
        let last = serde_json::to_vec(&TurnReply {
            agent_id: "agent-7".to_string(),
            pane_id: "p".to_string(),
            sequence: 1,
            reply: FinalReply {
                turn_id: None,
                text: "last turn".to_string(),
                failed: false,
            },
        })
        .unwrap();
        let deck = fake_deck(
            path.clone(),
            vec![dot_agent_deck::daemon_protocol::CAP_TURN_REPLIES.to_string()],
            vec![last],
            vec![],
            Some(TURN_REPLIES_END_AGENT_EXITED),
        )
        .await;
        let source = DaemonTurnEvents::new(Arc::new(DaemonClient::new(path)));
        let events = source.subscribe(&target()).await.unwrap();
        let summariser = Recording::default();
        let mut heard = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(10),
            read_turns("tester", events, &summariser, |sentence| {
                heard.push(sentence);
                true
            }),
        )
        .await
        .expect("reading ends when the agent's stream does");
        assert_eq!(
            heard,
            vec![
                ReadingSentence {
                    kind: ReadingSentenceKind::Turn,
                    text: "summary of last turn".to_string(),
                },
                ended_sentence(),
            ]
        );
        deck.abort();
    }

    #[tokio::test]
    async fn voice_reading_reads_events_in_order_until_the_sink_refuses() {
        let summariser = Recording::default();
        let (send, events) = mpsc::channel(8);
        send.send(TurnEvent::Finished {
            reply: "one".to_string(),
        })
        .await
        .unwrap();
        send.send(TurnEvent::Permission {
            wants: "two".to_string(),
        })
        .await
        .unwrap();
        send.send(TurnEvent::Finished {
            reply: "three".to_string(),
        })
        .await
        .unwrap();
        drop(send);
        let mut heard = Vec::new();
        read_turns("tester", events, &summariser, |sentence| {
            heard.push(sentence.text);
            true
        })
        .await;
        assert_eq!(
            heard,
            vec![
                "summary of one".to_string(),
                "The tester is asking for permission: two.".to_string(),
                "summary of three".to_string(),
                READING_ENDED.to_string(),
            ],
            "events that end on the source's side end reading"
        );

        // A sink that stops listening ends the loop and drops the receiver,
        // which a source sees as a failed send.
        let (send, events) = mpsc::channel(8);
        send.send(TurnEvent::Finished {
            reply: "one".to_string(),
        })
        .await
        .unwrap();
        send.send(TurnEvent::Finished {
            reply: "two".to_string(),
        })
        .await
        .unwrap();
        let mut heard = 0;
        read_turns("tester", events, &summariser, |_| {
            heard += 1;
            false
        })
        .await;
        assert_eq!(heard, 1);
        assert!(send.is_closed());
    }

    /// A summary transport that records every body it is sent, as the
    /// Commands connection would receive it, and answers with a summary.
    #[derive(Clone, Default)]
    struct Posts(Arc<std::sync::Mutex<Vec<serde_json::Value>>>);

    impl SummaryTransport for Posts {
        fn post(&self, body: serde_json::Value) -> super::super::summary::PostFuture<'_> {
            self.0.lock().unwrap().push(body);
            Box::pin(async {
                Ok(serde_json::json!({ "choices": [{ "finish_reason": "stop",
                    "message": { "content": "The tester finished: done." } }] }))
            })
        }
    }

    /// A [`SettingsSummariser`] over an opt-in the test flips and `posts`.
    fn consent_gated(
        consent: Arc<std::sync::atomic::AtomicBool>,
        posts: Posts,
    ) -> SettingsSummariser {
        SettingsSummariser::new(
            Box::new(move || VoiceSettings {
                intent: IntentSettings::for_backend(
                    crate::settings::IntentBackend::OpenaiCompatible,
                ),
                reading: if consent.load(std::sync::atomic::Ordering::SeqCst) {
                    ReadingConsent::On
                } else {
                    ReadingConsent::Off
                },
                ..VoiceSettings::default()
            }),
            Box::new(move |_| Box::new(posts.clone())),
        )
    }

    /// Scenario (audit A-B1): reading starts with the opt-in on and a
    /// finished turn is summarised (one request); the opt-in is then turned
    /// off, and the next finished turn sends nothing — zero further requests —
    /// and the session ends with the "Reading off." sentence instead.
    #[tokio::test]
    async fn voice_reading_revoked_consent_sends_nothing_more_and_ends_reading() {
        let consent = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let posts = Posts::default();
        let summariser = consent_gated(Arc::clone(&consent), posts.clone());
        let (send, events) = mpsc::channel(8);
        let heard = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&heard);
        let reading = tokio::spawn(async move {
            read_turns("tester", events, &summariser, |sentence| {
                sink.lock().unwrap().push(sentence);
                true
            })
            .await;
        });
        send.send(TurnEvent::Finished {
            reply: "first reply".to_string(),
        })
        .await
        .unwrap();
        let until = tokio::time::Instant::now() + Duration::from_secs(5);
        while heard.lock().unwrap().is_empty() {
            assert!(
                tokio::time::Instant::now() < until,
                "the first turn was not read"
            );
            tokio::task::yield_now().await;
        }
        assert_eq!(posts.0.lock().unwrap().len(), 1);

        consent.store(false, std::sync::atomic::Ordering::SeqCst);
        send.send(TurnEvent::Finished {
            reply: "second reply, which must not leave".to_string(),
        })
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), reading)
            .await
            .expect("reading ends once consent is off")
            .unwrap();
        assert_eq!(
            posts.0.lock().unwrap().len(),
            1,
            "a request after consent was revoked"
        );
        let heard = heard.lock().unwrap();
        assert_eq!(heard.len(), 2, "{heard:?}");
        assert_eq!(heard[0].kind, ReadingSentenceKind::Turn);
        assert_eq!(heard[1], ended_sentence());
        assert!(send.is_closed(), "the subscription was not dropped");
    }

    /// Scenario (audit A-B1): consent is read again immediately before the
    /// summary request, so an opt-in turned off between the event and the
    /// request still sends nothing; permission prompts are not announced
    /// either once it is off.
    #[tokio::test]
    async fn voice_reading_summary_checks_consent_right_before_the_request() {
        let consent = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let posts = Posts::default();
        let summariser = consent_gated(Arc::clone(&consent), posts.clone());
        let summary = summariser
            .summarise(TurnSummaryRequest {
                agent: "tester",
                kind: TurnKind::Finished,
                reply: "a reply",
            })
            .await;
        assert_eq!(summary, None);
        assert!(posts.0.lock().unwrap().is_empty());

        let (send, events) = mpsc::channel(8);
        send.send(TurnEvent::Permission {
            wants: "run cargo publish".to_string(),
        })
        .await
        .unwrap();
        drop(send);
        let mut heard = Vec::new();
        read_turns("tester", events, &summariser, |sentence| {
            heard.push(sentence);
            true
        })
        .await;
        assert_eq!(heard, vec![ended_sentence()]);
    }
}
