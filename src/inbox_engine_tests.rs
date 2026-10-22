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
    hook_integration: None,
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
        Self::open_buffered(hub, pane, agent, component, arm, 1 << 16).await
    }

    /// [`Self::open`] for the plugin instance `instance`.
    async fn open_instance(
        hub: &Arc<InboxHub>,
        pane: &str,
        agent: &str,
        component: InboxComponent,
        arm: ReceiptArm,
        instance: u32,
    ) -> Peer {
        Self::open_with(hub, pane, agent, component, arm, 1 << 16, Some(instance)).await
    }

    /// [`Self::open`] over a connection that buffers only `buffer` bytes, so
    /// the daemon's writer stops while the test is not reading.
    async fn open_buffered(
        hub: &Arc<InboxHub>,
        pane: &str,
        agent: &str,
        component: InboxComponent,
        arm: ReceiptArm,
        buffer: usize,
    ) -> Peer {
        Self::open_with(hub, pane, agent, component, arm, buffer, None).await
    }

    async fn open_with(
        hub: &Arc<InboxHub>,
        pane: &str,
        agent: &str,
        component: InboxComponent,
        arm: ReceiptArm,
        buffer: usize,
        instance: Option<u32>,
    ) -> Peer {
        let (client, server) = tokio::io::duplex(buffer);
        let (read, write) = tokio::io::split(server);
        let serve = tokio::spawn(serve_waiter(
            Arc::clone(hub),
            pane.to_string(),
            agent.to_string(),
            InboxWaitMode::Stream,
            component,
            instance,
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

    /// The next hand-out line, ticking `hub` meanwhile: a repeat waits for
    /// the clock's spool check (§3), and no ticker runs in these tests.
    async fn handout_ticking(&mut self, hub: &InboxHub) -> Vec<InboxDelivery> {
        let started = Instant::now();
        loop {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "a hand-out within the time"
            );
            hub.tick();
            if let Ok(reply) = tokio::time::timeout(Duration::from_millis(20), self.next()).await
                && let Some(messages) = reply.messages
            {
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

/// Scenario: One pane holds a message an `inbox fetch` took (legacy) and one an OpenCode receipt stream took. With the ack-wait elapsed, the legacy ack-wait redelivers only its own; the receipt-owned hand-out stays where its attempt model put it even with its own ack deadline run out, and a receipt receiver never takes the legacy-owned one back.
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
    // Nor does the legacy ack-wait touch the receipt-owned hand-out, even
    // with its deadline run out.
    hub.run_out_ack_wait(receipt.as_str(), "p");
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
    assert!(spool.revoked(id.as_str(), first.a, first.e));
    peer.send(serde_json::json!({"committed": [at(&handed[0])]}))
        .await;
    until("the commit to settle", || {
        state_of(&hub, "p", id.as_str()) == StateName::Acknowledged
    })
    .await;
    // The hub removes the files one at a time on the thread that took the
    // commit, the revoke marker last, so wait for all of them: the records
    // going says nothing yet about the marker.
    until(
        "the reclaimed attempt's records and revoke marker to go",
        || {
            !record("handed").exists()
                && !record("committed").exists()
                && !spool.revoked(id.as_str(), first.a, first.e)
        },
    )
    .await;
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

/// A stand-in CLI named `name` running `unix` (a `/bin/sh` body) or, on
/// Windows, `windows` (a `cmd.exe` batch body, CRLF line endings, as a
/// `.cmd` the probe runs through `cmd.exe`).
fn probe_stand_in(
    dir: &std::path::Path,
    name: &str,
    unix: &str,
    windows: &str,
) -> std::path::PathBuf {
    #[cfg(unix)]
    {
        let _ = windows;
        let path = dir.join(name);
        crate::test_isolation::write_script(&path, format!("#!/bin/sh\n{unix}\n")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }
    #[cfg(not(unix))]
    {
        let _ = unix;
        let path = dir.join(format!("{name}.cmd"));
        let body = format!("@echo off\r\n{}\r\n", windows.replace('\n', "\r\n"));
        std::fs::write(&path, body).unwrap();
        path
    }
}

/// Scenario: The capability probe runs a stand-in for the CLI an adapter pins — a shell script, or on Windows a batch file whose output ends its lines with CRLF. clap refusing the flag proves the CLI too old, and so does the same refusal styled with ANSI colour; help naming the flag proves it; a probe that outlives its deadline, or any other answer, proves nothing and sets no notice.
#[spec("inbox/boundary/005")]
#[test]
fn boundary_005_the_capability_probe_proves_only_what_clap_says() {
    let dir = tempfile::tempdir().unwrap();
    let old = probe_stand_in(
        dir.path(),
        "old",
        "echo \"error: unexpected argument '--adapter-integration' found\" >&2\nexit 2",
        ">&2 echo error: unexpected argument '--adapter-integration' found\nexit /b 2",
    );
    let styled = probe_stand_in(
        dir.path(),
        "styled",
        "printf '\\033[1m\\033[31merror:\\033[0m unexpected argument \\047\\033[33m--adapter-integration\\033[0m\\047 found\\r\\n' >&2\nexit 2",
        ">&2 echo error: unexpected argument '--adapter-integration' found\nexit /b 2",
    );
    let new = probe_stand_in(
        dir.path(),
        "new",
        "echo 'Usage: dot-agent-deck inbox wait [OPTIONS] --agent <AGENT>'\necho '      --adapter-integration <N>'\nexit 0",
        "echo Usage: dot-agent-deck.exe inbox wait [OPTIONS] --agent ^<AGENT^>\necho       --adapter-integration ^<N^>\nexit /b 0",
    );
    let slow = probe_stand_in(dir.path(), "slow", "sleep 5", "ping -n 6 127.0.0.1 >nul");
    let other = probe_stand_in(
        dir.path(),
        "other",
        "echo 'error: something else' >&2\nexit 2",
        ">&2 echo error: something else\nexit /b 2",
    );
    let deadline = Duration::from_millis(500);
    assert_eq!(
        probe_cli(&old, Duration::from_secs(5)),
        ProbeVerdict::CliTooOld
    );
    assert_eq!(
        probe_cli(&styled, Duration::from_secs(5)),
        ProbeVerdict::CliTooOld,
        "styling and a CRLF do not hide the refusal"
    );
    assert_eq!(
        probe_cli(&new, Duration::from_secs(5)),
        ProbeVerdict::Supported
    );
    let started = Instant::now();
    assert_eq!(probe_cli(&slow, deadline), ProbeVerdict::Unknown);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the deadline bounds the probe"
    );
    assert_eq!(
        probe_cli(&other, Duration::from_secs(5)),
        ProbeVerdict::Unknown
    );
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
        let handed = if round == 0 {
            peer.handout().await
        } else {
            peer.handout_ticking(hub).await
        };
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
    let repeat = third.handout_ticking(&hub).await;
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
    // POSSESS_TTL is 300 ms; a closed incarnation is retired three of it
    // after its close, when no registration took its place.
    let hub = hub_with(Duration::from_millis(100), Duration::from_secs(60));
    let mut peer = Peer::open(&hub, "p", "a", InboxComponent::OpenCodePlugin, INTEGRATION).await;
    peer.next().await;
    let id = hub
        .post_direct(Recipient::bound("p", "a"), content("dead adapter"))
        .unwrap();
    let handed = peer.handout().await;
    peer.send(serde_json::json!({"handed": [at(&handed[0])]}))
        .await;
    peer.close().await;
    tokio::time::sleep(Duration::from_millis(400)).await;
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
    let repeat = next.handout_ticking(&hub).await;
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

/// Scenario: The daemon's receipt spool cannot start (its root is a plain file), and an OpenCode receipt stream registers. Its card names the storage failure as degraded receipts while its mail is still handed out, rather than the deck carrying on as if receipts protected it.
#[spec("inbox/boundary/016")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_016_a_spool_that_cannot_start_is_degraded_storage() {
    let state = tempfile::tempdir().unwrap();
    let state = state.path().canonicalize().unwrap();
    std::fs::write(state.join(crate::inbox_receipt::SPOOL_DIR_NAME), b"").unwrap();
    let error = crate::inbox_receipt::Spool::start(&state).unwrap_err();
    let hub = hub_with(Duration::from_secs(30), Duration::from_secs(60));
    hub.set_spool_error(&error.to_string());
    assert_eq!(hub.spool_env(), None, "nothing is pinned without a spool");
    let mut peer = Peer::open(&hub, "p", "a", InboxComponent::OpenCodePlugin, INTEGRATION).await;
    assert!(peer.next().await.registered.is_some());
    let id = hub
        .post_direct(Recipient::bound("p", "a"), content("still delivered"))
        .unwrap();
    let handed = peer.handout().await;
    assert_eq!(handed[0].id, id.as_str());
    let degraded = hub
        .summary("p", "a")
        .notices
        .receipts_degraded
        .expect("the card says receipts are degraded");
    assert!(
        degraded.contains("could not start") && degraded.contains(&error.to_string()),
        "it names the start failure: {degraded}"
    );
    peer.close().await;
}

/// Scenario: Run the capability probe against stand-in CLIs that come close to clap's refusal without being it (among them its words behind a cursor-control sequence, and its words followed by an unterminated escape), an old help that exits 0 without the flag, one that names the flag only in prose, one that writes a megabyte to each stream after its help, one that leaves a background process holding its output open, and one that hangs; then let a registration land before a late `cli_too_old`. Only clap's exact diagnostic on exit 2 is `cli_too_old` and only a help line listing the flag is support; the chatty CLI, which fails if a write to it fails, is drained and read as supported before the deadline; the probe returns by its deadline despite the held output, and kills and reaps the hung CLI; the late result changes nothing after the registration.
#[spec("inbox/boundary/017")]
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_017_the_probe_reads_only_clap_and_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let stand_in = |name: &str, body: &str| {
        let path = dir.path().join(name);
        crate::test_isolation::write_script(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    };
    let exact = "error: unexpected argument '--adapter-integration' found";
    let deadline = Duration::from_millis(500);
    for (name, body) in [
        (
            "loose",
            "echo \"warning: unexpected argument '--adapter-integration' found\" >&2\nexit 2"
                .to_string(),
        ),
        ("exit-1", format!("echo \"{exact}\" >&2\nexit 1")),
        (
            "tip-only",
            "echo \"error: unexpected argument '--bogus' found\" >&2\necho \"  tip: a similar argument exists: '--adapter-integration'\" >&2\nexit 2".to_string(),
        ),
        (
            "old-help",
            "echo 'Usage: dot-agent-deck inbox wait --agent <AGENT>'\necho '      --agent <AGENT>  The agent'\nexit 0".to_string(),
        ),
        (
            "prose",
            "echo 'Usage: dot-agent-deck inbox wait --agent <AGENT>'\necho '      --agent <AGENT>  Unlike --adapter-integration, names the agent'\nexit 0".to_string(),
        ),
        (
            // Cursor control, not styling: the line as written is not clap's.
            "cursor",
            "printf '\\033[2Kerror: unexpected argument \\047--adapter-integration\\047 found\\n' >&2\nexit 2".to_string(),
        ),
        (
            "unterminated",
            "printf 'error: unexpected argument \\047--adapter-integration\\047 found\\033[1\\n' >&2\nexit 2".to_string(),
        ),
    ] {
        let cli = stand_in(name, &body);
        assert_eq!(probe_cli(&cli, deadline), ProbeVerdict::Unknown, "{name}");
    }
    let refused = stand_in(
        "refused",
        &format!(
            "echo \"{exact}\" >&2\necho >&2\necho \"Usage: dot-agent-deck inbox wait --agent <AGENT>\" >&2\nexit 2"
        ),
    );
    assert_eq!(probe_cli(&refused, deadline), ProbeVerdict::CliTooOld);

    let chatty = stand_in(
        "chatty",
        // A write the probe stopped reading fails, as it fails a Rust CLI
        // (EPIPE), and turns the exit status non-zero.
        "echo '      --adapter-integration <N>  The integration'\nhead -c 1048576 /dev/zero | tr '\\0' x || exit 3\nhead -c 1048576 /dev/zero | tr '\\0' y >&2 || exit 3\nexit 0",
    );
    let started = Instant::now();
    assert_eq!(
        probe_cli(&chatty, Duration::from_secs(5)),
        ProbeVerdict::Supported,
        "both streams are drained past the cap"
    );
    assert!(started.elapsed() < Duration::from_secs(4));

    let held = stand_in(
        "held",
        "(sleep 3) &\necho '      --adapter-integration <N>  The integration'\nexit 0",
    );
    let started = Instant::now();
    let _ = probe_cli(&held, deadline);
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "output held open by another process does not outlast the deadline: {:?}",
        started.elapsed()
    );

    let pid_file = dir.path().join("hung.pid");
    let hung = stand_in(
        "hung",
        &format!("echo $$ > '{}'\nexec sleep 30", pid_file.display()),
    );
    assert_eq!(probe_cli(&hung, deadline), ProbeVerdict::Unknown);
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // SAFETY: signal 0 only asks whether `pid` still exists; a zombie still
    // does, so failure here means the child was killed and reaped.
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "the hung CLI is killed and reaped"
    );

    let hub = hub_with(Duration::from_secs(30), Duration::from_secs(60));
    hub.expect_integration("p", "a");
    let mut peer = Peer::open(&hub, "p", "a", InboxComponent::OpenCodePlugin, INTEGRATION).await;
    assert!(peer.next().await.registered.is_some());
    hub.set_cli_too_old("p", "a");
    assert_eq!(
        hub.integration_notice("p", "a"),
        None,
        "a registration outranks a late probe"
    );
    assert!(!hub.doorbell_facts("p", "a").blocked);
    let id = hub
        .post_direct(Recipient::bound("p", "a"), content("after a late probe"))
        .unwrap();
    assert_eq!(peer.handout().await[0].id, id.as_str());
    peer.close().await;
}

/// A hub with a real spool under a fresh state directory, kept alive by the
/// returned guard.
fn hub_with_spool(keepalive: Duration, ack_wait: Duration) -> (Arc<InboxHub>, tempfile::TempDir) {
    let state = tempfile::tempdir().unwrap();
    let hub = hub_with(keepalive, ack_wait);
    hub.set_spool(
        crate::inbox_receipt::Spool::start(&state.path().canonicalize().unwrap()).unwrap(),
    );
    (hub, state)
}

/// The `rec` directory of `pane`/`agent`'s generation spool.
fn rec_dir(hub: &InboxHub, pane: &str, agent: &str) -> std::path::PathBuf {
    let (pane, agent) = spool_names(pane, agent);
    hub.spool_env().unwrap().join(pane).join(agent).join("rec")
}

/// Publish `stage`'s receipt for the attempt `handed` names, as its watcher
/// would, carrying the cap the hand-out gave it.
fn publish_receipt(
    hub: &InboxHub,
    pane: &str,
    agent: &str,
    handed: &InboxDelivery,
    stage: crate::inbox_receipt::RecordStage,
) {
    let (p, a) = spool_names(pane, agent);
    let spool =
        crate::inbox_receipt::GenerationSpool::open(&hub.spool_env().unwrap(), &p, &a).unwrap();
    let at = attempt(handed);
    let caps = at.caps.clone().expect("the watcher is handed the caps");
    let cap = match stage {
        crate::inbox_receipt::RecordStage::Handed => caps.handed,
        crate::inbox_receipt::RecordStage::Committed => caps.commit,
    };
    spool
        .publish(
            &crate::inbox_receipt::RecordKey {
                msg: handed.id.clone(),
                a: at.a,
                e: at.e,
                stage,
            },
            &crate::inbox_receipt::Cap::from_hex(&cap).unwrap(),
        )
        .unwrap();
}

/// Tick `hub` for `span`, failing if `peer` is handed anything meanwhile.
async fn no_handout_while_ticking(
    hub: &Arc<InboxHub>,
    peer: &mut Peer,
    span: Duration,
    what: &str,
) {
    let started = Instant::now();
    while started.elapsed() < span {
        hub.tick();
        if let Ok(reply) = tokio::time::timeout(Duration::from_millis(20), peer.next()).await {
            assert!(
                reply.messages.is_none(),
                "{what}: a further copy was handed out: {reply:?}"
            );
        }
    }
}

fn attempts_of(hub: &InboxHub, pane: &str, agent: &str, id: &str) -> usize {
    hub.lock().receipts.gens[&(pane.to_string(), agent.to_string())]
        .model
        .delivery(id)
        .map_or(0, |d| d.attempts.len())
}

/// Scenario: For an OpenCode and then a Pi receipt stream, two messages are handed to a receipt stream, and a directory squats on each one's committed receipt name, so the spool lookup cannot say whether either committed. The stream's next snapshot omits the first, the second's issue grace runs out with no snapshot covering it, and the clock ticks through several possession TTLs: neither is handed out again, no new attempt is issued for either, and no repeat is even scheduled, so the doorbell's tick sees no waiting mail. Once the wait bound passes, both are listed on the card as uncertain, with Retry and Cancel, rather than repeated.
#[spec("inbox/boundary/019")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_019_an_unknown_lookup_blocks_every_repeat() {
    for component in [InboxComponent::OpenCodePlugin, InboxComponent::PiExtension] {
        let (hub, _state) = hub_with_spool(Duration::from_millis(50), Duration::from_secs(60));
        let mut peer = Peer::open(&hub, "p", "a", component, INTEGRATION).await;
        peer.next().await;
        let omitted = hub
            .post_direct(Recipient::bound("p", "a"), content("omitted"))
            .unwrap();
        let first = peer.handout().await;
        let expired = hub
            .post_direct(Recipient::bound("p", "a"), content("expired"))
            .unwrap();
        let second = peer.handout().await;
        for handed in [&first[0], &second[0]] {
            let at = attempt(handed);
            std::fs::create_dir(
                rec_dir(&hub, "p", "a").join(format!("{}.{}.{}.committed", handed.id, at.a, at.e)),
            )
            .unwrap();
        }
        peer.send(
            serde_json::json!({"possess": {"seq": 1, "seen": attempt(&first[0]).d, "held": []}}),
        )
        .await;
        // POSSESS_TTL is 150 ms; the wait bound is two of it.
        no_handout_while_ticking(
            &hub,
            &mut peer,
            Duration::from_millis(250),
            "unknown lookup",
        )
        .await;
        assert!(
            !hub.panes_with_queued().iter().any(|p| p == "p"),
            "no repeat is scheduled, so the doorbell's tick sees no waiting mail"
        );
        for id in [&omitted, &expired] {
            assert_eq!(
                attempts_of(&hub, "p", "a", id.as_str()),
                1,
                "no new attempt"
            );
            assert_eq!(state_of(&hub, "p", id.as_str()), StateName::Fetched);
        }
        until("both to be listed as uncertain", || {
            hub.tick();
            hub.summary("p", "a").notices.attention.len() == 2
        })
        .await;
        for item in hub.summary("p", "a").notices.attention {
            assert_eq!(item.kind, AttentionKind::Uncertain);
        }
        no_handout_while_ticking(&hub, &mut peer, Duration::from_millis(200), "uncertain").await;
        for id in [&omitted, &expired] {
            assert_eq!(
                attempts_of(&hub, "p", "a", id.as_str()),
                1,
                "still no repeat"
            );
        }
        peer.close().await;
    }
}

/// Scenario: For each receipt slot (OpenCode's two and Pi's), a receipt stream is handed a message, its watcher publishes the message's `handed` and `committed` receipts, and the stream dies before the commit control reaches the deck. The context reloads, and its new stream answers the resume `unknown`. The deck finds the published commit and settles the message, and the reloaded context is never handed a repeat.
#[spec("inbox/boundary/020")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_020_a_published_commit_settles_after_an_unknown_reload() {
    let (hub, _state) = hub_with_spool(Duration::from_secs(30), Duration::from_secs(60));
    for (pane, component) in [
        ("tui", InboxComponent::OpenCodePlugin),
        ("mini", InboxComponent::OpenCodeMini),
        ("pi", InboxComponent::PiExtension),
    ] {
        let mut first = Peer::open(&hub, pane, "a", component, INTEGRATION).await;
        first.next().await;
        let id = hub
            .post_direct(Recipient::bound(pane, "a"), content("started"))
            .unwrap();
        let handed = first.handout().await;
        for stage in [
            crate::inbox_receipt::RecordStage::Handed,
            crate::inbox_receipt::RecordStage::Committed,
        ] {
            publish_receipt(&hub, pane, "a", &handed[0], stage);
        }
        // The stream dies before its commit control is read.
        first.close().await;
        let mut reloaded = Peer::open(&hub, pane, "a", component, INTEGRATION).await;
        let registered = reloaded.next().await;
        assert_eq!(registered.resume.len(), 1, "{pane}: {registered:?}");
        reloaded
            .send(serde_json::json!({"resume_answers": [{"id": id.as_str(), "a": attempt(&handed[0]).a, "e": attempt(&handed[0]).e, "answer": "unknown"}]}))
            .await;
        // The answer fences the attempt before the clock reads the spool, so
        // only the check gate stands between the fence and a repeat.
        let key = (pane.to_string(), "a".to_string());
        until("the unknown answer to fence the attempt", || {
            hub.lock().receipts.gens[&key]
                .model
                .delivery(id.as_str())
                .and_then(|d| d.attempt(attempt(&handed[0]).a, attempt(&handed[0]).e))
                .is_some_and(|at| at.stage == crate::inbox_attempts::Stage::Fenced)
        })
        .await;
        let started = Instant::now();
        while state_of(&hub, pane, id.as_str()) != StateName::Acknowledged {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "{pane}: the published commit settles it"
            );
            hub.tick();
            if let Ok(reply) =
                tokio::time::timeout(Duration::from_millis(20), reloaded.next()).await
            {
                assert!(
                    reply.messages.is_none(),
                    "{pane}: no repeat for a committed message: {reply:?}"
                );
            }
        }
        no_handout_while_ticking(&hub, &mut reloaded, Duration::from_millis(200), pane).await;
        assert_eq!(attempts_of(&hub, pane, "a", id.as_str()), 1, "{pane}");
        reloaded.close().await;
    }
}

/// Scenario: A task delegated to an OpenCode pane, and again to a Pi pane, is handed to its receipt stream, which reports it handed and then closes for good: nothing registers in its place. The deck repeats nothing it could not hand to anyone, the doorbell's tick sees the pane as having waiting mail, and after several possession TTLs the card lists the task as uncertain. Retry pressed from one client is applied, and with still no receiver the task is listed as uncertain again; Cancel pressed from the other client then ends it, and the offer-free task is not reassigned.
#[spec("inbox/boundary/021")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_021_a_receiver_that_never_returns_reaches_uncertain() {
    for component in [InboxComponent::OpenCodePlugin, InboxComponent::PiExtension] {
        use crate::daemon_protocol::InboxActionOutcome;
        let (hub, _state) = hub_with_spool(Duration::from_millis(50), Duration::from_secs(60));
        let mut peer = Peer::open(&hub, "p", "a", component, INTEGRATION).await;
        peer.next().await;
        let (id, _end) = hub
            .post_delegate(
                Recipient::bound("p", "a"),
                "coder",
                Some(Recipient::bound("o", "oa")),
                DelegateLedger::default(),
                content("gone for good"),
                &|| true,
            )
            .unwrap();
        let handed = peer.handout().await;
        peer.send(serde_json::json!({"handed": [at(&handed[0])]}))
            .await;
        until("the handed report", || {
            hub.lock().receipts.gens[&("p".to_string(), "a".to_string())]
                .model
                .delivery(id.as_str())
                .and_then(|d| d.attempt(1, 1))
                .is_some_and(|at| at.stage == crate::inbox_attempts::Stage::Handed)
        })
        .await;
        peer.close().await;
        let mut waiting = false;
        until("the card to list it as uncertain", || {
            hub.tick();
            waiting |= hub.panes_with_queued().iter().any(|p| p == "p");
            hub.summary("p", "a").notices.attention.len() == 1
        })
        .await;
        assert!(waiting, "the doorbell's tick saw the waiting repeat");
        assert_eq!(attempts_of(&hub, "p", "a", id.as_str()), 1);
        let item = hub.summary("p", "a").notices.attention[0].clone();
        assert_eq!(item.kind, AttentionKind::Uncertain);
        let retry = hub.inbox_action("p", "a", id.as_str(), item.revision, true, "tui-1");
        assert_eq!(retry.outcome, InboxActionOutcome::Applied);
        until("it to be uncertain again, with no receiver", || {
            hub.tick();
            hub.summary("p", "a")
                .notices
                .attention
                .first()
                .is_some_and(|again| again.revision > item.revision)
        })
        .await;
        let again = hub.summary("p", "a").notices.attention[0].clone();
        let cancel = hub.inbox_action("p", "a", id.as_str(), again.revision, false, "desktop-1");
        assert_eq!(cancel.outcome, InboxActionOutcome::Applied);
        until("the cancel to end it", || {
            hub.tick();
            state_of(&hub, "p", id.as_str()) == StateName::Undeliverable
        })
        .await;
        assert!(hub.summary("p", "a").notices.attention.is_empty());
    }
}

/// Scenario: Two tasks are delegated to an OpenCode pane. The first is handed over and the user cancels it: its stream answers the revoke `already_handed`, the orchestrator is told it was cancelled after it may have reached the worker, and when the watcher's committed receipt later turns up in the spool the orchestrator gets the correction. The second is cancelled while still queued in the component, answered `revoked_ack`, and the orchestrator is told it never reached the worker.
#[spec("inbox/boundary/022")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_022_cancel_tells_the_orchestrator_what_it_knows() {
    use crate::daemon_protocol::InboxActionOutcome;
    let (hub, _state) = hub_with_spool(Duration::from_secs(30), Duration::from_secs(60));
    let mut peer = Peer::open(&hub, "p", "a", InboxComponent::OpenCodePlugin, INTEGRATION).await;
    peer.next().await;
    let delegate = |body: &str| {
        hub.post_delegate(
            Recipient::bound("p", "a"),
            "coder",
            Some(Recipient::bound("o", "oa")),
            DelegateLedger::default(),
            content(body),
            &|| true,
        )
        .unwrap()
        .0
    };
    let notices = |hub: &InboxHub| -> Vec<(String, String)> {
        hub.post_owed_notices();
        hub.list_with_bodies(Some("o"))
            .into_iter()
            .map(|m| (m.subject.clone(), m.text.clone().unwrap_or_default()))
            .collect()
    };
    let handed_task = delegate("handed task");
    let handed = peer.handout().await;
    let d = attempt(&handed[0]).d;
    peer.send(serde_json::json!({"handed": [at(&handed[0])], "possess": {"seq": 1, "seen": d, "held": [at(&handed[0])]}}))
        .await;
    let revision = |id: &MessageId| {
        hub.lock().receipts.gens[&("p".to_string(), "a".to_string())]
            .model
            .delivery(id.as_str())
            .unwrap()
            .revision
    };
    let cancel = hub.inbox_action(
        "p",
        "a",
        handed_task.as_str(),
        revision(&handed_task),
        false,
        "tui-1",
    );
    assert_eq!(cancel.outcome, InboxActionOutcome::Applied);
    let revoked = loop {
        let reply = peer.next().await;
        if !reply.revoke.is_empty() {
            break reply.revoke;
        }
    };
    peer.send(serde_json::json!({"already_handed": revoked}))
        .await;
    // The state lock is released before the owed notice is posted, so the
    // notice is waited for, not assumed published with the state.
    until("the cancel to end it and its notice", || {
        state_of(&hub, "p", handed_task.as_str()) == StateName::Undeliverable
            && !notices(&hub).is_empty()
    })
    .await;
    let told = notices(&hub);
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(told[0].0, CANCELLED_SUBJECT);
    assert!(
        told[0].1.contains(&compose_cancelled_notice(
            handed_task.as_str(),
            "coder",
            true
        )),
        "the complete notice: {}",
        told[0].1
    );
    assert!(
        told[0].1.contains("may already have reached the worker")
            && !told[0].1.contains("did not receive")
            && !told[0].1.contains("otherwise re-delegate or reassign"),
        "{}",
        told[0].1
    );
    // The watcher's commit receipt turns up later, with no control.
    publish_receipt(
        &hub,
        "p",
        "a",
        &handed[0],
        crate::inbox_receipt::RecordStage::Committed,
    );
    until("the correction", || {
        hub.tick();
        notices(&hub).len() == 2
    })
    .await;
    let told = notices(&hub);
    assert_eq!(told[1].0, LATE_ACK_SUBJECT);
    assert!(
        told[1].1.contains(&compose_after_all_notice(
            handed_task.as_str(),
            "coder",
            true
        )),
        "the complete correction: {}",
        told[1].1
    );

    let queued_task = delegate("queued task");
    let queued = peer.handout().await;
    let cancel = hub.inbox_action(
        "p",
        "a",
        queued_task.as_str(),
        revision(&queued_task),
        false,
        "desktop-1",
    );
    assert_eq!(cancel.outcome, InboxActionOutcome::Applied);
    loop {
        let reply = peer.next().await;
        if !reply.revoke.is_empty() {
            assert_eq!(reply.revoke[0].id, queued[0].id);
            peer.send(serde_json::json!({"revoked_ack": reply.revoke}))
                .await;
            break;
        }
    }
    until("the second cancel and its notice", || {
        state_of(&hub, "p", queued_task.as_str()) == StateName::Undeliverable
            && notices(&hub).len() >= 3
    })
    .await;
    let told = notices(&hub);
    assert_eq!(told.len(), 3, "{told:?}");
    assert!(
        told[2].1.contains(&compose_cancelled_notice(
            queued_task.as_str(),
            "coder",
            false
        )),
        "{}",
        told[2].1
    );
    assert!(told[2].1.contains("did not receive it"), "{}", told[2].1);
    peer.close().await;
}

/// Scenario: Two OpenCode generations' spools fail the deck's own reads: one generation's receipt directory cannot be listed, the other holds a receipt file that cannot be opened. Each has a message out to its receipt stream. The deck's clock meets the error, and each card says receipts are degraded, naming the error, in the snapshot and in a notice both clients receive.
#[spec("inbox/boundary/023")]
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_023_the_decks_own_spool_errors_degrade_the_generation() {
    use std::os::unix::fs::PermissionsExt;
    let (hub, _state) = hub_with_spool(Duration::from_secs(30), Duration::from_secs(60));
    let (tx, mut notices) = broadcast::channel(256);
    hub.set_broadcast(tx);
    let mut peers = Vec::new();
    for pane in ["listing", "reading"] {
        let mut peer =
            Peer::open(&hub, pane, "a", InboxComponent::OpenCodePlugin, INTEGRATION).await;
        peer.next().await;
        let rec = rec_dir(&hub, pane, "a");
        if pane == "listing" {
            std::fs::set_permissions(&rec, std::fs::Permissions::from_mode(0o300)).unwrap();
        } else {
            let squat = rec.join("m-k3xzq2ab-0f3c9a71d2e4b850.1.1.handed");
            std::fs::write(&squat, b"{}").unwrap();
            std::fs::set_permissions(&squat, std::fs::Permissions::from_mode(0o000)).unwrap();
        }
        hub.post_direct(Recipient::bound(pane, "a"), content(pane))
            .unwrap();
        peer.handout().await;
        peers.push((pane, rec, peer));
    }
    for (pane, _, _) in &peers {
        until("the degraded notice", || {
            hub.tick();
            hub.summary(pane, "a").notices.receipts_degraded.is_some()
        })
        .await;
        let degraded = hub.summary(pane, "a").notices.receipts_degraded.unwrap();
        assert!(degraded.contains("ermission denied"), "{pane}: {degraded}");
    }
    let mut told = std::collections::HashSet::new();
    while let Ok(BroadcastMsg::Inbox(notice)) = notices.try_recv() {
        if notice.attention_update && notice.notices.receipts_degraded.is_some() {
            told.insert(notice.pane_id.clone());
        }
    }
    assert_eq!(
        told.len(),
        2,
        "both clients are told for each card: {told:?}"
    );
    for (_, rec, peer) in peers {
        let _ = std::fs::set_permissions(&rec, std::fs::Permissions::from_mode(0o700));
        for entry in std::fs::read_dir(&rec).unwrap().flatten() {
            let _ = std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(0o600));
        }
        peer.close().await;
    }
}

/// Scenario: Fourteen messages of about five thousand characters each wait for an OpenCode receipt stream, whose hand-over budget is 64 KiB: together they exceed it. The first hand-out carries only as many as fit, and the rest are not issued — no attempt and still queued — until the next hand-out carries them.
#[spec("inbox/boundary/024")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_024_a_receipt_hand_out_stays_within_its_budget() {
    let hub = hub_with(Duration::from_secs(30), Duration::from_secs(60));
    let mut ids = Vec::new();
    for n in 0..14 {
        let body = format!("{n:02}{}", "x".repeat(5_000));
        ids.push(
            hub.post_direct(Recipient::bound("p", "a"), content(&body))
                .unwrap(),
        );
    }
    let mut peer = Peer::open(&hub, "p", "a", InboxComponent::OpenCodePlugin, INTEGRATION).await;
    peer.next().await;
    let budget = InboxComponent::OpenCodePlugin.inline_budget();
    let first = peer.handout().await;
    let used: usize = first
        .iter()
        .map(|d| budget.measure(&d.text) + budget.measure(BLOCK_SEPARATOR))
        .sum();
    assert!(first.len() < ids.len(), "{} of {}", first.len(), ids.len());
    assert!(used <= budget.limit(), "{used} > {}", budget.limit());
    for id in &ids[first.len()..] {
        assert_eq!(attempts_of(&hub, "p", "a", id.as_str()), 0, "not issued");
        assert_eq!(state_of(&hub, "p", id.as_str()), StateName::Queued);
    }
    hub.dispatch_agent("p", "a");
    let rest = peer.handout().await;
    assert_eq!(first.len() + rest.len(), ids.len());
    peer.close().await;
}

/// Offer `body` from an orchestrator pane to `candidates`, in order.
fn offer_to(hub: &InboxHub, body: &str, candidates: &[(Recipient, String)]) -> MessageId {
    hub.offer(
        Scope::Orchestration {
            token: "mixed".into(),
        },
        "jobs",
        None,
        Content {
            from: Sender::Agent {
                pane_id: "orch".into(),
                agent_id: "oa".into(),
                role: Some("orchestrator".into()),
                orchestration: Some("mixed".into()),
            },
            body_trust: BodyTrust::Trusted,
            ..content(body)
        },
        Recipient::bound("orch", "oa"),
        candidates,
        &|| Ok(()),
    )
    .unwrap()
    .id
}

fn live_all() -> Vec<(String, String)> {
    ["orch:oa", "oc:a", "pi:b", "claude:c"]
        .iter()
        .map(|pair| {
            let (pane, agent) = pair.split_once(':').unwrap();
            (pane.to_string(), agent.to_string())
        })
        .collect()
}

/// Scenario: A job is offered first to an OpenCode worker and then to a Claude worker. The OpenCode worker positively did not hand it over, so the job goes back to its pool and on to the Claude worker, whose hook takes it through the legacy fetch. The OpenCode worker's watcher then publishes a commit receipt for its own earlier attempt: the deck tells the offerer the job also reached the earlier holder, and the Claude worker's copy stays its own — not settled by that receipt — until its hook acknowledges it.
#[spec("inbox/boundary/026")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_026_an_offer_moved_from_opencode_to_a_legacy_fetch_worker_is_not_settled_by_a_late_receipt()
 {
    let (hub, _state) = hub_with_spool(Duration::from_secs(30), Duration::from_secs(60));
    let mut oc = Peer::open(&hub, "oc", "a", InboxComponent::OpenCodePlugin, INTEGRATION).await;
    oc.next().await;
    let candidates = [
        (Recipient::bound("oc", "a"), "opencode-worker".to_string()),
        (Recipient::bound("claude", "c"), "claude-worker".to_string()),
    ];
    let id = offer_to(&hub, "mixed job", &candidates);
    let handed = oc.handout().await;
    assert_eq!(handed[0].id, id.as_str());
    oc.send(serde_json::json!({"not_handed": [at(&handed[0])]}))
        .await;
    until("the job to go back to its pool", || {
        hub.lock().inbox.unassigned_offers().contains(&id)
    })
    .await;
    assert_eq!(
        hub.assign_offer(&id, &candidates, &live_all),
        Some(Recipient::bound("claude", "c")),
        "the next holder is the Claude worker"
    );
    let fetched = hub.fetch("claude", "c", Consumer::FetchCli, None, None);
    assert_eq!(fetched.len(), 1);
    assert_eq!(
        hub.lock().inbox.owner_of(id.as_str(), "claude", "c"),
        Some(crate::inbox::AssignmentOwner::Legacy)
    );
    publish_receipt(
        &hub,
        "oc",
        "a",
        &handed[0],
        crate::inbox_receipt::RecordStage::Committed,
    );
    let told = |hub: &InboxHub| {
        hub.post_owed_notices();
        hub.list_with_bodies(Some("orch"))
            .into_iter()
            .any(|m| m.subject == ALSO_REACHED_SUBJECT)
    };
    until("the offerer to hear the job also reached OpenCode", || {
        hub.tick();
        told(&hub)
    })
    .await;
    assert_eq!(
        state_of(&hub, "claude", id.as_str()),
        StateName::Fetched,
        "the Claude copy is not settled by the earlier holder's receipt"
    );
    assert!(matches!(
        hub.ack(id.as_str(), "claude", "c", AckHow::Explicit),
        AckOutcome::Claimed { .. }
    ));
    oc.close().await;
}

/// Scenario: A job is offered first to a Claude worker, whose hook takes it through the legacy fetch and lets its ack-wait run out, and then to an OpenCode worker, whose receipt stream is handed it. The hook's late acknowledgement of its earlier copy settles nothing: the OpenCode worker's copy stays with its open attempt until the OpenCode worker commits it.
#[spec("inbox/boundary/027")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_027_an_offer_moved_from_a_legacy_fetch_worker_to_opencode_is_not_settled_by_a_late_ack()
 {
    let (hub, _state) = hub_with_spool(Duration::from_secs(30), Duration::from_secs(60));
    let mut oc = Peer::open(&hub, "oc", "a", InboxComponent::OpenCodePlugin, INTEGRATION).await;
    oc.next().await;
    let candidates = [
        (Recipient::bound("claude", "c"), "claude-worker".to_string()),
        (Recipient::bound("oc", "a"), "opencode-worker".to_string()),
    ];
    let id = offer_to(&hub, "mixed job", &candidates);
    let fetched = hub.fetch("claude", "c", Consumer::FetchCli, None, None);
    assert_eq!(fetched.len(), 1, "the Claude worker holds it first");
    hub.run_out_ack_wait(id.as_str(), "claude");
    assert!(
        hub.lock().inbox.unassigned_offers().contains(&id),
        "and its ack-wait returns it to the pool"
    );
    assert_eq!(
        hub.assign_offer(&id, &candidates, &live_all),
        Some(Recipient::bound("oc", "a")),
        "the next holder is OpenCode"
    );
    let handed = oc.handout().await;
    assert_eq!(handed[0].id, id.as_str());
    assert!(
        !matches!(
            hub.ack(id.as_str(), "claude", "c", AckHow::Explicit),
            AckOutcome::Claimed { .. } | AckOutcome::Acknowledged
        ),
        "the hook's late ack is not the job's"
    );
    assert_eq!(state_of(&hub, "oc", id.as_str()), StateName::Fetched);
    assert_eq!(attempts_of(&hub, "oc", "a", id.as_str()), 1);
    oc.send(serde_json::json!({"committed": [at(&handed[0])]}))
        .await;
    until("the OpenCode worker's commit to claim it", || {
        state_of(&hub, "oc", id.as_str()) == StateName::Claimed
    })
    .await;
    oc.close().await;
}

/// The current revision of `id` in `pane`/`agent`'s attempt model.
fn revision_of(hub: &InboxHub, pane: &str, agent: &str, id: &str) -> u64 {
    hub.lock().receipts.gens[&(pane.to_string(), agent.to_string())]
        .model
        .delivery(id)
        .unwrap()
        .revision
}

/// Delegate a task to `pane`/`a` from orchestrator pane `o`.
fn delegate_to(hub: &InboxHub, pane: &str, body: &str) -> MessageId {
    hub.post_delegate(
        Recipient::bound(pane, "a"),
        "coder",
        Some(Recipient::bound("o", "oa")),
        DelegateLedger::default(),
        content(body),
        &|| true,
    )
    .unwrap()
    .0
}

/// The subjects and texts posted to orchestrator pane `o` so far.
fn orchestrator_notices(hub: &InboxHub) -> Vec<(String, String)> {
    hub.post_owed_notices();
    hub.list_with_bodies(Some("o"))
        .into_iter()
        .map(|m| (m.subject.clone(), m.text.clone().unwrap_or_default()))
        .collect()
}

/// Whether `id`'s attempt `(a, e)` in `pane`/`a` has had its evidence
/// reservation returned.
fn reclaimed(hub: &InboxHub, pane: &str, id: &str, a: u32, e: u32) -> bool {
    hub.lock().receipts.gens[&(pane.to_string(), "a".to_string())]
        .model
        .delivery(id)
        .and_then(|d| d.attempt(a, e))
        .is_some_and(|at| at.reclaimed)
}

/// Scenario: A task delegated to an OpenCode pane, and again to a Pi pane, is handed over, its watcher publishes the task's committed receipt, and the stream dies before the commit control reaches the deck. A reloaded context registers, and the user cancels the task before the context answers the resume `unknown`. The cancelled attempt keeps its receipt until the deck has read the spool: the orchestrator is told the task was cancelled after it may have reached the worker, and then that it was delivered after all. In a second pane the stream closes for good and the deck retires it, the watcher's late commit receipt lands, and the user cancels: the orchestrator again gets the correction. A third task, whose stream is retired with no receipt, is cancelled, and its evidence is returned once the deck has read the spool.
#[spec("inbox/boundary/028")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_028_cancel_keeps_a_fenced_attempts_receipt_until_it_is_read() {
    for component in [InboxComponent::OpenCodePlugin, InboxComponent::PiExtension] {
        use crate::daemon_protocol::InboxActionOutcome;
        use crate::inbox_receipt::RecordStage;
        let (hub, _state) = hub_with_spool(Duration::from_millis(50), Duration::from_secs(60));
        let mut first = Peer::open(&hub, "p", "a", component, INTEGRATION).await;
        first.next().await;
        let task = delegate_to(&hub, "p", "started then cancelled");
        let handed = first.handout().await;
        for stage in [RecordStage::Handed, RecordStage::Committed] {
            publish_receipt(&hub, "p", "a", &handed[0], stage);
        }
        first.close().await;
        let mut reloaded = Peer::open(&hub, "p", "a", component, INTEGRATION).await;
        assert_eq!(reloaded.next().await.resume.len(), 1);
        let cancel = hub.inbox_action(
            "p",
            "a",
            task.as_str(),
            revision_of(&hub, "p", "a", task.as_str()),
            false,
            "tui-1",
        );
        assert_eq!(cancel.outcome, InboxActionOutcome::Applied);
        let (a, e) = (attempt(&handed[0]).a, attempt(&handed[0]).e);
        reloaded
            .send(serde_json::json!({"resume_answers": [{"id": task.as_str(), "a": a, "e": e, "answer": "unknown"}]}))
            .await;
        until("the cancel to end it", || {
            state_of(&hub, "p", task.as_str()) == StateName::Undeliverable
        })
        .await;
        assert!(
            !reclaimed(&hub, "p", task.as_str(), a, e),
            "the fenced attempt keeps its receipt until the spool is read"
        );
        until("the correction", || {
            hub.tick();
            orchestrator_notices(&hub).len() == 2
        })
        .await;
        let told = orchestrator_notices(&hub);
        assert_eq!(told[0].0, CANCELLED_SUBJECT, "{told:?}");
        assert!(
            told[0].1.contains("may already have reached the worker"),
            "{}",
            told[0].1
        );
        assert_eq!(told[1].0, LATE_ACK_SUBJECT, "{told:?}");
        assert!(
            told[1]
                .1
                .contains(&compose_after_all_notice(task.as_str(), "coder", true)),
            "{}",
            told[1].1
        );
        reloaded.close().await;

        // POSSESS_TTL is 150 ms: a stream closed with no successor is retired
        // 450 ms later, which fences what it held.
        for (pane, published) in [("q", true), ("r", false)] {
            let mut peer = Peer::open(&hub, pane, "a", component, INTEGRATION).await;
            peer.next().await;
            let task = delegate_to(&hub, pane, "retired");
            let handed = peer.handout().await;
            let (a, e) = (attempt(&handed[0]).a, attempt(&handed[0]).e);
            peer.send(serde_json::json!({"handed": [at(&handed[0])]}))
                .await;
            peer.close().await;
            let key = (pane.to_string(), "a".to_string());
            until("the closed stream's retirement", || {
                hub.tick();
                hub.lock().receipts.gens[&key]
                    .model
                    .delivery(task.as_str())
                    .and_then(|d| d.attempt(a, e))
                    .is_some_and(|at| at.stage == crate::inbox_attempts::Stage::Fenced)
            })
            .await;
            if published {
                publish_receipt(&hub, pane, "a", &handed[0], RecordStage::Committed);
            }
            let before = orchestrator_notices(&hub).len();
            let cancel = hub.inbox_action(
                pane,
                "a",
                task.as_str(),
                revision_of(&hub, pane, "a", task.as_str()),
                false,
                "desktop-1",
            );
            assert_eq!(cancel.outcome, InboxActionOutcome::Applied, "{pane}");
            assert_eq!(
                state_of(&hub, pane, task.as_str()),
                StateName::Undeliverable,
                "{pane}"
            );
            assert!(
                !reclaimed(&hub, pane, task.as_str(), a, e),
                "{pane}: kept until the spool is read"
            );
            if published {
                until("the correction after a retirement", || {
                    hub.tick();
                    orchestrator_notices(&hub).len() == before + 2
                })
                .await;
                let told = orchestrator_notices(&hub);
                assert_eq!(told[before + 1].0, LATE_ACK_SUBJECT, "{told:?}");
            } else {
                until("a clean read to return the evidence", || {
                    hub.tick();
                    reclaimed(&hub, pane, task.as_str(), a, e)
                })
                .await;
                assert_eq!(
                    orchestrator_notices(&hub).len(),
                    before + 1,
                    "no correction without a receipt"
                );
            }
        }
    }
}

/// Scenario: For an OpenCode and then a Pi receipt stream, a message's first hand-out lapses unpossessed, the spool shows no receipt for it, and the deck hands out a marked repeat, which the receiver holds. The first hand-out's watcher then publishes its commit receipt late, and before the deck reads the spool again the receiver positively refuses the repeat. No third copy is handed out on the strength of the earlier check: the deck reads the spool, finds the receipt and settles the message. On a second message, a directory squats on the first hand-out's receipt name after the repeat went out, so the lookup cannot tell: refusing the repeat hands out nothing more, and the message is listed as uncertain instead.
#[spec("inbox/boundary/029")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_029_an_earlier_absence_check_never_licenses_a_later_repeat() {
    for component in [InboxComponent::OpenCodePlugin, InboxComponent::PiExtension] {
        for unknown in [false, true] {
            let what = if unknown { "unknown" } else { "published" };
            let (hub, _state) = hub_with_spool(Duration::from_millis(50), Duration::from_secs(60));
            let mut peer = Peer::open(&hub, "p", "a", component, INTEGRATION).await;
            peer.next().await;
            let id = hub
                .post_direct(Recipient::bound("p", "a"), content(what))
                .unwrap();
            let first = peer.handout().await;
            peer.send(
                serde_json::json!({"possess": {"seq": 1, "seen": attempt(&first[0]).d, "held": []}}),
            )
            .await;
            let repeat = peer.handout_ticking(&hub).await;
            assert!(attempt(&repeat[0]).repeat, "{what}: the marked repeat");
            peer.send(serde_json::json!({"possess": {"seq": 2, "seen": attempt(&repeat[0]).d, "held": [at(&repeat[0])]}}))
                .await;
            if unknown {
                let at = attempt(&first[0]);
                std::fs::create_dir(
                    rec_dir(&hub, "p", "a").join(format!("{}.{}.{}.committed", id, at.a, at.e)),
                )
                .unwrap();
            } else {
                publish_receipt(
                    &hub,
                    "p",
                    "a",
                    &first[0],
                    crate::inbox_receipt::RecordStage::Committed,
                );
            }
            peer.send(serde_json::json!({"not_handed": [at(&repeat[0])]}))
                .await;
            if let Ok(reply) = tokio::time::timeout(Duration::from_millis(300), async {
                loop {
                    let reply = peer.next().await;
                    if reply.messages.is_some() {
                        return reply;
                    }
                }
            })
            .await
            {
                panic!("{what}: a third copy was handed out on the earlier check: {reply:?}");
            }
            if unknown {
                no_handout_while_ticking(&hub, &mut peer, Duration::from_millis(200), what).await;
                until("it to be listed as uncertain", || {
                    hub.tick();
                    hub.summary("p", "a").notices.attention.len() == 1
                })
                .await;
                assert_eq!(
                    hub.summary("p", "a").notices.attention[0].kind,
                    AttentionKind::Uncertain
                );
            } else {
                until("the receipt to settle it", || {
                    hub.tick();
                    state_of(&hub, "p", id.as_str()) == StateName::Acknowledged
                })
                .await;
            }
            assert_eq!(attempts_of(&hub, "p", "a", id.as_str()), 2, "{what}");
            peer.close().await;
        }
    }
}

/// The revokes on the lines `peer` receives within `span`, keepalives
/// skipped; empty when none arrives.
async fn revokes_within(peer: &mut Peer, span: Duration) -> Vec<crate::event::InboxAttemptRef> {
    let deadline = tokio::time::Instant::now() + span;
    loop {
        match tokio::time::timeout_at(deadline, peer.next()).await {
            Ok(reply) if !reply.revoke.is_empty() => return reply.revoke,
            Ok(_) => {}
            Err(_) => return Vec::new(),
        }
    }
}

/// Fill the queue of lines waiting for `pane`/`agent`'s stream `rx` while its
/// writer is stopped (a connection that buffers little, nobody reading), so
/// the next line the engine sends it is dropped. Answers how many were queued.
async fn fill_the_queue(hub: &InboxHub, pane: &str, agent: &str, rx: u64) -> usize {
    let tx = hub.lock().receipts.gens[&(pane.to_string(), agent.to_string())].links
        [&crate::inbox_attempts::Rx(rx)]
        .tx
        .clone();
    let mut queued = 0;
    let started = Instant::now();
    loop {
        match tx.try_send(WaiterSignal::Line(Box::new(InboxWaitReply::keepalive()))) {
            Ok(()) => queued += 1,
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) if tx.capacity() == 0 => {
                // Full, and still full a moment later: the writer is stopped.
                tokio::time::sleep(Duration::from_millis(50)).await;
                if tx.capacity() == 0 {
                    return queued;
                }
            }
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {}
            Err(error) => panic!("the stream's queue is gone: {error}"),
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the queue never filled"
        );
    }
}

fn queue_capacity(hub: &InboxHub, pane: &str, agent: &str, rx: u64) -> usize {
    hub.lock().receipts.gens[&(pane.to_string(), agent.to_string())].links
        [&crate::inbox_attempts::Rx(rx)]
        .tx
        .capacity()
}

/// One attempt's stage and whether its evidence was returned.
fn attempt_state(
    hub: &InboxHub,
    pane: &str,
    agent: &str,
    id: &str,
    a: u32,
    e: u32,
) -> (crate::inbox_attempts::Stage, bool) {
    let st = hub.lock();
    let at = st.receipts.gens[&(pane.to_string(), agent.to_string())]
        .model
        .delivery(id)
        .unwrap()
        .attempt(a, e)
        .unwrap()
        .clone();
    (at.stage, at.reclaimed)
}

fn evidence_reserved(hub: &InboxHub, pane: &str, agent: &str) -> usize {
    hub.lock().receipts.gens[&(pane.to_string(), agent.to_string())]
        .model
        .evidence_reserved()
}

/// Hand `id` to `peer`, then let that hand-out lapse unpossessed and take its
/// marked repeat: answers both hand-outs.
async fn first_and_repeat(hub: &InboxHub, peer: &mut Peer) -> (InboxDelivery, InboxDelivery) {
    let first = peer.handout().await.remove(0);
    peer.send(serde_json::json!({"possess": {"seq": 1, "seen": attempt(&first).d, "held": []}}))
        .await;
    let repeat = peer.handout_ticking(hub).await.remove(0);
    assert!(attempt(&repeat).repeat, "the second hand-out is the repeat");
    peer.send(serde_json::json!({"possess": {"seq": 2, "seen": attempt(&repeat).d, "held": [at(&repeat)]}}))
        .await;
    (first, repeat)
}

/// Scenario: On an OpenCode and on a Pi receiver alike, a message's first hand-out lapses and its marked repeat goes to the same OpenCode receiver, whose watcher then disconnects. The repeat's commit receipt settles the message while no stream is connected, so the revoke of the first hand-out has nowhere to go. The watcher reconnects for the same context and answers the first hand-out retained: the revoke is sent to it at once, the context answers that it was already handed (the call was aborted without a commit), and the attempt is final and its evidence returned.
#[spec("inbox/boundary/038")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_038_a_revoke_with_no_link_is_sent_again_on_the_retained_rebind() {
    for component in [InboxComponent::OpenCodePlugin, InboxComponent::PiExtension] {
        let (hub, _state) = hub_with_spool(Duration::from_millis(300), Duration::from_secs(2));
        let mut peer = Peer::open(&hub, "p", "a", component, INTEGRATION).await;
        let old_rx = peer.next().await.registered.unwrap().rx;
        let id = hub
            .post_direct(Recipient::bound("p", "a"), content("revoke me"))
            .unwrap();
        let (first, repeat) = first_and_repeat(&hub, &mut peer).await;
        peer.close().await;
        until("the watcher's link to go", || {
            !hub.lock().receipts.gens[&("p".to_string(), "a".to_string())]
                .links
                .contains_key(&crate::inbox_attempts::Rx(old_rx))
        })
        .await;
        publish_receipt(
            &hub,
            "p",
            "a",
            &repeat,
            crate::inbox_receipt::RecordStage::Committed,
        );
        until("the repeat's receipt to settle it", || {
            hub.tick();
            state_of(&hub, "p", id.as_str()) == StateName::Acknowledged
        })
        .await;
        let (a, e) = (attempt(&first).a, attempt(&first).e);
        assert!(
            !attempt_state(&hub, "p", "a", id.as_str(), a, e)
                .0
                .is_final(),
            "precondition: the first hand-out's revoke had no link and is unanswered"
        );

        let mut peer = Peer::open(
            &hub,
            "p",
            "a",
            component,
            ReceiptArm {
                resumed: true,
                ..INTEGRATION
            },
        )
        .await;
        let registered = peer.next().await;
        assert_eq!(
            registered
                .resume
                .iter()
                .map(|r| (r.id.as_str(), r.a, r.e))
                .collect::<Vec<_>>(),
            vec![(id.as_str(), a, e)]
        );
        peer.send(serde_json::json!({"resume_answers": [{"id": id.as_str(), "a": a, "e": e, "answer": "retained"}]}))
            .await;
        // Well inside the ack-wait, with no clock running: only the rebind can
        // have sent it.
        let revoked = revokes_within(&mut peer, Duration::from_secs(1)).await;
        assert_eq!(
            revoked
                .iter()
                .map(|r| (r.id.as_str(), r.a, r.e))
                .collect::<Vec<_>>(),
            vec![(id.as_str(), a, e)],
            "the retained rebind sends the owed revoke again"
        );
        peer.send(serde_json::json!({"already_handed": [{"id": id.as_str(), "a": a, "e": e}]}))
            .await;
        until(
            "the answer to end the attempt and return its evidence",
            || {
                attempt_state(&hub, "p", "a", id.as_str(), a, e)
                    == (crate::inbox_attempts::Stage::HandedInert, true)
            },
        )
        .await;
        assert_eq!(evidence_reserved(&hub, "p", "a"), 0);
        peer.close().await;
    }
}

/// PR #1735 review (4307236310): a message posted while its receiver's queue
/// of waiting lines is full is not issued — a hand-out line that does not fit
/// would be dropped with its attempt spent — and is handed out once the
/// receiver reads its lines again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_stream_queue_is_issued_nothing() {
    let hub = hub_with(Duration::from_secs(30), Duration::from_secs(60));
    let mut peer = Peer::open_buffered(
        &hub,
        "p",
        "a",
        InboxComponent::PiExtension,
        INTEGRATION,
        256,
    )
    .await;
    let rx = peer.next().await.registered.unwrap().rx;
    assert!(fill_the_queue(&hub, "p", "a", rx).await > 0);
    let id = hub
        .post_direct(Recipient::bound("p", "a"), content("wait for room"))
        .unwrap();
    assert_eq!(
        queue_capacity(&hub, "p", "a", rx),
        0,
        "precondition: the queue was full when the message was posted"
    );
    assert_eq!(
        attempts_of(&hub, "p", "a", id.as_str()),
        0,
        "an attempt was issued into a full queue"
    );
    assert_eq!(state_of(&hub, "p", id.as_str()), StateName::Queued);
    let handed = peer.handout_ticking(&hub).await;
    assert_eq!(
        handed.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
        vec![id.as_str()]
    );
    assert_eq!(attempts_of(&hub, "p", "a", id.as_str()), 1);
    peer.close().await;
}

/// PRD #1666 part 2: a delegation that supersedes a task whose orchestrator
/// was told it is uncertain retires the task's watch and, with it, the
/// generation's record that the orchestrator was told, which no later commit
/// could use any more.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_superseded_uncertain_task_leaves_no_told_marker() {
    let hub = hub_with(Duration::from_secs(30), Duration::from_secs(60));
    let mut peer = Peer::open(&hub, "p", "a", InboxComponent::OpenCodePlugin, INTEGRATION).await;
    peer.next().await;
    let delegate = |body: &str| {
        hub.post_delegate(
            Recipient::bound("p", "a"),
            "coder",
            Some(Recipient::bound("o", "oa")),
            DelegateLedger::default(),
            content(body),
            &|| true,
        )
        .unwrap()
        .0
    };
    let id = delegate("uncertain task");
    let mut seq = 0;
    lapse_until_uncertain(&hub, &mut peer, id.as_str(), &mut seq).await;
    let key = ("p".to_string(), "a".to_string());
    let marked = || {
        hub.lock().receipts.gens[&key]
            .uncertain_told
            .contains(id.as_str())
    };
    until("the orchestrator to be told it is uncertain", || {
        hub.tick();
        marked()
    })
    .await;
    delegate("newer task");
    assert!(
        !hub.lock().delegates.contains_key(id.as_str()),
        "the newer delegation retired the watch"
    );
    assert!(!marked(), "and the marker with it");
    peer.close().await;
}

/// Reviewer R9T-03 (PR #1735 review 4298669219): a task delegated to a Pi
/// receiver is reported not handed on every attempt, so it runs out of
/// attempts: the orchestrator is told it is undeliverable and the worker's
/// card shows the error. The receiver's commit of the last attempt then
/// contradicts those reports. The task is listed acknowledged, the
/// orchestrator is corrected once, and the card's error is cleared.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_commit_after_running_out_of_attempts_corrects_the_sender_and_the_card() {
    let hub = hub_with(Duration::from_secs(30), Duration::from_secs(60));
    let cards: Arc<Mutex<Vec<crate::agent_pty::DeliveryNotice>>> = Arc::default();
    hub.set_card_notice_sink({
        let cards = Arc::clone(&cards);
        Arc::new(move |notice| cards.lock().unwrap().push(notice))
    });
    let mut peer = Peer::open(&hub, "p", "a", InboxComponent::PiExtension, INTEGRATION).await;
    peer.next().await;
    let task = delegate_to(&hub, "p", "refused every time");
    let mut last = None;
    for _ in 0..crate::inbox::DEFAULT_MAX_DELIVER {
        let handed = peer.handout_ticking(&hub).await.remove(0);
        peer.send(serde_json::json!({"not_handed": [at(&handed)]}))
            .await;
        last = Some(handed);
    }
    let last = last.expect("handed out at least once");
    until("the task to run out of attempts", || {
        state_of(&hub, "p", task.as_str()) == StateName::Undeliverable
    })
    .await;
    // Owed notices are posted after the hub lock is released, by whichever
    // call releases it: the stream's own task may be posting them.
    until("the orchestrator to be told", || {
        !orchestrator_notices(&hub).is_empty()
    })
    .await;
    let told = orchestrator_notices(&hub);
    assert_eq!(
        told.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>(),
        vec![UNDELIVERED_SUBJECT],
        "precondition: the orchestrator was told the task is undeliverable"
    );
    until("the card to be told of the error", || {
        cards.lock().unwrap().iter().any(|n| !n.cleared)
    })
    .await;

    peer.send(serde_json::json!({"committed": [at(&last)]}))
        .await;
    until("the commit to settle the task", || {
        state_of(&hub, "p", task.as_str()) == StateName::Acknowledged
    })
    .await;
    until("the correction and the card's clearing", || {
        orchestrator_notices(&hub).len() >= 2
            && cards
                .lock()
                .unwrap()
                .iter()
                .any(|n| n.cleared && n.delivery_id == task.as_str())
    })
    .await;
    let told = orchestrator_notices(&hub);
    assert_eq!(
        told.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>(),
        vec![UNDELIVERED_SUBJECT, LATE_ACK_SUBJECT],
        "the orchestrator is corrected once: {told:?}"
    );
    assert!(
        told[1].1.contains(&compose_late_ack_notice(
            task.as_str(),
            "coder",
            LateAckReport::Undeliverable
        )),
        "{}",
        told[1].1
    );
    let cleared = cards
        .lock()
        .unwrap()
        .iter()
        .filter(|n| n.cleared && n.delivery_id == task.as_str())
        .count();
    assert_eq!(cleared, 1, "the card's error is cleared once");
    peer.close().await;
}

/// PR #1817 review 4347326623: a delegated task whose hand-outs lapse
/// unpossessed becomes uncertain, and its orchestrator is told so. The
/// worker then runs `ack <id>`, which commits the attempt. The orchestrator's
/// correction is posted by the ack itself, not left for the ticker's next
/// tick.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_explicit_ack_of_an_uncertain_task_posts_its_correction_at_once() {
    let hub = hub_with(Duration::from_secs(30), Duration::from_secs(60));
    let mut peer = Peer::open(&hub, "p", "a", InboxComponent::OpenCodePlugin, INTEGRATION).await;
    peer.next().await;
    let task = delegate_to(&hub, "p", "uncertain, then acked");
    let mut seq = 0;
    lapse_until_uncertain(&hub, &mut peer, task.as_str(), &mut seq).await;
    until("the orchestrator to be told it is uncertain", || {
        hub.tick();
        orchestrator_notices(&hub)
            .iter()
            .any(|(subject, _)| subject == UNCERTAIN_SUBJECT)
    })
    .await;

    assert_eq!(
        hub.ack(task.as_str(), "p", "a", AckHow::Explicit),
        AckOutcome::Acknowledged
    );
    // Read without posting what is owed: the ack must have posted it.
    let subjects = hub
        .list_with_bodies(Some("o"))
        .into_iter()
        .map(|m| m.subject.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        subjects,
        vec![UNCERTAIN_SUBJECT, LATE_ACK_SUBJECT],
        "the ack posts the correction itself"
    );
    peer.close().await;
}

/// Read out every line the daemon has written to `peer` so far, keepalives
/// included, until none arrives for a moment.
async fn drain_lines(peer: &mut Peer) {
    while let Ok(Ok(Some(_))) =
        tokio::time::timeout(Duration::from_millis(200), peer.lines.next_line()).await
    {}
}

/// Auditor R9T-A02 / reviewer R9T-02: with room for one more line on a
/// receiver's queue, one pass issues a message and has not yet written its
/// hand-out when a second pass posts, issues and writes another. The first
/// pass's line still fits: the second message waits instead of taking its
/// room, and both reach the receiver on their first attempts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pass_holds_its_room_on_the_queue_until_it_writes() {
    let hub = hub_with(Duration::from_secs(30), Duration::from_secs(60));
    let mut peer = Peer::open_buffered(
        &hub,
        "p",
        "a",
        InboxComponent::PiExtension,
        INTEGRATION,
        256,
    )
    .await;
    let rx = peer.next().await.registered.unwrap().rx;
    let key = ("p".to_string(), "a".to_string());
    let tx = hub.lock().receipts.gens[&key].links[&crate::inbox_attempts::Rx(rx)]
        .tx
        .clone();
    let kept = tx.try_reserve_owned().expect("room for a line");
    assert!(fill_the_queue(&hub, "p", "a", rx).await > 0);
    let first = hub
        .post_direct(Recipient::bound("p", "a"), content("first"))
        .unwrap();
    assert_eq!(
        attempts_of(&hub, "p", "a", first.as_str()),
        0,
        "precondition: posted while the queue was full"
    );
    drop(kept);
    assert_eq!(
        queue_capacity(&hub, "p", "a", rx),
        1,
        "precondition: room for exactly one line"
    );

    let mut unwritten = engine::Outbox::new();
    {
        let mut st = hub.lock();
        engine::dispatch(&mut st, &key, Instant::now(), &mut unwritten);
    }
    assert_eq!(
        attempts_of(&hub, "p", "a", first.as_str()),
        1,
        "precondition: the held pass issued the first message"
    );
    let second = hub
        .post_direct(Recipient::bound("p", "a"), content("second"))
        .unwrap();
    engine::flush(unwritten);

    let mut handed: Vec<(String, u32, bool)> = Vec::new();
    let started = Instant::now();
    while handed.len() < 2 && started.elapsed() < Duration::from_secs(5) {
        hub.tick();
        if let Ok(reply) = tokio::time::timeout(Duration::from_millis(20), peer.next()).await
            && let Some(messages) = reply.messages
        {
            handed.extend(messages.iter().map(|d| {
                let at = attempt(d);
                (d.id.clone(), at.a, at.repeat)
            }));
        }
    }
    assert_eq!(
        handed,
        vec![
            (first.to_string(), 1, false),
            (second.to_string(), 1, false)
        ],
        "an issued hand-out was dropped: every message must reach the receiver on its first \
         attempt"
    );
    peer.close().await;
}

/// Reviewer R9T-02: a doorbell fetch by a receiver whose queue of waiting
/// lines is full hands nothing over — the caps of whatever it issued could
/// not reach the watcher — and the mail stays waiting. Once the receiver
/// reads its lines again, the next fetch hands it over and its caps reach the
/// stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_doorbell_fetch_into_a_full_queue_hands_nothing_over() {
    let hub = hub_with(Duration::from_secs(30), Duration::from_secs(60));
    let mut peer = Peer::open_buffered(
        &hub,
        "p",
        "a",
        InboxComponent::PiExtension,
        ReceiptArm {
            doorbell_only: true,
            ..INTEGRATION
        },
        256,
    )
    .await;
    let rx = peer.next().await.registered.unwrap().rx;
    assert!(fill_the_queue(&hub, "p", "a", rx).await > 0);
    let id = hub
        .post_direct(Recipient::bound("p", "a"), content("ring"))
        .unwrap();
    let fetched = hub.receipt_fetch("p", "a", rx, None, None).unwrap();
    assert!(
        fetched.is_empty(),
        "a fetch handed {} message(s) over while their caps could not reach the stream",
        fetched.len()
    );
    assert_eq!(attempts_of(&hub, "p", "a", id.as_str()), 0);
    assert_eq!(state_of(&hub, "p", id.as_str()), StateName::Queued);

    drain_lines(&mut peer).await;
    let fetched = hub.receipt_fetch("p", "a", rx, None, None).unwrap();
    assert_eq!(
        fetched.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
        vec![id.as_str()]
    );
    let issued = peer.next().await.issued;
    assert_eq!(
        issued.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
        vec![id.as_str()],
        "the caps reached the stream"
    );
    peer.close().await;
}

/// Scenario: On an OpenCode and on a Pi receiver alike, a message is handed to a receiver whose connection stops taking lines, so its queue of waiting lines fills; Cancel's revoke finds the queue full and is dropped, while the stream stays connected. The test reads the queue out: no revoke arrives before one ack-wait has passed, and the clock's next pass after it sends the revoke again on the same stream, which answers it; the message ends cancelled and its evidence is returned.
#[spec("inbox/boundary/039")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_039_a_revoke_dropped_by_a_full_queue_is_sent_again() {
    for component in [InboxComponent::OpenCodePlugin, InboxComponent::PiExtension] {
        let ack_wait = Duration::from_millis(600);
        let hub = hub_with(Duration::from_secs(30), ack_wait);
        let mut peer = Peer::open_buffered(&hub, "p", "a", component, INTEGRATION, 256).await;
        let rx = peer.next().await.registered.unwrap().rx;
        let id = hub
            .post_direct(Recipient::bound("p", "a"), content("cancel me"))
            .unwrap();
        let handed = peer.handout().await.remove(0);
        assert!(fill_the_queue(&hub, "p", "a", rx).await > 0);
        let revision = hub.lock().receipts.gens[&("p".to_string(), "a".to_string())]
            .model
            .delivery(id.as_str())
            .unwrap()
            .revision;
        let cancelled = Instant::now();
        let cancel = hub.inbox_action("p", "a", id.as_str(), revision, false, "desktop");
        assert_eq!(
            cancel.outcome,
            crate::daemon_protocol::InboxActionOutcome::Applied
        );
        assert_eq!(
            queue_capacity(&hub, "p", "a", rx),
            0,
            "precondition: the queue was full when the revoke was sent, so it was dropped"
        );
        hub.tick();
        let early = revokes_within(&mut peer, Duration::from_millis(250)).await;
        assert!(
            early.is_empty() && cancelled.elapsed() < ack_wait,
            "a revoke arrived before the ack-wait: {early:?}"
        );
        tokio::time::sleep(ack_wait.saturating_sub(cancelled.elapsed())).await;
        hub.tick();
        let (a, e) = (attempt(&handed).a, attempt(&handed).e);
        let revoked = revokes_within(&mut peer, Duration::from_secs(2)).await;
        assert_eq!(
            revoked
                .iter()
                .map(|r| (r.id.as_str(), r.a, r.e))
                .collect::<Vec<_>>(),
            vec![(id.as_str(), a, e)],
            "the clock sends the dropped revoke again"
        );
        assert!(
            hub.lock().receipts.gens[&("p".to_string(), "a".to_string())]
                .links
                .contains_key(&crate::inbox_attempts::Rx(rx)),
            "the stream stayed connected throughout"
        );
        peer.send(serde_json::json!({"revoked_ack": [{"id": id.as_str(), "a": a, "e": e}]}))
            .await;
        until("the answer to end the message", || {
            state_of(&hub, "p", id.as_str()) == StateName::Undeliverable
        })
        .await;
        assert_eq!(
            attempt_state(&hub, "p", "a", id.as_str(), a, e),
            (crate::inbox_attempts::Stage::NotHanded, true)
        );
        assert_eq!(evidence_reserved(&hub, "p", "a"), 0);
        peer.close().await;
    }
}

/// Scenario: On an OpenCode and on a Pi receiver alike, with the hub's own clock running and no test driving it, a message's first hand-out lapses and its marked repeat goes to the same OpenCode receiver; the receiver's queue then fills, and the repeat's commit receipt settles the message, so the revoke of the first hand-out is dropped. With only that settled message and its open, revoked first attempt left, the clock still has a deadline, and through it the revoke is sent again; the receiver answers already handed, the attempt's evidence is returned, and no further revoke follows.
#[spec("inbox/boundary/040")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_040_an_owed_revoke_keeps_the_clock_running_for_a_settled_message() {
    for component in [InboxComponent::OpenCodePlugin, InboxComponent::PiExtension] {
        let ack_wait = Duration::from_millis(400);
        let (hub, _state) = hub_with_spool(Duration::from_millis(300), ack_wait);
        let ticker = hub.spawn_ticker();
        let mut peer = Peer::open_buffered(&hub, "p", "a", component, INTEGRATION, 256).await;
        let rx = peer.next().await.registered.unwrap().rx;
        let id = hub
            .post_direct(Recipient::bound("p", "a"), content("settle me"))
            .unwrap();
        let first = peer.handout().await.remove(0);
        peer.send(
            serde_json::json!({"possess": {"seq": 1, "seen": attempt(&first).d, "held": []}}),
        )
        .await;
        let repeat = peer.handout().await.remove(0);
        assert!(
            attempt(&repeat).repeat,
            "the clock repeated the first hand-out"
        );
        // The snapshot holding the repeat rides with a final hand-over check, so
        // the grant that answers it proves the snapshot was applied before the
        // queue stops moving.
        peer.send(serde_json::json!({
            "possess": {"seq": 2, "seen": attempt(&repeat).d, "held": [at(&repeat)]},
            "may": {"q": 1, "id": repeat.id, "a": attempt(&repeat).a, "e": attempt(&repeat).e},
        }))
        .await;
        loop {
            if peer.next().await.grant.is_some() {
                break;
            }
        }
        assert!(fill_the_queue(&hub, "p", "a", rx).await > 0);
        publish_receipt(
            &hub,
            "p",
            "a",
            &repeat,
            crate::inbox_receipt::RecordStage::Committed,
        );
        until("the clock to find the receipt and settle it", || {
            state_of(&hub, "p", id.as_str()) == StateName::Acknowledged
        })
        .await;
        assert_eq!(
            queue_capacity(&hub, "p", "a", rx),
            0,
            "precondition: the queue was full when the revoke was sent, so it was dropped"
        );
        let (a, e) = (attempt(&first).a, attempt(&first).e);
        assert!(
            !attempt_state(&hub, "p", "a", id.as_str(), a, e)
                .0
                .is_final()
        );
        assert!(
            hub.lock().receipts.next_deadline(Instant::now()).is_some(),
            "a settled message with an owed revoke keeps a deadline"
        );
        let revoked = revokes_within(&mut peer, ack_wait * 5).await;
        assert_eq!(
            revoked
                .iter()
                .map(|r| (r.id.as_str(), r.a, r.e))
                .collect::<Vec<_>>(),
            vec![(id.as_str(), a, e)],
            "the clock sends the dropped revoke again"
        );
        peer.send(serde_json::json!({"already_handed": [{"id": id.as_str(), "a": a, "e": e}]}))
            .await;
        until(
            "the answer to end the attempt and return its evidence",
            || {
                attempt_state(&hub, "p", "a", id.as_str(), a, e)
                    == (crate::inbox_attempts::Stage::HandedInert, true)
            },
        )
        .await;
        assert_eq!(evidence_reserved(&hub, "p", "a"), 0);
        assert!(
            revokes_within(&mut peer, ack_wait * 3).await.is_empty(),
            "a revoke after the answer"
        );
        ticker.abort();
        peer.close().await;
    }
}

/// The `(id, a, e)` of each attempt a registration lists.
fn refs_of<'a>(refs: impl IntoIterator<Item = (&'a str, u32, u32)>) -> Vec<(String, u32, u32)> {
    let mut refs: Vec<_> = refs
        .into_iter()
        .map(|(id, a, e)| (id.to_string(), a, e))
        .collect();
    refs.sort();
    refs
}

fn key_of(delivery: &InboxDelivery) -> (String, u32, u32) {
    (
        delivery.id.clone(),
        attempt(delivery).a,
        attempt(delivery).e,
    )
}

/// Scenario: One OpenCode agent's TUI watcher is handed four messages: one is cancelled and answered already handed, one is cancelled and answered not handed, one commits, one stays open, and one is reported not handed outside a Cancel and handed out again; its server context's doorbell fetch binds a fifth, which is cancelled and answered already handed. When each context's watcher is replaced, the server's registration lists only its own already-handed attempt as correctable, and the TUI's lists only its own, with that attempt's commit cap and no hand-out cap, while the open attempts are asked back instead and the not-handed one, which keeps no verifier, gets nothing; the commit then reported for it returns its evidence. Another agent's registration on the same pane, and a registration from a plugin instance other than the one this agent pinned, get nothing.
#[spec("inbox/opencode/053")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opencode_053_registration_hands_back_commit_authority_only_for_its_own_slot() {
    let hub = hub_with(Duration::from_secs(30), Duration::from_secs(60));
    let doorbell_only = ReceiptArm {
        doorbell_only: true,
        ..INTEGRATION
    };
    let resumed = ReceiptArm {
        resumed: true,
        ..INTEGRATION
    };
    let cancel = |id: &str| {
        let revision = hub.lock().receipts.gens[&("p".to_string(), "a".to_string())]
            .model
            .delivery(id)
            .unwrap()
            .revision;
        hub.inbox_action("p", "a", id, revision, false, "desktop")
    };

    let mut server = Peer::open_instance(
        &hub,
        "p",
        "a",
        InboxComponent::OpenCodeMini,
        doorbell_only,
        1,
    )
    .await;
    let server_rx = server.next().await.registered.unwrap().rx;
    let on_server = hub
        .post_direct(Recipient::bound("p", "a"), content("server"))
        .unwrap();
    let fetched = hub
        .receipt_fetch("p", "a", server_rx, Some(on_server.as_str()), None)
        .unwrap();
    let server_attempt = (
        on_server.to_string(),
        attempt(&fetched[0]).a,
        attempt(&fetched[0]).e,
    );
    assert!(!server.next().await.issued.is_empty());
    cancel(on_server.as_str());
    let revoked = revokes_within(&mut server, Duration::from_secs(2)).await;
    server
        .send(serde_json::json!({"already_handed": revoked}))
        .await;

    let mut tui = Peer::open_instance(
        &hub,
        "p",
        "a",
        InboxComponent::OpenCodePlugin,
        INTEGRATION,
        1,
    )
    .await;
    tui.next().await;
    let ids: Vec<MessageId> = ["inert", "not-handed", "committed", "open", "requeued"]
        .into_iter()
        .map(|body| {
            hub.post_direct(Recipient::bound("p", "a"), content(body))
                .unwrap()
        })
        .collect();
    let mut handed = Vec::new();
    while handed.len() < ids.len() {
        handed.extend(tui.handout().await);
    }
    let of = |id: &MessageId| handed.iter().find(|d| d.id == id.as_str()).unwrap().clone();
    let (inert, not_handed, committed, open, requeued) = (
        of(&ids[0]),
        of(&ids[1]),
        of(&ids[2]),
        of(&ids[3]),
        of(&ids[4]),
    );
    let seen = handed.iter().map(|d| attempt(d).d).max().unwrap();
    tui.send(serde_json::json!({"possess": {"seq": 1, "seen": seen, "held": handed.iter().map(at).collect::<Vec<_>>()}}))
        .await;
    tui.send(serde_json::json!({"committed": [at(&committed)]}))
        .await;
    cancel(&inert.id);
    cancel(&not_handed.id);
    let mut answered = 0;
    while answered < 2 {
        for revoked in tui.next().await.revoke {
            answered += 1;
            let answer = if revoked.id == inert.id {
                "already_handed"
            } else {
                "revoked_ack"
            };
            tui.send(serde_json::json!({ answer: [revoked] })).await;
        }
    }
    // Positively not handed, outside a Cancel: final with no verifier, and
    // not reclaimed, since its message goes on — handed out again.
    tui.send(serde_json::json!({"not_handed": [at(&requeued)]}))
        .await;
    let again = tui.handout().await.remove(0);
    assert_eq!(again.id, requeued.id);
    until("every answer to be applied", || {
        state_of(&hub, "p", &inert.id) == StateName::Undeliverable
            && state_of(&hub, "p", &not_handed.id) == StateName::Undeliverable
            && state_of(&hub, "p", &committed.id) == StateName::Acknowledged
            && state_of(&hub, "p", on_server.as_str()) == StateName::Undeliverable
    })
    .await;

    let mut server = Peer::open_instance(
        &hub,
        "p",
        "a",
        InboxComponent::OpenCodeMini,
        doorbell_only,
        1,
    )
    .await;
    let registered = server.next().await;
    assert_eq!(
        refs_of(
            registered
                .correctable
                .iter()
                .map(|c| (c.id.as_str(), c.a, c.e))
        ),
        vec![server_attempt.clone()],
        "the server slot gets its own already-handed attempt back"
    );

    let mut tui =
        Peer::open_instance(&hub, "p", "a", InboxComponent::OpenCodePlugin, resumed, 1).await;
    let registered = tui.next().await;
    assert_eq!(
        refs_of(registered.resume.iter().map(|r| (r.id.as_str(), r.a, r.e))),
        refs_of([&open, &again].map(|d| (d.id.as_str(), attempt(d).a, attempt(d).e))),
        "the open attempts are asked back, not handed authority"
    );
    assert!(
        !attempt_state(
            &hub,
            "p",
            "a",
            &requeued.id,
            attempt(&requeued).a,
            attempt(&requeued).e
        )
        .1,
        "precondition: the not-handed attempt of a message still going on is not reclaimed"
    );
    assert_eq!(
        refs_of(
            registered
                .correctable
                .iter()
                .map(|c| (c.id.as_str(), c.a, c.e))
        ),
        vec![key_of(&inert)],
        "only this slot's final attempt that still verifies"
    );
    let caps = attempt(&inert).caps.clone().unwrap();
    assert_eq!(
        registered.correctable[0].commit.as_deref(),
        Some(caps.commit.as_str())
    );
    let line = serde_json::to_string(&registered).unwrap();
    assert!(
        !line.contains(&caps.handed),
        "the hand-out cap is not handed back: {line}"
    );

    // The commit the replacement publishes with that authority corrects the
    // sender and returns the attempt's evidence.
    tui.send(serde_json::json!({"committed": [at(&inert)]}))
        .await;
    until(
        "the late commit to end the attempt and return its evidence",
        || {
            attempt_state(
                &hub,
                "p",
                "a",
                &inert.id,
                attempt(&inert).a,
                attempt(&inert).e,
            ) == (crate::inbox_attempts::Stage::Committed, true)
        },
    )
    .await;

    let mut other = Peer::open(&hub, "p", "b", InboxComponent::OpenCodePlugin, INTEGRATION).await;
    let registered = other.next().await;
    assert!(registered.registered.is_some());
    assert!(
        registered.correctable.is_empty(),
        "another agent's generation gets nothing"
    );

    let mut stale =
        Peer::open_instance(&hub, "p", "a", InboxComponent::OpenCodePlugin, resumed, 2).await;
    let refused = stale.next().await;
    assert!(
        refused.refused.is_some() && refused.correctable.is_empty(),
        "another plugin instance is refused and gets nothing: {refused:?}"
    );
    for peer in [server, tui, other, stale] {
        peer.close().await;
    }
}

/// PRD #1666 G3: a turn edge the registry accepted, numbered `seq`.
fn turn(seq: u64, edge: crate::agent_pty::TurnEdge) -> crate::agent_pty::TurnSeen {
    crate::agent_pty::TurnSeen { seq, edge }
}

/// Run the receipt clock at `now`, with no spool check to apply.
fn tick_at(hub: &InboxHub, now: Instant) {
    let mut st = hub.lock();
    let mut out = Vec::new();
    engine::tick(&mut st, now, &HashMap::new(), &mut out);
}

/// Give `key`'s generation a Pi receiver (the slot no component registers
/// yet) holding `id`, handed over at `t`.
fn pi_handed(
    hub: &InboxHub,
    key: &AgentKey,
    id: &str,
    t: Instant,
) -> (crate::inbox_attempts::Rx, crate::inbox_attempts::Issued) {
    use crate::inbox_attempts::{Slot, Stage};
    let mut st = hub.lock();
    let generation = engine::generation_mut(&mut st, key, None);
    let rx = generation.model.register(t, Slot::Pi, false).rx;
    generation.model.post(id, false);
    let issued = generation.model.issue(t, id, rx).expect("issue");
    generation
        .model
        .snapshot(t, rx, 1, issued.d, &[(id, issued.a, issued.e)]);
    generation
        .model
        .stage(t, id, issued.a, issued.e, Stage::Handed, Some(rx));
    (rx, issued)
}

fn not_started(hub: &InboxHub, key: &AgentKey, id: &str) -> bool {
    hub.lock().receipts.gens[key]
        .model
        .delivery(id)
        .is_some_and(|d| d.not_started())
}

/// Scenario: A Pi generation reports a turn starting before any receipt
/// receiver exists, a receiver registers, and a message is handed while the
/// turn runs; then the turn ends, a second Idle and an older Started arrive,
/// and on another generation the watcher respawns halfway through the window.
/// Only the real turn end moves the clock and starts the window, the clock
/// wakes for it, and an exited generation's clock never comes back.
#[spec("inbox/attempts/022")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attempts_022_only_a_turn_that_ended_moves_the_turn_clock() {
    use crate::agent_pty::TurnEdge::{Ended, Started};
    use crate::inbox_attempts::{ResumeAnswer, Slot, TurnView};
    let hub = hub_with(Duration::from_secs(3600), Duration::from_secs(7200));
    let w = hub.lock().receipts.limits.not_started_after;
    let key: AgentKey = ("p".into(), "a".into());
    let view = |hub: &InboxHub| hub.lock().receipts.turn_view(&key);
    let t = Instant::now();
    hub.note_turn("p", "a", turn(1, Started), t);
    assert!(
        view(&hub).busy,
        "a turn reported before registration counts"
    );
    assert!(
        !hub.lock().receipts.gens.contains_key(&key),
        "an edge creates no receipt generation"
    );
    let (tx, _lines) = tokio::sync::mpsc::channel(8);
    engine::register(
        &mut hub.lock(),
        &key,
        InboxComponent::OpenCodeMini,
        false,
        false,
        1,
        tx,
        None,
        t,
    )
    .expect("registered");
    assert_eq!(
        view(&hub),
        TurnView {
            busy: true,
            idle_since: None
        },
        "a registration reads the clock without resetting it"
    );
    pi_handed(&hub, &key, "m", t);
    tick_at(&hub, t + 2 * w);
    assert!(!not_started(&hub, &key, "m"), "busy: no attention");
    let end = t + 2 * w;
    hub.note_turn("p", "a", turn(2, Ended), end);
    hub.note_turn("p", "a", turn(3, Ended), end + Duration::from_secs(60));
    assert_eq!(
        view(&hub),
        TurnView {
            busy: false,
            idle_since: Some(end)
        },
        "a second Idle with no turn under way moves nothing"
    );
    hub.note_turn("p", "a", turn(2, Started), end + Duration::from_secs(61));
    assert!(!view(&hub).busy, "an edge older than the last applied");
    tick_at(&hub, end + w - Duration::from_secs(1));
    assert!(!not_started(&hub, &key, "m"));
    tick_at(&hub, end + w);
    assert!(
        not_started(&hub, &key, "m"),
        "one window after the turn ended"
    );
    hub.agent_exited("p", "a");
    hub.note_turn("p", "a", turn(9, Started), end + w);
    assert!(
        !hub.lock().receipts.turns.contains_key(&key),
        "an exited generation's clock does not come back"
    );

    // Another generation, idle from the start: the window runs from the
    // hand-over, and a respawn halfway through moves nothing.
    let hub = hub_with(Duration::from_secs(3600), Duration::from_secs(7200));
    let key: AgentKey = ("q".into(), "b".into());
    let (_, i) = pi_handed(&hub, &key, "n", t);
    assert_eq!(
        hub.lock().receipts.next_deadline(t),
        Some(t + w),
        "the clock wakes for the window"
    );
    let half = t + w / 2;
    {
        let mut st = hub.lock();
        let model = &mut st.receipts.gens.get_mut(&key).unwrap().model;
        let rx = model.register(half, Slot::Pi, true).rx;
        model.resume_answer(half, rx, "n", i.a, i.e, ResumeAnswer::Retained);
        model.snapshot(half, rx, 1, 0, &[("n", i.a, i.e)]);
    }
    tick_at(&hub, t + w);
    assert!(
        not_started(&hub, &key, "n"),
        "neither the respawn nor a keepalive postpones the window"
    );
}

/// The card a client hydrates from `summary` for `a` in `p`.
fn card_of(summary: &InboxSummary) -> crate::inbox_display::CardInbox {
    let mut state = crate::state::AppState::default();
    state.register_pane("p".to_string());
    state.apply_event(crate::event::AgentEvent {
        session_id: "gen-p".into(),
        agent_type: crate::event::AgentType::Pi,
        event_type: crate::event::EventType::SessionStart,
        tool_name: None,
        tool_detail: None,
        cwd: None,
        timestamp: chrono::Utc::now(),
        user_prompt: None,
        metadata: HashMap::new(),
        pane_id: Some("p".into()),
        agent_id: Some("a".into()),
        agent_version: None,
        schema_version: None,
        live_target: None,
        background: None,
        background_revision: None,
    });
    state.apply_inbox_summary("p", "a", summary);
    state.sessions["gen-p"].card_inbox().unwrap_or_default()
}

/// Scenario: A deck message handed to a Pi agent that starts no turn for the
/// window is listed on the card as one that may not have reached the agent,
/// not as an uncertain delivery; a turn starting takes it off the next push,
/// held_long replaces it once the hour is up, and a commit clears it. A card
/// listing one uncertain, one not-started and one held-long message counts
/// each once and leads with the uncertain one.
#[spec("inbox/boundary/045")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_045_a_message_the_agent_may_not_have_received_is_its_own_row() {
    use crate::agent_pty::TurnEdge::{Ended, Started};
    use crate::inbox_display::{
        CardInbox, NOT_STARTED, NOT_STARTED_REASON, UNCERTAIN, UNCERTAIN_REASON,
    };
    let hub = hub_with(Duration::from_secs(3600), Duration::from_secs(7200));
    let w = hub.lock().receipts.limits.not_started_after;
    hub.lock().receipts.limits.held_long_after = 2 * w;
    let key: AgentKey = ("p".into(), "a".into());
    let id = hub
        .post_direct(Recipient::bound("p", "a"), content("first"))
        .unwrap();
    let t = Instant::now();
    let (_, i) = pi_handed(&hub, &key, id.as_str(), t);
    let kinds = |notices: &InboxCardNotices| -> Vec<AttentionKind> {
        notices.attention.iter().map(|item| item.kind).collect()
    };
    let told = |hub: &InboxHub| kinds(&hub.lock().receipts.told[&key]);
    tick_at(&hub, t + w);
    let summary = hub.summary("p", "a");
    assert_eq!(kinds(&summary.notices), vec![AttentionKind::NotStarted]);
    assert_eq!(told(&hub), vec![AttentionKind::NotStarted], "pushed");
    assert!(!AttentionKind::NotStarted.uncertain());
    assert!(AttentionKind::NotStarted.retry_offered());
    let card = card_of(&summary);
    assert_eq!(
        (card.uncertain, card.not_started, card.held_long),
        (0, 1, 0)
    );
    assert_eq!(card.full_text(), Some(format!("✉ {NOT_STARTED}")));
    assert_eq!(card.notice_text().as_deref(), Some(NOT_STARTED_REASON));

    hub.note_turn("p", "a", turn(1, Started), t + w);
    assert_eq!(told(&hub), vec![], "a turn starting withdraws it at once");
    hub.note_turn("p", "a", turn(2, Ended), t + w);
    tick_at(&hub, t + 2 * w);
    assert_eq!(
        kinds(&hub.summary("p", "a").notices),
        vec![AttentionKind::HeldLong],
        "held_long takes the row once the hour is up, one row"
    );
    {
        let mut st = hub.lock();
        let model = &mut st.receipts.gens.get_mut(&key).unwrap().model;
        model.stage(
            t + 2 * w,
            id.as_str(),
            i.a,
            i.e,
            crate::inbox_attempts::Stage::Committed,
            None,
        );
        let mut out = Vec::new();
        engine::apply(&mut st, &key, t + 2 * w, &mut out);
    }
    assert_eq!(told(&hub), vec![], "a commit clears it");

    let item = |n: u64, kind| InboxAttentionItem {
        message_id: format!("m-k3xzq2ab-0f3c9a71d2e4b85{n}"),
        revision: n,
        kind,
        from_label: "the orchestrator".into(),
        subject: "review".into(),
        task_id: None,
    };
    let mut mixed = hub.summary("p", "a");
    mixed.notices.attention = vec![
        item(1, AttentionKind::Uncertain),
        item(2, AttentionKind::NotStarted),
        item(3, AttentionKind::HeldLong),
    ];
    let card = card_of(&mixed);
    assert_eq!(
        CardInbox {
            uncertain: card.uncertain,
            not_started: card.not_started,
            held_long: card.held_long,
            ..CardInbox::default()
        },
        CardInbox {
            uncertain: 1,
            not_started: 1,
            held_long: 1,
            ..CardInbox::default()
        }
    );
    assert_eq!(card.full_text(), Some(format!("✉ {UNCERTAIN}")));
    assert_eq!(card.notice_text().as_deref(), Some(UNCERTAIN_REASON));
}

/// Scenario: A task is delegated to a Pi pane and handed to its receipt stream, which reports the commit; the same is done for an OpenCode pane. The Pi task settles acknowledged as confirmed by the extension, the OpenCode one by the plugin, each with one delivery, and neither leaves its delegate watch running.
#[spec("inbox/boundary/031")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_031_a_pi_receipt_commit_settles_as_extension_confirmed() {
    let (hub, _state) = hub_with_spool(Duration::from_secs(30), Duration::from_secs(60));
    for (pane, component, how) in [
        (
            "pi",
            InboxComponent::PiExtension,
            AckHow::ExtensionConfirmed,
        ),
        (
            "oc",
            InboxComponent::OpenCodePlugin,
            AckHow::PluginConfirmed,
        ),
    ] {
        let mut peer = Peer::open(&hub, pane, "a", component, INTEGRATION).await;
        peer.next().await;
        let id = delegate_to(&hub, pane, "commit me");
        assert!(hub.lock().delegates.contains_key(id.as_str()), "{pane}");
        let handed = peer.handout().await;
        peer.send(serde_json::json!({"committed": [at(&handed[0])]}))
            .await;
        until("the commit to settle", || {
            state_of(&hub, pane, id.as_str()) == StateName::Acknowledged
        })
        .await;
        let summary = hub
            .list(pane)
            .into_iter()
            .find(|m| m.id == id)
            .expect("listed");
        assert_eq!(summary.ack_how, Some(how), "{pane}");
        assert_eq!(summary.deliveries, 1, "{pane}");
        assert!(
            !hub.lock().delegates.contains_key(id.as_str()),
            "{pane}: the delegate watch ended"
        );
        peer.close().await;
    }
}

/// Scenario: A task delegated to a Pi pane is handed over, the user cancels it and the stream answers that the agent may already have it. The stream then closes, and each of two later registrations of Pi's watcher is handed back the authority to publish that task's commit; the third reports it. The orchestrator hears once that the task was cancelled after it may have reached the worker, and once that it was delivered after all, and the attempt's evidence is returned.
#[spec("inbox/boundary/033")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_033_a_cancelled_pi_attempts_commit_corrects_across_respawns() {
    use crate::daemon_protocol::InboxActionOutcome;
    let hub = hub_with(Duration::from_secs(30), Duration::from_secs(60));
    let resumed = ReceiptArm {
        resumed: true,
        ..INTEGRATION
    };
    let mut first = Peer::open(&hub, "p", "a", InboxComponent::PiExtension, INTEGRATION).await;
    first.next().await;
    let task = delegate_to(&hub, "p", "cancelled, then committed");
    let handed = first.handout().await;
    let (a, e) = (attempt(&handed[0]).a, attempt(&handed[0]).e);
    first
        .send(serde_json::json!({"handed": [at(&handed[0])]}))
        .await;
    let cancel = hub.inbox_action(
        "p",
        "a",
        task.as_str(),
        revision_of(&hub, "p", "a", task.as_str()),
        false,
        "tui-1",
    );
    assert_eq!(cancel.outcome, InboxActionOutcome::Applied);
    let revoked = revokes_within(&mut first, Duration::from_secs(2)).await;
    first
        .send(serde_json::json!({"already_handed": revoked}))
        .await;
    until("the cancel to end it", || {
        state_of(&hub, "p", task.as_str()) == StateName::Undeliverable
    })
    .await;
    first.close().await;
    for round in 1..=2 {
        let mut again = Peer::open(&hub, "p", "a", InboxComponent::PiExtension, resumed).await;
        let registered = again.next().await;
        let listed = registered.correctable;
        assert!(
            listed
                .iter()
                .any(|c| c.id == task.as_str() && c.a == a && c.e == e),
            "round {round}: the commit authority is handed back: {listed:?}"
        );
        again.close().await;
    }
    let mut last = Peer::open(&hub, "p", "a", InboxComponent::PiExtension, resumed).await;
    last.next().await;
    last.send(serde_json::json!({"committed": [at(&handed[0])]}))
        .await;
    until("the correction", || {
        hub.tick();
        orchestrator_notices(&hub)
            .iter()
            .any(|(subject, _)| subject == LATE_ACK_SUBJECT)
    })
    .await;
    until("the evidence to be returned", || {
        reclaimed(&hub, "p", task.as_str(), a, e)
    })
    .await;
    let told: Vec<String> = orchestrator_notices(&hub)
        .into_iter()
        .map(|(subject, _)| subject)
        .collect();
    assert_eq!(
        told.iter().filter(|s| *s == CANCELLED_SUBJECT).count(),
        1,
        "{told:?}"
    );
    assert_eq!(
        told.iter().filter(|s| *s == LATE_ACK_SUBJECT).count(),
        1,
        "{told:?}"
    );
    last.close().await;
}

/// Scenario: A job is offered first to an OpenCode worker and then to a Pi worker, both on the receipt stream. The OpenCode worker positively did not hand it over, so it goes back to its pool and on to Pi's stream; the OpenCode worker's watcher then publishes a commit receipt for its own earlier attempt. The offerer hears the job also reached the earlier holder, and Pi's copy stays Pi's until Pi's own commit settles it.
#[spec("inbox/boundary/034")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_034_an_offer_moved_from_opencode_to_pi_on_receipts_is_settled_only_by_pi() {
    let (hub, _state) = hub_with_spool(Duration::from_secs(30), Duration::from_secs(60));
    let mut oc = Peer::open(&hub, "oc", "a", InboxComponent::OpenCodePlugin, INTEGRATION).await;
    oc.next().await;
    let mut pi = Peer::open(&hub, "pi", "b", InboxComponent::PiExtension, INTEGRATION).await;
    pi.next().await;
    let candidates = [
        (Recipient::bound("oc", "a"), "opencode-worker".to_string()),
        (Recipient::bound("pi", "b"), "pi-worker".to_string()),
    ];
    let id = offer_to(&hub, "mixed job", &candidates);
    let handed = oc.handout().await;
    assert_eq!(handed[0].id, id.as_str());
    oc.send(serde_json::json!({"not_handed": [at(&handed[0])]}))
        .await;
    until("the job to go back to its pool", || {
        hub.lock().inbox.unassigned_offers().contains(&id)
    })
    .await;
    assert_eq!(
        hub.assign_offer(&id, &candidates, &live_all),
        Some(Recipient::bound("pi", "b"))
    );
    hub.dispatch_agent("pi", "b");
    let on_pi = pi.handout().await;
    assert_eq!(on_pi[0].id, id.as_str());
    publish_receipt(
        &hub,
        "oc",
        "a",
        &handed[0],
        crate::inbox_receipt::RecordStage::Committed,
    );
    let told = |hub: &InboxHub| {
        hub.post_owed_notices();
        hub.list_with_bodies(Some("orch"))
            .into_iter()
            .any(|m| m.subject == ALSO_REACHED_SUBJECT)
    };
    until("the offerer to hear the job also reached OpenCode", || {
        hub.tick();
        told(&hub)
    })
    .await;
    assert_eq!(
        state_of(&hub, "pi", id.as_str()),
        StateName::Fetched,
        "Pi's copy is not settled by the earlier holder's receipt"
    );
    pi.send(serde_json::json!({"committed": [at(&on_pi[0])]}))
        .await;
    until("Pi's own commit to settle it", || {
        state_of(&hub, "pi", id.as_str()) != StateName::Fetched
    })
    .await;
    oc.close().await;
    pi.close().await;
}

/// Scenario: A job is offered first to a Pi worker on the receipt stream and then to an OpenCode worker. Pi's hand-out lapses with nobody possessing it and every later attempt lapses too, and the job is never handed back to its pool from a lapse; it moves to OpenCode only once Pi positively did not hand it over, and Pi's late report then settles nothing.
#[spec("inbox/boundary/035")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_035_a_lapsed_pi_offer_is_never_returned_from_a_lapse() {
    let (hub, _state) = hub_with_spool(Duration::from_millis(50), Duration::from_secs(60));
    let mut pi = Peer::open(&hub, "pi", "b", InboxComponent::PiExtension, INTEGRATION).await;
    pi.next().await;
    let mut oc = Peer::open(&hub, "oc", "a", InboxComponent::OpenCodePlugin, INTEGRATION).await;
    oc.next().await;
    let candidates = [
        (Recipient::bound("pi", "b"), "pi-worker".to_string()),
        (Recipient::bound("oc", "a"), "opencode-worker".to_string()),
    ];
    let id = offer_to(&hub, "mixed job", &candidates);
    let handed = pi.handout().await;
    pi.send(serde_json::json!({"possess": {"seq": 1, "seen": attempt(&handed[0]).d, "held": []}}))
        .await;
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(600) {
        hub.tick();
        assert!(
            !hub.lock().inbox.unassigned_offers().contains(&id),
            "a lapse never returns the offer"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let latest = hub.lock().receipts.gens[&("pi".to_string(), "b".to_string())]
        .model
        .delivery(id.as_str())
        .map(|d| d.attempts.clone())
        .unwrap_or_default();
    pi.send(serde_json::json!({"not_handed": latest.iter().map(|at| serde_json::json!({"id": id.as_str(), "a": at.a, "e": at.e})).collect::<Vec<_>>()}))
        .await;
    until("the job to go back to its pool", || {
        hub.tick();
        hub.lock().inbox.unassigned_offers().contains(&id)
    })
    .await;
    assert_eq!(
        hub.assign_offer(&id, &candidates, &live_all),
        Some(Recipient::bound("oc", "a"))
    );
    hub.dispatch_agent("oc", "a");
    let on_oc = oc.handout().await;
    assert_eq!(on_oc[0].id, id.as_str());
    pi.send(serde_json::json!({"committed": [at(&handed[0])]}))
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    hub.tick();
    assert_eq!(
        state_of(&hub, "oc", id.as_str()),
        StateName::Fetched,
        "Pi's late report settles nothing of OpenCode's copy"
    );
    oc.close().await;
    pi.close().await;
}

/// Scenario: A message is handed to an OpenCode and then a Pi receipt stream, which holds it past the held_long span; the user presses Retry and a marked second copy is handed. The inbox listing counts two deliveries. A second message on each stream lapses through both automatic repeats and is counted once per copy handed, and a Retry from uncertain counts one more.
#[spec("inbox/boundary/046")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_046_deliveries_counts_every_receipt_hand_out() {
    use crate::daemon_protocol::InboxActionOutcome;
    let deliveries = |hub: &InboxHub, pane: &str, id: &MessageId| -> u32 {
        hub.list(pane)
            .into_iter()
            .find(|m| &m.id == id)
            .expect("listed")
            .deliveries
    };
    for (pane, component) in [
        ("oc", InboxComponent::OpenCodePlugin),
        ("pi", InboxComponent::PiExtension),
    ] {
        let hub = hub_with(Duration::from_secs(30), Duration::from_secs(60));
        hub.lock().receipts.limits.held_long_after = Duration::ZERO;
        let mut peer = Peer::open(&hub, pane, "a", component, INTEGRATION).await;
        peer.next().await;
        let id = hub
            .post_direct(Recipient::bound(pane, "a"), content("held"))
            .unwrap();
        let handed = peer.handout().await;
        assert_eq!(deliveries(&hub, pane, &id), 1, "{pane}: the first copy");
        peer.send(serde_json::json!({"possess": {"seq": 1, "seen": attempt(&handed[0]).d, "held": [at(&handed[0])]}}))
            .await;
        let mut item = None;
        until("held_long attention", || {
            hub.tick();
            item = hub
                .summary(pane, "a")
                .notices
                .attention
                .into_iter()
                .find(|item| item.kind == AttentionKind::HeldLong);
            item.is_some()
        })
        .await;
        let revision = item.unwrap().revision;
        let retry = hub.inbox_action(pane, "a", id.as_str(), revision, true, "tui-1");
        assert_eq!(retry.outcome, InboxActionOutcome::Applied, "{pane}");
        let manual = peer.handout().await;
        assert!(attempt(&manual[0]).repeat, "{pane}: a marked repeat");
        assert_eq!(state_of(&hub, pane, id.as_str()), StateName::Fetched);
        assert_eq!(
            deliveries(&hub, pane, &id),
            2,
            "{pane}: Retry's copy is counted"
        );
        peer.close().await;

        let hub = hub_with(Duration::from_secs(30), Duration::from_secs(60));
        let mut peer = Peer::open(&hub, pane, "a", component, INTEGRATION).await;
        peer.next().await;
        let id = hub
            .post_direct(Recipient::bound(pane, "a"), content("lapsing"))
            .unwrap();
        let mut seq = 0;
        let seen = lapse_until_uncertain(&hub, &mut peer, id.as_str(), &mut seq).await;
        assert_eq!(
            deliveries(&hub, pane, &id),
            seen.len() as u32,
            "{pane}: each automatic repeat is counted"
        );
        let mut item = None;
        until("the card to list it", || {
            hub.tick();
            item = hub.summary(pane, "a").notices.attention.into_iter().next();
            item.is_some()
        })
        .await;
        let retry = hub.inbox_action(
            pane,
            "a",
            id.as_str(),
            item.unwrap().revision,
            true,
            "tui-1",
        );
        assert_eq!(retry.outcome, InboxActionOutcome::Applied, "{pane}");
        peer.handout().await;
        assert_eq!(
            deliveries(&hub, pane, &id),
            seen.len() as u32 + 1,
            "{pane}: and so is the Retry from uncertain"
        );
        peer.close().await;
    }
}

/// Scenario: Each component that takes its mail one batch at a time (Claude's waiter and hooks, Codex and Devin hooks, an unknown component) opens a stream for an agent with a task queued. Every stream is refused `unsupported`, arms nothing and leaves the task queued.
#[spec("inbox/boundary/036")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_036_a_stream_from_a_component_that_cannot_stream_is_refused() {
    use tokio::io::AsyncReadExt;
    let hub = Arc::new(
        InboxHub::new(deck(), InboxLimits::default(), Duration::from_secs(30))
            .with_write_deadline(Duration::from_millis(300)),
    );
    let id = hub
        .post_direct(Recipient::bound("p", "a"), content(&"x".repeat(32_000)))
        .unwrap();
    for component in [
        InboxComponent::ClaudeWaiter,
        InboxComponent::ClaudeHooks,
        InboxComponent::CodexHooks,
        InboxComponent::DevinHooks,
        InboxComponent::Other,
    ] {
        let (client, server) = tokio::io::duplex(1 << 16);
        let (read, write) = tokio::io::split(server);
        let serve = tokio::spawn(serve_waiter(
            Arc::clone(&hub),
            "p".to_string(),
            "a".to_string(),
            InboxWaitMode::Stream,
            component,
            Some(4242),
            INTEGRATION,
            BufReader::new(read),
            write,
            |_: &str| {},
        ));
        let mut answer = String::new();
        let (mut from_daemon, _to_daemon) = tokio::io::split(client);
        tokio::time::timeout(
            Duration::from_secs(5),
            from_daemon.read_to_string(&mut answer),
        )
        .await
        .expect("a refused stream ends")
        .unwrap();
        serve.await.unwrap();
        let reply: InboxWaitReply = serde_json::from_str(answer.trim()).unwrap();
        assert_eq!(
            reply.refused.as_deref(),
            Some("unsupported"),
            "{component:?}"
        );
        assert_eq!(hub.armed("p", "a"), 0, "{component:?} arms nothing");
        assert_eq!(
            state_of(&hub, "p", id.as_str()),
            StateName::Queued,
            "{component:?}"
        );
    }
}

/// Scenario: A Pi receiver is handed a message and reports that it handed
/// nothing over, while its stream takes mail only by doorbell fetch so
/// nothing is pushed to it again yet. The agent then runs `ack` on the
/// message. The ack is refused as unknown and changes nothing: no attempt
/// is committed, the inbox copy and the model both stay queued, and once
/// the stream takes pushed mail again the message is handed out again as
/// usual.
#[spec("inbox/boundary/047")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_047_an_ack_of_a_message_never_handed_over_changes_nothing() {
    use crate::inbox_attempts::{MessageState as Model, Rx, Stage};
    let hub = hub_with(Duration::from_secs(30), Duration::from_secs(60));
    let mut peer = Peer::open(&hub, "p", "a", InboxComponent::PiExtension, INTEGRATION).await;
    let rx = Rx(peer.next().await.registered.unwrap().rx);
    let key = ("p".to_string(), "a".to_string());
    let id = hub
        .post_direct(Recipient::bound("p", "a"), content("never handed"))
        .unwrap();
    let first = peer.handout().await.remove(0);
    // A doorbell-only stream is never pushed mail, so the requeued message
    // waits for the ack instead of going straight out again.
    let doorbell_only = |only: bool| {
        hub.lock()
            .receipts
            .gens
            .get_mut(&key)
            .unwrap()
            .links
            .get_mut(&rx)
            .unwrap()
            .doorbell_only = only;
    };
    doorbell_only(true);
    peer.send(serde_json::json!({"not_handed": [at(&first)]}))
        .await;
    let model = |hub: &InboxHub| {
        hub.lock().receipts.gens[&key]
            .model
            .delivery(id.as_str())
            .map(|d| {
                (
                    d.state,
                    d.attempts.iter().map(|at| at.stage).collect::<Vec<_>>(),
                )
            })
            .unwrap()
    };
    until("the not-handed report to requeue it", || {
        model(&hub) == (Model::Queued, vec![Stage::NotHanded])
    })
    .await;

    assert_eq!(
        hub.ack(id.as_str(), "p", "a", AckHow::Explicit),
        AckOutcome::Unknown
    );
    assert_eq!(
        model(&hub),
        (Model::Queued, vec![Stage::NotHanded]),
        "nothing was committed, so no witness was contradicted"
    );
    assert_eq!(state_of(&hub, "p", id.as_str()), StateName::Queued);

    doorbell_only(false);
    let again = peer.handout_ticking(&hub).await;
    assert_eq!(
        again.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
        vec![id.as_str()]
    );
    assert_eq!(attempts_of(&hub, "p", "a", id.as_str()), 2);
    peer.close().await;
}

/// A task delegated to `p`/`a`, issued at `t` to a Claude once-waiter (the
/// slot no component registers before H3) through the doorbell-fetch path,
/// with its caps file written as H3's serve path will write it. The waiter
/// wrote the block — with `handed`, it also published its `handed` record —
/// and its connection closed before any control reached the deck, so the
/// attempt is fenced, possibly handed. Answers the task, its attempt, and
/// the generation's spool.
fn once_waiter_fenced(
    hub: &InboxHub,
    t: Instant,
    handed: bool,
) -> (
    MessageId,
    (u32, u32),
    Arc<crate::inbox_receipt::GenerationSpool>,
) {
    use crate::inbox_attempts::Slot;
    use crate::inbox_receipt::{RecordKey, RecordStage};
    let key: AgentKey = ("p".into(), "a".into());
    let task = delegate_to(hub, "p", "queued in a busy turn's notification");
    let spool = hub.generation_spool("p", "a").unwrap().unwrap();
    let (tx, _lines) = tokio::sync::mpsc::channel(8);
    let rx = {
        let mut st = hub.lock();
        let generation = engine::generation_mut(&mut st, &key, Some(Ok(Arc::clone(&spool))));
        let rx = generation.model.register(t, Slot::OnceWaiter, false).rx;
        generation.links.insert(
            rx,
            engine::Link {
                conn_id: 1,
                tx,
                component: InboxComponent::ClaudeWaiter,
                doorbell_only: true,
                kind: engine::LinkKind::Once { spent: true },
            },
        );
        rx
    };
    let mut out = engine::Outbox::new();
    let batch = engine::doorbell_fetch(&mut hub.lock(), &key, rx, None, None, t, &mut out).unwrap();
    drop(out);
    let at = attempt(&batch[0]);
    let (a, e) = (at.a, at.e);
    let caps = hub.lock().receipts.gens[&key]
        .evidence
        .store()
        .caps(task.as_str(), a, e)
        .cloned()
        .expect("the deck minted the attempt's caps");
    spool.write_caps(task.as_str(), a, e, &caps).unwrap();
    if handed {
        let key = RecordKey {
            msg: task.as_str().to_string(),
            a,
            e,
            stage: RecordStage::Handed,
        };
        spool.publish(&key, &caps.handed).unwrap();
    }
    {
        let mut st = hub.lock();
        let generation = st.receipts.gens.get_mut(&key).unwrap();
        generation.links.remove(&rx);
        generation.model.connection_closed(t, rx);
        let mut out = engine::Outbox::new();
        engine::apply(&mut st, &key, t, &mut out);
    }
    (task, (a, e), spool)
}

/// What Claude's prompt hook does with a submitted notification naming
/// `(id, A, e)`: an exact caps lookup and, when the caps are there, the
/// `committed` record. Answers whether it found them.
fn prompt_commits(
    spool: &crate::inbox_receipt::GenerationSpool,
    id: &str,
    (a, e): (u32, u32),
) -> bool {
    use crate::inbox_receipt::{CapsLookup, RecordKey, RecordStage};
    match spool.read_caps_checked(id, a, e) {
        CapsLookup::Found(caps) => {
            let key = RecordKey {
                msg: id.to_string(),
                a,
                e,
                stage: RecordStage::Committed,
            };
            spool.publish(&key, &caps.commit).unwrap();
            true
        }
        CapsLookup::Missing => false,
        other => panic!("{id}: {other:?}"),
    }
}

/// Whether the deck still holds the commit authority of `id`'s attempt: its
/// evidence store has the caps and the model has not reclaimed it.
fn authority_held(hub: &InboxHub, id: &str, (a, e): (u32, u32)) -> bool {
    let key: AgentKey = ("p".into(), "a".into());
    let caps = hub.lock().receipts.gens[&key]
        .evidence
        .store()
        .caps(id, a, e)
        .is_some();
    caps && !reclaimed(hub, "p", id, a, e)
}

/// Scenario: A Claude once-waiter writes a delegated task's block while the agent is busy and dies before it reports anything, once with its `handed` record published and once without; the deck fences the attempt and the user cancels the task. While the turn stays busy well past the approval window, every receipt check keeps the attempt's caps; when Claude finally submits the queued notification, the prompt hook finds the caps, commits, and the orchestrator is told the task was delivered after all. When the turn ends shortly after the fence, the caps are kept through that idle grace even past the approval window, and the clock is set to wake when the grace ends; a commit inside it still corrects the sender, while with no commit the next check after it returns the evidence, removes the caps file and a later notification commits nothing.
#[spec("inbox/boundary/056")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_056_a_fenced_waiters_commit_authority_lasts_while_the_turn_clock_holds_it() {
    use crate::agent_pty::TurnEdge::{Ended, Started};
    use crate::daemon_protocol::InboxActionOutcome;
    let wait = Duration::from_secs(60);
    let key: AgentKey = ("p".into(), "a".into());
    let cancel = |hub: &InboxHub, task: &MessageId| {
        let outcome = hub.inbox_action(
            "p",
            "a",
            task.as_str(),
            revision_of(hub, "p", "a", task.as_str()),
            false,
            "tui-1",
        );
        assert_eq!(outcome.outcome, InboxActionOutcome::Applied);
        assert_eq!(state_of(hub, "p", task.as_str()), StateName::Undeliverable);
    };
    let corrected = |hub: &InboxHub, task: &MessageId| {
        orchestrator_notices(hub).iter().any(|(subject, text)| {
            subject == LATE_ACK_SUBJECT
                && text.contains(&compose_after_all_notice(task.as_str(), "coder", true))
        })
    };

    for handed in [false, true] {
        let what = if handed { "handed record" } else { "no record" };
        let (hub, _state) = hub_with_spool(Duration::from_secs(30), wait);
        let t = Instant::now();
        hub.note_turn("p", "a", turn(1, Started), t);
        let (task, attempt, spool) = once_waiter_fenced(&hub, t, handed);
        cancel(&hub, &task);
        for n in 1..=4 {
            hub.tick_receipts_with(|| t + wait * n);
            assert!(
                authority_held(&hub, task.as_str(), attempt),
                "{what}: still held {n} ack-waits into the busy turn"
            );
        }
        assert_eq!(
            hub.lock().receipts.gens[&key]
                .model
                .verify_due_at(t + wait * 4),
            None,
            "{what}: a busy turn sets no instant; its end does"
        );
        assert!(!corrected(&hub, &task), "{what}: precondition");
        assert!(
            prompt_commits(&spool, task.as_str(), attempt),
            "{what}: the prompt hook finds the caps"
        );
        hub.tick_receipts_with(|| t + wait * 5);
        assert!(corrected(&hub, &task), "{what}: delivered after all");
    }

    for commit in [true, false] {
        let what = if commit {
            "committed"
        } else {
            "never submitted"
        };
        let (hub, _state) = hub_with_spool(Duration::from_secs(30), wait);
        let t = Instant::now();
        hub.note_turn("p", "a", turn(1, Started), t);
        let (task, attempt, spool) = once_waiter_fenced(&hub, t, false);
        cancel(&hub, &task);
        let ended = t + wait / 2;
        hub.note_turn("p", "a", turn(2, Ended), ended);
        let grace_ends = ended + wait;
        assert_eq!(
            hub.lock().receipts.next_deadline(t + wait),
            Some(grace_ends),
            "{what}: the clock wakes when the grace ends, not at the approval's end"
        );
        hub.tick_receipts_with(|| t + wait + wait / 4);
        assert!(
            authority_held(&hub, task.as_str(), attempt),
            "{what}: past the approval window, inside the idle grace"
        );
        if commit {
            assert!(prompt_commits(&spool, task.as_str(), attempt));
            hub.tick_receipts_with(|| grace_ends - Duration::from_secs(1));
            assert!(corrected(&hub, &task), "{what}");
        } else {
            hub.tick_receipts_with(|| grace_ends);
            assert!(
                !authority_held(&hub, task.as_str(), attempt),
                "{what}: retired once the turn clock lets go"
            );
            assert!(
                !prompt_commits(&spool, task.as_str(), attempt),
                "{what}: the caps file went with the evidence"
            );
            hub.tick_receipts_with(|| grace_ends + wait);
            assert!(!corrected(&hub, &task), "{what}: no correction");
        }
    }
}

/// Claude's once-waiter's end of one connection to `p`/`a`, declaring
/// `declared` as its one-shot integration.
fn open_once(hub: &Arc<InboxHub>, declared: Option<u32>) -> Peer {
    let (client, server) = tokio::io::duplex(1 << 16);
    let (read, write) = tokio::io::split(server);
    let serve = tokio::spawn(serve_waiter(
        Arc::clone(hub),
        "p".to_string(),
        "a".to_string(),
        InboxWaitMode::Once,
        InboxComponent::ClaudeWaiter,
        Some(4242),
        ReceiptArm {
            hook_integration: declared,
            ..ReceiptArm::default()
        },
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

/// Claude's prompt hook's end of one connection to `p`/`a`: its fetch, for
/// the doorbell that named `named`, declaring `declared`.
fn open_hook(hub: &Arc<InboxHub>, named: Option<&str>, declared: Option<u32>) -> Peer {
    let (client, server) = tokio::io::duplex(1 << 16);
    let (read, write) = tokio::io::split(server);
    let serve = tokio::spawn(serve_hook(
        Arc::clone(hub),
        "p".to_string(),
        "a".to_string(),
        InboxComponent::ClaudeHooks,
        HookFetch {
            named: named.map(str::to_string),
            budget: Some(InboxComponent::ClaudeHooks.inline_budget()),
            hook_integration: declared,
        },
        BufReader::new(read),
        write,
    ));
    let (from, to) = tokio::io::split(client);
    Peer {
        lines: BufReader::new(from).lines(),
        to,
        serve,
    }
}

/// The caps file of `id`'s attempt `(a, e)` in `p`/`a`'s generation spool.
fn caps_file(hub: &InboxHub, id: &str, a: u32, e: u32) -> crate::inbox_receipt::CapsLookup {
    let (p, ag) = spool_names("p", "a");
    crate::inbox_receipt::GenerationSpool::open(&hub.spool_env().unwrap(), &p, &ag)
        .unwrap()
        .read_caps_checked(id, a, e)
}

fn integration_notice(hub: &InboxHub) -> (Option<IntegrationNotice>, Option<String>) {
    let notices = hub.summary("p", "a").notices;
    (notices.integration, notices.integration_command)
}

/// Scenario: A Claude once-waiter that declares no one-shot integration, and one that declares less than the deck's, are each refused as hooks too old before anything is issued: the queued task stays queued with no attempt, and the agent's card names the hooks-too-old notice with the deck's own hooks install command. A Claude prompt hook's fetch is refused the same way. A waiter at the integration registers as the once-waiter slot with its approval window, and is handed the task with its opener naming attempt 1.1, without the attempt's caps, whose caps file the deck has written for the prompt hook; the notice is gone.
#[spec("inbox/boundary/048")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_048_the_hook_integration_floor_holds_before_anything_is_issued() {
    let (hub, _state) = hub_with_spool(Duration::from_secs(30), Duration::from_secs(60));
    let fix = "/opt/deck/dot-agent-deck hooks install --agent claude-code";
    hub.set_hooks_remedy(Some(fix.to_string()));
    let key: AgentKey = ("p".into(), "a".into());
    let id = hub
        .post_direct(Recipient::bound("p", "a"), content("behind the floor"))
        .unwrap();
    let nothing_issued = |hub: &InboxHub| {
        hub.lock()
            .receipts
            .gens
            .get(&key)
            .and_then(|generation| generation.model.delivery(id.as_str()))
            .is_none_or(|delivery| delivery.attempts.is_empty())
    };
    for declared in [None, Some(HOOK_INTEGRATION - 1)] {
        let mut peer = open_once(&hub, declared);
        let reply = peer.next().await;
        assert_eq!(
            reply.refused.as_deref(),
            Some("hooks_too_old"),
            "{declared:?}"
        );
        assert!(reply.messages.is_none());
        peer.close().await;
        assert!(nothing_issued(&hub), "{declared:?}: nothing issued");
        assert_eq!(state_of(&hub, "p", id.as_str()), StateName::Queued);
        assert_eq!(hub.armed("p", "a"), 0);
        assert_eq!(
            integration_notice(&hub),
            (Some(IntegrationNotice::HooksTooOld), Some(fix.to_string())),
            "{declared:?}"
        );
    }
    assert!(
        hub.doorbell_facts("p", "a").blocked,
        "no doorbell for stale hooks"
    );
    let mut hook = open_hook(&hub, Some(id.as_str()), None);
    assert_eq!(
        hook.next().await.refused.as_deref(),
        Some("hooks_too_old"),
        "a stale prompt hook is refused too"
    );
    hook.close().await;
    assert!(nothing_issued(&hub));

    let mut peer = open_once(&hub, Some(HOOK_INTEGRATION));
    let registered = peer.next().await.registered.expect("registered first");
    assert_eq!(registered.slot, "once_waiter");
    assert_eq!(registered.grant_ttl_ms, Some(60_000));
    let batch = peer.handout().await;
    assert_eq!(batch.len(), 1);
    let at = attempt(&batch[0]);
    assert_eq!((at.a, at.e), (1, 1));
    assert!(
        at.caps.is_none(),
        "the caps stay in the deck and the caps file"
    );
    assert!(
        batch[0]
            .text
            .starts_with(&format!("[dot-agent-deck message {id} delivery 1.1 from ")),
        "{}",
        batch[0].text
    );
    assert!(matches!(
        caps_file(&hub, id.as_str(), 1, 1),
        crate::inbox_receipt::CapsLookup::Found(_)
    ));
    assert_eq!(integration_notice(&hub), (None, None));
    peer.close().await;
}

/// A writer that takes the first line it is given whole, then of the line
/// after it only `take` bytes before failing, and whose flush fails once it
/// has taken that line whole when `flush_fails` is set.
struct CutWriter {
    lines: usize,
    take: usize,
    taken: usize,
    flush_fails: bool,
    written: Vec<u8>,
}

impl tokio::io::AsyncWrite for CutWriter {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let this = &mut *self;
        if this.lines == 0 {
            this.written.extend_from_slice(buf);
            if buf.ends_with(b"\n") {
                this.lines = 1;
            }
            return std::task::Poll::Ready(Ok(buf.len()));
        }
        let room = this.take.saturating_sub(this.taken).min(buf.len());
        if room == 0 {
            return std::task::Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
        }
        this.taken += room;
        this.written.extend_from_slice(&buf[..room]);
        std::task::Poll::Ready(Ok(room))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let failed = self.flush_fails && self.lines > 0 && self.taken > 0;
        std::task::Poll::Ready(if failed {
            Err(std::io::ErrorKind::BrokenPipe.into())
        } else {
            Ok(())
        })
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

/// Scenario: The deck writes a once-waiter's batch line to a connection that takes none of it, then to one that takes part of it without its newline, then to one that takes all of it but whose flush fails. In the first two the waiter, which reads only whole lines, cannot have parsed it: the attempt is positively not handed and the task is queued again for the next receiver, not repeated. In the third the line may have reached it: the attempt is final and possibly handed, left to the lapse path, and the task is not queued again.
#[spec("inbox/boundary/051")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_051_a_batch_line_whose_newline_never_went_is_not_handed() {
    use crate::inbox_attempts::{MessageState as Model, Stage};
    let key: AgentKey = ("p".into(), "a".into());
    for (take, flush_fails, handed) in [
        (0, false, false),
        (40, false, false),
        (usize::MAX, true, true),
    ] {
        let what = format!("take {take}, flush fails {flush_fails}");
        let (hub, _state) = hub_with_spool(Duration::from_secs(30), Duration::from_secs(60));
        let id = hub
            .post_direct(Recipient::bound("p", "a"), content("a once batch"))
            .unwrap();
        let (_client, server) = tokio::io::duplex(1 << 16);
        let (read, _write) = tokio::io::split(server);
        let writer = CutWriter {
            lines: 0,
            take,
            taken: 0,
            flush_fails,
            written: Vec::new(),
        };
        tokio::time::timeout(
            Duration::from_secs(5),
            serve_waiter(
                Arc::clone(&hub),
                "p".to_string(),
                "a".to_string(),
                InboxWaitMode::Once,
                InboxComponent::ClaudeWaiter,
                Some(4242),
                ReceiptArm {
                    hook_integration: Some(HOOK_INTEGRATION),
                    ..ReceiptArm::default()
                },
                BufReader::new(read),
                writer,
                |_: &str| {},
            ),
        )
        .await
        .expect("a failed hand-over ends the waiter");
        let (state, stages) = hub.lock().receipts.gens[&key]
            .model
            .delivery(id.as_str())
            .map(|d| {
                (
                    d.state,
                    d.attempts.iter().map(|at| at.stage).collect::<Vec<_>>(),
                )
            })
            .unwrap();
        if handed {
            assert_eq!(stages, vec![Stage::Fenced], "{what}: possibly handed");
            assert_eq!(state, Model::Attempting, "{what}");
            assert_ne!(
                state_of(&hub, "p", id.as_str()),
                StateName::Queued,
                "{what}"
            );
        } else {
            assert_eq!(
                stages,
                vec![Stage::NotHanded],
                "{what}: positively not handed"
            );
            assert_eq!(state, Model::Queued, "{what}");
            assert_eq!(
                state_of(&hub, "p", id.as_str()),
                StateName::Queued,
                "{what}"
            );
            let mut next = open_once(&hub, Some(HOOK_INTEGRATION));
            assert!(next.next().await.registered.is_some());
            let again = next.handout().await;
            assert!(
                !again[0].text.contains("This may repeat"),
                "{what}: not a repeat: {}",
                again[0].text
            );
            next.close().await;
        }
    }
}

/// Scenario: Two Claude once-waiters register one after the other: the older is told it was superseded and its connection ends with nothing handed to it. The newer is handed the first task and is then spent: a second task posted after it is handed nothing on that connection, and the next waiter to register is handed it.
#[spec("inbox/boundary/052")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_052_a_newer_waiter_supersedes_and_a_spent_one_takes_nothing() {
    let (hub, _state) = hub_with_spool(Duration::from_secs(30), Duration::from_secs(60));
    let mut older = open_once(&hub, Some(HOOK_INTEGRATION));
    assert!(older.next().await.registered.is_some());
    let mut newer = open_once(&hub, Some(HOOK_INTEGRATION));
    assert!(newer.next().await.registered.is_some());
    assert!(
        older.next().await.superseded,
        "the older waiter is superseded"
    );
    let ended = tokio::time::timeout(Duration::from_secs(5), older.lines.next_line())
        .await
        .expect("its connection ends")
        .unwrap();
    assert!(ended.is_none(), "nothing after the supersession: {ended:?}");
    assert_eq!(hub.armed("p", "a"), 1);
    let first = hub
        .post_direct(Recipient::bound("p", "a"), content("first"))
        .unwrap();
    assert_eq!(newer.handout().await[0].id, first.as_str());
    assert_eq!(hub.armed("p", "a"), 0, "a spent waiter is armed no longer");
    let second = hub
        .post_direct(Recipient::bound("p", "a"), content("second"))
        .unwrap();
    for _ in 0..10 {
        hub.tick();
        if let Ok(reply) = tokio::time::timeout(Duration::from_millis(30), newer.next()).await {
            assert!(
                reply.messages.is_none(),
                "a spent waiter takes nothing: {reply:?}"
            );
        }
    }
    let mut next = open_once(&hub, Some(HOOK_INTEGRATION));
    assert!(next.next().await.registered.is_some());
    assert_eq!(next.handout().await[0].id, second.as_str());
    next.close().await;
    newer.close().await;
}

/// Scenario: A once-waiter's batch is held while its caps file is written. Meanwhile the deck still answers a listing and takes a post on another thread. Once the write goes on, the waiter is handed the batch. With the spool's caps directory made unwritable, a batch whose caps file cannot be written is never sent: its attempt is positively not handed, the task is queued again, the agent's card says receipts are degraded, and the waiter's connection is closed with nothing on it. A Claude prompt hook that takes a task and then reports nothing is closed by the deck's own clock when its hold runs out, and its attempt is final, possibly handed.
#[cfg(unix)]
#[spec("inbox/boundary/055")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn boundary_055_caps_files_are_written_off_the_lock_and_holds_end_on_the_clock() {
    use crate::inbox_attempts::Stage;
    use std::os::unix::fs::PermissionsExt;
    let key: AgentKey = ("p".into(), "a".into());
    let (hub, _state) = hub_with_spool(Duration::from_secs(30), Duration::from_secs(60));
    let mut peer = open_once(&hub, Some(HOOK_INTEGRATION));
    assert!(peer.next().await.registered.is_some());
    let (entered_tx, entered) = std::sync::mpsc::channel();
    let (go, go_rx) = std::sync::mpsc::channel();
    *hub.hold_caps.lock().unwrap() = Some((entered_tx, go_rx));
    let posting = {
        let hub = Arc::clone(&hub);
        std::thread::spawn(move || {
            hub.post_direct(Recipient::bound("p", "a"), content("held"))
                .unwrap()
        })
    };
    entered
        .recv_timeout(Duration::from_secs(5))
        .expect("the caps write started");
    let listed = {
        let hub = Arc::clone(&hub);
        std::thread::spawn(move || {
            let listed = hub.list("p").len();
            hub.post_direct(Recipient::bound("p", "a"), content("meanwhile"))
                .unwrap();
            listed
        })
    };
    let started = Instant::now();
    while !listed.is_finished() {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the deck answers while a caps file is written"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(listed.join().unwrap() >= 1);
    go.send(()).unwrap();
    let held = posting.join().unwrap();
    *hub.hold_caps.lock().unwrap() = None;
    assert_eq!(peer.handout().await[0].id, held.as_str());
    peer.close().await;

    let caps_dir = hub
        .spool_env()
        .unwrap()
        .join(spool_names("p", "a").0)
        .join(spool_names("p", "a").1)
        .join("caps");
    std::fs::set_permissions(&caps_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let mut denied = open_once(&hub, Some(HOOK_INTEGRATION));
    assert!(denied.next().await.registered.is_some());
    let stages = |hub: &InboxHub, id: &str| {
        hub.lock().receipts.gens[&key]
            .model
            .delivery(id)
            .map(|d| d.attempts.iter().map(|at| at.stage).collect::<Vec<_>>())
            .unwrap_or_default()
    };
    let ended = tokio::time::timeout(Duration::from_secs(5), denied.lines.next_line())
        .await
        .expect("the connection ends")
        .unwrap();
    std::fs::set_permissions(&caps_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(ended.is_none(), "nothing is written for it: {ended:?}");
    let meanwhile = hub
        .list("p")
        .into_iter()
        .find(|m| m.subject == "about meanwhile")
        .unwrap()
        .id;
    assert_eq!(stages(&hub, meanwhile.as_str()), vec![Stage::NotHanded]);
    assert_eq!(state_of(&hub, "p", meanwhile.as_str()), StateName::Queued);
    assert!(
        hub.summary("p", "a")
            .notices
            .receipts_degraded
            .is_some_and(|error| error.contains("caps file")),
        "{:?}",
        hub.summary("p", "a").notices
    );
    let _ = denied.serve.await;

    let (hub, _state) = hub_with_spool(Duration::from_secs(30), Duration::from_millis(300));
    let ticker = hub.spawn_ticker();
    let id = hub
        .post_direct(Recipient::bound("p", "a"), content("held by a hook"))
        .unwrap();
    let mut hook = open_hook(&hub, Some(id.as_str()), Some(HOOK_INTEGRATION));
    let first = hook.next().await;
    assert_eq!(first.messages.map(|m| m.len()), Some(1));
    let ended = tokio::time::timeout(Duration::from_secs(5), hook.lines.next_line())
        .await
        .expect("the deck closes a hook whose hold ran out")
        .unwrap();
    assert!(ended.is_none(), "{ended:?}");
    assert_eq!(stages(&hub, id.as_str()), vec![Stage::Fenced]);
    ticker.abort();
}

/// Scenario: A Claude prompt hook fetches the task its doorbell named as a receiver of its own and is handed it with the caps it publishes with, its opener naming attempt 1.1; a task posted while it holds the connection is not pushed to it. A second hook registered beside it takes nothing the first holds and retires nothing. The first reports the block written: the deck commits it and the task settles; it reports nothing for the second task, which stays queued.
#[spec("inbox/boundary/057")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_057_a_hook_receiver_takes_only_what_its_fetch_asked_for() {
    let (hub, _state) = hub_with_spool(Duration::from_secs(30), Duration::from_secs(60));
    let id = hub
        .post_direct(Recipient::bound("p", "a"), content("rung"))
        .unwrap();
    let mut hook = open_hook(&hub, Some(id.as_str()), Some(HOOK_INTEGRATION));
    let reply = hook.next().await;
    assert_eq!(
        reply.registered.as_ref().map(|r| r.slot.as_str()),
        Some("hook")
    );
    let batch = reply.messages.unwrap();
    assert_eq!(batch.len(), 1);
    assert!(
        attempt(&batch[0]).caps.is_some(),
        "a hook publishes with its caps"
    );
    assert!(batch[0].text.contains(" delivery 1.1 from "));
    let later = hub
        .post_direct(Recipient::bound("p", "a"), content("later"))
        .unwrap();
    let mut other = open_hook(&hub, Some(id.as_str()), Some(HOOK_INTEGRATION));
    assert_eq!(
        other.next().await.messages.map(|m| m.len()),
        Some(0),
        "the named message is held by the first hook"
    );
    other.close().await;
    for _ in 0..5 {
        hub.tick();
        if let Ok(reply) = tokio::time::timeout(Duration::from_millis(30), hook.next()).await {
            assert!(
                reply.messages.is_none(),
                "nothing is pushed to a hook: {reply:?}"
            );
        }
    }
    publish_receipt(
        &hub,
        "p",
        "a",
        &batch[0],
        crate::inbox_receipt::RecordStage::Committed,
    );
    hook.send(serde_json::json!({"committed": [at(&batch[0])]}))
        .await;
    hook.close().await;
    until("the hook's commit to settle the task", || {
        state_of(&hub, "p", id.as_str()) == StateName::Acknowledged
    })
    .await;
    assert_eq!(state_of(&hub, "p", later.as_str()), StateName::Queued);
}

/// Hold every caps-file write until the test lets it go: the returned
/// receiver hears each write start, and each `()` sent on the sender lets
/// one go on.
fn hold_caps(hub: &InboxHub) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
    let (entered_tx, entered) = std::sync::mpsc::channel();
    let (go, go_rx) = std::sync::mpsc::channel();
    *hub.hold_caps.lock().unwrap() = Some((entered_tx, go_rx));
    (entered, go)
}

/// The id of `p`'s message about `body`, once it is listed.
fn listed_id(hub: &InboxHub, body: &str) -> String {
    let subject = format!("about {body}");
    hub.list("p")
        .into_iter()
        .find(|m| m.subject == subject)
        .expect("the message is listed")
        .id
        .as_str()
        .to_string()
}

/// Scenario: A Claude once-waiter is issued a task, and while the deck is still writing the batch's caps file a newer waiter registers and supersedes it, and a Claude prompt hook's fetch for that task is handed nothing. When the write finishes, the batch, which never left the deck, is not sent: its attempt is positively not handed (not left possibly handed), the task goes back to the queue, and the newer waiter is handed it at once, not marked as a possible repeat.
#[spec("inbox/boundary/058")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn boundary_058_a_batch_superseded_before_its_release_is_not_handed() {
    use crate::inbox_attempts::Stage;
    let (hub, _state) = hub_with_spool(Duration::from_secs(30), Duration::from_secs(60));
    let mut older = open_once(&hub, Some(HOOK_INTEGRATION));
    assert!(older.next().await.registered.is_some());
    let (entered, go) = hold_caps(&hub);
    let posting = {
        let hub = Arc::clone(&hub);
        std::thread::spawn(move || {
            hub.post_direct(Recipient::bound("p", "a"), content("superseded meanwhile"))
                .unwrap()
        })
    };
    entered
        .recv_timeout(Duration::from_secs(5))
        .expect("the caps write started");
    let id = listed_id(&hub, "superseded meanwhile");
    let mut newer = open_once(&hub, Some(HOOK_INTEGRATION));
    assert!(newer.next().await.registered.is_some());
    assert!(
        older.next().await.superseded,
        "the older waiter is superseded"
    );
    assert_eq!(
        attempt_state(&hub, "p", "a", &id, 1, 1).0,
        Stage::Fenced,
        "the supersession fenced the attempt the deck still holds"
    );
    let mut hook = open_hook(&hub, Some(&id), Some(HOOK_INTEGRATION));
    assert_eq!(
        hook.next().await.messages.map(|m| m.len()),
        Some(0),
        "a hook's fetch takes nothing the deck still holds"
    );
    hook.close().await;
    // One write for the held batch, one for the newer waiter's.
    go.send(()).unwrap();
    go.send(()).unwrap();
    assert_eq!(posting.join().unwrap().as_str(), id);
    *hub.hold_caps.lock().unwrap() = None;
    let batch = newer.handout().await;
    assert_eq!(batch[0].id, id);
    assert_eq!((attempt(&batch[0]).a, attempt(&batch[0]).e), (1, 2));
    assert!(
        !batch[0].text.contains("This may repeat"),
        "the batch never left the deck, so this is no repeat: {}",
        batch[0].text
    );
    assert_eq!(
        attempt_state(&hub, "p", "a", &id, 1, 1).0,
        Stage::NotHanded,
        "the deck's own witness, with its receiver gone"
    );
    newer.close().await;
    older.close().await;
}

/// The gate a [`GatedWriter`] waits at.
#[derive(Default)]
struct Gate {
    blocked: bool,
    open: bool,
    waker: Option<std::task::Waker>,
}

/// A writer that takes its first line whole, then waits at its gate before
/// the next and, once the gate opens, takes not one byte of it.
struct GatedWriter {
    lines: usize,
    gate: Arc<std::sync::Mutex<Gate>>,
}

impl tokio::io::AsyncWrite for GatedWriter {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if self.lines == 0 {
            if buf.ends_with(b"\n") {
                self.lines = 1;
            }
            return std::task::Poll::Ready(Ok(buf.len()));
        }
        let mut gate = self.gate.lock().unwrap();
        if gate.open {
            return std::task::Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
        }
        gate.blocked = true;
        gate.waker = Some(cx.waker().clone());
        std::task::Poll::Pending
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

/// Scenario: A Claude once-waiter is released its batch, and the deck's write of the batch line stalls before a byte of it is taken. A newer waiter registers meanwhile and supersedes the first, and then the stalled write fails having taken nothing. The deck records the attempt positively not handed even though its waiter is gone, the task goes back to the queue, and the newer waiter is handed it unmarked.
#[spec("inbox/boundary/059")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn boundary_059_a_counted_write_that_took_nothing_is_not_handed_after_supersession() {
    use crate::inbox_attempts::Stage;
    let (hub, _state) = hub_with_spool(Duration::from_secs(30), Duration::from_secs(60));
    let gate = Arc::new(std::sync::Mutex::new(Gate::default()));
    let (_client, server) = tokio::io::duplex(1 << 16);
    let (read, _write) = tokio::io::split(server);
    let older = tokio::spawn(serve_waiter(
        Arc::clone(&hub),
        "p".to_string(),
        "a".to_string(),
        InboxWaitMode::Once,
        InboxComponent::ClaudeWaiter,
        Some(4242),
        ReceiptArm {
            hook_integration: Some(HOOK_INTEGRATION),
            ..ReceiptArm::default()
        },
        BufReader::new(read),
        GatedWriter {
            lines: 0,
            gate: Arc::clone(&gate),
        },
        |_: &str| {},
    ));
    until("the older waiter to arm", || hub.armed("p", "a") == 1).await;
    let id = hub
        .post_direct(Recipient::bound("p", "a"), content("stalled write"))
        .unwrap();
    until("the batch line's write to stall", || {
        gate.lock().unwrap().blocked
    })
    .await;
    let mut newer = open_once(&hub, Some(HOOK_INTEGRATION));
    assert!(newer.next().await.registered.is_some());
    assert_eq!(
        attempt_state(&hub, "p", "a", id.as_str(), 1, 1).0,
        Stage::Fenced,
        "the supersession fenced the attempt mid-write"
    );
    {
        let mut gate = gate.lock().unwrap();
        gate.open = true;
        if let Some(waker) = gate.waker.take() {
            waker.wake();
        }
    }
    tokio::time::timeout(Duration::from_secs(5), older)
        .await
        .expect("the failed write ends the older waiter")
        .unwrap();
    let batch = newer.handout().await;
    assert_eq!(batch[0].id, id.as_str());
    assert_eq!((attempt(&batch[0]).a, attempt(&batch[0]).e), (1, 2));
    assert!(
        !batch[0].text.contains("This may repeat"),
        "{}",
        batch[0].text
    );
    assert_eq!(
        attempt_state(&hub, "p", "a", id.as_str(), 1, 1).0,
        Stage::NotHanded
    );
    newer.close().await;
}

/// Scenario: A Claude once-waiter is issued a task, and while the deck writes the batch's caps file the waiter is superseded and the task cancelled. The clock then runs for a while inside the batch's staging deadline: the cancelled task stays held, its attempt keeps its evidence, and nothing is removed under the write. When the write finishes, the attempt is not handed, the task ends cancelled, and its caps file is removed with its evidence. A second batch whose generation exits during its write has its caps file removed too.
#[cfg(unix)]
#[spec("inbox/boundary/060")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn boundary_060_a_late_caps_write_leaves_no_file_behind() {
    use crate::inbox_attempts::{MessageState as Model, Stage};
    use crate::inbox_receipt::CapsLookup;
    let key: AgentKey = ("p".into(), "a".into());
    // The staging deadline is one ack-wait from staging (R2F-R01,
    // inbox/boundary/062 runs past it); this test stays inside it.
    let (hub, _state) = hub_with_spool(Duration::from_secs(30), Duration::from_secs(5));
    let mut older = open_once(&hub, Some(HOOK_INTEGRATION));
    assert!(older.next().await.registered.is_some());
    let (entered, go) = hold_caps(&hub);
    let posting = {
        let hub = Arc::clone(&hub);
        std::thread::spawn(move || {
            hub.post_direct(Recipient::bound("p", "a"), content("cancelled meanwhile"))
                .unwrap()
        })
    };
    entered
        .recv_timeout(Duration::from_secs(5))
        .expect("the caps write started");
    let id = listed_id(&hub, "cancelled meanwhile");
    let mut newer = open_once(&hub, Some(HOOK_INTEGRATION));
    assert!(newer.next().await.registered.is_some());
    let revision = hub.lock().receipts.gens[&key]
        .model
        .delivery(&id)
        .unwrap()
        .revision;
    let cancel = hub.inbox_action("p", "a", &id, revision, false, "desktop-1");
    assert_eq!(
        cancel.outcome,
        crate::daemon_protocol::InboxActionOutcome::Applied
    );
    for _ in 0..6 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        hub.tick();
    }
    assert_eq!(
        attempt_state(&hub, "p", "a", &id, 1, 1),
        (Stage::Fenced, false),
        "nothing reclaims an attempt whose caps file is still being written"
    );
    assert_eq!(
        hub.lock().receipts.gens[&key].model.state(&id),
        Some(Model::CancelHeld)
    );
    go.send(()).unwrap();
    assert_eq!(posting.join().unwrap().as_str(), id);
    until("the task to end cancelled", || {
        state_of(&hub, "p", &id) == StateName::Undeliverable
    })
    .await;
    assert_eq!(
        attempt_state(&hub, "p", "a", &id, 1, 1),
        (Stage::NotHanded, true)
    );
    assert!(
        matches!(caps_file(&hub, &id, 1, 1), CapsLookup::Missing),
        "the late caps file was removed with the attempt"
    );
    assert_eq!(evidence_reserved(&hub, "p", "a"), 0);

    let posting = {
        let hub = Arc::clone(&hub);
        std::thread::spawn(move || {
            hub.post_direct(Recipient::bound("p", "a"), content("exited meanwhile"))
                .unwrap()
        })
    };
    entered
        .recv_timeout(Duration::from_secs(5))
        .expect("the second caps write started");
    let second = listed_id(&hub, "exited meanwhile");
    {
        // The generation exits as `engine::exit` makes it; the spawned
        // removal of its whole directory is left out, so only the release
        // can remove the file written after it.
        let mut st = hub.lock();
        let _ = engine::exit(&mut st, &key, Instant::now());
    }
    go.send(()).unwrap();
    posting.join().unwrap();
    *hub.hold_caps.lock().unwrap() = None;
    assert!(
        matches!(caps_file(&hub, &second, 1, 1), CapsLookup::Missing),
        "a caps file written for an exited generation is removed"
    );
    newer.close().await;
    older.close().await;
}

/// Scenario: Claude's prompt-hook commit is sent straight to the deck, naming exact attempts, as any process in the agent's tree could send it. An attempt whose caps file could not be written, so the deck never sent it, is not committed even with a commit receipt on its cap in the spool, and the task stays queued. An attempt the deck is still holding back is not committed either, receipt or not, and neither is it by an explicit ack. A released attempt with no commit receipt in the spool, or with one carrying the wrong cap, is not committed. Once the commit receipt is published on the attempt's own commit cap, the same request commits it and the task is acknowledged.
#[cfg(unix)]
#[spec("inbox/boundary/061")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn boundary_061_an_exact_commit_needs_a_released_attempt_and_its_receipt() {
    use crate::event::InboxAttemptRef;
    use crate::inbox_attempts::Stage;
    use crate::inbox_receipt::{CapsLookup, RecordKey, RecordStage};
    use std::os::unix::fs::PermissionsExt;
    let (hub, _state) = hub_with_spool(Duration::from_secs(30), Duration::from_secs(60));
    let commit = |id: &str, a: u32, e: u32| {
        hub.one_shot_committed(
            "p",
            "a",
            &[InboxAttemptRef {
                id: id.to_string(),
                a,
                e,
            }],
            None,
        )
    };
    let mut first = open_once(&hub, Some(HOOK_INTEGRATION));
    assert!(first.next().await.registered.is_some());
    let (p, ag) = spool_names("p", "a");
    let spool =
        crate::inbox_receipt::GenerationSpool::open(&hub.spool_env().unwrap(), &p, &ag).unwrap();
    let record = |id: &str, e: u32| RecordKey {
        msg: id.to_string(),
        a: 1,
        e,
        stage: RecordStage::Committed,
    };
    // A commit receipt published on the attempt's own commit cap, as the
    // deck minted it: what a holder of that cap could publish.
    let publish_commit = |id: &str, e: u32| {
        let cap = hub.lock().receipts.gens[&("p".to_string(), "a".to_string())]
            .evidence
            .store()
            .caps(id, 1, e)
            .expect("the deck minted the attempt's caps")
            .commit
            .clone();
        spool.publish(&record(id, e), &cap).unwrap();
    };
    let caps_dir = hub
        .spool_env()
        .unwrap()
        .join(spool_names("p", "a").0)
        .join(spool_names("p", "a").1)
        .join("caps");
    std::fs::set_permissions(&caps_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let id = hub
        .post_direct(Recipient::bound("p", "a"), content("never sent"))
        .unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(5), first.lines.next_line())
        .await
        .expect("the connection ends")
        .unwrap();
    std::fs::set_permissions(&caps_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(ended.is_none(), "nothing was sent: {ended:?}");
    assert_eq!(
        attempt_state(&hub, "p", "a", id.as_str(), 1, 1).0,
        Stage::NotHanded
    );
    publish_commit(id.as_str(), 1);
    assert_eq!(
        commit(id.as_str(), 1, 1),
        0,
        "a not-handed attempt, even with a receipt on its cap"
    );
    assert_eq!(state_of(&hub, "p", id.as_str()), StateName::Queued);
    assert_eq!(
        attempt_state(&hub, "p", "a", id.as_str(), 1, 1).0,
        Stage::NotHanded
    );

    let (entered, go) = hold_caps(&hub);
    let mut second = open_once(&hub, Some(HOOK_INTEGRATION));
    assert!(second.next().await.registered.is_some());
    entered
        .recv_timeout(Duration::from_secs(5))
        .expect("the caps write started");
    publish_commit(id.as_str(), 2);
    assert_eq!(
        commit(id.as_str(), 1, 2),
        0,
        "a withheld attempt, even with a receipt on its cap"
    );
    assert_eq!(
        hub.ack(id.as_str(), "p", "a", AckHow::Explicit),
        AckOutcome::Unknown,
        "no attempt reached the agent, so an ack commits none"
    );
    assert_eq!(
        attempt_state(&hub, "p", "a", id.as_str(), 1, 2).0,
        Stage::Issued
    );
    std::fs::remove_file(rec_dir(&hub, "p", "a").join(record(id.as_str(), 2).file_name())).unwrap();
    go.send(()).unwrap();
    let batch = second.handout().await;
    *hub.hold_caps.lock().unwrap() = None;
    assert_eq!((attempt(&batch[0]).a, attempt(&batch[0]).e), (1, 2));
    assert_eq!(commit(id.as_str(), 1, 2), 0, "no receipt in the spool");
    let CapsLookup::Found(caps) = caps_file(&hub, id.as_str(), 1, 2) else {
        panic!("the released attempt's caps file");
    };
    let key = record(id.as_str(), 2);
    spool.publish(&key, &caps.handed).unwrap();
    assert_eq!(commit(id.as_str(), 1, 2), 0, "a receipt on the wrong cap");
    assert_eq!(state_of(&hub, "p", id.as_str()), StateName::Fetched);
    std::fs::remove_file(rec_dir(&hub, "p", "a").join(key.file_name())).unwrap();
    spool.publish(&key, &caps.commit).unwrap();
    assert_eq!(commit(id.as_str(), 1, 2), 1, "its own commit receipt");
    assert_eq!(state_of(&hub, "p", id.as_str()), StateName::Acknowledged);
    second.close().await;
}

/// A hub with a real spool and its own clock running, whose producers hand
/// nothing to a waiter themselves ([`ENV_TEST_PAUSE_WAITER_DELIVERY`]), so a
/// batch is issued and staged only by the clock's tick, which then writes
/// its caps files itself. A generation other than `p`/`a` expects an
/// integration soon, which wakes the clock once the test has posted.
#[cfg(unix)]
fn hub_staged_by_its_clock(
    ack_wait: Duration,
) -> (
    Arc<InboxHub>,
    tempfile::TempDir,
    tokio::task::JoinHandle<()>,
) {
    let state = tempfile::tempdir().unwrap();
    let limits = InboxLimits {
        ack_wait,
        ..InboxLimits::default()
    };
    let mut hub = InboxHub::new(deck(), limits, Duration::from_secs(30));
    hub.pause_waiter_delivery = true;
    let hub = Arc::new(hub);
    hub.set_spool(
        crate::inbox_receipt::Spool::start(&state.path().canonicalize().unwrap()).unwrap(),
    );
    hub.lock().receipts.arm_deadline = Duration::from_millis(50);
    let ticker = hub.spawn_ticker();
    (hub, state, ticker)
}

/// Scenario: A Claude once-waiter is armed, and a task posted for it is issued and staged by the deck's own clock, whose tick then starts writing the batch's caps file, and the write hangs. Nothing else drives the clock. With the write still hung, the clock runs past one ack-wait from staging: the attempt is recorded not handed by the deck, the task goes back to the queue, and the card says receipts are degraded. When the write finally returns, the waiter it was meant for is sent nothing, and a waiter armed afterwards is handed the task as attempt 1.2, not marked as a possible repeat. In a second pane, a task whose clock-owned caps write hangs is cancelled: once the staging deadline passes it ends cancelled and its evidence is returned while the write is still hung, and when the write returns the caps file it linked is removed.
#[cfg(unix)]
#[spec("inbox/boundary/062")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn boundary_062_a_caps_write_that_never_returns_is_overtaken_by_its_staging_deadline() {
    use crate::inbox_attempts::Stage;
    use crate::inbox_receipt::CapsLookup;
    let ack_wait = Duration::from_millis(150);
    let (hub, _state, ticker) = hub_staged_by_its_clock(ack_wait);
    let mut waiter = open_once(&hub, Some(HOOK_INTEGRATION));
    assert!(waiter.next().await.registered.is_some());
    let (entered, go) = hold_caps(&hub);
    let id = hub
        .post_direct(Recipient::bound("p", "a"), content("hung write"))
        .unwrap();
    assert!(
        hub.lock().receipts.staged.is_empty(),
        "the post staged nothing itself"
    );
    hub.expect_integration("p", "b");
    let started = tokio::task::spawn_blocking(move || {
        entered
            .recv_timeout(Duration::from_secs(5))
            .expect("the clock's tick started the caps write")
    });
    started.await.unwrap();
    let id = id.as_str().to_string();
    until(
        "the staging deadline to pass with the write still hung",
        || attempt_state(&hub, "p", "a", &id, 1, 1).0 == Stage::NotHanded,
    )
    .await;
    assert_eq!(state_of(&hub, "p", &id), StateName::Queued);
    let degraded = hub.summary("p", "a").notices.receipts_degraded;
    assert!(
        degraded
            .as_deref()
            .is_some_and(|why| why.contains("did not finish")),
        "the card names the stalled write: {degraded:?}"
    );
    go.send(()).unwrap();
    *hub.hold_caps.lock().unwrap() = None;
    let ended = tokio::time::timeout(Duration::from_secs(5), waiter.lines.next_line())
        .await
        .expect("the waiter's connection ends")
        .unwrap();
    assert!(ended.is_none(), "the late release sent nothing: {ended:?}");
    assert_eq!(state_of(&hub, "p", &id), StateName::Queued);
    let mut next = open_once(&hub, Some(HOOK_INTEGRATION));
    assert!(next.next().await.registered.is_some());
    let batch = next.handout().await;
    assert_eq!(batch[0].id, id);
    assert_eq!((attempt(&batch[0]).a, attempt(&batch[0]).e), (1, 2));
    assert!(
        !batch[0].text.contains("This may repeat"),
        "attempt 1.1 never left the deck: {}",
        batch[0].text
    );
    next.close().await;
    ticker.abort();

    let (hub, _state, ticker) = hub_staged_by_its_clock(ack_wait);
    let mut waiter = open_once(&hub, Some(HOOK_INTEGRATION));
    assert!(waiter.next().await.registered.is_some());
    let (entered, go) = hold_caps(&hub);
    let id = hub
        .post_direct(Recipient::bound("p", "a"), content("cancelled while hung"))
        .unwrap();
    hub.expect_integration("p", "b");
    let started = tokio::task::spawn_blocking(move || {
        entered
            .recv_timeout(Duration::from_secs(5))
            .expect("the clock's tick started the caps write")
    });
    started.await.unwrap();
    let id = id.as_str().to_string();
    let revision = hub.lock().receipts.gens[&("p".to_string(), "a".to_string())]
        .model
        .delivery(&id)
        .unwrap()
        .revision;
    let cancel = hub.inbox_action("p", "a", &id, revision, false, "desktop-1");
    assert_eq!(
        cancel.outcome,
        crate::daemon_protocol::InboxActionOutcome::Applied
    );
    until("the staging deadline to end the cancelled task", || {
        state_of(&hub, "p", &id) == StateName::Undeliverable
    })
    .await;
    assert_eq!(
        attempt_state(&hub, "p", "a", &id, 1, 1),
        (Stage::NotHanded, true),
        "cancelled and reclaimed with the write still hung"
    );
    assert_eq!(evidence_reserved(&hub, "p", "a"), 0);
    go.send(()).unwrap();
    *hub.hold_caps.lock().unwrap() = None;
    // The Cancel revokes the attempt on the waiter's connection; no batch
    // follows it, and the late release closes it.
    while let Some(line) = tokio::time::timeout(Duration::from_secs(5), waiter.lines.next_line())
        .await
        .expect("the waiter's connection ends")
        .unwrap()
    {
        let reply: InboxWaitReply = serde_json::from_str(&line).unwrap();
        assert!(
            reply.messages.is_none(),
            "the late release sent nothing: {line}"
        );
    }
    // The removal runs in the same tick as that release, once it is done.
    until("the caps file the late write linked to be removed", || {
        !hub.clock_io.lock().unwrap().running
            && matches!(caps_file(&hub, &id, 1, 1), CapsLookup::Missing)
    })
    .await;
    ticker.abort();
}

/// Scenario: A Claude once-waiter is released a task, and its prompt hook publishes the task's committed receipt on the attempt's own cap. The exact-commit request's receipt lookup finds it, and then, before the request reaches the deck's lock, the deck records the attempt not handed. The request is refused under the lock, and the clock's next spool check still applies that receipt: the commit contradicting the not-handed witness is recorded and the task is acknowledged.
#[cfg(unix)]
#[spec("inbox/boundary/063")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn boundary_063_an_exact_commit_refused_after_its_lookup_still_reaches_the_clock() {
    use crate::event::InboxAttemptRef;
    use crate::inbox_attempts::Stage;
    use crate::inbox_receipt::{CapsLookup, RecordKey, RecordStage};
    let key: AgentKey = ("p".into(), "a".into());
    let (hub, _state) = hub_with_spool(Duration::from_secs(30), Duration::from_secs(60));
    let mut waiter = open_once(&hub, Some(HOOK_INTEGRATION));
    let rx = waiter
        .next()
        .await
        .registered
        .expect("the waiter registered")
        .rx;
    let id = hub
        .post_direct(Recipient::bound("p", "a"), content("contradicted"))
        .unwrap();
    let batch = waiter.handout().await;
    assert_eq!((attempt(&batch[0]).a, attempt(&batch[0]).e), (1, 1));
    let CapsLookup::Found(caps) = caps_file(&hub, id.as_str(), 1, 1) else {
        panic!("the released attempt's caps file");
    };
    let (p, ag) = spool_names("p", "a");
    crate::inbox_receipt::GenerationSpool::open(&hub.spool_env().unwrap(), &p, &ag)
        .unwrap()
        .publish(
            &RecordKey {
                msg: id.as_str().to_string(),
                a: 1,
                e: 1,
                stage: RecordStage::Committed,
            },
            &caps.commit,
        )
        .unwrap();
    let named = [InboxAttemptRef {
        id: id.as_str().to_string(),
        a: 1,
        e: 1,
    }];
    let witnessed = hub.witnessed_commits(&key, &named);
    assert_eq!(witnessed.len(), 1, "the lookup found the receipt");
    hub.deck_not_handed(&key, crate::inbox_attempts::Rx(rx), &named);
    assert_eq!(
        attempt_state(&hub, "p", "a", id.as_str(), 1, 1).0,
        Stage::NotHanded
    );
    let mut out = engine::Outbox::new();
    let applied = {
        let mut st = hub.lock();
        engine::one_shot_committed(&mut st, &key, &witnessed, None, Instant::now(), &mut out)
    };
    engine::flush(out);
    assert_eq!(
        applied, 0,
        "refused under the lock: the attempt is not handed"
    );
    assert_eq!(state_of(&hub, "p", id.as_str()), StateName::Queued);
    hub.tick();
    assert_eq!(
        attempt_state(&hub, "p", "a", id.as_str(), 1, 1).0,
        Stage::Committed,
        "the clock's scan applied the receipt the refused request had found"
    );
    assert_eq!(state_of(&hub, "p", id.as_str()), StateName::Acknowledged);
    waiter.close().await;
}

/// Scenario: A Claude once-waiter's task is committed while the generation's caps directory cannot be written to, so removing the reclaimed attempt's caps file fails. The caps file stays and the removal is kept; once the directory can be written again, the clock's next tick removes it. A second task's removal that keeps failing is tried on three ticks in all and then given up, its caps file left for the agent's exit.
#[cfg(unix)]
#[spec("inbox/boundary/064")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn boundary_064_a_failed_removal_is_tried_again_on_the_next_tick() {
    use crate::event::InboxAttemptRef;
    use crate::inbox_receipt::{CapsLookup, RecordKey, RecordStage};
    use std::os::unix::fs::PermissionsExt;
    let (hub, _state) = hub_with_spool(Duration::from_secs(30), Duration::from_secs(60));
    let mut waiter = open_once(&hub, Some(HOOK_INTEGRATION));
    assert!(waiter.next().await.registered.is_some());
    let (p, ag) = spool_names("p", "a");
    let spool =
        crate::inbox_receipt::GenerationSpool::open(&hub.spool_env().unwrap(), &p, &ag).unwrap();
    let caps_dir = hub.spool_env().unwrap().join(&p).join(&ag).join("caps");
    let failed = || hub.lock().receipts.failed_removals.len();
    let commit_with_caps_locked = |id: &str| {
        let CapsLookup::Found(caps) = caps_file(&hub, id, 1, 1) else {
            panic!("the released attempt's caps file");
        };
        let key = RecordKey {
            msg: id.to_string(),
            a: 1,
            e: 1,
            stage: RecordStage::Committed,
        };
        spool.publish(&key, &caps.commit).unwrap();
        std::fs::set_permissions(&caps_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let named = [InboxAttemptRef {
            id: id.to_string(),
            a: 1,
            e: 1,
        }];
        assert_eq!(hub.one_shot_committed("p", "a", &named, None), 1);
    };

    let id = hub
        .post_direct(Recipient::bound("p", "a"), content("removed later"))
        .unwrap();
    waiter.handout().await;
    commit_with_caps_locked(id.as_str());
    assert!(reclaimed(&hub, "p", id.as_str(), 1, 1));
    assert!(
        matches!(caps_file(&hub, id.as_str(), 1, 1), CapsLookup::Found(_)),
        "the removal failed"
    );
    assert_eq!(failed(), 1, "and is kept for the next tick");
    std::fs::set_permissions(&caps_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    hub.tick();
    assert!(
        matches!(caps_file(&hub, id.as_str(), 1, 1), CapsLookup::Missing),
        "the next tick removed it"
    );
    assert_eq!(failed(), 0);
    waiter.close().await;

    let mut waiter = open_once(&hub, Some(HOOK_INTEGRATION));
    assert!(waiter.next().await.registered.is_some());
    let second = hub
        .post_direct(Recipient::bound("p", "a"), content("never removed"))
        .unwrap();
    waiter.handout().await;
    commit_with_caps_locked(second.as_str());
    for tick in 1..engine::REMOVAL_TRIES {
        assert_eq!(failed(), 1, "kept before tick {tick}");
        hub.tick();
    }
    assert_eq!(
        failed(),
        0,
        "given up after {} tries",
        engine::REMOVAL_TRIES
    );
    std::fs::set_permissions(&caps_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(
        caps_file(&hub, second.as_str(), 1, 1),
        CapsLookup::Found(_)
    ));
    waiter.close().await;
}

/// The refusal code `pane`/`a`'s card names now.
fn refusal_of(hub: &InboxHub, pane: &str) -> Option<String> {
    hub.summary(pane, "a").notices.admission_refused
}

/// Scenario: The deck refuses a worker's own request for its messages before any wait: its card says nothing. Once a task delegated to it waits, the card names the refusal, a client is told so and the task's orchestrator is told; an admitted request clears it. A refusal in a new episode tells the task's orchestrator once more, naming the task and the cause, however often it repeats; an orchestrator in the worker's own pane is never told. The worker's exit clears the notice, and a refusal for a generation that no longer holds its pane is ignored.
#[spec("inbox/boundary/065")]
#[test]
fn boundary_065_a_refused_request_marks_its_own_card_while_mail_waits() {
    let hub = hub_with(Duration::from_secs(30), Duration::from_secs(60));
    let (tx, mut rx) = broadcast::channel(256);
    hub.set_broadcast(tx);
    hub.admission_refused("p", "a", "inbox_peer_unidentified");
    assert_eq!(refusal_of(&hub, "p"), None, "nothing waits for it");

    let first = delegate_to(&hub, "p", "first task");
    assert_eq!(
        refusal_of(&hub, "p").as_deref(),
        Some("inbox_peer_unidentified"),
        "a task now waits"
    );
    let mut told = false;
    while let Ok(message) = rx.try_recv() {
        if let BroadcastMsg::Inbox(notice) = message
            && notice.attention_update
            && notice.notices.admission_refused.as_deref() == Some("inbox_peer_unidentified")
        {
            told = true;
        }
    }
    assert!(told, "clients are told the card's notice");
    assert_eq!(
        orchestrator_notices(&hub).len(),
        1,
        "a task delegated into the open episode tells its orchestrator (inbox/boundary/066)"
    );

    hub.admission_admitted("p", "a");
    assert_eq!(refusal_of(&hub, "p"), None, "an admitted request clears it");

    for _ in 0..3 {
        hub.admission_refused("p", "a", "inbox_nested_agent");
    }
    assert_eq!(refusal_of(&hub, "p").as_deref(), Some("inbox_nested_agent"));
    let notices = orchestrator_notices(&hub);
    assert_eq!(notices.len(), 2, "one notice per episode: {notices:?}");
    let (subject, text) = &notices[1];
    assert_eq!(subject, ADMISSION_REFUSED_SUBJECT);
    assert!(text.contains(first.as_str()), "names the task: {text}");
    assert!(
        text.contains(crate::inbox_display::ADMISSION_CAUSE_NESTED),
        "names the cause: {text}"
    );

    // An orchestrator in the refused worker's own pane is that agent itself.
    hub.post_delegate(
        Recipient::bound("q", "a"),
        "self",
        Some(Recipient::bound("q", "a")),
        DelegateLedger::default(),
        content("to itself"),
        &|| true,
    )
    .unwrap();
    hub.admission_refused("q", "a", "inbox_other_user");
    assert_eq!(refusal_of(&hub, "q").as_deref(), Some("inbox_other_user"));
    hub.post_owed_notices();
    assert!(
        hub.list_with_bodies(Some("q"))
            .iter()
            .all(|m| m.subject != ADMISSION_REFUSED_SUBJECT),
        "no notice to the refused agent itself"
    );

    hub.agent_exited("p", "a");
    assert!(
        !hub.lock()
            .admission_refused
            .contains_key(&("p".into(), "a".into()))
    );

    hub.set_live_agent_check(Arc::new(|pane: &str, _: &str| pane != "gone"));
    hub.admission_refused("gone", "a", "inbox_other_user");
    assert!(
        !hub.lock()
            .admission_refused
            .contains_key(&("gone".into(), "a".into())),
        "a generation no longer in its pane records nothing"
    );
}

/// The refusal notices posted to orchestrator pane `pane` so far.
fn refusal_notices(hub: &InboxHub, pane: &str) -> Vec<String> {
    hub.post_owed_notices();
    hub.list_with_bodies(Some(pane))
        .into_iter()
        .filter(|m| m.subject == ADMISSION_REFUSED_SUBJECT)
        .map(|m| m.text.clone().unwrap_or_default())
        .collect()
}

/// Scenario: The deck refuses a worker's own request for its messages before anything is delegated to it. An orchestrator then delegates a task to it: that orchestrator is told once, naming the task and the cause, and is not told again however often the refusal repeats or it delegates again. A second orchestrator delegating into the same refusal is told once too. Once a request is admitted, a new refusal tells the orchestrator whose task waits then, once.
#[spec("inbox/boundary/066")]
#[test]
fn boundary_066_an_open_refusal_tells_each_orchestrator_that_delegates_into_it_once() {
    let hub = hub_with(Duration::from_secs(30), Duration::from_secs(60));
    hub.admission_refused("p", "a", "inbox_peer_unidentified");
    assert!(
        refusal_notices(&hub, "o").is_empty(),
        "nothing delegated yet"
    );

    let first = delegate_to(&hub, "p", "first task");
    let told = refusal_notices(&hub, "o");
    assert_eq!(
        told.len(),
        1,
        "the orchestrator is told when its task starts waiting: {told:?}"
    );
    assert!(told[0].contains(first.as_str()), "names the task: {told:?}");
    assert!(
        told[0].contains(crate::inbox_display::ADMISSION_CAUSE_UNIDENTIFIED),
        "names the cause: {told:?}"
    );
    for _ in 0..3 {
        hub.admission_refused("p", "a", "inbox_peer_unidentified");
    }
    delegate_to(&hub, "p", "second task");
    hub.admission_refused("p", "a", "inbox_peer_unidentified");
    assert_eq!(
        refusal_notices(&hub, "o").len(),
        1,
        "once in the episode, however often it repeats or the orchestrator delegates again"
    );

    let third = hub
        .post_delegate(
            Recipient::bound("p", "a"),
            "reviewer",
            Some(Recipient::bound("o2", "ob")),
            DelegateLedger::default(),
            content("third task"),
            &|| true,
        )
        .unwrap()
        .0;
    hub.admission_refused("p", "a", "inbox_peer_unidentified");
    let other = refusal_notices(&hub, "o2");
    assert_eq!(
        other.len(),
        1,
        "the second orchestrator is told once: {other:?}"
    );
    assert!(other[0].contains(third.as_str()), "{other:?}");
    assert_eq!(refusal_notices(&hub, "o").len(), 1);

    hub.admission_admitted("p", "a");
    hub.admission_refused("p", "a", "inbox_other_user");
    hub.admission_refused("p", "a", "inbox_other_user");
    assert_eq!(
        refusal_notices(&hub, "o2").len(),
        2,
        "a new episode tells the orchestrator whose task waits, once"
    );
    assert_eq!(
        refusal_notices(&hub, "o").len(),
        1,
        "its own task was superseded"
    );
}
