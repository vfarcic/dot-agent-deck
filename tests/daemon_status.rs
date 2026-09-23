//! Real-subprocess coverage for `dot-agent-deck daemon status [--json]`, a
//! read-only diagnostic over `AttachRequest::ListAgents` (see
//! `tests/CATALOG.md`'s `daemon/status` section).
//!
//! `daemon/status/001` and `/004` use raw `AgentEvent`s to seed fields the
//! lifecycle CLI cannot carry. `/002` and `/005` exercise the real
//! `agent-event --type running` subprocess with the pane/agent environment a
//! daemon-managed spawn receives. `/003` queries a scratch attach-socket path
//! with nothing listening, and `/006` queries one where a stub peer accepts
//! the connection and then never replies — the only staging that reaches the
//! [`STATUS_REQUEST_TIMEOUT`] deadline, since `/003`'s path fails at
//! `connect(2)` long before it.

use std::path::Path;
use std::time::Duration;

use dot_agent_deck::agent_pty::{
    DOT_AGENT_DECK_AGENT_ID, DOT_AGENT_DECK_PANE_ID, SpawnOptions, TabMembership,
};
use dot_agent_deck::daemon_client::{DaemonClient, StartAgentOptions};
#[cfg(unix)]
use dot_agent_deck::daemon_status::STATUS_REQUEST_TIMEOUT;
use dot_agent_deck::event::{AgentEvent, AgentType, BroadcastMsg, EventType};
use dot_agent_deck::state::SessionStatus;
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
const JSON_LABEL: &str = "status-json-label-4c8f7a";
#[cfg(unix)]
const JSON_MODE: &str = "status-json-mode-5d9a2b";
#[cfg(unix)]
const JSON_TOOL: &str = "Read";
#[cfg(unix)]
const LEAK_PANE: &str = "status-leak-pane-4d8f02";
#[cfg(unix)]
const PROMPT_SENTINEL: &str = "DAEMON-STATUS-PROMPT-LEAK-SENTINEL-9f2a1c";
#[cfg(unix)]
const TOOL_DETAIL_SENTINEL: &str = "DAEMON-STATUS-TOOL-DETAIL-SENTINEL-7c4b2e";
#[cfg(unix)]
const CLI_DRIVEN_PANE: &str = "status-cli-driven-pane-6a8d31";
#[cfg(unix)]
const CLI_CONTROL_PANE: &str = "status-cli-control-pane-8c2f47";

/// Absolute ceiling on ONE wedged-peer `daemon status` run, watchdogged in
/// this test rather than trusted to the binary.
///
/// Deliberately a literal, and deliberately NOT derived from
/// [`STATUS_REQUEST_TIMEOUT`]: a bound that follows the constant it is
/// guarding would sail through a build that raised that constant to a minute,
/// which is half of what `daemon/status/006` exists to catch. The other half —
/// a build that dropped the deadline entirely — hangs forever, so without a
/// watchdog the test would HANG rather than fail, which reports nothing to
/// anyone.
///
/// 8s leaves the passing run (~3s of deadline plus subprocess startup, both
/// modes concurrent against one stub) a wide margin while keeping even the
/// failing run inside the fast tier's budget.
#[cfg(unix)]
const WEDGED_PEER_DEADLINE: std::time::Duration = std::time::Duration::from_secs(8);

/// Slack over [`STATUS_REQUEST_TIMEOUT`] allowed for forking the debug binary,
/// dynamically linking it, and CI scheduling noise before the deadline it
/// declares can even start running.
///
/// Paired with [`WEDGED_PEER_DEADLINE`], not redundant with it: this one is
/// relative, and catches a build whose code no longer honours the constant it
/// still declares (a diagnostic that says "no response within 3s" after
/// waiting six is lying to the operator reading it).
#[cfg(unix)]
const WEDGED_PEER_SPAWN_SLACK: std::time::Duration = std::time::Duration::from_secs(4);

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

/// A peer bound to an attach-socket path that ACCEPTS every connection and
/// then never writes a byte back.
///
/// A peer that is merely ABSENT does not reach [`STATUS_REQUEST_TIMEOUT`],
/// which is what issue #458 is about: `daemon/status/003` points the CLI at a
/// path with nothing bound, and that fails at `connect(2)` in microseconds and
/// takes the transport-error branch. Accepting and then staying silent is the
/// cheap, deterministic way to hold the client at the deadline instead — a
/// real daemon can of course get there by simply being slow, but nothing in
/// the suite can stage that on demand.
///
/// The accepted streams are HELD in `held` rather than dropped, and that is
/// load-bearing: dropping one closes the connection, the client reads EOF, and
/// `run_daemon_status_cli` reports `unavailable (<transport error>)` from its
/// `Ok(Err(_))` arm instead of the timeout arm — a green-looking run of the
/// wrong branch.
#[cfg(unix)]
struct WedgedPeer {
    path: std::path::PathBuf,
    held: std::sync::Arc<tokio::sync::Mutex<Vec<tokio::net::UnixStream>>>,
    acceptor: tokio::task::JoinHandle<()>,
}

#[cfg(unix)]
impl WedgedPeer {
    /// Bind the stub inside a scratch directory. The caller owns the path, so
    /// this never touches the per-user attach socket a real daemon on this
    /// machine is listening on.
    async fn bind(path: std::path::PathBuf) -> Self {
        let listener = tokio::net::UnixListener::bind(&path)
            .unwrap_or_else(|e| panic!("bind the wedged stub listener at {}: {e}", path.display()));
        let held = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let accepted = std::sync::Arc::clone(&held);
        let acceptor = tokio::spawn(async move {
            while let Ok((stream, _addr)) = listener.accept().await {
                accepted.lock().await.push(stream);
            }
        });
        Self {
            path,
            held,
            acceptor,
        }
    }

    /// How many connections the stub has accepted and is still holding open.
    async fn connections_accepted(&self) -> usize {
        self.held.lock().await.len()
    }

    /// Tear the stub down through its own handle: stop the acceptor, close
    /// every held connection, and unlink the socket inode (binding a Unix
    /// socket creates a file that closing the listener does not remove).
    async fn shutdown(self) {
        let Self {
            path,
            held,
            acceptor,
        } = self;
        acceptor.abort();
        let _ = acceptor.await;
        drop(held);
        let _ = std::fs::remove_file(&path);
    }
}

/// One watchdogged CLI run: what the subprocess reported, and how long the
/// caller actually waited for it.
#[cfg(unix)]
struct TimedCliRun {
    result: CliStatusResult,
    elapsed: std::time::Duration,
}

/// Run the REAL `dot-agent-deck daemon status [--json]` CLI against
/// `attach_socket` under this test's own wall-clock watchdog.
///
/// `kill_on_drop` is what makes the watchdog real rather than decorative: on
/// expiry `tokio::time::timeout` drops the wait future, which drops the
/// [`tokio::process::Child`] it owns, which sends the child a `SIGKILL`.
/// Without it a build that lost its deadline would leave a `daemon status`
/// process blocked on the stub's socket for the rest of the test binary's
/// life.
#[cfg(unix)]
async fn run_daemon_status_cli_bounded(
    attach_socket: &Path,
    json: bool,
    ceiling: std::time::Duration,
) -> TimedCliRun {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"));
    cmd.arg("daemon").arg("status");
    if json {
        cmd.arg("--json");
    }
    cmd.env("DOT_AGENT_DECK_ATTACH_SOCKET", attach_socket)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let started = std::time::Instant::now();
    let child = cmd
        .spawn()
        .expect("spawn the real `dot-agent-deck daemon status` CLI as a subprocess");
    let output = tokio::time::timeout(ceiling, child.wait_with_output())
        .await
        .unwrap_or_else(|_| {
            panic!(
                "`dot-agent-deck daemon status{}` did not return within {ceiling:?} against a \
                 peer that accepted the connection and never replied. The command bounds its \
                 whole connect+request round trip with STATUS_REQUEST_TIMEOUT ({}s, \
                 `src/daemon_status.rs`); it is now either gone, raised past this test's own \
                 ceiling, or no longer wrapping this path, so `daemon status` hangs on a \
                 wedged daemon instead of reporting it",
                if json { " --json" } else { "" },
                STATUS_REQUEST_TIMEOUT.as_secs(),
            )
        })
        .expect("collect the daemon status CLI subprocess output");
    TimedCliRun {
        result: CliStatusResult {
            status: output.status,
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        },
        elapsed: started.elapsed(),
    }
}

/// Wait until the in-process daemon's attach endpoint accepts connections.
/// `spawn_inprocess_daemon` proves hook-socket readiness, but the attach bind
/// happens immediately afterward and a TUI-style `StartAgent` must not race it.
#[cfg(unix)]
async fn wait_for_attach_socket(attach_socket: &Path, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if tokio::net::UnixStream::connect(attach_socket).await.is_ok() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "in-process daemon attach socket {} was not accepting connections within {timeout:?}",
            attach_socket.display()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Spawn a pane through the same attach request the TUI uses. The registry
/// injects `DOT_AGENT_DECK_AGENT_ID`; the returned id is then replayed into the
/// real `agent-event` subprocess exactly as it is inside the managed pane.
#[cfg(unix)]
async fn start_tui_managed_agent(
    daemon: &common::InProcDaemon,
    pane_id: &str,
    cwd: &str,
) -> String {
    wait_for_attach_socket(&daemon.attach_path, Duration::from_secs(5)).await;
    DaemonClient::new(daemon.attach_path.clone())
        .start_agent(StartAgentOptions {
            command: Some("cat".to_string()),
            cwd: Some(cwd.to_string()),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane_id.to_string())],
            agent_type: Some(AgentType::Pi),
            ..StartAgentOptions::default()
        })
        .await
        .expect("spawn TUI-managed agent through the daemon attach socket")
}

/// Invoke the REAL `agent-event` CLI and wait until the in-process daemon has
/// received its raw `AgentEvent`. Observing the broadcast proves the
/// fire-and-forget subprocess reached the daemon before status is queried.
#[cfg(unix)]
async fn run_agent_event_cli(
    daemon: &common::InProcDaemon,
    pane_id: &str,
    agent_id: &str,
    cwd: &Path,
) -> AgentEvent {
    let mut events = daemon.event_tx.subscribe();
    let hook_path = daemon.hook_path.clone();
    let pane_id_owned = pane_id.to_string();
    let agent_id_owned = agent_id.to_string();
    let cwd = cwd.to_path_buf();
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
            .arg("agent-event")
            .arg("--type")
            .arg("running")
            .current_dir(&cwd)
            .env_clear()
            .env("HOME", &cwd)
            .env("DOT_AGENT_DECK_SOCKET", &hook_path)
            .env(DOT_AGENT_DECK_PANE_ID, &pane_id_owned)
            .env(DOT_AGENT_DECK_AGENT_ID, &agent_id_owned)
            .output()
            .expect("run the real `dot-agent-deck agent-event --type running` CLI")
    })
    .await
    .expect("agent-event CLI subprocess task did not panic");
    assert!(
        output.status.success(),
        "`agent-event --type running` must reach the in-process daemon; status={:?} stdout={:?} stderr={:?}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let observed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await {
                Ok(BroadcastMsg::Event(event))
                    if event.pane_id.as_deref() == Some(pane_id)
                        && event.agent_id.as_deref() == Some(agent_id) =>
                {
                    break event;
                }
                Ok(_) => continue,
                Err(error) => panic!("daemon event broadcast closed before CLI event: {error}"),
            }
        }
    })
    .await
    .expect("daemon did not broadcast the real CLI event within 5s");
    assert_eq!(
        observed.event_type,
        EventType::Thinking,
        "the real `running` CLI event must carry the Thinking lifecycle state"
    );
    observed
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
    // remaining textual difference must come from the documented live
    // diagnostic columns (status and active tool), not merely from two agents
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

/// Scenario: Spawn a fully described managed pane through the daemon attach API, drive it through the real `agent-event --type running` CLI and into an active tool, then invoke `dot-agent-deck daemon status --json`. Assert the document pins the exact current schema version, every public field in a populated agent row, and all six supported live-status strings.
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
    wait_for_attach_socket(&daemon.attach_path, Duration::from_secs(5)).await;
    let agent_id = DaemonClient::new(daemon.attach_path.clone())
        .start_agent(StartAgentOptions {
            command: Some("cat".to_string()),
            cwd: Some(cwd_str.clone()),
            display_name: Some(JSON_LABEL.to_string()),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), JSON_PANE.to_string())],
            tab_membership: Some(TabMembership::Mode {
                name: JSON_MODE.to_string(),
            }),
            agent_type: Some(AgentType::Pi),
            ..StartAgentOptions::default()
        })
        .await
        .expect("spawn fully described pane through the TUI's StartAgent attach path");
    let observed = run_agent_event_cli(&daemon, JSON_PANE, &agent_id, cwd.path()).await;
    assert_eq!(observed.pane_id.as_deref(), Some(JSON_PANE));
    assert_eq!(observed.agent_id.as_deref(), Some(agent_id.as_str()));

    let mut tool_event = thinking_event(JSON_PANE, &agent_id, None);
    tool_event.session_id = format!("{JSON_PANE}-session");
    tool_event.event_type = EventType::ToolStart;
    tool_event.tool_name = Some(JSON_TOOL.to_string());
    common::write_hook_line(
        &daemon.hook_path,
        &serde_json::to_string(&tool_event).expect("serialize populated schema-row ToolStart"),
    )
    .expect("write populated schema-row ToolStart to the daemon hook socket");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let populated = {
            let state = daemon.state.read().await;
            state.sessions.values().any(|session| {
                session.agent_id.as_deref() == Some(agent_id.as_str())
                    && session.pane_id.as_deref() == Some(JSON_PANE)
                    && session.status == SessionStatus::Working
                    && session
                        .active_tool
                        .as_ref()
                        .is_some_and(|tool| tool.name == JSON_TOOL)
            })
        };
        if populated {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the representative schema row never reached Working with active tool {JSON_TOOL:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

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
    let document = parsed
        .as_object()
        .unwrap_or_else(|| panic!("`daemon status --json` must emit an object; got {parsed:?}"));
    assert_eq!(
        document
            .get("schema_version")
            .and_then(serde_json::Value::as_u64),
        Some(2),
        "the document must publish the expected current schema version; got {parsed:?}"
    );
    let top_level_fields: std::collections::BTreeSet<&str> =
        document.keys().map(String::as_str).collect();
    assert_eq!(
        top_level_fields,
        std::collections::BTreeSet::from(["agents", "schema_version"]),
        "the current schema version must pin the top-level JSON field names; got {parsed:?}"
    );
    let agents = document
        .get("agents")
        .and_then(serde_json::Value::as_array)
        .unwrap_or_else(|| panic!("the pinned `agents` field must be an array; got {parsed:?}"));
    let agent = agents
        .iter()
        .find(|entry| entry.get("pane_id").and_then(serde_json::Value::as_str) == Some(JSON_PANE))
        .unwrap_or_else(|| panic!("no JSON agent entry named pane {JSON_PANE:?}; got {parsed:?}"));
    let agent_fields: std::collections::BTreeSet<&str> = agent
        .as_object()
        .expect("each `agents` entry must be an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        agent_fields,
        std::collections::BTreeSet::from([
            "active_tool",
            "agent_id",
            "cwd",
            "label",
            "pane_id",
            "role",
            "status",
        ]),
        "the current schema version must pin every public field name on a fully populated agent row; got agent={agent:?}"
    );
    assert_eq!(
        agent,
        &serde_json::json!({
            "active_tool": { "name": JSON_TOOL },
            "agent_id": agent_id,
            "cwd": cwd_str,
            "label": JSON_LABEL,
            "pane_id": JSON_PANE,
            "role": format!("mode:{JSON_MODE}"),
            "status": "Working",
        }),
        "the representative row must pin the value and shape of every public field"
    );

    for (status, expected) in [
        (SessionStatus::Thinking, "Thinking"),
        (SessionStatus::Working, "Working"),
        (SessionStatus::Compacting, "Compacting"),
        (SessionStatus::WaitingForInput, "WaitingForInput"),
        (SessionStatus::Idle, "Idle"),
        (SessionStatus::Error, "Error"),
    ] {
        assert_eq!(
            serde_json::to_value(status).expect("serialize supported public status"),
            serde_json::Value::String(expected.to_string()),
            "the public schema must keep the exact status string {expected:?}"
        );
    }

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
         it queried; the socket now exists at {}",
        attach_path.display()
    );
}

/// Scenario: Drive a managed agent into a live tool call whose event carries distinctive prompt and tool-detail sentinels, then run both forms of the real `dot-agent-deck daemon status` CLI. Assert neither the human table nor the JSON document reveals either private value.
#[spec("daemon/status/004")]
#[test]
#[cfg(unix)]
fn daemon_status_004_outputs_never_leak_prompt_or_tool_detail() {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build prompt-leak status runtime")
        .block_on(daemon_status_004_outputs_never_leak_prompt_or_tool_detail_inner());
}

#[cfg(unix)]
async fn daemon_status_004_outputs_never_leak_prompt_or_tool_detail_inner() {
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

    let mut event = thinking_event(LEAK_PANE, &agent_id, Some(PROMPT_SENTINEL));
    event.event_type = EventType::ToolStart;
    event.tool_name = Some("Read".to_string());
    event.tool_detail = Some(TOOL_DETAIL_SENTINEL.to_string());
    common::write_hook_line(
        &daemon.hook_path,
        &serde_json::to_string(&event).expect("serialize sentinel-carrying ToolStart event"),
    )
    .expect("write sentinel-carrying ToolStart event");
    wait_for_live_session(&daemon, LEAK_PANE, &agent_id, Duration::from_secs(5)).await;
    {
        let state = daemon.state.read().await;
        let seeded = state.sessions.values().any(|session| {
            session.agent_id.as_deref() == Some(agent_id.as_str())
                && session.last_user_prompt.as_deref() == Some(PROMPT_SENTINEL)
                && session
                    .active_tool
                    .as_ref()
                    .and_then(|tool| tool.detail.as_deref())
                    == Some(TOOL_DETAIL_SENTINEL)
        });
        assert!(
            seeded,
            "test precondition: the prompt and tool-detail sentinels never reached live state"
        );
    }

    let human = run_daemon_status_cli(&daemon.attach_path, false).await;
    assert!(
        human.status.success(),
        "`daemon status` must succeed against a live daemon with a managed agent; got \
         status={:?} stdout={:?} stderr={:?}",
        human.status,
        human.stdout,
        human.stderr
    );
    assert!(
        !human.stdout.contains(PROMPT_SENTINEL)
            && !human.stderr.contains(PROMPT_SENTINEL)
            && !human.stdout.contains(TOOL_DETAIL_SENTINEL)
            && !human.stderr.contains(TOOL_DETAIL_SENTINEL),
        "human `daemon status` output leaked private prompt/tool detail; stdout={:?} stderr={:?}",
        human.stdout,
        human.stderr
    );

    let json = run_daemon_status_cli(&daemon.attach_path, true).await;
    assert!(
        json.status.success(),
        "`daemon status --json` must succeed against a live daemon with a managed agent; got status={:?} stdout={:?} stderr={:?}",
        json.status,
        json.stdout,
        json.stderr
    );
    assert!(
        !json.stdout.contains(PROMPT_SENTINEL)
            && !json.stderr.contains(PROMPT_SENTINEL)
            && !json.stdout.contains(TOOL_DETAIL_SENTINEL)
            && !json.stderr.contains(TOOL_DETAIL_SENTINEL),
        "`daemon status --json` leaked private prompt/tool detail; prompt_sentinel_present={} tool_detail_sentinel_present={} stdout={:?} stderr={:?}",
        json.stdout.contains(PROMPT_SENTINEL) || json.stderr.contains(PROMPT_SENTINEL),
        json.stdout.contains(TOOL_DETAIL_SENTINEL) || json.stderr.contains(TOOL_DETAIL_SENTINEL),
        json.stdout,
        json.stderr
    );

    daemon.registry.shutdown_all();
}

/// Scenario: Spawn two managed panes through the same attach request the TUI uses, then run the real `agent-event --type running` CLI from one pane with the daemon-injected pane and agent ids. Assert the human and JSON status commands distinguish that live agent from the untouched control and carry a live status instead of the placeholder.
#[spec("daemon/status/005")]
#[test]
#[cfg(unix)]
fn daemon_status_005_real_agent_event_cli_joins_live_state() {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build real-agent-event daemon-status runtime")
        .block_on(daemon_status_005_real_agent_event_cli_joins_live_state_inner());
}

#[cfg(unix)]
async fn daemon_status_005_real_agent_event_cli_joins_live_state_inner() {
    let daemon = common::spawn_inprocess_daemon().await;
    let cwd = common::race_safe_tempdir();
    let cwd_str = cwd.path().to_string_lossy().into_owned();
    let driven_agent_id = start_tui_managed_agent(&daemon, CLI_DRIVEN_PANE, &cwd_str).await;
    let control_agent_id = start_tui_managed_agent(&daemon, CLI_CONTROL_PANE, &cwd_str).await;

    let observed =
        run_agent_event_cli(&daemon, CLI_DRIVEN_PANE, &driven_agent_id, cwd.path()).await;

    let human = run_daemon_status_cli(&daemon.attach_path, false).await;
    assert!(
        human.status.success(),
        "`daemon status` failed after a real agent-event; status={:?} stdout={:?} stderr={:?}",
        human.status,
        human.stdout,
        human.stderr
    );
    let driven_lines: Vec<&str> = human
        .stdout
        .lines()
        .filter(|line| line.contains(CLI_DRIVEN_PANE))
        .collect();
    let control_lines: Vec<&str> = human
        .stdout
        .lines()
        .filter(|line| line.contains(CLI_CONTROL_PANE))
        .collect();
    assert!(
        !driven_lines.is_empty() && !control_lines.is_empty(),
        "human status output must name both TUI-spawned panes; stdout={:?}",
        human.stdout
    );
    let normalize_identity_fields = |lines: &[&str], pane_id: &str, agent_id: &str| {
        lines
            .iter()
            .flat_map(|line| line.split_whitespace())
            .filter(|field| *field != pane_id && *field != agent_id)
            .collect::<Vec<_>>()
            .join(" ")
    };
    let driven_text = normalize_identity_fields(&driven_lines, CLI_DRIVEN_PANE, &driven_agent_id);
    let control_text =
        normalize_identity_fields(&control_lines, CLI_CONTROL_PANE, &control_agent_id);
    let human_has_live_status = driven_text != control_text;

    let json = run_daemon_status_cli(&daemon.attach_path, true).await;
    assert!(
        json.status.success(),
        "`daemon status --json` failed after a real agent-event; status={:?} stdout={:?} stderr={:?}",
        json.status,
        json.stdout,
        json.stderr
    );
    let document: serde_json::Value =
        serde_json::from_str(json.stdout.trim()).unwrap_or_else(|e| {
            panic!(
                "`daemon status --json` did not parse after a real agent-event: {e}; stdout={:?}",
                json.stdout
            )
        });
    let driven_json = document
        .get("agents")
        .and_then(serde_json::Value::as_array)
        .and_then(|agents| {
            agents.iter().find(|agent| {
                agent.get("pane_id").and_then(serde_json::Value::as_str) == Some(CLI_DRIVEN_PANE)
            })
        });
    let json_has_live_status = driven_json.and_then(|agent| agent.get("status")).is_some();

    assert!(
        human_has_live_status && json_has_live_status,
        "the real CLI event reached the daemon with matching ids but `ListAgents` did not join it to the TUI-spawned agent: human_live={human_has_live_status} json_status_present={json_has_live_status} observed_event={{pane_id:{:?}, agent_id:{:?}, event_type:{:?}}} driven_row={driven_text:?} control_row={control_text:?} driven_json={driven_json:?}",
        observed.pane_id,
        observed.agent_id,
        observed.event_type
    );

    daemon.registry.shutdown_all();
}

/// Scenario: Bind a stub Unix socket in a scratch directory that accepts every connection and then never writes a reply, and point the REAL `dot-agent-deck daemon status` CLI at it in both its human and `--json` forms at once. Assert each subprocess returns inside this test's own 8s watchdog rather than hanging, exits non-zero without panicking, prints the `no response within 3s` timeout diagnostic naming the command's declared deadline, and emits nothing on stdout — so a wedged daemon is reported rather than waited on, and `--json` never fabricates an empty document for one.
#[spec("daemon/status/006")]
#[test]
#[cfg(unix)]
fn daemon_status_006_wedged_peer_times_out_instead_of_hanging() {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build wedged-peer daemon-status runtime")
        .block_on(daemon_status_006_wedged_peer_times_out_instead_of_hanging_inner());
}

#[cfg(unix)]
async fn daemon_status_006_wedged_peer_times_out_instead_of_hanging_inner() {
    common::init_test_env();
    let scratch = common::race_safe_tempdir();
    // `attach.sock` exactly, not a descriptive name: this socket is BOUND, so
    // it is charged against `HARNESS_SOCKET_OVERHEAD`, whose 48 bytes budget
    // 12 for the longest bound socket name and leave `MAX_TEMP_BASE_LEN` at
    // 55. A 55-byte base plus the harness's per-process and per-test segments
    // plus `/attach.sock` comes to exactly 103 — `sun_path`'s usable limit on
    // macOS — so any longer name here overruns it and `bind(2)` fails with
    // `AF_UNIX path too long` before the deadline is ever reached. The scratch
    // dir is already unique, so the plain name is unambiguous anyway.
    // (`daemon/status/003`'s longer `never-bound-attach.sock` is fine under
    // the same rule precisely because nothing ever binds it.)
    let attach_path = scratch.path().join("attach.sock");
    let peer = WedgedPeer::bind(attach_path.clone()).await;

    // Both modes concurrently against the one stub: each pays the same ~3s
    // deadline, so running them in parallel keeps the whole test at roughly
    // one deadline instead of two.
    let (human, json) = tokio::join!(
        run_daemon_status_cli_bounded(&attach_path, false, WEDGED_PEER_DEADLINE),
        run_daemon_status_cli_bounded(&attach_path, true, WEDGED_PEER_DEADLINE),
    );

    let accepted = peer.connections_accepted().await;
    peer.shutdown().await;

    assert_eq!(
        accepted, 2,
        "the stub must have ACCEPTED one connection per CLI invocation; anything else means \
         this ran `daemon/status/003`'s connect-failure path again rather than the \
         accepted-but-unanswered one the round-trip deadline exists for"
    );

    for (mode, run) in [("daemon status", &human), ("daemon status --json", &json)] {
        assert!(
            !run.result.status.success(),
            "`{mode}` against a peer that never replies must not report success; got \
             status={:?} stdout={:?} stderr={:?}",
            run.result.status,
            run.result.stdout,
            run.result.stderr
        );
        assert!(
            !run.result.stderr.contains("panicked"),
            "a wedged daemon must produce a controlled diagnostic, not a Rust panic; \
             `{mode}` stderr={:?}",
            run.result.stderr
        );
        assert!(
            run.elapsed <= STATUS_REQUEST_TIMEOUT + WEDGED_PEER_SPAWN_SLACK,
            "`{mode}` took {:?}, past its own declared {:?} round-trip deadline plus {:?} of \
             spawn slack — the command is no longer honouring the STATUS_REQUEST_TIMEOUT it \
             still reports to the operator",
            run.elapsed,
            STATUS_REQUEST_TIMEOUT,
            WEDGED_PEER_SPAWN_SLACK
        );
        // The phrase is the branch's own signature: `run_daemon_status_cli`
        // prints it from the `Err(_elapsed)` arm alone, so matching it rules
        // out a transport error or a server-side `ok: false` having produced
        // the non-zero exit instead.
        let expected = format!("no response within {}s", STATUS_REQUEST_TIMEOUT.as_secs());
        assert!(
            run.result.stderr.contains(&expected),
            "`{mode}` must report the round-trip deadline expiring, naming the deadline it \
             actually applied; expected stderr to contain {expected:?}, got stderr={:?} \
             stdout={:?}",
            run.result.stderr,
            run.result.stdout
        );
        assert!(
            run.result.stdout.trim().is_empty(),
            "a timed-out `{mode}` must print no status output at all — for `--json` in \
             particular, an empty-but-well-formed document would tell a consumer the daemon \
             is up with zero agents, which is the opposite of what happened; stdout={:?}",
            run.result.stdout
        );
    }
}
