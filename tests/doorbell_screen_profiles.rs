//! `inbox/doorbell/036`: the doorbell's screen profiles and the release
//! recogniser over raw captures of real launches (M1, M2), read through the
//! lossless grid (`src/doorbell_screen.rs`, `src/release_scanner.rs`).
use std::collections::HashMap;
use std::path::PathBuf;

use dot_agent_deck::doorbell_activation::{
    AGENT_FACTS, Activation, AgentFacts, Measurement, NoGo, RECORDED_RELEASES, facts_verdict,
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

#[path = "support/installed_release.rs"]
mod installed_release;

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

/// Scenario: Replay the recorded start-up of Codex 0.164.0, Codex 0.160.0 and Claude Code 2.1.310, 2.1.312, 2.1.317 and 2.1.318 through the release scanner at the PTY reader's recorded split, one byte at a time, and as one push. Each latches its own release every way.
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
            "claude-inbox/r5wake/claude-2.1.312-start-50x200",
            AgentType::ClaudeCode,
            "2.1.312",
        ),
        (
            "claude-inbox/r5wake/claude-2.1.312-composer-24x80",
            AgentType::ClaudeCode,
            "2.1.312",
        ),
        (
            "claude-inbox/r5wake/claude-2.1.317-start-50x200",
            AgentType::ClaudeCode,
            "2.1.317",
        ),
        (
            "claude-inbox/r5wake/claude-2.1.317-composer-24x80",
            AgentType::ClaudeCode,
            "2.1.317",
        ),
        (
            "claude-inbox/r5wake/claude-2.1.318-start-50x200",
            AgentType::ClaudeCode,
            "2.1.318",
        ),
        (
            "claude-inbox/r5wake/claude-2.1.318-composer-24x80",
            AgentType::ClaudeCode,
            "2.1.318",
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

/// Scenario: Read the `--version` output a lane-2 measurement gets from an installed Claude Code or Codex: a bare release, a pre-release, a build tag and no version. Only a bare release is usable; then the same versions are drawn in each agent's start-up header, and the deck's release scanner latches exactly the versions the measurements accept.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_installed_release_shape_follows_the_release_scanner() {
    use installed_release::{VersionShape, version_shape};
    let release = |v: &str| VersionShape::Release(v.to_string());
    let tagged = |v: &str| VersionShape::Tagged(v.to_string());
    for (program, output, expected) in [
        ("claude", "2.1.319 (Claude Code)\n", release("2.1.319")),
        (
            "claude",
            "2.1.320-beta.1 (Claude Code)\n",
            tagged("2.1.320-beta.1"),
        ),
        (
            "claude",
            "2.1.320+abc (Claude Code)\n",
            tagged("2.1.320+abc"),
        ),
        ("claude", "Claude Code 2.1.319\n", VersionShape::NoRelease),
        ("claude", "", VersionShape::NoRelease),
        ("codex", "codex-cli 0.164.0\n", release("0.164.0")),
        (
            "codex",
            "codex-cli 0.165.0-alpha.2\n",
            tagged("0.165.0-alpha.2"),
        ),
        ("codex", "codex-cli\n", VersionShape::NoRelease),
    ] {
        assert_eq!(
            version_shape(program, output),
            expected,
            "{program} --version printed {output:?}"
        );
    }
    // What a measurement accepts is exactly what the deck latches from the
    // same version in the agent's header: a bare release, nothing tagged.
    for version in [
        "2.1.319",
        "0.164.0",
        "2.1.320-beta.1",
        "2.1.320+abc",
        "0.165.0-alpha.2",
        "2.1.320-rc.1+abc",
        "2.01.319",
        "2.1",
    ] {
        for (program, output, header) in [
            (
                "claude",
                format!("{version} (Claude Code)\n"),
                format!("\x1b[2;12HClaude Code v{version}\r\n"),
            ),
            (
                "codex",
                format!("codex-cli {version}\n"),
                format!("\x1b[2;6HOpenAI Codex (v{version})\r\n"),
            ),
        ] {
            let latched = match latch_of(header.as_bytes()) {
                ScanLatch::Latched { release, .. } => Some(release.to_string()),
                _ => None,
            };
            let accepted = match version_shape(program, &output) {
                VersionShape::Release(release) => Some(release),
                _ => None,
            };
            assert_eq!(
                accepted, latched,
                "{program} {version}: the measurement and the release scanner disagree"
            );
        }
    }
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

/// Scenario: Replay the recorded Claude releases' composers and menu surfaces at every recorded size and palette; the main composer stays ready with warning phrases quoted in its transcript, while overview, menus, shell mode and exit warning refuse readiness. Compound-key captures reveal retained spaces and preserve the draft after blank Enter. Replay every Claude fixture at every recorded PTY read boundary with production parsing and recorded resizes, pinning each composer-bearing frame's ready and empty-ready decisions in the committed ledger.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_claude_composer_over_real_frames() {
    for release in ["2.1.310", "2.1.312", "2.1.317", "2.1.318"] {
        let trace = fixture(&format!(
            "claude-inbox/r5wake/claude-{release}-composer-24x80"
        ));
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
    for release in ["2.1.312", "2.1.317", "2.1.318"] {
        let keys = fixture(&format!(
            "claude-inbox/r5wake/claude-{release}-compound-keys-50x200"
        ));
        for (point, expected) in [("reviewer-reveal", "X  Z"), ("end", "X   Z")] {
            assert_eq!(
                (CLAUDE_PROFILE.composer)(&keys.grid(point)).unwrap().text,
                expected,
                "{release} {point}"
            );
        }
        let before = keys.grid("blank-enter-before");
        let after = keys.grid("blank-enter-after");
        assert_eq!(before.cursor, after.cursor, "{release}");
        assert_eq!(
            (CLAUDE_PROFILE.composer)(&before),
            (CLAUDE_PROFILE.composer)(&after),
            "{release}"
        );
    }

    let mut wrong_surfaces = Vec::new();
    for release in ["2.1.310", "2.1.312", "2.1.317", "2.1.318"] {
        for set in [
            "50x200",
            "24x80",
            "40x120",
            "light-50x200",
            "truecolor-dark-50x200",
            "truecolor-light-50x200",
            "dark-ansi-50x200",
            "light-ansi-50x200",
            "dark-daltonized-50x200",
        ] {
            let trace = fixture(&format!("claude-inbox/r5wake/claude-{release}-menus-{set}"));
            for (point, expected) in [
                ("empty", true),
                ("transcript-quotation", true),
                ("overview", false),
                ("slash", false),
                ("at", false),
                ("bang", false),
                ("exit-warning", false),
            ] {
                let grid = trace.grid(point);
                let ready = (CLAUDE_PROFILE.ready)(&grid);
                let empty_and_ready = CLAUDE_PROFILE.empty_and_ready(&grid);
                eprintln!(
                    "Claude {release} {set} {point}: ready={ready}, empty_and_ready={empty_and_ready}, expected {expected}"
                );
                if ready != expected || empty_and_ready != expected {
                    let failure = if expected {
                        "wrong refusal (availability)"
                    } else {
                        "wrong acceptance (safety)"
                    };
                    wrong_surfaces.push(format!(
                        "Claude {release} {set} {point}: {failure}: ready={ready}, empty_and_ready={empty_and_ready}, expected {expected}\n{}",
                        trace.parser_upto(trace.points[point]).screen().contents()
                    ));
                }
            }
            let quotation = trace
                .parser_upto(trace.points["transcript-quotation"])
                .screen()
                .contents()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            for phrase in [
                "Your conversation moved to the background",
                "describe a task for a new session",
                "Press Ctrl-C again to exit",
            ] {
                assert!(
                    quotation.contains(phrase),
                    "missing real transcript quote {phrase:?}"
                );
            }
        }
    }
    assert!(
        wrong_surfaces.is_empty(),
        "Claude must recognise the main composer and refuse other input surfaces:\n{}",
        wrong_surfaces.join("\n\n")
    );
    assert_claude_corpus_decisions();
}

/// Scenario: Replay every committed menu capture of Claude Code 2.1.310, 2.1.312, 2.1.317 and 2.1.318 at its recorded size and palette, and read the foreground of the main composer's upper rule, the shell-mode glyph and the overview's prompt where each was drawn. Each must be the colour the live acquisition measured for that palette; the live test prints these for the installed release rather than asserting them, because the screen profile reads only which of rule and prompt is in the default colour.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_committed_claude_menu_captures_draw_the_recorded_palettes() {
    use vt100::Color::{Idx, Rgb};

    // The main composer's upper rule, the shell glyph and the overview's own
    // prompt glyph, as `doorbell_036_live_release_menu_frames` asserted them
    // on each recorded release before it measured the installed one.
    let indexed_dark = (Idx(244), Idx(211), Idx(246));
    for release in ["2.1.310", "2.1.312", "2.1.317", "2.1.318"] {
        for (set, (rule, shell, overview)) in [
            ("24x80", indexed_dark),
            ("40x120", indexed_dark),
            ("50x200", indexed_dark),
            ("light-50x200", (Idx(245), Idx(199), Idx(241))),
            (
                "truecolor-dark-50x200",
                (Rgb(136, 136, 136), Rgb(253, 93, 177), Rgb(153, 153, 153)),
            ),
            (
                "truecolor-light-50x200",
                (Rgb(138, 138, 138), Rgb(255, 0, 135), Rgb(102, 102, 102)),
            ),
            ("dark-ansi-50x200", (Idx(7), Idx(13), Idx(7))),
            ("light-ansi-50x200", (Idx(7), Idx(5), Idx(8))),
            ("dark-daltonized-50x200", (Idx(244), Idx(75), Idx(246))),
        ] {
            let trace = fixture(&format!("claude-inbox/r5wake/claude-{release}-menus-{set}"));
            let rows = trace.rows;
            let foreground = |point: &str, row: u16| {
                trace
                    .parser_upto(trace.points[point])
                    .screen()
                    .cell(row, 0)
                    .unwrap()
                    .fgcolor()
            };
            assert_eq!(
                foreground("empty", rows - 4),
                rule,
                "{release} {set} main rule"
            );
            assert_eq!(
                foreground("bang", rows - 3),
                shell,
                "{release} {set} shell glyph"
            );
            assert_eq!(
                foreground("overview", rows - 3),
                overview,
                "{release} {set} overview prompt"
            );
        }
    }
}

/// Scenario: Read every committed capture index and measurement summary under `tests/fixtures/claude-inbox/` that names the release it was recorded on and its executable's SHA-256. Each must name the hash of that release's executable as the recorders pinned it before the live measurements moved to the installed release, and each of the four recorded releases must be named somewhere.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_committed_claude_captures_name_their_recorded_executable() {
    // The recorded releases' executables, as the recorders under
    // `tests/support/` pinned them until issue #2146.
    const RECORDED: [(&str, &str); 4] = [
        (
            "2.1.310",
            "b73676430af12f97316a798b8371f81d640f90a58bdcb8f3ce56a835bb47df56",
        ),
        (
            "2.1.312",
            "4f4178ce41d83e23341ebfbaecdbcf8ea6bf7baea0690025abe49eeec3a5d01d",
        ),
        (
            "2.1.317",
            "eb224419e344575faa4a1f39b290b649425a10cd5ee2ef1d003480cd9ae57eb0",
        ),
        (
            "2.1.318",
            "708c77b204db30086ff6f1ed34577bda03e1be74ced80557cbf7b54249ebf0bd",
        ),
    ];
    fn walk(dir: &std::path::Path, found: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, found);
            } else if path.extension().is_some_and(|e| e == "json") {
                found.push(path);
            }
        }
    }
    let mut files = Vec::new();
    walk(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude-inbox"),
        &mut files,
    );
    let mut seen: HashMap<&str, usize> = HashMap::new();
    for file in &files {
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
        let (Some(release), Some(hash)) = (
            value["release"].as_str(),
            value["executable_sha256"]
                .as_str()
                .or(value["sha256"].as_str()),
        ) else {
            continue;
        };
        let pinned = RECORDED
            .iter()
            .find(|(recorded, _)| *recorded == release)
            .unwrap_or_else(|| {
                panic!(
                    "{}: a committed capture of {release}, a release with no recorded hash here",
                    file.display()
                )
            });
        assert_eq!(hash, pinned.1, "{}: {release}", file.display());
        *seen.entry(pinned.0).or_default() += 1;
    }
    for (release, _) in RECORDED {
        assert!(
            seen.get(release).is_some_and(|n| *n > 0),
            "no committed capture names {release}: {seen:?}"
        );
    }
    eprintln!("committed Claude Code captures per recorded executable: {seen:?}");
}

/// Scenario: Replay the history-recall captures of Claude Code 2.1.317 and 2.1.318 at 50×200 and 24×80: Up from the empty composer draws `─── History i/n` in place of the composer's upper rule over the recalled prompt, Ctrl+U on a recalled prompt draws the plain composer while Claude keeps its history position, and walking straight back to that cleared prompt draws a History rule over a blank box with the cursor at the indent. The profile reads no composer, no readiness and no empty box in any History-ruled frame, and the two releases draw the same rule and prompt rows after the first frame; were the label read as a rule, the blank-slot frame would be the empty ready composer.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_claude_history_recall_frames_are_refused() {
    use dot_agent_deck::doorbell_screen::frame;
    let p = &CLAUDE_PROFILE;
    for size in ["50x200", "24x80"] {
        let mut drawn: Vec<Vec<(String, String, String)>> = Vec::new();
        for release in ["2.1.317", "2.1.318"] {
            let trace = fixture(&format!(
                "claude-inbox/r5wake/claude-{release}-history-{size}"
            ));
            // The upper-rule row and the prompt row of the last `❯` composer.
            let rows = |grid: &Grid| -> Option<(u16, String, String)> {
                let prompt = (1..grid.rows)
                    .rev()
                    .find(|&r| grid.cell(r, 0).is_some_and(|c| c.text == "❯"))?;
                Some((
                    prompt,
                    grid.text_from(prompt - 1, 0),
                    grid.text_from(prompt, 0)
                        .replace('\u{a0}', " ")
                        .trim_end()
                        .to_string(),
                ))
            };
            let mut names: Vec<(&String, &usize)> = trace
                .points
                .iter()
                .filter(|(name, _)| !name.starts_with("mark:"))
                .collect();
            names.sort_by_key(|&(name, at)| (*at, name.clone()));
            let mut seen = Vec::new();
            let mut history = 0;
            for (name, _) in names {
                let grid = trace.grid(name);
                let Some((prompt, rule, row)) = rows(&grid) else {
                    continue;
                };
                // The fresh composer's `Try "…"` placeholder is chosen at
                // random per launch, so the first frame is not compared.
                if name != "empty" {
                    seen.push((name.clone(), rule.clone(), row.clone()));
                }
                if !rule.starts_with("─── History ") {
                    continue;
                }
                history += 1;
                assert!(
                    (p.composer)(&grid).is_none() && !(p.ready)(&grid) && !p.empty_and_ready(&grid),
                    "{release} {size} {name}: a History-ruled frame must be refused"
                );
                assert!(
                    frame(p, &grid, 0, true, None).composer.is_none(),
                    "{release} {size} {name}: admission reads no composer under a History rule"
                );
                if row == "❯" {
                    assert_eq!(grid.cursor, (prompt, 2), "{release} {size} {name}");
                    let mut as_rule = grid.clone();
                    for cell in &mut as_rule.cells[prompt as usize - 1] {
                        cell.text = "─".to_string();
                    }
                    assert!(
                        p.empty_and_ready(&as_rule),
                        "{release} {size} {name}: with its label read as a rule, this blank box is the empty ready composer"
                    );
                }
            }
            let at = |point: &str| {
                seen.iter()
                    .find(|(name, ..)| name == point)
                    .map(|(_, rule, row)| (rule.clone(), row.clone()))
                    .unwrap_or_else(|| panic!("{release} {size}: no point {point}"))
            };
            let (rule, row) = at("up-1");
            assert!(
                rule.starts_with("─── History 6/6 "),
                "{release} {size} up-1 {rule:?}"
            );
            assert_eq!(row, "❯ HISTORYSIXfoxtrot");
            let (rule, row) = at("up-2");
            assert!(
                rule.starts_with("─── History 5/6 "),
                "{release} {size} up-2 {rule:?}"
            );
            assert_eq!(row, "❯ HISTORYFIVEecho");
            for blank in ["slot-down-1", "slot-up-2", "slot-down-3"] {
                let (rule, row) = at(blank);
                assert!(
                    rule.starts_with("─── History ") && row == "❯",
                    "{release} {size} {blank}: the slot walked back to after Ctrl+U {rule:?} {row:?}"
                );
            }
            assert!(
                !at("after-ctrl-u").0.contains("History"),
                "{release} {size}: Ctrl+U on a recalled prompt draws the plain composer (the history position is kept: the next Up recalls the next-older entry)"
            );
            assert!(p.empty_and_ready(&trace.grid("after-ctrl-u")));
            assert_eq!(
                history, 24,
                "{release} {size}: History-ruled points (24 in every committed capture)"
            );
            drawn.push(seen);
        }
        assert_eq!(
            drawn[0], drawn[1],
            "{size}: 2.1.317 and 2.1.318 draw the same rule and prompt rows at every point after the first"
        );
    }
}

/// Scenario: Replay the Q143b captures of real Claude Code 2.1.317 and 2.1.318 at 50×200 and 24×80, five trials for each release, size, case and press count: a recalled prompt cleared with one or four Ctrl+U presses, then the deck-shaped line typed in one write and a lone Enter, either straight away (the plain rule) or after Up and Down back onto the cleared slot (`History k/6`). The profile reads the plain-drawn state as empty and ready and the History-labelled one as no composer, reads the typed line as held exactly before the Enter, and each hook payload is the line alone, with the six seeded history entries recalled intact afterwards.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_claude_history_submit_replay() {
    use sha2::Digest;
    let p = &CLAUDE_PROFILE;
    let seeded = [
        "HISTORYSIXfoxtrot",
        "HISTORYFIVEecho",
        "HISTORYFOURdelta",
        "HISTORYTHREEcharlie",
        "HISTORYTWObravo",
        "HISTORYONEalpha",
    ];
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude-inbox/history-recall-submit");
    let mut trials = 0;
    let mut rung = 0;
    for release in ["2.1.317", "2.1.318"] {
        let mut stems: Vec<String> = std::fs::read_dir(base.join(release))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .map(|path| path.file_stem().unwrap().to_str().unwrap().to_owned())
            .collect();
        stems.sort();
        for stem in stems {
            let name = format!("{release}/{stem}");
            let trace = fixture(&format!("claude-inbox/history-recall-submit/{name}"));
            let record: serde_json::Value =
                serde_json::from_slice(&std::fs::read(base.join(format!("{name}.json"))).unwrap())
                    .unwrap();
            assert_eq!(record["release"], release, "{name}");
            assert_eq!(
                trace.read_lens.iter().sum::<usize>(),
                trace.bytes.len(),
                "{name}"
            );
            assert_eq!(
                record["trace_sha256"].as_str().unwrap(),
                sha2::Sha256::digest(&trace.bytes)
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>(),
                "{name}"
            );
            trials += 1;
            let outcome = &record["outcome"];
            let line = outcome["line"].as_str().unwrap();
            let labelled = outcome["case"] == "labelled";
            // The plain-drawn state after Ctrl+U, blank or placeholder, is
            // the empty ready composer to the profile.
            assert!(
                p.empty_and_ready(&trace.grid("cleared")),
                "{name}: the plain-drawn cleared state reads empty and ready"
            );
            if outcome["reachable"] == false {
                assert_eq!(record["hook_prompts"], serde_json::json!([]), "{name}");
                continue;
            }
            rung += 1;
            let before = trace.grid("before-ring");
            if labelled {
                assert!(
                    (p.composer)(&before).is_none() && !p.empty_and_ready(&before),
                    "{name}: the History-labelled cleared slot is refused"
                );
            } else {
                assert!(p.empty_and_ready(&before), "{name}");
            }
            assert_eq!(
                p.holds(&trace.grid("typed"), line),
                Some(ComposerText::Exact),
                "{name}: the line held exactly before the Enter"
            );
            assert_eq!(
                record["hook_prompts"],
                serde_json::json!([line]),
                "{name}: one submission, the line alone"
            );
            assert!(p.empty_and_ready(&trace.grid("after-clear")), "{name}");
            assert_eq!(
                outcome["recalled"],
                serde_json::json!(seeded),
                "{name}: the history afterwards"
            );
        }
    }
    assert_eq!(
        trials, 80,
        "2 releases × 2 cases × 2 press counts × 2 sizes × 5 trials"
    );
    assert!(rung > 0);
}

/// Characterisation at recorded read boundaries, not at arbitrary byte splits.
/// The fixture name, one-based read number, byte offset and current geometry
/// identify each frame. Include every frame with a production composer,
/// including populated editors and other surfaces sharing the composer shape.
fn assert_claude_corpus_decisions() {
    use std::fmt::Write;

    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude-inbox/r5wake");
    let mut stems: Vec<_> = std::fs::read_dir(&base)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "bin"))
        .map(|path| path.file_stem().unwrap().to_str().unwrap().to_owned())
        .collect();
    stems.sort();
    assert!(!stems.is_empty(), "Claude corpus must not be empty");
    let mut actual = String::from(
        "# Production Grid at recorded PTY read boundaries; resizes apply before the next byte.\n\
         # Frame = fixture/read-number@end-offset:rowsxcols; columns = ready empty-and-ready.\n\
         # Characterisation of current production decisions, not an independent verdict per row; no claim about arbitrary splits or unknown screens.\n\
         # Update only with a reviewed recogniser change; name the moved frames in the PR or report.\n",
    );
    for stem in &stems {
        let trace = fixture(&format!("claude-inbox/r5wake/{stem}"));
        assert_eq!(
            trace.read_lens.iter().sum::<usize>(),
            trace.bytes.len(),
            "{stem}"
        );
        let mut parser = dot_agent_deck::vt_grid::parser(trace.rows, trace.cols, 0);
        let mut edges = trace.edges.iter().peekable();
        let mut offset = 0;
        for (read, len) in trace.read_lens.iter().enumerate() {
            let end = offset + len;
            while let Some(&&(at, rows, cols)) = edges.peek().filter(|edge| edge.0 < end) {
                assert!(at >= offset, "{stem}: resize edges must be ordered");
                parser.process(&trace.bytes[offset..at]);
                parser
                    .screen_mut()
                    .set_size(rows.try_into().unwrap(), cols.try_into().unwrap());
                offset = at;
                edges.next();
            }
            parser.process(&trace.bytes[offset..end]);
            offset = end;
            let grid = Grid::of(parser.screen(), end as u64);
            if (CLAUDE_PROFILE.composer)(&grid).is_some() {
                let decision = |ready| if ready { "ready" } else { "refused" };
                writeln!(
                    actual,
                    "{stem}/{}@{end}:{}x{} {} {}",
                    read + 1,
                    grid.rows,
                    grid.cols,
                    decision((CLAUDE_PROFILE.ready)(&grid)),
                    decision(CLAUDE_PROFILE.empty_and_ready(&grid))
                )
                .unwrap();
            }
        }
    }
    let expected = include_str!("fixtures/claude-inbox/r5wake/claude-ready.expectations.txt");
    if actual != expected {
        // A separate candidate never updates the oracle. Review its frame-level
        // diff before explicitly replacing the committed expectations.
        let artifact_dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
        let candidate = artifact_dir.join("claude-ready.actual.txt");
        let saved = std::fs::create_dir_all(&artifact_dir)
            .and_then(|()| std::fs::write(&candidate, &actual));
        let first = actual
            .lines()
            .zip(expected.lines())
            .position(|(a, e)| a != e)
            .unwrap_or_else(|| actual.lines().count().min(expected.lines().count()));
        panic!(
            "Claude corpus decision changed at ledger line {}: expected {:?}, actual {:?}; expected {} lines, actual {} lines over {} fixtures; candidate {}: {:?}",
            first + 1,
            expected.lines().nth(first),
            actual.lines().nth(first),
            expected.lines().count(),
            actual.lines().count(),
            stems.len(),
            candidate.display(),
            saved
        );
    }
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

/// Scenario: Replay recorded Claude frames and synthesize blank and multiline drafts, including spaces with the cursor returned to the start. An untouched frame may admit a visually empty box but must refuse visible multiline content; recorded empty boxes remain eligible. The touched-box authority is exercised by inbox/probe/024 and 025 through real checks.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_blanks_and_blank_lines_the_claude_screen_cannot_show() {
    use dot_agent_deck::doorbell_screen::frame;
    let trace = fixture("claude-inbox/r5wake/claude-2.1.310-composer-24x80");
    let p = &CLAUDE_PROFILE;
    let at = trace.points["cleared-01"];
    let with = |extra: &[u8]| {
        let mut bytes = trace.bytes[..at].to_vec();
        bytes.extend_from_slice(extra);
        Grid::parse(&bytes, trace.rows, trace.cols)
    };
    let ready = |grid: &Grid| frame(p, grid, 0, true, None).empty_and_ready();
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
    assert!(ready(&moved_back));
    let rule = "\u{2500}".repeat(usize::from(trace.cols));
    // Spaces written as cells, the cursor moved back: a real 2.1.310 draws
    // this for spaces it holds after a repaint, and for the empty box once
    // `End` then `Ctrl+U` cleared them, which writes blanks over them
    // (measured 2026-10-26). The observer must distinguish their contents.
    let written = with(b"   \x1b[3D");
    let composer = (p.composer)(&written).expect("the composer");
    assert_eq!(composer.text, "");
    assert!(composer.cursor_at_end && !composer.hidden, "{composer:?}");
    assert!(
        ready(&written),
        "an untouched frame may admit the blank screen"
    );
    for (case, extra) in [
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
        assert!(!ready(&grid), "{case}");
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
            assert!(ready(&grid), "{stem} {name}");
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

/// Scenario: Parse Claude Code 2.1.310, 2.1.312, 2.1.317 and 2.1.318 frames and a Codex dialog frame at the width they were drawn at, at one column more and at one column fewer. The width witness holds at the drawn width only; and every recorded Claude release's resizes each start with the same full-redraw marker.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_width_witness_and_redraw_marker() {
    for release in ["2.1.310", "2.1.312", "2.1.317", "2.1.318"] {
        let claude = fixture(&format!(
            "claude-inbox/r5wake/claude-{release}-start-50x200"
        ));
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
    }
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
    for release in ["2.1.310", "2.1.312", "2.1.317", "2.1.318"] {
        let layout = fixture(&format!(
            "claude-inbox/r5wake/claude-{release}-layout-50x200"
        ));
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

/// Scenario: Take each recorded Codex release's measured key effects, and Codex's facts' own, and ask the table about facts whose every other measurement is verified, its head echo included. Every byte of the line's first stage, the token head `(xxxxxx)`, is inert in every dialog, so the release is Go and staged with a first stage of eight characters; the same record with a token letter made to commit in folder trust is first_stage_commits naming that dialog and key, and without its head echo it is missing_measurement.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_go_no_go_over_measured_key_effects() {
    use dot_agent_deck::doorbell_activation::{DialogKeys, KeyEffect, first_stage_bytes};
    let tables = RECORDED_RELEASES
        .iter()
        .filter(|r| r.measured.agent == AgentType::Codex)
        .map(|r| (r.release.to_string(), &r.measured))
        .chain(
            AGENT_FACTS
                .iter()
                .filter(|f| f.agent == AgentType::Codex)
                .map(|f| ("Codex's facts".to_string(), f)),
        );
    for (release, record) in tables {
        struct Named<'a> {
            release: String,
            facts: &'a AgentFacts<'static>,
        }
        let record = Named {
            release,
            facts: record,
        };
        let Measurement::Verified(dialogs) = record.facts.key_effects else {
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
        let complete = |key_effects, head_echo| AgentFacts {
            ready_composer: Measurement::Verified(()),
            echo: Measurement::Verified(()),
            submit_route: Measurement::Verified(()),
            layout: Measurement::Verified(()),
            key_effects,
            input_mode: Measurement::Verified(()),
            release_recogniser: Measurement::Verified(()),
            parser_cost: Measurement::Verified(()),
            head_echo,
            ..record.facts.clone()
        };
        assert_eq!(first_stage_bytes().count(), TOKEN_HEAD_ALPHABET);
        assert!(
            matches!(
                facts_verdict(&complete(
                    Measurement::Verified(dialogs),
                    Measurement::Verified(())
                )),
                Activation::Go { staged: true, .. }
            ),
            "{}",
            record.release
        );
        assert_eq!(
            facts_verdict(&complete(
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
            facts_verdict(&complete(
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

/// Scenario: Replay recorded Claude Code 2.1.310 and 2.1.312 composers in indexed and truecolor modes, the Claude Code 2.1.318 composer in indexed mode, and the Codex composer, through the frame builder and transaction without a prompt hook. The empty composer admits one line and one Enter, the typed line is held exactly, and its transcript echo confirms Submitted; dirty screens carry no composer or dialog. Both Claude truecolor captures wrap the user message onto a shaded continuation row.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_the_transaction_reads_real_frames_through_the_profile() {
    use dot_agent_deck::doorbell_screen::frame;
    use dot_agent_deck::doorbell_txn::Outcome;
    let mut failures = Vec::new();
    for (stem, profile) in [
        (
            "claude-inbox/r5wake/claude-2.1.310-composer-24x80",
            &CLAUDE_PROFILE,
        ),
        (
            "claude-inbox/r5wake/claude-2.1.312-composer-24x80",
            &CLAUDE_PROFILE,
        ),
        (
            "claude-inbox/r5wake/claude-2.1.318-composer-24x80",
            &CLAUDE_PROFILE,
        ),
        (
            "claude-inbox/r5wake/claude-2.1.310-truecolor-composer-24x80",
            &CLAUDE_PROFILE,
        ),
        (
            "claude-inbox/r5wake/claude-2.1.312-truecolor-composer-24x80",
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
        let after = trace.reads_between(trace.points["mark:cr-1"], trace.points["after-01"]);
        // These measurements deliberately block the prompt hook, which later
        // restores the draft. Replay through the first empty post-Enter frame
        // and hold it: the transient transcript must suffice without that hook.
        let echo_end = after
            .iter()
            .position(|grid| frame(profile, grid, 1, true, Some(line)).empty_and_ready())
            .expect("an empty composer after Enter");
        let outcome = replay_transaction(&trace, profile, &after[..=echo_end]);
        eprintln!("{stem}: no-hook transaction = {outcome:?}");
        if outcome != Outcome::Submitted {
            failures.push(format!("{stem}: expected Submitted, got {outcome:?}"));
        }
        let submitted = after
            .iter()
            .map(|grid| frame(profile, grid, 3, true, Some(line)))
            .find(|f| f.user_cells == vec![line.clone()]);
        if let Some(submitted) = submitted {
            assert!(!submitted.holds_exactly(line));
        } else {
            failures.push(format!("{stem}: no frame shows the line as a user message"));
        }
        let dirty = frame(profile, &trace.grid("typed-01"), 4, false, Some(line));
        assert!(!dirty.clean);
        assert!(dirty.composer.is_none() && dirty.dialog.is_none());
        assert!(
            dirty.text.contains("dot-agent-deck: message"),
            "text is still read"
        );
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Drive the production transaction with captured frames and a clock advanced
/// to its wakeups. No hook evidence is fabricated; the writer records exactly
/// the line and Enter, and the profile must supply the submit evidence.
fn replay_transaction(
    trace: &Trace,
    profile: &ScreenProfile,
    after: &[Grid],
) -> dot_agent_deck::doorbell_txn::Outcome {
    use dot_agent_deck::doorbell_screen::frame;
    use dot_agent_deck::doorbell_txn::{
        Action, Admission, AttemptCounter, AttemptRing, Caps, Event, Job, LatchCell, Seam,
        SeamRead, TOKEN_ALPHABET, admit,
    };
    use dot_agent_deck::inbox::MessageId;
    use std::collections::VecDeque;
    use std::time::Instant;

    struct At(SeamRead);
    impl Seam for At {
        fn read(&mut self) -> Option<SeamRead> {
            Some(self.0.clone())
        }
    }
    let line = &trace.lines[0];
    let id = MessageId::parse(
        line.split("message ")
            .nth(1)
            .unwrap()
            .split(' ')
            .next()
            .unwrap(),
    )
    .unwrap();
    let empty = frame(profile, &trace.grid("empty"), 1, true, Some(line));
    let typed = frame(profile, &trace.grid("typed-01"), 1, true, Some(line));
    let admission = Admission {
        activation: Ok(()),
        mode: Ok(()),
        ledger_idle: true,
        watermark: 0,
        job_mark: 0,
        frame: empty.clone(),
        nudge_in_flight: false,
        settling: false,
        submit_route: true,
        still_admitted: true,
    };
    let cell = LatchCell::default();
    let counter = AttemptCounter::default();
    let ring = AttemptRing::default();
    let mut draws = line.as_bytes()[1..7]
        .iter()
        .map(|letter| TOKEN_ALPHABET.iter().position(|b| b == letter).unwrap() as u8)
        .cycle();
    let start = Instant::now();
    let mut txn = admit(
        &admission,
        &counter,
        &ring,
        &cell,
        || draws.next().unwrap(),
        &id,
        Caps {
            delete_key: false,
            placeholder: false,
            hook_route: false,
            transcript_route: true,
            staged_head: None,
        },
        start,
    )
    .unwrap();
    assert_eq!(txn.line(), line);
    let mut seam = At(SeamRead {
        watermark: 0,
        job_mark: 0,
        frame: empty,
        mode: Ok(()),
        open: true,
    });
    let mut actions = txn.start(&mut seam, start);
    let mut events = VecDeque::new();
    let mut writes = Vec::new();
    let mut now = start;
    for _ in 0..1000 {
        let mut wake = None;
        for action in actions {
            match action {
                Action::Enqueue(job, claim) => {
                    assert!(claim.start());
                    let accepted = match &job {
                        Job::Line(text) => text.len(),
                        Job::Cr => 1,
                        Job::Deletes(_) => panic!("unexpected cleanup write: {job:?}"),
                    };
                    events.push_back(Event::JobStarted);
                    events.push_back(Event::JobEnded { accepted, ok: true });
                    if matches!(job, Job::Line(_)) {
                        events.push_back(Event::Frame(typed.clone()));
                    } else {
                        events.extend(
                            after.iter().map(|grid| {
                                Event::Frame(frame(profile, grid, 1, true, Some(line)))
                            }),
                        );
                    }
                    writes.push(job);
                }
                Action::WakeAt(next) => wake = Some(next),
                Action::Release(outcome) => {
                    assert_eq!(writes, vec![Job::Line(line.clone()), Job::Cr]);
                    return outcome;
                }
                Action::Reproject => {}
            }
        }
        let event = events.pop_front().unwrap_or_else(|| {
            now = wake.unwrap_or(txn.deadline());
            Event::Tick
        });
        if let Event::Frame(view) = &event {
            seam.0.frame = view.clone();
        }
        actions = txn.on(event, &mut seam, now);
    }
    panic!("transaction did not release");
}

/// Scenario: Replay each release's captured truecolor user echo with its background changed to indexed 237 as a positive control, then replay near-miss RGB backgrounds, assistant quotes, partial lines, a missing user marker, and a composer still holding the line. On the unchanged RGB background, an assistant quote, a user marker displaced to column 1, and a tool-output block containing the exact line supply no submit evidence with an empty composer. The still-held composer refuses a submit even when the indexed control supplies a recognised transcript row, and mutating only a continuation row also refuses the echo.
#[spec("inbox/doorbell/047")]
#[test]
fn doorbell_047_claude_truecolor_transcript_controls() {
    use dot_agent_deck::doorbell_screen::frame;
    use dot_agent_deck::doorbell_txn::{Cause, Outcome};
    use vt100::Color;

    for release in ["2.1.310", "2.1.312"] {
        let trace = fixture(&format!(
            "claude-inbox/r5wake/claude-{release}-truecolor-composer-24x80"
        ));
        let line = &trace.lines[0];
        let echo = trace
            .reads_between(trace.points["mark:cr-1"], trace.points["after-01"])
            .into_iter()
            .find(|grid| {
                grid.cells
                    .iter()
                    .any(|row| row[0].text == "❯" && row[0].bg == Color::Rgb(55, 55, 55))
            })
            .expect("captured RGB user row");
        let user_row = echo
            .cells
            .iter()
            .position(|row| row[0].text == "❯" && row[0].bg == Color::Rgb(55, 55, 55))
            .unwrap();
        assert_eq!(echo.cells[user_row + 1][0].bg, Color::Rgb(55, 55, 55));
        assert!(frame(&CLAUDE_PROFILE, &echo, 1, true, Some(line)).empty_and_ready());

        let recolor = |grid: &mut Grid, color| {
            for row in &mut grid.cells[user_row..=user_row + 1] {
                for cell in row {
                    if cell.bg == Color::Rgb(55, 55, 55) {
                        cell.bg = color;
                    }
                }
            }
        };
        let mut indexed = echo.clone();
        recolor(&mut indexed, Color::Idx(237));
        assert_eq!(
            replay_transaction(&trace, &CLAUDE_PROFILE, std::slice::from_ref(&indexed)),
            Outcome::Submitted,
            "{release}: indexed-237 control"
        );
        eprintln!("{release}: indexed-237 control = Submitted");
        let mut negatives = Vec::new();
        for color in [
            Color::Rgb(55, 55, 56),
            Color::Rgb(56, 55, 55),
            Color::Rgb(55, 56, 55),
            Color::Rgb(0, 0, 0),
            Color::Rgb(255, 255, 255),
        ] {
            let mut other = echo.clone();
            recolor(&mut other, color);
            negatives.push((format!("other background {color:?}"), other));
        }
        let mut quote = echo.clone();
        recolor(&mut quote, Color::Default);
        quote.cells[user_row][0].text = "●".into();
        negatives.push(("assistant quote".into(), quote));

        // Synthetic controls derived from each measured truecolor echo:
        // retain its exact line, shaded continuation, geometry and empty
        // composer; change only the two-cell prefix. These are not captured
        // assistant/tool responses: their prefixes model those output blocks.
        assert_eq!(echo.cells[user_row][1].text, " ");
        for (name, prefix) in [
            ("RGB assistant quote", ["●", " "]),
            ("RGB user marker displaced to column 1", [" ", "❯"]),
            ("RGB tool-output block", ["⎿", " "]),
        ] {
            let mut control = echo.clone();
            for (cell, text) in control.cells[user_row].iter_mut().zip(prefix) {
                cell.text = text.into();
            }
            assert_eq!(control.cells[user_row][0].bg, Color::Rgb(55, 55, 55));
            assert_eq!(control.cells[user_row][2..], echo.cells[user_row][2..]);
            assert_eq!(control.cells[user_row + 1], echo.cells[user_row + 1]);
            assert!(frame(&CLAUDE_PROFILE, &control, 1, true, Some(line)).empty_and_ready());
            negatives.push((name.into(), control));
        }

        let mut partial = echo.clone();
        let last = partial.cells[user_row + 1]
            .iter_mut()
            .rfind(|cell| !cell.text.trim().is_empty())
            .unwrap();
        assert_eq!(last.text, "h");
        last.text = " ".into();
        negatives.push(("partial line".into(), partial));

        let mut no_marker = echo.clone();
        no_marker.cells[user_row][0].text = " ".into();
        negatives.push(("RGB row without the marker".into(), no_marker));

        let mut continuation = echo.clone();
        for cell in &mut continuation.cells[user_row + 1] {
            cell.bg = Color::Rgb(55, 55, 56);
        }
        negatives.push(("wrong continuation background".into(), continuation));

        // The indexed variant makes this refusal load-bearing before the RGB
        // recogniser lands: it already supplies recognised transcript evidence.
        for (name, mut held) in [("RGB", echo.clone()), ("indexed", indexed)] {
            let typed = trace.grid("typed-01");
            for row in user_row + 2..held.rows as usize {
                held.cells[row] = typed.cells[row].clone();
                held.wrapped[row] = typed.wrapped[row];
            }
            held.cursor = typed.cursor;
            assert!(frame(&CLAUDE_PROFILE, &held, 1, true, Some(line)).holds_exactly(line));
            negatives.push((format!("{name} composer still holds the line"), held));
        }
        for (name, grid) in negatives {
            let outcome = replay_transaction(&trace, &CLAUDE_PROFILE, &[grid]);
            let expected = if name.contains("composer still holds") {
                Outcome::Unresolved(Cause::WithdrawalRefused)
            } else {
                Outcome::Unresolved(Cause::NoSubmitEvidence)
            };
            assert_eq!(outcome, expected, "{release}: {name}");
            eprintln!("{release}: {name} = {outcome:?}");
        }
    }
}
