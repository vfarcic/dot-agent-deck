#![cfg(all(feature = "e2e", unix))]

//! Synthetic, PTY-attached checks of the opt-in OTLP export contract.

mod common;

use common::TuiDeck;
use dot_agent_deck::agent_pty::AgentRecord;
use dot_agent_deck::daemon_client::{DaemonClient, StartAgentOptions};
use dot_agent_deck::daemon_protocol::AttachRequest;
use serde_json::{Value, json};
use spec::spec;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const EXPORT_WAIT: Duration = Duration::from_secs(25);
const OUTPUT_SECRET: &str = "PRIVATE_OUTPUT_1741_8ab2";
const INPUT_SECRET: &str = "PRIVATE_INPUT_1741_7ce9";
const HOOK_SECRET: &str = "PRIVATE_HOOK_1741_3df5";
const TITLE_SECRET: &str = "PRIVATE_TITLE_1741_6ba4";
const HEADER_SECRET: &str = "PRIVATE_HEADER_1741_9ad1";
const FILE_SECRET: &str = "PRIVATE_FILE_1741_31bf";
const BRANCH_SECRET: &str = "PRIVATE_BRANCH_1741_901a";
const COMMAND_SECRET: &str = "PRIVATE_COMMAND_1741_faec";
const CWD_SECRET: &str = "PRIVATE_CWD_1741_5cfe";
const ENV_SECRET: &str = "PRIVATE_ENV_1741_89de";

fn write_executable(path: &Path, source: &str) {
    std::fs::write(path, source).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn integer_attribute(value: &Value, key: &str) -> u64 {
    let attr = value["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|attr| attr["key"] == key)
        .unwrap_or_else(|| panic!("missing integer attribute {key}: {value}"));
    attr["value"]["intValue"]
        .as_u64()
        .unwrap_or_else(|| attr["value"]["intValue"].as_str().unwrap().parse().unwrap())
}

#[derive(Clone, Debug)]
struct Request {
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    accepted: bool,
}

struct Receiver {
    addr: SocketAddr,
    endpoint: String,
    requests: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Receiver {
    fn start() -> Self {
        Self::responding(Vec::new())
    }

    // Repeat the final response after the scripted responses are exhausted.
    fn responding(responses: Vec<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind isolated OTLP receiver");
        let addr = listener.local_addr().unwrap();
        let endpoint = format!("http://{addr}");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let captured = Arc::clone(&requests);
        let stopped = Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            // Blocking accept: Drop wakes it with a connection of its own.
            for (count, stream) in listener.incoming().enumerate() {
                if stopped.load(Ordering::Relaxed) {
                    break;
                }
                let stream = stream.expect("accept OTLP request");
                let response = responses.get(count).or_else(|| responses.last());
                let response = response.map(String::as_str).unwrap_or("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
                let mut request = read_request(&stream);
                request.accepted = response.starts_with("HTTP/1.1 200");
                captured.lock().unwrap().push(request);
                let mut stream = stream;
                // A bounded exporter may close a retry/shutdown request.
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Self {
            addr,
            endpoint,
            requests,
            stop,
            worker: Some(worker),
        }
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    fn spans(&self) -> Vec<Value> {
        self.requests()
            .iter()
            .filter(|request| request.accepted && request.path == "/v1/traces")
            .flat_map(|request| {
                let body: Value =
                    serde_json::from_slice(&request.body).expect("OTLP traces must be JSON");
                let mut spans = Vec::new();
                for resource in body["resourceSpans"].as_array().expect("resourceSpans") {
                    for scope in resource["scopeSpans"].as_array().expect("scopeSpans") {
                        spans.extend(scope["spans"].as_array().expect("spans").iter().cloned());
                    }
                }
                spans
            })
            .collect()
    }

    fn run(&self, pane: &str) -> Value {
        assert!(
            common::wait_until(EXPORT_WAIT, || self.spans().iter().any(|span| {
                span["name"] == "agent.run" && attribute(span, "deck.pane.id") == pane
            })),
            "OTLP receiver never received agent.run for pane {pane}; request paths: {:?}",
            self.requests().iter().map(|r| &r.path).collect::<Vec<_>>()
        );
        self.spans()
            .into_iter()
            .find(|span| span["name"] == "agent.run" && attribute(span, "deck.pane.id") == pane)
            .unwrap()
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Unblock the worker's accept so it can observe `stop`.
        let _ = TcpStream::connect(self.addr);
        if let Some(worker) = self.worker.take() {
            worker.join().expect("OTLP receiver thread");
        }
    }
}

fn read_request(stream: &TcpStream) -> Request {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let path = line
        .split_whitespace()
        .nth(1)
        .expect("HTTP path")
        .to_string();
    let mut headers = Vec::new();
    loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        let (key, value) = line.trim_end().split_once(':').expect("HTTP header");
        headers.push((key.to_ascii_lowercase(), value.trim().to_string()));
    }
    let mut body = Vec::new();
    if headers
        .iter()
        .any(|(key, value)| key == "transfer-encoding" && value.eq_ignore_ascii_case("chunked"))
    {
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            let size = usize::from_str_radix(line.trim().split(';').next().unwrap(), 16).unwrap();
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
        let size = headers
            .iter()
            .find(|(key, _)| key == "content-length")
            .map(|(_, value)| value.parse::<usize>().expect("Content-Length"))
            .unwrap_or(0);
        body.resize(size, 0);
        reader.read_exact(&mut body).unwrap();
    }
    Request {
        path,
        headers,
        body,
        accepted: false,
    }
}

fn attribute(value: &Value, key: &str) -> String {
    value["attributes"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|attr| attr["key"] == key)
        .and_then(|attr| attr["value"]["stringValue"].as_str())
        .unwrap_or_default()
        .to_string()
}

fn launch(receiver: Option<&Receiver>, private: bool) -> TuiDeck {
    launch_protocol(receiver, private, "http/json")
}

fn launch_protocol(receiver: Option<&Receiver>, private: bool, protocol: &str) -> TuiDeck {
    let mut builder = TuiDeck::builder()
        .without_agent_credentials()
        .impersonating_pane_signals()
        .with_pty_size(180, 45)
        .with_continue_session(
            if private { TITLE_SECRET } else { "telemetry-standin" },
            format!("/bin/sh -c 'env > telemetry-agent.env; printf \"{OUTPUT_SECRET}\\n\"; exec cat > telemetry-input.txt'"),
        );
    if let Some(receiver) = receiver {
        builder = builder
            .with_env("OTEL_EXPORTER_OTLP_ENDPOINT", &receiver.endpoint)
            .with_env("OTEL_EXPORTER_OTLP_PROTOCOL", protocol);
    }
    if private {
        builder = builder.with_env("SOME_API_KEY", ENV_SECRET).with_env(
            "OTEL_EXPORTER_OTLP_HEADERS",
            format!("x-secret={HEADER_SECRET}"),
        );
    }
    let deck = builder.launch_with_fixture("minimal");
    deck.wait_for_string(OUTPUT_SECRET);
    common::wait_for_file_contains(
        &deck.workdir().join("telemetry-agent.env"),
        "DOT_AGENT_DECK_PANE_ID=",
    );
    deck
}

fn agent(deck: &TuiDeck) -> AgentRecord {
    common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .next()
        .expect("one restored stand-in agent")
}

fn env(deck: &TuiDeck) -> String {
    std::fs::read_to_string(deck.workdir().join("telemetry-agent.env")).unwrap()
}

fn env_value<'a>(env: &'a str, key: &str) -> Option<&'a str> {
    env.lines().find_map(|line| {
        line.strip_prefix(key)
            .and_then(|suffix| suffix.strip_prefix('='))
    })
}

fn close(deck: &TuiDeck, agent: &AgentRecord) {
    let reply = common::attach_request_on(
        deck.attach_socket_path(),
        &AttachRequest::StopAgent {
            id: agent.id.clone(),
        },
    )
    .expect("close stand-in pane");
    assert!(reply.ok, "StopAgent: {reply:?}");
}

fn start_on_pane(
    deck: &TuiDeck,
    previous: &AgentRecord,
    cwd: &std::path::Path,
    command: String,
) -> AgentRecord {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("attach client runtime");
    let client = DaemonClient::new(deck.attach_socket_path().to_path_buf());
    let id = runtime
        .block_on(client.start_agent(StartAgentOptions {
            command: Some(command),
            cwd: Some(cwd.to_string_lossy().into_owned()),
            display_name: previous.display_name.clone(),
            env: vec![(
                "DOT_AGENT_DECK_PANE_ID".into(),
                previous.pane_id_env.clone().unwrap(),
            )],
            tab_membership: previous.tab_membership.clone(),
            ..StartAgentOptions::default()
        }))
        .expect("start successor on the same pane");
    common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .find(|record| record.id == id)
        .expect("successor is registered")
}

fn statuses(deck: &TuiDeck, agent: &AgentRecord, secret: &str) {
    deck.send_bytes(b"\x04");
    deck.wait_for_string("Dir:");
    for (event, label) in [("thinking", "Thinking"), ("tool_start", "Working")] {
        let payload = json!({
            "session_id": "telemetry-synthetic", "pane_id": agent.pane_id_env,
            "agent_id": agent.id, "agent_type": "claude_code", "event_type": event,
            "timestamp": chrono::Utc::now().to_rfc3339(), "cwd": deck.workdir(),
            "tool_name": "Bash", "tool_detail": secret, "user_prompt": secret,
            "metadata": {"private_payload": secret}
        });
        common::write_hook_line(deck.hook_socket_path(), &payload.to_string()).unwrap();
        deck.wait_for_string(label);
    }
}

/// Scenario: Restore and close a stand-in pane without OTEL configuration. Its environment must contain no injected telemetry attributes or Claude enablement, and the local receiver must remain empty across an export interval.
#[spec("telemetry/otlp/001")]
#[test]
fn telemetry_otlp_001_unconfigured_sends_nothing() {
    let receiver = Receiver::start();
    let deck = launch(None, false);
    let environment = env(&deck);
    assert!(env_value(&environment, "OTEL_RESOURCE_ATTRIBUTES").is_none());
    assert!(env_value(&environment, "CLAUDE_CODE_ENABLE_TELEMETRY").is_none());
    close(&deck, &agent(&deck));
    assert!(
        !common::wait_until(Duration::from_secs(6), || !receiver.requests().is_empty()),
        "unconfigured deck exported telemetry"
    );
}

/// Scenario: Restore a stand-in pane with a local OTLP endpoint, show Thinking and Working through synthetic hooks, then close it. The receiver must get the pane's completed run, child status intervals, transition events and identifying resource attributes.
#[spec("telemetry/otlp/002")]
#[test]
fn telemetry_otlp_002_exports_run_and_status_intervals() {
    let receiver = Receiver::start();
    let deck = launch(Some(&receiver), false);
    let agent = agent(&deck);
    statuses(&deck, &agent, "synthetic tool detail");
    close(&deck, &agent);
    let pane = agent.pane_id_env.as_deref().unwrap();
    let run = receiver.run(pane);
    assert!(!attribute(&run, "deck.run.id").is_empty(), "run id: {run}");
    assert_eq!(attribute(&run, "deck.agent"), "none");
    assert_eq!(attribute(&run, "deck.run.end_reason"), "closed");
    let spans = receiver.spans();
    let children: Vec<_> = spans
        .iter()
        .filter(|span| span["name"] == "agent.status" && attribute(span, "deck.pane.id") == pane)
        .collect();
    assert!(
        children.len() >= 2,
        "expected two completed status intervals: {spans:?}"
    );
    for status in &children {
        assert_eq!(status["parentSpanId"], run["spanId"]);
        assert_eq!(status["traceId"], run["traceId"]);
        assert_eq!(
            attribute(status, "deck.run.id"),
            attribute(&run, "deck.run.id")
        );
    }
    assert!(
        children
            .iter()
            .any(|span| attribute(span, "deck.status") == "thinking")
    );
    assert!(
        children
            .iter()
            .any(|span| attribute(span, "deck.status") == "working")
    );
    let transitions: Vec<_> = run["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["name"] == "deck.status_change")
        .collect();
    assert!(transitions.len() >= 2, "status change events: {run}");
    assert!(
        transitions
            .iter()
            .any(|event| attribute(event, "deck.status.to") == "thinking")
    );
    assert!(
        transitions
            .iter()
            .any(|event| attribute(event, "deck.status.to") == "working")
    );
    for request in receiver
        .requests()
        .iter()
        .filter(|request| request.path == "/v1/traces")
    {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        for resource in body["resourceSpans"].as_array().unwrap() {
            let resource = &resource["resource"];
            assert_eq!(attribute(resource, "service.name"), "dot-agent-deck");
            assert!(!attribute(resource, "service.version").is_empty());
            assert!(!attribute(resource, "host.name").is_empty());
            assert_eq!(attribute(resource, "deck.telemetry.schema_version"), "1");
        }
    }
}

/// Scenario: Start a stand-in that records its environment with OTLP configured, then close it. Its propagated run id must equal the exported run's id, and the configured endpoint must reach it unchanged.
#[spec("telemetry/otlp/003")]
#[test]
fn telemetry_otlp_003_agent_and_deck_share_run_id() {
    let receiver = Receiver::start();
    let deck = launch(Some(&receiver), false);
    let agent = agent(&deck);
    let environment = env(&deck);
    let attributes = env_value(&environment, "OTEL_RESOURCE_ATTRIBUTES")
        .expect("agent must receive OTEL_RESOURCE_ATTRIBUTES containing deck.run.id");
    let run_id = attributes
        .split(',')
        .find_map(|pair| pair.strip_prefix("deck.run.id="))
        .expect("agent resource must contain deck.run.id");
    assert!(!run_id.is_empty());
    assert_eq!(
        env_value(&environment, "OTEL_EXPORTER_OTLP_ENDPOINT"),
        Some(receiver.endpoint.as_str())
    );
    close(&deck, &agent);
    let run = receiver.run(agent.pane_id_env.as_deref().unwrap());
    assert_eq!(attribute(&run, "deck.run.id"), run_id);
}

/// Scenario: Print private output, type private input, and send private hook fields while a privately titled pane exports with a secret header, then start a successor in a privately named git directory with private file contents, branch, command argument and ordinary environment value. Require an intervention, the authentication header and the telemetry exporter's startup log, and reject every private sentinel from exported bodies, URIs, non-authentication headers and telemetry-module log lines.
#[spec("telemetry/otlp/004")]
#[test]
fn telemetry_otlp_004_content_stays_out_of_request_bodies() {
    private_content_stays_out_of_exports("http/json");
}

/// Scenario: Export HTTP/protobuf from a privately titled stand-in pane while printing output, typing input and sending prompt/tool hook fields, then run a successor in a private git cwd that prints private file contents with private command and environment values. Require accepted run exports, an intervention and the intended authentication header, and reject the private sentinels from request bodies, URIs, non-authentication headers and telemetry-module logs.
#[spec("telemetry/otlp/013")]
#[test]
fn telemetry_otlp_013_protobuf_content_stays_out_of_request_bodies() {
    private_content_stays_out_of_exports("http/protobuf");
}

fn body_contains(body: &[u8], text: &str) -> bool {
    body.windows(text.len())
        .any(|window| window == text.as_bytes())
}

fn wait_for_private_run(receiver: &Receiver, run_id: &str) -> Request {
    let find = || {
        receiver.requests().into_iter().find(|request| {
            request.accepted
                && request.path == "/v1/traces"
                && body_contains(&request.body, "agent.run")
                && body_contains(&request.body, run_id)
        })
    };
    assert!(
        common::wait_until(EXPORT_WAIT, || find().is_some()),
        "private run {run_id} never reached the receiver in an accepted agent.run body"
    );
    find().unwrap()
}

fn private_content_stays_out_of_exports(protocol: &str) {
    let receiver = Receiver::start();
    let deck = launch_protocol(Some(&receiver), true, protocol);
    let agent = agent(&deck);
    let agent_env = env(&deck);
    let run_id = env_value(&agent_env, "OTEL_RESOURCE_ATTRIBUTES")
        .unwrap()
        .split(',')
        .find_map(|pair| pair.strip_prefix("deck.run.id="))
        .unwrap();
    assert!(!run_id.is_empty(), "stand-in must receive a run id");
    deck.send_bytes(format!("{INPUT_SECRET}\r").as_bytes());
    common::wait_for_file_contains(&deck.workdir().join("telemetry-input.txt"), INPUT_SECRET);
    statuses(&deck, &agent, HOOK_SECRET);
    close(&deck, &agent);
    let request = wait_for_private_run(&receiver, run_id);
    let intervention_present = if protocol == "http/json" {
        let run = receiver.run(agent.pane_id_env.as_deref().unwrap());
        run["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["name"] == "deck.intervention")
    } else {
        // The event name is a length-delimited field, distinct from the
        // deck.interventions.count attribute even when its value is zero.
        request
            .body
            .windows(b"\x12\x11deck.intervention".len())
            .any(|window| window == b"\x12\x11deck.intervention")
    };
    assert!(
        intervention_present,
        "typed human input must create an exported intervention"
    );
    let encoding = if protocol == "http/protobuf" {
        "protobuf"
    } else {
        "json"
    };
    assert!(
        request
            .headers
            .iter()
            .any(|(key, value)| { key == "content-type" && value.contains(encoding) }),
        "accepted run must use {protocol}"
    );

    let private_cwd = deck.workdir().join(CWD_SECRET);
    std::fs::create_dir(&private_cwd).unwrap();
    for args in [
        vec!["init", "--quiet"],
        vec![
            "symbolic-ref",
            "HEAD",
            &format!("refs/heads/{BRANCH_SECRET}"),
        ],
    ] {
        let result = common::fixture_git(&private_cwd, deck.workdir())
            .args(&args)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "prepare private git fixture: {result:?}"
        );
    }
    std::fs::write(private_cwd.join("private.txt"), FILE_SECRET).unwrap();
    let successor = start_on_pane(
        &deck,
        &agent,
        &private_cwd,
        format!(
            "/bin/sh -c 'env > telemetry-private.env; while :; do cat private.txt; sleep 0.1; done' standin {COMMAND_SECRET}"
        ),
    );
    common::wait_for_file_contains(&private_cwd.join("telemetry-private.env"), ENV_SECRET);
    let private_env = std::fs::read_to_string(private_cwd.join("telemetry-private.env")).unwrap();
    assert_eq!(env_value(&private_env, "SOME_API_KEY"), Some(ENV_SECRET));
    assert!(env_value(&private_env, "PWD").unwrap().contains(CWD_SECRET));
    // The stand-in must actually read/print the private file before ending.
    assert!(
        common::wait_until(Duration::from_secs(10), || {
            String::from_utf8_lossy(&common::pane_snapshot_on(
                deck.attach_socket_path(),
                &successor.id,
            ))
            .contains(FILE_SECRET)
        }),
        "stand-in never printed the private fixture file"
    );
    let successor_run_id = env_value(&private_env, "OTEL_RESOURCE_ATTRIBUTES")
        .unwrap()
        .split(',')
        .find_map(|pair| pair.strip_prefix("deck.run.id="))
        .unwrap()
        .to_string();
    close(&deck, &successor);
    assert!(
        !successor_run_id.is_empty(),
        "successor must receive a run id"
    );
    wait_for_private_run(&receiver, &successor_run_id);
    let requests = receiver.requests();
    assert!(
        requests.iter().any(|request| request
            .headers
            .iter()
            .any(|(key, value)| key == "x-secret" && value == HEADER_SECRET)),
        "OTLP authentication header did not arrive"
    );
    let sentinels = [
        OUTPUT_SECRET,
        INPUT_SECRET,
        HOOK_SECRET,
        TITLE_SECRET,
        HEADER_SECRET,
        FILE_SECRET,
        BRANCH_SECRET,
        COMMAND_SECRET,
        CWD_SECRET,
        ENV_SECRET,
    ];
    for request in requests {
        for sentinel in sentinels {
            assert!(
                !body_contains(&request.body, sentinel),
                "private sentinel {sentinel} leaked into OTLP body"
            );
            assert!(
                !request.path.contains(sentinel),
                "private sentinel {sentinel} leaked into OTLP URI"
            );
            assert!(
                request
                    .headers
                    .iter()
                    .filter(|(key, _)| key != "x-secret")
                    .all(|(key, value)| !key.contains(sentinel) && !value.contains(sentinel)),
                "private sentinel {sentinel} leaked into a non-authentication header"
            );
        }
    }
    let log =
        std::fs::read_to_string(deck.workdir().join("deck.log")).expect("captured daemon log");
    let telemetry_lines: Vec<_> = log
        .lines()
        .filter(|line| {
            line.contains(" dot_agent_deck::telemetry: ")
                || line.contains(" dot_agent_deck::telemetry::")
        })
        .collect();
    assert!(
        telemetry_lines
            .iter()
            .any(|line| line.contains("exporting agent-run traces")),
        "captured daemon log must contain the telemetry exporter's startup line"
    );
    for sentinel in sentinels {
        assert!(
            telemetry_lines.iter().all(|line| !line.contains(sentinel)),
            "private sentinel {sentinel} leaked into a telemetry-module log line"
        );
    }
}

/// Scenario: Restore two panes running a PATH-first stand-in named claude, with OTLP configured and no real credentials. Its captured environment must preserve the daemon's explicit telemetry opt-out in one case and supply Claude's default telemetry switches in the other, without enabling prompt or tool-detail logging.
#[spec("telemetry/otlp/006")]
#[test]
fn telemetry_otlp_006_claude_switches_preserve_user_opt_out() {
    use std::os::unix::fs::PermissionsExt;

    let receiver = Receiver::start();
    let shim_dir = common::harness_tempdir().expect("isolated Claude stand-in directory");
    let shim = shim_dir.path().join("claude");
    std::fs::write(
        &shim,
        "#!/bin/sh\nif [ \"$1\" = '--version' ]; then printf 'synthetic claude\\n'; exit 0; fi\nenv > telemetry-agent.env.tmp\nmv telemetry-agent.env.tmp telemetry-agent.env\nprintf 'TELEMETRY_CLAUDE_STANDIN_READY\\n'\nexec cat\n",
    )
    .expect("write Claude stand-in");
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755))
        .expect("make Claude stand-in executable");
    let path = format!(
        "{}:{}",
        shim_dir.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut environments = Vec::new();
    // Exercise the opt-out first so its evidence is available even while the
    // subsequent default-enablement assertion is RED during implementation.
    for opt_out in [true, false] {
        let mut builder = TuiDeck::builder()
            .without_agent_credentials()
            .with_pty_size(180, 45)
            .with_env("PATH", &path)
            .with_env("OTEL_EXPORTER_OTLP_ENDPOINT", &receiver.endpoint)
            .with_continue_session("telemetry-claude-standin", "claude");
        if opt_out {
            builder = builder
                .with_env("CLAUDE_CODE_ENABLE_TELEMETRY", "0")
                .with_env("OTEL_METRICS_EXPORTER", "none");
        }
        let deck = builder.launch_with_fixture("minimal");
        deck.wait_for_string("TELEMETRY_CLAUDE_STANDIN_READY");
        common::wait_for_file_contains(
            &deck.workdir().join("telemetry-agent.env"),
            "DOT_AGENT_DECK_PANE_ID=",
        );
        environments.push(env(&deck));
        close(&deck, &agent(&deck));
    }
    let opted_out = &environments[0];
    assert_eq!(
        env_value(opted_out, "CLAUDE_CODE_ENABLE_TELEMETRY"),
        Some("0"),
        "Claude must preserve the user's telemetry opt-out"
    );
    assert_eq!(
        env_value(opted_out, "OTEL_METRICS_EXPORTER"),
        Some("none"),
        "Claude must preserve the user's disabled metrics exporter"
    );
    for environment in &environments {
        for key in ["OTEL_LOG_USER_PROMPTS", "OTEL_LOG_TOOL_DETAILS"] {
            assert!(
                env_value(environment, key).is_none(),
                "the deck must never enable Claude content logging via {key}"
            );
        }
    }
    let defaults = &environments[1];
    for (key, expected) in [
        ("CLAUDE_CODE_ENABLE_TELEMETRY", "1"),
        ("OTEL_METRICS_EXPORTER", "otlp"),
        ("OTEL_LOGS_EXPORTER", "otlp"),
        ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/protobuf"),
    ] {
        assert_eq!(
            env_value(defaults, key),
            Some(expected),
            "configured export must supply Claude's unset {key}"
        );
    }
}

/// Scenario: Close a stand-in exporting with a custom authentication header to a receiver that redirects every request to another local port. The configured receiver must get a request, while the redirect target must receive neither a request nor the header sentinel.
#[spec("telemetry/otlp/008")]
#[test]
fn telemetry_otlp_008_redirect_never_forwards_credentials() {
    let destination = Receiver::start();
    let source = Receiver::responding(vec![format!(
        "HTTP/1.1 307 Temporary Redirect\r\nLocation: {}/v1/traces\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        destination.endpoint
    )]);
    let deck = TuiDeck::builder()
        .without_agent_credentials()
        .with_env("OTEL_EXPORTER_OTLP_ENDPOINT", &source.endpoint)
        .with_env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json")
        .with_env(
            "OTEL_EXPORTER_OTLP_HEADERS",
            format!("x-honeycomb-team={HEADER_SECRET}"),
        )
        .with_continue_session(
            "telemetry-redirect",
            "/bin/sh -c 'printf TELEMETRY_REDIRECT_READY; exec cat'",
        )
        .launch_with_fixture("minimal");
    deck.wait_for_string("TELEMETRY_REDIRECT_READY");
    close(&deck, &agent(&deck));
    assert!(
        common::wait_until(EXPORT_WAIT, || !source.requests().is_empty()),
        "configured receiver never got an export request"
    );
    assert!(
        source.requests().iter().any(|request| request
            .headers
            .iter()
            .any(|(key, value)| key == "x-honeycomb-team" && value == HEADER_SECRET)),
        "custom authentication header must reach only the configured receiver"
    );
    let redirected = common::wait_until(Duration::from_secs(6), || {
        !destination.requests().is_empty()
    });
    let leaked = destination.requests().iter().any(|request| {
        request
            .headers
            .iter()
            .any(|(_, value)| value.contains(HEADER_SECRET))
    });
    assert!(
        !leaked,
        "custom authentication sentinel reached the redirect target"
    );
    assert!(
        !redirected,
        "OTLP exporter followed a cross-origin redirect"
    );
}

/// Scenario: Close a stand-in while its receiver rejects the first two export attempts with 503 and Retry-After, then accepts requests. The completed run must eventually arrive in exactly one accepted batch with the same run id carried by the retried requests.
#[spec("telemetry/otlp/009")]
#[test]
fn telemetry_otlp_009_transient_failure_retries_run_once() {
    let unavailable = "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string();
    let receiver = Receiver::responding(vec![unavailable.clone(), unavailable,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".into()]);
    let deck = launch(Some(&receiver), false);
    let agent = agent(&deck);
    close(&deck, &agent);
    let run = receiver.run(agent.pane_id_env.as_deref().unwrap());
    let run_id = attribute(&run, "deck.run.id");
    assert!(
        receiver.requests().len() >= 3,
        "503 export failures were not retried"
    );
    assert!(
        receiver
            .requests()
            .iter()
            .take(2)
            .all(|request| !request.accepted
                && String::from_utf8_lossy(&request.body).contains(&run_id)),
        "retry must retain the original completed run batch"
    );
    assert!(
        !common::wait_until(Duration::from_secs(6), || receiver
            .spans()
            .iter()
            .filter(|span| {
                span["name"] == "agent.run" && attribute(span, "deck.run.id") == run_id
            })
            .count()
            > 1),
        "retry delivered duplicate agent.run ids"
    );
}

/// Scenario: Show Thinking for a stand-in waiting on an exit gate, publish its final Working hook, and immediately release it to exit naturally. The completed run must say exited and retain the last Working interval and status-change event.
#[spec("telemetry/otlp/010")]
#[test]
fn telemetry_otlp_010_natural_exit_retains_last_status() {
    use dot_agent_deck::event::EventType;

    let receiver = Receiver::start();
    let deck = TuiDeck::builder()
        .without_agent_credentials()
        .impersonating_pane_signals()
        .with_env("OTEL_EXPORTER_OTLP_ENDPOINT", &receiver.endpoint)
        .with_env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json")
        .with_continue_session("telemetry-natural-exit", "/bin/sh -c 'printf TELEMETRY_EXIT_READY; while [ ! -f telemetry-exit ]; do sleep 0.02; done'")
        .launch_with_fixture("minimal");
    deck.wait_for_string("TELEMETRY_EXIT_READY");
    let agent = agent(&deck);
    let pane = agent.pane_id_env.as_deref().unwrap();
    let events = deck.subscribe_events();
    deck.send_bytes(b"\x04");
    deck.wait_for_string("Dir:");
    let hook = |event: &str| {
        json!({
            "session_id": "telemetry-natural", "pane_id": pane, "agent_id": agent.id,
            "agent_type": "claude_code", "event_type": event,
            "timestamp": chrono::Utc::now().to_rfc3339(), "cwd": deck.workdir(), "tool_name": "Bash"
        })
    };
    common::write_hook_line(deck.hook_socket_path(), &hook("thinking").to_string()).unwrap();
    deck.wait_for_string("Thinking");
    common::write_hook_line(deck.hook_socket_path(), &hook("tool_start").to_string()).unwrap();
    // Establish publication before EOF, without waiting for the telemetry
    // subscriber or the TUI to consume that final status.
    events.wait_for(
        |event| event.event_type == EventType::ToolStart,
        Duration::from_secs(10),
    );
    std::fs::write(deck.workdir().join("telemetry-exit"), "exit").unwrap();
    let run = receiver.run(pane);
    assert_eq!(
        attribute(&run, "deck.run.end_reason"),
        "exited",
        "natural exit must not look like StopAgent"
    );
    assert!(
        receiver
            .spans()
            .iter()
            .any(|span| span["name"] == "agent.status"
                && attribute(span, "deck.run.id") == attribute(&run, "deck.run.id")
                && attribute(span, "deck.status") == "working"),
        "final Working status interval was lost before natural exit: {run}"
    );
    assert!(
        run["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["name"] == "deck.status_change"
                && attribute(event, "deck.status.to") == "working"),
        "final Working status change was lost before natural exit: {run}"
    );
}

/// Scenario: Open a two-role orchestration with a stand-in worker that records its propagated identity, then delegate to its clear=true role to replace it. Closing the replacement must deliver two distinct completed runs for that pane, and the replacement's recorded identity must match its own exported run.
#[spec("telemetry/otlp/011")]
#[test]
fn telemetry_otlp_011_respawn_mints_a_new_run_id() {
    use dot_agent_deck::agent_pty::TabMembership;

    let receiver = Receiver::start();
    let deck = TuiDeck::builder()
        .without_agent_credentials()
        .impersonating_pane_signals()
        .with_pty_size(180, 45)
        .with_env("OTEL_EXPORTER_OTLP_ENDPOINT", &receiver.endpoint)
        .with_env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json")
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");
    let command = "/bin/sh -c 'env > telemetry-respawn.env.tmp; mv telemetry-respawn.env.tmp telemetry-respawn.env; printf TELEMETRY_RESPAWN_READY; exec cat'";
    std::fs::write(deck.workdir().join(".dot-agent-deck.toml"), format!(
        "[[orchestrations]]\nname = \"telemetry-respawn\"\n[[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"cat\"\nstart = true\n[[orchestrations.roles]]\nname = \"coder\"\ncommand = {command:?}\nclear = true\n"
    )).unwrap();
    deck.send_keys(b"\x0e");
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" ");
    deck.wait_for_string("No mode");
    deck.send_keys(b"\x1b[C");
    deck.send_keys(b"\r");
    deck.send_keys(b"\r");
    deck.wait_for_string("coder");
    common::wait_for_file_contains(
        &deck.workdir().join("telemetry-respawn.env"),
        "deck.run.id=",
    );
    let record = |role: &str| {
        common::agent_records_on(deck.attach_socket_path()).into_iter().find(|record| {
        matches!(&record.tab_membership, Some(TabMembership::Orchestration { role_name, .. }) if role_name == role)
    })
    };
    let first = record("coder").expect("first worker");
    let orchestrator = record("orchestrator").expect("orchestrator");
    let run_id_from_env = || {
        let environment =
            std::fs::read_to_string(deck.workdir().join("telemetry-respawn.env")).unwrap();
        env_value(&environment, "OTEL_RESOURCE_ATTRIBUTES")
            .unwrap()
            .split(',')
            .find_map(|pair| pair.strip_prefix("deck.run.id="))
            .unwrap()
            .to_string()
    };
    let first_id = run_id_from_env();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args([
            "delegate",
            "--to",
            "coder",
            "--task",
            "synthetic respawn telemetry check",
        ])
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env(
            "DOT_AGENT_DECK_PANE_ID",
            orchestrator.pane_id_env.as_deref().unwrap(),
        )
        .env("HOME", deck.home_dir())
        .current_dir(deck.workdir())
        .output()
        .expect("delegate to clear=true worker");
    assert!(
        output.status.success(),
        "respawn delegate failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        common::wait_until(Duration::from_secs(15), || run_id_from_env() != first_id),
        "respawn did not propagate a fresh deck.run.id"
    );
    let second_id = run_id_from_env();
    let second = record("coder").expect("replacement worker");
    assert_ne!(first.id, second.id);
    close(&deck, &second);
    let pane = first.pane_id_env.as_deref().unwrap();
    assert!(
        common::wait_until(EXPORT_WAIT, || receiver
            .spans()
            .iter()
            .filter(|span| {
                span["name"] == "agent.run" && attribute(span, "deck.pane.id") == pane
            })
            .count()
            == 2),
        "respawn must export two completed runs for one pane"
    );
    let runs: Vec<_> = receiver
        .spans()
        .into_iter()
        .filter(|span| span["name"] == "agent.run" && attribute(span, "deck.pane.id") == pane)
        .collect();
    let first_run = runs
        .iter()
        .find(|run| attribute(run, "deck.run.id") == first_id)
        .expect("first run id");
    assert!(
        matches!(
            attribute(first_run, "deck.run.end_reason").as_str(),
            "unknown" | "closed"
        ),
        "replacement must end the previous run: {first_run}"
    );
    assert!(
        runs.iter()
            .any(|run| attribute(run, "deck.run.id") == second_id),
        "replacement's propagated id must equal its exported run id"
    );
}

/// Scenario: Keep a stand-in run open and stop its isolated daemon through the harness's normal SIGTERM path. The daemon must exit gracefully in under fifteen seconds and flush that open run with daemon_shutdown as its end reason.
#[spec("telemetry/otlp/012")]
#[test]
fn telemetry_otlp_012_daemon_shutdown_flushes_open_run() {
    let receiver = Receiver::start();
    let deck = launch(Some(&receiver), false);
    let agent = agent(&deck);
    let start = Instant::now();
    let outcome = common::stop_fixture_daemon(deck.workdir(), deck.attach_socket_path());
    assert!(
        matches!(outcome, common::FixtureDaemonStop::Stopped { .. }),
        "daemon did not exit through graceful SIGTERM: {outcome:?}"
    );
    assert!(
        start.elapsed() < Duration::from_secs(15),
        "daemon shutdown exceeded fifteen seconds: {:?}",
        start.elapsed()
    );
    let run = receiver.run(agent.pane_id_env.as_deref().unwrap());
    assert_eq!(
        attribute(&run, "deck.run.end_reason"),
        "daemon_shutdown",
        "shutdown must flush an open run before stopping agents"
    );
}

/// Scenario: Send shell-tool start/end hooks for ordinary, wrapped, sandboxed and private commands to a stand-in pane, plus a non-shell tool. Close the pane and inspect completed operation spans for safe program/word metadata, parent identity, duration and count, rejecting private or raw command text from every request body.
#[spec("telemetry/otlp/014")]
#[test]
fn telemetry_otlp_014_shell_operations_classify_without_content() {
    use dot_agent_deck::event::EventType;

    let receiver = Receiver::start();
    let deck = launch(Some(&receiver), true);
    let agent = agent(&deck);
    let pane = agent.pane_id_env.as_deref().unwrap();
    let events = deck.subscribe_events();
    let cases = [
        ("rwx sandbox exec -- cargo test-fast", "rwx", "sandbox"),
        ("rwx run ci", "rwx", "run"),
        ("FOO=1 /usr/bin/make build", "make", "build"),
        ("cd /tmp/x && npm run lint", "npm", "run"),
        ("echo PRIVATE_WORD_1741_17ad", "echo", ""),
        ("cat PRIVATE_FILE_1741_91bd", "cat", ""),
        ("kubectl apply -f /secret/path", "kubectl", "apply"),
        ("npm run \"Weird Name\"", "npm", "run"),
        ("npm \"Weird Name\"", "npm", ""),
        (
            "env FOO=1 sudo command time nice /usr/bin/cargo --locked check",
            "cargo",
            "check",
        ),
        (
            "export TOKEN=PRIVATE_ASSIGNMENT_1741; true || make check",
            "make",
            "check",
        ),
        ("rg privateword1741", "rg", ""),
        ("printf privateword1741", "printf", ""),
        ("grep privateword1741", "grep", ""),
        ("sed privateword1741", "sed", ""),
        ("awk privateword1741", "awk", ""),
        ("PRIVATE_PROGRAM_1741 PRIVATE_WORD_1741", "", ""),
        ("make a234567890123456789012345678901234", "make", ""),
        ("make check\necho PRIVATE_SECOND_LINE_1741", "make", "check"),
    ];
    for (tool, command) in std::iter::once(("Read", "PRIVATE_NON_SHELL_1741"))
        .chain(cases.iter().map(|(command, ..)| ("Bash", *command)))
    {
        for (kind, expected) in [
            ("tool_start", EventType::ToolStart),
            ("tool_end", EventType::ToolEnd),
        ] {
            let payload = json!({
                "session_id": "telemetry-operations", "pane_id": pane,
                "agent_id": agent.id, "agent_type": "claude_code", "event_type": kind,
                "timestamp": chrono::Utc::now().to_rfc3339(), "cwd": deck.workdir(),
                "tool_name": tool, "tool_detail": command
            });
            common::write_hook_line(deck.hook_socket_path(), &payload.to_string()).unwrap();
            events.wait_for(
                |event| {
                    event.event_type == expected && event.tool_detail.as_deref() == Some(command)
                },
                Duration::from_secs(10),
            );
        }
    }
    close(&deck, &agent);
    let run = receiver.run(pane);
    let spans = receiver.spans();
    let operations: Vec<_> = spans
        .iter()
        .filter(|span| span["name"] == "agent.operation" && attribute(span, "deck.pane.id") == pane)
        .collect();
    assert_eq!(
        operations.len(),
        cases.len(),
        "one operation per shell start/end pair, excluding Read; exported spans: {spans:?}"
    );
    let mut actual: Vec<_> = operations
        .iter()
        .map(|span| {
            (
                attribute(span, "deck.operation.program"),
                attribute(span, "deck.operation.word"),
            )
        })
        .collect();
    let mut expected: Vec<_> = cases
        .iter()
        .map(|(_, program, word)| (program.to_string(), word.to_string()))
        .collect();
    actual.sort();
    expected.sort();
    assert_eq!(actual, expected, "operation classification");
    for operation in &operations {
        for key in ["deck.operation.program", "deck.operation.word"] {
            if attribute(operation, key).is_empty() {
                assert!(
                    !operation["attributes"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|attr| attr["key"] == key),
                    "unsafe attribute must be absent, not empty: {operation}"
                );
            }
        }
        assert_eq!(operation["parentSpanId"], run["spanId"]);
        assert_eq!(operation["traceId"], run["traceId"]);
        assert_eq!(
            attribute(operation, "deck.run.id"),
            attribute(&run, "deck.run.id")
        );
        let nanos = |key: &str| operation[key].as_str().unwrap().parse::<u64>().unwrap();
        assert!(
            nanos("endTimeUnixNano") >= nanos("startTimeUnixNano"),
            "operation duration: {operation}"
        );
    }
    assert_eq!(
        integer_attribute(&run, "deck.operations.count"),
        cases.len() as u64
    );
    for request in receiver.requests() {
        let body = String::from_utf8_lossy(&request.body);
        for secret in [
            "PRIVATE_",
            "privateword1741",
            "/secret/path",
            "/tmp/x",
            "Weird Name",
        ] {
            assert!(!body.contains(secret), "operation content leaked: {secret}");
        }
        for (command, ..) in cases {
            assert!(!body.contains(command), "raw command leaked: {command}");
        }
    }
}

/// Scenario: Link a stand-in pane to an offline fixture PR whose fake gh rollup changes pending, failing, pending, passing, observing each served check state before advancing. Closing the pane must export four CI-change events, two completed CI intervals with final check counts, two rounds and one failure.
#[spec("telemetry/otlp/015")]
#[test]
fn telemetry_otlp_015_ci_rounds_follow_polled_check_rollups() {
    let receiver = Receiver::start();
    let scratch = common::harness_tempdir().unwrap();
    let repo = scratch.path().join("repo");
    let bin = scratch.path().join("bin");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    for args in [
        vec!["init", "-b", "telemetry-ci"],
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
    let reply = scratch.path().join("rollup.json");
    write_executable(
        &bin.join("gh"),
        "#!/bin/sh\n[ \"$1\" = pr ] && [ \"$2\" = list ] || exit 91\ncat \"$TELEMETRY_GH_REPLY\"\n",
    );
    let set_checks = |conclusion: Option<&str>| {
        let checks = match conclusion {
            Some(conclusion) => {
                json!([{"__typename": "CheckRun", "name": "build", "workflowName": "CI", "status": "COMPLETED", "conclusion": conclusion}, {"__typename": "CheckRun", "name": "optional", "workflowName": "CI", "status": "COMPLETED", "conclusion": "SKIPPED"}])
            }
            None => {
                json!([{"__typename": "CheckRun", "name": "build", "workflowName": "CI", "status": "IN_PROGRESS", "conclusion": null}])
            }
        };
        let value = json!([{"number": 1741, "state": "OPEN", "isDraft": false, "reviewDecision": "REVIEW_REQUIRED", "headRefName": "telemetry-ci", "url": "https://github.com/test-org/test-repo/pull/1741", "statusCheckRollup": checks}]);
        let pending = scratch.path().join("rollup.pending");
        std::fs::write(&pending, value.to_string()).unwrap();
        std::fs::rename(&pending, &reply).unwrap();
    };
    set_checks(None);
    let deck = TuiDeck::builder()
        .without_agent_credentials()
        .impersonating_pane_signals()
        .with_pty_size(180, 45)
        .with_env(
            "PATH",
            format!("{}:{}", bin.display(), common::path_with_binary_dir()),
        )
        .with_env("TELEMETRY_GH_REPLY", reply.to_string_lossy())
        .with_env("DOT_AGENT_DECK_PR_REFRESH_SECS", "1")
        .with_env("OTEL_EXPORTER_OTLP_ENDPOINT", &receiver.endpoint)
        .with_env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json")
        .with_continue_session("telemetry-ci-bootstrap", "cat")
        .launch_with_fixture("minimal");
    deck.wait_for_string("[Command Mode Ctrl+D]");
    let previous = agent(&deck);
    close(&deck, &previous);
    let current = start_on_pane(&deck, &previous, &repo, "cat".into());
    let pane = current.pane_id_env.as_deref().unwrap();
    let hook = json!({"session_id": "telemetry-ci", "pane_id": pane, "agent_id": current.id,
        "agent_type": "claude_code", "event_type": "session_start", "cwd": repo,
        "timestamp": chrono::Utc::now().to_rfc3339()});
    common::write_hook_line(deck.hook_socket_path(), &hook.to_string()).unwrap();
    for (conclusion, expected) in [
        (None, "pending"),
        (Some("FAILURE"), "failing"),
        (None, "pending"),
        (Some("SUCCESS"), "passing"),
    ] {
        set_checks(conclusion);
        assert!(
            common::wait_until(Duration::from_secs(15), || {
                common::agent_records_on(deck.attach_socket_path())
                    .iter()
                    .find(|record| record.id == current.id)
                    .and_then(|record| record.live.as_ref())
                    .and_then(|live| live.pull_request.as_ref())
                    .and_then(|pr| pr.checks.as_ref())
                    .is_some_and(|checks| {
                        serde_json::to_value(checks).unwrap()["state"] == expected
                    })
            }),
            "fixture PR never served check state {expected}"
        );
    }
    close(&deck, &current);
    assert!(
        common::wait_until(EXPORT_WAIT, || receiver
            .spans()
            .iter()
            .filter(|span| span["name"] == "agent.run" && attribute(span, "deck.pane.id") == pane)
            .count()
            == 2),
        "bootstrap and linked PR runs must both finish"
    );
    let runs: Vec<_> = receiver
        .spans()
        .into_iter()
        .filter(|span| span["name"] == "agent.run" && attribute(span, "deck.pane.id") == pane)
        .collect();
    let run = runs
        .iter()
        .max_by_key(|run| {
            run["startTimeUnixNano"]
                .as_str()
                .unwrap()
                .parse::<u64>()
                .unwrap()
        })
        .unwrap();
    let changes: Vec<_> = run["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["name"] == "deck.ci.change")
        .collect();
    assert_eq!(changes.len(), 4, "CI changes: {run}");
    assert!(
        !changes[0]["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|attr| attr["key"] == "deck.ci.from"),
        "first CI event must omit the unknown prior state: {}",
        changes[0]
    );
    for (i, expected) in ["pending", "failing", "pending", "passing"]
        .iter()
        .enumerate()
    {
        assert_eq!(attribute(changes[i], "deck.ci.to"), *expected);
        assert_eq!(integer_attribute(changes[i], "deck.pr.number"), 1741);
        assert_eq!(
            attribute(changes[i], "deck.ci.from"),
            if i == 0 {
                ""
            } else {
                ["pending", "failing", "pending"][i - 1]
            }
        );
    }
    assert_eq!(integer_attribute(run, "deck.ci.rounds.count"), 2);
    assert_eq!(integer_attribute(run, "deck.ci.failures.count"), 1);
    let intervals: Vec<_> = receiver
        .spans()
        .into_iter()
        .filter(|span| {
            span["name"] == "agent.ci"
                && attribute(span, "deck.run.id") == attribute(run, "deck.run.id")
        })
        .collect();
    assert_eq!(intervals.len(), 2, "CI intervals: {intervals:?}");
    for (result, passing, failing) in [("failing", 0, 1), ("passing", 1, 0)] {
        let span = intervals
            .iter()
            .find(|span| attribute(span, "deck.ci.result") == result)
            .unwrap_or_else(|| panic!("missing CI result {result}: {intervals:?}"));
        assert_eq!(span["parentSpanId"], run["spanId"]);
        assert_eq!(span["traceId"], run["traceId"]);
        assert_eq!(integer_attribute(span, "deck.pr.number"), 1741);
        assert_eq!(integer_attribute(span, "deck.ci.checks.passing"), passing);
        assert_eq!(integer_attribute(span, "deck.ci.checks.failing"), failing);
        assert_eq!(integer_attribute(span, "deck.ci.checks.skipped"), 1);
    }
}

const AGENT_LAUNCH_RECORDER: &str = r#"#!/usr/bin/env python3
import json, os, stat, sys
if 'app-server' in sys.argv:
    sys.exit(0)
record = {'argv': sys.argv[1:], 'env': dict(os.environ)}
if '--config' in sys.argv:
    path = sys.argv[sys.argv.index('--config') + 1]
    record['config_path'] = path
    record['config_mode'] = stat.S_IMODE(os.stat(path).st_mode)
    record['config'] = open(path).read()
with open('telemetry-launch.json.tmp', 'w') as output:
    json.dump(record, output)
os.rename('telemetry-launch.json.tmp', 'telemetry-launch.json')
print('TELEMETRY_LAUNCH_READY', flush=True)
os.execvp('cat', ['cat'])
"#;

/// A `codex` at `path` as npm installs it: a link to `bin/codex.js` in an
/// `@openai/codex` package under `root`, here holding `source`. The deck's
/// shim adds to Codex only in a form it recognises, and this is one.
fn npm_codex_stand_in(root: &Path, path: &Path, source: &str) {
    let package = root.join("lib/node_modules/@openai/codex/bin");
    std::fs::create_dir_all(&package).unwrap();
    write_executable(&package.join("codex.js"), source);
    std::os::unix::fs::symlink(package.join("codex.js"), path).unwrap();
}

/// A native `devin` at `path` that runs the script `script` with its own
/// arguments, built with `cc`: the deck's shim adds to Devin only when it is
/// a native program, and the recorder is a Python script.
fn native_devin_stand_in(path: &Path, script: &Path) {
    let script = script.to_str().unwrap();
    assert!(
        !script.contains(['"', '\\', '\n']),
        "a C string literal: {script}"
    );
    let source = path.with_extension("c");
    std::fs::write(
        &source,
        format!(
            "#include <unistd.h>\nint main(int argc, char **argv) {{ (void)argc; execv(\"{script}\", argv); return 127; }}\n"
        ),
    )
    .unwrap();
    let out = std::process::Command::new("cc")
        .arg("-o")
        .arg(path)
        .arg(&source)
        .output()
        .expect("cc on PATH to build the native Devin stand-in");
    assert!(out.status.success(), "building the Devin stand-in: {out:?}");
}

fn launch_record(deck: &TuiDeck) -> Value {
    let path = deck.workdir().join("telemetry-launch.json");
    common::wait_for_file_contains(&path, "argv");
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn assert_launch_path(record: &Value, deck: &TuiDeck, original_path: &str, enabled: bool) {
    let path = record["env"]["PATH"].as_str().expect("agent PATH");
    let shim = deck.workdir().join("state/telemetry/bin");
    if enabled {
        assert_eq!(
            std::env::split_paths(path).next().as_deref(),
            Some(shim.as_path()),
            "export ON must prepend the shim even for a scripted/unknown agent"
        );
    } else {
        assert_eq!(path, original_path, "export OFF must preserve agent PATH");
        assert!(
            !std::env::split_paths(path).any(|entry| entry == shim),
            "export OFF must not put the telemetry shim on PATH"
        );
    }
}

// Preserve the fixture PATH through the daemon's login-shell capture, without
// depending on host /etc/profile additions. All real launch commands still run.
fn launch_test_shell(root: &Path) -> std::path::PathBuf {
    let shell = root.join("fixture-shell");
    write_executable(
        &shell,
        "#!/bin/sh\nif [ \"$1\" = -ilc ]; then shift; exec /bin/sh -c \"$@\"; fi\nexec /bin/sh \"$@\"\n",
    );
    shell
}

/// Scenario: Start a Codex recorder, installed where npm puts Codex's launcher, directly and through scripts that call codex by name, change CODEX_HOME, opt out of trace export or put the real Codex's directory in front of PATH, and through an absolute executable path. Inspect the real argv and environment for safe signal activation, preserved arguments and user choices, no header secret or logs exporter, and unchanged PATH/argv when export is off.
#[spec("telemetry/otlp/016")]
#[test]
fn telemetry_otlp_016_codex_launch_enables_only_safe_signals() {
    let receiver = Receiver::start();
    for case in [
        "script",
        "plain",
        "user-config",
        "script-home",
        "script-traces-none",
        "script-prepend",
        "absolute",
        "off",
    ] {
        let enabled = case != "off";
        let user_config = case == "user-config";
        let scratch = common::harness_tempdir().unwrap();
        let bin = scratch.path().join("bin");
        let codex_home = scratch.path().join("codex-home");
        let alt_home = scratch.path().join("alternate-codex-home");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&codex_home).unwrap();
        std::fs::create_dir_all(&alt_home).unwrap();
        std::fs::write(
            alt_home.join("config.toml"),
            "[otel]\nmetrics_exporter = \"none\"\n",
        )
        .unwrap();
        npm_codex_stand_in(scratch.path(), &bin.join("codex"), AGENT_LAUNCH_RECORDER);
        if user_config {
            std::fs::write(
                codex_home.join("config.toml"),
                "[otel]\ntrace_exporter = \"none\"\n",
            )
            .unwrap();
        }
        let wrapper = scratch.path().join("run-codex.sh");
        let setup = match case {
            "script-home" => format!(
                "export CODEX_HOME={}\n",
                shell_quote(alt_home.to_str().unwrap())
            ),
            "script-traces-none" => "export OTEL_TRACES_EXPORTER=none\n".to_string(),
            // The documented limit: the real Codex's directory ahead of the
            // deck's on PATH finds the real one first.
            "script-prepend" => format!(
                "export PATH={}:\"$PATH\"\n",
                shell_quote(bin.to_str().unwrap())
            ),
            _ => "export FOO=1\n".to_string(),
        };
        write_executable(&wrapper, &format!("#!/bin/sh\n{setup}codex \"$@\"\n"));
        let program = match case {
            "plain" | "user-config" => "codex".to_string(),
            "absolute" => shell_quote(bin.join("codex").to_str().unwrap()),
            _ => shell_quote(wrapper.to_str().unwrap()),
        };
        let original_path = format!("{}:{}", bin.display(), common::path_with_binary_dir());
        let mut builder = TuiDeck::builder()
            .without_agent_credentials()
            .with_env("SHELL", launch_test_shell(scratch.path()).to_string_lossy())
            .with_env("PATH", &original_path)
            .with_env("CODEX_HOME", codex_home.to_string_lossy())
            .with_continue_session(
                "telemetry-codex-recorder",
                format!("{program} 'argument with spaces' --fixture-flag"),
            );
        if enabled {
            builder = builder
                .with_env("OTEL_EXPORTER_OTLP_ENDPOINT", &receiver.endpoint)
                .with_env(
                    "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
                    format!("{}/codex-traces", receiver.endpoint),
                )
                .with_env(
                    "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
                    format!("{}/codex-metrics", receiver.endpoint),
                )
                .with_env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json")
                .with_env(
                    "OTEL_EXPORTER_OTLP_HEADERS",
                    format!("x-secret={HEADER_SECRET}"),
                );
        }
        let deck = builder.launch_with_fixture("minimal");
        let record = launch_record(&deck);
        let argv: Vec<_> = record["argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| arg.as_str().unwrap())
            .collect();
        if case == "script-prepend" {
            let entries: Vec<_> = std::env::split_paths(record["env"]["PATH"].as_str().unwrap())
                .take(2)
                .collect();
            assert_eq!(
                entries,
                [bin.clone(), deck.workdir().join("state/telemetry/bin")],
                "the script's own directory ahead of the deck's"
            );
        } else {
            assert_launch_path(&record, &deck, &original_path, enabled);
        }
        assert_eq!(
            record["env"]["CODEX_HOME"],
            if case == "script-home" {
                alt_home.to_str().unwrap()
            } else {
                codex_home.to_str().unwrap()
            },
            "{case}: effective CODEX_HOME"
        );
        if !matches!(case, "plain" | "user-config" | "absolute") {
            assert_eq!(
                record["env"]["FOO"],
                if matches!(
                    case,
                    "script-home" | "script-traces-none" | "script-prepend"
                ) {
                    Value::Null
                } else {
                    json!("1")
                },
                "{case}: script must really execute"
            );
        }
        assert!(
            !argv.join(" ").contains(HEADER_SECRET),
            "secret in Codex argv"
        );
        assert!(
            !argv.iter().any(|arg| arg.starts_with("otel.exporter")),
            "deck must leave content-bearing log export off: {argv:?}"
        );
        let overrides: Vec<_> = argv
            .windows(2)
            .filter(|pair| pair[0] == "-c" && pair[1].starts_with("otel."))
            .map(|pair| pair[1])
            .collect();
        let original_args: Vec<_> = argv
            .iter()
            .enumerate()
            .filter_map(|(index, arg)| {
                if (*arg == "-c"
                    && argv
                        .get(index + 1)
                        .is_some_and(|next| next.starts_with("otel.")))
                    || (index > 0 && argv[index - 1] == "-c" && arg.starts_with("otel."))
                {
                    None
                } else {
                    Some(*arg)
                }
            })
            .collect();
        assert_eq!(
            original_args,
            ["argument with spaces", "--fixture-flag"],
            "{case}: preserve original argv"
        );
        close(&deck, &agent(&deck));
        if enabled
            && !matches!(
                case,
                "user-config" | "script-home" | "script-prepend" | "absolute"
            )
        {
            assert_eq!(
                overrides.len(),
                if case == "script-traces-none" { 1 } else { 2 },
                "{case}: Codex needs only the enabled trace/metric activation: {argv:?}"
            );
            for (signal, endpoint) in [("trace", "codex-traces"), ("metrics", "codex-metrics")] {
                if case == "script-traces-none" && signal == "trace" {
                    assert!(
                        !overrides
                            .iter()
                            .any(|value| value.starts_with("otel.trace_exporter=")),
                        "in-script trace opt-out: {argv:?}"
                    );
                    continue;
                }
                let value = overrides
                    .iter()
                    .find(|value| value.starts_with(&format!("otel.{signal}_exporter=")))
                    .unwrap_or_else(|| panic!("missing Codex {signal} exporter: {argv:?}"));
                assert!(
                    value.contains(&format!("{}/{endpoint}", receiver.endpoint)),
                    "signal endpoint: {value}"
                );
                assert!(value.contains("json"), "signal protocol: {value}");
            }
            assert_eq!(
                record["env"]["OTEL_EXPORTER_OTLP_HEADERS"],
                format!("x-secret={HEADER_SECRET}")
            );
            assert!(
                record["env"]["OTEL_RESOURCE_ATTRIBUTES"]
                    .as_str()
                    .unwrap()
                    .contains("deck.run.id=")
            );
        } else {
            assert!(
                overrides.is_empty(),
                "{case}: off/user-owned/absolute/shadowed launch must not receive overrides: {argv:?}"
            );
            if !enabled {
                assert!(record["env"].get("OTEL_RESOURCE_ATTRIBUTES").is_none());
            }
        }
    }
}

/// Scenario: Start a Devin recorder behind a native program directly and through scripts that call devin by name, select another config home, disable OTel or metrics, or supply their own config, plus an absolute-path launch and export off. Inspect argv, PATH and the private overlay for preserved user settings, identity, permissions and absent header secrets, then close the pane and require overlay removal.
#[spec("telemetry/otlp/017")]
#[test]
fn telemetry_otlp_017_devin_launch_config_is_private_and_temporary() {
    for case in [
        "script",
        "plain",
        "user-otel",
        "script-home",
        "script-disabled",
        "script-metrics-none",
        "script-config",
        "absolute",
        "off",
    ] {
        let enabled = case != "off";
        let user_otel = case == "user-otel";
        let receiver = Receiver::start();
        let scratch = common::harness_tempdir().unwrap();
        let bin = scratch.path().join("bin");
        let config_home = scratch.path().join("config");
        let alt_config_home = scratch.path().join("alternate-config");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(config_home.join("devin")).unwrap();
        std::fs::create_dir_all(alt_config_home.join("devin")).unwrap();
        std::fs::write(
            alt_config_home.join("devin/config.json"),
            r#"{"theme_mode":"light"}"#,
        )
        .unwrap();
        let recorder = scratch.path().join("devin-recorder.py");
        write_executable(&recorder, AGENT_LAUNCH_RECORDER);
        native_devin_stand_in(&bin.join("devin"), &recorder);
        let source = if user_otel {
            "{ // user choices\n\"theme_mode\": \"dark\", \"otel\": {\"endpoint\": \"http://127.0.0.1:9/user-owned\", \"disable_user_prompt_logging\": false, \"disable_tool_logging\": false, \"resource_attributes\": {\"user.marker\": \"preserved\"}}}"
        } else {
            "{ // user config must survive\n\"theme_mode\": \"dark\"}"
        };
        std::fs::write(config_home.join("devin/config.json"), source).unwrap();
        let explicit_config = scratch.path().join("explicit-devin.json");
        std::fs::write(
            &explicit_config,
            r#"{"theme_mode":"light","otel":{"enabled":false}}"#,
        )
        .unwrap();
        let disabled_home = scratch.path().join("disabled-config");
        std::fs::create_dir_all(disabled_home.join("devin")).unwrap();
        std::fs::write(
            disabled_home.join("devin/config.json"),
            r#"{"otel":{"enabled":false}}"#,
        )
        .unwrap();
        let wrapper = scratch.path().join("run-devin.sh");
        let setup = match case {
            "script-home" => format!(
                "export XDG_CONFIG_HOME={}\n",
                shell_quote(alt_config_home.to_str().unwrap())
            ),
            "script-disabled" => format!(
                "export XDG_CONFIG_HOME={}\n",
                shell_quote(disabled_home.to_str().unwrap())
            ),
            "script-metrics-none" => "export OTEL_METRICS_EXPORTER=none\n".to_string(),
            _ => "export FOO=1\n".to_string(),
        };
        let config_arg = if case == "script-config" {
            format!(
                "--config {} ",
                shell_quote(explicit_config.to_str().unwrap())
            )
        } else {
            String::new()
        };
        write_executable(
            &wrapper,
            &format!("#!/bin/sh\n{setup}devin {config_arg}\"$@\"\n"),
        );
        let program = match case {
            "plain" | "user-otel" => "devin".to_string(),
            "absolute" => shell_quote(bin.join("devin").to_str().unwrap()),
            _ => shell_quote(wrapper.to_str().unwrap()),
        };
        let original_path = format!("{}:{}", bin.display(), common::path_with_binary_dir());
        let mut builder = TuiDeck::builder()
            .without_agent_credentials()
            .with_env("SHELL", launch_test_shell(scratch.path()).to_string_lossy())
            .with_env("PATH", &original_path)
            .with_env("XDG_CONFIG_HOME", config_home.to_string_lossy())
            .with_continue_session(
                "telemetry-devin-recorder",
                format!("{program} 'argument with spaces' --fixture-flag"),
            );
        if enabled {
            builder = builder
                .with_env("OTEL_EXPORTER_OTLP_ENDPOINT", &receiver.endpoint)
                .with_env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json")
                .with_env("OTEL_RESOURCE_ATTRIBUTES", "user.region=test")
                .with_env(
                    "OTEL_EXPORTER_OTLP_HEADERS",
                    format!("x-secret={HEADER_SECRET}"),
                );
        }
        let deck = builder.launch_with_fixture("minimal");
        let record = launch_record(&deck);
        assert_launch_path(&record, &deck, &original_path, enabled);
        assert!(!record["argv"].to_string().contains(HEADER_SECRET));
        let argv = record["argv"].as_array().unwrap();
        let original_args: Vec<_> = argv
            .iter()
            .enumerate()
            .filter_map(|(index, arg)| {
                if arg == "--config" || (index > 0 && argv[index - 1] == "--config") {
                    None
                } else {
                    Some(arg.as_str().unwrap())
                }
            })
            .collect();
        assert_eq!(
            original_args,
            ["argument with spaces", "--fixture-flag"],
            "{case}: preserve original argv"
        );
        let launched = agent(&deck);
        let generated_path = record.get("config_path").and_then(Value::as_str);
        if let Some(path) = generated_path {
            assert!(
                Path::new(path).exists(),
                "{case}: config must exist while agent is alive"
            );
        }
        close(&deck, &launched);
        if matches!(
            case,
            "off" | "absolute" | "script-disabled" | "script-config"
        ) {
            if case == "script-config" {
                assert_eq!(
                    generated_path,
                    explicit_config.to_str(),
                    "explicit --config must be unchanged"
                );
                assert!(
                    explicit_config.exists(),
                    "user config must not be deleted at run end"
                );
                continue;
            }
            assert!(
                record.get("config_path").is_none(),
                "{case}: disabled/user-owned/absolute launch must not add --config; argv: {}",
                record["argv"]
            );
            if !enabled {
                assert!(record["env"].get("OTEL_RESOURCE_ATTRIBUTES").is_none());
            }
            continue;
        }
        let path = record["config_path"].as_str().unwrap_or_else(|| {
            panic!(
                "{case}: enabled Devin launch must pass --config; argv: {}",
                record["argv"]
            )
        });
        assert!(
            Path::new(path).starts_with(deck.workdir().join("state")),
            "Devin overlay must live under the isolated daemon state dir: {path}"
        );
        assert_eq!(record["config_mode"], 0o600, "Devin overlay permissions");
        let raw = record["config"].as_str().unwrap();
        assert!(
            !raw.contains(HEADER_SECRET),
            "header secret in Devin config"
        );
        let config: Value = serde_json::from_str(raw).expect("generated Devin overlay JSON");
        assert_eq!(
            config["theme_mode"],
            if case == "script-home" {
                "light"
            } else {
                "dark"
            },
            "JSONC user settings must survive"
        );
        let otel = &config["otel"];
        let resources = &otel["resource_attributes"];
        assert!(
            !resources["deck.run.id"]
                .as_str()
                .unwrap_or_default()
                .is_empty(),
            "Devin resource run id: {config}"
        );
        assert_eq!(resources["user.region"], "test");
        assert_eq!(otel["disable_user_prompt_logging"], !user_otel);
        assert_eq!(otel["disable_tool_logging"], !user_otel);
        if case == "script-metrics-none" {
            assert!(
                otel.get("metrics_endpoint").is_none() && otel.get("endpoint").is_none(),
                "in-script metric opt-out: {config}"
            );
            assert_eq!(
                otel["logs_endpoint"],
                format!("{}/v1/logs", receiver.endpoint)
            );
        }
        if user_otel {
            assert_eq!(otel["endpoint"], "http://127.0.0.1:9/user-owned");
            assert_eq!(resources["user.marker"], "preserved");
        } else {
            assert!(
                otel.to_string().contains(&receiver.endpoint),
                "Devin endpoint missing: {config}"
            );
        }
        assert!(
            common::wait_until(Duration::from_secs(10), || !Path::new(path).exists()),
            "Devin overlay remains after pane close: {path}"
        );
        let run = receiver.run(launched.pane_id_env.as_deref().unwrap());
        assert_eq!(resources["deck.run.id"], attribute(&run, "deck.run.id"));
    }
}
