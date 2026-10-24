//! `inbox/doorbell/044`: the evidence state over a real Codex layout trace
//! (`src/evidence_state.rs`).
use std::path::PathBuf;

use dot_agent_deck::doorbell_screen::CODEX_PROFILE;
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
