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

/// Post `body` to `pane`'s agent through the attach socket; answers its id.
fn post(daemon: &DaemonProc, pane: &Pane, subject: &str, body: &str) -> String {
    let reply = attach(
        daemon,
        json!({"op":"post-message", "pane_id":pane.id, "expected_agent_id":pane.agent,
        "subject":subject, "body":body, "origin":"seed"}),
    );
    assert_eq!(reply["ok"], true, "post {subject}: {reply}");
    reply["id"].as_str().expect("posted id").to_string()
}

/// `id`'s state in `pane`'s listing, lower-cased, or `""` when it is not
/// listed.
fn state_of(daemon: &DaemonProc, pane: &Pane, id: &str) -> String {
    listing(daemon, pane)["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|message| message["id"] == id)
        .map(|message| message["state"].to_string().to_ascii_lowercase())
        .unwrap_or_default()
}

/// One request on the hook socket from this test process, answered with one
/// line.
fn hook(daemon: &DaemonProc, request: &Value) -> Value {
    let mut socket = UnixStream::connect(&daemon.hook_socket).expect("hook socket");
    socket.set_read_timeout(Some(WAIT)).unwrap();
    writeln!(socket, "{request}").unwrap();
    let mut raw = String::new();
    BufReader::new(socket)
        .read_line(&mut raw)
        .unwrap_or_else(|error| panic!("no reply ({error}): {raw:?}"));
    serde_json::from_str(&raw).unwrap_or_else(|error| panic!("reply ({error}): {raw:?}"))
}

/// A hook-socket connection that has sent `request`, kept open.
fn connect(daemon: &DaemonProc, request: &Value) -> BufReader<UnixStream> {
    let mut socket = UnixStream::connect(&daemon.hook_socket).expect("hook socket");
    socket.set_read_timeout(Some(WAIT)).unwrap();
    writeln!(socket, "{request}").unwrap();
    BufReader::new(socket)
}

/// The next JSON line on `socket`.
fn read_line(socket: &mut BufReader<UnixStream>) -> Value {
    let mut raw = String::new();
    socket
        .read_line(&mut raw)
        .unwrap_or_else(|error| panic!("no reply ({error}): {raw:?}"));
    serde_json::from_str(&raw).unwrap_or_else(|error| panic!("reply ({error}): {raw:?}"))
}

/// The pids from `pid`'s parent up to the top of the process table, read with
/// `ps`, so the walk is the platform's own and not the daemon's.
fn ancestors_of(pid: u32) -> Vec<u32> {
    let mut chain = Vec::new();
    let mut current = pid;
    for _ in 0..64 {
        let out = std::process::Command::new("ps")
            .args(["-o", "ppid=", "-p", &current.to_string()])
            .output()
            .expect("ps");
        let Ok(parent) = String::from_utf8_lossy(&out.stdout).trim().parse::<u32>() else {
            break;
        };
        if parent <= 1 {
            chain.push(parent);
            break;
        }
        chain.push(parent);
        current = parent;
    }
    chain
}

/// Scenario: Inside a deck pane, the agent starts a long-lived helper and lets go of it, so the helper runs outside the process tree the deck recorded for the pane, as Claude Code's background session host does, carrying the pane's environment. From there the helper's real CLI fetches and acknowledges the agent's queued task, the real Claude Code prompt hook takes a task its doorbell names, and a hook shell announces native wake and arms a stream that receives a third task.
#[spec("inbox/protocol/021")]
#[test]
fn inbox_protocol_021_a_pane_capability_holder_outside_the_agent_tree_takes_its_mail() {
    let daemon = common::spawn_daemon_serve(None, "0");
    // The helper is an `xargs` the agent starts in a subshell that exits at
    // once, so the helper is re-parented away from the agent: a process the
    // pane's agent started, holding its environment, that the deck's walk up
    // from the agent's pid does not reach. Each NUL-separated command is one
    // `/bin/sh -c` hook shell under it, as a background host's tool shells
    // and hooks are.
    let host = [
        "printf '%s' \"$PPID\" > host.pid",
        "while [ ! -f fetch-go ]; do sleep 0.05; done; \
         \"$INBOX_TEST_BIN\" inbox fetch > fetch.stdout 2> fetch.stderr; printf '%s' \"$?\" > fetch.exit",
        "while [ ! -f hook-go ]; do sleep 0.05; done; \
         \"$INBOX_TEST_BIN\" hook --agent claude-code < prompt.json > hook.stdout 2> hook.stderr; printf '%s' \"$?\" > hook.exit",
        "while [ ! -f announce-go ]; do sleep 0.05; done; \
         \"$INBOX_TEST_BIN\" inbox announce --agent pi 2> announce.stderr",
        "while [ ! -f wait-go ]; do sleep 0.05; done; \
         sleep 20 | \"$INBOX_TEST_BIN\" inbox wait --agent pi --stream --adapter-integration 2 > wait.stdout 2> wait.stderr",
    ];
    std::fs::write(daemon.home.join("host.nul"), host.join("\0") + "\0").unwrap();
    std::fs::write(
        daemon.home.join("agent.sh"),
        "#!/bin/sh\nprintf '%s' \"$$\" > agent.pid\n\
         ( /usr/bin/xargs -0 -n 1 /bin/sh -c < host.nul > /dev/null 2>&1 & )\n\
         exec /bin/cat\n",
    )
    .unwrap();
    let command = format!(
        "exec env INBOX_TEST_BIN='{}' /bin/sh agent.sh",
        env!("CARGO_BIN_EXE_dot-agent-deck").replace('\'', "'\\''"),
    );
    let pane = pane(&daemon, "detached-host", &command);
    let read = |name: &str| std::fs::read_to_string(daemon.home.join(name)).unwrap_or_default();
    assert!(
        common::wait_until(WAIT, || read("host.pid").parse::<u32>().is_ok()
            && read("agent.pid").parse::<u32>().is_ok()),
        "the helper and the agent record their pids"
    );
    let agent_pid: u32 = read("agent.pid").parse().unwrap();
    let host_pid: u32 = read("host.pid").parse().unwrap();
    let chain = ancestors_of(host_pid);
    assert!(
        !chain.contains(&agent_pid),
        "the helper {host_pid} must run outside the agent {agent_pid}'s tree; its ancestors: {chain:?}"
    );

    // The CLI an agent's tool shell runs: fetch, which acknowledges what it
    // printed.
    let fetched = post(
        &daemon,
        &pane,
        "fetched task",
        "DETACHED-FETCH-SENTINEL-71c2",
    );
    std::fs::write(daemon.home.join("fetch-go"), b"go").unwrap();
    assert!(
        common::wait_until(WAIT, || !read("fetch.exit").is_empty()),
        "the helper's fetch must finish"
    );
    assert_eq!(
        read("fetch.exit"),
        "0",
        "the helper's fetch must succeed; its stderr: {}",
        read("fetch.stderr")
    );
    assert!(
        read("fetch.stdout").contains("DETACHED-FETCH-SENTINEL-71c2"),
        "{}",
        read("fetch.stdout")
    );
    assert!(
        common::wait_until(WAIT, || state_of(&daemon, &pane, &fetched)
            .contains("acknowledged")),
        "the fetch acknowledges what it printed: {}",
        listing(&daemon, &pane)
    );

    // Claude Code's prompt hook, for a doorbell naming the next task: it takes
    // the task on receipts and prints it into the prompt.
    let rung = post(&daemon, &pane, "rung task", "DETACHED-HOOK-SENTINEL-5e09");
    std::fs::write(
        daemon.home.join("prompt.json"),
        json!({"session_id":"detached", "hook_event_name":"UserPromptSubmit",
            "prompt":format!(" dot-agent-deck: message {rung} waiting, run: dot-agent-deck inbox fetch"),
            "cwd":daemon.home})
        .to_string(),
    )
    .unwrap();
    std::fs::write(daemon.home.join("hook-go"), b"go").unwrap();
    assert!(
        common::wait_until(WAIT, || !read("hook.exit").is_empty()),
        "the helper's prompt hook must finish"
    );
    assert!(
        read("hook.stdout").contains("DETACHED-HOOK-SENTINEL-5e09"),
        "the prompt hook must print the task its doorbell names; exit {}, stdout: {}, stderr: {}",
        read("hook.exit"),
        read("hook.stdout"),
        read("hook.stderr")
    );

    // A receiver: one hook shell announces native wake, the next arms a
    // stream, and the stream is handed the next task.
    std::fs::write(daemon.home.join("announce-go"), b"go").unwrap();
    let last = std::cell::RefCell::new(Value::Null);
    assert!(
        common::wait_until(WAIT, || {
            *last.borrow_mut() = listing(&daemon, &pane);
            last.borrow()["wake"] == "native"
        }),
        "the helper's announce must be accepted; last listing: {}, its stderr: {}",
        last.borrow(),
        read("announce.stderr")
    );
    post(&daemon, &pane, "streamed task", BODY);
    std::fs::write(daemon.home.join("wait-go"), b"go").unwrap();
    assert!(
        common::wait_until(WAIT, || read("wait.stdout").contains(BODY)),
        "the helper's stream must be admitted and take the task; its stdout: {:?}, its stderr: {:?}",
        read("wait.stdout"),
        read("wait.stderr")
    );
}

/// Scenario: Two panes run, and a third pane's agent is stopped and started again so it has a new generation. From outside every pane, requests for the first pane's queued task carrying the second pane's capability, no capability at all, or the third pane's earlier generation's capability are each refused with their own named cause, and every task stays queued. The same outside process holding the first pane's current capability is refused the old plain fetch, which takes nothing, then fetches its task on receipts and acknowledges it.
#[spec("inbox/protocol/022")]
#[test]
fn inbox_protocol_022_a_wrong_missing_or_earlier_capability_takes_nothing() {
    let daemon = common::spawn_daemon_serve(None, "0");
    let owner = pane(&daemon, "capability-owner", "exec /bin/cat");
    let other = pane(&daemon, "capability-other", "exec /bin/cat");
    let task = post(&daemon, &owner, "owner task", BODY);
    let fetch = |pane: &str, agent: &str, token: Option<&str>| {
        let mut request = json!({"message_type":"inbox_receipt_fetch", "pane_id":pane,
            "agent_id":agent, "consumer":"cli", "hook_integration":1});
        if let Some(token) = token {
            request["token"] = json!(token);
        }
        hook(&daemon, &request)
    };
    let ack = |pane: &str, agent: &str, token: Option<&str>, id: &str| {
        let mut request = json!({"message_type":"ack", "pane_id":pane, "agent_id":agent,
            "delivery_id":id});
        if let Some(token) = token {
            request["token"] = json!(token);
        }
        hook(&daemon, &request)
    };

    // Another pane's capability naming this pane.
    let reply = fetch(&owner.id, &owner.agent, Some(&other.token));
    assert_eq!(reply["refused"], "token_names_another_pane", "{reply}");
    assert!(reply.get("messages").is_none_or(Value::is_null), "{reply}");
    let reply = ack(&owner.id, &owner.agent, Some(&other.token), &task);
    assert_eq!(reply["reason"], "token_names_another_pane", "{reply}");
    // No capability at all, for a pane that was issued one.
    let reply = fetch(&owner.id, &owner.agent, None);
    assert_eq!(reply["refused"], "missing_token", "{reply}");
    let reply = ack(&owner.id, &owner.agent, None, &task);
    assert_eq!(reply["reason"], "missing_token", "{reply}");
    assert!(state_of(&daemon, &owner, &task).contains("queued"));

    // An earlier generation's capability, after its pane's agent was replaced.
    let earlier = pane(&daemon, "capability-respawned", "exec /bin/cat");
    let stopped = attach(&daemon, json!({"op":"stop-agent", "id":earlier.agent}));
    assert_eq!(stopped["ok"], true, "stop the first generation: {stopped}");
    let current = pane(&daemon, "capability-respawned", "exec /bin/cat");
    assert_ne!(current.agent, earlier.agent);
    let successor_task = post(
        &daemon,
        &current,
        "successor task",
        "SUCCESSOR-SENTINEL-0d4a",
    );
    let reply = fetch(&current.id, &earlier.agent, Some(&earlier.token));
    assert_eq!(reply["refused"], "unknown_token", "{reply}");
    let reply = fetch(&current.id, &current.agent, Some(&earlier.token));
    assert_eq!(reply["refused"], "unknown_token", "{reply}");
    let reply = ack(
        &current.id,
        &current.agent,
        Some(&earlier.token),
        &successor_task,
    );
    assert_eq!(reply["reason"], "unknown_token", "{reply}");
    assert!(state_of(&daemon, &current, &successor_task).contains("queued"));

    // The pane's current capability, from the same outside process: the old
    // plain fetch is refused for what it is (PRD #1666 part 3, H5), and a
    // current fetch takes the task, which its ack settles.
    let mut plain = json!({"message_type":"inbox_fetch", "pane_id":owner.id,
        "agent_id":owner.agent, "consumer":"cli", "ack":true, "token":owner.token});
    let reply = hook(&daemon, &plain);
    assert_eq!(reply["refused"], "cli_too_old", "{reply}");
    assert!(state_of(&daemon, &owner, &task).contains("queued"));
    plain["message_type"] = json!("inbox_receipt_fetch");
    plain["hook_integration"] = json!(1);
    let mut socket = connect(&daemon, &plain);
    let reply = read_line(&mut socket);
    assert!(
        reply["refused"].is_null(),
        "the current capability is admitted: {reply}"
    );
    assert!(reply["messages"].to_string().contains(BODY), "{reply}");
    let held = &reply["messages"][0];
    let may = json!({"may": {"q": 0, "id": held["id"], "a": held["a"], "e": held["e"]}});
    writeln!(socket.get_mut(), "{may}").unwrap();
    assert_eq!(read_line(&mut socket)["grant"]["ok"], true);
    let reply = ack(&owner.id, &owner.agent, Some(&owner.token), &task);
    assert_eq!(reply["delivery"], "acknowledged", "{reply}");
    assert!(
        common::wait_until(WAIT, || state_of(&daemon, &owner, &task)
            .contains("acknowledged")),
        "{}",
        listing(&daemon, &owner)
    );
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
