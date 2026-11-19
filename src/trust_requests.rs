//! Issue #2330: asking for an orchestration's approval inside the deck.
//!
//! PRD #1885 made the daemon refuse every unattended start of an orchestration
//! whose `.dot-agent-deck.toml` text is not the approved text — an agent's
//! `dispatch --orchestration`, a `delegate` that respawns a role, `pane
//! restart` / `pane spawn`, a schedule firing, **Run now**, a scheduled issue
//! dispatch — and print the `dot-agent-deck trust approve` command to run "from
//! a terminal outside the deck". This module is what lets the user answer that
//! refusal inside the deck instead: each refusal raises a **request**, which
//! the daemon tells every attached client about, and a client shows the review
//! it already has for the New Agent form. Approving goes through the existing
//! `ApproveProjectTrust` (so the approval is recorded exactly as before);
//! denying goes through [`crate::daemon_protocol::AttachRequest::DenyTrustRequest`].
//!
//! # The rules ([`TrustRequestHub`])
//!
//! - **Once per change.** A request is keyed by project, orchestration and the
//!   digest of the text ([`RequestKey`]) — the key an approval covers. Raising
//!   a key that is already pending changes nothing, so a retry opens no second
//!   dialog. A new text for the same project and orchestration replaces the
//!   pending request for the old one, which approving could no longer record.
//! - **Deny is remembered** for that exact key until the daemon restarts, so a
//!   schedule firing every few minutes does not re-open the dialog each time.
//!   The New Agent form and `trust approve` can still approve it; an approval
//!   for the project and orchestration forgets the denial.
//! - **Memory only.** Pending requests and denials are never written to disk; a
//!   daemon restart drops them, and the next refused attempt raises its request
//!   again. Approvals stay in the approval store and survive restarts as before.
//! - **No client attached:** the request stays pending; a client learns of it
//!   from its next `Hello` (or `SubscribeEventsWithSnapshot`) reply.
//! - **The refused agents are told the answer** (issue #2348). An agent whose
//!   `dispatch`, `delegate`, `pane restart` or `pane spawn` was refused is
//!   recorded against the request ([`RefusedCaller`]), and when the request
//!   closes each of them is sent one deck notice through its inbox: approved,
//!   retry now; declined; or closed unanswered because the config changed
//!   ([`Answer`]). The approval still starts nothing: the refused request may
//!   be stale by then, so the agent decides whether to retry. A caller that is
//!   gone by then is skipped, and a schedule (no agent) is told nothing.
//!
//! # Only the serving daemon raises requests
//!
//! The hub that [`refusal_for`] consults is the process-wide one, and it serves
//! only once [`install`] has given it the daemon's broadcast channel, which
//! `daemon serve` does. Everywhere else — the TUI, the CLI, an in-process test
//! daemon — [`refusal_for`] answers the PRD #1885 refusal unchanged.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::broadcast;

use crate::daemon_protocol::{TrustRequest, TrustRequestList};
use crate::event::BroadcastMsg;
use crate::project_trust::TrustCheck;

/// The most requests pending at once. A real deck has a handful (one per
/// orchestration whose text changed); past this, a refusal names the CLI
/// command only and raises nothing, so the pending list cannot grow without
/// bound however many projects an agent dispatches into.
pub const MAX_PENDING_REQUESTS: usize = 32;

/// The most denials remembered. Past it, the oldest is forgotten, which at
/// worst asks about that text once more.
pub const MAX_DENIALS: usize = 256;

/// The longest [`TrustRequest::asked_by`] label kept, in characters.
pub const MAX_ASKED_BY_CHARS: usize = 128;

/// The most refused callers recorded against one request (issue #2348). Past
/// it, a further caller is refused as usual but not told the answer.
pub const MAX_WAITING_CALLERS: usize = 64;

/// How often the serving daemon re-checks pending requests against the
/// approval store and the configs ([`run_sweep`]): an approval made from the
/// CLI, or a config edited since, closes the request in every client within
/// this long.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(2);

/// What a request is about: one exact text of one orchestration of one project.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RequestKey {
    pub project: String,
    pub orchestration: String,
    pub digest: String,
}

impl RequestKey {
    pub fn of(check: &TrustCheck) -> Self {
        RequestKey {
            project: check.project.display().to_string(),
            orchestration: check.orchestration.clone(),
            digest: check.digest(),
        }
    }

    fn of_request(request: &TrustRequest) -> Self {
        RequestKey {
            project: request.project.clone(),
            orchestration: request.orchestration.clone(),
            digest: request.digest.clone(),
        }
    }

    fn same_orchestration(&self, other: &RequestKey) -> bool {
        self.project == other.project && self.orchestration == other.orchestration
    }
}

/// What raising a request for a refused attempt did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Raised {
    /// A request for this text is pending (raised now, or already).
    Pending,
    /// The user declined this text in the deck since the daemon started.
    Declined,
    /// Nothing was raised: too many requests are pending already.
    Full,
}

/// What [`TrustRequestHub::deny`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denied {
    /// The request was pending and is now declined.
    Declined,
    /// No such request is pending: another client answered it first, or it
    /// was settled meanwhile.
    Unknown,
}

/// Issue #2348: an agent whose attempt was refused for an unapproved
/// orchestration, told the answer when its request closes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusedCaller {
    /// The caller's pane.
    pub pane_id: String,
    /// The agent that held the pane when it was refused: a notice is posted
    /// to that agent only, so a pane that changed hands is skipped.
    pub agent_id: String,
    /// What it ran — `dispatch`, `delegate`, `pane restart`, `pane spawn` —
    /// named in the notice ("Retry your dispatch now").
    pub verb: String,
}

impl RefusedCaller {
    pub fn new(
        pane_id: impl Into<String>,
        agent_id: impl Into<String>,
        verb: impl Into<String>,
    ) -> Self {
        Self {
            pane_id: pane_id.into(),
            agent_id: agent_id.into(),
            verb: verb.into(),
        }
    }

    fn same_agent(&self, other: &RefusedCaller) -> bool {
        self.pane_id == other.pane_id && self.agent_id == other.agent_id
    }
}

/// Issue #2348: how a request closed, as its refused callers are told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// Approved — in a client, from `trust approve`, or found approved by a
    /// later check. The caller may retry now.
    Approved,
    /// The user declined it in a client.
    Declined,
    /// Closed without an answer: the config now reads other text that
    /// nobody is being asked about, or no longer names the orchestration.
    /// A retry asks again.
    Withdrawn,
}

/// Issue #2348: one deck notice to one refused caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallerNotice {
    pub pane_id: String,
    pub agent_id: String,
    pub answer: Answer,
    pub subject: &'static str,
    pub body: String,
}

/// Where the serving daemon posts [`CallerNotice`]s ([`inbox_sink`] in
/// `daemon serve`), answering what became of the notice. Called off the hub's
/// lock.
pub type NoticeSink = Arc<dyn Fn(&CallerNotice) -> Posted + Send + Sync>;

/// What a [`NoticeSink`] did with a notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Posted {
    /// It is in the caller's inbox.
    Delivered,
    /// The agent no longer holds its pane: nobody is left to tell.
    Gone,
    /// The caller is still there, but its inbox (or the deck) refused the
    /// notice, full for now: it is tried again every [`SWEEP_INTERVAL`] until
    /// it goes in or the caller is gone, since the refusal told the caller to
    /// wait for it (PR #2351 review).
    Refused,
}

/// The most notices kept for another try ([`Posted::Refused`]). Past it the
/// oldest is given up, with a warning.
pub const MAX_RETRYING_NOTICES: usize = 256;

/// The pending requests and the remembered denials of one daemon.
#[derive(Debug, Default)]
pub struct TrustRequestHub {
    next_id: u64,
    revision: u64,
    pending: Vec<TrustRequest>,
    /// Oldest first, so the cap forgets the oldest.
    denied: Vec<RequestKey>,
    /// Issue #2348: the callers each pending request (by id) refused, in the
    /// order they were refused.
    waiting: BTreeMap<u64, Vec<RefusedCaller>>,
    /// Issue #2348: notices owed by the changes made since the last
    /// [`Self::take_notices`].
    outbox: Vec<CallerNotice>,
}

impl TrustRequestHub {
    /// Every pending request, oldest first, at the current revision.
    pub fn list(&self) -> TrustRequestList {
        TrustRequestList {
            revision: self.revision,
            requests: self.pending.clone(),
        }
    }

    fn changed(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    /// Issue #2348: the notices owed since the last call, oldest first.
    pub fn take_notices(&mut self) -> Vec<CallerNotice> {
        std::mem::take(&mut self.outbox)
    }

    /// Whether `caller` is recorded against the pending request for `key`.
    pub fn is_waiting(&self, key: &RequestKey, caller: &RefusedCaller) -> bool {
        self.pending
            .iter()
            .find(|r| RequestKey::of_request(r) == *key)
            .and_then(|r| self.waiting.get(&r.id))
            .is_some_and(|callers| callers.contains(caller))
    }

    /// Record `caller` against request `id`, once per agent and verb.
    fn record_caller(&mut self, id: u64, caller: &RefusedCaller) {
        let callers = self.waiting.entry(id).or_default();
        if callers.contains(caller) || callers.len() >= MAX_WAITING_CALLERS {
            return;
        }
        callers.push(caller.clone());
    }

    /// `request` closed with `answer`: owe each of its callers one notice,
    /// naming every verb that agent was refused but `except`, the attempt
    /// already starting it.
    fn told(&mut self, request: &TrustRequest, answer: Answer, except: Option<&RefusedCaller>) {
        let callers = self.waiting.remove(&request.id).unwrap_or_default();
        let mut agents: Vec<(RefusedCaller, Vec<String>)> = Vec::new();
        for caller in callers {
            // Only the attempt that found it approved: the same agent's other
            // refused verbs are still waiting (PR #2351 review).
            if except == Some(&caller) {
                continue;
            }
            match agents.iter_mut().find(|(a, _)| a.same_agent(&caller)) {
                Some((_, verbs)) => verbs.push(caller.verb),
                None => {
                    let verb = caller.verb.clone();
                    agents.push((caller, vec![verb]));
                }
            }
        }
        for (caller, verbs) in agents {
            let (subject, body) = crate::project_trust::request_answer_notice(
                &request.project,
                &request.orchestration,
                &verbs,
                answer,
            );
            self.outbox.push(CallerNotice {
                pane_id: caller.pane_id,
                agent_id: caller.agent_id,
                answer,
                subject,
                body,
            });
        }
    }

    /// Raise a request for `check`'s text, refused while `asked_by` (what
    /// tried to start it: `dispatch`, a schedule's name, `pane restart`, …).
    /// Answers what the refusal should say, and whether the list changed.
    pub fn raise(&mut self, check: &TrustCheck, asked_by: &str) -> (Raised, bool) {
        self.raise_for(check, asked_by, None)
    }

    /// [`Self::raise`], recording `caller` — the agent refused, when an agent
    /// was — so it is told the answer when the request closes (issue #2348).
    pub fn raise_for(
        &mut self,
        check: &TrustCheck,
        asked_by: &str,
        caller: Option<&RefusedCaller>,
    ) -> (Raised, bool) {
        let key = RequestKey::of(check);
        if self.denied.contains(&key) {
            return (Raised::Declined, false);
        }
        if let Some(id) = self
            .pending
            .iter()
            .find(|r| RequestKey::of_request(r) == key)
            .map(|r| r.id)
        {
            if let Some(caller) = caller {
                self.record_caller(id, caller);
            }
            return (Raised::Pending, false);
        }
        // A pending request for an older text of the same orchestration can no
        // longer be approved as shown; this one takes its place, and so do the
        // callers waiting on it, since their retry reads the config as it now
        // is (issue #2348).
        let (replaced_requests, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|r| RequestKey::of_request(r).same_orchestration(&key));
        self.pending = kept;
        let replaced = !replaced_requests.is_empty();
        if self.pending.len() >= MAX_PENDING_REQUESTS {
            if replaced {
                self.changed();
                // Nothing takes their place: their callers are told the
                // request closed unanswered, and a retry asks again.
                for old in &replaced_requests {
                    self.told(old, Answer::Withdrawn, None);
                }
            }
            return (Raised::Full, replaced);
        }
        // Carried over only from a request raised for the same config file: a
        // linked worktree shares the project's key but reads its own file, so
        // another worktree's callers would retry their own text, which this
        // request's answer says nothing about. They are told the request
        // closed unanswered instead (PR #2351 review).
        let config_dir = check.config_dir.display().to_string();
        let mut carried: Vec<RefusedCaller> = Vec::new();
        for old in &replaced_requests {
            if old.config_dir == config_dir {
                carried.extend(self.waiting.remove(&old.id).unwrap_or_default());
            } else {
                self.told(old, Answer::Withdrawn, None);
            }
        }
        self.next_id = self.next_id.wrapping_add(1);
        let asked_by: String = crate::untrusted_text::escape_control_and_bidi(asked_by)
            .chars()
            .take(MAX_ASKED_BY_CHARS)
            .collect();
        self.pending.push(TrustRequest {
            id: self.next_id,
            project: key.project,
            config_dir: check.config_dir.display().to_string(),
            orchestration: key.orchestration,
            digest: key.digest,
            asked_by,
        });
        let id = self.next_id;
        for old in carried {
            self.record_caller(id, &old);
        }
        if let Some(caller) = caller {
            self.record_caller(id, caller);
        }
        self.changed();
        (Raised::Pending, true)
    }

    /// The user declined request `id`, shown with text `digest`: close it, and
    /// remember its text. A request whose text is not `digest` is not the one
    /// the user saw (a restarted daemon numbers anew), so nothing is declined.
    pub fn deny(&mut self, id: u64, digest: &str) -> Denied {
        let Some(at) = self
            .pending
            .iter()
            .position(|r| r.id == id && r.digest == digest)
        else {
            return Denied::Unknown;
        };
        let request = self.pending.remove(at);
        let key = RequestKey::of_request(&request);
        if !self.denied.contains(&key) {
            if self.denied.len() >= MAX_DENIALS {
                self.denied.remove(0);
            }
            self.denied.push(key);
        }
        self.told(&request, Answer::Declined, None);
        self.changed();
        Denied::Declined
    }

    /// `project`'s `orchestration` was approved as text `digest` (by a client,
    /// or found approved by a check): close its pending request and forget a
    /// denial of that same text. A denial of another text of the orchestration
    /// stays — the user declined that text, and approving a different one (a
    /// linked worktree's, say) does not answer it. Answers whether the list
    /// changed.
    pub fn settle_approved(&mut self, project: &str, orchestration: &str, digest: &str) -> bool {
        self.settle_approved_except(project, orchestration, digest, None)
    }

    /// [`Self::settle_approved`], telling every caller the request refused
    /// that it was approved — but `except`, the caller whose own attempt just
    /// found it approved and is already starting it (issue #2348).
    pub fn settle_approved_except(
        &mut self,
        project: &str,
        orchestration: &str,
        digest: &str,
        except: Option<&RefusedCaller>,
    ) -> bool {
        let key = RequestKey {
            project: project.to_string(),
            orchestration: orchestration.to_string(),
            digest: digest.to_string(),
        };
        self.denied.retain(|d| *d != key);
        let (closed, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|r| RequestKey::of_request(r) == key);
        self.pending = kept;
        for request in &closed {
            self.told(request, Answer::Approved, except);
        }
        let changed = !closed.is_empty();
        if changed {
            self.changed();
        }
        changed
    }

    /// Drop the pending requests `stale` names (by id), each closed with the
    /// answer it names ([`stale_requests_blocking`]). Answers whether the
    /// list changed.
    pub fn drop_requests(&mut self, stale: &BTreeMap<u64, Answer>) -> bool {
        let (closed, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|r| stale.contains_key(&r.id));
        self.pending = kept;
        for request in &closed {
            self.told(request, stale[&request.id], None);
        }
        let changed = !closed.is_empty();
        if changed {
            self.changed();
        }
        changed
    }
}

/// The process-wide hub and the channel its changes are broadcast on, which is
/// `None` until [`install`] (so nothing outside `daemon serve` raises).
struct Served {
    hub: TrustRequestHub,
    tx: Option<broadcast::Sender<BroadcastMsg>>,
    /// Issue #2348: where the refused callers' notices go.
    sink: Option<NoticeSink>,
    /// Issue #2348: notices a full inbox refused, tried again by the sweep.
    retrying: Vec<CallerNotice>,
}

static SERVED: Mutex<Served> = Mutex::new(Served {
    hub: TrustRequestHub {
        next_id: 0,
        revision: 0,
        pending: Vec::new(),
        denied: Vec::new(),
        waiting: BTreeMap::new(),
        outbox: Vec::new(),
    },
    tx: None,
    sink: None,
    retrying: Vec::new(),
});

fn served() -> std::sync::MutexGuard<'static, Served> {
    SERVED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Broadcast `served`'s list. Sent under the hub's lock, so two changes are
/// broadcast in the order they were made; each carries the revision it was
/// made at, and a client keeps the highest it has seen.
fn broadcast(served: &Served) {
    if let Some(tx) = &served.tx {
        let _ = tx.send(BroadcastMsg::TrustRequests(served.hub.list()));
    }
}

/// `daemon serve`: raise requests from here on, broadcast every change of
/// them on `tx`, and post each refused caller's notice through `sink`.
pub fn install(tx: broadcast::Sender<BroadcastMsg>, sink: Option<NoticeSink>) {
    let mut served = served();
    served.tx = Some(tx);
    served.sink = sink;
}

/// Take the notices `served`'s last change owes, with the sink to post them
/// through; post them with [`deliver`] once the hub's lock is released.
fn owed(served: &mut Served) -> Option<(NoticeSink, Vec<CallerNotice>)> {
    let notices = served.hub.take_notices();
    if notices.is_empty() {
        return None;
    }
    Some((served.sink.clone()?, notices))
}

/// Post `owed`'s notices: on the blocking pool when a runtime is running (a
/// post may save a body to disk, and callers may hold the deck's state), and
/// in place otherwise.
fn deliver(owed: Option<(NoticeSink, Vec<CallerNotice>)>) {
    let Some((sink, notices)) = owed else {
        return;
    };
    let post = move || {
        let refused: Vec<CallerNotice> = notices
            .into_iter()
            .filter(|notice| sink(notice) == Posted::Refused)
            .collect();
        if refused.is_empty() {
            return;
        }
        let mut served = served();
        for notice in refused {
            if served.retrying.len() >= MAX_RETRYING_NOTICES {
                let dropped = served.retrying.remove(0);
                tracing::warn!(
                    pane_id = %crate::config_validation::escape_id_for_log(&dropped.pane_id),
                    "trust request: too many answers are waiting for room in their callers' \
                     inboxes; the oldest is given up"
                );
            }
            served.retrying.push(notice);
        }
    };
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn_blocking(post);
        }
        Err(_) => post(),
    }
}

/// The notices a full inbox refused, with the sink to try them again through
/// ([`deliver`]); `None` when there are none.
fn take_retrying() -> Option<(NoticeSink, Vec<CallerNotice>)> {
    let mut served = served();
    let notices = std::mem::take(&mut served.retrying);
    served
        .sink
        .clone()
        .filter(|_| !notices.is_empty())
        .map(|sink| (sink, notices))
}

/// Issue #2348: the serving daemon's sink — post each notice to its caller's
/// inbox as a deck notice, only while the agent it names still holds its
/// pane. A caller that is gone is skipped.
pub fn inbox_sink(registry: std::sync::Weak<crate::agent_pty::AgentPtyRegistry>) -> NoticeSink {
    Arc::new(move |notice: &CallerNotice| {
        let Some(registry) = registry.upgrade() else {
            return Posted::Gone;
        };
        let holds = || {
            registry.pane_current_agent_id(&notice.pane_id).as_deref()
                == Some(notice.agent_id.as_str())
        };
        if !holds() {
            tracing::info!(
                pane_id = %crate::config_validation::escape_id_for_log(&notice.pane_id),
                answer = ?notice.answer,
                "trust request: a refused caller is gone, so it is not told the answer"
            );
            return Posted::Gone;
        }
        let to = crate::inbox::Recipient::bound(&notice.pane_id, &notice.agent_id);
        match crate::inbox_hub::post_notice_to_live(
            &registry,
            to,
            notice.subject,
            notice.body.clone(),
        ) {
            Ok(id) => {
                tracing::info!(
                    pane_id = %crate::config_validation::escape_id_for_log(&notice.pane_id),
                    message_id = %id,
                    answer = ?notice.answer,
                    "trust request: told a refused caller the answer"
                );
                Posted::Delivered
            }
            Err(_) if !holds() => Posted::Gone,
            Err(error) => {
                tracing::info!(
                    pane_id = %crate::config_validation::escape_id_for_log(&notice.pane_id),
                    %error,
                    answer = ?notice.answer,
                    "trust request: a refused caller's inbox refused the answer; it is tried \
                     again"
                );
                Posted::Refused
            }
        }
    })
}

/// Whether this process raises requests (it is a serving daemon).
pub fn is_served() -> bool {
    served().tx.is_some()
}

/// The pending requests, for a `Hello` or snapshot reply. `None` outside a
/// serving daemon, so such a reply carries no list.
pub fn list() -> Option<TrustRequestList> {
    let served = served();
    served.tx.as_ref()?;
    Some(served.hub.list())
}

/// The refusal for `check`, as an unattended path reports it, or `None` when
/// it is approved. In a serving daemon, an unapproved check raises a request
/// ([`TrustRequestHub::raise`]) and the refusal says so — or says the user
/// declined it — and an approved one closes any request pending for its text.
/// Elsewhere, [`TrustCheck::refusal`] unchanged.
///
/// `caller` is the agent that tried, when an agent did (issue #2348): it is
/// recorded against the request and told the answer when the request closes.
pub fn refusal_for(
    check: &TrustCheck,
    asked_by: &str,
    caller: Option<&RefusedCaller>,
) -> Option<String> {
    let mut served = served();
    if served.tx.is_none() {
        return check.refusal();
    }
    if check.verdict.is_approved() {
        // The common case — every approved delegate — has nothing to settle.
        if served.hub.pending.is_empty() && served.hub.denied.is_empty() {
            return None;
        }
        let digest = check.digest();
        let project = check.project.display().to_string();
        if served
            .hub
            .settle_approved_except(&project, &check.orchestration, &digest, caller)
        {
            broadcast(&served);
        }
        let owed = owed(&mut served);
        drop(served);
        deliver(owed);
        return None;
    }
    let (raised, changed) = served.hub.raise_for(check, asked_by, caller);
    if changed {
        broadcast(&served);
    }
    // Issue #2348: the refusal promises a notice only to a caller the hub
    // recorded (and has somewhere to post it).
    let told = served.sink.is_some()
        && caller.is_some_and(|caller| served.hub.is_waiting(&RequestKey::of(check), caller));
    let owed = owed(&mut served);
    drop(served);
    deliver(owed);
    Some(match raised {
        Raised::Pending => crate::project_trust::requested_refusal_text(check, told),
        Raised::Declined => crate::project_trust::declined_refusal_text(check),
        Raised::Full => check.refusal()?,
    })
}

/// A client approved `orchestration` of the config in `config_dir` as text
/// `digest` (`ApproveProjectTrust`): close its request.
pub fn note_approved(project: &Path, orchestration: &str, digest: &str) {
    let mut served = served();
    if served.tx.is_none() {
        return;
    }
    if served
        .hub
        .settle_approved(&project.display().to_string(), orchestration, digest)
    {
        broadcast(&served);
    }
    let owed = owed(&mut served);
    drop(served);
    deliver(owed);
}

/// The user declined request `id` in a client (`DenyTrustRequest`). Answers
/// what that did and the list as it stands after it (`None` outside a serving
/// daemon).
pub fn deny(id: u64, digest: &str) -> (Denied, Option<TrustRequestList>) {
    let mut served = served();
    if served.tx.is_none() {
        return (Denied::Unknown, None);
    }
    let denied = served.hub.deny(id, digest);
    if denied == Denied::Declined {
        tracing::info!(
            request = id,
            "a client declined an orchestration's approval request"
        );
        broadcast(&served);
    }
    let list = served.hub.list();
    let owed = owed(&mut served);
    drop(served);
    deliver(owed);
    (denied, Some(list))
}

/// Which of `pending` no longer needs an answer, and how each closed:
/// approved since it was raised (from the CLI, or another path) is
/// [`Answer::Approved`]; its config now reading other text, or the config or
/// the orchestration being gone, is [`Answer::Withdrawn`]. A store that cannot
/// be read right now settles nothing: the request stays until a read answers.
/// Blocking — reads the store and the configs.
pub fn stale_requests_blocking(
    store: &crate::project_trust::TrustStore,
    pending: &[TrustRequest],
) -> BTreeMap<u64, Answer> {
    pending
        .iter()
        .filter_map(|request| {
            let answer = match crate::project_trust::inspect(
                store,
                Path::new(&request.config_dir),
                &request.orchestration,
            ) {
                Ok(check) if check.verdict.is_approved() => Answer::Approved,
                Ok(check) if check.digest() != request.digest => Answer::Withdrawn,
                Ok(_) => return None,
                Err(crate::project_trust::TrustError::Store(_)) => return None,
                Err(_) => Answer::Withdrawn,
            };
            Some((request.id, answer))
        })
        .collect()
}

/// `daemon serve`: every [`SWEEP_INTERVAL`] while requests are pending, drop
/// the ones [`stale_requests_blocking`] finds no longer need an answer, so a
/// request approved from the CLI, or whose config was edited since, closes in
/// every client without another attempt. Runs for the daemon's lifetime.
pub async fn run_sweep() {
    let mut tick = tokio::time::interval(SWEEP_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        // Issue #2348: another try for the answers a full inbox refused.
        deliver(take_retrying());
        let pending = served().hub.pending.clone();
        if pending.is_empty() {
            continue;
        }
        let stale = match tokio::task::spawn_blocking(move || {
            stale_requests_blocking(
                &crate::project_trust::TrustStore::default_location(),
                &pending,
            )
        })
        .await
        {
            Ok(stale) => stale,
            Err(_) => continue,
        };
        if stale.is_empty() {
            continue;
        }
        let mut served = served();
        if served.hub.drop_requests(&stale) {
            broadcast(&served);
        }
        let owed = owed(&mut served);
        drop(served);
        deliver(owed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_trust::{ApprovedRole, TrustVerdict};
    use std::path::PathBuf;

    fn check(project: &str, orchestration: &str, command: &str) -> TrustCheck {
        TrustCheck {
            project: PathBuf::from(project),
            config_dir: PathBuf::from(project),
            orchestration: orchestration.to_string(),
            roles: vec![ApprovedRole {
                name: "lead".into(),
                command: command.into(),
                agent: None,
                start: true,
                clear: false,
                description: None,
                prompt_template: None,
            }],
            verdict: TrustVerdict::NeverApproved,
        }
    }

    #[test]
    fn a_refused_text_raises_one_request_however_often_it_is_retried() {
        let mut hub = TrustRequestHub::default();
        let c = check("/p", "team", "claude");
        assert_eq!(hub.raise(&c, "dispatch"), (Raised::Pending, true));
        assert_eq!(hub.raise(&c, "dispatch"), (Raised::Pending, false));
        assert_eq!(hub.raise(&c, "schedule nightly"), (Raised::Pending, false));
        let list = hub.list();
        assert_eq!(list.requests.len(), 1);
        assert_eq!(list.requests[0].asked_by, "dispatch");
        assert_eq!(list.requests[0].digest, c.digest());
        assert_eq!(list.requests[0].config_dir, "/p");
    }

    #[test]
    fn a_new_text_replaces_the_pending_request_for_the_old_one() {
        let mut hub = TrustRequestHub::default();
        hub.raise(&check("/p", "team", "claude"), "dispatch");
        let first = hub.list().requests[0].id;
        let newer = check("/p", "team", "claude --model haiku");
        assert_eq!(hub.raise(&newer, "dispatch"), (Raised::Pending, true));
        let list = hub.list();
        assert_eq!(list.requests.len(), 1);
        assert_ne!(list.requests[0].id, first, "ids are never reused");
        assert_eq!(list.requests[0].digest, newer.digest());
        // Another orchestration, or another project, is a request of its own.
        hub.raise(&check("/p", "solo", "claude"), "dispatch");
        hub.raise(&check("/q", "team", "claude"), "dispatch");
        assert_eq!(hub.list().requests.len(), 3);
    }

    #[test]
    fn a_denial_closes_the_request_and_is_remembered_for_that_text_only() {
        let mut hub = TrustRequestHub::default();
        let c = check("/p", "team", "claude");
        hub.raise(&c, "dispatch");
        let id = hub.list().requests[0].id;
        let rev = hub.list().revision;
        assert_eq!(
            hub.deny(id, "another text"),
            Denied::Unknown,
            "a deny of text the request does not carry declines nothing"
        );
        assert_eq!(hub.deny(id, &c.digest()), Denied::Declined);
        assert!(hub.list().requests.is_empty());
        assert!(hub.list().revision > rev);
        assert_eq!(
            hub.deny(id, &c.digest()),
            Denied::Unknown,
            "a second answer finds nothing"
        );
        assert_eq!(hub.raise(&c, "schedule nightly"), (Raised::Declined, false));
        assert!(
            hub.list().requests.is_empty(),
            "a declined text asks no more"
        );
        // Changed text asks again.
        let edited = check("/p", "team", "claude --resume");
        assert_eq!(hub.raise(&edited, "dispatch"), (Raised::Pending, true));
    }

    #[test]
    fn an_approval_closes_the_request_and_forgets_the_denial() {
        let mut hub = TrustRequestHub::default();
        let c = check("/p", "team", "claude");
        hub.raise(&c, "dispatch");
        assert!(hub.settle_approved("/p", "team", &c.digest()));
        assert!(hub.list().requests.is_empty());
        assert!(!hub.settle_approved("/p", "team", &c.digest()));
        hub.raise(&c, "dispatch");
        let id = hub.list().requests[0].id;
        hub.deny(id, &c.digest());
        // Approving ANOTHER text of the orchestration leaves this denial.
        hub.settle_approved("/p", "team", "other-digest");
        assert_eq!(hub.raise(&c, "dispatch").0, Raised::Declined);
        hub.settle_approved("/p", "team", &c.digest());
        // Approved, then revoked: the old denial no longer holds.
        assert_eq!(hub.raise(&c, "dispatch"), (Raised::Pending, true));
    }

    #[test]
    fn the_pending_list_is_bounded() {
        let mut hub = TrustRequestHub::default();
        for n in 0..MAX_PENDING_REQUESTS {
            assert_eq!(
                hub.raise(&check(&format!("/p{n}"), "team", "claude"), "dispatch")
                    .0,
                Raised::Pending
            );
        }
        assert_eq!(
            hub.raise(&check("/over", "team", "claude"), "dispatch"),
            (Raised::Full, false)
        );
        assert_eq!(hub.list().requests.len(), MAX_PENDING_REQUESTS);
    }

    #[test]
    fn the_asked_by_label_is_escaped_and_bounded() {
        let mut hub = TrustRequestHub::default();
        let label = format!("evil\u{202e}{}", "x".repeat(400));
        hub.raise(&check("/p", "team", "claude"), &label);
        let shown = &hub.list().requests[0].asked_by;
        assert!(!shown.contains('\u{202e}'));
        assert_eq!(shown.chars().count(), MAX_ASKED_BY_CHARS);
    }

    /// The process-wide hub serves only after `install`, then raises a
    /// request on a refusal, says so in the refusal, broadcasts the list, says
    /// "declined" once denied, and closes the request when a check finds the
    /// text approved. (nextest runs each test in its own process, so this one's
    /// install reaches no other test.)
    #[test]
    fn the_served_hub_raises_broadcasts_and_words_the_refusal() {
        let c = check("/served-hub-test", "team", "claude");
        assert_eq!(
            refusal_for(&c, "dispatch", None),
            c.refusal(),
            "an unserved process refuses as PRD #1885 did"
        );
        assert!(list().is_none());
        let (tx, mut rx) = broadcast::channel(8);
        install(tx, None);
        assert!(is_served());

        let refusal = refusal_for(&c, "dispatch", None).expect("refused");
        assert!(refusal.contains("requested in the deck"), "{refusal}");
        assert!(
            refusal.contains("dot-agent-deck trust approve"),
            "{refusal}"
        );
        assert!(refusal.contains("never approved"), "{refusal}");
        let Ok(BroadcastMsg::TrustRequests(sent)) = rx.try_recv() else {
            panic!("a raised request is broadcast");
        };
        assert_eq!(sent.requests.len(), 1);
        assert_eq!(list().expect("served").requests, sent.requests);
        // The retry: same refusal, nothing broadcast.
        assert!(refusal_for(&c, "dispatch", None).is_some());
        assert!(rx.try_recv().is_err(), "a retry broadcasts nothing");

        let (denied, after) = deny(sent.requests[0].id, &c.digest());
        assert_eq!(denied, Denied::Declined);
        assert!(after.expect("served").requests.is_empty());
        assert!(matches!(rx.try_recv(), Ok(BroadcastMsg::TrustRequests(_))));
        let refusal = refusal_for(&c, "schedule", None).expect("refused");
        assert!(
            refusal.contains(crate::project_trust::DECLINED_IN_DECK),
            "{refusal}"
        );
        assert!(refusal.contains("New Agent form"), "{refusal}");

        // Approved text: no refusal, and the denial is forgotten.
        let mut approved = c.clone();
        approved.verdict = TrustVerdict::Approved;
        assert_eq!(refusal_for(&approved, "dispatch", None), None);
        assert!(
            refusal_for(&c, "dispatch", None)
                .expect("refused")
                .contains("requested in the deck")
        );
        let id = list().expect("served").requests[0].id;
        note_approved(&c.project, &c.orchestration, &c.digest());
        assert!(list().expect("served").requests.iter().all(|r| r.id != id));
        served().tx = None;
    }

    /// A pending request is stale once its orchestration is approved, once
    /// its config reads other text, and once the config no longer reads;
    /// otherwise it stays.
    #[test]
    fn the_sweep_drops_what_no_longer_needs_an_answer() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("swept");
        std::fs::create_dir_all(&dir).unwrap();
        let config = |command: &str| {
            std::fs::write(
                dir.join(crate::project_config::CONFIG_FILE_NAME),
                format!(
                    "[[orchestrations]]\nname = \"team\"\n\n[[orchestrations.roles]]\n\
                     name = \"lead\"\ncommand = \"{command}\"\nstart = true\n"
                ),
            )
            .unwrap();
        };
        config("cat");
        let store = crate::project_trust::TrustStore::default_location();
        let now = crate::project_trust::inspect(&store, &dir, "team").expect("inspect");
        assert!(!now.verdict.is_approved());
        let mut hub = TrustRequestHub::default();
        hub.raise(&now, "dispatch");
        let pending = hub.list().requests;
        assert!(stale_requests_blocking(&store, &pending).is_empty());

        config("cat --edited");
        assert_eq!(
            stale_requests_blocking(&store, &pending),
            BTreeMap::from([(pending[0].id, Answer::Withdrawn)]),
            "other text: approving this request could not record it"
        );
        config("cat");
        crate::project_trust::approve_all_for_test(&dir);
        assert_eq!(
            stale_requests_blocking(&store, &pending),
            BTreeMap::from([(pending[0].id, Answer::Approved)]),
            "approved elsewhere"
        );
        let mut gone = pending.clone();
        gone[0].config_dir = tmp.path().join("missing").display().to_string();
        assert_eq!(
            stale_requests_blocking(&store, &gone),
            BTreeMap::from([(pending[0].id, Answer::Withdrawn)])
        );
    }

    #[test]
    fn the_wire_spells_the_verb_the_broadcast_and_the_list() {
        use crate::daemon_protocol::AttachRequest;
        let deny = serde_json::to_value(AttachRequest::DenyTrustRequest {
            id: 3,
            digest: "d".into(),
        })
        .unwrap();
        assert_eq!(
            deny,
            serde_json::json!({"op": "deny-trust-request", "id": 3, "digest": "d"})
        );
        let list = TrustRequestList {
            revision: 4,
            requests: vec![TrustRequest {
                id: 3,
                project: "/p".into(),
                config_dir: "/p/wt".into(),
                orchestration: "team".into(),
                digest: "d".into(),
                asked_by: "dispatch".into(),
            }],
        };
        let sent = serde_json::to_value(BroadcastMsg::TrustRequests(list.clone())).unwrap();
        assert_eq!(sent["kind"], "trust_requests");
        assert_eq!(sent["revision"], 4);
        let back: BroadcastMsg = serde_json::from_value(sent).unwrap();
        assert!(matches!(back, BroadcastMsg::TrustRequests(l) if l == list));
        // A reply without the field (an older daemon) decodes with none.
        let resp: crate::daemon_protocol::AttachResponse =
            serde_json::from_str(r#"{"ok":true}"#).unwrap();
        assert!(resp.trust_requests.is_none());
    }

    #[test]
    fn a_client_takes_requests_only_from_a_daemon_that_advertises_them() {
        use crate::daemon_protocol::{AttachResponse, CAP_TRUST_REQUESTS};
        let listed = TrustRequestList {
            revision: 1,
            requests: vec![TrustRequest {
                id: 1,
                project: "/p".into(),
                config_dir: "/p".into(),
                orchestration: "team".into(),
                digest: "d".into(),
                asked_by: "evil\u{1b}[2J".into(),
            }],
        };
        let mut resp = AttachResponse::ok();
        resp.trust_requests = Some(listed.clone());
        assert_eq!(crate::daemon_client::trust_requests_of(&resp), None);
        resp.capabilities = Some(vec![CAP_TRUST_REQUESTS.to_string()]);
        let taken = crate::daemon_client::trust_requests_of(&resp).expect("advertised");
        assert!(!taken.requests[0].asked_by.contains('\u{1b}'));
        resp.trust_requests = None;
        assert_eq!(
            crate::daemon_client::trust_requests_of(&resp),
            Some(TrustRequestList::default()),
            "an advertising daemon with no list reports none pending"
        );
        let many = TrustRequestList {
            revision: 1,
            requests: vec![listed.requests[0].clone(); 100],
        };
        assert_eq!(
            many.sanitized().requests.len(),
            crate::daemon_protocol::MAX_TRUST_REQUESTS_SHOWN
        );
        assert!(crate::daemon_protocol::DAEMON_CAPABILITIES.contains(&CAP_TRUST_REQUESTS));
    }

    fn caller(pane: &str, agent: &str, verb: &str) -> RefusedCaller {
        RefusedCaller::new(pane, agent, verb)
    }

    /// Issue #2348: every agent refused for a request is told once when it is
    /// approved, naming each verb it was refused; a schedule (no caller) is
    /// told nothing, and a second settle tells no one again.
    #[test]
    fn an_approval_tells_each_refused_caller_once() {
        let mut hub = TrustRequestHub::default();
        let c = check("/p", "team", "claude");
        let dispatcher = caller("1", "agent-a", "dispatch");
        hub.raise_for(&c, "dispatch", Some(&dispatcher));
        hub.raise_for(&c, "dispatch", Some(&dispatcher));
        hub.raise_for(
            &c,
            "pane spawn",
            Some(&caller("1", "agent-a", "pane spawn")),
        );
        hub.raise_for(&c, "delegate", Some(&caller("2", "agent-b", "delegate")));
        hub.raise_for(&c, "scheduled task 'nightly'", None);
        assert!(hub.is_waiting(&RequestKey::of(&c), &dispatcher));
        assert!(hub.take_notices().is_empty(), "nothing is owed yet");

        assert!(hub.settle_approved("/p", "team", &c.digest()));
        let notices = hub.take_notices();
        assert_eq!(notices.len(), 2, "one per agent: {notices:?}");
        assert_eq!(
            (notices[0].pane_id.as_str(), notices[0].agent_id.as_str()),
            ("1", "agent-a")
        );
        assert!(notices.iter().all(|n| n.answer == Answer::Approved));
        assert_eq!(notices[0].subject, "orchestration approved");
        assert!(
            notices[0]
                .body
                .contains("Retry your dispatch and pane spawn now"),
            "{}",
            notices[0].body
        );
        assert!(
            notices[0]
                .body
                .contains("'team' of project /p was approved")
        );
        assert!(notices[1].body.contains("Retry your delegate now"));
        assert!(!hub.settle_approved("/p", "team", &c.digest()));
        assert!(hub.take_notices().is_empty(), "told once per answer");
        assert!(!hub.is_waiting(&RequestKey::of(&c), &dispatcher));
    }

    #[test]
    fn a_denial_tells_the_refused_callers_it_was_declined() {
        let mut hub = TrustRequestHub::default();
        let c = check("/p", "team", "claude");
        hub.raise_for(&c, "dispatch", Some(&caller("1", "agent-a", "dispatch")));
        let id = hub.list().requests[0].id;
        assert_eq!(hub.deny(id, "another text"), Denied::Unknown);
        assert!(hub.take_notices().is_empty());
        assert_eq!(hub.deny(id, &c.digest()), Denied::Declined);
        let notices = hub.take_notices();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].answer, Answer::Declined);
        assert_eq!(notices[0].subject, "orchestration declined");
        assert!(
            notices[0]
                .body
                .contains(crate::project_trust::DECLINED_IN_DECK),
            "{}",
            notices[0].body
        );
        assert!(notices[0].body.contains("do not retry"));
        // A declined text records no caller: the refusal already says so.
        hub.raise_for(&c, "dispatch", Some(&caller("1", "agent-a", "dispatch")));
        hub.settle_approved("/p", "team", &c.digest());
        assert!(hub.take_notices().is_empty());
    }

    /// The sweep closes a request approved from the CLI as approved, and one
    /// whose config changed as closed unanswered; each caller is told.
    #[test]
    fn the_sweep_tells_the_callers_how_each_request_closed() {
        let mut hub = TrustRequestHub::default();
        let a = check("/p", "team", "claude");
        let b = check("/q", "team", "claude");
        hub.raise_for(&a, "dispatch", Some(&caller("1", "agent-a", "dispatch")));
        hub.raise_for(
            &b,
            "pane restart",
            Some(&caller("2", "agent-b", "pane restart")),
        );
        let ids: Vec<u64> = hub.list().requests.iter().map(|r| r.id).collect();
        assert!(hub.drop_requests(&BTreeMap::from([
            (ids[0], Answer::Approved),
            (ids[1], Answer::Withdrawn),
        ])));
        let notices = hub.take_notices();
        assert_eq!(notices.len(), 2);
        assert_eq!(notices[0].answer, Answer::Approved);
        assert_eq!(notices[1].answer, Answer::Withdrawn);
        assert_eq!(notices[1].subject, "approval request closed");
        assert!(
            notices[1].body.contains("Retry your pane restart"),
            "{}",
            notices[1].body
        );
    }

    /// A new text of the orchestration takes the old request's place, and
    /// its callers with it: their retry reads the config as it now reads.
    #[test]
    fn a_replacing_request_keeps_the_callers_waiting_on_the_old_one() {
        let mut hub = TrustRequestHub::default();
        let old = check("/p", "team", "claude");
        let new = check("/p", "team", "claude --model haiku");
        hub.raise_for(&old, "dispatch", Some(&caller("1", "agent-a", "dispatch")));
        hub.raise_for(&new, "delegate", Some(&caller("2", "agent-b", "delegate")));
        assert!(hub.take_notices().is_empty(), "replacing tells no one");
        hub.settle_approved("/p", "team", &new.digest());
        let told: Vec<String> = hub.take_notices().into_iter().map(|n| n.pane_id).collect();
        assert_eq!(told, ["1", "2"]);
    }

    /// The caller whose own retry found the text approved is already starting
    /// it, so it is not told to retry; the others are.
    #[test]
    fn the_caller_that_found_it_approved_is_not_told_to_retry() {
        let mut hub = TrustRequestHub::default();
        let c = check("/p", "team", "claude");
        let a = caller("1", "agent-a", "dispatch");
        hub.raise_for(&c, "dispatch", Some(&a));
        hub.raise_for(&c, "delegate", Some(&caller("2", "agent-b", "delegate")));
        hub.settle_approved_except("/p", "team", &c.digest(), Some(&a));
        let told: Vec<String> = hub.take_notices().into_iter().map(|n| n.pane_id).collect();
        assert_eq!(told, ["2"]);
    }

    /// PR #2351 review: the attempt that found the text approved is left
    /// out, but the same agent's other refused verbs are still told.
    #[test]
    fn only_the_attempt_that_found_it_approved_is_left_out() {
        let mut hub = TrustRequestHub::default();
        let c = check("/p", "team", "claude");
        let restart = caller("1", "agent-a", "pane restart");
        hub.raise_for(&c, "pane restart", Some(&restart));
        hub.raise_for(&c, "delegate", Some(&caller("1", "agent-a", "delegate")));
        hub.settle_approved_except("/p", "team", &c.digest(), Some(&restart));
        let notices = hub.take_notices();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(
            notices[0].body.contains("Retry your delegate now"),
            "{}",
            notices[0].body
        );
        assert!(!notices[0].body.contains("pane restart"));
    }

    /// PR #2351 review: a linked worktree shares the project's key but reads
    /// its own file, so its callers are not handed another worktree's answer:
    /// they are told the request closed unanswered.
    #[test]
    fn another_worktrees_text_closes_the_old_callers_unanswered() {
        let mut hub = TrustRequestHub::default();
        let a = check("/p", "team", "claude");
        let mut b = check("/p", "team", "claude --model haiku");
        b.config_dir = PathBuf::from("/p-worktree");
        hub.raise_for(&a, "dispatch", Some(&caller("1", "agent-a", "dispatch")));
        hub.raise_for(&b, "dispatch", Some(&caller("2", "agent-b", "dispatch")));
        let notices = hub.take_notices();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert_eq!(notices[0].pane_id, "1");
        assert_eq!(notices[0].answer, Answer::Withdrawn);
        assert!(notices[0].body.contains("another worktree"));
        let id = hub.list().requests[0].id;
        hub.deny(id, &b.digest());
        let told: Vec<String> = hub.take_notices().into_iter().map(|n| n.pane_id).collect();
        assert_eq!(told, ["2"], "only B's caller hears B's answer");
    }

    #[test]
    fn the_callers_of_one_request_are_bounded() {
        let mut hub = TrustRequestHub::default();
        let c = check("/p", "team", "claude");
        for n in 0..MAX_WAITING_CALLERS + 10 {
            hub.raise_for(
                &c,
                "dispatch",
                Some(&caller(&n.to_string(), "a", "dispatch")),
            );
        }
        hub.settle_approved("/p", "team", &c.digest());
        assert_eq!(hub.take_notices().len(), MAX_WAITING_CALLERS);
    }

    /// Issue #2348 through the served hub: the refusal promises the caller a
    /// notice only when one will be sent, and the sink receives it once the
    /// request is answered — approved by a client (`note_approved`) or
    /// declined (`deny`). (nextest runs each test in its own process.)
    #[test]
    fn the_served_hub_posts_the_answer_to_the_refused_caller() {
        let (tx, _rx) = broadcast::channel(8);
        let got: Arc<Mutex<Vec<CallerNotice>>> = Arc::default();
        let sink_got = Arc::clone(&got);
        install(
            tx,
            Some(Arc::new(move |n: &CallerNotice| {
                sink_got.lock().unwrap().push(n.clone());
                Posted::Delivered
            })),
        );
        let c = check("/served-notice-test", "team", "claude");
        let a = caller("7", "agent-7", "dispatch");

        let schedule = refusal_for(&c, "scheduled task 'nightly'", None).expect("refused");
        assert!(
            schedule.contains(crate::project_trust::REQUESTED_RETRY),
            "{schedule}"
        );
        assert!(!schedule.contains(crate::project_trust::REQUESTED_WILL_TELL));
        let refusal = refusal_for(&c, "dispatch", Some(&a)).expect("refused");
        assert!(
            refusal.contains(crate::project_trust::REQUESTED_WILL_TELL),
            "{refusal}"
        );
        assert!(refusal.contains("requested in the deck"), "{refusal}");
        assert!(got.lock().unwrap().is_empty());

        note_approved(&c.project, &c.orchestration, &c.digest());
        let notices = std::mem::take(&mut *got.lock().unwrap());
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert_eq!(notices[0].pane_id, "7");
        assert_eq!(notices[0].agent_id, "agent-7");
        assert_eq!(notices[0].answer, Answer::Approved);

        let edited = check("/served-notice-test", "team", "claude --edited");
        refusal_for(
            &edited,
            "delegate",
            Some(&caller("8", "agent-8", "delegate")),
        );
        let id = list().expect("served").requests[0].id;
        deny(id, &edited.digest());
        let notices = std::mem::take(&mut *got.lock().unwrap());
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert_eq!(notices[0].answer, Answer::Declined);
        served().tx = None;
        served().sink = None;
    }

    /// PR #2351 review: an answer a full inbox refused is kept and tried
    /// again (the sweep's [`take_retrying`]) until it goes in; one whose
    /// caller is gone is dropped.
    #[test]
    fn an_answer_a_full_inbox_refused_is_tried_again() {
        let (tx, _rx) = broadcast::channel(8);
        let attempts: Arc<Mutex<Vec<String>>> = Arc::default();
        let seen = Arc::clone(&attempts);
        install(
            tx,
            Some(Arc::new(move |n: &CallerNotice| {
                let mut seen = seen.lock().unwrap();
                seen.push(n.pane_id.clone());
                let tries = seen.iter().filter(|p| **p == n.pane_id).count();
                match n.pane_id.as_str() {
                    "gone" => Posted::Gone,
                    _ if tries < 3 => Posted::Refused,
                    _ => Posted::Delivered,
                }
            })),
        );
        let c = check("/retry-test", "team", "claude");
        refusal_for(&c, "dispatch", Some(&caller("full", "agent-f", "dispatch")));
        refusal_for(&c, "dispatch", Some(&caller("gone", "agent-g", "dispatch")));
        note_approved(&c.project, &c.orchestration, &c.digest());
        assert_eq!(served().retrying.len(), 1, "the refused one is kept");
        deliver(take_retrying());
        assert_eq!(served().retrying.len(), 1, "still refused: kept");
        deliver(take_retrying());
        assert!(served().retrying.is_empty(), "delivered on the third try");
        assert!(take_retrying().is_none());
        let tries = attempts.lock().unwrap().clone();
        assert_eq!(tries.iter().filter(|p| *p == "full").count(), 3);
        assert_eq!(tries.iter().filter(|p| *p == "gone").count(), 1);
        served().tx = None;
        served().sink = None;
    }

    #[test]
    fn dropping_requests_moves_the_revision_only_when_one_went() {
        let mut hub = TrustRequestHub::default();
        hub.raise(&check("/p", "team", "claude"), "dispatch");
        let id = hub.list().requests[0].id;
        let rev = hub.list().revision;
        assert!(!hub.drop_requests(&BTreeMap::from([(id + 100, Answer::Withdrawn)])));
        assert_eq!(hub.list().revision, rev);
        assert!(hub.drop_requests(&BTreeMap::from([(id, Answer::Withdrawn)])));
        assert!(hub.list().revision > rev);
        assert!(hub.list().requests.is_empty());
    }
}
