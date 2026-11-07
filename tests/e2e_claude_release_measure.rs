#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]

//! Exact-release M1/Q32 measurements, inside the disposable TUI harness.
mod common;

use dot_agent_deck::doorbell_screen::{
    CLAUDE_PROFILE, ComposerText, Grid, WidthsSeen, find_marker,
};
use dot_agent_deck::event::AgentType;
use dot_agent_deck::release_scanner::{HEADER_RECOGNISERS, ReleaseScanner, ScanLatch};
use serde_json::Value;
use spec::spec;
use std::path::PathBuf;
use std::time::Duration;

struct Capture {
    root: PathBuf,
    /// The release the recorder read from the pinned executable.
    release: String,
    index: Value,
    result: Value,
    bytes: Vec<u8>,
}

impl Capture {
    fn point(&self, name: &str) -> usize {
        self.index["points"][name].as_u64().unwrap() as usize
    }

    fn parser(&self, end: usize) -> vt100::Parser {
        let mut parser = dot_agent_deck::vt_grid::parser(
            self.index["rows"].as_u64().unwrap() as u16,
            self.index["cols"].as_u64().unwrap() as u16,
            0,
        );
        let mut offset = 0;
        for edge in self.index["edges"].as_array().unwrap() {
            let at = edge["off"].as_u64().unwrap() as usize;
            if at >= end {
                break;
            }
            parser.process(&self.bytes[offset..at]);
            parser.screen_mut().set_size(
                (edge["rows"].as_u64().unwrap() as u16).try_into().unwrap(),
                (edge["cols"].as_u64().unwrap() as u16).try_into().unwrap(),
            );
            offset = at;
        }
        parser.process(&self.bytes[offset..end]);
        parser
    }

    fn grid(&self, name: &str) -> Grid {
        let end = self.point(name);
        Grid::of(self.parser(end).screen(), end as u64)
    }

    fn verify_start(&self) {
        let mut scanner = ReleaseScanner::new(
            self.index["rows"].as_u64().unwrap() as u16,
            self.index["cols"].as_u64().unwrap() as u16,
            HEADER_RECOGNISERS,
        )
        .unwrap();
        let mut offset = 0;
        for len in self.index["read_lens"].as_array().unwrap() {
            let end = offset + len.as_u64().unwrap() as usize;
            scanner.feed(&self.bytes[offset..end]);
            offset = end;
        }
        assert!(
            matches!(scanner.latch(), ScanLatch::Latched { agent: AgentType::ClaudeCode, release, .. } if release.to_string() == self.release),
            "{:?}; {}",
            scanner.latch(),
            self.root.display()
        );
        assert!(
            CLAUDE_PROFILE.empty_and_ready(&self.grid("empty")),
            "{}",
            self.parser(self.point("empty")).screen().contents()
        );
        assert!(
            self.result["modes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|mode| mode["bits"] == serde_json::json!([false, false]))
        );
    }

    fn verify_composer(&self) {
        self.verify_start();
        assert!(CLAUDE_PROFILE.placeholder_empty(&self.grid("empty")));
        let hooks: Vec<Value> = std::fs::read_to_string(self.root.join("hook.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(hooks.len(), 15);
        let mut max_echo_ns = 0;
        let mut max_hook_ns = 0;
        let mut max_transcript_ns = 0;
        for i in 1..=15 {
            let line = self.index["lines"][i - 1].as_str().unwrap();
            let typed = self.grid(&format!("typed-{i:02}"));
            assert_eq!(
                CLAUDE_PROFILE.holds(&typed, line),
                Some(ComposerText::Exact),
                "trial {i}; {}",
                self.root.display()
            );
            assert_eq!(hooks[i - 1]["payload"]["prompt"], line);
            let start = self.point(&format!("mark:cr-{i}"));
            let end = self.point(&format!("after-{i:02}"));
            let sent = self.result["events"]
                .as_array()
                .unwrap()
                .iter()
                .find(|event| event["send"] == line)
                .unwrap()["t"]
                .as_u64()
                .unwrap();
            let submitted = self.result["events"]
                .as_array()
                .unwrap()
                .iter()
                .find(|event| event["send"] == "\r" && event["off"].as_u64() == Some(start as u64))
                .unwrap()["t"]
                .as_u64()
                .unwrap();
            let hook_ns = hooks[i - 1]["t"].as_u64().unwrap() - submitted;
            assert!(
                hook_ns < 1_000_000_000,
                "trial {i}: hook exceeded submit confirmation cap"
            );
            max_hook_ns = max_hook_ns.max(hook_ns);
            let mut exact_at = None;
            let mut offset = 0;
            let mut echoed = false;
            for read in self.result["reads"].as_array().unwrap() {
                offset += read["len"].as_u64().unwrap() as usize;
                let at = read["t"].as_u64().unwrap();
                if at >= sent && at < submitted && exact_at.is_none() {
                    let grid = Grid::of(self.parser(offset).screen(), offset as u64);
                    if CLAUDE_PROFILE.holds(&grid, line) == Some(ComposerText::Exact) {
                        exact_at = Some(at);
                    }
                }
                if offset > start && offset <= end {
                    let grid = Grid::of(self.parser(offset).screen(), offset as u64);
                    if !echoed && (CLAUDE_PROFILE.transcript_user_line)(&grid, line) {
                        let elapsed = at - submitted;
                        assert!(
                            elapsed < 1_000_000_000,
                            "trial {i}: transcript exceeded submit cap"
                        );
                        max_transcript_ns = max_transcript_ns.max(elapsed);
                        echoed = true;
                    }
                }
            }
            let echo_ns = exact_at.expect("line must echo before Enter") - sent;
            assert!(echo_ns < 1_500_000_000, "trial {i}: echo exceeded cap");
            max_echo_ns = max_echo_ns.max(echo_ns);
            assert!(
                echoed,
                "trial {i}: no transcript user echo; {}",
                self.root.display()
            );
            let cleared = self.grid(&format!("cleared-{i:02}"));
            assert!(CLAUDE_PROFILE.empty_and_ready(&cleared));
            assert!(!CLAUDE_PROFILE.placeholder_empty(&cleared));
        }
        // The recorder waits (bounded at 5 s) for the blocked prompt to come
        // back into the composer before clearing it, and records how long
        // that took. A prompt that never came back, or came back after the
        // bound, fails here instead of passing with `null`.
        let restores = self.result["restores"].as_array().unwrap();
        assert_eq!(
            restores.len(),
            15,
            "one restore per trial; {}",
            self.root.display()
        );
        let mut max_restore_ms = 0f64;
        for (i, restore) in restores.iter().enumerate() {
            assert_eq!(restore["trial"], i + 1);
            let ms = restore["ms_after_hook"].as_f64().unwrap_or_else(|| {
                panic!(
                    "trial {}: the blocked prompt did not come back within the 5 s bound; {}",
                    i + 1,
                    self.root.display()
                )
            });
            assert!(
                ms < 5_500.0,
                "trial {}: the blocked prompt came back {ms} ms after the hook",
                i + 1
            );
            max_restore_ms = max_restore_ms.max(ms);
        }
        eprintln!(
            "{}: 15/15 exact echoes, hook payloads and transcript echoes; maxima ms: echo={:.3}, hook={:.3}, transcript={:.3}, prompt restored after hook={:.3}",
            self.root.display(),
            max_echo_ns as f64 / 1e6,
            max_hook_ns as f64 / 1e6,
            max_transcript_ns as f64 / 1e6,
            max_restore_ms
        );
        for i in 1..=3 {
            let deleted = self.grid(&format!("deleted-{i:02}"));
            assert!(CLAUDE_PROFILE.empty_and_ready(&deleted));
            assert!(!CLAUDE_PROFILE.placeholder_empty(&deleted));
        }
        if self.index["cols"] == 200 {
            for kind in ["resize", "nudge"] {
                let mut max_ns = 0;
                for i in 1..=10 {
                    let mark = self.point(&format!("mark:{kind}-{i}"));
                    let shot = self.point(&format!("{kind}-{i:02}"));
                    let marker = find_marker(&self.bytes[mark..shot], CLAUDE_PROFILE.redraw_marker)
                        .unwrap_or_else(|| panic!("{kind}-{i}: redraw differs"));
                    let complete = mark + marker + CLAUDE_PROFILE.redraw_marker.len();
                    let sent = self.result["events"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|event| {
                            event["resize"].is_array() && event["off"].as_u64() == Some(mark as u64)
                        })
                        .unwrap()["t"]
                        .as_u64()
                        .unwrap();
                    let read = self.result["reads"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|read| {
                            read["off"].as_u64().unwrap() + read["len"].as_u64().unwrap()
                                >= complete as u64
                        })
                        .unwrap()["t"]
                        .as_u64()
                        .unwrap();
                    max_ns = max_ns.max(read - sent);
                    let parser = self.parser(shot);
                    let cols = parser.screen().size().1.get();
                    assert!(
                        (CLAUDE_PROFILE.width_witness)(parser.screen(), &WidthsSeen::of([cols])),
                        "{kind}-{i}: width differs"
                    );
                }
                eprintln!(
                    "{}: 10/10 {kind} redraws and width witnesses; first redraw max {:.3} ms",
                    self.root.display(),
                    max_ns as f64 / 1e6
                );
            }
        }
    }
}

fn measure_with_path(kind: &str, rows: u16, cols: u16, path: Option<&str>) -> Capture {
    record(None, kind, rows, cols, &[], path)
}

/// Runs a recorder in the private TUI harness. The committed release
/// recorder is always written as `measure.py`; `entry`, when given, is a
/// further recorder (its file name and source) that imports it and is the
/// one run, with `extra` after the usual `kind out rows cols` arguments.
fn record(
    entry: Option<(&str, &str)>,
    kind: &str,
    rows: u16,
    cols: u16,
    extra: &[String],
    path: Option<&str>,
) -> Capture {
    let root = tempfile::Builder::new()
        .prefix("dad-claude-release-measure-")
        .tempdir_in("/var/tmp")
        .unwrap()
        .keep();
    let output = root.join("capture");
    let quote = |text: &str| format!("'{}'", text.replace('\'', "'\\''"));
    let mut script = root.join("measure.py");
    std::fs::write(&script, include_str!("support/claude_release_measure.py")).unwrap();
    if let Some((name, source)) = entry {
        script = root.join(name);
        std::fs::write(&script, source).unwrap();
    }
    let mut command = format!(
        "exec python3 {} {kind} {} {rows} {cols}",
        quote(&script.to_string_lossy()),
        quote(&output.to_string_lossy())
    );
    for argument in extra {
        command.push(' ');
        command.push_str(&quote(argument));
    }
    let mut builder = common::TuiDeck::builder();
    if let Some(path) = path {
        builder = builder.with_env("PATH", path);
    }
    let deck = builder
        .with_env(
            "PYTHONPATH",
            std::env::var("PYTHONPATH").unwrap_or_default(),
        )
        .with_pty_size(220, 60)
        .with_imported_claude_credentials()
        .with_claude_trust_workdir()
        .with_continue_session("release-measure", command)
        .launch_with_fixture("minimal");
    // A further recorder rewrites result.json after the release recorder's
    // save, and marks the rewrite done; wait for the mark, not the file.
    let finished = output.join(if entry.is_some() {
        "done"
    } else {
        "result.json"
    });
    assert!(
        common::wait_until(Duration::from_secs(140), || finished.exists()
            || root.join("launcher-error.json").exists()),
        "measurement did not finish; {}\n{}",
        output.display(),
        deck.snapshot_grid()
    );
    assert!(
        !root.join("launcher-error.json").exists(),
        "recorder setup failed: {}",
        std::fs::read_to_string(root.join("launcher-error.json")).unwrap_or_default()
    );
    let read_json =
        |name: &str| serde_json::from_slice(&std::fs::read(output.join(name)).unwrap()).unwrap();
    let result: Value = read_json("result.json");
    eprintln!(
        "Claude {} {kind} {rows}x{cols} evidence: {}",
        result["release"].as_str().unwrap(),
        output.display()
    );
    assert!(
        result["error"].is_null(),
        "measurement premise differed: {}; {}",
        result["error"],
        output.display()
    );
    let capture = Capture {
        release: result["release"].as_str().unwrap().to_string(),
        index: read_json("index.json"),
        result,
        bytes: std::fs::read(output.join("trace.bin")).unwrap(),
        root: output,
    };
    drop(deck);
    capture
}

fn assert_empty_menu_composer(capture: &Capture) {
    // Check the complete bounded editor independently of the ready recogniser.
    let parser = capture.parser(capture.point("transcript-quotation"));
    let screen = parser.screen();
    let (rows, cols) = screen.size();
    let (rows, cols) = (rows.get(), cols.get());
    let prompt = (1..rows)
        .rev()
        .find(|&row| screen.cell(row, 0).unwrap().contents() == "❯")
        .expect("quotation must leave a main composer");
    assert_eq!(screen.cell(prompt - 1, 0).unwrap().contents(), "─");
    let bottom = (prompt + 1..rows)
        .find(|&row| screen.cell(row, 0).unwrap().contents() == "─")
        .expect("main composer must have a lower rule");
    for row in prompt..bottom {
        for col in 0..cols {
            if (row, col) != (prompt, 0) {
                assert!(
                    screen.cell(row, col).unwrap().contents().trim().is_empty(),
                    "retained quotation draft at ({row},{col}): {}",
                    screen.contents()
                );
            }
        }
    }
    assert_eq!(screen.cursor_position(), (prompt, 2));
}

/// Scenario: Open the slash menu, file menu, shell mode, exit warning and background overview in isolated real Claude Code 2.1.310, 2.1.312 and 2.1.317 composers at 24×80, 40×120 and 50×200 in the indexed dark theme, at 50×200 in the indexed light theme, and at 50×200 in truecolor dark/light and indexed dark-ansi/light-ansi/dark-daltonized themes. Capture their raw bytes and rendered frames, including a normal composer with warning phrases quoted in a real user transcript echo whose submission is blocked before a model turn.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_release_menu_frames() {
    skip_unless!(common::check_claude_available());
    for release in ["2.1.310", "2.1.312", "2.1.317"] {
        menu_frames(release);
    }
}

/// Scenario: Open the slash menu, file menu, shell mode, exit warning and background overview in isolated real Claude Code 2.1.318 composers, the release installed on 2026-11-04, at the nine sizes and palettes 2.1.310, 2.1.312 and 2.1.317 were recorded at. Each set must draw the same rule, shell and overview foregrounds and the same screen signatures as those releases, with its one submission blocked before a model turn.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_release_318_menu_frames() {
    skip_unless!(common::check_claude_available());
    menu_frames("2.1.318");
}

/// One release's nine menu size/palette recordings, each checked against the
/// foregrounds and screen signatures measured on 2.1.310 and 2.1.312.
fn menu_frames(release: &str) {
    use vt100::Color::{self, Idx, Rgb};

    let (_root, path) = pinned_path(release).unwrap_or_else(|| {
        panic!(
            "set DOT_AGENT_DECK_CLAUDE_MENU_{} to the pinned executable",
            release.replace('.', "_")
        )
    });
    // Measured foregrounds: the main composer's upper rule, the shell
    // glyph and the overview's own prompt glyph.
    let indexed_dark = (Idx(244), Idx(211), Idx(246));
    for (kind, rows, cols, theme, colorterm, (rule, shell, overview)) in [
        ("menus", 24, 80, "dark", None, indexed_dark),
        ("menus", 40, 120, "dark", None, indexed_dark),
        ("menus", 50, 200, "dark", None, indexed_dark),
        (
            "menus-light",
            50,
            200,
            "light",
            None,
            (Idx(245), Idx(199), Idx(241)),
        ),
        (
            "menus-truecolor-dark",
            50,
            200,
            "dark",
            Some("truecolor"),
            (Rgb(136, 136, 136), Rgb(253, 93, 177), Rgb(153, 153, 153)),
        ),
        (
            "menus-truecolor-light",
            50,
            200,
            "light",
            Some("truecolor"),
            (Rgb(138, 138, 138), Rgb(255, 0, 135), Rgb(102, 102, 102)),
        ),
        (
            "menus-dark-ansi",
            50,
            200,
            "dark-ansi",
            None,
            (Idx(7), Idx(13), Idx(7)),
        ),
        (
            "menus-light-ansi",
            50,
            200,
            "light-ansi",
            None,
            (Idx(7), Idx(5), Idx(8)),
        ),
        (
            "menus-dark-daltonized",
            50,
            200,
            "dark-daltonized",
            None,
            (Idx(244), Idx(75), Idx(246)),
        ),
    ] {
        let capture = measure_with_path(kind, rows, cols, Some(&path));
        eprintln!(
            "{release} {kind} {rows}x{cols} capture: {}",
            capture.root.display()
        );
        assert_eq!(capture.result["release"], release);
        assert_eq!(capture.result["theme"], theme);
        assert_eq!(capture.result["colorterm"], serde_json::json!(colorterm));
        let foreground = |point: &str, row| {
            capture
                .parser(capture.point(point))
                .screen()
                .cell(row, 0)
                .unwrap()
                .fgcolor()
        };
        assert_eq!(foreground("empty", rows - 4), rule, "{kind} main rule");
        assert_eq!(foreground("bang", rows - 3), shell, "{kind} shell glyph");
        assert_eq!(
            foreground("overview", rows - 3),
            overview,
            "{kind} overview glyph"
        );
        // The structural premise the recogniser reads, beyond the palette:
        // the main composer has a coloured rule and a Default prompt, the
        // overview a Default rule and a coloured prompt.
        assert_eq!(
            foreground("empty", rows - 3),
            Color::Default,
            "{kind} main prompt"
        );
        assert_eq!(
            foreground("overview", rows - 4),
            Color::Default,
            "{kind} overview rule"
        );
        assert_empty_menu_composer(&capture);
        assert!(
            capture
                .parser(capture.point("overview"))
                .screen()
                .contents()
                .lines()
                .any(|row| {
                    row.starts_with('❯') && row.contains("describe a task for a new session")
                }),
            "{release}: overview must show its own composer, beyond the transcript quotation"
        );
        for (point, signature) in [
            ("slash", "/add-dir"),
            ("at", ".dot-agent-deck.toml"),
            ("bang", "! for shell mode"),
            ("exit-warning", "Press Ctrl-C again to exit"),
            ("overview", "describe a task for a new session"),
            (
                "transcript-quotation",
                "Original prompt: Quoted UI phrases:",
            ),
        ] {
            assert!(
                capture
                    .parser(capture.point(point))
                    .screen()
                    .contents()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .contains(signature),
                "{release} {point}: missing real screen signature {signature:?}"
            );
        }
        let mut rendered = String::new();
        for point in [
            "empty",
            "slash",
            "at",
            "bang",
            "exit-warning",
            "transcript-quotation",
            "overview",
        ] {
            let parser = capture.parser(capture.point(point));
            eprintln!("{release} {point}:\n{}", parser.screen().contents());
            rendered.push_str(&format!(
                "{release} {point} offset={} cursor={:?}\n{}\n\n",
                capture.point(point),
                parser.screen().cursor_position(),
                parser.screen().contents()
            ));
            let palette: Vec<_> = (0..rows)
                .filter_map(|row| {
                    let cell = parser.screen().cell(row, 0)?;
                    matches!(cell.contents(), "❯" | "─" | "!").then(|| {
                        format!(
                            "row {row}: {:?} fg={:?} bg={:?}",
                            cell.contents(),
                            cell.fgcolor(),
                            cell.bgcolor()
                        )
                    })
                })
                .collect();
            rendered.push_str(&format!("{}\n\n", palette.join("\n")));
        }
        std::fs::write(capture.root.join("rendered.txt"), rendered).unwrap();
        let hooks = std::fs::read_to_string(capture.root.join("hook.jsonl")).unwrap();
        assert_eq!(
            hooks.lines().count(),
            1,
            "one blocked submission; no model turn"
        );
    }
}

/// A private executable search path whose `claude` is the pinned executable
/// `DOT_AGENT_DECK_CLAUDE_MENU_<release>` names, outside the checkout. The
/// directory lives as long as the returned guard.
fn pinned_path(release: &str) -> Option<(tempfile::TempDir, String)> {
    let key = format!("DOT_AGENT_DECK_CLAUDE_MENU_{}", release.replace('.', "_"));
    let binary = std::env::var(&key).ok()?;
    let root = tempfile::Builder::new()
        .prefix("dad-claude-menu-bin-")
        .tempdir_in("/var/tmp")
        .unwrap();
    std::os::unix::fs::symlink(&binary, root.path().join("claude")).unwrap();
    let path = format!(
        "{}:{}",
        root.path().display(),
        std::env::var("PATH").unwrap()
    );
    Some((root, path))
}

/// The search path a release's measurement runs under: the pinned executable
/// when its variable is set, else the host's `claude`, whose version
/// [`preflight`] then checks.
fn release_path(release: &str) -> (Option<tempfile::TempDir>, Option<String>) {
    match pinned_path(release) {
        Some((root, path)) => (Some(root), Some(path)),
        None => (None, None),
    }
}

fn preflight(release: &str, path: Option<&str>) {
    let python = std::process::Command::new("python3")
        .args(["-c", "import pyte"])
        .output()
        .unwrap();
    assert!(
        python.status.success(),
        "the measurement controller requires Python pyte"
    );
    let mut command = std::process::Command::new("claude");
    if let Some(path) = path {
        command.env("PATH", path);
    }
    let version = command.arg("--version").output().unwrap();
    assert!(version.status.success());
    assert!(
        String::from_utf8_lossy(&version.stdout).starts_with(&format!("{release} ")),
        "this measurement requires the pinned Claude Code {release} executable: set \
         DOT_AGENT_DECK_CLAUDE_MENU_{} or put it first on PATH",
        release.replace('.', "_")
    );
}

fn startups(release: &str, count: usize) {
    let (_root, path) = release_path(release);
    preflight(release, path.as_deref());
    for _ in 0..count {
        let capture = measure_with_path("startup", 50, 200, path.as_deref());
        capture.verify_start();
        assert!(CLAUDE_PROFILE.empty_and_ready(&capture.grid("end")));
        for sample in 0..200 {
            let grid = capture.grid(&format!("idle-{sample:03}"));
            assert!(CLAUDE_PROFILE.empty_and_ready(&grid));
            assert!((CLAUDE_PROFILE.dialog)(&grid).is_none());
        }
    }
}

fn composer(release: &str, rows: u16, cols: u16) {
    let (_root, path) = release_path(release);
    preflight(release, path.as_deref());
    measure_with_path("composer", rows, cols, path.as_deref()).verify_composer();
}

/// Scenario: Launch four isolated real Haiku composers on pinned Claude Code 2.1.312, leaving each idle for twenty seconds. Each must retain the measured ready composer, raw input mode and recognisable release header without a post-ready dialog.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_release_startup_first_four() {
    skip_unless!(common::check_claude_available());
    startups("2.1.312", 4);
}

/// Scenario: Launch three further isolated real Haiku composers on pinned Claude Code 2.1.312 and observe each for twenty seconds. The recorded profile must still read the ready composer and release.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_release_startup_next_three() {
    skip_unless!(common::check_claude_available());
    startups("2.1.312", 3);
}

/// Scenario: Launch the last three of ten isolated real Haiku composers on pinned Claude Code 2.1.312 and observe each for twenty seconds. The recorded profile must still read the ready composer and release.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_release_startup_last_three() {
    skip_unless!(common::check_claude_available());
    startups("2.1.312", 3);
}

/// Scenario: Type and submit fifteen token-led doorbell lines into real Haiku at 24 by 80, blocking every submission before a model turn. Replay the raw capture to verify exact composer and hook echoes, transcript user echoes, and the blank composer after clears and deletes.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_release_small_composer() {
    skip_unless!(common::check_claude_available());
    composer("2.1.312", 24, 80);
}

/// Scenario: Type and submit fifteen token-led doorbell lines into real Haiku at 50 by 200 with model turns blocked, then delete three lines, resize ten times and nudge ten times. Replay the capture to verify the same composer and hook premises, full redraw markers and width witnesses.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_release_large_composer_and_layout() {
    skip_unless!(common::check_claude_available());
    composer("2.1.312", 50, 200);
}

/// Scenario: Launch four isolated real Haiku composers on pinned Claude Code 2.1.317, the release installed on 2026-11-03, leaving each idle for twenty seconds. Each must show the measured ready composer, raw input mode and its own release header, with no dialog after it is ready.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_release_317_startup_first_four() {
    skip_unless!(common::check_claude_available());
    startups("2.1.317", 4);
}

/// Scenario: Launch three further isolated real Haiku composers on pinned Claude Code 2.1.317 and observe each for twenty seconds. The recorded profile must still read the ready composer and release.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_release_317_startup_next_three() {
    skip_unless!(common::check_claude_available());
    startups("2.1.317", 3);
}

/// Scenario: Launch the last three of ten isolated real Haiku composers on pinned Claude Code 2.1.317 and observe each for twenty seconds. The recorded profile must still read the ready composer and release.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_release_317_startup_last_three() {
    skip_unless!(common::check_claude_available());
    startups("2.1.317", 3);
}

/// Scenario: Type and submit fifteen token-led doorbell lines into real Haiku on pinned Claude Code 2.1.317 at 24 by 80, blocking every submission before a model turn. Replay the raw capture to verify exact composer and hook echoes, transcript user echoes, and the blank composer after clears and deletes.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_release_317_small_composer() {
    skip_unless!(common::check_claude_available());
    composer("2.1.317", 24, 80);
}

/// Scenario: Type and submit fifteen token-led doorbell lines into real Haiku on pinned Claude Code 2.1.317 at 50 by 200 with model turns blocked, then delete three lines, resize ten times and nudge ten times. Replay the capture to verify the same composer and hook premises, full redraw markers and width witnesses.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_release_317_large_composer_and_layout() {
    skip_unless!(common::check_claude_available());
    composer("2.1.317", 50, 200);
}

/// Scenario: Type the nineteen Q32 compound-key sequences into real Haiku on pinned Claude Code 2.1.312, revealing retained text with harmless markers instead of submitting it. The reviewer's sequence must retain two spaces, Enter after non-ASCII deletion must preserve the two-space draft, and three spaces must remain after Left three times.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_release_compound_keys() {
    skip_unless!(common::check_claude_available());
    compound_keys("2.1.312");
}

/// Scenario: Type the nineteen Q32 compound-key sequences into real Haiku on pinned Claude Code 2.1.317, revealing retained text with harmless markers instead of submitting it. Every sequence must leave the composer 2.1.312 left, including two spaces after the reviewer's sequence, the two-space draft after Enter following a non-ASCII deletion, and three spaces after Left three times.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_release_317_compound_keys() {
    skip_unless!(common::check_claude_available());
    compound_keys("2.1.317");
}

/// Scenario: Launch four isolated real Haiku composers on pinned Claude Code 2.1.318, the release installed on 2026-11-04, leaving each idle for twenty seconds. Each must show the measured ready composer, raw input mode and its own release header, with no dialog after it is ready.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_release_318_startup_first_four() {
    skip_unless!(common::check_claude_available());
    startups("2.1.318", 4);
}

/// Scenario: Launch three further isolated real Haiku composers on pinned Claude Code 2.1.318 and observe each for twenty seconds. The recorded profile must still read the ready composer and release.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_release_318_startup_next_three() {
    skip_unless!(common::check_claude_available());
    startups("2.1.318", 3);
}

/// Scenario: Launch the last three of ten isolated real Haiku composers on pinned Claude Code 2.1.318 and observe each for twenty seconds. The recorded profile must still read the ready composer and release.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_release_318_startup_last_three() {
    skip_unless!(common::check_claude_available());
    startups("2.1.318", 3);
}

/// Scenario: Type and submit fifteen token-led doorbell lines into real Haiku on pinned Claude Code 2.1.318 at 24 by 80, blocking every submission before a model turn. Replay the raw capture to verify exact composer and hook echoes, transcript user echoes, every blocked prompt's restore within the bound, and the blank composer after clears and deletes.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_release_318_small_composer() {
    skip_unless!(common::check_claude_available());
    composer("2.1.318", 24, 80);
}

/// Scenario: Type and submit fifteen token-led doorbell lines into real Haiku on pinned Claude Code 2.1.318 at 50 by 200 with model turns blocked, then delete three lines, resize ten times and nudge ten times. Replay the capture to verify the same composer and hook premises, full redraw markers and width witnesses.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_release_318_large_composer_and_layout() {
    skip_unless!(common::check_claude_available());
    composer("2.1.318", 50, 200);
}

/// Scenario: Type the nineteen Q32 compound-key sequences into real Haiku on pinned Claude Code 2.1.318, revealing retained text with harmless markers instead of submitting it. Every sequence must leave the composer 2.1.312 left, including two spaces after the reviewer's sequence, the two-space draft after Enter following a non-ASCII deletion, and three spaces after Left three times.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_release_318_compound_keys() {
    skip_unless!(common::check_claude_available());
    compound_keys("2.1.318");
}

fn compound_keys(release: &str) {
    let (_root, path) = release_path(release);
    preflight(release, path.as_deref());
    let capture = measure_with_path("keys", 50, 200, path.as_deref());
    capture.verify_start();
    let expected = [
        "X  Z|",
        "XadZ|",
        "X  Z|",
        "XZ|",
        "XZ|",
        "Xab|\nZ|",
        "Xab|\nZ|",
        "X|\ncdZ|",
        "X |\n Z|",
        "X   Z|",
        "XZ|",
        "XZ|",
        "X|\n|\nZ|",
        "X  |\nZ|",
        "X   Z|",
        "X|\nZ|",
        "XZ|",
        "XZ|",
        "XabZ|",
    ];
    assert_eq!(capture.result["keys"].as_array().unwrap().len(), 21);
    for (case, expected) in capture.result["keys"]
        .as_array()
        .unwrap()
        .iter()
        .zip(expected)
    {
        assert_eq!(
            case["composer"].as_str().unwrap().replace('|', ""),
            expected.replace('|', ""),
            "Q32 premise differs for {}; {}",
            case["case"],
            capture.root.display()
        );
    }
    assert_eq!(capture.result["keys"][19]["composer"], "X  Z");
    assert_eq!(capture.result["keys"][20]["composer"], "X   Z");
}

/// Scenario: In isolated real Haiku composers on pinned Claude Code 2.1.317 and 2.1.318, at 50×200 and 24×80, seed six prompts from an earlier session in the disposable HOME's history, submit one line (blocked by the hook before a model turn) and clear it, then walk the prompt history with Up and Down from the empty composer, leave a recalled prompt with Escape, Ctrl+U, Ctrl+C and Enter (blocked again) in turn, and press Up over a typed draft. Every frame is rendered with what the screen profile reads in it, so the history-recall surface can be judged against the profile.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_release_history_recall_frames() {
    skip_unless!(common::check_claude_available());
    for release in ["2.1.317", "2.1.318"] {
        let (_root, path) = pinned_path(release).unwrap_or_else(|| {
            panic!(
                "set DOT_AGENT_DECK_CLAUDE_MENU_{} to the pinned executable",
                release.replace('.', "_")
            )
        });
        preflight(release, Some(&path));
        for (rows, cols) in [(50, 200), (24, 80)] {
            let capture = measure_with_path("history", rows, cols, Some(&path));
            assert_eq!(capture.result["release"], release);
            let hooks = std::fs::read_to_string(capture.root.join("hook.jsonl")).unwrap();
            assert_eq!(
                hooks.lines().count(),
                2,
                "two blocked submissions; no model turn; {}",
                capture.root.display()
            );
            let mut rendered = String::new();
            let mut points: Vec<(&String, usize)> = capture.index["points"]
                .as_object()
                .unwrap()
                .iter()
                .filter(|(name, _)| !name.starts_with("mark:"))
                .map(|(name, at)| (name, at.as_u64().unwrap() as usize))
                .collect();
            points.sort_by_key(|&(_, at)| at);
            for (name, at) in points {
                let parser = capture.parser(at);
                let grid = Grid::of(parser.screen(), at as u64);
                let composer = (CLAUDE_PROFILE.composer)(&grid);
                let verdict = format!(
                    "{release} {rows}x{cols} {name}@{at}: composer={:?} ready={} empty_and_ready={} dialog={:?}",
                    composer
                        .as_ref()
                        .map(|c| (&c.text, c.cursor_at_end, c.hidden)),
                    (CLAUDE_PROFILE.ready)(&grid),
                    CLAUDE_PROFILE.empty_and_ready(&grid),
                    (CLAUDE_PROFILE.dialog)(&grid)
                );
                eprintln!("{verdict}");
                rendered.push_str(&format!(
                    "{verdict}\ncursor={:?}\n{}\n\n",
                    parser.screen().cursor_position(),
                    parser.screen().contents()
                ));
            }
            std::fs::write(capture.root.join("rendered.txt"), rendered).unwrap();
            eprintln!(
                "{release} history {rows}x{cols} capture: {}",
                capture.root.display()
            );
            verify_history(&capture);
        }
    }
}

/// The row above the last `❯` composer row, and the composer row, as drawn.
fn history_rows(capture: &Capture, point: &str) -> (String, String) {
    let parser = capture.parser(capture.point(point));
    let contents = parser.screen().contents();
    let lines: Vec<&str> = contents.lines().collect();
    let prompt = lines
        .iter()
        .rposition(|row| row.starts_with('❯'))
        .unwrap_or_else(|| panic!("{point}: no composer row; {}", capture.root.display()));
    // Claude draws an editable draft as `❯` and a no-break space.
    (
        lines[prompt.saturating_sub(1)].to_string(),
        lines[prompt].replace('\u{a0}', " ").trim_end().to_string(),
    )
}

/// The history-recall premises, and what the profile reads there: Up from
/// the empty composer draws `─── History i/n ` in place of the composer's
/// upper rule over the recalled prompt, a blocked prompt is not recalled,
/// Ctrl+U on a recalled prompt draws the plain composer (the history
/// position is kept: the next Up recalls the next-older entry), and the
/// profile reads no composer under a History rule, whatever the box holds.
fn verify_history(capture: &Capture) {
    let (rule, prompt) = history_rows(capture, "up-1");
    assert!(rule.starts_with("─── History 6/6 "), "up-1 rule {rule:?}");
    assert_eq!(prompt.trim_end(), "❯ HISTORYSIXfoxtrot", "up-1");
    let (rule, prompt) = history_rows(capture, "up-2");
    assert!(rule.starts_with("─── History 5/6 "), "up-2 rule {rule:?}");
    assert_eq!(prompt.trim_end(), "❯ HISTORYFIVEecho", "up-2");
    for way in ["escape", "ctrl-u", "ctrl-c", "manual-mode", "enter", "slot"] {
        let (rule, prompt) = history_rows(capture, &format!("recalled-before-{way}"));
        assert!(
            rule.starts_with("─── History ") && prompt.trim_end() != "❯",
            "recalled-before-{way}: no recalled prompt under a History rule {rule:?} {prompt:?}; {}",
            capture.root.display()
        );
    }
    let cleared = capture.grid("after-ctrl-u");
    assert!(
        !history_rows(capture, "after-ctrl-u").0.contains("History"),
        "Ctrl+U on a recalled prompt draws the plain composer (the history position is kept: the next Up recalls the next-older entry)"
    );
    assert!(CLAUDE_PROFILE.empty_and_ready(&cleared), "after-ctrl-u");
    let (_, manual) = history_rows(capture, "recalled-before-manual-mode");
    let footer = capture
        .parser(capture.point("recalled-before-manual-mode"))
        .screen()
        .contents();
    assert!(
        footer.contains("⏸ manual mode on"),
        "the manual-mode footer under a recalled prompt; {}",
        capture.root.display()
    );
    let mut history_frames = 0;
    let mut blank_history_frames = Vec::new();
    for (name, _) in capture.index["points"].as_object().unwrap() {
        if name.starts_with("mark:") {
            continue;
        }
        let parser = capture.parser(capture.point(name));
        if !parser.screen().contents().contains('❯') {
            continue;
        }
        let (rule, prompt) = history_rows(capture, name);
        if !rule.starts_with("─── History ") {
            continue;
        }
        history_frames += 1;
        if prompt.trim_end() == "❯" {
            blank_history_frames.push(name.clone());
        }
        let grid = capture.grid(name);
        assert!(
            (CLAUDE_PROFILE.composer)(&grid).is_none()
                && !(CLAUDE_PROFILE.ready)(&grid)
                && !CLAUDE_PROFILE.empty_and_ready(&grid),
            "{name}: the profile must refuse a History-ruled frame; {}",
            capture.root.display()
        );
    }
    eprintln!(
        "{} history {}x{}: {history_frames} History-ruled frames, all refused (no composer); blank box under a History rule at {blank_history_frames:?}; manual-mode recall {manual:?}; recall ups {}",
        capture.release, capture.index["rows"], capture.index["cols"], capture.result["keys"]
    );
}

/// Scenario: In isolated real Haiku composers on pinned Claude Code 2.1.317, at 50×200 and 24×80 and five trials each, seed six prompts from an earlier session, recall slot k with Up, clear it with one and, in other trials, four Ctrl+U presses so Claude draws the plain empty composer, then type a line shaped like the deck's doorbell line in one write and press Enter on its own (blocked by the hook before a model turn). The submitted prompt, the screen after it and the history walked afterwards are recorded with what the screen profile reads at every point.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_release_317_history_submit_plain() {
    skip_unless!(common::check_claude_available());
    history_submit("2.1.317", "plain");
}

/// Scenario: In isolated real Haiku composers on pinned Claude Code 2.1.317, at 50×200 and 24×80 and five trials each, recall slot k, clear it with one or four Ctrl+U presses, press Up and Down back onto it so Claude draws `History k/6` over an empty box (blank or the placeholder) when it kept its position (a trial where it did not rings nothing and is counted), then type a line shaped like the deck's doorbell line in one write and press Enter on its own (blocked by the hook before a model turn). The submitted prompt, the screen after it and the history walked afterwards are recorded with what the screen profile reads at every point.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_release_317_history_submit_labelled() {
    skip_unless!(common::check_claude_available());
    history_submit("2.1.317", "labelled");
}

/// Scenario: The plain-drawn case of `doorbell_036_live_release_317_history_submit_plain`, on pinned Claude Code 2.1.318: recall slot k, one or four Ctrl+U presses, then the deck-shaped line in one write and a lone Enter (blocked before a model turn), five trials at each of 50×200 and 24×80, recording the submitted prompt, the screen after it and the history afterwards.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_release_318_history_submit_plain() {
    skip_unless!(common::check_claude_available());
    history_submit("2.1.318", "plain");
}

/// Scenario: The History-labelled case of `doorbell_036_live_release_317_history_submit_labelled`, on pinned Claude Code 2.1.318: recall slot k, one or four Ctrl+U presses, Up and Down back onto the cleared `History k/6` slot when reachable, then the deck-shaped line in one write and a lone Enter (blocked before a model turn), five trials at each of 50×200 and 24×80, recording the submitted prompt, the screen after it and the history afterwards.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_release_318_history_submit_labelled() {
    skip_unless!(common::check_claude_available());
    history_submit("2.1.318", "labelled");
}

/// The seeded prompts, newest first, as Up recalls them.
const SEEDED_HISTORY: [&str; 6] = [
    "HISTORYSIXfoxtrot",
    "HISTORYFIVEecho",
    "HISTORYFOURdelta",
    "HISTORYTHREEcharlie",
    "HISTORYTWObravo",
    "HISTORYONEalpha",
];

/// Q143b: the deck's line plus Enter after Ctrl+U on a recalled prompt, in
/// `case` (`plain` or `labelled`), with one and with four Ctrl+U presses,
/// five trials at each size, trial t from slot `7 - t` (`6/6` down to
/// `2/6`). Every trial is recorded before any outcome is asserted, so one
/// surprise does not hide the others; a `labelled` trial whose blank
/// `History k/6` slot is not reachable rings nothing and is counted.
fn history_submit(release: &str, case: &str) {
    let (_root, path) = pinned_path(release).unwrap_or_else(|| {
        panic!(
            "set DOT_AGENT_DECK_CLAUDE_MENU_{} to the pinned executable",
            release.replace('.', "_")
        )
    });
    preflight(release, Some(&path));
    let mut outcomes = Vec::new();
    for presses in [1usize, 4] {
        for (rows, cols) in [(50, 200), (24, 80)] {
            for trial in 1..=5usize {
                outcomes.push(history_submit_trial(
                    release, case, presses, rows, cols, trial, &path,
                ));
            }
        }
    }
    let mut rung = 0;
    let mut unreachable = Vec::new();
    for outcome in &outcomes {
        let at = outcome["capture"].as_str().unwrap();
        let label = format!(
            "{} x{} {} t{}",
            outcome["case"], outcome["presses"], outcome["size"], outcome["trial"]
        );
        if outcome["reachable"] == false {
            assert_eq!(outcome["prompts"], serde_json::json!([]), "{label}; {at}");
            unreachable.push(label);
            continue;
        }
        rung += 1;
        let line = outcome["line"].as_str().unwrap();
        let slot_text = outcome["slot_text"].as_str().unwrap();
        assert_eq!(
            outcome["prompts"],
            serde_json::json!([line]),
            "{label}: one submission, the line alone; {at}"
        );
        assert!(
            !outcome["prompts"][0].as_str().unwrap().contains(slot_text),
            "{label}: {at}"
        );
        assert_eq!(
            outcome["typed_holds"], "Some(Exact)",
            "{label}: the profile reads the line held exactly before the Enter, as the ring's echo check does; {at}"
        );
        assert_eq!(outcome["after_enter_dialog"], "None", "{label}; {at}");
        assert_eq!(
            outcome["after_clear_empty_and_ready"], true,
            "{label}: the composer cleared after the blocked line is empty and ready; {at}"
        );
        assert_eq!(
            outcome["recalled"],
            serde_json::json!(SEEDED_HISTORY),
            "{label}: the history afterwards, newest first, slot k's text back and no new or leaked entry; {at}"
        );
    }
    eprintln!(
        "{release} {case}: {} trials, {rung} rung, each the line alone with the history intact; not reachable: {unreachable:?}",
        outcomes.len()
    );
}

/// One Q143b trial, recorded with what the profile reads at the points the
/// question turns on.
fn history_submit_trial(
    release: &str,
    case: &str,
    presses: usize,
    rows: u16,
    cols: u16,
    trial: usize,
    path: &str,
) -> Value {
    let alphabet = dot_agent_deck::doorbell_txn::TOKEN_ALPHABET;
    let token: String = (0..6)
        .map(|i| char::from(alphabet[(trial * 3 + i * 5 + presses * 7 + cols as usize) % 16]))
        .collect();
    let id = format!("m-hist319{}-0f3c9a71d2e4b8{presses}{trial}", &case[..1]);
    let line = format!("({token}){}", dot_agent_deck::doorbell::doorbell_line(&id));
    let capture = record(
        Some((
            "history_submit.py",
            include_str!("support/claude_history_submit_measure.py"),
        )),
        case,
        rows,
        cols,
        &[trial.to_string(), presses.to_string(), line.clone()],
        Some(path),
    );
    assert_eq!(capture.result["release"], release);
    let rendered = render_points(
        &capture,
        &format!("{release} {case} x{presses} {rows}x{cols} t{trial}"),
    );
    std::fs::write(capture.root.join("rendered.txt"), rendered).unwrap();
    let prompts: Vec<String> = std::fs::read_to_string(capture.root.join("hook.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|row| {
            let hook: Value = serde_json::from_str(row).unwrap();
            hook["payload"]["prompt"].as_str().unwrap_or("").to_string()
        })
        .collect();
    let steps = capture.result["steps"].as_array().unwrap().clone();
    let step = |name: &str| {
        steps
            .iter()
            .find(|s| s["point"] == name)
            .cloned()
            .unwrap_or(Value::Null)
    };
    let has = |name: &str| capture.index["points"].get(name).is_some();
    let empty_and_ready =
        |name: &str| has(name).then(|| CLAUDE_PROFILE.empty_and_ready(&capture.grid(name)));
    let walk: Vec<(String, String)> = steps
        .iter()
        .filter(|s| s["point"].as_str().unwrap().starts_with("walk-"))
        .map(|s| {
            (
                s["rule"].as_str().unwrap_or("").chars().take(20).collect(),
                s["box"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect();
    // The distinct entries the walk recalled under a History rule, in order.
    let mut recalled: Vec<String> = Vec::new();
    for (rule, entry) in &walk {
        if rule.starts_with("─── History ") && !recalled.contains(entry) {
            recalled.push(entry.clone());
        }
    }
    let outcome = serde_json::json!({
        "release": release, "case": case, "presses": presses,
        "size": format!("{rows}x{cols}"), "trial": trial,
        "slot": format!("{}/6", 7 - trial), "slot_text": SEEDED_HISTORY[trial - 1],
        "line": line, "prompts": prompts,
        "reachable": capture.result["reachable"],
        "cleared": step("cleared"),
        "cleared_empty_and_ready": empty_and_ready("cleared"),
        "slot_up": step("slot-up"),
        "slot_down": step("slot-down"),
        "before_ring": step("before-ring"),
        "before_ring_empty_and_ready": empty_and_ready("before-ring"),
        "typed": step("typed"),
        "typed_holds": has("typed").then(|| format!("{:?}", CLAUDE_PROFILE.holds(&capture.grid("typed"), &line))),
        "after_enter": step("after-enter"),
        "after_enter_dialog": has("after-enter").then(|| format!("{:?}", (CLAUDE_PROFILE.dialog)(&capture.grid("after-enter")))),
        "after_clear": step("after-clear"),
        "after_clear_empty_and_ready": empty_and_ready("after-clear"),
        "walk": walk,
        "recalled": recalled,
        "history_log": capture.result["history_log"],
        "echo_ms": capture.result["echo_ns"].as_f64().map(|ns| ns / 1e6),
        "hook_ms": capture.result["hook_ns"].as_f64().map(|ns| ns / 1e6),
        "load": capture.result["load"],
        "capture": capture.root.display().to_string(),
    });
    eprintln!("history-submit outcome: {outcome}");
    std::fs::write(
        capture.root.join("outcome.json"),
        serde_json::to_string_pretty(&outcome).unwrap(),
    )
    .unwrap();
    outcome
}

/// Every named point of a capture with what the screen profile reads there,
/// and the screen.
fn render_points(capture: &Capture, label: &str) -> String {
    let mut rendered = String::new();
    let mut points: Vec<(&String, usize)> = capture.index["points"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(name, _)| !name.starts_with("mark:"))
        .map(|(name, at)| (name, at.as_u64().unwrap() as usize))
        .collect();
    points.sort_by_key(|&(name, at)| (at, name.clone()));
    for (name, at) in points {
        let parser = capture.parser(at);
        let grid = Grid::of(parser.screen(), at as u64);
        let composer = (CLAUDE_PROFILE.composer)(&grid);
        rendered.push_str(&format!(
            "{label} {name}@{at}: composer={:?} ready={} empty_and_ready={} dialog={:?}\ncursor={:?}\n{}\n\n",
            composer
                .as_ref()
                .map(|c| (&c.text, c.cursor_at_end, c.hidden)),
            (CLAUDE_PROFILE.ready)(&grid),
            CLAUDE_PROFILE.empty_and_ready(&grid),
            (CLAUDE_PROFILE.dialog)(&grid),
            parser.screen().cursor_position(),
            parser.screen().contents()
        ));
    }
    rendered
}
