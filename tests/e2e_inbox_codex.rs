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
with open(base + '.env','w') as f: json.dump(dict(os.environ), f)
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
        let base = daemon.home.join("codex-inbox");
        std::fs::write(base.with_extension("screen"), text).unwrap();
        let reply = attach(
            daemon,
            json!({"op":"start-agent","command":common::capability_export_command(&format!("exec '{}'",program.display())),
            "agent_type":"codex","cwd":daemon.home,"rows":40,"cols":140,
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

/// Scenario: Paint an unknown startup dialog in wrapped synthetic Codex and queue a task. The deck must withhold every byte until the child paints the positively recognised composer, then ring the fixed line without a launch prompt.
#[spec("inbox/codex/004")]
#[test]
fn codex_004_unknown_dialog_holds_until_composer() {
    let codex = Codex::new(UNKNOWN);
    let id = codex.queued(BODY);
    codex.no_bytes();
    codex.paint(COMPOSER);
    assert!(
        common::wait_until(WAIT, || bytes(&codex.pane)
            == format!("{}\r", fixed_line(&id)).as_bytes()),
        "recognised composer must ring: {:?}",
        bytes(&codex.pane)
    );
    codex.paint(UNKNOWN);
    fetch(&codex.daemon, &codex.pane, true);
    unchanged(&codex.pane, Duration::from_millis(1300));
    assert_eq!(
        bytes(&codex.pane),
        format!("{}\r", fixed_line(&id)).as_bytes(),
        "no cleanup CR into the unknown dialog"
    );
}

/// Scenario: Start wrapped synthetic Codex directly at its empty composer before any own hook fires and queue a task. The fixed doorbell must be submitted without a launch-command prompt or body bytes.
#[spec("inbox/codex/005")]
#[test]
fn codex_005_recognised_composer_rings_before_first_hook() {
    let codex = Codex::new(COMPOSER);
    let id = codex.queued(BODY);
    assert!(
        common::wait_until(WAIT, || bytes(&codex.pane)
            == format!("{}\r", fixed_line(&id)).as_bytes()),
        "composer must ring before the first own hook: {:?}",
        bytes(&codex.pane)
    );
}

/// Scenario: Strand a fixed line in Codex's recognized composer and pause its CR-only retry before admission. Fetch that selected task while another stays queued; releasing the attempt must send no CR and must not submit the settled task.
#[spec("inbox/doorbell/031")]
#[test]
fn doorbell_031_codex_flush_only_rechecks_selected_message() {
    let codex = Codex::new_with(
        COMPOSER,
        &[
            ("DOT_AGENT_DECK_DOORBELL_SCHEDULE_MS", "500"),
            ("DOT_AGENT_DECK_TEST_DOORBELL_PAUSE_PANE", "codex-inbox"),
            ("DOT_AGENT_DECK_TEST_DOORBELL_PAUSE_DIR", "ring-barriers"),
        ],
    );
    let root = codex.daemon.home.join("ring-barriers");
    std::fs::create_dir_all(&root).unwrap();
    let _budget = InterleaveBudget::start(&root);
    std::fs::write(root.join("before-pre-cr.armed"), "").unwrap();
    let base = codex.pane.capture.with_extension("strand");
    std::fs::write(base.with_extension("empty"), COMPOSER).unwrap();
    std::fs::write(&base, "").unwrap();
    let id = codex.queued(BODY);
    assert!(
        common::wait_until(wait_budget(), || root
            .join("before-pre-cr.reached")
            .exists()),
        "recognized Codex CR-only retry must reach its fence"
    );
    let first = format!("{}\r", fixed_line(&id));
    assert_eq!(bytes(&codex.pane), first.as_bytes());
    fetch(&codex.daemon, &codex.pane, true);
    state(&codex.daemon, &codex.pane, &id, "acknowledged");
    let other = codex.queued("CODEX-SECOND-QUEUED");
    state(&codex.daemon, &codex.pane, &other, "queued");
    std::fs::write(root.join("completed.armed"), "").unwrap();
    std::fs::write(root.join("before-pre-cr.release"), "").unwrap();
    assert!(
        common::wait_until(wait_budget(), || root.join("completed.reached").exists()),
        "CR-only attempt must complete"
    );
    drained(&codex.pane);
    assert_eq!(
        bytes(&codex.pane),
        first.as_bytes(),
        "Codex CR-only retry must not submit the settled selected ID while another message remains queued"
    );
    assert!(
        std::fs::read_to_string(base.with_extension("submitted"))
            .unwrap_or_default()
            .is_empty(),
        "settled stranded Codex line was submitted"
    );
    std::fs::write(root.join("completed.release"), "").unwrap();
}

/// Scenario: Type into synthetic Codex and press Enter during its paste burst, which leaves the draft visible. A queued message must send zero bytes while that text remains, even after a native hook; clearing the composer must let the fixed doorbell ring separately, including when the screen poll starts after the placeholder has been replaced by the doorbell.
#[spec("inbox/codex/012")]
#[test]
fn codex_012_retained_paste_burst_draft_holds_doorbell() {
    for own_hook in [false, true] {
        let codex = Codex::new(COMPOSER);
        if own_hook {
            codex.run_hook("SessionStart", None);
        }
        let base = codex.pane.capture.with_extension("keep");
        std::fs::write(&base, "").unwrap();
        std::fs::write(base.with_extension("empty"), COMPOSER).unwrap();
        let mut input = common::AttachInput::attach(&codex.daemon.attach_socket, &codex.pane.agent);
        input.write("human-retained-draft\r");
        assert!(
            common::wait_until(WAIT, || screen(&codex.daemon, &codex.pane)
                .contains("› human-retained-draft")),
            "stand-in must keep the human draft visibly on Enter"
        );
        let before = bytes(&codex.pane);
        let id = codex.queued(BODY);
        assert!(
            !common::wait_until(Duration::from_millis(1300), || bytes(&codex.pane) != before),
            "retained visible Codex draft must receive ZERO doorbell bytes (own_hook={own_hook}); before={before:?}; after={:?}\nPane:\n{}",
            bytes(&codex.pane),
            screen(&codex.daemon, &codex.pane)
        );
        state(&codex.daemon, &codex.pane, &id, "queued");
        input.write("\x15");
        if own_hook {
            // Reproduce a slow screen poll deterministically: the doorbell
            // may finish before the poll observes the empty placeholder.
            assert!(common::wait_until(WAIT, || bytes(&codex.pane)
                .ends_with(format!("{}\r", fixed_line(&id)).as_bytes())));
        }
        assert!(
            common::wait_until(WAIT, || {
                let shown = screen(&codex.daemon, &codex.pane);
                shown.contains("Ask Codex to do anything") || shown.contains(&fixed_line(&id))
            }),
            "Ctrl+U must show an empty composer or its subsequently delivered doorbell"
        );
        assert!(
            common::wait_until(WAIT, || bytes(&codex.pane)
                .ends_with(format!("{}\r", fixed_line(&id)).as_bytes())),
            "empty composer must release doorbell: {:?}",
            bytes(&codex.pane)
        );
    }
}

/// Scenario: Start wrapped synthetic Codex at its empty composer, make it drop the first Enter after the doorbell line (as a real Codex 0.160.0 was seen to do), and queue a task. The line stays in the composer with the shortcuts hint hidden; the next ring must submit it with one bare carriage return instead of waiting for an empty composer forever, and must not type a second line on top of it.
#[spec("inbox/codex/013")]
#[test]
fn codex_013_a_stranded_doorbell_line_is_submitted_by_the_next_ring() {
    let codex = Codex::new_with(COMPOSER, &[("DOT_AGENT_DECK_DOORBELL_SCHEDULE_MS", "400")]);
    let strand = codex.pane.capture.with_extension("strand");
    std::fs::write(strand.with_extension("empty"), COMPOSER).unwrap();
    std::fs::write(&strand, "").unwrap();
    let id = codex.queued(BODY);
    let line = fixed_line(&id);
    assert!(
        common::wait_until(WAIT, || screen(&codex.daemon, &codex.pane)
            .contains(&format!("›  {}", line.trim_start()))),
        "the stand-in must leave the rung line in its composer: {}",
        screen(&codex.daemon, &codex.pane)
    );
    let flushed = format!("{line}\r\r");
    assert!(
        common::wait_until(WAIT, || bytes(&codex.pane).starts_with(flushed.as_bytes())),
        "the next ring must submit the stranded line with a bare CR: {:?}\nPane:\n{}",
        String::from_utf8_lossy(&bytes(&codex.pane)),
        screen(&codex.daemon, &codex.pane)
    );
    // What follows the flush is the next ring's own pre-ring CR (the stand-in
    // keeps the message queued), never a line typed on top of the flush.
    assert!(
        common::wait_until(WAIT, || bytes(&codex.pane).len() > flushed.len()),
        "the doorbell must ring again while the message stays queued"
    );
    assert_eq!(
        bytes(&codex.pane)[flushed.len()],
        b'\r',
        "no second line may be typed on top of the flush: {:?}",
        String::from_utf8_lossy(&bytes(&codex.pane))
    );
    let submitted = strand.with_extension("submitted");
    assert_eq!(
        std::fs::read_to_string(&submitted)
            .unwrap_or_default()
            .lines()
            .next(),
        Some(line.as_str()),
        "the flush must submit the stranded line itself"
    );
}

/// The stand-in's composer holding `text`, shortcuts hint hidden as Codex
/// 0.160 hides it while the composer holds text.
fn holding(text: &str) -> String {
    COMPOSER
        .replace("Ask Codex to do anything", text)
        .replace("? for shortcuts", "")
}

/// Scenario: Start wrapped synthetic Codex that drops the first Enter after text, queue a task and let its ring strand in the composer. Then repaint the composer so it holds a doorbell-shaped line naming a different message, cursor after it, while the deck's own ring is still outstanding: it must receive zero bytes. Repainting the deck's own line brings the flushing carriage return.
#[spec("inbox/codex/014")]
#[test]
fn codex_014_a_doorbell_shaped_line_the_deck_did_not_type_gets_no_byte() {
    let codex = Codex::new_with(COMPOSER, &[("DOT_AGENT_DECK_DOORBELL_SCHEDULE_MS", "2000")]);
    let strand = codex.pane.capture.with_extension("strand");
    std::fs::write(strand.with_extension("empty"), COMPOSER).unwrap();
    std::fs::write(&strand, "").unwrap();
    let id = codex.queued(BODY);
    let line = fixed_line(&id);
    let rung = format!("{line}\r");
    assert!(
        common::wait_until(WAIT, || bytes(&codex.pane) == rung.as_bytes()
            && screen(&codex.daemon, &codex.pane)
                .contains(&format!("›  {}", line.trim_start()))),
        "the stand-in must leave the rung line in its composer: {:?}\nPane:\n{}",
        String::from_utf8_lossy(&bytes(&codex.pane)),
        screen(&codex.daemon, &codex.pane)
    );
    let crafted = fixed_line("m-crafted0-0000000000000000");
    std::fs::write(
        codex.pane.capture.with_extension("screen"),
        holding(&crafted),
    )
    .unwrap();
    assert!(
        common::wait_until(WAIT, || screen(&codex.daemon, &codex.pane)
            .contains(crafted.trim_start())),
        "the crafted line must be on screen before the next ring is due: {}",
        screen(&codex.daemon, &codex.pane)
    );
    assert!(
        !common::wait_until(Duration::from_millis(4500), || bytes(&codex.pane).len()
            > rung.len()),
        "a doorbell-shaped line naming another message must get no byte, not even the flushing CR: {:?}\nPane:\n{}",
        String::from_utf8_lossy(&bytes(&codex.pane)),
        screen(&codex.daemon, &codex.pane)
    );
    state(&codex.daemon, &codex.pane, &id, "queued");
    // Control: the same ring, due all along, flushes the deck's own line.
    std::fs::write(codex.pane.capture.with_extension("screen"), holding(&line)).unwrap();
    let flushed = format!("{line}\r\r");
    assert!(
        common::wait_until(WAIT, || bytes(&codex.pane).starts_with(flushed.as_bytes())),
        "the deck's own stranded line must still be flushed: {:?}\nPane:\n{}",
        String::from_utf8_lossy(&bytes(&codex.pane)),
        screen(&codex.daemon, &codex.pane)
    );
}

/// Scenario: Start wrapped synthetic Codex that drops the first Enter after text, queue a task, and let the next ring flush the stranded line. Then repaint the composer holding that same line and fetch the message so it leaves `queued` while no ring is due: the owed cleanup is the one carriage return that goes, and after it the still-painted line gets zero bytes. A second message queued under that line, which the deck never typed there, gets nothing either until the composer is empty again.
#[spec("inbox/codex/015")]
#[test]
fn codex_015_no_flush_once_the_cleanup_cr_dropped_the_record() {
    let codex = Codex::new_with(
        COMPOSER,
        &[("DOT_AGENT_DECK_DOORBELL_SCHEDULE_MS", "400,60000")],
    );
    let strand = codex.pane.capture.with_extension("strand");
    std::fs::write(strand.with_extension("empty"), COMPOSER).unwrap();
    std::fs::write(&strand, "").unwrap();
    let id = codex.queued(BODY);
    let line = fixed_line(&id);
    let flushed = format!("{line}\r\r");
    assert!(
        common::wait_until(WAIT, || bytes(&codex.pane) == flushed.as_bytes()
            && screen(&codex.daemon, &codex.pane)
                .contains("› Ask Codex to do anything")),
        "the second ring must flush the stranded line and leave the composer empty: {:?}\nPane:\n{}",
        String::from_utf8_lossy(&bytes(&codex.pane)),
        screen(&codex.daemon, &codex.pane)
    );
    std::fs::write(codex.pane.capture.with_extension("screen"), holding(&line)).unwrap();
    assert!(
        common::wait_until(WAIT, || screen(&codex.daemon, &codex.pane)
            .contains(&format!("›  {}", line.trim_start()))),
        "the repainted line must be on screen before the message leaves queued: {}",
        screen(&codex.daemon, &codex.pane)
    );
    fetch(&codex.daemon, &codex.pane, true);
    let cleaned = format!("{flushed}\r");
    assert!(
        common::wait_until(WAIT, || bytes(&codex.pane) == cleaned.as_bytes()),
        "the message leaving queued is owed its one cleanup CR: {:?}\nPane:\n{}",
        String::from_utf8_lossy(&bytes(&codex.pane)),
        screen(&codex.daemon, &codex.pane)
    );
    // The stand-in repaints its empty composer after that CR; paint the line
    // back only once it has, so its repaint cannot land on top.
    assert!(
        common::wait_until(WAIT, || screen(&codex.daemon, &codex.pane)
            .contains("› Ask Codex to do anything")),
        "the stand-in must repaint after the cleanup CR: {}",
        screen(&codex.daemon, &codex.pane)
    );
    std::fs::write(codex.pane.capture.with_extension("screen"), holding(&line)).unwrap();
    assert!(
        common::wait_until(WAIT, || screen(&codex.daemon, &codex.pane)
            .contains(&format!("›  {}", line.trim_start()))),
        "the line must be painted back: {}",
        screen(&codex.daemon, &codex.pane)
    );
    assert!(
        !common::wait_until(Duration::from_millis(4500), || bytes(&codex.pane).len()
            > cleaned.len()),
        "once the message has left queued its line gets no further CR: {:?}\nPane:\n{}",
        String::from_utf8_lossy(&bytes(&codex.pane)),
        screen(&codex.daemon, &codex.pane)
    );
    let next = codex.queued(BODY);
    assert!(
        !common::wait_until(Duration::from_millis(4500), || bytes(&codex.pane).len()
            > cleaned.len()),
        "a line the deck has no record of gets nothing, not even for a due ring: {:?}\nPane:\n{}",
        String::from_utf8_lossy(&bytes(&codex.pane)),
        screen(&codex.daemon, &codex.pane)
    );
    state(&codex.daemon, &codex.pane, &next, "queued");
    // Control: once the composer is empty the doorbell rings again.
    std::fs::write(codex.pane.capture.with_extension("screen"), COMPOSER).unwrap();
    let rung_again = format!("{cleaned}{}\r", fixed_line(&next));
    assert!(
        common::wait_until(WAIT, || bytes(&codex.pane)
            .starts_with(rung_again.as_bytes())),
        "an empty composer must take the next ring: {:?}",
        String::from_utf8_lossy(&bytes(&codex.pane))
    );
}

/// Scenario: Start wrapped synthetic Codex that, as real Codex 0.160.0 did under CPU pressure, takes the doorbell's Enter as a newline in its composer, leaving the line there with the cursor on the empty row below it at the composer indent, and queue a task. The next ring must submit the line with one bare carriage return instead of waiting forever, and type no second line on top of it.
#[spec("inbox/codex/016")]
#[test]
fn codex_016_a_line_whose_enter_became_a_newline_is_submitted_by_the_next_ring() {
    let codex = Codex::new_with(COMPOSER, &[("DOT_AGENT_DECK_DOORBELL_SCHEDULE_MS", "400")]);
    let newline = codex.pane.capture.with_extension("newline");
    std::fs::write(newline.with_extension("empty"), COMPOSER).unwrap();
    std::fs::write(&newline, "").unwrap();
    let id = codex.queued(BODY);
    let line = fixed_line(&id);
    let rung = format!("{line}\r");
    assert!(
        common::wait_until(WAIT, || bytes(&codex.pane).starts_with(rung.as_bytes())
            && screen(&codex.daemon, &codex.pane)
                .contains(&format!("›  {}", line.trim_start()))),
        "the stand-in must leave the rung line in its composer: {:?}\nPane:\n{}",
        String::from_utf8_lossy(&bytes(&codex.pane)),
        screen(&codex.daemon, &codex.pane)
    );
    let flushed = format!("{rung}\r");
    assert!(
        common::wait_until(WAIT, || bytes(&codex.pane).starts_with(flushed.as_bytes())),
        "the next ring must submit the line with a bare CR: {:?}\nPane:\n{}",
        String::from_utf8_lossy(&bytes(&codex.pane)),
        screen(&codex.daemon, &codex.pane)
    );
    let submitted = newline.with_extension("submitted");
    assert!(
        common::wait_until(WAIT, || std::fs::read_to_string(&submitted)
            .unwrap_or_default()
            .lines()
            .next()
            == Some(line.as_str())),
        "the flush must submit the line itself: {:?}",
        std::fs::read_to_string(&submitted)
    );
    // The stand-in keeps the message queued, so the doorbell rings again:
    // that ring starts with its own pre-ring CR, never a line on top.
    assert!(
        common::wait_until(WAIT, || bytes(&codex.pane).len() > flushed.len()),
        "the doorbell must ring again while the message stays queued"
    );
    assert_eq!(
        bytes(&codex.pane)[flushed.len()],
        b'\r',
        "no second line may be typed on top of the flush: {:?}",
        String::from_utf8_lossy(&bytes(&codex.pane))
    );
}

/// Scenario: Start wrapped synthetic Codex that takes the doorbell's Enter as a newline in its composer and then also ignores the first flushing carriage return, and queue a task. The line is still the deck's own and still stranded, so the following ring must send another lone carriage return, which submits it: one swallowed flush must not strand the worker.
#[spec("inbox/codex/017")]
#[test]
fn codex_017_a_swallowed_flush_is_retried_by_the_next_ring() {
    let codex = Codex::new_with(COMPOSER, &[("DOT_AGENT_DECK_DOORBELL_SCHEDULE_MS", "400")]);
    let newline = codex.pane.capture.with_extension("newline");
    std::fs::write(newline.with_extension("empty"), COMPOSER).unwrap();
    std::fs::write(newline.with_extension("swallow"), "").unwrap();
    std::fs::write(&newline, "").unwrap();
    let id = codex.queued(BODY);
    let line = fixed_line(&id);
    let swallowed = format!("{line}\r\r");
    assert!(
        common::wait_until(WAIT, || bytes(&codex.pane)
            .starts_with(swallowed.as_bytes())
            && !newline.with_extension("swallow").exists()),
        "the stand-in must swallow the first flushing CR: {:?}\nPane:\n{}",
        String::from_utf8_lossy(&bytes(&codex.pane)),
        screen(&codex.daemon, &codex.pane)
    );
    let submitted = newline.with_extension("submitted");
    let retried = format!("{swallowed}\r");
    assert!(
        common::wait_until(WAIT, || bytes(&codex.pane).starts_with(retried.as_bytes())
            && std::fs::read_to_string(&submitted)
                .unwrap_or_default()
                .lines()
                .next()
                == Some(line.as_str())),
        "the next ring's lone CR must submit the still-stranded line: {:?} submitted {:?}\nPane:\n{}",
        String::from_utf8_lossy(&bytes(&codex.pane)),
        std::fs::read_to_string(&submitted),
        screen(&codex.daemon, &codex.pane)
    );
}

/// Scenario: Report Codex's own busy hook, replace its screen with an unfamiliar mid-session screen and queue a task. Its own Idle must still wait for an empty recognised composer; restarting in the same pane on a startup dialog must keep every byte out of that dialog.
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
    codex.paint(COMPOSER);
    assert!(common::wait_until(WAIT, || bytes(&codex.pane)
        .windows(fixed_line(&id).len())
        .any(|w| w == fixed_line(&id).as_bytes())));
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

/// Scenario: With none of the deck's Codex hooks running (untrusted), queue a task for idle synthetic Codex; the deck types its doorbell, and a stand-in model runs the real `dot-agent-deck inbox fetch` in the pane, which prints the framing and the block naming its attempt and settles it by its receipt. With the pane's receipt records made unwritable, a second fetch still prints and settles its task, and the card says messages may repeat.
#[spec("inbox/codex/020")]
#[test]
fn codex_020_untrusted_hooks_a_model_run_fetch_commits_on_receipts() {
    let codex = Codex::new(COMPOSER);
    let id = codex.queued("untrusted-hooks task 7c41");
    assert!(
        common::wait_until(WAIT, || bytes(&codex.pane)
            == format!("{}\r", fixed_line(&id)).as_bytes()),
        "the doorbell asks the model to fetch: {:?}",
        bytes(&codex.pane)
    );
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

/// Scenario: With Codex's prompt hook switched off and its Stop hook trusted, an idle task reaches the model through the doorbell and a model-run `inbox fetch`, and a busy task through the real Stop hook; then a Stop hook and a model-run fetch run at once over two queued tasks. Each task is handed over to exactly one of them, printed once and settled by its receipt once.
#[spec("inbox/codex/021")]
#[test]
fn codex_021_prompt_hook_off_doorbell_fetch_idle_and_stop_busy_each_once() {
    let codex = Codex::new(COMPOSER);
    let idle = codex.queued("idle task via fetch 51aa");
    assert!(
        common::wait_until(WAIT, || bytes(&codex.pane)
            == format!("{}\r", fixed_line(&idle)).as_bytes()),
        "{:?}",
        bytes(&codex.pane)
    );
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
