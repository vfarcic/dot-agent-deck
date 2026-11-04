#![cfg(all(feature = "e2e", feature = "e2e-live", target_os = "linux"))]

//! The deck checking a real Claude Code input box, as a person typing into
//! it meets it (PRD #1666 part 3, Q35-28's lane-2 gates `inbox/probe/040`
//! and `042`).
//!
//! Each test runs real interactive Claude Code in an installed pane
//! (`support/claude_installed.rs`): the production doorbell tick decides
//! when to check, writes the check's byte, and Claude Code runs the deck's
//! helper as its editor. No e2e editor-probe assumption is set; a build that
//! doesn't install `Direct(Claude)` panes, or a Claude Code release whose
//! editor probe isn't verified, reports `SKIP:`.
//!
//! `040` needs no model turn (nothing is submitted); `042` delivers tasks.
mod common;
#[path = "support/helper_pause.rs"]
mod helper_pause;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/claude_installed.rs"]
mod installed;

use helper_pause::{Pauser, helpers};
use installed::{BUDGET, Installed};
use serde_json::json;
use spec::spec;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

/// The user's editor: a terminal program that reads what the person types
/// into it (`read`, until `LATE` or `T042_EDITOR_WAIT` seconds) and appends
/// the printable bytes to the file, or empties the file (`empty`). Each run
/// appends one JSON line to `T042_EDITOR_LOG`.
const EDITOR: &str = r#"#!/usr/bin/env python3
import json, os, select, sys, termios, time, tty
path = sys.argv[-1]
with open(path, "rb") as f:
    initial = f.read()
got = b""
mode = os.environ.get("T042_EDITOR_MODE", "read")
if mode == "read":
    fd = os.open("/dev/tty", os.O_RDWR)
    old = termios.tcgetattr(fd)
    try:
        # TCSANOW: TCSAFLUSH, setraw's default, would discard what the
        # person typed before this line ran.
        tty.setraw(fd, termios.TCSANOW)
        deadline = time.monotonic() + float(os.environ.get("T042_EDITOR_WAIT", "3"))
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
with open(os.environ["T042_EDITOR_LOG"], "a") as f:
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
        let path = dir.path().join("t042-editor");
        std::fs::write(&path, EDITOR).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self { dir }
    }

    fn path(&self) -> String {
        self.dir.path().join("t042-editor").display().to_string()
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
            ("T042_EDITOR_LOG".into(), self.log_path()),
            ("T042_EDITOR_MODE".into(), mode.into()),
        ]
    }
}

// ---------------------------------------------------------------------------
// inbox/probe/040
// ---------------------------------------------------------------------------

/// The number of trials per path: `DAD_PROBE_040_TRIALS`, else 20.
fn trials() -> usize {
    std::env::var("DAD_PROBE_040_TRIALS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(20)
}

/// M44c's path, as the production helper meets it.
#[derive(Clone, Copy)]
struct Path040 {
    name: &'static str,
    point: &'static str,
    hold: Duration,
    kill: bool,
}

/// Each trial in a fresh installed pane: type `BASE`, queue a task so the
/// deck checks the box, and while that check's helper is held at its pause
/// type `EARLY`, Ctrl+G and `LATE` 250 ms apart. Every trial must end with
/// the composer holding `BASEEARLYLATE`.
///
/// One trial per pane: the person's own editor, which their Ctrl+G opens,
/// leaves the pane's writer scope open under the production `Hold` policy,
/// and no further check is admitted in that generation (amendment 6 §1.3).
fn overlap_trials(path: Path040) {
    skip_unless!(installed::checkable());
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
        let claude = Installed::new(&env_ref);
        pauser.start(claude.deck.home_dir());
        claude.assert_installed();
        claude.kill_waiter();
        claude.deck.send_keys(b"BASE");
        claude.expect_composer("BASE");
        settle(&claude, &pauser);
        let trace_from = pauser.trace().len();
        let editor_runs = editor.runs().len();
        let log_from = claude.log().len();
        pauser.arm(path.point, path.hold, path.kill);
        let name = format!("overlap-{}-{trial}-040.txt", path.name);
        let id = claude.post(&claude.directive(&name));
        assert!(
            common::wait_until(Duration::from_secs(60), || pauser.hit().is_some()),
            "trial {trial}: the check's helper must reach its {} pause; checks={:?}; {}",
            path.point,
            claude.checks(log_from),
            claude.evidence()
        );
        let t0 = pauser.hit().unwrap();
        for (ms, keys) in [(250, &b"EARLY"[..]), (500, b"\x07"), (750, b"LATE")] {
            helper_pause::until(t0 + Duration::from_millis(ms));
            claude.deck.send_keys(keys);
        }
        assert!(
            common::wait_until(Duration::from_secs(60), || pauser.done()),
            "trial {trial}: the held helper must be let go"
        );
        let landed = common::wait_until(Duration::from_secs(30), || {
            claude.composer().as_deref() == Some("BASEEARLYLATE")
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
        let message = claude.message(&id);
        let row = json!({
            "trial": trial, "exact": landed, "composer": claude.composer(),
            "branches": branches, "armed_exit": armed, "killed_pid": killed,
            "editor_runs": runs, "checks": claude.checks(log_from),
            "deliveries": message["deliveries"],
        });
        eprintln!("040 {} {row}", path.name);
        assert_eq!(
            branches.first().copied(),
            Some("Armed"),
            "trial {trial}: the held helper must be the deck's armed check; {row}"
        );
        assert_eq!(
            message["deliveries"], 0,
            "trial {trial}: the task waits behind the non-empty box; {row}"
        );
        if landed {
            exact += 1;
        }
        rows.push(row);
        if !landed {
            last_evidence = format!("{}\nhelper trace:\n{}", claude.evidence(), pauser.trace());
            break;
        }
    }
    let summary = json!({
        "path": path.name, "point": path.point, "hold_ms": path.hold.as_millis() as u64,
        "kill": path.kill, "trials": rows.len(), "exact": exact,
        "release": installed::installed_release().ok(),
    });
    eprintln!("040 summary {summary}");
    if let Ok(out) = std::env::var("DAD_PROBE_040_OUT") {
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
        "every trial must end with BASEEARLYLATE: {summary}; last row {}; {last_evidence}",
        rows.last().unwrap(),
    );
}

/// No helper is alive and no pause has arrived for a second and a half.
fn settle(claude: &Installed, pauser: &Pauser) {
    let quiet = std::cell::Cell::new((Pauser::pauses(&pauser.trace()).len(), Instant::now()));
    assert!(
        common::wait_until(Duration::from_secs(60), || {
            let n = Pauser::pauses(&pauser.trace()).len();
            if n != quiet.get().0 || !helpers(claude.deck.home_dir()).is_empty() {
                quiet.set((n, Instant::now()));
            }
            quiet.get().1.elapsed() >= Duration::from_millis(1500)
        }),
        "the pane must settle between trials; {}",
        claude.evidence()
    );
}

/// Scenario: Type BASE into real interactive Claude Code in an installed pane and queue a task so the deck checks the box, then for each trial touch the box and hold the check's own helper for one second just before its report to the deck, typing EARLY, Ctrl+G and LATE 250 ms apart while it is held. Every trial must end with exactly BASEEARLYLATE in the input box: the typing waits for the check, Ctrl+G then opens the person's own editor, and LATE reaches them.
#[spec("inbox/probe/040")]
#[test]
fn probe_040_live_delayed_report_keeps_overlapping_typing() {
    overlap_trials(Path040 {
        name: "delayed-ipc",
        point: "ready",
        hold: Duration::from_millis(1000),
        kill: false,
    });
}

/// Scenario: As the delayed case, but the check's helper is held for 2.5 seconds before its report, past its exchange's two-second bound, so the helper gives up on the deck and prints its notice. Every trial must still end with exactly BASEEARLYLATE in the input box.
#[spec("inbox/probe/040")]
#[test]
fn probe_040_live_failed_report_with_notice_keeps_overlapping_typing() {
    overlap_trials(Path040 {
        name: "transport-notice",
        point: "ready",
        hold: Duration::from_millis(2500),
        kill: false,
    });
}

/// Scenario: As the delayed case, but the check's helper is held for 11 seconds at its very start, before it reads anything, past the ten-second wait after which the deck counts the check's byte as unanswered. Every trial must still end with exactly BASEEARLYLATE in the input box.
#[spec("inbox/probe/040")]
#[test]
fn probe_040_live_late_helper_keeps_overlapping_typing() {
    overlap_trials(Path040 {
        name: "late-helper",
        point: "start",
        hold: Duration::from_millis(11_000),
        kill: false,
    });
}

/// Scenario: As the delayed case, but one second into the hold the check's helper is killed with SIGKILL by its captured pid, so it never reports. Every trial must still end with exactly BASEEARLYLATE in the input box.
#[spec("inbox/probe/040")]
#[test]
fn probe_040_live_killed_helper_keeps_overlapping_typing() {
    overlap_trials(Path040 {
        name: "kill",
        point: "ready",
        hold: Duration::from_millis(1000),
        kill: true,
    });
}

// ---------------------------------------------------------------------------
// inbox/probe/042
// ---------------------------------------------------------------------------

/// Scenario: In real interactive Claude Code in an installed pane, with its native waiter disabled after a doorbell-delivered control task, type a draft and queue a sentinel task; the deck's check reads the box non-empty and holds the task, and after a further touch that leaves the draft a second check holds it again, the draft unchanged, the task undelivered and nothing but check bytes typed. The person then empties the box by Backspace, by End then Ctrl+U, and by Ctrl+C, one round each: the next check reads it empty and that round's task is delivered exactly once by the doorbell. In a last round the person empties it with their own editor through Ctrl+G: the deck then refuses to check that pane again, the card says it cannot check the box, nothing is typed, and the person's next prompt hands the task over once.
#[spec("inbox/probe/042")]
#[test]
fn probe_042_live_a_touched_box_holds_after_checks_and_delivers_once_emptied() {
    skip_unless!(installed::checkable());
    let editor = UserEditor::new();
    let mut env = editor.env("empty");
    let trace = editor.dir.path().join("helper-trace");
    env.push((
        "DOT_AGENT_DECK_E2E_HELPER_TRACE".into(),
        trace.display().to_string(),
    ));
    let env_ref: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let claude = Installed::new(&env_ref);
    let helper_trace = || std::fs::read_to_string(&trace).unwrap_or_default();
    claude.assert_installed();
    claude.doorbell_ready("p042");
    let rounds: [(&str, &[u8]); 4] = [
        ("backspace", &[]),
        ("end-ctrl-u", b"\x05\x15"),
        ("ctrl-c", b"\x03"),
        ("own-editor", b"\x07"),
    ];
    // `DAD_PROBE_042_ROUNDS` (comma-separated means) runs a subset, to
    // diagnose one round on its own.
    let only = std::env::var("DAD_PROBE_042_ROUNDS").ok();
    for (means, empty_keys) in rounds {
        if only
            .as_deref()
            .is_some_and(|o| !o.split(',').any(|m| m == means))
        {
            continue;
        }
        let draft = format!("p042-{means}-draft");
        claude.deck.send_keys(draft.as_bytes());
        claude.expect_composer(&draft);
        let since = claude.log().len();
        let name = format!("p042-{means}-0428.txt");
        let id = claude.post(&claude.directive(&name));
        // First check: the box is read non-empty and the task held.
        assert!(
            common::wait_until(BUDGET, || claude.record()["inbox"]["wake_hold"]
                == "possible_draft"),
            "{means}: a check must hold the task; checks={:?}; {}",
            claude.checks(since),
            claude.evidence()
        );
        held_for(&claude, &id, &name, &draft, since, means);
        // A further touch that leaves the draft: checked again, held again.
        let again = claude.log().len();
        claude.deck.send_keys(b" \x7f");
        assert!(
            common::wait_until(BUDGET, || claude
                .checks(again)
                .iter()
                .any(|c| c == "Written")),
            "{means}: the touch must bring a second check; {}",
            claude.evidence()
        );
        held_for(&claude, &id, &name, &draft, again, means);
        let written = claude
            .checks(since)
            .iter()
            .filter(|c| *c == "Written")
            .count();
        assert!(
            written >= 2,
            "{means}: at least two checks held it, got {written}"
        );
        assert_eq!(
            claude.empty_reads(since),
            0,
            "{means}: no check read the draft empty; {}",
            claude.evidence()
        );
        // Empty the box.
        let emptied_at = claude.log().len();
        if empty_keys.is_empty() {
            claude.deck.send_keys(&vec![0x7f; draft.len()]);
        } else {
            claude.deck.send_keys(empty_keys);
        }
        assert!(
            common::wait_until(Duration::from_secs(10), || claude.composer().as_deref()
                == Some("")),
            "{means}: the box must empty, got {:?}; helper trace:\n{}\n{}",
            claude.composer(),
            helper_trace(),
            claude.evidence()
        );
        if means == "own-editor" {
            let runs = editor.runs();
            assert!(
                runs.last().is_some_and(|r| r["initial"]
                    .as_str()
                    .is_some_and(|t| t.trim_end() == draft)
                    && r["final"] == ""),
                "the person's own editor opened with the draft and emptied it: {runs:?}"
            );
            // Under the production `Hold` policy the person's own editor run
            // leaves the writer scope open for the generation's life
            // (amendment 6 §1.3): the next check is refused, the card says
            // the deck can't check the box, and nothing is typed.
            assert!(
                common::wait_until(BUDGET, || claude.log()[emptied_at..].lines().any(|l| {
                    l.contains("probe refused when scheduled")
                        && l.contains("Scope(Open(EditorReturnUnchecked))")
                }) && claude.record()["inbox"]["wake_hold"]
                    == "cannot_check"),
                "own-editor: the next check is refused and the card says cannot_check; {}",
                claude.evidence()
            );
            common::wait_until(Duration::from_secs(5), || {
                let message = claude.message(&id);
                assert_eq!(message["state"], "queued", "own-editor: {message}");
                assert_eq!(message["deliveries"], 0, "own-editor: {message}");
                let log = claude.log();
                assert!(
                    !log[emptied_at..].contains("doorbell rang")
                        && !log[emptied_at..].contains("empty read applied"),
                    "own-editor: no check read the box and no ring; {}",
                    claude.evidence()
                );
                assert!(
                    claude.ops(emptied_at).iter().all(|o| !o.deck),
                    "own-editor: the deck wrote nothing: {:?}",
                    claude.ops(emptied_at)
                );
                false
            });
            // The person's next prompt hands the task over (the prompt route).
            claude
                .deck
                .send_keys(b"Check for new messages and do what they say.");
            claude.deck.send_keys(b"\r");
            claude.done(&id, &name);
            assert!(
                !claude.log()[emptied_at..].contains("doorbell rang"),
                "own-editor: the prompt route delivered it, not the doorbell; {}",
                claude.evidence()
            );
        } else {
            claude.done(&id, &name);
            let log = claude.log()[emptied_at..].to_string();
            assert!(
                log.contains("empty read applied") && log.contains("doorbell rang"),
                "{means}: a check read the emptied box, then the doorbell rang; {}",
                claude.evidence()
            );
        }
        eprintln!(
            "042 {means}: held after {written} checks, emptied, one delivery: {}",
            claude.message(&id)
        );
        eprintln!("042 {means}: helper trace so far:\n{}", helper_trace());
        claude.idle_without_waiter();
    }
}

/// For three seconds the task stays queued with no delivery, the draft is
/// exactly as typed, no doorbell rang, and the deck wrote nothing to the
/// pane but its checks' one-byte probes.
fn held_for(claude: &Installed, id: &str, name: &str, draft: &str, since: usize, means: &str) {
    common::wait_until(Duration::from_secs(3), || {
        let message = claude.message(id);
        assert_eq!(message["state"], "queued", "{means}: {message}");
        assert_eq!(message["deliveries"], 0, "{means}: {message}");
        // While a check runs, Claude Code hides its input box; any box it
        // draws holds the draft exactly.
        let composer = claude.composer();
        assert!(
            composer.is_none() || composer.as_deref() == Some(draft),
            "{means}: the draft changed to {composer:?}; {}",
            claude.evidence()
        );
        assert!(!claude.deck.workdir().join(name).exists());
        let log = claude.log();
        assert!(
            !log[since..].contains("doorbell rang"),
            "{means}: no ring over a non-empty box; {}",
            claude.evidence()
        );
        assert!(
            claude
                .ops(since)
                .iter()
                .filter(|o| o.deck)
                .all(|o| o.len == 1),
            "{means}: the deck wrote only check bytes: {:?}; {}",
            claude.ops(since),
            claude.evidence()
        );
        false
    });
    claude.expect_composer(draft);
}
