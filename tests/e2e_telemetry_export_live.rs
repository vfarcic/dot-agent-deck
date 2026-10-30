#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]

//! Join real interactive agent telemetry to the deck's exported run.

mod common;

use common::TuiDeck;
use dot_agent_deck::daemon_protocol::AttachRequest;
use serde_json::Value;
use spec::spec;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::Duration;

const EXPORT_WAIT: Duration = Duration::from_secs(25);

#[derive(Clone, Debug)]
struct Request {
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
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
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind isolated OTLP receiver");
        let addr = listener.local_addr().unwrap();
        let endpoint = format!("http://{addr}");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let captured = Arc::clone(&requests);
        let stopped = Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            // Blocking accept: Drop wakes it with a connection of its own.
            for stream in listener.incoming() {
                if stopped.load(Ordering::Relaxed) {
                    break;
                }
                let stream = stream.expect("accept OTLP request");
                // Native SDKs can open a connection and cancel it at shutdown
                // before sending an HTTP request. Keep accepting the deck's
                // final batch after that empty connection.
                let Some(request) = read_request(&stream) else {
                    continue;
                };
                let protobuf = request
                    .headers
                    .iter()
                    .any(|(key, value)| key == "content-type" && value.contains("protobuf"));
                captured.lock().unwrap().push(request);
                let mut stream = stream;
                let response: &[u8] = if protobuf {
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/x-protobuf\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                } else {
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                };
                let _ = stream.write_all(response);
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
            .filter(|request| request.path == "/v1/traces")
            // OpenCode chooses its own encoding, independently of the deck's
            // HTTP/JSON setting. Only the deck's JSON traces contain agent.run.
            .filter(|request| serde_json::from_slice::<Value>(&request.body).is_ok())
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

fn read_request(stream: &TcpStream) -> Option<Request> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let path = line.split_whitespace().nth(1)?.to_string();
    let mut headers = Vec::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        if line == "\r\n" {
            break;
        }
        let (key, value) = line.trim_end().split_once(':')?;
        headers.push((key.to_ascii_lowercase(), value.trim().to_string()));
    }
    let mut body = Vec::new();
    if headers
        .iter()
        .any(|(key, value)| key == "transfer-encoding" && value.eq_ignore_ascii_case("chunked"))
    {
        loop {
            line.clear();
            if reader.read_line(&mut line).ok()? == 0 {
                return None;
            }
            let size = usize::from_str_radix(line.trim().split(';').next()?, 16).ok()?;
            if size == 0 {
                break;
            }
            let start = body.len();
            body.resize(start + size, 0);
            reader.read_exact(&mut body[start..]).ok()?;
            let mut crlf = [0; 2];
            reader.read_exact(&mut crlf).ok()?;
        }
    } else {
        let size = headers
            .iter()
            .find(|(key, _)| key == "content-length")
            .map(|(_, value)| value.parse::<usize>().expect("Content-Length"))
            .unwrap_or(0);
        body.resize(size, 0);
        reader.read_exact(&mut body).ok()?;
    }
    Some(Request {
        path,
        headers,
        body,
    })
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

/// Scenario: Start trusted interactive Claude Haiku in a real attached pane and ask it to copy a uniquely named sentinel through Bash, visibly completing its turn. Close the pane and require the deck's run plus Claude's own metrics or logs to reach the local receiver with the same run id, while deck-owned trace requests exclude the prompt, file contents, filenames and working-directory path.
#[spec("telemetry/otlp/005")]
#[test]
fn telemetry_otlp_005_real_claude_telemetry_joins_deck_run() {
    skip_unless!(common::check_claude_available());
    let receiver = Receiver::start();
    let deck = TuiDeck::builder()
        .with_pty_size(180, 50)
        .with_imported_claude_credentials()
        .with_claude_trust_workdir()
        .with_env("OTEL_EXPORTER_OTLP_ENDPOINT", &receiver.endpoint)
        .with_env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json")
        .with_env("OTEL_METRIC_EXPORT_INTERVAL", "2000")
        .with_env("OTEL_LOGS_EXPORT_INTERVAL", "2000")
        .with_continue_session(
            "telemetry-haiku",
            "claude --model claude-haiku-4-5-20251001 --allowedTools Bash Read Write",
        )
        .launch_with_fixture("chain-smoke-claude");
    const SENTINEL: &str = "telemetry_1741_sentinel_4b9e.txt";
    const PROOF: &str = "telemetry_1741_proof_4b9e.txt";
    std::fs::write(deck.workdir().join(SENTINEL), "TELEMETRY_REAL_AGENT_OK\n").unwrap();
    deck.wait_for_string("Claude Code v");
    deck.send_bytes(b"\x04");
    deck.wait_for_string("Dir:");
    let agent = common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .find(|agent| agent.display_name.as_deref() == Some("telemetry-haiku"))
        .expect("real interactive Haiku pane");
    let pane = agent.pane_id_env.as_deref().expect("pane id");
    let start = deck.stream_text().len();
    let directive = format!(
        "Use Bash exactly once to run: sleep 3; cat '{}/{SENTINEL}' > '{}/{PROOF}'. Do not modify any other file. Then reply TELEMETRY_DONE and end your turn.",
        deck.workdir().display(),
        deck.workdir().display(),
    );
    common::post_message_on(deck.attach_socket_path(), pane, &agent.id, &directive)
        .expect("post sentinel task to Claude");
    assert!(
        common::wait_until(Duration::from_secs(120), || {
            let stream = deck.stream_text();
            let turn = stream.get(start..).unwrap_or_default();
            common::match_needles_in_order(
                turn.as_bytes(),
                &["Thinking", "Working", "Bash", "Idle"],
            ) == 4
        }),
        "real Claude must visibly complete Thinking → Working/Bash → Idle:\n{}",
        deck.snapshot_grid()
    );
    common::wait_for_file_trimmed_eq(
        &deck.workdir().join(PROOF),
        "TELEMETRY_REAL_AGENT_OK",
        Duration::from_secs(10),
    )
    .expect("real Claude must copy the unique fixture sentinel");
    assert!(
        common::wait_until(EXPORT_WAIT, || receiver.requests().iter().any(|request| {
            matches!(request.path.as_str(), "/v1/metrics" | "/v1/logs")
        })),
        "Claude Code never exported its own metrics or logs to the configured receiver"
    );
    let response = common::attach_request_on(
        deck.attach_socket_path(),
        &AttachRequest::StopAgent {
            id: agent.id.clone(),
        },
    )
    .unwrap();
    assert!(response.ok, "close Claude pane: {response:?}");
    let run = receiver.run(pane);
    let run_id = attribute(&run, "deck.run.id");
    assert!(!run_id.is_empty(), "deck run must have a join id");
    assert!(
        common::wait_until(EXPORT_WAIT, || receiver.requests().iter().any(|request| {
            if !matches!(request.path.as_str(), "/v1/metrics" | "/v1/logs") {
                return false;
            }
            let body: Value =
                serde_json::from_slice(&request.body).expect("Claude OTLP body must be JSON");
            ["resourceMetrics", "resourceLogs"].iter().any(|key| {
                body[key]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|resource| attribute(&resource["resource"], "deck.run.id") == run_id)
            })
        })),
        "Claude metrics/logs must carry the deck's deck.run.id"
    );
    assert!(
        receiver.requests().iter().any(|request| request
            .headers
            .iter()
            .any(|(key, value)| { key == "content-type" && value.contains("json") })),
        "receiver must observe HTTP/JSON telemetry"
    );
    assert_deck_requests_exclude(
        &receiver,
        &[
            &directive,
            "TELEMETRY_REAL_AGENT_OK",
            SENTINEL,
            PROOF,
            &deck.workdir().to_string_lossy(),
        ],
    );
}

/// Scenario: Run trusted interactive Claude Haiku in an attached pane and ask it to copy a unique sentinel through Bash, observing Thinking, Working and Idle. Exit Claude normally so its final usage is written, then require provider-reported Haiku tokens, estimated cost and completed termination on the deck's exported run.
#[spec("telemetry/usage/007")]
#[test]
fn telemetry_usage_007_real_interactive_haiku_usage() {
    skip_unless!(common::check_claude_available());
    let receiver = Receiver::start();
    let deck = TuiDeck::builder()
        .with_pty_size(180, 50)
        .with_imported_claude_credentials()
        .with_claude_trust_workdir()
        .with_env("OTEL_EXPORTER_OTLP_ENDPOINT", &receiver.endpoint)
        .with_env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json")
        .with_continue_session(
            "usage-haiku",
            "claude --model claude-haiku-4-5-20251001 --allowedTools Bash Read Write",
        )
        .launch_with_fixture("chain-smoke-claude");
    const SENTINEL: &str = "usage_1858_sentinel_9f07.txt";
    const PROOF: &str = "usage_1858_proof_9f07.txt";
    std::fs::write(deck.workdir().join(SENTINEL), "USAGE_REAL_AGENT_OK_9f07\n").unwrap();
    deck.wait_for_string("Claude Code v");
    deck.send_bytes(b"\x04");
    deck.wait_for_string("Dir:");
    let agent = common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .find(|agent| agent.display_name.as_deref() == Some("usage-haiku"))
        .expect("interactive Haiku pane");
    let pane = agent.pane_id_env.as_deref().unwrap();
    let start = deck.stream_text().len();
    let directive = format!(
        "Use Bash exactly once to run: sleep 3; cat '{}/{SENTINEL}' > '{}/{PROOF}'. Do not modify any other file. Then reply USAGE_DONE and end your turn.",
        deck.workdir().display(),
        deck.workdir().display(),
    );
    common::post_message_on(deck.attach_socket_path(), pane, &agent.id, &directive)
        .expect("post real usage task");
    assert!(
        common::wait_until(Duration::from_secs(120), || {
            let stream = deck.stream_text();
            common::match_needles_in_order(
                stream.get(start..).unwrap_or_default().as_bytes(),
                &["Thinking", "Working", "Bash", "Idle"],
            ) == 4
        }),
        "Haiku must visibly complete Thinking → Working/Bash → Idle:\n{}",
        deck.snapshot_grid()
    );
    common::wait_for_file_trimmed_eq(
        &deck.workdir().join(PROOF),
        "USAGE_REAL_AGENT_OK_9f07",
        Duration::from_secs(10),
    )
    .expect("Haiku must copy the unique sentinel");
    // Claude writes cost-state on a normal exit; killing its PTY discards it.
    deck.send_bytes(b"\x04");
    deck.send_keys(b"/exit");
    deck.wait_for_string("/exit");
    deck.send_keys(b"\r");
    let run = receiver.run(pane);
    assert_eq!(
        attribute(&run, "deck.tokens.provenance"),
        "provider_reported",
        "Haiku run must carry usage: {run}"
    );
    assert_eq!(
        attribute(&run, "deck.cost.provenance"),
        "estimated",
        "Haiku cost must be a labelled estimate: {run}"
    );
    assert_eq!(
        attribute(&run, "deck.run.termination"),
        "completed",
        "Haiku completed its turn: {run}"
    );
    assert_eq!(
        attribute(&run, "deck.run.termination.provenance"),
        "provider_reported"
    );
    let usage = run["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| {
            event["name"] == "deck.model_usage"
                && attribute(event, "gen_ai.response.model").starts_with("claude-haiku-")
        })
        .expect("Haiku model usage event");
    assert_eq!(
        attribute(usage, "deck.tokens.provenance"),
        "provider_reported"
    );
    assert_eq!(attribute(usage, "deck.cost.provenance"), "estimated");
    for value in [&run, usage] {
        for key in [
            "gen_ai.usage.input_tokens",
            "gen_ai.usage.output_tokens",
            "deck.cost.microusd",
        ] {
            let integer = value["attributes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|attr| attr["key"] == key)
                .and_then(|attr| {
                    attr["value"]["intValue"]
                        .as_u64()
                        .or_else(|| attr["value"]["intValue"].as_str()?.parse().ok())
                });
            assert!(
                integer.is_some_and(|n| n > 0),
                "positive {key} expected: {value}"
            );
        }
    }
}

// OTLP logs and traces both wrap each resource in field 1, whose resource is
// field 1 again. Decode resource string attributes only; do not search raw
// bytes, where a log body could accidentally satisfy the join assertion.
fn protobuf_messages(bytes: &[u8], wanted: u64) -> Vec<&[u8]> {
    fn varint(bytes: &[u8], offset: &mut usize) -> Option<u64> {
        let mut value = 0;
        for shift in (0..64).step_by(7) {
            let byte = *bytes.get(*offset)?;
            *offset += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }

    let mut offset = 0;
    let mut found = Vec::new();
    while offset < bytes.len() {
        let tag = varint(bytes, &mut offset).expect("protobuf field tag");
        match tag & 7 {
            0 => {
                varint(bytes, &mut offset).expect("protobuf varint");
            }
            1 => offset += 8,
            2 => {
                let len = usize::try_from(varint(bytes, &mut offset).expect("protobuf length"))
                    .expect("protobuf length fits usize");
                let end = offset.checked_add(len).expect("protobuf field length");
                let value = bytes.get(offset..end).expect("complete protobuf field");
                if tag >> 3 == wanted {
                    found.push(value);
                }
                offset = end;
            }
            5 => offset += 4,
            wire => panic!("unsupported protobuf wire type {wire}"),
        }
        assert!(offset <= bytes.len(), "complete protobuf scalar");
    }
    found
}

fn request_resources(request: &Request) -> Vec<Value> {
    if let Ok(body) = serde_json::from_slice::<Value>(&request.body) {
        return ["resourceLogs", "resourceSpans", "resourceMetrics"]
            .iter()
            .flat_map(|key| body[key].as_array().into_iter().flatten())
            .map(|entry| entry["resource"].clone())
            .collect();
    }
    assert!(
        request
            .headers
            .iter()
            .any(|(key, value)| key == "content-type" && value.contains("protobuf")),
        "native OpenCode request must use OTLP JSON or protobuf"
    );
    protobuf_messages(&request.body, 1)
        .into_iter()
        .flat_map(|entry| protobuf_messages(entry, 1))
        .map(|resource| {
            let attributes: Vec<Value> = protobuf_messages(resource, 1)
                .into_iter()
                .filter_map(|entry| {
                    let key = *protobuf_messages(entry, 1).first()?;
                    let value = *protobuf_messages(entry, 2).first()?;
                    let strings = protobuf_messages(value, 1);
                    let value = *strings.first()?;
                    Some(serde_json::json!({
                        "key": std::str::from_utf8(key).expect("resource attribute key"),
                        "value": {"stringValue": std::str::from_utf8(value).expect("resource string value")}
                    }))
                })
                .collect();
            serde_json::json!({"attributes": attributes})
        })
        .collect()
}

fn assert_deck_requests_exclude(receiver: &Receiver, private_strings: &[&str]) {
    let requests: Vec<_> = receiver
        .requests()
        .into_iter()
        .filter(|request| {
            request.path == "/v1/traces"
                && request_resources(request)
                    .iter()
                    .any(|resource| attribute(resource, "service.name") == "dot-agent-deck")
        })
        .collect();
    assert!(
        !requests.is_empty(),
        "privacy assertion must inspect at least one deck-owned trace request"
    );
    for request in requests {
        for private in private_strings {
            assert!(!private.is_empty(), "privacy sentinel must be nonempty");
            assert!(
                !request
                    .body
                    .windows(private.len())
                    .any(|window| window == private.as_bytes()),
                "private string {private:?} leaked into a deck-owned OTLP trace request"
            );
        }
    }
}

/// Scenario: Start real interactive OpenCode on the default Haiku, a cheap mini model or the subscription-compatible Luna override and ask it to copy a uniquely named fixture sentinel with Bash, visibly completing its turn. Close the pane and require its deck run and OpenCode's own logs or traces to reach the local receiver with the same resource run id, while deck-owned trace requests exclude the prompt, file contents, filenames and working-directory path.
#[spec("telemetry/otlp/007")]
#[test]
fn telemetry_otlp_007_real_opencode_telemetry_joins_deck_run() {
    skip_unless!(if common::opencode_test_model().contains("mini")
        || common::opencode_test_model() == "openai/gpt-5.6-luna"
        || common::opencode_test_model() == common::OPENCODE_TEST_MODEL_DEFAULT
    {
        Ok(())
    } else {
        Err(
            "telemetry/otlp/007 requires the default Haiku model or a cheap mini or openai/gpt-5.6-luna override in DOT_AGENT_DECK_OPENCODE_TEST_MODEL"
                .to_string(),
        )
    });
    skip_unless!(common::check_opencode_available());
    let receiver = Receiver::start();
    let deck = TuiDeck::builder()
        .with_pty_size(180, 50)
        .with_imported_opencode_credentials()
        .with_env("PATH", common::path_with_binary_dir())
        .with_env("OPENCODE_DISABLE_AUTOUPDATE", "1")
        // Issue #2022: a config dir whose tui.json lists a plugin (the deck's)
        // makes OpenCode npm-install @opencode-ai/plugin into it before its
        // first frame. In this fresh HOME that is a registry download that
        // overran the composer wait. The deck's plugins import no SDK, and
        // OpenCode logs a failed install and carries on, so fail it at once.
        .with_env("npm_config_offline", "true")
        .with_env("OTEL_EXPORTER_OTLP_ENDPOINT", &receiver.endpoint)
        .with_env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json")
        .with_continue_session(
            "telemetry-opencode-mini",
            format!("opencode --model {} --auto", common::opencode_test_model()),
        )
        .launch_with_fixture("minimal");
    const SENTINEL: &str = "telemetry_opencode_sentinel_7c31.txt";
    const PROOF: &str = "telemetry_opencode_proof_7c31.txt";
    std::fs::write(deck.workdir().join(SENTINEL), "TELEMETRY_OPENCODE_OK\n").unwrap();
    let composer_wait = std::time::Instant::now();
    let composer = deck.wait_for_grid_string_within("Ask anything", Duration::from_secs(60));
    let agent = common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .find(|agent| agent.display_name.as_deref() == Some("telemetry-opencode-mini"));
    assert!(
        composer,
        "real interactive OpenCode must render its composer:\n{}\n{}",
        deck.snapshot_grid(),
        opencode_startup_evidence(
            &deck,
            &receiver,
            agent.as_ref().map(|agent| agent.id.as_str())
        )
    );
    eprintln!(
        "OpenCode composer appeared after {:?}",
        composer_wait.elapsed()
    );
    let agent = agent.expect("real interactive OpenCode pane");
    let pane = agent.pane_id_env.as_deref().expect("pane id");
    let start = deck.stream_text().len();
    let directive = format!(
        "Use Bash exactly once to run: sleep 3; cat '{}/{SENTINEL}' > '{}/{PROOF}'. Do not modify any other file. Then reply with TELEMETRY_ followed immediately by OPENCODE_DONE and end your turn.",
        deck.workdir().display(),
        deck.workdir().display(),
    );
    let mut input = common::AttachInput::attach(deck.attach_socket_path(), &agent.id);
    input.write_steps([
        (directive.as_str(), Duration::from_millis(750)),
        ("\r", Duration::ZERO),
    ]);
    assert!(
        common::wait_until(Duration::from_secs(120), || {
            deck.stream_text()
                .get(start..)
                .unwrap_or_default()
                .contains("Working")
        }),
        "real OpenCode must visibly work after the task:\n{}",
        deck.snapshot_grid()
    );
    common::wait_for_file_trimmed_eq(
        &deck.workdir().join(PROOF),
        "TELEMETRY_OPENCODE_OK",
        Duration::from_secs(120),
    )
    .expect("real OpenCode must copy the unique fixture sentinel");
    assert!(
        deck.wait_for_grid_string_within("TELEMETRY_OPENCODE_DONE", Duration::from_secs(60)),
        "real OpenCode must visibly finish its turn:\n{}",
        deck.snapshot_grid()
    );
    deck.send_bytes(b"\x04");
    assert!(
        deck.wait_for_grid_string_within("Idle", Duration::from_secs(30)),
        "the completed real OpenCode card must show Idle:\n{}",
        deck.snapshot_grid()
    );
    assert!(
        common::wait_until(EXPORT_WAIT, || receiver.requests().iter().any(|request| {
            matches!(request.path.as_str(), "/v1/logs" | "/v1/traces")
                && request_resources(request)
                    .iter()
                    .any(|resource| attribute(resource, "service.name") == "opencode")
        })),
        "OpenCode never exported its own logs or traces to the configured receiver"
    );
    let response = common::attach_request_on(
        deck.attach_socket_path(),
        &AttachRequest::StopAgent {
            id: agent.id.clone(),
        },
    )
    .unwrap();
    assert!(response.ok, "close OpenCode pane: {response:?}");
    let run = receiver.run(pane);
    let run_id = attribute(&run, "deck.run.id");
    assert!(!run_id.is_empty(), "deck run must have a join id");
    assert!(
        common::wait_until(EXPORT_WAIT, || receiver.requests().iter().any(|request| {
            matches!(request.path.as_str(), "/v1/logs" | "/v1/traces")
                && request_resources(request).iter().any(|resource| {
                    attribute(resource, "service.name") == "opencode"
                        && attribute(resource, "deck.run.id") == run_id
                })
        })),
        "OpenCode logs/traces must carry the deck's deck.run.id in their resource"
    );
    assert_deck_requests_exclude(
        &receiver,
        &[
            &directive,
            "TELEMETRY_OPENCODE_OK",
            SENTINEL,
            PROOF,
            &deck.workdir().to_string_lossy(),
        ],
    );
}

/// Issue #2022: what OpenCode was doing when its composer never appeared —
/// the bytes it wrote to its PTY, its processes, its own log and the logs it
/// exported. Only read on a failure.
fn opencode_startup_evidence(deck: &TuiDeck, receiver: &Receiver, agent: Option<&str>) -> String {
    use std::fmt::Write as _;
    let mut out = String::from("--- OpenCode startup evidence (issue #2022) ---\n");
    match agent {
        Some(id) => {
            let raw = common::pane_snapshot_on(deck.attach_socket_path(), id);
            let _ = writeln!(
                out,
                "pane PTY bytes from the daemon: {}; tail: {:?}",
                raw.len(),
                String::from_utf8_lossy(&raw[raw.len().saturating_sub(2048)..])
            );
        }
        None => out.push_str("no agent record named telemetry-opencode-mini\n"),
    }
    let workdir = deck.workdir().canonicalize().unwrap_or_default();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let dir = entry.path();
        if std::fs::read_link(dir.join("cwd")).ok().as_deref() != Some(workdir.as_path()) {
            continue;
        }
        let cmdline = std::fs::read(dir.join("cmdline"))
            .map(|bytes| String::from_utf8_lossy(&bytes).replace('\0', " "))
            .unwrap_or_default();
        let stat = std::fs::read_to_string(dir.join("stat")).unwrap_or_default();
        let wchan = std::fs::read_to_string(dir.join("wchan")).unwrap_or_default();
        let _ = writeln!(out, "process {stat:.120} wchan={wchan} cmd={cmdline:.200}");
        let pid = entry.file_name().to_string_lossy().to_string();
        if let Ok(ss) = std::process::Command::new("ss").args(["-tnpH"]).output() {
            for line in String::from_utf8_lossy(&ss.stdout).lines() {
                if line.contains(&format!("pid={pid},")) {
                    let _ = writeln!(out, "  socket {line}");
                }
            }
        }
    }
    let home = deck.home_dir();
    for dir in [
        home.join(".local/share/opencode/log"),
        home.join(".local/state/opencode/locks"),
        home.join(".cache/opencode"),
    ] {
        let _ = writeln!(out, "{}:", dir.display());
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let _ = writeln!(out, "  {}", path.display());
            if dir.ends_with("log") {
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                let lines: Vec<_> = text.lines().collect();
                for line in &lines[lines.len().saturating_sub(80)..] {
                    let _ = writeln!(out, "    {line:.400}");
                }
            }
        }
    }
    for request in receiver.requests() {
        if request_resources(&request)
            .iter()
            .any(|resource| attribute(resource, "service.name") == "opencode")
        {
            let body = String::from_utf8_lossy(&request.body);
            let _ = writeln!(out, "OpenCode {}: {body:.4000}", request.path);
        }
    }
    common::redact_credentials_for_output(&out)
}

fn finish_native_join(
    receiver: &Receiver,
    deck: &TuiDeck,
    native_name: &str,
    private_strings: &[&str],
    forbid_logs: bool,
) {
    let agent = common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .next()
        .unwrap();
    let pane = agent.pane_id_env.as_deref().unwrap();
    assert!(
        common::wait_until(EXPORT_WAIT, || receiver.requests().iter().any(|request| {
            request_resources(request).iter().any(|resource| {
                attribute(resource, "service.name") != "dot-agent-deck"
                    && !attribute(resource, "deck.run.id").is_empty()
            })
        })),
        "{native_name} never exported its own telemetry before pane close; request paths: {:?}",
        receiver
            .requests()
            .iter()
            .map(|request| &request.path)
            .collect::<Vec<_>>()
    );
    let response = common::attach_request_on(
        deck.attach_socket_path(),
        &AttachRequest::StopAgent {
            id: agent.id.clone(),
        },
    )
    .unwrap();
    assert!(response.ok, "close {native_name} pane: {response:?}");
    let run = receiver.run(pane);
    let run_id = attribute(&run, "deck.run.id");
    assert!(!run_id.is_empty());
    assert!(
        common::wait_until(EXPORT_WAIT, || receiver.requests().iter().any(|request| {
            (!forbid_logs || matches!(request.path.as_str(), "/v1/traces" | "/v1/metrics"))
                && request_resources(request).iter().any(|resource| {
                    attribute(resource, "service.name") != "dot-agent-deck"
                        && attribute(resource, "deck.run.id") == run_id
                })
        })),
        "{native_name} never exported its own telemetry with resource deck.run.id={run_id}; paths/resources: {:?}",
        receiver
            .requests()
            .iter()
            .map(|request| (&request.path, request_resources(request)))
            .collect::<Vec<_>>()
    );
    let requests = receiver.requests();
    if forbid_logs {
        // Issue #1981 M8: the deck's own warnings arrive as logs too; only
        // the agent's must stay off.
        assert!(
            !requests.iter().any(|request| request.path == "/v1/logs"
                && request_resources(request)
                    .iter()
                    .any(|resource| attribute(resource, "service.name") != "dot-agent-deck")),
            "Codex logs carry tool content and must stay off"
        );
    }
    for request in &requests {
        for private in private_strings {
            assert!(!private.is_empty());
            assert!(
                !request
                    .body
                    .windows(private.len())
                    .any(|bytes| bytes == private.as_bytes()),
                "private sentinel {private:?} in {native_name} or deck OTLP payload"
            );
        }
    }
}

/// Scenario: Start real interactive Codex on cheap Luna through a shell script that exports a variable and calls codex by name, then ask it to copy a unique fixture through its shell and observe the proof and completed response. Closing the pane must deliver Codex's own traces or metrics with the deck's resource run id, no Codex logs and no prompt/file/output sentinel in any captured request.
#[spec("telemetry/otlp/018")]
#[test]
fn telemetry_otlp_018_real_codex_safe_telemetry_joins_deck_run() {
    skip_unless!(common::check_codex_available());
    let receiver = Receiver::start();
    let scratch = common::harness_tempdir().unwrap();
    let wrapper = scratch.path().join("run-codex.sh");
    std::fs::write(&wrapper, "#!/bin/sh\nexport FOO=1\ncodex \"$@\"\n").unwrap();
    let wrapper = wrapper.to_str().unwrap().replace('\'', "'\\''");
    let deck = TuiDeck::builder()
        .with_pty_size(180, 50)
        .with_imported_codex_credentials()
        .with_env("PATH", common::path_with_binary_dir())
        .with_env("OTEL_EXPORTER_OTLP_ENDPOINT", &receiver.endpoint)
        .with_env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json")
        .with_env("OTEL_METRIC_EXPORT_INTERVAL", "1000")
        .with_continue_session("telemetry-codex-luna", format!("/bin/sh '{wrapper}' --model {} --dangerously-bypass-approvals-and-sandbox -c 'model_reasoning_effort=\"low\"' -c 'analytics.enabled=false'", common::codex_test_model()))
        .launch_with_fixture("minimal");
    const SENTINEL: &str = "telemetry_codex_private_fixture_1741.txt";
    const PROOF: &str = "telemetry_codex_proof_1741.txt";
    const CONTENT: &str = "PRIVATE_CODEX_FILE_1741_e6da";
    const PROMPT: &str = "PRIVATE_CODEX_PROMPT_1741_ae51";
    const OUTPUT: &str = "PRIVATE_CODEX_OUTPUT_1741_d9a7";
    std::fs::write(deck.workdir().join(SENTINEL), CONTENT).unwrap();
    assert!(
        deck.wait_for_grid_string_within("Ask Codex to do anything", Duration::from_secs(60)),
        "Codex composer did not become ready:\n{}",
        deck.snapshot_grid()
    );
    let directive = format!(
        "{PROMPT}: Use the shell exactly once to copy {SENTINEL} to {PROOF} in the current directory. Do not change any other file. Then reply exactly {OUTPUT} and end the turn."
    );
    deck.send_keys(directive.as_bytes());
    deck.wait_for_string(PROMPT);
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        deck.send_keys(b"\r");
        if deck.wait_for_grid_string_within("Ask Codex to do anything", Duration::from_secs(2)) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Codex did not submit prompt:\n{}",
            deck.snapshot_grid()
        );
    }
    common::wait_for_file_trimmed_eq(
        &deck.workdir().join(PROOF),
        CONTENT,
        Duration::from_secs(100),
    )
    .unwrap_or_else(|state| {
        panic!(
            "real Codex did not do the sentinel work: {state}\n{}",
            deck.snapshot_grid()
        )
    });
    assert!(
        deck.wait_for_grid_string_within(OUTPUT, Duration::from_secs(30)),
        "Codex did not visibly finish:\n{}",
        deck.snapshot_grid()
    );
    deck.send_bytes(b"\x04");
    assert!(
        deck.wait_for_grid_string_within("Idle", Duration::from_secs(20)),
        "Codex card did not settle:\n{}",
        deck.snapshot_grid()
    );
    finish_native_join(
        &receiver,
        &deck,
        "Codex",
        &[PROMPT, CONTENT, SENTINEL, OUTPUT],
        true,
    );
}

/// Scenario: Start real interactive Pi Haiku through the deck's bundled extension and post an inbox directive to copy a uniquely named fixture and reply with a private output marker. Observe the real work and settled card, then require Pi's own metadata export to share the deck's resource run id while prompt, file and output markers stay out of every captured request.
#[spec("telemetry/otlp/019")]
#[test]
fn telemetry_otlp_019_real_pi_metadata_joins_deck_run() {
    skip_unless!(common::anthropic_api_key().map(|_| ()).ok_or_else(|| {
        "Pi Haiku requires an Anthropic API key; no provider fallback".to_string()
    }));
    let receiver = Receiver::start();
    let deck = TuiDeck::builder()
        .with_pty_size(180, 50)
        .with_env("PATH", common::path_with_binary_dir())
        .with_env("ANTHROPIC_API_KEY", common::anthropic_api_key().unwrap())
        .with_env("DOT_AGENT_DECK_EXPERIMENTAL", "1")
        .with_env("PI_TELEMETRY", "0")
        .with_env("OTEL_EXPORTER_OTLP_ENDPOINT", &receiver.endpoint)
        .with_env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json")
        .with_continue_session(
            "telemetry-pi-haiku",
            "pi --provider anthropic --model claude-haiku-4-5 --approve",
        )
        .launch_with_fixture("minimal");
    const SENTINEL: &str = "telemetry_pi_private_fixture_1741.txt";
    const PROOF: &str = "telemetry_pi_proof_1741.txt";
    const CONTENT: &str = "PRIVATE_PI_FILE_1741_79a1";
    const PROMPT: &str = "PRIVATE_PI_PROMPT_1741_f7ab";
    const OUTPUT: &str = "PRIVATE_PI_OUTPUT_1741_a9d3";
    std::fs::write(deck.workdir().join(SENTINEL), CONTENT).unwrap();
    assert!(
        deck.wait_for_grid_string_within("claude-haiku", Duration::from_secs(60)),
        "Pi Haiku did not become ready:\n{}",
        deck.snapshot_grid()
    );
    let directive = format!(
        "{PROMPT}: Use bash exactly once to copy {SENTINEL} to {PROOF} in the current directory. Do not change any other file. Then reply exactly {OUTPUT} and end the turn."
    );
    let agent = common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .next()
        .unwrap();
    common::post_message_on(
        deck.attach_socket_path(),
        agent.pane_id_env.as_deref().unwrap(),
        &agent.id,
        &directive,
    )
    .expect("post sentinel directive to Pi's native inbox");
    common::wait_for_file_trimmed_eq(
        &deck.workdir().join(PROOF),
        CONTENT,
        Duration::from_secs(100),
    )
    .unwrap_or_else(|state| {
        panic!(
            "real Pi Haiku did not do the sentinel work: {state}\n{}",
            deck.snapshot_grid()
        )
    });
    assert!(
        deck.wait_for_grid_string_within(OUTPUT, Duration::from_secs(30)),
        "Pi did not visibly finish:\n{}",
        deck.snapshot_grid()
    );
    deck.send_bytes(b"\x04");
    assert!(
        deck.wait_for_grid_string_within("Idle", Duration::from_secs(20)),
        "Pi card did not settle:\n{}",
        deck.snapshot_grid()
    );
    finish_native_join(
        &receiver,
        &deck,
        "Pi",
        &[PROMPT, CONTENT, SENTINEL, OUTPUT],
        false,
    );
}

/// Scenario: When Devin is authenticated, start its real interactive pane, ask it to write a unique proof file and visibly complete, then close it. Its own metadata export must carry the deck's resource run id and omit the directive and output markers; missing authentication is reported as unverified by the required-real gate.
#[spec("telemetry/otlp/020")]
#[test]
fn telemetry_otlp_020_real_devin_telemetry_join_when_authenticated() {
    skip_unless!(common::check_devin_available());
    let receiver = Receiver::start();
    let deck = TuiDeck::builder()
        .with_pty_size(180, 50)
        .with_imported_devin_credentials()
        .with_env("OTEL_EXPORTER_OTLP_ENDPOINT", &receiver.endpoint)
        .with_env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json")
        .with_continue_session(
            "telemetry-devin-live",
            "devin --respect-workspace-trust false --permission-mode auto",
        )
        .launch_with_fixture("minimal");
    assert!(
        deck.wait_for_grid_string_within("Ask Devin", Duration::from_secs(60)),
        "Devin did not become ready:\n{}",
        deck.snapshot_grid()
    );
    const PROMPT: &str = "PRIVATE_DEVIN_PROMPT_1741_e8ac";
    const OUTPUT: &str = "PRIVATE_DEVIN_OUTPUT_1741_a18b";
    let directive = format!(
        "{PROMPT}: Use exec once to write exactly TELEMETRY_DEVIN_OK to telemetry_devin_proof_1741.txt. Then reply exactly {OUTPUT} and end the turn."
    );
    deck.send_keys(directive.as_bytes());
    deck.wait_for_string(PROMPT);
    deck.send_keys(b"\r");
    common::wait_for_file_trimmed_eq(
        &deck.workdir().join("telemetry_devin_proof_1741.txt"),
        "TELEMETRY_DEVIN_OK",
        Duration::from_secs(100),
    )
    .expect("real Devin sentinel work");
    assert!(
        deck.wait_for_grid_string_within(OUTPUT, Duration::from_secs(30)),
        "Devin did not visibly finish:\n{}",
        deck.snapshot_grid()
    );
    finish_native_join(&receiver, &deck, "Devin", &[PROMPT, OUTPUT], false);
}
