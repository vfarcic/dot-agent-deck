#![cfg(all(feature = "e2e", unix))]

//! Usage accounting observed at a local OTLP receiver, using offline agents.

mod common;

use common::TuiDeck;
use dot_agent_deck::daemon_client::{DaemonClient, StartAgentOptions};
use dot_agent_deck::daemon_protocol::AttachRequest;
use dot_agent_deck::event::AgentType;
use serde_json::{Value, json};
use spec::spec;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(25);

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

    fn run(&self, id: &str) -> Value {
        let find = || {
            self.spans()
                .into_iter()
                .find(|span| span["name"] == "agent.run" && string(span, "deck.run.id") == id)
        };
        assert!(
            common::wait_until(WAIT, || find().is_some()),
            "completed agent.run {id} never reached OTLP receiver: {:?}",
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

fn expect_string(value: &Value, key: &str, expected: &str) {
    assert_eq!(
        string(value, key),
        expected,
        "exported attribute {key}: {value}"
    );
}

fn expect_int(value: &Value, key: &str, expected: u64) {
    let actual = attr(value, key).and_then(|v| {
        v["intValue"]
            .as_u64()
            .or_else(|| v["intValue"].as_str()?.parse().ok())
    });
    assert_eq!(
        actual,
        Some(expected),
        "exported integer attribute {key}: {value}"
    );
}

fn unavailable(value: &Value, field: &str, reason: &str, numeric: &[&str]) {
    expect_string(value, &format!("deck.{field}.provenance"), "unavailable");
    expect_string(value, &format!("deck.{field}.unavailable_reason"), reason);
    for key in numeric {
        assert!(
            attr(value, key).is_none(),
            "unavailable {key} must be absent, never zero: {value}"
        );
    }
}

const TOKEN_KEYS: &[&str] = &[
    "gen_ai.usage.input_tokens",
    "gen_ai.usage.output_tokens",
    "gen_ai.usage.cache_read.input_tokens",
    "gen_ai.usage.cache_write.input_tokens",
];

fn tokens(value: &Value, counts: [u64; 4]) {
    expect_string(value, "deck.tokens.provenance", "provider_reported");
    for (key, count) in TOKEN_KEYS.iter().zip(counts) {
        expect_int(value, key, count);
    }
}

fn model(run: &Value, name: &str) -> Value {
    run["events"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|event| {
            event["name"] == "deck.model_usage" && string(event, "gen_ai.response.model") == name
        })
        .cloned()
        .unwrap_or_else(|| panic!("missing deck.model_usage for {name}: {run}"))
}

fn ended(run: &Value, termination: &str, provenance: &str) {
    expect_string(run, "deck.run.termination", termination);
    expect_string(run, "deck.run.termination.provenance", provenance);
}

fn launch(receiver: &Receiver) -> TuiDeck {
    let deck = TuiDeck::builder()
        .without_agent_credentials()
        .impersonating_pane_signals()
        .with_pty_size(180, 45)
        .with_env("PATH", common::path_with_binary_dir())
        .with_env("OTEL_EXPORTER_OTLP_ENDPOINT", &receiver.endpoint)
        .with_env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json")
        .with_continue_session("usage-bootstrap", "cat")
        .launch_with_fixture("minimal");
    deck.wait_for_string("[Command Mode Ctrl+D]");
    let boot = common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .next()
        .unwrap();
    let reply = common::attach_request_on(
        deck.attach_socket_path(),
        &AttachRequest::StopAgent { id: boot.id },
    )
    .unwrap();
    assert!(reply.ok);
    deck
}

// The stand-in writes only the documented local formats. On the old binary
// DOT_AGENT_DECK_USAGE_FILE is absent; still exit normally so RED describes
// the missing exported attributes rather than a stand-in setup exception.
const STANDIN: &str = r#"import json, os, pathlib, sys, time
name, kind = sys.argv[1:]
base = pathlib.Path.cwd()
ready = base / (name + '.ready')
ready.write_text(json.dumps(dict(os.environ)))
while not (base / (name + '.go')).exists(): time.sleep(.02)
data = json.loads((base / (name + '.fixture')).read_text())
child = base / (name + '.child')
if child.exists():
    import socket, struct
    config = json.loads(child.read_text())
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(20)
        connection.connect(config['socket'])
        def exact(n):
            result = b''
            while len(result) < n:
                part = connection.recv(n - len(result))
                assert part, 'attach connection ended early'
                result += part
            return result
        def request(value):
            payload = json.dumps(value).encode()
            connection.sendall(struct.pack('>BI', config['request_kind'], len(payload)) + payload)
            kind, size = struct.unpack('>BI', exact(5))
            assert kind == config['response_kind'], kind
            reply = json.loads(exact(size))
            assert reply['ok'], reply
            return reply
        request(config['hello'])
        value = config['start']
        # Model the environment an agent actually inherits when starting work.
        for key in ('DOT_AGENT_DECK_USAGE_FILE', 'OTEL_RESOURCE_ATTRIBUTES'):
            if key in os.environ: value['env'].append([key, os.environ[key]])
        reply = request(value)
        (base / (name + '.child-id')).write_text(reply['id'])
if kind in ('pi', 'opencode'):
    target = os.environ.get('DOT_AGENT_DECK_USAGE_FILE')
else:
    target = data['path']
if target:
    target = pathlib.Path(target)
    target.parent.mkdir(parents=True, exist_ok=True)
    with target.open('a') as out:
        for record in data['records']: out.write(json.dumps(record, separators=(',', ':')) + '\n')
print('USAGE_STANDIN_FINISHED_' + name, flush=True)
"#;

fn start(
    deck: &TuiDeck,
    name: &str,
    kind: AgentType,
    cwd: &std::path::Path,
    command: String,
) -> (String, String) {
    let pane = format!("usage-{name}");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let id = runtime
        .block_on(
            DaemonClient::new(deck.attach_socket_path().to_path_buf()).start_agent(
                StartAgentOptions {
                    command: Some(command),
                    cwd: Some(cwd.to_string_lossy().into_owned()),
                    display_name: Some(name.into()),
                    agent_type: Some(kind),
                    env: vec![("DOT_AGENT_DECK_PANE_ID".into(), pane.clone())],
                    ..Default::default()
                },
            ),
        )
        .expect("start offline usage stand-in");
    // ListAgents omits exited processes, including commands that finish before
    // start_agent returns. Both identities are already known without that lookup.
    (id, pane)
}

fn fixture_run(
    deck: &TuiDeck,
    receiver: &Receiver,
    name: &str,
    kind: AgentType,
    path: Option<&std::path::Path>,
    records: Vec<Value>,
    cwd: &std::path::Path,
) -> Value {
    std::fs::write(cwd.join("usage-standin.py"), STANDIN).unwrap();
    std::fs::write(
        cwd.join(format!("{name}.fixture")),
        json!({"path":path,"records":records}).to_string(),
    )
    .unwrap();
    let family = match kind {
        AgentType::ClaudeCode => "claude",
        AgentType::Codex => "codex",
        AgentType::Pi => "pi",
        AgentType::OpenCode => "opencode",
        _ => unreachable!(),
    };
    let (agent_id, pane) = start(
        deck,
        name,
        kind.clone(),
        cwd,
        format!("python3 usage-standin.py {name} {family}"),
    );
    let ready = cwd.join(format!("{name}.ready"));
    assert!(
        common::wait_until(WAIT, || std::fs::read_to_string(&ready)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .is_some()),
        "stand-in never became ready"
    );
    let environment: Value =
        serde_json::from_str(&std::fs::read_to_string(ready).unwrap()).unwrap();
    let id = environment["OTEL_RESOURCE_ATTRIBUTES"]
        .as_str()
        .unwrap()
        .split(',')
        .find_map(|s| s.strip_prefix("deck.run.id="))
        .unwrap();
    if matches!(kind, AgentType::ClaudeCode | AgentType::Codex) {
        let payload = json!({"session_id": name, "pane_id":pane, "agent_id":agent_id,
            "agent_type": if kind == AgentType::Codex {"codex"} else {"claude_code"}, "event_type":"thinking",
            "timestamp":chrono::Utc::now().to_rfc3339(), "cwd":cwd,
            "metadata":{"codex_transcript_path":path}});
        let events = deck.subscribe_events();
        common::write_hook_line(deck.hook_socket_path(), &payload.to_string()).unwrap();
        events.wait_for(|event| event.agent_id.as_deref() == Some(&agent_id), WAIT);
    }
    std::fs::write(cwd.join(format!("{name}.go")), "go").unwrap();
    receiver.run(id)
}

fn codex_records(ending: Value) -> Vec<Value> {
    vec![
        json!({"type":"turn_context","payload":{"model":"gpt-5.4-mini"}}),
        json!({"type":"event_msg","payload":{"type":"task_started"}}),
        json!({"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":600,"output_tokens":50}},"rate_limits":{"plan_type":"pro","primary":{"used_percent":12.0,"window_minutes":10080,"resets_at":1790500000}}}}),
        json!({"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1500,"cached_input_tokens":900,"output_tokens":80}},"rate_limits":{"plan_type":"pro","primary":{"used_percent":13.5,"window_minutes":10080,"resets_at":1790500000}}}}),
        json!({"type":"event_msg","payload":ending}),
    ]
}

/// Scenario: Offline Codex panes write rollout totals and allowance samples, then finish, fail or interrupt their turn. Their exported runs must retain per-model tokens, allowance endpoints and the provider's termination kind.
#[spec("telemetry/usage/001")]
#[test]
fn telemetry_usage_001_codex_tokens_quota_and_termination() {
    let receiver = Receiver::start();
    let deck = launch(&receiver);
    let mut runs = Vec::new();
    for (name, ending, termination) in [
        (
            "codex-completed",
            json!({"type":"task_complete","last_agent_message":"done"}),
            "completed",
        ),
        (
            "codex-failed",
            json!({"type":"task_complete","error":{"message":"offline failure","codex_error_info":"server_overloaded"}}),
            "failed",
        ),
        (
            "codex-interrupted",
            json!({"type":"turn_aborted"}),
            "interrupted",
        ),
    ] {
        let path = deck
            .home_dir()
            .join(format!(".codex/sessions/2026/10/26/rollout-{name}.jsonl"));
        runs.push((
            fixture_run(
                &deck,
                &receiver,
                name,
                AgentType::Codex,
                Some(&path),
                codex_records(ending),
                deck.workdir(),
            ),
            termination,
        ));
    }
    for (run, termination) in runs {
        tokens(&run, [1500, 80, 900, 0]);
        tokens(&model(&run, "gpt-5.4-mini"), [1500, 80, 900, 0]);
        unavailable(&run, "cost", "no_channel", &["deck.cost.microusd"]);
        expect_string(&run, "deck.quota.provenance", "provider_reported");
        expect_string(&run, "deck.quota.plan", "pro");
        for (key, count) in [
            ("deck.quota.window_minutes", 10080),
            ("deck.quota.resets_at", 1790500000),
            ("deck.quota.used_from_bp", 1200),
            ("deck.quota.used_to_bp", 1350),
        ] {
            expect_int(&run, key, count);
        }
        ended(&run, termination, "provider_reported");
        expect_string(&run, "deck.run.last_turn", termination);
        if termination == "failed" {
            expect_string(&run, "error.type", "server_overloaded");
        }
    }
}

/// Scenario: An offline OpenCode pane reports tokens with a subscription cost of zero. Its exported model and run totals must retain the tokens while reporting cost as unavailable with subscription_zero rather than a numeric zero.
#[spec("telemetry/usage/002")]
#[test]
fn telemetry_usage_002_opencode_subscription_zero_is_unavailable() {
    let receiver = Receiver::start();
    let deck = launch(&receiver);
    let run = fixture_run(
        &deck,
        &receiver,
        "opencode-subscription",
        AgentType::OpenCode,
        None,
        vec![
            json!({"v":1,"type":"reply","agent":"opencode","id":"msg_1","model":"gpt-5.4-mini","input":100,"output":20,"cache_read":0,"cache_write":0,"cost_usd":0,"stop":"stop"}),
        ],
        deck.workdir(),
    );
    for value in [&run, &model(&run, "gpt-5.4-mini")] {
        tokens(value, [100, 20, 0, 0]);
        unavailable(value, "cost", "subscription_zero", &["deck.cost.microusd"]);
    }
    ended(&run, "completed", "provider_reported");
}

/// Scenario: An offline Pi pane appends one reply with input, cache and output counts and a list-price cost. Its exported model and run totals must include cache reads in input and label cost as an estimate.
#[spec("telemetry/usage/003")]
#[test]
fn telemetry_usage_003_pi_tokens_and_estimated_cost() {
    let receiver = Receiver::start();
    let deck = launch(&receiver);
    let run = fixture_run(
        &deck,
        &receiver,
        "pi-estimate",
        AgentType::Pi,
        None,
        vec![
            json!({"v":1,"type":"reply","agent":"pi","model":"gpt-5.4-mini","input":100,"output":20,"cache_read":50,"cache_write":0,"cost_usd":0.0042,"stop":"stop"}),
        ],
        deck.workdir(),
    );
    for value in [&run, &model(&run, "gpt-5.4-mini")] {
        tokens(value, [150, 20, 50, 0]);
        expect_int(value, "deck.cost.microusd", 4200);
        expect_string(value, "deck.cost.provenance", "estimated");
    }
    unavailable(
        &run,
        "quota",
        "no_channel",
        &["deck.quota.used_from_bp", "deck.quota.used_to_bp"],
    );
    ended(&run, "completed", "provider_reported");
}

/// Scenario: Arbitrary shell commands exit immediately with success and failure, without waiting for a live-agent lookup. Their exported runs must report measured termination and no-channel usage, with numeric usage fields absent and nonzero_exit only on failure.
#[spec("telemetry/usage/004")]
#[test]
fn telemetry_usage_004_commands_have_no_channel_and_measured_exit() {
    let receiver = Receiver::start();
    let deck = launch(&receiver);
    let mut runs = Vec::new();
    for code in [0, 3] {
        let (_, pane) = start(
            &deck,
            &format!("command-{code}"),
            AgentType::None,
            deck.workdir(),
            format!("sh -c 'exit {code}'"),
        );
        let find = || {
            receiver
                .spans()
                .into_iter()
                .find(|s| s["name"] == "agent.run" && string(s, "deck.pane.id") == pane)
        };
        assert!(
            common::wait_until(WAIT, || find().is_some()),
            "command run never exported"
        );
        runs.push((find().unwrap(), code));
    }
    for (run, code) in runs {
        unavailable(&run, "tokens", "no_channel", TOKEN_KEYS);
        unavailable(&run, "cost", "no_channel", &["deck.cost.microusd"]);
        unavailable(
            &run,
            "quota",
            "no_channel",
            &["deck.quota.used_from_bp", "deck.quota.used_to_bp"],
        );
        let events: Vec<_> = run["events"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|e| e["name"] == "deck.model_usage")
            .collect();
        assert_eq!(events.len(), 1, "one unavailable model event: {run}");
        assert!(attr(events[0], "gen_ai.response.model").is_none());
        unavailable(events[0], "tokens", "no_channel", TOKEN_KEYS);
        unavailable(events[0], "cost", "no_channel", &["deck.cost.microusd"]);
        ended(
            &run,
            if code == 0 { "completed" } else { "failed" },
            "measured",
        );
        if code == 3 {
            expect_string(&run, "error.type", "nonzero_exit");
        } else {
            assert!(attr(&run, "error.type").is_none());
        }
    }
}

fn claude_records(name: &str, model: &str, error: Option<(&str, &str)>) -> Vec<Value> {
    let mut assistant = json!({"type":"assistant","isSidechain":false,"message":{"model":model,"stop_reason":"end_turn","content":[{"type":"text","text":"offline reply"}]}});
    if let Some((kind, message)) = error {
        assistant["isApiErrorMessage"] = json!(true);
        assistant["error"] = json!(kind);
        assistant["apiErrorStatus"] = json!(429);
        assistant["message"]["stop_reason"] = json!("stop_sequence");
        assistant["message"]["content"][0]["text"] = json!(message);
    }
    vec![
        json!({"type":"user","isSidechain":false,"message":{"role":"user","content":"offline prompt"}}),
        assistant,
        json!({"type":"cost-state","sessionId":name,"totalCostUSD":0.0123,"startTime":1790000000000_u64,"modelUsage":{model:{"inputTokens":10,"outputTokens":20,"cacheReadInputTokens":300,"cacheCreationInputTokens":40,"costUSD":0.0123}},"hasUnknownModelCost":false}),
    ]
}

fn claude_path(deck: &TuiDeck, name: &str, cwd: &std::path::Path) -> std::path::PathBuf {
    let project: String = cwd
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    deck.home_dir()
        .join(".claude/projects")
        .join(project)
        .join(format!("{name}.jsonl"))
}

/// Scenario: Offline Claude panes append their completed transcript and cost-state, including a rate-limited final turn. Their exported model and run totals must match the transcript, label cost estimated and preserve the failure kind and HTTP status.
#[spec("telemetry/usage/005")]
#[test]
fn telemetry_usage_005_claude_cost_state_and_api_failure() {
    let receiver = Receiver::start();
    let deck = launch(&receiver);
    let mut runs = Vec::new();
    for (name, error) in [
        ("claude-completed", None),
        (
            "claude-rate-limit",
            Some(("rate_limit", "offline provider error")),
        ),
    ] {
        let path = claude_path(&deck, name, deck.workdir());
        runs.push((
            fixture_run(
                &deck,
                &receiver,
                name,
                AgentType::ClaudeCode,
                Some(&path),
                claude_records(name, "claude-haiku-5-5", error),
                deck.workdir(),
            ),
            error.is_some(),
        ));
    }
    for (run, failed) in runs {
        for value in [&run, &model(&run, "claude-haiku-5-5")] {
            tokens(value, [350, 20, 300, 40]);
            expect_int(value, "deck.cost.microusd", 12300);
            expect_string(value, "deck.cost.provenance", "estimated");
        }
        let termination = if failed { "failed" } else { "completed" };
        ended(&run, termination, "provider_reported");
        expect_string(&run, "deck.run.last_turn", termination);
        if failed {
            expect_string(&run, "error.type", "rate_limit");
            expect_int(&run, "http.response.status_code", 429);
        }
    }
}

/// Scenario: An offline Claude transcript contains distinct private markers in prompt, reply, tool result, error text, cwd, model and error kind. The completed run must export usage and the sanitized failure while request bodies and telemetry-module logs exclude those markers.
#[spec("telemetry/usage/006")]
#[test]
fn telemetry_usage_006_content_is_excluded_from_usage_exports() {
    let receiver = Receiver::start();
    let deck = launch(&receiver);
    let secrets = [
        "PRIVATE_USAGE_PROMPT_81ab",
        "PRIVATE_USAGE_REPLY_22cd",
        "PRIVATE_USAGE_TOOL_33ef",
        "PRIVATE_USAGE_ERROR_44ab",
        "PRIVATE_USAGE_CWD_55cd",
        "PRIVATE_USAGE_MODEL_66ef",
        "PRIVATE_USAGE_KIND_77ab",
    ];
    let cwd = deck.workdir().join(secrets[4]);
    std::fs::create_dir(&cwd).unwrap();
    let name = "claude-private";
    let path = claude_path(&deck, name, &cwd);
    let mut records = claude_records(name, secrets[5], Some((secrets[6], secrets[3])));
    records[0]["message"]["content"] = json!(secrets[0]);
    records.insert(1,json!({"type":"assistant","message":{"model":secrets[5],"content":[{"type":"text","text":secrets[1]}],"stop_reason":"tool_use"}}));
    records.insert(2,json!({"type":"user","toolUseResult":secrets[2],"message":{"role":"user","content":[{"type":"tool_result","content":secrets[2]}]}}));
    let run = fixture_run(
        &deck,
        &receiver,
        name,
        AgentType::ClaudeCode,
        Some(&path),
        records,
        &cwd,
    );
    for body in receiver.bodies.lock().unwrap().iter() {
        let body = String::from_utf8_lossy(body);
        for secret in secrets {
            assert!(
                !body.contains(secret),
                "private marker {secret} leaked into OTLP request body"
            );
        }
    }
    let log = std::fs::read_to_string(deck.workdir().join("deck.log")).unwrap();
    let telemetry: Vec<_> = log
        .lines()
        .filter(|line| {
            line.contains(" dot_agent_deck::telemetry:")
                || line.contains(" dot_agent_deck::telemetry::")
        })
        .collect();
    assert!(
        telemetry
            .iter()
            .any(|line| line.contains("exporting agent-run traces")),
        "telemetry log capture must be active"
    );
    for secret in secrets {
        assert!(
            telemetry.iter().all(|line| !line.contains(secret)),
            "private marker {secret} leaked into telemetry log"
        );
    }
    // Positive control: dropping the entire accounting record is not privacy.
    tokens(&run, [350, 20, 300, 40]);
    ended(&run, "failed", "provider_reported");
    expect_string(&run, "error.type", "other");
    let events: Vec<_> = run["events"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["name"] == "deck.model_usage")
        .collect();
    assert_eq!(events.len(), 1);
    assert!(attr(events[0], "gen_ai.response.model").is_none());
}

/// Scenario: A synthetic Pi parent starts a second Pi pane from inside its own process and passes along its inherited telemetry environment. Parent and child append different usage, and the receiver must associate each model total with its own run id rather than combining them.
#[spec("telemetry/usage/008")]
#[test]
fn telemetry_usage_008_child_usage_has_its_own_run_identity() {
    use dot_agent_deck::daemon_protocol::{KIND_REQ, KIND_RESP, PROTOCOL_VERSION};
    let receiver = Receiver::start();
    let deck = launch(&receiver);
    let child_name = "pi-child";
    std::fs::write(
        deck.workdir().join(format!("{child_name}.fixture")),
        json!({"records":[{
            "v":1,"type":"reply","agent":"pi","model":"gpt-5.4-mini","input":100,"output":20,
            "cache_read":0,"cache_write":0,"cost_usd":0.0042,"stop":"stop"
        }]})
        .to_string(),
    )
    .unwrap();
    std::fs::write(deck.workdir().join("pi-parent.child"),json!({
        "socket":deck.attach_socket_path(),"request_kind":KIND_REQ,"response_kind":KIND_RESP,
        "hello":{"op":"hello","client_version":PROTOCOL_VERSION,"follow_on":true},
        "start":{"op":"start-agent","command":format!("python3 usage-standin.py {child_name} pi"),
            "cwd":deck.workdir(),"display_name":child_name,"agent_type":"pi",
            "env":[["DOT_AGENT_DECK_PANE_ID","usage-child"]]}
    }).to_string()).unwrap();
    let parent = fixture_run(
        &deck,
        &receiver,
        "pi-parent",
        AgentType::Pi,
        None,
        vec![json!({
            "v":1,"type":"reply","agent":"pi","model":"gpt-5.4-mini","input":10,"output":2,
            "cache_read":0,"cache_write":0,"cost_usd":0.00042,"stop":"stop"
        })],
        deck.workdir(),
    );
    let ready = deck.workdir().join(format!("{child_name}.ready"));
    assert!(
        common::wait_until(WAIT, || std::fs::read_to_string(&ready)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .is_some()),
        "child stand-in never became ready"
    );
    let environment: Value =
        serde_json::from_str(&std::fs::read_to_string(ready).unwrap()).unwrap();
    let child_id = environment["OTEL_RESOURCE_ATTRIBUTES"]
        .as_str()
        .unwrap()
        .split(',')
        .find_map(|s| s.strip_prefix("deck.run.id="))
        .unwrap();
    std::fs::write(deck.workdir().join(format!("{child_name}.go")), "go").unwrap();
    let child = receiver.run(child_id);
    assert_ne!(
        string(&parent, "deck.run.id"),
        string(&child, "deck.run.id"),
        "parent and child require independent run ids"
    );
    for (run, counts, cost) in [
        (&parent, [10, 2, 0, 0], 420),
        (&child, [100, 20, 0, 0], 4200),
    ] {
        tokens(run, counts);
        tokens(&model(run, "gpt-5.4-mini"), counts);
        expect_int(run, "deck.cost.microusd", cost);
        expect_string(run, "deck.cost.provenance", "estimated");
        ended(run, "completed", "provider_reported");
    }
}
