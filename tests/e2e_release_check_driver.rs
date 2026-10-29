#![cfg(all(feature = "e2e", target_os = "linux"))]

//! SC-06: the Rust PTY collector drives only Python screens, in a private
//! directory, and hands actual captured operations to SC-01. No daemon runs.
use dot_agent_deck::release_check::driver::{DriverPlan, DriverReference, collect, evaluate};
use dot_agent_deck::release_check::evaluate::{
    Evaluation, Limits, OracleCapture, OracleReference, Outcome, evaluate_oracle,
};
use serde_json::{Value, json};
use spec::spec;
use std::num::NonZeroU16;
use std::path::Path;

const LINE: &str = "(abcdef) synthetic";
const Q: u64 = 50_000_000;
const ERASE: &[u8] = b"\x1b[26;3H\x1b[2X\x1b[H";

fn frames() -> Value {
    serde_json::from_str(include_str!("fixtures/release-check/driver-frames.json")).unwrap()
}

fn oracle(agent: &str, names: &[&str], rows: u16, cols: u16) -> Value {
    let frames = frames();
    json!({
        "rows": rows, "cols": cols,
        "templates": names.iter().map(|name| json!({
            "name": name, "phase": "idle", "ansi": frames[agent][name]
        })).collect::<Vec<_>>(),
        "transient_modes": [2026], "allowed_controls": [],
        "history_lines": [], "variable_regions": [], "fields": []
    })
}

fn reference(agent: &str, typing: bool) -> Value {
    let mut checkpoints = vec![json!({
        "name": "ready", "expected_template": "ready", "fact": "ready_composer",
        "oracle": oracle(agent, &["ready"], 50, 200)
    })];
    if typing {
        checkpoints.push(json!({
            "name": "typed", "expected_template": "typed", "fact": "echo",
            "oracle": oracle(agent, &["ready", "typed"], 50, 200)
        }));
    }
    json!({"agent": agent, "release": "0.1.2", "checkpoints": checkpoints})
}

fn plan(root: &Path, agent: &str, typing: bool, knobs: &[(&str, &str)]) -> Value {
    let support = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support");
    let mut env = json!({
        "HOME": root, "XDG_CONFIG_HOME": root.join("config"),
        "XDG_DATA_HOME": root.join("data"), "XDG_STATE_HOME": root.join("state"),
        "XDG_CACHE_HOME": root.join("cache"), "XDG_RUNTIME_DIR": root.join("runtime"),
        "CLAUDE_CONFIG_DIR": root.join("claude"), "CODEX_HOME": root.join("codex"),
        "TMPDIR": root, "PATH": "/usr/bin:/bin", "TERM": "xterm-256color",
        "LANG": "C.UTF-8", "LC_ALL": "C.UTF-8", "TZ": "UTC"
    });
    for (name, value) in knobs {
        env[format!("DOT_AGENT_DECK_TEST_COMPOSER_{name}")] = json!(value);
    }
    let mut steps = vec![json!({"op": "checkpoint", "name": "ready"})];
    if typing {
        steps.extend([
            json!({"op": "send", "bytes": LINE.as_bytes()}),
            json!({"op": "checkpoint", "name": "typed"}),
        ]);
    }
    json!({
        "program": "/usr/bin/python3",
        "args": [support.join(format!("{agent}_composer.py")), root.join("screen"),
                 "/bin/true", "0.1.2"],
        "env": env, "cwd": root, "rows": 50, "cols": 200,
        "quiet_ns": Q, "checkpoint_quiet_ns": 2 * Q,
        "deadline_ms": 10000, "max_bytes": 1048576, "read_size": 65536,
        "framing": if agent == "claude" { "quiet_gap" } else { "synchronized" },
        "steps": steps
    })
}

fn capture(plan: Value) -> Value {
    let plan: DriverPlan = serde_json::from_value(plan).unwrap();
    collect(&plan).expect("bounded private PTY capture")
}

fn measure(reference: Value, capture: &Value) -> Evaluation {
    let reference: DriverReference = serde_json::from_value(reference).unwrap();
    evaluate(&reference, capture)
}

fn passed(report: &Evaluation) {
    assert_eq!(report.outcome, Outcome::Passed, "{report:?}");
}

fn differs(report: &Evaluation, expected: &str) {
    match &report.outcome {
        Outcome::Differs { fact, detail } => {
            assert_eq!(fact, expected, "{report:?}");
            assert!(!detail.is_empty(), "failed fact must retain evidence");
        }
        other => panic!("expected Differs {{ fact: {expected:?} }}, got {other:?}"),
    }
}

fn bytes(value: &Value) -> Vec<u8> {
    serde_json::from_value(value.clone()).unwrap()
}

fn replay(reference: Value, trace: Value) -> Evaluation {
    let reference: OracleReference = serde_json::from_value(reference).unwrap();
    let trace: OracleCapture = serde_json::from_value(trace).unwrap();
    evaluate_oracle(&reference, &trace, Limits::default())
}

// Independent vte operation boundaries, solely to position replay cuts and
// audit captured gaps. No screen or verdict is implemented by this helper.
#[derive(Default)]
struct Operations(usize);

impl vte::Perform for Operations {
    fn print(&mut self, _: char) {
        self.0 += 1;
    }
    fn execute(&mut self, _: u8) {
        self.0 += 1;
    }
    fn csi_dispatch(&mut self, _: &vte::Params, _: &[u8], _: bool, _: char) {
        self.0 += 1;
    }
    fn esc_dispatch(&mut self, _: &[u8], _: bool, _: u8) {
        self.0 += 1;
    }
    fn osc_dispatch(&mut self, _: &[&[u8]], _: bool) {
        self.0 += 1;
    }
}

fn operation_ends(raw: &[u8]) -> Vec<usize> {
    let mut parser = vte::Parser::new();
    let mut operations = Operations::default();
    let mut ends = vec![0];
    for (i, byte) in raw.iter().enumerate() {
        let before = operations.0;
        parser.advance(&mut operations, &[*byte]);
        if operations.0 != before {
            ends.push(i + 1);
        }
    }
    ends
}

fn trace_bytes(trace: &Value) -> Vec<u8> {
    trace["reads"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|read| bytes(&read["bytes"]))
        .collect()
}

fn rechunk(trace: &Value, width: usize) -> Value {
    let raw = trace_bytes(trace);
    let ends = operation_ends(&raw);
    let mut boundaries: Vec<usize> = trace["gap_events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|gap| ends[gap["after_operation"].as_u64().unwrap() as usize])
        .collect();
    boundaries.extend([0, raw.len()]);
    boundaries.sort_unstable();
    boundaries.dedup();
    let mut out = trace.clone();
    // Replay timestamps deliberately supply no new timing information. Gaps
    // are the captured operation-indexed events, not re-derived from chunks.
    out["reads"] = json!(
        boundaries
            .windows(2)
            .flat_map(|pair| {
                raw[pair[0]..pair[1]]
                    .chunks(width)
                    .map(|chunk| {
                        json!({
                            "at_ns": 0, "bytes": chunk
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>()
    );
    assert_eq!(out["gap_events"], trace["gap_events"]);
    assert_eq!(trace_bytes(&out), raw);
    out
}

/// Scenario: Launch both synthetic agents, type one line, delete it and resize the PTY. The driver keeps the actual bytes, read times, termios and resize offsets, and the evaluator accepts the reviewed screens.
#[spec("inbox/doorbell/060")]
#[test]
fn doorbell_060_driver_collects_standin_bytes_modes_and_layout() {
    for agent in ["claude", "codex"] {
        let root = tempfile::tempdir().unwrap();
        let mut plan = plan(root.path(), agent, true, &[]);
        plan["steps"].as_array_mut().unwrap().extend([
            json!({"op": "send", "bytes": vec![127u8; LINE.len()]}),
            json!({"op": "checkpoint", "name": "deleted"}),
            json!({"op": "resize", "rows": 40, "cols": 140}),
            json!({"op": "checkpoint", "name": "resized"}),
        ]);
        let capture = capture(plan);
        let mut reference = reference(agent, true);
        reference["checkpoints"].as_array_mut().unwrap().extend([
            json!({"name": "deleted", "expected_template": "ready", "fact": "delete_key",
                   "oracle": oracle(agent, &["ready", "typed"], 50, 200)}),
            json!({"name": "resized", "expected_template": "resized", "fact": "layout",
                   "oracle": oracle(agent, &["resized"], 40, 140)}),
        ]);
        passed(&measure(reference, &capture));
        let frames = frames();
        let expected = ["ready", "typed", "deleted", "resized"]
            .map(|name| frames[agent][name].as_str().unwrap())
            .concat();
        assert_eq!(bytes(&capture["bytes"]), expected.as_bytes(), "{agent}");
        let reads = capture["reads"].as_array().unwrap();
        assert!(!reads.is_empty());
        let mut offset = 0;
        let mut time = 0;
        for read in reads {
            assert_eq!(read["off"].as_u64().unwrap(), offset);
            let now = read["t"].as_u64().unwrap();
            assert!(now >= time, "capture times must be monotonic");
            time = now;
            offset += read["len"].as_u64().unwrap();
        }
        assert_eq!(offset as usize, expected.len());
        let lens: Vec<_> = reads
            .iter()
            .filter(|r| r["len"] != 0)
            .map(|r| r["len"].clone())
            .collect();
        assert_eq!(capture["index"]["read_lens"], json!(lens));
        let resize_at = expected.len() - frames[agent]["resized"].as_str().unwrap().len();
        assert!(
            capture["index"]["edges"]
                .as_array()
                .unwrap()
                .iter()
                .any(|edge| edge["off"] == resize_at && edge["rows"] == 40 && edge["cols"] == 140)
        );
        assert!(
            capture["modes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["bits"] == json!([false, false]))
        );
    }
}

/// Scenario: Run each composer faithfully, then separately change its release, input mode, echo, complete screen or transient output. The evaluator names the failed fact for every changed candidate.
#[spec("inbox/doorbell/060")]
#[test]
fn doorbell_060_driver_fault_knobs_name_the_failed_fact() {
    for agent in ["claude", "codex"] {
        let root = tempfile::tempdir().unwrap();
        passed(&measure(
            reference(agent, true),
            &capture(plan(root.path(), agent, true, &[])),
        ));
        for (knob, value, fact, typing) in [
            ("RELEASE", "9.8.7", "release_recogniser", false),
            ("DRIVER_FAULT", "echo_mode", "input_mode", false),
            ("DRIVER_FAULT", "missing_echo", "echo", true),
            ("DRIVER_FAULT", "overlay", "transient_content", false),
            ("DRIVER_FAULT", "moved_glyph", "transient_content", false),
            ("DRIVER_FAULT", "mode", "mode_transient", false),
            ("DRIVER_FAULT", "control", "unrecognised_control", false),
            ("DRIVER_FAULT", "modal", "unrecognised_frame", false),
        ] {
            let root = tempfile::tempdir().unwrap();
            let observed = capture(plan(root.path(), agent, typing, &[(knob, value)]));
            differs(&measure(reference(agent, typing), &observed), fact);
        }
    }
}

/// Scenario: Capture an overlay drawn and erased in one stand-in write. Its final grid equals the good grid, but replaying the whole frame in one read still fails at the hidden operation.
#[spec("inbox/doorbell/060")]
#[test]
fn doorbell_060_driver_overlay_inside_one_replay_read() {
    for agent in ["claude", "codex"] {
        let root = tempfile::tempdir().unwrap();
        let capture = capture(plan(
            root.path(),
            agent,
            false,
            &[("DRIVER_FAULT", "overlay")],
        ));
        let trace = &capture["observations"][0]["oracle"];
        let raw = trace_bytes(trace);
        let mut observed = vt100::Parser::new(
            NonZeroU16::new(50).unwrap(),
            NonZeroU16::new(200).unwrap(),
            0,
        );
        observed.process(&raw);
        let mut good = vt100::Parser::new(
            NonZeroU16::new(50).unwrap(),
            NonZeroU16::new(200).unwrap(),
            0,
        );
        good.process(frames()[agent]["ready"].as_str().unwrap().as_bytes());
        assert_eq!(
            observed.screen().contents_formatted(),
            good.screen().contents_formatted()
        );
        let whole = rechunk(trace, usize::MAX);
        assert_eq!(whole["reads"].as_array().unwrap().len(), 1);
        differs(
            &replay(oracle(agent, &["ready"], 50, 200), whole),
            "transient_content",
        );
    }
}

/// Scenario: Pause between Claude's ready and typed screens. Capture derives exactly the elapsed-read gap at the last completed operation before typing, retaining the measured final quiet observation separately from phase end.
#[spec("inbox/doorbell/060")]
#[test]
fn doorbell_060_driver_derives_gaps_once_at_capture() {
    let root = tempfile::tempdir().unwrap();
    let capture = capture(plan(root.path(), "claude", true, &[]));
    let trace = &capture["observations"][1]["oracle"];
    let raw = trace_bytes(trace);
    let gaps = trace["gap_events"].as_array().unwrap();
    let ends = operation_ends(&raw);
    let reads: Vec<_> = capture["reads"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|read| read["len"].as_u64().unwrap() > 0)
        .collect();
    let mut expected = Vec::new();
    for pair in reads.windows(2) {
        let elapsed = pair[1]["t"].as_u64().unwrap() - pair[0]["t"].as_u64().unwrap();
        if elapsed >= Q {
            let off = pair[1]["off"].as_u64().unwrap() as usize;
            let operations = ends.partition_point(|end| *end <= off) - 1;
            expected.push(operations);
            assert!(gaps.iter().any(|gap|
                gap["after_operation"] == operations && gap["elapsed_ns"] == elapsed));
        }
    }
    assert!(
        !expected.is_empty(),
        "the controller must actually wait across Q"
    );
    let last_operation = ends.len() - 1;
    for gap in gaps {
        let position = gap["after_operation"].as_u64().unwrap() as usize;
        assert!(
            expected.contains(&position) || position == last_operation,
            "invented gap at operation {position}"
        );
        assert!(gap["elapsed_ns"].as_u64().unwrap() >= Q);
    }
    assert!(
        gaps.iter()
            .any(|gap| gap["after_operation"] == last_operation),
        "the terminal quiet observation needs its own capture-time evidence"
    );
    let terminal = capture["quiet_observations"]
        .as_array()
        .unwrap()
        .last()
        .unwrap();
    assert_eq!(terminal["off"], raw.len());
    assert!(terminal["elapsed_ns"].as_u64().unwrap() >= Q);
}

/// Scenario: Capture faithful and faulty stand-ins, then split their output into one-byte, seven-byte or whole-frame reads without moving a captured gap. Every verdict and frame/stall count stays equal, including splits inside UTF-8 and CSI sequences.
#[spec("inbox/doorbell/060")]
#[test]
fn doorbell_060_driver_timing_preserving_rechunking() {
    for agent in ["claude", "codex"] {
        for fault in [None, Some("overlay"), Some("mode"), Some("control")] {
            let root = tempfile::tempdir().unwrap();
            let knobs = fault.map(|f| vec![("DRIVER_FAULT", f)]).unwrap_or_default();
            let capture = capture(plan(root.path(), agent, true, &knobs));
            let trace = &capture["observations"][1]["oracle"];
            let reference = oracle(agent, &["ready", "typed"], 50, 200);
            let baseline = replay(reference.clone(), trace.clone());
            if let Some(fault) = fault {
                differs(
                    &baseline,
                    match fault {
                        "overlay" => "transient_content",
                        "mode" => "mode_transient",
                        _ => "unrecognised_control",
                    },
                );
            } else {
                passed(&baseline);
            }
            for width in [1, 7, usize::MAX] {
                let report = replay(reference.clone(), rechunk(trace, width));
                assert_eq!(
                    report.outcome, baseline.outcome,
                    "{agent}/{fault:?}/{width}"
                );
                assert_eq!(report.complete_frames, baseline.complete_frames);
                assert_eq!(report.stalls, baseline.stalls);
                assert_eq!(report.accepted_t1_stalls, baseline.accepted_t1_stalls);
            }
        }
    }
}

/// Scenario: Capture Claude erasing and restoring its old placeholder before typing, preserving the original quiet gaps. Add a gap just after the erased screen: this new frame boundary makes the later restored old glyphs fail.
#[spec("inbox/doorbell/060")]
#[test]
fn doorbell_060_driver_added_gap_can_change_the_verdict() {
    let root = tempfile::tempdir().unwrap();
    let capture = capture(plan(
        root.path(),
        "claude",
        true,
        &[("TIMING_RESTORE", "1")],
    ));
    let trace = &capture["observations"][1]["oracle"];
    let mut reference = oracle("claude", &["ready", "typed"], 50, 200);
    let erased = format!(
        "{}{}",
        frames()["claude"]["ready"].as_str().unwrap(),
        std::str::from_utf8(ERASE).unwrap()
    );
    reference["templates"].as_array_mut().unwrap().push(json!({
        "name": "erased", "phase": "idle", "ansi": erased
    }));
    for width in [1, 7, usize::MAX] {
        passed(&replay(reference.clone(), rechunk(trace, width)));
    }
    let raw = trace_bytes(trace);
    let erase_end = raw.windows(ERASE.len()).position(|w| w == ERASE).unwrap() + ERASE.len();
    let after_operation = operation_ends(&raw[..erase_end]).len() - 1;
    let mut changed = trace.clone();
    let gaps = changed["gap_events"].as_array_mut().unwrap();
    assert!(
        !gaps
            .iter()
            .any(|gap| gap["after_operation"] == after_operation)
    );
    gaps.push(json!({"after_operation": after_operation, "elapsed_ns": Q}));
    gaps.sort_by_key(|gap| gap["after_operation"].as_u64().unwrap());
    for width in [1, 7, usize::MAX] {
        differs(
            &replay(reference.clone(), rechunk(&changed, width)),
            "transient_content",
        );
    }
}

/// Scenario: Run both synthetic composers' default and fault controls in private PTYs. Defaults match frozen parent bytes, and every enabled knob changes only its declared observable output or termios.
#[spec("inbox/doorbell/060")]
#[test]
fn doorbell_060_driver_standin_knob_self_checks() {
    let output = std::process::Command::new("/usr/bin/python3")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/composer_knob_checks.py"))
        .arg("DriverKnobs")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
