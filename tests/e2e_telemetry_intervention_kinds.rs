#![cfg(all(feature = "e2e", unix))]

//! Synthetic input through an attached TUI, observed at an isolated OTLP receiver.
//! The stand-in records raw input; it never calls a model or reads credentials.

mod common;

use common::TuiDeck;
use dot_agent_deck::agent_pty::AgentRecord;
use dot_agent_deck::daemon_client::{AnswerReport, DaemonClient};
use dot_agent_deck::daemon_protocol::AttachRequest;
use dot_agent_deck::event::{AgentEvent, DaemonMessage, EventType, QuestionSignal};
use dot_agent_deck::question::{
    AnswerChannel, OptionRole, PendingQuestion, Question, QuestionAnswer, QuestionKind,
    QuestionOption,
};
use serde_json::{Value, json};
use spec::spec;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(25);
const PRIVATE: &str = "PRIVATE_INTERVENTION_1981_68a7";
const READY: &str = "intervention-standin-ready";

struct Receiver {
    addr: SocketAddr,
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
                reader.read_line(&mut line).unwrap();
                assert!(line.starts_with("POST /v1/traces "), "{line}");
                let mut length = 0;
                let mut chunked = false;
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    let (key, value) = line.trim_end().split_once(':').unwrap();
                    if key.eq_ignore_ascii_case("content-length") {
                        length = value.trim().parse().unwrap();
                    }
                    if key.eq_ignore_ascii_case("transfer-encoding") {
                        chunked = value.trim().eq_ignore_ascii_case("chunked");
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
                        reader.read_exact(&mut [0; 2]).unwrap();
                    }
                } else {
                    body.resize(length, 0);
                    reader.read_exact(&mut body).unwrap();
                }
                captured.lock().unwrap().push(body);
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
            }
        });
        Self {
            addr,
            bodies,
            stop,
            worker: Some(worker),
        }
    }

    fn runs(&self, pane: &str) -> Vec<Value> {
        self.bodies
            .lock()
            .unwrap()
            .iter()
            .flat_map(|body| {
                let body: Value = serde_json::from_slice(body).expect("OTLP JSON");
                let mut spans = Vec::new();
                for resource in body["resourceSpans"].as_array().unwrap() {
                    for scope in resource["scopeSpans"].as_array().unwrap() {
                        spans.extend(
                            scope["spans"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .filter(|span| {
                                    span["name"] == "agent.run"
                                        && attribute(span, "deck.pane.id") == pane
                                })
                                .cloned(),
                        );
                    }
                }
                spans
            })
            .collect()
    }

    fn run(&self, pane: &str) -> Value {
        assert!(
            common::wait_until(WAIT, || !self.runs(pane).is_empty()),
            "receiver never received completed agent.run for {pane}"
        );
        let runs = self.runs(pane);
        assert_eq!(runs.len(), 1, "one completed run per pane");
        runs[0].clone()
    }

    fn assert_private(&self) {
        for body in self.bodies.lock().unwrap().iter() {
            assert!(
                !body
                    .windows(PRIVATE.len())
                    .any(|bytes| bytes == PRIVATE.as_bytes()),
                "typed/pasted/question/inbox content leaked in OTLP body"
            );
        }
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.addr);
        self.worker.take().unwrap().join().unwrap();
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
        .to_owned()
}

fn launch(receiver: &Receiver) -> (TuiDeck, AgentRecord) {
    let deck = TuiDeck::builder()
        .without_agent_credentials()
        .impersonating_pane_signals()
        .with_pty_size(180, 45)
        .with_env("OTEL_EXPORTER_OTLP_ENDPOINT", format!("http://{}", receiver.addr))
        .with_env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json")
        // Raw input prevents Ctrl+C from killing the stand-in and records
        // cursor/paste sequences as received. Advertise bracketed-paste mode.
        .with_continue_session("intervention-standin",
            common::capability_export_command(&format!("/bin/sh -c 'stty raw -echo; printf \"\\033[?2004h{READY}\\n\"; exec cat > intervention-input.bin'")))
        .launch_with_fixture("minimal");
    deck.wait_for_string(READY);
    let agents = common::agent_records_on(deck.attach_socket_path());
    assert_eq!(agents.len(), 1);
    (deck, agents[0].clone())
}

fn send(deck: &TuiDeck, bytes: &[u8]) {
    send_via(deck, bytes, || deck.send_bytes(bytes));
}

// Preserve bytes a terminal key parser normalizes (double Escape and mouse
// reports). This is the public attach input path, also used by the desktop,
// against the same live pane the TUI is displaying.
fn send_attached(deck: &TuiDeck, agent: &AgentRecord, bytes: &[u8]) {
    send_via(deck, bytes, || {
        let client = DaemonClient::new(deck.attach_socket_path().to_path_buf());
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut stream = client.attach(&agent.id).await.unwrap();
                stream.write_input(bytes).await.unwrap();
                stream.detach().await.unwrap();
            });
    });
}

fn send_via(deck: &TuiDeck, bytes: &[u8], write: impl FnOnce()) {
    let path = deck.workdir().join("intervention-input.bin");
    let before = std::fs::read(&path).unwrap_or_default().len();
    write();
    assert!(
        common::wait_until(WAIT, || std::fs::read(&path).is_ok_and(|got| {
            got.get(before..)
                .is_some_and(|tail| tail.windows(bytes.len()).any(|part| part == bytes))
        })),
        "stand-in never received {bytes:?}; grid:\n{}",
        deck.snapshot_grid()
    );
}

fn finish(receiver: &Receiver, deck: &TuiDeck, agent: &AgentRecord) -> Value {
    let response = common::attach_request_on(
        deck.attach_socket_path(),
        &AttachRequest::StopAgent {
            id: agent.id.clone(),
        },
    )
    .unwrap();
    assert!(response.ok, "StopAgent: {response:?}");
    let run = receiver.run(agent.pane_id_env.as_deref().unwrap());
    receiver.assert_private();
    run
}

fn assert_kinds(run: &Value, expected: &[&str]) {
    let count = run["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|attr| attr["key"] == "deck.interventions.count")
        .unwrap()["value"]["intValue"]
        .clone();
    let count = count
        .as_u64()
        .unwrap_or_else(|| count.as_str().unwrap().parse().unwrap());
    let kinds: Vec<_> = run["events"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|event| event["name"] == "deck.intervention")
        .map(|event| attribute(event, "deck.intervention.kind"))
        .collect();
    assert_eq!(
        count,
        expected.len() as u64,
        "burst count; exported kinds={kinds:?}"
    );
    assert_eq!(kinds, expected, "exported intervention kinds");
}

fn event(agent: &AgentRecord, event_type: EventType) -> AgentEvent {
    serde_json::from_value(json!({
        "session_id": "intervention-synthetic", "pane_id": agent.pane_id_env,
        "agent_id": agent.id, "agent_type": "claude_code", "event_type": event_type,
        "timestamp": chrono::Utc::now().to_rfc3339(), "metadata": {}
    }))
    .unwrap()
}

fn hook(deck: &TuiDeck, event: &AgentEvent) {
    let events = deck.subscribe_events();
    let mut payload = serde_json::to_value(event).unwrap();
    // Current synthetic hook, rather than a hook predating inbox support.
    payload["deck_build"] = json!("");
    payload["token"] = json!(capability(deck, event.agent_id.as_deref().unwrap()));
    common::write_hook_line(deck.hook_socket_path(), &payload.to_string()).unwrap();
    events.wait_for(
        |seen| seen.agent_id == event.agent_id && seen.event_type == event.event_type,
        WAIT,
    );
}

fn capability(deck: &TuiDeck, agent_id: &str) -> String {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(common::recorded_hook_capability(deck.workdir(), agent_id))
        .expect("stand-in capability")
}

fn question() -> PendingQuestion {
    PendingQuestion {
        id: "intervention-question".into(),
        kind: QuestionKind::Permission,
        questions: vec![Question {
            prompt: PRIVATE.into(),
            header: None,
            multi_select: false,
            options: vec![QuestionOption {
                index: 1,
                label: "Yes".into(),
                description: None,
                role: OptionRole::AllowOnce,
                keyboard_only: false,
                scope: None,
                maybe_hidden: false,
            }],
        }],
        raised_at_ms: chrono::Utc::now().timestamp_millis(),
        tool: None,
        channel: AnswerChannel::Held,
        subagent_id: None,
        revision: None,
    }
}

/// Scenario: Type printable steering text, Backspace and Enter into an attached stand-in pane. The completed OTLP run has one text intervention and contains none of the typed content.
#[spec("telemetry/otlp/028")]
#[test]
fn telemetry_otlp_028_text_intervention() {
    let receiver = Receiver::start();
    let (deck, agent) = launch(&receiver);
    send(&deck, format!("{PRIVATE}\x7f\r").as_bytes());
    assert_kinds(&finish(&receiver, &deck, &agent), &["text"]);
}

/// Scenario: Paste a private sentinel with bracketed-paste markers into an attached stand-in pane. The exported run counts one paste intervention without exporting the sentinel.
#[spec("telemetry/otlp/029")]
#[test]
fn telemetry_otlp_029_paste_intervention() {
    let receiver = Receiver::start();
    let (deck, agent) = launch(&receiver);
    send(&deck, format!("\x1b[200~{PRIVATE}\x1b[201~").as_bytes());
    assert_kinds(&finish(&receiver, &deck, &agent), &["paste"]);
}

/// Scenario: Send cursor navigation and an Alt chord through the TUI, then mouse reports through an attach stream in a separate run. Each stand-in receives the bytes and exports one navigate intervention rather than text or interrupt.
#[spec("telemetry/otlp/030")]
#[test]
fn telemetry_otlp_030_navigation_intervention() {
    let mut runs = Vec::new();
    for mouse in [false, true] {
        let receiver = Receiver::start();
        let (deck, agent) = launch(&receiver);
        if mouse {
            send_attached(&deck, &agent, b"\x1b[<0;10;10M\x1b[<0;10;10m");
        } else {
            send(&deck, b"\x1b[A\x1b[B\x1b[C\x1b[D\x1bx");
        }
        runs.push(finish(&receiver, &deck, &agent));
    }
    for run in runs {
        assert_kinds(&run, &["navigate"]);
    }
}

/// Scenario: Send Ctrl+C and a lone Escape through the TUI and double Escape through an attach stream to separate stand-in runs. Each completed run exports exactly one interrupt, including an Escape held until run closure.
#[spec("telemetry/otlp/031")]
#[test]
fn telemetry_otlp_031_interrupt_intervention() {
    let mut runs = Vec::new();
    for bytes in [b"\x03".as_slice(), b"\x1b", b"\x1b\x1b"] {
        let receiver = Receiver::start();
        let (deck, agent) = launch(&receiver);
        if bytes == b"\x1b\x1b" {
            send_attached(&deck, &agent, bytes);
        } else {
            send(&deck, bytes);
        }
        runs.push(finish(&receiver, &deck, &agent));
    }
    for run in runs {
        assert_kinds(&run, &["interrupt"]);
    }
}

/// Scenario: Raise a synthetic permission prompt and type a private answer into the attached pane. Repeat with a plain waiting-for-input prompt; both runs export one answer intervention without its text.
#[spec("telemetry/otlp/032")]
#[test]
fn telemetry_otlp_032_typed_answer_intervention() {
    let mut actual = Vec::new();
    for event_type in [EventType::PermissionRequest, EventType::WaitingForInput] {
        let receiver = Receiver::start();
        let (deck, agent) = launch(&receiver);
        hook(&deck, &event(&agent, event_type));
        send(&deck, format!("{PRIVATE}\r").as_bytes());
        actual.push(finish(&receiver, &deck, &agent));
    }
    for run in actual {
        assert_kinds(&run, &["answer"]);
    }
}

fn hold_question(deck: &TuiDeck, agent: &AgentRecord) -> (BufReader<UnixStream>, PendingQuestion) {
    let mut asking = event(agent, EventType::PermissionRequest);
    asking.set_question(&question());
    let signal = DaemonMessage::Question(Box::new(QuestionSignal {
        pane_id: agent.pane_id_env.clone().unwrap(),
        token: Some(capability(deck, &agent.id)),
        event: asking,
        hold: true,
        ack: false,
        confirm_hold: true,
    }));
    let mut stream = UnixStream::connect(deck.hook_socket_path()).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    writeln!(stream, "{}", serde_json::to_string(&signal).unwrap()).unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let held: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(held["outcome"], "held", "{held}");
    let pending = common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .find(|record| record.id == agent.id)
        .unwrap()
        .live
        .unwrap()
        .pending_question
        .unwrap();
    (reader, pending)
}

/// Scenario: Hold a synthetic permission hook open and answer it through the same client API used by desktop voice answering. The hook receives the chosen answer, no answer keys reach the pane, and the exported run counts one answer intervention.
#[spec("telemetry/otlp/033")]
#[test]
fn telemetry_otlp_033_deck_held_answer_intervention() {
    let receiver = Receiver::start();
    let (deck, agent) = launch(&receiver);
    let (mut reader, pending) = hold_question(&deck, &agent);
    let client = DaemonClient::new(deck.attach_socket_path().to_path_buf());
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(client.answer_question_while(
            || true,
            &agent.id,
            &pending.id,
            pending.revision,
            vec![QuestionAnswer {
                question_index: 0,
                option_indices: vec![1],
                text: None,
            }],
            false,
        ))
        .unwrap();
    assert_eq!(result, AnswerReport::Answered);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let answered: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(answered["outcome"], "answered", "{answered}");
    assert!(
        std::fs::read(deck.workdir().join("intervention-input.bin"))
            .unwrap()
            .is_empty(),
        "held answer must use the hook channel"
    );
    assert_kinds(&finish(&receiver, &deck, &agent), &["answer"]);
}

/// Scenario: Type private steering text and then Ctrl+C in the same attached-pane burst. One intervention is exported with interrupt precedence, and the typed text remains private.
#[spec("telemetry/otlp/034")]
#[test]
fn telemetry_otlp_034_interrupt_upgrades_text_burst() {
    let receiver = Receiver::start();
    let (deck, agent) = launch(&receiver);
    send(&deck, PRIVATE.as_bytes());
    send(&deck, b"\x03");
    assert_kinds(&finish(&receiver, &deck, &agent), &["interrupt"]);
}

/// Scenario: Post an inbox task to an attached stand-in and wait for its automatic doorbell to reach the pane without typing any human input. Its completed run exports zero interventions and none of the task content.
#[spec("telemetry/otlp/035")]
#[test]
fn telemetry_otlp_035_deck_doorbell_is_not_intervention() {
    let receiver = Receiver::start();
    let (deck, agent) = launch(&receiver);
    hook(&deck, &event(&agent, EventType::SessionStart));
    hook(&deck, &event(&agent, EventType::Idle));
    common::post_message_on(
        deck.attach_socket_path(),
        agent.pane_id_env.as_deref().unwrap(),
        &agent.id,
        PRIVATE,
    )
    .unwrap();
    common::wait_for_file_contains(
        &deck.workdir().join("intervention-input.bin"),
        "inbox fetch",
    );
    assert_kinds(&finish(&receiver, &deck, &agent), &[]);
}

/// Scenario: Report Claude's idle-prompt notification and then type a new private steering prompt through the attached pane. The exported intervention remains text because the idle notification is not a question.
#[spec("telemetry/otlp/036")]
#[test]
fn telemetry_otlp_036_idle_notification_is_text() {
    let receiver = Receiver::start();
    let (deck, agent) = launch(&receiver);
    let mut idle = event(&agent, EventType::WaitingForInput);
    idle.metadata
        .insert("notification_type".into(), "idle_prompt".into());
    hook(&deck, &idle);
    send(&deck, format!("{PRIVATE}\r").as_bytes());
    assert_kinds(&finish(&receiver, &deck, &agent), &["text"]);
}

/// Scenario: Exercise mixed bursts in separate attached panes: navigation plus text, text plus paste, a paste during a permission prompt, and Escape during a permission prompt. Each run exports one intervention with the strongest kind, without any private payload.
#[spec("telemetry/otlp/037")]
#[test]
fn telemetry_otlp_037_kind_precedence_and_question_escape() {
    let mut runs = Vec::new();
    for (has_question, bytes, expected) in [
        (false, format!("\x1b[A{PRIVATE}"), "text"),
        (
            false,
            format!("{PRIVATE}\x1b[200~{PRIVATE}\x1b[201~"),
            "paste",
        ),
        (true, format!("\x1b[200~{PRIVATE}\x1b[201~"), "answer"),
        (true, "\x1b".to_owned(), "interrupt"),
    ] {
        let receiver = Receiver::start();
        let (deck, agent) = launch(&receiver);
        // Keep the producer's held connection alive while input is sent:
        // ordinary status ingress intentionally strips question metadata.
        let _held = has_question.then(|| hold_question(&deck, &agent));
        send(&deck, bytes.as_bytes());
        runs.push((finish(&receiver, &deck, &agent), expected));
    }
    for (run, expected) in runs {
        assert_kinds(&run, &[expected]);
    }
}
