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
//! # The seam M2 fills: [`TurnEventSource`]
//!
//! Where turn events come from is the daemon's to say (PRD #1497 D5: the final
//! reply arrives in the daemon, from Claude Code's hooks and Codex's session
//! log, and a remote deck's files are reachable only by its daemon). M5 is
//! built against the trait alone, and the shipped source is [`NoTurnEvents`],
//! which refuses every subscription with [`NO_TURN_EVENTS`] — so until M2
//! plugs in a daemon subscription, "reading on" says in plain words that
//! reading is not available rather than starting a mode that never speaks
//! (CLAUDE.md rule 20).
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

use serde::Serialize;
use tokio::sync::mpsc;

use super::summary::{Summary, TurnKind, TurnSummaryRequest, spoken_name};

/// Why "reading on" cannot start while no daemon supplies turn events (PRD
/// #1497 M2 replaces [`NoTurnEvents`]). A fragment, rendered into
/// [`unavailable_sentence`].
pub const NO_TURN_EVENTS: &str = "this daemon does not report when an agent finishes a turn yet";

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

/// The source shipped until M2: refuses every subscription with
/// [`NO_TURN_EVENTS`].
#[derive(Debug, Default, Clone, Copy)]
pub struct NoTurnEvents;

impl TurnEventSource for NoTurnEvents {
    fn subscribe(&self, _target: &ReadingTarget) -> SubscribeFuture<'_> {
        Box::pin(async { Err(NO_TURN_EVENTS.to_string()) })
    }
}

/// Why an agent of `agent_type` cannot be read, as a fragment, or `None` when
/// nothing is known against it (CLAUDE.md rule 20).
///
/// TODO(PRD #1497 M2/M6): fill in per agent once the daemon's turn-end data is
/// known for each — Claude Code (hooks), Codex (session log), OpenCode, and Pi
/// and Devin — and name every gap in `docs/desktop/voice.md`. Empty today, so
/// the source's own answer is the only refusal.
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

    #[tokio::test]
    async fn voice_reading_no_turn_events_refuses_in_plain_words() {
        let target = ReadingTarget {
            deck_id: "deck".to_string(),
            agent_id: "agent".to_string(),
            agent_type: Some("claude_code".to_string()),
        };
        let refused = NoTurnEvents.subscribe(&target).await.unwrap_err();
        assert_eq!(
            unavailable_sentence(&refused),
            "Reading is not available: this daemon does not report when an agent finishes a turn yet."
        );
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
