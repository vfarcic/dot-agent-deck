#![cfg(all(feature = "e2e", unix))]

//! Real hook/wait CLI children of a scripted Claude parent. No model credential.
mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;

use inbox::*;
use serde_json::{Value, json};
use spec::spec;
use std::cell::{Cell, RefCell};
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
    parent_as(endpoint, name, Some("claude_code"))
}

/// [`parent`] spawned as `agent_type`; `None` starts it from its command
/// alone, as a role or a resumed session is, with no agent type to go by.
fn parent_as(endpoint: &impl Endpoint, name: &str, agent_type: Option<&str>) -> Pane {
    parent_with(endpoint, name, agent_type, false)
}

/// [`parent`] drawing the e2e stand-in profile's screen and selecting that
/// profile, so the doorbell types into it (PRD #1666 part 3, round 5-wake: in
/// an e2e build the deck types into no Claude Code release, since none is
/// measured there).
fn parent_profiled(endpoint: &impl Endpoint, name: &str) -> Pane {
    parent_with(endpoint, name, Some("claude_code"), true)
}

fn parent_with(
    endpoint: &impl Endpoint,
    name: &str,
    agent_type: Option<&str>,
    profiled: bool,
) -> Pane {
    let script = endpoint.cwd().join(format!("{name}.py"));
    std::fs::write(&script, r#"import os, sys, tty, termios, json, subprocess, glob, select, time
tty.setraw(0, termios.TCSANOW)
base, deck = sys.argv[1:3]
profiled = sys.argv[3:] == ['profiled']
line = bytearray()
taken = []
def redraw():
    cols = os.get_terminal_size(0).columns
    content = bytes(line) if line else b'\x1b[2m<empty>\x1b[22m'
    os.write(1, b'\x1b[2J\x1b[HDOORBELL-STANDIN-READY\r\n' + b'W' * cols + b'\r\nCLAUDE-PARENT-READY\r\n'
             + b''.join(b'SUBMITTED:' + t + b'\r\n' for t in taken[-5:])
             + b'COMPOSER: ' + content)
def publish(suffix, value):
    path = base + '.' + suffix
    with open(path + '.tmp', 'w') as f: json.dump(value, f)
    os.replace(path + '.tmp', path)
with open(base + '.pid', 'w') as f: f.write(str(os.getpid()))
capture = open(base + '.bytes', 'ab', buffering=0)
if profiled:
    redraw()
else:
    os.write(1, b'CLAUDE-PARENT-READY\r\n')
running = {}
while True:
    for path in sorted(glob.glob(base + '.cmd-*')):
        with open(path) as f: request = json.load(f)
        os.unlink(path)
        tag = request['tag']
        payload = json.dumps(request['payload']).encode()
        env = dict(os.environ)
        env.update(request.get('env', {}))
        if request['kind'] == 'wait':
            out = open(base + '.stdout-' + tag, 'wb')
            err = open(base + '.stderr-' + tag, 'wb')
            argv = ['/bin/sh', '-c', request['shell']] if 'shell' in request else [deck, 'inbox', 'wait', '--agent', 'claude-code']
            child = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=out, stderr=err, env=env)
            child.stdin.write(payload)
            child.stdin.close()
            publish('waiter-' + tag, child.pid)
            running[tag] = (child, out, err)
        elif request['kind'] == 'raw':
            import socket
            conn = socket.socket(socket.AF_UNIX)
            conn.settimeout(15)
            conn.connect(request['socket'])
            conn.sendall(payload + b'\n')
            reply = b''
            while not reply.endswith(b'\n'):
                chunk = conn.recv(4096)
                if not chunk: break
                reply += chunk
            conn.close()
            publish('result-' + tag, {'code':0, 'stdout':reply.decode(), 'stderr':''})
        else:
            argv = ['/bin/sh', '-c', request['shell']] if 'shell' in request else [deck, 'hook', '--agent', 'claude-code']
            result = subprocess.run(argv, input=payload, capture_output=True, timeout=15, env=env)
            publish('result-' + tag, {'code':result.returncode, 'stdout':result.stdout.decode(), 'stderr':result.stderr.decode()})
            if not profiled:
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
        if not profiled:
            os.write(1, chunk)
            continue
        for c in chunk:
            if c == 13:
                taken.append(bytes(line))
                line.clear()
            elif c == 21:
                line.clear()
            elif c in (8, 127):
                if line: line.pop()
            elif c >= 32:
                line.append(c)
        redraw()
"#).unwrap();
    let base = endpoint.cwd().join(format!(".native-{name}"));
    let quote = |path: &std::path::Path| path.to_string_lossy().replace('\'', "'\\''");
    let command = common::capability_export_command(&format!(
        // Keep the managed shell alive after the scripted Claude dies. This
        // makes /007 test waiter parent-death handling, rather than the
        // daemon's whole-pane teardown killing the waiter for us.
        "{}python3 '{}' '{}' '{}'{}",
        // The profiled stand-in is the pane's own process, as the profile
        // requires, so nothing outlives it.
        if profiled { "exec " } else { "" },
        quote(&script),
        quote(&base),
        quote(std::path::Path::new(env!("CARGO_BIN_EXE_dot-agent-deck"))),
        if profiled {
            " profiled"
        } else {
            "; exec /bin/sleep 600"
        }
    ));
    let mut start = json!({"op":"start-agent","command":command,
        "cwd":endpoint.cwd(),"rows":40,"cols":140,
        "env":[["DOT_AGENT_DECK_PANE_ID",name],["PATH",claude_path(endpoint.cwd())]],"display_name":name});
    if let Some(agent_type) = agent_type {
        start["agent_type"] = json!(agent_type);
    }
    if profiled {
        let env = start["env"].as_array_mut().unwrap();
        env.push(json!(["DOT_AGENT_DECK_TEST_COMPOSER_PROFILE", "standin"]));
        env.push(json!(["DOT_AGENT_DECK_TEST_COMPOSER_BINARY", script]));
    }
    let reply = attach(endpoint, start);
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
    command_with(pane, tag, json!({"kind":kind,"payload":payload}));
}

/// [`command`] with the request's extra fields: `env` for the child, and for
/// a waiter or a hook `shell`, the command line to run instead of the test's
/// own deck.
fn command_with(pane: &Pane, tag: &str, mut request: Value) {
    request["tag"] = json!(tag);
    let pending = sidecar(pane, &format!("pending-{tag}"));
    std::fs::write(&pending, serde_json::to_vec(&request).unwrap()).unwrap();
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

/// Scenario: Arm three real Once waiter children one after another, representing startup and two clears. Each newer waiter takes the older one's place, which exits quietly with nothing on stdout or stderr, so only one is ever armed. A queued task then wakes only the newest, with framed stderr and exit 2, and no typed bytes enter Claude's pane.
#[spec("inbox/claude/006")]
#[test]
fn claude_006_clear_pileup_wakes_once_through_newest_waiter() {
    let daemon = daemon(&[]);
    let pane = parent(&daemon, "claude-pileup");
    for (index, source) in ["startup", "clear", "clear"].into_iter().enumerate() {
        wait(&pane, &format!("w{index}"), source);
        armed(&daemon, &pane, 1);
        if index > 0 {
            let older = result(&pane, &format!("w{}", index - 1));
            assert_eq!(
                older["code"], 0,
                "a superseded waiter exits quietly: {older}"
            );
            assert_eq!(older["stdout"], "");
            assert_eq!(older["stderr"], "", "a superseded waiter wakes nothing");
        }
    }
    let id = post(&daemon, &pane, NATIVE_BODY);
    let delivered = result(&pane, "w2");
    assert_eq!(delivered["code"], 2, "Once delivery wakes Claude");
    assert_eq!(delivered["stdout"], "");
    let text = delivered["stderr"].as_str().unwrap();
    assert!(
        text.contains(NATIVE_BODY)
            && text.contains(&format!("[dot-agent-deck message {id} delivery 1.1 from ")),
        "framed native body naming its attempt: {text}"
    );
    state(&daemon, &pane, &id, "fetched");
    armed(&daemon, &pane, 0);
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

/// Scenario: Submit a human prompt in an attached scripted Claude pane, then deliver a framed native waiter notification. The notification must commit the message's exact attempt, settling it, and display its subject with an envelope while preserving the human prompt history instead of recording task-notification markup as a user prompt.
#[spec("inbox/claude/009")]
#[test]
fn claude_009_native_notification_settles_and_shows_subject_without_polluting_user_history() {
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

/// Every cap the deck wrote for `id`'s attempts into a spool under `root`:
/// the hex lines of each caps file named for it.
fn caps_of(root: &std::path::Path, id: &str) -> Vec<String> {
    fn walk(dir: &std::path::Path, id: &str, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, id, out);
            } else if path.parent().and_then(|p| p.file_name()) == Some("caps".as_ref())
                && entry.file_name().to_string_lossy().starts_with(id)
                && let Ok(text) = std::fs::read_to_string(&path)
            {
                out.extend(text.lines().map(str::to_string));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, id, &mut out);
    out
}

/// Whether `text` holds a run of 32 lowercase hex digits, the shape of a
/// receipt cap.
fn holds_cap_shaped(text: &str) -> bool {
    let mut run = 0;
    for c in text.chars() {
        run = if c.is_ascii_digit() || ('a'..='f').contains(&c) {
            run + 1
        } else {
            0
        };
        if run >= 32 {
            return true;
        }
    }
    false
}

/// A UserPromptSubmit hook payload carrying `prompt`, as Claude submits a
/// waiter's notification.
fn notification(pane: &Pane, stderr: &str) -> Value {
    let mut hook = payload(pane, "UserPromptSubmit");
    hook["prompt"] = json!(format!(
        "<task-notification><system-reminder>Stop hook blocking error from command inbox wait: {stderr}</system-reminder></task-notification>"
    ));
    hook
}

fn message_of(endpoint: &impl Endpoint, pane: &Pane, id: &str) -> Value {
    listing(endpoint, pane)["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == id)
        .cloned()
        .unwrap_or_else(|| panic!("missing message {id}"))
}

/// Scenario: Arm a real Claude waiter and queue a task. The waiter wakes Claude with the task's block on stderr, its opener naming the attempt as `delivery 1.1`, and no cap the deck wrote for it appears anywhere in its output. A submitted prompt carrying that block with another attempt's number, or with none, commits nothing; the real notification, submitted through the real prompt hook, commits exactly that attempt, which settles the task, and the hook prints nothing that carries a cap. A waiter armed again at Stop afterwards is handed nothing.
#[spec("inbox/claude/016")]
#[test]
fn claude_016_a_notification_commits_its_exact_attempt_through_its_caps_file() {
    let daemon = daemon(&[]);
    let pane = parent(&daemon, "claude-exact");
    wait(&pane, "wake", "startup");
    armed(&daemon, &pane, 1);
    let id = post(&daemon, &pane, NATIVE_BODY);
    let delivered = result(&pane, "wake");
    assert_eq!(delivered["code"], 2, "the waiter wakes Claude: {delivered}");
    let stderr = delivered["stderr"].as_str().unwrap().to_string();
    assert!(
        stderr.contains(&format!("[dot-agent-deck message {id} delivery 1.1 from "))
            && stderr.contains(NATIVE_BODY),
        "the block names its attempt: {stderr}"
    );
    // The spool is under the daemon's state directory, beside its HOME.
    let caps = caps_of(daemon.home().parent().unwrap(), &id);
    assert_eq!(caps.len(), 2, "the deck wrote the attempt's two caps");
    for cap in &caps {
        assert!(!stderr.contains(cap.as_str()), "no cap on stderr");
    }
    assert!(
        !holds_cap_shaped(&stderr),
        "nothing cap-shaped on stderr: {stderr}"
    );
    state(&daemon, &pane, &id, "fetched");

    for (tag, forged) in [
        (
            "other-attempt",
            stderr.replace("delivery 1.1", "delivery 1.2"),
        ),
        ("no-attempt", stderr.replace(" delivery 1.1", "")),
    ] {
        let out = run_hook(&pane, tag, notification(&pane, &forged));
        assert_eq!(out["stdout"], "", "{tag}: nothing to hand over");
        state(&daemon, &pane, &id, "fetched");
    }

    let out = run_hook(&pane, "notification", notification(&pane, &stderr));
    state(&daemon, &pane, &id, "acknowledged");
    for text in [&out["stdout"], &out["stderr"]] {
        let text = text.as_str().unwrap();
        assert!(!holds_cap_shaped(text), "the hook prints no cap: {text}");
    }
    let mut stop = payload(&pane, "Stop");
    stop["source"] = json!("stop");
    command(&pane, "rearm", "wait", stop);
    armed(&daemon, &pane, 1);
    unchanged(&pane, Duration::from_millis(1500));
    assert!(
        !sidecar(&pane, "result-rearm").exists(),
        "a settled task is handed to no later waiter"
    );
    assert_eq!(message_of(&daemon, &pane, &id)["deliveries"], 1);
}

/// Scenario: With a short ack-wait, arm a real Claude waiter and queue a task. The waiter delivers it and no prompt carrying it is ever submitted, while the agent stays idle; a waiter armed again receives one marked repeat of it, naming attempt 1.2 and saying it may repeat the earlier delivery. That one is lost too, and so is a second marked repeat; after the next idle grace the task is uncertain, offered for Retry or Cancel, and the waiter armed after it is handed nothing more.
#[spec("inbox/claude/017")]
#[test]
fn claude_017_a_lost_notification_repeats_marked_then_turns_uncertain() {
    let daemon = daemon(&[("DOT_AGENT_DECK_INBOX_ACK_WAIT_MS", "1500")]);
    let pane = parent(&daemon, "claude-lost");
    wait(&pane, "first", "startup");
    armed(&daemon, &pane, 1);
    let id = post(&daemon, &pane, NATIVE_BODY);
    let first = result(&pane, "first");
    assert_eq!(first["code"], 2, "{first}");
    let repeat_line = format!("This may repeat dot-agent-deck message {id}");
    assert!(!first["stderr"].as_str().unwrap().contains(&repeat_line));
    for (tag, e) in [("second", 2), ("third", 3)] {
        wait(&pane, tag, "stop");
        armed(&daemon, &pane, 1);
        let again = result(&pane, tag);
        assert_eq!(again["code"], 2, "{tag}: {again}");
        let text = again["stderr"].as_str().unwrap();
        assert!(
            text.contains(&format!(
                "[dot-agent-deck message {id} delivery 1.{e} from "
            )) && text.contains(&repeat_line),
            "{tag}: a marked repeat: {text}"
        );
    }
    wait(&pane, "after", "stop");
    let uncertain = || {
        record(&daemon, &pane)["inbox"]["attention"]
            .as_array()
            .is_some_and(|items| {
                items
                    .iter()
                    .any(|item| item["message_id"] == id && item["kind"] == "uncertain")
            })
    };
    assert!(
        common::wait_until(common::load_scaled(Duration::from_secs(15)), uncertain),
        "uncertain after the repeats ran out: {}",
        record(&daemon, &pane)
    );
    unchanged(&pane, Duration::from_millis(500));
    assert!(!sidecar(&pane, "result-after").exists(), "no third repeat");
    assert_eq!(message_of(&daemon, &pane, &id)["deliveries"], 3);
}

/// Scenario: With a short keepalive and ack-wait, a real Claude waiter armed at the end of the last turn delivers a task while the agent is in a tool call that reports nothing further for longer than three keepalives and two ack-waits. No repeat is handed out while the tool runs. The turn then ends, a waiter is armed again, and the queued notification is submitted: the task is committed once, with one delivery, and the new waiter is handed nothing.
#[spec("inbox/claude/018")]
#[test]
fn claude_018_a_notification_queued_behind_a_long_quiet_tool_commits_once() {
    let daemon = daemon(&[
        ("DOT_AGENT_DECK_INBOX_ACK_WAIT_MS", "2000"),
        ("DOT_AGENT_DECK_INBOX_KEEPALIVE_MS", "200"),
    ]);
    let pane = parent(&daemon, "claude-quiet");
    wait(&pane, "armed", "stop");
    armed(&daemon, &pane, 1);
    let mut human = payload(&pane, "UserPromptSubmit");
    human["prompt"] = json!(HUMAN);
    run_hook(&pane, "prompt", human);
    let mut tool = payload(&pane, "PreToolUse");
    tool["tool_name"] = json!("Bash");
    tool["tool_input"] = json!({"command": "sleep 150"});
    run_hook(&pane, "tool", tool);
    let id = post(&daemon, &pane, NATIVE_BODY);
    let delivered = result(&pane, "armed");
    assert_eq!(delivered["code"], 2, "{delivered}");
    let stderr = delivered["stderr"].as_str().unwrap().to_string();
    unchanged(&pane, common::load_scaled(Duration::from_millis(4500)));
    assert_eq!(
        message_of(&daemon, &pane, &id)["deliveries"],
        1,
        "nothing is repeated while the turn holds the notification"
    );
    state(&daemon, &pane, &id, "fetched");
    let mut stop = payload(&pane, "Stop");
    stop["stop_hook_active"] = json!(false);
    run_hook(&pane, "stop", stop);
    wait(&pane, "rearm", "stop");
    armed(&daemon, &pane, 1);
    run_hook(&pane, "notification", notification(&pane, &stderr));
    state(&daemon, &pane, &id, "acknowledged");
    unchanged(&pane, Duration::from_millis(1200));
    assert!(!sidecar(&pane, "result-rearm").exists());
    assert_eq!(message_of(&daemon, &pane, &id)["deliveries"], 1);
}

/// Scenario: In a pane started from its command alone, with no agent type, and with no waiter armed and a short ack-wait, give the real Claude prompt hook a doorbell line for a queued task: it takes the task as a receiver of its own, prints it as additionalContext with its opener naming the attempt, and that commits it, with nothing cap-shaped in what it prints. A second task's doorbell is then given to a hook that is killed after the deck approved its hand-over and before it printed anything: the task is not lost and not shown as delivered, and the next doorbell hands it over once more, marked as a possible repeat, which commits it.
#[spec("inbox/claude/019")]
#[test]
fn claude_019_a_doorbell_hook_commits_what_it_printed_and_a_killed_one_repeats_marked() {
    let daemon = daemon(&[("DOT_AGENT_DECK_INBOX_ACK_WAIT_MS", "1500")]);
    let pane = parent_as(&daemon, "claude-doorbell", None);
    let id = post(&daemon, &pane, NATIVE_BODY);
    let mut hook = payload(&pane, "UserPromptSubmit");
    hook["prompt"] = json!(fixed_line(&id));
    let out = run_hook(&pane, "inject", hook);
    let context = context(&out, "UserPromptSubmit");
    assert!(
        context.contains(&format!("[dot-agent-deck message {id} delivery 1.1 from "))
            && context.contains(NATIVE_BODY),
        "{context}"
    );
    assert!(!holds_cap_shaped(out["stdout"].as_str().unwrap()), "{out}");
    assert!(!holds_cap_shaped(out["stderr"].as_str().unwrap()), "{out}");
    state(&daemon, &pane, &id, "acknowledged");

    let second = post(&daemon, &pane, &format!("{NATIVE_BODY}-second"));
    let mut hook = payload(&pane, "UserPromptSubmit");
    hook["prompt"] = json!(fixed_line(&second));
    command_with(
        &pane,
        "killed",
        json!({"kind":"hook","payload":hook,
               "env":{"DOT_AGENT_DECK_INBOX_TEST_EXIT_AFTER_GRANT":"1"}}),
    );
    let killed = result(&pane, "killed");
    assert_eq!(
        killed["stdout"], "",
        "the killed hook printed nothing: {killed}"
    );
    state(&daemon, &pane, &second, "fetched");
    let repeat_line = format!("This may repeat dot-agent-deck message {second}");
    let again = RefCell::new(String::new());
    let tries = Cell::new(0);
    assert!(
        common::wait_until(common::load_scaled(Duration::from_secs(15)), || {
            let mut hook = payload(&pane, "UserPromptSubmit");
            hook["prompt"] = json!(fixed_line(&second));
            tries.set(tries.get() + 1);
            let out = run_hook(&pane, &format!("retry-{}", tries.get()), hook);
            let printed = out["stdout"].as_str().unwrap_or_default().to_string();
            *again.borrow_mut() = format!("{}{printed}|", again.borrow());
            printed.contains(&repeat_line)
        }),
        "the next doorbell hands it over marked: {}",
        again.borrow()
    );
    state(&daemon, &pane, &second, "acknowledged");
    assert_eq!(message_of(&daemon, &pane, &second)["deliveries"], 2);
}

/// Scenario: Claude's settings name a dot-agent-deck too old for the deck's messages as the waiter. The waiter it starts is refused before anything is issued: it exits quietly, a queued task stays queued with no doorbell typed, and the agent's card names the hooks-too-old notice with the command the deck gives to fix it. Running `hooks install --agent claude-code` from the deck's own binary rewrites the settings; the waiter they now name registers, is handed the task, and the notice is gone.
#[spec("inbox/claude/020")]
#[test]
fn claude_020_hooks_too_old_names_the_fix_and_the_fix_delivers() {
    use std::os::unix::fs::PermissionsExt;
    let daemon = daemon(&[]);
    let home = daemon.home().to_path_buf();
    let old = home.join("old-deck/dot-agent-deck");
    std::fs::create_dir_all(old.parent().unwrap()).unwrap();
    std::fs::write(
        &old,
        format!(
            "#!/bin/sh\nDOT_AGENT_DECK_INBOX_TEST_HOOK_INTEGRATION=none exec '{}' \"$@\"\n",
            env!("CARGO_BIN_EXE_dot-agent-deck")
        ),
    )
    .unwrap();
    std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o755)).unwrap();
    let settings = home.join(".claude/settings.json");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    let rule = json!([{"hooks":[
        {"type":"command","command":format!("'{}' hook --agent claude-code", old.display())},
        {"type":"command","command":format!("'{}' inbox wait --agent claude-code", old.display()),
         "asyncRewake":true,"timeout":86400}
    ]}]);
    std::fs::write(
        &settings,
        json!({"hooks":{"SessionStart":rule,"Stop":rule}}).to_string(),
    )
    .unwrap();
    let waiter_command = || {
        let value: Value = serde_json::from_slice(&std::fs::read(&settings).unwrap()).unwrap();
        value["hooks"]["SessionStart"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|rule| rule["hooks"].as_array().unwrap().clone())
            .find_map(|hook| {
                hook["command"]
                    .as_str()
                    .filter(|command| command.ends_with(" inbox wait --agent claude-code"))
                    .map(str::to_string)
            })
            .expect("a waiter in Claude's settings")
    };
    let pane = parent(&daemon, "claude-stale");
    command_with(
        &pane,
        "stale",
        json!({"kind":"wait","shell":waiter_command(),"payload":payload(&pane, "SessionStart")}),
    );
    let refused = result(&pane, "stale");
    assert_eq!(refused["code"], 0, "refused quietly: {refused}");
    assert_eq!(refused["stderr"], "");
    let notice = || record(&daemon, &pane)["inbox"].clone();
    assert!(
        common::wait_until(WAIT, || notice()["integration"] == "hooks_too_old"),
        "the card names the stale hooks: {}",
        notice()
    );
    let command = notice()["integration_command"].clone();
    if let Some(command) = command.as_str() {
        assert!(
            command.ends_with(" hooks install --agent claude-code"),
            "the fix is the deck's own hooks install: {command}"
        );
    }
    armed(&daemon, &pane, 0);
    let id = post(&daemon, &pane, NATIVE_BODY);
    unchanged(&pane, Duration::from_millis(1500));
    state(&daemon, &pane, &id, "queued");
    assert!(
        bytes(&pane).is_empty(),
        "no doorbell for hooks that cannot take it"
    );

    // The harness seeds the deck's durable copy into HOME; the daemon may be
    // running it, so it is used as it is, never written over.
    let installed = home.join(".local/bin/dot-agent-deck");
    if !installed.exists() {
        std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
        if std::fs::hard_link(env!("CARGO_BIN_EXE_dot-agent-deck"), &installed).is_err() {
            std::fs::copy(env!("CARGO_BIN_EXE_dot-agent-deck"), &installed).unwrap();
        }
    }
    let install = std::process::Command::new(&installed)
        .args(["hooks", "install", "--agent", "claude-code"])
        .env_clear()
        .env("HOME", &home)
        .env("PATH", claude_path(&home))
        .current_dir(&home)
        .output()
        .unwrap();
    assert!(install.status.success(), "hooks install: {install:?}");
    let fixed = waiter_command();
    assert!(
        !fixed.contains(&old.display().to_string()),
        "the install re-pinned the waiter: {fixed}"
    );
    command_with(
        &pane,
        "fixed",
        json!({"kind":"wait","shell":fixed,"payload":payload(&pane, "SessionStart")}),
    );
    let delivered = result(&pane, "fixed");
    assert_eq!(
        delivered["code"], 2,
        "the fixed waiter is handed the task: {delivered}"
    );
    assert!(delivered["stderr"].as_str().unwrap().contains(NATIVE_BODY));
    state(&daemon, &pane, &id, "fetched");
    assert!(notice()["integration"].is_null(), "{}", notice());
}

/// The `caps` directory of the one generation spool under `root`.
fn caps_dir(root: &std::path::Path) -> PathBuf {
    fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if entry.file_name() == "caps" {
                    out.push(path);
                } else {
                    walk(&path, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    assert_eq!(out.len(), 1, "one generation spool: {out:?}");
    out.remove(0)
}

/// Send `request` to the deck's hook socket from inside `pane`'s agent, as a
/// process the agent runs could, and return its one-line answer.
fn raw(daemon: &impl Endpoint, pane: &Pane, tag: &str, request: Value) -> Value {
    command_with(
        pane,
        tag,
        json!({"kind":"raw","payload":request,"socket":daemon.hook_socket()}),
    );
    let out = result(pane, tag);
    serde_json::from_str(out["stdout"].as_str().unwrap().trim())
        .unwrap_or_else(|error| panic!("{tag}: a JSON answer: {error}; {out}"))
}

/// Scenario: In a pane started from its command alone, with no agent type and no deck waiter in Claude's settings, the real Claude SessionStart hook runs, so Claude's own hook event is all that says the agent is Claude. A queued task's doorbell is then given to a prompt hook too old for the deck's messages. Its fetch is refused as hooks too old instead of being served the old way: the hook prints nothing of the task, the task stays queued, and the agent's card names the hooks-too-old notice.
#[spec("inbox/claude/022")]
#[test]
fn claude_022_a_stale_hook_in_a_bare_claude_pane_is_refused_visibly() {
    let daemon = daemon(&[]);
    let pane = parent_as(&daemon, "claude-bare-stale", None);
    run_hook(&pane, "start", payload(&pane, "SessionStart"));
    assert!(
        common::wait_until(WAIT, || record(&daemon, &pane)["agent_type"]
            == "claude_code"),
        "Claude's hook event typed the pane: {}",
        record(&daemon, &pane)
    );
    let id = post(&daemon, &pane, NATIVE_BODY);
    let mut hook = payload(&pane, "UserPromptSubmit");
    hook["prompt"] = json!(fixed_line(&id));
    command_with(
        &pane,
        "stale",
        json!({"kind":"hook","payload":hook,
               "env":{"DOT_AGENT_DECK_INBOX_TEST_HOOK_INTEGRATION":"none"}}),
    );
    let stale = result(&pane, "stale");
    assert!(
        !stale["stdout"].as_str().unwrap().contains(NATIVE_BODY),
        "the stale hook is handed nothing: {stale}"
    );
    let notice = || record(&daemon, &pane)["inbox"].clone();
    assert!(
        common::wait_until(WAIT, || notice()["integration"] == "hooks_too_old"),
        "the card names the stale hooks: {}",
        notice()
    );
    state(&daemon, &pane, &id, "queued");
}

/// The env that makes a hook stand in for a dot-agent-deck from before the
/// deck's messages: no version stamp, and it never asks for mail.
fn pre_inbox_env() -> Value {
    json!({"DOT_AGENT_DECK_INBOX_TEST_HOOK_INTEGRATION":"pre-inbox"})
}

/// Scenario: A Claude agent's hooks run a dot-agent-deck from before the deck's messages, which never asks for them, so its SessionStart is all the deck hears. The agent's card names the hooks-too-old notice with the deck's fix, while a task posted to it is still offered the old way: once the agent is idle the doorbell is typed, and running the `dot-agent-deck inbox fetch` it names hands the task over. Once its hooks run this deck again, the notice clears and the prompt hook answering the next task's doorbell is handed that task.
#[spec("inbox/claude/047")]
#[test]
fn claude_047_a_pre_inbox_hook_that_never_asks_for_mail_is_named_and_recovers() {
    let daemon = daemon(&[]);
    let pane = parent_profiled(&daemon, "claude-pre-inbox");
    command_with(
        &pane,
        "stale-start",
        json!({"kind":"hook","payload":payload(&pane, "SessionStart"),"env":pre_inbox_env()}),
    );
    let stale = result(&pane, "stale-start");
    assert_eq!(stale["code"], 0, "{stale}");
    assert_eq!(
        stale["stdout"], "",
        "a pre-inbox hook frames nothing: {stale}"
    );
    let notice = || record(&daemon, &pane)["inbox"].clone();
    assert!(
        common::wait_until(WAIT, || notice()["integration"] == "hooks_too_old"),
        "the card names the stale hooks: {}",
        notice()
    );
    if let Some(command) = notice()["integration_command"].as_str() {
        assert!(
            command.ends_with(" hooks install --agent claude-code"),
            "the fix is the deck's own hooks install: {command}"
        );
    }
    let id = post(&daemon, &pane, NATIVE_BODY);
    state(&daemon, &pane, &id, "queued");
    // The notice holds nothing: the doorbell still asks the model to fetch
    // with the CLI, which needs no hook.
    command_with(
        &pane,
        "stale-stop",
        json!({"kind":"hook","payload":payload(&pane, "Stop"),"env":pre_inbox_env()}),
    );
    assert_eq!(result(&pane, "stale-stop")["code"], 0);
    assert!(
        common::wait_until(WAIT, || String::from_utf8_lossy(&bytes(&pane))
            .split('\r')
            .any(|line| is_ring_line(line, &id))),
        "the doorbell is typed for the old hooks: {:?}",
        String::from_utf8_lossy(&bytes(&pane))
    );
    assert_eq!(notice()["integration"], "hooks_too_old", "{}", notice());
    // The model doing what the doorbell says is handed the task.
    command_with(
        &pane,
        "cli-fetch",
        json!({"kind":"wait","payload":{},
               "shell":format!("'{}' inbox fetch", env!("CARGO_BIN_EXE_dot-agent-deck"))}),
    );
    let fetched = result(&pane, "cli-fetch");
    assert!(
        fetched["stdout"].as_str().unwrap().contains(NATIVE_BODY),
        "the CLI fetch is handed the task: {fetched}"
    );
    state(&daemon, &pane, &id, "acknowledged");

    // The user updated the hooks and restarted the agent: its events come
    // from this deck's hook again.
    run_hook(&pane, "fixed-start", payload(&pane, "SessionStart"));
    assert!(
        common::wait_until(WAIT, || notice()["integration"].is_null()),
        "the notice clears once the hooks run this deck: {}",
        notice()
    );
    let next = post(&daemon, &pane, &format!("{NATIVE_BODY}-next"));
    let mut hook = payload(&pane, "UserPromptSubmit");
    hook["prompt"] = json!(fixed_line(&next));
    let handed = run_hook(&pane, "fixed-prompt", hook);
    assert!(
        context(&handed, "UserPromptSubmit").contains(&format!("{NATIVE_BODY}-next")),
        "the prompt hook is handed the next task: {handed}"
    );
    state(&daemon, &pane, &next, "acknowledged");
}

/// Scenario: Claude's settings are an install from before the deck's messages, naming another live dot-agent-deck whose version cannot be read for its status hook and no waiter; its SessionStart names the hooks-too-old notice while a task waits. `hooks refresh` from the deck's installed copy, as an upgrade runs it, leaves that live install alone and names the command to run; once that copy is gone, the same refresh pins the deck's copy and adds the waiter. The waiter the settings now name is handed the task, and the notice is gone.
#[spec("inbox/claude/048")]
#[test]
fn claude_048_an_upgrade_refreshes_pre_inbox_hooks_and_the_waiter_delivers() {
    use std::os::unix::fs::PermissionsExt;
    let daemon = daemon(&[]);
    let home = daemon.home().to_path_buf();
    let old = home.join("old-deck/dot-agent-deck");
    std::fs::create_dir_all(old.parent().unwrap()).unwrap();
    // Its version is unreadable, so no copy takes its pin over, wherever the
    // harness puts HOME.
    std::fs::write(
        &old,
        format!(
            "#!/bin/sh\n[ \"$1\" = --version ] && {{ echo 'dot-agent-deck (older)'; exit 0; }}\nDOT_AGENT_DECK_INBOX_TEST_HOOK_INTEGRATION=pre-inbox exec '{}' \"$@\"\n",
            env!("CARGO_BIN_EXE_dot-agent-deck")
        ),
    )
    .unwrap();
    std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o755)).unwrap();
    let settings = home.join(".claude/settings.json");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    let rule = json!([{"hooks":[
        {"type":"command","command":format!("'{}' hook --agent claude-code", old.display())}
    ]}]);
    std::fs::write(
        &settings,
        json!({"hooks":{"SessionStart":rule,"UserPromptSubmit":rule,"Stop":rule}}).to_string(),
    )
    .unwrap();
    let session_start = |suffix: &str| {
        let value: Value = serde_json::from_slice(&std::fs::read(&settings).unwrap()).unwrap();
        value["hooks"]["SessionStart"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|rule| rule["hooks"].as_array().unwrap().clone())
            .find_map(|hook| {
                hook["command"]
                    .as_str()
                    .filter(|command| command.ends_with(suffix))
                    .map(str::to_string)
            })
    };
    let pane = parent(&daemon, "claude-pre-inbox-settings");
    command_with(
        &pane,
        "stale",
        json!({"kind":"hook","shell":session_start(" hook --agent claude-code").unwrap(),
               "payload":payload(&pane, "SessionStart")}),
    );
    assert_eq!(result(&pane, "stale")["code"], 0);
    let notice = || record(&daemon, &pane)["inbox"].clone();
    assert!(
        common::wait_until(WAIT, || notice()["integration"] == "hooks_too_old"),
        "the card names the stale hooks: {}",
        notice()
    );
    let id = post(&daemon, &pane, NATIVE_BODY);
    state(&daemon, &pane, &id, "queued");

    // The harness seeds the deck's durable copy into HOME; the daemon may be
    // running it, so it is used as it is, never written over.
    let installed = home.join(".local/bin/dot-agent-deck");
    if !installed.exists() {
        std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
        if std::fs::hard_link(env!("CARGO_BIN_EXE_dot-agent-deck"), &installed).is_err() {
            std::fs::copy(env!("CARGO_BIN_EXE_dot-agent-deck"), &installed).unwrap();
        }
    }
    let refresh = || {
        let out = std::process::Command::new(&installed)
            .args(["hooks", "refresh", "--json"])
            .env_clear()
            .env("HOME", &home)
            .env("PATH", claude_path(&home))
            .current_dir(&home)
            .output()
            .unwrap();
        let report: Value = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|error| panic!("a JSON report: {error}; {out:?}"));
        (out.status.success(), report)
    };
    let (ok, kept) = refresh();
    assert!(!ok, "a live install it kept needs attention: {kept}");
    assert_eq!(kept["ok"], false, "{kept}");
    let lines = kept["lines"].as_array().unwrap();
    assert!(
        lines.iter().any(|line| line["text"]
            .as_str()
            .unwrap()
            .contains(&*old.to_string_lossy())),
        "it names the install it left alone: {kept}"
    );
    assert!(
        lines.iter().any(|line| line["command"]
            .as_str()
            .is_some_and(|command| command.ends_with(" hooks install --agent claude-code"))),
        "it names the command that pins this copy: {kept}"
    );
    assert!(
        session_start(" hook --agent claude-code")
            .unwrap()
            .contains(&*old.to_string_lossy()),
        "the live install is left alone"
    );

    // The upgrade moved the older copy away: the same refresh repairs the pin.
    std::fs::remove_file(&old).unwrap();
    let (ok, fixed) = refresh();
    assert!(ok, "{fixed}");
    assert_eq!(fixed["ok"], true, "{fixed}");
    let waiter = session_start(" inbox wait --agent claude-code").expect("the waiter is added");
    assert!(!waiter.contains(&*old.to_string_lossy()), "{waiter}");
    command_with(
        &pane,
        "fixed",
        json!({"kind":"wait","shell":waiter,"payload":payload(&pane, "SessionStart")}),
    );
    let delivered = result(&pane, "fixed");
    assert_eq!(
        delivered["code"], 2,
        "the refreshed waiter is handed the task: {delivered}"
    );
    assert!(delivered["stderr"].as_str().unwrap().contains(NATIVE_BODY));
    state(&daemon, &pane, &id, "fetched");
    assert!(notice()["integration"].is_null(), "{}", notice());
}

/// Scenario: A process in a Claude agent's own tree sends the deck the prompt hook's exact-commit request directly, without the hook. First the deck cannot write the caps file of the task's first attempt, so it never sends it: a request naming that attempt commits nothing and the task stays queued. A waiter armed again is handed attempt 1.2: a request naming it, with no commit receipt in the spool, commits nothing either and the task stays fetched. The real notification through the real prompt hook, which publishes that receipt, commits it.
#[spec("inbox/claude/023")]
#[test]
fn claude_023_a_raw_exact_commit_needs_the_attempts_receipt() {
    use std::os::unix::fs::PermissionsExt;
    let daemon = daemon(&[]);
    let pane = parent(&daemon, "claude-raw");
    wait(&pane, "first", "startup");
    armed(&daemon, &pane, 1);
    let caps = caps_dir(daemon.home().parent().unwrap());
    std::fs::set_permissions(&caps, std::fs::Permissions::from_mode(0o500)).unwrap();
    let id = post(&daemon, &pane, NATIVE_BODY);
    let first = result(&pane, "first");
    std::fs::set_permissions(&caps, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(first["code"], 0, "nothing reached the waiter: {first}");
    assert_eq!(first["stderr"], "", "{first}");
    state(&daemon, &pane, &id, "queued");
    let commit = |a: u32, e: u32| {
        json!({"message_type":"inbox_committed","pane_id":pane.id,"agent_id":pane.agent,
               "token":pane.token,"attempts":[{"id":id,"a":a,"e":e}]})
    };
    // The deck answers once it has applied what it accepted, so the state
    // read after the answer is the request's outcome.
    let reply = raw(&daemon, &pane, "never-sent", commit(1, 1));
    assert!(reply.get("refused").is_none(), "admitted: {reply}");
    state(&daemon, &pane, &id, "queued");

    wait(&pane, "second", "stop");
    let second = result(&pane, "second");
    assert_eq!(second["code"], 2, "the next waiter is handed it: {second}");
    let stderr = second["stderr"].as_str().unwrap().to_string();
    assert!(stderr.contains(" delivery 1.2 from "), "{stderr}");
    state(&daemon, &pane, &id, "fetched");
    raw(&daemon, &pane, "no-receipt", commit(1, 2));
    state(&daemon, &pane, &id, "fetched");

    run_hook(&pane, "notification", notification(&pane, &stderr));
    state(&daemon, &pane, &id, "acknowledged");
}

/// Scenario: In a pane started from its command alone, with no agent type and no deck waiter in Claude's settings, Claude's SessionStart hook — a process inside the pane's agent, as Claude's own hooks are — has connected to the deck but its event has not been read yet when a prompt hook too old for the deck's messages fetches a queued task's doorbell. The deck does not wait for that connection: the stale hook is refused before the SessionStart event is sent, prints nothing, the task stays queued and the card says the hooks are too old. A current prompt hook in the same pane is then handed the task, also before SessionStart's event is in.
#[spec("inbox/claude/024")]
#[test]
fn claude_024_a_stale_hook_is_refused_without_waiting_for_an_earlier_session_start() {
    let daemon = daemon(&[]);
    let pane = parent_as(&daemon, "claude-bare-early", None);
    let id = post(&daemon, &pane, NATIVE_BODY);
    // SessionStart's connection, made from inside the pane's agent and
    // accepted before the fetch's, with its line not yet sent: the child
    // connects, says so, and sends the event only once told to.
    let event = json!({"session_id":format!("native-{}", pane.agent),
        "pane_id":pane.id,"agent_id":pane.agent,"token":pane.token,
        "deck_build":SYNTHETIC_DECK_BUILD,
        "agent_type":"claude_code","event_type":"session_start",
        "timestamp":chrono::Utc::now(),"cwd":daemon.cwd(),"metadata":{}});
    let line = sidecar(&pane, "session-start-line");
    std::fs::write(&line, format!("{event}\n")).unwrap();
    let connected = sidecar(&pane, "session-start-connected");
    let go = sidecar(&pane, "session-start-go");
    let script = format!(
        "import socket, os, time\n\
         c = socket.socket(socket.AF_UNIX)\n\
         c.connect({socket:?})\n\
         open({connected:?}, 'w').close()\n\
         end = time.time() + 30\n\
         while not os.path.exists({go:?}) and time.time() < end: time.sleep(0.01)\n\
         c.sendall(open({line:?}, 'rb').read())\n\
         c.shutdown(socket.SHUT_WR)\n\
         c.recv(4096)\n",
        socket = daemon.hook_socket().to_string_lossy(),
        connected = connected.to_string_lossy(),
        go = go.to_string_lossy(),
        line = line.to_string_lossy(),
    );
    let script_path = sidecar(&pane, "session-start.py");
    std::fs::write(&script_path, script).unwrap();
    command_with(
        &pane,
        "session-start",
        json!({"kind":"wait","payload":{},
               "shell":format!("python3 '{}'", script_path.to_string_lossy())}),
    );
    assert!(
        common::wait_until(WAIT, || connected.exists()),
        "SessionStart's hook connected: {:?}",
        std::fs::read_to_string(sidecar(&pane, "stderr-session-start"))
    );
    let mut hook = payload(&pane, "UserPromptSubmit");
    hook["prompt"] = json!(fixed_line(&id));
    command_with(
        &pane,
        "stale",
        json!({"kind":"hook","payload":hook.clone(),
               "env":{"DOT_AGENT_DECK_INBOX_TEST_HOOK_INTEGRATION":"none"}}),
    );
    // PRD #1666 part 3 (H5): no legacy service is left for a wait to decide
    // between, so the stale fetch is refused with SessionStart still unsent.
    let stale = result(&pane, "stale");
    assert!(
        !stale["stdout"].as_str().unwrap().contains(NATIVE_BODY),
        "the stale hook is handed nothing: {stale}"
    );
    let notice = || record(&daemon, &pane)["inbox"].clone();
    assert!(
        common::wait_until(WAIT, || notice()["integration"] == "hooks_too_old"),
        "the card names the stale hooks: {}",
        notice()
    );
    state(&daemon, &pane, &id, "queued");
    command_with(&pane, "current", json!({"kind":"hook","payload":hook}));
    let current = result(&pane, "current");
    let printed: Value =
        serde_json::from_str(current["stdout"].as_str().unwrap().trim()).unwrap_or(Value::Null);
    assert!(
        printed["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .is_some_and(|context| context.contains(NATIVE_BODY)),
        "a current hook is handed the task without waiting either: {current}"
    );
    std::fs::write(&go, b"").unwrap();
}

/// Scenario: A process in a Claude agent's own tree asks the deck directly whether it serves one-shot receipts, then sends the receipt verbs for what they do not serve: a once-wait from a Codex component, a Claude stream wait, a hook fetch naming a stream's or the waiter's component, and a fetch a stream bound. The deck answers the question with the receipt support and the senders it serves; it refuses each of the others as unsupported, and a task queued beforehand stays queued.
#[spec("inbox/claude/031")]
#[test]
fn claude_031_the_deck_answers_its_receipts_and_refuses_what_they_do_not_serve() {
    let daemon = daemon(&[]);
    let pane = parent(&daemon, "claude-receipts");
    let id = post(&daemon, &pane, NATIVE_BODY);
    let ask = |tag: &str, verb: &str, extra: Value| {
        let mut request = json!({"message_type":verb, "pane_id":pane.id,
            "agent_id":pane.agent, "token":pane.token, "pid":pane.pid});
        for (key, value) in extra.as_object().unwrap() {
            request[key] = value.clone();
        }
        raw(&daemon, &pane, tag, request)
    };
    let answer = ask("question", "inbox_receipts", json!({}));
    assert_eq!(answer["one_shot_receipts"], 1, "{answer}");
    // PRD #1666 part 3 (H4): and the senders it serves beyond Claude Code's.
    assert_eq!(
        answer["one_shot_senders"],
        json!(["claude_waiter", "claude_hooks", "codex_hooks", "cli"]),
        "{answer}"
    );
    for (tag, verb, extra) in [
        (
            "codex-once",
            "inbox_receipt_wait",
            json!({"component":"codex_hooks", "mode":"once", "hook_integration":1}),
        ),
        (
            "claude-stream",
            "inbox_receipt_wait",
            json!({"component":"claude_waiter", "mode":"stream", "hook_integration":1}),
        ),
        (
            "stream-component-fetch",
            "inbox_receipt_fetch",
            json!({"consumer":"doorbell_hook", "component":"opencode_plugin",
                "max_chars":65536, "hook_integration":1}),
        ),
        (
            "waiter-component-fetch",
            "inbox_receipt_fetch",
            json!({"consumer":"turn_end_hook", "component":"claude_waiter",
                "max_chars":65536, "hook_integration":1}),
        ),
        (
            "bound-fetch",
            "inbox_receipt_fetch",
            json!({"consumer":"doorbell_hook", "max_chars":65536, "rx":7,
                "hook_integration":1}),
        ),
    ] {
        let reply = ask(tag, verb, extra);
        assert_eq!(reply["refused"], "unsupported", "{tag}: {reply}");
        assert!(reply.get("messages").is_none(), "{tag}: {reply}");
    }
    state(&daemon, &pane, &id, "queued");
}
