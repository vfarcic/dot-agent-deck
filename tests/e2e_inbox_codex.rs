#![cfg(all(feature = "e2e", unix))]

//! Codex inbox contract through the real wrapper, hook CLI and daemon.
//! Screen fixtures transcribe the 0.160.0 spike, with scratch paths substituted.
//! No Codex CLI, model or credential is used.
mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;

use inbox::*;
use serde_json::{Value, json};
use spec::spec;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::Duration;

const UPDATE: &str = include_str!("fixtures/codex-inbox/update.txt");
const FOLDER: &str = include_str!("fixtures/codex-inbox/folder.txt");
const HOOKS: &str = include_str!("fixtures/codex-inbox/hooks.txt");
const UNKNOWN: &str = include_str!("fixtures/codex-inbox/unknown.txt");
const COMPOSER: &str = include_str!("fixtures/codex-inbox/composer.txt");

struct Codex {
    pane: Pane,
    daemon: common::DaemonProc,
    /// The receipt spool the deck pinned for the pane, as its environment
    /// names it: what the agent's hooks and its model-run `inbox fetch` see.
    spool: Option<String>,
    _bin: tempfile::TempDir,
}

impl Codex {
    fn new(screen_text: &str) -> Self {
        Self::new_with(screen_text, &[])
    }

    fn new_with(screen_text: &str, extra: &[(&str, &str)]) -> Self {
        let bin = common::harness_tempdir().unwrap();
        let program = bin.path().join("codex");
        std::fs::write(&program, r#"#!/usr/bin/python3
import os, sys, json, tty, termios, select
if 'app-server' in sys.argv:
    for line in sys.stdin:
        req = json.loads(line)
        if 'id' not in req: continue
        result = {'userAgent':'inbox-fixture','codexHome':os.environ.get('CODEX_HOME',''), 'platformFamily':'unix','platformOs':'linux'} if req['method'] == 'initialize' else {'data':[]}
        print(json.dumps({'id':req['id'],'result':result}), flush=True)
    sys.exit(0)
tty.setraw(0, termios.TCSANOW)
base = os.environ['INBOX_CODEX_BASE']
with open(base + '.env','w') as f: json.dump({k: os.environ[k] for k in ['DOT_AGENT_DECK_INBOX_SPOOL'] if k in os.environ}, f)
with open(base + '.pid','w') as f: f.write(str(os.getpid()))
capture = open(base + '.bytes','ab',buffering=0)
previous = None
draft = bytearray()
while True:
    with open(base + '.screen') as f: screen = f.read()
    if screen != previous:
        os.write(1, b'\x1b[2J\x1b[H' + screen.replace('\n','\r\n').encode())
        # Codex 0.160 keeps its cursor right after the composer's text.
        rows = screen.split('\n')
        prompts = [n for n, row in enumerate(rows) if row.startswith('›')]
        if prompts:
            last = prompts[-1]
            while last + 1 < len(rows) and rows[last + 1].strip(): last += 1
            if draft.endswith(b'\n'):
                # Its Enter taken as a newline: the cursor is on the empty
                # composer row below the line, at the composer indent.
                os.write(1, f'\x1b[{last + 2};3H'.encode())
            else:
                os.write(1, f'\x1b[{last + 1};{len(rows[last].rstrip()) + 1}H'.encode())
        previous = screen
    if select.select([0],[],[],0.025)[0]:
        chunk = os.read(0,4096)
        if not chunk: break
        capture.write(chunk)
        newline = os.path.exists(base + '.newline')
        strand = newline or os.path.exists(base + '.strand')
        if strand or os.path.exists(base + '.keep'):
            for c in chunk:
                if c == 21: draft.clear()
                elif c == 13:
                    # Codex's paste-burst Enter keeps the draft. A stranding
                    # Codex drops only the first Enter after text, then submits;
                    # a newline one (Codex 0.160 under load) takes that Enter as
                    # a newline in the composer instead, and with `.swallow`
                    # present ignores the next Enter too.
                    if newline and draft and not draft.endswith(b'\n'):
                        draft.append(10)
                    elif newline and draft and os.path.exists(base + '.swallow'):
                        os.remove(base + '.swallow')
                    elif strand and (not draft or newline or os.path.exists(base + '.dropped')):
                        with open(base + '.submitted', 'ab') as f: f.write(bytes(draft).rstrip(b'\n') + b'\n')
                        draft.clear()
                    elif strand: open(base + '.dropped', 'w').close()
                else: draft.append(c)
            with open(base + '.empty') as f: empty = f.read()
            shown = empty.replace('Ask Codex to do anything', draft.decode() if draft else 'Ask Codex to do anything')
            # Codex 0.160 hides its shortcuts hint while the composer holds text.
            if strand and draft: shown = shown.replace('? for shortcuts', ' ' * 15)
            with open(base + '.screen', 'w') as f: f.write(shown)
    elif os.path.exists(base + '.drain'):
        os.remove(base + '.drain')
        open(base + '.drained', 'w').close()
"#).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!("{}:/usr/bin:/bin", bin.path().display());
        let mut env = vec![("PATH", path.as_str())];
        env.extend_from_slice(extra);
        // The tests run Codex's hooks and fetch from the test process, outside
        // the stand-in pane's agent; inbox/protocol/014 and /015 pin the binding.
        let daemon = daemon_unbound(&env);
        let pane = Self::start(&daemon, screen_text, &program);
        let spool = pane_env(&pane, "DOT_AGENT_DECK_INBOX_SPOOL");
        Self {
            pane,
            daemon,
            spool,
            _bin: bin,
        }
    }

    fn start(daemon: &common::DaemonProc, text: &str, program: &std::path::Path) -> Pane {
        Self::start_as(daemon, text, program, Some("codex"))
    }

    /// [`Self::start`], spawned as `agent_type`, or with none for the deck to
    /// learn from the agent's own hooks.
    fn start_as(
        daemon: &common::DaemonProc,
        text: &str,
        program: &std::path::Path,
        agent_type: Option<&str>,
    ) -> Pane {
        let base = daemon.home.join("codex-inbox");
        std::fs::write(base.with_extension("screen"), text).unwrap();
        let reply = attach(
            daemon,
            json!({"op":"start-agent","command":common::capability_export_command(&format!("exec '{}'",program.display())),
            "agent_type":agent_type,"cwd":daemon.home,"rows":40,"cols":140,
            "env":[["DOT_AGENT_DECK_PANE_ID","codex-inbox"],["INBOX_CODEX_BASE",base],
                ["CODEX_HOME",daemon.home.join(".codex")],
                ["PATH",format!("{}:/usr/bin:/bin",program.parent().unwrap().display())]],"display_name":"codex inbox"}),
        );
        assert_eq!(reply["ok"], true, "spawn wrapped Codex: {reply}");
        let agent = reply["id"].as_str().unwrap().to_string();
        assert!(
            common::wait_until(common::child_boot_budget(), || base
                .with_extension("pid")
                .exists()),
            "wrapped recorder must boot"
        );
        let token = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(common::recorded_hook_capability(&daemon.home, &agent))
            .unwrap();
        let pane = Pane {
            id: "codex-inbox".into(),
            agent,
            token,
            pid: std::fs::read_to_string(base.with_extension("pid"))
                .unwrap()
                .parse()
                .unwrap(),
            capture: base.with_extension("bytes"),
            attach_socket: daemon.attach_socket.clone(),
        };
        assert!(
            common::wait_until(WAIT, || screen(daemon, &pane)
                .contains(text.lines().find(|l| !l.trim().is_empty()).unwrap().trim())),
            "fixture must paint: {}",
            screen(daemon, &pane)
        );
        pane
    }

    fn paint(&self, text: &str) {
        std::fs::write(self.pane.capture.with_extension("screen"), text).unwrap();
        let needle = text.lines().find(|l| !l.trim().is_empty()).unwrap().trim();
        assert!(common::wait_until(WAIT, || screen(
            &self.daemon,
            &self.pane
        )
        .contains(needle)));
    }

    fn run_hook(&self, event: &str, prompt: Option<&str>) -> String {
        let output = self.hook_output(event, prompt);
        assert!(output.status.success(), "hook: {output:?}");
        String::from_utf8(output.stdout).unwrap()
    }

    /// The deck's own Codex hook for `event`, run with the pane's identity
    /// and spool, and what it did.
    fn hook_output(&self, event: &str, prompt: Option<&str>) -> std::process::Output {
        let mut payload = json!({"hook_event_name":event,"session_id":"codex-inbox-session","cwd":self.daemon.home});
        if let Some(prompt) = prompt {
            payload["prompt"] = json!(prompt);
        }
        let mut child = self
            .in_pane(&["hook", "--agent", "codex"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    /// `dot-agent-deck <args>` with the environment the pane's processes
    /// get: its identity, its capability and its receipt spool.
    fn in_pane(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"));
        command
            .args(args)
            .env_clear()
            .env("HOME", &self.daemon.home)
            .env("CODEX_HOME", self.daemon.home.join(".codex"))
            .env("DOT_AGENT_DECK_SOCKET", &self.daemon.hook_socket)
            .env("DOT_AGENT_DECK_PANE_ID", &self.pane.id)
            .env("DOT_AGENT_DECK_AGENT_ID", &self.pane.agent)
            .env("DOT_AGENT_DECK_PANE_CAPABILITY", &self.pane.token);
        if let Some(spool) = &self.spool {
            command.env("DOT_AGENT_DECK_INBOX_SPOOL", spool);
        }
        command
    }

    /// What a model-run `dot-agent-deck inbox fetch` in the pane prints.
    fn model_fetch(&self) -> std::process::Output {
        self.in_pane(&["inbox", "fetch"])
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// The listing's entry for message `id`.
    fn summary(&self, id: &str) -> Value {
        listing(&self.daemon, &self.pane)["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("message {id} is listed"))
    }

    /// Message `id` settled on its receipt, its exact attempt committed by
    /// the receiver it was handed to, and handed over `deliveries` times:
    /// not acknowledged by id (`hook_handover`, `fetched_by_cli`).
    fn committed(&self, id: &str, deliveries: u64) {
        state(&self.daemon, &self.pane, id, "acknowledged");
        let summary = self.summary(id);
        assert_eq!(
            summary["ack_how"], "plugin_confirmed",
            "settled by its receipt, not by an id-only ack: {summary}"
        );
        assert_eq!(summary["deliveries"], deliveries, "{summary}");
    }

    fn queued(&self, body: &str) -> String {
        enqueue(&self.daemon, &self.pane, body)
    }

    fn busy(&self, prompt: &str) {
        self.run_hook("SessionStart", None);
        self.run_hook("UserPromptSubmit", Some(prompt));
        assert!(
            common::wait_until(WAIT, || {
                attach(&self.daemon, json!({"op":"list-agents"}))["agent_records"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|record| {
                        record["id"] == self.pane.agent && record["live"]["status"] == "Thinking"
                    })
            }),
            "own native hooks must establish a busy Codex generation"
        );
    }

    fn no_bytes(&self) {
        assert!(
            !common::wait_until(Duration::from_millis(1300), || !bytes(&self.pane)
                .is_empty()),
            "guarded Codex pane must receive ZERO bytes, including CR: {:?} ({:?})\nPane:\n{}",
            bytes(&self.pane),
            String::from_utf8_lossy(&bytes(&self.pane)),
            screen(&self.daemon, &self.pane)
        );
    }
}

/// Scenario: Start wrapped synthetic Codex in raw mode on the measured update-now dialog and queue a task. The dialog must receive zero bytes, including bare carriage returns, while it remains on screen.
#[spec("inbox/codex/001")]
#[test]
fn codex_001_update_dialog_receives_zero_bytes() {
    let codex = Codex::new(UPDATE);
    codex.queued(BODY);
    codex.no_bytes();
}

/// Scenario: Start wrapped synthetic Codex on its folder-trust dialog and queue a task. Raw-input readiness must not type a doorbell or carriage return into that dialog.
#[spec("inbox/codex/002")]
#[test]
fn codex_002_folder_dialog_receives_zero_bytes() {
    let codex = Codex::new(FOLDER);
    codex.queued(BODY);
    codex.no_bytes();
}

/// Scenario: Start wrapped synthetic Codex on the hooks-review screen and queue a task. The deck must leave the review selection untouched by sending zero input bytes.
#[spec("inbox/codex/003")]
#[test]
fn codex_003_hooks_dialog_receives_zero_bytes() {
    let codex = Codex::new(HOOKS);
    codex.queued(BODY);
    codex.no_bytes();
}

/// Scenario: Paint an unknown startup dialog in wrapped synthetic Codex and queue a task, then paint its recognised empty composer. The deck types into no Codex release (its first stage commits what it types), so the pane must receive zero bytes on the dialog and on the composer, and the task must wait queued until a receiver takes it.
#[spec("inbox/codex/004")]
#[test]
fn codex_004_codex_gets_no_byte_on_a_dialog_or_its_composer() {
    let codex = Codex::new(UNKNOWN);
    let id = codex.queued(BODY);
    codex.no_bytes();
    codex.paint(COMPOSER);
    codex.no_bytes();
    state(&codex.daemon, &codex.pane, &id, "queued");
    fetch(&codex.daemon, &codex.pane, true);
    state(&codex.daemon, &codex.pane, &id, "acknowledged");
    unchanged(&codex.pane, Duration::from_millis(500));
    assert!(
        bytes(&codex.pane).is_empty(),
        "no byte after the fetch either"
    );
}

/// Scenario: Start wrapped synthetic Codex directly at its empty composer before any own hook fires and queue a task, then report its own SessionStart. The pane must receive zero bytes either way: no doorbell, no launch-command prompt and no body.
#[spec("inbox/codex/005")]
#[test]
fn codex_005_an_empty_composer_gets_no_byte_before_or_after_its_first_hook() {
    let codex = Codex::new(COMPOSER);
    let id = codex.queued(BODY);
    codex.no_bytes();
    codex.run_hook("SessionStart", None);
    codex.no_bytes();
    state(&codex.daemon, &codex.pane, &id, "queued");
}

/// Scenario: Start the synthetic Codex at its empty composer under a name the deck does not recognise and with no agent type, then let its own hooks report it as Codex and queue a task. The deck learns the type from the hooks and still types nothing: zero bytes reach the pane, the task waits queued, and the agent's WAKE names its hooks' turn-end route, not the doorbell.
#[spec("inbox/codex/024")]
#[test]
fn codex_024_a_pane_its_hooks_call_codex_gets_no_byte() {
    let mut codex = Codex::new(UNKNOWN);
    assert_eq!(
        attach(
            &codex.daemon,
            json!({"op":"stop-agent","id":codex.pane.agent})
        )["ok"],
        true
    );
    for suffix in ["pid", "bytes"] {
        let _ = std::fs::remove_file(codex.pane.capture.with_extension(suffix));
    }
    let untyped = codex._bin.path().join("assistant");
    std::fs::copy(codex._bin.path().join("codex"), &untyped).unwrap();
    codex.pane = Codex::start_as(&codex.daemon, COMPOSER, &untyped, None);
    codex.run_hook("SessionStart", None);
    codex.run_hook("Stop", None);
    let id = codex.queued(BODY);
    codex.no_bytes();
    state(&codex.daemon, &codex.pane, &id, "queued");
    // Codex's own hooks take its messages at the end of its turns, and
    // that is the route WAKE names, not the doorbell.
    assert!(
        common::wait_until(WAIT, || record(&codex)["inbox"]["wake"] == "turn_end"),
        "{}",
        record(&codex)
    );
}

/// Scenario: Report Codex's own busy hook, replace its screen with an unfamiliar mid-session screen and queue a task. Its own Idle must type nothing, and neither must its recognised composer drawn after, since the deck types into no Codex release; restarting in the same pane on a startup dialog must keep every byte out of that dialog.
#[spec("inbox/codex/006")]
#[test]
fn codex_006_own_hook_gates_status_and_restart_resets_guard() {
    let mut codex = Codex::new(COMPOSER);
    codex.busy("human work");
    codex.paint(UNKNOWN);
    let id = codex.queued(BODY);
    codex.no_bytes();
    // Own-hook status is necessary, but never licenses input into an unknown
    // screen or a visibly occupied composer. Stop hand-over is pinned below.
    let request = json!({"session_id":"codex-inbox-session", "pane_id":codex.pane.id,
        "agent_id":codex.pane.agent, "token":codex.pane.token, "agent_type":"codex",
        "event_type":"idle", "timestamp":chrono::Utc::now(), "cwd":codex.daemon.home,
        "metadata":{}});
    let mut socket = inbox::connect(&codex.daemon, &request).into_inner();
    socket.shutdown(std::net::Shutdown::Write).unwrap();
    let mut response = Vec::new();
    std::io::Read::read_to_end(&mut socket, &mut response).unwrap();
    codex.no_bytes();
    // Nor does its recognised composer: the deck types into no Codex release.
    codex.paint(COMPOSER);
    codex.no_bytes();
    state(&codex.daemon, &codex.pane, &id, "queued");
    assert_eq!(
        attach(
            &codex.daemon,
            json!({"op":"stop-agent","id":codex.pane.agent})
        )["ok"],
        true
    );
    for suffix in ["pid", "bytes"] {
        let _ = std::fs::remove_file(codex.pane.capture.with_extension(suffix));
    }
    codex.pane = Codex::start(&codex.daemon, UNKNOWN, &codex._bin.path().join("codex"));
    codex.queued("NEW-GENERATION-MESSAGE");
    codex.no_bytes();
}

/// Scenario: Queue several token-dense tasks while synthetic Codex is busy, then invoke its real Stop hook. It must print a `decision: block` JSON whose whole line fits 6000 UTF-8 bytes, carrying a bounded subset of the tasks, each block naming its attempt; those are settled by their receipts (never an id-only ack) and the rest stay queued, issued to no one, until later Stops take them; an empty Stop prints nothing and nothing is typed into the pane.
#[spec("inbox/codex/019")]
#[test]
fn codex_019_stop_hands_over_on_receipts_and_keeps_overflow_unissued() {
    let codex = Codex::new(COMPOSER);
    codex.busy("busy human turn");
    let ids: Vec<_> = (0..4)
        .map(|n| codex.queued(&format!("codex-task-{n}: {}", "界".repeat(700))))
        .collect();
    let stdout = codex.run_hook("Stop", None);
    assert!(
        stdout.len() <= 6000,
        "the whole Stop output fits Codex's 6000-byte budget: {}",
        stdout.len()
    );
    let output: Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!(
            "queued busy Codex needs Stop decision:block with framed reason; {e}; stdout={stdout:?}"
        )
    });
    assert_eq!(output["decision"], "block");
    let reason = output["reason"].as_str().expect("Stop reason");
    let handed: Vec<_> = ids
        .iter()
        .filter(|id| reason.contains(id.as_str()))
        .collect();
    assert!(
        !handed.is_empty() && handed.len() < ids.len(),
        "budget must retain overflow: {reason}"
    );
    for id in &ids {
        if reason.contains(id.as_str()) {
            assert!(
                reason.contains(&format!("[dot-agent-deck message {id} delivery 1.1 from ")),
                "a one-shot's block names its attempt: {reason}"
            );
            codex.committed(id, 1);
        } else {
            state(&codex.daemon, &codex.pane, id, "queued");
            assert_eq!(
                codex.summary(id)["deliveries"],
                0,
                "overflow was never issued"
            );
        }
    }
    for _ in 0..ids.len() {
        if !listing(&codex.daemon, &codex.pane)["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["state"] == "queued")
        {
            break;
        }
        let stdout = codex.run_hook("Stop", None);
        assert!(stdout.len() <= 6000, "{}", stdout.len());
        let more: Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(more["decision"], "block");
    }
    for id in &ids {
        codex.committed(id, 1);
    }
    assert!(
        codex.run_hook("Stop", None).trim().is_empty(),
        "empty inbox stops normally"
    );
    assert!(
        bytes(&codex.pane).is_empty(),
        "busy hand-over must not type a doorbell"
    );
}

/// Scenario: While synthetic Codex is busy, queue several token-dense messages and submit the fixed doorbell to its real UserPromptSubmit hook. Its whole JSON output fits 6000 UTF-8 bytes and its additionalContext carries a bounded subset of framed tasks, each naming its attempt; those are settled by their receipts, the rest stay queued and unissued, the deck line stays out of human prompt history and the card shows an envelope.
#[spec("inbox/codex/018")]
#[test]
fn codex_018_doorbell_submit_hands_over_on_receipts_within_the_byte_budget() {
    let codex = Codex::new(COMPOSER);
    codex.busy("original human prompt");
    let ids: Vec<_> = (0..4)
        .map(|n| codex.queued(&format!("submit-{n}: \"quoted\" \\ {}", "界".repeat(700))))
        .collect();
    let events = common::EventSub::subscribe(&codex.daemon.attach_socket);
    let stdout = codex.run_hook("UserPromptSubmit", Some(fixed_line(&ids[0]).trim_start()));
    assert!(
        stdout.len() <= 6000,
        "the whole hook output, JSON escaping included, fits 6000 bytes: {}",
        stdout.len()
    );
    let output: Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!("Codex doorbell submit must inject additionalContext; {e}; stdout={stdout:?}")
    });
    assert_eq!(
        output["hookSpecificOutput"]["hookEventName"],
        "UserPromptSubmit"
    );
    let context = output["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(context.contains("[dot-agent-deck message"));
    let handed = ids
        .iter()
        .filter(|id| context.contains(id.as_str()))
        .count();
    assert!(
        handed > 0 && handed < ids.len(),
        "hand over a bounded subset and retain overflow"
    );
    for id in &ids {
        if context.contains(id.as_str()) {
            assert!(
                context.contains(&format!("[dot-agent-deck message {id} delivery 1.1 from ")),
                "{context}"
            );
            codex.committed(id, 1);
        } else {
            state(&codex.daemon, &codex.pane, id, "queued");
            assert_eq!(
                codex.summary(id)["deliveries"],
                0,
                "overflow was never issued"
            );
        }
    }
    let records = attach(&codex.daemon, json!({"op":"list-agents"}));
    let record = records["agent_records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == codex.pane.agent)
        .unwrap();
    assert_eq!(record["live"]["last_user_prompt"], "original human prompt");
    assert!(
        !record["live"]["first_prompts"]
            .to_string()
            .contains("waiting, run:")
    );
    events.wait_for(
        |event| {
            event.pane_id.as_deref() == Some(codex.pane.id.as_str())
                && event.event_type == dot_agent_deck::event::EventType::Thinking
                && event
                    .user_prompt
                    .as_deref()
                    .is_some_and(|prompt| prompt.starts_with("✉ "))
        },
        WAIT,
    );
}

/// The record `list-agents` gives the Codex pane's agent.
fn record(codex: &Codex) -> Value {
    attach(&codex.daemon, json!({"op":"list-agents"}))["agent_records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == codex.pane.agent)
        .cloned()
        .expect("the Codex agent's record")
}

/// Scenario: With none of the deck's Codex hooks running (untrusted), queue a task for idle synthetic Codex; the deck types nothing into Codex, and a stand-in model runs the real `dot-agent-deck inbox fetch` in the pane, which prints the framing and the block naming its attempt and settles it by its receipt. With the pane's receipt records made unwritable, a second fetch still prints and settles its task, and the card says messages may repeat.
#[spec("inbox/codex/020")]
#[test]
fn codex_020_untrusted_hooks_a_model_run_fetch_commits_on_receipts() {
    let codex = Codex::new(COMPOSER);
    let id = codex.queued("untrusted-hooks task 7c41");
    // The deck types no doorbell into Codex; the model runs the fetch the
    // way it would for the deck's own line.
    codex.no_bytes();
    let fetched = codex.model_fetch();
    let stdout = String::from_utf8_lossy(&fetched.stdout);
    assert!(fetched.status.success(), "fetch: {fetched:?}");
    assert!(
        stdout.starts_with(dot_agent_deck::inbox_hub::AGENT_FRAMING)
            && stdout.contains(&format!("[dot-agent-deck message {id} delivery 1.1 from "))
            && stdout.contains("untrusted-hooks task 7c41"),
        "{stdout}"
    );
    codex.committed(&id, 1);
    assert!(
        record(&codex)["inbox"]["receipts_degraded"].is_null(),
        "{}",
        record(&codex)
    );

    let spool = codex.spool.clone().expect("the deck pins the pane a spool");
    let rec = std::path::Path::new(&spool)
        .join(dot_agent_deck::inbox::encode_token(&codex.pane.id))
        .join(dot_agent_deck::inbox::encode_token(&codex.pane.agent))
        .join("rec");
    assert!(rec.is_dir(), "{}", rec.display());
    std::fs::set_permissions(&rec, std::fs::Permissions::from_mode(0o500)).unwrap();
    let second = codex.queued("sandboxed-spool task 2b9e");
    let fetched = codex.model_fetch();
    std::fs::set_permissions(&rec, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(fetched.status.success(), "fetch: {fetched:?}");
    assert!(
        String::from_utf8_lossy(&fetched.stdout).contains("sandboxed-spool task 2b9e"),
        "{fetched:?}"
    );
    codex.committed(&second, 1);
    assert!(
        common::wait_until(WAIT, || record(&codex)["inbox"]["receipts_degraded"]
            .as_str()
            .is_some_and(|why| why.contains("receipt"))),
        "a receipt it could not record degrades the card: {}",
        record(&codex)
    );
}

/// Scenario: With Codex's prompt hook switched off and its Stop hook trusted, an idle task reaches the model through a model-run `inbox fetch` with nothing typed into the pane, and a busy task through the real Stop hook; then a Stop hook and a model-run fetch run at once over two queued tasks. Each task is handed over to exactly one of them, printed once and settled by its receipt once.
#[spec("inbox/codex/021")]
#[test]
fn codex_021_prompt_hook_off_doorbell_fetch_idle_and_stop_busy_each_once() {
    let codex = Codex::new(COMPOSER);
    let idle = codex.queued("idle task via fetch 51aa");
    codex.no_bytes();
    let fetched = codex.model_fetch();
    assert!(fetched.status.success(), "{fetched:?}");
    assert!(String::from_utf8_lossy(&fetched.stdout).contains("idle task via fetch 51aa"));
    codex.committed(&idle, 1);

    codex.busy("a long human turn");
    let busy = codex.queued("busy task via stop 8d03");
    let stdout = codex.run_hook("Stop", None);
    let output: Value = serde_json::from_str(&stdout).expect("a Stop block");
    assert!(
        output["reason"]
            .as_str()
            .unwrap()
            .contains("busy task via stop 8d03")
    );
    codex.committed(&busy, 1);

    let pair = [
        codex.queued("overlap task one 0e7f"),
        codex.queued("overlap task two 93c2"),
    ];
    let (stop, fetch) = std::thread::scope(|scope| {
        let stop = scope.spawn(|| codex.hook_output("Stop", None));
        let fetch = scope.spawn(|| codex.model_fetch());
        (stop.join().unwrap(), fetch.join().unwrap())
    });
    assert!(stop.status.success(), "{stop:?}");
    assert!(fetch.status.success(), "{fetch:?}");
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&stop.stdout),
        String::from_utf8_lossy(&fetch.stdout)
    );
    for (id, body) in pair
        .iter()
        .zip(["overlap task one 0e7f", "overlap task two 93c2"])
    {
        assert_eq!(
            printed.matches(body).count(),
            1,
            "{id} is handed to one of them, once: {printed}"
        );
        codex.committed(id, 1);
    }
}
