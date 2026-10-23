//! Binary-only inbox fixtures shared by the two doorbell test targets.
//! No production inbox types: S2's public JSON seams are the contract.

#![allow(dead_code)] // Each target uses a different subset of this fixture.

use crate::common::{self, DaemonProc};
use dot_agent_deck::daemon_protocol::{KIND_REQ, KIND_RESP, KIND_STREAM_END, KIND_STREAM_OUT};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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

thread_local! {
    static INTERLEAVE_DEADLINE: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
    static INTERLEAVE_ROOT: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// One six-second operation budget, shorter than the seam's eight seconds.
/// Unwinding releases every armed barrier before the fixture is torn down.
pub struct InterleaveBudget {
    root: PathBuf,
}

impl InterleaveBudget {
    pub fn start(root: &Path) -> Self {
        assert!(
            INTERLEAVE_DEADLINE
                .with(|cell| cell.replace(Some(Instant::now() + Duration::from_secs(6))))
                .is_none()
        );
        INTERLEAVE_ROOT.with(|cell| *cell.borrow_mut() = Some(root.into()));
        Self { root: root.into() }
    }
}

impl Drop for InterleaveBudget {
    fn drop(&mut self) {
        for phase in [
            "decided",
            "after-pre-cr",
            "after-delay",
            "before-submit",
            "before-pre-cr",
            "completed",
        ] {
            let _ = std::fs::write(self.root.join(format!("{phase}.release")), "");
        }
        INTERLEAVE_DEADLINE.with(|cell| cell.set(None));
        INTERLEAVE_ROOT.with(|cell| *cell.borrow_mut() = None);
    }
}

pub fn wait_budget() -> Duration {
    INTERLEAVE_ROOT.with(|cell| {
        if let Some(root) = cell.borrow().as_ref() {
            for phase in [
                "decided",
                "after-pre-cr",
                "after-delay",
                "before-submit",
                "before-pre-cr",
                "completed",
            ] {
                assert!(
                    !root.join(format!("{phase}.timed-out")).exists(),
                    "interleave seam timed out at {phase}"
                );
            }
        }
    });
    INTERLEAVE_DEADLINE.with(|cell| {
        cell.get().map_or(WAIT, |deadline| {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(
                !left.is_zero(),
                "interleave exceeded its shared six-second fixture budget"
            );
            left.min(WAIT)
        })
    })
}

fn read_exact_budgeted(socket: &mut UnixStream, mut bytes: &mut [u8]) -> std::io::Result<()> {
    while !bytes.is_empty() {
        socket.set_read_timeout(Some(wait_budget()))?;
        let read = socket.read(bytes)?;
        if read == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        bytes = &mut bytes[read..];
    }
    Ok(())
}

fn write_all_budgeted(socket: &mut UnixStream, mut bytes: &[u8]) -> std::io::Result<()> {
    while !bytes.is_empty() {
        socket.set_write_timeout(Some(wait_budget()))?;
        let written = socket.write(bytes)?;
        if written == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        bytes = &bytes[written..];
    }
    Ok(())
}

/// A completed policy tick at an explicitly advanced age, including all its
/// spawned attempts. The recorder drain then makes zero input a history oracle.
pub fn clock_step(daemon: &impl Endpoint, pane: &Pane, age_ms: u64) {
    let root = daemon.home().join("ring-barriers");
    std::fs::write(root.join("clock-ms"), age_ms.to_string()).unwrap();
    assert!(
        common::wait_until(wait_budget(), || {
            assert!(
                bytes(pane).is_empty(),
                "unready generation received automatic input at controlled policy age {age_ms}ms: {:?}",
                String::from_utf8_lossy(&bytes(pane))
            );
            std::fs::read_to_string(root.join("clock-observed"))
                .is_ok_and(|age| age == age_ms.to_string())
        }),
        "policy tick must settle at controlled age {age_ms}ms"
    );
    drained(pane);
    assert!(
        bytes(pane).is_empty(),
        "unready generation received automatic input at controlled policy age {age_ms}ms: {:?}",
        String::from_utf8_lossy(&bytes(pane))
    );
}

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
    socket.set_read_timeout(Some(wait_budget())).unwrap();
    socket.set_write_timeout(Some(wait_budget())).unwrap();
    let bytes = serde_json::to_vec(&req).unwrap();
    write_all_budgeted(&mut socket, &[KIND_REQ]).unwrap();
    write_all_budgeted(&mut socket, &(bytes.len() as u32).to_be_bytes()).unwrap();
    write_all_budgeted(&mut socket, &bytes).unwrap();
    let mut header = [0; 5];
    read_exact_budgeted(&mut socket, &mut header).unwrap_or_else(|error| {
        panic!(
            "attach op {} must answer (missing inbox seam): {error}",
            req["op"]
        )
    });
    assert_eq!(header[0], KIND_RESP);
    let len = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
    assert!(len < 1024 * 1024);
    let mut bytes = vec![0; len];
    read_exact_budgeted(&mut socket, &mut bytes).unwrap();
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
    std::fs::write(&script, r#"import os, sys, tty, termios, time, json, select
tty.setraw(0, termios.TCSANOW)
base = sys.argv[1]
with open(base + '.env', 'w') as f: json.dump({k: os.environ[k] for k in ['DOT_AGENT_DECK_INBOX_SPOOL'] if k in os.environ}, f)
with open(base + '.pid', 'w') as f: f.write(str(os.getpid()))
capture = open(base + '.bytes', 'ab', buffering=0)
turns = open(base + '.turns', 'a', buffering=1)
label = os.environ.get('DOORBELL_STANDIN_LABEL', '')
os.write(1, ('DOORBELL-STANDIN-READY' + label + '\r\n').encode())
line = bytearray()
while True:
    if not select.select([0], [], [], 0.025)[0]:
        if os.path.exists(base + '.drain'):
            os.remove(base + '.drain')
            open(base + '.drained', 'w').close()
        if label: os.write(1, ('\r\n' + label + '\r\n').encode())
        continue
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

/// The value of `name` in the environment the deck gave the stand-in's
/// process, of the keys it writes beside its pid as it starts: only
/// `DOT_AGENT_DECK_INBOX_SPOOL`, so the file never holds the pane's capability
/// or anything else the stand-in inherited.
pub fn pane_env(pane: &Pane, name: &str) -> Option<String> {
    let env: Value = serde_json::from_str(
        &std::fs::read_to_string(pane.capture.with_extension("env")).expect("the pane's env"),
    )
    .expect("the pane's env is JSON");
    env[name].as_str().map(str::to_string)
}

pub fn request(pane: &Pane, verb: &str) -> Value {
    json!({"message_type":verb,"pane_id":pane.id,"agent_id":pane.agent,"token":pane.token})
}

pub fn connect(daemon: &impl Endpoint, req: &Value) -> BufReader<UnixStream> {
    let mut socket = UnixStream::connect(daemon.hook_socket()).unwrap();
    socket.set_read_timeout(Some(wait_budget())).unwrap();
    socket.set_write_timeout(Some(wait_budget())).unwrap();
    write_all_budgeted(&mut socket, format!("{req}\n").as_bytes()).unwrap();
    BufReader::new(socket)
}

pub fn line(socket: &mut BufReader<UnixStream>) -> Value {
    if INTERLEAVE_DEADLINE.with(|cell| cell.get().is_none()) {
        // Receipt observers install their own shorter socket timeout.
        let mut text = String::new();
        let result = socket.read_line(&mut text);
        assert!(
            matches!(result, Ok(n) if n > 0),
            "inbox verb must answer a JSON line: {result:?}, {text:?}"
        );
        return serde_json::from_str(&text).unwrap();
    }
    let mut text = Vec::new();
    loop {
        socket
            .get_mut()
            .set_read_timeout(Some(wait_budget()))
            .unwrap();
        let buffer = socket
            .fill_buf()
            .expect("inbox verb must answer within fixture budget");
        assert!(
            !buffer.is_empty(),
            "inbox verb must answer a JSON line: {text:?}"
        );
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(buffer.len(), |index| index + 1);
        text.extend_from_slice(&buffer[..take]);
        socket.consume(take);
        if newline.is_some() {
            return serde_json::from_slice(&text).unwrap();
        }
    }
}

pub fn hook(daemon: &impl Endpoint, req: Value) -> Value {
    line(&mut connect(daemon, &req))
}

/// The version stamp a synthetic hook line carries. Every hook line from a
/// current deck is stamped, and an unstamped Claude line reads as hooks from
/// before the deck's messages (PRD #1666 part 3, R-3); an empty build counts as
/// stamped and is never compared with the daemon's own.
pub const SYNTHETIC_DECK_BUILD: &str = "";

pub fn status(daemon: &impl Endpoint, pane: &Pane, event: &str, agent_type: &str) {
    let req = json!({"session_id":format!("doorbell-{}", pane.agent),
        "pane_id":pane.id,"agent_id":pane.agent,"token":pane.token,
        "deck_build":SYNTHETIC_DECK_BUILD,
        "agent_type":agent_type,"event_type":event,"timestamp":chrono::Utc::now(),
        "cwd":daemon.cwd(),"metadata":if event == "quota_blocked" {
            json!({"quota_blocked_kind":"usage_limit","quota_blocked_detail":"synthetic quota"})
        } else { json!({}) }});
    let mut socket = connect(daemon, &req).into_inner();
    // EOF is an ingestion barrier, not an unbounded timing guess.
    socket.shutdown(std::net::Shutdown::Write).unwrap();
    loop {
        socket.set_read_timeout(Some(wait_budget())).unwrap();
        if socket
            .read(&mut [0; 1024])
            .expect("status ingestion barrier")
            == 0
        {
            break;
        }
    }
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
        common::wait_until(wait_budget(), || {
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
        common::wait_until(wait_budget(), || listing(daemon, pane)["armed"] == count),
        "armed must become {count}: {}",
        listing(daemon, pane)
    );
}

/// PRD #1666: a receipt watcher is registered for `pane` (its wake reads
/// `native`).
pub fn receipt_registered(daemon: &impl Endpoint, pane: &Pane) {
    assert!(
        common::wait_until(wait_budget(), || listing(daemon, pane)["wake"] == "native"),
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
    stream
        .get_mut()
        .set_read_timeout(Some(wait_budget()))
        .unwrap();
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

/// A Claude Code once-waiter, armed from the test process at the one-shot
/// integration (PRD #1666 part 2 §2.7). It reads nothing; a test that needs
/// its batch drives the real waiter instead.
pub fn waiter(daemon: &impl Endpoint, pane: &Pane) -> BufReader<UnixStream> {
    let mut req = request(pane, "inbox_wait");
    req["component"] = json!("claude_waiter");
    req["hook_integration"] = json!(1);
    req["component_version"] = json!("synthetic-doorbell");
    req["mode"] = json!("once");
    req["session"] = json!({"source":"startup", "session_id":"doorbell-waiter"});
    req["pid"] = json!(pane.pid);
    connect(daemon, &req)
}

/// Take what is waiting for `pane`'s agent as a current `dot-agent-deck inbox
/// fetch` does (PRD #1666 part 3): a one-shot receipt fetch, consumer `cli`,
/// at the one-shot integration, granted each block before it would print it
/// (`may`). With `ack`, the agent then acknowledges each message it was
/// handed (`ack <id>`), which settles it. Without, it is [`take`] with the
/// fetch's connection closed at once: each block may have reached the agent
/// and nothing settles it, and with its receiver gone the deck soon offers it
/// again, marked as a possible repeat (an `ack` of it still settles it).
/// Answers the fetch's reply, each message carrying its attempt.
pub fn fetch(daemon: &impl Endpoint, pane: &Pane, ack: bool) -> Value {
    let (mut socket, reply) = take_granted(daemon, pane);
    if ack {
        for message in reply["messages"].as_array().into_iter().flatten() {
            let mut ack = request(pane, "ack");
            ack["delivery_id"] = message["id"].clone();
            let answer = hook(daemon, ack);
            assert!(
                answer["accepted"] == true
                    && matches!(
                        answer["delivery"].as_str(),
                        Some("acknowledged" | "claimed")
                    ),
                "the fetched {message} is acknowledged: {answer}"
            );
        }
    } else {
        report_handed(daemon, pane, &mut socket, &reply);
    }
    close(socket);
    reply
}

/// [`fetch`] without acknowledging, its receiver kept: each block may have
/// reached the agent, its `handed` receipt is published and reported, and
/// the deck holds the messages for this receiver while its connection stays
/// open (a hook's hold, one ack-wait). Answers the open connection and the
/// fetch's reply.
pub fn take(daemon: &impl Endpoint, pane: &Pane) -> (BufReader<UnixStream>, Value) {
    let (mut socket, reply) = take_granted(daemon, pane);
    report_handed(daemon, pane, &mut socket, &reply);
    (socket, reply)
}

/// A one-shot receipt fetch for `pane`'s agent, granted each block it was
/// handed; its connection open.
fn take_granted(daemon: &impl Endpoint, pane: &Pane) -> (BufReader<UnixStream>, Value) {
    let mut req = request(pane, "inbox_receipt_fetch");
    req["consumer"] = json!("cli");
    req["hook_integration"] = json!(1);
    req["max_chars"] = json!(65536);
    let mut socket = connect(daemon, &req);
    let reply = line(&mut socket);
    assert!(reply["messages"].is_array(), "fetch blocks: {reply}");
    for (q, message) in reply["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let may =
            json!({"may": {"q": q, "id": message["id"], "a": message["a"], "e": message["e"]}});
        writeln!(socket.get_mut(), "{may}").unwrap();
        let grant = line(&mut socket);
        assert_eq!(
            grant["grant"]["ok"], true,
            "the fetch may print {message}: {grant}"
        );
    }
    (socket, reply)
}

/// Publish each block of `reply` as possibly handed to the agent, as an
/// `inbox fetch` whose output may not have arrived whole does, and report it
/// on `socket`.
fn report_handed(
    daemon: &impl Endpoint,
    pane: &Pane,
    socket: &mut BufReader<UnixStream>,
    reply: &Value,
) {
    let messages = reply["messages"].as_array().cloned().unwrap_or_default();
    if messages.is_empty() {
        return;
    }
    let spool = dot_agent_deck::inbox_receipt::GenerationSpool::open(
        &spool_instance(daemon),
        &pane.id,
        &pane.agent,
    )
    .expect("the pane's receipt spool opens");
    let mut handed = Vec::new();
    for message in &messages {
        let cap = dot_agent_deck::inbox_receipt::Cap::from_hex(
            message["caps"]["handed"]
                .as_str()
                .expect("the hand-out's caps"),
        )
        .expect("a cap");
        spool
            .publish(
                &dot_agent_deck::inbox_receipt::RecordKey {
                    msg: message["id"].as_str().unwrap().to_string(),
                    a: message["a"].as_u64().unwrap() as u32,
                    e: message["e"].as_u64().unwrap() as u32,
                    stage: dot_agent_deck::inbox_receipt::RecordStage::Handed,
                },
                &cap,
            )
            .expect("the handed receipt is published");
        handed.push(json!({"id": message["id"], "a": message["a"], "e": message["e"]}));
    }
    writeln!(socket.get_mut(), "{}", json!({"handed": handed})).unwrap();
}

/// End a one-shot receiver: closing our half ends it, and the deck closes its
/// own once it has applied every control written before.
pub fn close(mut socket: BufReader<UnixStream>) {
    socket
        .get_mut()
        .shutdown(std::net::Shutdown::Write)
        .unwrap();
    let mut rest = String::new();
    let _ = socket.read_to_string(&mut rest);
}

/// The receipt spool instance `daemon` runs, the directory a pane's
/// `DOT_AGENT_DECK_INBOX_SPOOL` names: the newest under its state directory,
/// which the harness puts beside its `HOME`.
pub fn spool_instance(daemon: &impl Endpoint) -> std::path::PathBuf {
    let root = daemon
        .home()
        .parent()
        .expect("the harness's work directory")
        .join("state")
        .join(dot_agent_deck::inbox_receipt::SPOOL_DIR_NAME);
    std::fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("the daemon's receipt spool {root:?}: {e}"))
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .max_by_key(|entry| entry.metadata().and_then(|m| m.modified()).ok())
        .map(|entry| entry.path())
        .unwrap_or_else(|| panic!("no receipt spool instance under {root:?}"))
}

/// [`fetch`] whose `inbox fetch` printed nothing: it tells the deck its
/// hand-out positively did not reach the agent (`not_handed`), so the deck
/// queues the messages again at once. Answers the fetch's reply.
pub fn fetch_not_handed(daemon: &impl Endpoint, pane: &Pane) -> Value {
    let mut req = request(pane, "inbox_receipt_fetch");
    req["consumer"] = json!("cli");
    req["hook_integration"] = json!(1);
    req["max_chars"] = json!(65536);
    let mut socket = connect(daemon, &req);
    let reply = line(&mut socket);
    let attempts: Vec<Value> = reply["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|m| json!({"id": m["id"], "a": m["a"], "e": m["e"]}))
        .collect();
    assert!(
        !attempts.is_empty(),
        "the fetch is handed something: {reply}"
    );
    writeln!(socket.get_mut(), "{}", json!({"not_handed": attempts})).unwrap();
    close(socket);
    reply
}

pub fn fixed_line(id: &str) -> String {
    format!("{PREFIX}{id} waiting, run: dot-agent-deck inbox fetch")
}

pub fn bytes(pane: &Pane) -> Vec<u8> {
    std::fs::read(&pane.capture).unwrap_or_default()
}

/// Acknowledge an empty input queue after the daemon's completed write.
pub fn drained(pane: &Pane) {
    let marker = pane.capture.with_extension("drained");
    let _ = std::fs::remove_file(&marker);
    std::fs::write(pane.capture.with_extension("drain"), "").unwrap();
    assert!(
        common::wait_until(wait_budget(), || marker.exists()),
        "recorder must drain completed attempt"
    );
    let _ = wait_budget();
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
    let snapshot = if INTERLEAVE_DEADLINE.with(|cell| cell.get().is_some()) {
        // The general snapshot helper owns a separate ten-second deadline.
        // While parked, every frame must use this fixture's remaining budget.
        let mut socket = UnixStream::connect(daemon.attach_socket()).unwrap();
        let payload = serde_json::to_vec(&json!({"op":"snapshot", "id":pane.agent})).unwrap();
        write_all_budgeted(&mut socket, &[KIND_REQ]).unwrap();
        write_all_budgeted(&mut socket, &(payload.len() as u32).to_be_bytes()).unwrap();
        write_all_budgeted(&mut socket, &payload).unwrap();
        let mut snapshot = Vec::new();
        loop {
            let mut header = [0; 5];
            read_exact_budgeted(&mut socket, &mut header).expect("budgeted snapshot header");
            let len = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
            assert!(len < 1024 * 1024, "fixture snapshot frame exceeds budget");
            let mut payload = vec![0; len];
            read_exact_budgeted(&mut socket, &mut payload).expect("budgeted snapshot payload");
            match header[0] {
                KIND_STREAM_OUT => snapshot.extend_from_slice(&payload),
                KIND_STREAM_END => break,
                _ => {}
            }
        }
        snapshot
    } else {
        common::pane_snapshot_on(daemon.attach_socket(), &pane.agent)
    };
    let mut parser = dot_agent_deck::vt_grid::parser(40, 140, 0);
    parser.process(&snapshot);
    parser.screen().contents()
}

pub fn ring(daemon: &impl Endpoint, pane: &Pane, id: &str) {
    let line = fixed_line(id);
    assert!(
        common::wait_until(wait_budget(), || rings(pane)
            .iter()
            .any(|turn| turn["line"] == line)),
        "doorbell must submit {line:?}; received={:?}; grid={}",
        bytes(pane),
        screen(daemon, pane)
    );
    assert!(
        common::wait_until(wait_budget(), || screen(daemon, pane)
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
    if INTERLEAVE_DEADLINE.with(|cell| cell.get().is_some()) {
        assert!(
            wait_budget() >= duration,
            "negative observation must fit the remaining shared fixture budget"
        );
    }
    let before = bytes(pane);
    assert!(
        !common::wait_until(duration, || bytes(pane) != before),
        "guarded pane received bytes: before={before:?}, after={:?}",
        bytes(pane)
    );
    let _ = wait_budget();
}

pub fn input(daemon: &impl Endpoint, pane: &Pane, text: &str) -> common::AttachInput {
    let mut input = common::AttachInput::attach(daemon.attach_socket(), &pane.agent);
    input.write(text);
    assert!(
        common::wait_until(wait_budget(), || bytes(pane).ends_with(text.as_bytes())),
        "input not received"
    );
    input
}
