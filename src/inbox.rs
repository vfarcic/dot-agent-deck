//! PRD #1666: the daemon's per-agent inbox, as pure in-memory data.
//!
//! Every message the deck hands an agent — a delegated task, a worker's report,
//! a deck notice, a seed — is posted here and waits until the agent-side
//! component (or the agent, answering the doorbell) fetches and acknowledges it.
//! Nothing in this module does I/O: the daemon owns the sockets, the clock and
//! the doorbell, and calls in with the current [`Instant`] on every operation,
//! so the semantics are deterministic under test.
//!
//! The semantics are JetStream-shaped (decision 1 of the PRD): globally unique
//! ids that double as dedup keys, explicit acknowledgement, redelivery after an
//! ack-wait, a `max_deliver` bound, and offers that go to one taker at a time
//! under a time-limited claim.
//!
//! **States.** A message is `queued` until a consumer takes it (`fetched`), and
//! `acknowledged` once an ack arrives. A fetch that is not acknowledged within
//! the ack-wait returns it to `queued`; after `max_deliver` hand-overs it becomes
//! `undeliverable`, as it does when its bound agent exits. An offer's ack is a
//! claim (`claimed`), which ends in `acknowledged` on completion or returns the
//! offer to its pool, excluding everyone who held it before. A message in
//! `fetched` or `claimed` is never handed to a second consumer.
//!
//! **Bounds.** Every limit in [`InboxLimits`] refuses explicitly ([`PostError`]):
//! nothing is ever dropped to make room.
//!
//! **Copies of a publish share an id.** A publish fans out to one entry per
//! subscriber, all carrying the publish's id, each with its own state, as a
//! JetStream message is acknowledged per consumer. An entry is therefore
//! identified by its id plus its recipient pane.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub use crate::deck_identity::{DeckId, is_valid_deck_id};

/// Ack-wait: how long a fetched message waits for its ack before it is
/// redelivered.
pub const DEFAULT_ACK_WAIT: Duration = Duration::from_secs(60);
/// Hand-overs after which an unacknowledged message is undeliverable.
pub const DEFAULT_MAX_DELIVER: u32 = 5;
/// PRD #1666 long-turn delivery: how long after a message's first fetch a
/// streaming consumer's `hold` still refunds a redelivery ([`Inbox::hold`]).
/// Past it, `max_deliver` applies as it does to every other consumer.
/// Documented in `docs/develop/agent-messaging.md` ("Long turns: hold and the
/// late ack").
pub const DEFAULT_HOLD_BOUND: Duration = Duration::from_secs(60 * 60);
/// Queued plus fetched messages one recipient pane may hold.
pub const DEFAULT_MAX_PER_RECIPIENT: usize = 256;
/// Live (not yet acknowledged or undeliverable) messages in the whole daemon.
pub const DEFAULT_MAX_MESSAGES: usize = 4096;
/// Largest body, in bytes: the task-file bound.
pub const DEFAULT_MAX_BODY_BYTES: usize = crate::bounded_read::MAX_TASK_BYTES as usize;
/// Acknowledged or undeliverable messages kept per pane for listing.
pub const DEFAULT_HISTORY_PER_PANE: usize = 64;
/// Hardening M3: see [`InboxLimits::max_history_messages`].
pub const DEFAULT_MAX_HISTORY_MESSAGES: usize = 1024;
/// Hardening M3: see [`InboxLimits::max_history_bytes`].
pub const DEFAULT_MAX_HISTORY_BYTES: usize = 16 * 1024 * 1024;
/// Hardening M3: see [`InboxLimits::history_body_ttl`].
pub const DEFAULT_HISTORY_BODY_TTL: Duration = Duration::from_secs(60 * 60);
/// How long a message id is remembered for dedup.
pub const DEFAULT_DEDUP_WINDOW: Duration = Duration::from_secs(10 * 60);
/// How many message ids are remembered for dedup.
pub const DEFAULT_DEDUP_MAX_IDS: usize = 4096;
/// Open offers one scope may hold.
pub const DEFAULT_MAX_OFFERS_PER_SCOPE: usize = 256;
/// Publishes plus offers one sender agent may make per minute.
pub const DEFAULT_RATE_PER_MINUTE: u32 = 60;
/// Claim length when an offer names none.
pub const DEFAULT_CLAIM_TTL: Duration = Duration::from_secs(30 * 60);
/// Shortest claim an offer may ask for.
pub const DEFAULT_MIN_CLAIM_TTL: Duration = Duration::from_secs(60);
/// Longest claim an offer may ask for.
pub const MAX_CLAIM_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// The rate-limit window.
const RATE_WINDOW: Duration = Duration::from_secs(60);

/// Environment overrides, one per limit, read by [`InboxLimits::from_env`].
pub const ENV_ACK_WAIT_MS: &str = "DOT_AGENT_DECK_INBOX_ACK_WAIT_MS";
pub const ENV_MAX_DELIVER: &str = "DOT_AGENT_DECK_INBOX_MAX_DELIVER";
pub const ENV_MAX_PER_RECIPIENT: &str = "DOT_AGENT_DECK_INBOX_MAX_PER_RECIPIENT";
pub const ENV_MAX_MESSAGES: &str = "DOT_AGENT_DECK_INBOX_MAX_MESSAGES";
pub const ENV_MAX_BODY_BYTES: &str = "DOT_AGENT_DECK_INBOX_MAX_BODY_BYTES";
pub const ENV_HISTORY_PER_PANE: &str = "DOT_AGENT_DECK_INBOX_HISTORY_PER_PANE";
pub const ENV_DEDUP_WINDOW_MS: &str = "DOT_AGENT_DECK_INBOX_DEDUP_WINDOW_MS";
pub const ENV_DEDUP_MAX_IDS: &str = "DOT_AGENT_DECK_INBOX_DEDUP_MAX_IDS";
pub const ENV_MAX_OFFERS_PER_SCOPE: &str = "DOT_AGENT_DECK_INBOX_MAX_OFFERS_PER_SCOPE";
pub const ENV_RATE_PER_MINUTE: &str = "DOT_AGENT_DECK_INBOX_RATE_PER_MINUTE";
pub const ENV_MIN_CLAIM_TTL_MS: &str = "DOT_AGENT_DECK_INBOX_MIN_CLAIM_TTL_MS";

// ---------------------------------------------------------------------------
// Ids and addresses
// ---------------------------------------------------------------------------

/// `m-<deck>-<16 hex>`: globally unique across decks and restarts, and the
/// dedup key. Serialises as its bare string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct MessageId(String);

impl MessageId {
    /// A fresh id on `deck`, with 64 random bits from the operating system.
    pub fn generate(deck: &DeckId) -> Self {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes)
            .expect("OS randomness is unavailable; cannot mint a message id");
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        Self(format!("m-{deck}-{hex}"))
    }

    /// `Some` when `text` is exactly a message id.
    pub fn parse(text: &str) -> Option<Self> {
        is_valid_message_id(text).then(|| Self(text.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The deck the id was minted on.
    pub fn deck(&self) -> &str {
        &self.0[2..2 + crate::deck_identity::DECK_ID_LEN]
    }
}

impl std::fmt::Display for MessageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for MessageId {
    type Error = String;
    fn try_from(text: String) -> Result<Self, Self::Error> {
        if is_valid_message_id(&text) {
            Ok(Self(text))
        } else {
            Err(format!("not a message id: {text:?}"))
        }
    }
}

impl From<MessageId> for String {
    fn from(id: MessageId) -> Self {
        id.0
    }
}

/// The longest topic or pool name.
pub const MAX_LABEL_LEN: usize = 64;
/// The most topics or pools one `subscribe` or `take_offers` list may name.
pub const MAX_SUBSCRIPTIONS: usize = 64;

/// A topic or offer-pool name: `[a-z0-9][a-z0-9-]{0,63}`. Inert wherever it is
/// carried — in an address, a subject, a log line — so the wire and the config
/// refuse anything else rather than escaping it.
pub fn is_valid_label(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_LABEL_LEN
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// `m-[a-z2-7]{8}-[0-9a-f]{16}`. Replaces `delegate_retry::is_valid_delivery_id`;
/// an old `d-…` delivery id is not a message id.
pub fn is_valid_message_id(text: &str) -> bool {
    let Some(rest) = text.strip_prefix("m-") else {
        return false;
    };
    let Some((deck, hex)) = rest.split_once('-') else {
        return false;
    };
    is_valid_deck_id(deck)
        && hex.len() == 16
        && hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Encode one address token into `[A-Za-z0-9-]` plus `_` escapes: every other
/// byte, `_` included, becomes `_` and two lowercase hex digits. Reversible by
/// [`decode_token`], and never empty (an empty token is `_`).
pub fn encode_token(raw: &str) -> String {
    if raw.is_empty() {
        return "_".to_string();
    }
    let mut out = String::with_capacity(raw.len());
    for b in raw.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' {
            out.push(b as char);
        } else {
            out.push_str(&format!("_{b:02x}"));
        }
    }
    out
}

/// The inverse of [`encode_token`]; `None` for text it never produces.
pub fn decode_token(token: &str) -> Option<String> {
    if token == "_" {
        return Some(String::new());
    }
    let bytes = token.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'_' => {
                let hex = token.get(i + 1..i + 3)?;
                if !hex
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                {
                    return None;
                }
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b if b.is_ascii_alphanumeric() || b == b'-' => {
                out.push(b);
                i += 1;
            }
            _ => return None,
        }
    }
    String::from_utf8(out).ok()
}

/// The group a publish or offer is delivered within (§2.2).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum Scope {
    /// One orchestration instance, by its per-tab token (never its config
    /// name, which two instances can share — #463).
    Orchestration { token: String },
    /// A dispatch tree, by the pane at its root.
    Dispatch { root_pane: String },
}

impl Scope {
    /// The scope's address token, before encoding: the orchestration token, or
    /// `d-<root pane>`.
    pub fn token(&self) -> String {
        match self {
            Self::Orchestration { token } => token.clone(),
            Self::Dispatch { root_pane } => format!("d-{root_pane}"),
        }
    }
}

/// NATS-subject-shaped addresses; [`std::fmt::Display`] renders them.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Address {
    /// `inbox.<deck>.<pane>`
    Direct { deck: DeckId, pane_id: String },
    /// `topic.<deck>.<scope>.<topic>`
    Topic {
        deck: DeckId,
        scope: Scope,
        topic: String,
    },
    /// `offer.<deck>.<scope>.<pool>`
    Offer {
        deck: DeckId,
        scope: Scope,
        pool: String,
    },
}

impl std::fmt::Display for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Direct { deck, pane_id } => write!(f, "inbox.{deck}.{}", encode_token(pane_id)),
            Self::Topic { deck, scope, topic } => write!(
                f,
                "topic.{deck}.{}.{}",
                encode_token(&scope.token()),
                encode_token(topic)
            ),
            Self::Offer { deck, scope, pool } => write!(
                f,
                "offer.{deck}.{}.{}",
                encode_token(&scope.token()),
                encode_token(pool)
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// The message record
// ---------------------------------------------------------------------------

/// The pane a message is for and, when bound, the one agent generation in it
/// that may receive it. Pane ids are recycled and agent ids change on respawn,
/// so producers bind whenever they know the agent.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Recipient {
    pub pane_id: String,
    pub agent_id: Option<String>,
}

impl Recipient {
    pub fn bound(pane_id: impl Into<String>, agent_id: impl Into<String>) -> Self {
        Self {
            pane_id: pane_id.into(),
            agent_id: Some(agent_id.into()),
        }
    }

    pub fn unbound(pane_id: impl Into<String>) -> Self {
        Self {
            pane_id: pane_id.into(),
            agent_id: None,
        }
    }

    /// Whether the agent `agent_id` in pane `pane_id` is this recipient.
    pub fn admits(&self, pane_id: &str, agent_id: &str) -> bool {
        self.pane_id == pane_id && self.agent_id.as_deref().is_none_or(|a| a == agent_id)
    }

    /// The same pane and agent generation, treating an unbound side as a
    /// wildcard for the agent.
    fn same_holder(&self, other: &Recipient) -> bool {
        self.pane_id == other.pane_id
            && match (&self.agent_id, &other.agent_id) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            }
    }
}

/// Who a message is from. The daemon fills it from the attested caller, never
/// from anything the caller claims.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum Sender {
    /// The deck itself: a notice, a seed, a scheduled task, a dispatch report.
    Deck { reason: String },
    /// An agent: the orchestrator delegating, a worker reporting or publishing.
    Agent {
        pane_id: String,
        agent_id: String,
        role: Option<String>,
        orchestration: Option<String>,
    },
}

impl Sender {
    /// The sending agent, for the per-sender rate limit.
    pub fn agent_id(&self) -> Option<&str> {
        match self {
            Self::Agent { agent_id, .. } => Some(agent_id),
            Self::Deck { .. } => None,
        }
    }
}

/// Whether a body is rendered as written or fenced as another agent's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BodyTrust {
    Trusted,
    Untrusted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageKind {
    Direct,
    Publish { topic: String },
    Offer { pool: String, claim_ttl: Duration },
}

impl MessageKind {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Publish { .. } => "publish",
            Self::Offer { .. } => "offer",
        }
    }
}

/// What took a message out of the queue.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Consumer {
    /// A single-use `inbox wait` connection (Claude's waiter).
    Waiter { conn_id: u64 },
    /// A long-lived streaming connection (OpenCode plugin, Pi extension).
    Stream { conn_id: u64 },
    /// A turn-end hook handing the content over (Codex `Stop`).
    TurnEndHook,
    /// A hook injecting the content after the doorbell (Claude
    /// `UserPromptSubmit`).
    DoorbellHook,
    /// `dot-agent-deck inbox fetch`.
    FetchCli,
}

/// What counted as the acknowledgement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AckHow {
    /// The message's opener was seen in a submitted prompt.
    PromptObserved,
    PluginConfirmed,
    ExtensionConfirmed,
    /// A hook handed the content over in its output.
    HookHandover,
    /// The CLI printed it.
    FetchedByCli,
    /// An explicit `ack <id>`.
    Explicit,
    /// An offer's claimant completed it.
    OfferCompleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UndeliverableReason {
    /// The bound agent exited while the message was still owed to it.
    AgentExited,
    /// `max_deliver` hand-overs, none acknowledged.
    MaxDeliver,
    /// A newer message replaced it before it was acknowledged.
    Superseded,
    /// The inbox is shutting down.
    InboxShut,
}

/// The state machine. See the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageState {
    Queued,
    Fetched {
        by: Consumer,
        at: Instant,
        ack_deadline: Instant,
    },
    /// An offer whose recipient took it on.
    Claimed {
        by: Recipient,
        at: Instant,
        expires: Instant,
    },
    Acknowledged {
        at: Instant,
        how: AckHow,
    },
    Undeliverable {
        at: Instant,
        reason: UndeliverableReason,
    },
}

impl MessageState {
    pub fn name(&self) -> StateName {
        match self {
            Self::Queued => StateName::Queued,
            Self::Fetched { .. } => StateName::Fetched,
            Self::Claimed { .. } => StateName::Claimed,
            Self::Acknowledged { .. } => StateName::Acknowledged,
            Self::Undeliverable { .. } => StateName::Undeliverable,
        }
    }

    /// Acknowledged or undeliverable: nothing more will happen to it.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Acknowledged { .. } | Self::Undeliverable { .. })
    }
}

/// A state's public spelling: lower case on every wire and in every listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateName {
    Queued,
    Fetched,
    Claimed,
    Acknowledged,
    Undeliverable,
}

impl StateName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Fetched => "fetched",
            Self::Claimed => "claimed",
            Self::Acknowledged => "acknowledged",
            Self::Undeliverable => "undeliverable",
        }
    }
}

impl std::fmt::Display for StateName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One message as the inbox holds it. For a publish this is one subscriber's
/// copy; for an offer, the one offer whatever its current holder.
#[derive(Debug, Clone)]
pub struct Message {
    pub id: MessageId,
    pub kind: MessageKind,
    pub to: Address,
    /// `None` only for an offer no one currently holds.
    pub recipient: Option<Recipient>,
    pub from: Sender,
    /// One line, for status and cards.
    pub subject: String,
    /// Shared between the copies of a publish.
    pub body: Arc<str>,
    pub body_trust: BodyTrust,
    /// The `.dot-agent-deck/` file that carries the full body.
    pub spill_path: Option<PathBuf>,
    /// See [`Content::preface`].
    pub preface: Option<String>,
    pub created_at: Instant,
    pub state: MessageState,
    /// Hand-overs so far: each fetch counts one, and for an offer each
    /// assignment to a holder. A redelivery its consumer holds is refunded
    /// ([`Inbox::hold`]).
    pub deliveries: u32,
    /// When it was first fetched, which starts [`InboxLimits::hold_bound`];
    /// for an offer, first fetched by its current holder.
    pub first_fetched_at: Option<Instant>,
    pub history: Vec<(Instant, StateName)>,
    /// Offers only: everyone who has held the offer, never offered it again.
    pub excluded: Vec<Recipient>,
    /// Offers only: when an offer assigned to a holder that has not fetched
    /// it goes back to its pool ([`InboxLimits::offer_assign_wait`]).
    pub assign_deadline: Option<Instant>,
    /// Offers only: whether the holder's stream has said it holds the offer
    /// since it was last handed to it again ([`Inbox::hold`]).
    pub offer_hold: OfferHold,
}

/// PRD #1666 long-turn delivery: where an offer's holder stands on holding it.
/// An offer fetched by a stream is handed to that stream again at the end of
/// its ack-wait instead of going back to its pool, for as long as the stream
/// answers each of those hand-overs with `hold` — bounded by the offer's claim
/// TTL and by [`InboxLimits::hold_bound`] from this assignment's first fetch.
/// Reset whenever the offer leaves its holder.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OfferHold {
    /// Not handed to the holder again yet on this assignment.
    #[default]
    NotAsked,
    /// Handed to the holder's stream again; it has not answered with `hold`.
    Asked,
    /// The holder's stream answered with `hold`.
    Held,
}

impl Message {
    /// A serialisable view for listings and status.
    pub fn summary(&self, now: Instant) -> MessageSummary {
        let (topic, pool) = match &self.kind {
            MessageKind::Direct => (None, None),
            MessageKind::Publish { topic } => (Some(topic.clone()), None),
            MessageKind::Offer { pool, .. } => (None, Some(pool.clone())),
        };
        MessageSummary {
            id: self.id.clone(),
            kind: self.kind.name().to_string(),
            topic,
            pool,
            to: self.to.to_string(),
            recipient: self.recipient.clone(),
            bind_agent: self.recipient.as_ref().and_then(|r| r.agent_id.clone()),
            from: self.from.clone(),
            subject: self.subject.clone(),
            body_trust: self.body_trust,
            spill_path: self.spill_path.clone(),
            state: self.state.name(),
            undeliverable_reason: match &self.state {
                MessageState::Undeliverable { reason, .. } => Some(*reason),
                _ => None,
            },
            ack_how: match &self.state {
                MessageState::Acknowledged { how, .. } => Some(*how),
                _ => None,
            },
            deliveries: self.deliveries,
            age_ms: millis(now.saturating_duration_since(self.created_at)),
            text: None,
        }
    }

    fn is_offer(&self) -> bool {
        matches!(self.kind, MessageKind::Offer { .. })
    }

    fn scope(&self) -> Option<&Scope> {
        match &self.to {
            Address::Topic { scope, .. } | Address::Offer { scope, .. } => Some(scope),
            Address::Direct { .. } => None,
        }
    }
}

/// [`Message::summary`]: what `inbox list` and status read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageSummary {
    pub id: MessageId,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<String>,
    pub to: String,
    pub recipient: Option<Recipient>,
    /// PRD #1666 S5: the agent generation the message is bound to (§2.1's
    /// `bind_agent`, `recipient.agent_id`), named flat so a listing can be
    /// matched against an agent id directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind_agent: Option<String>,
    pub from: Sender,
    pub subject: String,
    pub body_trust: BodyTrust,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spill_path: Option<PathBuf>,
    pub state: StateName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undeliverable_reason: Option<UndeliverableReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ack_how: Option<AckHow>,
    pub deliveries: u32,
    pub age_ms: u64,
    /// PRD #1666 S5: the message rendered whole, as the agent sees it — set
    /// only when a listing asks for bodies
    /// ([`crate::daemon_protocol::AttachRequest::ListInbox`]'s `bodies`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// Per-pane counts for status and cards.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InboxCounts {
    pub queued: usize,
    pub fetched: usize,
    pub claimed: usize,
    pub undeliverable: usize,
    pub last_acked_at: Option<Instant>,
}

/// A state change, for the daemon's broadcasts and sender notices. Drained
/// with [`Inbox::drain_events`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxEvent {
    pub message_id: MessageId,
    pub kind: &'static str,
    /// The holder after the change; `None` for an offer back in its pool.
    pub recipient: Option<Recipient>,
    pub from: Sender,
    /// Whether the body is the deck's or another agent's, which decides how
    /// the sender is labelled.
    pub body_trust: BodyTrust,
    pub subject: String,
    pub state: StateName,
    pub undeliverable_reason: Option<UndeliverableReason>,
    /// An offer that left a holder: who held it and why it left, so that
    /// holder's card is told and the offerer learns of an expired claim.
    pub returned: Option<OfferReturn>,
}

/// An offer leaving the holder it was assigned to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferReturn {
    pub previous: Recipient,
    pub cause: ReturnCause,
}

/// Why an offer left its holder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReturnCause {
    /// The holder never fetched it within [`InboxLimits::offer_assign_wait`].
    NotFetched,
    /// The holder fetched it and did not claim it within the ack wait.
    AckWaitExpired,
    /// The claim lasted longer than its TTL without a completion.
    ClaimExpired,
    /// The claimant handed it back.
    Released,
    /// The holder's agent exited.
    HolderExited,
}

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

/// Every bound and timing the inbox applies. [`Default`] is the production
/// value; [`InboxLimits::from_env`] applies the `DOT_AGENT_DECK_INBOX_*`
/// overrides tests use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxLimits {
    pub ack_wait: Duration,
    pub max_deliver: u32,
    /// See [`DEFAULT_HOLD_BOUND`].
    pub hold_bound: Duration,
    pub max_per_recipient: usize,
    pub max_messages: usize,
    pub max_body_bytes: usize,
    pub history_per_pane: usize,
    pub dedup_window: Duration,
    pub dedup_max_ids: usize,
    pub max_offers_per_scope: usize,
    pub rate_per_minute: u32,
    pub default_claim_ttl: Duration,
    pub min_claim_ttl: Duration,
    pub max_claim_ttl: Duration,
    /// Hardening M3: acknowledged and undeliverable messages kept deck-wide.
    pub max_history_messages: usize,
    /// Hardening M3: body bytes of those messages kept deck-wide.
    pub max_history_bytes: usize,
    /// Hardening M3: how long a terminal message keeps its body for
    /// diagnostics; its summary stays until it leaves the history.
    pub history_body_ttl: Duration,
    /// How long an offer may wait with a holder that has not fetched it
    /// before it returns to its pool for someone else: the same deadline as
    /// the undelivered notice (`DOT_AGENT_DECK_INBOX_UNDELIVERED_NOTICE_AFTER_MS`).
    pub offer_assign_wait: Duration,
}

impl Default for InboxLimits {
    fn default() -> Self {
        Self {
            ack_wait: DEFAULT_ACK_WAIT,
            max_deliver: DEFAULT_MAX_DELIVER,
            hold_bound: DEFAULT_HOLD_BOUND,
            max_per_recipient: DEFAULT_MAX_PER_RECIPIENT,
            max_messages: DEFAULT_MAX_MESSAGES,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            history_per_pane: DEFAULT_HISTORY_PER_PANE,
            dedup_window: DEFAULT_DEDUP_WINDOW,
            dedup_max_ids: DEFAULT_DEDUP_MAX_IDS,
            max_offers_per_scope: DEFAULT_MAX_OFFERS_PER_SCOPE,
            rate_per_minute: DEFAULT_RATE_PER_MINUTE,
            default_claim_ttl: DEFAULT_CLAIM_TTL,
            min_claim_ttl: DEFAULT_MIN_CLAIM_TTL,
            max_claim_ttl: MAX_CLAIM_TTL,
            max_history_messages: DEFAULT_MAX_HISTORY_MESSAGES,
            max_history_bytes: DEFAULT_MAX_HISTORY_BYTES,
            history_body_ttl: DEFAULT_HISTORY_BODY_TTL,
            offer_assign_wait: crate::inbox_hub::DEFAULT_UNDELIVERED_NOTICE_AFTER,
        }
    }
}

impl InboxLimits {
    /// The defaults with the process environment's overrides applied.
    pub fn from_env() -> Self {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// The defaults with overrides from `lookup`. A value that is not a
    /// positive integer is ignored, with a warning, rather than turning a bound
    /// off.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let positive = |name: &str| -> Option<u64> {
            let raw = lookup(name)?;
            match raw.trim().parse::<u64>() {
                Ok(n) if n > 0 => Some(n),
                _ => {
                    tracing::warn!(name, value = %raw, "ignoring an inbox limit that is not a positive integer");
                    None
                }
            }
        };
        let ms = |name: &str| positive(name).map(Duration::from_millis);
        let count = |name: &str| positive(name).map(|n| usize::try_from(n).unwrap_or(usize::MAX));
        let small = |name: &str| positive(name).map(|n| u32::try_from(n).unwrap_or(u32::MAX));
        let d = Self::default();
        let min_claim_ttl = ms(ENV_MIN_CLAIM_TTL_MS).unwrap_or(d.min_claim_ttl);
        Self {
            ack_wait: ms(ENV_ACK_WAIT_MS).unwrap_or(d.ack_wait),
            max_deliver: small(ENV_MAX_DELIVER).unwrap_or(d.max_deliver),
            hold_bound: d.hold_bound,
            max_per_recipient: count(ENV_MAX_PER_RECIPIENT).unwrap_or(d.max_per_recipient),
            max_messages: count(ENV_MAX_MESSAGES).unwrap_or(d.max_messages),
            max_body_bytes: count(ENV_MAX_BODY_BYTES).unwrap_or(d.max_body_bytes),
            history_per_pane: count(ENV_HISTORY_PER_PANE).unwrap_or(d.history_per_pane),
            dedup_window: ms(ENV_DEDUP_WINDOW_MS).unwrap_or(d.dedup_window),
            dedup_max_ids: count(ENV_DEDUP_MAX_IDS).unwrap_or(d.dedup_max_ids),
            max_offers_per_scope: count(ENV_MAX_OFFERS_PER_SCOPE).unwrap_or(d.max_offers_per_scope),
            rate_per_minute: small(ENV_RATE_PER_MINUTE).unwrap_or(d.rate_per_minute),
            default_claim_ttl: d.default_claim_ttl.max(min_claim_ttl),
            min_claim_ttl,
            max_claim_ttl: d.max_claim_ttl.max(min_claim_ttl),
            max_history_messages: d.max_history_messages,
            max_history_bytes: d.max_history_bytes,
            history_body_ttl: d.history_body_ttl,
            offer_assign_wait: ms(crate::inbox_hub::ENV_UNDELIVERED_NOTICE_AFTER_MS)
                .unwrap_or(d.offer_assign_wait),
        }
    }
}

// ---------------------------------------------------------------------------
// Operations and their outcomes
// ---------------------------------------------------------------------------

/// What a producer supplies for any kind of message.
#[derive(Debug, Clone)]
pub struct Content {
    pub from: Sender,
    pub subject: String,
    pub body: String,
    pub body_trust: BodyTrust,
    pub spill_path: Option<PathBuf>,
    /// A sender-chosen id for an idempotent re-post; minted when `None`.
    pub id: Option<MessageId>,
    /// PRD #1666 S5: deck-written text shown before the body, outside the
    /// untrusted frame — what the deck itself says about an agent-authored
    /// body (an unsolicited report, where the full text was saved).
    pub preface: Option<String>,
}

impl Content {
    /// PRD #1666 S5: a trusted message from the deck itself. `reason` names
    /// what kind ([`Sender::Deck`]); the sender label is derived from it.
    pub fn from_deck(
        reason: impl Into<String>,
        subject: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            from: Sender::Deck {
                reason: reason.into(),
            },
            subject: subject.into(),
            body: body.into(),
            body_trust: BodyTrust::Trusted,
            spill_path: None,
            id: None,
            preface: None,
        }
    }
}

/// Why a post was refused. Every bound refuses with one of these: the inbox
/// never drops a message to make room.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case", tag = "refused")]
pub enum PostError {
    #[error("the inbox of pane {pane_id} is full ({limit} messages waiting)")]
    InboxFull { pane_id: String, limit: usize },
    #[error("the deck holds the most messages it may ({limit})")]
    DeckFull { limit: usize },
    #[error("the message body is {bytes} bytes, over the {limit}-byte limit")]
    BodyTooLarge { bytes: usize, limit: usize },
    #[error("message {id} was already posted (now {existing_state:?})")]
    Duplicate {
        id: MessageId,
        /// `None` once the earlier message has aged out of the history.
        existing_state: Option<StateName>,
    },
    #[error("more than {limit_per_minute} publishes and offers in a minute")]
    RateLimited { limit_per_minute: u32 },
    #[error("the scope already holds {limit} open offers")]
    TooManyOffers { limit: usize },
    #[error("a claim must last between {min_ms} ms and {max_ms} ms")]
    ClaimTtlOutOfRange { min_ms: u64, max_ms: u64 },
    /// PR #1735 fifth review (N4): the agent generation a bound message was
    /// for had left its pane by the time the message would have gone in, so
    /// nothing was posted rather than a message nobody can take.
    #[error("the agent this message is for no longer holds pane {pane_id}; nothing was posted")]
    RecipientGone { pane_id: String },
}

/// [`Inbox::publish`]'s result. Subscribers whose inbox is full are named,
/// not skipped silently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    pub id: MessageId,
    pub delivered_to: usize,
    pub refused_full: Vec<String>,
}

/// [`Inbox::offer`]'s result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offered {
    pub id: MessageId,
    /// `None` when no candidate is eligible yet; the offer waits in its pool.
    pub offered_to: Option<Recipient>,
}

/// The answer to an ack. Serialises in snake case (`already_acknowledged`,
/// `wrong_agent`, …).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AckOutcome {
    Acknowledged,
    /// An offer's ack: the caller now holds it until `expires_in_ms` passes.
    Claimed {
        expires_in_ms: u64,
    },
    /// Already acknowledged: a no-op. (A claimant acking its claimed offer
    /// again gets `Claimed` with the time left.)
    AlreadyAcknowledged,
    /// The message exists but is not this agent's.
    WrongAgent,
    /// Too late: the message is undeliverable.
    Undeliverable,
    /// No such message (an old `d-…` delivery id included).
    Unknown,
}

/// The answer to an offer's `complete` or `release`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimOutcome {
    Completed,
    Released,
    /// The caller's claim ran out before it settled it: the offer went back
    /// to its pool, as it does when the ticker finds the claim expired.
    Expired,
    /// The offer is not claimed by this caller.
    NotClaimant,
    /// Not an open offer.
    Unknown,
}

// ---------------------------------------------------------------------------
// The inbox
// ---------------------------------------------------------------------------

/// All messages one daemon holds. See the module docs.
#[derive(Debug)]
pub struct Inbox {
    deck: DeckId,
    limits: InboxLimits,
    /// Live messages and the per-pane terminal history, oldest first.
    messages: Vec<Message>,
    /// Ids posted inside the dedup window, oldest first.
    dedup: VecDeque<(Instant, MessageId)>,
    /// Last state of every id still in `dedup`.
    dedup_state: HashMap<MessageId, StateName>,
    /// Publish and offer times per sending agent, inside the rate window.
    rate: HashMap<String, VecDeque<Instant>>,
    events: Vec<InboxEvent>,
    /// PR #1735 second review: room held for answers the deck has promised
    /// ([`Self::reserve_reply`]), which every other post counts as taken.
    reserved_replies: usize,
}

impl Inbox {
    pub fn new(deck: DeckId, limits: InboxLimits) -> Self {
        Self {
            deck,
            limits,
            messages: Vec::new(),
            dedup: VecDeque::new(),
            dedup_state: HashMap::new(),
            rate: HashMap::new(),
            events: Vec::new(),
            reserved_replies: 0,
        }
    }

    pub fn deck(&self) -> &DeckId {
        &self.deck
    }

    pub fn limits(&self) -> &InboxLimits {
        &self.limits
    }

    /// Audit F2: the refusal [`Self::post_direct`] would give `content` for
    /// `to` now, without posting it, so a caller can refuse before it saves
    /// the body to a file.
    pub fn admit_direct(
        &mut self,
        now: Instant,
        to: &Recipient,
        content: &Content,
    ) -> Result<(), PostError> {
        self.admit_reply(now, content)?;
        if !self.has_room_for(&to.pane_id) {
            return Err(PostError::InboxFull {
                pane_id: to.pane_id.clone(),
                limit: self.limits.max_per_recipient,
            });
        }
        Ok(())
    }

    /// The refusal a post of `content` would get now whoever it is for: its
    /// body, its id and the deck's room. A new agent's first prompt is
    /// admitted with it before the agent starts ([`crate::spawn::admit_seed`]).
    pub fn admit_reply(&mut self, now: Instant, content: &Content) -> Result<(), PostError> {
        self.expire_dedup(now);
        self.check_body(content)?;
        self.check_id(content.id.as_ref())?;
        self.check_room(1)
    }

    /// Post one direct message to `to`.
    pub fn post_direct(
        &mut self,
        now: Instant,
        to: Recipient,
        content: Content,
    ) -> Result<MessageId, PostError> {
        self.admit_direct(now, &to, &content)?;
        Ok(self.insert_direct(now, to, content))
    }

    /// PR #1735 second review: hold room for one answer the deck owes a
    /// request it is accepting (a dispatch's result), so posts that arrive
    /// while the request runs cannot take it. Refused when the deck has no
    /// room now. Given back by [`Self::post_reserved_reply`] or
    /// [`Self::release_reply`].
    pub fn reserve_reply(&mut self) -> Result<(), PostError> {
        self.check_room(1)?;
        self.reserved_replies += 1;
        Ok(())
    }

    /// PR #1735 fifth review (B-01, B-02): hold room for a delegated task to
    /// `pane_id` that replaces `replacing` older tasks still live there, as
    /// [`Self::reserve_reply`] holds it for an answer. Its body and id are
    /// checked now, before anything is retired or restarted for it. The older
    /// tasks give their room back as the task goes in, so a deck or an inbox
    /// they fill is no refusal. The reservation counts against every other
    /// post's deck-wide room at once, so the room those tasks free stays taken
    /// by it; posted with [`Self::post_reserved_reply`], it is not checked
    /// against the recipient's backlog again.
    pub fn reserve_delegate(
        &mut self,
        now: Instant,
        pane_id: &str,
        content: &Content,
        replacing: usize,
    ) -> Result<(), PostError> {
        self.admit_reserved_reply(now, content)?;
        if self.live_count() + self.reserved_replies + 1 > self.limits.max_messages + replacing {
            return Err(PostError::DeckFull {
                limit: self.limits.max_messages,
            });
        }
        if self.live_for_pane(pane_id) >= self.limits.max_per_recipient + replacing {
            return Err(PostError::InboxFull {
                pane_id: pane_id.to_string(),
                limit: self.limits.max_per_recipient,
            });
        }
        self.reserved_replies += 1;
        Ok(())
    }

    /// TEST: how many reservations are held.
    #[cfg(test)]
    pub fn reserved_replies(&self) -> usize {
        self.reserved_replies
    }

    /// Give back one reservation [`Self::reserve_reply`] made, unused.
    pub fn release_reply(&mut self) {
        self.reserved_replies = self.reserved_replies.saturating_sub(1);
    }

    /// The refusal [`Self::post_reserved_reply`] would give `content` now: its
    /// body and id only, since the reservation is its room.
    pub fn admit_reserved_reply(
        &mut self,
        now: Instant,
        content: &Content,
    ) -> Result<(), PostError> {
        self.expire_dedup(now);
        self.check_body(content)?;
        self.check_id(content.id.as_ref())
    }

    /// Post the answer one [`Self::reserve_reply`] held room for, to `to`,
    /// using that reservation. Neither the recipient's own backlog nor the
    /// deck-wide bound refuses it; its body and id are checked as any post's
    /// are, and a refusal leaves the reservation held.
    pub fn post_reserved_reply(
        &mut self,
        now: Instant,
        to: Recipient,
        content: Content,
    ) -> Result<MessageId, PostError> {
        self.admit_reserved_reply(now, &content)?;
        self.release_reply();
        Ok(self.insert_direct(now, to, content))
    }

    fn insert_direct(&mut self, now: Instant, to: Recipient, content: Content) -> MessageId {
        let id = self.register_id(now, content.id.clone());
        let address = Address::Direct {
            deck: self.deck.clone(),
            pane_id: to.pane_id.clone(),
        };
        let body: Arc<str> = Arc::from(content.body.as_str());
        self.insert(
            now,
            id.clone(),
            MessageKind::Direct,
            address,
            Some(to),
            &content,
            body,
        );
        self.trim_history(now);
        id
    }

    /// Audit F2: the refusal [`Self::publish`] would give `content` for
    /// `recipients` now, without publishing it.
    pub fn admit_publish(
        &mut self,
        now: Instant,
        content: &Content,
        recipients: &[Recipient],
    ) -> Result<(), PostError> {
        self.expire_dedup(now);
        self.check_body(content)?;
        self.check_rate(now, &content.from)?;
        self.check_id(content.id.as_ref())?;
        let room = recipients
            .iter()
            .filter(|r| self.has_room_for(&r.pane_id))
            .count();
        self.check_room(room)
    }

    /// Publish to every recipient in `recipients` (resolved by
    /// [`publish_recipients`]). Each gets its own copy under the one id.
    pub fn publish(
        &mut self,
        now: Instant,
        scope: Scope,
        topic: &str,
        content: Content,
        recipients: &[Recipient],
    ) -> Result<Published, PostError> {
        self.admit_publish(now, &content, recipients)?;
        let (room, full): (Vec<&Recipient>, Vec<&Recipient>) = recipients
            .iter()
            .partition(|r| self.has_room_for(&r.pane_id));
        let id = self.register_id(now, content.id.clone());
        self.record_rate(now, &content.from);
        let address = Address::Topic {
            deck: self.deck.clone(),
            scope,
            topic: topic.to_string(),
        };
        let body: Arc<str> = Arc::from(content.body.as_str());
        for recipient in &room {
            self.insert(
                now,
                id.clone(),
                MessageKind::Publish {
                    topic: topic.to_string(),
                },
                address.clone(),
                Some((*recipient).clone()),
                &content,
                body.clone(),
            );
        }
        self.trim_history(now);
        Ok(Published {
            id,
            delivered_to: room.len(),
            refused_full: full.iter().map(|r| r.pane_id.clone()).collect(),
        })
    }

    /// Offer a job in `pool` and push it to the first eligible candidate
    /// (resolved by [`offer_candidates`]). With none eligible it waits in its
    /// pool for [`Inbox::assign_offer`].
    pub fn offer(
        &mut self,
        now: Instant,
        scope: Scope,
        pool: &str,
        claim_ttl: Option<Duration>,
        content: Content,
        candidates: &[Recipient],
    ) -> Result<Offered, PostError> {
        let claim_ttl = self.admit_offer(now, &scope, claim_ttl, &content)?;
        let id = self.register_id(now, content.id.clone());
        self.record_rate(now, &content.from);
        let address = Address::Offer {
            deck: self.deck.clone(),
            scope,
            pool: pool.to_string(),
        };
        let body: Arc<str> = Arc::from(content.body.as_str());
        self.insert(
            now,
            id.clone(),
            MessageKind::Offer {
                pool: pool.to_string(),
                claim_ttl,
            },
            address,
            None,
            &content,
            body,
        );
        let offered_to = self.assign_offer(now, &id, candidates);
        Ok(Offered { id, offered_to })
    }

    /// Audit F2: the refusal [`Self::offer`] would give `content` in `scope`
    /// now, without offering it. Answers the claim time the offer would get.
    pub fn admit_offer(
        &mut self,
        now: Instant,
        scope: &Scope,
        claim_ttl: Option<Duration>,
        content: &Content,
    ) -> Result<Duration, PostError> {
        self.expire_dedup(now);
        self.check_body(content)?;
        let claim_ttl = claim_ttl.unwrap_or(self.limits.default_claim_ttl);
        if claim_ttl < self.limits.min_claim_ttl || claim_ttl > self.limits.max_claim_ttl {
            return Err(PostError::ClaimTtlOutOfRange {
                min_ms: millis(self.limits.min_claim_ttl),
                max_ms: millis(self.limits.max_claim_ttl),
            });
        }
        self.check_rate(now, &content.from)?;
        let open = self
            .messages
            .iter()
            .filter(|m| m.is_offer() && !m.state.is_terminal() && m.scope() == Some(scope))
            .count();
        if open >= self.limits.max_offers_per_scope {
            return Err(PostError::TooManyOffers {
                limit: self.limits.max_offers_per_scope,
            });
        }
        self.check_id(content.id.as_ref())?;
        self.check_room(1)?;
        Ok(claim_ttl)
    }

    /// Push an offer that no one holds to the first candidate that has not
    /// held it before. `None` when every candidate is excluded (or full), or
    /// the offer is not waiting in its pool.
    pub fn assign_offer(
        &mut self,
        now: Instant,
        id: &MessageId,
        candidates: &[Recipient],
    ) -> Option<Recipient> {
        let index = self.messages.iter().position(|m| {
            &m.id == id && m.is_offer() && m.recipient.is_none() && m.state == MessageState::Queued
        })?;
        let excluded = &self.messages[index].excluded;
        let chosen = candidates
            .iter()
            .find(|c| {
                !excluded.iter().any(|e| e.same_holder(c))
                    && self.live_for_pane(&c.pane_id) < self.limits.max_per_recipient
            })?
            .clone();
        self.messages[index].recipient = Some(chosen.clone());
        self.messages[index].deliveries += 1;
        self.messages[index].assign_deadline = Some(now + self.limits.offer_assign_wait);
        self.transition(index, now, MessageState::Queued);
        Some(chosen)
    }

    /// Offers waiting in their pool with no holder, oldest first.
    pub fn unassigned_offers(&self) -> Vec<MessageId> {
        self.messages
            .iter()
            .filter(|m| m.is_offer() && m.recipient.is_none() && m.state == MessageState::Queued)
            .map(|m| m.id.clone())
            .collect()
    }

    /// Hand `agent_id` in `pane_id` every queued message it may receive, up to
    /// `max`, oldest first. Each becomes fetched by `consumer`: no other
    /// consumer gets it until it returns to the queue.
    pub fn fetch(
        &mut self,
        now: Instant,
        pane_id: &str,
        agent_id: &str,
        consumer: Consumer,
        max: Option<usize>,
    ) -> Vec<Message> {
        let ack_deadline = now + self.limits.ack_wait;
        let indices: Vec<usize> = self
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                m.state == MessageState::Queued
                    && m.recipient
                        .as_ref()
                        .is_some_and(|r| r.admits(pane_id, agent_id))
            })
            .map(|(i, _)| i)
            .take(max.unwrap_or(usize::MAX))
            .collect();
        let mut out = Vec::with_capacity(indices.len());
        for index in indices {
            // An offer counted its delivery when it was assigned.
            if !self.messages[index].is_offer() {
                self.messages[index].deliveries += 1;
            }
            self.messages[index].first_fetched_at.get_or_insert(now);
            self.transition(
                index,
                now,
                MessageState::Fetched {
                    by: consumer.clone(),
                    at: now,
                    ack_deadline,
                },
            );
            out.push(self.messages[index].clone());
        }
        out
    }

    /// [`Inbox::fetch`] and acknowledge in one step, for a consumer whose
    /// hand-over is itself the delivery (the CLI, a turn-end hook). Offers are
    /// claimed rather than acknowledged.
    pub fn fetch_acked(
        &mut self,
        now: Instant,
        pane_id: &str,
        agent_id: &str,
        consumer: Consumer,
        how: AckHow,
        max: Option<usize>,
    ) -> Vec<Message> {
        let fetched = self.fetch(now, pane_id, agent_id, consumer, max);
        for message in &fetched {
            self.ack(now, message.id.as_str(), pane_id, agent_id, how);
        }
        fetched
    }

    /// Acknowledge message `id` on behalf of `agent_id` in `pane_id`. A queued
    /// message may be acknowledged directly (its content was seen without a
    /// fetch). Any id that is not a message id answers `Unknown`.
    pub fn ack(
        &mut self,
        now: Instant,
        id: &str,
        pane_id: &str,
        agent_id: &str,
        how: AckHow,
    ) -> AckOutcome {
        let Some(index) = self.find_for(id, pane_id, agent_id) else {
            return if self.messages.iter().any(|m| m.id.as_str() == id) {
                AckOutcome::WrongAgent
            } else {
                AckOutcome::Unknown
            };
        };
        let message = &self.messages[index];
        match &message.state {
            MessageState::Acknowledged { .. } => AckOutcome::AlreadyAcknowledged,
            // PRD #1666 long-turn delivery: a message that ran out of
            // hand-overs while its agent was still holding it reached the agent
            // after all. The ack is the proof, so it is recorded; whoever was
            // told it was undeliverable is corrected by the daemon. An offer
            // went back to its pool and was told "not taken", so it stays put.
            MessageState::Undeliverable {
                reason: UndeliverableReason::MaxDeliver,
                ..
            } if !message.is_offer() => {
                self.transition(index, now, MessageState::Acknowledged { at: now, how });
                self.trim_history(now);
                AckOutcome::Acknowledged
            }
            MessageState::Undeliverable { .. } => AckOutcome::Undeliverable,
            MessageState::Claimed { expires, .. } => AckOutcome::Claimed {
                expires_in_ms: millis(expires.saturating_duration_since(now)),
            },
            MessageState::Queued | MessageState::Fetched { .. } => {
                if let MessageKind::Offer { claim_ttl, .. } = message.kind {
                    let by = message.recipient.clone().unwrap_or_else(|| {
                        Recipient::bound(pane_id.to_string(), agent_id.to_string())
                    });
                    let expires = now + claim_ttl;
                    self.transition(
                        index,
                        now,
                        MessageState::Claimed {
                            by,
                            at: now,
                            expires,
                        },
                    );
                    AckOutcome::Claimed {
                        expires_in_ms: millis(claim_ttl),
                    }
                } else {
                    self.transition(index, now, MessageState::Acknowledged { at: now, how });
                    self.trim_history(now);
                    AckOutcome::Acknowledged
                }
            }
        }
    }

    /// PRD #1666 long-turn delivery: the streaming consumer holding message
    /// `id` answered its redelivery with `hold` — the agent still has the
    /// message queued behind the turn in progress, and it was not handed over
    /// again. That redelivery's hand-over is refunded (`deliveries` never drops
    /// below one), so a turn longer than ack-wait × `max_deliver` does not make
    /// the message undeliverable. For an offer, whose count is assignments
    /// rather than fetches, nothing is refunded: the hold keeps the offer with
    /// its holder at the end of the ack-wait ([`OfferHold`]). Honoured only
    /// while the message is fetched by `consumer` and was first fetched less
    /// than [`InboxLimits::hold_bound`] ago — for an offer, less than its claim
    /// TTL too, from this assignment's first fetch; past that bound
    /// `max_deliver` applies, and an offer returns to its pool, as for every
    /// other consumer. Returns whether the hold was honoured.
    pub fn hold(
        &mut self,
        now: Instant,
        id: &str,
        pane_id: &str,
        agent_id: &str,
        consumer: &Consumer,
    ) -> bool {
        let Some(index) = self.find_for(id, pane_id, agent_id) else {
            return false;
        };
        let within_bound = self.within_hold_bound(index, now);
        let message = &mut self.messages[index];
        let fetched_by_consumer =
            matches!(&message.state, MessageState::Fetched { by, .. } if by == consumer);
        if !fetched_by_consumer || !within_bound {
            return false;
        }
        if message.is_offer() {
            message.offer_hold = OfferHold::Held;
        } else {
            message.deliveries = message.deliveries.saturating_sub(1).max(1);
        }
        true
    }

    /// Message `index` was first fetched less than its hold bound ago:
    /// [`InboxLimits::hold_bound`], and for an offer its claim TTL too.
    fn within_hold_bound(&self, index: usize, now: Instant) -> bool {
        let message = &self.messages[index];
        let bound = match message.kind {
            MessageKind::Offer { claim_ttl, .. } => claim_ttl.min(self.limits.hold_bound),
            _ => self.limits.hold_bound,
        };
        message
            .first_fetched_at
            .is_some_and(|first| now.saturating_duration_since(first) < bound)
    }

    /// PRD #1666 long-turn delivery: an offer whose ack-wait ran out stays with
    /// its holder when a stream fetched it, the stream has not left a hand-over
    /// of it unanswered, and it is within its hold bound ([`OfferHold`]).
    fn offer_stays_with_holder(&self, index: usize, now: Instant) -> bool {
        let message = &self.messages[index];
        message.is_offer()
            && matches!(
                &message.state,
                MessageState::Fetched {
                    by: Consumer::Stream { .. },
                    ..
                }
            )
            && message.offer_hold != OfferHold::Asked
            && self.within_hold_bound(index, now)
    }

    /// A consumer handing a fetched message back unread. It is redelivered
    /// (or becomes undeliverable at `max_deliver`). Returns whether the
    /// message was fetched by this agent.
    pub fn nack(&mut self, now: Instant, id: &str, pane_id: &str, agent_id: &str) -> bool {
        let Some(index) = self.find_for(id, pane_id, agent_id) else {
            return false;
        };
        if !matches!(self.messages[index].state, MessageState::Fetched { .. }) {
            return false;
        }
        self.redeliver(index, now);
        self.trim_history(now);
        true
    }

    /// A consumer whose connection dropped before it handed its messages
    /// over: everything it fetched and nobody acknowledged is redelivered.
    pub fn requeue_consumer(&mut self, now: Instant, consumer: &Consumer) -> usize {
        let indices: Vec<usize> = self
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| matches!(&m.state, MessageState::Fetched { by, .. } if by == consumer))
            .map(|(i, _)| i)
            .collect();
        for &index in &indices {
            self.redeliver(index, now);
        }
        self.trim_history(now);
        indices.len()
    }

    /// The claimant of offer `id` finished the job.
    pub fn complete(
        &mut self,
        now: Instant,
        id: &str,
        pane_id: &str,
        agent_id: &str,
    ) -> ClaimOutcome {
        match self.claimed_by(id, pane_id, agent_id) {
            Ok(index) if self.expire_claim_if_due(index, now) => ClaimOutcome::Expired,
            Ok(index) => {
                self.transition(
                    index,
                    now,
                    MessageState::Acknowledged {
                        at: now,
                        how: AckHow::OfferCompleted,
                    },
                );
                self.trim_history(now);
                ClaimOutcome::Completed
            }
            Err(outcome) => outcome,
        }
    }

    /// The claimant of offer `id` gives it back: it returns to its pool,
    /// never to be offered to this claimant again.
    pub fn release(
        &mut self,
        now: Instant,
        id: &str,
        pane_id: &str,
        agent_id: &str,
    ) -> ClaimOutcome {
        match self.claimed_by(id, pane_id, agent_id) {
            Ok(index) if self.expire_claim_if_due(index, now) => ClaimOutcome::Expired,
            Ok(index) => {
                self.return_offer(index, now, ReturnCause::Released);
                self.trim_history(now);
                ClaimOutcome::Released
            }
            Err(outcome) => outcome,
        }
    }

    /// The agent `agent_id` in `pane_id` exited. Messages bound to it that are
    /// still owed become undeliverable; offers it held return to their pool.
    /// Unbound messages wait for the pane's next agent.
    pub fn agent_exited(&mut self, now: Instant, pane_id: &str, agent_id: &str) {
        let mut index = self.messages.len();
        while index > 0 {
            index -= 1;
            let m = &self.messages[index];
            if m.state.is_terminal() {
                continue;
            }
            let held = m
                .recipient
                .as_ref()
                .is_some_and(|r| r.pane_id == pane_id && r.agent_id.as_deref() == Some(agent_id));
            if !held {
                continue;
            }
            if m.is_offer() {
                self.return_offer(index, now, ReturnCause::HolderExited);
            } else {
                self.transition(
                    index,
                    now,
                    MessageState::Undeliverable {
                        at: now,
                        reason: UndeliverableReason::AgentExited,
                    },
                );
            }
        }
        self.trim_history(now);
    }

    /// Retire a message that is not yet acknowledged, for `reason`. Returns
    /// whether it was live.
    pub fn retire(
        &mut self,
        now: Instant,
        id: &str,
        pane_id: &str,
        reason: UndeliverableReason,
    ) -> bool {
        let Some(index) = self.messages.iter().position(|m| {
            m.id.as_str() == id
                && !m.state.is_terminal()
                && m.recipient.as_ref().is_some_and(|r| r.pane_id == pane_id)
        }) else {
            return false;
        };
        self.transition(index, now, MessageState::Undeliverable { at: now, reason });
        self.trim_history(now);
        true
    }

    /// Apply every deadline that has passed by `now`: unacknowledged fetches
    /// are redelivered (or become undeliverable at `max_deliver`), expired
    /// claims and offers their holder never fetched return to their pool, and
    /// the dedup and rate windows move on.
    pub fn tick(&mut self, now: Instant) {
        let mut index = self.messages.len();
        while index > 0 {
            index -= 1;
            match &self.messages[index].state {
                MessageState::Fetched { ack_deadline, .. } if *ack_deadline <= now => {
                    if self.offer_stays_with_holder(index, now) {
                        // Handed to the same holder again, uncounted: an
                        // offer counted its delivery when it was assigned.
                        let message = &mut self.messages[index];
                        message.offer_hold = OfferHold::Asked;
                        message.assign_deadline = Some(now + self.limits.offer_assign_wait);
                        self.transition(index, now, MessageState::Queued);
                    } else {
                        self.redeliver(index, now);
                    }
                }
                MessageState::Claimed { expires, .. } if *expires <= now => {
                    self.return_offer(index, now, ReturnCause::ClaimExpired);
                }
                MessageState::Queued
                    if self.messages[index]
                        .assign_deadline
                        .is_some_and(|due| due <= now) =>
                {
                    self.return_offer(index, now, ReturnCause::NotFetched);
                }
                _ => {}
            }
        }
        self.trim_history(now);
        self.expire_dedup(now);
        self.rate.retain(|_, times| {
            while times
                .front()
                .is_some_and(|t| now.duration_since(*t) >= RATE_WINDOW)
            {
                times.pop_front();
            }
            !times.is_empty()
        });
    }

    /// The earliest ack deadline, claim expiry or unfetched offer's
    /// assignment deadline, so the daemon can sleep until there is something
    /// for [`Inbox::tick`] to do.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.messages
            .iter()
            .filter_map(|m| match &m.state {
                MessageState::Fetched { ack_deadline, .. } => Some(*ack_deadline),
                MessageState::Claimed { expires, .. } => Some(*expires),
                MessageState::Queued => m.assign_deadline,
                _ => None,
            })
            .min()
    }

    /// Every message for `pane_id`, live and recent history, oldest first.
    pub fn list(&self, pane_id: &str) -> Vec<&Message> {
        self.messages
            .iter()
            .filter(|m| m.recipient.as_ref().is_some_and(|r| r.pane_id == pane_id))
            .collect()
    }

    /// Every message the inbox holds, oldest first.
    pub fn all(&self) -> &[Message] {
        &self.messages
    }

    /// The copy of `id` held for `pane_id`, or the offer with that id.
    pub fn get(&self, id: &str, pane_id: &str) -> Option<&Message> {
        self.messages.iter().find(|m| {
            m.id.as_str() == id
                && (m.recipient.as_ref().is_some_and(|r| r.pane_id == pane_id)
                    || (m.is_offer() && m.recipient.is_none()))
        })
    }

    pub fn counts(&self, pane_id: &str) -> InboxCounts {
        let mut counts = InboxCounts::default();
        for m in self.list(pane_id) {
            match &m.state {
                MessageState::Queued => counts.queued += 1,
                MessageState::Fetched { .. } => counts.fetched += 1,
                MessageState::Claimed { .. } => counts.claimed += 1,
                MessageState::Undeliverable { .. } => counts.undeliverable += 1,
                MessageState::Acknowledged { at, .. } => {
                    counts.last_acked_at = Some(counts.last_acked_at.map_or(*at, |t| t.max(*at)));
                }
            }
        }
        counts
    }

    /// Messages not yet acknowledged or undeliverable, daemon-wide.
    pub fn live_count(&self) -> usize {
        self.messages
            .iter()
            .filter(|m| !m.state.is_terminal())
            .count()
    }

    /// Hardening M3: drop `pane_id`'s acknowledged and undeliverable history,
    /// once the pane is closed. Live messages are untouched.
    pub fn forget_pane_history(&mut self, pane_id: &str) {
        self.messages.retain(|m| {
            !m.state.is_terminal() || m.recipient.as_ref().is_none_or(|r| r.pane_id != pane_id)
        });
    }

    pub fn drain_events(&mut self) -> Vec<InboxEvent> {
        std::mem::take(&mut self.events)
    }

    // --- internals ---------------------------------------------------------

    fn check_body(&self, content: &Content) -> Result<(), PostError> {
        let bytes = content.body.len();
        if bytes > self.limits.max_body_bytes {
            return Err(PostError::BodyTooLarge {
                bytes,
                limit: self.limits.max_body_bytes,
            });
        }
        Ok(())
    }

    /// `pane_id`'s inbox is below its per-recipient bound.
    fn has_room_for(&self, pane_id: &str) -> bool {
        self.live_for_pane(pane_id) < self.limits.max_per_recipient
    }

    fn check_room(&self, needed: usize) -> Result<(), PostError> {
        if self.live_count() + self.reserved_replies + needed > self.limits.max_messages {
            return Err(PostError::DeckFull {
                limit: self.limits.max_messages,
            });
        }
        Ok(())
    }

    fn check_rate(&self, now: Instant, from: &Sender) -> Result<(), PostError> {
        let Some(agent) = from.agent_id() else {
            return Ok(());
        };
        let recent = self.rate.get(agent).map_or(0, |times| {
            times
                .iter()
                .filter(|t| now.duration_since(**t) < RATE_WINDOW)
                .count()
        });
        if recent >= self.limits.rate_per_minute as usize {
            return Err(PostError::RateLimited {
                limit_per_minute: self.limits.rate_per_minute,
            });
        }
        Ok(())
    }

    fn record_rate(&mut self, now: Instant, from: &Sender) {
        if let Some(agent) = from.agent_id() {
            self.rate
                .entry(agent.to_string())
                .or_default()
                .push_back(now);
        }
    }

    /// Refuse a sender-chosen id that is already held or still inside the
    /// dedup window.
    fn check_id(&self, wanted: Option<&MessageId>) -> Result<(), PostError> {
        let Some(id) = wanted else {
            return Ok(());
        };
        let held = self.messages.iter().find(|m| &m.id == id);
        if held.is_some() || self.dedup_state.contains_key(id) {
            let existing_state = held
                .map(|m| m.state.name())
                .or_else(|| self.dedup_state.get(id).copied());
            return Err(PostError::Duplicate {
                id: id.clone(),
                existing_state,
            });
        }
        Ok(())
    }

    /// The sender's id (already through [`Self::check_id`]) or a fresh one,
    /// remembered for the dedup window. Called only once nothing can refuse
    /// the post, so a refused post never burns its id.
    fn register_id(&mut self, now: Instant, wanted: Option<MessageId>) -> MessageId {
        let id = wanted.unwrap_or_else(|| {
            loop {
                let id = MessageId::generate(&self.deck);
                if !self.dedup_state.contains_key(&id) && !self.messages.iter().any(|m| m.id == id)
                {
                    break id;
                }
            }
        });
        self.dedup.push_back((now, id.clone()));
        self.dedup_state.insert(id.clone(), StateName::Queued);
        while self.dedup.len() > self.limits.dedup_max_ids {
            if let Some((_, old)) = self.dedup.pop_front() {
                self.dedup_state.remove(&old);
            }
        }
        id
    }

    fn expire_dedup(&mut self, now: Instant) {
        while let Some((at, _)) = self.dedup.front() {
            if now.saturating_duration_since(*at) < self.limits.dedup_window {
                break;
            }
            if let Some((_, old)) = self.dedup.pop_front() {
                self.dedup_state.remove(&old);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn insert(
        &mut self,
        now: Instant,
        id: MessageId,
        kind: MessageKind,
        to: Address,
        recipient: Option<Recipient>,
        content: &Content,
        body: Arc<str>,
    ) {
        // Hardening M3: what travels beside the body is bounded here, where
        // it is stored, so the deck's memory bound is not a body-only one.
        let message = Message {
            id,
            kind,
            to,
            recipient,
            from: capped_sender(&content.from),
            subject: capped_subject(&content.subject),
            body,
            body_trust: content.body_trust,
            spill_path: content
                .spill_path
                .clone()
                .filter(|path| path.as_os_str().len() <= MAX_STORED_PATH_BYTES),
            preface: content
                .preface
                .as_deref()
                .map(|preface| cut_bytes(preface, MAX_STORED_PREFACE_BYTES).to_string()),
            created_at: now,
            state: MessageState::Queued,
            deliveries: 0,
            first_fetched_at: None,
            history: Vec::new(),
            excluded: Vec::new(),
            assign_deadline: None,
            offer_hold: OfferHold::NotAsked,
        };
        self.messages.push(message);
        let index = self.messages.len() - 1;
        self.transition(index, now, MessageState::Queued);
    }

    fn live_for_pane(&self, pane_id: &str) -> usize {
        self.messages
            .iter()
            .filter(|m| {
                matches!(m.state, MessageState::Queued | MessageState::Fetched { .. })
                    && m.recipient.as_ref().is_some_and(|r| r.pane_id == pane_id)
            })
            .count()
    }

    /// The copy of `id` this agent may act on.
    fn find_for(&self, id: &str, pane_id: &str, agent_id: &str) -> Option<usize> {
        self.messages.iter().position(|m| {
            m.id.as_str() == id
                && m.recipient
                    .as_ref()
                    .is_some_and(|r| r.admits(pane_id, agent_id))
        })
    }

    fn claimed_by(&self, id: &str, pane_id: &str, agent_id: &str) -> Result<usize, ClaimOutcome> {
        let Some(index) = self
            .messages
            .iter()
            .position(|m| m.id.as_str() == id && m.is_offer() && !m.state.is_terminal())
        else {
            return Err(ClaimOutcome::Unknown);
        };
        match &self.messages[index].state {
            MessageState::Claimed { by, .. } if by.admits(pane_id, agent_id) => Ok(index),
            _ => Err(ClaimOutcome::NotClaimant),
        }
    }

    /// The claim on offer `index` ran out by `now` and the ticker has not
    /// returned the offer yet: return it now, as [`Inbox::tick`] would. A
    /// claim is honoured only until it expires, whichever of the two gets to
    /// the inbox first.
    fn expire_claim_if_due(&mut self, index: usize, now: Instant) -> bool {
        let due = matches!(
            &self.messages[index].state,
            MessageState::Claimed { expires, .. } if *expires <= now
        );
        if due {
            self.return_offer(index, now, ReturnCause::ClaimExpired);
            self.trim_history(now);
        }
        due
    }

    /// A fetched message that was not acknowledged goes back to the queue, or
    /// becomes undeliverable once it has been handed over `max_deliver` times.
    /// An offer goes back to its pool instead, excluding its holder.
    fn redeliver(&mut self, index: usize, now: Instant) {
        if self.messages[index].is_offer() {
            self.return_offer(index, now, ReturnCause::AckWaitExpired);
        } else if self.messages[index].deliveries >= self.limits.max_deliver {
            self.transition(
                index,
                now,
                MessageState::Undeliverable {
                    at: now,
                    reason: UndeliverableReason::MaxDeliver,
                },
            );
        } else {
            self.transition(index, now, MessageState::Queued);
        }
    }

    /// An offer leaves its holder and returns to its pool, never to be offered
    /// to that holder again — or becomes undeliverable at `max_deliver`.
    fn return_offer(&mut self, index: usize, now: Instant, cause: ReturnCause) {
        let message = &mut self.messages[index];
        message.assign_deadline = None;
        message.first_fetched_at = None;
        message.offer_hold = OfferHold::NotAsked;
        let previous = message.recipient.take();
        if let Some(holder) = &previous {
            message.excluded.push(holder.clone());
        }
        if message.deliveries >= self.limits.max_deliver {
            self.transition(
                index,
                now,
                MessageState::Undeliverable {
                    at: now,
                    reason: UndeliverableReason::MaxDeliver,
                },
            );
        } else {
            self.transition(index, now, MessageState::Queued);
        }
        if let (Some(previous), Some(event)) = (previous, self.events.last_mut()) {
            event.returned = Some(OfferReturn { previous, cause });
        }
    }

    /// The one place a state changes: records history, the dedup state and an
    /// event.
    fn transition(&mut self, index: usize, now: Instant, state: MessageState) {
        let message = &mut self.messages[index];
        message.state = state;
        if !matches!(message.state, MessageState::Queued) {
            message.assign_deadline = None;
        }
        let name = message.state.name();
        message.history.push((now, name));
        if let Some(known) = self.dedup_state.get_mut(&message.id) {
            *known = name;
        }
        self.events.push(InboxEvent {
            message_id: message.id.clone(),
            kind: message.kind.name(),
            recipient: message.recipient.clone(),
            from: message.from.clone(),
            body_trust: message.body_trust,
            subject: message.subject.clone(),
            state: name,
            undeliverable_reason: match &message.state {
                MessageState::Undeliverable { reason, .. } => Some(*reason),
                _ => None,
            },
            returned: None,
        });
    }

    /// Keep at most `history_per_pane` acknowledged or undeliverable messages
    /// per pane (offers that ended in their pool count as one more "pane"),
    /// dropping the oldest. Run at the end of every operation that can end a
    /// message, never inside one, so no index an operation holds moves.
    ///
    /// PRD #1666 hardening M3: and bound the history deck-wide too — at most
    /// [`InboxLimits::max_history_messages`] messages and
    /// [`InboxLimits::max_history_bytes`] of their bodies, oldest dropped
    /// first — and drop the body of a message that ended longer than
    /// [`InboxLimits::history_body_ttl`] ago, keeping its summary. Live
    /// messages are never touched.
    fn trim_history(&mut self, now: Instant) {
        let ttl = self.limits.history_body_ttl;
        for m in self.messages.iter_mut().filter(|m| m.state.is_terminal()) {
            let ended = m.history.last().map_or(m.created_at, |(at, _)| *at);
            if !m.body.is_empty() && now.saturating_duration_since(ended) >= ttl {
                m.body = Arc::from("");
            }
        }
        let limit = self.limits.history_per_pane;
        let holder = |m: &Message| m.recipient.as_ref().map(|r| r.pane_id.clone());
        let mut excess: HashMap<Option<String>, usize> = HashMap::new();
        for m in self.messages.iter().filter(|m| m.state.is_terminal()) {
            *excess.entry(holder(m)).or_default() += 1;
        }
        if excess.values().any(|n| *n > limit) {
            for n in excess.values_mut() {
                *n = n.saturating_sub(limit);
            }
            self.messages.retain(|m| {
                if !m.state.is_terminal() {
                    return true;
                }
                match excess.get_mut(&holder(m)) {
                    Some(n) if *n > 0 => {
                        *n -= 1;
                        false
                    }
                    _ => true,
                }
            });
        }
        let (mut count, mut bytes) = self
            .messages
            .iter()
            .filter(|m| m.state.is_terminal())
            .fold((0usize, 0usize), |(n, b), m| (n + 1, b + m.body.len()));
        if count <= self.limits.max_history_messages && bytes <= self.limits.max_history_bytes {
            return;
        }
        let (max_count, max_bytes) = (
            self.limits.max_history_messages,
            self.limits.max_history_bytes,
        );
        self.messages.retain(|m| {
            if !m.state.is_terminal() || (count <= max_count && bytes <= max_bytes) {
                return true;
            }
            count -= 1;
            bytes -= m.body.len();
            false
        });
    }
}

/// Hardening M3: the most of a subject the inbox stores, in characters.
const MAX_STORED_SUBJECT_CHARS: usize = 200;

/// Hardening M3: the most of a preface the inbox stores, in bytes.
const MAX_STORED_PREFACE_BYTES: usize = 8 * 1024;

/// Hardening M3: the most of a sender's reason, role or orchestration name
/// the inbox stores, in bytes.
const MAX_STORED_LABEL_BYTES: usize = 256;

/// Hardening M3: a spill path longer than this is not a path any agent could
/// open, and is not stored.
const MAX_STORED_PATH_BYTES: usize = 4096;

/// The longest prefix of `text` within `max` bytes, cut at a char boundary.
fn cut_bytes(text: &str, max: usize) -> &str {
    let mut end = max.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// A subject as stored: one line, at most [`MAX_STORED_SUBJECT_CHARS`].
fn capped_subject(subject: &str) -> String {
    let one_line: String = subject
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    crate::untrusted_text::label_line(one_line.trim(), MAX_STORED_SUBJECT_CHARS)
}

/// A sender as stored, its free-text names bounded.
fn capped_sender(from: &Sender) -> Sender {
    let cap = |text: &str| cut_bytes(text, MAX_STORED_LABEL_BYTES).to_string();
    match from {
        Sender::Deck { reason } => Sender::Deck {
            reason: cap(reason),
        },
        Sender::Agent {
            pane_id,
            agent_id,
            role,
            orchestration,
        } => Sender::Agent {
            pane_id: pane_id.clone(),
            agent_id: agent_id.clone(),
            role: role.as_deref().map(cap),
            orchestration: orchestration.as_deref().map(cap),
        },
    }
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

// ---------------------------------------------------------------------------
// Scope and subscription resolution (pure functions over the daemon's facts)
// ---------------------------------------------------------------------------

/// The topics and pools configured for one project, built from
/// `.dot-agent-deck.toml` (`subscribe` on an orchestration and its roles,
/// `take_offers` on a role, `[dispatch] subscribe`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Subscriptions {
    /// By orchestration config name.
    pub orchestrations: HashMap<String, OrchestrationSubscriptions>,
    /// `[dispatch] subscribe`: what a dispatch caller pane receives.
    pub dispatch: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrchestrationSubscriptions {
    /// Topics every role of the orchestration receives.
    pub subscribe: Vec<String>,
    /// By role name.
    pub roles: HashMap<String, RoleSubscriptions>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoleSubscriptions {
    pub subscribe: Vec<String>,
    pub take_offers: Vec<String>,
}

/// What the daemon knows about one live pane, for scope resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneFacts {
    pub pane_id: String,
    pub agent_id: String,
    /// The orchestration instance the pane is a role of: its token, its
    /// config name, and the role.
    pub orchestration: Option<RoleFacts>,
    /// The pane that dispatched the unit this pane belongs to.
    pub dispatch_caller: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleFacts {
    pub token: String,
    pub name: String,
    pub role: String,
}

/// One pane in a resolved scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeMember {
    pub recipient: Recipient,
    pub orchestration_name: Option<String>,
    pub role: Option<String>,
    /// Some pane in the scope was dispatched by this one.
    pub is_dispatch_caller: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedScope {
    pub scope: Scope,
    /// In the order of the `panes` given to [`resolve_scope`].
    pub members: Vec<ScopeMember>,
}

/// The scope of a publish or offer from `sender_pane`: its orchestration
/// instance, widened to the whole dispatch tree when the sender is part of
/// one. `None` when the sender is in neither (or is not among `panes`).
pub fn resolve_scope(sender_pane: &str, panes: &[PaneFacts]) -> Option<ResolvedScope> {
    let sender = panes.iter().find(|p| p.pane_id == sender_pane)?;
    let callers: HashSet<&str> = panes
        .iter()
        .filter_map(|p| p.dispatch_caller.as_deref())
        .collect();
    let sender_group = Group::of(sender);
    // Inside a dispatch tree the scope is the whole tree; otherwise it is the
    // sender's orchestration instance.
    let tree_root = dispatch_root(&sender_group, panes).or_else(|| {
        // No caller above: the sender's group is the top of a tree when one of
        // its panes dispatched something.
        panes
            .iter()
            .find(|p| Group::of(p) == sender_group && callers.contains(p.pane_id.as_str()))
            .map(|p| p.pane_id.clone())
    });
    let (scope, in_scope): (Scope, Vec<bool>) = match tree_root {
        Some(root) => {
            let root_group = panes
                .iter()
                .find(|p| p.pane_id == root)
                .map(Group::of)
                .unwrap_or_else(|| Group::Pane(root.clone()));
            let in_scope = panes
                .iter()
                .map(|p| {
                    let group = Group::of(p);
                    group == root_group
                        || dispatch_root(&group, panes).as_deref() == Some(root.as_str())
                })
                .collect();
            (Scope::Dispatch { root_pane: root }, in_scope)
        }
        None => {
            let token = sender.orchestration.as_ref()?.token.clone();
            let in_scope = panes
                .iter()
                .map(|p| p.orchestration.as_ref().is_some_and(|o| o.token == token))
                .collect();
            (Scope::Orchestration { token }, in_scope)
        }
    };
    let members = panes
        .iter()
        .zip(in_scope)
        .filter(|(_, member)| *member)
        .map(|(p, _)| ScopeMember {
            recipient: Recipient::bound(p.pane_id.clone(), p.agent_id.clone()),
            orchestration_name: p.orchestration.as_ref().map(|o| o.name.clone()),
            role: p.orchestration.as_ref().map(|o| o.role.clone()),
            is_dispatch_caller: callers.contains(p.pane_id.as_str()),
        })
        .collect();
    Some(ResolvedScope { scope, members })
}

/// An orchestration instance, or a lone pane outside one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Group {
    Orchestration(String),
    Pane(String),
}

impl Group {
    fn of(pane: &PaneFacts) -> Self {
        match &pane.orchestration {
            Some(o) => Self::Orchestration(o.token.clone()),
            None => Self::Pane(pane.pane_id.clone()),
        }
    }
}

/// The root pane of the dispatch tree `group` sits in: follow the recorded
/// callers up until a group has none. `None` when `group` was not dispatched.
/// A cycle in the facts ends the walk rather than looping.
fn dispatch_root(group: &Group, panes: &[PaneFacts]) -> Option<String> {
    let mut seen = HashSet::new();
    let mut group = group.clone();
    let mut root = None;
    while seen.insert(group.clone()) {
        let Some(caller) = panes
            .iter()
            .filter(|p| Group::of(p) == group)
            .find_map(|p| p.dispatch_caller.clone())
        else {
            break;
        };
        group = match panes.iter().find(|p| p.pane_id == caller) {
            Some(caller_pane) => Group::of(caller_pane),
            None => Group::Pane(caller.clone()),
        };
        root = Some(caller);
    }
    root
}

/// Everyone in `members` subscribed to `topic`, except the sender: through
/// their orchestration, their role, or (for a dispatch caller) `[dispatch]`.
pub fn publish_recipients(
    members: &[ScopeMember],
    subscriptions: &Subscriptions,
    topic: &str,
    sender_pane: &str,
) -> Vec<Recipient> {
    let has = |topics: &[String]| topics.iter().any(|t| t == topic);
    members
        .iter()
        .filter(|m| m.recipient.pane_id != sender_pane)
        .filter(|m| {
            let orchestration = m
                .orchestration_name
                .as_ref()
                .and_then(|name| subscriptions.orchestrations.get(name));
            let via_orchestration = orchestration.is_some_and(|o| has(&o.subscribe));
            let via_role = orchestration
                .zip(m.role.as_ref())
                .and_then(|(o, role)| o.roles.get(role))
                .is_some_and(|r| has(&r.subscribe));
            let via_dispatch = m.is_dispatch_caller && has(&subscriptions.dispatch);
            via_orchestration || via_role || via_dispatch
        })
        .map(|m| m.recipient.clone())
        .collect()
}

/// Everyone in `members` whose role takes offers from `pool`, except the
/// sender and anyone in `exclude`, in member order.
pub fn offer_candidates(
    members: &[ScopeMember],
    subscriptions: &Subscriptions,
    pool: &str,
    sender_pane: &str,
    exclude: &[Recipient],
) -> Vec<Recipient> {
    members
        .iter()
        .filter(|m| m.recipient.pane_id != sender_pane)
        .filter(|m| !exclude.iter().any(|e| e.same_holder(&m.recipient)))
        .filter(|m| {
            m.orchestration_name
                .as_ref()
                .and_then(|name| subscriptions.orchestrations.get(name))
                .zip(m.role.as_ref())
                .and_then(|(o, role)| o.roles.get(role))
                .is_some_and(|r| r.take_offers.iter().any(|p| p == pool))
        })
        .map(|m| m.recipient.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use spec::spec;

    const SECOND: Duration = Duration::from_secs(1);

    fn deck() -> DeckId {
        DeckId::parse("k3xzq2ab").unwrap()
    }

    fn inbox_with(limits: InboxLimits) -> Inbox {
        Inbox::new(deck(), limits)
    }

    fn inbox() -> Inbox {
        inbox_with(InboxLimits::default())
    }

    fn from_deck() -> Content {
        Content {
            from: Sender::Deck {
                reason: "test".into(),
            },
            subject: "task".into(),
            body: "do the thing".into(),
            body_trust: BodyTrust::Trusted,
            spill_path: None,
            id: None,
            preface: None,
        }
    }

    fn from_agent(pane: &str, agent: &str) -> Content {
        Content {
            from: Sender::Agent {
                pane_id: pane.into(),
                agent_id: agent.into(),
                role: Some("orchestrator".into()),
                orchestration: Some("tdd".into()),
            },
            body_trust: BodyTrust::Untrusted,
            ..from_deck()
        }
    }

    fn worker() -> Recipient {
        Recipient::bound("2", "agent-w1")
    }

    fn state_of(inbox: &Inbox, id: &MessageId, pane: &str) -> StateName {
        inbox
            .get(id.as_str(), pane)
            .expect("message held")
            .state
            .name()
    }

    /// Scenario: Mint a thousand message ids on one deck and check each
    /// against the `m-<deck>-<16 hex>` form; feed the validator old `d-…`
    /// delivery ids and malformed variants. Every minted id is valid and
    /// distinct, carries its deck, and nothing else passes.
    #[spec("inbox/core/001")]
    #[test]
    fn inbox_core_001_message_ids_are_well_formed_and_unique() {
        let deck = deck();
        let mut seen = HashSet::new();
        for _ in 0..1000 {
            let id = MessageId::generate(&deck);
            assert!(is_valid_message_id(id.as_str()), "{id}");
            assert_eq!(id.deck(), deck.as_str());
            assert!(seen.insert(id), "a minted id repeated");
        }
        assert!(is_valid_message_id("m-k3xzq2ab-0f3c9a71d2e4b850"));
        for bad in [
            "d-0f3c9a71",
            "m-k3xzq2ab-0f3c9a71d2e4b85",
            "m-k3xzq2ab-0f3c9a71d2e4b8500",
            "m-k3xzq2ab-0F3C9A71D2E4B850",
            "m-K3XZQ2AB-0f3c9a71d2e4b850",
            "m-k3x9q2ab-0f3c9a71d2e4b850",
            "m-k3xzq2a-0f3c9a71d2e4b850",
            "x-k3xzq2ab-0f3c9a71d2e4b850",
            "m-k3xzq2ab_0f3c9a71d2e4b850",
            "",
        ] {
            assert!(!is_valid_message_id(bad), "{bad:?}");
            assert!(MessageId::parse(bad).is_none());
        }
        let id = MessageId::parse("m-k3xzq2ab-0f3c9a71d2e4b850").unwrap();
        assert_eq!(
            serde_json::to_string(&id).unwrap(),
            "\"m-k3xzq2ab-0f3c9a71d2e4b850\""
        );
        assert!(serde_json::from_str::<MessageId>("\"d-0f3c9a71\"").is_err());
    }

    /// Scenario: Post a message under a sender-chosen id, then post the same
    /// id again inside and after the dedup window, and overflow the window's
    /// id count. A repeat inside the window is refused as a duplicate naming
    /// the first one's state and enqueues nothing; once the window has passed
    /// and the first message has left the history, the id is accepted again.
    #[spec("inbox/core/003")]
    #[test]
    fn inbox_core_003_dedup_window_refuses_a_repeated_id() {
        let t0 = Instant::now();
        let mut inbox = inbox_with(InboxLimits {
            history_per_pane: 1,
            dedup_max_ids: 3,
            ..InboxLimits::default()
        });
        let chosen = MessageId::parse("m-k3xzq2ab-00000000000000aa").unwrap();
        let first = Content {
            id: Some(chosen.clone()),
            ..from_deck()
        };
        assert_eq!(
            inbox.post_direct(t0, worker(), first.clone()).unwrap(),
            chosen
        );
        assert_eq!(
            inbox.post_direct(t0 + SECOND, worker(), first.clone()),
            Err(PostError::Duplicate {
                id: chosen.clone(),
                existing_state: Some(StateName::Queued),
            })
        );
        assert_eq!(inbox.list("2").len(), 1, "the duplicate enqueued nothing");

        inbox.ack(
            t0 + SECOND,
            chosen.as_str(),
            "2",
            "agent-w1",
            AckHow::Explicit,
        );
        assert_eq!(
            inbox.post_direct(t0 + 2 * SECOND, worker(), first.clone()),
            Err(PostError::Duplicate {
                id: chosen.clone(),
                existing_state: Some(StateName::Acknowledged),
            })
        );
        // A newer acknowledged message pushes the first out of the one-deep
        // history; the dedup window still remembers its id and state.
        let newer = inbox
            .post_direct(t0 + 2 * SECOND, worker(), from_deck())
            .unwrap();
        inbox.ack(
            t0 + 2 * SECOND,
            newer.as_str(),
            "2",
            "agent-w1",
            AckHow::Explicit,
        );
        assert!(inbox.get(chosen.as_str(), "2").is_none());
        assert!(matches!(
            inbox.post_direct(t0 + 3 * SECOND, worker(), first.clone()),
            Err(PostError::Duplicate {
                existing_state: Some(StateName::Acknowledged),
                ..
            })
        ));
        // After the window the id is free again.
        let later = t0 + DEFAULT_DEDUP_WINDOW + SECOND;
        assert_eq!(
            inbox.post_direct(later, worker(), first.clone()).unwrap(),
            chosen
        );

        // The id-count bound forgets the oldest id early.
        let mut inbox = inbox_with(InboxLimits {
            history_per_pane: 1,
            dedup_max_ids: 2,
            ..InboxLimits::default()
        });
        assert_eq!(
            inbox.post_direct(t0, worker(), first.clone()).unwrap(),
            chosen
        );
        inbox.ack(t0, chosen.as_str(), "2", "agent-w1", AckHow::Explicit);
        for _ in 0..2 {
            let id = inbox.post_direct(t0, worker(), from_deck()).unwrap();
            inbox.ack(t0, id.as_str(), "2", "agent-w1", AckHow::Explicit);
        }
        assert_eq!(inbox.post_direct(t0, worker(), first).unwrap(), chosen);
    }

    /// Scenario: Fetch a message and let its ack-wait pass without an ack,
    /// with the ack-wait set through `DOT_AGENT_DECK_INBOX_ACK_WAIT_MS`. It
    /// stays fetched until the deadline, returns to queued at it with its
    /// delivery count kept, and is then fetched again.
    #[spec("inbox/core/004")]
    #[test]
    fn inbox_core_004_unacknowledged_fetch_is_redelivered_after_ack_wait() {
        let t0 = Instant::now();
        let limits =
            InboxLimits::from_lookup(|name| (name == ENV_ACK_WAIT_MS).then(|| "5000".to_string()));
        assert_eq!(limits.ack_wait, Duration::from_secs(5));
        let mut inbox = inbox_with(limits);
        let id = inbox.post_direct(t0, worker(), from_deck()).unwrap();
        let fetched = inbox.fetch(t0, "2", "agent-w1", Consumer::FetchCli, None);
        assert_eq!(fetched.len(), 1);
        assert_eq!(inbox.next_deadline(), Some(t0 + Duration::from_secs(5)));

        inbox.tick(t0 + Duration::from_millis(4999));
        assert_eq!(state_of(&inbox, &id, "2"), StateName::Fetched);
        inbox.tick(t0 + Duration::from_secs(5));
        assert_eq!(state_of(&inbox, &id, "2"), StateName::Queued);
        assert_eq!(inbox.get(id.as_str(), "2").unwrap().deliveries, 1);
        assert_eq!(inbox.next_deadline(), None);

        let again = inbox.fetch(
            t0 + Duration::from_secs(6),
            "2",
            "agent-w1",
            Consumer::FetchCli,
            None,
        );
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].deliveries, 2);
    }

    /// Scenario: Fetch a message and let every ack-wait expire, five times.
    /// After the fifth unacknowledged hand-over it becomes undeliverable for
    /// `max_deliver`, an event says so, and no further fetch returns it.
    #[spec("inbox/core/005")]
    #[test]
    fn inbox_core_005_max_deliver_makes_a_message_undeliverable() {
        let t0 = Instant::now();
        let mut inbox = inbox();
        let id = inbox.post_direct(t0, worker(), from_deck()).unwrap();
        let mut now = t0;
        for round in 1..=DEFAULT_MAX_DELIVER {
            let fetched = inbox.fetch(now, "2", "agent-w1", Consumer::Waiter { conn_id: 7 }, None);
            assert_eq!(fetched.len(), 1, "round {round}");
            now += DEFAULT_ACK_WAIT;
            inbox.tick(now);
        }
        let message = inbox.get(id.as_str(), "2").unwrap();
        assert_eq!(
            message.state,
            MessageState::Undeliverable {
                at: now,
                reason: UndeliverableReason::MaxDeliver,
            }
        );
        assert_eq!(message.deliveries, DEFAULT_MAX_DELIVER);
        let events = inbox.drain_events();
        let last = events.last().unwrap();
        assert_eq!(last.state, StateName::Undeliverable);
        assert_eq!(
            last.undeliverable_reason,
            Some(UndeliverableReason::MaxDeliver)
        );
        assert!(
            inbox
                .fetch(now, "2", "agent-w1", Consumer::FetchCli, None)
                .is_empty()
        );
        assert_eq!(inbox.counts("2").undeliverable, 1);
    }

    /// Scenario: A streaming consumer keeps a fetched message queued behind a
    /// long turn and answers each redelivery with `hold`. Every redelivery's
    /// hand-over is refunded, so the message outlives `max_deliver` ack-waits
    /// while the consumer holds it; a hold from another consumer, or for a
    /// queued message, refunds nothing; past the hold bound the redeliveries
    /// count again and it becomes undeliverable at `max_deliver`.
    #[spec("inbox/core/018")]
    #[test]
    fn inbox_core_018_a_held_redelivery_is_refunded_until_the_bound() {
        let t0 = Instant::now();
        let ack_wait = Duration::from_secs(1);
        let limits = InboxLimits {
            ack_wait,
            max_deliver: 2,
            hold_bound: Duration::from_secs(30),
            ..InboxLimits::default()
        };
        let mut inbox = inbox_with(limits);
        let id = inbox.post_direct(t0, worker(), from_deck()).unwrap();
        let stream = Consumer::Stream { conn_id: 9 };
        let mut now = t0;
        assert_eq!(
            inbox
                .fetch(now, "2", "agent-w1", stream.clone(), None)
                .len(),
            1
        );
        // A first hand-over held is never refunded below one.
        assert!(inbox.hold(now, id.as_str(), "2", "agent-w1", &stream));
        assert_eq!(inbox.get(id.as_str(), "2").unwrap().deliveries, 1);
        // Ten ack-waits, five times `max_deliver`, each redelivery held.
        for round in 0..10 {
            now += ack_wait;
            inbox.tick(now);
            assert_eq!(
                state_of(&inbox, &id, "2"),
                StateName::Queued,
                "round {round}"
            );
            assert_eq!(
                inbox
                    .fetch(now, "2", "agent-w1", stream.clone(), None)
                    .len(),
                1,
                "round {round}"
            );
            assert_eq!(inbox.get(id.as_str(), "2").unwrap().deliveries, 2);
            assert!(
                !inbox.hold(
                    now,
                    id.as_str(),
                    "2",
                    "agent-w1",
                    &Consumer::Stream { conn_id: 10 }
                ),
                "another consumer's hold refunds nothing"
            );
            assert!(
                !inbox.hold(now, id.as_str(), "2", "agent-other", &stream),
                "another agent's hold refunds nothing"
            );
            assert!(inbox.hold(now, id.as_str(), "2", "agent-w1", &stream));
            assert_eq!(inbox.get(id.as_str(), "2").unwrap().deliveries, 1);
        }
        now += ack_wait;
        inbox.tick(now);
        assert!(
            !inbox.hold(now, id.as_str(), "2", "agent-w1", &stream),
            "a queued message is not held"
        );
        // Past the bound a hold no longer refunds, and max_deliver applies.
        now = t0 + Duration::from_secs(30);
        inbox.fetch(now, "2", "agent-w1", stream.clone(), None);
        assert!(!inbox.hold(now, id.as_str(), "2", "agent-w1", &stream));
        assert_eq!(inbox.get(id.as_str(), "2").unwrap().deliveries, 2);
        now += ack_wait;
        inbox.tick(now);
        assert_eq!(
            inbox.get(id.as_str(), "2").unwrap().state,
            MessageState::Undeliverable {
                at: now,
                reason: UndeliverableReason::MaxDeliver,
            }
        );
        assert_eq!(DEFAULT_HOLD_BOUND, Duration::from_secs(60 * 60));
        assert_eq!(InboxLimits::default().hold_bound, DEFAULT_HOLD_BOUND);
    }

    /// Scenario: An offer assigned to a worker whose stream fetched it is not
    /// claimed within the ack-wait. At each ack-wait the offer is handed to
    /// the same stream again rather than back to its pool, for as long as the
    /// stream answers with `hold`: it is never reassigned and its assignment
    /// count does not move, and the holder's ack still claims it. Once the
    /// claim TTL or the hold bound has passed since that assignment's first
    /// fetch, or a redelivery goes unanswered, it returns to its pool as it
    /// does for a consumer that cannot hold.
    #[spec("inbox/core/020")]
    #[test]
    fn inbox_core_020_a_held_offer_stays_with_its_holder_until_the_bound() {
        let t0 = Instant::now();
        let ack_wait = Duration::from_secs(10);
        let claim_ttl = Duration::from_secs(60);
        let alice = Recipient::bound("5", "agent-a");
        let bob = Recipient::bound("6", "agent-b");
        let stream = Consumer::Stream { conn_id: 9 };
        let offer = |inbox: &mut Inbox, now| {
            let offered = inbox
                .offer(
                    now,
                    scope(),
                    "review",
                    Some(claim_ttl),
                    from_agent("1", "agent-o"),
                    &[alice.clone(), bob.clone()],
                )
                .unwrap();
            assert_eq!(offered.offered_to, Some(alice.clone()));
            offered.id
        };
        // Held through every ack-wait: re-handed to the same stream, held, and
        // finally claimed by its holder, never by anyone else.
        let held_rounds = |inbox: &mut Inbox, id: &MessageId, from: Instant, rounds: u32| {
            let mut now = from;
            for round in 0..rounds {
                now += ack_wait;
                inbox.tick(now);
                assert!(inbox.unassigned_offers().is_empty(), "round {round}");
                let message = inbox.get(id.as_str(), "5").unwrap();
                assert_eq!(message.recipient, Some(alice.clone()), "round {round}");
                assert_eq!(message.deliveries, 1, "round {round}");
                assert_eq!(
                    inbox.fetch(now, "5", "agent-a", stream.clone(), None).len(),
                    1,
                    "the holder's stream is handed the offer again (round {round})"
                );
                assert!(
                    inbox
                        .fetch(now, "6", "agent-b", Consumer::FetchCli, None)
                        .is_empty()
                );
                assert!(inbox.hold(now, id.as_str(), "5", "agent-a", &stream));
            }
            now
        };
        let limits = InboxLimits {
            ack_wait,
            ..InboxLimits::default()
        };
        let mut inbox = inbox_with(limits.clone());
        let id = offer(&mut inbox, t0);
        inbox.fetch(t0, "5", "agent-a", stream.clone(), None);
        let now = held_rounds(&mut inbox, &id, t0, 4);
        assert_eq!(
            inbox.ack(now, id.as_str(), "5", "agent-a", AckHow::PluginConfirmed),
            AckOutcome::Claimed {
                expires_in_ms: millis(claim_ttl)
            }
        );

        // Past the claim TTL since the first fetch, the next ack-wait returns
        // it to the pool, excluding its holder.
        let mut inbox = inbox_with(limits.clone());
        let id = offer(&mut inbox, t0);
        inbox.fetch(t0, "5", "agent-a", stream.clone(), None);
        let now = held_rounds(&mut inbox, &id, t0, 5);
        inbox.tick(now + ack_wait);
        assert_eq!(inbox.unassigned_offers(), vec![id.clone()]);
        assert_eq!(
            inbox.assign_offer(now + ack_wait, &id, &[alice.clone(), bob.clone()]),
            Some(bob.clone())
        );

        // A hold bound shorter than the claim TTL ends it first.
        let mut inbox = inbox_with(InboxLimits {
            hold_bound: Duration::from_secs(30),
            ..limits.clone()
        });
        let id = offer(&mut inbox, t0);
        inbox.fetch(t0, "5", "agent-a", stream.clone(), None);
        let now = held_rounds(&mut inbox, &id, t0, 2);
        inbox.tick(now + ack_wait);
        assert_eq!(inbox.unassigned_offers(), vec![id.clone()]);
        assert!(
            !inbox.hold(now + ack_wait, id.as_str(), "5", "agent-a", &stream),
            "an offer back in its pool is not held"
        );

        // A redelivery the stream does not answer with hold ends it.
        let mut inbox = inbox_with(limits.clone());
        let id = offer(&mut inbox, t0);
        inbox.fetch(t0, "5", "agent-a", stream.clone(), None);
        inbox.tick(t0 + ack_wait);
        assert!(inbox.unassigned_offers().is_empty());
        inbox.fetch(t0 + ack_wait, "5", "agent-a", stream.clone(), None);
        inbox.tick(t0 + 2 * ack_wait);
        assert_eq!(inbox.unassigned_offers(), vec![id.clone()]);

        // A consumer that cannot hold — a once waiter — loses it at the first
        // ack-wait, as before.
        let mut inbox = inbox_with(limits);
        let id = offer(&mut inbox, t0);
        inbox.fetch(t0, "5", "agent-a", Consumer::Waiter { conn_id: 3 }, None);
        inbox.tick(t0 + ack_wait);
        assert_eq!(inbox.unassigned_offers(), vec![id]);
    }

    /// Scenario: A message runs out of hand-overs and becomes undeliverable,
    /// then its agent acknowledges it. The late ack is recorded: the message
    /// is acknowledged and the answer says so. A message undeliverable for
    /// any other reason still answers undeliverable, as does an offer that
    /// ran out of hand-overs.
    #[spec("inbox/core/019")]
    #[test]
    fn inbox_core_019_a_late_ack_after_max_deliver_is_recorded() {
        let t0 = Instant::now();
        let limits = InboxLimits {
            max_deliver: 1,
            ..InboxLimits::default()
        };
        let mut inbox = inbox_with(limits);
        let id = inbox.post_direct(t0, worker(), from_deck()).unwrap();
        inbox.fetch(t0, "2", "agent-w1", Consumer::Waiter { conn_id: 3 }, None);
        let now = t0 + DEFAULT_ACK_WAIT;
        inbox.tick(now);
        assert_eq!(state_of(&inbox, &id, "2"), StateName::Undeliverable);
        inbox.drain_events();
        assert_eq!(
            inbox.ack(now, id.as_str(), "2", "agent-other", AckHow::Explicit),
            AckOutcome::WrongAgent
        );
        assert_eq!(
            inbox.ack(now, id.as_str(), "2", "agent-w1", AckHow::PromptObserved),
            AckOutcome::Acknowledged
        );
        assert_eq!(
            inbox.get(id.as_str(), "2").unwrap().state,
            MessageState::Acknowledged {
                at: now,
                how: AckHow::PromptObserved,
            }
        );
        let events = inbox.drain_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].state, StateName::Acknowledged);
        assert_eq!(
            inbox.ack(now, id.as_str(), "2", "agent-w1", AckHow::Explicit),
            AckOutcome::AlreadyAcknowledged
        );

        let exited = inbox.post_direct(now, worker(), from_deck()).unwrap();
        inbox.agent_exited(now, "2", "agent-w1");
        assert_eq!(
            inbox.ack(now, exited.as_str(), "2", "agent-w1", AckHow::Explicit),
            AckOutcome::Undeliverable
        );

        let offered = inbox
            .offer(
                now,
                scope(),
                "pool",
                None,
                from_agent("1", "agent-o"),
                &[Recipient::bound("2", "agent-w1")],
            )
            .unwrap();
        inbox.fetch(now, "2", "agent-w1", Consumer::FetchCli, None);
        let later = now + DEFAULT_ACK_WAIT;
        inbox.tick(later);
        assert_ne!(
            inbox.ack(
                later,
                offered.id.as_str(),
                "2",
                "agent-w1",
                AckHow::Explicit
            ),
            AckOutcome::Acknowledged,
            "an offer that ran out of hand-overs was told \"not taken\" and stays so"
        );
        let offer = inbox.all().iter().find(|m| m.id == offered.id).unwrap();
        assert_eq!(offer.state.name(), StateName::Undeliverable);
    }

    /// Scenario: Fill one recipient's inbox, then the whole deck, then post an
    /// oversized body. Each post past a bound is refused with an explicit
    /// error (`InboxFull`, `DeckFull`, `BodyTooLarge`) and every message
    /// already queued is still there.
    #[spec("inbox/core/006")]
    #[test]
    fn inbox_core_006_bounds_refuse_explicitly_and_drop_nothing() {
        let t0 = Instant::now();
        let mut inbox = inbox_with(InboxLimits {
            max_per_recipient: 2,
            max_messages: 3,
            max_body_bytes: 16,
            ..InboxLimits::default()
        });
        let a = inbox.post_direct(t0, worker(), from_deck()).unwrap();
        let b = inbox.post_direct(t0, worker(), from_deck()).unwrap();
        assert_eq!(
            inbox.post_direct(t0, worker(), from_deck()),
            Err(PostError::InboxFull {
                pane_id: "2".into(),
                limit: 2,
            })
        );
        let ids: Vec<_> = inbox.list("2").iter().map(|m| m.id.clone()).collect();
        assert_eq!(ids, vec![a.clone(), b]);

        // A refused post burns no id: the same chosen id posts once room exists.
        let chosen = MessageId::parse("m-k3xzq2ab-00000000000000bb").unwrap();
        let with_id = Content {
            id: Some(chosen.clone()),
            ..from_deck()
        };
        assert!(matches!(
            inbox.post_direct(t0, worker(), with_id.clone()),
            Err(PostError::InboxFull { .. })
        ));
        inbox.ack(t0, a.as_str(), "2", "agent-w1", AckHow::Explicit);
        assert_eq!(inbox.post_direct(t0, worker(), with_id).unwrap(), chosen);

        inbox
            .post_direct(t0, Recipient::bound("3", "agent-x"), from_deck())
            .unwrap();
        assert_eq!(
            inbox.post_direct(t0, Recipient::bound("4", "agent-y"), from_deck()),
            Err(PostError::DeckFull { limit: 3 })
        );
        assert_eq!(inbox.live_count(), 3);

        let big = Content {
            body: "x".repeat(17),
            ..from_deck()
        };
        assert_eq!(
            inbox.post_direct(t0, Recipient::bound("3", "agent-x"), big),
            Err(PostError::BodyTooLarge {
                bytes: 17,
                limit: 16
            })
        );
        assert_eq!(InboxLimits::default().max_body_bytes, 1024 * 1024);
    }

    /// Scenario: Two consumers fetch the same pane's inbox one after the
    /// other, and a third consumer's connection drops after it fetched. The
    /// first fetch takes the message; the second gets nothing while it is
    /// fetched; a dropped consumer's messages return to the queue.
    #[spec("inbox/core/007")]
    #[test]
    fn inbox_core_007_a_fetched_message_is_handed_to_one_consumer() {
        let t0 = Instant::now();
        let mut inbox = inbox();
        let id = inbox.post_direct(t0, worker(), from_deck()).unwrap();
        let waiter = Consumer::Waiter { conn_id: 1 };
        assert_eq!(
            inbox.fetch(t0, "2", "agent-w1", waiter.clone(), None).len(),
            1
        );
        assert!(
            inbox
                .fetch(t0, "2", "agent-w1", Consumer::FetchCli, None)
                .is_empty()
        );
        assert!(
            inbox
                .fetch(t0, "2", "agent-w1", Consumer::Stream { conn_id: 2 }, None)
                .is_empty()
        );

        assert_eq!(
            inbox.requeue_consumer(t0 + SECOND, &Consumer::Stream { conn_id: 2 }),
            0
        );
        assert_eq!(inbox.requeue_consumer(t0 + SECOND, &waiter), 1);
        assert_eq!(state_of(&inbox, &id, "2"), StateName::Queued);
        assert_eq!(
            inbox
                .fetch(t0 + SECOND, "2", "agent-w1", Consumer::FetchCli, None)
                .len(),
            1
        );

        // A message bound to another generation of the pane is not handed to
        // this one; an unbound message is.
        inbox
            .post_direct(t0, Recipient::bound("2", "agent-w0"), from_deck())
            .unwrap();
        inbox
            .post_direct(t0, Recipient::unbound("2"), from_deck())
            .unwrap();
        let got = inbox.fetch(t0, "2", "agent-w1", Consumer::FetchCli, None);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].recipient, Some(Recipient::unbound("2")));
    }

    /// Scenario: Acknowledge messages as their own agent, as another agent,
    /// twice, after they turned undeliverable, and with an old `d-…` id. Each
    /// answer is explicit: acknowledged, wrong agent (state unchanged),
    /// already acknowledged, undeliverable, unknown; fetch-and-ack does both
    /// in one step.
    #[spec("inbox/core/008")]
    #[test]
    fn inbox_core_008_ack_outcomes_are_explicit() {
        let t0 = Instant::now();
        let mut inbox = inbox();
        let id = inbox.post_direct(t0, worker(), from_deck()).unwrap();
        assert_eq!(
            inbox.ack(t0, id.as_str(), "3", "agent-other", AckHow::Explicit),
            AckOutcome::WrongAgent
        );
        assert_eq!(
            inbox.ack(t0, id.as_str(), "2", "agent-w0", AckHow::Explicit),
            AckOutcome::WrongAgent
        );
        assert_eq!(state_of(&inbox, &id, "2"), StateName::Queued);
        inbox.fetch(t0, "2", "agent-w1", Consumer::DoorbellHook, None);
        assert_eq!(
            inbox.ack(t0, id.as_str(), "2", "agent-w1", AckHow::PromptObserved),
            AckOutcome::Acknowledged
        );
        assert_eq!(
            inbox.ack(t0, id.as_str(), "2", "agent-w1", AckHow::PromptObserved),
            AckOutcome::AlreadyAcknowledged
        );
        assert_eq!(inbox.counts("2").last_acked_at, Some(t0));
        assert_eq!(
            inbox.ack(t0, "d-0f3c9a71", "2", "agent-w1", AckHow::Explicit),
            AckOutcome::Unknown
        );
        assert_eq!(
            inbox.ack(
                t0,
                "m-k3xzq2ab-0000000000000000",
                "2",
                "agent-w1",
                AckHow::Explicit
            ),
            AckOutcome::Unknown
        );

        let gone = inbox.post_direct(t0, worker(), from_deck()).unwrap();
        assert!(inbox.retire(t0, gone.as_str(), "2", UndeliverableReason::Superseded));
        assert_eq!(
            inbox.ack(t0, gone.as_str(), "2", "agent-w1", AckHow::Explicit),
            AckOutcome::Undeliverable
        );

        let a = inbox.post_direct(t0, worker(), from_deck()).unwrap();
        let taken = inbox.fetch_acked(
            t0,
            "2",
            "agent-w1",
            Consumer::FetchCli,
            AckHow::FetchedByCli,
            None,
        );
        assert_eq!(taken.len(), 1);
        assert_eq!(state_of(&inbox, &a, "2"), StateName::Acknowledged);
    }

    fn scope() -> Scope {
        Scope::Orchestration {
            token: "tok-1".into(),
        }
    }

    /// Scenario: Offer a job to a pool with two eligible candidates, then try
    /// to fetch it from both. Only the first candidate is offered it and can
    /// fetch it; its ack claims the offer for the claim TTL and completing it
    /// acknowledges it.
    #[spec("inbox/core/009")]
    #[test]
    fn inbox_core_009_an_offer_goes_to_exactly_one_taker() {
        let t0 = Instant::now();
        let mut inbox = inbox();
        let alice = Recipient::bound("5", "agent-a");
        let bob = Recipient::bound("6", "agent-b");
        let offered = inbox
            .offer(
                t0,
                scope(),
                "review",
                Some(Duration::from_secs(120)),
                from_agent("1", "agent-o"),
                &[alice.clone(), bob.clone()],
            )
            .unwrap();
        assert_eq!(offered.offered_to, Some(alice.clone()));
        assert!(
            inbox
                .fetch(t0, "6", "agent-b", Consumer::FetchCli, None)
                .is_empty()
        );
        assert_eq!(
            inbox
                .fetch(t0, "5", "agent-a", Consumer::FetchCli, None)
                .len(),
            1
        );
        assert_eq!(
            inbox.ack(t0, offered.id.as_str(), "6", "agent-b", AckHow::Explicit),
            AckOutcome::WrongAgent
        );
        assert_eq!(
            inbox.ack(t0, offered.id.as_str(), "5", "agent-a", AckHow::Explicit),
            AckOutcome::Claimed {
                expires_in_ms: 120_000
            }
        );
        assert_eq!(state_of(&inbox, &offered.id, "5"), StateName::Claimed);
        assert_eq!(
            inbox.complete(t0, offered.id.as_str(), "6", "agent-b"),
            ClaimOutcome::NotClaimant
        );
        assert_eq!(
            inbox.complete(t0 + SECOND, offered.id.as_str(), "5", "agent-a"),
            ClaimOutcome::Completed
        );
        assert_eq!(state_of(&inbox, &offered.id, "5"), StateName::Acknowledged);
        assert_eq!(
            inbox.offer(
                t0,
                scope(),
                "review",
                Some(Duration::from_secs(1)),
                from_agent("1", "agent-o"),
                &[]
            ),
            Err(PostError::ClaimTtlOutOfRange {
                min_ms: 60_000,
                max_ms: 86_400_000,
            })
        );
    }

    /// PR #1735 review: a claim settled after it expired, but before the
    /// ticker returned the offer, is not honoured. Completing or releasing it
    /// then returns the offer to its pool, as the ticker would have.
    #[test]
    fn a_claim_settled_after_its_expiry_returns_the_offer_to_its_pool() {
        for complete in [true, false] {
            let t0 = Instant::now();
            let mut inbox = inbox();
            let alice = Recipient::bound("5", "agent-a");
            let ttl = Duration::from_secs(120);
            let offered = inbox
                .offer(
                    t0,
                    scope(),
                    "review",
                    Some(ttl),
                    from_agent("1", "agent-o"),
                    std::slice::from_ref(&alice),
                )
                .unwrap();
            inbox.fetch(t0, "5", "agent-a", Consumer::FetchCli, None);
            inbox.ack(t0, offered.id.as_str(), "5", "agent-a", AckHow::Explicit);
            let late = t0 + ttl + SECOND;
            let outcome = if complete {
                inbox.complete(late, offered.id.as_str(), "5", "agent-a")
            } else {
                inbox.release(late, offered.id.as_str(), "5", "agent-a")
            };
            assert_eq!(outcome, ClaimOutcome::Expired, "complete={complete}");
            assert!(
                inbox.unassigned_offers().contains(&offered.id),
                "the expired claim's offer is back in its pool (complete={complete})"
            );
        }
    }

    /// Scenario: Let a claimed offer's claim expire, then release the next
    /// claim. Each time the offer returns to its pool and is re-offered to a
    /// candidate that has not held it, never to a previous claimant; with only
    /// previous claimants left it waits unassigned.
    #[spec("inbox/core/010")]
    #[test]
    fn inbox_core_010_claim_expiry_reoffers_excluding_previous_claimants() {
        let t0 = Instant::now();
        let mut inbox = inbox();
        let alice = Recipient::bound("5", "agent-a");
        let bob = Recipient::bound("6", "agent-b");
        let carol = Recipient::bound("7", "agent-c");
        let all = [alice.clone(), bob.clone(), carol.clone()];
        let ttl = Duration::from_secs(60);
        let offered = inbox
            .offer(
                t0,
                scope(),
                "review",
                Some(ttl),
                from_agent("1", "agent-o"),
                &all,
            )
            .unwrap();
        let id = offered.id;
        inbox.ack(t0, id.as_str(), "5", "agent-a", AckHow::Explicit);
        assert_eq!(inbox.next_deadline(), Some(t0 + ttl));

        inbox.tick(t0 + ttl);
        assert_eq!(inbox.unassigned_offers(), vec![id.clone()]);
        assert_eq!(inbox.assign_offer(t0 + ttl, &id, &all), Some(bob.clone()));

        inbox.ack(t0 + ttl, id.as_str(), "6", "agent-b", AckHow::Explicit);
        assert_eq!(
            inbox.release(t0 + ttl, id.as_str(), "6", "agent-b"),
            ClaimOutcome::Released
        );
        assert_eq!(inbox.assign_offer(t0 + ttl, &id, &all), Some(carol));

        // Carol's fetch is never acknowledged: the offer leaves her too.
        inbox.fetch(t0 + ttl, "7", "agent-c", Consumer::FetchCli, None);
        inbox.tick(t0 + ttl + DEFAULT_ACK_WAIT);
        assert_eq!(inbox.assign_offer(t0 + 2 * ttl, &id, &all), None);
        assert_eq!(inbox.unassigned_offers(), vec![id.clone()]);
        let excluded = &inbox.get(id.as_str(), "").unwrap().excluded;
        assert_eq!(excluded.len(), 3);

        // The candidate list helper drops excluded holders too.
        let members: Vec<ScopeMember> = all
            .iter()
            .map(|r| ScopeMember {
                recipient: r.clone(),
                orchestration_name: Some("tdd".into()),
                role: Some("reviewer".into()),
                is_dispatch_caller: false,
            })
            .collect();
        let subs = subscriptions();
        assert_eq!(
            offer_candidates(&members, &subs, "review", "1", &[alice.clone(), bob]),
            vec![Recipient::bound("7", "agent-c")]
        );
    }

    /// Scenario: A worker with a bound message, an unbound message and a
    /// claimed offer exits. The bound message becomes undeliverable because
    /// the agent exited, the unbound one waits for the pane's next agent, and
    /// the offer returns to its pool excluding the exited agent.
    #[spec("inbox/core/011")]
    #[test]
    fn inbox_core_011_an_exited_agent_leaves_its_bound_messages_undeliverable() {
        let t0 = Instant::now();
        let mut inbox = inbox();
        let bound = inbox.post_direct(t0, worker(), from_deck()).unwrap();
        let fetched = inbox.post_direct(t0, worker(), from_deck()).unwrap();
        inbox.fetch(t0, "2", "agent-w1", Consumer::FetchCli, Some(1));
        let unbound = inbox
            .post_direct(t0, Recipient::unbound("2"), from_deck())
            .unwrap();
        let offer = inbox
            .offer(t0, scope(), "review", None, from_deck(), &[worker()])
            .unwrap()
            .id;
        inbox.ack(t0, offer.as_str(), "2", "agent-w1", AckHow::Explicit);

        inbox.agent_exited(t0 + SECOND, "2", "agent-w1");
        for id in [&bound, &fetched] {
            assert_eq!(
                inbox.get(id.as_str(), "2").unwrap().state,
                MessageState::Undeliverable {
                    at: t0 + SECOND,
                    reason: UndeliverableReason::AgentExited,
                }
            );
        }
        assert_eq!(state_of(&inbox, &unbound, "2"), StateName::Queued);
        assert_eq!(inbox.unassigned_offers(), vec![offer.clone()]);
        assert_eq!(inbox.assign_offer(t0, &offer, &[worker()]), None);
        assert_eq!(
            inbox
                .fetch(t0, "2", "agent-w2", Consumer::FetchCli, None)
                .iter()
                .map(|m| m.id.clone())
                .collect::<Vec<_>>(),
            vec![unbound]
        );
    }

    fn role(pane: &str, agent: &str, token: &str, name: &str, role: &str) -> PaneFacts {
        PaneFacts {
            pane_id: pane.into(),
            agent_id: agent.into(),
            orchestration: Some(RoleFacts {
                token: token.into(),
                name: name.into(),
                role: role.into(),
            }),
            dispatch_caller: None,
        }
    }

    fn panes_of(members: &[ScopeMember]) -> Vec<&str> {
        members
            .iter()
            .map(|m| m.recipient.pane_id.as_str())
            .collect()
    }

    /// Scenario: Resolve the scope of a role in one of two orchestrations that
    /// share a config name, then of panes in a dispatch tree. The first scope
    /// is the sender's own instance only (#463); in the tree, every unit and
    /// the dispatcher at its root share one `d-<root>` scope, and a pane in
    /// neither has no scope.
    #[spec("inbox/core/012")]
    #[test]
    fn inbox_core_012_scope_is_the_instance_or_the_dispatch_tree() {
        let panes = vec![
            role("1", "a1", "tok-1", "tdd", "orchestrator"),
            role("2", "a2", "tok-1", "tdd", "coder"),
            role("3", "a3", "tok-2", "tdd", "orchestrator"),
            role("4", "a4", "tok-2", "tdd", "coder"),
            PaneFacts {
                pane_id: "9".into(),
                agent_id: "a9".into(),
                orchestration: None,
                dispatch_caller: None,
            },
        ];
        let resolved = resolve_scope("2", &panes).unwrap();
        assert_eq!(
            resolved.scope,
            Scope::Orchestration {
                token: "tok-1".into()
            }
        );
        assert_eq!(panes_of(&resolved.members), vec!["1", "2"]);
        assert!(resolve_scope("9", &panes).is_none());
        assert!(resolve_scope("missing", &panes).is_none());

        // A dispatcher pane "d" dispatched a single-agent unit "u1" and an
        // orchestration unit (tok-3, whose orchestrator "o3" dispatched "u2").
        let mut tree = vec![
            PaneFacts {
                pane_id: "d".into(),
                agent_id: "ad".into(),
                orchestration: None,
                dispatch_caller: None,
            },
            PaneFacts {
                pane_id: "u1".into(),
                agent_id: "au1".into(),
                orchestration: None,
                dispatch_caller: Some("d".into()),
            },
            role("o3", "ao3", "tok-3", "tdd", "orchestrator"),
            role("c3", "ac3", "tok-3", "tdd", "coder"),
            PaneFacts {
                pane_id: "u2".into(),
                agent_id: "au2".into(),
                orchestration: None,
                dispatch_caller: Some("o3".into()),
            },
            role("x", "ax", "tok-4", "tdd", "coder"),
        ];
        tree[2].dispatch_caller = Some("d".into());
        let expected = Scope::Dispatch {
            root_pane: "d".into(),
        };
        for sender in ["d", "u1", "o3", "c3", "u2"] {
            let resolved = resolve_scope(sender, &tree).unwrap();
            assert_eq!(resolved.scope, expected, "sender {sender}");
            assert_eq!(
                panes_of(&resolved.members),
                vec!["d", "u1", "o3", "c3", "u2"]
            );
        }
        let callers: Vec<_> = resolve_scope("u2", &tree)
            .unwrap()
            .members
            .into_iter()
            .filter(|m| m.is_dispatch_caller)
            .map(|m| m.recipient.pane_id)
            .collect();
        assert_eq!(callers, vec!["d", "o3"]);
        assert_eq!(
            resolve_scope("x", &tree).unwrap().scope,
            Scope::Orchestration {
                token: "tok-4".into()
            }
        );

        // A cycle in the recorded callers ends the walk instead of looping.
        let cycle = vec![
            PaneFacts {
                pane_id: "p".into(),
                agent_id: "ap".into(),
                orchestration: None,
                dispatch_caller: Some("q".into()),
            },
            PaneFacts {
                pane_id: "q".into(),
                agent_id: "aq".into(),
                orchestration: None,
                dispatch_caller: Some("p".into()),
            },
        ];
        assert!(matches!(
            resolve_scope("p", &cycle).unwrap().scope,
            Scope::Dispatch { .. }
        ));
    }

    fn subscriptions() -> Subscriptions {
        let mut roles = HashMap::new();
        roles.insert(
            "reviewer".to_string(),
            RoleSubscriptions {
                subscribe: vec!["coder-done".into()],
                take_offers: vec!["review".into()],
            },
        );
        roles.insert("coder".to_string(), RoleSubscriptions::default());
        let mut orchestrations = HashMap::new();
        orchestrations.insert(
            "tdd".to_string(),
            OrchestrationSubscriptions {
                subscribe: vec!["release-ready".into()],
                roles,
            },
        );
        Subscriptions {
            orchestrations,
            dispatch: vec!["unit-done".into()],
        }
    }

    /// Scenario: Resolve who receives a publish and who may be offered a job
    /// from configured subscriptions. Orchestration-wide topics reach every
    /// role, role topics only that role, `[dispatch]` topics only dispatch
    /// callers, offers only roles that take the pool; the sender never
    /// receives its own publish; an unsubscribed topic reaches no one.
    #[spec("inbox/core/013")]
    #[test]
    fn inbox_core_013_subscriptions_resolve_recipients_and_candidates() {
        let member = |pane: &str, role: Option<&str>, caller: bool| ScopeMember {
            recipient: Recipient::bound(pane, format!("a{pane}")),
            orchestration_name: role.map(|_| "tdd".to_string()),
            role: role.map(str::to_string),
            is_dispatch_caller: caller,
        };
        let members = vec![
            member("d", None, true),
            member("1", Some("orchestrator"), true),
            member("2", Some("coder"), false),
            member("3", Some("reviewer"), false),
            member("4", Some("reviewer"), false),
        ];
        let subs = subscriptions();
        let to = |topic: &str, sender: &str| -> Vec<String> {
            publish_recipients(&members, &subs, topic, sender)
                .into_iter()
                .map(|r| r.pane_id)
                .collect()
        };
        assert_eq!(to("release-ready", "2"), vec!["1", "3", "4"]);
        assert_eq!(to("coder-done", "2"), vec!["3", "4"]);
        assert_eq!(to("coder-done", "3"), vec!["4"]);
        assert_eq!(to("unit-done", "2"), vec!["d", "1"]);
        assert!(to("nobody-listens", "2").is_empty());
        let candidates: Vec<String> = offer_candidates(&members, &subs, "review", "1", &[])
            .into_iter()
            .map(|r| r.pane_id)
            .collect();
        assert_eq!(candidates, vec!["3", "4"]);
        assert!(offer_candidates(&members, &subs, "deploy", "1", &[]).is_empty());

        // Publishing fans out one copy per recipient under one id.
        let t0 = Instant::now();
        let mut inbox = inbox();
        let recipients = publish_recipients(&members, &subs, "coder-done", "2");
        let published = inbox
            .publish(
                t0,
                scope(),
                "coder-done",
                from_agent("2", "a2"),
                &recipients,
            )
            .unwrap();
        assert_eq!(published.delivered_to, 2);
        assert!(published.refused_full.is_empty());
        for pane in ["3", "4"] {
            let copy = inbox.get(published.id.as_str(), pane).unwrap();
            assert_eq!(copy.to.to_string(), "topic.k3xzq2ab.tok-1.coder-done");
            assert_eq!(copy.body_trust, BodyTrust::Untrusted);
        }
        assert_eq!(
            inbox.ack(t0, published.id.as_str(), "3", "a3", AckHow::Explicit),
            AckOutcome::Acknowledged
        );
        assert_eq!(state_of(&inbox, &published.id, "4"), StateName::Queued);
        let none = inbox
            .publish(t0, scope(), "nobody-listens", from_agent("2", "a2"), &[])
            .unwrap();
        assert_eq!(none.delivered_to, 0);
    }

    /// Scenario: Serialise states, ack outcomes, refusals and a listing
    /// summary, and build limits from an environment lookup. Every state's
    /// public spelling is lower case (`queued`, `fetched`, `claimed`,
    /// `acknowledged`, `undeliverable`), outcomes are snake case, and
    /// non-positive overrides are ignored rather than disabling a bound.
    #[spec("inbox/core/014")]
    #[test]
    fn inbox_core_014_wire_spellings_and_limit_overrides() {
        for (state, text) in [
            (StateName::Queued, "queued"),
            (StateName::Fetched, "fetched"),
            (StateName::Claimed, "claimed"),
            (StateName::Acknowledged, "acknowledged"),
            (StateName::Undeliverable, "undeliverable"),
        ] {
            assert_eq!(state.as_str(), text);
            assert_eq!(
                serde_json::to_value(state).unwrap(),
                serde_json::json!(text)
            );
        }
        assert_eq!(
            serde_json::to_value(AckOutcome::AlreadyAcknowledged).unwrap(),
            serde_json::json!("already_acknowledged")
        );
        assert_eq!(
            serde_json::to_value(AckOutcome::WrongAgent).unwrap(),
            serde_json::json!("wrong_agent")
        );
        assert_eq!(
            serde_json::to_value(PostError::InboxFull {
                pane_id: "2".into(),
                limit: 256
            })
            .unwrap(),
            serde_json::json!({"refused": "inbox_full", "pane_id": "2", "limit": 256})
        );

        let t0 = Instant::now();
        let mut inbox = inbox();
        let id = inbox.post_direct(t0, worker(), from_deck()).unwrap();
        let summary = inbox.list("2")[0].summary(t0 + SECOND);
        let json = serde_json::to_value(&summary).unwrap();
        assert_eq!(json["id"], serde_json::json!(id.as_str()));
        assert_eq!(json["state"], "queued");
        assert_eq!(json["kind"], "direct");
        assert_eq!(json["to"], "inbox.k3xzq2ab.2");
        assert_eq!(json["age_ms"], 1000);
        let round: MessageSummary = serde_json::from_value(json).unwrap();
        assert_eq!(round, summary);

        let env: HashMap<&str, &str> = [
            (ENV_ACK_WAIT_MS, "250"),
            (ENV_MAX_DELIVER, "2"),
            (ENV_MAX_PER_RECIPIENT, "0"),
            (ENV_MAX_MESSAGES, "lots"),
            (ENV_MIN_CLAIM_TTL_MS, "100"),
        ]
        .into_iter()
        .collect();
        let limits = InboxLimits::from_lookup(|name| env.get(name).map(|v| v.to_string()));
        assert_eq!(limits.ack_wait, Duration::from_millis(250));
        assert_eq!(limits.max_deliver, 2);
        assert_eq!(limits.max_per_recipient, DEFAULT_MAX_PER_RECIPIENT);
        assert_eq!(limits.max_messages, DEFAULT_MAX_MESSAGES);
        assert_eq!(limits.min_claim_ttl, Duration::from_millis(100));
        assert_eq!(InboxLimits::from_lookup(|_| None), InboxLimits::default());
    }

    /// Scenario: One agent publishes and offers more than the per-minute rate
    /// allows, then waits out the window. The publish past the limit is
    /// refused as rate-limited and enqueues nothing; deck senders are not
    /// limited; after a minute the agent may publish again.
    #[spec("inbox/core/015")]
    #[test]
    fn inbox_core_015_publish_and_offer_are_rate_limited_per_agent() {
        let t0 = Instant::now();
        let mut inbox = inbox_with(InboxLimits {
            rate_per_minute: 3,
            ..InboxLimits::default()
        });
        let to = [Recipient::bound("3", "a3")];
        for _ in 0..2 {
            inbox
                .publish(t0, scope(), "t", from_agent("2", "a2"), &to)
                .unwrap();
        }
        inbox
            .offer(t0, scope(), "p", None, from_agent("2", "a2"), &to)
            .unwrap();
        assert_eq!(
            inbox.publish(t0, scope(), "t", from_agent("2", "a2"), &to),
            Err(PostError::RateLimited {
                limit_per_minute: 3
            })
        );
        assert_eq!(inbox.list("3").len(), 3);
        inbox
            .publish(t0, scope(), "t", from_agent("5", "a5"), &to)
            .unwrap();
        for _ in 0..5 {
            inbox.publish(t0, scope(), "t", from_deck(), &to).unwrap();
        }
        inbox.tick(t0 + Duration::from_secs(60));
        inbox
            .publish(
                t0 + Duration::from_secs(60),
                scope(),
                "t",
                from_agent("2", "a2"),
                &to,
            )
            .unwrap();
    }

    /// Hardening M3: terminal history is bounded deck-wide, not only per
    /// pane: acknowledged messages left behind by many short-lived panes stay
    /// within the deck's history bound, newest kept, and no live message is
    /// ever trimmed.
    #[test]
    fn history_is_bounded_across_panes() {
        let t0 = Instant::now();
        let mut inbox = inbox_with(InboxLimits::default());
        let live = inbox
            .post_direct(t0, Recipient::bound("live", "agent-live"), from_deck())
            .unwrap();
        let mut newest = None;
        for pane in 0..40 {
            let pane = format!("short-lived-{pane}");
            for _ in 0..64 {
                let id = inbox
                    .post_direct(t0, Recipient::bound(&pane, "agent"), from_deck())
                    .unwrap();
                inbox.ack(t0, id.as_str(), &pane, "agent", AckHow::Explicit);
                newest = Some(id);
            }
        }
        let terminal = inbox.all().iter().filter(|m| m.state.is_terminal()).count();
        assert!(terminal <= 1_024, "{terminal} terminal messages kept");
        assert!(inbox.all().iter().any(|m| m.id == live));
        assert!(inbox.all().iter().any(|m| Some(&m.id) == newest.as_ref()));
    }

    /// Hardening M3: retained history is bounded in bytes, a terminal
    /// message's body is dropped once its diagnostic lifetime ends, and a
    /// closed pane's history goes with it.
    #[test]
    fn history_bytes_body_lifetime_and_closed_panes_are_bounded() {
        let t0 = Instant::now();
        let mut inbox = inbox_with(InboxLimits {
            max_history_bytes: 1_000,
            history_body_ttl: Duration::from_secs(60),
            ..InboxLimits::default()
        });
        let body = |n: usize| Content {
            body: "b".repeat(n),
            ..from_deck()
        };
        for _ in 0..5 {
            let id = inbox.post_direct(t0, worker(), body(400)).unwrap();
            inbox.ack(t0, id.as_str(), "2", "agent-w1", AckHow::Explicit);
        }
        let kept: usize = inbox
            .all()
            .iter()
            .filter(|m| m.state.is_terminal())
            .map(|m| m.body.len())
            .sum();
        assert!(kept <= 1_000, "{kept} bytes of history kept");
        let later = t0 + Duration::from_secs(61);
        let live = inbox.post_direct(later, worker(), body(10)).unwrap();
        assert!(
            inbox
                .all()
                .iter()
                .filter(|m| m.state.is_terminal())
                .all(|m| m.body.is_empty()),
            "terminal bodies past their lifetime are dropped"
        );
        inbox.forget_pane_history("2");
        let left: Vec<_> = inbox.list("2").iter().map(|m| m.id.clone()).collect();
        assert_eq!(left, vec![live], "only the live message is left");
    }

    /// Hardening M3: what a producer supplies beside the body is bounded and
    /// made one line when it is stored.
    #[test]
    fn message_metadata_is_capped_when_stored() {
        let t0 = Instant::now();
        let mut inbox = inbox_with(InboxLimits::default());
        let content = Content {
            subject: format!("line one\nline two {}", "s".repeat(10_000)),
            preface: Some("p".repeat(100_000)),
            from: Sender::Deck {
                reason: format!("schedule:{}", "r".repeat(10_000)),
            },
            spill_path: Some(PathBuf::from("/".to_string() + &"d".repeat(10_000))),
            ..from_deck()
        };
        let id = inbox.post_direct(t0, worker(), content).unwrap();
        let stored = inbox.get(id.as_str(), "2").unwrap();
        assert!(!stored.subject.contains('\n'), "{:?}", stored.subject);
        assert!(stored.subject.chars().count() <= 200);
        assert!(stored.preface.as_ref().unwrap().len() <= 8 * 1024);
        match &stored.from {
            Sender::Deck { reason } => assert!(reason.len() <= 256),
            other => panic!("{other:?}"),
        }
        assert_eq!(stored.spill_path, None, "an oversized path is not kept");
    }

    /// Scenario: Acknowledge more messages for one pane than its history ring
    /// holds, and render addresses whose tokens contain characters outside
    /// the subject alphabet. Only the newest acknowledged messages are kept
    /// while live ones are never trimmed, and every token encodes into
    /// `[A-Za-z0-9_-]` and decodes back.
    #[spec("inbox/core/016")]
    #[test]
    fn inbox_core_016_history_ring_and_address_tokens() {
        let t0 = Instant::now();
        let mut inbox = inbox_with(InboxLimits {
            history_per_pane: 2,
            ..InboxLimits::default()
        });
        let live = inbox.post_direct(t0, worker(), from_deck()).unwrap();
        let mut acked = Vec::new();
        for _ in 0..4 {
            let id = inbox.post_direct(t0, worker(), from_deck()).unwrap();
            inbox.ack(t0, id.as_str(), "2", "agent-w1", AckHow::Explicit);
            acked.push(id);
        }
        let kept: Vec<_> = inbox.list("2").iter().map(|m| m.id.clone()).collect();
        assert_eq!(kept, vec![live, acked[2].clone(), acked[3].clone()]);

        for raw in ["2", "desktop-9ff5-0", "a.b c", "role_x", "ünï", "%", ""] {
            let token = encode_token(raw);
            assert!(
                token
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
                "{token}"
            );
            assert!(!token.is_empty());
            assert_eq!(decode_token(&token).as_deref(), Some(raw), "{token}");
        }
        assert_eq!(decode_token("a.b"), None);
        assert_eq!(decode_token("_zz"), None);
        let address = Address::Offer {
            deck: deck(),
            scope: Scope::Dispatch {
                root_pane: "pane.1".into(),
            },
            pool: "review".into(),
        };
        assert_eq!(address.to_string(), "offer.k3xzq2ab.d-pane_2e1.review");
        assert_eq!(
            Address::Direct {
                deck: deck(),
                pane_id: "sched-dispatch-x_1".into()
            }
            .to_string(),
            "inbox.k3xzq2ab.sched-dispatch-x_5f1"
        );
    }
}
