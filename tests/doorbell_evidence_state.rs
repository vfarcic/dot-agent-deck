//! `inbox/doorbell/044`: the evidence state over a real Codex layout trace
//! (`src/evidence_state.rs`).
use std::path::PathBuf;

use dot_agent_deck::doorbell_screen::{CLAUDE_PROFILE, CODEX_PROFILE, ScreenProfile};
use dot_agent_deck::evidence_state::{
    AGREEMENT_TWINS_MAX, CleanBasis, EdgeReason, EvidenceState, EvidenceStatus,
};
use spec::spec;

/// A recorded Codex layout run: its bytes, where each PTY read ended, its
/// resizes and its screenshots.
struct Run {
    bytes: Vec<u8>,
    rows: u16,
    cols: u16,
    read_ends: Vec<usize>,
    edges: Vec<(usize, u16, u16)>,
    index: serde_json::Value,
}

fn run(stem: &str) -> Run {
    run_in("codex-inbox", stem)
}

fn run_in(dir: &str, stem: &str) -> Run {
    run_at(&format!("{dir}/r5wake"), stem)
}

/// A recorded run under `tests/fixtures/<dir>`.
fn run_at(dir: &str, stem: &str) -> Run {
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(dir)
        .join(stem);
    let bytes = std::fs::read(format!("{}.bin", base.display())).unwrap();
    let index: serde_json::Value =
        serde_json::from_slice(&std::fs::read(format!("{}.json", base.display())).unwrap())
            .unwrap();
    let mut end = 0;
    let read_ends = index["read_lens"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| {
            end += v.as_u64().unwrap() as usize;
            end
        })
        .collect();
    let edges = index["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["off"].as_u64().unwrap() as usize,
                e["rows"].as_u64().unwrap() as u16,
                e["cols"].as_u64().unwrap() as u16,
            )
        })
        .collect();
    Run {
        rows: index["rows"].as_u64().unwrap() as u16,
        cols: index["cols"].as_u64().unwrap() as u16,
        bytes,
        read_ends,
        edges,
        index,
    }
}

impl Run {
    fn point(&self, name: &str) -> usize {
        self.index["points"][name].as_u64().unwrap() as usize
    }

    /// Push the bytes from `*pos` to `end` as the PTY reader did, read by
    /// read, a read cut where `end` falls inside it.
    fn feed(&self, state: &mut EvidenceState, pos: &mut usize, end: usize) {
        while *pos < end {
            let next = self
                .read_ends
                .iter()
                .copied()
                .find(|&e| e > *pos)
                .unwrap_or(end)
                .min(end);
            state.push(&self.bytes[*pos..next], |_| Vec::new());
            *pos = next;
        }
    }

    /// The first full-redraw marker at or after `from`, before `to`.
    fn marker_after(&self, from: usize, to: usize) -> Option<usize> {
        let marker = CODEX_PROFILE.redraw_marker;
        self.bytes[from..to]
            .windows(marker.len())
            .position(|w| w == marker)
            .map(|p| from + p)
    }
}

/// Scenario: Replay Codex 0.164.0's recorded start-up, ten resizes and ten one-column nudges through an evidence state, each edge at its recorded place in the stream and the bytes pushed read by read as the PTY returned them. The state turns Clean at start-up once the centred logo shows, every edge turns it Dirty, it is Clean again at each edge's screenshot five seconds later, and at least one nudge's first redraw after the restore, drawn for the narrower width, leaves it pending rather than Clean.
#[spec("inbox/doorbell/044")]
#[test]
fn doorbell_044_codex_layout_trace_through_the_evidence_state() {
    let run = run("codex-0.164.0-layout-start-50x200");
    let mut state = EvidenceState::new(Some(&CODEX_PROFILE), run.rows, run.cols);
    let mut pos = 0;
    run.feed(&mut state, &mut pos, run.point("empty"));
    assert!(
        matches!(state.status(), EvidenceStatus::Clean { .. }),
        "start-up: {:?}",
        state.status()
    );
    let mut cols = run.cols;
    let mut stale_first_redraws = 0;
    for (i, &(off, r, c)) in run.edges.iter().enumerate() {
        run.feed(&mut state, &mut pos, off);
        let reason = if c + 1 == cols {
            EdgeReason::NudgeShrink
        } else if cols + 1 == c {
            EdgeReason::NudgeRestore
        } else {
            EdgeReason::Resize
        };
        state.edge(reason, r, c);
        assert!(
            matches!(state.status(), EvidenceStatus::Dirty { .. }),
            "edge {i}"
        );
        cols = c;
        let next_edge = run.edges.get(i + 1).map(|e| e.0).unwrap_or(run.bytes.len());
        if reason == EdgeReason::NudgeRestore
            && let Some(first) = run.marker_after(off, next_edge)
        {
            // Through the first redraw after the restore, up to the next one.
            let second = run.marker_after(first + 1, next_edge).unwrap_or(next_edge);
            run.feed(&mut state, &mut pos, second);
            if matches!(state.status(), EvidenceStatus::Pending { .. }) {
                stale_first_redraws += 1;
            }
        }
    }
    run.feed(&mut state, &mut pos, run.bytes.len());
    assert!(matches!(state.status(), EvidenceStatus::Clean { .. }));
    assert!(
        stale_first_redraws >= 1,
        "no nudge's first redraw was drawn for the narrower width"
    );
}

/// Scenario: Replay the same recorded run, and at each edge's screenshot ask the evidence state whether it is Clean. Every one of the twenty edges (ten resizes, ten nudges) is Clean again by its screenshot.
#[spec("inbox/doorbell/044")]
#[test]
fn doorbell_044_each_recorded_edge_ends_clean() {
    let run = run("codex-0.164.0-layout-start-50x200");
    let mut state = EvidenceState::new(Some(&CODEX_PROFILE), run.rows, run.cols);
    let mut pos = 0;
    let mut shots: Vec<(usize, String)> = run.index["points"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(k, _)| {
            (k.starts_with("resize-") || k.starts_with("nudge-")) && !k.starts_with("mark:")
        })
        .map(|(k, v)| (v.as_u64().unwrap() as usize, k.clone()))
        .collect();
    shots.sort();
    assert_eq!(shots.len(), 20);
    let mut edges = run.edges.iter().peekable();
    for (shot, name) in shots {
        while let Some(&&(off, r, c)) = edges.peek() {
            if off >= shot {
                break;
            }
            run.feed(&mut state, &mut pos, off);
            state.edge(EdgeReason::Resize, r, c);
            edges.next();
        }
        run.feed(&mut state, &mut pos, shot);
        assert!(
            matches!(state.status(), EvidenceStatus::Clean { .. }),
            "{name}: {:?}",
            state.status()
        );
    }
}

/// Codex's full-redraw marker.
const CODEX_REDRAW: &str = "\x1b[?2026h\x1b[1;1H\x1b[J";

/// `text`, then written blanks up to `cols`: a row padded to the edge of a
/// terminal `cols` wide.
fn padded(text: &str, cols: u16) -> String {
    format!("{text}{}", " ".repeat(cols as usize - text.chars().count()))
}

/// The bytes of a Codex redraw up to its padded row, the row itself (drawn
/// for a terminal `cols` wide) and what follows it. Codex's recorded frames
/// follow the row with a cursor move to the next thing they draw.
type Frame = (String, String, String);

/// A Codex redraw for a terminal `cols` wide: a dialog's highlighted choice
/// (bold, inverse) padded to its edge, then the next choice.
fn codex_choice_parts(cols: u16) -> Frame {
    (
        format!("{CODEX_REDRAW}\x1b[2;3H\x1b[1mFolder access\x1b[0m\x1b[8;1H\x1b[1;7m"),
        padded("› 1. Trust and continue", cols),
        "\x1b[0m\x1b[9;3H2. Back".to_string(),
    )
}

/// A Codex redraw for a terminal `cols` wide: the dim row under a finished
/// turn, padded to its edge, then the composer's row.
fn codex_turn_parts(cols: u16) -> Frame {
    (
        format!("{CODEX_REDRAW}\x1b[12;1H\x1b[2m"),
        padded("  Worked for 3s • 7:03 PM", cols),
        "\x1b[0m\x1b[44;1H ".to_string(),
    )
}

fn whole((head, row, tail): Frame) -> Vec<u8> {
    format!("{head}{row}{tail}").into_bytes()
}

fn codex_choice(cols: u16) -> Vec<u8> {
    whole(codex_choice_parts(cols))
}

fn codex_turn(cols: u16) -> Vec<u8> {
    whole(codex_turn_parts(cols))
}

/// A Codex start-up redraw for a terminal `drawn` columns wide: the logo's
/// top row at `drawn / 2 - 5`.
fn codex_logo(drawn: u16) -> Vec<u8> {
    format!("{CODEX_REDRAW}\x1b[17;{}H⣿⣿⣿⣿⣿⣿", drawn / 2 - 5 + 1).into_bytes()
}

/// A Claude Code redraw: the full-redraw marker and an empty composer
/// between two rules `rule` cells long.
fn claude_frame_parts(rule: u16) -> Frame {
    let rule = "─".repeat(rule as usize);
    (
        "\x1b[2J\x1b[H\x1b[20;1H".to_string(),
        rule.clone(),
        format!("\x1b[21;1H❯\u{a0}\x1b[22;1H{rule}\x1b[23;3Hstatus\x1b[21;3H"),
    )
}

fn claude_frame(rule: u16) -> Vec<u8> {
    whole(claude_frame_parts(rule))
}

fn clean(state: &EvidenceState) -> bool {
    matches!(state.status(), EvidenceStatus::Clean { .. })
}

/// Codex's profile with the agreement rule off (issue #2262), for the tests
/// of what Codex's width and pair witnesses alone decide: with it on, a
/// frame those witnesses refuse can still be Clean on agreement, and the
/// agreement tests below say when.
fn codex_witness_only() -> &'static ScreenProfile {
    static PROFILE: std::sync::OnceLock<ScreenProfile> = std::sync::OnceLock::new();
    PROFILE.get_or_init(|| ScreenProfile {
        agreement: false,
        ..CODEX_PROFILE
    })
}

fn pending_after_edge(state: &EvidenceState) -> bool {
    matches!(
        state.status(),
        EvidenceStatus::Pending {
            after_edge: Some(_),
            ..
        }
    )
}

/// Scenario: Shrink a Codex terminal and a Claude Code terminal, then push each agent's full redraw drawn for the old, wider width: a dialog's padded choice and Codex's padded turn row, and Claude Code's rule one cell longer than the new width (the 81-cell rule after an edge to 80). Each stays pending: a padded row drawn wider runs past the new edge. The redraw drawn for the width in force turns each Clean.
#[spec("inbox/doorbell/044")]
#[test]
fn doorbell_044_a_redraw_for_a_wider_terminal_stays_pending() {
    for (stale, fresh) in [
        (codex_choice(200), codex_choice(180)),
        (codex_turn(200), codex_turn(180)),
        (codex_choice(181), codex_turn(180)),
    ] {
        let mut state = EvidenceState::new(Some(&CODEX_PROFILE), 50, 200);
        state.push(&codex_choice(200), |_| Vec::new());
        assert!(clean(&state), "the control at the spawn width");
        state.edge(EdgeReason::Resize, 50, 180);
        state.push(&stale, |_| Vec::new());
        assert!(pending_after_edge(&state), "{:?}", state.status());
        state.push(&fresh, |_| Vec::new());
        assert!(clean(&state), "{:?}", state.status());
    }
    let mut state = EvidenceState::new(Some(&CLAUDE_PROFILE), 24, 81);
    state.push(&claude_frame(81), |_| Vec::new());
    assert!(clean(&state));
    state.edge(EdgeReason::Resize, 24, 80);
    state.push(&claude_frame(81), |_| Vec::new());
    assert!(pending_after_edge(&state), "{:?}", state.status());
    state.push(&claude_frame(80), |_| Vec::new());
    assert!(clean(&state));
}

/// Scenario: Codex's start-up logo sits at the same column for the two widths 2k and 2k + 1. Move a terminal from 201 to 200 columns (the even width), and another from 200 to 201 (the odd width), and push a logo drawn for each width: every one turns the state Clean on the pair witness (issue #2124), since it reads the same at either width. A terminal that has only ever been 200 or only ever 201 columns wide is Clean on its own logo as a width witness, and one moved from 200 to 198, a width that does not share the logo's column, refuses the 200-column logo on the witnesses alone (with the agreement rule off) and takes its own.
#[spec("inbox/doorbell/044")]
#[test]
fn doorbell_044_a_logo_two_adjacent_widths_share_is_clean_on_the_pair_witness() {
    for (spawned, now) in [(201, 200), (200, 201)] {
        for drawn in [spawned, now] {
            let mut state = EvidenceState::new(Some(&CODEX_PROFILE), 50, spawned);
            state.edge(EdgeReason::Resize, 50, now);
            state.push(&codex_logo(drawn), |_| Vec::new());
            assert!(
                clean(&state) && state.pair_held(),
                "{spawned} → {now}, drawn for {drawn}: {:?}",
                state.status()
            );
        }
    }
    for cols in [200, 201] {
        let mut state = EvidenceState::new(Some(&CODEX_PROFILE), 50, cols);
        state.push(&codex_logo(cols), |_| Vec::new());
        assert!(clean(&state), "only ever {cols} wide");
        assert!(!state.pair_held(), "only ever {cols} wide");
    }
    let mut state = EvidenceState::new(Some(codex_witness_only()), 50, 200);
    state.edge(EdgeReason::Resize, 50, 198);
    state.push(&codex_logo(200), |_| Vec::new());
    assert!(pending_after_edge(&state));
    state.push(&codex_logo(198), |_| Vec::new());
    assert!(clean(&state));
    assert!(!state.pair_held());
}

/// Scenario: Reproduce issue #2124's pane: a Codex generation started at 180 columns whose pane went to 117, then 116, then 117 again, as an orchestration's layout can, and push Codex's start-up logo drawn for 117 columns. The state is Clean on the pair witness, where before it stayed pending until the agent's first prompt. The same holds with the pane left at 116 after 117.
#[spec("inbox/doorbell/044")]
#[test]
fn doorbell_044_a_fresh_codex_at_117_columns_after_116_is_clean_on_its_logo() {
    for (path, now) in [([117, 116, 117], 117), ([116, 117, 116], 116)] {
        let mut state = EvidenceState::new(Some(&CODEX_PROFILE), 50, 180);
        for cols in path {
            state.edge(EdgeReason::Resize, 50, cols);
        }
        assert_eq!(state.widths(), (now, vec![116, 117, 180]));
        state.push(&codex_logo(now), |_| Vec::new());
        assert!(
            clean(&state) && state.pair_held(),
            "{path:?}: {:?}",
            state.status()
        );
    }
}

/// Scenario: With the terminal having had both 200 and 201 columns, push Codex's logo together with writes that a terminal 200 wide and one 201 wide would show differently: a character at column 200 (clamped into column 199 at 200 wide), a wide character from column 199 (which only the wider terminal fits there), a move past the edge followed by a move back, an erase to the end of the row and a character (Qodo's review of the first version, which only watched the last column), and text across 201 columns. Each stays pending, at 200 and at 201 columns, and so does a later logo that reads the same at both: the pair witness is spent until the next layout edge. A width witness, Codex's padded turn row drawn for the width in force, still turns it Clean.
#[spec("inbox/doorbell/044")]
#[test]
fn doorbell_044_a_logo_that_reads_differently_at_the_two_widths_stays_pending() {
    let across = |cols: usize| format!("\x1b[30;1H{}", "x".repeat(cols));
    let writes = [
        "\x1b[30;201Hx".to_string(),
        "\x1b[30;200H\u{4e2d}".to_string(),
        "\x1b[30;201H\x1b[D\x1b[KX".to_string(),
        across(201),
    ];
    for (spawned, now) in [(201, 200), (200, 201)] {
        for write in &writes {
            let mut state = EvidenceState::new(Some(&CODEX_PROFILE), 50, spawned);
            state.edge(EdgeReason::Resize, 50, now);
            let mut frame = codex_logo(now);
            frame.extend_from_slice(write.as_bytes());
            frame.extend_from_slice(b"\x1b[47;3H");
            state.push(&frame, |_| Vec::new());
            assert!(
                pending_after_edge(&state),
                "{spawned} → {now}, {write:?}: {:?}",
                state.status()
            );
            state.push(&codex_logo(now), |_| Vec::new());
            assert!(pending_after_edge(&state), "{spawned} → {now}, {write:?}");
            state.push(&codex_turn(now), |_| Vec::new());
            assert!(clean(&state) && !state.pair_held(), "{spawned} → {now}");
        }
    }
}

/// Scenario: Turn a 201-to-200-column Codex state Clean on its logo (the pair witness), push a write both widths show alike and see it stay Clean, then push a move past the edge, a move back, an erase to the end of the row and a character, which the two widths place in different columns: the state returns to pending, so a frame read there no longer stands. A second state, Clean on the logo the same way, gets Codex's padded turn row drawn for 200 columns, a width witness: it stays Clean, now on the width witness.
#[spec("inbox/doorbell/044")]
#[test]
fn doorbell_044_clean_on_the_pair_witness_ends_when_the_readings_differ() {
    let mut state = EvidenceState::new(Some(&CODEX_PROFILE), 50, 201);
    state.edge(EdgeReason::Resize, 50, 200);
    state.push(&codex_logo(200), |_| Vec::new());
    assert!(clean(&state) && state.pair_held());
    state.push(b"\x1b[47;3Hhello", |_| Vec::new());
    assert!(
        clean(&state) && state.pair_held(),
        "a write both widths show alike"
    );
    state.push(b"\x1b[30;201H\x1b[D\x1b[KX\x1b[47;8H", |_| Vec::new());
    assert!(pending_after_edge(&state), "{:?}", state.status());
    assert!(!state.pair_held());
    assert!(state.clean_frame().is_none());

    let mut state = EvidenceState::new(Some(&CODEX_PROFILE), 50, 201);
    state.edge(EdgeReason::Resize, 50, 200);
    state.push(&codex_logo(200), |_| Vec::new());
    assert!(clean(&state) && state.pair_held());
    state.push(&codex_turn(200), |_| Vec::new());
    assert!(clean(&state), "{:?}", state.status());
    assert!(!state.pair_held());
}

/// Scenario: Replay each of Codex 0.164.0's recorded redraws after an edge to an even width W (its ten resizes, to 160, 120, 100, 90, 180, 140, 110, 170, 130 and 200 columns, and its ten nudges' restores to 200), from the edge to the next one, through two evidence states that have both had W and W + 1: one now W columns wide, the width the redraw was drawn for, and one now W + 1 wide, reading the same bytes one column wider. Both end Clean on the pair witness, and every row reads the same in both.
#[spec("inbox/doorbell/044")]
#[test]
fn doorbell_044_recorded_redraws_read_the_same_at_both_widths_of_the_pair() {
    let run = run("codex-0.164.0-layout-start-50x200");
    let mut seen = 0;
    for (i, &(off, rows, w)) in run.edges.iter().enumerate() {
        let end = run.edges.get(i + 1).map(|e| e.0).unwrap_or(run.bytes.len());
        // Edges to an even width only: each recorded nudge shrinks to 199
        // columns and is restored four bytes later.
        if w % 2 != 0 {
            continue;
        }
        let read = |spawned: u16, now: u16| {
            let mut state = EvidenceState::new(Some(&CODEX_PROFILE), rows, spawned);
            state.edge(EdgeReason::Resize, rows, now);
            let mut pos = off;
            run.feed(&mut state, &mut pos, end);
            assert!(
                clean(&state) && state.pair_held(),
                "resize to {w}, read at {now}: {:?}",
                state.status()
            );
            state.clean_frame().unwrap()
        };
        let (drawn, wider) = (read(w + 1, w), read(w, w + 1));
        for r in 0..rows {
            assert_eq!(
                drawn.text_from(r, 0),
                wider.text_from(r, 0),
                "resize to {w}, row {r}"
            );
        }
        seen += 1;
    }
    assert_eq!(seen, 20);
}

/// Scenario: Push redraws whose rows reach the right edge without being a witness: for Codex, read on its witnesses alone, plain text exactly as wide as the terminal, plain text padded to the edge, the dim turn row's words run to the edge with no padding, and a numbered choice padded to the edge but not highlighted; for Claude Code, a row of other characters across the width and a rule with one cell broken. Each leaves the state pending, and still pending after a carriage return or a cursor move ends the row; a real witness pushed after them turns it Clean.
#[spec("inbox/doorbell/044")]
#[test]
fn doorbell_044_text_reaching_the_edge_is_not_a_witness() {
    let cols = 200;
    let mut state = EvidenceState::new(Some(codex_witness_only()), 50, cols);
    for row in [
        "x".repeat(cols as usize),
        padded("  hello", cols),
        format!(
            "\x1b[2m  Worked for {}\x1b[0m",
            "x".repeat(cols as usize - 13)
        ),
        padded("› 1. Trust and continue", cols),
    ] {
        state.push(format!("{CODEX_REDRAW}\x1b[5;1H{row}").as_bytes(), |_| {
            Vec::new()
        });
        assert!(
            matches!(state.status(), EvidenceStatus::Pending { .. }),
            "{row:?}"
        );
        // The row has ended once the cursor leaves it, so from here on only
        // the row's shape keeps it from being a witness.
        for tail in ["\r", "\x1b[45;1H"] {
            state.push(tail.as_bytes(), |_| Vec::new());
            assert!(
                matches!(state.status(), EvidenceStatus::Pending { .. }),
                "{row:?} then {tail:?}"
            );
        }
    }
    state.push(&codex_turn(cols), |_| Vec::new());
    assert!(clean(&state));

    let mut state = EvidenceState::new(Some(&CLAUDE_PROFILE), 24, 80);
    let broken = format!("{}x{}", "─".repeat(40), "─".repeat(39));
    for row in ["=".repeat(80), broken] {
        state.push(format!("\x1b[2J\x1b[H\x1b[20;1H{row}").as_bytes(), |_| {
            Vec::new()
        });
        assert!(
            matches!(state.status(), EvidenceStatus::Pending { .. }),
            "{row:?}"
        );
        for tail in ["\r\n", "\x1b[22;1H"] {
            state.push(tail.as_bytes(), |_| Vec::new());
            assert!(
                matches!(state.status(), EvidenceStatus::Pending { .. }),
                "{row:?} then {tail:?}"
            );
        }
    }
    state.push(&claude_frame(80), |_| Vec::new());
    assert!(clean(&state));
}

/// The stale frames a shrink can leave behind, each with the profile, the
/// width the terminal had and the width it has now: Claude Code's rule one
/// cell and twenty cells too long, and Codex's choice and turn rows padded one
/// column and twenty columns too wide, read on Codex's witnesses alone
/// ([`codex_witness_only`]; the agreement rule's own stale-frame test is
/// `doorbell_044_agreement_ends_at_the_first_reading_that_differs`).
fn stale_frames() -> Vec<(&'static str, &'static ScreenProfile, u16, u16, Frame, Frame)> {
    let mut out: Vec<(&'static str, &'static ScreenProfile, u16, u16, Frame, Frame)> = Vec::new();
    for (old, new) in [(81, 80), (200, 180)] {
        out.push((
            "claude rule",
            &CLAUDE_PROFILE,
            old,
            new,
            claude_frame_parts(old),
            claude_frame_parts(new),
        ));
    }
    for (old, new) in [(181, 180), (200, 180)] {
        out.push((
            "codex choice",
            codex_witness_only(),
            old,
            new,
            codex_choice_parts(old),
            codex_choice_parts(new),
        ));
        out.push((
            "codex turn",
            codex_witness_only(),
            old,
            new,
            codex_turn_parts(old),
            codex_turn_parts(new),
        ));
    }
    out
}

/// A state of `profile` spawned `old` columns wide, Clean on its own frame,
/// then shrunk to `new`.
fn shrunk(
    profile: &'static ScreenProfile,
    rows: u16,
    old: u16,
    new: u16,
    own: &[u8],
) -> EvidenceState {
    let mut state = EvidenceState::new(Some(profile), rows, old);
    state.push(own, |_| Vec::new());
    assert!(clean(&state), "the control at the spawn width");
    state.edge(EdgeReason::Resize, rows, new);
    state
}

/// Push `chunks` in order, asserting after each push that the state is not
/// Clean and gives no clean frame.
fn never_clean<'a>(
    state: &mut EvidenceState,
    chunks: impl IntoIterator<Item = &'a [u8]>,
    what: &str,
) {
    for (i, chunk) in chunks.into_iter().enumerate() {
        state.push(chunk, |_| Vec::new());
        assert!(
            !clean(state) && state.clean_frame().is_none(),
            "{what}: Clean after push {i}: {:?}",
            state.status()
        );
    }
}

/// Scenario: Shrink a Claude Code terminal and a Codex terminal (read on Codex's witnesses alone), then push each agent's redraw drawn for the old, wider width with the PTY read cut right after the last cell that fits the new width: Claude Code's rule, and Codex's padded choice and turn rows, one column and twenty columns too wide. At the cut the row looks exactly like the one drawn for the new width, but the state stays pending there, after the rest of the stale redraw arrives, and when the redraw is pushed one byte at a time or cut at any other single place. The redraw drawn for the width in force, pushed whole, one byte at a time or cut anywhere, turns it Clean.
#[spec("inbox/doorbell/044")]
#[test]
fn doorbell_044_a_wider_row_cut_at_the_new_edge_is_never_clean() {
    let rows = 50;
    for (what, profile, old, new, stale, fresh) in stale_frames() {
        let own = match what {
            "claude rule" => claude_frame(old),
            _ => codex_choice(old),
        };
        let bytes = whole(stale.clone());
        let fits: String = stale.1.chars().take(new as usize).collect();
        let cut = stale.0.len() + fits.len();
        assert!(
            bytes.len() > cut && stale.1.chars().count() > new as usize,
            "{what} {old} → {new}: the stale row is wider than the new edge"
        );

        // The reviewers' cut: right after the last cell that fits.
        let mut state = shrunk(profile, rows, old, new, &own);
        never_clean(
            &mut state,
            [&bytes[..cut], &bytes[cut..]],
            &format!("{what} {old} → {new}, cut at the edge"),
        );
        assert!(pending_after_edge(&state), "{:?}", state.status());

        // One byte per push.
        let mut state = shrunk(profile, rows, old, new, &own);
        never_clean(
            &mut state,
            bytes.chunks(1),
            &format!("{what} {old} → {new}, a byte at a time"),
        );

        // Every other single cut.
        for at in 1..bytes.len() {
            let mut state = shrunk(profile, rows, old, new, &own);
            never_clean(
                &mut state,
                [&bytes[..at], &bytes[at..]],
                &format!("{what} {old} → {new}, cut at {at}"),
            );
        }

        // The redraw for the width in force, after the stale one: Clean however
        // it is cut.
        let fresh = whole(fresh);
        let mut state = shrunk(profile, rows, old, new, &own);
        state.push(&bytes, |_| Vec::new());
        state.push(&fresh, |_| Vec::new());
        assert!(clean(&state), "{what} {new}: {:?}", state.status());
        let mut state = shrunk(profile, rows, old, new, &own);
        state.push(&bytes, |_| Vec::new());
        for byte in fresh.chunks(1) {
            state.push(byte, |_| Vec::new());
        }
        assert!(clean(&state), "{what} {new}, a byte at a time");
        for at in 1..fresh.len() {
            let mut state = shrunk(profile, rows, old, new, &own);
            state.push(&bytes, |_| Vec::new());
            state.push(&fresh[..at], |_| Vec::new());
            state.push(&fresh[at..], |_| Vec::new());
            assert!(clean(&state), "{what} {new}, cut at {at}");
        }
    }
}

/// Scenario: Push a Codex choice row (read on Codex's witnesses alone) and a Claude Code rule drawn for the width in force, ending the push right after the row's last cell. The state stays pending until the next byte moves the cursor off the row, and turns Clean then.
#[spec("inbox/doorbell/044")]
#[test]
fn doorbell_044_a_witness_row_counts_once_the_row_has_ended() {
    for (profile, (head, row, tail), cols) in [
        (codex_witness_only(), codex_choice_parts(180), 180),
        (codex_witness_only(), codex_turn_parts(180), 180),
        (&CLAUDE_PROFILE, claude_frame_parts(80), 80),
    ] {
        let mut state = EvidenceState::new(Some(profile), 50, cols);
        state.push(format!("{head}{row}").as_bytes(), |_| Vec::new());
        assert!(
            matches!(state.status(), EvidenceStatus::Pending { .. }),
            "{row:?}: {:?}",
            state.status()
        );
        assert!(state.clean_frame().is_none());
        state.push(tail.as_bytes(), |_| Vec::new());
        assert!(clean(&state), "{row:?}: {:?}", state.status());
    }
}

/// How a replay cuts the recorded bytes into pushes.
#[derive(Clone, Copy, Debug)]
enum Cuts {
    /// As the PTY returned them.
    Reads,
    /// One byte per push.
    Bytes,
    /// At pseudo-random places, from this seed.
    Random(u64),
}

/// Replay `stem` through an evidence state of `profile`, each edge at its
/// recorded place, the bytes cut into pushes by `cuts`, and assert that the
/// state is Clean at each of the twenty edges' screenshots.
fn each_edge_ends_clean(dir: &str, stem: &str, profile: &'static ScreenProfile, cuts: Cuts) {
    let run = run_in(dir, stem);
    let mut state = EvidenceState::new(Some(profile), run.rows, run.cols);
    let mut rng = match cuts {
        Cuts::Random(seed) => seed | 1,
        _ => 1,
    };
    let mut pos = 0;
    let mut feed = |state: &mut EvidenceState, pos: &mut usize, end: usize| match cuts {
        Cuts::Reads => run.feed(state, pos, end),
        Cuts::Bytes => {
            while *pos < end {
                state.push(&run.bytes[*pos..*pos + 1], |_| Vec::new());
                *pos += 1;
            }
        }
        Cuts::Random(_) => {
            while *pos < end {
                // xorshift64
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                let next = (*pos + 1 + (rng % 512) as usize).min(end);
                state.push(&run.bytes[*pos..next], |_| Vec::new());
                *pos = next;
            }
        }
    };
    let mut shots: Vec<(usize, String)> = run.index["points"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(k, _)| k.starts_with("resize-") || k.starts_with("nudge-"))
        .map(|(k, v)| (v.as_u64().unwrap() as usize, k.clone()))
        .collect();
    shots.sort();
    assert_eq!(shots.len(), 20, "{stem}");
    let mut edges = run.edges.iter().peekable();
    for (shot, name) in shots {
        while let Some(&&(off, r, c)) = edges.peek() {
            if off >= shot {
                break;
            }
            feed(&mut state, &mut pos, off);
            state.edge(EdgeReason::Resize, r, c);
            edges.next();
        }
        feed(&mut state, &mut pos, shot);
        assert!(
            matches!(state.status(), EvidenceStatus::Clean { .. }),
            "{stem} {name} ({cuts:?}): {:?}",
            state.status()
        );
    }
}

/// Replay `stem` read by read, one byte per push and cut at pseudo-random
/// places, and assert every edge Clean by its screenshot each time.
fn edges_end_clean_however_cut(dir: &str, stem: &str, profile: &'static ScreenProfile) {
    for cuts in [
        Cuts::Reads,
        Cuts::Bytes,
        Cuts::Random(0x5eed_1666),
        Cuts::Random(0xdead_beef),
    ] {
        each_edge_ends_clean(dir, stem, profile, cuts);
    }
}

/// Scenario: Replay Codex 0.164.0's recorded start-up run, with its ten resizes and ten nudges at their recorded places, through an evidence state three ways: read by read as the PTY returned the bytes, one byte per push, and cut at pseudo-random places. Every edge is Clean again by its screenshot, however the bytes were cut.
#[spec("inbox/doorbell/044")]
#[test]
fn doorbell_044_codex_0164_start_edges_end_clean_however_cut() {
    edges_end_clean_however_cut(
        "codex-inbox",
        "codex-0.164.0-layout-start-50x200",
        &CODEX_PROFILE,
    );
}

/// Scenario: Replay Codex 0.164.0's recorded run after a first prompt, with its ten resizes and ten nudges at their recorded places, read by read, one byte per push and cut at pseudo-random places. Every edge is Clean again by its screenshot, however the bytes were cut.
#[spec("inbox/doorbell/044")]
#[test]
fn doorbell_044_codex_0164_turn_edges_end_clean_however_cut() {
    edges_end_clean_however_cut(
        "codex-inbox",
        "codex-0.164.0-layout-turn-50x200",
        &CODEX_PROFILE,
    );
}

/// Scenario: Replay Codex 0.160.0's recorded start-up run, with its ten resizes and ten nudges at their recorded places, read by read, one byte per push and cut at pseudo-random places. Every edge is Clean again by its screenshot, however the bytes were cut.
#[spec("inbox/doorbell/044")]
#[test]
fn doorbell_044_codex_0160_start_edges_end_clean_however_cut() {
    edges_end_clean_however_cut(
        "codex-inbox",
        "codex-0.160.0-layout-start-50x200",
        &CODEX_PROFILE,
    );
}

/// Scenario: Replay Claude Code 2.1.310's recorded run, with its ten resizes and ten nudges at their recorded places, through an evidence state read by read, one byte per push and cut at pseudo-random places. Every edge is Clean again by its screenshot, however the bytes were cut.
#[spec("inbox/doorbell/044")]
#[test]
fn doorbell_044_claude_2_1_310_edges_end_clean_however_cut() {
    edges_end_clean_however_cut(
        "claude-inbox",
        "claude-2.1.310-layout-50x200",
        &CLAUDE_PROFILE,
    );
}

/// Scenario: Start a Codex generation's evidence state at 180 columns and resize its pane to 117, then 116, then 117 again, as an orchestration's split can (the revcxact review's MEDIUM-2). The widths the state reports for a diagnosis line are the current 117 and every width it has had, 116 and 117 among them, which is what makes Codex's logo the pair witness rather than a width witness.
#[spec("inbox/doorbell/044")]
#[test]
fn doorbell_044_the_state_reports_every_width_its_pane_has_had() {
    let mut state = EvidenceState::new(Some(&CODEX_PROFILE), 50, 180);
    assert_eq!(state.widths(), (180, vec![180]));
    for cols in [117, 116, 117] {
        state.edge(EdgeReason::Resize, 50, cols);
    }
    assert_eq!(state.widths(), (117, vec![116, 117, 180]));
}

/// A Codex frame with no witness: the composer's empty row and its
/// placeholder, the cursor after the prompt glyph.
fn codex_composer_only() -> Vec<u8> {
    format!("{CODEX_REDRAW}\x1b[20;1H› \x1b[2mAsk Codex to do anything\x1b[0m\x1b[20;3H")
        .into_bytes()
}

/// Scenario: Replay real Codex 0.169.0 before its first prompt in two panes under 32 rows, where it draws no logo, as the PTY read it: one 27 × 162 whose box was typed into (`x`) and cleared, then nudged to 161 columns and back, and one 24 × 80 nudged to 79 and back. Read on Codex's witnesses alone, neither state is ever Clean, the hold issue #2262 reported. With the agreement rule each is Clean on agreement once the replay ends, at the width in force with the other width read alike, and the frame shows Codex's empty composer with its placeholder.
#[spec("inbox/doorbell/044")]
#[test]
fn doorbell_044_a_short_codex_with_no_witness_is_clean_on_agreement() {
    for stem in ["codex-0.169.0-touched-27x162", "codex-0.169.0-nudged-24x80"] {
        let run = run_at("codex-inbox/short-pane", stem);
        assert_eq!(run.edges.len(), 2, "{stem}: a nudge's shrink and restore");
        for agreement in [false, true] {
            let profile = if agreement {
                &CODEX_PROFILE
            } else {
                codex_witness_only()
            };
            let mut state = EvidenceState::new(Some(profile), run.rows, run.cols);
            let mut ever = false;
            let mut pos = 0;
            for (i, &(off, rows, cols)) in run.edges.iter().enumerate() {
                while pos < off {
                    let next = run
                        .read_ends
                        .iter()
                        .copied()
                        .find(|&e| e > pos)
                        .unwrap_or(off);
                    let next = next.min(off);
                    state.push(&run.bytes[pos..next], |_| Vec::new());
                    ever |= clean(&state);
                    pos = next;
                }
                let reason = if i == 0 {
                    EdgeReason::NudgeShrink
                } else {
                    EdgeReason::NudgeRestore
                };
                state.edge(reason, rows, cols);
            }
            let rest: Vec<usize> = run.read_ends.iter().copied().filter(|&e| e > pos).collect();
            for end in rest {
                state.push(&run.bytes[pos..end], |_| Vec::new());
                ever |= clean(&state);
                pos = end;
            }
            if !agreement {
                assert!(!ever, "{stem}: Clean on a witness: {:?}", state.status());
                continue;
            }
            assert_eq!(
                state.clean_basis(),
                Some(CleanBasis::Agreement),
                "{stem}: {:?}",
                state.status()
            );
            assert_eq!(state.widths(), (run.cols, vec![run.cols - 1, run.cols]));
            let frame = state.clean_frame().unwrap();
            assert!(
                CODEX_PROFILE.empty_and_ready(&frame) && CODEX_PROFILE.placeholder_empty(&frame),
                "{stem}: the empty composer with its placeholder"
            );
        }
    }
}

/// Scenario: Shrink Codex terminals from 181 and from 200 columns to 180, both widths now recorded at one row count, then push Codex's padded choice and turn rows drawn for the old width: whole, a byte at a time, and cut once at every byte. The push that reads the row differently at the two widths ends the agreement rule: right after the last cell that fits the new edge both widths still show the same screen, and the next byte, which wraps at the narrower width only, leaves the state not Clean with the frame read at the cut no longer standing; once the stale redraw is all in the state is pending, gives no frame, and any frame read on agreement earlier no longer stands. The redraw drawn for the width in force turns it Clean on its width witness.
#[spec("inbox/doorbell/044")]
#[test]
fn doorbell_044_agreement_ends_at_the_first_reading_that_differs() {
    let rows = 50;
    for (old, new) in [(181, 180), (200, 180)] {
        for (what, stale, fresh) in [
            ("choice", codex_choice_parts(old), codex_choice(new)),
            ("turn", codex_turn_parts(old), codex_turn(new)),
        ] {
            let bytes = whole(stale.clone());
            let fits: String = stale.1.chars().take(new as usize).collect();
            let cut = stale.0.len() + fits.len();
            let shrunk = || shrunk(&CODEX_PROFILE, rows, old, new, &codex_choice(old));
            let ended = |state: &EvidenceState, how: &str| {
                assert!(
                    pending_after_edge(state) && state.clean_frame().is_none(),
                    "{what} {old} → {new}, {how}: {:?}",
                    state.status()
                );
            };

            let mut state = shrunk();
            state.push(&bytes, |_| Vec::new());
            ended(&state, "whole");

            // Right after the last cell that fits, a terminal of either width
            // shows the same screen, so a frame read there is the one on
            // screen; the next byte wraps at the narrower width only, which
            // ends the rule, and the frame read at the cut no longer stands.
            let mut state = shrunk();
            state.push(&bytes[..cut], |_| Vec::new());
            let mark = state.clean_frame_marked().map(|(_, mark)| mark);
            state.push(&bytes[cut..cut + 1], |_| Vec::new());
            assert!(
                !clean(&state) && mark.is_none_or(|mark| !state.frame_unchanged(mark)),
                "{what} {old} → {new}: Clean past the edge cut: {:?}",
                state.status()
            );
            state.push(&bytes[cut + 1..], |_| Vec::new());
            ended(&state, "cut at the edge");

            let mut state = shrunk();
            let mut marks = Vec::new();
            for byte in bytes.chunks(1) {
                state.push(byte, |_| Vec::new());
                marks.extend(state.clean_frame_marked().map(|(_, mark)| mark));
            }
            ended(&state, "a byte at a time");
            assert!(marks.iter().all(|&mark| !state.frame_unchanged(mark)));

            for at in 1..bytes.len() {
                let mut state = shrunk();
                state.push(&bytes[..at], |_| Vec::new());
                let mark = state.clean_frame_marked().map(|(_, mark)| mark);
                state.push(&bytes[at..], |_| Vec::new());
                ended(&state, &format!("cut at {at}"));
                assert!(mark.is_none_or(|mark| !state.frame_unchanged(mark)));
            }

            let mut state = shrunk();
            state.push(&bytes, |_| Vec::new());
            state.push(&fresh, |_| Vec::new());
            assert_eq!(state.clean_basis(), Some(CleanBasis::Witness), "{what}");
        }
    }
}

/// Scenario: Push a Codex frame with no witness, only its empty composer. It is Clean on agreement in a terminal that has only ever had one width, and in one that has had up to four other widths at one row count. It stays pending in one that has had five other widths, in one whose row count changed, and for Claude Code, whose profile does not opt in. In a terminal that has had 100 and 90 columns, a write past the narrower edge reads differently at the two widths and the frame stays pending. Codex's start-up logo drawn for 200 columns, read at 198 after 200, reads the same at both and is Clean on agreement, where the witnesses alone refuse it.
#[spec("inbox/doorbell/044")]
#[test]
fn doorbell_044_agreement_needs_one_row_count_and_few_enough_widths() {
    let frame = codex_composer_only();
    let mut state = EvidenceState::new(Some(&CODEX_PROFILE), 24, 100);
    state.push(&frame, |_| Vec::new());
    assert_eq!(state.clean_basis(), Some(CleanBasis::Agreement));

    let widths = |others: &[u16]| {
        let mut state = EvidenceState::new(Some(&CODEX_PROFILE), 24, 100);
        for &cols in others {
            state.edge(EdgeReason::Resize, 24, cols);
        }
        state.edge(EdgeReason::Resize, 24, 100);
        state.push(&frame, |_| Vec::new());
        state
    };
    let four: Vec<u16> = (96..96 + AGREEMENT_TWINS_MAX as u16).collect();
    assert_eq!(
        widths(&four).clean_basis(),
        Some(CleanBasis::Agreement),
        "{four:?}"
    );
    let five: Vec<u16> = (95..96 + AGREEMENT_TWINS_MAX as u16).collect();
    let state = widths(&five);
    assert!(pending_after_edge(&state), "{five:?}: {:?}", state.status());

    let mut state = EvidenceState::new(Some(&CODEX_PROFILE), 24, 100);
    state.edge(EdgeReason::Resize, 30, 100);
    state.edge(EdgeReason::Resize, 24, 100);
    state.push(&frame, |_| Vec::new());
    assert!(pending_after_edge(&state), "{:?}", state.status());

    let mut state = EvidenceState::new(Some(&CLAUDE_PROFILE), 24, 100);
    state.push(&frame, |_| Vec::new());
    assert!(!clean(&state), "{:?}", state.status());

    let mut state = EvidenceState::new(Some(&CODEX_PROFILE), 24, 100);
    state.edge(EdgeReason::Resize, 24, 90);
    let mut past = frame.clone();
    past.extend_from_slice(b"\x1b[5;95Hx\x1b[20;3H");
    state.push(&past, |_| Vec::new());
    assert!(pending_after_edge(&state), "{:?}", state.status());

    let mut state = EvidenceState::new(Some(&CODEX_PROFILE), 50, 200);
    state.edge(EdgeReason::Resize, 50, 198);
    state.push(&codex_logo(200), |_| Vec::new());
    assert_eq!(state.clean_basis(), Some(CleanBasis::Agreement));
    assert!(!state.pair_held());
}
