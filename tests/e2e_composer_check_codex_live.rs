#![cfg(all(feature = "e2e", feature = "e2e-live", target_os = "linux"))]

//! The deck checking a real Codex input box, as a person typing into it
//! meets it (PRD #1666 part 3, the Codex activation's lane-2 gates
//! `inbox/probe/041` and `043`, the Codex counterparts of `040` and `042`).
//!
//! Each test runs real interactive Codex in an installed `Direct(Codex)`
//! pane (`support/codex_installed.rs`): the production doorbell tick decides
//! when to check, writes the check's byte, and Codex runs the deck's helper
//! as its editor. They run on the build's own Codex activation, with no e2e
//! assumption, each in a fresh `CODEX_HOME` (Codex's first start there).
//!
//! **Codex is checked only over its placeholder, by design**: admission's
//! surface conjunct for Codex is `placeholder_empty`, so a box showing text
//! is refused and never checked. So `041`, unlike Claude's `040` (a check
//! over a visible `BASE`), starts each trial from a box the person typed
//! into and emptied, which Codex draws with its placeholder, and `043`
//! holds a visible draft with nothing typed, then checks the emptied box.
//!
//! `041` needs no model turn (nothing is submitted); `043` delivers tasks.
#[path = "support/codex_installed.rs"]
mod codex_installed;
mod common;
#[path = "support/helper_pause.rs"]
mod helper_pause;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/claude_installed.rs"]
mod installed;
#[path = "support/codex_inbox_live.rs"]
mod live;

use codex_installed::{BUDGET, CprAnswerer, Installed};
use helper_pause::{Pauser, helpers};
use serde_json::json;
use spec::spec;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

/// The user's editor: a terminal program that reads what the person types
/// into it (`read`, until `LATE` or `T041_EDITOR_WAIT` seconds) and appends
/// the printable bytes to the file, or empties the file (`empty`). Each run
/// appends one JSON line to `T041_EDITOR_LOG`.
const EDITOR: &str = r#"#!/usr/bin/env python3
import json, os, select, sys, termios, time, tty
path = sys.argv[-1]
with open(path, "rb") as f:
    initial = f.read()
got = b""
mode = os.environ.get("T041_EDITOR_MODE", "read")
if mode == "read":
    fd = os.open("/dev/tty", os.O_RDWR)
    old = termios.tcgetattr(fd)
    try:
        # TCSANOW: TCSAFLUSH, setraw's default, would discard what the
        # person typed before this line ran.
        tty.setraw(fd, termios.TCSANOW)
        deadline = time.monotonic() + float(os.environ.get("T041_EDITOR_WAIT", "3"))
        while b"LATE" not in got:
            left = deadline - time.monotonic()
            if left <= 0:
                break
            ready, _, _ = select.select([fd], [], [], left)
            if ready:
                chunk = os.read(fd, 64)
                if not chunk:
                    break
                got += chunk
    finally:
        termios.tcsetattr(fd, termios.TCSADRAIN, old)
        os.close(fd)
    body = initial[:-1] if initial.endswith(b"\n") else initial
    text = body + bytes(b for b in got if 32 <= b < 127)
else:
    text = b""
with open(path, "wb") as f:
    f.write(text)
with open(os.environ["T041_EDITOR_LOG"], "a") as f:
    f.write(json.dumps({"mode": mode, "initial": initial.decode(errors="replace"),
                        "read_hex": got.hex(), "final": text.decode(errors="replace"),
                        "pid": os.getpid(), "ppid": os.getppid()}) + "\n")
"#;

/// The user's editor and its log, in a directory of the test's own.
struct UserEditor {
    dir: tempfile::TempDir,
}

impl UserEditor {
    fn new() -> Self {
        let dir = common::harness_tempdir().unwrap();
        let path = dir.path().join("t041-editor");
        std::fs::write(&path, EDITOR).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self { dir }
    }

    fn path(&self) -> String {
        self.dir.path().join("t041-editor").display().to_string()
    }

    fn log_path(&self) -> String {
        self.dir.path().join("editor.jsonl").display().to_string()
    }

    fn runs(&self) -> Vec<serde_json::Value> {
        std::fs::read_to_string(self.log_path())
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// The deck's and the editor's variables: the user's `VISUAL` and
    /// `EDITOR` are this editor, as the deck's spawn sees them.
    fn env(&self, mode: &str) -> Vec<(String, String)> {
        vec![
            ("VISUAL".into(), self.path()),
            ("EDITOR".into(), self.path()),
            ("T041_EDITOR_LOG".into(), self.log_path()),
            ("T041_EDITOR_MODE".into(), mode.into()),
        ]
    }
}

// ---------------------------------------------------------------------------
// inbox/probe/041
// ---------------------------------------------------------------------------

/// The number of trials per path: `DAD_PROBE_041_TRIALS`, else 20.
fn trials() -> usize {
    std::env::var("DAD_PROBE_041_TRIALS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(20)
}

/// M44c's path, as the production helper meets it.
#[derive(Clone, Copy)]
struct Path041 {
    name: &'static str,
    point: &'static str,
    hold: Duration,
    kill: bool,
    exit: Exit041,
}

/// How the held check's helper ends on a path: what defines the path.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Exit041 {
    /// It reports the box it read non-empty, with no notice.
    Mine,
    /// Its report fails and it prints its notice.
    Notice,
    /// It is killed, so it reports nothing and leaves no exit.
    Killed,
}

/// `hex`'s bytes (the editor log's `read_hex`); `None` if it isn't hex.
fn unhex(hex: &str) -> Option<Vec<u8>> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Each trial in a fresh installed pane: type `BASE` and delete it, so the
/// box is touched and Codex draws its placeholder again, queue a task so the
/// deck checks the box, and while that check's helper is held at its pause
/// type `EARLY`, Ctrl+G and `LATE` 250 ms apart. Every trial must end with
/// the composer holding `EARLYLATE` and the task undelivered: the check read
/// a box the person has typed into since, so it rings nothing.
///
/// One trial per pane: the person's own editor, which their Ctrl+G opens,
/// leaves the pane's writer scope open under the production `Hold` policy,
/// and no further check is admitted in that generation.
fn overlap_trials(path: Path041) {
    skip_unless!(codex_installed::checkable());
    let editor = UserEditor::new();
    let mut rows = Vec::new();
    let mut exact = 0;
    let mut last_evidence = String::new();
    for trial in 0..trials() {
        let mut pauser = Pauser::new();
        let mut env = editor.env("read");
        env.extend(pauser.env());
        let env_ref: Vec<(&str, &str)> =
            env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let codex = Installed::new(&env_ref);
        pauser.start(codex.deck.home_dir());
        codex.assert_installed();
        codex.wait_armable();
        let mut person = codex.input();
        person.write("BASE");
        codex.expect_composer("BASE");
        person.write("\x7f\x7f\x7f\x7f");
        codex.expect_composer("");
        settle(&codex, &pauser);
        let trace_from = pauser.trace().len();
        let editor_runs = editor.runs().len();
        let log_from = codex.log().len();
        pauser.arm(path.point, path.hold, path.kill);
        let name = format!("overlap-{}-{trial}-041.txt", path.name);
        let id = codex.post(&name);
        assert!(
            common::wait_until(Duration::from_secs(60), || pauser.hit().is_some()),
            "trial {trial}: the check's helper must reach its {} pause; checks={:?}; {}",
            path.point,
            codex.checks(log_from),
            codex.evidence()
        );
        let t0 = pauser.hit().unwrap();
        for (ms, keys) in [(250, "EARLY"), (500, "\x07"), (750, "LATE")] {
            helper_pause::until(t0 + Duration::from_millis(ms));
            person.write(keys);
        }
        assert!(
            common::wait_until(Duration::from_secs(60), || pauser.done()),
            "trial {trial}: the held helper must be let go"
        );
        let landed = common::wait_until(Duration::from_secs(30), || {
            codex.composer().as_deref() == Some("EARLYLATE")
        });
        let killed = pauser.disarm();
        let trace = pauser.trace()[trace_from..].to_string();
        let branches: Vec<&str> = trace
            .lines()
            .filter_map(|l| l.split_once(" branch ").map(|(_, b)| b))
            .map(|b| b.split(" class ").next().unwrap_or(b))
            .collect();
        let armed: Vec<&str> = trace
            .lines()
            .filter_map(|l| l.split_once(" armed ").map(|(_, a)| a))
            .collect();
        let runs = editor.runs()[editor_runs..].to_vec();
        let message = codex.message(&id);
        let row = json!({
            "trial": trial, "exact": landed, "composer": codex.composer(),
            "branches": branches, "armed_exit": armed, "killed_pid": killed,
            "editor_runs": runs, "checks": codex.checks(log_from),
            "deliveries": message["deliveries"],
        });
        eprintln!("041 {} {row}", path.name);
        assert_eq!(
            branches.first().copied(),
            Some("Armed"),
            "trial {trial}: the held helper must be the deck's armed check; {row}"
        );
        // Bounded: a ring the stale empty read let through gets three more
        // seconds to show.
        common::wait_until(Duration::from_secs(3), || {
            assert_eq!(
                codex.message(&id)["deliveries"],
                0,
                "trial {trial}: the task waits behind the box the person typed into; {row}; {}",
                codex.evidence()
            );
            false
        });
        assert_eq!(
            codex.composer().as_deref(),
            Some("EARLYLATE"),
            "trial {trial}: nothing but the person's typing reached the box; {row}"
        );
        // The route, not only the box: Ctrl+G opened the person's editor once,
        // on the box as typed before it, and LATE reached that editor.
        assert_eq!(
            runs.len(),
            1,
            "trial {trial}: Ctrl+G opens the person's editor exactly once; {row}; {}",
            codex.evidence()
        );
        assert_eq!(
            runs[0]["initial"].as_str().map(str::trim_end),
            Some("EARLY"),
            "trial {trial}: the editor opens on the box as typed before Ctrl+G; {row}"
        );
        assert_eq!(
            runs[0]["read_hex"].as_str().and_then(unhex).as_deref(),
            Some(&b"LATE"[..]),
            "trial {trial}: LATE reaches the person's editor; {row}"
        );
        match path.exit {
            // The box was empty when the check's byte reached Codex: what
            // the person typed during the check was held behind it.
            Exit041::Mine => assert!(
                killed.is_none()
                    && armed.len() == 1
                    && armed[0].contains("Mine { read: Empty }")
                    && armed[0].contains("notice: false"),
                "trial {trial}: the held helper reports the box it read empty, with no notice; {row}"
            ),
            Exit041::Notice => assert!(
                killed.is_none() && armed.len() == 1 && armed[0].contains("notice: true"),
                "trial {trial}: the held helper's report fails and it prints its notice; {row}"
            ),
            Exit041::Killed => assert!(
                killed.is_some() && armed.is_empty(),
                "trial {trial}: the held helper is killed and leaves no exit; {row}"
            ),
        }
        if landed {
            exact += 1;
        }
        rows.push(row);
        if !landed {
            last_evidence = format!("{}\nhelper trace:\n{}", codex.evidence(), pauser.trace());
            break;
        }
    }
    let summary = json!({
        "path": path.name, "point": path.point, "hold_ms": path.hold.as_millis() as u64,
        "kill": path.kill, "trials": rows.len(), "exact": exact,
        "release": codex_installed::installed_release().ok(),
    });
    eprintln!("041 summary {summary}");
    if let Ok(out) = std::env::var("DAD_PROBE_041_OUT") {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(out)
            .unwrap();
        writeln!(f, "{}", json!({"summary": summary, "rows": rows})).unwrap();
    }
    assert_eq!(
        exact,
        trials(),
        "every trial must end with EARLYLATE: {summary}; last row {}; {last_evidence}",
        rows.last().unwrap(),
    );
}

/// No helper is alive and no pause has arrived for a second and a half.
fn settle(codex: &Installed, pauser: &Pauser) {
    let quiet = std::cell::Cell::new((Pauser::pauses(&pauser.trace()).len(), Instant::now()));
    assert!(
        common::wait_until(Duration::from_secs(60), || {
            let n = Pauser::pauses(&pauser.trace()).len();
            if n != quiet.get().0 || !helpers(codex.deck.home_dir()).is_empty() {
                quiet.set((n, Instant::now()));
            }
            quiet.get().1.elapsed() >= Duration::from_millis(1500)
        }),
        "the pane must settle between trials; {}",
        codex.evidence()
    );
}

/// Scenario: Type BASE into real interactive Codex in an installed pane and delete it, so Codex shows its placeholder over a touched box, and queue a task so the deck checks the box; for each trial hold the check's own helper for one second just before its report to the deck, typing EARLY, Ctrl+G and LATE 250 ms apart while it is held. Every trial must end with exactly EARLYLATE in the input box and the task undelivered: the typing waits for the check, the helper reports the box it read empty, Ctrl+G then opens the person's own editor once, on EARLY, and LATE reaches it.
#[spec("inbox/probe/041")]
#[test]
fn probe_041_live_delayed_report_keeps_overlapping_typing() {
    overlap_trials(Path041 {
        name: "delayed-ipc",
        point: "ready",
        hold: Duration::from_millis(1000),
        kill: false,
        exit: Exit041::Mine,
    });
}

/// Scenario: As the delayed case, but the check's helper is held for 2.5 seconds before its report, past its exchange's two-second bound, so the helper gives up on the deck and prints its notice. Every trial must still end with exactly EARLYLATE in Codex's input box and the task undelivered, the person's editor having opened once and read LATE.
#[spec("inbox/probe/041")]
#[test]
fn probe_041_live_failed_report_with_notice_keeps_overlapping_typing() {
    overlap_trials(Path041 {
        name: "transport-notice",
        point: "ready",
        hold: Duration::from_millis(2500),
        kill: false,
        exit: Exit041::Notice,
    });
}

/// Scenario: As the delayed case, but the check's helper is held for 11 seconds at its very start, before it reads anything, past the ten-second wait after which the deck counts the check's byte as unanswered. Every trial must still end with exactly EARLYLATE in Codex's input box and the task undelivered, the person's editor having opened once and read LATE, and the helper still reports the box it read empty.
#[spec("inbox/probe/041")]
#[test]
fn probe_041_live_late_helper_keeps_overlapping_typing() {
    overlap_trials(Path041 {
        name: "late-helper",
        point: "start",
        hold: Duration::from_millis(11_000),
        kill: false,
        exit: Exit041::Mine,
    });
}

/// Scenario: As the delayed case, but one second into the hold the check's helper is killed with SIGKILL by its captured pid, so it never reports. Every trial must still end with exactly EARLYLATE in Codex's input box and the task undelivered, the person's editor having opened once and read LATE.
#[spec("inbox/probe/041")]
#[test]
fn probe_041_live_killed_helper_keeps_overlapping_typing() {
    overlap_trials(Path041 {
        name: "kill",
        point: "ready",
        hold: Duration::from_millis(1000),
        kill: true,
        exit: Exit041::Killed,
    });
}

// ---------------------------------------------------------------------------
// inbox/probe/043
// ---------------------------------------------------------------------------

/// Scenario: In real interactive Codex in an installed pane, with a test-owned client attached that answers Codex's cursor position queries as the desktop app's terminal does (a stand-in, not the desktop), type a draft and queue a sentinel task: while the draft shows, the deck neither checks nor rings, the task stays queued and nothing is typed. The person then empties the box, by Backspace in one round and by Ctrl+U in the next, so Codex draws its placeholder again over a touched box: the deck's check reads it empty, the doorbell rings, and that round's task is acted on and acknowledged after exactly one hand-over.
#[spec("inbox/probe/043")]
#[test]
fn probe_043_live_a_touched_box_with_a_client_attached_is_checked_and_delivered() {
    skip_unless!(codex_installed::checkable());
    let trace_dir = common::harness_tempdir().unwrap();
    let trace = trace_dir.path().join("helper-trace");
    let trace_s = trace.display().to_string();
    let codex = Installed::new(&[("DOT_AGENT_DECK_E2E_HELPER_TRACE", &trace_s)]);
    let helper_trace = || std::fs::read_to_string(&trace).unwrap_or_default();
    codex.assert_installed();
    codex.wait_armable();
    let record = codex.record();
    let answerer = CprAnswerer::attach(
        codex.deck.attach_socket_path(),
        &codex.pane.agent,
        record["rows"].as_u64().unwrap() as u16,
        record["cols"].as_u64().unwrap() as u16,
    );
    let mut person = codex.input();
    for (means, empty_keys) in [("backspace", ""), ("ctrl-u", "\x15")] {
        let draft = format!("p043-{means}-draft");
        person.write(&draft);
        codex.expect_composer(&draft);
        let since = codex.log().len();
        let name = format!("p043-{means}-0431.txt");
        let id = codex.post(&name);
        // While Codex shows the draft, its box is no surface the deck checks.
        held_for(&codex, &id, &name, &draft, since, means);
        assert!(
            codex.ops(since).iter().all(|o| !o.deck),
            "{means}: the deck typed nothing over the draft: {:?}; {}",
            codex.ops(since),
            codex.evidence()
        );
        assert_eq!(
            codex.empty_reads(since),
            0,
            "{means}: no check read the draft empty; {}",
            codex.evidence()
        );
        let emptied_at = codex.log().len();
        if empty_keys.is_empty() {
            person.write(&"\x7f".repeat(draft.len()));
        } else {
            person.write(empty_keys);
        }
        let delivered = common::wait_until(BUDGET, || codex.deck.workdir().join(&name).exists());
        assert!(
            delivered,
            "{means}: the emptied box's check must lead to the task; helper trace:\n{}\n{}",
            helper_trace(),
            codex.evidence()
        );
        codex.done(&id, &name);
        let log = codex.log()[emptied_at..].to_string();
        let at = |needle: &str| log.lines().position(|l| l.contains(needle));
        let (written, applied, rang) = (
            log.lines()
                .position(|l| l.contains("doorbell tick probe") && l.contains("outcome=Written")),
            at("empty read applied"),
            at("doorbell rang"),
        );
        assert!(
            matches!((written, applied, rang), (Some(w), Some(a), Some(r)) if w < a && a < r),
            "{means}: a check of the emptied box, its empty read, then the ring \
             (written={written:?}, applied={applied:?}, rang={rang:?}); {}",
            codex.evidence()
        );
        eprintln!(
            "043 {means}: held over the draft, emptied, checked, one delivery: {}; cpr answers so far {}",
            codex.message(&id),
            answerer.answered()
        );
        codex.idle();
    }
}

/// For three seconds the task stays queued with no delivery, the draft is
/// exactly as typed whenever the box is drawn, no doorbell rang, and the
/// deck wrote nothing to the pane but one-byte checks.
fn held_for(codex: &Installed, id: &str, name: &str, draft: &str, since: usize, means: &str) {
    common::wait_until(Duration::from_secs(3), || {
        let message = codex.message(id);
        assert_eq!(message["state"], "queued", "{means}: {message}");
        assert_eq!(message["deliveries"], 0, "{means}: {message}");
        let composer = codex.composer();
        assert!(
            composer.is_none() || composer.as_deref() == Some(draft),
            "{means}: the draft changed to {composer:?}; {}",
            codex.evidence()
        );
        assert!(!codex.deck.workdir().join(name).exists());
        let log = codex.log();
        assert!(
            !log[since..].contains("doorbell rang"),
            "{means}: no ring over a non-empty box; {}",
            codex.evidence()
        );
        assert!(
            codex
                .ops(since)
                .iter()
                .filter(|o| o.deck)
                .all(|o| o.len == 1),
            "{means}: the deck wrote only check bytes: {:?}; {}",
            codex.ops(since),
            codex.evidence()
        );
        false
    });
    codex.expect_composer(draft);
}
