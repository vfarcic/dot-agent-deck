#![cfg(all(feature = "e2e", feature = "e2e-live"))]
#![cfg(unix)]
//! Scheduled manually with --test-threads 1: never opt in on a shared box by accident.
#[path = "support/codex_inbox_live.rs"]
mod codex;
mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "common/inbox_load.rs"]
mod load;
#[path = "support/opencode_inbox_live.rs"]
mod opencode;
#[path = "support/pi_inbox_live.rs"]
mod pi;

use common::TuiDeck;
use inbox::Pane;
use serde_json::{Value, json};
use spec::spec;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// These are absolute per-agent windows: contention does not reset the budget
// for every message. Scheduling must give this binary a nextest timeout > 360s.
const NATIVE_DEADLINE: Duration = Duration::from_secs(240);
const CODEX_DEADLINE: Duration = Duration::from_secs(300);

fn record(deck: &TuiDeck, pane: &Pane) -> Value {
    inbox::attach(deck, json!({"op":"list-agents"}))["agent_records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == pane.agent)
        .expect("live agent record")
        .clone()
}

fn pane_from(deck: &TuiDeck, cwd: &Path, reply: Value) -> Pane {
    assert_eq!(reply["ok"], true, "real agent spawn: {reply}");
    let agent = reply["id"].as_str().unwrap().to_owned();
    let token = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(common::recorded_hook_capability(cwd, &agent))
        .unwrap();
    let r = common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .find(|r| r.id == agent)
        .unwrap();
    Pane {
        id: r.pane_id_env.unwrap(),
        agent,
        token,
        pid: 0,
        capture: PathBuf::new(),
        attach_socket: deck.attach_socket_path().into(),
    }
}

fn directive(work: &Path, phase: &str, n: usize) -> String {
    format!(
        "First run this exact command with the shell tool:\n```sh\nprintf '%s' 'INBOX-LOAD-ACTION-{phase}-{n}' | tee '{}'\n```\nThis message is a new task, including when it arrives in a previous tool's result. Execute its command once for this message id before replying. Reply with the command's stdout followed by DONE-{phase}-{n}, then end your turn.",
        work.join(format!("inbox-load-{phase}-{n}-49ab.txt"))
            .display()
    )
}

fn post(deck: &TuiDeck, pane: &Pane, body: &str, subject: &str) -> String {
    let reply = inbox::attach(
        deck,
        json!({"op":"post-message", "pane_id":pane.id,
        "expected_agent_id":pane.agent, "subject":subject, "body":body, "origin":"seed"}),
    );
    assert_eq!(reply["ok"], true, "load post must never vanish: {reply}");
    reply["id"]
        .as_str()
        .expect("durable-in-session message id")
        .into()
}

fn message(deck: &TuiDeck, pane: &Pane, id: &str) -> Option<Value> {
    inbox::listing(deck, pane)["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == id)
        .cloned()
}

fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

fn codex_cli_listing(deck: &TuiDeck, pane: &Pane, work: &Path, ids: &[(String, String, usize)]) {
    let output = std::process::Command::new(common::contained_deck_binary())
        .args(["inbox", "list", "--json"])
        .env_clear()
        .env("HOME", deck.home_dir())
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env("DOT_AGENT_DECK_ATTACH_SOCKET", deck.attach_socket_path())
        .env("DOT_AGENT_DECK_PANE_ID", &pane.id)
        .env("DOT_AGENT_DECK_AGENT_ID", &pane.agent)
        .env("DOT_AGENT_DECK_PANE_CAPABILITY", &pane.token)
        .current_dir(work)
        .output()
        .unwrap();
    assert!(output.status.success(), "Codex inbox list: {output:?}");
    let listed: Value = serde_json::from_slice(&output.stdout).expect("inbox list JSON");
    let messages = listed
        .as_array()
        .or_else(|| listed["messages"].as_array())
        .expect("listed messages");
    for (id, _, _) in ids {
        let m = messages
            .iter()
            .find(|m| m["id"] == *id)
            .unwrap_or_else(|| panic!("Codex message {id} absent from inbox list: {listed}"));
        assert_eq!(
            m["state"], "acknowledged",
            "the inbox list CLI must show the Codex message acknowledged: {m}"
        );
    }
}

fn delivered(
    deck: &TuiDeck,
    pane: &Pane,
    work: &Path,
    ids: &[(String, String, usize)],
    deadline: Instant,
    doorbell_only: bool,
) {
    let passed = common::wait_until(remaining(deadline), || {
        ids.iter().all(|(id, phase, n)| {
            let Some(m) = message(deck, pane, id) else {
                return false;
            };
            let acknowledged = m["state"] == "acknowledged";
            let acted =
                std::fs::read_to_string(work.join(format!("inbox-load-{phase}-{n}-49ab.txt")))
                    .is_ok_and(|text| text == format!("INBOX-LOAD-ACTION-{phase}-{n}"));
            // Codex too (PRD #1666): its doorbell rings while it is idle and
            // its Stop hook hands messages over at a turn's end, so a message
            // left queued is a lost delivery, not an explained one. Accepting
            // `queued` here let a run with nothing acknowledged pass at once.
            acknowledged && acted
        })
    });
    assert!(
        passed,
        "every message must be acknowledged and acted on: ids={ids:?}, inbox={}, grid={}\n{}",
        inbox::listing(deck, pane),
        deck.snapshot_grid(),
        common::load_context::failure_report()
    );
    if doorbell_only {
        codex_cli_listing(deck, pane, work, ids);
    }
    let acknowledged = ids
        .iter()
        .filter(|(id, _, _)| message(deck, pane, id).is_some_and(|m| m["state"] == "acknowledged"))
        .count();
    let acted = ids
        .iter()
        .filter(|(_, phase, n)| {
            std::fs::read_to_string(work.join(format!("inbox-load-{phase}-{n}-49ab.txt")))
                .is_ok_and(|text| text == format!("INBOX-LOAD-ACTION-{phase}-{n}"))
        })
        .count();
    eprintln!(
        "inbox load: phase={} total={} acknowledged={acknowledged} acted={acted} remaining_budget={:?}",
        ids.first()
            .map(|(_, phase, _)| phase.as_str())
            .unwrap_or("empty"),
        ids.len(),
        remaining(deadline)
    );
}

fn idle(deck: &TuiDeck, pane: &Pane, deadline: Instant) {
    assert!(
        common::wait_until(remaining(deadline), || record(deck, pane)["live"]["status"]
            == "Idle"),
        "agent must settle before the next phase: {}",
        record(deck, pane)
    );
}

fn batch(
    deck: &TuiDeck,
    pane: &Pane,
    work: &Path,
    phase: &str,
    count: usize,
) -> Vec<(String, String, usize)> {
    (0..count)
        .map(|n| {
            (
                post(
                    deck,
                    pane,
                    &directive(work, phase, n),
                    &format!("load {phase} {n}"),
                ),
                phase.into(),
                n,
            )
        })
        .collect()
}

fn clean_composer(deck: &TuiDeck, pane: &Pane, deadline: Instant) {
    assert!(
        common::wait_until(remaining(deadline), || {
            let r = record(deck, pane);
            let rows = r["rows"].as_u64().unwrap() as u16;
            let cols = r["cols"].as_u64().unwrap() as u16;
            let mut terminal = dot_agent_deck::vt_grid::parser(rows, cols, 0);
            terminal.process(&common::pane_snapshot_on(
                deck.attach_socket_path(),
                &pane.agent,
            ));
            let bottom = terminal
                .screen()
                .rows(0, cols)
                .skip(rows.saturating_sub(4) as usize)
                .collect::<Vec<_>>()
                .join("\n");
            !bottom.contains("waiting, run:")
                && !bottom.contains("dot-agent-deck: message")
                && !bottom.contains("[dot-agent-deck message")
        }),
        "deck text must not be stranded in the vt100 composer: {}",
        deck.snapshot_grid()
    );
}

// Reuse the established agent setup (hooks, onboarding, isolated HOME and
// wrapper trust). A second genuine interactive pane exercises an actual spawn
// seed, queued atomically by StartAgent, plus four posts made during its boot.
fn seeds(deck: &TuiDeck, work: &Path, kind: &str, deadline: Instant, doorbell_only: bool) {
    let command = match kind {
        "claude_code" => {
            "exec claude --model claude-haiku-4-5-20251001 --allowedTools Bash Read Write".into()
        }
        "opencode" => format!(
            "exec opencode --model {} --auto",
            common::opencode_test_model()
        ),
        "pi" => format!("exec pi {} --approve", common::pi_test_model().cli_args()),
        "codex" => format!(
            "exec codex --model {} --sandbox workspace-write --ask-for-approval never -c 'sandbox_workspace_write.network_access=true' -c 'model_reasoning_effort=\"low\"'",
            common::codex_test_model()
        ),
        _ => unreachable!(),
    };
    let phase = format!("{kind}-seed");
    let path = format!(
        "{}:{}",
        deck.home_dir().join(".local/bin").display(),
        std::env::var("PATH").unwrap()
    );
    let reply = inbox::attach(
        deck,
        json!({"op":"start-agent", "command":common::capability_export_command(&command),
        "agent_type":if kind == "opencode" { "open_code" } else { kind }, "cwd":work, "rows":40,"cols":140,"seed":directive(work, &phase, 0),
        "display_name":"load-seed", "env":[["DOT_AGENT_DECK_PANE_ID","load-seed"],["PATH",path],
        ["CODEX_HOME",deck.home_dir().join(".codex")],["PI_CODING_AGENT_DIR",deck.home_dir().join(".pi/agent")],
        ["DECK_TEST_PI_EVENTS",work.join("pi-load-seed-events.jsonl")],
        ["DECK_TEST_ROUTE",work.join("opencode-load-seed-route.json")],
        ["XDG_CONFIG_HOME",deck.home_dir().join(".config")], ["XDG_DATA_HOME",deck.home_dir().join(".local/share")],
        ["XDG_STATE_HOME",deck.home_dir().join(".local/state")], ["XDG_CACHE_HOME",deck.home_dir().join(".cache")]]}),
    );
    let seeded = pane_from(deck, work, reply);
    let seed = std::cell::RefCell::new(None);
    assert!(
        common::wait_until(remaining(deadline), || {
            *seed.borrow_mut() = inbox::listing(deck, &seeded)["messages"]
                .as_array()
                .unwrap()
                .first()
                .cloned();
            seed.borrow().is_some()
        }),
        "spawn must leave an inspectable seed message"
    );
    let mut ids = vec![(
        seed.into_inner().unwrap()["id"].as_str().unwrap().into(),
        phase.clone(),
        0,
    )];
    for n in 1..5 {
        ids.push((
            post(
                deck,
                &seeded,
                &directive(work, &phase, n),
                "load during boot",
            ),
            phase.clone(),
            n,
        ));
    }
    delivered(deck, &seeded, work, &ids, deadline, doorbell_only);
    idle(deck, &seeded, deadline);
    clean_composer(deck, &seeded, deadline);
}

fn exercise(deck: &TuiDeck, pane: &Pane, work: &Path, kind: &str, doorbell_only: bool) {
    let deadline = Instant::now()
        + if doorbell_only {
            CODEX_DEADLINE
        } else {
            NATIVE_DEADLINE
        };
    seeds(deck, work, kind, deadline, doorbell_only);
    if kind == "opencode" {
        // An untouched home screen has no completed session turn from which
        // OpenCode could emit Idle. Its visible composer and armed native
        // watcher are the precondition for the first batch; after that batch
        // we still require the genuine session Idle status below.
        assert!(
            common::wait_until(remaining(deadline), || {
                inbox::screen(deck, pane).contains("Ask anything")
                    && inbox::listing(deck, pane)["armed"].as_u64().unwrap_or(0) > 0
            }),
            "untouched OpenCode home must be visible and armed: {}",
            deck.snapshot_grid()
        );
    } else {
        idle(deck, pane, deadline);
    }
    let idle_ids = batch(deck, pane, work, "idle", 5);
    delivered(deck, pane, work, &idle_ids, deadline, doorbell_only);
    idle(deck, pane, deadline);
    let started = work.join("inbox-load-busy-start-49ab.txt");
    let ended = work.join("inbox-load-busy-end-49ab.txt");
    let mut input = common::AttachInput::attach(deck.attach_socket_path(), &pane.agent);
    let busy_prompt = format!(
        "Use the shell tool to run exactly: printf busy > '{}'; sleep 30; printf done > '{}'. Then reply BUSY-DONE and stop.",
        started.display(),
        ended.display()
    );
    input.write(&busy_prompt);
    // Observe the human prompt arriving before submitting it under load.
    assert!(
        common::wait_until(remaining(deadline), || {
            let r = record(deck, pane);
            let mut terminal = dot_agent_deck::vt_grid::parser(
                r["rows"].as_u64().unwrap() as u16,
                r["cols"].as_u64().unwrap() as u16,
                0,
            );
            terminal.process(&common::pane_snapshot_on(
                deck.attach_socket_path(),
                &pane.agent,
            ));
            terminal.screen().contents().contains("BUSY-DONE")
        }),
        "busy prompt must render before its submit"
    );
    input.write("\r");
    if kind == "codex" {
        // A painted prompt does not end Codex's paste burst: its first Enter
        // can insert a newline. Like questions::submit, press Enter until the
        // agent reports this user prompt (not Idle from the preceding turn),
        // or its tool starts. Keep the original absolute load-test deadline.
        // Hooks truncate reported prompts, so match this turn's busy prefix.
        let busy_prefix: String = busy_prompt.chars().take(64).collect();
        let submitted = || {
            started.exists()
                || record(deck, pane)["live"]["last_user_prompt"]
                    .as_str()
                    .is_some_and(|text| text.starts_with(&busy_prefix))
        };
        let mut enters = 1;
        while !common::wait_until(remaining(deadline).min(Duration::from_secs(2)), submitted) {
            assert!(
                Instant::now() < deadline,
                "busy prompt must be submitted before waiting for its tool: {}",
                deck.snapshot_grid()
            );
            eprintln!(
                "inbox load: busy prompt not yet submitted after {enters} Enter(s); pane={}",
                inbox::screen(deck, pane)
            );
            input.write("\r");
            enters += 1;
        }
        eprintln!("inbox load: busy prompt proven submitted after {enters} Enter(s)");
    }
    assert!(
        common::wait_until(remaining(deadline), || started.exists()),
        "busy phase must enter its genuine sleep tool"
    );
    let busy_ids = batch(deck, pane, work, "busy", 10);
    assert!(
        !ended.exists(),
        "busy messages must be sent while sleep is still running"
    );
    delivered(deck, pane, work, &busy_ids, deadline, doorbell_only);
    idle(deck, pane, deadline);
    if kind == "claude_code" {
        let before_clear = inbox::listing(deck, pane)["armed"].as_u64().unwrap_or(0);
        input.write("/clear\r");
        assert!(
            common::wait_until(remaining(deadline), || inbox::listing(deck, pane)["armed"]
                .as_u64()
                .unwrap_or(0)
                > before_clear),
            "clear must arm another native waiter"
        );
        let clear_ids = batch(deck, pane, work, "clear", 5);
        delivered(deck, pane, work, &clear_ids, deadline, false);
        idle(deck, pane, deadline);
    }
    clean_composer(deck, pane, deadline);
}

fn claude() -> (TuiDeck, Pane) {
    let deck = TuiDeck::builder()
        .with_pty_size(180, 50)
        .with_imported_claude_credentials()
        .with_claude_trust_workdir()
        .with_env("DOT_AGENT_DECK_INBOX_ACK_WAIT_MS", "360000")
        .with_env("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "600")
        .with_continue_session(
            "inbox-load-haiku",
            common::capability_export_command(
                "exec claude --model claude-haiku-4-5-20251001 --allowedTools Bash Read Write",
            ),
        )
        .launch_with_fixture("minimal");
    deck.wait_for_string("inbox-load-haiku");
    let agent = std::cell::RefCell::new(None);
    assert!(common::wait_until(Duration::from_secs(120), || {
        *agent.borrow_mut() = common::agent_records_on(deck.attach_socket_path())
            .into_iter()
            .find(|r| r.display_name.as_deref() == Some("inbox-load-haiku"));
        agent.borrow().is_some()
    }));
    let pane = pane_from(
        &deck,
        deck.workdir(),
        json!({"ok":true,"id":agent.into_inner().unwrap().id}),
    );
    assert!(
        common::wait_until(
            Duration::from_secs(120),
            || inbox::listing(&deck, &pane)["armed"].as_u64().unwrap_or(0) > 0
        ),
        "SessionStart must arm the real Claude waiter under load"
    );
    assert!(
        deck.wait_for_grid_string_within("Claude Code v", Duration::from_secs(120)),
        "restored interactive Claude pane must be attached"
    );
    (deck, pane)
}

fn codex_fixture() -> (TuiDeck, Pane) {
    // Reuse Codex's migration/hook-trust setup, with a lifetime long enough
    // for its load-test deadline plus startup (the normal fixture uses 300s).
    let deck = TuiDeck::builder()
        .with_pty_size(180, 50)
        .with_imported_codex_credentials()
        .with_env("DOT_AGENT_DECK_LOG", "codex-inbox-load.log")
        .with_env("DOT_AGENT_DECK_DOORBELL_SCHEDULE_MS", "30000")
        .with_env("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "600")
        .launch_with_fixture("minimal");
    assert!(deck.wait_for_grid_string_within("No active agents", Duration::from_secs(120)));
    codex::prepare(&deck);
    let command = common::capability_export_command(&format!(
        "exec codex --model {} --sandbox workspace-write --ask-for-approval never -c 'sandbox_workspace_write.network_access=true' -c 'model_reasoning_effort=\"low\"'",
        common::codex_test_model()
    ));
    let reply = inbox::attach(
        &deck,
        json!({"op":"start-agent", "command":command,
        "agent_type":"codex", "cwd":deck.workdir(), "rows":40, "cols":140,
        "display_name":"inbox-load-codex", "env":[["CODEX_HOME",deck.home_dir().join(".codex")],
        ["DOT_AGENT_DECK_PANE_ID","inbox-load-codex"]]}),
    );
    let pane = pane_from(&deck, deck.workdir(), reply);
    assert!(
        common::wait_until(Duration::from_secs(120), || inbox::screen(&deck, &pane)
            .contains("? for shortcuts")),
        "real Codex must finish startup"
    );
    deck.wait_for_string("inbox-load-codex");
    deck.send_bytes(b"1");
    assert!(
        deck.wait_for_grid_string_within("? for shortcuts", Duration::from_secs(120)),
        "real Codex must be attached"
    );
    (deck, pane)
}

/// Scenario: Under three busy-loop processes per CPU, run real interactive Claude Haiku through spawn seed, idle, a sleep-tool turn and /clear, asking it to execute each message's command before replying with its output. Every one of at least 25 messages must be acknowledged and create its sentinel before the agent deadline, with no deck text stranded in the composer.
#[spec("inbox/load/001")]
#[test]
fn load_001_claude_acknowledges_seed_idle_busy_and_clear_under_cpu_pressure() {
    skip_unless!(load::enabled());
    skip_unless!(common::check_claude_available());
    let _pressure = load::CpuPressure::start();
    let (deck, pane) = claude();
    exercise(&deck, &pane, deck.workdir(), "claude_code", false);
}

/// Scenario: Under three busy-loop processes per CPU, run real interactive OpenCode on opencode_test_model through spawn seed, idle and a sleep-tool turn. All 20 messages must be acknowledged and acted on before its deadline, with no submitted deck content left in its composer.
#[spec("inbox/load/002")]
#[test]
fn load_002_opencode_acknowledges_seed_idle_and_busy_under_cpu_pressure() {
    skip_unless!(load::enabled());
    skip_unless!(opencode::available());
    let _pressure = load::CpuPressure::start();
    let op = opencode::OpenCode::new(false, 0, true);
    exercise(&op.deck, &op.pane, &op.work, "opencode", false);
}

/// Scenario: Under three busy-loop processes per CPU, run real interactive Pi through spawn seed, idle and a sleep-tool turn. All 20 messages must be acknowledged and create their unique sentinels before its deadline, leaving no deck text in its composer.
#[spec("inbox/load/003")]
#[test]
fn load_003_pi_acknowledges_seed_idle_and_busy_under_cpu_pressure() {
    skip_unless!(load::enabled());
    skip_unless!(pi::available());
    let _pressure = load::CpuPressure::start();
    let pi = pi::Pi::new();
    exercise(&pi.deck, &pi.pane, &pi.work, "pi", false);
}

/// Scenario: Under three busy-loop processes per CPU, run real interactive Codex on DOT_AGENT_DECK_CODEX_TEST_MODEL through spawn seed, idle and a sleep-tool turn, proving the busy prompt was submitted even when Enter first inserts a newline. Each of 20 messages must be acknowledged and acted on, and listed as acknowledged by the inbox list CLI; no deck line may remain in the composer.
#[spec("inbox/load/004")]
#[test]
fn load_004_codex_never_loses_seed_idle_or_busy_messages_under_cpu_pressure() {
    skip_unless!(load::enabled());
    skip_unless!(common::check_codex_available());
    let _pressure = load::CpuPressure::start();
    let (deck, pane) = codex_fixture();
    exercise(&deck, &pane, deck.workdir(), "codex", true);
}
