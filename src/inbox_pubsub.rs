//! PRD #1666 S11: the daemon's half of `inbox publish`, `inbox offer`,
//! `inbox complete` and `inbox release`.
//!
//! The inbox store ([`crate::inbox`]) holds the messages and resolves scopes
//! and subscriptions as pure functions over facts. This module gathers those
//! facts from the daemon — which panes are live and with which agent, which
//! orchestration instance and role each belongs to, which pane dispatched which
//! unit, and what each member's `.dot-agent-deck.toml` subscribes it to — and
//! answers the four verbs. It also runs the assigner that hands an offer back
//! in its pool to the next taker ([`spawn_offer_assigner`]).
//!
//! Who may do what: any attested agent in a scope may publish; only an
//! orchestrator — a start role, or the agent that dispatched a unit, both still
//! in their panes, that owes it a report — may offer; only an offer's claimant may complete or release it.
//! Each verb acts for the agent the request's token attests, and only while
//! that agent is still its pane's live agent. The process check (the request
//! comes from the agent's own process tree) is the hook loop's, before these
//! handlers run.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, info, warn};

use crate::agent_pty::AgentPtyRegistry;
use crate::event::{
    InboxClaimReply, InboxClaimRequest, InboxOfferRequest, InboxPostReply, InboxPublishRequest,
};
use crate::inbox::{
    BodyTrust, ClaimOutcome, Content, MessageId, PaneFacts, PostError, Recipient, ResolvedScope,
    RoleFacts, ScopeMember, Sender, is_valid_label, is_valid_message_id, offer_candidates,
    publish_recipients, resolve_scope,
};
use crate::inbox_hub::{MintedSpill, PostRefusal, Stale};
use crate::state::{ProjectConfigs, SharedState};

/// How many times a publish or offer reads its scope again when an agent in
/// it changed between reading it and posting (audit F5).
const MAX_RESOLVE_ATTEMPTS: usize = 3;

/// How often the assigner looks at pooled offers even when nothing woke it,
/// so a taker that joined the scope after the offer is found.
const ASSIGNER_SWEEP: Duration = Duration::from_secs(30);

/// What the daemon knows about the panes a scope is resolved from, read once
/// per request.
struct ScopeView {
    facts: Vec<PaneFacts>,
    /// The directory whose `.dot-agent-deck.toml` subscribes each pane.
    cwd_of: HashMap<String, String>,
    orchestrators: HashSet<String>,
    /// Panes whose live agent dispatched a unit, still live, that owes it a
    /// report.
    dispatch_callers: HashSet<String>,
    /// Each pane's sender facts, read with the rest.
    senders: HashMap<String, SenderFacts>,
    configs: ProjectConfigs,
}

/// A post's sender as the daemon's own records have it, never the request.
#[derive(Debug, Clone, Default)]
struct SenderFacts {
    role: Option<String>,
    orchestration: Option<String>,
}

impl ScopeView {
    /// Gather the facts, then read the project configs of every pane's
    /// directory with no state guard held.
    async fn gather(state: &SharedState, registry: &AgentPtyRegistry) -> Self {
        let live = registry.live_pane_agents();
        // Audit F1: an edge counts only while both of its panes are still held
        // by the agents that made it. A respawn of either end leaves the edge
        // retained (its report is still owed, or refused, by agent), and read
        // by pane alone it would put the successor in its predecessor's tree
        // and let it offer jobs as the dispatcher.
        let callers: HashMap<String, String> = {
            let holder: HashMap<&str, &str> = live
                .iter()
                .map(|(pane, agent)| (pane.as_str(), agent.as_str()))
                .collect();
            registry
                .dispatch_edges()
                .into_iter()
                .filter(|edge| {
                    holder.get(edge.unit_pane_id.as_str()) == Some(&edge.unit_agent_id.as_str())
                        && holder.get(edge.caller_pane_id.as_str())
                            == Some(&edge.caller_agent_id.as_str())
                })
                .map(|edge| (edge.unit_pane_id, edge.caller_pane_id))
                .collect()
        };
        let (facts, cwd_of, orchestrators, senders) = {
            let state = state.read().await;
            let mut cwd_of = HashMap::new();
            let mut senders = HashMap::new();
            let mut facts: Vec<PaneFacts> = live
                .into_iter()
                .map(|(pane_id, agent_id)| {
                    let orchestration =
                        state
                            .pane_orchestration_map
                            .get(&pane_id)
                            .and_then(|identity| {
                                Some(RoleFacts {
                                    token: identity.id.clone(),
                                    name: identity.name.clone(),
                                    role: state.pane_role_map.get(&pane_id)?.clone(),
                                })
                            });
                    let cwd = state.pane_cwd_map.get(&pane_id).cloned().or_else(|| {
                        registry
                            .agent_record_any(&agent_id)
                            .and_then(|record| record.cwd)
                    });
                    if let Some(cwd) = cwd {
                        cwd_of.insert(pane_id.clone(), cwd);
                    }
                    senders.insert(
                        pane_id.clone(),
                        SenderFacts {
                            role: state.pane_role_map.get(&pane_id).cloned(),
                            orchestration: state
                                .pane_orchestration_map
                                .get(&pane_id)
                                .map(|identity| identity.name.clone()),
                        },
                    );
                    PaneFacts {
                        dispatch_caller: callers.get(&pane_id).cloned(),
                        pane_id,
                        agent_id,
                        orchestration,
                    }
                })
                .collect();
            // A stable order, so "the first free taker" does not depend on a
            // map's iteration order.
            facts.sort_by(|a, b| a.pane_id.cmp(&b.pane_id));
            (facts, cwd_of, state.orchestrator_pane_ids.clone(), senders)
        };
        let mut dirs: Vec<String> = cwd_of.values().cloned().collect();
        dirs.sort();
        dirs.dedup();
        let configs = ProjectConfigs::load(dirs).await;
        Self {
            facts,
            cwd_of,
            orchestrators,
            dispatch_callers: callers.into_values().collect(),
            senders,
            configs,
        }
    }

    /// The sender of a post from `agent_id` in `pane_id`, and the label its
    /// subject names it by, read with the scope it is posted in.
    fn sender(&self, pane_id: &str, agent_id: &str) -> (Sender, String) {
        let facts = self.senders.get(pane_id).cloned().unwrap_or_default();
        let label = facts.role.clone().unwrap_or_else(|| "an agent".to_string());
        (
            Sender::Agent {
                pane_id: pane_id.to_string(),
                agent_id: agent_id.to_string(),
                role: facts.role,
                orchestration: facts.orchestration,
            },
            label,
        )
    }

    fn scope_of(&self, sender_pane: &str) -> Option<ResolvedScope> {
        resolve_scope(sender_pane, &self.facts)
    }

    /// `members` grouped by the directory whose config subscribes them, each
    /// group with that config's subscriptions. A member whose directory has
    /// no config is subscribed to nothing.
    fn by_config(
        &self,
        members: &[ScopeMember],
    ) -> Vec<(crate::inbox::Subscriptions, Vec<ScopeMember>)> {
        let mut groups: HashMap<&str, Vec<ScopeMember>> = HashMap::new();
        for member in members {
            if let Some(cwd) = self.cwd_of.get(&member.recipient.pane_id) {
                groups.entry(cwd).or_default().push(member.clone());
            }
        }
        let mut groups: Vec<(&str, Vec<ScopeMember>)> = groups.into_iter().collect();
        groups.sort_by(|a, b| a.0.cmp(b.0));
        groups
            .into_iter()
            .filter_map(|(cwd, members)| Some((self.configs.get(cwd)?.subscriptions(), members)))
            .collect()
    }

    /// Everyone in `scope` subscribed to `topic`, except the sender, once each.
    fn subscribers(&self, scope: &ResolvedScope, topic: &str, sender_pane: &str) -> Vec<Recipient> {
        let mut out: Vec<Recipient> = Vec::new();
        for (subscriptions, members) in self.by_config(&scope.members) {
            for recipient in publish_recipients(&members, &subscriptions, topic, sender_pane) {
                if !out.iter().any(|r| r.pane_id == recipient.pane_id) {
                    out.push(recipient);
                }
            }
        }
        out
    }

    /// Everyone in `scope` whose role takes jobs from `pool`, except the
    /// offerer, with each one's role, in member order.
    fn takers(
        &self,
        scope: &ResolvedScope,
        pool: &str,
        offerer_pane: &str,
    ) -> Vec<(Recipient, String)> {
        let role_of: HashMap<&str, &str> = scope
            .members
            .iter()
            .filter_map(|m| Some((m.recipient.pane_id.as_str(), m.role.as_deref()?)))
            .collect();
        let mut out: Vec<(Recipient, String)> = Vec::new();
        for (subscriptions, members) in self.by_config(&scope.members) {
            for recipient in offer_candidates(&members, &subscriptions, pool, offerer_pane, &[]) {
                if out.iter().any(|(r, _)| r.pane_id == recipient.pane_id) {
                    continue;
                }
                let role = role_of
                    .get(recipient.pane_id.as_str())
                    .map(|r| r.to_string())
                    .unwrap_or_default();
                out.push((recipient, role));
            }
        }
        let order: HashMap<&str, usize> = scope
            .members
            .iter()
            .enumerate()
            .map(|(i, m)| (m.recipient.pane_id.as_str(), i))
            .collect();
        out.sort_by_key(|(r, _)| order.get(r.pane_id.as_str()).copied().unwrap_or(usize::MAX));
        out
    }

    fn may_offer(&self, pane_id: &str) -> bool {
        self.orchestrators.contains(pane_id) || self.dispatch_callers.contains(pane_id)
    }
}

/// The refusal code of a [`PostError`] (`rate_limited`, `inbox_full`, …).
fn post_error_code(error: &PostError) -> String {
    serde_json::to_value(error)
        .ok()
        .and_then(|value| value.get("refused")?.as_str().map(str::to_string))
        .unwrap_or_else(|| "refused".to_string())
}

fn refused_post(error: &PostError) -> InboxPostReply {
    InboxPostReply::refused(&post_error_code(error), &error.to_string())
}

/// The refusal for a request from an agent that is no longer its pane's.
const NOT_LIVE: &str = "not_live";
const NOT_LIVE_MESSAGE: &str =
    "this agent is no longer the live agent of its pane, so it may not post or settle jobs";

fn is_live(registry: &AgentPtyRegistry, pane_id: &str, agent_id: &str) -> bool {
    registry.pane_current_agent_id(pane_id).as_deref() == Some(agent_id)
}

/// Keep the whole of an over-long body reachable (hardening H4), in the
/// sender's own directory.
///
/// Call it only once the inbox has admitted the post (audit F2), and keep the
/// answered spill only once the post is in: dropped, it removes the file.
fn retain(
    registry: &AgentPtyRegistry,
    pane_id: &str,
    agent_id: &str,
    content: &mut Content,
) -> Result<crate::inbox_hub::MintedSpill, String> {
    let dirs = crate::inbox_hub::spill_dirs(registry, pane_id, Some(agent_id));
    crate::inbox_hub::retain_full_body_minted(&registry.inbox().deck(), pane_id, content, &dirs)
}

/// The refusal for a long body [`retain`] could not save.
fn body_not_saved(error: &str) -> InboxPostReply {
    InboxPostReply::refused(
        "body_not_saved",
        &format!("the message is too long to hand over whole and could not be saved: {error}"),
    )
}

/// Audit F5: the check a post makes as it is inserted, under the inbox's lock
/// ([`crate::inbox_hub::InboxHub::publish`]): the sender still holds its pane,
/// and so does every member of the scope it was resolved in. A post read from
/// facts that changed while the request awaited the state or the project
/// configs then neither speaks for an agent that has left nor waits for one.
fn revalidator<'a>(
    registry: &'a AgentPtyRegistry,
    sender: &'a Recipient,
    members: &'a [Recipient],
) -> impl Fn() -> Result<(), Stale> + 'a {
    move || {
        let live: HashMap<String, String> = registry.live_pane_agents().into_iter().collect();
        let holds = |r: &Recipient| {
            r.agent_id
                .as_deref()
                .is_none_or(|agent| live.get(&r.pane_id).map(String::as_str) == Some(agent))
        };
        if !holds(sender) {
            Err(Stale::Sender)
        } else if members.iter().all(holds) {
            Ok(())
        } else {
            Err(Stale::Scope)
        }
    }
}

/// [`retain`] once per post: a body an earlier try already saved is named
/// again rather than saved twice. The refusal to answer when it cannot be.
fn save_once(
    registry: &AgentPtyRegistry,
    pane_id: &str,
    agent_id: &str,
    content: &mut Content,
    saved: &mut Option<MintedSpill>,
) -> Option<InboxPostReply> {
    content.spill_path = saved
        .as_ref()
        .and_then(MintedSpill::path)
        .map(std::path::Path::to_path_buf);
    if content.spill_path.is_some() {
        return None;
    }
    match retain(registry, pane_id, agent_id, content) {
        Ok(spill) => {
            if spill.path().is_some() {
                *saved = Some(spill);
            }
            None
        }
        Err(error) => Some(body_not_saved(&error)),
    }
}

/// One try at a publish or offer, against one [`ScopeView`].
enum Attempt {
    /// The answer to give.
    Done(InboxPostReply),
    /// Someone in the scope left between reading it and posting: read it again.
    Stale,
}

/// The refusal once the scope kept changing under a post.
const SCOPE_CHANGED: &str = "scope_changed";
const SCOPE_CHANGED_MESSAGE: &str =
    "the agents in this scope kept changing while the message was being posted; nothing was posted";

/// `inbox_publish`: announce to every subscriber of the topic in the sender's
/// scope. The body is another agent's text, so it is fenced as UNTRUSTED.
pub async fn handle_publish(
    registry: &AgentPtyRegistry,
    state: &SharedState,
    req: &InboxPublishRequest,
    agent_id: &str,
) -> InboxPostReply {
    if !is_valid_label(&req.topic) {
        return InboxPostReply::refused(
            "invalid_topic",
            "a topic is 1 to 64 lowercase letters, digits and hyphens, starting with a letter or digit",
        );
    }
    if !is_live(registry, &req.pane_id, agent_id) {
        return InboxPostReply::refused(NOT_LIVE, NOT_LIVE_MESSAGE);
    }
    let mut saved = None;
    for _ in 0..MAX_RESOLVE_ATTEMPTS {
        let view = ScopeView::gather(state, registry).await;
        if let Attempt::Done(reply) = publish_in(registry, req, agent_id, &view, &mut saved) {
            return reply;
        }
        debug!(pane_id = %req.pane_id, "inbox: the scope changed under a publish; reading it again");
    }
    InboxPostReply::refused(SCOPE_CHANGED, SCOPE_CHANGED_MESSAGE)
}

/// One try at `req` against the facts in `view`. `saved` is the body an
/// earlier try saved, kept once a try is accepted.
fn publish_in(
    registry: &AgentPtyRegistry,
    req: &InboxPublishRequest,
    agent_id: &str,
    view: &ScopeView,
    saved: &mut Option<MintedSpill>,
) -> Attempt {
    let Some(scope) = view.scope_of(&req.pane_id) else {
        return Attempt::Done(InboxPostReply::refused(
            "no_scope",
            "this pane is in no orchestration or dispatch tree, so there is no one to publish to",
        ));
    };
    let recipients = view.subscribers(&scope, &req.topic, &req.pane_id);
    let (from, role) = view.sender(&req.pane_id, agent_id);
    let mut content = Content {
        from,
        subject: format!("announcement \"{}\" from {role}", req.topic),
        body: req.body.clone(),
        body_trust: BodyTrust::Untrusted,
        spill_path: None,
        id: None,
        preface: None,
    };
    if let Err(error) = registry.inbox().admit_publish(&content, &recipients) {
        info!(pane_id = %req.pane_id, %error, "inbox: publish refused");
        return Attempt::Done(refused_post(&error));
    }
    if let Some(refusal) = save_once(registry, &req.pane_id, agent_id, &mut content, saved) {
        return Attempt::Done(refusal);
    }
    let sender = Recipient::bound(req.pane_id.clone(), agent_id.to_string());
    let members: Vec<Recipient> = scope.members.iter().map(|m| m.recipient.clone()).collect();
    match registry.inbox().publish(
        scope.scope,
        &req.topic,
        content,
        &recipients,
        &revalidator(registry, &sender, &members),
    ) {
        Ok(published) => {
            if let Some(spill) = saved.take() {
                spill.keep();
            }
            Attempt::Done(InboxPostReply {
                id: Some(published.id.to_string()),
                delivered_to: Some(published.delivered_to),
                refused_full: published.refused_full,
                ..InboxPostReply::default()
            })
        }
        Err(refusal) => settle_refusal(&req.pane_id, "publish", refusal),
    }
}

/// The answer to a publish or offer the inbox did not insert.
fn settle_refusal(pane_id: &str, verb: &str, refusal: PostRefusal) -> Attempt {
    match refusal {
        PostRefusal::Refused(error) => {
            info!(pane_id, verb, %error, "inbox: post refused");
            Attempt::Done(refused_post(&error))
        }
        PostRefusal::Stale(Stale::Sender) => {
            info!(
                pane_id,
                verb, "inbox: the poster left its pane before its post was inserted"
            );
            Attempt::Done(InboxPostReply::refused(NOT_LIVE, NOT_LIVE_MESSAGE))
        }
        PostRefusal::Stale(Stale::Scope) => Attempt::Stale,
    }
}

/// The deck's own words ahead of an offered job, outside its body: what the
/// taker is to do with it.
fn offer_preface(id: &MessageId, pool: &str) -> String {
    format!(
        "Your orchestrator offered this job to the workers that take jobs from pool \"{pool}\", \
         and it was handed to you: it is yours alone now. Do the work it describes. When it is \
         done, run `dot-agent-deck inbox complete {id}`. If you cannot do it, run \
         `dot-agent-deck inbox release {id}` so another worker can take it."
    )
}

/// `inbox_offer`: offer a job to the first free taker of the pool in the
/// orchestrator's scope. The job is the orchestrator's instruction, trusted as
/// a `delegate` is.
pub async fn handle_offer(
    registry: &AgentPtyRegistry,
    state: &SharedState,
    req: &InboxOfferRequest,
    agent_id: &str,
) -> InboxPostReply {
    if !is_valid_label(&req.pool) {
        return InboxPostReply::refused(
            "invalid_pool",
            "a pool is 1 to 64 lowercase letters, digits and hyphens, starting with a letter or digit",
        );
    }
    if !is_live(registry, &req.pane_id, agent_id) {
        return InboxPostReply::refused(NOT_LIVE, NOT_LIVE_MESSAGE);
    }
    let mut saved = None;
    for _ in 0..MAX_RESOLVE_ATTEMPTS {
        let view = ScopeView::gather(state, registry).await;
        if let Attempt::Done(reply) = offer_in(registry, req, agent_id, &view, &mut saved) {
            return reply;
        }
        debug!(pane_id = %req.pane_id, "inbox: the scope changed under an offer; reading it again");
    }
    InboxPostReply::refused(SCOPE_CHANGED, SCOPE_CHANGED_MESSAGE)
}

/// One try at `req` against the facts in `view`, as [`publish_in`].
fn offer_in(
    registry: &AgentPtyRegistry,
    req: &InboxOfferRequest,
    agent_id: &str,
    view: &ScopeView,
    saved: &mut Option<MintedSpill>,
) -> Attempt {
    if !view.may_offer(&req.pane_id) {
        warn!(pane_id = %req.pane_id, "inbox: offer from a pane that is not an orchestrator");
        return Attempt::Done(InboxPostReply::refused(
            "not_orchestrator",
            "only an orchestration's orchestrator, or a pane that dispatched a unit, may offer jobs",
        ));
    }
    let Some(scope) = view.scope_of(&req.pane_id) else {
        return Attempt::Done(InboxPostReply::refused(
            "no_scope",
            "this pane is in no orchestration or dispatch tree, so there is no one to offer to",
        ));
    };
    let takers = view.takers(&scope, &req.pool, &req.pane_id);
    let (from, role) = view.sender(&req.pane_id, agent_id);
    let id = MessageId::generate(&registry.inbox().deck());
    let mut content = Content {
        from,
        subject: format!("job \"{}\" offered by {role}", req.pool),
        body: req.body.clone(),
        body_trust: BodyTrust::Trusted,
        spill_path: None,
        preface: Some(offer_preface(&id, &req.pool)),
        id: Some(id),
    };
    let claim_ttl = req.claim_ttl_ms.map(Duration::from_millis);
    if let Err(error) = registry
        .inbox()
        .admit_offer(&scope.scope, claim_ttl, &content)
    {
        info!(pane_id = %req.pane_id, %error, "inbox: offer refused");
        return Attempt::Done(refused_post(&error));
    }
    if let Some(refusal) = save_once(registry, &req.pane_id, agent_id, &mut content, saved) {
        return Attempt::Done(refusal);
    }
    let offerer = Recipient::bound(req.pane_id.clone(), agent_id.to_string());
    let members: Vec<Recipient> = scope.members.iter().map(|m| m.recipient.clone()).collect();
    match registry.inbox().offer(
        scope.scope,
        &req.pool,
        claim_ttl,
        content,
        offerer.clone(),
        &takers,
        &revalidator(registry, &offerer, &members),
    ) {
        Ok(offered) => {
            if let Some(spill) = saved.take() {
                spill.keep();
            }
            Attempt::Done(InboxPostReply {
                id: Some(offered.id.to_string()),
                offered_to: offered.offered_to.map(|r| r.pane_id),
                ..InboxPostReply::default()
            })
        }
        Err(refusal) => settle_refusal(&req.pane_id, "offer", refusal),
    }
}

/// `inbox_complete` (`complete = true`) or `inbox_release`: the claimant
/// settles its claim on an offered job.
pub fn handle_claim(
    registry: &AgentPtyRegistry,
    req: &InboxClaimRequest,
    agent_id: &str,
    complete: bool,
) -> InboxClaimReply {
    if !is_valid_message_id(&req.id) {
        return InboxClaimReply::refused("unknown", "that is not an inbox message id");
    }
    if !is_live(registry, &req.pane_id, agent_id) {
        return InboxClaimReply::refused(NOT_LIVE, NOT_LIVE_MESSAGE);
    }
    let hub = registry.inbox();
    let outcome = if complete {
        hub.complete(&req.id, &req.pane_id, agent_id)
    } else {
        hub.release(&req.id, &req.pane_id, agent_id)
    };
    info!(pane_id = %req.pane_id, message_id = %req.id, ?outcome, "inbox: claim settled");
    match outcome {
        ClaimOutcome::Completed => InboxClaimReply {
            outcome: Some("completed".to_string()),
            ..InboxClaimReply::default()
        },
        ClaimOutcome::Released => InboxClaimReply {
            outcome: Some("released".to_string()),
            ..InboxClaimReply::default()
        },
        ClaimOutcome::NotClaimant => InboxClaimReply::refused(
            "not_claimant",
            "this job is not claimed by this agent: only its claimant may complete or release it",
        ),
        ClaimOutcome::Unknown => {
            InboxClaimReply::refused("unknown", "no open offered job has that id")
        }
    }
}

/// Hand every offer that is back in its pool to the next taker of its pool in
/// its offerer's scope: when one returns (released, expired, never fetched,
/// its holder gone), and every [`ASSIGNER_SWEEP`] for a taker that joined
/// since. An offer whose pool has no free taker waits.
pub fn spawn_offer_assigner(
    registry: Arc<AgentPtyRegistry>,
    state: SharedState,
) -> tokio::task::JoinHandle<()> {
    let pooled = registry.inbox().pooled_signal();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = pooled.notified() => {}
                _ = tokio::time::sleep(ASSIGNER_SWEEP) => {}
            }
            let offers = registry.inbox().pooled_offers();
            if offers.is_empty() {
                continue;
            }
            let view = ScopeView::gather(&state, &registry).await;
            for offer in offers {
                let Some(offerer_agent) = offer.offerer.agent_id.as_deref() else {
                    continue;
                };
                // The scope is the offerer's while it lives; once it is gone
                // its jobs wait in their pool.
                if !is_live(&registry, &offer.offerer.pane_id, offerer_agent) {
                    continue;
                }
                let Some(scope) = view.scope_of(&offer.offerer.pane_id) else {
                    continue;
                };
                let takers = view.takers(&scope, &offer.pool, &offer.offerer.pane_id);
                registry.inbox().assign_offer(&offer.id, &takers);
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_pty::{DOT_AGENT_DECK_PANE_ID, SpawnOptions};
    use crate::dispatch_return::DispatchCaller;
    use crate::state::AppState;

    fn spawn_in(registry: &Arc<AgentPtyRegistry>, pane: &str, cwd: Option<&str>) -> String {
        registry
            .spawn_agent(SpawnOptions {
                command: Some("cat"),
                cwd,
                env: crate::test_isolation::pin_unreachable_endpoints(vec![(
                    DOT_AGENT_DECK_PANE_ID.to_string(),
                    pane.to_string(),
                )]),
                ..SpawnOptions::default()
            })
            .expect("spawn stand-in")
    }

    fn shared() -> SharedState {
        Arc::new(tokio::sync::RwLock::new(AppState::default()))
    }

    fn offer_from(pane: &str, agent: &str) -> InboxOfferRequest {
        InboxOfferRequest {
            pane_id: pane.to_string(),
            agent_id: Some(agent.to_string()),
            token: None,
            pool: "jobs".to_string(),
            body: "do the job".to_string(),
            claim_ttl_ms: None,
        }
    }

    /// `caller` dispatched the unit in `unit`, as `dispatch` retains it.
    fn dispatched(registry: &AgentPtyRegistry, unit: (&str, &str), caller: (&str, &str)) {
        registry.register_dispatch_return(
            unit.0,
            unit.1,
            DispatchCaller {
                pane_id: caller.0.to_string(),
                agent_id: caller.1.to_string(),
                unit_name: "unit".to_string(),
            },
        );
    }

    fn in_scope(view: &ScopeView, sender: &str, member: &str) -> bool {
        view.scope_of(sender)
            .is_some_and(|scope| scope.members.iter().any(|m| m.recipient.pane_id == member))
    }

    /// Audit F1: a dispatch edge is retained across a respawn of its caller,
    /// but it names the agent that dispatched. The successor in that pane
    /// joins neither the unit's tree nor gains the right to offer it jobs.
    #[tokio::test]
    async fn a_respawned_dispatcher_inherits_neither_scope_nor_offer_authority() {
        let registry = Arc::new(AgentPtyRegistry::new());
        let caller = spawn_in(&registry, "f1-caller", None);
        let unit = spawn_in(&registry, "f1-unit", None);
        dispatched(&registry, ("f1-unit", &unit), ("f1-caller", &caller));
        let state = shared();

        let view = ScopeView::gather(&state, &registry).await;
        assert!(
            view.may_offer("f1-caller"),
            "the dispatcher itself may offer"
        );
        assert!(in_scope(&view, "f1-caller", "f1-unit"));

        let successor = registry
            .respawn_agent_for_pane("f1-caller", "cat")
            .await
            .expect("respawn the dispatcher");
        assert_ne!(successor, caller);
        assert_eq!(
            registry.outstanding_dispatch_returns(),
            1,
            "precondition: the respawn leaves the edge retained"
        );

        let view = ScopeView::gather(&state, &registry).await;
        assert!(
            !view.may_offer("f1-caller"),
            "the successor did not dispatch anything"
        );
        assert!(!in_scope(&view, "f1-caller", "f1-unit"));
        assert!(!in_scope(&view, "f1-unit", "f1-caller"));
        let reply = handle_offer(
            &registry,
            &state,
            &offer_from("f1-caller", &successor),
            &successor,
        )
        .await;
        assert_eq!(
            reply.refused.as_deref(),
            Some("not_orchestrator"),
            "{reply:?}"
        );
        registry.shutdown_all();
    }

    /// Audit F1, the unit side: the successor of a dispatched unit's agent was
    /// never dispatched, so it is not in its predecessor's dispatch tree, and
    /// the edge no longer makes its caller an orchestrator.
    #[tokio::test]
    async fn a_respawned_unit_is_not_in_its_predecessors_dispatch_tree() {
        let registry = Arc::new(AgentPtyRegistry::new());
        let caller = spawn_in(&registry, "f1b-caller", None);
        let unit = spawn_in(&registry, "f1b-unit", None);
        dispatched(&registry, ("f1b-unit", &unit), ("f1b-caller", &caller));
        let state = shared();
        assert!(in_scope(
            &ScopeView::gather(&state, &registry).await,
            "f1b-caller",
            "f1b-unit"
        ));

        registry
            .respawn_agent_for_pane("f1b-unit", "cat")
            .await
            .expect("respawn the unit");
        assert_eq!(registry.outstanding_dispatch_returns(), 1);

        let view = ScopeView::gather(&state, &registry).await;
        assert!(!in_scope(&view, "f1b-caller", "f1b-unit"));
        assert!(!in_scope(&view, "f1b-unit", "f1b-caller"));
        assert!(!view.may_offer("f1b-caller"));
        registry.shutdown_all();
    }

    /// The long-message files the deck saved in `dir`.
    fn saved_bodies(dir: &std::path::Path) -> usize {
        std::fs::read_dir(dir.join(".dot-agent-deck")).map_or(0, |entries| {
            entries
                .filter_map(Result::ok)
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with("inbox-message-")
                })
                .count()
        })
    }

    /// A body too long for the tightest hand-over, so the deck saves it whole.
    fn long_body() -> String {
        "a long body. ".repeat(1_000)
    }

    /// Audit F2: a publish or offer the inbox refuses — over the body limit as
    /// sent on the wire, over the sender's rate, with a claim time out of
    /// range, or into a scope already holding its most open offers — saves
    /// nothing. Accepted long posts still keep their saved body.
    #[tokio::test]
    async fn refused_publishes_and_offers_save_no_body() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let registry = Arc::new(AgentPtyRegistry::new());
        let caller = spawn_in(&registry, "f2-caller", Some(cwd));
        let unit = spawn_in(&registry, "f2-unit", Some(cwd));
        dispatched(&registry, ("f2-unit", &unit), ("f2-caller", &caller));
        let state = shared();
        let publish = |body: String| InboxPublishRequest {
            pane_id: "f2-unit".to_string(),
            agent_id: Some(unit.clone()),
            token: None,
            topic: "news".to_string(),
            body,
        };

        let over = "x".repeat(crate::inbox::DEFAULT_MAX_BODY_BYTES + 1);
        let reply = handle_publish(&registry, &state, &publish(over.clone()), &unit).await;
        assert_eq!(
            reply.refused.as_deref(),
            Some("body_too_large"),
            "{reply:?}"
        );
        assert_eq!(
            saved_bodies(dir.path()),
            0,
            "a body over the limit is saved"
        );

        let rate = crate::inbox::DEFAULT_RATE_PER_MINUTE as usize;
        for _ in 0..rate {
            let reply = handle_publish(&registry, &state, &publish(long_body()), &unit).await;
            assert!(reply.refused.is_none(), "{reply:?}");
        }
        assert_eq!(
            saved_bodies(dir.path()),
            rate,
            "each accepted long publish keeps its body"
        );
        for _ in 0..3 {
            let reply = handle_publish(&registry, &state, &publish(long_body()), &unit).await;
            assert_eq!(reply.refused.as_deref(), Some("rate_limited"), "{reply:?}");
        }
        assert_eq!(
            saved_bodies(dir.path()),
            rate,
            "a publish over the rate is saved"
        );

        let mut offer = offer_from("f2-caller", &caller);
        offer.body = long_body();
        offer.claim_ttl_ms = Some(1);
        let reply = handle_offer(&registry, &state, &offer, &caller).await;
        assert_eq!(
            reply.refused.as_deref(),
            Some("claim_ttl_out_of_range"),
            "{reply:?}"
        );
        offer.claim_ttl_ms = None;
        offer.body = over;
        let reply = handle_offer(&registry, &state, &offer, &caller).await;
        assert_eq!(
            reply.refused.as_deref(),
            Some("body_too_large"),
            "{reply:?}"
        );
        assert_eq!(saved_bodies(dir.path()), rate, "a refused offer is saved");

        let scope = crate::inbox::Scope::Dispatch {
            root_pane: "f2-caller".to_string(),
        };
        for n in 0..crate::inbox::DEFAULT_MAX_OFFERS_PER_SCOPE {
            registry
                .inbox()
                .offer(
                    scope.clone(),
                    "jobs",
                    None,
                    Content::from_deck("test", format!("job {n}"), "fill the scope"),
                    Recipient::bound("f2-caller", caller.clone()),
                    &[],
                    &|| Ok(()),
                )
                .expect("fill the scope's open offers");
        }
        offer.body = long_body();
        let reply = handle_offer(&registry, &state, &offer, &caller).await;
        assert_eq!(
            reply.refused.as_deref(),
            Some("too_many_offers"),
            "{reply:?}"
        );
        assert_eq!(
            saved_bodies(dir.path()),
            rate,
            "an offer into a full scope is saved"
        );
        registry.shutdown_all();
    }

    /// Audit F5: an offer whose offerer is replaced after its scope was read —
    /// the reads a request awaits on — is refused when it is posted, and
    /// nothing is queued. The offerer leaving is not a change to retry.
    #[tokio::test]
    async fn an_offer_resolved_before_its_offerer_was_replaced_is_refused() {
        let registry = Arc::new(AgentPtyRegistry::new());
        let caller = spawn_in(&registry, "f5-caller", None);
        let unit = spawn_in(&registry, "f5-unit", None);
        dispatched(&registry, ("f5-unit", &unit), ("f5-caller", &caller));
        let state = shared();

        let view = ScopeView::gather(&state, &registry).await;
        registry
            .respawn_agent_for_pane("f5-caller", "cat")
            .await
            .expect("respawn the offerer");
        let attempt = offer_in(
            &registry,
            &offer_from("f5-caller", &caller),
            &caller,
            &view,
            &mut None,
        );
        let Attempt::Done(reply) = attempt else {
            panic!("the offerer itself left: that is a refusal, not a scope to read again");
        };
        assert_eq!(reply.refused.as_deref(), Some("not_live"), "{reply:?}");
        assert!(
            registry.inbox().list_all().is_empty(),
            "the departed offerer's job was posted"
        );
        registry.shutdown_all();
    }

    /// Audit F5: a publish whose subscriber is replaced after the scope was
    /// read queues no copy for the subscriber's departed agent; the publish
    /// reads its scope again instead.
    #[tokio::test]
    async fn a_publish_resolved_before_a_subscriber_was_replaced_queues_nothing_for_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".dot-agent-deck.toml"),
            "[dispatch]\nsubscribe = [\"news\"]\n",
        )
        .unwrap();
        let registry = Arc::new(AgentPtyRegistry::new());
        let caller = spawn_in(&registry, "f5s-caller", Some(dir.path().to_str().unwrap()));
        let unit = spawn_in(&registry, "f5s-unit", None);
        dispatched(&registry, ("f5s-unit", &unit), ("f5s-caller", &caller));
        let state = shared();

        let view = ScopeView::gather(&state, &registry).await;
        let scope = view
            .scope_of("f5s-unit")
            .expect("the unit is in its dispatch tree");
        assert!(
            view.subscribers(&scope, "news", "f5s-unit")
                .iter()
                .any(|r| r.agent_id.as_deref() == Some(caller.as_str())),
            "precondition: the dispatcher subscribes to the topic"
        );
        registry
            .respawn_agent_for_pane("f5s-caller", "cat")
            .await
            .expect("respawn the subscriber");
        let publish = InboxPublishRequest {
            pane_id: "f5s-unit".to_string(),
            agent_id: Some(unit.clone()),
            token: None,
            topic: "news".to_string(),
            body: "the build is green".to_string(),
        };
        let attempt = publish_in(&registry, &publish, &unit, &view, &mut None);
        assert!(
            registry
                .inbox()
                .list_all()
                .iter()
                .all(|m| m.bind_agent.as_deref() != Some(caller.as_str())),
            "a copy was queued for the subscriber's departed agent"
        );
        assert!(
            matches!(attempt, Attempt::Stale),
            "the scope changed under the publish: it is read again"
        );
        registry.shutdown_all();
    }
}
