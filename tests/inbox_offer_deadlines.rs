//! Offer assignment deadlines use the store's supplied clock, without sleeps.
use dot_agent_deck::inbox::{
    AckHow, BodyTrust, Consumer, Content, DeckId, Inbox, InboxLimits, Recipient, Scope, Sender,
};
use dot_agent_deck::inbox_hub::DEFAULT_UNDELIVERED_NOTICE_AFTER;
use spec::spec;
use std::time::{Duration, Instant};

/// Scenario: Assign an offer to a worker that never fetches it and advance the store clock to its undelivered deadline. The job must leave that worker, return to its pool, and go to the other subscriber, with a fresh deadline that stops applying once the new holder's receipt receiver is handed it: the store sets a handed-out offer no deadline of its own (its attempt model owns it). The new holder's claim then expires on its TTL, not a moment before, and the job returns to its pool excluding both holders.
#[spec("inbox/core/017")]
#[test]
fn inbox_core_017_unfetched_offer_returns_to_pool_at_undelivered_deadline() {
    let now = Instant::now();
    let mut store = Inbox::new(DeckId::parse("k3xzq2ab").unwrap(), InboxLimits::default());
    let first = Recipient::bound("first", "agent-first");
    let second = Recipient::bound("second", "agent-second");
    let candidates = [first.clone(), second.clone()];
    let offered = store
        .offer(
            now,
            Scope::Orchestration {
                token: "deadline-team".into(),
            },
            "review",
            None,
            Content {
                from: Sender::Agent {
                    pane_id: "orchestrator".into(),
                    agent_id: "agent-orchestrator".into(),
                    role: Some("orchestrator".into()),
                    orchestration: Some("deadline-team".into()),
                },
                body_trust: BodyTrust::Trusted,
                ..Content::from_deck("test", "unfetched job", "create the sentinel")
            },
            &candidates,
        )
        .unwrap();
    assert_eq!(offered.offered_to, Some(first.clone()));
    let due = now + DEFAULT_UNDELIVERED_NOTICE_AFTER;
    store.tick(due - Duration::from_nanos(1));
    assert!(
        store.unassigned_offers().is_empty(),
        "do not return an offer early"
    );
    assert_eq!(store.counts(&first.pane_id).queued, 1);
    store.tick(due);
    assert_eq!(
        store.unassigned_offers(),
        vec![offered.id.clone()],
        "assigned but unfetched offer must return at undelivered_after"
    );
    assert_eq!(store.counts(&first.pane_id).queued, 0);
    assert_eq!(
        store.assign_offer(due, &offered.id, &candidates),
        Some(second.clone()),
        "exclude the unfetched holder"
    );
    assert_eq!(
        store.next_deadline(),
        Some(due + DEFAULT_UNDELIVERED_NOTICE_AFTER),
        "new holder needs a fresh assignment deadline"
    );
    assert!(
        store
            .take_receipt(
                due,
                offered.id.as_str(),
                &second.pane_id,
                "agent-second",
                Consumer::FetchCli,
            )
            .is_some()
    );
    assert_eq!(
        store.next_deadline(),
        None,
        "a hand-out ends the assignment deadline and sets the store none of its own"
    );
    store.ack(
        due,
        offered.id.as_str(),
        &second.pane_id,
        "agent-second",
        AckHow::Explicit,
    );
    let expires = due + store.limits().default_claim_ttl;
    assert_eq!(
        store.next_deadline(),
        Some(expires),
        "claim replaces assignment deadline with claim TTL"
    );
    store.tick(expires - Duration::from_nanos(1));
    assert!(
        store.unassigned_offers().is_empty(),
        "the claim still holds"
    );
    store.tick(expires);
    assert_eq!(store.unassigned_offers(), vec![offered.id.clone()]);
    assert_eq!(
        store.assign_offer(expires, &offered.id, &candidates),
        None,
        "both earlier holders are excluded"
    );
}
