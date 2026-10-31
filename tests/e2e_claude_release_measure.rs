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
use std::path::{Path, PathBuf};
use std::time::Duration;

struct Capture {
    root: PathBuf,
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
            matches!(scanner.latch(), ScanLatch::Latched { agent: AgentType::ClaudeCode, release, .. } if release.to_string() == "2.1.312"),
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
        eprintln!(
            "{}: 15/15 exact echoes, hook payloads and transcript echoes; maxima ms: echo={:.3}, hook={:.3}, transcript={:.3}",
            self.root.display(),
            max_echo_ns as f64 / 1e6,
            max_hook_ns as f64 / 1e6,
            max_transcript_ns as f64 / 1e6
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

fn measure(kind: &str, rows: u16, cols: u16) -> Capture {
    measure_with_path(kind, rows, cols, None)
}

fn measure_with_path(kind: &str, rows: u16, cols: u16, path: Option<&str>) -> Capture {
    let root = tempfile::Builder::new()
        .prefix("dad-claude-2312-measure-")
        .tempdir_in("/var/tmp")
        .unwrap()
        .keep();
    let output = root.join("capture");
    let quote = |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"));
    let script = root.join("measure.py");
    std::fs::write(&script, include_str!("support/claude_release_measure.py")).unwrap();
    let command = format!(
        "exec python3 {} {kind} {} {rows} {cols}",
        quote(&script),
        quote(&output)
    );
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
    assert!(
        common::wait_until(Duration::from_secs(140), || output
            .join("result.json")
            .exists()
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
        index: read_json("index.json"),
        result,
        bytes: std::fs::read(output.join("trace.bin")).unwrap(),
        root: output,
    };
    drop(deck);
    capture
}

/// Scenario: Open the slash menu, file menu, shell mode, exit warning and background overview in isolated real Claude Code 2.1.310 and 2.1.312 composers. Capture their raw bytes and rendered frames, including a normal composer with warning phrases quoted in a real user transcript echo whose submission is blocked before a model turn.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_release_menu_frames() {
    skip_unless!(common::check_claude_available());
    for release in ["2.1.310", "2.1.312"] {
        let key = format!("DOT_AGENT_DECK_CLAUDE_MENU_{}", release.replace('.', "_"));
        let binary =
            std::env::var(&key).unwrap_or_else(|_| panic!("set {key} to the pinned executable"));
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
        let capture = measure_with_path("menus", 50, 200, Some(&path));
        assert_eq!(capture.result["release"], release);
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

fn preflight() {
    let python = std::process::Command::new("python3")
        .args(["-c", "import pyte"])
        .output()
        .unwrap();
    assert!(
        python.status.success(),
        "the measurement controller requires Python pyte"
    );
    let version = std::process::Command::new("claude")
        .arg("--version")
        .output()
        .unwrap();
    assert!(version.status.success());
    assert!(
        String::from_utf8_lossy(&version.stdout).starts_with("2.1.312 "),
        "this measurement requires the pinned Claude Code 2.1.312 executable"
    );
}

fn startups(count: usize) {
    preflight();
    for _ in 0..count {
        let capture = measure("startup", 50, 200);
        capture.verify_start();
        assert!(CLAUDE_PROFILE.empty_and_ready(&capture.grid("end")));
        for sample in 0..200 {
            let grid = capture.grid(&format!("idle-{sample:03}"));
            assert!(CLAUDE_PROFILE.empty_and_ready(&grid));
            assert!((CLAUDE_PROFILE.dialog)(&grid).is_none());
        }
    }
}

/// Scenario: Launch four isolated real Haiku composers on pinned Claude Code 2.1.312, leaving each idle for twenty seconds. Each must retain the measured ready composer, raw input mode and recognisable release header without a post-ready dialog.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_release_startup_first_four() {
    skip_unless!(common::check_claude_available());
    startups(4);
}

/// Scenario: Launch three further isolated real Haiku composers on pinned Claude Code 2.1.312 and observe each for twenty seconds. The recorded profile must still read the ready composer and release.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_release_startup_next_three() {
    skip_unless!(common::check_claude_available());
    startups(3);
}

/// Scenario: Launch the last three of ten isolated real Haiku composers on pinned Claude Code 2.1.312 and observe each for twenty seconds. The recorded profile must still read the ready composer and release.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_release_startup_last_three() {
    skip_unless!(common::check_claude_available());
    startups(3);
}

/// Scenario: Type and submit fifteen token-led doorbell lines into real Haiku at 24 by 80, blocking every submission before a model turn. Replay the raw capture to verify exact composer and hook echoes, transcript user echoes, and the blank composer after clears and deletes.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_release_small_composer() {
    skip_unless!(common::check_claude_available());
    preflight();
    measure("composer", 24, 80).verify_composer();
}

/// Scenario: Type and submit fifteen token-led doorbell lines into real Haiku at 50 by 200 with model turns blocked, then delete three lines, resize ten times and nudge ten times. Replay the capture to verify the same composer and hook premises, full redraw markers and width witnesses.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_release_large_composer_and_layout() {
    skip_unless!(common::check_claude_available());
    preflight();
    measure("composer", 50, 200).verify_composer();
}

/// Scenario: Type the nineteen Q32 compound-key sequences into real Haiku on pinned Claude Code 2.1.312, revealing retained text with harmless markers instead of submitting it. The reviewer's sequence must retain two spaces, Enter after non-ASCII deletion must preserve the two-space draft, and three spaces must remain after Left three times.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_release_compound_keys() {
    skip_unless!(common::check_claude_available());
    preflight();
    let capture = measure("keys", 50, 200);
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
