#![cfg(all(feature = "e2e", unix))]

//! PRD #1666 part 3, Q27 and Q28 (`inbox/codex/027`, `inbox/codex/028`): the
//! doorbell rung into a Codex generation the deck has measured, through the
//! real wrapper, writer, hook CLI and daemon, against a scripted Codex screen
//! (`tests/support/codex_composer.py`) that draws the measured header,
//! composer, transcript and dialogs. No Codex CLI, model or credential is
//! used.
//!
//! What is synthetic, and so not proven here: the screen is a script, not
//! Codex; 027's generations are made ready by its own hooks run by hand
//! (`Deck::idle`), where a real Codex sends its first only with its first
//! prompt (028 has no such step); the hooks are trusted through a `codex
//! app-server` stand-in, so the wrapper reports nothing from the screen's
//! output (the untrusted-hooks path is not covered here); and until the
//! release records carry 0.164.0's `head_echo` (the Q27 Go change) the daemon
//! is asked to assume it (`DOT_AGENT_DECK_E2E_ASSUME_CODEX_HEAD_ECHO`, read
//! only by the e2e build).
mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;

use inbox::*;
use serde_json::{Value, json};
use spec::spec;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// The measured release, and one that inherits from it (Q13).
const MEASURED: &str = "0.164.0";
const INHERITED: &str = "0.165.0";
/// The settle windows the deck gives them: 0.164.0's slowest measured
/// start-up dialog plus half, and the inherited floor.
const MEASURED_SETTLE: Duration = Duration::from_millis(6030);
const INHERITED_SETTLE: Duration = Duration::from_secs(10);
/// Long enough for a fresh generation's settle window and its ring.
const RING_WAIT: Duration = Duration::from_secs(40);

/// The daemon's and the pane's `PATH`: first the `codex app-server`
/// stand-in the deck's wrapper asks for its hooks' trust hashes, so the hooks
/// are trusted and the wrapper reports nothing from the screen's output.
fn path() -> String {
    let stand_in = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-synthetic");
    format!("{}:/usr/bin:/bin", stand_in.display())
}

/// What that stand-in answers to `hooks/list`: the deck's own hooks, by the
/// command the install wrote (`tests/e2e_codex_status.rs` answers the same).
fn hook_list_response() -> String {
    let entry = |event: &str, camel: &str| {
        json!({
            "key": format!("__CODEX_HOME__/hooks.json:{event}:0:0"),
            "eventName": camel, "handlerType": "command", "matcher": null,
            "command": "__DECK_COMMAND__", "timeoutSec": 600, "statusMessage": null,
            "sourcePath": "__CODEX_HOME__/hooks.json", "source": "user", "pluginId": null,
            "displayOrder": 0, "enabled": true, "isManaged": false,
            "currentHash": format!("sha256:{event}"), "trustStatus": "untrusted"
        })
    };
    json!({"id": 2, "result": {"data": [{"cwd": "/workspace", "hooks": [
        entry("session_start", "sessionStart"),
        entry("user_prompt_submit", "userPromptSubmit"),
        entry("pre_tool_use", "preToolUse"),
        entry("post_tool_use", "postToolUse"),
        entry("permission_request", "permissionRequest"),
        entry("stop", "stop"),
    ], "warnings": [], "errors": []}]}})
    .to_string()
}

/// One sandbox daemon with its log.
struct Deck {
    daemon: common::DaemonProc,
    log: PathBuf,
    script: PathBuf,
    _bin: tempfile::TempDir,
}

/// One scripted Codex generation in a pane.
struct Codex {
    pane: Pane,
    base: PathBuf,
}

impl Deck {
    fn new() -> Self {
        let bin = common::harness_tempdir().unwrap();
        let script = bin.path().join("codex-composer.py");
        std::fs::write(&script, include_str!("support/codex_composer.py")).unwrap();
        let log = bin.path().join("deck.log");
        let hooks = hook_list_response();
        let daemon = daemon_unbound(&[
            ("PATH", path().as_str()),
            ("CODEX_HOOK_LIST_RESPONSE", hooks.as_str()),
            ("DOT_AGENT_DECK_LOG", log.to_str().unwrap()),
            ("RUST_LOG", "doorbell=debug"),
            ("DOT_AGENT_DECK_E2E_ASSUME_CODEX_HEAD_ECHO", MEASURED),
            // A held task shows its reason after two seconds, not two minutes.
            ("DOT_AGENT_DECK_INBOX_WAKE_STALL_AFTER_MS", "2000"),
        ]);
        Deck {
            daemon,
            log,
            script,
            _bin: bin,
        }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// The log's lines naming `agent` (quoted, as the doorbell's lines put
    /// it, or bare, as `pane release` puts it) and containing `needle`.
    fn lines(&self, agent: &str, needle: &str) -> Vec<String> {
        let quoted = format!("agent_id=\"{agent}\"");
        let bare = format!("agent_id={agent} ");
        self.log()
            .lines()
            .filter(|l| (l.contains(&quoted) || l.contains(&bare)) && l.contains(needle))
            .map(str::to_string)
            .collect()
    }

    /// Start the scripted Codex screen drawing `release` in pane `name`,
    /// spawned as `agent_type`, or with none for the deck to learn from the
    /// agent's own hooks; `before` is written beside it first.
    fn codex(
        &self,
        name: &str,
        release: &str,
        agent_type: Option<&str>,
        before: &[(&str, &str)],
    ) -> Codex {
        self.codex_with(name, release, agent_type, before, (40, 140), "")
    }

    /// [`Self::codex`] at `size` (rows, columns), with `launcher` run in the
    /// pane's shell before the screen starts, as a launcher prints before the
    /// agent it runs.
    fn codex_with(
        &self,
        name: &str,
        release: &str,
        agent_type: Option<&str>,
        before: &[(&str, &str)],
        size: (u16, u16),
        launcher: &str,
    ) -> Codex {
        let base = self.daemon.home.join(name);
        for (suffix, text) in before {
            std::fs::write(format!("{}{suffix}", base.display()), text).unwrap();
        }
        let quote = |path: &Path| path.to_string_lossy().replace('\'', "'\\''");
        let command = common::capability_export_command(&format!(
            "{launcher}exec python3 '{}' '{}' '{}' '{release}'",
            quote(&self.script),
            quote(&base),
            quote(Path::new(env!("CARGO_BIN_EXE_dot-agent-deck")))
        ));
        let reply = attach(
            &self.daemon,
            json!({"op":"start-agent","command":command,"agent_type":agent_type,
            "cwd":self.daemon.home,"rows":size.0,"cols":size.1,
            "env":[["DOT_AGENT_DECK_PANE_ID",name],["CODEX_HOME",self.daemon.home.join(".codex")],
                ["PATH",path()]],
            "display_name":name}),
        );
        assert_eq!(reply["ok"], true, "start the scripted Codex: {reply}");
        let agent = reply["id"].as_str().unwrap().to_string();
        let pid = base.with_extension("pid");
        assert!(
            common::wait_until(common::child_boot_budget(), || std::fs::read_to_string(
                &pid
            )
            .is_ok_and(|text| text.parse::<u32>().is_ok())),
            "the scripted Codex must start"
        );
        let token = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(common::recorded_hook_capability(&self.daemon.home, &agent))
            .expect("the pane's capability");
        let pane = Pane {
            id: name.into(),
            agent,
            token,
            pid: std::fs::read_to_string(&pid).unwrap().parse().unwrap(),
            capture: base.with_extension("bytes"),
            attach_socket: self.daemon.attach_socket.clone(),
        };
        Codex { pane, base }
    }

    /// The deck's own Codex hook for `event`, run with the pane's identity.
    fn hook(&self, codex: &Codex, event: &str) {
        // The generation's own session, never another pane's or its
        // predecessor's.
        let payload = json!({"hook_event_name":event,"session_id":format!("{}-session", codex.pane.agent),
            "cwd":self.daemon.home});
        let mut child = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
            .args(["hook", "--agent", "codex"])
            .env_clear()
            .env("HOME", &self.daemon.home)
            .env("CODEX_HOME", self.daemon.home.join(".codex"))
            .env("DOT_AGENT_DECK_SOCKET", &self.daemon.hook_socket)
            .env("DOT_AGENT_DECK_PANE_ID", &codex.pane.id)
            .env("DOT_AGENT_DECK_AGENT_ID", &codex.pane.agent)
            .env("DOT_AGENT_DECK_PANE_CAPABILITY", &codex.pane.token)
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
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "hook {event}: {output:?}");
    }

    /// The generation's own SessionStart and Stop: started, then idle.
    fn idle(&self, codex: &Codex) {
        self.hook(codex, "SessionStart");
        self.hook(codex, "Stop");
    }

    /// The deck latched `release` from the generation's start-up header.
    fn latched(&self, codex: &Codex, release: &str) {
        let want = format!("release={release}");
        assert!(
            common::wait_until(WAIT, || !self
                .lines(&codex.pane.agent, "pane release")
                .iter()
                .all(|l| !l.contains(&want))),
            "the deck latches {release}: {}",
            self.log()
        );
    }

    /// The settle anchor the deck took for the generation (CLOCK_MONOTONIC
    /// ns).
    fn anchor(&self, codex: &Codex) -> u64 {
        let line = self
            .lines(&codex.pane.agent, "settle anchor")
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("a settle anchor: {}", self.log()));
        let at = line.find("anchor=").unwrap() + "anchor=".len();
        line[at..]
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .unwrap()
            .parse()
            .unwrap()
    }

    /// The agent's record, as `list-agents` and the desktop read it.
    fn record(&self, codex: &Codex) -> Value {
        attach(&self.daemon, json!({"op":"list-agents"}))["agent_records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == codex.pane.agent)
            .cloned()
            .unwrap_or_default()
    }

    fn tripped(&self) -> bool {
        self.log().contains("pane release: tripwire set")
    }

    /// Stop the generation in `codex`'s pane, keeping the pane's name free.
    fn stop(&self, codex: Codex) {
        assert_eq!(
            attach(
                &self.daemon,
                json!({"op":"stop-agent","id":codex.pane.agent})
            )["ok"],
            true
        );
        let base = codex.base.clone();
        // Its drop asks for the same stop again, which the daemon ignores.
        drop(codex);
        for suffix in [
            "pid",
            "bytes",
            "inputs",
            "turns",
            "hooks",
            "dialog_bytes",
            "dialog",
            "dialog_on_head",
            "keep",
            "swallow",
            "extra",
            "hidden",
            "redraw",
            "noheader",
        ] {
            let _ = std::fs::remove_file(base.with_extension(suffix));
        }
    }
}

impl Codex {
    fn file(&self, suffix: &str) -> PathBuf {
        PathBuf::from(format!("{}{suffix}", self.base.display()))
    }

    fn set(&self, suffix: &str, text: &str) {
        std::fs::write(self.file(suffix), text).unwrap();
    }

    fn clear(&self, suffix: &str) {
        let _ = std::fs::remove_file(self.file(suffix));
    }

    /// Each read the screen took from its terminal: when, and the bytes.
    fn inputs(&self) -> Vec<(u64, String)> {
        std::fs::read_to_string(self.base.with_extension("inputs"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .map(|v| {
                (
                    v["t"].as_u64().unwrap(),
                    v["bytes"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    fn typed(&self) -> String {
        String::from_utf8_lossy(&bytes(&self.pane)).into_owned()
    }

    fn dialog_bytes(&self) -> Vec<u8> {
        std::fs::read(self.base.with_extension("dialog_bytes")).unwrap_or_default()
    }

    /// The doorbell line the deck typed for `id`, once submitted.
    fn rung(&self, id: &str) -> Option<String> {
        rung_line(&self.pane, id)
    }
}

/// The token head `(xxxxxx)` of a ring line.
fn head(line: &str) -> &str {
    &line[..8]
}

/// Ring `id` into `codex` and wait for it to be submitted and the task taken
/// by the prompt hook; returns the line.
fn delivered(deck: &Deck, codex: &Codex, id: &str) -> String {
    assert!(
        common::wait_until(RING_WAIT, || codex.rung(id).is_some()),
        "the doorbell rings {id}: {:?}\n{}",
        codex.typed(),
        deck.log()
    );
    state(&deck.daemon, &codex.pane, id, "acknowledged");
    codex.rung(id).unwrap()
}

/// Scenario: Start the scripted Codex screen drawing measured release 0.164.0 behind the real wrapper, report its own SessionStart and Stop, and queue a task. No byte reaches it before its settle window (the slowest measured start-up dialog plus half, 6.03 s after the anchor) has passed; then the deck types the token head `(xxxxxx)` alone, the rest of the line once the head is on screen, and one Enter, nothing else; Codex's own prompt hook hands the task over and it is acknowledged, and the record's WAKE names Codex's turn-end hand-over with nothing holding the doorbell.
#[spec("inbox/codex/027")]
#[test]
fn codex_027_a_measured_codex_is_rung_in_two_stages_after_its_settle_window() {
    let deck = Deck::new();
    let codex = deck.codex("codex-027", MEASURED, Some("codex"), &[]);
    deck.latched(&codex, MEASURED);
    deck.idle(&codex);
    let id = enqueue(&deck.daemon, &codex.pane, BODY);
    let line = delivered(&deck, &codex, &id);
    assert!(is_ring_line(&line, &id), "{line}");
    assert_eq!(
        codex.typed(),
        format!("{line}\r"),
        "the head, the rest and one Enter, nothing else"
    );
    let inputs = codex.inputs();
    assert_eq!(
        inputs[0].1,
        head(&line),
        "the first write is the token head alone: {inputs:?}"
    );
    let anchor = deck.anchor(&codex);
    assert!(
        inputs[0].0 >= anchor + MEASURED_SETTLE.as_nanos() as u64,
        "no byte before the settle window: first {} ns after the anchor",
        inputs[0].0.saturating_sub(anchor)
    );
    let attempts = deck.lines(&codex.pane.agent, "doorbell attempt");
    assert!(
        attempts
            .iter()
            .any(|l| l.contains("staged=true") && l.contains("release=\"0.164.0\"")),
        "{attempts:?}"
    );
    // Codex's own Stop hook takes its messages at turn end, the route WAKE
    // names first; nothing holds the doorbell behind it.
    let record = deck.record(&codex);
    assert_eq!(record["inbox"]["wake"], "turn_end", "{record}");
    assert!(record["inbox"]["wake_hold"].is_null(), "{record}");
}

/// Scenario: With measured Codex 0.164.0's header latched and the generation idle, open each of its post-ready dialogs in turn (update, folder trust, hooks review) and queue a task. After the settle window the deck's admission sees the dialog and types nothing, neither into the dialog nor anywhere else, and logs the dialog as why, with no tripwire set on a measured release; once the dialog closes the empty composer is rung and the task delivered.
#[spec("inbox/codex/027")]
#[test]
fn codex_027_a_dialog_at_admission_gets_no_byte() {
    for dialog in ["update", "trust", "hooks"] {
        let deck = Deck::new();
        let codex = deck.codex(&format!("codex-027-{dialog}"), MEASURED, Some("codex"), &[]);
        deck.latched(&codex, MEASURED);
        codex.set(".dialog", dialog);
        deck.idle(&codex);
        let id = enqueue(&deck.daemon, &codex.pane, BODY);
        let why = format!("Dialog(\"{dialog}\")");
        assert!(
            common::wait_until(RING_WAIT, || !deck
                .lines(&codex.pane.agent, &why)
                .is_empty()),
            "{dialog}: admission refuses the dialog: {}",
            deck.log()
        );
        unchanged(&codex.pane, Duration::from_millis(1500));
        assert!(
            bytes(&codex.pane).is_empty(),
            "{dialog}: {:?}",
            codex.typed()
        );
        assert!(codex.dialog_bytes().is_empty(), "{dialog}");
        state(&deck.daemon, &codex.pane, &id, "queued");
        assert!(
            !deck.tripped(),
            "{dialog}: a measured release has no tripwire"
        );

        codex.clear(".dialog");
        let line = delivered(&deck, &codex, &id);
        assert_eq!(codex.typed(), format!("{line}\r"), "{dialog}");
    }
}

/// Scenario: Measured Codex 0.164.0 opens its folder-trust dialog the moment the deck's token head lands in its composer. The deck types nothing more: no rest of the line, no Enter and no delete, so the dialog receives no byte; the attempt ends unresolved with the dialog named, and the card says a dialog opened while the deck was typing.
#[spec("inbox/codex/027")]
#[test]
fn codex_027_a_dialog_on_the_head_gets_nothing_more() {
    let deck = Deck::new();
    let codex = deck.codex(
        "codex-027-head",
        MEASURED,
        Some("codex"),
        &[(".dialog_on_head", "")],
    );
    deck.latched(&codex, MEASURED);
    deck.idle(&codex);
    let id = enqueue(&deck.daemon, &codex.pane, BODY);
    assert!(
        common::wait_until(RING_WAIT, || !deck
            .lines(&codex.pane.agent, "doorbell attempt ended")
            .is_empty()),
        "the attempt ends: {}",
        deck.log()
    );
    let ended = deck.lines(&codex.pane.agent, "doorbell attempt ended");
    assert!(
        ended[0].contains("DialogAfterTyping"),
        "the dialog is the cause: {ended:?}"
    );
    unchanged(&codex.pane, Duration::from_millis(1500));
    let typed = codex.typed();
    assert_eq!(typed.len(), 8, "the head alone: {typed:?}");
    assert!(typed.starts_with('(') && typed.ends_with(')'), "{typed:?}");
    let inputs = codex.inputs();
    assert!(
        inputs[0].0 >= deck.anchor(&codex) + MEASURED_SETTLE.as_nanos() as u64,
        "no byte before the measured settle window"
    );
    assert!(
        codex.dialog_bytes().is_empty(),
        "{:?}",
        codex.dialog_bytes()
    );
    state(&deck.daemon, &codex.pane, &id, "queued");
    assert!(
        common::wait_until(WAIT, || deck.record(&codex)["inbox"]["wake_hold"]
            == "dialog_during_ring"),
        "{}",
        deck.record(&codex)
    );
}

/// Scenario: Type a doorbell-shaped line naming another message into measured Codex 0.164.0's composer, as a user would, and queue a task: the deck adds no byte (no line, no Enter, no delete) through its settle window and well past it. Then deliver a task, type that ring's own old line back into the composer and queue another: again nothing is added. Clearing the composer lets the doorbell ring and deliver.
#[spec("inbox/codex/027")]
#[test]
fn codex_027_a_doorbell_shaped_line_the_deck_did_not_type_gets_no_byte() {
    let deck = Deck::new();
    let codex = deck.codex("codex-027-foreign", MEASURED, Some("codex"), &[]);
    deck.latched(&codex, MEASURED);
    deck.idle(&codex);
    let mut user = common::AttachInput::attach(&deck.daemon.attach_socket, &codex.pane.agent);
    let foreign = format!("(zzzzzz){}", fixed_line("m-crafted0-0000000000000000"));
    user.write(&foreign);
    assert!(common::wait_until(WAIT, || codex.typed() == foreign));
    let id = enqueue(&deck.daemon, &codex.pane, BODY);
    unchanged(&codex.pane, MEASURED_SETTLE + Duration::from_secs(3));
    assert_eq!(
        codex.typed(),
        foreign,
        "nothing added to a line it did not type"
    );
    state(&deck.daemon, &codex.pane, &id, "queued");

    user.write("\x15");
    let line = delivered(&deck, &codex, &id);
    assert_eq!(codex.typed(), format!("{foreign}\x15{line}\r"));

    // The deck's own earlier line, typed back by the user.
    let before = codex.typed();
    user.write(&line);
    assert!(common::wait_until(WAIT, || codex.typed().len() == before.len() + line.len()));
    let second = enqueue(&deck.daemon, &codex.pane, "SECOND-TASK-027");
    unchanged(&codex.pane, Duration::from_secs(4));
    assert_eq!(codex.typed(), format!("{before}{line}"));
    state(&deck.daemon, &codex.pane, &second, "queued");
    user.write("\x15");
    let again = delivered(&deck, &codex, &second);
    assert_ne!(head(&again), head(&line), "a fresh token");
}

/// Scenario: Measured Codex 0.164.0 keeps its draft on Enter (its paste-burst Enter). The user types a draft and presses Enter, the draft stays in the composer, and a task is queued: the deck adds no byte, since pressing Enter does not prove the composer emptied. Once the user clears the composer, the doorbell rings and the task is delivered.
#[spec("inbox/codex/027")]
#[test]
fn codex_027_a_draft_kept_on_enter_holds_the_doorbell() {
    let deck = Deck::new();
    let codex = deck.codex("codex-027-keep", MEASURED, Some("codex"), &[(".keep", "")]);
    deck.latched(&codex, MEASURED);
    deck.idle(&codex);
    let mut user = common::AttachInput::attach(&deck.daemon.attach_socket, &codex.pane.agent);
    user.write(&format!("{DRAFT}\r"));
    assert!(common::wait_until(WAIT, || codex.typed() == format!("{DRAFT}\r")));
    let id = enqueue(&deck.daemon, &codex.pane, BODY);
    unchanged(&codex.pane, MEASURED_SETTLE + Duration::from_secs(3));
    assert_eq!(codex.typed(), format!("{DRAFT}\r"));
    state(&deck.daemon, &codex.pane, &id, "queued");

    codex.clear(".keep");
    user.write("\x15");
    let line = delivered(&deck, &codex, &id);
    assert_eq!(codex.typed(), format!("{DRAFT}\r\x15{line}\r"));
}

/// Scenario: Start the scripted Codex screen with no agent type, under a command the deck does not recognise, drawing measured release 0.164.0's header; its own hooks then report it as Codex and it redraws. The deck takes the type from its hooks and the release from its header, and rings it: an absent spawn type is no reason to hold it.
#[spec("inbox/codex/027")]
#[test]
fn codex_027_a_pane_its_hooks_call_codex_is_rung() {
    let deck = Deck::new();
    let codex = deck.codex("codex-027-untyped", MEASURED, None, &[]);
    deck.idle(&codex);
    codex.set(".redraw", "");
    let id = enqueue(&deck.daemon, &codex.pane, BODY);
    let line = delivered(&deck, &codex, &id);
    assert_eq!(codex.typed(), format!("{line}\r"));
    assert_eq!(head(&codex.inputs()[0].1), head(&line));
}

/// Scenario: Codex 0.165.0, a release the deck has not measured, inherits measured 0.164.0's profile. Its update dialog is open when, after the inherited settle window of 10 s, the deck's admission reads it: nothing is typed and the release's tripwire is set, so a new generation of 0.165.0 in the same pane is held as release_tripwire, while a measured 0.164.0 generation in another pane is still rung.
#[spec("inbox/codex/027")]
#[test]
fn codex_027_a_dialog_on_an_inherited_release_sets_its_tripwire() {
    let deck = Deck::new();
    let codex = deck.codex("codex-027-inherit", INHERITED, Some("codex"), &[]);
    deck.latched(&codex, INHERITED);
    codex.set(".dialog", "update");
    deck.idle(&codex);
    let id = enqueue(&deck.daemon, &codex.pane, BODY);
    assert!(
        common::wait_until(RING_WAIT, || deck.tripped()),
        "the tripwire is set: {}",
        deck.log()
    );
    let refused = deck.lines(&codex.pane.agent, "Dialog(\"update\")");
    assert!(!refused.is_empty(), "{}", deck.log());
    assert!(bytes(&codex.pane).is_empty(), "{:?}", codex.typed());
    assert!(codex.dialog_bytes().is_empty());
    state(&deck.daemon, &codex.pane, &id, "queued");
    let first = refused[0].clone();
    assert!(
        deck.log().find(&first).unwrap() < deck.log().find("pane release: tripwire set").unwrap(),
        "the refusal comes first"
    );

    deck.stop(codex);
    let next = deck.codex("codex-027-inherit", INHERITED, Some("codex"), &[]);
    deck.latched(&next, INHERITED);
    assert!(
        common::wait_until(WAIT, || deck
            .lines(&next.pane.agent, "doorbell off")
            .iter()
            .any(|l| l.contains("release_tripwire"))),
        "the next generation of 0.165.0 is held: {}",
        deck.log()
    );

    let measured = deck.codex("codex-027-measured", MEASURED, Some("codex"), &[]);
    deck.latched(&measured, MEASURED);
    deck.idle(&measured);
    let id = enqueue(&deck.daemon, &measured.pane, BODY);
    delivered(&deck, &measured, &id);
}

/// Scenario: On inherited Codex 0.165.0 the folder-trust dialog opens the moment the deck's token head lands. The deck types nothing more, the attempt ends unresolved with the dialog named, and the release's tripwire is set.
#[spec("inbox/codex/027")]
#[test]
fn codex_027_a_dialog_mid_attempt_on_an_inherited_release_sets_its_tripwire() {
    let deck = Deck::new();
    let codex = deck.codex(
        "codex-027-inherit-head",
        INHERITED,
        Some("codex"),
        &[(".dialog_on_head", "")],
    );
    deck.latched(&codex, INHERITED);
    deck.idle(&codex);
    enqueue(&deck.daemon, &codex.pane, BODY);
    assert!(
        common::wait_until(RING_WAIT, || deck.tripped()),
        "the tripwire is set: {}",
        deck.log()
    );
    let ended = deck.lines(&codex.pane.agent, "doorbell attempt ended");
    assert!(
        ended.iter().any(|l| l.contains("DialogAfterTyping")),
        "{ended:?}"
    );
    assert_eq!(codex.typed().len(), 8, "{:?}", codex.typed());
    let inputs = codex.inputs();
    assert!(
        inputs[0].0 >= deck.anchor(&codex) + INHERITED_SETTLE.as_nanos() as u64,
        "no byte before the inherited settle window"
    );
}

/// Scenario: On measured Codex 0.164.0, an unrecognised screen gets no byte, and a later swallowed Enter leaves the task queued with no submit evidence. A fresh generation with a transcript row reading "Update now" still delivers its task because that row is not a dialog.
#[spec("inbox/codex/027")]
#[test]
fn codex_027_unrecognised_screen_and_missing_submit_evidence_stay_safe() {
    let deck = Deck::new();
    let codex = deck.codex(
        "codex-027-measured-generic",
        MEASURED,
        Some("codex"),
        &[(".hidden", "")],
    );
    deck.latched(&codex, MEASURED);
    deck.idle(&codex);
    let id = enqueue(&deck.daemon, &codex.pane, BODY);
    unchanged(&codex.pane, MEASURED_SETTLE + Duration::from_secs(2));
    assert!(bytes(&codex.pane).is_empty(), "{:?}", codex.typed());

    codex.set(".swallow", "");
    codex.clear(".hidden");
    assert!(
        common::wait_until(RING_WAIT, || deck
            .lines(&codex.pane.agent, "doorbell attempt ended")
            .iter()
            .any(|l| l.contains("NoSubmitEvidence"))),
        "the Enter gets no submit evidence: {}",
        deck.log()
    );
    state(&deck.daemon, &codex.pane, &id, "queued");

    // A fresh generation, a transcript row reading "Update now".
    deck.stop(codex);
    let next = deck.codex(
        "codex-027-measured-generic",
        MEASURED,
        Some("codex"),
        &[(".extra", "Update now")],
    );
    deck.latched(&next, MEASURED);
    deck.idle(&next);
    let id = enqueue(&deck.daemon, &next.pane, "AFTER-UPDATE-NOW-027");
    delivered(&deck, &next, &id);
}

// ---------------------------------------------------------------------------
// Q28 (`inbox/codex/028`): a Codex rung before its first prompt. No case here
// runs a hook by hand before the ring: a real Codex's first hook comes with
// its first prompt, which is what the doorbell types.
// ---------------------------------------------------------------------------

impl Codex {
    /// Whether any hook of the generation's own has run yet.
    fn hooked(&self) -> bool {
        self.base.with_extension("hooks").exists()
    }

    /// The hook events the generation ran, in order.
    fn hook_events(&self) -> Vec<String> {
        std::fs::read_to_string(self.base.with_extension("hooks"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .map(|v| v["event"].as_str().unwrap_or_default().to_string())
            .collect()
    }
}

/// No byte reaches `codex` for `long`, and `id` is still queued.
fn held(deck: &Deck, codex: &Codex, id: &str, long: Duration, why: &str) {
    unchanged(&codex.pane, long);
    assert!(
        bytes(&codex.pane).is_empty(),
        "{why}: {:?}\n{}",
        codex.typed(),
        deck.log()
    );
    state(&deck.daemon, &codex.pane, id, "queued");
}

/// The card's hold for `codex` becomes `hold`.
fn holds(deck: &Deck, codex: &Codex, hold: &str) {
    assert!(
        common::wait_until(WAIT, || deck.record(codex)["inbox"]["wake_hold"] == hold),
        "the card holds as {hold}: {}",
        deck.record(codex)
    );
}

/// Scenario: Start the scripted Codex screen drawing measured release 0.164.0 behind the real wrapper and queue a task, running no hook by hand: like a real Codex worker the deck has just started, it reports nothing until its first prompt. After its settle window the deck types the token head, the rest of the line and one Enter on the generation's own proof, assuming no hook route; the Enter brings the generation's first own hook, whose prompt hook hands the task over, and it is acknowledged.
#[spec("inbox/codex/028")]
#[test]
fn codex_028_a_fresh_codex_is_rung_before_its_first_prompt() {
    let deck = Deck::new();
    let codex = deck.codex("codex-028-fresh", MEASURED, Some("codex"), &[]);
    deck.latched(&codex, MEASURED);
    let id = enqueue(&deck.daemon, &codex.pane, BODY);
    let line = delivered(&deck, &codex, &id);
    assert_eq!(codex.typed(), format!("{line}\r"));
    let inputs = codex.inputs();
    assert_eq!(inputs[0].1, head(&line), "{inputs:?}");
    assert!(
        inputs[0].0 >= deck.anchor(&codex) + MEASURED_SETTLE.as_nanos() as u64,
        "no byte before the settle window"
    );
    let attempts = deck.lines(&codex.pane.agent, "doorbell attempt");
    assert!(
        attempts
            .iter()
            .any(|l| l.contains("hook_route=false") && l.contains("staged=true")),
        "rung with no hook route assumed: {attempts:?}"
    );
    assert_eq!(
        codex.hook_events().first().map(String::as_str),
        Some("UserPromptSubmit"),
        "its first own hook is the one the ring's Enter brought"
    );
}

/// Scenario: A measured Codex 0.164.0 generation reports its own SessionStart and Stop, holds a task of its own behind a screen that is not its composer, and is stopped. A replacement starts in the same pane, also showing no composer yet, and a shell-idle report carrying the predecessor's session arrives for it; a task is queued for it. Nothing reaches the replacement through its settle window and beyond: neither the predecessor's Idle nor that report is its readiness. Once its own composer is drawn it is rung on its own proof, with no hook of its own, and only its own task is delivered.
#[spec("inbox/codex/028")]
#[test]
fn codex_028_a_predecessors_idle_never_readies_its_replacement() {
    let deck = Deck::new();
    let old = deck.codex(
        "codex-028-respawn",
        MEASURED,
        Some("codex"),
        &[(".hidden", "")],
    );
    deck.latched(&old, MEASURED);
    deck.idle(&old);
    let old_id = enqueue(&deck.daemon, &old.pane, "PREDECESSOR-TASK-028");
    held(&deck, &old, &old_id, Duration::from_secs(2), "no composer");
    let old_session = format!("{}-session", old.pane.agent);
    deck.stop(old);

    let new = deck.codex(
        "codex-028-respawn",
        MEASURED,
        Some("codex"),
        &[(".hidden", "")],
    );
    deck.latched(&new, MEASURED);
    let event = json!({"session_id":old_session,
        "pane_id":new.pane.id,"agent_id":new.pane.agent,"token":new.pane.token,
        "agent_type":"none","event_type":"shell_idle",
        "timestamp":chrono::Utc::now(),"metadata":{}});
    let mut socket = connect(&deck.daemon, &event).into_inner();
    socket.shutdown(std::net::Shutdown::Write).unwrap();
    std::io::Read::read_to_end(&mut socket, &mut Vec::new()).unwrap();
    let id = enqueue(&deck.daemon, &new.pane, BODY);
    held(
        &deck,
        &new,
        &id,
        MEASURED_SETTLE + Duration::from_secs(3),
        "the replacement has shown no composer of its own",
    );
    assert!(!new.hooked(), "no hook of its own ran");

    new.clear(".hidden");
    let line = delivered(&deck, &new, &id);
    assert_eq!(new.typed(), format!("{line}\r"));
    assert!(
        !new.typed().contains(&old_id),
        "the predecessor's task rang"
    );
}

/// Scenario: A fresh measured Codex 0.164.0 that has run no hook draws its header and composer, then raises one of its post-ready dialogs (update, folder trust, hooks review) before a task is queued. After its settle window the deck types nothing, neither into the dialog nor anywhere else, and logs the dialog as why. Once the dialog is answered and closes, with no prompt typed, the empty composer is rung and the task delivered.
#[spec("inbox/codex/028")]
#[test]
fn codex_028_a_fresh_codex_behind_a_dialog_gets_no_byte_until_it_closes() {
    for dialog in ["update", "trust", "hooks"] {
        let deck = Deck::new();
        let codex = deck.codex(&format!("codex-028-{dialog}"), MEASURED, Some("codex"), &[]);
        deck.latched(&codex, MEASURED);
        // Codex raises these after its composer is drawn (M2).
        codex.set(".dialog", dialog);
        let title = match dialog {
            "update" => "Update available!",
            "trust" => "Folder access",
            _ => "Hooks need review",
        };
        assert!(
            common::wait_until(WAIT, || screen(&deck.daemon, &codex.pane).contains(title)),
            "{dialog}: the dialog is up"
        );
        let id = enqueue(&deck.daemon, &codex.pane, BODY);
        let why = format!("Dialog(\"{dialog}\")");
        assert!(
            common::wait_until(RING_WAIT, || !deck
                .lines(&codex.pane.agent, &why)
                .is_empty()),
            "{dialog}: admission refuses the dialog: {}",
            deck.log()
        );
        held(&deck, &codex, &id, Duration::from_millis(1500), dialog);
        assert!(codex.dialog_bytes().is_empty(), "{dialog}");
        assert!(!deck.tripped(), "{dialog}");

        codex.clear(".dialog");
        let line = delivered(&deck, &codex, &id);
        assert_eq!(codex.typed(), format!("{line}\r"), "{dialog}");
    }
}

/// Scenario: A Codex screen that draws no start-up header, as a resumed session's screen does not, shows its empty composer behind the real wrapper, with an earlier prompt in its transcript, and a task is queued. With no release read from its own header, the deck types nothing, logs `unknown_release` as why, and the card shows that it cannot type.
#[spec("inbox/codex/028")]
#[test]
fn codex_028_a_codex_without_its_header_gets_no_byte() {
    let deck = Deck::new();
    let codex = deck.codex(
        "codex-028-resume",
        MEASURED,
        Some("codex"),
        &[(".noheader", ""), (".extra", "an earlier prompt")],
    );
    let id = enqueue(&deck.daemon, &codex.pane, BODY);
    assert!(
        common::wait_until(WAIT, || deck
            .lines(&codex.pane.agent, "doorbell off")
            .iter()
            .any(|l| l.contains("unknown_release"))),
        "held as unknown_release: {}",
        deck.log()
    );
    held(
        &deck,
        &codex,
        &id,
        MEASURED_SETTLE + Duration::from_secs(3),
        "no header",
    );
    holds(&deck, &codex, "cannot_type");
    assert!(deck.lines(&codex.pane.agent, "pane release").is_empty());
}

/// Scenario: A launcher prints its own line in the pane and then runs the scripted Codex screen, as `devbox run` does: the fresh generation is still rung on its own measured proof once that screen is up. A second pane's launcher prints a line laid out like Codex 0.164.0's header, so the deck reads a release off it, puts its terminal in raw mode and then only records what it is sent, drawing no composer: it gets no byte, and its card says it waits for a prompt.
#[spec("inbox/codex/028")]
#[test]
fn codex_028_launcher_output_is_no_readiness() {
    let deck = Deck::new();
    let codex = deck.codex_with(
        "codex-028-launcher",
        MEASURED,
        Some("codex"),
        &[],
        (40, 140),
        "printf 'Info: Running script \"codex-big\" on /work\\r\\n'; ",
    );
    deck.latched(&codex, MEASURED);
    let id = enqueue(&deck.daemon, &codex.pane, BODY);
    let line = delivered(&deck, &codex, &id);
    assert_eq!(codex.typed(), format!("{line}\r"));

    let base = deck.daemon.home.join("codex-028-launcher-only");
    let capture = base.with_extension("bytes");
    let command = common::capability_export_command(&format!(
        "stty raw -echo; printf '\\r\\n     OpenAI Codex (v{MEASURED})\\r\\n'; exec cat > '{}'",
        capture.display()
    ));
    let reply = attach(
        &deck.daemon,
        json!({"op":"start-agent","command":command,"agent_type":"codex",
        "cwd":deck.daemon.home,"rows":40,"cols":140,
        "env":[["DOT_AGENT_DECK_PANE_ID","codex-028-launcher-only"],
            ["CODEX_HOME",deck.daemon.home.join(".codex")],["PATH",path()]],
        "display_name":"codex-028-launcher-only"}),
    );
    assert_eq!(reply["ok"], true, "{reply}");
    let agent = reply["id"].as_str().unwrap().to_string();
    let token = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(common::recorded_hook_capability(&deck.daemon.home, &agent))
        .expect("the pane's capability");
    let launcher = Codex {
        pane: Pane {
            id: "codex-028-launcher-only".into(),
            agent,
            token,
            pid: 0,
            capture,
            attach_socket: deck.daemon.attach_socket.clone(),
        },
        base,
    };
    deck.latched(&launcher, MEASURED);
    let id = enqueue(&deck.daemon, &launcher.pane, BODY);
    held(
        &deck,
        &launcher,
        &id,
        MEASURED_SETTLE + Duration::from_secs(3),
        "a header with no composer",
    );
    holds(&deck, &launcher, "awaiting_prompt");
}

/// Scenario: A fresh measured Codex 0.164.0 starts at 24 × 80, where Codex draws no logo before its first prompt, so its start-up frame carries no width witness and the deck cannot read it as Clean. A queued task gets no byte through the settle window and beyond, and the card shows the hold, that the agent may be waiting at a prompt, rather than the deck typing into a frame it could not prove.
#[spec("inbox/codex/028")]
#[test]
fn codex_028_a_small_fresh_codex_without_a_width_witness_is_held_visibly() {
    let deck = Deck::new();
    let codex = deck.codex_with(
        "codex-028-small",
        MEASURED,
        Some("codex"),
        &[],
        (24, 80),
        "",
    );
    deck.latched(&codex, MEASURED);
    let id = enqueue(&deck.daemon, &codex.pane, BODY);
    held(
        &deck,
        &codex,
        &id,
        MEASURED_SETTLE + Duration::from_secs(4),
        "no width witness",
    );
    holds(&deck, &codex, "awaiting_prompt");
}
