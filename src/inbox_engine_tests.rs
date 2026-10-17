//! PRD #1666 §13 step 4: the receipt engine through the hub's own entry
//! points — a receipt stream served over an in-memory duplex, the attach
//! verbs' hub half, the floor and the spool — with the clock real but short.

use super::*;
use crate::event::InboxWaitReply;
use crate::inbox_display::{AttentionKind, IntegrationNotice};
use spec::spec;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn deck() -> DeckId {
    DeckId::parse("k3xzq2ab").unwrap()
}

fn content(body: &str) -> Content {
    Content {
        from: Sender::Deck {
            reason: "seed".to_string(),
        },
        subject: format!("about {body}"),
        body: body.to_string(),
        body_trust: BodyTrust::Trusted,
        spill_path: None,
        id: None,
        preface: None,
    }
}

/// A hub whose keepalive is `keepalive`, so `POSSESS_TTL` is three of it, and
/// whose ack-wait is `ack_wait`.
fn hub_with(keepalive: Duration, ack_wait: Duration) -> Arc<InboxHub> {
    let limits = InboxLimits {
        ack_wait,
        ..InboxLimits::default()
    };
    Arc::new(InboxHub::new(deck(), limits, keepalive))
}

const INTEGRATION: ReceiptArm = ReceiptArm {
    adapter_integration: Some(ADAPTER_INTEGRATION),
    doorbell_only: false,
    resumed: false,
};

/// The component's end of one receipt stream.
struct Peer {
    lines: tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
    to: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    serve: tokio::task::JoinHandle<()>,
}

impl Peer {
    async fn open(
        hub: &Arc<InboxHub>,
        pane: &str,
        agent: &str,
        component: InboxComponent,
        arm: ReceiptArm,
    ) -> Peer {
        let (client, server) = tokio::io::duplex(1 << 16);
        let (read, write) = tokio::io::split(server);
        let serve = tokio::spawn(serve_waiter(
            Arc::clone(hub),
            pane.to_string(),
            agent.to_string(),
            InboxWaitMode::Stream,
            component,
            None,
            arm,
            BufReader::new(read),
            write,
            |_: &str| {},
        ));
        let (from, to) = tokio::io::split(client);
        Peer {
            lines: BufReader::new(from).lines(),
            to,
            serve,
        }
    }

    /// The next line that is not a keepalive.
    async fn next(&mut self) -> InboxWaitReply {
        loop {
            let line = tokio::time::timeout(Duration::from_secs(5), self.lines.next_line())
                .await
                .expect("the daemon writes a line in time")
                .unwrap()
                .expect("the stream is still open");
            let reply: InboxWaitReply = serde_json::from_str(&line).unwrap();
            if !reply.keepalive {
                return reply;
            }
        }
    }

    /// The next hand-out line, skipping anything else.
    async fn handout(&mut self) -> Vec<InboxDelivery> {
        loop {
            if let Some(messages) = self.next().await.messages {
                return messages;
            }
        }
    }

    async fn send(&mut self, control: serde_json::Value) {
        self.to
            .write_all(format!("{control}\n").as_bytes())
            .await
            .unwrap();
        self.to.flush().await.unwrap();
    }

    async fn close(mut self) {
        let _ = self.to.shutdown().await;
        drop(self.lines);
        let _ = tokio::time::timeout(Duration::from_secs(5), self.serve).await;
    }
}

fn attempt(delivery: &InboxDelivery) -> &crate::event::InboxDeliveryAttempt {
    delivery
        .attempt
        .as_ref()
        .expect("a receipt hand-out names its attempt")
}

fn at(delivery: &InboxDelivery) -> serde_json::Value {
    serde_json::json!({"id": delivery.id, "a": attempt(delivery).a, "e": attempt(delivery).e})
}

fn state_of(hub: &InboxHub, pane: &str, id: &str) -> StateName {
    hub.list(pane)
        .into_iter()
        .find(|m| m.id.as_str() == id)
        .map(|m| m.state)
        .expect("the message is listed")
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Scenario: One pane holds a message an `inbox fetch` took (legacy) and one an OpenCode receipt stream took. With the ack-wait elapsed, the legacy ack-wait redelivers only its own; the receipt-owned hand-out stays where its attempt model put it, the legacy hold and nack refuse it even from its own consumer, and a receipt receiver never takes the legacy-owned one back.
#[spec("inbox/boundary/001")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_001_legacy_ack_wait_touches_only_legacy_owned_assignments() {
    let hub = hub_with(Duration::from_secs(30), Duration::from_millis(50));
    let legacy = hub
        .post_direct(Recipient::bound("p", "a"), content("legacy"))
        .unwrap();
    let fetched = hub.fetch("p", "a", Consumer::FetchCli, None, None);
    assert_eq!(fetched.len(), 1);
    let mut peer = Peer::open(&hub, "p", "a", InboxComponent::OpenCodePlugin, INTEGRATION).await;
    assert!(peer.next().await.registered.is_some());
    let receipt = hub
        .post_direct(Recipient::bound("p", "a"), content("receipt"))
        .unwrap();
    let handed = peer.handout().await;
    assert_eq!(handed[0].id, receipt.as_str());
    tokio::time::sleep(Duration::from_millis(80)).await;
    hub.tick();
    assert_eq!(
        state_of(&hub, "p", legacy.as_str()),
        StateName::Queued,
        "the legacy hand-out is redelivered"
    );
    assert_eq!(
        state_of(&hub, "p", receipt.as_str()),
        StateName::Fetched,
        "the receipt-owned hand-out is not the ack-wait's"
    );
    assert_eq!(
        hub.lock().inbox.owner_of(legacy.as_str(), "p", "a"),
        Some(crate::inbox::AssignmentOwner::Legacy)
    );
    // A receipt receiver never takes the legacy-owned message back: it is
    // not handed out, and no attempt is issued for it.
    hub.dispatch_agent("p", "a");
    assert_eq!(state_of(&hub, "p", legacy.as_str()), StateName::Queued);
    let leaked = tokio::time::timeout(Duration::from_millis(300), async {
        loop {
            if peer
                .next()
                .await
                .messages
                .is_some_and(|m| m.iter().any(|d| d.id == legacy.as_str()))
            {
                return;
            }
        }
    })
    .await;
    assert!(
        leaked.is_err(),
        "a legacy-owned message reached the receipt stream"
    );
    assert!(
        hub.lock().receipts.gens[&("p".to_string(), "a".to_string())]
            .model
            .delivery(legacy.as_str())
            .is_none_or(|d| d.attempts.is_empty()),
        "no attempt is issued for a legacy-owned message"
    );
    // Nor does the legacy hold, or nack, touch the receipt-owned hand-out,
    // even sent as its own consumer.
    let consumer = match &hub.lock().inbox.get(receipt.as_str(), "p").unwrap().state {
        MessageState::Fetched { by, .. } => by.clone(),
        other => panic!("fetched: {other:?}"),
    };
    assert!(!hub.hold(receipt.as_str(), "p", "a", &consumer));
    assert!(!hub.nack(receipt.as_str(), "p", "a"));
    assert_eq!(state_of(&hub, "p", receipt.as_str()), StateName::Fetched);
    peer.close().await;
}

/// Scenario: A receipt stream is handed a message, its watcher publishes the `handed` and `committed` receipts into the spool, and the component reports the commit. The message settles, the attempt's evidence reservation is returned, and every spool file of that attempt is removed.
#[spec("inbox/boundary/002")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_002_returning_a_reservation_removes_the_attempts_files() {
    let state = tempfile::tempdir().unwrap();
    let state = state.path().canonicalize().unwrap();
    let hub = hub_with(Duration::from_secs(30), Duration::from_secs(60));
    hub.set_spool(crate::inbox_receipt::Spool::start(&state).unwrap());
    let mut peer = Peer::open(&hub, "p", "a", InboxComponent::OpenCodePlugin, INTEGRATION).await;
    peer.next().await;
    let id = hub
        .post_direct(Recipient::bound("p", "a"), content("settle me"))
        .unwrap();
    let handed = peer.handout().await;
    let first = attempt(&handed[0]).clone();
    let caps = first.caps.clone().expect("the watcher is handed the caps");
    let (pane, agent) = spool_names("p", "a");
    let spool =
        crate::inbox_receipt::GenerationSpool::open(&hub.spool_env().unwrap(), &pane, &agent)
            .unwrap();
    for (stage, cap) in [
        (crate::inbox_receipt::RecordStage::Handed, &caps.handed),
        (crate::inbox_receipt::RecordStage::Committed, &caps.commit),
    ] {
        spool
            .publish(
                &crate::inbox_receipt::RecordKey {
                    msg: id.to_string(),
                    a: first.a,
                    e: first.e,
                    stage,
                },
                &crate::inbox_receipt::Cap::from_hex(cap).unwrap(),
            )
            .unwrap();
    }
    spool.write_revoke(id.as_str(), first.a, first.e).unwrap();
    let record = |stage: &str| {
        hub.spool_env()
            .unwrap()
            .join(&pane)
            .join(&agent)
            .join("rec")
            .join(format!("{id}.{}.{}.{stage}", first.a, first.e))
    };
    assert!(record("handed").exists() && record("committed").exists());
    peer.send(serde_json::json!({"committed": [at(&handed[0])]}))
        .await;
    until("the commit to settle", || {
        state_of(&hub, "p", id.as_str()) == StateName::Acknowledged
    })
    .await;
    until("the reclaimed attempt's files to go", || {
        !record("handed").exists() && !record("committed").exists()
    })
    .await;
    assert!(
        !spool.revoked(id.as_str(), first.a, first.e),
        "the revoke marker went too"
    );
    peer.close().await;
}

/// Scenario: An OpenCode generation starts with an integration-2 adapter and nothing registers by the readiness deadline. Its card says its integration has not connected yet and its mail waits queued; a late registration clears the notice and delivers the queued mail, with no restart.
#[spec("inbox/boundary/003")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_003_a_late_registration_clears_not_armed_and_delivers() {
    let hub = hub_with(Duration::from_secs(30), Duration::from_secs(60));
    hub.lock().receipts.arm_deadline = Duration::from_millis(50);
    let (tx, mut rx) = broadcast::channel(64);
    hub.set_broadcast(tx);
    hub.expect_integration("p", "a");
    let id = hub
        .post_direct(Recipient::bound("p", "a"), content("waiting"))
        .unwrap();
    assert_eq!(
        hub.integration_notice("p", "a"),
        None,
        "not before the deadline"
    );
    tokio::time::sleep(Duration::from_millis(80)).await;
    hub.tick();
    assert_eq!(
        hub.integration_notice("p", "a"),
        Some(IntegrationNotice::NotArmed)
    );
    assert_eq!(
        hub.summary("p", "a").notices.integration,
        Some(IntegrationNotice::NotArmed),
        "the snapshot carries it"
    );
    assert_eq!(state_of(&hub, "p", id.as_str()), StateName::Queued);
    let mut told = Vec::new();
    while let Ok(BroadcastMsg::Inbox(notice)) = rx.try_recv() {
        told.push(notice);
    }
    assert!(
        told.iter()
            .any(|n| n.attention_update
                && n.notices.integration == Some(IntegrationNotice::NotArmed)),
        "both clients are told: {told:?}"
    );
    let mut peer = Peer::open(&hub, "p", "a", InboxComponent::OpenCodePlugin, INTEGRATION).await;
    peer.next().await;
    let handed = peer.handout().await;
    assert_eq!(handed[0].id, id.as_str(), "the queued mail is delivered");
    assert_eq!(hub.integration_notice("p", "a"), None);
    let mut cleared = false;
    while let Ok(BroadcastMsg::Inbox(notice)) = rx.try_recv() {
        cleared |= notice.attention_update && notice.notices.integration.is_none();
    }
    assert!(cleared, "the clients are told it cleared");
    peer.close().await;
}

/// Scenario: An OpenCode plugin from before the floor arms its stream without declaring an integration. It is refused, nothing registers, its mail waits queued, its doorbell stays silent and its card says the integration is out of date.
#[spec("inbox/boundary/004")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_004_an_old_adapter_is_refused_and_its_mail_waits() {
    let hub = hub_with(Duration::from_secs(30), Duration::from_secs(60));
    let id = hub
        .post_direct(Recipient::bound("p", "a"), content("waiting"))
        .unwrap();
    for declared in [None, Some(1)] {
        let mut peer = Peer::open(
            &hub,
            "p",
            "a",
            InboxComponent::OpenCodePlugin,
            ReceiptArm {
                adapter_integration: declared,
                ..ReceiptArm::default()
            },
        )
        .await;
        let reply = peer.next().await;
        assert_eq!(
            reply.refused.as_deref(),
            Some("adapter_too_old"),
            "{declared:?}"
        );
        assert_eq!(
            reply.error.as_deref(),
            Some(crate::inbox_display::ADAPTER_TOO_OLD)
        );
        peer.close().await;
    }
    assert_eq!(
        hub.integration_notice("p", "a"),
        Some(IntegrationNotice::AdapterTooOld)
    );
    assert_eq!(state_of(&hub, "p", id.as_str()), StateName::Queued);
    assert!(
        hub.doorbell_facts("p", "a").blocked,
        "no doorbell for an incompatible integration"
    );
    assert_eq!(hub.armed("p", "a"), 0);
}

/// Scenario: The capability probe runs a stand-in for the CLI an adapter pins. clap refusing the flag proves the CLI too old; help naming the flag proves it; a probe that outlives its deadline, or any other answer, proves nothing and sets no notice.
#[spec("inbox/boundary/005")]
#[test]
fn boundary_005_the_capability_probe_proves_only_what_clap_says() {
    let dir = tempfile::tempdir().unwrap();
    let stand_in = |name: &str, body: &str| {
        let path = dir.path().join(name);
        crate::test_isolation::write_script(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    };
    let old = stand_in(
        "old",
        "echo \"error: unexpected argument '--adapter-integration' found\" >&2\nexit 2",
    );
    let new = stand_in(
        "new",
        "echo 'Usage: dot-agent-deck inbox wait [OPTIONS] --agent <AGENT>'\necho '      --adapter-integration <N>'\nexit 0",
    );
    let slow = stand_in("slow", "sleep 5");
    let other = stand_in("other", "echo 'error: something else' >&2\nexit 2");
    let deadline = Duration::from_millis(500);
    assert_eq!(probe_cli(&old, deadline), ProbeVerdict::CliTooOld);
    assert_eq!(probe_cli(&new, deadline), ProbeVerdict::Supported);
    let started = Instant::now();
    assert_eq!(probe_cli(&slow, deadline), ProbeVerdict::Unknown);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the deadline bounds the probe"
    );
    assert_eq!(probe_cli(&other, deadline), ProbeVerdict::Unknown);
    assert_eq!(
        probe_cli(&dir.path().join("missing"), deadline),
        ProbeVerdict::Unknown
    );
    let hub = hub_with(Duration::from_secs(30), Duration::from_secs(60));
    hub.set_cli_too_old("p", "a");
    assert_eq!(
        hub.integration_notice("p", "a"),
        Some(IntegrationNotice::CliTooOld)
    );
    assert!(hub.doorbell_facts("p", "a").blocked);
}

/// Hand `id` out on `peer` until its attempts run out of automatic repeats:
/// each hand-out is omitted by the next snapshot, so it lapses unpossessed.
async fn lapse_until_uncertain(
    hub: &Arc<InboxHub>,
    peer: &mut Peer,
    id: &str,
    seq: &mut u64,
) -> Vec<InboxDelivery> {
    let mut seen = Vec::new();
    for round in 0..=crate::inbox_attempts::MAX_AUTO_REPEATS {
        let handed = peer.handout().await;
        assert_eq!(handed[0].id, id, "round {round}");
        let d = attempt(&handed[0]).d;
        assert_eq!(attempt(&handed[0]).repeat, round > 0, "repeats are marked");
        if round > 0 {
            assert!(
                handed[0]
                    .text
                    .contains(&crate::inbox_display::repeat_line(id)),
                "the frame carries the possible-repeat line"
            );
        }
        seen.push(handed[0].clone());
        *seq += 1;
        peer.send(serde_json::json!({"possess": {"seq": *seq, "seen": d, "held": []}}))
            .await;
        hub.tick();
    }
    seen
}

/// Scenario: A delivered message's attempts lapse unpossessed through both marked repeats, and the card lists it as uncertain with its revision. Two clients press Retry at that revision: one is applied and sends a marked repeat, the other is told another client acted first; a Cancel then revokes the open attempt and the message ends cancelled.
#[spec("inbox/boundary/006")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_006_uncertain_retry_and_cancel_through_the_hub() {
    let hub = hub_with(Duration::from_secs(30), Duration::from_secs(60));
    let mut peer = Peer::open(&hub, "p", "a", InboxComponent::OpenCodePlugin, INTEGRATION).await;
    peer.next().await;
    let id = hub
        .post_direct(Recipient::bound("p", "a"), content("unsure"))
        .unwrap();
    let mut seq = 0;
    lapse_until_uncertain(&hub, &mut peer, id.as_str(), &mut seq).await;
    until("the card to list it", || {
        hub.tick();
        hub.summary("p", "a").notices.attention.len() == 1
    })
    .await;
    let item = hub.summary("p", "a").notices.attention[0].clone();
    assert_eq!(item.kind, AttentionKind::Uncertain);
    assert_eq!(item.message_id, id.as_str());
    assert_eq!(item.subject, "about unsure");
    let first = hub.inbox_action("p", "a", id.as_str(), item.revision, true, "desktop-1");
    let second = hub.inbox_action("p", "a", id.as_str(), item.revision, true, "tui-1");
    let again = hub.inbox_action("p", "a", id.as_str(), item.revision, true, "desktop-1");
    use crate::daemon_protocol::InboxActionOutcome;
    assert_eq!(first.outcome, InboxActionOutcome::Applied);
    assert_eq!(
        second.outcome,
        InboxActionOutcome::Stale,
        "the second client is told so"
    );
    assert_eq!(
        again.outcome,
        InboxActionOutcome::Applied,
        "a retransmission answers as before"
    );
    let manual = peer.handout().await;
    assert_eq!(manual.len(), 1, "one repeat for two presses");
    assert!(attempt(&manual[0]).repeat);
    assert!(
        hub.summary("p", "a").notices.attention.is_empty(),
        "no longer uncertain"
    );
    let revision = hub.lock().receipts.gens[&("p".to_string(), "a".to_string())]
        .model
        .delivery(id.as_str())
        .unwrap()
        .revision;
    let cancel = hub.inbox_action("p", "a", id.as_str(), revision, false, "desktop-2");
    assert_eq!(cancel.outcome, InboxActionOutcome::Applied);
    // Cancel revokes every open attempt, the lapsed repeats included; until
    // each is answered the message is held, never repeated.
    let mut answered = 0;
    while state_of(&hub, "p", id.as_str()) != StateName::Undeliverable {
        let reply = peer.next().await;
        assert!(
            reply.messages.is_none(),
            "nothing is handed out under the latch"
        );
        for revoked in reply.revoke {
            assert_eq!(revoked.id, id.as_str());
            answered += 1;
            peer.send(serde_json::json!({"revoked_ack": [revoked]}))
                .await;
        }
        if answered == 1 + crate::inbox_attempts::MAX_AUTO_REPEATS as usize + 1 {
            until("the cancel to end the message", || {
                state_of(&hub, "p", id.as_str()) == StateName::Undeliverable
            })
            .await;
        }
    }
    assert_eq!(
        answered, 4,
        "the first hand-out, two automatic repeats and the Retry"
    );
    let reason = hub
        .list("p")
        .into_iter()
        .find(|m| m.id == id)
        .and_then(|m| m.undeliverable_reason);
    assert_eq!(reason, Some(UndeliverableReason::Cancelled));
    peer.close().await;
}

/// Scenario: The OpenCode server and TUI contexts register side by side. A doorbell substitution binds its attempt to the server context's stream; the TUI context's watcher reconnecting retires only the TUI incarnation, so the server context's attempt is neither resumed nor repeated, and its commit settles it.
#[spec("inbox/boundary/007")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_007_server_and_tui_contexts_never_fence_each_other() {
    let hub = hub_with(Duration::from_secs(30), Duration::from_secs(60));
    let mut server = Peer::open(
        &hub,
        "p",
        "a",
        InboxComponent::OpenCodeMini,
        ReceiptArm {
            doorbell_only: true,
            ..INTEGRATION
        },
    )
    .await;
    let rx = server.next().await.registered.unwrap().rx;
    assert_eq!(
        hub.armed("p", "a"),
        0,
        "a doorbell-only stream does not count as armed"
    );
    let id = hub
        .post_direct(Recipient::bound("p", "a"), content("doorbell"))
        .unwrap();
    let fetched = hub
        .receipt_fetch("p", "a", rx, Some(id.as_str()), None)
        .unwrap();
    assert_eq!(fetched.len(), 1);
    assert!(
        attempt(&fetched[0]).caps.is_none(),
        "caps never reach the fetch's caller"
    );
    let issued = server.next().await.issued;
    assert_eq!(
        issued[0].id,
        id.as_str(),
        "the caps went to the server stream"
    );
    server
        .send(serde_json::json!({"possess": {"seq": 1, "seen": attempt(&fetched[0]).d, "held": [at(&fetched[0])]}}))
        .await;
    let mut tui = Peer::open(&hub, "p", "a", InboxComponent::OpenCodePlugin, INTEGRATION).await;
    assert!(tui.next().await.resume.is_empty());
    tui.close().await;
    let mut tui = Peer::open(&hub, "p", "a", InboxComponent::OpenCodePlugin, INTEGRATION).await;
    let registered = tui.next().await;
    assert!(
        registered.resume.is_empty(),
        "the server context's attempt is not the TUI's to resume"
    );
    hub.tick();
    assert_eq!(state_of(&hub, "p", id.as_str()), StateName::Fetched);
    server
        .send(serde_json::json!({"committed": [at(&fetched[0])]}))
        .await;
    until("the server context's commit", || {
        state_of(&hub, "p", id.as_str()) == StateName::Acknowledged
    })
    .await;
    server.close().await;
    tui.close().await;
}

/// Scenario: The TUI context's watcher reconnects while the context still holds an attempt: the new incarnation is asked to resume it, answers retained, and the attempt is rebound with no repeat. A reloaded context that knows nothing answers unknown instead, and the message gets one marked repeat on the same generation.
#[spec("inbox/boundary/008")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_008_retained_resume_rebinds_and_unknown_resume_repeats() {
    let hub = hub_with(Duration::from_secs(30), Duration::from_secs(60));
    let mut first = Peer::open(&hub, "p", "a", InboxComponent::OpenCodePlugin, INTEGRATION).await;
    first.next().await;
    let id = hub
        .post_direct(Recipient::bound("p", "a"), content("keep me"))
        .unwrap();
    let handed = first.handout().await;
    first
        .send(serde_json::json!({"possess": {"seq": 1, "seen": attempt(&handed[0]).d, "held": [at(&handed[0])]}}))
        .await;
    // The watcher respawns for the same context.
    let mut second = Peer::open(
        &hub,
        "p",
        "a",
        InboxComponent::OpenCodePlugin,
        ReceiptArm {
            resumed: true,
            ..INTEGRATION
        },
    )
    .await;
    let registered = second.next().await;
    assert_eq!(registered.resume.len(), 1, "{registered:?}");
    second
        .send(serde_json::json!({"resume_answers": [{"id": id.as_str(), "a": attempt(&handed[0]).a, "e": attempt(&handed[0]).e, "answer": "retained"}]}))
        .await;
    let rebound = loop {
        let reply = second.next().await;
        assert!(
            reply.messages.is_none(),
            "a retained attempt is not repeated"
        );
        if !reply.rebound.is_empty() {
            break reply.rebound;
        }
    };
    second
        .send(serde_json::json!({"possess": {"seq": 2, "seen": rebound[0].d, "held": [at(&handed[0])]}}))
        .await;
    hub.tick();
    assert_eq!(state_of(&hub, "p", id.as_str()), StateName::Fetched);
    first.close().await;
    // The context reloads and knows nothing of it.
    let mut third = Peer::open(&hub, "p", "a", InboxComponent::OpenCodePlugin, INTEGRATION).await;
    let registered = third.next().await;
    assert_eq!(registered.resume.len(), 1);
    third
        .send(serde_json::json!({"resume_answers": [{"id": id.as_str(), "a": attempt(&handed[0]).a, "e": attempt(&handed[0]).e, "answer": "unknown"}]}))
        .await;
    let repeat = third.handout().await;
    assert_eq!(repeat[0].id, id.as_str());
    assert!(
        attempt(&repeat[0]).repeat,
        "a marked repeat to the same generation"
    );
    assert_eq!(attempt(&repeat[0]).a, attempt(&handed[0]).a);
    second.close().await;
    third.close().await;
}

/// Scenario: The TUI context's watcher closes its stream and nothing registers in its place for a while. Nothing is requeued; with no snapshot the attempt lapses unpossessed. The next registration of the slot is asked to resume it, and a context that knows nothing of it answers unknown and gets a marked repeat, never a requeue.
#[spec("inbox/boundary/009")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_009_a_closed_stream_decides_nothing_and_the_next_registration_resumes() {
    let hub = hub_with(Duration::from_millis(50), Duration::from_secs(60));
    let mut peer = Peer::open(&hub, "p", "a", InboxComponent::OpenCodePlugin, INTEGRATION).await;
    peer.next().await;
    let id = hub
        .post_direct(Recipient::bound("p", "a"), content("dead adapter"))
        .unwrap();
    let handed = peer.handout().await;
    peer.send(serde_json::json!({"handed": [at(&handed[0])]}))
        .await;
    peer.close().await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    hub.tick();
    assert_eq!(
        state_of(&hub, "p", id.as_str()),
        StateName::Fetched,
        "a closed stream requeues nothing"
    );
    let mut next = Peer::open(&hub, "p", "a", InboxComponent::OpenCodePlugin, INTEGRATION).await;
    let registered = next.next().await;
    assert_eq!(
        registered.resume.len(),
        1,
        "the slot's incarnation is asked to resume"
    );
    next.send(serde_json::json!({"resume_answers": [{"id": id.as_str(), "a": 1, "e": attempt(&handed[0]).e, "answer": "unknown"}]}))
        .await;
    let repeat = next.handout().await;
    assert!(attempt(&repeat[0]).repeat);
    next.close().await;
}

/// Scenario: A component holds a message across many keepalives with fresh snapshots naming it. It is never repeated and never lapses; held for the held_long span it raises one attention item on the card, which a commit clears.
#[spec("inbox/boundary/010")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_010_fresh_possession_never_repeats_and_held_long_is_attention() {
    let hub = hub_with(Duration::from_millis(60), Duration::from_secs(60));
    hub.lock().receipts.limits.held_long_after = Duration::from_millis(400);
    // The limits the model already holds are its own; a fresh generation
    // takes the shortened span.
    let mut peer = Peer::open(&hub, "p", "a", InboxComponent::OpenCodePlugin, INTEGRATION).await;
    peer.next().await;
    let id = hub
        .post_direct(Recipient::bound("p", "a"), content("long turn"))
        .unwrap();
    let handed = peer.handout().await;
    let d = attempt(&handed[0]).d;
    let mut held_long = false;
    for seq in 1..=12u64 {
        peer.send(
            serde_json::json!({"possess": {"seq": seq, "seen": d, "held": [at(&handed[0])]}}),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(60)).await;
        hub.tick();
        let summary = hub.summary("p", "a");
        held_long |= summary
            .notices
            .attention
            .iter()
            .any(|item| item.kind == AttentionKind::HeldLong);
    }
    assert!(held_long, "held past the span raises held_long attention");
    assert_eq!(state_of(&hub, "p", id.as_str()), StateName::Fetched);
    peer.send(serde_json::json!({"committed": [at(&handed[0])]}))
        .await;
    until("the commit", || {
        state_of(&hub, "p", id.as_str()) == StateName::Acknowledged
    })
    .await;
    assert!(hub.summary("p", "a").notices.attention.is_empty());
    peer.close().await;
}
