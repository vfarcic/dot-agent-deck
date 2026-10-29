//! SC-01 contract tests. Oracle fixtures are intentionally tiny reviewed grids;
//! recorded-fact tests replay shipped captures, without starting any agent.
//! The evaluator's Passed is a measurement result, not publication authority.
use std::path::PathBuf;

use dot_agent_deck::doorbell_activation::{DialogKeys, KeyEffect, Measurement, RELEASE_RECORDS};
use dot_agent_deck::release_check::evaluate::{
    Evaluation, Limits, OracleCapture, OracleReference, Outcome, RecordedCapture,
    evaluate_key_effects, evaluate_oracle, evaluate_recording,
};
use serde_json::{Value, json};
use spec::spec;

const SYNC_ON: &str = "\x1b[?2026h";
const SYNC_OFF: &str = "\x1b[?2026l";

fn reference() -> Value {
    serde_json::from_str(include_str!("fixtures/release-check/oracle.json")).unwrap()
}

fn control(name: &str) -> String {
    let controls: Value =
        serde_json::from_str(include_str!("fixtures/release-check/controls.json")).unwrap();
    controls[name].as_str().unwrap().to_owned()
}

fn sync(frame: &str) -> String {
    format!("{SYNC_ON}{frame}{SYNC_OFF}")
}

fn capture(bytes: &str) -> Value {
    json!({
        "rows": 4, "cols": 12, "phase": "idle", "framing": "synchronized",
        "reads": [{"at_ns": 0, "bytes": bytes.as_bytes()}],
        "gap_events": [], "phase_ended": true
    })
}

fn transition(middle: &str) -> Value {
    capture(&format!(
        "{}{SYNC_ON}{middle}{}{SYNC_OFF}",
        sync(&control("before")),
        control("after")
    ))
}

fn run(reference: Value, capture: Value, limits: Limits) -> Evaluation {
    let reference: OracleReference = serde_json::from_value(reference).unwrap();
    let capture: OracleCapture = serde_json::from_value(capture).unwrap();
    evaluate_oracle(&reference, &capture, limits)
}

fn evaluate(capture: Value) -> Evaluation {
    run(reference(), capture, Limits::default())
}

fn passed(report: &Evaluation) {
    assert_eq!(report.outcome, Outcome::Passed, "{report:?}");
}

fn differs(report: &Evaluation, expected: &str) {
    match &report.outcome {
        Outcome::Differs { fact, detail } => {
            assert_eq!(fact, expected, "{report:?}");
            assert!(!detail.is_empty(), "a failed fact needs retained evidence");
        }
        other => panic!("expected Differs {{ fact: {expected:?} }}, got {other:?}"),
    }
}

fn inconclusive(report: &Evaluation, expected: &str) {
    match &report.outcome {
        Outcome::Inconclusive { reason } => assert_eq!(reason, expected, "{report:?}"),
        other => panic!("expected Inconclusive {{ reason: {expected:?} }}, got {other:?}"),
    }
}

fn quiet(middle: &str, gaps: &[(usize, u64)]) -> Value {
    let mut trace = capture(&format!(
        "{}{middle}{}",
        control("before"),
        control("after")
    ));
    trace["framing"] = json!("quiet_gap");
    trace["gap_events"] = json!(
        gaps.iter()
            .map(|(op, ns)| json!({
                "after_operation": op, "elapsed_ns": ns
            }))
            .collect::<Vec<_>>()
    );
    trace
}

/// Re-split bytes only. The already-derived operation-indexed gaps survive verbatim.
fn rechunk(trace: &Value, width: usize) -> Value {
    let bytes: Vec<u8> = trace["reads"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|read| {
            read["bytes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|b| b.as_u64().unwrap() as u8)
        })
        .collect();
    let mut split = trace.clone();
    split["reads"] = json!(
        bytes
            .chunks(width)
            .map(|bytes| json!({
                "at_ns": 0, "bytes": bytes
            }))
            .collect::<Vec<_>>()
    );
    assert_eq!(split["gap_events"], trace["gap_events"]);
    split
}

fn recording(stem: &str) -> RecordedCapture {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    RecordedCapture {
        bytes: std::fs::read(root.join(format!("{stem}.bin"))).unwrap(),
        index: serde_json::from_slice(&std::fs::read(root.join(format!("{stem}.json"))).unwrap())
            .unwrap(),
    }
}

fn recorded(stem: &str, release: &str) -> Evaluation {
    let record = RELEASE_RECORDS
        .iter()
        .find(|r| r.release.to_string() == release)
        .unwrap();
    evaluate_recording(record, &recording(stem))
}

fn claude_record() -> &'static dot_agent_deck::doorbell_activation::ReleaseRecord<'static> {
    RELEASE_RECORDS
        .iter()
        .find(|r| r.release.to_string() == "2.1.312")
        .unwrap()
}

/// Replace evidence bytes and rebase indexes; do not add a test-only failure knob.
fn replace_bytes(trace: &mut RecordedCapture, start: usize, end: usize, bytes: &[u8]) {
    let shift = bytes.len() as i64 - (end - start) as i64;
    trace.bytes.splice(start..end, bytes.iter().copied());
    for point in trace.index["points"].as_object_mut().unwrap().values_mut() {
        let offset = point.as_u64().unwrap() as usize;
        if offset >= end {
            *point = json!((offset as i64 + shift) as usize);
        } else if offset > start {
            *point = json!(start + bytes.len());
        }
    }
    // One push deliberately removes read boundaries from the fact-only replay.
    // The oracle's timing tests above retain gap events separately.
    trace.index["read_lens"] = json!([trace.bytes.len()]);
    assert!(trace.index["edges"].as_array().unwrap().is_empty());
}

/// Scenario: Replay the Codex capture with its first header shifted one column to the right. The otherwise familiar launch fails its release_recogniser fact.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_evaluator_header_one_column_over() {
    let mut trace = recording("codex-inbox/r5wake/codex-0.164.0-composer-24x80");
    let record = RELEASE_RECORDS
        .iter()
        .find(|r| r.release.to_string() == "0.164.0")
        .unwrap();
    passed(&evaluate_recording(record, &trace));
    let at = trace
        .bytes
        .windows(b"OpenAI Codex".len())
        .position(|b| b == b"OpenAI Codex")
        .unwrap();
    replace_bytes(&mut trace, at, at, b"\x1b[C");
    differs(&evaluate_recording(record, &trace), "release_recogniser");
}

/// Scenario: Replay a truecolor user echo, then change its shaded RGB background from 55 to 56 in every channel. Only the original supplies the recorded transcript_echo fact.
#[spec("inbox/doorbell/047")]
#[test]
fn doorbell_047_evaluator_rgb_56_transcript() {
    let mut trace = recording("claude-inbox/r5wake/claude-2.1.312-truecolor-composer-24x80");
    passed(&evaluate_recording(claude_record(), &trace));
    let needle = b"48;2;55;55;55";
    let mut changed = 0;
    for pos in 0..=trace.bytes.len() - needle.len() {
        if &trace.bytes[pos..pos + needle.len()] == needle {
            trace.bytes[pos..pos + needle.len()].copy_from_slice(b"48;2;56;56;56");
            changed += 1;
        }
    }
    assert!(
        changed > 0,
        "fixture must contain the truecolor user background"
    );
    differs(
        &evaluate_recording(claude_record(), &trace),
        "transcript_echo",
    );
}

/// Scenario: Remove the bytes that draw the first typed doorbell line while keeping its trial index. The evaluator names the missing echo instead of accepting the other fourteen echoes.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_evaluator_missing_echo() {
    let mut trace = recording("claude-inbox/r5wake/claude-2.1.312-composer-24x80");
    let start = trace.index["points"]["mark:trial-1"].as_u64().unwrap() as usize;
    let end = trace.index["points"]["typed-01"].as_u64().unwrap() as usize;
    replace_bytes(&mut trace, start, end, &[]);
    differs(&evaluate_recording(claude_record(), &trace), "echo");
}

/// Scenario: Replace the first deleted Claude composer with its original placeholder-bearing empty screen. Readiness still holds, but the placeholder_empty capability disagrees with the recorded deletion behavior.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_evaluator_placeholder_after_delete() {
    let mut trace = recording("claude-inbox/r5wake/claude-2.1.312-composer-24x80");
    let empty = trace.index["points"]["empty"].as_u64().unwrap() as usize;
    let mut parser = dot_agent_deck::vt_grid::parser(24, 80, 0);
    parser.process(&trace.bytes[..empty]);
    let snapshot = parser.screen().contents_formatted();
    let grid = dot_agent_deck::doorbell_screen::Grid::of(parser.screen(), empty as u64);
    assert!(dot_agent_deck::doorbell_screen::CLAUDE_PROFILE.placeholder_empty(&grid));
    let start = trace.index["points"]["dtyped-01"].as_u64().unwrap() as usize;
    let end = trace.index["points"]["deleted-01"].as_u64().unwrap() as usize;
    replace_bytes(&mut trace, start, end, snapshot.as_bytes());
    differs(
        &evaluate_recording(claude_record(), &trace),
        "placeholder_empty",
    );
}

/// Scenario: Replay each key classification of the measured Codex dialogs, then make a formerly inert token letter commit in the update dialog. The evaluator names key_effects and retains that dialog and key as evidence.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_evaluator_committing_key() {
    let record = RELEASE_RECORDS
        .iter()
        .find(|r| r.release.to_string() == "0.164.0")
        .unwrap();
    let Measurement::Verified(reference) = record.key_effects else {
        panic!("verified fixture");
    };
    passed(&evaluate_key_effects(reference, reference));
    let dialog = reference.iter().find(|d| d.dialog == "update").unwrap();
    let mut keys = dialog.keys.to_vec();
    let (key, effect) = keys.iter_mut().find(|(key, _)| *key == b'd').unwrap();
    assert_eq!(*effect, KeyEffect::Inert);
    let key = *key;
    *effect = KeyEffect::Commits;
    let mut candidate = reference.to_vec();
    *candidate.iter_mut().find(|d| d.dialog == "update").unwrap() = DialogKeys {
        dialog: "update",
        keys: &keys,
    };
    let report = evaluate_key_effects(reference, &candidate);
    differs(&report, "key_effects");
    let Outcome::Differs { detail, .. } = report.outcome else {
        unreachable!()
    };
    assert!(detail.contains("update"));
    assert!(detail.contains(char::from(key)));
}

/// Scenario: Replay the shipped Claude and Codex composer captures against the facts their own records declare. Every captured fact agrees; these offline measurements grant no runtime authority.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_evaluator_shipped_captures_match_their_records() {
    for (agent, releases) in [
        ("claude", &["2.1.310", "2.1.312"][..]),
        ("codex", &["0.160.0", "0.164.0"][..]),
    ] {
        for release in releases {
            for suffix in if agent == "claude" {
                &["composer-24x80", "truecolor-composer-24x80"][..]
            } else {
                &["composer-24x80"][..]
            } {
                passed(&recorded(
                    &format!("{agent}-inbox/r5wake/{agent}-{release}-{suffix}"),
                    release,
                ));
            }
        }
    }
}

/// Scenario: Add a novel overlay above an otherwise familiar composer and then erase it within the same read. The evaluator retains the bad operation and reports transient_content.
#[spec("inbox/doorbell/060")]
#[test]
fn doorbell_060_overlay_within_one_read() {
    passed(&evaluate(transition("")));
    differs(
        &evaluate(transition(&control("overlay"))),
        "transient_content",
    );
}

/// Scenario: Deliver the same hidden overlay in one read, at the overlay boundary, and one byte at a time through its escape sequences. Every chunking reports the same transient_content fact.
#[spec("inbox/doorbell/060")]
#[test]
fn doorbell_060_overlay_rechunked_inside_escape() {
    let trace = transition(&control("overlay"));
    for width in [1, 7, sync(&control("before")).len(), usize::MAX] {
        differs(&evaluate(rechunk(&trace, width)), "transient_content");
    }
    let mut split = trace.clone();
    let bytes = trace["reads"][0]["bytes"].as_array().unwrap();
    let edge = bytes
        .iter()
        .position(|b| b.as_u64() == Some(u64::from(b'!')))
        .unwrap()
        + 1;
    split["reads"] = json!([
        {"at_ns": 0, "bytes": &bytes[..edge]},
        {"at_ns": 0, "bytes": &bytes[edge..]}
    ]);
    differs(&evaluate(split), "transient_content");
}

/// Scenario: Write a glyph from a neighbouring frame at a position where neither neighbour contains it, then erase it. Matching the glyph alphabet does not excuse the moved content.
#[spec("inbox/doorbell/060")]
#[test]
fn doorbell_060_neighbour_glyph_at_another_position() {
    differs(
        &evaluate(transition(&control("moved_glyph"))),
        "transient_content",
    );
}

/// Scenario: Partly erase the old frame before drawing the new one. The neighbour-cell and blank mixture passes as the explicitly accepted T-1 residual.
#[spec("inbox/doorbell/060")]
#[test]
fn doorbell_060_t1_partial_erase_residual() {
    passed(&evaluate(transition(&control("partial_erase"))));
}

/// Scenario: Clear the whole old screen and then redraw the reviewed new frame. Default blank cells are allowed during the redraw.
#[spec("inbox/doorbell/060")]
#[test]
fn doorbell_060_clear_then_redraw() {
    passed(&evaluate(transition(&control("clear"))));
}

/// Scenario: Erase a coloured row using the same background as its neighbouring frames, then redraw it. Those coloured blanks pass the transitional rule.
#[spec("inbox/doorbell/060")]
#[test]
fn doorbell_060_erase_under_coloured_background() {
    let mut oracle = reference();
    for template in oracle["templates"].as_array_mut().unwrap() {
        template["ansi"] = json!(format!(
            "\x1b[48;5;237m{}",
            template["ansi"].as_str().unwrap()
        ));
    }
    let bytes = format!(
        "{}{}",
        sync(&format!("\x1b[48;5;237m{}", control("before"))),
        sync(&format!("{}{}", control("partial_erase"), control("after")))
    );
    passed(&run(oracle, capture(&bytes), Limits::default()));
}

/// Scenario: Put two synchronized redraws in one read. Both boundaries count, and an unrecognised complete first redraw cannot disappear behind the second.
#[spec("inbox/doorbell/060")]
#[test]
fn doorbell_060_two_sync_blocks_in_one_read() {
    let good = evaluate(capture(&format!(
        "{}{}",
        sync(&control("before")),
        sync(&control("after"))
    )));
    passed(&good);
    assert_eq!(good.complete_frames, 2);
    differs(
        &evaluate(capture(&format!(
            "{}{}",
            sync("\x1b[HAB  \x1b[H"),
            sync(&control("after"))
        ))),
        "unrecognised_frame",
    );
}

/// Scenario: End a phase after opening a synchronized redraw but before completing it. Missing the next frame is inconclusive rather than a pass.
#[spec("inbox/doorbell/060")]
#[test]
fn doorbell_060_phase_ends_mid_transition() {
    inconclusive(
        &evaluate(capture(&format!(
            "{}{SYNC_ON}\x1b[H",
            sync(&control("before"))
        ))),
        "unterminated frame",
    );
}

/// Scenario: Enable mouse reporting and restore it inside one otherwise valid redraw. The undeclared transient mode still fails.
#[spec("inbox/doorbell/060")]
#[test]
fn doorbell_060_undeclared_mode_restored() {
    let mut declared = reference();
    declared["transient_modes"]
        .as_array_mut()
        .unwrap()
        .push(json!(1000));
    passed(&run(
        declared,
        transition(&control("mode")),
        Limits::default(),
    ));
    differs(&evaluate(transition(&control("mode"))), "mode_transient");
}

/// Scenario: Send an OSC 52 clipboard control between reviewed frames. Its lack of visible grid changes does not make the undeclared control acceptable.
#[spec("inbox/doorbell/060")]
#[test]
fn doorbell_060_osc52_control() {
    let mut declared = reference();
    declared["allowed_controls"] = json!([control("clipboard")]);
    passed(&run(
        declared,
        transition(&control("clipboard")),
        Limits::default(),
    ));
    differs(
        &evaluate(transition(&control("clipboard"))),
        "unrecognised_control",
    );
}

/// Scenario: Draw the future frame's top row, scroll that new row into history and redraw. Its cells are allowed, but the discarded line is neither old history nor declared history.
#[spec("inbox/doorbell/060")]
#[test]
fn doorbell_060_undeclared_history_line() {
    let mut declared = reference();
    declared["history_lines"] = json!(["ABXY"]);
    passed(&run(
        declared,
        transition(&control("scroll_future")),
        Limits::default(),
    ));
    differs(
        &evaluate(transition(&control("scroll_future"))),
        "unrecognised_history",
    );
}

/// Scenario: Scroll a previous frame's second row to the top and then replace it with the next frame's different row. The intermediate shifted cells pass because the old frame is adjusted by the actual scroll.
#[spec("inbox/doorbell/060")]
#[test]
fn doorbell_060_scroll_adjusts_previous_frame() {
    let mut oracle = reference();
    let before = "\x1b[2;1HABCD\x1b[H";
    let after = "\x1b[HWXYZ\x1b[H";
    oracle["templates"][0]["ansi"] = json!(before);
    oracle["templates"][1]["ansi"] = json!(after);
    let bytes = format!("{}{SYNC_ON}\x1b[4;1H\n{after}{SYNC_OFF}", sync(before));
    passed(&run(oracle, capture(&bytes), Limits::default()));
}

/// Scenario: Pause at a partly erased screen for a quiet gap, then finish the redraw. The intermediate neighbour-cell screen is accepted and counted as one stall.
#[spec("inbox/doorbell/060")]
#[test]
fn doorbell_060_quiet_gap_stall_counted() {
    let report = evaluate(quiet(
        &control("partial_erase"),
        &[(6, 50_000_000), (9, 50_000_000), (15, 50_000_000)],
    ));
    passed(&report);
    assert_eq!(report.stalls, 1);
}

/// Scenario: Fill an unfinished transition past each replay limit using harmless cursor operations. A byte overflow and an operation overflow each report limit: transition.
#[spec("inbox/doorbell/060")]
#[test]
fn doorbell_060_transition_limits() {
    let trace = transition(&"\x1b[H".repeat(20));
    for limits in [
        Limits {
            transition_bytes: 64,
            transition_operations: 100_000,
        },
        Limits {
            transition_bytes: 1_048_576,
            transition_operations: 10,
        },
    ] {
        inconclusive(
            &run(reference(), trace.clone(), limits),
            "limit: transition",
        );
    }
}

/// Scenario: Leave a partial-erase mixture visible for ten times the quiet gap before finishing. The accepted T-1 stall is counted and reported explicitly despite its duration.
#[spec("inbox/doorbell/060")]
#[test]
fn doorbell_060_long_t1_stall_reported() {
    let report = evaluate(quiet(
        &control("partial_erase"),
        &[(6, 50_000_000), (9, 500_000_000), (15, 50_000_000)],
    ));
    passed(&report);
    assert_eq!(report.stalls, 1);
    assert_eq!(report.accepted_t1_stalls, 1);
}

/// Scenario: Leave a novel cell on screen across a quiet gap before erasing it and completing the redraw. A pause cannot turn that cell into an accepted stall.
#[spec("inbox/doorbell/060")]
#[test]
fn doorbell_060_non_neighbour_past_quiet_gap() {
    // before=6 ops, CUP + print=2; erase + home=2; after=6.
    let middle = "\x1b[3;1H!\x1b[2K\x1b[H";
    differs(
        &evaluate(quiet(
            middle,
            &[(6, 50_000_000), (8, 500_000_000), (16, 50_000_000)],
        )),
        "transient_content",
    );
}

/// Scenario: Pause three times in one redraw, each pause shorter than the operation budget by itself. The pending evidence spans all stalls and eventually exceeds the shared transition budget.
#[spec("inbox/doorbell/060")]
#[test]
fn doorbell_060_stalls_share_transition_bound() {
    let middle = control("partial_erase").repeat(3);
    let trace = quiet(
        &middle,
        &[
            (6, 50_000_000),
            (9, 50_000_000),
            (12, 50_000_000),
            (15, 50_000_000),
            (21, 50_000_000),
        ],
    );
    inconclusive(
        &run(
            reference(),
            trace,
            Limits {
                transition_bytes: 1_048_576,
                transition_operations: 12,
            },
        ),
        "limit: transition",
    );
}

fn timing_reference() -> Value {
    let mut oracle = reference();
    oracle["templates"].as_array_mut().unwrap().push(json!({
        "name": "erased", "phase": "idle", "ansi": "\x1b[HAB  \x1b[H"
    }));
    oracle
}

/// Scenario: Rechunk a redraw without changing its gap events and then add a gap at its erased intermediate template. Rechunking preserves the verdict; the added frame boundary changes which cells a later restore may draw.
#[spec("inbox/doorbell/060")]
#[test]
fn doorbell_060_timing_preserving_rechunk_and_changed_gap_control() {
    let middle = format!(
        "{}{}",
        control("partial_erase"),
        control("restore_previous")
    );
    let trace = quiet(&middle, &[(6, 50_000_000), (19, 50_000_000)]);
    for width in [1, 7, usize::MAX] {
        passed(&run(
            timing_reference(),
            rechunk(&trace, width),
            Limits::default(),
        ));
    }
    let changed = quiet(
        &middle,
        &[(6, 50_000_000), (9, 50_000_000), (19, 50_000_000)],
    );
    differs(
        &run(timing_reference(), changed, Limits::default()),
        "transient_content",
    );
}

/// Scenario: Read at 0, 100, 140 and 180 milliseconds, so merging the middle reads under the first timestamp would invent an 80-millisecond gap. Preserve the original operation-indexed gaps when merging; only the deliberately invented gap changes the verdict.
#[spec("inbox/doorbell/060")]
#[test]
fn doorbell_060_timestamp_merge_must_not_invent_gap() {
    let mut trace = quiet("", &[(6, 100_000_000), (20, 50_000_000)]);
    trace["reads"] = json!([
        {"at_ns": 0, "bytes": control("before").as_bytes()},
        {"at_ns": 100_000_000, "bytes": control("partial_erase").as_bytes()},
        {"at_ns": 140_000_000, "bytes": b"\x1b[H"},
        {"at_ns": 180_000_000, "bytes": format!("{}{}", control("restore_previous"), control("after")).as_bytes()}
    ]);
    let at = |i: usize| trace["reads"][i]["at_ns"].as_u64().unwrap();
    assert!(at(3) - at(2) < 50_000_000);
    assert!(at(3) - at(1) >= 50_000_000);
    passed(&run(timing_reference(), trace.clone(), Limits::default()));
    let merged = rechunk(&trace, usize::MAX);
    assert_eq!(merged["gap_events"], trace["gap_events"]);
    passed(&run(timing_reference(), merged, Limits::default()));
    trace["gap_events"]
        .as_array_mut()
        .unwrap()
        .insert(1, json!({"after_operation": 10, "elapsed_ns": 80_000_000}));
    differs(
        &run(timing_reference(), trace, Limits::default()),
        "transient_content",
    );
}

/// Scenario: Keep the old composer frame and add a new overlay region at a completed redraw. A familiar composer alone cannot make the whole screen recognised.
#[spec("inbox/doorbell/060")]
#[test]
fn doorbell_060_old_composer_new_overlay_region() {
    let bytes = format!(
        "{}{}",
        sync(&control("before")),
        sync(&format!("{}{}", control("before"), control("new_modal")))
    );
    differs(&evaluate(capture(&bytes)), "unrecognised_frame");
}

/// Scenario: Keep a known transcript row and add a modal below it. The unchanged transcript does not permit the new modal region.
#[spec("inbox/doorbell/060")]
#[test]
fn doorbell_060_old_transcript_new_modal_region() {
    let mut oracle = reference();
    oracle["templates"][0]["phase"] = json!("submitted");
    let mut trace = capture(&sync(&format!(
        "{}{}",
        control("before"),
        control("new_modal")
    )));
    trace["phase"] = json!("submitted");
    differs(&run(oracle, trace, Limits::default()), "unrecognised_frame");
}

/// Scenario: Offer an extra complete dialog not among the reviewed idle templates. It is an unrecognised frame even though it appears at a valid boundary.
#[spec("inbox/doorbell/060")]
#[test]
fn doorbell_060_extra_dialog() {
    differs(
        &evaluate(capture(&sync(&control("new_modal")))),
        "unrecognised_frame",
    );
}

/// Scenario: Duplicate an allowed template and replay a frame that matches both copies. The evaluator reports ambiguous_frame instead of choosing one.
#[spec("inbox/doorbell/060")]
#[test]
fn doorbell_060_ambiguous_frame() {
    let mut oracle = reference();
    let duplicate = oracle["templates"][0].clone();
    oracle["templates"].as_array_mut().unwrap().push(duplicate);
    differs(
        &run(
            oracle,
            capture(&sync(&control("before"))),
            Limits::default(),
        ),
        "ambiguous_frame",
    );
}
