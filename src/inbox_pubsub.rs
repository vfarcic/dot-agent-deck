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

use tracing::{info, warn};

use crate::agent_pty::AgentPtyRegistry;
use crate::event::{
    InboxClaimReply, InboxClaimRequest, InboxOfferRequest, InboxPostReply, InboxPublishRequest,
};
use crate::inbox::{
    BodyTrust, ClaimOutcome, Content, MessageId, PaneFacts, PostError, Recipient, ResolvedScope,
    RoleFacts, ScopeMember, Sender, is_valid_label, is_valid_message_id, offer_candidates,
    publish_recipients, resolve_scope,
};
use crate::state::{ProjectConfigs, SharedState};

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
    configs: ProjectConfigs,
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
        let (facts, cwd_of, orchestrators) = {
            let state = state.read().await;
            let mut cwd_of = HashMap::new();
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
            (facts, cwd_of, state.orchestrator_pane_ids.clone())
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
            configs,
        }
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

/// The sender of a post from `agent_id` in `pane_id`, its role and
/// orchestration taken from the daemon's own records, never the request.
async fn sender(state: &SharedState, pane_id: &str, agent_id: &str) -> (Sender, String) {
    let state = state.read().await;
    let role = state.pane_role_map.get(pane_id).cloned();
    let orchestration = state
        .pane_orchestration_map
        .get(pane_id)
        .map(|identity| identity.name.clone());
    let label = role.clone().unwrap_or_else(|| "an agent".to_string());
    (
        Sender::Agent {
            pane_id: pane_id.to_string(),
            agent_id: agent_id.to_string(),
            role,
            orchestration,
        },
        label,
    )
}

/// Keep the whole of an over-long body reachable (hardening H4), in the
/// sender's own directory. The refusal to answer when it cannot be saved.
fn retain(
    registry: &AgentPtyRegistry,
    pane_id: &str,
    agent_id: &str,
    content: &mut Content,
) -> Option<InboxPostReply> {
    let dirs = crate::inbox_hub::spill_dirs(registry, pane_id, Some(agent_id));
    let error =
        crate::inbox_hub::retain_full_body(&registry.inbox().deck(), pane_id, content, &dirs)
            .err()?;
    Some(InboxPostReply::refused(
        "body_not_saved",
        &format!("the message is too long to hand over whole and could not be saved: {error}"),
    ))
}

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
    let view = ScopeView::gather(state, registry).await;
    let Some(scope) = view.scope_of(&req.pane_id) else {
        return InboxPostReply::refused(
            "no_scope",
            "this pane is in no orchestration or dispatch tree, so there is no one to publish to",
        );
    };
    let recipients = view.subscribers(&scope, &req.topic, &req.pane_id);
    let (from, role) = sender(state, &req.pane_id, agent_id).await;
    let mut content = Content {
        from,
        subject: format!("announcement \"{}\" from {role}", req.topic),
        body: req.body.clone(),
        body_trust: BodyTrust::Untrusted,
        spill_path: None,
        id: None,
        preface: None,
    };
    if let Some(refusal) = retain(registry, &req.pane_id, agent_id, &mut content) {
        return refusal;
    }
    match registry
        .inbox()
        .publish(scope.scope, &req.topic, content, &recipients)
    {
        Ok(published) => InboxPostReply {
            id: Some(published.id.to_string()),
            delivered_to: Some(published.delivered_to),
            refused_full: published.refused_full,
            ..InboxPostReply::default()
        },
        Err(error) => {
            info!(pane_id = %req.pane_id, %error, "inbox: publish refused");
            refused_post(&error)
        }
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
    let view = ScopeView::gather(state, registry).await;
    if !view.may_offer(&req.pane_id) {
        warn!(pane_id = %req.pane_id, "inbox: offer from a pane that is not an orchestrator");
        return InboxPostReply::refused(
            "not_orchestrator",
            "only an orchestration's orchestrator, or a pane that dispatched a unit, may offer jobs",
        );
    }
    let Some(scope) = view.scope_of(&req.pane_id) else {
        return InboxPostReply::refused(
            "no_scope",
            "this pane is in no orchestration or dispatch tree, so there is no one to offer to",
        );
    };
    let takers = view.takers(&scope, &req.pool, &req.pane_id);
    let (from, role) = sender(state, &req.pane_id, agent_id).await;
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
    if let Some(refusal) = retain(registry, &req.pane_id, agent_id, &mut content) {
        return refusal;
    }
    let offerer = Recipient::bound(req.pane_id.clone(), agent_id.to_string());
    match registry.inbox().offer(
        scope.scope,
        &req.pool,
        req.claim_ttl_ms.map(Duration::from_millis),
        content,
        offerer,
        &takers,
    ) {
        Ok(offered) => InboxPostReply {
            id: Some(offered.id.to_string()),
            offered_to: offered.offered_to.map(|r| r.pane_id),
            ..InboxPostReply::default()
        },
        Err(error) => {
            info!(pane_id = %req.pane_id, %error, "inbox: offer refused");
            refused_post(&error)
        }
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
}
