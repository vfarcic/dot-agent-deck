#![cfg(all(feature = "e2e", unix))]

mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/inbox_route.rs"]
mod route;

use inbox::*;
use serde_json::json;
use spec::spec;
use std::time::Duration;

/// Scenario: Queue a task for an Idle synthetic agent with no armed component. Its pane must receive exactly one doorbell line, led by its ring token, and one submission, while the task body remains only in the inbox; an empty inbox never rings.
#[spec("inbox/doorbell/001")]
#[test]
fn doorbell_001_idle_unarmed_gets_only_fixed_submitted_line() {
    let daemon = daemon_unbound(&[]);
    let pane = pane(&daemon, "idle");
    status(&daemon, &pane, "idle", "codex");
    unchanged(&pane, Duration::from_millis(300));
    let id = enqueue(&daemon, &pane, BODY);
    armed(&daemon, &pane, 0);
    state(&daemon, &pane, &id, "queued");
    ring(&daemon, &pane, &id);
    assert_eq!(
        bytes(&pane),
        format!("{}\r", rung_line(&pane, &id).unwrap()).as_bytes(),
        "exact token-led line, no body, paste markers, LF or extra submits"
    );
    assert_eq!(rings(&pane).len(), 1);
    state(&daemon, &pane, &id, "queued");
}

/// Scenario: Queue one task before any status is reported, then report each busy or dialog status from each supported agent type. The pane must receive nothing until an Idle hook permits the token-led doorbell.
#[spec("inbox/doorbell/002")]
#[test]
fn doorbell_002_unknown_dialogs_and_busy_states_never_ring() {
    let daemon = daemon_unbound(&[]);
    for agent_type in ["claude_code", "open_code", "pi", "codex", "devin"] {
        let pane = pane(&daemon, agent_type);
        let id = enqueue(&daemon, &pane, BODY);
        unchanged(&pane, Duration::from_millis(350)); // Unknown: no hook yet.
        assert!(
            bytes(&pane).is_empty(),
            "Unknown {agent_type} received input"
        );
        for event in [
            "waiting_for_input",
            "permission_request",
            "thinking",
            "tool_start",
            "compacting",
            "quota_blocked",
        ] {
            status(&daemon, &pane, event, agent_type);
            unchanged(&pane, Duration::from_millis(350));
            assert!(
                bytes(&pane).is_empty(),
                "{event}/{agent_type} must suppress doorbell"
            );
            state(&daemon, &pane, &id, "queued");
        }
        // Genuine work clears the sticky Blocked status before Idle.
        status(&daemon, &pane, "thinking", agent_type);
        status(&daemon, &pane, "idle", agent_type);
        ring(&daemon, &pane, &id);
        fetch(&daemon, &pane, true);
    }
}

/// Scenario: Hold a native Stream waiter connected with its hand-over paused and a task still Queued. No doorbell may type even after native grace expires; dropping the waiter through EOF must release the token-led line for that same Queued task.
#[spec("inbox/doorbell/003")]
#[test]
fn doorbell_003_armed_waiter_suppresses_ring_until_eof() {
    // Test-only seam: preserve registration and EOF processing, but do not
    // claim/deliver Queued work through waiters. Without it a live waiter
    // immediately makes the message Fetched, which cannot prove the armed
    // guard independently of the Queued guard.
    let daemon = daemon_unbound(&[("DOT_AGENT_DECK_INBOX_TEST_PAUSE_WAITER_DELIVERY", "1")]);
    let pane = pane(&daemon, "armed");
    status(&daemon, &pane, "idle", "pi");
    let wait = waiter(&daemon, &pane);
    armed(&daemon, &pane, 1);
    let id = enqueue(&daemon, &pane, BODY);
    state(&daemon, &pane, &id, "queued");
    unchanged(&pane, Duration::from_millis(1200));
    assert!(
        bytes(&pane).is_empty(),
        "armed native waiter must suppress a Queued task's ring after grace"
    );
    drop(wait);
    armed(&daemon, &pane, 0);
    state(&daemon, &pane, &id, "queued");
    ring(&daemon, &pane, &id);
}

/// Scenario: Announce a Native component without arming a waiter and queue a task for an Idle pane. The token-led line must remain absent during arm_grace, then ring if the component has still not armed; a TurnEnd announce gets no native grace.
#[spec("inbox/doorbell/004")]
#[test]
fn doorbell_004_native_announce_gets_arm_grace() {
    // Keep the TurnEnd observation window shorter than Native grace even
    // when the harness widens process-scheduling budgets under host load.
    let grace = common::load_scaled(Duration::from_millis(1200));
    let grace_ms = grace.as_millis().to_string();
    let daemon = daemon_unbound(&[("DOT_AGENT_DECK_INBOX_ARM_GRACE_MS", &grace_ms)]);
    let pane = pane(&daemon, "native-grace");
    status(&daemon, &pane, "idle", "claude_code");
    announce(&daemon, &pane, "native");
    let started = std::time::Instant::now();
    let id = enqueue(&daemon, &pane, BODY);
    unchanged(&pane, Duration::from_millis(500));
    assert!(
        bytes(&pane).is_empty(),
        "Native announce must allow time to arm"
    );
    ring(&daemon, &pane, &id);
    assert!(
        started.elapsed() >= grace - Duration::from_millis(100),
        "native grace expired too early"
    );

    let turn_end = inbox::pane(&daemon, "turn-end");
    status(&daemon, &turn_end, "idle", "codex");
    announce(&daemon, &turn_end, "turn_end");
    let id = enqueue(&daemon, &turn_end, BODY);
    assert!(
        common::wait_until(common::load_scaled(Duration::from_millis(1000)), || {
            !rings(&turn_end).is_empty()
        }),
        "TurnEnd is unarmed and must not inherit Native grace"
    );
    ring(&daemon, &turn_end, &id);
}

/// Scenario: Leave one Idle pane's task Queued across five accelerated ring intervals, then fetch it without ack, the fetch's receiver kept open. Rings must follow the configured increasing delays and final repeated delay; Fetched and later Acknowledged stop further rings.
#[spec("inbox/doorbell/005")]
#[test]
fn doorbell_005_rerings_on_schedule_and_stops_on_fetch() {
    let scratch = common::harness_tempdir().unwrap();
    let log = scratch.path().join("doorbell.log");
    let daemon = daemon_unbound(&[
        ("DOT_AGENT_DECK_LOG", log.to_str().unwrap()),
        ("DOT_AGENT_DECK_DOORBELL_SCHEDULE_MS", "300,500,700,900"),
        ("DOT_AGENT_DECK_INBOX_ACK_WAIT_MS", "30000"),
    ]);
    let pane = pane(&daemon, "schedule");
    status(&daemon, &pane, "idle", "codex");
    let id = enqueue(&daemon, &pane, BODY);
    ring(&daemon, &pane, &id);
    assert!(
        common::wait_until(WAIT, || rings(&pane).len() >= 6),
        "Queued task must re-ring through final repeated interval: {:?}",
        rings(&pane)
    );
    // A starved stand-in can read several completed rings in one os.read.
    // Its read timestamps measure scheduling, not the daemon's ring policy.
    // Match six submitted lines to six daemon completion boundaries instead.
    assert!(common::wait_until(WAIT, || {
        ring_times(&log, &pane).len() >= 6
    }));
    let observed = ring_times(&log, &pane);
    for (pair, delay) in observed.windows(2).take(5).zip([0.3, 0.5, 0.7, 0.9, 0.9]) {
        let elapsed = (pair[1] - pair[0]).num_microseconds().unwrap() as f64 / 1_000_000.0;
        // 150ms pre-ring flush/submit can add delay; a lower bound pins the
        // policy without treating a loaded machine as a functional failure.
        assert!(
            elapsed >= delay - 0.05,
            "ring interval {elapsed}s shorter than {delay}s"
        );
    }
    state(&daemon, &pane, &id, "queued");
    let (held, _) = take(&daemon, &pane);
    state(&daemon, &pane, &id, "fetched");
    let count = rings(&pane).len();
    assert!(
        !common::wait_until(Duration::from_millis(1800), || rings(&pane).len() != count),
        "Fetched message must stop ring schedule"
    );
    let mut ack = request(&pane, "ack");
    ack["delivery_id"] = json!(id);
    hook(&daemon, ack);
    state(&daemon, &pane, &id, "acknowledged");
    close(held);
    unchanged(&pane, Duration::from_millis(1100));
}

fn ring_times(path: &std::path::Path, pane: &inbox::Pane) -> Vec<chrono::DateTime<chrono::Utc>> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| {
            line.contains("doorbell rang") && line.contains(&format!("agent_id={}", pane.agent))
        })
        .map(|line| {
            line.split_whitespace()
                .next()
                .unwrap()
                .parse()
                .expect("daemon ring log RFC3339 timestamp")
        })
        .collect()
}

/// Scenario: Queue three tasks while the pane is busy, then report Idle. One token-led line must cover the batch, carry a queued id, and a single fetch must return and acknowledge all three bodies.
#[spec("inbox/doorbell/006")]
#[test]
fn doorbell_006_one_ring_coalesces_all_queued_messages() {
    let daemon = daemon_unbound(&[]);
    let pane = pane(&daemon, "coalesced");
    status(&daemon, &pane, "thinking", "codex");
    let ids: Vec<_> = (0..3)
        .map(|n| enqueue(&daemon, &pane, &format!("{BODY}-{n}")))
        .collect();
    unchanged(&pane, Duration::from_millis(300));
    assert!(bytes(&pane).is_empty());
    status(&daemon, &pane, "idle", "codex");
    assert!(
        common::wait_until(WAIT, || !rings(&pane).is_empty()),
        "batch never rang"
    );
    let observed = rings(&pane);
    assert_eq!(observed.len(), 1, "one owed ring covers the whole batch");
    let ringing_id = ids
        .iter()
        .find(|id| is_ring_line(observed[0]["line"].as_str().unwrap(), id))
        .expect("ring must name a queued id");
    ring(&daemon, &pane, ringing_id);
    let fetched = fetch(&daemon, &pane, true).to_string();
    for (n, id) in ids.iter().enumerate() {
        assert!(
            fetched.contains(id) && fetched.contains(&format!("{BODY}-{n}")),
            "missing batch member: {fetched}"
        );
        state(&daemon, &pane, id, "acknowledged");
    }
    assert!(!common::wait_until(Duration::from_millis(500), || rings(
        &pane
    )
    .len()
        != 1));
    assert!(!screen(&daemon, &pane).contains(BODY));
}

/// Scenario: Let each supported synthetic agent leave a doorbell outstanding until the next scheduled ring, then have another consumer fetch the task. The pane must receive exactly two token-led lines with different tokens, each followed by one CR, and nothing after the fetch: no bare CR before a ring and none to clean up after one.
#[spec("inbox/doorbell/007")]
#[test]
fn doorbell_007_outstanding_ring_rerings_with_a_fresh_token_and_no_bare_cr() {
    let daemon = daemon_unbound(&[("DOT_AGENT_DECK_DOORBELL_SCHEDULE_MS", "1500")]);
    for agent_type in ["claude_code", "open_code", "pi", "codex", "devin"] {
        let pane = pane(&daemon, agent_type);
        status(&daemon, &pane, "idle", agent_type);
        let id = enqueue(&daemon, &pane, BODY);
        ring(&daemon, &pane, &id);
        assert!(
            common::wait_until(WAIT, || rings(&pane).len() >= 2),
            "second ring never submitted"
        );
        let lines = ring_lines(&pane);
        assert!(
            lines.iter().all(|line| is_ring_line(line, &id)),
            "{agent_type}: {lines:?}"
        );
        assert_ne!(
            lines[0], lines[1],
            "{agent_type}: each ring has its own token"
        );
        let expected = format!("{}\r{}\r", lines[0], lines[1]);
        assert_eq!(
            String::from_utf8_lossy(&bytes(&pane)),
            expected,
            "{agent_type}: two lines, one CR each"
        );
        fetch(&daemon, &pane, true);
        state(&daemon, &pane, &id, "acknowledged");
        unchanged(&pane, Duration::from_millis(1800));
        assert_eq!(rings(&pane).len(), 2);
        assert_eq!(
            String::from_utf8_lossy(&bytes(&pane)),
            expected,
            "{agent_type}: nothing to clean up after a submitted ring"
        );
        assert!(
            turns(&pane).iter().all(|turn| turn["line"] != ""),
            "{agent_type}: no empty composer was submitted"
        );
    }
}

/// The agent's `wake_hold`, as the daemon's agent records show it.
fn wake_hold(daemon: &impl Endpoint, pane: &inbox::Pane) -> serde_json::Value {
    attach(daemon, json!({"op":"list-agents"}))["agent_records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["id"] == pane.agent)
        .map(|record| record["inbox"]["wake_hold"].clone())
        .unwrap_or_default()
}

/// Wait for the doorbell's line and its Enter in `pane`, and answer the line.
fn line_and_enter(pane: &inbox::Pane, id: &str) -> String {
    let typed = || String::from_utf8_lossy(&bytes(pane)).into_owned();
    assert!(
        common::wait_until(WAIT, || typed()
            .strip_suffix('\r')
            .is_some_and(|line| is_ring_line(line, id))),
        "the doorbell must type its line and one Enter: {:?}",
        typed()
    );
    typed().trim_end_matches('\r').to_string()
}

/// Scenario: An Idle stand-in does not take the deck's Enter, its composer keeping the line. After its submit window the deck deletes exactly the line it typed and sends no second Enter, nothing is submitted, the task stays queued, no further ring follows in the episode, and the inbox names not_taken.
#[spec("inbox/doorbell/042")]
#[test]
fn doorbell_042_an_enter_the_agent_did_not_take_is_taken_back_and_named() {
    let daemon = daemon_unbound(&[
        ("DOT_AGENT_DECK_INBOX_WAKE_STALL_AFTER_MS", "300"),
        ("DOT_AGENT_DECK_DOORBELL_SCHEDULE_MS", "500"),
    ]);
    let pane = pane(&daemon, "dropped-enter");
    std::fs::write(pane.capture.with_extension("drop-enter"), "").unwrap();
    status(&daemon, &pane, "idle", "claude_code");
    let id = enqueue(&daemon, &pane, BODY);
    let line = line_and_enter(&pane, &id);
    let expected = format!("{line}\r{}", "\x7f".repeat(line.chars().count()));
    assert!(
        common::wait_until(WAIT, || bytes(&pane) == expected.as_bytes()),
        "the deck must delete exactly its line: {:?}",
        String::from_utf8_lossy(&bytes(&pane))
    );
    assert!(
        common::wait_until(WAIT, || wake_hold(&daemon, &pane)
            .to_string()
            .contains("not_taken")),
        "the inbox names not_taken: {}",
        wake_hold(&daemon, &pane)
    );
    unchanged(&pane, Duration::from_millis(1500));
    assert!(turns(&pane).is_empty(), "nothing was submitted");
    state(&daemon, &pane, &id, "queued");
}

/// Scenario: An Idle stand-in does not take the deck's Enter, and the user types a draft while the deck waits for the submit. The deck takes back exactly the line it typed first, and only then does the user's draft reach the pane, exactly as typed: one Enter in all, and no submission of the line and the draft together.
#[spec("inbox/doorbell/042")]
#[test]
fn doorbell_042_the_users_draft_waits_for_the_take_back() {
    let daemon = daemon_unbound(&[]);
    let pane = pane(&daemon, "dropped-enter-draft");
    std::fs::write(pane.capture.with_extension("drop-enter"), "").unwrap();
    status(&daemon, &pane, "idle", "claude_code");
    let mut user = common::AttachInput::attach(&daemon.attach_socket, &pane.agent);
    let id = enqueue(&daemon, &pane, BODY);
    let line = line_and_enter(&pane, &id);
    user.write(DRAFT);
    let expected = format!("{line}\r{}{DRAFT}", "\x7f".repeat(line.chars().count()));
    assert!(
        common::wait_until(WAIT, || bytes(&pane) == expected.as_bytes()),
        "the take-back must come before the user's draft: {:?}",
        String::from_utf8_lossy(&bytes(&pane))
    );
    assert!(
        common::wait_until(WAIT, || screen(&daemon, &pane)
            .contains(&format!("COMPOSER: {DRAFT}"))),
        "the user's draft sits in the composer alone"
    );
    unchanged(&pane, Duration::from_millis(800));
    assert!(turns(&pane).is_empty(), "nothing was submitted");
}

/// Scenario: Hold a task behind an old agent's human draft, stop it, and spawn a new agent in the same pane identity, on both one-worker and four-worker daemons. A predecessor-session ShellIdle must not type into the replacement at completed policy ages of 5 seconds, 10 seconds and up to one day, nor during the real observation window; its own Idle then submits only the newly bound task, which can be fetched.
#[spec("inbox/doorbell/008")]
#[test]
fn doorbell_008_respawn_never_rings_old_agents_message() {
    for workers in ["1", "4"] {
        assert_respawn_waits_for_ready(workers);
    }
}

fn assert_respawn_waits_for_ready(workers: &str) {
    use std::os::unix::fs::PermissionsExt;

    let daemon = daemon_unbound(&[
        ("DOT_AGENT_DECK_TEST_DOORBELL_PAUSE_DIR", "ring-barriers"),
        ("TOKIO_WORKER_THREADS", workers),
    ]);
    // Prepare this fixture's control directory before the first human-input
    // observation. A daemon-side mkdir can race its socket-bind umask and
    // leave the directory without the owner's search bit.
    let controls = daemon.home.join("ring-barriers");
    std::fs::create_dir_all(&controls).unwrap();
    std::fs::set_permissions(&controls, std::fs::Permissions::from_mode(0o700)).unwrap();
    let old = pane(&daemon, "respawn");
    status(&daemon, &old, "idle", "claude_code");
    let user = input(&daemon, &old, DRAFT);
    let old_id = enqueue(&daemon, &old, BODY);
    unchanged(&old, Duration::from_millis(350));
    let reply = attach(&daemon, json!({"op":"stop-agent","id":old.agent}));
    assert_eq!(reply["ok"], true, "stop old agent: {reply}");
    drop(user);
    for path in [
        &old.capture,
        &old.capture.with_extension("turns"),
        &old.capture.with_extension("pid"),
    ] {
        std::fs::remove_file(path).unwrap();
    }
    let new = pane(&daemon, "respawn");
    assert_ne!(old.agent, new.agent);
    let id = enqueue(&daemon, &new, BODY);
    // The monitor's first idle sample uses the retained old hook session id
    // but the sampled NEW agent id. Match its marker-less ShellIdle carrier;
    // the token merely admits this synthetic frame through the hook socket.
    let event = json!({"session_id":format!("doorbell-{}", old.agent),
        "pane_id":new.id,"agent_id":new.agent,"token":new.token,
        "agent_type":"none","event_type":"shell_idle",
        "timestamp":chrono::Utc::now(),"metadata":{}});
    let mut socket = connect(&daemon, &event).into_inner();
    socket.shutdown(std::net::Shutdown::Write).unwrap();
    std::io::Read::read_to_end(&mut socket, &mut Vec::new()).unwrap();
    for age_ms in [5_000, 10_000, 86_399_000, 86_400_000] {
        clock_step(&daemon, &new, age_ms);
    }
    std::fs::remove_file(daemon.home.join("ring-barriers/clock-ms")).unwrap();
    assert!(
        !common::wait_until(Duration::from_millis(6500), || !bytes(&new).is_empty()),
        "replacement received automatic input before current-generation readiness: {:?}; grid={}",
        String::from_utf8_lossy(&bytes(&new)),
        screen(&daemon, &new)
    );
    state(&daemon, &new, &id, "queued");
    status(&daemon, &new, "idle", "claude_code");
    assert!(
        common::wait_until(Duration::from_secs(1), || !rings(&new).is_empty()),
        "own ready must promptly submit replacement's doorbell line"
    );
    ring(&daemon, &new, &id);
    assert!(
        !String::from_utf8_lossy(&bytes(&new)).contains(&old_id),
        "old message rang in replacement"
    );
    let fetched = fetch(&daemon, &new, true).to_string();
    assert!(fetched.contains(&id) && fetched.contains(BODY), "{fetched}");
}

/// Scenario: Queue a task for a synthetic pane with a bare hydrated placeholder at the oldest representable startup, or its own native SessionStart aged to one day and the oldest representable startup in both session and process records. Each pane must receive zero automatic bytes until its own Idle hook permits the token-led line and an in-pane fetch.
#[spec("inbox/doorbell/008")]
#[test]
fn doorbell_008_placeholder_alone_does_not_establish_readiness() {
    for (session_start, old) in [
        (false, chrono::DateTime::<chrono::Utc>::MIN_UTC),
        (true, chrono::Utc::now() - chrono::Duration::days(1)),
        (true, chrono::DateTime::<chrono::Utc>::MIN_UTC),
    ] {
        assert_aged_generation_waits_for_ready(session_start, old);
    }
}

fn assert_aged_generation_waits_for_ready(session_start: bool, old: chrono::DateTime<chrono::Utc>) {
    struct Fixture {
        daemon: common::InProcDaemon,
        root: tempfile::TempDir,
    }
    impl Endpoint for Fixture {
        fn home(&self) -> &std::path::Path {
            self.root.path()
        }
        fn cwd(&self) -> &std::path::Path {
            self.root.path()
        }
        fn attach_socket(&self) -> &std::path::Path {
            &self.daemon.attach_path
        }
        fn hook_socket(&self) -> &std::path::Path {
            &self.daemon.hook_path
        }
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let fixture = Fixture {
        daemon: runtime.block_on(common::spawn_inprocess_daemon()),
        root: common::harness_tempdir().unwrap(),
    };
    let pane = route::start(&fixture, "placeholder-readiness", json!({}));
    if session_start {
        // Started, its prompt not drawn yet: SessionStart alone is not the
        // readiness a ring needs.
        route::hold_ready(&fixture, true);
        assert!(
            common::wait_until(WAIT, || screen(&fixture, &pane)
                .contains("DOORBELL-STANDIN-BOOTING")),
            "the stand-in must draw no prompt"
        );
        status(&fixture, &pane, "session_start", "claude_code");
    }
    runtime.block_on(async {
        let mut state = fixture.daemon.state.write().await;
        if !session_start {
            state.register_pane(pane.id.clone());
            state.insert_placeholder_session(pane.id.clone(), None, None, Some(pane.agent.clone()));
        }
        // Age the data the real admission and its independent writer recheck
        // read, rather than only the scheduler's injected decision clock.
        let mut aged = 0;
        for session in state.sessions.values_mut() {
            if session.agent_id.as_deref() == Some(pane.agent.as_str()) {
                session.started_at = old;
                session.last_activity = old;
                for event in &mut session.recent_events {
                    event.timestamp = old;
                }
                aged += 1;
            }
        }
        assert_eq!(
            aged, 1,
            "the actual generation must be aged (SessionStart={session_start})"
        );
        fixture
            .daemon
            .registry
            .set_spawned_at_for_test(&pane.agent, old);
        assert!(
            fixture
                .daemon
                .registry
                .agent_records()
                .iter()
                .any(|record| {
                    record.id == pane.agent && record.spawned_at_ms == Some(old.timestamp_millis())
                }),
            "the independent process-start source must also be aged"
        );
    });
    let id = enqueue(&fixture, &pane, BODY);
    assert!(
        !common::wait_until(Duration::from_millis(6500), || !bytes(&pane).is_empty()),
        "aged generation received automatic input before current-generation readiness (SessionStart={session_start}, started_at={old}): {:?}; grid={}",
        String::from_utf8_lossy(&bytes(&pane)),
        screen(&fixture, &pane)
    );
    state(&fixture, &pane, &id, "queued");
    route::fetch_enable(&pane);
    if session_start {
        route::hold_ready(&fixture, false);
    }
    status(&fixture, &pane, "idle", "claude_code");
    assert!(
        common::wait_until(Duration::from_secs(1), || bytes(&pane).ends_with(b"\r")),
        "own ready must promptly submit placeholder's doorbell line"
    );
    // Fetch from the fixture agent itself: this in-process daemon deliberately
    // keeps real component ancestry admission, unlike daemon_unbound fixtures.
    let fetched = route::fetched(&fixture, &pane, &id, BODY, "");
    assert!(fetched.contains(&id) && fetched.contains(BODY), "{fetched}");
}

/// Scenario: Pause an outstanding task's retry after decision and acknowledge that task while the agent remains Idle and another message stays queued. Releasing the attempt must add no further bytes; an unchanged selected-message control still submits its retry.
#[spec("inbox/doorbell/031")]
#[test]
fn doorbell_031_ack_after_decision_cancels_stale_rering() {
    assert_ack_fences_rering("decided", Settle::Ack);
}

/// Scenario: Fetch the selected message without acknowledging it while its retry is paused after decision, then queue another task. The fetched ID must receive no further input while the queued control can progress.
#[spec("inbox/doorbell/031")]
#[test]
fn doorbell_031_fetch_after_decision_cancels_stale_rering() {
    assert_ack_fences_rering("decided", Settle::Fetch);
}

/// Scenario: Pause a retry after its token-led line reaches the pane, with the writer held and before its Enter, then acknowledge the selected task and leave another message queued. Releasing it must delete exactly the line it typed and send no Enter; an unchanged selected-message control still submits its line.
#[spec("inbox/doorbell/031")]
#[test]
fn doorbell_031_ack_after_the_line_takes_it_back() {
    assert_ack_fences_rering("after-line", Settle::Ack);
}

/// Scenario: A human input frame arrives while a retry is paused after decision, before writer admission. The queued task must remain deferred with the draft unsent; the unchanged-message control still completes its retry.
#[spec("inbox/doorbell/031")]
#[test]
fn doorbell_031_human_draft_after_decision_withholds_retry() {
    assert_ack_fences_rering("decided", Settle::HumanDraft);
}

/// Scenario: After the retry's line reaches the pane, a human input frame arrives while the writer is still held. The deck must finish its own attempt first, its Enter and then the human's bytes exactly as typed, and submit nothing of the human's; the unchanged-message control still submits.
#[spec("inbox/doorbell/031")]
#[test]
fn doorbell_031_human_frame_after_the_line_waits_for_the_submit() {
    assert_ack_fences_rering("after-line", Settle::HumanDraft);
}

/// Scenario: Leave completion and writer-held barriers unreleased in separate fixtures until their test-only timeouts unwind the tasks. Each pane must then accept and submit another scheduled ring, proving recovery of its writer and logical busy slot.
#[spec("inbox/doorbell/031")]
#[test]
fn doorbell_031_timed_out_barrier_does_not_strand_slot() {
    for phase in ["completed", "admitted"] {
        let daemon = daemon_unbound(&[
            ("DOT_AGENT_DECK_DOORBELL_SCHEDULE_MS", "500"),
            ("DOT_AGENT_DECK_TEST_DOORBELL_PAUSE_PANE", "timeout-control"),
            ("DOT_AGENT_DECK_TEST_DOORBELL_PAUSE_DIR", "ring-barriers"),
        ]);
        let root = daemon.home.join("ring-barriers");
        std::fs::create_dir_all(&root).unwrap();
        let pane = pane(&daemon, "timeout-control");
        std::fs::write(root.join(format!("{phase}.armed")), "").unwrap();
        status(&daemon, &pane, "idle", "claude_code");
        let id = enqueue(&daemon, &pane, BODY);
        ring(&daemon, &pane, &id);
        assert!(
            common::wait_until(WAIT, || root.join(format!("{phase}.reached")).exists()),
            "timeout control must reach its seam"
        );
        assert!(
            common::wait_until(WAIT, || root.join(format!("{phase}.timed-out")).exists()),
            "unreleased seam must report its bounded timeout"
        );
        assert!(
            common::wait_until(WAIT, || rings(&pane).len() >= 2),
            "seam timeout stranded the doorbell's writer or busy slot"
        );
        fetch(&daemon, &pane, true);
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Settle {
    /// Fetch and acknowledge the selected message.
    Ack,
    /// Fetch it without acknowledging, the receiver kept open.
    Fetch,
    /// Leave it queued and type a human draft into the pane.
    HumanDraft,
}

fn assert_ack_fences_rering(phase: &str, settle: Settle) {
    // Run the queued control FIRST, so a RED below still proves the barrier
    // releases a real re-ring with the ordinary payload and submission.
    for intervene in [false, true] {
        let daemon = daemon_unbound(&[
            ("DOT_AGENT_DECK_DOORBELL_SCHEDULE_MS", "500"),
            ("DOT_AGENT_DECK_TEST_DOORBELL_PAUSE_PANE", "fenced"),
            ("DOT_AGENT_DECK_TEST_DOORBELL_PAUSE_DIR", "ring-barriers"),
            ("RUST_LOG", "dot_agent_deck::doorbell=debug"),
        ]);
        let root = daemon.home.join("ring-barriers");
        std::fs::create_dir_all(&root).unwrap();
        let pane = pane(&daemon, "fenced");
        // Attach before the writer is parked. The observation seam proves the
        // human frame arrived while its PTY write waits behind that writer.
        let mut user = common::AttachInput::attach(&daemon.attach_socket, &pane.agent);
        let _budget = InterleaveBudget::start(&root);
        std::fs::write(root.join(format!("{phase}.armed")), "").unwrap();
        status(&daemon, &pane, "idle", "claude_code");
        let id = enqueue(&daemon, &pane, BODY);
        ring(&daemon, &pane, &id);
        let first = format!("{}\r", rung_line(&pane, &id).unwrap());
        assert!(
            common::wait_until(wait_budget(), || root
                .join(format!("{phase}.reached"))
                .exists()),
            "re-ring must reach {phase} barrier"
        );
        // Parked: after decision nothing more is written; after the line, the
        // retry's own token-led line and no Enter.
        let retry_line = || {
            let text = String::from_utf8_lossy(&bytes(&pane)).into_owned();
            text.strip_prefix(&first)
                .filter(|rest| is_ring_line(rest, &id))
                .map(str::to_string)
        };
        let parked = if phase == "after-line" {
            assert!(
                common::wait_until(wait_budget(), || retry_line().is_some()),
                "{phase}: barrier must follow the retry's line: {:?}",
                String::from_utf8_lossy(&bytes(&pane))
            );
            retry_line().unwrap()
        } else {
            assert!(
                common::wait_until(wait_budget(), || bytes(&pane) == first.as_bytes()),
                "{phase}: barrier must precede the retry: {:?}",
                String::from_utf8_lossy(&bytes(&pane))
            );
            String::new()
        };
        let mut _held = None;
        if intervene && settle != Settle::HumanDraft {
            // Without an ack the fetch keeps its receiver, which holds the
            // message for it (PRD #1666 part 3).
            let fetched = if settle == Settle::Ack {
                fetch(&daemon, &pane, true)
            } else {
                let (socket, reply) = take(&daemon, &pane);
                _held = Some(socket);
                reply
            }
            .to_string();
            assert!(fetched.contains(&id) && fetched.contains(BODY), "{fetched}");
            state(
                &daemon,
                &pane,
                &id,
                if settle == Settle::Ack {
                    "acknowledged"
                } else {
                    "fetched"
                },
            );
        } else {
            state(&daemon, &pane, &id, "queued");
        }
        let other = enqueue(&daemon, &pane, "SECOND-QUEUED-MESSAGE");
        state(&daemon, &pane, &other, "queued");
        if intervene && settle == Settle::HumanDraft {
            user.write(DRAFT);
            assert!(
                common::wait_until(wait_budget(), || std::fs::read_to_string(
                    root.join("observations")
                )
                .unwrap_or_default()
                .contains("human-arrived fenced")),
                "human frame must arrive before releasing the writer"
            );
        }
        std::fs::write(root.join("completed.armed"), "").unwrap();
        std::fs::write(root.join(format!("{phase}.release")), "").unwrap();
        assert!(
            common::wait_until(wait_budget(), || root.join("completed.reached").exists()),
            "{phase}: attempt must complete within the bounded seam; timeout={}",
            root.join(format!("{phase}.timed-out")).exists()
        );
        drained(&pane);
        if !intervene {
            assert!(
                common::wait_until(wait_budget(), || rings(&pane).len() >= 2),
                "{phase}: still-queued control must submit its second ring"
            );
            let lines = ring_lines(&pane);
            assert!(is_ring_line(&lines[1], &id), "{lines:?}");
            assert_eq!(
                String::from_utf8_lossy(&bytes(&pane)),
                format!("{first}{}\r", lines[1])
            );
            fetch(&daemon, &pane, true);
            std::fs::write(root.join("completed.release"), "").unwrap();
            continue;
        }
        let expected = match (phase, settle) {
            // The line typed, then settled: deleted, no Enter.
            ("after-line", Settle::Ack | Settle::Fetch) => {
                format!("{first}{parked}{}", "\x7f".repeat(parked.chars().count()))
            }
            // The human's frame waits behind the deck's own Enter.
            ("after-line", Settle::HumanDraft) => format!("{first}{parked}\r{DRAFT}"),
            (_, Settle::HumanDraft) => format!("{first}{DRAFT}"),
            _ => first.clone(),
        };
        assert!(
            common::wait_until(wait_budget(), || bytes(&pane) == expected.as_bytes()),
            "{phase}: expected {expected:?}, received {:?}",
            String::from_utf8_lossy(&bytes(&pane))
        );
        let rang = if (phase, settle) == ("after-line", Settle::HumanDraft) {
            2
        } else {
            1
        };
        assert_eq!(rings(&pane).len(), rang, "{phase}: submitted rings");
        std::fs::write(root.join("completed.release"), "").unwrap();
        if settle == Settle::HumanDraft {
            // The draft stays in the composer, so nothing else is typed.
            unchanged(&pane, Duration::from_millis(600));
        } else {
            ring(&daemon, &pane, &other);
            fetch(&daemon, &pane, true);
        }
    }
}

/// Scenario: Type a worker draft, queue a task, and keep typing while delivery waits. Enter must submit the human draft separately, after which exactly the token-led doorbell arrives without blocking the user's input.
#[spec("scheduler/dispatch/023")]
#[test]
fn dispatch_023_doorbell_waits_for_unsent_worker_draft() {
    let daemon = daemon_unbound(&[]);
    let pane = pane(&daemon, "worker-draft");
    status(&daemon, &pane, "idle", "codex");
    let mut user = input(&daemon, &pane, DRAFT);
    let id = enqueue(&daemon, &pane, BODY);
    unchanged(&pane, Duration::from_millis(500));
    user.write("-still-typing");
    assert!(
        common::wait_until(WAIT, || screen(&daemon, &pane)
            .contains(&format!("{DRAFT}-still-typing"))),
        "waiting automatic writer blocked user keystrokes"
    );
    unchanged(&pane, Duration::from_millis(300));
    assert!(rings(&pane).is_empty());
    user.write("\r");
    ring(&daemon, &pane, &id);
    assert_eq!(
        bytes(&pane),
        format!("{DRAFT}-still-typing\r{}\r", rung_line(&pane, &id).unwrap()).as_bytes(),
        "draft and doorbell must be separate submitted lines"
    );
}

/// Scenario: Forward only terminal focus, mouse and query-response bytes into an Idle worker pane, then queue a task. These terminal reports must not count as a human draft, so the token-led doorbell reaches the pane without Enter.
#[spec("scheduler/dispatch/023")]
#[test]
fn dispatch_023_terminal_reports_do_not_hold_doorbell() {
    let daemon = daemon_unbound(&[]);
    let pane = pane(&daemon, "terminal-reports");
    status(&daemon, &pane, "idle", "codex");
    let reports =
        "\x1b[<64;10;5M\x1b[I\x1b[10;5R\x1b[?1;2c\x1b]10;rgb:ffff/ffff/ffff\x07\x1bP1$r0m\x1b\\";
    let _user = input(&daemon, &pane, reports);
    let id = enqueue(&daemon, &pane, BODY);
    let expected = || rung_line(&pane, &id).map(|line| format!("{reports}{line}\r"));
    assert!(
        common::wait_until(WAIT, || expected()
            .is_some_and(|expected| bytes(&pane) == expected.as_bytes())),
        "terminal reports must not defer the doorbell: {:?}",
        bytes(&pane)
    );
    assert!(screen(&daemon, &pane).contains("waiting, run: dot-agent-deck inbox fetch"));
}

/// Scenario: Type and delete text in OpenCode and Pi panes with their native receipt streams registered, then queue a message. Each native stream must hand over its message without a probe or notice, and its card must not acquire a composer-check reason.
#[spec("inbox/probe/037")]
#[test]
fn probe_037_absent_opencode_and_pi_keep_native_delivery() {
    for (kind, component) in [("open_code", "opencode_plugin"), ("pi", "pi_extension")] {
        let daemon = inbox::daemon_unbound(&[]);
        let pane = inbox::pane_with(&daemon, kind, json!({"agent_type":kind,"profile":false}));
        inbox::status(&daemon, &pane, "idle", kind);
        let _human = inbox::input(&daemon, &pane, "x\x7f");
        let mut stream = inbox::receipt_stream_as(&daemon, &pane, component);
        let id = inbox::enqueue(&daemon, &pane, inbox::BODY);
        let message = inbox::receipt_handout(&mut stream, &id);
        assert_eq!(message["id"], id);
        assert!(message.to_string().contains(inbox::BODY));
        let records = inbox::attach(&daemon, json!({"op":"list-agents"}));
        let record = records["agent_records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == pane.agent)
            .unwrap();
        assert_eq!(record["inbox"]["wake"], "native", "{record}");
        assert!(record["inbox"]["wake_hold"].is_null(), "{record}");
        inbox::unchanged(&pane, Duration::from_millis(250));
        assert_eq!(inbox::bytes(&pane), b"x\x7f");
    }
}

/// Scenario: Announce Devin's ordinary turn-end receipt route without typing into its pane, queue mail, and fetch it through the ordinary CLI receipt path. Its existing cannot-type reason and prompt route remain available without a check or notice being typed.
#[spec("inbox/probe/037")]
#[test]
fn probe_037_absent_devin_keeps_its_prompt_fetch_route() {
    let daemon = inbox::daemon_unbound(&[("DOT_AGENT_DECK_INBOX_WAKE_STALL_AFTER_MS", "100")]);
    let pane = inbox::pane_with(
        &daemon,
        "devin-absent",
        json!({"agent_type":"devin","profile":false}),
    );
    inbox::status(&daemon, &pane, "idle", "devin");
    let mut announcement = inbox::request(&pane, "inbox_announce");
    announcement["component"] = json!("devin_hooks");
    announcement["component_version"] = json!("synthetic-absent");
    announcement["wake"] = json!("turn_end");
    announcement["pid"] = json!(pane.pid);
    assert_eq!(inbox::hook(&daemon, announcement)["accepted"], true);
    let id = inbox::enqueue(&daemon, &pane, inbox::BODY);
    assert!(
        common::wait_until(WAIT, || {
            let records = inbox::attach(&daemon, json!({"op":"list-agents"}));
            records["agent_records"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["id"] == pane.agent && r["inbox"]["wake_hold"] == "cannot_type")
        }),
        "Devin's untouched prompt route must retain cannot_type: {}",
        inbox::attach(&daemon, json!({"op":"list-agents"}))
    );
    let records = inbox::attach(&daemon, json!({"op":"list-agents"}));
    let record = records["agent_records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == pane.agent)
        .unwrap();
    assert_eq!(record["inbox"]["wake"], "turn_end", "{record}");
    assert!(
        !matches!(
            record["inbox"]["wake_hold"].as_str(),
            Some("needs_fetch" | "cannot_check")
        ),
        "{record}"
    );
    assert!(inbox::fetch(&daemon, &pane, true).to_string().contains(&id));
    assert_eq!(inbox::bytes(&pane), b"");
}

/// Scenario: Interrupt working OpenCode and Pi panes with two Escapes, and separately press Escapes while they are already Idle and repeat their Idle reports. Their registered native streams still deliver queued mail once without a typed fallback.
#[spec("inbox/doorbell/019")]
#[test]
fn doorbell_019_native_opencode_and_pi_deliver_after_escapes() {
    for (kind, component) in [("open_code", "opencode_plugin"), ("pi", "pi_extension")] {
        for already_idle in [false, true] {
            let daemon = inbox::daemon_unbound(&[]);
            let pane = inbox::pane_with(&daemon, kind, json!({"agent_type":kind,"profile":false}));
            inbox::status(
                &daemon,
                &pane,
                if already_idle { "idle" } else { "thinking" },
                kind,
            );
            let mut human = inbox::input(&daemon, &pane, "\x1b");
            inbox::unchanged(&pane, Duration::from_millis(300));
            human.write("\x1bx\x7f");
            assert!(common::wait_until(WAIT, || inbox::bytes(&pane) == b"\x1b\x1bx\x7f"));
            inbox::status(&daemon, &pane, "idle", kind);
            inbox::status(&daemon, &pane, "idle", kind);
            let mut stream = inbox::receipt_stream_as(&daemon, &pane, component);
            let id = inbox::enqueue(&daemon, &pane, inbox::BODY);
            let delivered = inbox::receipt_handout(&mut stream, &id);
            assert_eq!(delivered["id"], id);
            assert!(delivered.to_string().contains(inbox::BODY));
            inbox::unchanged(&pane, Duration::from_millis(250));
            assert_eq!(inbox::bytes(&pane), b"\x1b\x1bx\x7f");
        }
    }
}
