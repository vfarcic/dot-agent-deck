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
//!
//! # Only the serving daemon raises requests
//!
//! The hub that [`refusal_for`] consults is the process-wide one, and it serves
//! only once [`install`] has given it the daemon's broadcast channel, which
//! `daemon serve` does. Everywhere else — the TUI, the CLI, an in-process test
//! daemon — [`refusal_for`] answers the PRD #1885 refusal unchanged.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Mutex;
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
    fn of(check: &TrustCheck) -> Self {
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

/// The pending requests and the remembered denials of one daemon.
#[derive(Debug, Default)]
pub struct TrustRequestHub {
    next_id: u64,
    revision: u64,
    pending: Vec<TrustRequest>,
    /// Oldest first, so the cap forgets the oldest.
    denied: Vec<RequestKey>,
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

    /// Raise a request for `check`'s text, refused while `asked_by` (what
    /// tried to start it: `dispatch`, a schedule's name, `pane restart`, …).
    /// Answers what the refusal should say, and whether the list changed.
    pub fn raise(&mut self, check: &TrustCheck, asked_by: &str) -> (Raised, bool) {
        let key = RequestKey::of(check);
        if self.denied.contains(&key) {
            return (Raised::Declined, false);
        }
        if self
            .pending
            .iter()
            .any(|r| RequestKey::of_request(r) == key)
        {
            return (Raised::Pending, false);
        }
        // A pending request for an older text of the same orchestration can no
        // longer be approved as shown; this one takes its place.
        let before = self.pending.len();
        self.pending
            .retain(|r| !RequestKey::of_request(r).same_orchestration(&key));
        let replaced = self.pending.len() != before;
        if self.pending.len() >= MAX_PENDING_REQUESTS {
            if replaced {
                self.changed();
            }
            return (Raised::Full, replaced);
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
        let key = RequestKey {
            project: project.to_string(),
            orchestration: orchestration.to_string(),
            digest: digest.to_string(),
        };
        self.denied.retain(|d| *d != key);
        let before = self.pending.len();
        self.pending.retain(|r| RequestKey::of_request(r) != key);
        let changed = self.pending.len() != before;
        if changed {
            self.changed();
        }
        changed
    }

    /// Drop the pending requests `stale` names (by id). Answers whether the
    /// list changed.
    pub fn drop_requests(&mut self, stale: &BTreeSet<u64>) -> bool {
        let before = self.pending.len();
        self.pending.retain(|r| !stale.contains(&r.id));
        let changed = self.pending.len() != before;
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
}

static SERVED: Mutex<Served> = Mutex::new(Served {
    hub: TrustRequestHub {
        next_id: 0,
        revision: 0,
        pending: Vec::new(),
        denied: Vec::new(),
    },
    tx: None,
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

/// `daemon serve`: raise requests from here on, and broadcast every change of
/// them on `tx`.
pub fn install(tx: broadcast::Sender<BroadcastMsg>) {
    served().tx = Some(tx);
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
pub fn refusal_for(check: &TrustCheck, asked_by: &str) -> Option<String> {
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
            .settle_approved(&project, &check.orchestration, &digest)
        {
            broadcast(&served);
        }
        return None;
    }
    let (raised, changed) = served.hub.raise(check, asked_by);
    if changed {
        broadcast(&served);
    }
    drop(served);
    Some(match raised {
        Raised::Pending => crate::project_trust::requested_refusal_text(check),
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
    (denied, Some(served.hub.list()))
}

/// Which of `pending` no longer needs an answer: approved since it was raised
/// (from the CLI, or another path), its config now reads other text, or the
/// config or the orchestration is gone. A store that cannot be read right now
/// settles nothing: the request stays until a read answers. Blocking — reads
/// the store and the configs.
pub fn stale_requests_blocking(
    store: &crate::project_trust::TrustStore,
    pending: &[TrustRequest],
) -> BTreeSet<u64> {
    pending
        .iter()
        .filter(|request| {
            match crate::project_trust::inspect(
                store,
                Path::new(&request.config_dir),
                &request.orchestration,
            ) {
                Ok(check) => check.verdict.is_approved() || check.digest() != request.digest,
                Err(crate::project_trust::TrustError::Store(_)) => false,
                Err(_) => true,
            }
        })
        .map(|request| request.id)
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
            refusal_for(&c, "dispatch"),
            c.refusal(),
            "an unserved process refuses as PRD #1885 did"
        );
        assert!(list().is_none());
        let (tx, mut rx) = broadcast::channel(8);
        install(tx);
        assert!(is_served());

        let refusal = refusal_for(&c, "dispatch").expect("refused");
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
        assert!(refusal_for(&c, "dispatch").is_some());
        assert!(rx.try_recv().is_err(), "a retry broadcasts nothing");

        let (denied, after) = deny(sent.requests[0].id, &c.digest());
        assert_eq!(denied, Denied::Declined);
        assert!(after.expect("served").requests.is_empty());
        assert!(matches!(rx.try_recv(), Ok(BroadcastMsg::TrustRequests(_))));
        let refusal = refusal_for(&c, "schedule").expect("refused");
        assert!(
            refusal.contains(crate::project_trust::DECLINED_IN_DECK),
            "{refusal}"
        );
        assert!(refusal.contains("New Agent form"), "{refusal}");

        // Approved text: no refusal, and the denial is forgotten.
        let mut approved = c.clone();
        approved.verdict = TrustVerdict::Approved;
        assert_eq!(refusal_for(&approved, "dispatch"), None);
        assert!(
            refusal_for(&c, "dispatch")
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
            BTreeSet::from([pending[0].id]),
            "other text: approving this request could not record it"
        );
        config("cat");
        crate::project_trust::approve_all_for_test(&dir);
        assert_eq!(
            stale_requests_blocking(&store, &pending),
            BTreeSet::from([pending[0].id]),
            "approved elsewhere"
        );
        let mut gone = pending.clone();
        gone[0].config_dir = tmp.path().join("missing").display().to_string();
        assert_eq!(stale_requests_blocking(&store, &gone).len(), 1);
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

    #[test]
    fn dropping_requests_moves_the_revision_only_when_one_went() {
        let mut hub = TrustRequestHub::default();
        hub.raise(&check("/p", "team", "claude"), "dispatch");
        let id = hub.list().requests[0].id;
        let rev = hub.list().revision;
        assert!(!hub.drop_requests(&BTreeSet::from([id + 100])));
        assert_eq!(hub.list().revision, rev);
        assert!(hub.drop_requests(&BTreeSet::from([id])));
        assert!(hub.list().revision > rev);
        assert!(hub.list().requests.is_empty());
    }
}
