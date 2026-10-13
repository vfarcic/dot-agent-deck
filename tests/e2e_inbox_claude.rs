#![cfg(all(feature = "e2e", unix))]

//! Real hook/wait CLI children of a scripted Claude parent. No model credential.
mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;

use inbox::*;
use serde_json::{Value, json};
use spec::spec;
use std::path::PathBuf;
use std::time::Duration;

const NATIVE_BODY: &str = "CLAUDE-NATIVE-BODY-8b9f: preserve\nsecond line";
const SUBJECT: &str = "native fixture";
const HUMAN: &str = "human-prompt-6c51";

fn claude_path(root: &std::path::Path) -> String {
    use std::os::unix::fs::PermissionsExt;
    let bin = root.join("claude-version-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let claude = bin.join("claude");
    std::fs::write(&claude, "#!/bin/sh\nprintf '2.1.296 (Claude Code)\\n'\n").unwrap();
    std::fs::set_permissions(claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    format!("{}:/usr/bin:/bin", bin.display())
}

fn sidecar(pane: &Pane, suffix: &str) -> PathBuf {
    pane.capture.with_extension(suffix)
}

fn parent(endpoint: &impl Endpoint, name: &str) -> Pane {
    let script = endpoint.cwd().join(format!("{name}.py"));
    std::fs::write(&script, r#"import os, sys, tty, termios, json, subprocess, glob, select, time
tty.setraw(0, termios.TCSANOW)
base, deck = sys.argv[1:]
def publish(suffix, value):
    path = base + '.' + suffix
    with open(path + '.tmp', 'w') as f: json.dump(value, f)
    os.replace(path + '.tmp', path)
with open(base + '.pid', 'w') as f: f.write(str(os.getpid()))
capture = open(base + '.bytes', 'ab', buffering=0)
os.write(1, b'CLAUDE-PARENT-READY\r\n')
running = {}
while True:
    for path in sorted(glob.glob(base + '.cmd-*')):
        with open(path) as f: request = json.load(f)
        os.unlink(path)
        tag = request['tag']
        payload = json.dumps(request['payload']).encode()
        if request['kind'] == 'wait':
            out = open(base + '.stdout-' + tag, 'wb')
            err = open(base + '.stderr-' + tag, 'wb')
            child = subprocess.Popen([deck, 'inbox', 'wait', '--agent', 'claude-code'], stdin=subprocess.PIPE, stdout=out, stderr=err)
            child.stdin.write(payload)
            child.stdin.close()
            publish('waiter-' + tag, child.pid)
            running[tag] = (child, out, err)
        else:
            result = subprocess.run([deck, 'hook', '--agent', 'claude-code'], input=payload, capture_output=True, timeout=15)
            publish('result-' + tag, {'code':result.returncode, 'stdout':result.stdout.decode(), 'stderr':result.stderr.decode()})
            os.write(1, b'HOOK-FINISHED:' + tag.encode() + b'\r\n')
    for tag, (child, out, err) in list(running.items()):
        code = child.poll()
        if code is not None:
            out.close(); err.close()
            publish('result-' + tag, {'code':code, 'stdout':open(base + '.stdout-' + tag).read(), 'stderr':open(base + '.stderr-' + tag).read()})
            del running[tag]
    if select.select([0], [], [], 0.025)[0]:
        chunk = os.read(0, 4096)
        if not chunk: break
        capture.write(chunk)
        os.write(1, chunk)
"#).unwrap();
    let base = endpoint.cwd().join(format!(".native-{name}"));
    let quote = |path: &std::path::Path| path.to_string_lossy().replace('\'', "'\\''");
    let command = common::capability_export_command(&format!(
        // Keep the managed shell alive after the scripted Claude dies. This
        // makes /007 test waiter parent-death handling, rather than the
        // daemon's whole-pane teardown killing the waiter for us.
        "python3 '{}' '{}' '{}'; exec /bin/sleep 600",
        quote(&script),
        quote(&base),
        quote(std::path::Path::new(env!("CARGO_BIN_EXE_dot-agent-deck")))
    ));
    let reply = attach(
        endpoint,
        json!({"op":"start-agent","command":command,
        "cwd":endpoint.cwd(),"rows":40,"cols":140,"agent_type":"claude_code",
        "env":[["DOT_AGENT_DECK_PANE_ID",name],["PATH",claude_path(endpoint.cwd())]],"display_name":name}),
    );
    assert_eq!(reply["ok"], true, "scripted Claude spawn: {reply}");
    let agent = reply["id"].as_str().unwrap().to_string();
    let pid_path = base.with_extension("pid");
    assert!(
        common::wait_until(common::child_boot_budget(), || std::fs::read_to_string(
            &pid_path
        )
        .is_ok_and(|text| text.parse::<u32>().is_ok())),
        "scripted Claude must export its pid"
    );
    let token = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(common::recorded_hook_capability(endpoint.cwd(), &agent))
        .unwrap();
    Pane {
        id: name.into(),
        agent,
        token,
        pid: std::fs::read_to_string(pid_path).unwrap().parse().unwrap(),
        capture: base.with_extension("bytes"),
        attach_socket: endpoint.attach_socket().into(),
    }
}

fn payload(pane: &Pane, event: &str) -> Value {
    json!({"hook_event_name":event,"session_id":format!("native-{}",pane.agent),"source":"startup"})
}

fn command(pane: &Pane, tag: &str, kind: &str, payload: Value) {
    let pending = sidecar(pane, &format!("pending-{tag}"));
    std::fs::write(
        &pending,
        serde_json::to_vec(&json!({"tag":tag,"kind":kind,"payload":payload})).unwrap(),
    )
    .unwrap();
    std::fs::rename(pending, sidecar(pane, &format!("cmd-{tag}"))).unwrap();
}

fn result(pane: &Pane, tag: &str) -> Value {
    let path = sidecar(pane, &format!("result-{tag}"));
    assert!(
        common::wait_until(common::load_scaled(Duration::from_secs(20)), || path
            .exists()),
        "hook/waiter {tag} must finish; stderr={:?}",
        std::fs::read_to_string(sidecar(pane, &format!("stderr-{tag}")))
    );
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn run_hook(pane: &Pane, tag: &str, payload: Value) -> Value {
    command(pane, tag, "hook", payload);
    let result = result(pane, tag);
    assert_eq!(result["code"], 0, "hook {tag}: {result}");
    result
}

fn wait(pane: &Pane, tag: &str, source: &str) -> u32 {
    let mut hook = payload(pane, "SessionStart");
    hook["source"] = json!(source);
    command(pane, tag, "wait", hook);
    let path = sidecar(pane, &format!("waiter-{tag}"));
    assert!(
        common::wait_until(common::child_boot_budget(), || path.exists()),
        "waiter must spawn"
    );
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn post(endpoint: &impl Endpoint, pane: &Pane, body: &str) -> String {
    let reply = attach(
        endpoint,
        json!({"op":"post-message","pane_id":pane.id,
        "expected_agent_id":pane.agent,"subject":SUBJECT,"body":body,"origin":"seed"}),
    );
    assert_eq!(reply["ok"], true, "native fixture post: {reply}");
    reply["id"].as_str().unwrap().into()
}

fn context(result: &Value, event: &str) -> String {
    let output: Value =
        serde_json::from_str(result["stdout"].as_str().unwrap()).unwrap_or_else(|error| {
            panic!("{event} must print hook-specific context JSON: {error}; actual={result}")
        });
    assert_eq!(output["hookSpecificOutput"]["hookEventName"], event);
    output["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("additionalContext string")
        .into()
}

fn record(endpoint: &impl Endpoint, pane: &Pane) -> Value {
    let reply = attach(endpoint, json!({"op":"list-agents"}));
    reply["agent_records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == pane.agent)
        .unwrap()
        .clone()
}

/// Scenario: Install the deck's waiter in Claude's settings and queue a spawn-time seed before scripted Claude reports SessionStart. Its real status hook must provide deck framing and native arm grace before Idle can ring; a subsequently armed waiter receives that seed on stderr with exit 2 and no automatic PTY bytes.
#[spec("inbox/claude/005")]
#[test]
fn claude_005_session_start_frames_and_announces_before_idle() {
    let daemon = daemon(&[("DOT_AGENT_DECK_INBOX_ARM_GRACE_MS", "30000")]);
    // A Claude that announces Native is one whose settings carry the deck's
    // waiter (review SF-2).
    seed_waiter_settings(daemon.home());
    let pane = parent(&daemon, "claude-boot");
    let id = post(&daemon, &pane, NATIVE_BODY);
    let out = run_hook(&pane, "start", payload(&pane, "SessionStart"));
    // The no-ring outcome is checked before parsing stdout: this also detects
    // a hook which frames correctly but announces after the Idle status.
    unchanged(&pane, Duration::from_millis(1000));
    assert!(
        bytes(&pane).is_empty(),
        "SessionStart must announce Native before the Idle that could ring the seed: {:?}",
        bytes(&pane)
    );
    let framing = context(&out, "SessionStart");
    assert!(
        framing.contains("dot-agent-deck") && framing.contains("message"),
        "deck framing: {framing}"
    );
    wait(&pane, "boot", "startup");
    let delivered = result(&pane, "boot");
    assert_eq!(delivered["code"], 2);
    assert_eq!(delivered["stdout"], "");
    assert!(delivered["stderr"].as_str().unwrap().contains(&id));
    assert!(delivered["stderr"].as_str().unwrap().contains(NATIVE_BODY));
    state(&daemon, &pane, &id, "fetched");
    assert!(
        bytes(&pane).is_empty(),
        "native content never uses PTY input"
    );
}

/// Scenario: Arm three real Once waiter children representing startup and two clears. Queue one task; only the newest waiter wakes with framed stderr and exit 2, while the two older waiters remain armed and no typed bytes enter Claude's pane.
#[spec("inbox/claude/006")]
#[test]
fn claude_006_clear_pileup_wakes_once_through_newest_waiter() {
    let daemon = daemon(&[]);
    let pane = parent(&daemon, "claude-pileup");
    for (index, source) in ["startup", "clear", "clear"].into_iter().enumerate() {
        wait(&pane, &format!("w{index}"), source);
        armed(&daemon, &pane, index as u64 + 1);
    }
    let id = post(&daemon, &pane, NATIVE_BODY);
    let delivered = result(&pane, "w2");
    assert_eq!(delivered["code"], 2, "Once delivery wakes Claude");
    assert_eq!(delivered["stdout"], "");
    let text = delivered["stderr"].as_str().unwrap();
    assert!(
        text.contains(NATIVE_BODY) && text.contains(&format!("[dot-agent-deck message {id}")),
        "framed native body: {text}"
    );
    state(&daemon, &pane, &id, "fetched");
    armed(&daemon, &pane, 2);
    for old in ["w0", "w1"] {
        assert!(
            !sidecar(&pane, &format!("result-{old}")).exists(),
            "older waiter duplicated the wake"
        );
    }
    assert!(bytes(&pane).is_empty());
}

/// Scenario: Arm a real Once waiter below a scripted Claude parent, then kill only that parent PID. The waiter must exit after losing its parent and the daemon must report zero armed waiters, even though stdin was already closed normally after the hook JSON.
#[spec("inbox/claude/007")]
#[test]
fn claude_007_waiter_exits_when_its_parent_dies() {
    let daemon = daemon(&[]);
    let pane = parent(&daemon, "claude-orphan");
    let waiter = wait(&pane, "orphan", "startup");
    struct WaiterGuard(u32);
    impl Drop for WaiterGuard {
        fn drop(&mut self) {
            if common::process_running(self.0 as i32) {
                // SAFETY: this PID was spawned by this fixture's parent only.
                unsafe { libc::kill(self.0 as i32, libc::SIGKILL) };
            }
        }
    }
    let _waiter_guard = WaiterGuard(waiter);
    armed(&daemon, &pane, 1);
    // SAFETY: this is the exact fixture-owned parent PID exported above.
    assert_eq!(unsafe { libc::kill(pane.pid as i32, libc::SIGKILL) }, 0);
    assert_ne!(
        record(&daemon, &pane)["crashed"],
        true,
        "fixture supervisor must keep the managed pane alive"
    );
    assert!(
        common::wait_until(common::load_scaled(Duration::from_secs(6)), || {
            !common::process_running(waiter as i32)
        }),
        "Once waiter {waiter} must exit when scripted Claude parent {} dies",
        pane.pid
    );
    armed(&daemon, &pane, 0);
}

/// Scenario: Give scripted Claude a leading-space-trimmed doorbell and then the original fixed line as UserPromptSubmit input. Each hook must fetch and acknowledge the queued message, inject its framed content via additionalContext, and leave ordinary human prompts without an injected context.
#[spec("inbox/claude/008")]
#[test]
fn claude_008_doorbell_injects_content_even_when_leading_space_is_trimmed() {
    let daemon = daemon(&[]);
    let pane = parent(&daemon, "claude-inject");
    for (index, trimmed) in [true, false].into_iter().enumerate() {
        let body = format!("{NATIVE_BODY}-{index}");
        let id = post(&daemon, &pane, &body);
        let fixed = fixed_line(&id);
        let mut hook = payload(&pane, "UserPromptSubmit");
        hook["prompt"] = json!(if trimmed { fixed.trim_start() } else { &fixed });
        let output = run_hook(&pane, &format!("inject{index}"), hook);
        let content = context(&output, "UserPromptSubmit");
        assert!(
            content.contains(&body) && content.contains(&format!("[dot-agent-deck message {id}")),
            "hook must inject queued content: {content}"
        );
        state(&daemon, &pane, &id, "acknowledged");
        assert_eq!(
            listing(&daemon, &pane)["messages"]
                .as_array()
                .unwrap()
                .len(),
            index + 1
        );
    }
    let mut ordinary = payload(&pane, "UserPromptSubmit");
    ordinary["prompt"] = json!(HUMAN);
    let output = run_hook(&pane, "human", ordinary);
    assert_eq!(
        output["stdout"], "",
        "ordinary prompts have no deck injection"
    );
}

/// Scenario: Submit a human prompt in an attached scripted Claude pane, then deliver a framed native waiter notification. The notification must acknowledge the message and display its subject with an envelope while preserving the human prompt history instead of recording task-notification markup as a user prompt.
#[spec("inbox/claude/009")]
#[test]
fn claude_009_native_notification_acks_and_shows_subject_without_polluting_user_history() {
    let deck = common::TuiDeck::builder()
        .with_pty_size(160, 45)
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");
    let pane = parent(&deck, "claude-card");
    run_hook(&pane, "start", payload(&pane, "SessionStart"));
    let mut human = payload(&pane, "UserPromptSubmit");
    human["prompt"] = json!(HUMAN);
    run_hook(&pane, "human", human);
    wait(&pane, "wake", "startup");
    armed(&deck, &pane, 1);
    let id = post(&deck, &pane, NATIVE_BODY);
    let delivered = result(&pane, "wake");
    assert_eq!(delivered["code"], 2);
    state(&deck, &pane, &id, "fetched");
    let mut hook = payload(&pane, "UserPromptSubmit");
    hook["prompt"] = json!(format!(
        "<task-notification><system-reminder>Stop hook blocking error from command inbox wait: {}</system-reminder></task-notification>",
        delivered["stderr"].as_str().unwrap()
    ));
    run_hook(&pane, "notification", hook);
    state(&deck, &pane, &id, "acknowledged");
    assert!(
        deck.wait_for_grid_string_within(&format!("✉ {SUBJECT}"), WAIT),
        "card must identify a deck message by its subject: {}",
        deck.snapshot_grid()
    );
    let live = record(&deck, &pane)["live"].clone();
    assert_eq!(
        live["last_user_prompt"], HUMAN,
        "deck notification must preserve last human prompt: {live}"
    );
    assert_eq!(
        live["first_prompts"],
        json!([HUMAN]),
        "deck notification must not enter human prompt history: {live}"
    );
    assert!(!deck.snapshot_grid().contains("<task-notification>"));
}

/// Run the real Claude SessionStart hook in `home` against an isolated
/// recording hook socket; the hook's output and every line each connection
/// carried.
fn record_session_start(home: &std::path::Path) -> (std::process::Output, Vec<Vec<Value>>) {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::process::{Command, Stdio};
    let socket = home.join("hook.sock");
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).unwrap();
    let reader = std::thread::spawn(move || {
        let mut connections = Vec::new();
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(10)))
                        .unwrap();
                    let mut stream = BufReader::new(stream);
                    let mut lines = Vec::new();
                    loop {
                        let mut line = String::new();
                        match stream.read_line(&mut line) {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                let value: Value = serde_json::from_str(&line).unwrap();
                                if value["recording_complete"] == true {
                                    return connections;
                                }
                                if value["message_type"] == "inbox_announce" {
                                    writeln!(stream.get_mut(), "{{\"accepted\":true}}").unwrap();
                                }
                                lines.push(value);
                            }
                        }
                    }
                    connections.push(lines);
                }
                Err(error) => panic!("recording hook socket: {error}"),
            }
        }
    });
    let mut hook = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args(["hook", "--agent", "claude-code"])
        .env_clear()
        .env("HOME", home)
        .env("PATH", claude_path(home))
        .env("DOT_AGENT_DECK_SOCKET", &socket)
        .env("DOT_AGENT_DECK_PANE_ID", "recorded-claude")
        .env("DOT_AGENT_DECK_AGENT_ID", "recorded-agent")
        .env("DOT_AGENT_DECK_PANE_CAPABILITY", "a".repeat(64))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    hook.stdin.take().unwrap().write_all(br#"{"hook_event_name":"SessionStart","session_id":"ordered-native","source":"startup"}"#).unwrap();
    let output = hook.wait_with_output().unwrap();
    // A separate final connection wakes accept after the CLI has exited;
    // no polling delay is needed to settle the recorded connections.
    let mut complete = UnixStream::connect(&socket).unwrap();
    writeln!(complete, "{{\"recording_complete\":true}}").unwrap();
    let connections = reader.join().unwrap();
    (output, connections)
}

/// The Claude settings a supported install leaves: SessionStart's status
/// handler and the deck's native waiter.
fn seed_waiter_settings(home: &std::path::Path) {
    let dir = home.join(".claude");
    std::fs::create_dir_all(&dir).unwrap();
    let settings = json!({"hooks":{"SessionStart":[{"hooks":[
        {"type":"command","command":"/opt/dot-agent-deck hook --agent claude-code"},
        {"type":"command","command":"/opt/dot-agent-deck inbox wait --agent claude-code",
         "asyncRewake":true,"timeout":86400}
    ]}]}});
    std::fs::write(dir.join("settings.json"), settings.to_string()).unwrap();
}

/// Scenario: Run the real Claude SessionStart hook, with the deck's native waiter installed in Claude's settings, against an isolated recording hook socket. Its Native announcement and status event must arrive in that order on one connection, so a queued seed cannot become eligible to ring between them.
#[spec("inbox/claude/010")]
#[test]
fn claude_010_session_start_announces_before_status_on_one_connection() {
    let scratch = common::harness_tempdir().unwrap();
    seed_waiter_settings(scratch.path());
    let (output, connections) = record_session_start(scratch.path());
    assert!(output.status.success(), "status hook: {output:?}");
    assert_eq!(
        connections.len(),
        1,
        "announce and status must share one connection: {connections:?}"
    );
    assert_eq!(
        connections[0].len(),
        2,
        "SessionStart must send Native announce then status: {connections:?}"
    );
    assert_eq!(connections[0][0]["message_type"], "inbox_announce");
    assert_eq!(connections[0][0]["wake"], "native");
    assert_eq!(connections[0][1]["event_type"], "session_start");
}

/// Scenario: Run the real Claude SessionStart hook in a HOME whose Claude settings carry no deck waiter, as a Claude below the native-wake version gate is installed, and again with no settings at all. The hook must send its status event and no Native announcement, so the doorbell rings without waiting out the arm grace for a waiter that will never arm.
#[spec("inbox/claude/015")]
#[test]
fn claude_015_session_start_without_a_waiter_announces_nothing() {
    let scratch = common::harness_tempdir().unwrap();
    let (output, connections) = record_session_start(scratch.path());
    assert!(output.status.success(), "status hook: {output:?}");
    let dir = scratch.path().join(".claude");
    std::fs::create_dir_all(&dir).unwrap();
    let below_gate = json!({"hooks":{"SessionStart":[{"hooks":[
        {"type":"command","command":"/opt/dot-agent-deck hook --agent claude-code"}
    ]}]}});
    std::fs::write(dir.join("settings.json"), below_gate.to_string()).unwrap();
    let (again, more) = record_session_start(scratch.path());
    assert!(again.status.success(), "status hook: {again:?}");
    for lines in connections.iter().chain(&more) {
        assert!(
            lines
                .iter()
                .all(|line| line["message_type"] != "inbox_announce"),
            "no waiter, no Native announcement: {lines:?}"
        );
    }
    let statuses = connections
        .iter()
        .chain(&more)
        .flatten()
        .filter(|line| line["event_type"] == "session_start")
        .count();
    assert_eq!(statuses, 2, "{connections:?} {more:?}");
}
