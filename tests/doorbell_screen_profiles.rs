//! `inbox/doorbell/036`: the doorbell's screen profiles and the release
//! recogniser over raw captures of real launches (M1, M2), read through the
//! lossless grid (`src/doorbell_screen.rs`, `src/release_scanner.rs`).
use std::collections::HashMap;
use std::path::PathBuf;

use dot_agent_deck::doorbell_activation::{
    Activation, Measurement, NoGo, RELEASE_RECORDS, ReleaseRecord, exact_verdict,
};
use dot_agent_deck::doorbell_screen::{
    CLAUDE_PROFILE, CODEX_PROFILE, ComposerText, Dialog, Grid, ScreenProfile, WidthsSeen,
    WithdrawalRefusal, find_marker, strict_withdrawal,
};
use dot_agent_deck::event::AgentType;
use dot_agent_deck::release_scanner::{
    HEADER_RECOGNISERS, ReleaseScanner, ScanLatch, latch_byte_at_a_time,
};
use semver::Version;
use spec::spec;

/// One recorded launch: the bytes, the sizes the PTY reader returned them in,
/// the stream offset of each named moment, and each resize.
struct Trace {
    bytes: Vec<u8>,
    rows: u16,
    cols: u16,
    read_lens: Vec<usize>,
    points: HashMap<String, usize>,
    edges: Vec<(usize, u16, u16)>,
    lines: Vec<String>,
}

fn fixture(stem: &str) -> Trace {
    // The stems carry release numbers, so the extension is appended, not set.
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let bytes = std::fs::read(base.join(format!("{stem}.bin"))).expect("fixture bytes");
    let index: serde_json::Value =
        serde_json::from_slice(&std::fs::read(base.join(format!("{stem}.json"))).expect("index"))
            .expect("index json");
    let num = |v: &serde_json::Value| v.as_u64().unwrap() as usize;
    Trace {
        rows: index["rows"].as_u64().unwrap() as u16,
        cols: index["cols"].as_u64().unwrap() as u16,
        read_lens: index["read_lens"]
            .as_array()
            .unwrap()
            .iter()
            .map(num)
            .collect(),
        points: index["points"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), num(v)))
            .collect(),
        edges: index["edges"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    num(&e["off"]),
                    e["rows"].as_u64().unwrap() as u16,
                    e["cols"].as_u64().unwrap() as u16,
                )
            })
            .collect(),
        lines: index["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap().to_string())
            .collect(),
        bytes,
    }
}

impl Trace {
    /// The screen after the first `end` bytes, every resize applied at its
    /// place in the stream.
    fn grid_upto(&self, end: usize) -> Grid {
        Grid::of(self.parser_upto(end).screen(), end as u64)
    }

    /// [`Self::grid_upto`]'s parser.
    fn parser_upto(&self, end: usize) -> vt100::Parser {
        let mut parser = vt100::Parser::new(
            std::num::NonZeroU16::new(self.rows).unwrap(),
            std::num::NonZeroU16::new(self.cols).unwrap(),
            0,
        );
        let mut pos = 0;
        for &(off, rows, cols) in &self.edges {
            // A screen saved at an offset shares it with the next resize,
            // which came after it.
            if off >= end {
                break;
            }
            parser.process(&self.bytes[pos..off]);
            pos = off;
            parser.screen_mut().set_size(
                std::num::NonZeroU16::new(rows).unwrap(),
                std::num::NonZeroU16::new(cols).unwrap(),
            );
        }
        parser.process(&self.bytes[pos..end]);
        parser
    }

    /// Every width the terminal had in the first `end` bytes.
    fn widths_upto(&self, end: usize) -> WidthsSeen {
        WidthsSeen::of(
            std::iter::once(self.cols).chain(
                self.edges
                    .iter()
                    .take_while(|(off, _, _)| *off < end)
                    .map(|&(_, _, cols)| cols),
            ),
        )
    }

    /// Whether the screen after the first `end` bytes is `profile`'s width
    /// witness.
    fn witness_upto(&self, profile: &ScreenProfile, end: usize) -> bool {
        (profile.width_witness)(self.parser_upto(end).screen(), &self.widths_upto(end))
    }

    fn grid(&self, point: &str) -> Grid {
        self.grid_upto(self.points[point])
    }

    /// The geometry in force just before stream offset `end`.
    fn size_at(&self, end: usize) -> (u16, u16) {
        self.edges
            .iter()
            .take_while(|(off, _, _)| *off < end)
            .last()
            .map(|&(_, r, c)| (r, c))
            .unwrap_or((self.rows, self.cols))
    }

    /// The screens after each read the PTY returned, from `from` to `to`.
    fn reads_between(&self, from: usize, to: usize) -> Vec<Grid> {
        let mut out = Vec::new();
        let mut end = 0;
        for len in &self.read_lens {
            end += len;
            if end > from && end <= to {
                out.push(self.grid_upto(end));
            }
        }
        out
    }
}

impl Trace {
    /// This trace with every geometry two columns wider: the agent's bytes
    /// replayed on a terminal it did not draw for.
    fn widened(&self) -> Trace {
        Trace {
            bytes: self.bytes.clone(),
            rows: self.rows,
            cols: self.cols + 2,
            read_lens: self.read_lens.clone(),
            points: self.points.clone(),
            edges: self.edges.iter().map(|&(o, r, c)| (o, r, c + 2)).collect(),
            lines: self.lines.clone(),
        }
    }
}

/// Whether `bytes`, parsed from a fresh terminal at `rows` × `cols` that has
/// had no other width, are `profile`'s width witness.
fn witness(profile: &ScreenProfile, bytes: &[u8], rows: u16, cols: u16) -> bool {
    let mut parser = vt100::Parser::new(
        std::num::NonZeroU16::new(rows).unwrap(),
        std::num::NonZeroU16::new(cols).unwrap(),
        0,
    );
    parser.process(bytes);
    (profile.width_witness)(parser.screen(), &WidthsSeen::of([cols]))
}

fn latch_at_split(trace: &Trace) -> ScanLatch {
    let mut scanner = ReleaseScanner::new(trace.rows, trace.cols, HEADER_RECOGNISERS).unwrap();
    let mut pos = 0;
    for len in &trace.read_lens {
        scanner.feed(&trace.bytes[pos..pos + len]);
        pos += len;
    }
    scanner.latch().clone()
}

fn latched(trace: &Trace) -> (AgentType, Version) {
    match latch_at_split(trace) {
        ScanLatch::Latched { agent, release, .. } => (agent, release),
        other => panic!("expected a latched release, got {other:?}"),
    }
}

/// Scenario: Replay the recorded start-up of Codex 0.164.0, Codex 0.160.0 and Claude Code 2.1.310 through the release scanner at the PTY reader's recorded split, one byte at a time, and as one push. Each latches its own release every way.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_release_headers_latch_from_real_launches() {
    for (stem, agent, release) in [
        (
            "codex-inbox/r5wake/codex-0.164.0-start-update-50x200",
            AgentType::Codex,
            "0.164.0",
        ),
        (
            "codex-inbox/r5wake/codex-0.160.0-start-update-50x200",
            AgentType::Codex,
            "0.160.0",
        ),
        (
            "codex-inbox/r5wake/codex-0.164.0-composer-24x80",
            AgentType::Codex,
            "0.164.0",
        ),
        (
            "codex-inbox/r5wake/codex-0.160.0-composer-24x80",
            AgentType::Codex,
            "0.160.0",
        ),
        (
            "claude-inbox/r5wake/claude-2.1.310-start-50x200",
            AgentType::ClaudeCode,
            "2.1.310",
        ),
        (
            "claude-inbox/r5wake/claude-2.1.310-composer-24x80",
            AgentType::ClaudeCode,
            "2.1.310",
        ),
        (
            "codex-inbox/r5wake/codex-0.164.0-deck-tester-fresh-46x130",
            AgentType::Codex,
            "0.164.0",
        ),
        (
            "codex-inbox/r5wake/codex-0.164.0-deck-tester-respawn-46x130",
            AgentType::Codex,
            "0.164.0",
        ),
    ] {
        let trace = fixture(stem);
        assert_eq!(
            latched(&trace),
            (agent, Version::parse(release).unwrap()),
            "{stem}"
        );
        // Pseudo-random splits (a fixed LCG, so a failure reproduces).
        let mut seed: u64 = 0x1666_5a4e;
        for _ in 0..8 {
            let mut s = ReleaseScanner::new(trace.rows, trace.cols, HEADER_RECOGNISERS).unwrap();
            let mut pos = 0;
            while pos < trace.bytes.len() {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let len = 1 + (seed >> 33) as usize % 700;
                let end = (pos + len).min(trace.bytes.len());
                s.feed(&trace.bytes[pos..end]);
                pos = end;
            }
            assert_eq!(
                s.latch(),
                &latch_byte_at_a_time(&trace.bytes, trace.rows, trace.cols).unwrap(),
                "{stem}"
            );
        }
        let one_push = {
            let mut s = ReleaseScanner::new(trace.rows, trace.cols, HEADER_RECOGNISERS).unwrap();
            s.feed(&trace.bytes);
            s.latch().clone()
        };
        let reference = latch_byte_at_a_time(&trace.bytes, trace.rows, trace.cols).unwrap();
        assert_eq!(latch_at_split(&trace), reference, "{stem}");
        assert_eq!(one_push, reference, "{stem}");
    }
}

fn latch_of(bytes: &[u8]) -> ScanLatch {
    let mut scanner = ReleaseScanner::new(24, 80, HEADER_RECOGNISERS).unwrap();
    scanner.feed(bytes);
    scanner.latch().clone()
}

/// Scenario: Feed header-like text in other shapes and positions: the Codex header one column off, without its `(v`, with a pre-release tag or unterminated, and Claude Code's header at another column or with a pre-release tag. None latches; a Claude Code version written in pieces latches once, complete, without a conflict.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_release_recogniser_negatives() {
    let codex = |col: u16, text: &str| format!("\x1b[2;{}H{text}\r\n", col + 1).into_bytes();
    assert!(matches!(
        latch_of(&codex(5, "OpenAI Codex (v0.164.0)")),
        ScanLatch::Latched { .. }
    ));
    for bytes in [
        codex(4, "OpenAI Codex (v0.164.0)"),
        codex(6, "OpenAI Codex (v0.164.0)"),
        codex(5, "OpenAI Codex v0.164.0"),
        codex(5, "OpenAI Codex (v0.165.0-alpha.1)"),
        codex(5, "OpenAI Codex (v0.164)"),
        codex(5, "OpenAI Codex (v0.164.0"),
        codex(5, "OpenAI codex (v0.164.0)"),
        b"\x1b[5;6HOpenAI Codex (v0.164.0)\r\n".to_vec(),
    ] {
        assert_eq!(
            latch_of(&bytes),
            ScanLatch::Pending,
            "{:?}",
            String::from_utf8_lossy(&bytes)
        );
    }
    let claude = |col: u16, text: &str| format!("\x1b[2;{}H{text}\r\n", col + 1).into_bytes();
    assert!(matches!(
        latch_of(&claude(11, "Claude Code v2.1.310")),
        ScanLatch::Latched { .. }
    ));
    for bytes in [
        claude(10, "Claude Code v2.1.310"),
        claude(11, "Claude Code  v2.1.310"),
        claude(11, "Claude Code v2.1.310-beta.2"),
        claude(11, "Claude Code 2.1.310"),
    ] {
        assert_eq!(
            latch_of(&bytes),
            ScanLatch::Pending,
            "{:?}",
            String::from_utf8_lossy(&bytes)
        );
    }
    // Written in pieces, the version's prefixes never latch (the cursor still
    // sits after the last digit), and the completed version latches once.
    let mut scanner = ReleaseScanner::new(24, 80, HEADER_RECOGNISERS).unwrap();
    scanner.feed(b"\x1b[2;12HClaude Code\x1b[24Gv2.1.3");
    assert_eq!(scanner.latch(), &ScanLatch::Pending);
    scanner.feed(b"10");
    assert_eq!(scanner.latch(), &ScanLatch::Pending);
    scanner.feed(b"\r");
    assert!(matches!(
        scanner.latch(),
        ScanLatch::Latched { release, .. } if *release == Version::new(2, 1, 310)
    ));
}

fn holds(profile: &ScreenProfile, grid: &Grid, line: &str) -> Option<ComposerText> {
    profile.holds(grid, line)
}

/// Scenario: Read the recorded Codex 0.164.0 and 0.160.0 composer sessions at the empty composer, after the deck's line was written, after its Enter, after the deletes, and for a prefix of the line still arriving. The profile sees the empty ready composer with its placeholder, the exact line with the cursor after it, the submitted line as a transcript user message and not as composer text, the prefix as a prefix, and the placeholder back after the deletes.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_codex_composer_over_real_frames() {
    for stem in [
        "codex-inbox/r5wake/codex-0.164.0-composer-24x80",
        "codex-inbox/r5wake/codex-0.160.0-composer-24x80",
    ] {
        let trace = fixture(stem);
        let p = &CODEX_PROFILE;
        let empty = trace.grid("empty");
        assert!(p.empty_and_ready(&empty), "{stem}: empty");
        assert!(p.placeholder_empty(&empty), "{stem}: placeholder");
        let line = &trace.lines[0];
        let typed = trace.grid("typed-01");
        assert_eq!(
            holds(p, &typed, line),
            Some(ComposerText::Exact),
            "{stem}: typed"
        );
        assert!(!p.empty_and_ready(&typed));
        assert!(
            !(p.transcript_user_line)(&typed, line),
            "{stem}: composer text is not a user cell"
        );
        assert_eq!(strict_withdrawal(p, &typed, line), Ok(line.chars().count()));
        let after = trace.grid("after-01");
        assert!((p.transcript_user_line)(&after, line), "{stem}: user cell");
        assert_eq!(
            holds(p, &after, line),
            Some(ComposerText::Empty { placeholder: true })
        );
        assert_eq!(
            strict_withdrawal(p, &after, line),
            Err(WithdrawalRefusal::NotOwnedText)
        );
        let deleted = trace.grid("deleted-01");
        assert!(p.placeholder_empty(&deleted), "{stem}: deleted");
        // The line still arriving: the composer drawing its first 40
        // characters, the cursor after them.
        let empty_end = trace.points["empty"];
        let row = (p.composer)(&empty).unwrap().prompt_row;
        let mut bytes = trace.bytes[..empty_end].to_vec();
        bytes.extend_from_slice(
            format!("\x1b[{};3H\x1b[0m{}\x1b[K", row + 1, &line[..40]).as_bytes(),
        );
        let partial = Grid::parse(&bytes, trace.rows, trace.cols);
        assert_eq!(
            holds(p, &partial, line),
            Some(ComposerText::Prefix { chars: 40 }),
            "{stem}"
        );
        // The same prefix with the cursor moved off it is not a prefix.
        bytes.extend_from_slice(b"\x1b[1;1H");
        assert_eq!(
            holds(p, &Grid::parse(&bytes, trace.rows, trace.cols), line),
            Some(ComposerText::Other)
        );
    }
}

/// Scenario: Read the recorded Claude Code 2.1.310 composer session at the empty composer, after the deck's line was written, at each read after its Enter, and after a clear. The profile sees the empty ready composer with its suggestion as placeholder, the exact line, the submitted line as a user message on its shaded row while it is drawn, and a cleared composer that is empty but shows no placeholder.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_claude_composer_over_real_frames() {
    let trace = fixture("claude-inbox/r5wake/claude-2.1.310-composer-24x80");
    let p = &CLAUDE_PROFILE;
    let empty = trace.grid("empty");
    assert!(p.empty_and_ready(&empty));
    assert!(p.placeholder_empty(&empty));
    let line = &trace.lines[0];
    let typed = trace.grid("typed-01");
    assert_eq!(holds(p, &typed, line), Some(ComposerText::Exact));
    assert!(!(p.transcript_user_line)(&typed, line));
    assert_eq!(strict_withdrawal(p, &typed, line), Ok(line.chars().count()));
    let after_cr = trace.reads_between(trace.points["mark:cr-1"], trace.points["after-01"]);
    assert!(
        after_cr.iter().any(|g| (p.transcript_user_line)(g, line)),
        "the submitted line is drawn as a user message"
    );
    let cleared = trace.grid("cleared-01");
    assert_eq!(
        holds(p, &cleared, line),
        Some(ComposerText::Empty { placeholder: false })
    );
    assert!(p.empty_and_ready(&cleared));
    assert!(
        !p.placeholder_empty(&cleared),
        "a blank composer is not the placeholder"
    );
}

/// Scenario: Take the recorded Claude Code 2.1.310 session at its cleared, empty composer and type three spaces into it, then move the cursor out of the composer instead. Neither is the empty ready composer, to the profile or to the frame admission reads, although both show no text: the blank composer with the cursor where Claude Code keeps it is the control. The spaces are appended to the real capture, as the terminal shows typed spaces; no session typing spaces was recorded.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_a_blank_draft_in_the_claude_composer_is_not_empty() {
    use dot_agent_deck::doorbell_screen::frame;
    let trace = fixture("claude-inbox/r5wake/claude-2.1.310-composer-24x80");
    let p = &CLAUDE_PROFILE;
    let at = trace.points["cleared-01"];
    let with = |extra: &[u8]| {
        let mut bytes = trace.bytes[..at].to_vec();
        bytes.extend_from_slice(extra);
        Grid::parse(&bytes, trace.rows, trace.cols)
    };
    let cleared = with(b"");
    assert!(p.empty_and_ready(&cleared), "the control");
    assert!(frame(p, &cleared, 0, true, None).empty_and_ready());
    for (case, extra) in [
        ("spaces", &b"   "[..]),
        ("cursor outside the composer", &b"\x1b[1;1H"[..]),
    ] {
        let grid = with(extra);
        let composer = (p.composer)(&grid).expect("the composer is still drawn");
        assert_eq!(composer.text, "", "{case}: the recogniser trims the blanks");
        assert!(!composer.cursor_at_end, "{case}");
        assert!(!p.empty_and_ready(&grid), "{case}");
        assert!(
            !frame(p, &grid, 0, true, None).empty_and_ready(),
            "{case}: admission reads no empty composer"
        );
    }
}

/// Scenario: Take the recorded Claude Code 2.1.310 session at its cleared composer and redraw it the way a real 2.1.310 session draws blanks it holds (measured 2026-10-26: typed spaces write no cell, the cursor only moves; a line of nothing is a row of nothing): three spaces with the cursor moved back to the start, the same spaces written as cells, a blank line with text below it and the cursor back on the first row, a line of spaces under the prompt row, and a letter, a blank line and more text with the cursor after the letter. The first looks exactly like the empty composer, so the frame is ready only while the input says the box is empty, never when it says a draft or cannot say; the next three are not empty whatever the input says, and the last does not hold the letter alone. Every recorded empty composer (fresh, cleared, deleted, resized, nudged) stays ready with the input proving it empty, the placeholder alone is ready while the input cannot say, and none is ready with the input saying a draft. The redraws are synthesized on the real capture; the measured session's own bytes are not committed.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_blanks_and_blank_lines_the_claude_screen_cannot_show() {
    use dot_agent_deck::composer_model::InputProof;
    use dot_agent_deck::doorbell_screen::frame;
    let trace = fixture("claude-inbox/r5wake/claude-2.1.310-composer-24x80");
    let p = &CLAUDE_PROFILE;
    let at = trace.points["cleared-01"];
    let with = |extra: &[u8]| {
        let mut bytes = trace.bytes[..at].to_vec();
        bytes.extend_from_slice(extra);
        Grid::parse(&bytes, trace.rows, trace.cols)
    };
    let ready = |grid: &Grid, input: InputProof| {
        let mut f = frame(p, grid, 0, true, None);
        f.input = input;
        f.empty_and_ready()
    };
    // The cleared composer: prompt on row 22 (one-based), its rules on 21
    // and 23. Three spaces, then Left three times, as Claude Code redraws
    // them: the cursor moved out and back, nothing written.
    let moved_back = with(b"\x1b[22;6H\x1b[22;3H");
    let composer = (p.composer)(&moved_back).expect("the composer");
    assert_eq!(composer.text, "");
    assert!(composer.cursor_at_end && !composer.hidden);
    assert_eq!(
        moved_back.cells,
        with(b"").cells,
        "the screen cannot tell the blanks from the empty composer"
    );
    assert!(
        p.empty_and_ready(&moved_back),
        "the screen alone says empty"
    );
    assert!(ready(&moved_back, InputProof::Empty));
    assert!(
        !ready(&moved_back, InputProof::Draft),
        "the input says a draft"
    );
    assert!(
        !ready(&moved_back, InputProof::Unknown),
        "no placeholder to prove it"
    );
    let rule = "\u{2500}".repeat(usize::from(trace.cols));
    for (case, extra) in [
        ("spaces written as cells", "   \x1b[3D".to_string()),
        (
            "a blank line with text below it",
            format!(
                "\x1b[19;1H{rule}\x1b[20;1H\u{276f}\u{a0}\x1b[K\x1b[21;1H\x1b[K\x1b[22;1H  abc\x1b[K\x1b[20;3H"
            ),
        ),
        (
            "a line of spaces under the prompt row",
            format!("\x1b[20;1H{rule}\x1b[21;1H\u{276f}\u{a0}\x1b[K\x1b[22;1H\x1b[K\x1b[21;3H"),
        ),
    ] {
        let grid = with(extra.as_bytes());
        let composer = (p.composer)(&grid).expect("the composer is still drawn");
        assert!(composer.hidden, "{case}: {composer:?}");
        assert_eq!(
            grid.cursor,
            (composer.prompt_row, 2),
            "{case}: the cursor is back at the start"
        );
        assert_eq!(p.holds(&grid, "\u{0}"), Some(ComposerText::Other), "{case}");
        assert!(!p.empty_and_ready(&grid), "{case}");
        assert!(!ready(&grid, InputProof::Empty), "{case}");
    }
    // Text, a blank line and more text, the cursor after the first: not the
    // first line alone.
    let gapped = with(
        format!(
            "\x1b[19;1H{rule}\x1b[20;1H\u{276f}\u{a0}x\x1b[K\x1b[21;1H\x1b[K\x1b[22;1H  abc\x1b[K\x1b[20;4H"
        )
        .as_bytes(),
    );
    let composer = (p.composer)(&gapped).expect("the composer");
    assert!(composer.hidden, "{composer:?}");
    assert_eq!(p.holds(&gapped, "x"), Some(ComposerText::Other));
    assert!(!frame(p, &gapped, 0, true, None).holds_exactly("x"));
    // Positive controls: every recorded empty composer.
    let mut empties = 0;
    for stem in [
        "claude-inbox/r5wake/claude-2.1.310-composer-24x80",
        "claude-inbox/r5wake/claude-2.1.310-layout-50x200",
        "claude-inbox/r5wake/claude-2.1.310-start-50x200",
    ] {
        let trace = fixture(stem);
        let mut points: Vec<(&String, &usize)> = trace
            .points
            .iter()
            .filter(|(name, _)| {
                ["empty", "cleared-", "deleted-", "resize-", "nudge-", "end"]
                    .iter()
                    .any(|p| name.starts_with(p))
            })
            .collect();
        points.sort();
        for (name, &end) in points {
            let grid = trace.grid_upto(end);
            if !(p.ready)(&grid) || grid.cols < p.min_cols {
                // A resize to a size the profile does not cover.
                continue;
            }
            let placeholder = p.placeholder_empty(&grid);
            assert!(ready(&grid, InputProof::Empty), "{stem} {name}");
            assert_eq!(
                ready(&grid, InputProof::Unknown),
                placeholder,
                "{stem} {name}: only the placeholder proves it while the input cannot"
            );
            assert!(!ready(&grid, InputProof::Draft), "{stem} {name}");
            empties += 1;
        }
    }
    assert!(empties >= 40, "the controls ran: {empties}");
}

/// Scenario: Read Codex 0.164.0's recorded update, folder-trust and hooks-review dialogs, its composer, and frames whose transcript merely mentions "update", "Update now", "login" or "trust". The three dialogs are recognised by name; the composer and the transcript mentions give no dialog verdict; an unknown title with the dialog's shape is unrecognised.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_dialog_markers_and_false_dialogs() {
    let p = &CODEX_PROFILE;
    for (stem, point, name) in [
        (
            "codex-inbox/r5wake/codex-0.164.0-start-update-50x200",
            "end",
            "update",
        ),
        (
            "codex-inbox/r5wake/codex-0.164.0-trust-50x200",
            "before",
            "trust",
        ),
        (
            "codex-inbox/r5wake/codex-0.164.0-hooks-50x200",
            "before",
            "hooks",
        ),
    ] {
        let grid = fixture(stem).grid(point);
        assert_eq!((p.dialog)(&grid), Some(Dialog::Measured(name)), "{stem}");
        assert!(
            !p.empty_and_ready(&grid),
            "{stem}: no composer under a dialog"
        );
        assert_eq!(
            strict_withdrawal(p, &grid, "(abcdfg) x"),
            Err(WithdrawalRefusal::Dialog)
        );
    }
    let composer = fixture("codex-inbox/r5wake/codex-0.164.0-composer-24x80");
    let empty_end = composer.points["empty"];
    assert_eq!((p.dialog)(&composer.grid("empty")), None);
    // Transcript rows that mention a dialog's words, drawn above the composer
    // the way Codex draws its transcript.
    for text in [
        "• update the README",
        "› 1. Update now (runs `npm install -g @openai/codex`)",
        "  Update now",
        "• please login again",
        "  Trust this folder? trust",
        "  Folder access",
    ] {
        let mut bytes = composer.bytes[..empty_end].to_vec();
        bytes.extend_from_slice(format!("\x1b7\x1b[5;1H{text}\x1b8").as_bytes());
        let grid = Grid::parse(&bytes, composer.rows, composer.cols);
        assert_eq!((p.dialog)(&grid), None, "{text}");
    }
    // The dialog shape with a title the profile does not know.
    let trust = fixture("codex-inbox/r5wake/codex-0.164.0-trust-50x200");
    let mut bytes = trust.bytes.clone();
    bytes.extend_from_slice(b"\x1b7\x1b[2;3H\x1b[1mSomething new  \x1b[0m\x1b8");
    assert_eq!(
        (p.dialog)(&Grid::parse(&bytes, trust.rows, trust.cols)),
        Some(Dialog::Unrecognised)
    );
    // Claude Code: a numbered choice is a dialog it has not measured.
    let claude = fixture("claude-inbox/r5wake/claude-2.1.310-composer-24x80");
    let mut bytes = claude.bytes[..claude.points["empty"]].to_vec();
    bytes.extend_from_slice("\x1b[6;1H❯ 1. Yes".as_bytes());
    assert_eq!(
        (CLAUDE_PROFILE.dialog)(&Grid::parse(&bytes, claude.rows, claude.cols)),
        Some(Dialog::Unrecognised)
    );
    assert_eq!((CLAUDE_PROFILE.dialog)(&claude.grid("empty")), None);
}

/// Scenario: Parse a Claude Code frame and a Codex dialog frame at the width they were drawn at, at one column more and at one column fewer. The width witness holds at the drawn width only; and Claude Code's recorded resizes each start with its full-redraw marker.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_width_witness_and_redraw_marker() {
    let claude = fixture("claude-inbox/r5wake/claude-2.1.310-start-50x200");
    let end = claude.points["end"];
    let bytes = &claude.bytes[..end];
    assert!(witness(&CLAUDE_PROFILE, bytes, claude.rows, claude.cols));
    assert!(!witness(
        &CLAUDE_PROFILE,
        bytes,
        claude.rows,
        claude.cols + 1
    ));
    assert!(!witness(
        &CLAUDE_PROFILE,
        bytes,
        claude.rows,
        claude.cols - 1
    ));
    let update = fixture("codex-inbox/r5wake/codex-0.164.0-start-update-50x200");
    assert!(witness(
        &CODEX_PROFILE,
        &update.bytes,
        update.rows,
        update.cols
    ));
    assert!(!witness(
        &CODEX_PROFILE,
        &update.bytes,
        update.rows,
        update.cols + 1
    ));
    assert!(!witness(
        &CODEX_PROFILE,
        &update.bytes,
        update.rows,
        update.cols - 1
    ));
    // Codex before its first prompt: the centred logo at 50 × 200. At 24 × 80
    // Codex draws no logo, so its first frame there has no witness and the
    // deck holds until a dialog or a finished turn draws one.
    let start = fixture("codex-inbox/r5wake/codex-0.164.0-layout-start-50x200");
    let end = start.points["empty"];
    assert!(witness(
        &CODEX_PROFILE,
        &start.bytes[..end],
        start.rows,
        start.cols
    ));
    assert!(!witness(
        &CODEX_PROFILE,
        &start.bytes[..end],
        start.rows,
        start.cols + 2
    ));
    let small = fixture("codex-inbox/r5wake/codex-0.164.0-composer-24x80");
    assert!(!small.witness_upto(&CODEX_PROFILE, small.points["empty"]));
    // Claude Code's recorded resizes and nudges: each edge's bytes carry its
    // full-redraw marker, and the frame after it is a witness at the width in
    // force and not two columns wider.
    let layout = fixture("claude-inbox/r5wake/claude-2.1.310-layout-50x200");
    for i in 1..=10 {
        for kind in ["resize", "nudge"] {
            let mark = layout.points[&format!("mark:{kind}-{i}")];
            let shot = layout.points[&format!("{kind}-{i:02}")];
            assert!(
                find_marker(&layout.bytes[mark..shot], CLAUDE_PROFILE.redraw_marker).is_some(),
                "{kind}-{i}: no redraw"
            );
            assert!(layout.witness_upto(&CLAUDE_PROFILE, shot), "{kind}-{i}");
            assert!(
                !layout.widened().witness_upto(&CLAUDE_PROFILE, shot),
                "{kind}-{i}: witness at the wrong width"
            );
        }
    }
}

/// Scenario: Replay Codex's recorded resizes and one-column nudges, before and after a first prompt, on 0.164.0 and 0.160.0. After each edge the bytes Codex writes contain its full-redraw marker, and the frame five seconds later is a width witness at the width in force and not at two columns more.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_codex_layout_edges() {
    for stem in [
        "codex-inbox/r5wake/codex-0.164.0-layout-start-50x200",
        "codex-inbox/r5wake/codex-0.164.0-layout-turn-50x200",
        "codex-inbox/r5wake/codex-0.160.0-layout-start-50x200",
    ] {
        let trace = fixture(stem);
        let mut edges = 0;
        for i in 1..=10 {
            for kind in ["resize", "nudge"] {
                let mark = trace.points[&format!("mark:{kind}-{i}")];
                let shot = trace.points[&format!("{kind}-{i:02}")];
                assert!(
                    find_marker(&trace.bytes[mark..shot], CODEX_PROFILE.redraw_marker).is_some(),
                    "{stem} {kind}-{i}: no redraw"
                );
                let (rows, cols) = trace.size_at(shot);
                let grid = trace.grid_upto(shot);
                assert_eq!((grid.rows, grid.cols), (rows, cols));
                assert!(
                    trace.witness_upto(&CODEX_PROFILE, shot),
                    "{stem} {kind}-{i}: no witness"
                );
                // The same bytes on a terminal two columns wider.
                assert!(
                    !trace.widened().witness_upto(&CODEX_PROFILE, shot),
                    "{stem} {kind}-{i}: witness at the wrong width"
                );
                edges += 1;
            }
        }
        assert_eq!(edges, 20);
    }
}

/// Scenario: Ask the strict withdrawal recogniser about a Codex frame holding the deck's line with the cursor moved off it, about owned text without a whole token, and about a pane narrower than the measurements. Each is refused with its reason.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_strict_withdrawal_refusals() {
    let trace = fixture("codex-inbox/r5wake/codex-0.164.0-composer-24x80");
    let line = &trace.lines[0];
    let end = trace.points["typed-01"];
    let mut moved = trace.bytes[..end].to_vec();
    moved.extend_from_slice(b"\x1b[1;1H");
    let grid = Grid::parse(&moved, trace.rows, trace.cols);
    assert_eq!(
        strict_withdrawal(&CODEX_PROFILE, &grid, line),
        Err(WithdrawalRefusal::CursorElsewhere)
    );
    let typed = trace.grid("typed-01");
    assert_eq!(
        strict_withdrawal(&CODEX_PROFILE, &typed, &line[1..]),
        Err(WithdrawalRefusal::NoWholeToken)
    );
    let narrow = Grid::parse(&trace.bytes[..end], trace.rows, 60);
    assert_eq!(
        strict_withdrawal(&CODEX_PROFILE, &narrow, line),
        Err(WithdrawalRefusal::Narrow)
    );
}

/// Scenario: Take each recorded Codex release's measured key effects and ask the table about a record whose every other measurement is verified, its head echo included. Every byte of the line's first stage, the token head `(xxxxxx)`, is inert in every dialog, so the release is Go and staged with a first stage of eight characters; the same record with a token letter made to commit in folder trust is first_stage_commits naming that dialog and key, and without its head echo it is missing_measurement.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_go_no_go_over_measured_key_effects() {
    use dot_agent_deck::doorbell_activation::{DialogKeys, KeyEffect, first_stage_bytes};
    for record in RELEASE_RECORDS
        .iter()
        .filter(|r| r.agent == AgentType::Codex)
    {
        let Measurement::Verified(dialogs) = record.key_effects else {
            panic!("{}: key effects measured", record.release);
        };
        // Every key the line can contain, plus DEL and CR, in every dialog.
        let line_keys: Vec<u8> = (b'a'..=b'z')
            .chain(b'0'..=b'9')
            .chain(*b"-: (),")
            .chain([0x7f, b'\r'])
            .collect();
        for dialog in dialogs {
            for key in &line_keys {
                assert!(
                    dialog.keys.iter().any(|(k, _)| k == key),
                    "{} {}: key {key:#04x} unmeasured",
                    record.release,
                    dialog.dialog
                );
            }
        }
        let complete = |key_effects, head_echo| ReleaseRecord {
            ready_composer: Measurement::Verified(()),
            echo: Measurement::Verified(()),
            submit_route: Measurement::Verified(()),
            layout: Measurement::Verified(()),
            key_effects,
            input_mode: Measurement::Verified(()),
            release_recogniser: Measurement::Verified(()),
            parser_cost: Measurement::Verified(()),
            head_echo,
            ..record.clone()
        };
        assert_eq!(first_stage_bytes().count(), TOKEN_HEAD_ALPHABET);
        assert!(
            matches!(
                exact_verdict(&complete(
                    Measurement::Verified(dialogs),
                    Measurement::Verified(())
                )),
                Activation::Go { staged: true, .. }
            ),
            "{}",
            record.release
        );
        assert_eq!(
            exact_verdict(&complete(
                Measurement::Verified(dialogs),
                Measurement::Absent
            )),
            Activation::NoGo(NoGo::MissingMeasurement("head_echo")),
            "{}",
            record.release
        );
        // `first_stage_commits` still holds a release whose dialog commits on
        // a token letter.
        let trust: Vec<(u8, KeyEffect)> = dialogs
            .iter()
            .find(|d| d.dialog == "trust")
            .expect("folder trust measured")
            .keys
            .iter()
            .map(|&(k, e)| (k, if k == b'p' { KeyEffect::Commits } else { e }))
            .collect();
        let edited: Vec<DialogKeys<'_>> = dialogs
            .iter()
            .map(|d| {
                if d.dialog == "trust" {
                    DialogKeys {
                        dialog: "trust",
                        keys: &trust,
                    }
                } else {
                    *d
                }
            })
            .collect();
        assert_eq!(
            exact_verdict(&complete(
                Measurement::Verified(&edited),
                Measurement::Verified(())
            )),
            Activation::NoGo(NoGo::FirstStageCommits {
                dialog: "trust".into(),
                key: b'p'
            }),
            "{}",
            record.release
        );
    }
}

/// The bytes the token head can contain: two parentheses and sixteen letters.
const TOKEN_HEAD_ALPHABET: usize = 18;

/// Scenario: Replay the staged composer series recorded on Codex 0.164.0 and 0.160.0 at 24 × 80 through the frame builder the transaction reads. On every trial the screen after the head's one write shows only the placeholder or a prefix of the head until the composer holds exactly the eight-character head with the cursor after it, and no dialog; after the rest's write it shows only prefixes of the line until it holds exactly the whole line; after the Enter a frame shows the line as a user message with the composer no longer holding it. On every head-only trial the composer holds exactly the head, and after the eight deletes it shows the placeholder again.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_the_staged_head_reads_through_the_profile() {
    use dot_agent_deck::doorbell_screen::{TOKEN_HEAD_CHARS, frame};
    for (stem, release) in [
        ("codex-inbox/r5wake/codex-0.164.0-staged-24x80", "0.164.0"),
        ("codex-inbox/r5wake/codex-0.160.0-staged-24x80", "0.160.0"),
    ] {
        let trace = fixture(stem);
        assert_eq!(
            latched(&trace),
            (AgentType::Codex, Version::parse(release).unwrap())
        );
        let trials: Vec<&String> = trace
            .lines
            .iter()
            .filter(|l| l.chars().count() > TOKEN_HEAD_CHARS)
            .collect();
        let heads: Vec<&String> = trace
            .lines
            .iter()
            .filter(|l| l.chars().count() == TOKEN_HEAD_CHARS)
            .collect();
        assert_eq!((trials.len(), heads.len()), (10, 3), "{stem}");
        let read = |grid: &Grid, line: &str| frame(&CODEX_PROFILE, grid, 1, true, Some(line));
        for (i, line) in trials.iter().enumerate() {
            let n = i + 1;
            let head = &line[..TOKEN_HEAD_CHARS];
            let typed_head = trace.points[&format!("mark:trial-{n}")];
            let typed_rest = trace.points[&format!("mark:rest-{n}")];
            let arriving = trace.reads_between(typed_head, typed_rest);
            for grid in &arriving {
                let f = read(grid, line);
                assert!(f.dialog.is_none(), "{stem} trial {n}: a dialog");
                if let Some(c) = &f.composer {
                    assert!(
                        c.placeholder || head.starts_with(c.text.as_str()),
                        "{stem} trial {n}: {:?} before the head's echo",
                        c.text
                    );
                }
            }
            let echoed = read(&trace.grid_upto(typed_rest), line);
            assert!(
                echoed.holds_exactly(head),
                "{stem} trial {n}: the head's echo {echoed:?}"
            );
            for grid in trace.reads_between(typed_rest, trace.points[&format!("typed-{n:02}")]) {
                if let Some(c) = &read(&grid, line).composer {
                    assert!(
                        line.starts_with(c.text.as_str()),
                        "{stem} trial {n}: {:?} before the line's echo",
                        c.text
                    );
                }
            }
            assert!(
                read(&trace.grid(&format!("typed-{n:02}")), line).holds_exactly(line),
                "{stem} trial {n}: the line's echo"
            );
            let submitted = trace
                .reads_between(
                    trace.points[&format!("mark:cr-{n}")],
                    trace.points[&format!("after-{n:02}")],
                )
                .iter()
                .map(|grid| read(grid, line))
                .any(|f| f.user_cells == vec![line.to_string()] && !f.holds_exactly(line));
            assert!(submitted, "{stem} trial {n}: the line as a user message");
        }
        for (i, head) in heads.iter().enumerate() {
            let n = i + 1;
            let before = read(
                &trace.grid_upto(trace.points[&format!("mark:deletes-{n}")]),
                head,
            );
            assert!(before.holds_exactly(head), "{stem} delete {n}: {before:?}");
            let after = read(&trace.grid(&format!("deleted-{n:02}")), head);
            assert!(after.empty_and_ready(), "{stem} delete {n}: {after:?}");
            assert!(
                after.composer.as_ref().is_some_and(|c| c.placeholder),
                "{stem} delete {n}: the placeholder"
            );
        }
    }
}

/// Scenario: Read recorded Claude Code 2.1.310 and Codex composer launches through the frame builder the doorbell's transaction reads: the empty composer, the composer holding a typed token-led line, the screen after that line was submitted, and the same screens as a state that is not Clean. The empty composer is empty and ready; the typed line is held exactly, cursor at its end, and is no user message; after its submit a frame shows it as a user message with the composer no longer holding it; a frame that is not Clean has no composer and no dialog, so nothing is decided on it.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_the_transaction_reads_real_frames_through_the_profile() {
    use dot_agent_deck::doorbell_screen::frame;
    for (stem, profile) in [
        (
            "claude-inbox/r5wake/claude-2.1.310-composer-24x80",
            &CLAUDE_PROFILE,
        ),
        (
            "codex-inbox/r5wake/codex-0.164.0-composer-24x80",
            &CODEX_PROFILE,
        ),
    ] {
        let trace = fixture(stem);
        let line = &trace.lines[0];
        let empty = frame(profile, &trace.grid("empty"), 3, true, Some(line));
        assert!(empty.empty_and_ready(), "{stem}: empty");
        assert_eq!(empty.epoch, 3);
        assert!(empty.user_cells.is_empty());
        let typed = frame(profile, &trace.grid("typed-01"), 3, true, Some(line));
        assert!(typed.holds_exactly(line), "{stem}: typed {typed:?}");
        assert!(!typed.empty_and_ready());
        assert!(
            typed.user_cells.is_empty(),
            "{stem}: the composer is no user cell"
        );
        let submitted = trace
            .reads_between(trace.points["mark:cr-1"], trace.points["after-01"])
            .iter()
            .map(|grid| frame(profile, grid, 3, true, Some(line)))
            .find(|f| f.user_cells == vec![line.clone()]);
        let submitted = submitted.unwrap_or_else(|| panic!("{stem}: the line as a user message"));
        assert!(!submitted.holds_exactly(line));
        let dirty = frame(profile, &trace.grid("typed-01"), 4, false, Some(line));
        assert!(!dirty.clean);
        assert!(dirty.composer.is_none() && dirty.dialog.is_none());
        assert!(
            dirty.text.contains("dot-agent-deck: message"),
            "text is still read"
        );
    }
}
