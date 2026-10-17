#![cfg(all(feature = "e2e", unix))]

mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;

use inbox::*;
use serde_json::json;
use spec::spec;
use std::time::Duration;

/// Scenario: Queue a task for an Idle synthetic agent with no armed component. Its pane must show exactly the fixed doorbell and a submission, while the task body remains only in the inbox; an empty inbox never rings.
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
        format!("{}\r", fixed_line(&id)).as_bytes(),
        "exact fixed line, no body, paste markers, LF or extra submits"
    );
    assert_eq!(rings(&pane).len(), 1);
    state(&daemon, &pane, &id, "queued");
}

/// Scenario: Queue one task before any status is reported, then report each busy or dialog status from each supported agent type. The pane must receive nothing until an Idle hook permits the fixed doorbell.
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

/// Scenario: Hold a native Stream waiter connected with its hand-over paused and a task still Queued. No doorbell may type even after native grace expires; dropping the waiter through EOF must release the fixed line for that same Queued task.
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

/// Scenario: Announce a Native component without arming a waiter and queue a task for an Idle pane. The fixed line must remain absent during arm_grace, then ring if the component has still not armed; a TurnEnd announce gets no native grace.
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

/// Scenario: Leave one Idle pane's task Queued across five accelerated ring intervals, then fetch it without ack. Rings must follow the configured increasing delays and final repeated delay; Fetched and later Acknowledged stop further rings.
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
    fetch(&daemon, &pane, false);
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

/// Scenario: Queue three tasks while the pane is busy, then report Idle. One fixed line must cover the batch, carry a queued id, and a single fetch must return and acknowledge all three bodies.
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
        .find(|id| observed[0]["line"] == fixed_line(id))
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

/// Scenario: Let each supported synthetic agent leave a doorbell outstanding until the next scheduled ring. Its exact input must contain a bare CR before the second fixed line, and one cleanup CR after another consumer fetches the task; neither CR creates a new message line.
#[spec("inbox/doorbell/007")]
#[test]
fn doorbell_007_outstanding_ring_gets_pre_ring_and_cleanup_cr() {
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
        let line = fixed_line(&id);
        assert_eq!(
            bytes(&pane),
            format!("{line}\r\r{line}\r").as_bytes(),
            "{agent_type}: pre-ring must be exactly one bare CR"
        );
        fetch(&daemon, &pane, true);
        state(&daemon, &pane, &id, "acknowledged");
        let cleaned = format!("{line}\r\r{line}\r\r");
        assert!(
            common::wait_until(WAIT, || bytes(&pane) == cleaned.as_bytes()),
            "{agent_type}: outstanding ring needs one cleanup CR; received={:?}",
            bytes(&pane)
        );
        unchanged(&pane, Duration::from_millis(1800));
        assert_eq!(rings(&pane).len(), 2);
        assert_eq!(
            turns(&pane)
                .iter()
                .filter(|turn| turn["line"] == "")
                .count(),
            2,
            "pre-ring and cleanup CRs submit only empty composers"
        );
    }
}

/// Scenario: Hold a task behind an old agent's human draft, stop it, and spawn a new agent in the same pane identity. Reporting the replacement Idle must not ring the old task; a newly bound task must still ring normally.
#[spec("inbox/doorbell/008")]
#[test]
fn doorbell_008_respawn_never_rings_old_agents_message() {
    let daemon = daemon_unbound(&[]);
    let old = pane(&daemon, "respawn");
    status(&daemon, &old, "idle", "codex");
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
    status(&daemon, &new, "idle", "codex");
    unchanged(&new, Duration::from_millis(1200));
    assert!(
        bytes(&new).is_empty(),
        "stale agent binding reached replacement"
    );
    let id = enqueue(&daemon, &new, BODY);
    ring(&daemon, &new, &id);
    assert!(
        !screen(&daemon, &new).contains(&old_id),
        "old message rang in replacement"
    );
}

/// Scenario: Type a worker draft, queue a task, and keep typing while delivery waits. Enter must submit the human draft separately, after which exactly the fixed doorbell arrives without blocking the user's input.
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
        format!("{DRAFT}-still-typing\r{}\r", fixed_line(&id)).as_bytes(),
        "draft and doorbell must be separate submitted lines"
    );
}

/// Scenario: Forward only terminal focus, mouse and query-response bytes into an Idle worker pane, then queue a task. These terminal reports must not count as a human draft, so the fixed doorbell reaches the pane without Enter.
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
    let expected = format!("{reports}{}\r", fixed_line(&id));
    assert!(
        common::wait_until(WAIT, || bytes(&pane) == expected.as_bytes()),
        "terminal reports must not defer the doorbell: {:?}",
        bytes(&pane)
    );
    assert!(screen(&daemon, &pane).contains("waiting, run: dot-agent-deck inbox fetch"));
}

/// Scenario: For each supported synthetic agent type, report the worker Thinking, forward the interrupt OpenCode uses (Escape, a 300 ms pause, Escape) as the user's keys, then report it Idle and queue a task. An interrupt is not a draft, so the fixed doorbell must follow the two Escapes and a fetch must deliver the task.
#[spec("scheduler/dispatch/023")]
#[test]
fn dispatch_023_an_interrupts_escapes_do_not_hold_doorbell() {
    let daemon = daemon_unbound(&[]);
    for agent_type in ["open_code", "claude_code", "codex", "pi"] {
        let pane = pane(
            &daemon,
            &format!("interrupt-{}", agent_type.replace('_', "-")),
        );
        status(&daemon, &pane, "thinking", agent_type);
        let mut user = input(&daemon, &pane, "\x1b");
        // OpenCode's interrupt: the second Escape about 300 ms later.
        unchanged(&pane, Duration::from_millis(300));
        user.write("\x1b");
        assert!(
            common::wait_until(WAIT, || bytes(&pane) == b"\x1b\x1b"),
            "{agent_type}: interrupt not received"
        );
        status(&daemon, &pane, "idle", agent_type);
        let id = enqueue(&daemon, &pane, BODY);
        let expected = format!("\x1b\x1b{}\r", fixed_line(&id));
        assert!(
            common::wait_until(WAIT, || bytes(&pane) == expected.as_bytes()),
            "{agent_type}: an interrupt held the doorbell: {:?}",
            bytes(&pane)
        );
        let fetched = fetch(&daemon, &pane, true).to_string();
        assert!(
            fetched.contains(&id) && fetched.contains(BODY),
            "{agent_type}: the task was not delivered: {fetched}"
        );
        state(&daemon, &pane, &id, "acknowledged");
    }
}

/// Scenario: Leave a typed draft followed by two Escapes, two Escapes followed by typing, and two Escapes pressed on an agent that is already Idle (a menu, not an interrupt), then report Idle where the agent was busy and queue a task in each pane. None of these is an interrupt that emptied the box, so each pane must receive nothing and its task must stay Queued.
#[spec("scheduler/dispatch/023")]
#[test]
fn dispatch_023_drafts_and_escapes_on_an_idle_agent_still_hold_doorbell() {
    let daemon = daemon_unbound(&[]);
    for (name, while_busy, once_idle) in [
        ("typed-then-escapes", "unsent\x1b\x1b", ""),
        ("escapes-then-typed", "\x1b\x1bunsent", ""),
        ("escapes-on-idle", "", "\x1b\x1b"),
    ] {
        let pane = pane(&daemon, name);
        status(&daemon, &pane, "thinking", "open_code");
        let mut user = (!while_busy.is_empty()).then(|| input(&daemon, &pane, while_busy));
        status(&daemon, &pane, "idle", "open_code");
        if !once_idle.is_empty() {
            user = Some(input(&daemon, &pane, once_idle));
        }
        let before = bytes(&pane);
        let id = enqueue(&daemon, &pane, BODY);
        unchanged(&pane, Duration::from_millis(800));
        assert_eq!(bytes(&pane), before, "{name}: the doorbell typed");
        assert!(rings(&pane).is_empty(), "{name}: the doorbell rang");
        state(&daemon, &pane, &id, "queued");
        drop(user);
    }
}
