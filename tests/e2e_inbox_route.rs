#![cfg(all(feature = "e2e", unix))]
//! Synthetic routing tests. Their daemons let inbox components connect from
//! the test process (`inbox::daemon_unbound`), which reads and settles the
//! stand-in panes' mail from outside them; inbox/protocol/014 and /015 pin the
//! binding itself.

mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/inbox_route.rs"]
mod route;

use inbox::*;
use serde_json::json;
use spec::spec;
use std::time::Duration;

/// Scenario: An attested orchestrator delegates a multiline task to a synthetic worker. One bound inbox message and a task file without an ack-first header must result; the worker fetches the framed task after the fixed doorbell, without receiving task bytes on stdin.
#[spec("orchestration/delegate/001")]
#[test]
fn delegate_001_posts_one_bound_task_and_worker_fetches() {
    let daemon = daemon_unbound(&[]);
    let (orch, worker) = route::team(&daemon, false);
    let id = route::delegate(&daemon, &orch, route::TASK, false);
    assert_eq!(route::count(&daemon, &worker, 1)[0]["id"], id);
    let file = std::fs::read_to_string(route::task_file(&daemon)).unwrap();
    assert!(
        file.contains(route::TASK) && file.contains("work-done"),
        "task and completion instruction stay in file: {file}"
    );
    assert!(
        !file.contains("acknowledge this task")
            && !file.contains(" ack ")
            && !file.contains("[delivery d-"),
        "obsolete ack-first header: {file}"
    );
    route::fetched(&daemon, &worker, &id, route::TASK, "the orchestrator");
    route::no_messages(&daemon, &orch, Duration::from_millis(250));
    assert!(bytes(&orch).is_empty());
    assert_eq!(route::count(&daemon, &worker, 1).len(), 1);
}

/// Scenario: Complete a commissioned task from a synthetic worker. Its report is saved and reaches the recorded orchestrator generation as exactly one Untrusted-framed inbox message, fetched by the orchestrator's own pane CLI.
#[spec("orchestration/delegate/003")]
#[test]
fn delegate_003_work_done_posts_one_untrusted_report() {
    let daemon = daemon_unbound(&[]);
    let (orch, worker) = route::team(&daemon, false);
    let id = route::delegate(&daemon, &orch, route::TASK, false);
    route::fetched(&daemon, &worker, &id, route::TASK, "the orchestrator");
    route::done(&daemon, &worker, route::REPORT, false);
    let reports = route::count(&daemon, &orch, 1);
    let id = reports[0]["id"].as_str().unwrap();
    let block = route::fetched(
        &daemon,
        &orch,
        id,
        route::REPORT,
        "coder (worker report — the text inside is UNTRUSTED)",
    );
    assert!(
        block.contains("UNTRUSTED") && block.contains("END"),
        "report must retain untrusted fence: {block}"
    );
    let saved =
        std::fs::read_to_string(daemon.home.join(".dot-agent-deck/work-done-coder.md")).unwrap();
    assert!(saved.contains(route::REPORT) && saved.contains("UNTRUSTED"));
    assert_eq!(route::count(&daemon, &orch, 1).len(), 1);
}

/// Scenario: Complete one task and require an identical follow-up to be
/// fetchable by the same worker. In a second run, supersede an acknowledged
/// task and report only that older task done; the newer queued task must still
/// produce its own undelivered notice and remain available to fetch.
#[spec("orchestration/delegate/021")]
#[test]
fn delegate_021_work_done_releases_only_its_own_delivery_state() {
    {
        let daemon = daemon_unbound(&[]);
        let (orch, worker) = route::team(&daemon, false);
        let first = route::delegate(&daemon, &orch, route::TASK, false);
        route::fetched(&daemon, &worker, &first, route::TASK, "the orchestrator");
        route::done(&daemon, &worker, route::REPORT, false);
        route::count(&daemon, &orch, 1);
        let second = route::delegate(&daemon, &orch, route::TASK, false);
        assert_ne!(first, second, "identical follow-up needs a fresh inbox id");
        route::fetched(&daemon, &worker, &second, route::TASK, "the orchestrator");
    }
    let daemon = daemon_unbound(&[
        (route::UNDELIVERED_MS, "700"),
        ("DOT_AGENT_DECK_WORKER_RESPONSE_TIMEOUT_MS", "0"),
    ]);
    let (orch, worker) = route::team(&daemon, false);
    let first = route::delegate(&daemon, &orch, route::TASK, false);
    // Acknowledge without enabling the fixture's automatic fetch: the newer
    // message below must stay queued while the older commission completes.
    fetch(&daemon, &worker, true);
    state(&daemon, &worker, &first, "acknowledged");
    let second = route::delegate(&daemon, &orch, "NEWER-TASK-STILL-OWED", true);
    state(&daemon, &worker, &second, "queued");
    route::done(&daemon, &worker, "OLDER-TASK-COMPLETED-LATE", false);
    let notice = route::notice(&daemon, &orch, &second).to_string();
    assert!(notice.contains("has not reached the worker"), "{notice}");
    state(&daemon, &worker, &second, "queued");
    route::fetched(
        &daemon,
        &worker,
        &second,
        "NEWER-TASK-STILL-OWED",
        "the orchestrator",
    );
}

/// Scenario: Fill a worker's single-message inbox, then delegate to it and
/// require an explicit refusal notice without a task being delivered. Its next
/// work-done must be labelled unsolicited, and after draining the inbox a
/// fresh delegation must be accepted and fetched rather than refused as busy.
#[spec("inbox/route/008")]
#[test]
fn route_008_refused_post_releases_the_commission_for_a_fresh_delegate() {
    let daemon = daemon_unbound(&[
        ("DOT_AGENT_DECK_INBOX_MAX_PER_RECIPIENT", "1"),
        ("DOT_AGENT_DECK_WORKER_RESPONSE_TIMEOUT_MS", "0"),
    ]);
    let (orch, worker) = route::team(&daemon, false);
    let filler = common::post_message_on(
        daemon.attach_socket(),
        &worker.id,
        &worker.agent,
        "FILL-THE-WORKER-INBOX",
    )
    .unwrap();
    let mut req = request(&orch, "delegate");
    req["task"] = json!("REFUSED-DELEGATE-TASK");
    req["to"] = json!(["coder"]);
    req["timestamp"] = json!(chrono::Utc::now());
    let refused = hook(&daemon, req);
    let refused_id = refused["message_ids"][0]
        .as_str()
        .expect("dispatch attempt id");
    let notice = route::notice(&daemon, &orch, "inbox refused");
    assert!(notice.to_string().contains("NOT delivered"));
    assert_eq!(route::count(&daemon, &worker, 1)[0]["id"], filler);
    assert!(
        !route::messages(&daemon, &worker)
            .iter()
            .any(|message| message["id"] == refused_id)
    );
    assert!(!String::from_utf8_lossy(&bytes(&worker)).contains("REFUSED-DELEGATE-TASK"));
    // `notice` fetched without acknowledging. Another fetch cannot take its
    // existing claim, so acknowledge that exact id before posting a report.
    let mut ack = request(&orch, "ack");
    ack["delivery_id"] = notice["messages"][0]["id"].clone();
    hook(&daemon, ack);
    state(
        &daemon,
        &orch,
        notice["messages"][0]["id"].as_str().unwrap(),
        "acknowledged",
    );
    route::done(&daemon, &worker, "UNCOMMISSIONED-AFTER-REFUSAL", false);
    route::count(&daemon, &orch, 2);
    let report = fetch(&daemon, &orch, true).to_string();
    assert!(
        report.contains("UNCOMMISSIONED-AFTER-REFUSAL")
            && report.contains("no outstanding delegation"),
        "{report}"
    );
    assert!(
        !daemon
            .home
            .join(".dot-agent-deck/work-done-coder.md")
            .exists()
    );
    fetch(&daemon, &worker, true);
    let accepted = route::delegate(&daemon, &orch, "FRESH-DELEGATE-AFTER-REFUSAL", false);
    route::fetched(
        &daemon,
        &worker,
        &accepted,
        "FRESH-DELEGATE-AFTER-REFUSAL",
        "the orchestrator",
    );
}

/// Scenario: Delegate with clear enabled after a predecessor's Idle hook. Keep the successor unready through predecessor ShellIdle samples for over five seconds; zero automatic input is allowed until its own Idle, which must promptly submit the newly bound task.
#[spec("inbox/route/001")]
#[test]
fn route_001_clear_posts_after_respawn_to_new_agent() {
    assert_clear_waits_for_ready(false);
}

/// Scenario: Delegate with clear enabled, report the successor's SessionStart, and age the hook history with predecessor-session shell samples. For 6.5 seconds its input must stay empty until its own Idle promptly submits the new task.
#[spec("inbox/route/001")]
#[test]
fn route_001_clear_session_start_is_not_ready() {
    assert_clear_waits_for_ready(true);
}

fn assert_clear_waits_for_ready(session_start: bool) {
    let daemon = daemon_unbound(&[]);
    let (orch, old) = route::team(&daemon, true);
    status(&daemon, &old, "idle", "claude_code");
    let id = route::delegate(&daemon, &orch, route::TASK, false);
    let successor = std::cell::RefCell::new(None);
    assert!(
        common::wait_until(WAIT, || {
            *successor.borrow_mut() = daemon
                .agent_records()
                .into_iter()
                .find(|r| r.pane_id_env.as_deref() == Some(&old.id) && r.id != old.agent)
                .map(|r| r.id);
            successor.borrow().is_some()
        }),
        "clear delegate must replace agent"
    );
    let new = route::observe(&daemon, &successor.into_inner().unwrap());
    assert_ne!(new.agent, old.agent);
    assert_eq!(route::count(&daemon, &new, 1)[0]["id"], id);
    {
        if session_start {
            status(&daemon, &new, "session_start", "claude_code");
        }
        // Age the bounded hook history past the predecessor's native hook.
        // Shell observations remain samples, not successor readiness.
        let samples: Vec<_> = (0..128)
            .map(|_| {
                json!({"session_id":format!("doorbell-{}", old.agent),
                "pane_id":new.id,"agent_id":new.agent,"token":new.token,
                "agent_type":"none","event_type":"shell_idle",
                "timestamp":chrono::Utc::now(),"metadata":{}})
            })
            .collect();
        for event in samples {
            let mut socket = connect(&daemon, &event).into_inner();
            socket.shutdown(std::net::Shutdown::Write).unwrap();
            std::io::Read::read_to_end(&mut socket, &mut Vec::new()).unwrap();
        }
        assert!(
            !common::wait_until(Duration::from_millis(6500), || !bytes(&new).is_empty()),
            "clear=true successor received automatic input before own ready (SessionStart={session_start}): {:?}",
            String::from_utf8_lossy(&bytes(&new))
        );
        state(&daemon, &new, &id, "queued");
    }
    // The submitted ring is consumed only once by the stand-in. Arm its
    // fetch before readiness so any correct wake/submit ordering can fetch.
    route::fetch_enable(&new);
    status(&daemon, &new, "idle", "claude_code");
    assert!(
        common::wait_until(Duration::from_secs(1), || bytes(&new).ends_with(b"\r")),
        "own ready must promptly deliver clear=true task, rather than a boot timer"
    );
    route::fetched(&daemon, &new, &id, route::TASK, "the orchestrator");
    assert!(!String::from_utf8_lossy(&bytes(&old)).contains(route::TASK));
    // Never stop a successor through an old pane's Drop: StopAgent uses id.
}

/// Scenario: Trigger quota-blocked, waiting-for-input and worker-exit notices in separate synthetic orchestrations. Each trigger posts one bound message labelled as a daemon notice, with no notice prose typed into the orchestrator.
#[spec("inbox/route/002")]
#[test]
fn route_002_worker_notices_are_bound_deck_messages() {
    for (event, needle) in [
        ("quota_blocked", "blocked"),
        ("waiting_for_input", "input"),
        ("exit", "exited"),
    ] {
        let daemon = daemon_unbound(&[("DOT_AGENT_DECK_WAITING_NOTICE_DEBOUNCE_MS", "100")]);
        let (orch, worker) = route::team(&daemon, false);
        let id = route::delegate(&daemon, &orch, route::TASK, false);
        route::fetched(&daemon, &worker, &id, route::TASK, "the orchestrator");
        if event == "exit" {
            // Natural exit, rather than StopAgent's deliberate-close semantics.
            let result = std::process::Command::new("kill")
                .args(["-TERM", &worker.pid.to_string()])
                .status()
                .unwrap();
            assert!(result.success());
        } else {
            status(&daemon, &worker, event, "claude_code");
        }
        route::notice(&daemon, &orch, needle);
        route::count(&daemon, &orch, 1);
    }
}

/// Scenario: Exit a bound worker before fetching its delegated message. The task remains inspectable as Undeliverable and the orchestrator receives a daemon notice naming that exact task id, rather than losing the task or delivering it to another generation.
#[spec("inbox/route/003")]
#[test]
fn route_003_exited_bound_agent_is_undeliverable_and_notifies_sender() {
    let daemon = daemon_unbound(&[]);
    let (orch, worker) = route::team(&daemon, false);
    let id = route::delegate(&daemon, &orch, route::TASK, false);
    std::process::Command::new("kill")
        .args(["-TERM", &worker.pid.to_string()])
        .status()
        .unwrap();
    let mut list = request(&orch, "inbox_list");
    list["all"] = json!(true);
    assert!(
        common::wait_until(WAIT, || {
            hook(&daemon, list.clone())["messages"]
                .as_array()
                .is_some_and(|messages| {
                    messages.iter().any(|message| {
                        message["id"] == id
                            && message["bind_agent"] == worker.agent
                            && message["state"]
                                .to_string()
                                .to_ascii_lowercase()
                                .contains("undeliverable")
                    })
                })
        }),
        "exited worker's bound task must remain inspectable as Undeliverable"
    );
    let notice = route::notice(&daemon, &orch, &id).to_string();
    assert!(
        notice.to_ascii_lowercase().contains("undeliverable"),
        "exit must name undeliverable state: {notice}"
    );
}

/// Scenario: Start each supported agent type with a daemon seed while it has sent no SessionStart, showing Codex's recognised empty composer in its stand-in. The seed must already be queued once and bound to that generation; Pi's GetSeed alias, which predates the receipt stream, is refused and leaves the seed queued, while the other panes fetch after their fixed doorbell without receiving typed content.
#[spec("inbox/route/004")]
#[test]
fn route_004_start_agent_seed_is_first_message_for_every_agent_type() {
    let daemon = daemon_unbound(&[]);
    let composer = daemon.home.join("codex-composer.txt");
    std::fs::write(&composer, include_str!("fixtures/codex-inbox/composer.txt")).unwrap();
    for agent_type in ["claude_code", "open_code", "pi", "codex", "devin"] {
        let mut extra = json!({"seed":route::TASK,"agent_type":agent_type});
        if agent_type == "codex" {
            extra["env"] = json!([
                ["DOT_AGENT_DECK_PANE_ID", agent_type],
                ["INBOX_ROUTE_CODEX_COMPOSER", composer]
            ]);
        }
        let pane = route::start(&daemon, agent_type, extra);
        let messages = route::count(&daemon, &pane, 1);
        assert!(
            messages[0]["state"]
                .to_string()
                .to_ascii_lowercase()
                .contains("queued")
        );
        route::only_doorbells(&pane, &messages);
        if agent_type == "pi" {
            // PRD #1666 G10: a Pi generation is on the receipt stream, so the
            // get-seed alias an extension from before it used is refused: no
            // seed, the message stays queued for the receiver.
            let first = hook(&daemon, request(&pane, "get_seed"));
            assert!(first["seed"].is_null(), "Pi GetSeed is refused: {first}");
            let id = messages[0]["id"].as_str().unwrap();
            state(&daemon, &pane, id, "queued");
            assert!(!String::from_utf8_lossy(&bytes(&pane)).contains(route::TASK));
            continue;
        }
        eprintln!("route_004: fetching the seed for {agent_type}");
        route::fetched(
            &daemon,
            &pane,
            messages[0]["id"].as_str().unwrap(),
            route::TASK,
            "",
        );
        route::count(&daemon, &pane, 1);
    }
}

/// Scenario: Start a daemon authoring agent with the dispatcher kind and no client seed. Exactly one bound inbox message contains the generated dispatcher instructions, with no seed bytes typed into its pane before any hook arrives.
#[spec("inbox/route/005")]
#[test]
fn route_005_daemon_authoring_seed_is_queued_without_readiness() {
    let daemon = daemon_unbound(&[]);
    let pane = route::start(&daemon, "author", json!({"authoring_kind":"dispatcher"}));
    let messages = route::count(&daemon, &pane, 1);
    assert!(bytes(&pane).is_empty());
    route::fetched(
        &daemon,
        &pane,
        messages[0]["id"].as_str().unwrap(),
        "one extra effector",
        "",
    );
    route::count(&daemon, &pane, 1);
}

/// Scenario: After a worker starts successfully, change its clear-enabled role command to a nonexistent executable and delegate again. The spawn failure reaches the orchestrator as one bound daemon-notice message, with no task content typed into any pane.
#[spec("inbox/route/006")]
#[test]
fn route_006_respawn_failure_posts_daemon_notice() {
    let daemon = daemon_unbound(&[]);
    let (orch, worker) = route::team(&daemon, true);
    let config = daemon.home.join(".dot-agent-deck.toml");
    let mut contents = std::fs::read_to_string(&config).unwrap();
    let start = contents.rfind("command = ").unwrap();
    let end = start + contents[start..].find('\n').unwrap();
    contents.replace_range(
        start..end,
        "command = 'inbox-route-missing-executable-6a83'",
    );
    std::fs::write(config, contents).unwrap();
    let mut req = request(&orch, "delegate");
    req["task"] = json!(route::TASK);
    req["to"] = json!(["coder"]);
    req["timestamp"] = json!(chrono::Utc::now());
    hook(&daemon, req);
    route::notice(&daemon, &orch, "respawn");
    route::count(&daemon, &orch, 1);
    assert!(!String::from_utf8_lossy(&bytes(&worker)).contains(route::TASK));
}

/// Scenario: Request configuration generation from a live synthetic card through the real TUI and accept the dialog. One bound configuration prompt must be queued without typed content; after Idle its pane fetches and renders the instructions.
#[spec("inbox/route/007")]
#[test]
fn route_007_tui_config_generation_posts_bound_message() {
    let deck = common::TuiDeck::builder()
        .with_pty_size(180, 50)
        .with_env("DOT_AGENT_DECK_DOORBELL_TICK_MS", "25")
        .with_env("DOT_AGENT_DECK_DOORBELL_SCHEDULE_MS", "30000")
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");
    let pane = route::start(&deck, "config-generator", json!({}));
    status(&deck, &pane, "session_start", "codex");
    deck.wait_for_absence("No active agents");
    status(&deck, &pane, "thinking", "codex");
    deck.send_keys(b"g");
    deck.wait_for_string("[Yes]");
    deck.send_keys(b"\r");
    let messages = route::count(&deck, &pane, 1);
    assert!(
        bytes(&pane).is_empty(),
        "config-gen prompt must not be typed"
    );
    let id = messages[0]["id"].as_str().unwrap();
    route::fetch_enable(&pane);
    status(&deck, &pane, "idle", "codex");
    state(&deck, &pane, id, "acknowledged");
    let fetched_path = pane.capture.with_extension("fetches");
    // The CLI acknowledges before the stand-in records its returned output.
    // Observe that record itself, rather than racing its creation after ack.
    common::wait_for_file_containing(&fetched_path, ".dot-agent-deck.toml", WAIT)
        .expect("the in-pane CLI must record the fetched configuration instructions");
    let fetched = std::fs::read_to_string(fetched_path).unwrap();
    assert!(
        fetched.contains(".dot-agent-deck.toml"),
        "configuration instructions: {fetched}"
    );
    assert!(!String::from_utf8_lossy(&bytes(&pane)).contains(".dot-agent-deck.toml"));
    route::count(&deck, &pane, 1);
}

/// Scenario: In separate runs leave a delegate Queued or Fetched past the accelerated undelivered deadline. The orchestrator receives one daemon-labelled inbox notice naming the message, worker, state and doorbell path, without pane text being typed. Replace another task's orchestrator before that deadline and require its successor to receive no stale notice while the worker's original task stays fetchable.
#[spec("orchestration/delegate/013")]
#[test]
fn delegate_013_silent_worker_surfaces_notice_in_orchestrator_pane() {
    route::undelivered(false);
    route::undelivered(true);
    assert_undelivered_notice_does_not_reach_successor_orchestrator();
}

fn assert_undelivered_notice_does_not_reach_successor_orchestrator() {
    let daemon = daemon_unbound(&[
        (route::UNDELIVERED_MS, "1500"),
        ("DOT_AGENT_DECK_WORKER_RESPONSE_TIMEOUT_MS", "0"),
    ]);
    let (orch, worker) = route::team(&daemon, false);
    let record = common::agent_records_on(daemon.attach_socket())
        .into_iter()
        .find(|record| record.id == orch.agent)
        .unwrap();
    let posted = std::time::Instant::now();
    let id = route::delegate(&daemon, &orch, route::TASK, false);
    assert!(route::messages(&daemon, &orch).is_empty());
    let stopped = attach(&daemon, json!({"op":"stop-agent","id":orch.agent}));
    assert_eq!(
        stopped["ok"], true,
        "stop only the original orchestrator: {stopped}"
    );
    let successor = route::start(
        &daemon,
        &orch.id,
        json!({"tab_membership":record.tab_membership}),
    );
    assert_ne!(successor.agent, orch.agent);
    assert!(
        posted.elapsed() < Duration::from_millis(1500),
        "successor must be ready before the original notice deadline"
    );
    assert!(
        !common::wait_until(Duration::from_secs(2), || {
            route::messages(&daemon, &successor).iter().any(|message| {
                message["bind_agent"] == successor.agent && message.to_string().contains(&id)
            }) || String::from_utf8_lossy(&bytes(&successor)).contains("has not reached the worker")
        }),
        "a stale undelivered notice reached the successor: {:?}",
        route::messages(&daemon, &successor)
    );
    state(&daemon, &worker, &id, "queued");
    route::fetched(&daemon, &worker, &id, route::TASK, "the orchestrator");
}

/// Scenario: Supersede an unacknowledged delegate with clear enabled, replacing its worker generation again. Only the successor's message is bound to the current worker and only its id appears in the eventual undelivered notice.
#[spec("orchestration/delegate/025")]
#[test]
fn delegate_025_superseded_generation_is_silent_while_new_watch_stays_armed() {
    route::superseded_generation();
}

/// Scenario: Leave a task Queued and then Fetched across two idle windows. No idle notice appears until the worker acknowledges it, after which the idle clock begins and one daemon notice reaches the orchestrator inbox.
#[spec("scheduler/idle-worker/026")]
#[test]
fn idle_worker_026_response_clocks_start_after_deferred_pointer() {
    route::idle_clock(26);
}

/// Scenario: Supersede an unacknowledged task with a newer one. The newer task starts no idle clock until its own acknowledgement, then one bound idle notice reaches the orchestrator inbox.
#[spec("scheduler/idle-worker/027")]
#[test]
fn idle_worker_027_queued_delegate_clocks_start_after_its_pointer() {
    route::idle_clock(27);
}

/// Scenario: Acknowledge a task and report work-done before its idle timeout. The orchestrator receives the completion in its inbox and never a stale idle-worker notice.
#[spec("scheduler/idle-worker/028")]
#[test]
fn idle_worker_028_waiting_idle_report_is_dropped_after_work_done() {
    route::idle_clock(28);
}

/// Scenario: Complete a task before fetching its delegate message. Work-done reaches the orchestrator inbox and retires the unacknowledged task so no later undelivered notice is posted.
#[spec("scheduler/idle-worker/029")]
#[test]
fn idle_worker_029_waiting_silence_report_is_dropped_after_work_done() {
    route::resolved_unacked(false);
}

/// Scenario: Supersede an unacknowledged task before its notice deadline. The orchestrator receives only the newer task id in its undelivered notice; the old task produces no stale notice.
#[spec("scheduler/idle-worker/031")]
#[test]
fn idle_worker_031_a_waiting_went_quiet_report_is_dropped_when_a_newer_pointer_lands() {
    route::resolved_unacked(true);
}

/// Scenario: Report a delegated worker waiting for input and let its notice enter the orchestrator inbox. Acknowledge the task while that notice is queued; the same notice remains fetchable exactly once, independent of task acknowledgement.
#[spec("scheduler/idle-worker/030")]
#[test]
fn idle_worker_030_a_waiting_notice_survives_its_own_pointer_landing_while_it_waits() {
    route::idle_clock(30);
}

/// The worker fetches its task through `inbox fetch` without acknowledging
/// it, as a hook does, and returns it.
fn fetched_unacknowledged(daemon: &common::DaemonProc, worker: &Pane, id: &str) {
    let reply = fetch(daemon, worker, false);
    assert!(
        reply["messages"]
            .as_array()
            .is_some_and(|messages| messages.iter().any(|m| m["id"] == id)),
        "the worker fetches the task: {reply}"
    );
}

/// The worker acknowledges `id` through the explicit ack route.
fn acknowledge(daemon: &common::DaemonProc, worker: &Pane, id: &str) {
    let mut ack = request(worker, "ack");
    ack["delivery_id"] = json!(id);
    assert_eq!(hook(daemon, ack)["accepted"], true);
}

fn message_state(daemon: &common::DaemonProc, worker: &Pane, id: &str) -> serde_json::Value {
    listing(daemon, worker)["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == id)
        .map(|m| m["state"].clone())
        .unwrap_or_default()
}

/// Scenario: A worker's OpenCode receipt stream holds a delegated task queued behind a long turn, naming it in a fresh snapshot every keepalive. For ten seconds — past ack-wait × max_deliver and past the undelivered-notice deadline — the task is never repeated nor undeliverable and the orchestrator gets no notice; when the stream commits it, it is delivered and the orchestrator still hears nothing.
#[spec("inbox/route/013")]
#[test]
fn route_013_a_task_possessed_through_a_long_turn_raises_no_false_notice() {
    let daemon = daemon_unbound(&[
        ("DOT_AGENT_DECK_INBOX_ACK_WAIT_MS", "1500"),
        ("DOT_AGENT_DECK_INBOX_MAX_DELIVER", "2"),
        (route::UNDELIVERED_MS, "4500"),
        ("DOT_AGENT_DECK_WORKER_RESPONSE_TIMEOUT_MS", "0"),
        ("DOT_AGENT_DECK_INBOX_KEEPALIVE_MS", "300"),
    ]);
    let (orch, worker) = route::team(&daemon, false);
    let mut stream = receipt_stream(&daemon, &worker);
    let id = route::delegate(&daemon, &orch, route::TASK, false);
    let handed = receipt_handout(&mut stream, &id);
    let written = possess_for(&mut stream, &handed, Duration::from_secs(10));
    assert!(
        !written
            .iter()
            .any(|w| w["messages"].as_array().is_some_and(|m| !m.is_empty())),
        "a possessed task is never repeated: {written:?}"
    );
    assert_eq!(message_state(&daemon, &worker, &id), "fetched");
    route::no_messages(&daemon, &orch, Duration::from_millis(300));
    receipt_control(
        &mut stream,
        json!({"committed": [{"id": id, "a": handed["a"], "e": handed["e"]}]}),
    );
    state(&daemon, &worker, &id, "acknowledged");
    route::no_messages(&daemon, &orch, Duration::from_millis(1200));
}

/// Scenario: A worker fetches a delegated task without acknowledging it, so when its one allowed hand-over's ack-wait runs out the task is undeliverable and the orchestrator is told so. The worker then acknowledges it: the task is recorded as delivered and the orchestrator gets a correction naming the task, saying it reached the worker after all.
#[spec("inbox/route/010")]
#[test]
fn route_010_a_late_ack_after_max_deliver_corrects_the_orchestrator() {
    let daemon = daemon_unbound(&[
        ("DOT_AGENT_DECK_INBOX_ACK_WAIT_MS", "1500"),
        ("DOT_AGENT_DECK_INBOX_MAX_DELIVER", "1"),
        (route::UNDELIVERED_MS, "4500"),
        ("DOT_AGENT_DECK_WORKER_RESPONSE_TIMEOUT_MS", "0"),
    ]);
    let (orch, worker) = route::team(&daemon, false);
    let id = route::delegate(&daemon, &orch, route::TASK, false);
    fetched_unacknowledged(&daemon, &worker, &id);
    state(&daemon, &worker, &id, "undeliverable");
    route::notice(&daemon, &orch, "is undeliverable");
    acknowledge(&daemon, &worker, &id);
    state(&daemon, &worker, &id, "acknowledged");
    let correction = route::notice(&daemon, &orch, "after all").to_string();
    assert!(
        correction.contains(&id) && correction.contains("coder"),
        "the correction names the task and the worker's role: {correction}"
    );
}

/// Scenario: A worker fetches a delegated task without acknowledging it, so the orchestrator is told the task has not reached the worker. The worker then acknowledges it before the deck stops trying: the task is recorded as delivered, and the orchestrator gets exactly one correction naming the task and the worker's role, saying it reached the worker after all.
#[spec("inbox/route/011")]
#[test]
fn route_011_an_ack_after_the_not_reached_notice_corrects_the_orchestrator_once() {
    let daemon = daemon_unbound(&[
        ("DOT_AGENT_DECK_INBOX_ACK_WAIT_MS", "1500"),
        ("DOT_AGENT_DECK_INBOX_MAX_DELIVER", "100"),
        (route::UNDELIVERED_MS, "3000"),
        ("DOT_AGENT_DECK_WORKER_RESPONSE_TIMEOUT_MS", "0"),
    ]);
    let (orch, worker) = route::team(&daemon, false);
    let id = route::delegate(&daemon, &orch, route::TASK, false);
    fetched_unacknowledged(&daemon, &worker, &id);
    let notice = route::notice(&daemon, &orch, "has not reached the worker").to_string();
    assert!(notice.contains(&id), "the notice names the task: {notice}");
    acknowledge(&daemon, &worker, &id);
    state(&daemon, &worker, &id, "acknowledged");
    let correction = route::notice(&daemon, &orch, "after all").to_string();
    assert!(
        correction.contains(&id) && correction.contains("coder"),
        "the correction names the task and the worker's role: {correction}"
    );
    let corrections = || {
        route::messages(&daemon, &orch)
            .iter()
            .filter(|m| m["subject"] == "task delivered after all")
            .count()
    };
    assert!(
        !common::wait_until(Duration::from_millis(1500), || corrections() > 1),
        "one correction per task: {:?}",
        route::messages(&daemon, &orch)
    );
    assert_eq!(corrections(), 1);
}

/// Scenario: Start an agent with a seed too long to hand over whole while neither the agent's directory nor the deck's state directory can take the file the deck saves it to. The start must answer an error saying the first prompt was refused, and no agent may be left running without its task.
#[spec("inbox/route/012")]
#[test]
fn route_012_a_start_whose_seed_cannot_be_posted_leaves_no_agent() {
    let daemon = daemon_unbound(&[]);
    let cwd = daemon.home.join("unsavable");
    std::fs::create_dir_all(&cwd).unwrap();
    // A file where each spill directory's `.dot-agent-deck/` would go, so the
    // seed passes admission (its size and the deck's room) and then cannot be
    // saved once the agent has started.
    std::fs::write(cwd.join(".dot-agent-deck"), b"not a directory").unwrap();
    let state_dir = daemon.schedules_path.parent().unwrap().join("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    std::fs::write(state_dir.join(".dot-agent-deck"), b"not a directory").unwrap();
    let seed = format!("ROUTE-012-UNSAVABLE-SEED {}", "x".repeat(64 * 1024));
    let reply = attach(
        &daemon,
        json!({"op":"start-agent", "command":"exec /bin/cat", "cwd":cwd, "rows":24,
        "cols":100, "env":[["DOT_AGENT_DECK_PANE_ID","unsavable"]],
        "display_name":"unsavable", "seed":seed}),
    );
    assert_eq!(reply["ok"], false, "a start that lost its seed: {reply}");
    assert!(
        reply.to_string().contains("first prompt"),
        "the refusal names the first prompt: {reply}"
    );
    let agents = || attach(&daemon, json!({"op":"list-agents"}));
    assert!(
        common::wait_until(Duration::from_secs(10), || {
            !agents().to_string().contains("unsavable")
        }),
        "no agent may stay without its task: {}",
        agents()
    );
}
