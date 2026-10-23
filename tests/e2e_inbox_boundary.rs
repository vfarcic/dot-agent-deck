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

/// Scenario: A Claude Code pane's legacy doorbell hook fetches as it did before Claude's hooks moved onto receipts, declaring no one-shot integration: it is refused as hooks too old, its mail waits queued and the snapshot names the stale hooks. The same doorbell fetch from an OpenCode pane — an old plugin, without the integration or a receiver — is refused, its mail waits queued, and the snapshot says its integration is out of date.
#[spec("inbox/boundary/014")]
#[test]
fn boundary_014_an_old_hook_fetch_is_refused_for_claude_and_opencode() {
    let daemon = daemon_unbound(&[]);
    for (agent_type, refusal, notice) in [
        ("claude_code", "hooks_too_old", "hooks_too_old"),
        ("open_code", "adapter_too_old", "adapter_too_old"),
    ] {
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
        assert_eq!(reply["refused"], refusal, "{reply}");
        assert!(reply.get("messages").is_none(), "{reply}");
        state(&daemon, &pane, &id, "queued");
        assert!(
            common::wait_until(WAIT, || record_inbox(&daemon, &pane)["integration"]
                == notice),
            "{}",
            record_inbox(&daemon, &pane)
        );
    }
}

/// Scenario: Two OpenCode roles are configured by their command alone, with no agent type: one runs the bare `opencode` found on its PATH, the other a path-qualified `…/bin/opencode`. Each spawn regenerates the deck's OpenCode plugin, runs the capability probe against the CLI the pane pins, and starts the readiness deadline, so the snapshot says `not_armed` until a receipt stream registers; a queued message is then handed to that stream and settles on its commit.
#[spec("inbox/boundary/018")]
#[test]
fn boundary_018_an_opencode_role_named_by_its_command_gets_the_receipt_policy() {
    use std::os::unix::fs::PermissionsExt;
    let daemon = daemon_unbound(&[("DOT_AGENT_DECK_INBOX_ARM_DEADLINE_MS", "400")]);
    let executable = |path: &std::path::Path, body: String| {
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    let plugin = daemon
        .home
        .join(".config")
        .join("opencode")
        .join("plugin")
        .join("dot-agent-deck.js");
    std::fs::create_dir_all(plugin.parent().unwrap().parent().unwrap()).unwrap();
    let probes = daemon.home.join("probes.log");
    let cli = daemon.home.join("probed-dot-agent-deck");
    executable(
        &cli,
        format!(
            "#!/bin/sh\necho \"$*\" >> '{}'\necho '      --adapter-integration <N>  The integration'\nexit 0\n",
            probes.display()
        ),
    );
    let probed = || {
        std::fs::read_to_string(&probes)
            .unwrap_or_default()
            .lines()
            .filter(|line| *line == "inbox wait --adapter-integration 2 --help")
            .count()
    };
    let inherited = std::env::var("PATH").unwrap_or_default();
    for (round, (name, bare)) in [("bare-opencode", true), ("qualified-opencode", false)]
        .into_iter()
        .enumerate()
    {
        let bin = daemon.home.join(format!("{name}-bin"));
        std::fs::create_dir_all(&bin).unwrap();
        let launcher = bin.join("opencode");
        executable(
            &launcher,
            format!(
                "#!/bin/sh\n(umask 077; printf '%s' \"$DOT_AGENT_DECK_PANE_CAPABILITY\" > \".test-capability-$DOT_AGENT_DECK_AGENT_ID\")\nexec python3 '{}' '{}'\n",
                daemon.home.join(format!("{name}.py")).display(),
                daemon.home.join(name).display()
            ),
        );
        let _ = std::fs::remove_file(&plugin);
        let (command, mut env) = if bare {
            (
                "opencode".to_string(),
                vec![json!(["PATH", format!("{}:{inherited}", bin.display())])],
            )
        } else {
            (launcher.to_string_lossy().into_owned(), Vec::new())
        };
        env.push(json!(["DOT_AGENT_DECK_BIN", cli]));
        let pane = pane_with(&daemon, name, json!({"command": command, "env": env}));
        assert!(
            common::wait_until(WAIT, || plugin.is_file()),
            "{name}: the spawn regenerates the plugin"
        );
        assert!(
            common::wait_until(WAIT, || probed() == round + 1),
            "{name}: the spawn runs the probe: {:?}",
            std::fs::read_to_string(&probes)
        );
        assert!(
            common::wait_until(WAIT, || record_inbox(&daemon, &pane)["integration"]
                == "not_armed"),
            "{name}: the readiness deadline runs: {}",
            record_inbox(&daemon, &pane)
        );
        let id = enqueue(&daemon, &pane, BODY);
        state(&daemon, &pane, &id, "queued");
        let mut stream = receipt_stream(&daemon, &pane);
        let handed = receipt_handout(&mut stream, &id);
        receipt_control(
            &mut stream,
            json!({"committed": [{"id": id, "a": handed["a"], "e": handed["e"]}]}),
        );
        state(&daemon, &pane, &id, "acknowledged");
        assert!(
            common::wait_until(WAIT, || record_inbox(&daemon, &pane)["integration"]
                .is_null()),
            "{name}: the registration clears the notice"
        );
    }
}

/// Scenario: An OpenCode plugin from before the integration floor is installed, pinned to another live copy of the deck. An OpenCode stand-in reads the plugin the moment it starts: it finds the deck's current integration in it, still pinned to that other copy, so starting the agent really refreshes its adapter without taking over another install's pin. Then the plugin is stale again and its directory cannot be written: the next start says on the card that the deck could not update the integration, rather than promising that a restart will.
#[spec("inbox/boundary/025")]
#[test]
fn boundary_025_a_start_refreshes_the_adapter_first_or_says_it_could_not() {
    use std::os::unix::fs::PermissionsExt;
    let daemon = daemon_unbound(&[("DOT_AGENT_DECK_INBOX_ARM_DEADLINE_MS", "600000")]);
    let executable = |path: &std::path::Path, body: String| {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    let plugin_dir = daemon.home.join(".config").join("opencode").join("plugin");
    let plugin = plugin_dir.join("dot-agent-deck.js");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    let foreign = daemon
        .home
        .join("foreign-install")
        .join("bin")
        .join("dot-agent-deck");
    executable(
        &foreign,
        "#!/bin/sh\necho '      --adapter-integration <N>  The integration'\nexit 0\n".to_string(),
    );
    let pin_line = format!(
        "const BINARY_PATH = {};",
        serde_json::to_string(&foreign.to_string_lossy()).unwrap()
    );
    let stale = format!("{pin_line}\nconst INBOX_ADAPTER_INTEGRATION = 1;\n");
    // A stand-in OpenCode that reads the plugin the moment it starts, before
    // it becomes the pane's python stand-in.
    let launcher = |name: &str| {
        let path = daemon.home.join(format!("{name}-bin")).join("opencode");
        executable(
            &path,
            format!(
                "#!/bin/sh\ngrep -h 'INBOX_ADAPTER_INTEGRATION = \\|BINARY_PATH = ' '{plugin}' > '{seen}.tmp'\nmv '{seen}.tmp' '{seen}'\n(umask 077; printf '%s' \"$DOT_AGENT_DECK_PANE_CAPABILITY\" > \".test-capability-$DOT_AGENT_DECK_AGENT_ID\")\nexec python3 '{script}' '{base}'\n",
                plugin = plugin.display(),
                seen = daemon.home.join(format!("{name}.seen")).display(),
                script = daemon.home.join(format!("{name}.py")).display(),
                base = daemon.home.join(name).display(),
            ),
        );
        (path, daemon.home.join(format!("{name}.seen")))
    };
    let seen = |out: &std::path::Path| {
        assert!(
            common::wait_until(WAIT, || out.is_file()),
            "the stand-in read the plugin"
        );
        std::fs::read_to_string(out).unwrap()
    };

    std::fs::write(&plugin, &stale).unwrap();
    let (command, first) = launcher("refreshed");
    pane_with(
        &daemon,
        "refreshed",
        json!({"command": command.to_string_lossy()}),
    );
    let read = seen(&first);
    assert!(
        read.contains(&format!(
            "const INBOX_ADAPTER_INTEGRATION = {};",
            dot_agent_deck::inbox_hub::ADAPTER_INTEGRATION
        )),
        "the agent loads the current integration, written before it started: {read}"
    );
    assert!(
        read.contains(&pin_line),
        "the other install's pin is kept: {read}"
    );

    std::fs::write(&plugin, &stale).unwrap();
    std::fs::set_permissions(&plugin_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let (command, second) = launcher("unwritable");
    let pane = pane_with(
        &daemon,
        "unwritable",
        json!({"command": command.to_string_lossy()}),
    );
    let read = seen(&second);
    let notice = common::wait_until(WAIT, || {
        record_inbox(&daemon, &pane)["integration"] == "refresh_failed"
    });
    std::fs::set_permissions(&plugin_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        read.contains("const INBOX_ADAPTER_INTEGRATION = 1;"),
        "nothing could be written: {read}"
    );
    assert!(
        notice,
        "the card says the deck could not update it: {}",
        record_inbox(&daemon, &pane)
    );
}

/// `dot-agent-deck inbox fetch` as a Devin stand-in pane's model runs it: the
/// pane's identity, capability and receipt spool.
fn devin_fetch(daemon: &common::DaemonProc, pane: &Pane) -> std::process::Command {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"));
    command
        .args(["inbox", "fetch"])
        .env_clear()
        .env("HOME", &daemon.home)
        .env("DOT_AGENT_DECK_SOCKET", &daemon.hook_socket)
        .env("DOT_AGENT_DECK_PANE_ID", &pane.id)
        .env("DOT_AGENT_DECK_AGENT_ID", &pane.agent)
        .env("DOT_AGENT_DECK_PANE_CAPABILITY", &pane.token)
        .stdin(std::process::Stdio::null());
    if let Some(spool) = pane_env(pane, "DOT_AGENT_DECK_INBOX_SPOOL") {
        command.env("DOT_AGENT_DECK_INBOX_SPOOL", spool);
    }
    command
}

/// Scenario: Queue a task for an idle stand-in spawned as Devin, which has no inbox hooks: the deck types its doorbell, and the model's `dot-agent-deck inbox fetch` is a one-shot receiver. Run with its stdout already closed, it writes nothing, says so, and the task is back in the queue at once; run again, it prints the block naming its attempt and the task is settled by its receipt.
#[spec("inbox/boundary/054")]
#[test]
fn boundary_054_a_devin_panes_model_run_fetch_is_a_one_shot_receiver() {
    let daemon = daemon_unbound(&[]);
    let pane = pane_with(&daemon, "devin-standin", json!({"agent_type":"devin"}));
    // Devin's own status hook says it is idle; it has no inbox hooks.
    status(&daemon, &pane, "idle", "devin");
    let id = enqueue(&daemon, &pane, BODY);
    ring(&daemon, &pane, &id);
    let mut closed = devin_fetch(&daemon, &pane)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    drop(closed.stdout.take());
    let closed = closed.wait_with_output().unwrap();
    assert!(!closed.status.success(), "{closed:?}");
    assert!(
        String::from_utf8_lossy(&closed.stderr)
            .contains(dot_agent_deck::inbox_cli::CLI_NOTHING_PRINTED),
        "{closed:?}"
    );
    state(&daemon, &pane, &id, "queued");
    let fetched = devin_fetch(&daemon, &pane).output().unwrap();
    assert!(fetched.status.success(), "{fetched:?}");
    let stdout = String::from_utf8_lossy(&fetched.stdout);
    assert!(
        stdout.contains(BODY)
            && stdout.contains(&format!("[dot-agent-deck message {id} delivery ")),
        "{stdout}"
    );
    state(&daemon, &pane, &id, "acknowledged");
    let summary = listing(&daemon, &pane)["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == id)
        .cloned()
        .unwrap();
    assert_eq!(summary["ack_how"], "plugin_confirmed", "{summary}");
}

/// Scenario: A deck daemon serves Codex, Devin and bare-command panes, each with a task waiting, and an older dot-agent-deck asks for their mail the old ways: a plain `inbox fetch` (with and without its acknowledge-on-fetch), a doorbell and a turn-end hook fetch declaring no one-shot integration, a once wait from a hook component, and `get-seed`. Each is answered at once with a refusal, takes nothing and acknowledges nothing: the task stays queued with no hand-over counted. An old hook's refusal puts the out-of-date hooks notice on the card, naming Codex's own `hooks install` for the Codex pane and no command where the deck does not know the agent. A current receipt fetch from the same pane is then handed the task.
#[spec("inbox/boundary/073")]
#[test]
fn boundary_073_every_old_way_to_take_mail_is_refused_and_takes_nothing() {
    let daemon = daemon_unbound(&[]);
    for (name, extra, command) in [
        (
            "old-codex",
            json!({"agent_type":"codex"}),
            Some("--agent codex"),
        ),
        ("old-devin", json!({"agent_type":"devin"}), None),
        ("old-bare", json!({}), None),
    ] {
        let pane = pane_with(&daemon, name, extra);
        let id = enqueue(&daemon, &pane, BODY);
        let untouched = |what: &str| {
            let listed = listing(&daemon, &pane);
            let message = listed["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["id"] == id.as_str())
                .cloned()
                .unwrap_or(Value::Null);
            assert_eq!(message["state"], "queued", "{name}: {what}: {listed}");
            assert_eq!(message["deliveries"], 0, "{name}: {what}: {listed}");
        };
        for ack in [false, true] {
            let mut req = request(&pane, "inbox_fetch");
            req["consumer"] = json!("cli");
            req["max_chars"] = json!(65536);
            req["ack"] = json!(ack);
            let started = std::time::Instant::now();
            let reply = hook(&daemon, req);
            assert_eq!(reply["refused"], "cli_too_old", "{name}: {reply}");
            assert!(reply.get("messages").is_none(), "{name}: {reply}");
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "{name}: answered at once"
            );
            untouched("plain fetch");
        }
        for consumer in ["doorbell_hook", "turn_end_hook"] {
            let mut req = request(&pane, "inbox_fetch");
            req["consumer"] = json!(consumer);
            req["max_chars"] = json!(9000);
            req["ack"] = json!(true);
            if consumer == "doorbell_hook" {
                req["doorbell"] = json!(id);
            }
            let started = std::time::Instant::now();
            let reply = hook(&daemon, req);
            assert_eq!(
                reply["refused"], "hooks_too_old",
                "{name} {consumer}: {reply}"
            );
            assert!(reply.get("messages").is_none(), "{name}: {reply}");
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "{name}: answered at once"
            );
            untouched(consumer);
        }
        assert!(
            common::wait_until(WAIT, || record_inbox(&daemon, &pane)["integration"]
                == "hooks_too_old"),
            "{name}: {}",
            record_inbox(&daemon, &pane)
        );
        let named = record_inbox(&daemon, &pane)["integration_command"].clone();
        match command {
            Some(agent) => assert!(
                named.as_str().is_some_and(|c| c.contains(agent)),
                "{name}: the card names the agent's own hooks install: {named}"
            ),
            None => assert!(
                named.is_null(),
                "{name}: no command it cannot name: {named}"
            ),
        }
        for component in ["codex_hooks", "other"] {
            let mut req = request(&pane, "inbox_wait");
            req["component"] = json!(component);
            req["component_version"] = json!("synthetic-old");
            req["mode"] = json!("once");
            req["pid"] = json!(pane.pid);
            let reply = line(&mut connect(&daemon, &req));
            assert_eq!(
                reply["refused"], "unsupported",
                "{name} {component}: {reply}"
            );
            untouched("once wait");
        }
        armed(&daemon, &pane, 0);
        let reply = hook(&daemon, request(&pane, "get_seed"));
        assert!(reply["seed"].is_null(), "{name}: {reply}");
        assert_eq!(reply["refused"], "get_seed_removed", "{name}: {reply}");
        untouched("get-seed");
        // A current receipt fetch is served.
        let mut req = request(&pane, "inbox_receipt_fetch");
        req["consumer"] = json!("cli");
        req["hook_integration"] = json!(1);
        let mut held = connect(&daemon, &req);
        let reply = line(&mut held);
        let handed = reply["messages"].as_array().cloned().unwrap_or_default();
        assert!(
            handed
                .iter()
                .any(|m| m["id"] == id.as_str() && m["a"].is_number()),
            "{name}: a current fetch is handed the task: {reply}"
        );
        state(&daemon, &pane, &id, "fetched");
    }
}
