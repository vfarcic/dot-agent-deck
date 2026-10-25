//! `inbox/doorbell/044`: the evidence state over a real Codex layout trace
//! (`src/evidence_state.rs`).
use std::path::PathBuf;

use dot_agent_deck::doorbell_screen::{CLAUDE_PROFILE, CODEX_PROFILE};
use dot_agent_deck::evidence_state::{EdgeReason, EvidenceState, EvidenceStatus};
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
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/codex-inbox/r5wake")
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

/// A Codex redraw for a terminal `cols` wide: a dialog's highlighted choice
/// (bold, inverse) padded to its edge.
fn codex_choice(cols: u16) -> Vec<u8> {
    format!(
        "{CODEX_REDRAW}\x1b[2;3H\x1b[1mFolder access\x1b[0m\x1b[8;1H\x1b[1;7m{}\x1b[0m",
        padded("› 1. Trust and continue", cols)
    )
    .into_bytes()
}

/// A Codex redraw for a terminal `cols` wide: the dim row under a finished
/// turn, padded to its edge.
fn codex_turn(cols: u16) -> Vec<u8> {
    format!(
        "{CODEX_REDRAW}\x1b[12;1H\x1b[2m{}\x1b[0m",
        padded("  Worked for 3s • 7:03 PM", cols)
    )
    .into_bytes()
}

/// A Codex start-up redraw for a terminal `drawn` columns wide: the logo's
/// top row at `drawn / 2 - 5`.
fn codex_logo(drawn: u16) -> Vec<u8> {
    format!("{CODEX_REDRAW}\x1b[17;{}H⣿⣿⣿⣿⣿⣿", drawn / 2 - 5 + 1).into_bytes()
}

/// A Claude Code redraw: the full-redraw marker and an empty composer
/// between two rules `rule` cells long.
fn claude_frame(rule: u16) -> Vec<u8> {
    let rule = "─".repeat(rule as usize);
    format!(
        "\x1b[2J\x1b[H\x1b[20;1H{rule}\x1b[21;1H❯\u{a0}\x1b[22;1H{rule}\x1b[23;3Hstatus\x1b[21;3H"
    )
    .into_bytes()
}

fn clean(state: &EvidenceState) -> bool {
    matches!(state.status(), EvidenceStatus::Clean { .. })
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

/// Scenario: Codex's start-up logo sits at the same column for the two widths 2k and 2k + 1. Move a terminal from 201 to 200 columns (the even width), and another from 200 to 201 (the odd width), and push a logo drawn for each width: every one stays pending, since the terminal has had both widths of the pair. A terminal that has only ever been 200 or only ever 201 columns wide is Clean on its own logo, and one moved from 200 to 198, a width that does not share the logo's column, refuses the 200-column logo and takes its own.
#[spec("inbox/doorbell/044")]
#[test]
fn doorbell_044_a_logo_two_adjacent_widths_share_stays_pending() {
    for (spawned, now) in [(201, 200), (200, 201)] {
        for drawn in [spawned, now] {
            let mut state = EvidenceState::new(Some(&CODEX_PROFILE), 50, spawned);
            state.edge(EdgeReason::Resize, 50, now);
            state.push(&codex_logo(drawn), |_| Vec::new());
            assert!(
                pending_after_edge(&state),
                "{spawned} → {now}, drawn for {drawn}: {:?}",
                state.status()
            );
        }
    }
    for cols in [200, 201] {
        let mut state = EvidenceState::new(Some(&CODEX_PROFILE), 50, cols);
        state.push(&codex_logo(cols), |_| Vec::new());
        assert!(clean(&state), "only ever {cols} wide");
    }
    let mut state = EvidenceState::new(Some(&CODEX_PROFILE), 50, 200);
    state.edge(EdgeReason::Resize, 50, 198);
    state.push(&codex_logo(200), |_| Vec::new());
    assert!(pending_after_edge(&state));
    state.push(&codex_logo(198), |_| Vec::new());
    assert!(clean(&state));
}

/// Scenario: Push redraws whose rows reach the right edge without being a witness: for Codex, plain text exactly as wide as the terminal, plain text padded to the edge, the dim turn row's words run to the edge with no padding, and a numbered choice padded to the edge but not highlighted; for Claude Code, a row of other characters across the width and a rule with one cell broken. Each leaves the state pending; a real witness pushed after them turns it Clean.
#[spec("inbox/doorbell/044")]
#[test]
fn doorbell_044_text_reaching_the_edge_is_not_a_witness() {
    let cols = 200;
    let mut state = EvidenceState::new(Some(&CODEX_PROFILE), 50, cols);
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
    }
    state.push(&claude_frame(80), |_| Vec::new());
    assert!(clean(&state));
}
