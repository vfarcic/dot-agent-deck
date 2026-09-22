#![cfg(all(feature = "e2e", unix))]

//! L2 lane-1 coverage for PRD #1223 M7's daemon-owned authoring seeds.
//!
//! These tests drive a headless `daemon serve` and send `StartAgent` as raw
//! JSON to pin the optional field's wire spelling independently of the typed
//! client. The stand-in is the paste-aware variant of the late-announcing
//! `claude` probe used by `prompt/new-pane/017`: it emits a genuine
//! `SessionStart` through the real hook socket, distinguishes bytes queued
//! before readiness from bytes read afterwards, and reports a bracketed paste
//! as one submitted turn without spending a credential.

mod common;

use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use common::DaemonProc;
use dot_agent_deck::authoring_seeds::AuthoringKind;
use dot_agent_deck::daemon_protocol::{KIND_REQ, KIND_RESP, PROTOCOL_VERSION};
use serde_json::{Value, json};
use spec::spec;

const AUTHORING_KINDS: [&str; 3] = ["schedule", "schedule-issues", "dispatcher"];
const PASTE_SUBMITTED: &str = "submitted|paste";

fn wire_path(path: &Path) -> String {
    path.to_str()
        .unwrap_or_else(|| panic!("harness paths are UTF-8: {}", path.display()))
        .to_string()
}

/// Send an untyped JSON request through the attach protocol. This stays local
/// because these scenarios deliberately pin `authoring_kind`'s public JSON
/// shape independently of the typed client wrapper.
fn send_json_request(daemon: &DaemonProc, request: &Value) -> Value {
    let mut stream = std::os::unix::net::UnixStream::connect(&daemon.attach_socket)
        .expect("connect to the daemon attach socket");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set attach read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .expect("set attach write timeout");

    let payload = serde_json::to_vec(request).expect("serialize raw attach request");
    let mut header = [0_u8; 5];
    header[0] = KIND_REQ;
    header[1..].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    stream.write_all(&header).expect("write request header");
    stream.write_all(&payload).expect("write request payload");
    stream.flush().expect("flush request");

    let mut response_header = [0_u8; 5];
    stream
        .read_exact(&mut response_header)
        .expect("read response header");
    assert_eq!(
        response_header[0], KIND_RESP,
        "raw attach request must receive a RESP frame"
    );
    let len = u32::from_be_bytes([
        response_header[1],
        response_header[2],
        response_header[3],
        response_header[4],
    ]) as usize;
    let mut body = vec![0_u8; len];
    stream.read_exact(&mut body).expect("read response payload");
    serde_json::from_slice(&body).expect("response payload is JSON")
}

fn start_stand_in(
    daemon: &DaemonProc,
    cwd: &Path,
    pane_id: &str,
    log_name: &str,
    announce_after_secs: u64,
    authoring_kind: Option<&str>,
) -> String {
    std::fs::create_dir_all(cwd)
        .unwrap_or_else(|e| panic!("create stand-in cwd {}: {e}", cwd.display()));
    let command = common::write_late_announcing_paste_agent(cwd, log_name, announce_after_secs);
    let mut request = json!({
        "op": "start-agent",
        "command": wire_path(&command),
        "cwd": wire_path(cwd),
        "rows": 24,
        "cols": 100,
        "env": [["DOT_AGENT_DECK_PANE_ID", pane_id]],
        "display_name": pane_id,
    });
    if let Some(kind) = authoring_kind {
        request["authoring_kind"] = Value::String(kind.to_string());
    }

    let response = send_json_request(daemon, &request);
    assert_eq!(
        response.get("ok").and_then(Value::as_bool),
        Some(true),
        "StartAgent for pane {pane_id:?} must succeed; response: {response}"
    );
    response
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("StartAgent success must carry an id; response: {response}"))
        .to_string()
}

fn stand_in_log(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|_| "<the stand-in wrote no log>".to_string())
}

fn submitted_pastes(log: &str) -> Vec<String> {
    let mut submitted = Vec::new();
    let mut lines = Vec::new();
    for line in log.lines() {
        if let Some(received) = line.strip_prefix(common::LATE_ANNOUNCE_RECEIVED) {
            lines.push(received);
        } else if line == PASTE_SUBMITTED {
            submitted.push(lines.join("\n"));
            lines.clear();
        }
    }
    submitted
}

fn assert_ready_then_received_once(log_path: &Path, expected: &str, kind: AuthoringKind) {
    const DELIVERY_WAIT: Duration = Duration::from_secs(15);
    assert!(
        common::wait_for_file_substr_count(log_path, PASTE_SUBMITTED, 1, DELIVERY_WAIT),
        "the {:?} authoring seed never reached the stand-in as one submitted paste after its \
         readiness announcement within {}s\nstand-in log:\n{}",
        kind.as_str(),
        DELIVERY_WAIT.as_secs(),
        stand_in_log(log_path)
    );

    let log = stand_in_log(log_path);
    assert!(
        log.lines().any(|line| line == common::LATE_ANNOUNCE_CLEAN),
        "the {:?} seed was already queued before the stand-in's genuine SessionStart (or the \
         readiness probe failed); delivery must use the existing readiness gate\nstand-in log:\n{log}",
        kind.as_str()
    );
    assert_eq!(
        submitted_pastes(&log),
        [expected],
        "the {:?} authoring start must submit the exact shared seed composition\nstand-in \
         log:\n{log}",
        kind.as_str()
    );
    let opening_line = expected
        .lines()
        .next()
        .expect("an authoring seed is non-empty");
    assert!(
        !common::wait_for_file_substr_count(
            log_path,
            &format!("{}{}", common::LATE_ANNOUNCE_RECEIVED, opening_line),
            2,
            Duration::from_millis(800),
        ),
        "the {:?} authoring seed was delivered a second time after its first successful \
         delivery\nstand-in log:\n{}",
        kind.as_str(),
        stand_in_log(log_path)
    );
    assert_eq!(
        common::count_file_substr(log_path, PASTE_SUBMITTED),
        1,
        "the {:?} authoring seed must produce exactly one submitted turn\nstand-in log:\n{}",
        kind.as_str(),
        stand_in_log(log_path)
    );
}

/// Scenario: Start one readiness-announcing stand-in for each advertised
/// authoring kind and require the matching TUI seed text to arrive once after
/// readiness. The options query and handshake must expose all three kinds and
/// the capability that makes sending `authoring_kind` safe.
#[spec("newagent/authoring/001")]
#[test]
fn newagent_authoring_001_each_kind_delivers_its_tui_seed_and_is_advertised() {
    let daemon = common::spawn_daemon_serve_with_env(None, "0", &[]);
    let fixture = common::harness_tempdir().expect("mint authoring-agent fixture");

    for kind in AuthoringKind::ALL {
        let kind_name = kind.as_str();
        let cwd = fixture.path().join(kind_name);
        let log_name = format!("{kind_name}-authoring.log");
        let log_path = cwd.join(&log_name);
        let pane_id = format!("newagent-authoring-001-{kind_name}");
        start_stand_in(&daemon, &cwd, &pane_id, &log_name, 1, Some(kind_name));
        assert_ready_then_received_once(&log_path, &kind.compose_seed(&cwd), kind);
    }

    let options_response = send_json_request(&daemon, &json!({"op": "new-agent-options"}));
    assert_eq!(
        options_response.get("ok").and_then(Value::as_bool),
        Some(true),
        "NewAgentOptions must succeed after authoring support lands; response: {options_response}"
    );
    let actual_kinds: Vec<&str> = options_response
        .get("new_agent_options")
        .and_then(|options| options.get("authoring_kinds"))
        .and_then(Value::as_array)
        .unwrap_or_else(|| {
            panic!("NewAgentOptions must carry authoring_kinds; response: {options_response}")
        })
        .iter()
        .map(|value| {
            value
                .as_str()
                .unwrap_or_else(|| panic!("authoring kind must be a string: {value}"))
        })
        .collect();
    assert_eq!(
        actual_kinds, AUTHORING_KINDS,
        "the daemon must advertise every authoring seed it can compose, in the specified order"
    );

    let hello = send_json_request(
        &daemon,
        &json!({
            "op": "hello",
            "client_version": PROTOCOL_VERSION,
        }),
    );
    let capabilities = hello
        .get("capabilities")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("Hello must carry capabilities; response: {hello}"));
    assert!(
        capabilities.iter().any(|value| value == "authoring-kind"),
        "Hello must advertise `authoring-kind` before a client sends the field; capabilities: \
         {capabilities:?}"
    );
}

/// Scenario: Start an authoring stand-in that withholds its genuine readiness
/// announcement for three seconds and a plain control with no authoring kind.
/// Nothing may reach either pane early; after the late announcement only the
/// authoring pane receives one seed, while the control remains untouched.
#[spec("newagent/authoring/002")]
#[test]
fn newagent_authoring_002_late_readiness_delivers_once_and_plain_start_delivers_nothing() {
    const KIND: AuthoringKind = AuthoringKind::Schedule;
    const ANNOUNCE_AFTER_SECS: u64 = 3;

    let daemon = common::spawn_daemon_serve_with_env(None, "0", &[]);
    let fixture = common::harness_tempdir().expect("mint late-authoring fixture");
    let authoring_cwd = fixture.path().join("late-authoring");
    let authoring_log = authoring_cwd.join("late-authoring.log");
    start_stand_in(
        &daemon,
        &authoring_cwd,
        "newagent-authoring-002-late",
        "late-authoring.log",
        ANNOUNCE_AFTER_SECS,
        Some(KIND.as_str()),
    );

    let plain_cwd = fixture.path().join("plain-control");
    let plain_log = plain_cwd.join("plain-control.log");
    start_stand_in(
        &daemon,
        &plain_cwd,
        "newagent-authoring-002-plain",
        "plain-control.log",
        0,
        None,
    );

    assert_ready_then_received_once(&authoring_log, &KIND.compose_seed(&authoring_cwd), KIND);
    assert!(
        common::wait_for_file_substr_count(
            &plain_log,
            common::LATE_ANNOUNCE_CLEAN,
            1,
            Duration::from_secs(10),
        ),
        "the plain control never announced readiness, so its no-delivery result would be \
         inconclusive; stand-in log:\n{}",
        stand_in_log(&plain_log)
    );
    assert!(
        !common::wait_for_file_substr_count(
            &plain_log,
            common::LATE_ANNOUNCE_RECEIVED,
            1,
            Duration::from_secs(2),
        ),
        "a StartAgent with no authoring_kind must receive no daemon-owned seed\nstand-in log:\n{}",
        stand_in_log(&plain_log)
    );
}
