//! Binary-only inbox fixtures shared by the two doorbell test targets.
//! No production inbox types: S2's public JSON seams are the contract.

#![allow(dead_code)] // Each target uses a different subset of this fixture.

use crate::common::{self, DaemonProc};
use dot_agent_deck::daemon_protocol::{KIND_REQ, KIND_RESP};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub trait Endpoint {
    fn home(&self) -> &Path;
    fn cwd(&self) -> &Path;
    fn attach_socket(&self) -> &Path;
    fn hook_socket(&self) -> &Path;
}
impl Endpoint for DaemonProc {
    fn home(&self) -> &Path {
        &self.home
    }
    fn cwd(&self) -> &Path {
        &self.home
    }
    fn attach_socket(&self) -> &Path {
        &self.attach_socket
    }
    fn hook_socket(&self) -> &Path {
        &self.hook_socket
    }
}
impl Endpoint for common::TuiDeck {
    fn home(&self) -> &Path {
        self.home_dir()
    }
    fn cwd(&self) -> &Path {
        self.workdir()
    }
    fn attach_socket(&self) -> &Path {
        self.attach_socket_path()
    }
    fn hook_socket(&self) -> &Path {
        self.hook_socket_path()
    }
}

pub const WAIT: Duration = Duration::from_secs(10);
pub const BODY: &str = "DOORBELL-CONTENT-SENTINEL-1666: never type this body";
pub const PREFIX: &str = " dot-agent-deck: message ";
pub const DRAFT: &str = "unsent-human-draft-1666";

pub struct Pane {
    pub id: String,
    pub agent: String,
    pub token: String,
    pub pid: u32,
    pub capture: PathBuf,
    pub(crate) attach_socket: PathBuf,
}

impl Drop for Pane {
    fn drop(&mut self) {
        // Close only this fixture-owned agent, including while unwinding a
        // deliberate RED assertion in an attached-TUI test.
        let _ = common::attach_request_on(
            &self.attach_socket,
            &dot_agent_deck::daemon_protocol::AttachRequest::StopAgent {
                id: self.agent.clone(),
            },
        );
    }
}

// New S4 overrides. Schedule values are successive delays; the final delay
// repeats. ACK_WAIT is already specified by S1.
pub fn daemon(extra: &[(&str, &str)]) -> DaemonProc {
    let mut env = vec![
        ("DOT_AGENT_DECK_DOORBELL_TICK_MS", "25"),
        ("DOT_AGENT_DECK_DOORBELL_SCHEDULE_MS", "30000"),
        ("DOT_AGENT_DECK_INBOX_ARM_GRACE_MS", "750"),
    ];
    env.extend_from_slice(extra);
    common::spawn_daemon_serve_with_env(None, "0", &env)
}

/// [`daemon`] for a synthetic test whose inbox components connect from the
/// test process, outside the pane's agent (PRD #1666 hardening M2). The
/// binding itself has its own test (inbox/protocol/014); real-agent tests
/// use [`daemon`], which keeps it.
pub fn daemon_unbound(extra: &[(&str, &str)]) -> DaemonProc {
    let mut env = vec![("DOT_AGENT_DECK_INBOX_TEST_UNBOUND_COMPONENTS", "1")];
    env.extend_from_slice(extra);
    daemon(&env)
}

pub fn attach(daemon: &impl Endpoint, req: Value) -> Value {
    let mut socket = UnixStream::connect(daemon.attach_socket()).unwrap();
    socket.set_read_timeout(Some(WAIT)).unwrap();
    socket.set_write_timeout(Some(WAIT)).unwrap();
    let bytes = serde_json::to_vec(&req).unwrap();
    socket.write_all(&[KIND_REQ]).unwrap();
    socket
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .unwrap();
    socket.write_all(&bytes).unwrap();
    let mut header = [0; 5];
    socket.read_exact(&mut header).unwrap_or_else(|error| {
        panic!(
            "attach op {} must answer (missing inbox seam): {error}",
            req["op"]
        )
    });
    assert_eq!(header[0], KIND_RESP);
    let len = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
    assert!(len < 1024 * 1024);
    let mut bytes = vec![0; len];
    socket.read_exact(&mut bytes).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

pub fn pane(daemon: &impl Endpoint, name: &str) -> Pane {
    pane_with(daemon, name, json!({}))
}

/// [`pane`], with `extra`'s fields merged into the start request (an
/// `agent_type`, more `env` pairs appended to the pane's own).
pub fn pane_with(daemon: &impl Endpoint, name: &str, extra: Value) -> Pane {
    let script = daemon.home().join(format!("{name}.py"));
    // Raw mode preserves CR versus LF and disables kernel echo. Render the
    // received line so the same bytes can also be checked through vt100.
    std::fs::write(&script, r#"import os, sys, tty, termios, time, json
tty.setraw(0, termios.TCSANOW)
base = sys.argv[1]
with open(base + '.pid', 'w') as f: f.write(str(os.getpid()))
capture = open(base + '.bytes', 'ab', buffering=0)
turns = open(base + '.turns', 'a', buffering=1)
os.write(1, b'DOORBELL-STANDIN-READY\r\n')
line = bytearray()
while True:
    chunk = os.read(0, 4096)
    if not chunk: break
    capture.write(chunk)
    for c in chunk:
        if c == 13:
            turns.write(json.dumps({'time':time.monotonic(), 'line':line.decode('utf-8', 'replace')}) + '\n')
            os.write(1, b'\r\nSUBMITTED:' + line + b'\r\n')
            line.clear()
        elif c == 21:
            line.clear()
            os.write(1, b'\r\x1b[2K')
        else:
            line.append(c)
            os.write(1, bytes([c]))
"#).unwrap();
    let base = daemon.home().join(name);
    let quote = |path: &std::path::Path| path.to_string_lossy().replace('\'', "'\\''");
    let command = format!("exec python3 '{}' '{}'", quote(&script), quote(&base));
    let mut start = json!({"op":"start-agent",
        "command":common::capability_export_command(&command), "cwd":daemon.cwd(),
        "rows":40,"cols":140,"env":[["DOT_AGENT_DECK_PANE_ID",name]],"display_name":name});
    for (key, value) in extra.as_object().into_iter().flatten() {
        if key == "env" {
            let env = start["env"].as_array_mut().unwrap();
            env.extend(value.as_array().cloned().unwrap_or_default());
        } else {
            start[key] = value.clone();
        }
    }
    let reply = attach(daemon, start);
    assert_eq!(reply["ok"], true, "stand-in spawn: {reply}");
    let agent = reply["id"].as_str().unwrap().to_string();
    let token = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(common::recorded_hook_capability(daemon.cwd(), &agent))
        .expect("stand-in capability");
    let pid_path = base.with_extension("pid");
    // This is child startup, not the doorbell's behavior deadline. Use the
    // harness's load-scaled boot ceiling for a stand-in not yet scheduled.
    let boot_budget = common::child_boot_budget();
    assert!(
        common::wait_until(boot_budget, || std::fs::read_to_string(&pid_path)
            .ok()
            .and_then(|text| text.parse::<u32>().ok())
            .is_some()),
        "stand-in {name} must export its pid within {boot_budget:?}"
    );
    let pane = Pane {
        id: name.into(),
        agent,
        token,
        pid: std::fs::read_to_string(pid_path).unwrap().parse().unwrap(),
        capture: base.with_extension("bytes"),
        attach_socket: daemon.attach_socket().to_path_buf(),
    };
    assert!(
        common::wait_until(boot_budget, || screen(daemon, &pane)
            .contains("DOORBELL-STANDIN-READY")),
        "stand-in {name} must render readiness within {boot_budget:?}"
    );
    pane
}

pub fn request(pane: &Pane, verb: &str) -> Value {
    json!({"message_type":verb,"pane_id":pane.id,"agent_id":pane.agent,"token":pane.token})
}

pub fn connect(daemon: &impl Endpoint, req: &Value) -> BufReader<UnixStream> {
    let mut socket = UnixStream::connect(daemon.hook_socket()).unwrap();
    socket.set_read_timeout(Some(WAIT)).unwrap();
    socket.set_write_timeout(Some(WAIT)).unwrap();
    writeln!(socket, "{req}").unwrap();
    BufReader::new(socket)
}

pub fn line(socket: &mut BufReader<UnixStream>) -> Value {
    let mut text = String::new();
    let result = socket.read_line(&mut text);
    assert!(
        matches!(result, Ok(n) if n > 0),
        "inbox verb must answer a JSON line: {result:?}, {text:?}"
    );
    serde_json::from_str(&text).unwrap()
}

pub fn hook(daemon: &impl Endpoint, req: Value) -> Value {
    line(&mut connect(daemon, &req))
}

pub fn status(daemon: &impl Endpoint, pane: &Pane, event: &str, agent_type: &str) {
    let req = json!({"session_id":format!("doorbell-{}", pane.agent),
        "pane_id":pane.id,"agent_id":pane.agent,"token":pane.token,
        "agent_type":agent_type,"event_type":event,"timestamp":chrono::Utc::now(),
        "cwd":daemon.cwd(),"metadata":if event == "quota_blocked" {
            json!({"quota_blocked_kind":"usage_limit","quota_blocked_detail":"synthetic quota"})
        } else { json!({}) }});
    let mut socket = connect(daemon, &req).into_inner();
    // EOF is an ingestion barrier, not an unbounded timing guess.
    socket.shutdown(std::net::Shutdown::Write).unwrap();
    let mut response = Vec::new();
    socket
        .read_to_end(&mut response)
        .expect("status ingestion barrier");
}

pub fn enqueue(daemon: &impl Endpoint, pane: &Pane, body: &str) -> String {
    let reply = attach(
        daemon,
        json!({"op":"post-message","pane_id":pane.id,
        "expected_agent_id":pane.agent,"subject":"doorbell fixture","body":body,"origin":"seed"}),
    );
    assert_eq!(reply["ok"], true, "S2 PostMessage enqueue seam: {reply}");
    reply["id"].as_str().expect("PostMessage message id").into()
}

pub fn listing(daemon: &impl Endpoint, pane: &Pane) -> Value {
    let mut req = request(pane, "inbox_list");
    req["all"] = json!(false);
    let reply = hook(daemon, req);
    assert!(reply["messages"].is_array(), "InboxList messages: {reply}");
    reply
}

pub fn state(daemon: &impl Endpoint, pane: &Pane, id: &str, expected: &str) {
    let latest = RefCell::new(Value::Null);
    assert!(
        common::wait_until(WAIT, || {
            *latest.borrow_mut() = listing(daemon, pane);
            latest.borrow()["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| {
                    message["id"] == id
                        && message["state"]
                            .to_string()
                            .to_ascii_lowercase()
                            .contains(expected)
                })
        }),
        "message {id} must become {expected}: {}",
        latest.borrow()
    );
}

pub fn armed(daemon: &impl Endpoint, pane: &Pane, count: u64) {
    assert!(
        common::wait_until(WAIT, || listing(daemon, pane)["armed"] == count),
        "armed must become {count}: {}",
        listing(daemon, pane)
    );
}

/// PRD #1666: a receipt watcher is registered for `pane` (its wake reads
/// `native`).
pub fn receipt_registered(daemon: &impl Endpoint, pane: &Pane) {
    assert!(
        common::wait_until(WAIT, || listing(daemon, pane)["wake"] == "native"),
        "a receipt watcher must register: {}",
        listing(daemon, pane)
    );
}

pub fn announce(daemon: &impl Endpoint, pane: &Pane, wake: &str) {
    let mut req = request(pane, "inbox_announce");
    req["component"] = json!("claude_waiter");
    req["component_version"] = json!("synthetic-doorbell");
    req["wake"] = json!(wake);
    req["pid"] = json!(pane.pid);
    assert_eq!(hook(daemon, req)["accepted"], true);
}

/// PRD #1666 §4: a stand-in for OpenCode's TUI receipt stream (its watcher),
/// armed from the test process at the adapter floor; returns once the deck
/// registered it.
pub fn receipt_stream(daemon: &impl Endpoint, pane: &Pane) -> BufReader<UnixStream> {
    receipt_stream_as(daemon, pane, "opencode_plugin")
}

/// [`receipt_stream`] for `component`'s watcher (`pi_extension`, …).
pub fn receipt_stream_as(
    daemon: &impl Endpoint,
    pane: &Pane,
    component: &str,
) -> BufReader<UnixStream> {
    let mut req = request(pane, "inbox_wait");
    req["component"] = json!(component);
    req["component_version"] = json!("synthetic-receipt");
    req["mode"] = json!("stream");
    req["pid"] = json!(pane.pid);
    req["adapter_integration"] = json!(2);
    let mut stream = connect(daemon, &req);
    let registered = line(&mut stream);
    assert!(
        registered["registered"].is_object(),
        "a receipt stream registers first: {registered}"
    );
    stream
}

/// Read a receipt stream until it hands over `id`, skipping keepalives and
/// other lines; the hand-out (`{id, a, e, d, …}`).
pub fn receipt_handout(stream: &mut BufReader<UnixStream>, id: &str) -> Value {
    loop {
        let reply = line(stream);
        if let Some(found) = reply["messages"]
            .as_array()
            .and_then(|messages| messages.iter().find(|m| m["id"] == id))
        {
            return found.clone();
        }
    }
}

/// Write one control line to a receipt stream.
pub fn receipt_control(stream: &mut BufReader<UnixStream>, control: Value) {
    writeln!(stream.get_mut(), "{control}").unwrap();
}

/// Every line a receipt stream writes within `span`, keepalives included.
pub fn receipt_lines_within(stream: &mut BufReader<UnixStream>, span: Duration) -> Vec<Value> {
    let until = std::time::Instant::now() + span;
    let mut out = Vec::new();
    while let Some(left) = until.checked_duration_since(std::time::Instant::now()) {
        stream
            .get_mut()
            .set_read_timeout(Some(left.max(Duration::from_millis(5))))
            .unwrap();
        let mut text = String::new();
        match stream.read_line(&mut text) {
            Ok(n) if n > 0 => out.extend(serde_json::from_str::<Value>(&text).ok()),
            _ => break,
        }
    }
    stream.get_mut().set_read_timeout(Some(WAIT)).unwrap();
    out
}

/// PRD #1666 §4: keep `held` possessed on `stream` for `span`, one fresh
/// snapshot every 150 ms, and return what the deck wrote meanwhile.
pub fn possess_for(stream: &mut BufReader<UnixStream>, held: &Value, span: Duration) -> Vec<Value> {
    let until = std::time::Instant::now() + span;
    let mut seq = 0u64;
    let mut written = Vec::new();
    while std::time::Instant::now() < until {
        seq += 1;
        receipt_control(
            stream,
            json!({"possess": {"seq": seq, "seen": held["d"], "held": [{"id": held["id"], "a": held["a"], "e": held["e"]}]}}),
        );
        written.extend(receipt_lines_within(stream, Duration::from_millis(150)));
    }
    written
}

/// A legacy stream waiter, armed from the test process. No component the deck
/// ships arms one since Pi moved to the receipt stream (PRD #1666), so it names
/// none (`other`).
pub fn waiter(daemon: &impl Endpoint, pane: &Pane) -> BufReader<UnixStream> {
    let mut req = request(pane, "inbox_wait");
    req["component"] = json!("other");
    req["component_version"] = json!("synthetic-doorbell");
    req["mode"] = json!("stream");
    req["pid"] = json!(pane.pid);
    connect(daemon, &req)
}

pub fn fetch(daemon: &impl Endpoint, pane: &Pane, ack: bool) -> Value {
    let mut req = request(pane, "inbox_fetch");
    req["consumer"] = json!("cli");
    req["max_chars"] = json!(65536);
    req["ack"] = json!(ack);
    let reply = hook(daemon, req);
    assert!(reply["messages"].is_array(), "fetch blocks: {reply}");
    reply
}

pub fn fixed_line(id: &str) -> String {
    format!("{PREFIX}{id} waiting, run: dot-agent-deck inbox fetch")
}

pub fn bytes(pane: &Pane) -> Vec<u8> {
    std::fs::read(&pane.capture).unwrap_or_default()
}

pub fn turns(pane: &Pane) -> Vec<Value> {
    std::fs::read_to_string(pane.capture.with_extension("turns"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

pub fn rings(pane: &Pane) -> Vec<Value> {
    turns(pane)
        .into_iter()
        .filter(|turn| {
            turn["line"]
                .as_str()
                .is_some_and(|line| line.starts_with(PREFIX))
        })
        .collect()
}

pub fn screen(daemon: &impl Endpoint, pane: &Pane) -> String {
    let snapshot = common::pane_snapshot_on(daemon.attach_socket(), &pane.agent);
    let mut parser = vt100::Parser::new(40, 140, 0);
    parser.process(&snapshot);
    parser.screen().contents()
}

pub fn ring(daemon: &impl Endpoint, pane: &Pane, id: &str) {
    let line = fixed_line(id);
    assert!(
        common::wait_until(WAIT, || rings(pane).iter().any(|turn| turn["line"] == line)),
        "doorbell must submit {line:?}; received={:?}; grid={}",
        bytes(pane),
        screen(daemon, pane)
    );
    assert!(
        common::wait_until(WAIT, || screen(daemon, pane)
            .contains(&format!("SUBMITTED:{line}"))),
        "submitted fixed doorbell must be visible: {}",
        screen(daemon, pane)
    );
    assert!(
        !screen(daemon, pane).contains(BODY),
        "message body was typed into pane"
    );
}

pub fn unchanged(pane: &Pane, duration: Duration) {
    let before = bytes(pane);
    assert!(
        !common::wait_until(duration, || bytes(pane) != before),
        "guarded pane received bytes: before={before:?}, after={:?}",
        bytes(pane)
    );
}

pub fn input(daemon: &impl Endpoint, pane: &Pane, text: &str) -> common::AttachInput {
    let mut input = common::AttachInput::attach(daemon.attach_socket(), &pane.agent);
    input.write(text);
    assert!(
        common::wait_until(WAIT, || bytes(pane).ends_with(text.as_bytes())),
        "input not received"
    );
    input
}
