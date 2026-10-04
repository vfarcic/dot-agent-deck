#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]

//! L2 REAL-`pi` proof that a Pi pane confirms the prompts the deck types into
//! it through its own report (issue #1567).
//!
//! The bundled extension reports every prompt Pi submits — an idle one from
//! `before_agent_start`, one queued while Pi is busy from `input` — and
//! declares that it does on every report (`agent-event --reports-prompts`). The
//! deck then treats a Pi pane like Claude Code: an automatic prompt it typed in
//! stays provisional until Pi's report of that prompt comes back, and is
//! re-submitted if it never does. An extension that declares nothing (one from
//! an older deck) keeps the old behaviour; the fast tier pins that half
//! (`spawn::tests`, `tests/pi_agent_event_cli.rs`).
//!
//! Two real-Pi paths, both typed into the pane's PTY rather than handed over
//! natively through `get-seed`:
//!
//! * `scheduler/pi/002` — a scheduled job's prompt, delivered by the daemon's
//!   spawn-time delivery (`crate::spawn`). The daemon log must record the
//!   delivery CONFIRMED by Pi's submitted prompt.
//! * `chain-smoke/pi/003` — a delegate's task pointer, typed into an idle Pi
//!   worker (`clear = false`, so no respawn and no native seed). Pi's own report
//!   of that pointer is the proof the delegate's in-place re-delivery waits for,
//!   and with it the pointer is submitted exactly once.
//!
//! Tier: lane 2 (`e2e-live`) — spawns a real agent and hits a real model, so it
//! runs on a developer's machine and nowhere in CI. Runtime-skipped (Decision
//! 26) when `pi` / `ANTHROPIC_API_KEY` is absent; set
//! `DOT_AGENT_DECK_REQUIRE_REAL_E2E=1` to make that skip a failure. The
//! `#[spec]` entry points are sync `#[test]`s (the linkage-check scanner links
//! `#[spec]` to the next plain `fn`), and all polling lives in `common`
//! (Decision 21).

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use dot_agent_deck::agent_pty::{DOT_AGENT_DECK_PANE_ID, SpawnOptions};
use dot_agent_deck::event::{AgentEvent, AgentType, DelegateSignal, EventType};

mod common;

use common::TuiDeck;

use spec::spec;

/// Cheapest tier in pi's Anthropic catalog — the model every pi lane-2 test
/// pins (TEMPORARY: Anthropic Haiku while the GPT accounts are without credit).
const PI_MODEL: &str = "claude-haiku-4-5";

/// `pi` on PATH AND a non-empty `ANTHROPIC_API_KEY` (checked, never printed).
/// Mirrors `e2e_pi_worker.rs::check_pi_available`.
fn check_pi_available() -> Result<(), String> {
    let ok = std::process::Command::new("pi")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        return Err("pi CLI not installed (could not invoke `pi --version`)".into());
    }
    match std::env::var("ANTHROPIC_API_KEY") {
        Ok(k) if !k.trim().is_empty() => Ok(()),
        _ => Err("ANTHROPIC_API_KEY not set — real-pi e2e needs Anthropic auth".into()),
    }
}

/// The freshly-built binary's dir, prepended to PATH so the extension's
/// `dot-agent-deck agent-event` (and the worker's `work-done`) resolve.
fn path_with_binary_dir() -> String {
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let bin_dir = Path::new(bin)
        .parent()
        .expect("test binary has a parent dir")
        .to_str()
        .expect("bin dir is UTF-8");
    format!("{bin_dir}:{}", std::env::var("PATH").unwrap_or_default())
}

/// Is `event` the extension's report of a submitted prompt containing `needle`,
/// from a producer that declared it reports every prompt?
fn declared_prompt_report(event: &AgentEvent, needle: &str) -> bool {
    event.agent_type == AgentType::Pi
        && event.event_type == EventType::Thinking
        && event.declares_prompt_reports()
        && event
            .user_prompt
            .as_deref()
            .is_some_and(|prompt| prompt.contains(needle))
}

/// The RFC 3339 timestamp `tracing`'s default formatter puts first on the first
/// log line containing `needle`, if there is one.
fn log_line_time(log: &str, needle: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    log.lines()
        .find(|line| line.contains(needle))
        .and_then(|line| line.split_whitespace().next())
        .and_then(|stamp| chrono::DateTime::parse_from_rfc3339(stamp).ok())
}

// ---------------------------------------------------------------------------
// scheduler/pi/002 — a typed-in scheduled prompt, confirmed by Pi's own report
// ---------------------------------------------------------------------------

const SCHEDULED_SENTINEL: &str = "pi_confirm_sentinel_3a7f.txt";
const SCHEDULED_SENTINEL_CONTENT: &str = "PI_CONFIRM_SENTINEL_OK";
const SCHEDULE_NAME: &str = "pi-typed-prompt";

/// Scenario: Launch the deck with a schedule whose command is an interactive real `pi` and whose prompt tells it to create `pi_confirm_sentinel_3a7f.txt`, then fire it with Run now. The deck types the prompt into the new Pi pane, and its card shows the prompt on its `Prmt:` row. Pi's own report of that prompt confirms the delivery in the deck log, never `cannot be confirmed`, and pi creates the sentinel.
#[spec("scheduler/pi/002")]
#[test]
fn scheduler_pi_002_typed_in_prompt_is_confirmed_by_pi_own_report() {
    skip_unless!(check_pi_available());

    let scratch = common::harness_tempdir().expect("scratch tempdir");
    let work = scratch.path().join("pi-work");
    std::fs::create_dir_all(&work).expect("create pi working_dir");
    let log = scratch.path().join("deck.log");
    let schedules = scratch.path().join("schedules.toml");

    // An interactive pi with NO prompt on its command line, so the schedule's
    // prompt goes through the deck's typed-in delivery, not pi's argv.
    let directive = format!(
        "Use your bash tool to create a file named {SCHEDULED_SENTINEL} in the current \
         directory whose entire contents are {SCHEDULED_SENTINEL_CONTENT}. Do nothing else."
    );
    std::fs::write(
        &schedules,
        format!(
            "[[scheduled_tasks]]\n\
             name = \"{SCHEDULE_NAME}\"\n\
             cron = \"0 0 1 1 *\"\n\
             working_dir = \"{}\"\n\
             command = \"pi --provider anthropic --model {PI_MODEL} --approve\"\n\
             prompt = \"{directive}\"\n\
             enabled = true\n\n",
            work.to_string_lossy()
        ),
    )
    .expect("write schedules.toml");

    let deck = TuiDeck::builder()
        .with_pty_size(200, 50)
        // pi authenticates with this (never printed); the deck's daemon and the
        // pi child inherit it.
        .with_env(
            "ANTHROPIC_API_KEY",
            std::env::var("ANTHROPIC_API_KEY").expect("checked non-empty by check_pi_available"),
        )
        // The built binary's dir, so the extension's `dot-agent-deck
        // agent-event` resolves; the daemon-startup auto-materialize finds `pi`
        // on this PATH and installs the bundled extension into the per-test HOME.
        .with_env("PATH", path_with_binary_dir())
        .with_env("DOT_AGENT_DECK_SCHEDULES", schedules.to_string_lossy())
        .with_env("DOT_AGENT_DECK_LOG", log.to_string_lossy())
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");

    common::attach_request_on(
        deck.attach_socket_path(),
        &dot_agent_deck::daemon_protocol::AttachRequest::RunNow {
            name: SCHEDULE_NAME.to_string(),
        },
    )
    .unwrap_or_else(|e| panic!("RunNow {SCHEDULE_NAME} over the attach socket failed: {e}"));

    // 1. What the user sees: the Pi pane's card carries the prompt the deck
    //    typed in, which only Pi's own report puts there.
    assert!(
        deck.wait_for_grid_string_within("Prmt:", Duration::from_secs(120)),
        "the scheduled Pi pane's card never showed the typed-in prompt (`Prmt:`) within 120s.\n\
         Final grid:\n{}",
        deck.snapshot_grid()
    );

    // 2. The delivery was CONFIRMED by that report.
    let confirmed = "prompt delivery confirmed by the agent's submitted prompt";
    let confirmation = common::wait_for_file_containing(&log, confirmed, Duration::from_secs(60));
    let log_text = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        confirmation.is_ok(),
        "the deck never recorded the scheduled prompt as confirmed: {confirmation:?}\n\
         === deck.log (delivery lines) ===\n{}\nFinal grid:\n{}",
        log_text
            .lines()
            .filter(|line| line.contains("prompt"))
            .collect::<Vec<_>>()
            .join("\n"),
        deck.snapshot_grid()
    );
    assert!(
        !log_text.contains("delivery cannot be confirmed by this agent"),
        "the delivery was finalized as unconfirmable, so the deck still treats this Pi pane \
         as one that cannot report its prompts"
    );
    // Evidence for the confirmation-latency floor (`submission_confirmation_latency`):
    // how long Pi's report took to come back after the deck's write.
    let written = log_line_time(&log_text, "prompt written to pane; provisional");
    let confirmed_at = log_line_time(&log_text, confirmed);
    if let (Some(written), Some(confirmed_at)) = (written, confirmed_at) {
        eprintln!(
            "scheduler/pi/002: write → confirmation {} ms; re-submissions: {}",
            (confirmed_at - written).num_milliseconds(),
            log_text
                .matches("prompt delivery unconfirmed; re-submitting")
                .count()
        );
    }

    // 3. The prompt did its work.
    let sentinel = work.join(SCHEDULED_SENTINEL);
    let created = common::wait_for_file_containing(
        &sentinel,
        SCHEDULED_SENTINEL_CONTENT,
        Duration::from_secs(120),
    );
    assert!(
        created.is_ok(),
        "pi confirmed the prompt but never created {SCHEDULED_SENTINEL:?}: {created:?}\n\
         Final grid:\n{}",
        deck.snapshot_grid()
    );
}

// ---------------------------------------------------------------------------
// chain-smoke/pi/003 — a typed-in delegate, confirmed by the Pi worker's report
// ---------------------------------------------------------------------------

const ORCH_PANE: &str = "synthetic-orchestrator-pane";
const WORKER_PANE: &str = "pi-confirm-worker-pane";
const WORKER_ROLE: &str = "coder";
const DELEGATE_SENTINEL: &str = "pi_delegate_sentinel_6b1c.txt";
const DELEGATE_SENTINEL_CONTENT: &str = "PI_DELEGATE_SENTINEL_OK";
/// When the deck's first in-place re-delivery of the pointer is due, pinned so
/// the test knows: a report arriving before it stops the re-delivery, and the
/// exactly-once check runs only after it would have fired.
const FIRST_REDELIVERY: Duration = Duration::from_secs(60);
/// How long the pointer report may take. Under [`FIRST_REDELIVERY`], so a report
/// that counts arrived while the re-delivery was still pending.
const POINTER_REPORT_WAIT: Duration = Duration::from_secs(50);

/// Scenario: Spawn a real `pi` worker with no role config, so a delegate is typed into its running pane, and wait for its session-start report declaring that it reports every prompt. Delegate to it a task to create `pi_delegate_sentinel_6b1c.txt`; pi reports the typed-in pointer as its own declared prompt, creates the sentinel and signals work-done. Past the moment the deck's first re-delivery would have fired, the pointer has still been submitted exactly once.
#[spec("chain-smoke/pi/003")]
#[test]
fn chain_smoke_pi_003_typed_in_delegate_is_confirmed_by_pi_worker_report() {
    skip_unless!(check_pi_available());
    // SAFETY: a stated residual, not a proof (issue #1516), as in
    // `chain-smoke/pi/002`. Set before the tokio runtime (and so any daemon
    // worker thread) is created below, and never written again; the threads
    // that exist here are this test's and libtest's runner thread, which waits
    // for it. nextest runs each test in its own process, so this never leaks.
    unsafe {
        std::env::set_var(
            dot_agent_deck::delegate_retry::DOT_AGENT_DECK_DELEGATE_RETRY_SCHEDULE_MS,
            FIRST_REDELIVERY.as_millis().to_string(),
        );
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("build multi-thread runtime");
    rt.block_on(chain_smoke_pi_003_inner());
}

async fn chain_smoke_pi_003_inner() {
    let daemon = common::spawn_inprocess_daemon().await;
    let sub = common::EventSub::subscribe(&daemon.attach_path);

    let cwd = common::race_safe_tempdir();
    let cwd_str = cwd
        .path()
        .to_str()
        .expect("orchestration cwd is UTF-8")
        .to_string();

    // This in-process daemon bypasses the `daemon serve` startup that installs
    // the extension, so stage it into the worker's HOME (as `chain-smoke/pi/002`).
    let pi_home = common::race_safe_tempdir();
    dot_agent_deck::orchestrator_ext::materialize(
        &dot_agent_deck::orchestrator_ext::extension_dir_under(pi_home.path()),
    )
    .expect("stage the bundled pi extension into the worker HOME");

    // `SpawnOptions.env` does not go through the TuiDeck `inherit_pass`, so the
    // key is forwarded here explicitly (never printed).
    let anthropic_key =
        std::env::var("ANTHROPIC_API_KEY").expect("checked non-empty by check_pi_available");
    let pi_command = format!("pi --provider anthropic --model {PI_MODEL} --approve");
    daemon
        .registry
        .spawn_agent(SpawnOptions {
            command: Some(&pi_command),
            cwd: Some(cwd_str.as_str()),
            rows: 40,
            cols: 120,
            env: vec![
                (DOT_AGENT_DECK_PANE_ID.to_string(), WORKER_PANE.to_string()),
                (
                    "DOT_AGENT_DECK_SOCKET".to_string(),
                    daemon.hook_path.display().to_string(),
                ),
                ("PATH".to_string(), path_with_binary_dir()),
                (
                    "HOME".to_string(),
                    pi_home.path().to_str().expect("pi home UTF-8").to_string(),
                ),
                ("ANTHROPIC_API_KEY".to_string(), anthropic_key),
            ],
            agent_type: Some(AgentType::Pi),
            ..SpawnOptions::default()
        })
        .expect("spawn the real pi worker");

    let worker_pane = || {
        daemon
            .registry
            .agent_records()
            .into_iter()
            .find(|r| r.pane_id_env.as_deref() == Some(WORKER_PANE))
            .and_then(|r| daemon.registry.snapshot(&r.id).ok())
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default()
    };

    // The worker's session-start report: pi is up, and its extension declares
    // that it reports every prompt.
    let started = sub.try_wait_for(
        |e| {
            e.pane_id.as_deref() == Some(WORKER_PANE)
                && e.agent_type == AgentType::Pi
                && e.event_type == EventType::Idle
                && e.declares_prompt_reports()
        },
        Duration::from_secs(90),
    );
    assert!(
        started.is_some(),
        "the pi worker never sent a declared session-start report within 90s.\n\
         === pi worker pane ===\n{}\n=== end ===\nObserved events: {:#?}",
        worker_pane(),
        sub.snapshot()
    );

    {
        let mut st = daemon.state.write().await;
        st.pane_role_map
            .insert(ORCH_PANE.to_string(), "orchestrator".to_string());
        st.pane_role_map
            .insert(WORKER_PANE.to_string(), WORKER_ROLE.to_string());
        st.orchestrator_pane_ids.insert(ORCH_PANE.to_string());
        let orch = dot_agent_deck::state::OrchestrationIdentity::NameCwd {
            name: "pi-confirm-orchestration".to_string(),
            cwd: cwd_str.clone(),
        };
        st.pane_orchestration_map
            .insert(ORCH_PANE.to_string(), orch.clone());
        st.pane_orchestration_map
            .insert(WORKER_PANE.to_string(), orch);
        st.pane_cwd_map
            .insert(WORKER_PANE.to_string(), cwd_str.clone());
        st.pane_cwd_map
            .insert(ORCH_PANE.to_string(), cwd_str.clone());
    }

    // The same no-questions / signal-yourself clauses `chain-smoke/pi/002`
    // settled on, for the same observed stalls.
    let task = format!(
        "Create a file named {DELEGATE_SENTINEL} in the current working directory whose entire \
         contents are exactly the text {DELEGATE_SENTINEL_CONTENT}. Use your shell tool to create \
         it. That is the only work to do — then follow the \"When done\" instructions below to \
         signal completion. Do not ask any follow-up questions before doing both steps: you have \
         everything you need, so choose any value the instructions leave up to you. Do not stop \
         or reply until you have followed the \"When done\" instructions yourself and signalled \
         completion. Never describe signalling as a next step, offer to do it later, or wait for \
         confirmation."
    );
    let delegated_at = std::time::Instant::now();
    daemon
        .state
        .read()
        .await
        .handle_delegate(
            DelegateSignal {
                pane_id: ORCH_PANE.to_string(),
                task,
                to: vec![WORKER_ROLE.to_string()],
                supersede: false,
                timestamp: chrono::Utc::now(),
                token: None,
            },
            &daemon.registry,
            &daemon.event_tx,
        )
        .await;

    // 1. The worker's own report of the typed-in pointer, carrying the
    //    declaration — the proof the delegate's re-delivery waits for.
    let pointer = sub.try_wait_for(
        |e| {
            e.pane_id.as_deref() == Some(WORKER_PANE)
                && declared_prompt_report(e, &format!("worker-task-{WORKER_ROLE}.md"))
        },
        POINTER_REPORT_WAIT,
    );
    let Some(pointer) = pointer else {
        panic!(
            "the pi worker never reported the delegate's pointer as a declared prompt within \
             {POINTER_REPORT_WAIT:?}.\n=== pi worker pane ===\n{}\n=== end ===\nObserved events: {:#?}",
            worker_pane(),
            sub.snapshot()
        );
    };
    let reported = pointer.user_prompt.unwrap_or_default();
    let delivery_id = reported
        .split("[delivery ")
        .nth(1)
        .and_then(|rest| rest.split(']').next())
        .map(str::to_string)
        .unwrap_or_else(|| panic!("the reported pointer carries no delivery id: {reported:?}"));

    // 2. The worker did the task and signalled work-done.
    let sentinel = cwd.path().join(DELEGATE_SENTINEL);
    let created = common::wait_for_file_containing_async(
        &sentinel,
        DELEGATE_SENTINEL_CONTENT,
        Duration::from_secs(240),
    )
    .await;
    assert!(
        created.is_ok(),
        "the pi worker never created {DELEGATE_SENTINEL:?}: {created:?}\n\
         === pi worker pane ===\n{}\n=== end ===",
        worker_pane()
    );
    let work_done = cwd
        .path()
        .join(".dot-agent-deck")
        .join(format!("work-done-{WORKER_ROLE}.md"));
    assert!(
        common::wait_for_path_async(&work_done, Duration::from_secs(120)).await,
        "the pi worker never signalled work-done (no {work_done:?}).\n\
         === pi worker pane ===\n{}\n=== end ===",
        worker_pane()
    );

    // 3. Confirmed once, typed once: the report arrived while the first
    //    re-delivery was still pending, so it must have stopped it. Watch until
    //    past the moment that re-delivery was due; a second report of the same
    //    delivery id would be pi submitting the pointer again.
    let past_first_redelivery = (delegated_at + FIRST_REDELIVERY + Duration::from_secs(10))
        .saturating_duration_since(std::time::Instant::now());
    let second = sub.try_wait_for(
        |e| {
            e.pane_id.as_deref() == Some(WORKER_PANE)
                && e.timestamp != pointer.timestamp
                && e.user_prompt
                    .as_deref()
                    .is_some_and(|p| p.contains(&delivery_id))
        },
        past_first_redelivery,
    );
    assert!(
        second.is_none(),
        "the delegate pointer {delivery_id} was submitted a second time ({second:?}) — the \
         worker's own report should have confirmed it the first time and stopped the \
         re-delivery due {FIRST_REDELIVERY:?} after the delegate"
    );
}
