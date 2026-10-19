//! PR #1735 fifth review: the process-identity chain a native integration
//! depends on, run against the real binary in the fast tier, so CI's
//! `build-macos` exercises it as well as Linux. A real daemon with its
//! production binding on; inside a pane, the agent runs two transient hook
//! shells, one announcing and one streaming; each CLI is a real child whose
//! socket peer, parent walk and start time the daemon reads from the platform.
//! Nothing here is stubbed: on a platform where peer discovery, the parent
//! lookup, the start-time lookup or shell resolution is wrong, the announce
//! or the stream is refused and the test fails.

#![cfg(unix)]

mod common;

use common::DaemonProc;
use dot_agent_deck::daemon_protocol::{KIND_REQ, KIND_RESP};
use serde_json::{Value, json};
use spec::spec;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

const BODY: &str = "INBOX-IDENTITY-SENTINEL-4b7e: inspect the fixture";
const WAIT: Duration = Duration::from_secs(20);

struct Pane {
    id: String,
    agent: String,
    token: String,
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
    let reply = attach(
        daemon,
        json!({"op":"start-agent", "command":common::capability_export_command(command),
        "cwd":daemon.home, "rows":24, "cols":100,
        "env":[["DOT_AGENT_DECK_PANE_ID",name]], "display_name":name}),
    );
    assert_eq!(reply["ok"], true, "stand-in spawn: {reply}");
    let agent = reply["id"].as_str().expect("spawned agent id").to_string();
    let token = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(common::recorded_hook_capability(&daemon.home, &agent))
        .expect("stand-in capability");
    Pane {
        id: name.into(),
        agent,
        token,
    }
}

/// `inbox_list` answers metadata only, so the test process may ask it.
fn listing(daemon: &DaemonProc, pane: &Pane) -> Value {
    let request = json!({"message_type":"inbox_list", "pane_id":pane.id,
        "agent_id":pane.agent, "token":pane.token, "all":false});
    let mut socket = UnixStream::connect(&daemon.hook_socket).expect("hook socket");
    socket.set_read_timeout(Some(WAIT)).unwrap();
    writeln!(socket, "{request}").unwrap();
    let mut raw = String::new();
    BufReader::new(socket)
        .read_line(&mut raw)
        .unwrap_or_else(|error| panic!("inbox_list: no reply ({error}): {raw:?}"));
    serde_json::from_str(&raw).unwrap_or_else(|error| panic!("inbox_list ({error}): {raw:?}"))
}

/// Scenario: Inside a deck pane, the agent runs one hook shell that announces native wake and then a second, separate hook shell whose stream waiter takes the queued task. The daemon reads each process's peer, parents and start time from the platform; the announce is accepted, the second shell's waiter is recognised as the same receiver and receives the task.
#[spec("inbox/protocol/019")]
#[test]
fn inbox_protocol_019_two_transient_hook_shells_reach_one_receiver_on_this_platform() {
    let daemon = common::spawn_daemon_serve(None, "0");
    // The pane's agent is `xargs`, a program that is not a shell, as a real
    // agent is not: a shell agent would read its own `/bin/sh -c` hooks as a
    // nested agent of the same kind. It runs each NUL-separated command below
    // as `/bin/sh -c <command>`, one after another — two transient hook
    // shells, then one that holds the pane open.
    let hooks = [
        "while [ ! -f announce-go ]; do sleep 0.05; done; \
         \"$INBOX_TEST_BIN\" inbox announce --agent pi 2> announce.stderr",
        "while [ ! -f wait-go ]; do sleep 0.05; done; \
         sleep 60 | \"$INBOX_TEST_BIN\" inbox wait --agent pi --stream --adapter-integration 2 > wait.stdout 2> wait.stderr",
        "exec sleep 600",
    ];
    std::fs::write(daemon.home.join("hooks.nul"), hooks.join("\0") + "\0").unwrap();
    let command = format!(
        "exec env INBOX_TEST_BIN='{}' /usr/bin/xargs -0 -n 1 /bin/sh -c < hooks.nul",
        env!("CARGO_BIN_EXE_dot-agent-deck").replace('\'', "'\\''"),
    );
    let pane = pane(&daemon, "identity-hooks", &command);
    let reply = attach(
        &daemon,
        json!({"op":"post-message", "pane_id":pane.id, "expected_agent_id":pane.agent,
        "subject":"identity task", "body":BODY, "origin":"seed"}),
    );
    assert_eq!(reply["ok"], true, "post the task: {reply}");
    let read = |name: &str| std::fs::read_to_string(daemon.home.join(name)).unwrap_or_default();

    std::fs::write(daemon.home.join("announce-go"), b"go").unwrap();
    let last = std::cell::RefCell::new(Value::Null);
    assert!(
        common::wait_until(WAIT, || {
            *last.borrow_mut() = listing(&daemon, &pane);
            last.borrow()["wake"] == "native"
        }),
        "the first hook shell's announce must be accepted; last listing: {}, its stderr: {}",
        last.borrow(),
        read("announce.stderr")
    );

    std::fs::write(daemon.home.join("wait-go"), b"go").unwrap();
    assert!(
        common::wait_until(WAIT, || read("wait.stdout").contains(BODY)),
        "the second hook shell's stream must be admitted as the same receiver and take the task; \
         its stdout: {:?}, its stderr: {:?}, listing: {}",
        read("wait.stdout"),
        read("wait.stderr"),
        listing(&daemon, &pane)
    );
}
