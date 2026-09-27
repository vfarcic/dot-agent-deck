#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]

//! L2 lane-2 proof for the desktop new-agent flow: the real deck is attached
//! while the desktop's daemon verb sequence browses into an ordinary directory,
//! starts an interactive cheap-model Claude there, and shows its work in a pane.

mod common;

use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::agent_pty::DOT_AGENT_DECK_PANE_ID;
use dot_agent_deck::daemon_protocol::{KIND_REQ, KIND_RESP};
use dot_agent_deck::event::SendResult;
use serde_json::{Value, json};
use spec::spec;

const FIXTURE_DIRECTORY: &str = "newagent-live-ordinary-directory";
const SENTINEL: &str = "newagent_live_sentinel_8f3a.txt";
const PANE_ID: &str = "newagent-live-claude";
const HAIKU_MODEL: &str = "claude-haiku-4-5-20251001";
const DIRECTIVE_PROMPT: &str = "Use the Bash tool to run `ls -a` in the current directory and \
    print every filename it lists verbatim, one per line, with no other commentary. Do not ask \
    what to do, offer choices, or wait for further instructions; this is the complete task.";

/// Send raw JSON through the attach protocol so this end-to-end scenario pins
/// the desktop-facing verb sequence independently of the typed Rust client.
fn send_json_request(socket: &Path, request: &Value) -> Value {
    let mut stream = std::os::unix::net::UnixStream::connect(socket)
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

fn successful_payload<'a>(response: &'a Value, key: &str, operation: &str) -> &'a Value {
    assert_eq!(
        response.get("ok").and_then(Value::as_bool),
        Some(true),
        "{operation} must succeed; response: {response}"
    );
    response
        .get(key)
        .unwrap_or_else(|| panic!("{operation} success must carry {key:?}; response: {response}"))
}

/// Scenario: Attach the real TUI, use only daemon-returned paths to browse from
/// its HOME into an ordinary directory, take Claude's command from
/// `NewAgentOptions`, and start an interactive Haiku agent there. Submit a
/// directive listing task and require the unique on-disk sentinel filename to
/// appear in the agent's rendered pane.
#[spec("newagent/live/001")]
#[test]
fn newagent_live_001_a_real_agent_reports_a_sentinel_from_the_browsed_directory() {
    skip_unless!(common::check_claude_available());

    let deck = TuiDeck::builder()
        .with_pty_size(110, 32)
        .with_imported_claude_credentials()
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");

    let fixture = deck.home_dir().join(FIXTURE_DIRECTORY);
    std::fs::create_dir(&fixture).expect("create ordinary directory fixture under daemon HOME");
    std::fs::write(fixture.join(SENTINEL), "new-agent live fixture\n")
        .expect("write unique fixture sentinel");
    assert!(
        !fixture.join(".dot-agent-deck.toml").exists(),
        "the browsed fixture must remain an ordinary non-project directory"
    );

    let socket = deck.attach_socket_path().to_path_buf();
    let events = deck.subscribe_events();

    // The first browser request intentionally has no path: the daemon chooses
    // its own HOME. The next request reuses the child path from that response
    // verbatim, so the client never joins a daemon-side navigation path.
    let home_response = send_json_request(&socket, &json!({"op": "list-directories"}));
    let home_listing = successful_payload(&home_response, "directories", "ListDirectories HOME");
    let expected_home = std::fs::canonicalize(deck.home_dir())
        .expect("canonicalize the isolated daemon HOME")
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        home_listing.get("path").and_then(Value::as_str),
        Some(expected_home.as_str()),
        "an absent-path listing must begin at the daemon process's canonical HOME"
    );
    let browsed_entry = home_listing
        .get("entries")
        .and_then(Value::as_array)
        .and_then(|entries| {
            entries
                .iter()
                .find(|entry| entry.get("name").and_then(Value::as_str) == Some(FIXTURE_DIRECTORY))
        })
        .unwrap_or_else(|| {
            panic!(
                "the daemon's HOME listing must return the fixture child; response: \
                 {home_response}"
            )
        });
    assert_eq!(
        browsed_entry.get("is_project").and_then(Value::as_bool),
        Some(false),
        "the sentinel directory has no .dot-agent-deck.toml and must be reported as ordinary"
    );
    let browsed_entry_path = browsed_entry
        .get("path")
        .and_then(Value::as_str)
        .expect("the daemon-returned child must carry its canonical path")
        .to_string();

    let directory_response = send_json_request(
        &socket,
        &json!({"op": "list-directories", "path": browsed_entry_path}),
    );
    let directory_listing = successful_payload(
        &directory_response,
        "directories",
        "ListDirectories fixture child",
    );
    let canonical_browsed_path = directory_listing
        .get("path")
        .and_then(Value::as_str)
        .expect("the browsed directory reply must carry its canonical path")
        .to_string();
    assert_eq!(
        canonical_browsed_path, browsed_entry_path,
        "the second listing must preserve the canonical child path supplied by the first"
    );

    let options_response = send_json_request(&socket, &json!({"op": "new-agent-options"}));
    let options = successful_payload(&options_response, "new_agent_options", "NewAgentOptions");
    let claude_default_command = options
        .get("agents")
        .and_then(Value::as_array)
        .and_then(|agents| {
            agents
                .iter()
                .find(|agent| agent.get("id").and_then(Value::as_str) == Some("claude"))
        })
        .and_then(|agent| agent.get("default_command"))
        .and_then(Value::as_str)
        .filter(|command| !command.trim().is_empty())
        .unwrap_or_else(|| {
            panic!(
                "NewAgentOptions must return Claude's default command; response: {options_response}"
            )
        });
    let command = format!("{claude_default_command} --model {HAIKU_MODEL} --allowedTools Bash");

    common::seed_claude_trust_in_home(
        deck.home_dir(),
        std::slice::from_ref(&canonical_browsed_path),
    )
    .expect("pre-trust the daemon-returned canonical fixture path for interactive Claude");

    let start_response = send_json_request(
        &socket,
        &json!({
            "op": "start-agent",
            "command": command,
            "cwd": canonical_browsed_path,
            "rows": 30,
            "cols": 108,
            "env": [[DOT_AGENT_DECK_PANE_ID, PANE_ID]],
            "display_name": "new-agent live proof",
            "agent_type": "claude_code",
        }),
    );
    assert_eq!(
        start_response.get("ok").and_then(Value::as_bool),
        Some(true),
        "StartAgent in the browsed ordinary directory must succeed; response: {start_response}"
    );
    let agent_id = start_response
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_else(|| {
            panic!("StartAgent success must carry an id; response: {start_response}")
        })
        .to_string();

    let session_id =
        events.wait_for_session_start_on_pane(PANE_ID, &agent_id, Duration::from_secs(180));
    deck.wait_for_absence("No active agents");
    deck.send_keys(b"1");

    if !common::wait_until_panes_settled(
        &socket,
        std::slice::from_ref(&agent_id),
        Duration::from_millis(1500),
        Duration::from_secs(8),
        Duration::from_secs(180),
    ) {
        eprintln!("warning: the new-agent pane never settled within 180s; delivering anyway");
    }

    let delivery = common::write_and_submit_with_identity_on(
        &socket,
        PANE_ID,
        DIRECTIVE_PROMPT,
        &agent_id,
        Some(&session_id),
    )
    .expect("WriteAndSubmit the directive prompt over the attach socket");
    assert_eq!(
        delivery.send_result,
        Some(SendResult::Applied),
        "the daemon refused to deliver the directive to pane {PANE_ID}: error={:?}, \
         send_result={:?}",
        delivery.error,
        delivery.send_result
    );

    const REPORT_WAIT: Duration = Duration::from_secs(240);
    let reported = common::wait_for_pane_text_on(&socket, &agent_id, SENTINEL, REPORT_WAIT);
    assert!(
        reported,
        "the real agent never reported the fixture sentinel {SENTINEL:?} within {}s after being \
         asked to list its current directory. The prompt does not contain the filename, so a \
         match requires the agent to inspect the browsed directory.\n=== agent pane ===\n{}\n=== \
         deck grid ===\n{}",
        REPORT_WAIT.as_secs(),
        common::pane_search_key_on(&socket, &agent_id),
        deck.snapshot_grid()
    );

    // The pane-level wait tolerates terminal normalization while this is the
    // user-facing claim: the same filename must be present on the attached
    // TUI's current vt100 grid.
    deck.wait_for_string(SENTINEL);
}
