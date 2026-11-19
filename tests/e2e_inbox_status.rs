#![cfg(all(feature = "e2e", unix))]

//! Attached-client tests of live inbox summaries, state-change broadcasts
//! and persisted deck identity. Synthetic panes spend no agent credential.

dot_agent_deck_test_harness::link!();
#[path = "support/inbox_doorbell.rs"]
mod inbox;

use dot_agent_deck::daemon_protocol::{KIND_EVENT, KIND_REQ, KIND_RESP, PROTOCOL_VERSION};
use dot_agent_deck::event::BroadcastMsg;
use inbox::*;
use serde_json::{Value, json};
use spec::spec;
use std::cell::RefCell;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

fn attention_client(daemon: &common::DaemonProc) -> common::TuiDeck {
    common::TuiDeck::builder()
        .with_pty_size(200, 50)
        .with_env(
            "DOT_AGENT_DECK_ATTACH_SOCKET",
            daemon.attach_socket.to_string_lossy(),
        )
        .with_env(
            "DOT_AGENT_DECK_SOCKET",
            daemon.hook_socket.to_string_lossy(),
        )
        .launch_with_fixture("minimal")
}

fn attention_header(deck: &common::TuiDeck) -> String {
    // Inspect chrome, not a held agent's already-working card notice.
    deck.snapshot_grid()
        .lines()
        .take_while(|line| !line.starts_with(['┌', '┏', '╭']))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The grid below the chrome [`attention_header`] reads: the cards, so a
/// held name in the header does not read as that agent's card on screen.
fn below_header(grid: &str) -> String {
    grid.lines()
        .skip_while(|line| !line.trim_start().starts_with(['┌', '┏', '╭']))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Issue #2387: the header names the held cards, the start role first, and
/// leaves each hold's words to its card, which carries the `held` marker.
fn assert_orchestrator_attention(deck: &common::TuiDeck, reason: &str) {
    let words: Value = serde_json::from_str(include_str!("fixtures/inbox-display.json")).unwrap();
    let named = format!(
        "{} {}: ",
        words["glyph"].as_str().unwrap(),
        words["attentionHeld"].as_str().unwrap()
    );
    assert!(
        common::wait_until(WAIT, || {
            let header = attention_header(deck);
            let Some(at) = header.find(&named) else {
                return false;
            };
            let names = &header[at + named.len()..];
            // The start role first, then the other held recipient; the
            // worker, which the deck holds nothing for, is not named.
            names.starts_with("attention-team / attention-orchestrator, attention-other")
                && !header.contains(reason)
                && !header.contains("coder")
        }),
        "deck-wide attention must name the held orchestrator first and the other held recipient, without the hold's words or the worker; grid:\n{}",
        deck.snapshot_grid()
    );
}

/// Issue #2387: a held card on screen says `held` beside its status.
fn assert_card_marked(deck: &common::TuiDeck, name: &str) {
    let held = |grid: &str| {
        grid.lines().any(|line| {
            line.trim_start().starts_with(['┌', '┏', '╭'])
                && line.contains(name)
                && line.contains(" held ")
        })
    };
    assert!(
        common::wait_until(WAIT, || held(&deck.snapshot_grid())),
        "{name}'s card must carry the held marker beside its status; grid:\n{}",
        deck.snapshot_grid()
    );
}

/// Scenario: A worker reports to an orchestrator whose input holds an unsent draft, while one other recipient is held too. The real attached TUI must name the orchestrator first and then the other recipient after the envelope glyph and `held:`, without the worker or the hold's words, in its header even on another tab, mark the other recipient's card `held`, open that card's tab when its name in the header is clicked, retain that attention across detach/reattach and tab switches, then clear it after the user clears the draft and the report is taken.
#[spec("inbox/status/031")]
#[test]
fn status_031_orchestrator_attention_survives_tabs_and_reattach() {
    let daemon = daemon_unbound(&[("DOT_AGENT_DECK_INBOX_WAKE_STALL_AFTER_MS", "100")]);
    let member = |role, index, start| {
        json!({"tab_membership": {
            "kind":"orchestration", "name":"attention-team", "role_name":role,
            "role_index":index, "is_start_role":start, "orchestration_id":"attention-team",
            "orchestration_cwd":daemon.cwd()
        }})
    };
    let orch = pane_with(
        &daemon,
        "attention-orchestrator",
        member("orchestrator", 0, true),
    );
    let worker = pane_with(&daemon, "attention-coder", member("coder", 1, false));
    let other = pane(&daemon, "attention-other");
    let mut deck = attention_client(&daemon);
    deck.wait_for_string("attention-team");
    // Attaching can select the orchestration tab. Choose the Dashboard
    // explicitly before checking its distinct card and starting the round-trip.
    let (col, row) = deck.wait_for_in_grid("Dashboard");
    deck.click(col + 1, row);
    deck.wait_until_grid(
        "Dashboard selected, showing its non-team recipient",
        |grid| grid.contains("attention-other") && !grid.contains("attention-coder"),
    );
    for agent in [&orch, &other] {
        status(&daemon, agent, "session_start", "pi");
        status(&daemon, agent, "idle", "pi");
    }
    let mut draft = input(&daemon, &orch, DRAFT);
    let mut other_draft = input(&daemon, &other, DRAFT);
    let mut report = request(&worker, "work_done");
    report["task"] = json!("ATTENTION-WORKER-REPORT-79d1");
    report["timestamp"] = json!(chrono::Utc::now());
    assert_eq!(
        hook(&daemon, report)["accepted"],
        true,
        "worker report accepted"
    );
    enqueue(&daemon, &other, "other held work");
    // Q35-21: a touched box no check can read holds with its reason, for
    // this pane `cannot_type`, the hold this attention names.
    assert!(
        common::wait_until(WAIT, || summary(&daemon, &orch)["wake_hold"]
            == "cannot_type"),
        "precondition: the orchestrator's real inbox is held for its draft: {}",
        summary(&daemon, &orch)
    );
    assert!(common::wait_until(
        WAIT,
        || summary(&daemon, &other)["wake_hold"] == "cannot_type"
    ));
    let words: Value = serde_json::from_str(include_str!("fixtures/inbox-display.json")).unwrap();
    let reason = words["wakeHolds"]["cannot_type"].as_str().unwrap();
    assert_orchestrator_attention(&deck, reason);
    assert_card_marked(&deck, "attention-other");
    // Dashboard -> the team's tab -> Dashboard. No focus or selection clears it.
    deck.send_keys(b"\x1b[C");
    deck.wait_until_grid("team tab selected, showing its worker", |grid| {
        let cards = below_header(grid);
        cards.contains("attention-coder") && !cards.contains("attention-other")
    });
    assert_orchestrator_attention(&deck, reason);
    // Issue #2387: a held name in the header opens that agent's card. On the
    // team's tab the only `attention-other` on screen is the header's.
    let (col, row) = deck.wait_for_in_grid("attention-other");
    deck.click(col + 1, row);
    deck.wait_until_grid(
        "clicking attention-other in the header shows its card on the Dashboard",
        |grid| {
            let cards = below_header(grid);
            cards.contains("attention-other") && !cards.contains("attention-coder")
        },
    );
    assert_orchestrator_attention(&deck, reason);
    deck.send_keys(b"\x03");
    deck.wait_for_string("Quit dot-agent-deck?");
    deck.send_keys(b"\r");
    assert_eq!(deck.wait_for_exit_within(WAIT), Some(true));
    let reattached = attention_client(&daemon);
    let (col, row) = reattached.wait_for_in_grid("Dashboard");
    reattached.click(col + 1, row);
    reattached.wait_until_grid(
        "reattached Dashboard shows its non-team recipient",
        |grid| grid.contains("attention-other") && !grid.contains("attention-coder"),
    );
    assert_orchestrator_attention(&reattached, reason);
    draft.write("\u{15}");
    other_draft.write("\u{15}");
    let taken = fetch(&daemon, &orch, true);
    assert!(taken.to_string().contains("ATTENTION-WORKER-REPORT-79d1"));
    fetch(&daemon, &other, true);
    assert!(
        // The item, in either form (`✉ held: …`, `✉ 2 held`); the pane
        // beside the cards still titles itself with an agent's name.
        common::wait_until(WAIT, || !attention_header(&reattached)
            .lines()
            .any(|line| line.contains('✉') && line.contains(" held"))),
        "clearing the drafts and taking the messages must clear header attention: {}",
        reattached.snapshot_grid()
    );
}

/// What `daemon status --json` and the human `daemon status` say about
/// `pane`: its row's `inbox` object, and its human row.
fn status_rows(daemon: &common::DaemonProc, pane: &Pane) -> (Value, String) {
    let run = |json: bool| {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"));
        command.args(["daemon", "status"]);
        if json {
            command.arg("--json");
        }
        let output = command
            .env("DOT_AGENT_DECK_ATTACH_SOCKET", &daemon.attach_socket)
            .env("DOT_AGENT_DECK_SOCKET", &daemon.hook_socket)
            .output()
            .expect("run `daemon status`");
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    };
    let parsed: Value = serde_json::from_str(&run(true)).expect("status JSON");
    let inbox = parsed["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|agent| agent["pane_id"] == pane.id.as_str())
        .map(|agent| agent["inbox"].clone())
        .unwrap_or_else(|| panic!("no status row for {}: {parsed}", pane.id));
    let human = run(false)
        .lines()
        .find(|line| line.starts_with(&format!("{}\t", pane.id)))
        .unwrap_or_else(|| panic!("no human status row for {}", pane.id))
        .to_string();
    (inbox, human)
}

/// Scenario: An orchestrator reads busy (Thinking) and a worker's report waits in its inbox. While the orchestrator keeps sending tool events the deck says nothing; once it has sent nothing for the busy bound, `daemon status` and its `--json` say the report has not been taken, and in the attached TUI the orchestrator's card says so and is marked `held`, and the header names it. Taking the report clears all of it.
#[spec("inbox/status/045")]
#[test]
fn status_045_mail_behind_a_silent_busy_status_is_shown_not_taken() {
    const BOUND_MS: u64 = 2000;
    let daemon = daemon_unbound(&[(
        "DOT_AGENT_DECK_INBOX_BUSY_STALL_AFTER_MS",
        &BOUND_MS.to_string(),
    )]);
    let member = |role, index, start| {
        json!({"tab_membership": {
            "kind":"orchestration", "name":"silent-team", "role_name":role,
            "role_index":index, "is_start_role":start, "orchestration_id":"silent-team",
            "orchestration_cwd":daemon.cwd()
        }})
    };
    let orch = pane_with(
        &daemon,
        "silent-orchestrator",
        member("orchestrator", 0, true),
    );
    let worker = pane_with(&daemon, "silent-coder", member("coder", 1, false));
    let deck = attention_client(&daemon);
    deck.wait_for_string("silent-team");
    // The orchestrator's card is on its team's tab: Dashboard, then one right.
    let (col, row) = deck.wait_for_in_grid("Dashboard");
    deck.click(col + 1, row);
    deck.send_keys(b"\x1b[C");
    deck.wait_until_grid("team tab selected, showing its cards", |grid| {
        below_header(grid).contains("silent-coder")
    });
    status(&daemon, &orch, "session_start", "claude_code");
    status(&daemon, &orch, "thinking", "claude_code");
    let mut report = request(&worker, "work_done");
    report["task"] = json!("SILENT-BUSY-WORKER-REPORT-2389");
    report["timestamp"] = json!(chrono::Utc::now());
    assert_eq!(
        hook(&daemon, report)["accepted"],
        true,
        "worker report accepted"
    );
    assert!(
        common::wait_until(WAIT, || summary(&daemon, &orch)["queued"] == 1),
        "precondition: the report waits for the orchestrator: {}",
        summary(&daemon, &orch)
    );
    // The guard: an agent that keeps sending hook events is working, so for
    // twice the bound nothing says its mail is stuck.
    let guard_until = Instant::now() + Duration::from_millis(BOUND_MS * 2);
    while Instant::now() < guard_until {
        status(&daemon, &orch, "tool_start", "claude_code");
        let inbox = summary(&daemon, &orch);
        assert!(
            inbox["wake_stalled"] != true && inbox["wake_hold"].is_null(),
            "an agent still sending events must not be reported stuck: {inbox}"
        );
        // The gap between events, observed rather than slept: the deck types
        // nothing into a pane that reads busy.
        unchanged(&orch, Duration::from_millis(250));
    }
    // Silence: the agent still reads busy and sends nothing more.
    assert!(
        common::wait_until(WAIT, || {
            let inbox = summary(&daemon, &orch);
            inbox["wake_stalled"] == true && inbox["wake_hold"] == "no_activity"
        }),
        "mail behind a busy status with no events past the bound must be reported not taken: {}",
        summary(&daemon, &orch)
    );
    let (json_inbox, human) = status_rows(&daemon, &orch);
    assert_eq!(json_inbox["wake_stalled"], true, "{json_inbox}");
    assert_eq!(json_inbox["wake_hold"], "no_activity", "{json_inbox}");
    assert!(
        human.contains("stalled"),
        "the human `daemon status` row must say the mail is stuck: {human:?}"
    );
    let words: Value = serde_json::from_str(include_str!("fixtures/inbox-display.json")).unwrap();
    let reason = words["wakeHolds"]["no_activity"]
        .as_str()
        .expect("shared words for no_activity");
    // The words' opening, which a card of any width shows before it cuts.
    let opening = &reason[..reason.find(" but ").expect("the words' opening")];
    assert!(
        common::wait_until(WAIT, || below_header(&deck.snapshot_grid())
            .contains(opening)),
        "the orchestrator's card must say its mail is not taken; grid:\n{}",
        deck.snapshot_grid()
    );
    assert_card_marked(&deck, "silent-orchestrator");
    // Issue #2387's header: it names the held card and leaves the words to it.
    let named = format!(
        "{} {}: silent-team / silent-orchestrator",
        words["glyph"].as_str().unwrap(),
        words["attentionHeld"].as_str().unwrap()
    );
    assert!(
        common::wait_until(WAIT, || {
            let header = attention_header(&deck);
            header.contains(&named) && !header.contains(opening)
        }),
        "the header must name the held orchestrator, without the words; grid:\n{}",
        deck.snapshot_grid()
    );
    let taken = fetch(&daemon, &orch, true);
    assert!(taken.to_string().contains("SILENT-BUSY-WORKER-REPORT-2389"));
    assert!(
        common::wait_until(WAIT, || {
            let inbox = summary(&daemon, &orch);
            inbox["wake_stalled"] != true && inbox["wake_hold"].is_null()
        }),
        "taking the report clears it: {}",
        summary(&daemon, &orch)
    );
    assert!(
        common::wait_until(WAIT, || {
            let grid = deck.snapshot_grid();
            !attention_header(&deck).contains(&named) && !below_header(&grid).contains(opening)
        }),
        "taking the report clears the card's words and the header: {}",
        deck.snapshot_grid()
    );
}

fn summary(daemon: &impl Endpoint, pane: &Pane) -> Value {
    let reply = attach(daemon, json!({"op":"list-agents"}));
    assert_eq!(reply["ok"], true, "ListAgents: {reply}");
    let record = reply["agent_records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["id"] == pane.agent)
        .unwrap();
    assert!(
        record["inbox"].is_object(),
        "ListAgents must carry AgentRecord.inbox: {record}"
    );
    record["inbox"].clone()
}

fn pending(daemon: &impl Endpoint, pane: &Pane, queued: u64, fetched: u64, wake: &str) {
    let value = summary(daemon, pane);
    assert_eq!(value["queued"], queued, "queued count: {value}");
    assert_eq!(value["fetched"], fetched, "fetched count: {value}");
    assert_eq!(value["undeliverable"], 0, "undeliverable count: {value}");
    assert_eq!(value["wake"], wake, "current wake path: {value}");
}

/// Scenario: Queue and fetch work for one synthetic agent while another remains empty, then announce Pi's extension without a watcher. ListAgents must show doorbell fallback until Pi's receipt stream registers, native while it is registered, and doorbell again after EOF; acknowledgements and other panes' counts remain separate.
#[spec("inbox/status/003")]
#[test]
fn status_003_list_agents_reports_counts_and_armed_wake() {
    let daemon = daemon_unbound(&[("DOT_AGENT_DECK_INBOX_TEST_PAUSE_WAITER_DELIVERY", "1")]);
    let worker = pane(&daemon, "status-worker");
    let control = pane(&daemon, "status-control");
    status(&daemon, &worker, "idle", "codex");
    status(&daemon, &control, "idle", "codex");
    let first = enqueue(&daemon, &worker, "status-first");
    fetch(&daemon, &worker, false);
    state(&daemon, &worker, &first, "fetched");
    enqueue(&daemon, &worker, "status-second");
    enqueue(&daemon, &worker, "status-third");
    pending(&daemon, &worker, 2, 1, "doorbell");
    pending(&daemon, &control, 0, 0, "doorbell");
    let mut ack = request(&worker, "ack");
    ack["delivery_id"] = json!(first);
    assert_eq!(hook(&daemon, ack)["accepted"], true);
    pending(&daemon, &worker, 2, 0, "doorbell");
    assert!(
        !summary(&daemon, &worker)["last_acked_at"].is_null(),
        "ack time must be reported"
    );
    let mut pi_announce = request(&worker, "inbox_announce");
    pi_announce["component"] = json!("pi_extension");
    pi_announce["component_version"] = json!("synthetic-pi");
    pi_announce["wake"] = json!("native");
    pi_announce["pid"] = json!(worker.pid);
    assert_eq!(hook(&daemon, pi_announce)["accepted"], true);
    pending(&daemon, &worker, 2, 0, "doorbell");
    assert_eq!(summary(&daemon, &worker)["component"], "pi_extension");
    // Pi's extension arms its receipt stream (PRD #1666).
    let wait = receipt_stream_as(&daemon, &worker, "pi_extension");
    pending(&daemon, &worker, 2, 0, "native");
    assert_eq!(summary(&daemon, &worker)["component"], "pi_extension");
    drop(wait);
    let listing_after = RefCell::new(Value::Null);
    assert!(
        common::wait_until(WAIT, || {
            *listing_after.borrow_mut() = summary(&daemon, &worker);
            listing_after.borrow()["wake"] == "doorbell"
        }),
        "the closed stream leaves the doorbell: {}",
        listing_after.borrow()
    );
    announce(&daemon, &worker, "turn_end");
    pending(&daemon, &worker, 2, 0, "turn_end");
}

fn frame(socket: &mut UnixStream) -> (u8, Value) {
    let mut header = [0; 5];
    socket.read_exact(&mut header).unwrap();
    let size = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
    assert!(size < 1024 * 1024);
    let mut body = vec![0; size];
    socket.read_exact(&mut body).unwrap();
    (header[0], serde_json::from_slice(&body).unwrap())
}

fn subscribe(daemon: &impl Endpoint) -> UnixStream {
    let mut socket = UnixStream::connect(daemon.attach_socket()).unwrap();
    socket.set_read_timeout(Some(WAIT)).unwrap();
    socket.set_write_timeout(Some(WAIT)).unwrap();
    let payload = serde_json::to_vec(&json!({"op":"subscribe-events"})).unwrap();
    socket.write_all(&[KIND_REQ]).unwrap();
    socket
        .write_all(&(payload.len() as u32).to_be_bytes())
        .unwrap();
    socket.write_all(&payload).unwrap();
    let (kind, reply) = frame(&mut socket);
    assert_eq!(kind, KIND_RESP);
    assert_eq!(reply["ok"], true, "subscription barrier: {reply}");
    socket
}

fn transition(socket: &mut UnixStream, pane: &Pane, id: &str, expected: &str) {
    let deadline = Instant::now() + WAIT;
    loop {
        socket
            .set_read_timeout(Some(deadline.saturating_duration_since(Instant::now())))
            .unwrap();
        let mut first = [0];
        socket.read_exact(&mut first).unwrap_or_else(|error| {
            panic!("attached client must receive Inbox broadcast for {id} -> {expected}: {error}")
        });
        let mut size = [0; 4];
        socket.read_exact(&mut size).unwrap();
        let size = u32::from_be_bytes(size) as usize;
        assert!(size < 1024 * 1024);
        let mut payload = vec![0; size];
        socket.read_exact(&mut payload).unwrap();
        assert_eq!(first[0], KIND_EVENT);
        let event: Value = serde_json::from_slice(&payload).unwrap();
        if event["kind"] != "inbox" {
            continue;
        }
        let typed: BroadcastMsg = serde_json::from_value(event.clone()).unwrap();
        assert_eq!(
            serde_json::to_value(typed).unwrap(),
            event,
            "Inbox must be a recognized BroadcastMsg variant"
        );
        if event["message_id"] != id {
            continue;
        }
        assert_eq!(event["pane_id"], pane.id);
        assert_eq!(event["subject"], "doorbell fixture");
        assert!(
            event["from_label"]
                .as_str()
                .is_some_and(|label| !label.is_empty()),
            "sender label: {event}"
        );
        assert_eq!(event["state"], expected, "broadcast ordering: {event}");
        return;
    }
}

/// Scenario: Subscribe an attached client before posting a message, then fetch it with an `inbox fetch` that prints nothing, so the deck takes it back, fetch it again and acknowledge it, and stop an agent with another queued message. The client must observe each Queued, Fetched, Queued, Fetched, Acknowledged and Undeliverable transition in order with the message identity, subject and sender label.
#[spec("inbox/status/004")]
#[test]
fn status_004_attached_client_receives_each_inbox_state_change() {
    let daemon = daemon_unbound(&[]);
    let worker = pane(&daemon, "broadcast-worker");
    let mut events = subscribe(&daemon);
    let id = enqueue(&daemon, &worker, "broadcast task");
    transition(&mut events, &worker, &id, "queued");
    fetch_not_handed(&daemon, &worker);
    transition(&mut events, &worker, &id, "fetched");
    transition(&mut events, &worker, &id, "queued");
    fetch(&daemon, &worker, false);
    transition(&mut events, &worker, &id, "fetched");
    let mut ack = request(&worker, "ack");
    ack["delivery_id"] = json!(id);
    assert_eq!(hook(&daemon, ack)["accepted"], true);
    transition(&mut events, &worker, &id, "acknowledged");
    let lost = enqueue(&daemon, &worker, "lost task");
    transition(&mut events, &worker, &lost, "queued");
    assert_eq!(
        attach(&daemon, json!({"op":"stop-agent","id":worker.agent}))["ok"],
        true
    );
    transition(&mut events, &worker, &lost, "undeliverable");
}

/// Scenario: Connect twice to the same daemon and request Hello, then post a message. Both replies must carry the same persisted deck_id, matching the deck token in that message's globally addressed id.
#[spec("inbox/status/005")]
#[test]
fn status_005_hello_names_the_inbox_deck() {
    let daemon = daemon_unbound(&[]);
    let worker = pane(&daemon, "hello-worker");
    let hello = || {
        attach(
            &daemon,
            json!({"op":"hello","client_version":PROTOCOL_VERSION}),
        )
    };
    let first = hello();
    assert_eq!(first["ok"], true, "Hello: {first}");
    let deck = first["deck_id"]
        .as_str()
        .unwrap_or_else(|| panic!("Hello must carry deck_id: {first}"));
    assert_eq!(deck.len(), 8, "deck id token: {deck}");
    assert_eq!(
        hello()["deck_id"],
        deck,
        "deck identity must be stable across clients"
    );
    let id = enqueue(&daemon, &worker, "hello identity");
    assert!(
        id.starts_with(&format!("m-{deck}-")),
        "Hello and inbox address must name the same deck: {id}"
    );
}
