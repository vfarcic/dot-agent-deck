#![cfg(all(feature = "e2e", unix))]
//! PRD #1666 §13 step 4: the migration boundary through the real daemon — the
//! capability-gated Retry and Cancel verbs, the adapter floor's notices in the
//! snapshot both clients hydrate from and in the live broadcast, and the
//! legacy hook command beside a receipt-owned OpenCode pane.
mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
use inbox::*;
use serde_json::{Value, json};
use spec::spec;
use std::time::Duration;

/// The pane's `AgentRecord.inbox`, as `ListAgents` answers it — what the TUI
/// and the desktop app hydrate their cards from.
fn record_inbox(daemon: &common::DaemonProc, pane: &Pane) -> Value {
    let records = attach(daemon, json!({"op":"list-agents"}));
    records["agent_records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == pane.agent)
        .map(|r| r["inbox"].clone())
        .unwrap_or(Value::Null)
}

/// A raw `SubscribeEvents` stream, collecting inbox broadcasts.
struct Broadcasts {
    socket: std::os::unix::net::UnixStream,
}

impl Broadcasts {
    fn open(daemon: &common::DaemonProc) -> Self {
        use dot_agent_deck::daemon_protocol::{KIND_REQ, KIND_RESP};
        use std::io::{Read, Write};
        let mut socket = std::os::unix::net::UnixStream::connect(&daemon.attach_socket).unwrap();
        socket.set_read_timeout(Some(WAIT)).unwrap();
        let payload = serde_json::to_vec(&json!({"op":"subscribe-events"})).unwrap();
        socket.write_all(&[KIND_REQ]).unwrap();
        socket
            .write_all(&(payload.len() as u32).to_be_bytes())
            .unwrap();
        socket.write_all(&payload).unwrap();
        let mut header = [0; 5];
        socket.read_exact(&mut header).unwrap();
        assert_eq!(header[0], KIND_RESP);
        let mut reply = vec![0; u32::from_be_bytes(header[1..].try_into().unwrap()) as usize];
        socket.read_exact(&mut reply).unwrap();
        Broadcasts { socket }
    }

    /// Read until an inbox broadcast for `pane` satisfies `pred`.
    fn until(&mut self, pane: &Pane, pred: impl Fn(&Value) -> bool) -> Value {
        use dot_agent_deck::daemon_protocol::KIND_EVENT;
        use std::io::Read;
        let started = std::time::Instant::now();
        loop {
            assert!(started.elapsed() < WAIT, "no matching inbox broadcast");
            let mut header = [0; 5];
            if self.socket.read_exact(&mut header).is_err() {
                continue;
            }
            let mut body = vec![0; u32::from_be_bytes(header[1..].try_into().unwrap()) as usize];
            self.socket.read_exact(&mut body).unwrap();
            if header[0] != KIND_EVENT {
                continue;
            }
            let Ok(value) = serde_json::from_slice::<Value>(&body) else {
                continue;
            };
            if value["kind"] == "inbox" && value["pane_id"] == pane.id && pred(&value) {
                return value;
            }
        }
    }
}

/// Scenario: Ask the daemon what it can do and press Retry and Cancel on messages it does not hold. The hello advertises `inbox-actions`, and each verb answers with an outcome — `unknown` — never the generic malformed-request refusal an older daemon gives.
#[spec("inbox/boundary/011")]
#[test]
fn boundary_011_the_daemon_advertises_and_answers_retry_and_cancel() {
    let daemon = daemon(&[]);
    let hello = attach(
        &daemon,
        json!({"op":"hello","client_version":dot_agent_deck::daemon_protocol::PROTOCOL_VERSION}),
    );
    assert!(
        hello["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == dot_agent_deck::daemon_protocol::CAP_INBOX_ACTIONS),
        "{hello}"
    );
    for op in ["inbox-retry", "inbox-cancel"] {
        for message_id in ["m-k3xzq2ab-0f3c9a71d2e4b850", "not a message id"] {
            let reply = attach(
                &daemon,
                json!({"op":op,"pane_id":"nobody","agent_id":"nobody","message_id":message_id,
                    "revision":0,"request_id":"press-1"}),
            );
            assert_eq!(reply["ok"], true, "{op}: {reply}");
            assert_eq!(reply["inbox_action"]["outcome"], "unknown", "{op}: {reply}");
        }
    }
}

/// Scenario: An OpenCode pane starts and its integration does not register by the readiness deadline. The snapshot both clients hydrate from says it has not connected yet and its mail waits queued; a late registration clears the notice in the snapshot and in a live broadcast, and the queued mail is handed to it with no restart.
#[spec("inbox/boundary/012")]
#[test]
fn boundary_012_a_slow_boot_shows_not_armed_until_a_late_registration_delivers() {
    let daemon = daemon_unbound(&[("DOT_AGENT_DECK_INBOX_ARM_DEADLINE_MS", "400")]);
    let mut broadcasts = Broadcasts::open(&daemon);
    let pane = pane_with(&daemon, "slow-opencode", json!({"agent_type":"open_code"}));
    let id = enqueue(&daemon, &pane, BODY);
    assert!(
        common::wait_until(WAIT, || record_inbox(&daemon, &pane)["integration"]
            == "not_armed"),
        "the snapshot says the integration has not connected: {}",
        record_inbox(&daemon, &pane)
    );
    broadcasts.until(&pane, |n| {
        n["attention_update"] == true && n["integration"] == "not_armed"
    });
    state(&daemon, &pane, &id, "queued");
    let mut stream = receipt_stream(&daemon, &pane);
    let handed = receipt_handout(&mut stream, &id);
    assert_eq!(
        handed["a"], 1,
        "the queued mail is delivered on registration"
    );
    assert!(
        common::wait_until(WAIT, || record_inbox(&daemon, &pane)["integration"]
            .is_null()),
        "the notice clears in the snapshot: {}",
        record_inbox(&daemon, &pane)
    );
    broadcasts.until(&pane, |n| {
        n["attention_update"] == true && n["integration"].is_null()
    });
}

/// Scenario: An OpenCode pane's adapter pins a CLI from before the integration flag (a stand-in that refuses it as clap does). The capability probe proves the CLI too old: the snapshot says to upgrade, the mail waits queued and no doorbell is typed. A probe that outlives its deadline proves nothing and sets no notice.
#[spec("inbox/boundary/013")]
#[test]
fn boundary_013_the_probe_marks_only_a_flag_rejecting_cli_too_old() {
    let daemon = daemon(&[("DOT_AGENT_DECK_INBOX_PROBE_DEADLINE_MS", "500")]);
    let old = daemon.home.join("old-dot-agent-deck");
    std::fs::write(
        &old,
        "#!/bin/sh\necho \"error: unexpected argument '--adapter-integration' found\" >&2\nexit 2\n",
    )
    .unwrap();
    let slow = daemon.home.join("slow-dot-agent-deck");
    std::fs::write(&slow, "#!/bin/sh\nsleep 5\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        for path in [&old, &slow] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    let too_old = pane_with(
        &daemon,
        "old-cli-opencode",
        json!({"agent_type":"open_code","env":[["DOT_AGENT_DECK_BIN", old]]}),
    );
    let id = enqueue(&daemon, &too_old, BODY);
    assert!(
        common::wait_until(WAIT, || record_inbox(&daemon, &too_old)["integration"]
            == "cli_too_old"),
        "{}",
        record_inbox(&daemon, &too_old)
    );
    state(&daemon, &too_old, &id, "queued");
    unchanged(&too_old, Duration::from_millis(1500));
    let unknown = pane_with(
        &daemon,
        "slow-cli-opencode",
        json!({"agent_type":"open_code","env":[["DOT_AGENT_DECK_BIN", slow]]}),
    );
    assert!(
        !common::wait_until(Duration::from_secs(2), || !record_inbox(&daemon, &unknown)
            ["integration"]
            .is_null()),
        "a probe that times out sets no notice: {}",
        record_inbox(&daemon, &unknown)
    );
}

/// Scenario: A Claude Code pane's legacy doorbell hook fetches as it always has and is served. The same doorbell fetch from an OpenCode pane — an old plugin, without the integration or a receiver — is refused, its mail waits queued, and the snapshot says its integration is out of date.
#[spec("inbox/boundary/014")]
#[test]
fn boundary_014_an_old_hook_command_is_served_and_an_old_opencode_fetch_is_refused() {
    let daemon = daemon_unbound(&[]);
    for (agent_type, served) in [("claude_code", true), ("open_code", false)] {
        let pane = pane_with(
            &daemon,
            &format!("legacy-{agent_type}"),
            json!({"agent_type":agent_type}),
        );
        let id = enqueue(&daemon, &pane, BODY);
        let mut req = request(&pane, "inbox_fetch");
        req["consumer"] = json!("doorbell_hook");
        req["max_chars"] = json!(9000);
        req["ack"] = json!(true);
        req["doorbell"] = json!(id);
        let reply = hook(&daemon, req);
        if served {
            assert_eq!(reply["messages"][0]["id"], id, "{reply}");
            state(&daemon, &pane, &id, "acknowledged");
        } else {
            assert_eq!(reply["refused"], "adapter_too_old", "{reply}");
            state(&daemon, &pane, &id, "queued");
            assert!(
                common::wait_until(WAIT, || record_inbox(&daemon, &pane)["integration"]
                    == "adapter_too_old"),
                "{}",
                record_inbox(&daemon, &pane)
            );
        }
    }
}
