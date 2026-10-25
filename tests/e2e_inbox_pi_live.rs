#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]
//! Native inbox delivery through the bundled extension in real interactive Pi.
mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/pi_inbox_live.rs"]
mod live;

use live::{BUDGET, Options, Pi, WINDOW_MS};
use spec::spec;
use std::time::Duration;

/// Scenario: Leave real Pi idle in a PTY-attached deck, then delegate a sentinel task through a genuine orchestrator CLI. Pi must visibly act, label its card with the envelope subject, write the sentinel, and acknowledge the message it commits.
#[spec("inbox/pi/001")]
#[test]
fn pi_001_live_idle_delegate_wakes_and_acknowledges() {
    skip_unless!(live::available());
    let pi = Pi::new();
    pi.idle();
    let id = pi.delegate("pi-idle-36de27.txt");
    pi.done(&id, "pi-idle-36de27.txt");
    assert!(
        pi.deck
            .wait_for_grid_string_within("✉ task from the orchestrator for coder", BUDGET),
        "Pi card must label the native task; {}",
        pi.evidence()
    );
    assert!(
        pi.deck
            .wait_for_stream_string_within("pi-idle-36de27.txt", BUDGET),
        "real Pi work must be visible in its attached pane; {}",
        pi.evidence()
    );
    pi.idle();
    assert!(
        !pi.log().contains("doorbell rang"),
        "idle Pi must receive natively; {}",
        pi.evidence()
    );
}

/// Scenario: While real Pi runs a slow bash tool specified in a fenced command, post three inbox messages through the client attach API in quick succession. They must append A, B and C after the current tool, enter Pi once each in separate turns, and each become acknowledged.
#[spec("inbox/pi/002")]
#[test]
fn pi_002_live_streaming_messages_run_in_order_one_per_turn() {
    skip_unless!(live::available());
    let pi = Pi::new();
    pi.prompt(&format!(
        "Use bash to run this exact command:\n```sh\nprintf started > '{}/busy-start'; sleep 30; printf '0\\n' > '{}/order.txt'\n```\nAfter the command finishes, reply BUSY-DONE.",
        pi.work.display(), pi.work.display()
    ));
    common::wait_for_file_trimmed_eq(&pi.work.join("busy-start"), "started", BUDGET).unwrap();
    let ids=['A','B','C'].map(|letter|pi.post_body(&format!("queued task {letter}"), &format!(
        "Use bash to run this exact command:\n```sh\nprintf '{letter}\\n' >> '{}/order.txt'\n```\nAfter the command finishes, reply DONE-{letter} and end your turn.",pi.work.display()
    )));
    assert!(
        !pi.work.join("order.txt").exists(),
        "three sends must finish while the original tool is streaming"
    );
    common::wait_for_file_trimmed_eq(&pi.work.join("order.txt"), "0\nA\nB\nC", BUDGET)
        .unwrap_or_else(|why| panic!("queued tasks ran out of order: {why}; {}", pi.evidence()));
    for id in &ids {
        inbox::state(&pi.deck, &pi.pane, id, "acknowledged");
        pi.committed_once(id);
    }
    let events = pi.events();
    let positions = ids
        .iter()
        .map(|id| {
            events
                .iter()
                .position(|e| {
                    e["type"] == "message_start"
                        && e["event"]["message"]["role"] == "user"
                        && e["event"]
                            .to_string()
                            .contains(&format!("[dot-agent-deck message {id} "))
                })
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert!(
        positions.windows(2).all(|w| w[0] < w[1]),
        "committed inbox messages must retain send order: {positions:?}"
    );
    for pair in positions.windows(2) {
        assert!(
            events[pair[0] + 1..pair[1]]
                .iter()
                .any(|e| e["type"] == "message_start"
                    && e["event"]["message"]["role"] == "assistant"),
            "a real assistant turn must separate each committed user message: {:?}",
            &events[pair[0]..=pair[1]]
        );
    }
    pi.idle();
}

/// Scenario: Finish a native delegate in real Pi, invoke /new, and inspect only children of its captured process id. The one watcher Pi's process runs is the same process after /new, with no second registration, and the next delegate executes and is acknowledged in the new session.
#[spec("inbox/pi/003")]
#[test]
#[cfg(target_os = "linux")]
fn pi_003_live_new_session_keeps_one_watcher_and_receives_next_message() {
    skip_unless!(live::available());
    let pi = Pi::new();
    let id = pi.delegate("pi-before-new-75ab19.txt");
    pi.done(&id, "pi-before-new-75ab19.txt");
    pi.idle();
    let old_session = pi.session();
    let old_watchers = pi.watcher_pids();
    assert_eq!(old_watchers.len(), 1);
    pi.prompt("/new");
    assert!(
        common::wait_until(BUDGET, || pi.session() != old_session),
        "/new must start a new session; {}",
        pi.evidence()
    );
    // PRD #1666 §0: one receiver per Pi process, so /new keeps its watcher.
    assert_eq!(
        pi.watcher_pids(),
        old_watchers,
        "/new keeps the one watcher; {}",
        pi.evidence()
    );
    pi.registered();
    let new_session = pi.session();
    let id = pi.delegate("pi-after-new-18c2ba.txt");
    pi.done(&id, "pi-after-new-18c2ba.txt");
    assert!(
        pi.users()
            .iter()
            .any(|e| e["session"] == new_session && e["event"].to_string().contains(&id)),
        "next message must be committed in the new session"
    );
    assert_eq!(pi.watcher_pids().len(), 1);
}

/// Scenario: Start real interactive Pi and capture its process id plus its one real inbox watcher, excluding the passive pipe observer. SIGKILL only that Pi process; its watcher must exit on stdin EOF.
#[spec("inbox/pi/004")]
#[test]
#[cfg(target_os = "linux")]
fn pi_004_live_sigkill_exits_the_child_watcher_on_stdin_eof() {
    skip_unless!(live::available());
    let pi = Pi::new();
    let watchers = pi.watcher_pids();
    assert_eq!(watchers.len(), 1);
    assert_eq!(unsafe { libc::kill(pi.pid(), libc::SIGKILL) }, 0);
    assert!(
        common::wait_until(BUDGET, || !common::process_running(watchers[0])),
        "Pi's child watcher must exit on stdin EOF; captured watcher={watchers:?}"
    );
}

/// Scenario: Keep real interactive Pi in a running bash tool, then delegate a sentinel task while that tool is visible in the attached pane. The task must stay queued without a may-not-have-reached notice or automatic repeat, then run once after the tool is released and become extension-confirmed.
#[spec("inbox/pi/017")]
#[test]
fn pi_017_live_long_tool_holds_followup_without_repeat_or_notice() {
    skip_unless!(live::available());
    let pi = Pi::with_options(Options {
        short_windows: true,
        ..Options::default()
    });
    pi.hold_tool();
    let name = "held-followup-61ea37.txt";
    let id = pi.append_task(name);
    pi.wait_report("handed", &id);
    // Eight seconds from observed handed exceeds three 2s ack waits and two
    // 2s G3 windows. The real tool stays gated throughout this interval.
    pi.observe_interval(Duration::from_millis(4 * WINDOW_MS), || {
        assert_eq!(pi.message(&id)["deliveries"], 1);
        assert!(pi.reports("committed", &id).is_empty());
        assert!(!pi.work.join(name).exists());
        assert!(
            serde_json::from_value::<dot_agent_deck::inbox_hub::InboxSummary>(
                pi.record()["inbox"].clone()
            )
            .unwrap()
            .notices
            .attention
            .iter()
            .all(|item| item.kind != dot_agent_deck::inbox_display::AttentionKind::NotStarted),
            "no notice during a real busy turn; {}",
            pi.evidence()
        );
    });
    pi.release_tool();
    pi.done(&id, name);
    pi.wait_report("committed", &id);
    pi.assert_receipt_order(&id);
    let events = pi.events();
    let end = events
        .iter()
        .position(|e| e["type"] == "tool_execution_end")
        .unwrap();
    let opener = events
        .iter()
        .position(|e| {
            e["type"] == "message_start"
                && e["event"]["message"]["role"] == "user"
                && e["event"].to_string().contains(&id)
        })
        .unwrap();
    assert!(end < opener, "held tool finishes before follow-up opener");
}

/// Scenario: Hand a native follow-up to real Pi while its current bash tool is still running, then kill only that Pi process's captured inbox watcher. The replacement watcher must resume and retain the task, and the task must run once after release without a repeat.
#[spec("inbox/pi/019")]
#[test]
#[cfg(target_os = "linux")]
fn pi_019_live_killed_watcher_retains_handed_followup_until_commit() {
    skip_unless!(live::available());
    let pi = Pi::with_options(Options {
        short_windows: true,
        ..Options::default()
    });
    pi.hold_tool();
    let name = "watcher-retained-fb9832.txt";
    let id = pi.append_task(name);
    pi.wait_report("handed", &id);
    assert!(pi.reports("committed", &id).is_empty());
    let watchers = pi.watcher_pids();
    assert_eq!(watchers.len(), 1, "real watcher descendant: {watchers:?}");
    let victim = watchers[0];
    assert_ne!(victim, 137197);
    assert_ne!(victim, pi.pid());
    let command = std::fs::read(format!("/proc/{victim}/cmdline")).unwrap();
    std::fs::write(pi.diagnostics.join("killed-watcher.json"),serde_json::json!({"pi":pi.pid(),"watcher":victim,"argv":command.split(|b|*b==0).map(|v|String::from_utf8_lossy(v).to_string()).collect::<Vec<_>>(),"status":std::fs::read_to_string(format!("/proc/{victim}/status")).unwrap()}).to_string()).unwrap();
    assert_eq!(unsafe { libc::kill(victim, libc::SIGKILL) }, 0);
    assert!(
        common::wait_until(BUDGET, || pi
            .watcher_pids()
            .iter()
            .any(|pid| *pid != victim)),
        "replacement watcher; {}",
        pi.evidence()
    );
    assert!(
        common::wait_until(BUDGET, || {
            let t = pi.timeline();
            t.iter().any(|v| v["resumed"] == true)
                && !pi.reports("resume", &id).is_empty()
                && pi.reports("resume_answers", &id).iter().any(|v| {
                    v["resume_answers"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|r| r["id"] == id && r["answer"] == "retained")
                })
                && !pi.reports("rebound", &id).is_empty()
        }),
        "replacement registers, asks, retains and rebounds; {}",
        pi.evidence()
    );
    assert!(!pi.work.join(name).exists());
    pi.release_tool();
    pi.done(&id, name);
    pi.wait_report("committed", &id);
    pi.assert_receipt_order(&id);
    assert!(pi.reports("uncommitted_release", &id).is_empty());
    let registrations = pi
        .timeline()
        .iter()
        .filter_map(|v| v["registered"]["rx"].as_u64())
        .collect::<std::collections::HashSet<_>>();
    assert!(
        registrations.len() >= 2,
        "new receiver incarnation: {registrations:?}"
    );
}

/// Scenario: Start real Pi with its watcher's registration reply held back, and delegate a sentinel task to it while it is Idle. The deck types into no Pi release: its log says so when the agent starts, no doorbell is attempted or typed while the task waits, and the task then reaches Pi through its own extension, which acts on it once.
#[spec("inbox/pi/020")]
#[test]
fn pi_020_live_the_deck_types_nothing_into_pi() {
    skip_unless!(live::available());
    let pi = Pi::with_options(Options {
        register_delay_ms: 4000,
        ..Options::default()
    });
    assert!(
        common::wait_until(BUDGET, || pi.log().lines().any(|line| line
            .contains("doorbell off")
            && line.contains("unmeasured_release"))),
        "the deck says it types nothing into Pi; {}",
        pi.evidence()
    );
    let name = "never-typed-41c7e2.txt";
    let id = pi.append_task(name);
    pi.done(&id, name);
    let log = pi.log();
    assert!(
        !log.contains("doorbell attempt") && !log.contains("doorbell rang"),
        "the deck typed into Pi; {}",
        pi.evidence()
    );
}

/// Scenario: Submit a deck doorbell line in real Pi while its watcher registration reply is delayed. A labelled --doorbell-only fixture fixes the hand-out route so in-place substitution can be observed; Pi must replace the input and execute the task once, without exercising the default boot route or asking the model to fetch it.
#[spec("inbox/pi/020")]
#[test]
fn pi_020_live_unarmed_doorbell_waits_for_registration_and_transforms_in_place() {
    skip_unless!(live::available());
    let pi = Pi::with_options(Options {
        register_delay_ms: 4000,
        doorbell_only: true,
        ..Options::default()
    });
    let name = "doorbell-transform-96df83.txt";
    let id = pi.append_task(name);
    assert!(
        pi.timeline().iter().all(|v| v["registered"].is_null()),
        "submit during receiver-visible registration delay; {}",
        pi.evidence()
    );
    pi.prompt(&dot_agent_deck::doorbell::doorbell_line(&id));
    pi.done(&id, name);
    pi.wait_report("committed", &id);
    pi.assert_receipt_order(&id);
    let timeline = pi.timeline();
    let registration = timeline
        .iter()
        .position(|v| !v["registered"].is_null())
        .unwrap();
    let fetches = timeline
        .iter()
        .enumerate()
        .filter(|(_, v)| v["verb"] == serde_json::json!(["inbox", "fetch"]) && v["doorbell"] == id)
        .collect::<Vec<_>>();
    assert_eq!(fetches.len(), 1, "one extension fetch: {timeline:?}");
    assert!(
        registration < fetches[0].0,
        "receiver registration precedes intercepted fetch"
    );
    assert_eq!(fetches[0].1["integration"], "2");
    assert_eq!(
        fetches[0].1["fetchRx"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap(),
        timeline[registration]["registered"]["rx"].as_u64().unwrap()
    );
    assert!(
        pi.users()
            .iter()
            .all(|u| !u["event"].to_string().contains(inbox::PREFIX.trim())),
        "raw doorbell must not reach model"
    );
    assert!(
        pi.events()
            .iter()
            .filter(|e| e["type"] == "tool_execution_start")
            .all(|e| !e["event"].to_string().contains("inbox fetch")),
        "model tools do not fetch"
    );
}

/// Scenario: Plant a stale extension in a scratch Pi agent directory and start real Pi through the deck. The deck must refresh the files before Pi boots, register integration 2, and let Pi visibly execute and acknowledge a delegated sentinel task once.
#[spec("inbox/pi/021")]
#[test]
fn pi_021_live_stale_extension_is_refreshed_before_delegate() {
    skip_unless!(live::available());
    let pi = Pi::with_options(Options {
        stale: true,
        ..Options::default()
    });
    assert!(
        !pi.work.join("child-home/.pi/agent/extensions").exists(),
        "the relative PI_CODING_AGENT_DIR in the child's cwd wins over its HOME"
    );
    for (file, expected) in dot_agent_deck::orchestrator_ext::EXTENSION_FILES {
        assert_eq!(
            std::fs::read_to_string(pi.agent_dir.join("extensions/dot-agent-deck").join(file))
                .unwrap(),
            *expected,
            "refreshed bundled {file}"
        );
    }
    assert!(
        pi.timeline()
            .iter()
            .any(|v| v["verb"] == serde_json::json!(["inbox", "wait"])
                && v["slot"] == "pi"
                && v["integration"] == "2")
    );
    let name = "refresh-native-4abde9.txt";
    let id = pi.append_task(name);
    pi.done(&id, name);
    pi.wait_report("committed", &id);
    pi.assert_receipt_order(&id);
}

/// Scenario: Start a Pi pane with an impossible extension target inside a scratch agent directory. The attached deck must show the Pi extension refresh remedy, naming orchestrator setup and the agent's HOME and PI_CODING_AGENT_DIR, instead of the OpenCode repair command.
#[spec("inbox/pi/021")]
#[test]
fn pi_021_live_unwritable_extension_shows_pi_refresh_remedy() {
    let installed = std::process::Command::new("pi")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(installed, "Pi must be installed for the real spawn failure");
    let pi = Pi::with_options(Options {
        obstructed: true,
        absent_auth: true,
        ..Options::default()
    });
    assert!(
        common::wait_until(BUDGET, || pi.record()["inbox"]["integration"]
            == "refresh_failed_pi"),
        "Pi refresh notice; {}",
        pi.evidence()
    );
    // This failure path returns before Pi's session_start. Wait for the TUI
    // to hydrate the orchestration tab before trying to select it.
    pi.deck.wait_for_string("pi-inbox");
    pi.deck.send_bytes(b"\t");
    pi.deck.wait_for_string("coder");
    assert!(
        pi.deck
            .wait_for_grid_string_within("could not update this agent's Pi extension", BUDGET),
        "Pi refresh warning visible on its card; {}",
        pi.evidence()
    );
    assert!(
        pi.deck
            .wait_for_grid_string_within("dot-agent-deck orchestrator setup", BUDGET),
        "Pi remedy rendered; {}",
        pi.evidence()
    );
    let grid = pi.deck.snapshot_grid();
    assert!(
        grid.contains("HOME") && grid.contains("PI_CODING_AGENT_DIR"),
        "{grid}"
    );
    assert!(!grid.contains("hooks install --agent opencode"), "{grid}");
}

/// Scenario: Delegate a sentinel task while real Pi is busy, wait until its follow-up is handed over, then press Escape in the attached Pi pane. The lost queued copy must produce one possible-repeat delivery, and that repeat must visibly execute the task once and become extension-confirmed.
#[spec("inbox/pi/018")]
#[test]
fn pi_018_live_escape_loses_followup_and_runs_one_marked_repeat() {
    skip_unless!(live::available());
    let pi = Pi::with_options(Options {
        short_windows: true,
        ..Options::default()
    });
    pi.hold_tool();
    let name = "escape-queued-59f83a.txt";
    let id = pi.append_task(name);
    pi.wait_report("handed", &id);
    assert!(pi.reports("committed", &id).is_empty());
    assert!(!pi.work.join(name).exists());
    let mut input = pi.input("");
    input.write("\x1b");
    assert!(
        common::wait_until(BUDGET, || pi
            .events()
            .iter()
            .any(|e| e["type"] == "agent_settled")),
        "Escape must cancel/settle the real tool; {}",
        pi.evidence()
    );
    // Capture the restored editor copy before clearing it, without submitting
    // it. One Escape only: an extra press can change Pi's queue semantics.
    assert!(
        common::wait_until(BUDGET, || pi
            .screen()
            .contains(&format!("[end of dot-agent-deck message {id}]"))),
        "Escape must restore the queued draft; {}",
        pi.evidence()
    );
    std::fs::write(
        pi.diagnostics.join("escape-restored-pane.txt"),
        common::redact_credentials_for_output(&pi.screen()),
    )
    .unwrap();
    // Pi's Ctrl+C clears the whole multiline editor; Ctrl+U clears one line.
    input.write("\x03");
    pi.wait_report("uncommitted_release", &id);
    assert!(
        pi.reports("not_handed", &id).is_empty(),
        "past the gate is lost, not not_handed"
    );
    common::wait_for_file_trimmed_eq(&pi.work.join(name), live::CONTENT, BUDGET)
        .unwrap_or_else(|why| panic!("marked repeat action: {why}; {}", pi.evidence()));
    inbox::state(&pi.deck, &pi.pane, &id, "acknowledged");
    assert_eq!(
        pi.message(&id)["deliveries"],
        2,
        "one possible repeat; {}",
        pi.evidence()
    );
    assert_eq!(pi.message(&id)["ack_how"], "extension_confirmed");
    let users = pi
        .users()
        .into_iter()
        .filter(|e| e["event"].to_string().contains(&id))
        .collect::<Vec<_>>();
    assert_eq!(
        users.len(),
        1,
        "the lost copy was never started on; {users:?}"
    );
    assert!(
        users[0]["event"]
            .to_string()
            .contains(&dot_agent_deck::inbox_display::repeat_line(&id)),
        "actual repeat opener is marked: {users:?}"
    );
    pi.wait_report("committed", &id);
    pi.assert_receipt_order(&id);
    pi.observe_interval(Duration::from_millis(2 * WINDOW_MS), || {
        assert_eq!(pi.message(&id)["deliveries"], 2)
    });
    assert_eq!(
        std::fs::read_to_string(pi.work.join(name)).unwrap(),
        live::CONTENT
    );
}

/// Scenario: Start real interactive Pi on an explicitly selected model with no provider authentication, then delegate a native task. The attached card and inbox picker must explain that it may not have reached Pi, retain one delivery for twice the notice window, and send exactly one marked repeat only when Retry is pressed.
#[spec("inbox/pi/022")]
#[test]
fn pi_022_live_absent_auth_shows_may_notice_without_resend_until_retry() {
    let installed = std::process::Command::new("pi")
        .arg("--version")
        .output()
        .map_err(|e| e.to_string())
        .and_then(|out| {
            if out.status.success() {
                Ok(())
            } else {
                Err("pi --version failed".into())
            }
        });
    skip_unless!(installed);
    let pi = Pi::with_options(Options {
        absent_auth: true,
        short_windows: true,
        ..Options::default()
    });
    pi.assert_authless();
    let id = pi.delegate("absent-auth-cc712f.txt");
    pi.wait_report("handed", &id);
    assert!(
        common::wait_until(BUDGET, || pi.screen().contains("No API key found")),
        "explicit missing-auth error required; {}",
        pi.evidence()
    );
    let no_start = || {
        assert!(
            pi.events().iter().all(|e| !matches!(
                e["type"].as_str(),
                Some("before_agent_start" | "agent_start" | "message_start" | "agent_settled")
            )),
            "absent-auth precondition: no native start/settle; {}",
            pi.evidence()
        );
        assert!(pi.reports("committed", &id).is_empty());
        assert!(
            pi.timeline().iter().all(|v| v["event"] != "running"),
            "absent auth sends no running report"
        );
        assert!(
            pi.daemon_events
                .snapshot()
                .iter()
                .all(|e| e.agent_id.as_deref() != Some(pi.pane.agent.as_str())
                    || !matches!(e.event_type, dot_agent_deck::event::EventType::Thinking)),
            "absent auth sends no daemon Started"
        );
        assert_ne!(pi.message(&id)["state"], "acknowledged");
    };
    pi.observe_interval(Duration::from_millis(2 * WINDOW_MS), || {
        no_start();
        assert_eq!(pi.message(&id)["deliveries"], 1);
    });
    pi.deck.send_bytes(b"\x04");
    assert!(
        pi.deck
            .wait_for_grid_string_within("may not have reached the agent", BUDGET),
        "real card notice; {}",
        pi.evidence()
    );
    pi.deck.send_bytes(b"i");
    assert!(
        pi.deck
            .wait_for_grid_string_within("Retry sends it again", BUDGET),
        "picker duplicate warning; {}",
        pi.evidence()
    );
    std::fs::write(
        pi.diagnostics.join("retry-picker.txt"),
        pi.deck.snapshot_grid(),
    )
    .unwrap();
    let grid = pi.deck.snapshot_grid();
    let reason = grid
        .lines()
        .filter_map(|line| line.split("│    ").nth(1))
        .map(|rest| rest.split('│').next().unwrap().trim())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        reason.contains(dot_agent_deck::inbox_display::NOT_STARTED_REASON),
        "picker must show the duplicate warning in full; {}",
        pi.evidence()
    );
    pi.deck.send_bytes(b"r");
    assert!(
        common::wait_until(BUDGET, || pi.reports("handed", &id).len() == 2),
        "repeat handed; {}",
        pi.evidence()
    );
    assert_eq!(
        pi.message(&id)["deliveries"],
        2,
        "Retry's second observed hand-over must be counted in public deliveries; {}",
        pi.evidence()
    );
    assert!(
        pi.events().iter().any(|e| e["type"] == "input"
            && e["event"]
                .to_string()
                .contains(&dot_agent_deck::inbox_display::repeat_line(&id))),
        "repeat's transformed input is marked; {}",
        pi.evidence()
    );
    pi.observe_interval(Duration::from_millis(2 * WINDOW_MS), || {
        no_start();
        assert_eq!(pi.message(&id)["deliveries"], 2);
    });
    assert!(!pi.work.join("absent-auth-cc712f.txt").exists());
}
