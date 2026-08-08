//! RED tests for this feature's `dot-agent-deck daemon status [--json]` — a
//! read-only diagnostic over the existing `AttachRequest::ListAgents` (see
//! `.dot-agent-deck/47-status-query-design.md` in the root checkout, and
//! `tests/CATALOG.md`'s `daemon/status` section). There is today no `status`
//! variant on `DaemonCmd` (`src/main.rs`) at all — every test below drives
//! the REAL `dot-agent-deck` binary as a subprocess, so until the subcommand
//! exists they fail via clap's own "unrecognized subcommand" error rather
//! than any assertion the tests themselves make.
//!
//! `daemon/status/001`/`002`/`004` bring up an in-process daemon
//! (`common::spawn_inprocess_daemon`) and drive a live status for a
//! `cat`-stub worker pane over the daemon's hook socket, exactly as
//! `agent-event --type running` would (`agent_event_type_from_state("running")
//! == EventType::Thinking`, `src/event.rs`) — except routed as a raw
//! `AgentEvent` so `daemon/status/004` can additionally carry a `user_prompt`
//! the `agent-event` CLI itself never sets. `daemon/status/003` queries a
//! scratch attach-socket path with nothing listening.

use std::path::Path;
use std::time::Duration;

use dot_agent_deck::agent_pty::{DOT_AGENT_DECK_PANE_ID, SpawnOptions};
use dot_agent_deck::event::{AgentEvent, AgentType, EventType};
#[cfg(unix)]
use spec::spec;

mod common;

#[cfg(unix)]
const DRIVEN_PANE: &str = "status-driven-pane-7f3a1c";
#[cfg(unix)]
const CONTROL_PANE: &str = "status-control-pane-9c1e4d";
#[cfg(unix)]
const JSON_PANE: &str = "status-json-pane-2b6e51";
#[cfg(unix)]
const LEAK_PANE: &str = "status-leak-pane-4d8f02";
#[cfg(unix)]
const PROMPT_SENTINEL: &str = "DAEMON-STATUS-PROMPT-LEAK-SENTINEL-9f2a1c";

/// Build the same raw `AgentEvent` the `agent-event --type running` CLI path
/// sends (`agent_event_type_from_state("running") == EventType::Thinking`),
/// except written directly to the hook socket so an optional `user_prompt`
/// can ride along — the `agent-event` subcommand always hardcodes it to
/// `None` (`src/main.rs`).
#[cfg(unix)]
fn thinking_event(pane_id: &str, agent_id: &str, user_prompt: Option<&str>) -> AgentEvent {
    AgentEvent {
        session_id: format!("status-thinking-{agent_id}"),
        agent_type: AgentType::Pi,
        event_type: EventType::Thinking,
        tool_name: None,
        tool_detail: None,
        cwd: None,
        timestamp: chrono::Utc::now(),
        user_prompt: user_prompt.map(str::to_string),
        metadata: std::collections::HashMap::new(),
        pane_id: Some(pane_id.to_string()),
        agent_id: Some(agent_id.to_string()),
        agent_version: None,
        schema_version: None,
        live_target: None,
    }
}

/// Poll `daemon.state.sessions` until an entry matches BOTH `agent_id` and
/// `pane_id` — the same `(agent_id, pane_id_env)` join `ListAgents` uses to
/// populate `AgentRecord.live` (`src/daemon_protocol.rs`). Driving the event
/// through the hook socket is fire-and-forget, so the caller cannot invoke
/// the CLI until the daemon has actually applied it.
#[cfg(unix)]
async fn wait_for_live_session(
    daemon: &common::InProcDaemon,
    pane_id: &str,
    agent_id: &str,
    timeout: Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        {
            let state = daemon.state.read().await;
            if state.sessions.values().any(|session| {
                session.agent_id.as_deref() == Some(agent_id)
                    && session.pane_id.as_deref() == Some(pane_id)
            }) {
                return;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no session for agent {agent_id:?} on pane {pane_id:?} appeared within {timeout:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(unix)]
struct CliStatusResult {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

/// Run the REAL `dot-agent-deck daemon status [--json]` CLI as a subprocess
/// against `attach_socket`, exactly as a caller on the host would —
/// `DOT_AGENT_DECK_ATTACH_SOCKET` is the only thing that redirects it away
/// from the real per-user daemon (`config::attach_socket_path`).
#[cfg(unix)]
async fn run_daemon_status_cli(attach_socket: &Path, json: bool) -> CliStatusResult {
    let attach_socket = attach_socket.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"));
        cmd.arg("daemon").arg("status");
        if json {
            cmd.arg("--json");
        }
        cmd.env("DOT_AGENT_DECK_ATTACH_SOCKET", &attach_socket);
        let output = cmd
            .output()
            .expect("run the real `dot-agent-deck daemon status` CLI as a subprocess");
        CliStatusResult {
            status: output.status,
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    })
    .await
    .expect("daemon status CLI subprocess task did not panic")
}

/// Scenario: Bring up an in-process daemon with two `cat`-stub worker panes registered as managed — one driven to a live `Thinking` status over the hook socket, the other left untouched as a same-daemon control. Run the REAL `dot-agent-deck daemon status` CLI as a subprocess against the daemon's attach socket. Assert it names both agents by pane id and that the driven agent's output line differs from the control agent's once the pane id itself is normalized out, proving the command surfaces the live status rather than an identical placeholder.
#[spec("daemon/status/001")]
#[test]
#[cfg(unix)]
fn daemon_status_001_reports_live_agent_status() {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build daemon-status live-state runtime")
        .block_on(daemon_status_001_reports_live_agent_status_inner());
}

#[cfg(unix)]
async fn daemon_status_001_reports_live_agent_status_inner() {
    let daemon = common::spawn_inprocess_daemon().await;
    let cwd = common::race_safe_tempdir();
    let cwd_str = cwd.path().to_string_lossy().into_owned();

    {
        let mut state = daemon.state.write().await;
        state.register_pane(DRIVEN_PANE.to_string());
    }

    let driven_agent_id = daemon
        .registry
        .spawn_agent(SpawnOptions {
            command: Some("cat"),
            cwd: Some(&cwd_str),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), DRIVEN_PANE.to_string())],
            ..SpawnOptions::default()
        })
        .expect("spawn driven worker stub");
    let control_agent_id = daemon
        .registry
        .spawn_agent(SpawnOptions {
            command: Some("cat"),
            cwd: Some(&cwd_str),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), CONTROL_PANE.to_string())],
            ..SpawnOptions::default()
        })
        .expect("spawn control worker stub");

    common::write_hook_line(
        &daemon.hook_path,
        &serde_json::to_string(&thinking_event(DRIVEN_PANE, &driven_agent_id, None))
            .expect("serialize driven Thinking event"),
    )
    .expect("write driven Thinking event");
    wait_for_live_session(
        &daemon,
        DRIVEN_PANE,
        &driven_agent_id,
        Duration::from_secs(5),
    )
    .await;

    let result = run_daemon_status_cli(&daemon.attach_path, false).await;
    assert!(
        result.status.success(),
        "`daemon status` must succeed against a live daemon with a managed agent; got \
         status={:?} stdout={:?} stderr={:?}",
        result.status,
        result.stdout,
        result.stderr
    );

    let driven_lines: Vec<&str> = result
        .stdout
        .lines()
        .filter(|line| line.contains(DRIVEN_PANE))
        .collect();
    let control_lines: Vec<&str> = result
        .stdout
        .lines()
        .filter(|line| line.contains(CONTROL_PANE))
        .collect();
    assert!(
        !driven_lines.is_empty(),
        "`daemon status` output never named the driven agent by its pane id {DRIVEN_PANE:?}; \
         stdout={:?}",
        result.stdout
    );
    assert!(
        !control_lines.is_empty(),
        "`daemon status` output never named the control agent by its pane id {CONTROL_PANE:?}; \
         stdout={:?}",
        result.stdout
    );

    // Normalize out BOTH identity fields (pane id and registry agent id —
    // the latter differs per spawn regardless of live status) so any
    // remaining textual difference is attributable to the status-derived
    // fields the the design rationale scopes `record.live` to (hook-session
    // id, status, active tool, last activity), not merely to two agents
    // having distinct identities.
    let driven_text = driven_lines
        .join("\n")
        .replace(DRIVEN_PANE, "<pane>")
        .replace(&driven_agent_id, "<agent>");
    let control_text = control_lines
        .join("\n")
        .replace(CONTROL_PANE, "<pane>")
        .replace(&control_agent_id, "<agent>");
    assert_ne!(
        driven_text, control_text,
        "the driven agent's row must visibly differ from the untouched control agent's row \
         once each row's own pane id AND registry agent id are normalized out — otherwise \
         `daemon status` is printing an identical placeholder for both instead of actually \
         showing a status; driven={:?} control={:?}",
        driven_text, control_text
    );

    daemon.registry.shutdown_all();
}

/// Scenario: Same in-process daemon and driven `Thinking` status as `daemon/status/001`, but invoke `dot-agent-deck daemon status --json`. Assert the CLI succeeds, its stdout parses as a JSON object, the parsed document carries a `schema_version` key, and the raw JSON text names the managed agent by pane id — without pinning any other field name.
#[spec("daemon/status/002")]
#[test]
#[cfg(unix)]
fn daemon_status_002_json_output_lists_the_managed_agent() {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build daemon-status json runtime")
        .block_on(daemon_status_002_json_output_lists_the_managed_agent_inner());
}

#[cfg(unix)]
async fn daemon_status_002_json_output_lists_the_managed_agent_inner() {
    let daemon = common::spawn_inprocess_daemon().await;
    let cwd = common::race_safe_tempdir();
    let cwd_str = cwd.path().to_string_lossy().into_owned();

    {
        let mut state = daemon.state.write().await;
        state.register_pane(JSON_PANE.to_string());
    }
    let agent_id = daemon
        .registry
        .spawn_agent(SpawnOptions {
            command: Some("cat"),
            cwd: Some(&cwd_str),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), JSON_PANE.to_string())],
            ..SpawnOptions::default()
        })
        .expect("spawn JSON-test worker stub");

    common::write_hook_line(
        &daemon.hook_path,
        &serde_json::to_string(&thinking_event(JSON_PANE, &agent_id, None))
            .expect("serialize Thinking event"),
    )
    .expect("write Thinking event");
    wait_for_live_session(&daemon, JSON_PANE, &agent_id, Duration::from_secs(5)).await;

    let result = run_daemon_status_cli(&daemon.attach_path, true).await;
    assert!(
        result.status.success(),
        "`daemon status --json` must succeed against a live daemon with a managed agent; got \
         status={:?} stdout={:?} stderr={:?}",
        result.status,
        result.stdout,
        result.stderr
    );

    let parsed: serde_json::Value =
        serde_json::from_str(result.stdout.trim()).unwrap_or_else(|e| {
            panic!(
                "`daemon status --json` stdout did not parse as JSON: {e}; stdout={:?}",
                result.stdout
            )
        });
    assert!(
        parsed.is_object(),
        "`daemon status --json` must emit a top-level JSON object; got {parsed:?}"
    );
    assert!(
        parsed.get("schema_version").is_some(),
        "`daemon status --json` must carry a `schema_version` field (the design rationale); \
         got {parsed:?}"
    );
    assert!(
        result.stdout.contains(JSON_PANE),
        "`daemon status --json` output has no entry naming the managed agent's pane id \
         {JSON_PANE:?}; stdout={:?}",
        result.stdout
    );

    daemon.registry.shutdown_all();
}

/// Scenario: With no daemon reachable at a fresh, never-bound attach-socket path, run the REAL `dot-agent-deck daemon status` CLI as a subprocess. Assert it fails without panicking, that the failure is distinguishable from clap's own "unrecognized subcommand" usage error, and that the queried socket path still does not exist afterward — a diagnostic must never bring the thing it diagnoses into existence.
#[spec("daemon/status/003")]
#[test]
#[cfg(unix)]
fn daemon_status_003_no_daemon_reports_unavailable_without_spawning_one() {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build no-daemon status runtime")
        .block_on(daemon_status_003_no_daemon_reports_unavailable_without_spawning_one_inner());
}

#[cfg(unix)]
async fn daemon_status_003_no_daemon_reports_unavailable_without_spawning_one_inner() {
    common::init_test_env();
    let scratch = common::race_safe_tempdir();
    let attach_path = scratch.path().join("never-bound-attach.sock");
    assert!(
        !attach_path.exists(),
        "test precondition violated: the scratch attach socket path must not already exist"
    );

    let result = run_daemon_status_cli(&attach_path, false).await;

    assert!(
        !result.status.success(),
        "`daemon status` against an unreachable daemon must not report success; got \
         status={:?} stdout={:?} stderr={:?}",
        result.status,
        result.stdout,
        result.stderr
    );
    assert!(
        !result.stderr.contains("panicked"),
        "an unreachable daemon must produce a controlled diagnostic, not a Rust panic; \
         stderr={:?}",
        result.stderr
    );
    // clap's own invalid-subcommand error always exits 2 and prints a
    // `Usage:` banner naming the parent command — that is the RED-today
    // failure shape, since `status` is not yet a `DaemonCmd` variant
    // (`src/main.rs`). Once it is a real subcommand, neither can appear
    // here again, so both checks stay meaningful post-implementation: they
    // rule out "this build's CLI does not understand the request" as the
    // reason, keeping it distinguishable from a genuinely-handled
    // "no daemon reachable" outcome.
    assert_ne!(
        result.status.code(),
        Some(2),
        "exit code 2 is clap's own generic usage/parse-error code; an implemented `daemon \
         status` reporting a genuinely unreachable daemon must use a code that does not \
         collide with it, or a caller cannot tell 'no daemon' apart from 'this build does not \
         understand the request'; status={:?} stdout={:?} stderr={:?}",
        result.status,
        result.stdout,
        result.stderr
    );
    assert!(
        !result.stderr.contains("Usage:"),
        "stderr still carries clap's own subcommand-usage banner, meaning `status` was not \
         recognized as a real `daemon` subcommand rather than being handled and reported as \
         unavailable; stderr={:?}",
        result.stderr
    );
    assert!(
        !attach_path.exists(),
        "a diagnostic query must never itself bring a daemon into existence at the socket path \
         it queried (the design rationale's non-perturbation contract); the socket now exists \
         at {}",
        attach_path.display()
    );
}

/// Scenario: Drive a live status for a managed agent whose session carries a distinctive sentinel in `last_user_prompt`/`first_prompts` (seeded via a raw hook `AgentEvent`, since the `agent-event` CLI itself never sets `user_prompt`). Run the REAL `dot-agent-deck daemon status` CLI as a subprocess. Assert it succeeds and the sentinel never appears anywhere in its combined stdout/stderr.
#[spec("daemon/status/004")]
#[test]
#[cfg(unix)]
fn daemon_status_004_human_output_never_leaks_prompt_text() {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build prompt-leak status runtime")
        .block_on(daemon_status_004_human_output_never_leaks_prompt_text_inner());
}

#[cfg(unix)]
async fn daemon_status_004_human_output_never_leaks_prompt_text_inner() {
    let daemon = common::spawn_inprocess_daemon().await;
    let cwd = common::race_safe_tempdir();
    let cwd_str = cwd.path().to_string_lossy().into_owned();

    {
        let mut state = daemon.state.write().await;
        state.register_pane(LEAK_PANE.to_string());
    }
    let agent_id = daemon
        .registry
        .spawn_agent(SpawnOptions {
            command: Some("cat"),
            cwd: Some(&cwd_str),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), LEAK_PANE.to_string())],
            ..SpawnOptions::default()
        })
        .expect("spawn prompt-leak worker stub");

    common::write_hook_line(
        &daemon.hook_path,
        &serde_json::to_string(&thinking_event(LEAK_PANE, &agent_id, Some(PROMPT_SENTINEL)))
            .expect("serialize sentinel-carrying Thinking event"),
    )
    .expect("write sentinel-carrying Thinking event");
    wait_for_live_session(&daemon, LEAK_PANE, &agent_id, Duration::from_secs(5)).await;
    {
        let state = daemon.state.read().await;
        let seeded = state.sessions.values().any(|session| {
            session.agent_id.as_deref() == Some(agent_id.as_str())
                && session.last_user_prompt.as_deref() == Some(PROMPT_SENTINEL)
        });
        assert!(
            seeded,
            "test precondition: the sentinel never made it into `last_user_prompt`"
        );
    }

    let result = run_daemon_status_cli(&daemon.attach_path, false).await;
    assert!(
        result.status.success(),
        "`daemon status` must succeed against a live daemon with a managed agent; got \
         status={:?} stdout={:?} stderr={:?}",
        result.status,
        result.stdout,
        result.stderr
    );
    assert!(
        !result.stdout.contains(PROMPT_SENTINEL) && !result.stderr.contains(PROMPT_SENTINEL),
        "`daemon status` output leaked prompt text (the design rationale: \"do not print prompt \
         text in the human view\"); stdout={:?} stderr={:?}",
        result.stdout,
        result.stderr
    );

    daemon.registry.shutdown_all();
}
