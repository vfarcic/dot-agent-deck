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

/// Scenario: An attested orchestrator delegates a multiline task to a synthetic worker. One bound inbox message and a task file without an ack-first header must result; the worker fetches the framed task after the fixed doorbell, without receiving task bytes on stdin.
#[spec("orchestration/delegate/001")]
#[test]
fn delegate_001_posts_one_bound_task_and_worker_fetches() {
    let daemon = daemon(&[]);
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
    let daemon = daemon(&[]);
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
        let daemon = daemon(&[]);
        let (orch, worker) = route::team(&daemon, false);
        let first = route::delegate(&daemon, &orch, route::TASK, false);
        route::fetched(&daemon, &worker, &first, route::TASK, "the orchestrator");
        route::done(&daemon, &worker, route::REPORT, false);
        route::count(&daemon, &orch, 1);
        let second = route::delegate(&daemon, &orch, route::TASK, false);
        assert_ne!(first, second, "identical follow-up needs a fresh inbox id");
        route::fetched(&daemon, &worker, &second, route::TASK, "the orchestrator");
    }
    let daemon = daemon(&[
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
    let daemon = daemon(&[
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

/// Scenario: Delegate with clear enabled to a live stand-in. The old process is replaced before posting; exactly one task is bound to the new agent and only that generation can fetch it.
#[spec("inbox/route/001")]
#[test]
fn route_001_clear_posts_after_respawn_to_new_agent() {
    let daemon = daemon(&[]);
    let (orch, old) = route::team(&daemon, true);
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
        let daemon = daemon(&[("DOT_AGENT_DECK_WAITING_NOTICE_DEBOUNCE_MS", "100")]);
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
    let daemon = daemon(&[]);
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

/// Scenario: Start each supported agent type with a daemon seed while it has sent no SessionStart, showing Codex's recognised empty composer in its stand-in. The seed must already be queued once and bound to that generation; Pi's legacy GetSeed alias consumes the oldest inbox item once, while the other panes fetch after their fixed doorbell without receiving typed content.
#[spec("inbox/route/004")]
#[test]
fn route_004_start_agent_seed_is_first_message_for_every_agent_type() {
    let daemon = daemon(&[]);
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
            let first = hook(&daemon, request(&pane, "get_seed"));
            let text = first["seed"]
                .as_str()
                .expect("Pi GetSeed must serve the inbox seed");
            let id = messages[0]["id"].as_str().unwrap();
            assert!(
                text.contains(route::TASK) && text.contains(id),
                "framed seed: {first}"
            );
            state(&daemon, &pane, id, "acknowledged");
            assert!(hook(&daemon, request(&pane, "get_seed"))["seed"].is_null());
            let next = enqueue(&daemon, &pane, "PI-GET-SEED-NEXT-79a2");
            let second = hook(&daemon, request(&pane, "get_seed"));
            assert!(
                second["seed"].as_str().is_some_and(
                    |text| text.contains(&next) && text.contains("PI-GET-SEED-NEXT-79a2")
                ),
                "alias must consume inbox messages after the startup seed too: {second}"
            );
            state(&daemon, &pane, &next, "acknowledged");
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
    let daemon = daemon(&[]);
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
    let daemon = daemon(&[]);
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
    let daemon = daemon(&[
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
