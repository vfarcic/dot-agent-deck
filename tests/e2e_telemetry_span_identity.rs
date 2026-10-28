#![cfg(all(feature = "e2e", unix))]

//! Offline OTLP contract checks for child identity, late agent detection and shared CI.

mod common;

use common::TuiDeck;
use dot_agent_deck::agent_pty::{AgentRecord, TabMembership};
use dot_agent_deck::daemon_client::{DaemonClient, StartAgentOptions};
use dot_agent_deck::daemon_protocol::AttachRequest;
use dot_agent_deck::event::{AgentType, EventType};
use serde_json::{Value, json};
use spec::spec;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(25);
const PR: u64 = 1981;

struct Receiver {
    addr: SocketAddr,
    endpoint: String,
    bodies: Arc<Mutex<Vec<Vec<u8>>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Receiver {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let captured = Arc::clone(&bodies);
        let stopped = Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stopped.load(Ordering::Relaxed) {
                    break;
                }
                let mut stream = stream.unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(&stream);
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    continue;
                }
                assert!(
                    line.starts_with("POST /v1/traces "),
                    "unexpected OTLP request: {line}"
                );
                let mut size = 0;
                let mut chunked = false;
                loop {
                    line.clear();
                    assert_ne!(
                        reader.read_line(&mut line).unwrap(),
                        0,
                        "incomplete HTTP headers"
                    );
                    if line == "\r\n" {
                        break;
                    }
                    let (key, value) = line.trim_end().split_once(':').unwrap();
                    if key.eq_ignore_ascii_case("content-length") {
                        size = value.trim().parse().unwrap();
                    }
                    if key.eq_ignore_ascii_case("transfer-encoding")
                        && value.trim().eq_ignore_ascii_case("chunked")
                    {
                        chunked = true;
                    }
                }
                let mut body = Vec::new();
                if chunked {
                    loop {
                        line.clear();
                        reader.read_line(&mut line).unwrap();
                        let size =
                            usize::from_str_radix(line.trim().split(';').next().unwrap(), 16)
                                .unwrap();
                        if size == 0 {
                            break;
                        }
                        let start = body.len();
                        body.resize(start + size, 0);
                        reader.read_exact(&mut body[start..]).unwrap();
                        let mut crlf = [0; 2];
                        reader.read_exact(&mut crlf).unwrap();
                    }
                } else {
                    body.resize(size, 0);
                    reader.read_exact(&mut body).unwrap();
                }
                captured.lock().unwrap().push(body);
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
            }
        });
        Self {
            addr,
            endpoint: format!("http://{addr}"),
            bodies,
            stop,
            worker: Some(worker),
        }
    }

    fn spans(&self) -> Vec<Value> {
        let mut spans = Vec::new();
        for body in self.bodies.lock().unwrap().iter() {
            let body: Value = serde_json::from_slice(body).expect("HTTP/JSON OTLP");
            for resource in body["resourceSpans"].as_array().into_iter().flatten() {
                for scope in resource["scopeSpans"].as_array().into_iter().flatten() {
                    spans.extend(scope["spans"].as_array().unwrap().iter().cloned());
                }
            }
        }
        spans
    }

    fn run(&self, record: &AgentRecord) -> Value {
        let pane = record.pane_id_env.as_deref().unwrap();
        let find = || {
            self.spans()
                .into_iter()
                .find(|span| span["name"] == "agent.run" && string(span, "deck.pane.id") == pane)
        };
        assert!(
            common::wait_until(WAIT, || find().is_some()),
            "no completed run for {pane}: {:?}",
            self.spans()
        );
        find().unwrap()
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.addr);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn attr<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value["attributes"]
        .as_array()?
        .iter()
        .find(|attr| attr["key"] == key)
        .map(|attr| &attr["value"])
}

fn string<'a>(value: &'a Value, key: &str) -> &'a str {
    attr(value, key)
        .and_then(|v| v["stringValue"].as_str())
        .unwrap_or_default()
}

fn integer(value: &Value, key: &str) -> u64 {
    let value = attr(value, key).unwrap_or_else(|| panic!("missing {key}: {value}"));
    value["intValue"]
        .as_u64()
        .unwrap_or_else(|| value["intValue"].as_str().unwrap().parse().unwrap())
}

fn close(deck: &TuiDeck, record: &AgentRecord) {
    let reply = common::attach_request_on(
        deck.attach_socket_path(),
        &AttachRequest::StopAgent {
            id: record.id.clone(),
        },
    )
    .unwrap();
    assert!(reply.ok, "StopAgent: {reply:?}");
}

fn start(
    deck: &TuiDeck,
    name: &str,
    cwd: &Path,
    kind: Option<AgentType>,
    role: bool,
) -> AgentRecord {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let id = runtime.block_on(DaemonClient::new(deck.attach_socket_path().to_path_buf()).start_agent(StartAgentOptions {
        command: Some(format!("/bin/sh -c 'env > {name}.env.tmp; mv {name}.env.tmp {name}.env; printf IDENTITY_READY; exec cat'")),
        cwd: Some(cwd.to_string_lossy().into_owned()),
        display_name: Some(name.into()),
        agent_type: kind,
        env: vec![("DOT_AGENT_DECK_PANE_ID".into(), format!("identity-{name}"))],
        tab_membership: role.then(|| TabMembership::Orchestration {
            name: "identity-team".into(), role_index: 1, role_name: "coder".into(), is_start_role: false,
            orchestration_cwd: Some(cwd.to_string_lossy().into_owned()), display_title: None,
            orchestration_id: Some("orch-telemetry-identity".into()),
        }),
        ..Default::default()
    })).expect("start synthetic pane through the real daemon");
    common::wait_for_file_contains(&cwd.join(format!("{name}.env")), "DOT_AGENT_DECK_PANE_ID=");
    common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .find(|record| record.id == id)
        .unwrap()
}

fn hook(deck: &TuiDeck, record: &AgentRecord, agent: &str, event: &str, expected: EventType) {
    let events = deck.subscribe_events();
    let payload = json!({"session_id":record.pane_id_env, "pane_id":record.pane_id_env, "agent_id":record.id,
        "agent_type":agent, "event_type":event, "timestamp":chrono::Utc::now().to_rfc3339(),
        "cwd":record.cwd, "tool_name":"Bash", "tool_detail":"cargo fmt"});
    common::write_hook_line(deck.hook_socket_path(), &payload.to_string()).unwrap();
    events.wait_for(
        |event| event.event_type == expected && event.agent_id.as_deref() == Some(&record.id),
        WAIT,
    );
}

fn exercise_hooks(deck: &TuiDeck, record: &AgentRecord, agent: &str) {
    hook(deck, record, agent, "thinking", EventType::Thinking);
    hook(deck, record, agent, "tool_start", EventType::ToolStart);
    hook(deck, record, agent, "tool_end", EventType::ToolEnd);
}

struct CiFixture {
    _scratch: tempfile::TempDir,
    repo: PathBuf,
    bin: PathBuf,
    reply: PathBuf,
}

impl CiFixture {
    fn new() -> Self {
        let scratch = common::harness_tempdir().unwrap();
        let repo = scratch.path().join("repo");
        let bin = scratch.path().join("bin");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        for args in [
            vec!["init", "-b", "PRIVATE_BRANCH_IDENTITY_1981"],
            vec!["commit", "--allow-empty", "-m", "fixture"],
            vec![
                "remote",
                "add",
                "origin",
                "https://github.com/test-org/test-repo.git",
            ],
        ] {
            let output = common::fixture_git(&repo, scratch.path())
                .args(&args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "fixture git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let gh = bin.join("gh");
        std::fs::write(&gh, "#!/bin/sh\n[ \"$1\" = pr ] && [ \"$2\" = list ] || exit 91\ncat \"$TELEMETRY_GH_REPLY\"\n").unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let reply = scratch.path().join("rollup.json");
        let fixture = Self {
            _scratch: scratch,
            repo,
            bin,
            reply,
        };
        fixture.set_checks(None);
        fixture
    }

    fn set_checks(&self, conclusion: Option<&str>) {
        let checks = match conclusion {
            Some(conclusion) => {
                json!([{"__typename":"CheckRun", "name":"build", "workflowName":"CI", "status":"COMPLETED", "conclusion":conclusion}])
            }
            None => {
                json!([{"__typename":"CheckRun", "name":"build", "workflowName":"CI", "status":"IN_PROGRESS", "conclusion":null}])
            }
        };
        let pending = self.reply.with_extension("pending");
        std::fs::write(&pending, json!([{"number":PR,"state":"OPEN","isDraft":false,"reviewDecision":"REVIEW_REQUIRED", "headRefName":"PRIVATE_BRANCH_IDENTITY_1981", "url":"https://github.com/test-org/test-repo/pull/1981", "statusCheckRollup":checks}]).to_string()).unwrap();
        std::fs::rename(pending, &self.reply).unwrap();
    }

    fn launch(&self, receiver: &Receiver) -> TuiDeck {
        let deck = TuiDeck::builder()
            .without_agent_credentials()
            .impersonating_pane_signals()
            .with_pty_size(180, 45)
            .with_env(
                "PATH",
                format!("{}:{}", self.bin.display(), common::path_with_binary_dir()),
            )
            .with_env("TELEMETRY_GH_REPLY", self.reply.to_string_lossy())
            .with_env("DOT_AGENT_DECK_PR_REFRESH_SECS", "1")
            .with_env("OTEL_EXPORTER_OTLP_ENDPOINT", &receiver.endpoint)
            .with_env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json")
            .with_continue_session("identity-bootstrap", "cat")
            .launch_with_fixture("minimal");
        deck.wait_for_string("[Command Mode Ctrl+D]");
        let bootstrap = common::agent_records_on(deck.attach_socket_path())
            .into_iter()
            .next()
            .unwrap();
        close(&deck, &bootstrap);
        deck
    }

    fn wait_checks(&self, deck: &TuiDeck, records: &[&AgentRecord], expected: &str) {
        assert!(
            common::wait_until(WAIT, || {
                let current = common::agent_records_on(deck.attach_socket_path());
                records.iter().all(|record| {
                    current
                        .iter()
                        .find(|r| r.id == record.id)
                        .and_then(|r| r.live.as_ref())
                        .and_then(|live| live.pull_request.as_ref())
                        .and_then(|pr| pr.checks.as_ref())
                        .is_some_and(|checks| {
                            serde_json::to_value(checks).unwrap()["state"] == expected
                        })
                })
            }),
            "all observers must see fixture checks {expected}"
        );
    }
}

/// Scenario: Open an orchestration worker with a declared Pi identity, send shell hooks and human input, write an offline model-usage reply and poll a pending then passing PR. Closing the worker must export the same agent, role and orchestration id on its run, child spans and each kind of deck event.
#[spec("telemetry/otlp/024")]
#[test]
fn telemetry_otlp_024_child_spans_and_events_carry_run_identity() {
    let receiver = Receiver::start();
    let fixture = CiFixture::new();
    let deck = fixture.launch(&receiver);
    let record = start(&deck, "role", &fixture.repo, Some(AgentType::Pi), true);
    exercise_hooks(&deck, &record, "pi");
    let mut input = common::AttachInput::attach(deck.attach_socket_path(), &record.id);
    input.write("IDENTITY_HUMAN_INPUT\n");
    assert!(
        common::wait_until(WAIT, || common::pane_search_key_on(
            deck.attach_socket_path(),
            &record.id
        )
        .contains("IDENTITY_HUMAN_INPUT")),
        "human input must reach stand-in PTY"
    );
    let environment = std::fs::read_to_string(fixture.repo.join("role.env")).unwrap();
    let usage_file = environment
        .lines()
        .find_map(|line| line.strip_prefix("DOT_AGENT_DECK_USAGE_FILE="))
        .expect("declared Pi stand-in gets a usage file");
    std::fs::write(usage_file, format!("{}\n", json!({"v":1,"type":"reply","agent":"pi","model":"gpt-5.4-mini","input":100,"output":20,"cache_read":0,"cache_write":0,"cost_usd":0.0042,"stop":"stop"}))).unwrap();
    fixture.wait_checks(&deck, &[&record], "pending");
    fixture.set_checks(Some("SUCCESS"));
    fixture.wait_checks(&deck, &[&record], "passing");
    close(&deck, &record);
    let run = receiver.run(&record);
    assert_eq!(string(&run, "deck.agent"), "pi");
    assert_eq!(string(&run, "deck.role"), "coder");
    assert_eq!(
        string(&run, "deck.orchestration.id"),
        "orch-telemetry-identity"
    );
    let spans = receiver.spans();
    let children: Vec<_> = spans
        .iter()
        .filter(|s| {
            s["name"] != "agent.run" && string(s, "deck.run.id") == string(&run, "deck.run.id")
        })
        .collect();
    for name in ["agent.operation", "agent.status", "agent.ci"] {
        assert!(
            children.iter().any(|s| s["name"] == name),
            "missing exercised child {name}: {spans:?}"
        );
    }
    let events = run["events"].as_array().unwrap();
    for name in [
        "deck.status_change",
        "deck.intervention",
        "deck.ci.change",
        "deck.model_usage",
    ] {
        assert!(
            events.iter().any(|e| e["name"] == name),
            "missing exercised event {name}: {run}"
        );
    }
    let mut missing = Vec::new();
    for value in children.into_iter().chain(events.iter().filter(|e| {
        e["name"]
            .as_str()
            .is_some_and(|name| name.starts_with("deck."))
    })) {
        for key in ["deck.agent", "deck.role", "deck.orchestration.id"] {
            if string(value, key) != string(&run, key) {
                missing.push(format!(
                    "{} {key}: expected {:?}, got {:?}",
                    value["name"],
                    string(&run, key),
                    string(value, key)
                ));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "child/event identity differs from agent.run:\n{}",
        missing.join("\n")
    );
}

/// Scenario: Start two stand-in panes in one worktree and let both observe pending, failing, pending and passing checks for the same offline PR. Closing both panes must export two CI intervals and four CI-change events in total, while each run retains its own two observed rounds and one observed failure.
#[spec("telemetry/otlp/025")]
#[test]
fn telemetry_otlp_025_shared_pr_exports_ci_history_once() {
    let receiver = Receiver::start();
    let fixture = CiFixture::new();
    let deck = fixture.launch(&receiver);
    let first = start(
        &deck,
        "observer-a",
        &fixture.repo,
        Some(AgentType::Pi),
        false,
    );
    let second = start(
        &deck,
        "observer-b",
        &fixture.repo,
        Some(AgentType::Pi),
        false,
    );
    for record in [&first, &second] {
        hook(
            &deck,
            record,
            "pi",
            "session_start",
            EventType::SessionStart,
        );
    }
    for (conclusion, state) in [
        (None, "pending"),
        (Some("FAILURE"), "failing"),
        (None, "pending"),
        (Some("SUCCESS"), "passing"),
    ] {
        fixture.set_checks(conclusion);
        fixture.wait_checks(&deck, &[&first, &second], state);
    }
    close(&deck, &first);
    close(&deck, &second);
    let runs = [receiver.run(&first), receiver.run(&second)];
    for run in &runs {
        assert_eq!(integer(run, "deck.pr.number"), PR);
        assert_eq!(
            integer(run, "deck.ci.rounds.count"),
            2,
            "each run counts its observed rounds"
        );
        assert_eq!(
            integer(run, "deck.ci.failures.count"),
            1,
            "each run counts its observed failures"
        );
    }
    let spans = receiver.spans();
    let intervals: Vec<_> = spans
        .iter()
        .filter(|s| s["name"] == "agent.ci" && integer(s, "deck.pr.number") == PR)
        .collect();
    let changes: Vec<_> = spans
        .iter()
        .flat_map(|s| s["events"].as_array().into_iter().flatten())
        .filter(|e| e["name"] == "deck.ci.change" && integer(e, "deck.pr.number") == PR)
        .collect();
    assert_eq!(
        (intervals.len(), changes.len()),
        (2, 4),
        "same PR CI history exported more than once: interval results={:?}, change states={:?}",
        intervals
            .iter()
            .map(|s| string(s, "deck.ci.result"))
            .collect::<Vec<_>>(),
        changes
            .iter()
            .map(|e| string(e, "deck.ci.to"))
            .collect::<Vec<_>>()
    );
    let mut results: Vec<_> = intervals
        .iter()
        .map(|s| string(s, "deck.ci.result"))
        .collect();
    results.sort();
    assert_eq!(results, ["failing", "passing"]);
    let states: Vec<_> = changes.iter().map(|e| string(e, "deck.ci.to")).collect();
    assert_eq!(states, ["pending", "failing", "pending", "passing"]);
}

/// Scenario: Run an unrecognised launcher that later reports Claude through hooks, a declared Pi launcher whose hooks report Claude, and a silent unknown launcher. Their completed exports must distinguish hook-learned and spawn-declared identity, retain none for the silent launcher and label post-hook child spans with the resolved agent.
#[spec("telemetry/otlp/026")]
#[test]
fn telemetry_otlp_026_hook_identity_binds_only_unknown_agents() {
    let receiver = Receiver::start();
    let fixture = CiFixture::new();
    let deck = fixture.launch(&receiver);
    let learned = start(&deck, "learned", deck.workdir(), None, false);
    let declared = start(
        &deck,
        "declared",
        deck.workdir(),
        Some(AgentType::Pi),
        false,
    );
    let silent = start(&deck, "silent", deck.workdir(), None, false);
    for record in [&learned, &declared] {
        exercise_hooks(&deck, record, "claude_code");
    }
    for record in [&learned, &declared, &silent] {
        close(&deck, record);
    }
    let runs = [
        receiver.run(&learned),
        receiver.run(&declared),
        receiver.run(&silent),
    ];
    let mut mismatches = Vec::new();
    for (run, agent, source) in [
        (&runs[0], "claude", "hook"),
        (&runs[1], "pi", "spawn"),
        (&runs[2], "none", ""),
    ] {
        for (key, expected) in [("deck.agent", agent), ("deck.agent.source", source)] {
            if string(run, key) != expected {
                mismatches.push(format!(
                    "{} {key}: expected {expected:?}, got {:?}",
                    string(run, "deck.pane.id"),
                    string(run, key)
                ));
            }
        }
    }
    // Chosen contract: an unknown run with no identifying hooks omits source.
    assert!(
        attr(&runs[2], "deck.agent.source").is_none(),
        "silent unknown run must omit deck.agent.source"
    );
    let spans = receiver.spans();
    for (run, expected) in [(&runs[0], "claude"), (&runs[1], "pi")] {
        for (name, status) in [
            ("agent.operation", ""),
            ("agent.status", "thinking"),
            ("agent.status", "working"),
        ] {
            let matching: Vec<_> = spans
                .iter()
                .filter(|s| {
                    s["name"] == name
                        && string(s, "deck.run.id") == string(run, "deck.run.id")
                        && (status.is_empty() || string(s, "deck.status") == status)
                })
                .collect();
            assert!(
                !matching.is_empty(),
                "missing post-hook {name}/{status}: {spans:?}"
            );
            for child in matching {
                if string(child, "deck.agent") != expected {
                    mismatches.push(format!(
                        "{} {name}/{status} deck.agent: expected {expected:?}, got {:?}",
                        string(run, "deck.pane.id"),
                        string(child, "deck.agent")
                    ));
                }
            }
        }
    }
    assert!(
        mismatches.is_empty(),
        "resolved agent identity is wrong:\n{}",
        mismatches.join("\n")
    );
}

struct SiblingWorktreeGuard(PathBuf);

impl Drop for SiblingWorktreeGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Scenario: Dispatch a single synthetic unit from a declared Pi caller, wait for the unit to start in its own worktree and close it. The completed unit run must carry the exact dispatch name from the deck registry and must not export the unit branch name.
#[spec("telemetry/otlp/027")]
#[test]
fn telemetry_otlp_027_dispatched_run_carries_registry_name() {
    const UNIT: &str = "identity-dispatch-unit";
    let receiver = Receiver::start();
    let fixture = CiFixture::new();
    let deck = fixture.launch(&receiver);
    common::commit_fixture_repo(deck.workdir());
    let caller = start(
        &deck,
        "dispatcher",
        deck.workdir(),
        Some(AgentType::Pi),
        false,
    );
    let worktree = deck.workdir().parent().unwrap().join(format!(
        "{}-dispatch-{UNIT}",
        deck.workdir().file_name().unwrap().to_string_lossy()
    ));
    let _guard = SiblingWorktreeGuard(worktree.clone());
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args([
            "dispatch",
            UNIT,
            "--task",
            "Synthetic telemetry identity check",
            "--single",
        ])
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env(
            "DOT_AGENT_DECK_PANE_ID",
            caller.pane_id_env.as_deref().unwrap(),
        )
        .env("HOME", deck.home_dir())
        .current_dir(deck.workdir())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "dispatch failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let find = || {
        common::agent_records_on(deck.attach_socket_path())
            .into_iter()
            .find(|record| record.cwd.as_deref() == Some(worktree.to_string_lossy().as_ref()))
    };
    assert!(
        common::wait_until(WAIT, || find().is_some()),
        "dispatched stand-in did not start"
    );
    let unit = find().unwrap();
    common::wait_for_file_contains(&worktree.join("dispatcher.env"), "DOT_AGENT_DECK_PANE_ID=");
    close(&deck, &unit);
    let run = receiver.run(&unit);
    let exported = serde_json::to_string(&receiver.spans()).unwrap();
    assert!(
        !exported.contains(&format!("agent/dispatch-{UNIT}")),
        "dispatch branch leaked into telemetry"
    );
    assert_eq!(
        string(&run, "deck.dispatch.name"),
        UNIT,
        "completed dispatched run must carry registry unit name: {run}"
    );
}
