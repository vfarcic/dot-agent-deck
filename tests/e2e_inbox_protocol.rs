#![cfg(all(feature = "e2e", unix))]

//! Lane-1 tests of the inbox's public wire and CLI. No inbox Rust types are
//! imported: these tests must compile before the daemon implements the verbs.
//! Message setup uses the design's PostMessage attach request (S3); implementing
//! that enqueue seam early lets S2 run independently of producer migration (S5).

mod common;

use common::DaemonProc;
use dot_agent_deck::daemon_protocol::{KIND_REQ, KIND_RESP};
use serde_json::{Value, json};
use spec::spec;
use std::cell::{Cell, RefCell};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const BODY: &str = "INBOX-S2-SENTINEL-c28f: inspect the fixture";
const WAIT: Duration = Duration::from_secs(10);
const FRAME: &str =
    "This session runs inside dot-agent-deck, the workspace the user runs their coding agents in";

struct Pane {
    id: String,
    agent: String,
    token: String,
    pid: u32,
}

/// The knob that lets this file's synthetic components connect from the test
/// process rather than from inside the pane's agent: these tests drive the
/// inbox wire directly. The binding itself is pinned by inbox/protocol/014,
/// with the knob off.
const UNBOUND_COMPONENTS: (&str, &str) = ("DOT_AGENT_DECK_INBOX_TEST_UNBOUND_COMPONENTS", "1");

fn daemon() -> DaemonProc {
    daemon_with(&[])
}

fn daemon_with(extra: &[(&str, &str)]) -> DaemonProc {
    let mut env = vec![UNBOUND_COMPONENTS];
    env.extend_from_slice(extra);
    common::spawn_daemon_serve_with_env(None, "0", &env)
}

fn attach(daemon: &DaemonProc, request: Value) -> Value {
    let mut socket = UnixStream::connect(&daemon.attach_socket).expect("attach socket");
    socket.set_read_timeout(Some(WAIT)).unwrap();
    socket.set_write_timeout(Some(WAIT)).unwrap();
    let bytes = serde_json::to_vec(&request).unwrap();
    socket.write_all(&[KIND_REQ]).unwrap();
    socket
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .unwrap();
    socket.write_all(&bytes).unwrap();
    let mut header = [0; 5];
    socket.read_exact(&mut header).expect("attach reply header");
    assert_eq!(header[0], KIND_RESP);
    let length = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
    assert!(length < 1024 * 1024, "bounded attach response");
    let mut bytes = vec![0; length];
    socket.read_exact(&mut bytes).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn pane(daemon: &DaemonProc, name: &str, command: &str) -> Pane {
    let command = format!(
        "(umask 077; printf '%s' \"$$\" > \".test-pid-$DOT_AGENT_DECK_AGENT_ID\"); {command}"
    );
    let reply = attach(
        daemon,
        json!({"op":"start-agent", "command":common::capability_export_command(&command),
        "cwd":daemon.home, "rows":24, "cols":100,
        "env":[["DOT_AGENT_DECK_PANE_ID",name]], "display_name":name}),
    );
    assert_eq!(reply["ok"], true, "stand-in spawn: {reply}");
    let agent = reply["id"].as_str().expect("spawned agent id").to_string();
    let token = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(common::recorded_hook_capability(&daemon.home, &agent))
        .expect("stand-in capability");
    let pid_path = daemon.home.join(format!(".test-pid-{agent}"));
    assert!(
        common::wait_until(WAIT, || std::fs::read_to_string(&pid_path)
            .ok()
            .and_then(|text| text.parse::<u32>().ok())
            .is_some()),
        "stand-in pid export"
    );
    let pid = std::fs::read_to_string(pid_path).unwrap().parse().unwrap();
    Pane {
        id: name.into(),
        agent,
        token,
        pid,
    }
}

fn request(pane: &Pane, verb: &str) -> Value {
    json!({"message_type":verb, "pane_id":pane.id, "agent_id":pane.agent, "token":pane.token})
}

fn connect(daemon: &DaemonProc, request: &Value) -> BufReader<UnixStream> {
    let mut socket = UnixStream::connect(&daemon.hook_socket).expect("hook socket");
    socket.set_read_timeout(Some(WAIT)).unwrap();
    socket.set_write_timeout(Some(WAIT)).unwrap();
    writeln!(socket, "{request}").unwrap();
    BufReader::new(socket)
}

fn line(socket: &mut BufReader<UnixStream>) -> Value {
    let mut text = String::new();
    let result = socket.read_line(&mut text);
    assert!(
        matches!(result, Ok(n) if n > 0),
        "inbox verb must answer a JSON line; got {result:?}, line={text:?} (missing verb on pre-S2 daemon)"
    );
    serde_json::from_str(&text).expect("inbox JSON reply")
}

fn hook(daemon: &DaemonProc, request: Value) -> Value {
    line(&mut connect(daemon, &request))
}

fn listing(daemon: &DaemonProc, pane: &Pane) -> Value {
    let mut req = request(pane, "inbox_list");
    req["all"] = json!(false);
    let reply = hook(daemon, req);
    assert!(reply["messages"].is_array(), "InboxList messages: {reply}");
    reply
}

// InboxList needs an `armed` count alongside `messages` for the public
// registration/EOF observation seam requested by S2 (status is S3).
fn armed(daemon: &DaemonProc, pane: &Pane, expected: u64) {
    let latest = RefCell::new(Value::Null);
    assert!(
        common::wait_until(WAIT, || {
            *latest.borrow_mut() = listing(daemon, pane);
            latest.borrow()["armed"].as_u64() == Some(expected)
        }),
        "InboxList armed must become {expected}; last reply: {}",
        latest.borrow()
    );
}

/// A legacy Once waiter, armed from the test process. Claude's waiter is on
/// receipts since PRD #1666 part 2's H3 (inbox/claude/006, inbox/boundary/052);
/// the legacy once path these tests pin stays for any other component until
/// H5 deletes it.
fn waiter(daemon: &DaemonProc, pane: &Pane) -> BufReader<UnixStream> {
    let mut req = request(pane, "inbox_wait");
    req["component"] = json!("other");
    req["component_version"] = json!("synthetic-s2");
    req["mode"] = json!("once");
    req["session"] = json!({"source":"startup", "session_id":"inbox-s2"});
    req["pid"] = json!(pane.pid);
    connect(daemon, &req)
}

fn enqueue(daemon: &DaemonProc, pane: &Pane) -> String {
    let reply = attach(
        daemon,
        json!({"op":"post-message", "pane_id":pane.id,
        "expected_agent_id":pane.agent, "subject":"synthetic S2 task", "body":BODY,
        "origin":"seed"}),
    );
    assert_eq!(
        reply["ok"], true,
        "PostMessage enqueue seam required for S2: {reply}"
    );
    reply["id"]
        .as_str()
        .expect("PostMessage message id")
        .to_string()
}

fn state(daemon: &DaemonProc, pane: &Pane, id: &str, expected: &str) {
    let reply = listing(daemon, pane);
    let message = reply["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["id"] == id)
        .unwrap_or_else(|| panic!("missing message {id}: {reply}"));
    // The state may carry details, but its public spelling remains searchable
    // in the summary (e.g. "queued" or {"acknowledged": {...}}).
    assert!(
        message["state"]
            .to_string()
            .to_ascii_lowercase()
            .contains(expected),
        "message {id} must be {expected}: {message}"
    );
}

fn blocks(reply: &Value, id: &str) {
    let text = reply["messages"].to_string();
    assert!(
        text.contains(&format!("[dot-agent-deck message {id} from "))
            && text.contains(BODY)
            && text.contains(&format!("[end of dot-agent-deck message {id}]")),
        "wait/fetch must return the rendered message block: {reply}"
    );
}

fn cli(daemon: &DaemonProc, pane: Option<&Pane>, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"));
    cmd.env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &daemon.home)
        .env("DOT_AGENT_DECK_SOCKET", &daemon.hook_socket)
        .env("DOT_AGENT_DECK_ATTACH_SOCKET", &daemon.attach_socket)
        .current_dir(&daemon.home)
        .args(args);
    if let Some(pane) = pane {
        cmd.env("DOT_AGENT_DECK_PANE_ID", &pane.id)
            .env("DOT_AGENT_DECK_AGENT_ID", &pane.agent)
            .env("DOT_AGENT_DECK_PANE_CAPABILITY", &pane.token);
    }
    cmd
}

struct BoundedChild(RefCell<Option<Child>>);
impl Drop for BoundedChild {
    fn drop(&mut self) {
        if let Some(child) = self.0.get_mut().as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
impl BoundedChild {
    fn start(cmd: &mut Command, stdin: &[u8]) -> Self {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        if let Some(mut input) = child.stdin.take() {
            let _ = input.write_all(stdin);
        }
        Self(RefCell::new(Some(child)))
    }
    fn finished(&self) -> bool {
        self.0
            .borrow_mut()
            .as_mut()
            .unwrap()
            .try_wait()
            .unwrap()
            .is_some()
    }
    fn output(self) -> Output {
        assert!(
            common::wait_until(WAIT, || self.finished()),
            "inbox CLI must exit within {WAIT:?}"
        );
        self.0
            .borrow_mut()
            .take()
            .unwrap()
            .wait_with_output()
            .unwrap()
    }
}

fn output(cmd: &mut Command) -> Output {
    BoundedChild::start(cmd, b"").output()
}

fn success(result: &Output) {
    assert!(
        result.status.success(),
        "inbox CLI must succeed: status={}, stdout={}, stderr={}",
        result.status,
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

/// Scenario: Start a token-bearing stand-in pane, announce its inbox component and fetch its empty inbox. Repeat fetch, list, announce, wait and ack with the token removed; each must explicitly refuse the caller rather than return messages or acknowledge anything.
#[spec("inbox/protocol/001")]
#[test]
fn inbox_protocol_001_attested_only_verbs() {
    let daemon = daemon();
    let pane = pane(&daemon, "attested", "exec /bin/cat");
    let mut announce = request(&pane, "inbox_announce");
    announce["component"] = json!("claude_waiter");
    announce["component_version"] = json!("synthetic-s2");
    announce["wake"] = json!("native");
    announce["pid"] = json!(pane.pid);
    let reply = hook(&daemon, announce.clone());
    assert_eq!(reply["accepted"], true, "attested announce: {reply}");
    let mut fetch = request(&pane, "inbox_fetch");
    fetch["consumer"] = json!("cli");
    fetch["max_chars"] = json!(9000);
    fetch["ack"] = json!(true);
    assert_eq!(hook(&daemon, fetch.clone())["messages"], json!([]));
    let mut ack = request(&pane, "ack");
    ack["delivery_id"] = json!("m-abcdefgh-0123456789abcdef");
    let mut list = request(&pane, "inbox_list");
    list["all"] = json!(false);
    let mut wait = announce.clone();
    wait["message_type"] = json!("inbox_wait");
    wait["mode"] = json!("once");
    for mut req in [fetch, ack, list, announce, wait] {
        req.as_object_mut().unwrap().remove("token");
        let reply = hook(&daemon, req);
        assert!(
            reply["accepted"] == false || reply.get("refused").is_some(),
            "unattested verb must refuse: {reply}"
        );
        assert!(
            reply.get("messages").is_none(),
            "refusal must not expose messages"
        );
    }
}

/// Scenario: Enqueue a message for one live stand-in and have a different attested stand-in acknowledge its id. The daemon must refuse WrongAgent, leave the recipient's message Queued, and then allow its owner to fetch and acknowledge it.
#[spec("inbox/protocol/002")]
#[test]
fn inbox_protocol_002_wrong_agent_cannot_ack() {
    let daemon = daemon();
    let owner = pane(&daemon, "owner", "exec /bin/cat");
    let other = pane(&daemon, "other", "exec /bin/cat");
    let id = enqueue(&daemon, &owner);
    let mut req = request(&other, "ack");
    req["delivery_id"] = json!(id);
    let reply = hook(&daemon, req);
    assert!(
        reply.to_string().contains("wrong_agent"),
        "wrong-agent ack must explicitly refuse: {reply}"
    );
    state(&daemon, &owner, &id, "queued");
    success(&output(&mut cli(
        &daemon,
        Some(&owner),
        &["inbox", "fetch"],
    )));
    state(&daemon, &owner, &id, "acknowledged");
}

/// Scenario: Start a stand-in with an empty inbox and acknowledge an old delivery id through both the socket and CLI. The socket must answer Unknown and the CLI must say the id is unknown rather than claim it acknowledged a message.
#[spec("inbox/protocol/003")]
#[test]
fn inbox_protocol_003_legacy_delivery_id_is_unknown() {
    let daemon = daemon();
    let pane = pane(&daemon, "legacy", "exec /bin/cat");
    assert_eq!(listing(&daemon, &pane)["messages"], json!([]));
    let mut req = request(&pane, "ack");
    req["delivery_id"] = json!("d-01234567");
    let reply = hook(&daemon, req);
    assert_eq!(reply["delivery"], "unknown", "old d-id: {reply}");
    let result = output(&mut cli(&daemon, Some(&pane), &["ack", "d-01234567"]));
    assert!(
        !result.status.success(),
        "unknown ack must not report success"
    );
    assert!(
        String::from_utf8_lossy(&result.stderr)
            .to_ascii_lowercase()
            .contains("unknown")
    );
}

/// Scenario: Arm two Once waiters on a stand-in and enqueue a message. Only the newest waiter receives its rendered block, the inbox shows Fetched, and a repeat ack from its owner is idempotent.
#[spec("inbox/protocol/004")]
#[test]
fn inbox_protocol_004_once_claims_newest_and_explicit_ack() {
    let daemon = daemon();
    let pane = pane(&daemon, "once", "exec /bin/cat");
    let mut older = waiter(&daemon, &pane);
    armed(&daemon, &pane, 1);
    let mut newer = waiter(&daemon, &pane);
    armed(&daemon, &pane, 2);
    let id = enqueue(&daemon, &pane);
    blocks(&line(&mut newer), &id);
    state(&daemon, &pane, &id, "fetched");
    armed(&daemon, &pane, 1);
    older
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(150)))
        .unwrap();
    let mut duplicate = String::new();
    let result = older.read_line(&mut duplicate);
    assert!(
        matches!(result, Err(ref error) if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)),
        "older Once waiter must receive no duplicate batch: {result:?}, line={duplicate:?}"
    );
    drop(older);
    armed(&daemon, &pane, 0);
    let mut req = request(&pane, "ack");
    req["delivery_id"] = json!(id);
    success(&output(&mut cli(&daemon, Some(&pane), &["ack", &id])));
    state(&daemon, &pane, &id, "acknowledged");
    assert_eq!(hook(&daemon, req)["delivery"], "already_acknowledged");
}

/// Scenario: Arm nine Once waiters for one pane, exceeding its default cap of eight. The oldest gets Superseded and the newest still receives the next message while the public armed count stays bounded.
#[spec("inbox/protocol/005")]
#[test]
fn inbox_protocol_005_waiter_cap_supersedes_oldest() {
    let daemon = daemon();
    let pane = pane(&daemon, "cap", "exec /bin/cat");
    let mut oldest = waiter(&daemon, &pane);
    armed(&daemon, &pane, 1);
    let mut rest = Vec::new();
    for count in 2..=8 {
        rest.push(waiter(&daemon, &pane));
        armed(&daemon, &pane, count);
    }
    let mut newest = waiter(&daemon, &pane);
    assert_eq!(line(&mut oldest)["superseded"], true);
    armed(&daemon, &pane, 8);
    let id = enqueue(&daemon, &pane);
    blocks(&line(&mut newest), &id);
    drop(rest);
    armed(&daemon, &pane, 0);
}

/// Scenario: Run the daemon with a short hook idle timeout and keepalive interval, then leave a waiter idle across several idle windows. It must keep emitting keepalives, remain armed, and receive a later message rather than being reaped.
#[spec("inbox/protocol/006")]
#[test]
fn inbox_protocol_006_waiter_outlives_hook_idle_timeout() {
    let daemon = daemon_with(&[
        ("DOT_AGENT_DECK_HOOK_IDLE_TIMEOUT_MS", "75"),
        ("DOT_AGENT_DECK_INBOX_KEEPALIVE_MS", "100"),
    ]);
    let pane = pane(&daemon, "idle", "exec /bin/cat");
    let mut wait = waiter(&daemon, &pane);
    armed(&daemon, &pane, 1);
    // Control: verify the daemon actually applied the short ordinary timeout.
    // Otherwise an ignored override could make the exemption test vacuous.
    let mut ordinary = UnixStream::connect(&daemon.hook_socket).unwrap();
    ordinary
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut byte = [0];
    assert_eq!(
        ordinary
            .read(&mut byte)
            .expect("ordinary hook peer must expire at the overridden idle timeout"),
        0
    );
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(400) {
        assert_eq!(
            line(&mut wait)["keepalive"],
            true,
            "idle waiter must get keepalives"
        );
    }
    armed(&daemon, &pane, 1);
    let id = enqueue(&daemon, &pane);
    let batch = loop {
        let reply = line(&mut wait);
        if reply["keepalive"] != true {
            break reply;
        }
        assert!(start.elapsed() < WAIT, "bounded wait for batch");
    };
    blocks(&batch, &id);
}

/// Scenario: Open two waiters for a stand-in and close their sockets one at a time without a message. Inbox list must show the armed count fall from two to one to zero, and a subsequent message must remain Queued for a fresh consumer.
#[spec("inbox/protocol/007")]
#[test]
fn inbox_protocol_007_waiter_eof_deregisters() {
    let daemon = daemon();
    let pane = pane(&daemon, "eof", "exec /bin/cat");
    let first = waiter(&daemon, &pane);
    armed(&daemon, &pane, 1);
    let second = waiter(&daemon, &pane);
    armed(&daemon, &pane, 2);
    drop(first);
    armed(&daemon, &pane, 1);
    drop(second);
    armed(&daemon, &pane, 0);
    let id = enqueue(&daemon, &pane);
    state(&daemon, &pane, &id, "queued");
}

/// Scenario: Register a legacy hook-only pane which has never received a capability, then fetch and acknowledge as that pane without a token. These inbox operations must explicitly refuse even though the legacy hook provenance policy admits the pane's status events.
#[spec("inbox/protocol/008")]
#[test]
fn inbox_protocol_008_legacy_unattested_pane_cannot_fetch_or_ack() {
    let daemon = daemon();
    let event = json!({"session_id":"legacy-unattested-s2", "pane_id":"legacy-unattested",
        "agent_id":"legacy-agent", "agent_type":"claude_code", "event_type":"session_start",
        "timestamp":chrono::Utc::now(), "cwd":daemon.home});
    let mut socket = UnixStream::connect(&daemon.hook_socket).unwrap();
    socket.set_read_timeout(Some(WAIT)).unwrap();
    writeln!(socket, "{event}").unwrap();
    socket.shutdown(std::net::Shutdown::Write).unwrap();
    let mut barrier = Vec::new();
    socket
        .read_to_end(&mut barrier)
        .expect("legacy event ingestion barrier");
    for req in [
        json!({"message_type":"inbox_fetch", "pane_id":"legacy-unattested", "agent_id":"legacy-agent",
            "consumer":"cli", "max_chars":9000, "ack":true}),
        json!({"message_type":"ack", "pane_id":"legacy-unattested", "agent_id":"legacy-agent",
            "delivery_id":"m-abcdefgh-0123456789abcdef"}),
    ] {
        let reply = hook(&daemon, req);
        assert!(
            reply["accepted"] == false || reply.get("refused").is_some(),
            "legacy Unattested is insufficient for inbox access: {reply}"
        );
        assert!(reply.get("messages").is_none());
    }
}

/// Scenario: Arm forty waiters across five panes, exceeding the ordinary hook pool's thirty-two connections without exceeding any pane's waiter cap. Inbox list and announce on another connection must still answer, proving armed waiters do not hold hook-ingest slots.
#[spec("inbox/protocol/009")]
#[test]
fn inbox_protocol_009_waiters_do_not_exhaust_hook_ingest_pool() {
    let daemon = daemon();
    let mut waiters = Vec::new();
    let mut panes = Vec::new();
    for index in 0..5 {
        let pane = pane(&daemon, &format!("pool-{index}"), "exec /bin/cat");
        for count in 1..=8 {
            waiters.push(waiter(&daemon, &pane));
            armed(&daemon, &pane, count);
        }
        panes.push(pane);
    }
    let pane = &panes[0];
    let mut announce = request(pane, "inbox_announce");
    announce["component"] = json!("claude_waiter");
    announce["component_version"] = json!("synthetic-s2");
    announce["wake"] = json!("native");
    announce["pid"] = json!(pane.pid);
    assert_eq!(hook(&daemon, announce)["accepted"], true);
    for pane in &panes {
        armed(&daemon, pane, 8);
    }
    drop(waiters);
    for pane in &panes {
        armed(&daemon, pane, 0);
    }
}

fn post(daemon: &DaemonProc, pane: &Pane, body: &str) -> String {
    let reply = attach(
        daemon,
        json!({"op":"post-message", "pane_id":pane.id,
        "expected_agent_id":pane.agent, "subject":"long task", "body":body,
        "origin":"seed"}),
    );
    assert_eq!(reply["ok"], true, "post-message: {reply}");
    reply["id"].as_str().expect("message id").to_string()
}

/// Scenario: Post a task far longer than the tightest agent budget, with its one directive at the very end, then fetch it within Codex's byte budget. The block must name a file the deck saved, owner-only, holding the whole task including the directive, so the part past the inline head is still reachable.
#[spec("inbox/protocol/011")]
#[test]
fn inbox_protocol_011_oversized_post_is_saved_whole_and_named() {
    let daemon = daemon();
    let pane = pane(&daemon, "oversized", "exec /bin/cat");
    let directive = "FINAL-DIRECTIVE-9d41: create the file oversized-sentinel.txt";
    let body = format!(
        "{}\n{directive}\n",
        "background detail line\n".repeat(1_500)
    );
    let id = post(&daemon, &pane, &body);
    let mut fetch = request(&pane, "inbox_fetch");
    fetch["consumer"] = json!("cli");
    fetch["max_chars"] = json!(4_800);
    fetch["max_bytes"] = json!(4_800);
    fetch["ack"] = json!(false);
    let reply = hook(&daemon, fetch);
    let text = reply["messages"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("one block: {reply}"));
    assert!(text.len() <= 4_800, "{} bytes", text.len());
    assert!(
        !text.contains(directive),
        "the directive is past the inline head"
    );
    let named = text
        .lines()
        .find_map(|line| line.strip_prefix("The full message is in "))
        .unwrap_or_else(|| panic!("the block must name where the rest is:\n{text}"));
    let path = std::path::PathBuf::from(named.trim_end_matches('.'));
    let saved = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("the named file {path:?} must be readable: {e}"));
    assert!(
        saved.contains(directive),
        "the saved file holds the whole task"
    );
    use std::os::unix::fs::PermissionsExt as _;
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o077,
        0,
        "owner-only"
    );
    state(&daemon, &pane, &id, "fetched");
}

/// Scenario: Stop a stand-in, start its replacement in the same pane and queue a task for the replacement. The predecessor's still-resolvable token then calls get-seed naming the replacement, and naming nobody; neither call may hand over or acknowledge the replacement's task, which its own token can still fetch.
#[spec("inbox/protocol/012")]
#[test]
fn inbox_protocol_012_get_seed_never_fetches_for_another_generation() {
    let daemon = daemon();
    let old = pane(&daemon, "generations", "exec /bin/cat");
    let reply = attach(&daemon, json!({"op":"stop-agent","id":old.agent}));
    assert_eq!(reply["ok"], true, "stop old agent: {reply}");
    let new = pane(&daemon, "generations", "exec /bin/cat");
    assert_ne!(old.agent, new.agent);
    let id = post(&daemon, &new, BODY);
    for agent in [Some(new.agent.as_str()), None] {
        let mut seed = json!({"message_type":"get_seed", "pane_id":old.id, "token":old.token});
        if let Some(agent) = agent {
            seed["agent_id"] = json!(agent);
        }
        let reply = hook(&daemon, seed);
        assert!(
            reply.get("seed").is_none_or(Value::is_null),
            "a predecessor's get-seed took the replacement's task: {reply}"
        );
        state(&daemon, &new, &id, "queued");
    }
    let mut fetch = request(&new, "inbox_fetch");
    fetch["consumer"] = json!("cli");
    fetch["max_chars"] = json!(9000);
    fetch["ack"] = json!(true);
    blocks(&hook(&daemon, fetch), &id);
}

/// Scenario: Run the daemon with the legacy warn provenance policy, queue a task for a token-bearing stand-in and call get-seed for its pane with no token. The unattested call must get no seed, and the task must stay Queued for its own agent.
#[spec("inbox/protocol/013")]
#[test]
fn inbox_protocol_013_unattested_get_seed_hands_over_no_inbox_message() {
    let daemon = daemon_with(&[("DOT_AGENT_DECK_HOOK_PROVENANCE", "warn")]);
    let pane = pane(&daemon, "warned", "exec /bin/cat");
    let id = post(&daemon, &pane, BODY);
    for agent in [Some(pane.agent.as_str()), None] {
        let mut seed = json!({"message_type":"get_seed", "pane_id":pane.id});
        if let Some(agent) = agent {
            seed["agent_id"] = json!(agent);
        }
        let reply = hook(&daemon, seed);
        assert!(
            reply.get("seed").is_none_or(Value::is_null),
            "an unattested get-seed took a task: {reply}"
        );
        state(&daemon, &pane, &id, "queued");
    }
}

/// Scenario: With the daemon binding inbox components to the pane's agent, a process outside the pane presents the pane's own token and agent id and tries to arm a stream, a once waiter and a Native announcement for it. Each must be refused as not the agent's process, nothing may arm, and the queued task stays Queued.
#[spec("inbox/protocol/014")]
#[test]
fn inbox_protocol_014_a_process_outside_the_agent_cannot_arm_its_inbox() {
    let daemon = common::spawn_daemon_serve(None, "0");
    let pane = pane(&daemon, "bound", "exec /bin/cat");
    let id = post(&daemon, &pane, BODY);
    for mode in ["stream", "once"] {
        let mut req = request(&pane, "inbox_wait");
        req["component"] = json!("pi_extension");
        req["component_version"] = json!("synthetic");
        req["mode"] = json!(mode);
        req["pid"] = json!(pane.pid);
        let reply = line(&mut connect(&daemon, &req));
        assert_eq!(
            reply["refused"], "inbox_not_agent_process",
            "{mode}: {reply}"
        );
        assert!(reply.get("messages").is_none_or(Value::is_null), "{reply}");
    }
    let mut announce = request(&pane, "inbox_announce");
    announce["component"] = json!("claude_hooks");
    announce["component_version"] = json!("synthetic");
    announce["wake"] = json!("native");
    announce["pid"] = json!(pane.pid);
    let reply = hook(&daemon, announce);
    assert_eq!(reply["accepted"], false, "{reply}");
    assert_eq!(reply["reason"], "inbox_not_agent_process", "{reply}");
    armed(&daemon, &pane, 0);
    state(&daemon, &pane, &id, "queued");
}

/// Scenario: With the daemon binding inbox components to the pane's agent, a process outside the pane presents the pane's own token and agent id and tries to fetch, fetch-and-acknowledge, acknowledge and get-seed the agent's queued task. Each is refused and the task stays Queued; the real fetch command run inside the pane then takes and acknowledges it.
#[spec("inbox/protocol/015")]
#[test]
fn inbox_protocol_015_a_process_outside_the_agent_cannot_take_its_mail() {
    let daemon = common::spawn_daemon_serve(None, "0");
    let script = daemon.home.join("fetch.sh");
    std::fs::write(&script, "#!/bin/sh\nwhile [ ! -f fetch-go ]; do sleep 0.05; done\n\"$INBOX_TEST_BIN\" inbox fetch > fetch.stdout 2> fetch.stderr\nprintf '%s' \"$?\" > fetch.exit\nexec /bin/cat\n").unwrap();
    // Quote only fixture-owned paths; the binary path contains no shell code.
    let command = format!(
        "INBOX_TEST_BIN='{}' /bin/sh '{}'",
        env!("CARGO_BIN_EXE_dot-agent-deck").replace('\'', "'\\''"),
        script.display()
    );
    let pane = pane(&daemon, "bound-mail", &command);
    let id = enqueue(&daemon, &pane);
    for ack in [false, true] {
        let mut fetch = request(&pane, "inbox_fetch");
        fetch["consumer"] = json!("cli");
        fetch["ack"] = json!(ack);
        let reply = hook(&daemon, fetch);
        assert_eq!(
            reply["refused"], "inbox_not_agent_process",
            "fetch ack={ack}: {reply}"
        );
        assert!(reply.get("messages").is_none_or(Value::is_null), "{reply}");
    }
    let mut ack = request(&pane, "ack");
    ack["delivery_id"] = json!(id);
    let reply = hook(&daemon, ack);
    assert_eq!(reply["accepted"], false, "ack: {reply}");
    assert_eq!(reply["reason"], "inbox_not_agent_process", "ack: {reply}");
    let reply = hook(&daemon, request(&pane, "get_seed"));
    assert!(reply["seed"].is_null(), "get_seed: {reply}");
    state(&daemon, &pane, &id, "queued");
    std::fs::write(daemon.home.join("fetch-go"), b"go").unwrap();
    assert!(
        common::wait_until(WAIT, || std::fs::read_to_string(
            daemon.home.join("fetch.exit")
        )
        .is_ok_and(|text| !text.is_empty())),
        "the in-pane fetch must complete"
    );
    let stderr = std::fs::read_to_string(daemon.home.join("fetch.stderr")).unwrap();
    assert_eq!(
        std::fs::read_to_string(daemon.home.join("fetch.exit")).unwrap(),
        "0",
        "in-pane fetch stderr: {stderr}"
    );
    let text = std::fs::read_to_string(daemon.home.join("fetch.stdout")).unwrap();
    assert!(text.contains(BODY), "in-pane fetch output: {text}");
    state(&daemon, &pane, &id, "acknowledged");
}

/// Scenario: With the daemon binding inbox components to the pane's agent, a process outside the pane sends a raw hook event with the pane's own token that names the agent's queued task as a prompt it observed. The task must stay Queued; the real Claude waiter, run inside the pane, is then handed the task, and the real Claude hook, run inside the pane for a prompt carrying the block the waiter wrote, commits it.
#[spec("inbox/protocol/016")]
#[test]
fn inbox_protocol_016_a_raw_event_from_outside_the_agent_acknowledges_nothing() {
    let daemon = common::spawn_daemon_serve(None, "0");
    let script = daemon.home.join("hook.sh");
    std::fs::write(&script, "#!/bin/sh\nwhile [ ! -f wait-go ]; do sleep 0.05; done\n\"$INBOX_TEST_BIN\" inbox wait --agent claude-code < /dev/null > wait.stdout 2> wait.stderr\nprintf '%s' \"$?\" > wait.exit\nwhile [ ! -f hook-go ]; do sleep 0.05; done\n\"$INBOX_TEST_BIN\" hook --agent claude-code < prompt.json > hook.stdout 2> hook.stderr\nprintf '%s' \"$?\" > hook.exit\nexec /bin/cat\n").unwrap();
    // Quote only fixture-owned paths; the binary path contains no shell code.
    // `exec` makes the script the agent's own process: a shell under the
    // launch shell would read as a nested agent of the same kind, and the
    // receiver check refuses the waiter it runs.
    let command = format!(
        "exec env INBOX_TEST_BIN='{}' /bin/sh '{}'",
        env!("CARGO_BIN_EXE_dot-agent-deck").replace('\'', "'\\''"),
        script.display()
    );
    let pane = pane(&daemon, "bound-event", &command);
    let id = enqueue(&daemon, &pane);
    let event = json!({"session_id":"outside", "agent_type":"claude_code",
        "event_type":"thinking", "timestamp":chrono::Utc::now(), "pane_id":pane.id,
        "agent_id":pane.agent, "token":pane.token,
        "metadata":{"deck_message_ids":id, "deck_message_ack_ids":id}});
    // One connection reads its lines in order, so the listing that follows the
    // event on the same connection answers after the event was ingested.
    let mut socket = connect(&daemon, &event);
    let mut list = request(&pane, "inbox_list");
    list["all"] = json!(false);
    writeln!(socket.get_mut(), "{list}").unwrap();
    let after_event = line(&mut socket);
    assert!(after_event["messages"].is_array(), "{after_event}");
    state(&daemon, &pane, &id, "queued");
    std::fs::write(daemon.home.join("wait-go"), b"go").unwrap();
    assert!(
        common::wait_until(WAIT, || std::fs::read_to_string(
            daemon.home.join("wait.exit")
        )
        .is_ok_and(|text| !text.is_empty())),
        "the in-pane waiter must hand the task over"
    );
    assert_eq!(
        std::fs::read_to_string(daemon.home.join("wait.exit")).unwrap(),
        "2"
    );
    let block = std::fs::read_to_string(daemon.home.join("wait.stderr")).unwrap();
    assert!(
        block.contains(&format!("[dot-agent-deck message {id} delivery 1.1 from ")),
        "{block}"
    );
    let prompt = format!("<task-notification>{block}</task-notification>");
    std::fs::write(
        daemon.home.join("prompt.json"),
        json!({"session_id":"in-pane", "hook_event_name":"UserPromptSubmit",
            "prompt":prompt, "cwd":daemon.home})
        .to_string(),
    )
    .unwrap();
    std::fs::write(daemon.home.join("hook-go"), b"go").unwrap();
    assert!(
        common::wait_until(WAIT, || std::fs::read_to_string(
            daemon.home.join("hook.exit")
        )
        .is_ok_and(|text| !text.is_empty())),
        "the in-pane hook must complete"
    );
    let stderr = std::fs::read_to_string(daemon.home.join("hook.stderr")).unwrap();
    assert_eq!(
        std::fs::read_to_string(daemon.home.join("hook.exit")).unwrap(),
        "0",
        "in-pane hook stderr: {stderr}"
    );
    let latest = RefCell::new(Value::Null);
    assert!(
        common::wait_until(WAIT, || {
            *latest.borrow_mut() = listing(&daemon, &pane);
            latest.borrow()["messages"]
                .to_string()
                .to_ascii_lowercase()
                .contains("acknowledged")
        }),
        "the in-pane hook must commit the task it was handed: {}",
        latest.borrow()
    );
    state(&daemon, &pane, &id, "acknowledged");
}

/// A hook-socket connection made by a process that has exited and been reaped
/// by the time the daemon reads it: a child of this test process connects a
/// socket this process created, then exits, and this process keeps using it.
/// The kernel still reports the child as the peer, and its ancestry can no
/// longer be read. Answers the connection and the exited child's pid.
fn connect_from_an_exited_process(path: &Path) -> (BufReader<UnixStream>, u32) {
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: plain syscalls on values this function owns. The forked child
    // calls only `connect` and `_exit`, both async-signal-safe.
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        assert!(fd >= 0, "socket: {}", std::io::Error::last_os_error());
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let bytes = path.as_os_str().as_bytes();
        assert!(bytes.len() < addr.sun_path.len(), "socket path too long");
        for (dst, src) in addr.sun_path.iter_mut().zip(bytes) {
            *dst = *src as libc::c_char;
        }
        let len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
        let child = libc::fork();
        if child == 0 {
            let rc = libc::connect(fd, &addr as *const _ as *const libc::sockaddr, len);
            libc::_exit(if rc == 0 { 0 } else { 1 });
        }
        assert!(child > 0, "fork: {}", std::io::Error::last_os_error());
        let mut status = 0;
        assert_eq!(libc::waitpid(child, &mut status, 0), child);
        assert!(
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
            "the child's connect failed"
        );
        let socket = UnixStream::from_raw_fd(fd);
        socket.set_read_timeout(Some(WAIT)).unwrap();
        socket.set_write_timeout(Some(WAIT)).unwrap();
        (BufReader::new(socket), child as u32)
    }
}

/// Scenario: With the daemon binding inbox components to the pane's agent, a connection whose creator exited before it is used presents the pane's own token and tries to fetch, acknowledge, arm a waiter for and get-seed the agent's queued task, and to announce a native wake naming that exited creator. The daemon can no longer read who made the connection, so each is refused and the task stays Queued; the real receipt watcher then run inside the pane still registers and is handed the task.
#[spec("inbox/protocol/017")]
#[test]
fn inbox_protocol_017_a_connection_of_unknown_ancestry_cannot_take_mail() {
    let daemon = common::spawn_daemon_serve(None, "0");
    let script = daemon.home.join("wait.sh");
    std::fs::write(&script, "#!/bin/sh\nwhile [ ! -f wait-go ]; do sleep 0.05; done\n\"$INBOX_TEST_BIN\" inbox wait --agent pi --stream --adapter-integration 2 > wait.stdout 2> wait.stderr\nexec /bin/cat\n").unwrap();
    // Quote only fixture-owned paths; the binary path contains no shell code.
    // `exec` makes the script's shell the pane's agent process, as an agent's
    // own binary is: a shell under the launch shell would read as a nested
    // agent of the same kind, and the receiver check refuses its waiter.
    let command = format!(
        "exec env INBOX_TEST_BIN='{}' /bin/sh '{}'",
        env!("CARGO_BIN_EXE_dot-agent-deck").replace('\'', "'\\''"),
        script.display()
    );
    let pane = pane(&daemon, "bound-unknown", &command);
    let id = enqueue(&daemon, &pane);
    // A failure names the verb, the endpoint and the raw reply, so a daemon
    // that drops the connection or answers something malformed says which
    // request it was (Qodo, PR #1735).
    let send = |request: &Value| -> Value {
        let verb = request["message_type"].as_str().unwrap_or("?").to_string();
        let endpoint = daemon.hook_socket.display();
        let (mut socket, _) = connect_from_an_exited_process(&daemon.hook_socket);
        writeln!(socket.get_mut(), "{request}").unwrap_or_else(|error| {
            panic!("{verb}: could not write the request to {endpoint}: {error}")
        });
        let mut raw = String::new();
        let read = socket.read_line(&mut raw);
        assert!(
            matches!(read, Ok(n) if n > 0),
            "{verb}: {endpoint} sent no reply line ({read:?}); raw reply: {raw:?}"
        );
        serde_json::from_str(&raw).unwrap_or_else(|error| {
            panic!("{verb}: {endpoint} answered with something other than JSON ({error}): {raw:?}")
        })
    };
    let mut fetch = request(&pane, "inbox_fetch");
    fetch["consumer"] = json!("cli");
    fetch["ack"] = json!(true);
    let reply = send(&fetch);
    assert_eq!(
        reply["refused"], "inbox_not_agent_process",
        "fetch: {reply}"
    );
    assert!(reply.get("messages").is_none_or(Value::is_null), "{reply}");
    let mut ack = request(&pane, "ack");
    ack["delivery_id"] = json!(id);
    let reply = send(&ack);
    assert_eq!(reply["reason"], "inbox_not_agent_process", "ack: {reply}");
    let mut wait = request(&pane, "inbox_wait");
    wait["component"] = json!("pi_extension");
    wait["component_version"] = json!("synthetic");
    wait["mode"] = json!("once");
    let reply = send(&wait);
    assert_eq!(reply["refused"], "inbox_not_agent_process", "wait: {reply}");
    let reply = send(&request(&pane, "get_seed"));
    assert!(reply["seed"].is_null(), "get_seed: {reply}");
    // PR #1735 third review (F1): an announce naming the exited creator would
    // pin that dead process as the agent's receiver, and lock the real one out.
    let (mut socket, creator) = connect_from_an_exited_process(&daemon.hook_socket);
    let mut announce = request(&pane, "inbox_announce");
    announce["component"] = json!("pi_extension");
    announce["component_version"] = json!("synthetic");
    announce["wake"] = json!("native");
    announce["pid"] = json!(creator);
    writeln!(socket.get_mut(), "{announce}").unwrap();
    let reply = line(&mut socket);
    assert_eq!(reply["accepted"], false, "announce: {reply}");
    assert_eq!(
        reply["reason"], "inbox_not_agent_process",
        "announce: {reply}"
    );
    state(&daemon, &pane, &id, "queued");
    std::fs::write(daemon.home.join("wait-go"), b"go").unwrap();
    let stderr = || std::fs::read_to_string(daemon.home.join("wait.stderr")).unwrap_or_default();
    let stdout = || std::fs::read_to_string(daemon.home.join("wait.stdout")).unwrap_or_default();
    assert!(
        common::wait_until(WAIT, || stdout().contains(&id)),
        "the in-pane receipt watcher must register and be handed the task; its stdout: {}, its \
         stderr: {}",
        stdout(),
        stderr()
    );
}

/// Scenario: Fill a deck that may hold one message, then have an agent pane ask it to dispatch a unit. The deck has no room for the dispatch's result, so it refuses the dispatch with deck_full instead of accepting it, and no unit is started.
#[spec("inbox/protocol/018")]
#[test]
fn inbox_protocol_018_a_dispatch_the_deck_has_no_room_to_answer_is_refused() {
    let daemon = daemon_with(&[("DOT_AGENT_DECK_INBOX_MAX_MESSAGES", "1")]);
    let pane = pane(&daemon, "dispatcher", "exec /bin/cat");
    enqueue(&daemon, &pane);
    let mut dispatch = request(&pane, "dispatch");
    dispatch["name"] = json!("no-room-unit");
    dispatch["task"] = json!("must not start");
    dispatch["timestamp"] = json!(chrono::Utc::now());
    let reply = hook(&daemon, dispatch);
    assert_eq!(reply["accepted"], false, "{reply}");
    assert_eq!(reply["reason"], "deck_full", "{reply}");
    let agents = attach(&daemon, json!({"op":"list-agents"}));
    assert!(
        !agents.to_string().contains("no-room-unit"),
        "nothing may be dispatched: {agents}"
    );
}

/// Scenario: Enqueue a task for a scripted agent pane and release it to run the real inbox fetch command using its injected identity. Its captured output must contain the session framing and complete message block; both the wire listing and inbox list CLI must show Acknowledged.
#[spec("inbox/cli/001")]
#[test]
fn inbox_cli_001_in_pane_fetch_frames_and_acknowledges() {
    let daemon = daemon();
    let script = daemon.home.join("fetch.sh");
    std::fs::write(&script, "#!/bin/sh\nwhile [ ! -f fetch-go ]; do sleep 0.05; done\n\"$INBOX_TEST_BIN\" inbox fetch > fetch.stdout 2> fetch.stderr\nprintf '%s' \"$?\" > fetch.exit\nexec /bin/cat\n").unwrap();
    // Quote only fixture-owned paths; the binary path contains no shell code.
    let command = format!(
        "INBOX_TEST_BIN='{}' /bin/sh '{}'",
        env!("CARGO_BIN_EXE_dot-agent-deck").replace('\'', "'\\''"),
        script.display()
    );
    let pane = pane(&daemon, "fetch", &command);
    let id = enqueue(&daemon, &pane);
    state(&daemon, &pane, &id, "queued");
    let before = output(&mut cli(&daemon, Some(&pane), &["inbox", "list"]));
    success(&before);
    let text = String::from_utf8_lossy(&before.stdout);
    assert!(
        text.contains(&id) && text.to_ascii_lowercase().contains("queued"),
        "list before fetch: {text}"
    );
    std::fs::write(daemon.home.join("fetch-go"), b"go").unwrap();
    assert!(
        common::wait_until(WAIT, || std::fs::read_to_string(
            daemon.home.join("fetch.exit")
        )
        .is_ok_and(|text| !text.is_empty())),
        "stand-in fetch must complete"
    );
    let stderr = std::fs::read_to_string(daemon.home.join("fetch.stderr")).unwrap();
    assert_eq!(
        std::fs::read_to_string(daemon.home.join("fetch.exit")).unwrap(),
        "0",
        "fetch stderr: {stderr}"
    );
    let text = std::fs::read_to_string(daemon.home.join("fetch.stdout")).unwrap();
    assert!(
        text.starts_with(FRAME)
            && text.contains(BODY)
            && text.contains(&format!("[dot-agent-deck message {id} from "))
            && text.contains(&format!("[end of dot-agent-deck message {id}]")),
        "framed CLI output: {text}"
    );
    state(&daemon, &pane, &id, "acknowledged");
    let after = output(&mut cli(&daemon, Some(&pane), &["inbox", "list"]));
    success(&after);
    let text = String::from_utf8_lossy(&after.stdout);
    assert!(
        text.contains(&id) && text.to_ascii_lowercase().contains("acknowledged"),
        "list after fetch: {text}"
    );
}

/// Scenario: Run the Claude Once CLI with hook JSON against a live daemon, wait until it is armed, and enqueue a task. The command must exit two with a rendered block on stderr and no stdout; fetching via a waiter leaves the message Fetched until an explicit acknowledgement.
#[spec("inbox/cli/002")]
#[test]
fn inbox_cli_002_claude_waiter_exits_two_with_stderr() {
    let daemon = daemon();
    let pane = pane(&daemon, "claude-wait", "exec /bin/cat");
    let child = BoundedChild::start(
        &mut cli(
            &daemon,
            Some(&pane),
            &["inbox", "wait", "--agent", "claude-code"],
        ),
        br#"{"session_id":"s2-cli", "source":"startup", "hook_event_name":"SessionStart"}"#,
    );
    assert!(common::wait_until(WAIT, || child.finished()
        || listing(&daemon, &pane)["armed"] == 1));
    assert!(
        !child.finished(),
        "Claude waiter must stay armed: {:?}",
        child.output()
    );
    let id = enqueue(&daemon, &pane);
    let result = child.output();
    assert_eq!(
        result.status.code(),
        Some(2),
        "Claude wake stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(result.stdout.is_empty());
    let text = String::from_utf8_lossy(&result.stderr);
    assert!(
        text.contains(BODY)
            && text.contains(&format!("[dot-agent-deck message {id} delivery 1.1 from "))
            && text.contains(&format!("[end of dot-agent-deck message {id}]")),
        "waiter stderr: {text}"
    );
    state(&daemon, &pane, &id, "fetched");
}

/// Scenario: Run the Claude waiter outside a deck and then with a pane identity pointing at an absent daemon. Both invocations must exit zero without stdout or stderr, so global hooks do not wake an unrelated Claude session.
#[spec("inbox/cli/003")]
#[test]
fn inbox_cli_003_waiter_without_deck_or_daemon_is_quiet() {
    let daemon = daemon();
    let hook_json =
        br#"{"session_id":"outside-s2", "source":"startup", "hook_event_name":"SessionStart"}"#;
    let result = BoundedChild::start(
        &mut cli(&daemon, None, &["inbox", "wait", "--agent", "claude-code"]),
        hook_json,
    )
    .output();
    success(&result);
    assert!(result.stdout.is_empty() && result.stderr.is_empty());
    let pane = pane(&daemon, "no-daemon", "exec /bin/cat");
    let result = BoundedChild::start(
        cli(
            &daemon,
            Some(&pane),
            &["inbox", "wait", "--agent", "claude-code"],
        )
        .env("DOT_AGENT_DECK_SOCKET", daemon.home.join("absent.sock")),
        hook_json,
    )
    .output();
    success(&result);
    assert!(result.stdout.is_empty() && result.stderr.is_empty());
}

fn mock_waiter_reply(daemon: &DaemonProc, pane: &Pane, path: &Path, reply: Option<&str>) -> Output {
    let listener = UnixListener::bind(path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let child = BoundedChild::start(
        cli(
            daemon,
            Some(pane),
            &["inbox", "wait", "--agent", "claude-code"],
        )
        .env("DOT_AGENT_DECK_SOCKET", path),
        br#"{"session_id":"s2-mock", "source":"clear", "hook_event_name":"SessionStart"}"#,
    );
    let accepted = RefCell::new(None);
    assert!(common::wait_until(WAIT, || {
        if let Ok((socket, _)) = listener.accept() {
            *accepted.borrow_mut() = Some(socket);
        }
        accepted.borrow().is_some() || child.finished()
    }));
    let Some(socket) = accepted.into_inner() else {
        return child.output();
    };
    socket.set_read_timeout(Some(WAIT)).unwrap();
    let mut socket = BufReader::new(socket);
    // The waiter asks whether the deck serves receipts, and waits on the
    // receipt verb once it says so, on the same connection.
    let question = line(&mut socket);
    assert_eq!(question["message_type"], "inbox_receipts");
    writeln!(socket.get_mut(), r#"{{"one_shot_receipts":1}}"#).unwrap();
    let req = line(&mut socket);
    assert_eq!(req["message_type"], "inbox_receipt_wait");
    assert_eq!(req["mode"], "once");
    assert_eq!(req["session"]["source"], "clear");
    if let Some(reply) = reply {
        writeln!(socket.get_mut(), "{reply}").unwrap();
    }
    drop(socket);
    child.output()
}

/// Scenario: Point the real Claude waiter CLI at a synthetic hook endpoint that says it serves receipts and then answers the wait with Superseded, NotInDeck or Refused, returns malformed JSON, or closes without a reply. Each case must exit zero with no message on either output stream. A deck that does not say it serves receipts is inbox/claude/028.
#[spec("inbox/cli/004")]
#[test]
fn inbox_cli_004_non_delivery_replies_never_wake() {
    let daemon = daemon();
    let pane = pane(&daemon, "quiet", "exec /bin/cat");
    for (index, reply) in [
        Some(r#"{"superseded":true}"#),
        Some(r#"{"not_in_deck":true}"#),
        Some(r#"{"refused":"synthetic refusal"}"#),
        Some("not JSON"),
        None,
    ]
    .into_iter()
    .enumerate()
    {
        let result = mock_waiter_reply(
            &daemon,
            &pane,
            &daemon.home.join(format!("mock-{index}.sock")),
            reply,
        );
        success(&result);
        assert!(
            result.stdout.is_empty() && result.stderr.is_empty(),
            "non-delivery {reply:?} must stay quiet"
        );
    }
}

/// Run `cmd` against a synthetic hook endpoint at `path` and record every
/// request line it sends, answering each with `reply` (no answer for `None`).
fn record_requests(
    cmd: &mut Command,
    stdin: &[u8],
    path: &Path,
    reply: impl Fn(&Value) -> Option<Value>,
) -> (Output, Vec<Value>) {
    let listener = UnixListener::bind(path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let child = BoundedChild::start(cmd.env("DOT_AGENT_DECK_SOCKET", path), stdin);
    let requests = RefCell::new(Vec::new());
    let serve = |socket: UnixStream| {
        socket.set_nonblocking(false).unwrap();
        socket.set_read_timeout(Some(WAIT)).unwrap();
        let mut socket = BufReader::new(socket);
        loop {
            let mut text = String::new();
            match socket.read_line(&mut text) {
                Ok(n) if n > 0 => {}
                _ => break,
            }
            let request: Value = serde_json::from_str(&text).expect("a JSON request line");
            if let Some(answer) = reply(&request) {
                let _ = writeln!(socket.get_mut(), "{answer}");
            }
            requests.borrow_mut().push(request);
        }
    };
    assert!(common::wait_until(WAIT, || {
        while let Ok((socket, _)) = listener.accept() {
            serve(socket);
        }
        child.finished()
    }));
    while let Ok((socket, _)) = listener.accept() {
        serve(socket);
    }
    (child.output(), requests.into_inner())
}

/// Scenario: Point the doorbell fetch an agent-side component runs (`inbox fetch --json --doorbell <id>`), and the Claude hook a doorbell-started prompt runs, at a synthetic hook endpoint that lists the named message as queued. Each must ask the deck for the fetch in one request that names the doorbell's message, never list the inbox and then fetch without that restriction.
#[spec("inbox/cli/005")]
#[test]
fn inbox_cli_005_a_doorbell_fetch_names_its_message_in_the_one_request() {
    let daemon = daemon();
    let pane = pane(&daemon, "doorbell-bound", "exec /bin/cat");
    let id = "m-k3xzq2ab-0f3c9a71d2e4b850";
    let block = format!(
        "[dot-agent-deck message {id} from the orchestrator]\nwork\n[end of dot-agent-deck message {id}]"
    );
    // Claude's prompt hook asks whether the deck serves receipts, then
    // fetches as a one-shot receiver on the receipt verb: its batch names
    // each attempt and carries its caps, and its `may` is answered.
    let reply = |request: &Value| match request["message_type"].as_str() {
        Some("inbox_receipts") => Some(json!({"one_shot_receipts":1})),
        Some("inbox_receipt_fetch") => Some(json!({
            "registered":{"rx":1, "slot":"hook", "keepalive_ms":1000, "grant_ttl_ms":5000},
            "messages":[{"id":id, "subject":"task", "from":"the orchestrator", "text":block,
                "a":1, "e":1, "d":1, "caps":{"handed":"00000000000000000000000000000000",
                "commit":"11111111111111111111111111111111"}}]})),
        Some("inbox_fetch") => Some(json!({"messages":[{"id":id, "subject":"task",
            "from":"the orchestrator", "text":block}]})),
        None if request["may"].is_object() => {
            Some(json!({"grant":{"q":request["may"]["q"], "ok":true}}))
        }
        Some("inbox_list") => Some(json!({"messages":[{"id":id, "state":"queued",
            "subject":"task"}], "armed":0})),
        Some("ack") => Some(json!({"kind":"signal_ack", "accepted":true,
            "delivery":"acknowledged"})),
        _ => None,
    };
    let (output, requests) = record_requests(
        &mut cli(
            &daemon,
            Some(&pane),
            &["inbox", "fetch", "--json", "--doorbell", id],
        ),
        b"",
        &daemon.home.join("doorbell-cli.sock"),
        reply,
    );
    success(&output);
    assert_eq!(requests.len(), 1, "one request: {requests:?}");
    assert_eq!(requests[0]["message_type"], "inbox_fetch", "{requests:?}");
    assert_eq!(requests[0]["doorbell"], id, "{requests:?}");
    assert_eq!(requests[0]["ack"], false, "{requests:?}");
    let line = format!(" dot-agent-deck: message {id} waiting, run: dot-agent-deck inbox fetch");
    let prompt = json!({"session_id":"doorbell-hook", "hook_event_name":"UserPromptSubmit",
        "prompt":line, "cwd":daemon.home})
    .to_string();
    let (output, requests) = record_requests(
        &mut cli(&daemon, Some(&pane), &["hook", "--agent", "claude-code"]),
        prompt.as_bytes(),
        &daemon.home.join("doorbell-hook.sock"),
        reply,
    );
    success(&output);
    let fetches: Vec<&Value> = requests
        .iter()
        .filter(|r| r["message_type"] == "inbox_receipt_fetch")
        .collect();
    assert_eq!(fetches.len(), 1, "{requests:?}");
    assert!(
        !requests.iter().any(|r| r["message_type"] == "inbox_fetch"),
        "a one-shot never fetches on the legacy verb: {requests:?}"
    );
    assert_eq!(fetches[0]["doorbell"], id, "{requests:?}");
    assert_eq!(fetches[0]["hook_integration"], 1, "{requests:?}");
    assert!(
        !requests.iter().any(|r| r["message_type"] == "inbox_list"),
        "{requests:?}"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("work"),
        "the hook hands the fetched block over: {:?}",
        output
    );
}

/// The hook-socket verbs that could hand mail over or commit it.
const MAIL_VERBS: &[&str] = &[
    "inbox_wait",
    "inbox_fetch",
    "inbox_receipt_wait",
    "inbox_receipt_fetch",
    "inbox_committed",
    "ack",
];

/// Scenario: Point Claude Code's waiter, its prompt hook answering a doorbell, and its prompt hook submitting a waiter's whole block at a stand-in for a 0.47.0 daemon, which hands its one queued message to the first legacy `inbox_wait` or `inbox_fetch` and answers nothing to a verb it cannot decode. Each asks whether the deck serves receipts, sends nothing that could take or commit mail, exits 1 and says on stderr that the deck did not answer in time, that an older daemon does not answer, and names `hooks install` (the committing hook in its own words, which promise nothing queued); the message is still there for a legacy `inbox fetch` afterwards.
#[spec("inbox/claude/028")]
#[test]
fn claude_028_newer_hooks_take_nothing_from_an_older_daemon() {
    let daemon = daemon();
    let pane = pane(&daemon, "older-daemon", "exec /bin/cat");
    let id = "m-k3xzq2ab-0f3c9a71d2e4b850";
    let body = "older-daemon body 5e1d";
    let block = format!(
        "[dot-agent-deck message {id} from the orchestrator]\n{body}\n[end of dot-agent-deck message {id}]"
    );
    let taken = Cell::new(false);
    let older = |request: &Value| match request["message_type"].as_str() {
        Some("inbox_wait" | "inbox_fetch") if !taken.replace(true) => Some(json!({"messages":[
            {"id":id, "subject":"task", "from":"the orchestrator", "text":block}]})),
        Some("inbox_wait" | "inbox_fetch") => Some(json!({"messages":[]})),
        Some("ack") => Some(json!({"kind":"signal_ack", "accepted":true,
            "delivery":"acknowledged"})),
        // `inbox_receipts`, the receipt verbs, `inbox_committed`: a 0.47.0
        // daemon decodes none of them, and answers nothing.
        _ => None,
    };
    let refused = |what: &str, notice: &str, output: &Output, requests: &[Value]| {
        assert_eq!(output.status.code(), Some(1), "{what}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(notice) && stderr.contains("hooks install --agent claude-code"),
            "{what} says why and how: {stderr}"
        );
        assert!(
            !stderr.contains(dot_agent_deck::inbox_oneshot::RECEIPTS_UNSUPPORTED),
            "{what} got no answer, so it does not say the deck is most likely older: {stderr}"
        );
        assert!(
            requests
                .iter()
                .any(|r| r["message_type"] == "inbox_receipts"),
            "{what} asks: {requests:?}"
        );
        for request in requests {
            let verb = request["message_type"].as_str().unwrap_or_default();
            assert!(
                !MAIL_VERBS.contains(&verb),
                "{what} sent {verb}: {requests:?}"
            );
        }
        assert!(!taken.get(), "{what} took the message");
        assert!(
            !String::from_utf8_lossy(&output.stdout).contains(body),
            "{what} handed nothing over"
        );
    };
    let quick = [("DOT_AGENT_DECK_INBOX_TEST_RECEIPTS_WAIT_MS", "300")];

    let session = json!({"hook_event_name":"SessionStart", "session_id":"older-daemon",
        "source":"startup"})
    .to_string();
    let (output, requests) = record_requests(
        cli(
            &daemon,
            Some(&pane),
            &["inbox", "wait", "--agent", "claude-code"],
        )
        .envs(quick),
        session.as_bytes(),
        &daemon.home.join("older-wait.sock"),
        older,
    );
    refused(
        "the waiter",
        dot_agent_deck::inbox_oneshot::RECEIPTS_TIMED_OUT,
        &output,
        &requests,
    );

    let doorbell = json!({"session_id":"older-daemon", "hook_event_name":"UserPromptSubmit",
        "prompt":format!(" dot-agent-deck: message {id} waiting, run: dot-agent-deck inbox fetch"),
        "cwd":daemon.home})
    .to_string();
    let (output, requests) = record_requests(
        cli(&daemon, Some(&pane), &["hook", "--agent", "claude-code"]).envs(quick),
        doorbell.as_bytes(),
        &daemon.home.join("older-doorbell.sock"),
        older,
    );
    refused(
        "the doorbell hook",
        dot_agent_deck::inbox_oneshot::RECEIPTS_TIMED_OUT,
        &output,
        &requests,
    );

    let notification = json!({"session_id":"older-daemon", "hook_event_name":"UserPromptSubmit",
        "prompt":format!("[dot-agent-deck message {id} delivery 1.1 from the orchestrator]\n{body}\n[end of dot-agent-deck message {id}]"),
        "cwd":daemon.home})
    .to_string();
    let (output, requests) = record_requests(
        cli(&daemon, Some(&pane), &["hook", "--agent", "claude-code"]).envs(quick),
        notification.as_bytes(),
        &daemon.home.join("older-commit.sock"),
        older,
    );
    refused(
        "the committing prompt hook",
        dot_agent_deck::inbox_oneshot::RECEIPTS_COMMIT_TIMED_OUT,
        &output,
        &requests,
    );

    let (output, _) = record_requests(
        &mut cli(&daemon, Some(&pane), &["inbox", "fetch"]),
        b"",
        &daemon.home.join("older-fetch.sock"),
        older,
    );
    success(&output);
    assert!(taken.get(), "a legacy fetch still takes it");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(body),
        "and prints it: {output:?}"
    );
}

/// Scenario: Submit one prompt to Claude Code's prompt hook that carries both a waiter's whole block and a doorbell for a second message, against a stand-in for a 0.47.0 daemon that answers nothing it cannot decode. The hook asks whether the deck serves receipts once, not once for the commit and again for the doorbell's fetch, sends nothing that could take or commit mail, and exits 1 saying, once, that the deck did not answer in time, in the commit's words, which promise nothing queued, and not the fetch's.
#[spec("inbox/claude/037")]
#[test]
fn claude_037_a_prompt_asks_an_older_daemon_once() {
    let daemon = daemon();
    let pane = pane(&daemon, "asks-once", "exec /bin/cat");
    let (id, rung) = ("m-k3xzq2ab-0f3c9a71d2e4b850", "m-k3xzq2ab-1a2b3c4d5e6f7081");
    let older = |request: &Value| match request["message_type"].as_str() {
        Some("inbox_wait" | "inbox_fetch") => Some(json!({"messages":[]})),
        _ => None,
    };
    let prompt = json!({"session_id":"asks-once", "hook_event_name":"UserPromptSubmit",
        "prompt":format!("[dot-agent-deck message {id} delivery 1.1 from the orchestrator]\nbody\n[end of dot-agent-deck message {id}]\n dot-agent-deck: message {rung} waiting, run: dot-agent-deck inbox fetch"),
        "cwd":daemon.home})
    .to_string();
    let (output, requests) = record_requests(
        cli(&daemon, Some(&pane), &["hook", "--agent", "claude-code"])
            .envs([("DOT_AGENT_DECK_INBOX_TEST_RECEIPTS_WAIT_MS", "300")]),
        prompt.as_bytes(),
        &daemon.home.join("asks-once.sock"),
        older,
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stderr
            .matches(dot_agent_deck::inbox_oneshot::RECEIPTS_COMMIT_TIMED_OUT)
            .count(),
        1,
        "{output:?}"
    );
    for not in [
        dot_agent_deck::inbox_oneshot::RECEIPTS_TIMED_OUT,
        dot_agent_deck::inbox_oneshot::RECEIPTS_UNSUPPORTED,
    ] {
        assert!(!stderr.contains(not), "{output:?}");
    }
    let questions = requests
        .iter()
        .filter(|r| r["message_type"] == "inbox_receipts")
        .count();
    assert_eq!(questions, 1, "{requests:?}");
    for request in &requests {
        let verb = request["message_type"].as_str().unwrap_or_default();
        assert!(!MAIL_VERBS.contains(&verb), "sent {verb}: {requests:?}");
    }
}

/// Scenario: Point Claude Code's waiter, its prompt hook answering a doorbell, and its prompt hook submitting a waiter's whole block at a hook socket of this user's own whose mode lets no one connect, so the connect is denied as it is for another user's private socket. Each exits 1 and says on stderr, in fixed text without the socket's path, that the deck socket refused it access, either because another user's deck listens there or because of the socket's permissions, and the remedy for each; the socket never accepts a connection, so no receipts question, which carries the token, is sent.
#[spec("inbox/claude/042")]
#[test]
fn claude_042_hooks_say_so_when_the_deck_socket_refuses_them() {
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: getuid(2) has no failure mode and touches no memory.
    if unsafe { libc::getuid() } == 0 {
        eprintln!("SKIP: root connects whatever a socket's mode says");
        return;
    }
    let daemon = daemon();
    let pane = pane(&daemon, "foreign-socket", "exec /bin/cat");
    let (id, rung) = ("m-k3xzq2ab-0f3c9a71d2e4b850", "m-k3xzq2ab-1a2b3c4d5e6f7081");
    let refused = |what: &str, name: &str, args: &[&str], stdin: String| {
        let path = daemon.home.join(name);
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let output = BoundedChild::start(
            cli(&daemon, Some(&pane), args)
                .env("DOT_AGENT_DECK_SOCKET", &path)
                .env("DOT_AGENT_DECK_INBOX_TEST_RECEIPTS_WAIT_MS", "300"),
            stdin.as_bytes(),
        )
        .output();
        assert_eq!(output.status.code(), Some(1), "{what}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(dot_agent_deck::inbox_oneshot::RECEIPTS_FOREIGN_SOCKET),
            "{what} says the socket refused it, and the remedies: {stderr}"
        );
        assert!(
            !stderr.contains(&*path.to_string_lossy()),
            "{what} names no path: {stderr}"
        );
        assert!(
            matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
            "{what} made no connection"
        );
    };
    refused(
        "the waiter",
        "foreign-wait.sock",
        &["inbox", "wait", "--agent", "claude-code"],
        json!({"hook_event_name":"SessionStart", "session_id":"foreign-socket",
            "source":"startup"})
        .to_string(),
    );
    refused(
        "the doorbell hook",
        "foreign-doorbell.sock",
        &["hook", "--agent", "claude-code"],
        json!({"session_id":"foreign-socket", "hook_event_name":"UserPromptSubmit",
            "prompt":format!(" dot-agent-deck: message {rung} waiting, run: dot-agent-deck inbox fetch"),
            "cwd":daemon.home})
        .to_string(),
    );
    refused(
        "the committing prompt hook",
        "foreign-commit.sock",
        &["hook", "--agent", "claude-code"],
        json!({"session_id":"foreign-socket", "hook_event_name":"UserPromptSubmit",
            "prompt":format!("[dot-agent-deck message {id} delivery 1.1 from the orchestrator]\nbody\n[end of dot-agent-deck message {id}]"),
            "cwd":daemon.home})
        .to_string(),
    );
}

/// Scenario: Submit one prompt to Claude Code's prompt hook that carries both a waiter's whole block and a doorbell for a second message, against a stand-in hook endpoint that serves receipts but waits 1.3 s before each answer, with the receipts budget set to 1.5 s. The hook is done within that one budget plus two seconds rather than spending a budget on each question and exchange: its commit asks first and is written in time, the doorbell's fetch, reached with the budget spent, asks nothing, and the hook exits 1 saying the deck did not answer in time, not that it is most likely older.
#[spec("inbox/claude/043")]
#[test]
fn claude_043_a_prompts_receipt_work_at_a_slow_deck_shares_one_budget() {
    use std::sync::{Arc, Condvar, Mutex};
    let daemon = daemon();
    let pane = pane(&daemon, "slow-deck", "exec /bin/cat");
    let (id, rung) = ("m-k3xzq2ab-0f3c9a71d2e4b850", "m-k3xzq2ab-1a2b3c4d5e6f7081");
    let (budget, delay) = (Duration::from_millis(1500), Duration::from_millis(1300));
    let path = daemon.home.join("slow-deck.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    // Set once the hook has exited: it ends the deck's held answers and,
    // with one last connect, its accept loop.
    let stop = Arc::new((Mutex::new(false), Condvar::new()));
    // Every connection served on a thread of its own, each answer held back
    // for `delay` (a slow deck); a line that is not a receipt verb (the
    // status event) is only recorded.
    let deck = {
        let (requests, stop) = (Arc::clone(&requests), Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut connections = Vec::new();
            for socket in listener.incoming() {
                if *stop.0.lock().unwrap() {
                    break;
                }
                let Ok(socket) = socket else { continue };
                let (requests, stop) = (Arc::clone(&requests), Arc::clone(&stop));
                connections.push(std::thread::spawn(move || {
                    let mut writer = socket.try_clone().unwrap();
                    for text in BufReader::new(socket).lines().map_while(Result::ok) {
                        let request: Value = serde_json::from_str(&text).unwrap_or_default();
                        let answer = match request["message_type"].as_str() {
                            Some("inbox_receipts") => Some(json!({"one_shot_receipts":1})),
                            Some("inbox_committed") => Some(json!({})),
                            Some("inbox_receipt_fetch") => Some(json!({"messages":[]})),
                            _ => None,
                        };
                        requests.lock().unwrap().push(request);
                        let Some(answer) = answer else { continue };
                        let (stopped, _) = stop
                            .1
                            .wait_timeout_while(stop.0.lock().unwrap(), delay, |stopped| !*stopped)
                            .unwrap();
                        if *stopped || writeln!(writer, "{answer}").is_err() {
                            break;
                        }
                    }
                }));
            }
            for connection in connections {
                let _ = connection.join();
            }
        })
    };
    let prompt = json!({"session_id":"slow-deck", "hook_event_name":"UserPromptSubmit",
        "prompt":format!("[dot-agent-deck message {id} delivery 1.1 from the orchestrator]\nbody\n[end of dot-agent-deck message {id}]\n dot-agent-deck: message {rung} waiting, run: dot-agent-deck inbox fetch"),
        "cwd":daemon.home})
    .to_string();
    let started = Instant::now();
    let child = BoundedChild::start(
        cli(&daemon, Some(&pane), &["hook", "--agent", "claude-code"])
            .env("DOT_AGENT_DECK_SOCKET", &path)
            .env(
                "DOT_AGENT_DECK_INBOX_TEST_RECEIPTS_WAIT_MS",
                budget.as_millis().to_string(),
            ),
        prompt.as_bytes(),
    );
    assert!(common::wait_until(WAIT, || child.finished()));
    let took = started.elapsed();
    let output = child.output();
    *stop.0.lock().unwrap() = true;
    stop.1.notify_all();
    let _ = UnixStream::connect(&path);
    deck.join().unwrap();
    // The slack for a loaded box to start the hook and schedule it late.
    let tolerance = Duration::from_secs(2);
    assert!(
        took < budget + tolerance,
        "the prompt hook took {took:?}, past one shared {budget:?}: {output:?}"
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(dot_agent_deck::inbox_oneshot::RECEIPTS_TIMED_OUT)
            && !stderr.contains(dot_agent_deck::inbox_oneshot::RECEIPTS_UNSUPPORTED),
        "the fetch out of time says so: {output:?}"
    );
    let requests = requests.lock().unwrap();
    let verbs: Vec<&str> = requests
        .iter()
        .filter_map(|r| r["message_type"].as_str())
        .filter(|verb| verb.starts_with("inbox_"))
        .collect();
    assert_eq!(
        verbs,
        ["inbox_receipts", "inbox_committed"],
        "the commit asks first and is written; the fetch asks nothing: {requests:?}"
    );
}
