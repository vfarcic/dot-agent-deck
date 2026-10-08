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

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use dot_agent_deck::daemon_client::{DaemonClient, GatedQuery};
use dot_agent_deck::daemon_protocol::FinalReply;
use dot_agent_deck::event::{AgentEvent, BroadcastMsg, EventType};
use serde::Serialize;
use tokio::sync::mpsc;

use super::summary::{Summary, TurnKind, TurnSummaryRequest, spoken_name};

/// Why "reading on" cannot start on a deck whose daemon predates the reply
/// stream (it does not advertise `turn-replies`). A fragment, rendered into
/// [`unavailable_sentence`].
pub const DAEMON_TOO_OLD: &str =
    "this deck's daemon is too old to report finished turns. Update dot-agent-deck on that machine";

/// Why "reading on" cannot start when the deck's daemon did not answer the
/// subscription. A fragment, rendered into [`unavailable_sentence`].
pub const DAEMON_UNREACHABLE: &str = "the deck did not answer";

/// How long [`coalesce`] holds back one half of a failed turn waiting for the
/// other, so the turn is announced once. Both halves leave the daemon within
/// one hook (a Codex session-log failure within one of its polls), so this only
/// has to cover two connections' delivery.
pub const FAILURE_COALESCE_WINDOW: Duration = Duration::from_secs(3);

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
/// reply stream ends.
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
/// A failed reply and an error status within [`FAILURE_COALESCE_WINDOW`] of
/// each other are one failed turn, announced as the reply's summary — it
/// carries the agent's own words. A failed reply and a usage-limit status are
/// announced as the usage limit, which says what the user has to do. Whichever
/// arrives first is held for the window (a status) or remembered for it (a
/// reply already announced), so the second is dropped. Everything else passes
/// through in arrival order, a held status first.
pub async fn coalesce(mut incoming: mpsc::Receiver<Incoming>, out: mpsc::Sender<TurnEvent>) {
    use tokio::time::{Instant, sleep_until};
    // A status held back for its window.
    let mut held: Option<(BlockCause, Instant)> = None;
    // The last failed turn already announced, and until when it absorbs the
    // other half.
    let mut announced: Option<Instant> = None;
    loop {
        let next = match held {
            Some((_, until)) => tokio::select! {
                message = incoming.recv() => Some(message),
                _ = sleep_until(until) => None,
                _ = out.closed() => return,
            },
            None => tokio::select! {
                message = incoming.recv() => Some(message),
                _ = out.closed() => return,
            },
        };
        let now = Instant::now();
        let absorbing = announced.is_some_and(|until| now < until);
        let mut emit = Vec::new();
        match next {
            // The window ran out with no reply: announce the status.
            None => {
                if let Some((cause, _)) = held.take() {
                    emit.push(TurnEvent::Blocked { cause });
                    announced = Some(now + FAILURE_COALESCE_WINDOW);
                }
            }
            Some(None) => {
                if let Some((cause, _)) = held.take() {
                    let _ = out.send(TurnEvent::Blocked { cause }).await;
                }
                return;
            }
            Some(Some(Incoming::Reply(reply))) if reply.failed => match held.take() {
                Some((BlockCause::Quota, _)) => {
                    emit.push(TurnEvent::Blocked {
                        cause: BlockCause::Quota,
                    });
                    announced = Some(now + FAILURE_COALESCE_WINDOW);
                }
                Some((BlockCause::Error, _)) | None if !absorbing => {
                    emit.push(TurnEvent::Failed { reply: reply.text });
                    announced = Some(now + FAILURE_COALESCE_WINDOW);
                }
                _ => {}
            },
            Some(Some(Incoming::Reply(reply))) => {
                if let Some((cause, _)) = held.take() {
                    emit.push(TurnEvent::Blocked { cause });
                }
                emit.push(TurnEvent::Finished { reply: reply.text });
            }
            Some(Some(Incoming::Status(TurnEvent::Blocked { cause }))) => {
                if !absorbing
                    && let Some((earlier, _)) = held.replace((cause, now + FAILURE_COALESCE_WINDOW))
                {
                    emit.push(TurnEvent::Blocked { cause: earlier });
                }
            }
            Some(Some(Incoming::Status(status))) => {
                if let Some((cause, _)) = held.take() {
                    emit.push(TurnEvent::Blocked { cause });
                    announced = Some(now + FAILURE_COALESCE_WINDOW);
                }
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

/// Why an agent of `agent_type` cannot be read, as a fragment, or `None` when
/// its final reply reaches the daemon (CLAUDE.md rule 20).
///
/// Claude Code and Codex report it in their `Stop` hooks (and Codex a failed
/// turn's in its session log), and OpenCode through the deck's plugin. Devin's
/// hooks go through the same Claude-compatible path, and its hook input names
/// the same `last_assistant_message` field, so it is not refused here. Pi is
/// the gap: the deck's Pi extension does not pass the agent's reply on.
pub fn agent_gap(agent_type: &str) -> Option<String> {
    match agent_type {
        "pi" => Some(
            "the deck does not receive Pi's replies yet, so it cannot read Pi's turns".to_string(),
        ),
        _ => None,
    }
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
}

/// One sentence to speak, as the webview receives it — and the only thing
/// about a turn that reaches it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadingSentence {
    pub kind: ReadingSentenceKind,
    pub text: String,
}

/// The future a [`TurnSummariser`] returns.
pub type SummaryFuture<'a> = Pin<Box<dyn Future<Output = Summary> + Send + 'a>>;

/// Summarises one turn. The real one is [`super::summary::summarise_turn`]
/// over the Commands connection, read per turn; tests pass their own.
pub trait TurnSummariser: Send + Sync {
    fn summarise<'a>(&'a self, request: TurnSummaryRequest<'a>) -> SummaryFuture<'a>;
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

/// One event, as the sentence to speak.
pub async fn announce(
    agent: &str,
    event: TurnEvent,
    summariser: &dyn TurnSummariser,
) -> ReadingSentence {
    let (kind, reply) = match event {
        TurnEvent::Permission { wants } => {
            return ReadingSentence {
                kind: ReadingSentenceKind::Permission,
                text: permission_sentence(agent, &wants),
            };
        }
        TurnEvent::Blocked { cause } => {
            return ReadingSentence {
                kind: ReadingSentenceKind::Blocked,
                text: blocked_sentence(agent, cause),
            };
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
        .await;
    ReadingSentence {
        kind: ReadingSentenceKind::Turn,
        text: summary.text,
    }
}

/// Read `events` until they end or `sink` refuses a sentence.
///
/// One event at a time, in order: a summary is waited for before the next
/// event is announced, so what is spoken never arrives out of the order it
/// happened in. `sink` answers `false` when nobody is listening any more (the
/// webview's channel closed), which ends the loop and drops `events` — the
/// unsubscribe.
pub async fn read_turns(
    agent: &str,
    mut events: TurnEvents,
    summariser: &dyn TurnSummariser,
    mut sink: impl FnMut(ReadingSentence) -> bool,
) {
    while let Some(event) = events.recv().await {
        let sentence = announce(agent, event, summariser).await;
        if !sink(sentence) {
            return;
        }
    }
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
        fn summarise<'a>(&'a self, request: TurnSummaryRequest<'a>) -> SummaryFuture<'a> {
            self.asked.lock().unwrap().push((
                request.agent.to_string(),
                request.kind,
                request.reply.to_string(),
            ));
            let text = format!("summary of {}", request.reply);
            Box::pin(async move {
                Summary {
                    text,
                    fallback: None,
                }
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
        .await;
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
        .await;
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
        .await;
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
        .await;
        assert_eq!(quota.kind, ReadingSentenceKind::Blocked);
        assert_eq!(quota.text, "The coder hit a usage limit and stopped.");
        let error = announce(
            "the reviewer",
            TurnEvent::Blocked {
                cause: BlockCause::Error,
            },
            &summariser,
        )
        .await;
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

    #[tokio::test(start_paused = true)]
    async fn voice_reading_a_failed_turn_is_announced_once_whichever_half_comes_first() {
        let failed_turn = vec![TurnEvent::Failed {
            reply: "I could not finish.".to_string(),
        }];
        // The error status first, then the failed reply within the window.
        assert_eq!(
            coalesced(vec![
                (blocked(BlockCause::Error), SHORT),
                (failed("I could not finish."), SHORT),
            ])
            .await,
            failed_turn
        );
        // The failed reply first, then the error status.
        assert_eq!(
            coalesced(vec![
                (failed("I could not finish."), SHORT),
                (blocked(BlockCause::Error), SHORT),
            ])
            .await,
            failed_turn
        );
        // A usage limit wins over the failed reply, in either order.
        let quota = vec![TurnEvent::Blocked {
            cause: BlockCause::Quota,
        }];
        assert_eq!(
            coalesced(vec![
                (blocked(BlockCause::Quota), SHORT),
                (failed("rate limited"), SHORT),
            ])
            .await,
            quota
        );
        // The usage limit announced when its window ran out, and the failed
        // reply arriving after that but within the announcement's window.
        assert_eq!(
            coalesced(vec![
                (
                    blocked(BlockCause::Quota),
                    FAILURE_COALESCE_WINDOW + Duration::from_secs(1)
                ),
                (failed("rate limited"), SHORT),
            ])
            .await,
            quota
        );
    }

    #[tokio::test(start_paused = true)]
    async fn voice_reading_an_error_with_no_reply_is_announced_after_the_window() {
        // No reply comes: the error is announced once the window runs out, and
        // a later, unrelated finished turn still is.
        let (tx, incoming) = mpsc::channel(16);
        let (out, mut events) = mpsc::channel(16);
        let merging = tokio::spawn(coalesce(incoming, out));
        tx.send(blocked(BlockCause::Error)).await.unwrap();
        tokio::time::sleep(SHORT).await;
        assert!(events.try_recv().is_err(), "held for the window");
        tokio::time::sleep(FAILURE_COALESCE_WINDOW).await;
        assert_eq!(
            events.recv().await,
            Some(TurnEvent::Blocked {
                cause: BlockCause::Error
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
        // Two failed turns far apart are two announcements.
        tx.send(failed("first")).await.unwrap();
        tokio::time::sleep(LONG).await;
        tx.send(failed("second")).await.unwrap();
        assert_eq!(
            events.recv().await.unwrap(),
            TurnEvent::Failed {
                reply: "first".into()
            }
        );
        assert_eq!(
            events.recv().await.unwrap(),
            TurnEvent::Failed {
                reply: "second".into()
            }
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

    #[test]
    fn voice_reading_only_pi_is_a_named_gap() {
        assert!(agent_gap("pi").is_some());
        for covered in ["claude_code", "codex", "open_code", "devin"] {
            assert_eq!(agent_gap(covered), None, "{covered}");
        }
    }

    /// A fake deck daemon on a Unix socket: answers `hello` with `capabilities`,
    /// confirms both subscriptions, and on each writes `frames` — the reply
    /// stream's and the status stream's — then holds the connection open.
    #[cfg(unix)]
    async fn fake_deck(
        path: std::path::PathBuf,
        capabilities: Vec<String>,
        reply_frames: Vec<Vec<u8>>,
        status_frames: Vec<Vec<u8>>,
    ) -> tokio::task::JoinHandle<()> {
        use dot_agent_deck::daemon_protocol::{
            AttachResponse, KIND_EVENT, PROTOCOL_VERSION, read_frame, write_frame, write_resp,
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
            ]
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
}
