//! PRD #1666 design v3.2 §13 step 4: the hub's receipt engine — one attempt
//! model (`crate::inbox_attempts`) per agent generation, driving delivery for
//! the receivers switched to the receipt policy, beside the legacy ack-wait
//! the others keep.
//!
//! A child of [`super`] (the hub): every function here runs under the hub's
//! lock on its [`HubState`], and none does file IO. Spool IO — the generation
//! directory a registration needs, the exact lookups before a lapse and the
//! removal of reclaimed evidence — runs off the executor and outside the lock,
//! in the hub methods that call in here.
//!
//! **One policy per assignment.** A message is handed to a receipt receiver
//! only while its inbox copy is queued and not `Legacy`-owned; taking it makes
//! the assignment `Receipt`-owned ([`crate::inbox::AssignmentOwner`]), which
//! the legacy ack-wait, requeue and nack then leave alone. The attempt model
//! decides everything after that, and [`sync`] mirrors each of its decisions
//! into the inbox: a settle acknowledges (or claims an offer), rule 3 queues
//! the message again, an undeliverable verdict retires it.
//!
//! **The adapter floor (§11).** A generation's [`Floor`] holds what the deck
//! knows of its integration: the capability probe's verdict, a registration
//! refused below [`super::ADAPTER_INTEGRATION`], and the readiness deadline.
//! While the integration is incompatible no component of the generation is
//! registered, so nothing is handed to it, and its doorbell stays silent; mail
//! waits queued.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::{
    AgentKey, BroadcastMsg, HubState, InboxAttentionItem, InboxCardNotices, LATE_ACK_SUBJECT,
    WaiterSignal, kind_sender_label, render_marked,
};
use crate::agent_pty::{TurnEdge, TurnSeen};
use crate::event::{
    InboxAttemptCaps, InboxAttemptRef, InboxBudget, InboxComponent, InboxCorrectable,
    InboxDelivery, InboxDeliveryAttempt, InboxGrant, InboxIssued, InboxNotice, InboxRebound,
    InboxRegistered, InboxStreamControl, InboxWaitReply,
};
use crate::inbox::{AckHow, AssignmentOwner, Consumer, MessageState, UndeliverableReason};
use crate::inbox_attempts::{
    AttemptModel, Effect, HandOverRefusal, LapseCheck, Limits, MessageState as Delivery,
    ResumeAnswer, Rx, Slot, Stage, TurnView, UncertainReason, UndeliverableReason as Verdict,
};
use crate::inbox_display::{AttentionKind, IntegrationNotice};
use crate::inbox_receipt::{EvidenceStore, GenerationSpool, RecordScanner};

/// How many lines may wait for one receipt stream's connection task. A line
/// that finds the queue full is dropped ([`flush`]); the stream itself stays
/// open, so a dropped revoke is sent again by the clock
/// ([`AttemptModel::revokes_due`]).
pub(super) const RECEIPT_SIGNAL_CAPACITY: usize = 64;

/// PRD #1666 §11: how long after spawn a generation whose adapter declares
/// the floor may go without a registration before its card says the
/// integration has not connected yet. Its own deadline, not the 10 s native
/// arm grace.
pub const ARM_DEADLINE: Duration = Duration::from_secs(120);

/// Overrides [`ARM_DEADLINE`], in milliseconds.
pub const ENV_ARM_DEADLINE_MS: &str = "DOT_AGENT_DECK_INBOX_ARM_DEADLINE_MS";

/// [`ARM_DEADLINE`] or its override.
pub fn arm_deadline_from_env() -> Duration {
    std::env::var(ENV_ARM_DEADLINE_MS)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map_or(ARM_DEADLINE, Duration::from_millis)
}

/// Overrides [`crate::inbox_attempts::NOT_STARTED_AFTER`], in milliseconds.
pub const ENV_NOT_STARTED_MS: &str = "DOT_AGENT_DECK_INBOX_NOT_STARTED_MS";

/// [`crate::inbox_attempts::NOT_STARTED_AFTER`] or its override.
pub fn not_started_after_from_env() -> Duration {
    std::env::var(ENV_NOT_STARTED_MS)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map_or(
            crate::inbox_attempts::NOT_STARTED_AFTER,
            Duration::from_millis,
        )
}

/// PRD #1666 G3: how many exited generations the turn clocks remember. A
/// turn edge delayed past its generation's exit is refused while that key is
/// still among them; the oldest is forgotten when the ring is full. The
/// registry checks the edge's owner and the daemon hands it here inside one
/// ingest, so the memory only has to outlast that, not hours.
pub(super) const EXITED_TURN_KEYS: usize = 1024;

/// PRD #1666 G3: one generation's turns, as its own hook reports reached the
/// daemon. Kept apart from [`Gen`]: an edge creates no receipt generation,
/// and a registration reads the clock without resetting it.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct TurnClock {
    pub(super) busy: bool,
    /// Set only when a turn under way ends.
    pub(super) idle_since: Option<Instant>,
    /// The registry's order of the last edge applied; an older one is
    /// ignored.
    pub(super) seq: u64,
}

/// The receiver slot a component's stream registers in (§4).
pub(super) fn slot_for(component: InboxComponent) -> Option<Slot> {
    match component {
        InboxComponent::OpenCodePlugin => Some(Slot::OpencodeTui),
        InboxComponent::OpenCodeMini => Some(Slot::OpencodeServer),
        InboxComponent::PiExtension => Some(Slot::Pi),
        _ => None,
    }
}

fn slot_name(slot: Slot) -> &'static str {
    match slot {
        Slot::OpencodeServer => "opencode_server",
        Slot::OpencodeTui => "opencode_tui",
        Slot::Pi => "pi",
        Slot::OnceWaiter => "once_waiter",
        Slot::Hook => "hook",
    }
}

/// What the deck knows of one generation's integration (§11).
#[derive(Debug, Default)]
pub(super) struct Floor {
    /// The generation's adapter declares the floor: without a registration
    /// by this instant the card says it has not connected yet.
    pub(super) arm_by: Option<Instant>,
    pub(super) registered: bool,
    /// The capability probe proved the agent's CLI lacks the flag.
    pub(super) cli_too_old: bool,
    /// A registration declared a missing or lower integration.
    pub(super) adapter_too_old: bool,
    /// The deck could not rewrite the generation's integration files as it
    /// started (`refresh_for_spawn`), so restarting does not update them:
    /// the notice in that integration's words.
    pub(super) refresh_failed: Option<IntegrationNotice>,
}

impl Floor {
    fn notice(&self, now: Instant) -> Option<IntegrationNotice> {
        if self.cli_too_old {
            Some(IntegrationNotice::CliTooOld)
        } else if let Some(notice) = self.refresh_failed.filter(|_| !self.registered) {
            Some(notice)
        } else if self.adapter_too_old {
            Some(IntegrationNotice::AdapterTooOld)
        } else if !self.registered && self.arm_by.is_some_and(|by| now >= by) {
            Some(IntegrationNotice::NotArmed)
        } else {
            None
        }
    }

    /// Incompatible: nothing is handed to the generation and its doorbell is
    /// silent. `not_armed` is readiness, not incompatibility.
    pub(super) fn blocks(&self) -> bool {
        self.cli_too_old || self.adapter_too_old
    }
}

/// One live receipt stream.
pub(super) struct Link {
    pub(super) conn_id: u64,
    pub(super) tx: mpsc::Sender<WaiterSignal>,
    pub(super) component: InboxComponent,
    pub(super) doorbell_only: bool,
}

/// The evidence a generation's attempts publish into its spool.
pub(super) struct Evidence {
    pub(super) store: EvidenceStore,
    pub(super) scanner: Option<RecordScanner>,
}

/// One agent generation's receipt-owned delivery.
pub(super) struct Gen {
    pub(super) model: AttemptModel,
    pub(super) spool: Option<Arc<GenerationSpool>>,
    pub(super) evidence: Arc<Mutex<Evidence>>,
    pub(super) links: HashMap<Rx, Link>,
    /// §3: the generation's receipts cannot be recorded; sticky until exit.
    pub(super) degraded: Option<String>,
    /// §7: hand-outs are paused for room in the evidence store.
    pub(super) paused: bool,
    /// Delegated tasks whose orchestrator was told their delivery is
    /// uncertain (§6.6), so a later commit corrects it once.
    pub(super) uncertain_told: HashSet<String>,
}

/// Every generation's receipt state, held in [`HubState::receipts`].
pub(super) struct Receipts {
    pub(super) limits: Limits,
    pub(super) gens: HashMap<AgentKey, Gen>,
    pub(super) floors: HashMap<AgentKey, Floor>,
    /// The card notices last broadcast per generation.
    pub(super) told: HashMap<AgentKey, InboxCardNotices>,
    /// Reclaimed attempts whose spool files go once the lock is released.
    pub(super) removals: Vec<(Arc<GenerationSpool>, String, u32, u32)>,
    pub(super) arm_deadline: Duration,
    /// When the clock last read the spools ([`tick`]).
    pub(super) last_check: Option<Instant>,
    /// PRD #1666 G3: every generation's turn clock ([`note_turn`]).
    pub(super) turns: HashMap<AgentKey, TurnClock>,
    /// Generations that exited, newest last, at most [`EXITED_TURN_KEYS`].
    pub(super) exited_turns: VecDeque<AgentKey>,
}

impl Receipts {
    pub(super) fn new(limits: Limits, arm_deadline: Duration) -> Self {
        Receipts {
            limits,
            gens: HashMap::new(),
            floors: HashMap::new(),
            told: HashMap::new(),
            removals: Vec::new(),
            arm_deadline,
            last_check: None,
            turns: HashMap::new(),
            exited_turns: VecDeque::new(),
        }
    }

    /// PRD #1666 G3: what `key`'s turn clock reads now; a generation that
    /// reported no turn reads idle, never seen busy.
    pub(super) fn turn_view(&self, key: &AgentKey) -> TurnView {
        self.turns
            .get(key)
            .map_or_else(TurnView::default, |clock| TurnView {
                busy: clock.busy,
                idle_since: clock.idle_since,
            })
    }

    /// The generation's floor blocks delivery (§11).
    pub(super) fn blocked(&self, key: &AgentKey) -> bool {
        self.floors.get(key).is_some_and(Floor::blocks)
    }

    /// The earliest instant [`tick`] has something to do for.
    pub(super) fn next_deadline(&self, now: Instant) -> Option<Instant> {
        let open = self.gens.values().any(|generation| {
            generation.model.ids().iter().any(|id| {
                generation
                    .model
                    .state(id)
                    .is_some_and(|state| !state.is_terminal())
            })
        });
        let step = (self.limits.possess_ttl / 3)
            .min(self.limits.ack_wait / 2)
            .max(Duration::from_millis(20));
        let arm = self
            .floors
            .values()
            .filter(|floor| !floor.registered)
            .filter_map(|floor| floor.arm_by)
            .filter(|by| *by > now)
            .min();
        // §3: an attempt that just lost possession is checked in the spool
        // soon, so its repeat (or its settling receipt) is not left waiting
        // for the next ordinary step; at most once a second while one waits.
        let checking = self.gens.values().any(|generation| {
            !generation.model.lapse_candidates(now).is_empty()
                || !generation.model.verify_candidates().is_empty()
        });
        let check = checking.then(|| {
            self.last_check
                .map_or(now, |at| at + step.min(Duration::from_secs(1)))
                .max(now)
        });
        // r8k R8G-04: a revoke still owed an answer is sent again by the
        // clock, also for a terminal message, which `open` does not count.
        let revoke = self
            .gens
            .values()
            .filter_map(|generation| generation.model.revoke_due_at())
            .min()
            .map(|due| due.max(now));
        // PRD #1666 G3: a not-started attention falls due on its own instant.
        let not_started = self
            .gens
            .iter()
            .filter_map(|(key, generation)| {
                generation.model.not_started_due_at(self.turn_view(key))
            })
            .min()
            .map(|due| due.max(now));
        [open.then(|| now + step), arm, check, revoke, not_started]
            .into_iter()
            .flatten()
            .min()
    }
}

/// A line to write to one receipt stream once the lock is released.
pub(super) type Outbox = Vec<(mpsc::Sender<WaiterSignal>, InboxWaitReply)>;

fn send(out: &mut Outbox, link: &Link, reply: InboxWaitReply) {
    out.push((link.tx.clone(), reply));
}

/// The generation for `key`, created with `spool` (or degraded by `spool`'s
/// error) when it does not exist yet.
pub(super) fn generation_mut<'a>(
    st: &'a mut HubState,
    key: &AgentKey,
    spool: Option<Result<Arc<GenerationSpool>, String>>,
) -> &'a mut Gen {
    let limits = st.receipts.limits;
    let missing = match &st.spool_error {
        Some(error) => format!("the receipt spool could not start: {error}"),
        None => "the deck has no receipt spool".to_string(),
    };
    st.receipts.gens.entry(key.clone()).or_insert_with(|| {
        let (spool, mut degraded) = match spool {
            Some(Ok(spool)) => (Some(spool), None),
            Some(Err(error)) => (None, Some(error)),
            None => (None, Some(missing)),
        };
        // §3: a generation the deck cannot read the receipts of is degraded,
        // and its lapses follow the degraded policy rather than a check.
        let scanner = spool.as_ref().and_then(|spool| match spool.scanner() {
            Ok(scanner) => Some(scanner),
            Err(error) => {
                degraded.get_or_insert_with(|| format!("could not read receipts: {error}"));
                None
            }
        });
        Gen {
            model: AttemptModel::new(limits, 1),
            spool,
            evidence: Arc::new(Mutex::new(Evidence {
                store: EvidenceStore::default(),
                scanner,
            })),
            links: HashMap::new(),
            degraded,
            paused: false,
            uncertain_told: HashSet::new(),
        }
    })
}

/// T8: a receipt stream registers for `key`. The slot's previous incarnation
/// is retired; its open attempts wait for this registration's resume answer.
#[allow(clippy::too_many_arguments)]
pub(super) fn register(
    st: &mut HubState,
    key: &AgentKey,
    component: InboxComponent,
    doorbell_only: bool,
    resumed: bool,
    conn_id: u64,
    tx: mpsc::Sender<WaiterSignal>,
    spool: Option<Result<Arc<GenerationSpool>, String>>,
    now: Instant,
) -> Option<InboxWaitReply> {
    let slot = slot_for(component)?;
    let floor = st.receipts.floors.entry(key.clone()).or_default();
    floor.registered = true;
    floor.adapter_too_old = false;
    floor.cli_too_old = false;
    floor.refresh_failed = None;
    let generation = generation_mut(st, key, spool);
    let registration = generation.model.register(now, slot, resumed);
    // r8k R8G-03: commit authority only, for the new watcher to publish a
    // late commit its predecessor died with; it strips the cap before the
    // component sees the line. An attempt whose caps the store no longer
    // holds is left out: there is no authority to hand back.
    let correctable: Vec<InboxCorrectable> = {
        let evidence = generation
            .evidence
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        registration
            .correctable
            .iter()
            .filter_map(|(id, a, e)| {
                let caps = evidence.store.caps(id, *a, *e)?;
                Some(InboxCorrectable {
                    id: id.clone(),
                    a: *a,
                    e: *e,
                    commit: Some(caps.commit.hex()),
                })
            })
            .collect()
    };
    generation.links.insert(
        registration.rx,
        Link {
            conn_id,
            tx,
            component,
            doorbell_only,
        },
    );
    info!(
        pane_id = %key.0,
        rx = registration.rx.0,
        slot = slot_name(slot),
        resume = registration.resume.len(),
        correctable = correctable.len(),
        "inbox: receipt receiver registered"
    );
    Some(InboxWaitReply {
        registered: Some(InboxRegistered {
            rx: registration.rx.0,
            slot: slot_name(slot).to_string(),
            keepalive_ms: (st.receipts.limits.possess_ttl
                / crate::inbox_attempts::KEEPALIVES_PER_POSSESS_TTL)
                .as_millis() as u64,
        }),
        resume: registration
            .resume
            .into_iter()
            .map(|(id, a, e)| InboxAttemptRef { id, a, e })
            .collect(),
        correctable,
        ..InboxWaitReply::default()
    })
}

/// A registration below the floor (§11): nothing is registered, the
/// generation's mail waits queued and its card says the integration is out of
/// date.
pub(super) fn refuse_adapter(st: &mut HubState, key: &AgentKey) {
    st.receipts
        .floors
        .entry(key.clone())
        .or_default()
        .adapter_too_old = true;
}

/// The stream `rx` of `key` ended. It decides nothing at once (§4: a stream
/// disconnect never requeues): the incarnation stays its slot's until the
/// next registration of that slot retires it and asks it to resume what it
/// still holds, and with no snapshot its attempts lapse when `POSSESS_TTL`
/// runs out, which is §6's marked repeat once the spool is checked. With no
/// registration in its place for a while, the model retires it and fences
/// what it held, so a Cancel can end those attempts and a message no
/// receiver takes is `Uncertain` rather than waiting forever.
pub(super) fn closed(st: &mut HubState, key: &AgentKey, rx: Rx, conn_id: u64, now: Instant) {
    let Some(generation) = st.receipts.gens.get_mut(key) else {
        return;
    };
    if generation
        .links
        .get(&rx)
        .is_some_and(|link| link.conn_id == conn_id)
    {
        generation.links.remove(&rx);
        generation.model.stream_closed(now, rx);
    }
}

/// Whether `id`'s inbox copy for `key` is one this generation's receipt
/// receiver may be handed now: queued, admitting the agent, not
/// `Legacy`-owned.
fn takeable(st: &HubState, key: &AgentKey, id: &str) -> bool {
    st.inbox.get(id, &key.0).is_some_and(|m| {
        m.state == MessageState::Queued
            && m.owner != Some(AssignmentOwner::Legacy)
            && m.recipient
                .as_ref()
                .is_some_and(|r| r.admits(&key.0, &key.1))
    })
}

/// The messages a hand-out to `key` may carry now, oldest first: scheduled
/// repeats, then queued mail no legacy consumer owns.
pub(super) fn candidates(st: &HubState, key: &AgentKey) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(generation) = st.receipts.gens.get(key) {
        for id in generation.model.ids() {
            let repeat = generation
                .model
                .delivery(&id)
                .is_some_and(|d| d.repeat_scheduled() && !d.state.is_terminal());
            let held = st.inbox.get(&id, &key.0).is_some_and(|m| {
                !m.state.is_terminal()
                    && m.owner == Some(AssignmentOwner::Receipt)
                    && m.recipient
                        .as_ref()
                        .is_some_and(|r| r.admits(&key.0, &key.1))
            });
            if repeat && held {
                out.push(id);
            }
        }
    }
    for m in st.inbox.list(&key.0) {
        let id = m.id.as_str();
        if takeable(st, key, id) && !out.iter().any(|o| o == id) {
            out.push(id.to_string());
        }
    }
    out
}

/// T1 for each candidate that fits `budget`, bound to `rx`. Returns the
/// rendered hand-outs, each with its attempt and caps.
fn issue_to(
    st: &mut HubState,
    key: &AgentKey,
    rx: Rx,
    ids: &[String],
    budget: Option<InboxBudget>,
    consumer: Consumer,
    now: Instant,
) -> Vec<InboxDelivery> {
    let mut out = Vec::new();
    let mut used = 0usize;
    for id in ids.iter().take(crate::inbox_prompt::MAX_IDS) {
        let Some(message) = st.inbox.get(id, &key.0).cloned() else {
            continue;
        };
        let offer = matches!(message.kind, crate::inbox::MessageKind::Offer { .. });
        let Some(generation) = st.receipts.gens.get_mut(key) else {
            return out;
        };
        generation.model.post(id, offer);
        // Measured before it is issued: a message that would take the batch
        // past the budget stays waiting for the next hand-out, unissued. The
        // first is always taken, even over the budget on its own.
        let marked = generation
            .model
            .delivery(id)
            .is_some_and(|d| d.repeat_scheduled());
        let len = budget.map(|budget| {
            let text = render_marked(&message, Some(budget), marked).text;
            budget.measure(&text) + budget.measure(super::BLOCK_SEPARATOR)
        });
        if let (Some(budget), Some(len)) = (budget, len)
            && !out.is_empty()
            && used + len > budget.limit()
        {
            break;
        }
        let issued = match generation.model.issue(now, id, rx) {
            Ok(issued) => issued,
            Err(crate::inbox_attempts::IssueRefusal::EvidencePaused) => {
                generation.paused = true;
                break;
            }
            Err(refusal) => {
                debug!(message_id = %id, ?refusal, "inbox: receipt hand-out refused");
                continue;
            }
        };
        generation.paused = false;
        let caps = match generation
            .evidence
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .store
            .issue(id, issued.a, issued.e)
        {
            Ok(caps) => Some(InboxAttemptCaps {
                handed: caps.handed.hex(),
                commit: caps.commit.hex(),
            }),
            Err(error) => {
                generation
                    .degraded
                    .get_or_insert_with(|| format!("could not mint receipt caps: {error}"));
                None
            }
        };
        if message.state == MessageState::Queued {
            st.inbox
                .take_receipt(now, id, &key.0, &key.1, consumer.clone());
        }
        let mut delivery = render_marked(&message, budget, issued.kind.marked());
        used += len.unwrap_or(0);
        delivery.attempt = Some(InboxDeliveryAttempt {
            a: issued.a,
            e: issued.e,
            d: issued.d,
            repeat: issued.kind.marked(),
            caps,
        });
        out.push(delivery);
    }
    out
}

/// Hand `key`'s takeable mail and scheduled repeats to its delivery receiver.
///
/// A generation whose floor blocks it (§11) has no receiver to hand to: a
/// refused component never registers, and a valid registration clears the
/// floor's verdicts.
pub(super) fn dispatch(st: &mut HubState, key: &AgentKey, now: Instant, out: &mut Outbox) {
    let Some(generation) = st.receipts.gens.get(key) else {
        return;
    };
    let Some((rx, link)) = generation
        .links
        .iter()
        .filter(|(rx, link)| !link.doorbell_only && generation.model.live(**rx))
        .max_by_key(|(_, link)| link.conn_id)
    else {
        return;
    };
    let (rx, conn_id, budget, tx) = (
        *rx,
        link.conn_id,
        link.component.inline_budget(),
        link.tx.clone(),
    );
    let ids = candidates(st, key);
    if ids.is_empty() {
        return;
    }
    let batch = issue_to(
        st,
        key,
        rx,
        &ids,
        Some(budget),
        Consumer::Stream { conn_id },
        now,
    );
    if !batch.is_empty() {
        out.push((tx, InboxWaitReply::messages(batch)));
    }
    apply(st, key, now, out);
}

/// A doorbell substitution by the component holding incarnation `rx` (§4):
/// the waiting mail is issued to `rx`, its caps go to that stream and its
/// bodies to the caller. With `named`, nothing is issued unless that message
/// is still waiting, decided under the same lock.
pub(super) fn doorbell_fetch(
    st: &mut HubState,
    key: &AgentKey,
    rx: Rx,
    named: Option<&str>,
    budget: Option<InboxBudget>,
    now: Instant,
    out: &mut Outbox,
) -> Result<Vec<InboxDelivery>, &'static str> {
    let Some(generation) = st.receipts.gens.get(key) else {
        return Err("unknown_receiver");
    };
    if !generation.model.live(rx) {
        return Err("unknown_receiver");
    }
    let Some(link) = generation.links.get(&rx) else {
        return Err("unknown_receiver");
    };
    let (conn_id, tx) = (link.conn_id, link.tx.clone());
    let ids = candidates(st, key);
    if let Some(named) = named
        && !ids.iter().any(|id| id == named)
    {
        return Ok(Vec::new());
    }
    let mut batch = issue_to(st, key, rx, &ids, budget, Consumer::Stream { conn_id }, now);
    let issued: Vec<InboxIssued> = batch
        .iter_mut()
        .filter_map(|delivery| {
            let attempt = delivery.attempt.as_mut()?;
            let caps = attempt.caps.take()?;
            Some(InboxIssued {
                id: delivery.id.clone(),
                a: attempt.a,
                e: attempt.e,
                d: attempt.d,
                caps,
            })
        })
        .collect();
    if !issued.is_empty() {
        out.push((
            tx,
            InboxWaitReply {
                issued,
                ..InboxWaitReply::default()
            },
        ));
    }
    apply(st, key, now, out);
    Ok(batch)
}

fn answer(text: &str) -> ResumeAnswer {
    match text {
        "retained" => ResumeAnswer::Retained,
        "not_handed" => ResumeAnswer::NotHanded,
        _ => ResumeAnswer::Unknown,
    }
}

fn refusal_code(refusal: HandOverRefusal) -> &'static str {
    match refusal {
        HandOverRefusal::Unknown => "unknown",
        HandOverRefusal::WrongReceiver => "wrong_receiver",
        HandOverRefusal::Committed => "committed",
        HandOverRefusal::Revoked => "revoked",
    }
}

/// One control line from incarnation `rx`'s stream (§2, §3, §4). Every
/// control but `committed` is honoured only from the attempt's own live
/// incarnation, which the model checks.
pub(super) fn control(
    st: &mut HubState,
    key: &AgentKey,
    rx: Rx,
    control: &InboxStreamControl,
    now: Instant,
    out: &mut Outbox,
) {
    let Some(generation) = st.receipts.gens.get_mut(key) else {
        return;
    };
    if let Some(error) = control.degraded.as_deref() {
        generation
            .degraded
            .get_or_insert_with(|| crate::untrusted_text::display_line(error, 200));
    }
    if let Some(possess) = &control.possess {
        let held: Vec<(&str, u32, u32)> = possess
            .held
            .iter()
            .map(|h| (h.id.as_str(), h.a, h.e))
            .collect();
        generation
            .model
            .snapshot(now, rx, possess.seq, possess.seen, &held);
    }
    for at in &control.handed {
        generation
            .model
            .stage(now, &at.id, at.a, at.e, Stage::Handed, Some(rx));
    }
    for at in &control.committed {
        generation
            .model
            .stage(now, &at.id, at.a, at.e, Stage::Committed, None);
    }
    for at in control.not_handed.iter().chain(&control.revoked_ack) {
        generation
            .model
            .stage(now, &at.id, at.a, at.e, Stage::NotHanded, Some(rx));
    }
    for at in &control.already_handed {
        generation
            .model
            .stage(now, &at.id, at.a, at.e, Stage::HandedInert, Some(rx));
    }
    let mut rebound = Vec::new();
    for reply in &control.resume_answers {
        let given = answer(&reply.answer);
        generation
            .model
            .resume_answer(now, rx, &reply.id, reply.a, reply.e, given);
        if given == ResumeAnswer::Retained
            && let Some(at) = generation
                .model
                .delivery(&reply.id)
                .and_then(|d| d.attempt(reply.a, reply.e))
            && at.rx == rx
        {
            // §5: the new watcher publishes this attempt's receipts, so it is
            // handed the attempt's caps again; it never passes them on.
            let caps = generation
                .evidence
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .store
                .caps(&reply.id, reply.a, reply.e)
                .map(|caps| InboxAttemptCaps {
                    handed: caps.handed.hex(),
                    commit: caps.commit.hex(),
                });
            rebound.push(InboxRebound {
                id: reply.id.clone(),
                a: reply.a,
                e: reply.e,
                d: at.d,
                caps,
            });
        }
    }
    let grant = control.may.as_ref().map(|may| {
        match generation.model.may_hand_over(&may.id, may.a, may.e, rx) {
            Ok(_) => InboxGrant {
                q: may.q,
                ok: true,
                refusal: None,
            },
            Err(refusal) => InboxGrant {
                q: may.q,
                ok: false,
                refusal: Some(refusal_code(refusal).to_string()),
            },
        }
    });
    if let Some(link) = generation.links.get(&rx)
        && (grant.is_some() || !rebound.is_empty())
    {
        send(
            out,
            link,
            InboxWaitReply {
                grant,
                rebound,
                ..InboxWaitReply::default()
            },
        );
    }
    apply(st, key, now, out);
    dispatch(st, key, now, out);
}

/// An explicit acknowledgement of a `Receipt`-owned message (`dot-agent-deck
/// ack <id>`): its agent says it has the message, which settles it — rule 1
/// needs one committed attempt of the current assignment, whichever it is.
pub(super) fn explicit_commit(st: &mut HubState, key: &AgentKey, id: &str, now: Instant) -> bool {
    let Some(generation) = st.receipts.gens.get_mut(key) else {
        return false;
    };
    let Some(delivery) = generation.model.delivery(id) else {
        return false;
    };
    let a = delivery.a;
    let Some(e) = delivery
        .attempts
        .iter()
        .filter(|at| at.a == a)
        .map(|at| at.e)
        .max()
    else {
        return false;
    };
    generation
        .model
        .stage(now, id, a, e, Stage::Committed, None);
    let mut out = Outbox::new();
    apply(st, key, now, &mut out);
    flush(out);
    true
}

/// Write `out`'s lines. A line for a stream whose queue is full is dropped
/// and the stream stays open (a dropped revoke is sent again by the clock,
/// [`AttemptModel::revokes_due`]); a stream that is gone is left to its own
/// connection task to notice.
pub(super) fn flush(out: Outbox) {
    for (tx, reply) in out {
        if tx.try_send(WaiterSignal::Line(Box::new(reply))).is_err() {
            debug!("inbox: a receipt stream is not taking its lines");
        }
    }
}

/// What one generation's spool IO found, for [`tick`] to apply under the
/// lock.
#[derive(Default)]
pub(super) struct Checked {
    /// Commits the spool holds for issued attempts, whichever they are.
    pub(super) committed: Vec<(String, u32, u32)>,
    /// Lapse candidates a completed check found no receipt for.
    pub(super) cleared: Vec<LapseCheck>,
    /// Verify candidates a completed check found no commit for.
    pub(super) verified: Vec<LapseCheck>,
    /// The deck's own spool error, if the IO met one (§3).
    pub(super) degraded: Option<String>,
}

/// T2/T3, then T4/T8/held_long and G3's not-started attention, for every generation: `checked` holds what
/// each generation's spool IO found. Every commit found is applied first, so
/// a receipt published for any issued attempt settles (or corrects) before
/// a lapse can repeat anything; only the candidates a completed check
/// cleared may then count toward an automatic repeat.
pub(super) fn tick(
    st: &mut HubState,
    now: Instant,
    checked: &HashMap<AgentKey, Checked>,
    out: &mut Outbox,
) {
    st.receipts.last_check = Some(now);
    let keys: Vec<AgentKey> = st.receipts.gens.keys().cloned().collect();
    for key in &keys {
        let turn = st.receipts.turn_view(key);
        if let Some(generation) = st.receipts.gens.get_mut(key) {
            let checked = checked.get(key);
            if let Some(error) = checked.and_then(|c| c.degraded.as_deref()) {
                generation
                    .degraded
                    .get_or_insert_with(|| format!("could not read receipts: {error}"));
            }
            for (id, a, e) in checked.map(|c| c.committed.as_slice()).unwrap_or_default() {
                if generation
                    .model
                    .stage(now, id, *a, *e, Stage::Committed, None)
                    != crate::inbox_attempts::StageOutcome::NoOp
                {
                    info!(message_id = %id, a, e, "inbox: a receipt the stream never reported was found");
                }
            }
            generation
                .model
                .tick_verified(checked.map(|c| c.verified.as_slice()).unwrap_or_default());
            generation.model.tick_turn(
                now,
                checked.map(|c| c.cleared.as_slice()).unwrap_or_default(),
                turn,
            );
        }
        apply(st, key, now, out);
        dispatch(st, key, now, out);
    }
    for key in st.receipts.floors.keys().cloned().collect::<Vec<_>>() {
        broadcast_notices(st, &key, now);
    }
}

/// PRD #1666 G3: generation `key` reported a turn edge the registry accepted
/// from its live owner, or kept for its reserved spawn, numbered `seen.seq`.
/// A turn starting makes the clock busy; one under way ending makes it idle
/// from `now`, the only place `idle_since` moves. An edge for an exited
/// generation still remembered in [`EXITED_TURN_KEYS`], or one older than
/// the last applied, changes nothing. Returns
/// whether the clock changed.
pub(super) fn note_turn(st: &mut HubState, key: &AgentKey, seen: TurnSeen, now: Instant) -> bool {
    if st.receipts.exited_turns.contains(key) {
        return false;
    }
    let clock = st.receipts.turns.entry(key.clone()).or_default();
    if seen.seq <= clock.seq {
        return false;
    }
    clock.seq = seen.seq;
    match seen.edge {
        TurnEdge::Started if !clock.busy => {
            clock.busy = true;
            true
        }
        TurnEdge::Ended if clock.busy => {
            clock.busy = false;
            clock.idle_since = Some(now);
            true
        }
        _ => false,
    }
}

/// One generation's spool IO for the clock (§3).
pub(super) struct Check {
    pub(super) key: AgentKey,
    pub(super) evidence: Arc<Mutex<Evidence>>,
    /// The lapse candidates the spool lookup must clear.
    pub(super) candidates: Vec<LapseCheck>,
    /// The ended messages' fenced attempts the lookup must verify.
    pub(super) verify: Vec<LapseCheck>,
}

/// The generations whose spool the clock reads now: those with a lapse
/// candidate to check, or with an attempt whose commit receipt could still
/// turn up.
pub(super) fn lapse_checks(st: &HubState, now: Instant) -> Vec<Check> {
    st.receipts
        .gens
        .iter()
        .filter_map(|(key, generation)| {
            let candidates = generation.model.lapse_candidates(now);
            let verify = generation.model.verify_candidates();
            (!candidates.is_empty() || !verify.is_empty() || generation.model.watching_receipts())
                .then(|| Check {
                    key: key.clone(),
                    evidence: Arc::clone(&generation.evidence),
                    candidates,
                    verify,
                })
        })
        .collect()
}

/// PRD #1666 G3: `key`'s turn clock goes, and the key joins the exited
/// generations, so a delayed edge for it is refused while it stays among the
/// [`EXITED_TURN_KEYS`] most recent.
pub(super) fn forget_turns(st: &mut HubState, key: &AgentKey) {
    st.receipts.turns.remove(key);
    if !st.receipts.exited_turns.contains(key) {
        if st.receipts.exited_turns.len() >= EXITED_TURN_KEYS {
            st.receipts.exited_turns.pop_front();
        }
        st.receipts.exited_turns.push_back(key.clone());
    }
}

/// T9 for `key`: its open attempts are fenced and its evidence goes. The
/// inbox's own exit already retired its messages.
pub(super) fn exit(
    st: &mut HubState,
    key: &AgentKey,
    now: Instant,
) -> Option<Arc<GenerationSpool>> {
    st.receipts.floors.remove(key);
    forget_turns(st, key);
    let told = st.receipts.told.remove(key);
    let generation = st.receipts.gens.remove(key);
    if told.is_some_and(|told| told != InboxCardNotices::default()) {
        send_notices(st, key, InboxCardNotices::default());
    }
    let mut generation = generation?;
    generation.model.exit(now);
    generation.model.drain_effects();
    generation.spool
}

/// Apply the model's verdicts for `key` to the inbox, act on its effects, and
/// tell clients when the card's notices changed.
pub(super) fn apply(st: &mut HubState, key: &AgentKey, now: Instant, out: &mut Outbox) {
    let Some(generation) = st.receipts.gens.get_mut(key) else {
        return;
    };
    let effects = generation.model.drain_effects();
    let mut returned: HashSet<String> = HashSet::new();
    for effect in effects {
        match effect {
            Effect::Revoke { id, a, e } => {
                let Some(generation) = st.receipts.gens.get(key) else {
                    continue;
                };
                if let Some(link) = generation
                    .model
                    .attempt_rx(&id, a, e)
                    .and_then(|rx| generation.links.get(&rx))
                {
                    send(
                        out,
                        link,
                        InboxWaitReply {
                            revoke: vec![InboxAttemptRef { id, a, e }],
                            ..InboxWaitReply::default()
                        },
                    );
                }
            }
            Effect::DeliveredAfterAll { id, despite_cancel } => {
                delivered_after_all(st, key, &id, despite_cancel);
            }
            Effect::WitnessViolation { id, a, e } => {
                warn!(message_id = %id, a, e, "inbox: a commit contradicted a not-handed witness");
            }
            Effect::AlsoReachedOldWorker { id, a } => {
                also_reached(st, key, &id, a);
            }
            Effect::ReturnToPool { id, .. } => {
                returned.insert(id);
            }
            Effect::HeldLong { id, revision } => {
                info!(message_id = %id, revision, "inbox: a deck message held for an hour");
            }
            Effect::EvidencePaused { reserved } => {
                if let Some(generation) = st.receipts.gens.get_mut(key) {
                    if !generation.paused {
                        warn!(pane_id = %key.0, reserved, "inbox: hand-outs paused for evidence room");
                    }
                    generation.paused = true;
                }
            }
            Effect::Reclaimed { id, a, e } => {
                let Some(generation) = st.receipts.gens.get(key) else {
                    continue;
                };
                generation
                    .evidence
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .store
                    .forget(&id, a, e);
                if let Some(spool) = generation.spool.clone() {
                    st.receipts.removals.push((spool, id, a, e));
                }
            }
        }
    }
    sync(st, key, now, &returned);
    super::drain_events(st);
    broadcast_notices(st, key, now);
}

/// Whether `key`'s receipts came through a receiver on `slot`.
fn generation_on(st: &HubState, key: &AgentKey, slot: Slot) -> bool {
    st.receipts
        .gens
        .get(key)
        .is_some_and(|generation| generation.model.has_slot(slot))
}

/// Mirror each of `key`'s model states into its inbox copy.
fn sync(st: &mut HubState, key: &AgentKey, now: Instant, returned: &HashSet<String>) {
    let Some(generation) = st.receipts.gens.get(key) else {
        return;
    };
    let states: Vec<(String, Delivery)> = generation
        .model
        .ids()
        .into_iter()
        .filter_map(|id| generation.model.state(&id).map(|state| (id, state)))
        .collect();
    for (id, state) in states {
        let Some(message) = st.inbox.get(&id, &key.0) else {
            continue;
        };
        if message.owner != Some(AssignmentOwner::Receipt) || message.state.is_terminal() {
            continue;
        }
        let fetched = matches!(message.state, MessageState::Fetched { .. });
        let admits = message
            .recipient
            .as_ref()
            .is_some_and(|r| r.admits(&key.0, &key.1));
        if !admits {
            continue;
        }
        if returned.contains(&id) {
            st.inbox.requeue_receipt(now, &id, &key.0);
            continue;
        }
        match state {
            Delivery::Settled => {
                // G9: a Pi extension's commit is confirmed as an extension's.
                let how = if generation_on(st, key, Slot::Pi) {
                    AckHow::ExtensionConfirmed
                } else {
                    AckHow::PluginConfirmed
                };
                st.inbox.ack(now, &id, &key.0, &key.1, how);
            }
            Delivery::Queued if fetched => {
                st.inbox.requeue_receipt(now, &id, &key.0);
            }
            Delivery::Undeliverable(Verdict::MaxDeliver) => {
                st.inbox
                    .retire(now, &id, &key.0, UndeliverableReason::MaxDeliver);
            }
            Delivery::Undeliverable(Verdict::Cancelled) => {
                let handed = st
                    .receipts
                    .gens
                    .get(key)
                    .and_then(|generation| generation.model.delivery(&id))
                    .is_some_and(|d| {
                        d.attempts
                            .iter()
                            .any(|at| at.a == d.a && at.stage != Stage::NotHanded)
                    });
                if handed {
                    st.cancelled_handed.insert(id.clone());
                }
                st.inbox
                    .retire(now, &id, &key.0, UndeliverableReason::Cancelled);
            }
            Delivery::Uncertain(_) => uncertain_notice(st, key, &id),
            _ => {}
        }
    }
}

/// §6.6: a delegated task whose delivery became uncertain tells its
/// orchestrator once.
fn uncertain_notice(st: &mut HubState, key: &AgentKey, id: &str) {
    let Some(watch) = st.delegates.get(id) else {
        return;
    };
    let Some(to) = watch.orchestrator.clone() else {
        return;
    };
    let role = watch.role.clone();
    let Some(generation) = st.receipts.gens.get_mut(key) else {
        return;
    };
    if !generation.uncertain_told.insert(id.to_string()) {
        return;
    }
    st.owed_notices.push((
        to,
        super::UNCERTAIN_SUBJECT,
        super::compose_uncertain_notice(id, &role),
    ));
}

/// Rule 1's correction (§2): a sender told otherwise hears the message
/// reached its agent after all.
fn delivered_after_all(st: &mut HubState, key: &AgentKey, id: &str, despite_cancel: bool) {
    let told = st
        .receipts
        .gens
        .get_mut(key)
        .is_some_and(|generation| generation.uncertain_told.remove(id));
    let Some(watch) = st.delegates.get(id) else {
        if let Some((to, role)) = st.cancelled_tasks.remove(id) {
            st.owed_notices.push((
                to,
                LATE_ACK_SUBJECT,
                super::compose_after_all_notice(id, &role, true),
            ));
        }
        return;
    };
    if !(told || despite_cancel || watch.notified) {
        return;
    }
    if let Some(to) = watch.orchestrator.clone() {
        let role = watch.role.clone();
        st.owed_notices.push((
            to,
            LATE_ACK_SUBJECT,
            super::compose_after_all_notice(id, &role, despite_cancel),
        ));
    }
}

/// T3: an earlier holder of an offer committed after it was reassigned — the
/// task reached it too (§2), and its offerer is told.
fn also_reached(st: &mut HubState, key: &AgentKey, id: &str, a: u32) {
    warn!(message_id = %id, a, pane_id = %key.0, "inbox: a reassigned offer also reached its earlier holder");
    let Some(offer) = st.offers.get(id) else {
        return;
    };
    let role = offer
        .roles
        .get(&key.0)
        .cloned()
        .unwrap_or_else(|| key.0.clone());
    let to = offer.offerer.clone();
    st.owed_notices.push((
        to,
        super::ALSO_REACHED_SUBJECT,
        super::compose_also_reached_notice(id, &role),
    ));
}

/// The card notices for `key` now (§6.5).
pub(super) fn notices(st: &HubState, key: &AgentKey, now: Instant) -> InboxCardNotices {
    let mut notices = InboxCardNotices {
        integration: st.receipts.floors.get(key).and_then(|f| f.notice(now)),
        ..InboxCardNotices::default()
    };
    let Some(generation) = st.receipts.gens.get(key) else {
        return notices;
    };
    notices.receipts_degraded = generation.degraded.clone();
    if generation.paused {
        notices.evidence_paused = Some(generation.model.holding_evidence());
    }
    for id in generation.model.ids() {
        let Some(delivery) = generation.model.delivery(&id) else {
            continue;
        };
        let kind = match delivery.state {
            Delivery::Uncertain(UncertainReason::Lapsed) => AttentionKind::Uncertain,
            Delivery::Uncertain(UncertainReason::OutOfAttempts) => AttentionKind::OutOfAttempts,
            _ if delivery.not_started() => AttentionKind::NotStarted,
            _ if delivery.held_long() => AttentionKind::HeldLong,
            _ => continue,
        };
        let Some(message) = st.inbox.get(&id, &key.0) else {
            continue;
        };
        if message.state.is_terminal() {
            continue;
        }
        notices.attention.push(InboxAttentionItem {
            message_id: id.clone(),
            revision: delivery.revision,
            kind,
            from_label: kind_sender_label(message.kind.name(), &message.from, message.body_trust),
            subject: message.subject.clone(),
            task_id: None,
        });
    }
    notices
}

/// Tell clients `key`'s card notices when they changed since last told.
pub(super) fn broadcast_notices(st: &mut HubState, key: &AgentKey, now: Instant) {
    let current = notices(st, key, now);
    if st.receipts.told.get(key) == Some(&current)
        || (current == InboxCardNotices::default() && !st.receipts.told.contains_key(key))
    {
        return;
    }
    st.receipts.told.insert(key.clone(), current.clone());
    send_notices(st, key, current);
}

fn send_notices(st: &HubState, key: &AgentKey, notices: InboxCardNotices) {
    let Some(tx) = st.broadcast.as_ref() else {
        return;
    };
    let named = notices.attention.first().map(|item| {
        (
            item.message_id.clone(),
            item.subject.clone(),
            item.from_label.clone(),
        )
    });
    let (message_id, subject, from_label) = named.unwrap_or_default();
    let _ = tx.send(BroadcastMsg::Inbox(InboxNotice {
        pane_id: key.0.clone(),
        agent_id: Some(key.1.clone()),
        message_id,
        subject,
        from_label,
        state: crate::inbox::StateName::Fetched,
        reason: None,
        returned: false,
        wake_stalled: false,
        wake_update: false,
        attention_update: true,
        notices,
    }));
}

/// T5/T6 (§6.4): the user's Retry or Cancel on `id` at `revision`, applied by
/// the hub-serialised compare-and-transition.
pub(super) fn action(
    st: &mut HubState,
    key: &AgentKey,
    id: &str,
    revision: u64,
    retry: bool,
    now: Instant,
    out: &mut Outbox,
) -> crate::inbox_attempts::ActionOutcome {
    use crate::inbox_attempts::ActionOutcome;
    let Some(generation) = st.receipts.gens.get_mut(key) else {
        return ActionOutcome::Unknown;
    };
    let outcome = if retry {
        generation.model.retry(now, id, 1, revision)
    } else {
        generation.model.cancel(now, id, 1, revision)
    };
    if !retry
        && matches!(outcome, ActionOutcome::Applied { .. })
        && let Some(watch) = st.delegates.get(id)
        && let Some(to) = watch.orchestrator.clone()
    {
        st.cancelled_tasks
            .insert(id.to_string(), (to, watch.role.clone()));
    }
    apply(st, key, now, out);
    dispatch(st, key, now, out);
    outcome
}

/// PRD #1666 §11 (a): how long the capability probe may run.
pub const PROBE_DEADLINE: Duration = Duration::from_secs(5);
/// Overrides [`PROBE_DEADLINE`], in milliseconds (tests).
pub const ENV_PROBE_DEADLINE_MS: &str = "DOT_AGENT_DECK_INBOX_PROBE_DEADLINE_MS";
/// PRD #1666 §11 (a): the most of the probe's output read, per stream.
pub const PROBE_OUTPUT_CAP: u64 = 64 * 1024;

/// What the capability probe proved about the CLI an adapter runs (§11 a).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeVerdict {
    /// Its help names `--adapter-integration`.
    Supported,
    /// clap refused `--adapter-integration` as an unexpected argument.
    CliTooOld,
    /// A timeout, a spawn failure, any other exit or output: nothing proven.
    Unknown,
}

/// clap's refusal of the integration flag, the whole first line of its
/// diagnostic on exit 2.
const CLAP_REFUSAL: &str = "error: unexpected argument '--adapter-integration' found";
/// clap's usage-error exit status.
const CLAP_USAGE_EXIT: i32 = 2;

/// PRD #1666 §11 (a): run `<binary> inbox wait --adapter-integration 2
/// --help` with a literal argv (no shell), within `deadline`, and say what it
/// proves. Each output stream is read up to [`PROBE_OUTPUT_CAP`] and drained
/// past it, so a chatty CLI never blocks on a full pipe; a CLI still running
/// at the deadline is killed and reaped, and output that is still open then
/// (another process holding it) proves nothing. Only clap's exact refusal of
/// the flag on its usage-error exit proves it missing, and only a help line
/// listing the flag proves it supported; anything else, an old help that
/// exits 0 without the flag or a diagnostic in other words included, is
/// unknown. Blocking: run it off the executor. What is bounded is the
/// return and the bytes kept, not the readers: a reader of a stream another
/// process still holds open is left behind, and lives until that process
/// closes it.
pub fn probe_cli(binary: &std::path::Path, deadline: Duration) -> ProbeVerdict {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let integration = super::ADAPTER_INTEGRATION.to_string();
    let started = Instant::now();
    let Ok(mut child) = Command::new(binary)
        .args([
            "inbox",
            "wait",
            "--adapter-integration",
            integration.as_str(),
            "--help",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    else {
        return ProbeVerdict::Unknown;
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let read = |pipe: Option<Box<dyn Read + Send>>, err: bool| {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut text = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.by_ref().take(PROBE_OUTPUT_CAP).read_to_end(&mut text);
                let _ = std::io::copy(&mut pipe, &mut std::io::sink());
            }
            let _ = tx.send((err, String::from_utf8_lossy(&text).into_owned()));
        });
    };
    read(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
        false,
    );
    read(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
        true,
    );
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let Some(status) = status else {
        return ProbeVerdict::Unknown;
    };
    let (mut stdout, mut stderr) = (None, None);
    while stdout.is_none() || stderr.is_none() {
        let left = deadline.saturating_sub(started.elapsed());
        match rx.recv_timeout(left.max(Duration::from_millis(50))) {
            Ok((true, text)) => stderr = Some(text),
            Ok((false, text)) => stdout = Some(text),
            Err(_) => return ProbeVerdict::Unknown,
        }
    }
    let (stdout, stderr) = (stdout.unwrap_or_default(), stderr.unwrap_or_default());
    let lists_flag = stdout.lines().any(|line| {
        plain(line)
            .trim_start()
            .starts_with("--adapter-integration ")
    });
    let first = stderr.lines().next().map(plain);
    if status.success() && lists_flag {
        ProbeVerdict::Supported
    } else if status.code() == Some(CLAP_USAGE_EXIT) && first.as_deref() == Some(CLAP_REFUSAL) {
        ProbeVerdict::CliTooOld
    } else {
        ProbeVerdict::Unknown
    }
}

/// One line of the probe's output as clap means it, on every platform: a
/// trailing carriage return (a CRLF the line split left behind, or a lone one)
/// and complete ANSI SGR sequences (`ESC [` digits and `;`, then `m`), the
/// styling clap may be told to force, are dropped. Any other escape, cursor
/// control included, or one cut short, is kept, so such a line matches
/// nothing; otherwise the comparison stays exact.
fn plain(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line.trim_end_matches('\r');
    while let Some(at) = rest.find('\u{1b}') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        let sgr = tail.strip_prefix("\u{1b}[").and_then(|params| {
            let end = params.find(|c: char| !(c.is_ascii_digit() || c == ';'))?;
            params[end..].starts_with('m').then_some(2 + end + 1)
        });
        match sgr {
            Some(len) => rest = &tail[len..],
            None => {
                out.push('\u{1b}');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// [`PROBE_DEADLINE`] or its override.
pub fn probe_deadline_from_env() -> Duration {
    std::env::var(ENV_PROBE_DEADLINE_MS)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map_or(PROBE_DEADLINE, Duration::from_millis)
}
