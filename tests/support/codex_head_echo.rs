//! Reading a Codex `head_echo` capture (`tests/support/codex_head_echo_measure.py`)
//! the way the deck reads its pane: every PTY read pushed, in order, through
//! the deck's own evidence state for Codex's screen profile, and the screen
//! after each read through the frame builder the doorbell's transaction
//! reads. Every frame is built as the frame builder builds a Clean one, so
//! the screen is read whatever the evidence state; whether that state was
//! Clean is recorded beside each reading, and asserted separately: the
//! transaction reads only Clean frames, and a fresh Codex at 24 × 80 draws no
//! width witness before its first turn.
//! The recorder only scheduled the writes with pyte; every reading here is the
//! deck's.
//!
//! Shared by the lane-2 recordings (`tests/e2e_codex_release_measure.rs`) and
//! the fast-tier replay of the committed captures
//! (`tests/codex_head_echo_evidence.rs`).
#![allow(dead_code)]

use dot_agent_deck::doorbell_screen::{
    CODEX_PROFILE, Grid, TOKEN_HEAD_CHARS, WithdrawalRefusal, frame, strict_withdrawal,
};
use dot_agent_deck::doorbell_txn::{ECHO_WAIT, Frame, SUBMIT_CONFIRM_WAIT, SUBMIT_DELAY};
use dot_agent_deck::evidence_state::{EvidenceState, EvidenceStatus};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

/// One capture: the raw bytes, its index (read lengths, named offsets, the
/// lines it typed) and its result (read and write times, per-trial notes).
pub struct Capture {
    pub name: String,
    pub bytes: Vec<u8>,
    pub index: Value,
    pub result: Value,
}

impl Capture {
    /// A capture as the recorder left it: `trace.bin`, `index.json`,
    /// `result.json` in `dir`.
    pub fn recorded(dir: &Path) -> Self {
        let json = |name: &str| -> Value {
            serde_json::from_slice(&std::fs::read(dir.join(name)).unwrap()).unwrap()
        };
        Self {
            name: dir.display().to_string(),
            bytes: std::fs::read(dir.join("trace.bin")).unwrap(),
            index: json("index.json"),
            result: json("result.json"),
        }
    }

    /// A committed capture: `<stem>.bin`, `<stem>.json`, `<stem>.result.json`.
    pub fn committed(stem: &str) -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let json = |ext: &str| -> Value {
            serde_json::from_slice(&std::fs::read(base.join(format!("{stem}{ext}"))).unwrap())
                .unwrap()
        };
        Self {
            name: stem.to_string(),
            bytes: std::fs::read(base.join(format!("{stem}.bin"))).unwrap(),
            index: json(".json"),
            result: json(".result.json"),
        }
    }

    pub fn point(&self, name: &str) -> usize {
        self.index["points"][name]
            .as_u64()
            .unwrap_or_else(|| panic!("{}: no point {name}", self.name)) as usize
    }

    fn rows(&self) -> u16 {
        self.index["rows"].as_u64().unwrap() as u16
    }

    fn cols(&self) -> u16 {
        self.index["cols"].as_u64().unwrap() as u16
    }

    pub fn lines(&self) -> Vec<String> {
        self.index["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap().to_string())
            .collect()
    }

    /// The monotonic time of the write labelled `label`, and when it returned.
    fn sent(&self, label: &str) -> (u64, u64) {
        let event = self.result["events"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["label"] == label)
            .unwrap_or_else(|| panic!("{}: no write {label}", self.name));
        (
            event["t"].as_u64().unwrap(),
            event["done"].as_u64().unwrap(),
        )
    }
}

/// What the deck read after one PTY read.
pub struct Read {
    /// Stream offset at the end of the read.
    pub end: usize,
    /// When the recorder got it (monotonic ns).
    pub t: u64,
    /// The deck's evidence state was Clean after this read (with no layout
    /// edge in a capture, its parser's screen is the screen read below).
    pub clean: bool,
    /// The frame builder's reading of the screen after this read, built as
    /// a Clean frame is whether or not `clean` holds.
    pub frame: Option<Frame>,
    /// Which of the capture's lines the transcript shows as a user message.
    pub users: Vec<usize>,
    /// The strict withdrawal recogniser's verdict for each owned text asked
    /// about at this read.
    pub strict: HashMap<String, Result<usize, WithdrawalRefusal>>,
}

/// Every read of `capture`, replayed. `owned_at(end)` names the owned texts
/// the strict recogniser is asked about at a read ending at `end`.
pub fn replay(capture: &Capture, owned_at: impl Fn(usize) -> Vec<String>) -> Vec<Read> {
    let lines = capture.lines();
    let mut state = EvidenceState::new(Some(&CODEX_PROFILE), capture.rows(), capture.cols());
    let mut screen = dot_agent_deck::vt_grid::parser(capture.rows(), capture.cols(), 0);
    let reads = capture.result["reads"].as_array().unwrap();
    let lens = capture.index["read_lens"].as_array().unwrap();
    assert_eq!(reads.len(), lens.len(), "{}", capture.name);
    let mut out = Vec::with_capacity(reads.len());
    let mut pos = 0;
    for (read, len) in reads.iter().zip(lens) {
        let len = len.as_u64().unwrap() as usize;
        assert_eq!(
            read["off"].as_u64().unwrap() as usize,
            pos,
            "{}",
            capture.name
        );
        let bytes = &capture.bytes[pos..pos + len];
        state.push(bytes, |_| Vec::new());
        screen.process(bytes);
        pos += len;
        let clean = matches!(state.status(), EvidenceStatus::Clean { .. });
        let grid = Grid::of(screen.screen(), pos as u64);
        if clean {
            assert_eq!(
                state.clean_frame().map(|g| g.cells),
                Some(grid.cells.clone()),
                "{}: the evidence parser's screen",
                capture.name
            );
        }
        let f = frame(&CODEX_PROFILE, &grid, state.layout_epoch(), true, None);
        let users = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| (CODEX_PROFILE.transcript_user_line)(&grid, l))
            .map(|(i, _)| i)
            .collect();
        let strict = owned_at(pos)
            .into_iter()
            .map(|owned| {
                let verdict = strict_withdrawal(&CODEX_PROFILE, &grid, &owned);
                (owned, verdict)
            })
            .collect();
        out.push(Read {
            end: pos,
            t: read["t"].as_u64().unwrap(),
            clean,
            frame: Some(f),
            users,
            strict,
        });
    }
    assert_eq!(pos, capture.bytes.len(), "{}", capture.name);
    out
}

fn holds_prefix(frame: &Frame, line: &str) -> bool {
    frame.composer.as_ref().is_some_and(|c| {
        !c.placeholder && !c.hidden && c.cursor_at_end && line.starts_with(c.text.as_str())
    })
}

/// The composer is drawn and shows its placeholder or a prefix of `text`. A
/// frame with no composer fails: an overlay that hides the composer is not
/// one of the transaction's readings, whether or not it is a recognised
/// dialog.
fn placeholder_or_prefix(frame: &Frame, text: &str) -> bool {
    frame
        .composer
        .as_ref()
        .is_some_and(|c| c.placeholder || text.starts_with(c.text.as_str()))
}

/// The last read that ends at or before `off`.
fn last_at(reads: &[Read], off: usize) -> &Read {
    reads
        .iter()
        .rev()
        .find(|r| r.end <= off)
        .expect("a read before the offset")
}

fn between(reads: &[Read], from: usize, to: usize) -> impl Iterator<Item = &Read> {
    reads.iter().filter(move |r| r.end > from && r.end <= to)
}

/// One staged trial's readings.
#[derive(Debug, Clone)]
pub struct Trial {
    pub n: usize,
    /// Head write to the first frame holding exactly the head (read on that
    /// frame whether or not the evidence state was Clean: see `clean`).
    pub head_echo_ms: f64,
    /// Rest write to the first frame holding exactly the whole line (as for
    /// `head_echo_ms`).
    pub line_echo_ms: f64,
    /// The rest's write returning to the Enter.
    pub floor_ms: f64,
    /// The Enter to the first frame with the line as a transcript user
    /// message and the composer no longer holding it.
    pub transcript_ms: f64,
    pub first_submit: bool,
    /// The deck's evidence state was Clean from admission to the Enter.
    pub clean: bool,
}

/// The staged trials of a `staged` capture, each checked against what the
/// transaction requires (panics naming the trial and the reading otherwise).
pub fn staged_trials(capture: &Capture) -> Vec<Trial> {
    let reads = replay(capture, |_| Vec::new());
    let lines = capture.lines();
    let trials = capture.result["trials"].as_array().unwrap();
    assert!(!trials.is_empty(), "{}: no trials", capture.name);
    let rollout: Vec<&str> = capture.result["rollout_user_messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let mut out = Vec::new();
    for (i, trial) in trials.iter().enumerate() {
        let n = i + 1;
        let line = trial["line"].as_str().unwrap();
        assert_eq!(lines[i], line, "{} trial {n}", capture.name);
        let head = &line[..TOKEN_HEAD_CHARS];
        let who = format!("{} trial {n}", capture.name);
        let at_trial = capture.point(&format!("mark:trial-{n}"));
        let at_rest = capture.point(&format!("mark:rest-{n}"));
        let at_cr = capture.point(&format!("mark:cr-{n}"));
        let (head_sent, _) = capture.sent(&format!("head-{n}"));
        let (rest_sent, rest_done) = capture.sent(&format!("rest-{n}"));
        let (cr_sent, _) = capture.sent(&format!("cr-{n}"));

        // Admitted: the last frame before the head is the empty ready
        // composer with no dialog.
        let admission = last_at(&reads, at_trial);
        let f = admission.frame.as_ref().unwrap();
        assert!(f.empty_and_ready(), "{who}: admission frame {f:?}");
        let clean = admission.clean && between(&reads, at_trial, at_cr).all(|r| r.clean);

        // The head alone: only the placeholder or a prefix of the head until
        // its exact echo, never a dialog or the line as a user message.
        let mut head_echo = None;
        for r in between(&reads, at_trial, at_rest) {
            let Some(f) = &r.frame else { continue };
            assert!(f.dialog.is_none(), "{who}: a dialog after the head: {f:?}");
            assert!(
                r.users.is_empty() || !r.users.contains(&i),
                "{who}: submitted early"
            );
            assert!(
                placeholder_or_prefix(f, head),
                "{who}: {:?} after the head",
                f.composer
            );
            if head_echo.is_none() && f.holds_exactly(head) {
                head_echo = Some(r.t);
            }
        }
        let head_echo =
            head_echo.unwrap_or_else(|| panic!("{who}: no frame held the head before the rest"));
        let seam = last_at(&reads, at_rest).frame.as_ref().unwrap();
        assert!(
            seam.holds_exactly(head),
            "{who}: the rest's seam frame {seam:?}"
        );
        let head_echo_ns = head_echo - head_sent;
        assert!(
            head_echo_ns < ECHO_WAIT.as_nanos() as u64,
            "{who}: head echo {head_echo_ns} ns"
        );

        // The rest: only prefixes of the line until it is held exactly.
        let mut line_echo = None;
        for r in between(&reads, at_rest, at_cr) {
            let Some(f) = &r.frame else { continue };
            assert!(f.dialog.is_none(), "{who}: a dialog after the rest: {f:?}");
            assert!(!r.users.contains(&i), "{who}: submitted before the Enter");
            assert!(
                holds_prefix(f, line)
                    && f.composer.as_ref().unwrap().text.len() >= TOKEN_HEAD_CHARS,
                "{who}: {:?} after the rest",
                f.composer
            );
            if line_echo.is_none() && f.holds_exactly(line) {
                line_echo = Some(r.t);
            }
        }
        let line_echo = line_echo
            .unwrap_or_else(|| panic!("{who}: the whole line never echoed before the Enter"));
        let line_echo_ns = line_echo - rest_sent;
        assert!(
            line_echo_ns < ECHO_WAIT.as_nanos() as u64,
            "{who}: line echo {line_echo_ns} ns"
        );
        // The exact composer echo immediately before the Enter.
        let pre_cr = last_at(&reads, at_cr).frame.as_ref().unwrap();
        assert!(
            pre_cr.holds_exactly(line),
            "{who}: before the Enter {pre_cr:?}"
        );
        let floor_ns = cr_sent - rest_done;
        assert!(
            floor_ns >= SUBMIT_DELAY.as_nanos() as u64,
            "{who}: Enter after {floor_ns} ns"
        );

        // Submitted, read off the transcript within the confirm wait.
        let confirm = SUBMIT_CONFIRM_WAIT.as_nanos() as u64;
        let submitted = reads
            .iter()
            .filter(|r| r.end > at_cr && r.t <= cr_sent + confirm)
            .find(|r| {
                r.users.contains(&i)
                    && r.frame.as_ref().is_some_and(|f| {
                        f.composer
                            .as_ref()
                            .is_none_or(|c| !c.text.starts_with(line))
                    })
            })
            .unwrap_or_else(|| panic!("{who}: no transcript user message within the confirm wait"));
        assert_eq!(
            rollout.iter().filter(|m| **m == line).count(),
            1,
            "{who}: Codex's own record of the submitted prompt"
        );
        out.push(Trial {
            n,
            head_echo_ms: head_echo_ns as f64 / 1e6,
            line_echo_ms: line_echo_ns as f64 / 1e6,
            floor_ms: floor_ns as f64 / 1e6,
            transcript_ms: (submitted.t - cr_sent) as f64 / 1e6,
            first_submit: n == 1,
            clean,
        });
    }
    // Nothing doorbell-shaped but the deck's own lines was submitted, each
    // exactly as typed.
    let shaped: Vec<&&str> = rollout
        .iter()
        .filter(|m| m.contains("dot-agent-deck: message"))
        .collect();
    assert_eq!(shaped.len(), trials.len(), "{}: {rollout:?}", capture.name);
    out
}

/// A whole token head that is never on screen: the deck's own head when the
/// composer holds someone else's line.
pub const NOT_OURS: &str = "(aaaaaa)";

/// One control's readings.
#[derive(Debug, Clone)]
pub struct Control {
    pub name: String,
    /// Write to the first frame holding exactly the typed text (every
    /// controls ring is at 50 × 200, read on frames built as Clean ones; the
    /// evidence state is not checked here).
    pub echo_ms: Option<f64>,
    /// The strict recogniser's verdict on the frame the deletes were sent on.
    pub strict: Option<Result<usize, WithdrawalRefusal>>,
    /// The deletes to the first frame showing the placeholder.
    pub placeholder_ms: Option<f64>,
    /// For an Enter control: whether Codex submitted the line.
    pub submitted: Option<bool>,
    /// For an Enter-after-echo control: the rest's write returning to the
    /// Enter.
    pub enter_ms: Option<f64>,
}

/// The controls of a `controls` capture, each checked (panics otherwise).
pub fn controls(capture: &Capture) -> Vec<Control> {
    let entries = capture.result["controls"].as_array().unwrap().clone();
    let windows: Vec<(usize, usize, String)> = entries
        .iter()
        .filter_map(|e| {
            let name = e["control"].as_str().unwrap();
            let text = e["text"].as_str().unwrap().to_string();
            let at = capture.index["points"][format!("mark:deletes:{name}")].as_u64()?;
            Some((at as usize, at as usize, text))
        })
        .collect();
    // Ask the strict recogniser on the frame each control's deletes went on,
    // about the owned text and, for the foreign lines, a fresh deck head.
    let reads = replay(capture, |end| {
        windows
            .iter()
            .filter(|(at, _, _)| end <= *at)
            .take(1)
            .map(|(_, _, text)| text.clone())
            .chain(std::iter::once(NOT_OURS.to_string()))
            .collect()
    });
    let mut out = Vec::new();
    for e in &entries {
        let name = e["control"].as_str().unwrap().to_string();
        let text = e["text"].as_str().unwrap();
        let who = format!("{} {name}", capture.name);
        let at = capture.point(&format!("mark:{name}"));
        let (sent, _) = capture.sent(&name);
        if name.starts_with("enter-") {
            let after = capture.point(&format!("typed:{name}"));
            let line_index = capture.lines().iter().position(|l| l == text).unwrap();
            let submitted = between(&reads, at, after).any(|r| r.users.contains(&line_index));
            for r in between(&reads, at, after) {
                if let Some(f) = &r.frame {
                    assert!(f.dialog.is_none(), "{who}: a dialog {f:?}");
                }
            }
            // An Enter after the echo: the frame it was written on holds
            // exactly the line, with the cursor after it.
            let mut enter_ms = None;
            if name.starts_with("enter-at-") {
                let at_cr = capture.point(&format!("mark:cr:{name}"));
                let f = last_at(&reads, at_cr).frame.as_ref().unwrap();
                assert!(f.holds_exactly(text), "{who}: before the Enter {f:?}");
                let (_, rest_done) = capture.sent(&format!("rest:{name}"));
                let (cr_sent, _) = capture.sent(&format!("cr:{name}"));
                enter_ms = Some((cr_sent - rest_done) as f64 / 1e6);
            }
            // Kept: the composer still starts with the line, and the strict
            // recogniser's verdict on it is recorded (after a kept Enter the
            // deck would withdraw only text it can confirm).
            let mut strict = None;
            if !submitted {
                let f = last_at(&reads, after).frame.as_ref().unwrap();
                assert!(
                    f.composer
                        .as_ref()
                        .is_some_and(|c| c.text.starts_with(text)),
                    "{who}: neither submitted nor kept: {f:?}"
                );
                assert!(!f.empty_and_ready(), "{who}: a kept line read as empty");
                let deletes_at = capture.point(&format!("mark:deletes:{name}"));
                strict = last_at(&reads, deletes_at).strict.get(text).cloned();
            }
            out.push(Control {
                name,
                echo_ms: None,
                strict,
                placeholder_ms: None,
                submitted: Some(submitted),
                enter_ms,
            });
            continue;
        }
        let deletes_at = capture.point(&format!("mark:deletes:{name}"));
        let (deletes_sent, _) = capture.sent(&format!("deletes:{name}"));
        let mut echo = None;
        for r in between(&reads, at, deletes_at) {
            let Some(f) = &r.frame else { continue };
            assert!(f.dialog.is_none(), "{who}: a dialog {f:?}");
            assert!(r.users.is_empty(), "{who}: something was submitted");
            if name.starts_with("second-") {
                let line = e["line"].as_str().unwrap();
                assert!(placeholder_or_prefix(f, line), "{who}: {:?}", f.composer);
            } else {
                assert!(placeholder_or_prefix(f, text), "{who}: {:?}", f.composer);
            }
            if echo.is_none() && f.holds_exactly(text) {
                echo = Some(r.t);
            }
        }
        // A trailing blank draws nothing: the screen cannot show that the
        // composer holds it, so such a text is never read as held exactly
        // (and the strict recogniser must refuse it).
        let confirmable = !text.ends_with(' ');
        assert_eq!(echo.is_some(), confirmable, "{who}: held exactly");
        let seam = last_at(&reads, deletes_at);
        let f = seam.frame.as_ref().unwrap();
        assert_eq!(
            f.holds_exactly(text),
            confirmable,
            "{who}: at the deletes {f:?}"
        );
        assert!(
            !f.empty_and_ready(),
            "{who}: a box holding text read as empty"
        );
        let strict = seam
            .strict
            .get(text)
            .cloned()
            .unwrap_or_else(|| panic!("{who}: no strict reading"));
        if !confirmable {
            assert!(
                strict.is_err(),
                "{who}: withdrew text the screen cannot confirm"
            );
        }
        // Text the deck does not own is never withdrawn as the deck's.
        assert_eq!(
            seam.strict.get(NOT_OURS),
            Some(&Err(WithdrawalRefusal::NotOwnedText)),
            "{who}: another head's withdrawal"
        );
        let mut placeholder = None;
        for r in reads.iter().filter(|r| r.end > deletes_at) {
            if let Some(f) = &r.frame {
                if f.dialog.is_none() && f.composer.as_ref().is_some_and(|c| c.placeholder) {
                    placeholder = Some(r.t);
                    break;
                }
                assert!(
                    r.users.is_empty(),
                    "{who}: something was submitted by the deletes"
                );
            }
        }
        let placeholder =
            placeholder.unwrap_or_else(|| panic!("{who}: no placeholder after the deletes"));
        out.push(Control {
            name,
            echo_ms: echo.map(|echo| (echo - sent) as f64 / 1e6),
            strict: Some(strict),
            placeholder_ms: Some((placeholder - deletes_sent) as f64 / 1e6),
            submitted: None,
            enter_ms: None,
        });
    }
    // Codex's own record agrees with the screen: each Enter control's line
    // is there once when the transcript showed it submitted and not at all
    // when it was kept, and nothing else doorbell-shaped was submitted (no
    // head, partial head, second stage or foreign line typed and deleted).
    let rollout: Vec<&str> = capture.result["rollout_user_messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    for c in out.iter().filter(|c| c.submitted.is_some()) {
        let text = entries
            .iter()
            .find(|e| e["control"] == c.name.as_str())
            .unwrap()["text"]
            .as_str()
            .unwrap();
        assert_eq!(
            rollout.iter().filter(|m| **m == text).count(),
            usize::from(c.submitted == Some(true)),
            "{} {}: Codex's own record",
            capture.name,
            c.name
        );
    }
    let shaped = rollout
        .iter()
        .filter(|m| m.contains("dot-agent-deck: message"))
        .count();
    assert_eq!(
        shaped,
        out.iter().filter(|c| c.submitted == Some(true)).count(),
        "{}: {rollout:?}",
        capture.name
    );
    out
}

/// The verdicts a `controls` capture must show (Q27), shared by the lane-2
/// recording and the fast replay of the committed capture: six whole heads
/// withdrawn with eight deletes; seven partial heads refused as no whole
/// token; the second stage's head plus none, one (a space), ten and forty
/// characters counted 8, refused as not owned text, 18 and 48; one foreign
/// and one old-token line; three lines with the Enter in the same burst kept
/// and refused as cursor elsewhere, and so not in Codex's record; and six
/// Enters after the line's echo submitted, each once in Codex's record
/// (`controls` checks the record and every reading behind these).
pub fn checked_controls(capture: &Capture) -> Vec<Control> {
    let controls = controls(capture);
    let named = |prefix: &'static str| controls.iter().filter(move |c| c.name.starts_with(prefix));
    assert_eq!(named("head-").count(), 6);
    for c in named("head-") {
        assert_eq!(c.strict, Some(Ok(TOKEN_HEAD_CHARS)), "{c:?}");
    }
    assert_eq!(named("partial-").count(), 7);
    for c in named("partial-") {
        assert_eq!(
            c.strict,
            Some(Err(WithdrawalRefusal::NoWholeToken)),
            "{c:?}"
        );
    }
    // The head and one byte of the rest, a space, draws as the head with the
    // cursor a cell further on: nothing on screen shows the space, so the
    // strict recogniser refuses it and the deck withdraws nothing.
    let second: Vec<_> = named("second-").map(|c| c.strict).collect();
    assert_eq!(
        second,
        [
            Some(Ok(8)),
            Some(Err(WithdrawalRefusal::NotOwnedText)),
            Some(Ok(18)),
            Some(Ok(48))
        ]
    );
    assert_eq!(named("foreign").count(), 1);
    assert_eq!(named("old-token").count(), 1);
    // An Enter in the same burst as the line is kept with it, and what is
    // kept is not the owned text with the cursor after it.
    let burst: Vec<_> = controls
        .iter()
        .filter(|c| c.name.starts_with("enter-") && !c.name.starts_with("enter-at-"))
        .collect();
    assert_eq!(burst.len(), 3);
    for c in burst {
        assert_eq!(c.submitted, Some(false), "{c:?}");
        assert_eq!(
            c.strict,
            Some(Err(WithdrawalRefusal::CursorElsewhere)),
            "{c:?}"
        );
    }
    // An Enter written after the line's exact echo submits it. The recorder
    // asks for it 50, 100 or 120 ms after the rest, but never before the
    // echo, so when it went is in `enter_ms`.
    assert_eq!(named("enter-at-").count(), 6);
    for c in named("enter-at-") {
        assert_eq!(c.submitted, Some(true), "{c:?}");
        assert!(c.enter_ms.is_some(), "{c:?}");
    }
    controls
}

/// min / median / max of `values`, in ms.
pub fn spread(values: &[f64]) -> (f64, f64, f64) {
    let mut v = values.to_vec();
    v.sort_by(f64::total_cmp);
    let median = if v.len() % 2 == 1 {
        v[v.len() / 2]
    } else {
        (v[v.len() / 2 - 1] + v[v.len() / 2]) / 2.0
    };
    (v[0], median, v[v.len() - 1])
}
