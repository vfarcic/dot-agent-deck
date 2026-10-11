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
// repeats. ACK_WAIT is already specified by S1. The legacy cap deliberately
// remains tiny: any path retaining its force-submit behavior fails our tests.
pub fn daemon(extra: &[(&str, &str)]) -> DaemonProc {
    let mut env = vec![
        ("DOT_AGENT_DECK_DOORBELL_TICK_MS", "25"),
        ("DOT_AGENT_DECK_DOORBELL_SCHEDULE_MS", "30000"),
        ("DOT_AGENT_DECK_INBOX_ARM_GRACE_MS", "750"),
        ("DOT_AGENT_DECK_DRAFT_DEFER_CAP_MS", "100"),
    ];
    env.extend_from_slice(extra);
    common::spawn_daemon_serve_with_env(None, "0", &env)
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
    let reply = attach(
        daemon,
        json!({"op":"start-agent",
        "command":common::capability_export_command(&command), "cwd":daemon.cwd(),
        "rows":40,"cols":140,"env":[["DOT_AGENT_DECK_PANE_ID",name]],"display_name":name}),
    );
    assert_eq!(reply["ok"], true, "stand-in spawn: {reply}");
    let agent = reply["id"].as_str().unwrap().to_string();
    let token = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(common::recorded_hook_capability(daemon.cwd(), &agent))
        .expect("stand-in capability");
    let pid_path = base.with_extension("pid");
    assert!(common::wait_until(WAIT, || std::fs::read_to_string(
        &pid_path
    )
    .ok()
    .and_then(|text| text.parse::<u32>().ok())
    .is_some()));
    let pane = Pane {
        id: name.into(),
        agent,
        token,
        pid: std::fs::read_to_string(pid_path).unwrap().parse().unwrap(),
        capture: base.with_extension("bytes"),
        attach_socket: daemon.attach_socket().to_path_buf(),
    };
    assert!(common::wait_until(WAIT, || screen(daemon, &pane)
        .contains("DOORBELL-STANDIN-READY")));
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

pub fn announce(daemon: &impl Endpoint, pane: &Pane, wake: &str) {
    let mut req = request(pane, "inbox_announce");
    req["component"] = json!("claude_waiter");
    req["component_version"] = json!("synthetic-doorbell");
    req["wake"] = json!(wake);
    req["pid"] = json!(pane.pid);
    assert_eq!(hook(daemon, req)["accepted"], true);
}

pub fn waiter(daemon: &impl Endpoint, pane: &Pane) -> BufReader<UnixStream> {
    let mut req = request(pane, "inbox_wait");
    req["component"] = json!("pi_extension");
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
