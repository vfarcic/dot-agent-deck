#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]

//! Codex 0.164.0's `head_echo` (PRD #1666 part 3, Q27): the token head alone,
//! its exact echo, the rest after it, at the deck's own timing, on the real
//! Codex composer, inside the disposable TUI harness. Each run records with
//! `tests/support/codex_head_echo_measure.py` and reads the capture back the
//! way the deck reads its pane (`tests/support/codex_head_echo.rs`).
mod common;
#[path = "support/codex_head_echo.rs"]
mod head_echo;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/codex_inbox_live.rs"]
mod live;

use head_echo::{Capture, spread};
use spec::spec;
use std::time::Duration;

/// The release this file measures, and its native executable's SHA-256 (the
/// recorder refuses any other).
const RELEASE: &str = "0.164.0";

/// How a run's Codex is set up.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Hooks {
    /// The deck's hooks installed and trusted, as a deck pane has them.
    Trusted,
    /// The deck's hooks installed, every one switched off in Codex's own
    /// trust records: Codex runs none of them (the untrusted-hooks path).
    Off,
    /// The deck's hooks installed in a Codex home of the run's own with no
    /// trust recorded, and Codex started through a launcher (the `devbox run
    /// codex-big` shape). The wrapper trusts nothing here because the
    /// recorder runs it with no pane id; a production pane's wrapper attempts
    /// the trust for such a launcher, fails to reach Codex's app-server, and
    /// Codex starts with the hooks untrusted all the same.
    Untrusted,
}

fn preflight() {
    let python = std::process::Command::new("python3")
        .args(["-c", "import pyte"])
        .output()
        .unwrap();
    assert!(
        python.status.success(),
        "the measurement controller requires Python pyte (run inside devbox)"
    );
    let version = std::process::Command::new("codex")
        .arg("--version")
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&version.stdout).contains(RELEASE),
        "this measurement requires Codex {RELEASE} first on PATH: {version:?}"
    );
}

/// Runs the recorder in a pane of a fresh harness deck and returns its
/// capture, kept under `/var/tmp` for export.
fn record(kind: &str, rows: u16, cols: u16, trials: usize, hooks: Hooks) -> Capture {
    let root = tempfile::Builder::new()
        .prefix("dad-codex-head-echo-")
        .tempdir_in("/var/tmp")
        .unwrap()
        .keep();
    let output = root.join("capture");
    let script = root.join("measure.py");
    std::fs::write(&script, include_str!("support/codex_head_echo_measure.py")).unwrap();
    let quote = |text: &str| format!("'{}'", text.replace('\'', "'\\''"));
    let command = format!(
        "exec python3 {} {kind} {} {rows} {cols} {} {} {trials}",
        quote(&script.to_string_lossy()),
        quote(&output.to_string_lossy()),
        quote(&common::contained_deck_binary().to_string_lossy()),
        quote(common::codex_test_model()),
    );
    let deck = common::TuiDeck::builder()
        .with_env(
            "PYTHONPATH",
            std::env::var("PYTHONPATH").unwrap_or_default(),
        )
        .with_env("PATH", common::path_with_binary_dir())
        .with_pty_size(220, 60)
        .with_imported_codex_credentials()
        .with_continue_session("codex-head-echo", command)
        .launch_with_fixture("minimal");
    // The deck's hooks installed and trusted before Codex starts, as a deck
    // pane's wrapper leaves them; the recorder waits for `go`.
    live::prepare(&deck);
    if hooks == Hooks::Off {
        let path = deck.home_dir().join(".codex/config.toml");
        let mut config: toml::Value =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let state = config
            .get_mut("hooks")
            .and_then(|h| h.get_mut("state"))
            .and_then(toml::Value::as_table_mut)
            .expect("the deck recorded its hooks' trust");
        for (_, entry) in state.iter_mut() {
            if let Some(entry) = entry.as_table_mut() {
                entry.insert("enabled".into(), toml::Value::Boolean(false));
            }
        }
        std::fs::write(path, toml::to_string(&config).unwrap()).unwrap();
    }
    // The untrusted run's Codex gets a home of its own: a copy of the
    // harness's with the trust records removed. The harness's deck re-trusts
    // its hooks in `.codex` itself at start-up, in the background, so a trust
    // record removed there can come back before Codex starts.
    let mut home = String::new();
    if hooks == Hooks::Untrusted {
        let from = deck.home_dir().join(".codex");
        let to = deck.home_dir().join(".codex-untrusted");
        std::fs::create_dir(&to).unwrap();
        for name in ["auth.json", "hooks.json"] {
            std::fs::copy(from.join(name), to.join(name)).unwrap();
        }
        let mut config: toml::Value =
            toml::from_str(&std::fs::read_to_string(from.join("config.toml")).unwrap()).unwrap();
        config
            .get_mut("hooks")
            .and_then(toml::Value::as_table_mut)
            .expect("the deck recorded its hooks' trust")
            .remove("state");
        std::fs::write(to.join("config.toml"), toml::to_string(&config).unwrap()).unwrap();
        home = to.to_string_lossy().into_owned();
    }
    // Renamed into place, so the recorder never reads it half written.
    std::fs::write(root.join("go.tmp"), home).unwrap();
    std::fs::rename(root.join("go.tmp"), root.join("go")).unwrap();
    let finished = output.join("result.json");
    assert!(
        common::wait_until(Duration::from_secs(280), || finished.exists()
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
    let capture = Capture::recorded(&output);
    eprintln!(
        "Codex {} {kind} {rows}x{cols} evidence: {}; load {} -> {}",
        capture.result["release"],
        output.display(),
        capture.result["load_start"]["loadavg"],
        capture.result["load_end"]["loadavg"],
    );
    assert_eq!(capture.result["release"], RELEASE);
    assert!(
        capture.result["error"].is_null(),
        "measurement premise differed: {}; {}",
        capture.result["error"],
        output.display()
    );
    // The recorder hashed the running Codex process's own executable and
    // refused any but the pinned one (it would have set `error`).
    assert!(
        capture.result["running_executables"]
            .as_array()
            .is_some_and(|r| !r.is_empty()),
        "no running executable recorded: {}",
        output.display()
    );
    drop(deck);
    capture
}

fn staged(rows: u16, cols: u16, trials: usize, hooks: Hooks) {
    skip_unless!(common::check_codex_available());
    preflight();
    let kind = if hooks == Hooks::Untrusted {
        "staged-launcher"
    } else {
        "staged"
    };
    let capture = record(kind, rows, cols, trials, hooks);
    let measured = head_echo::staged_trials(&capture);
    assert_eq!(measured.len(), trials);
    // At 50 × 200 Codex draws its width witness from the start, so every
    // ring is read on Clean frames; at 24 × 80 a fresh Codex draws none before
    // its first turn, which is reported, not asserted.
    if cols >= 200 {
        assert!(measured.iter().all(|t| t.clean), "{measured:?}");
    }
    eprintln!(
        "Clean from admission to Enter on {} of {} rings",
        measured.iter().filter(|t| t.clean).count(),
        measured.len()
    );
    let head: Vec<f64> = measured.iter().map(|t| t.head_echo_ms).collect();
    let line: Vec<f64> = measured.iter().map(|t| t.line_echo_ms).collect();
    let submit: Vec<f64> = measured.iter().map(|t| t.transcript_ms).collect();
    eprintln!(
        "head echo ms {:?}; line echo ms {:?}; transcript ms {:?}",
        spread(&head),
        spread(&line),
        spread(&submit)
    );
}

/// Scenario: Start real Codex 0.164.0 at 50 × 200 under the deck's own wrapper, with the deck's hooks installed and trusted, and ring it five times the way the deck's staged doorbell does: the eight-character token head alone, then the rest of the line as soon as the composer holds exactly the head, then Enter once it holds exactly the whole line and at least 150 ms after the rest was written, each turn left to end before the next. Read back through the deck's evidence state and screen profile, every trial shows only the placeholder or a prefix of the head until the head is held exactly, within the echo wait, with no dialog; then only prefixes of the line until it is held exactly before the Enter; then the line as a transcript user message within the confirm wait, and Codex's own record holds exactly that prompt once.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_codex_164_head_echo_large_first() {
    staged(50, 200, 5, Hooks::Trusted);
}

/// Scenario: The same five staged rings on real Codex 0.164.0 at 50 × 200 with trusted hooks, in a second fresh session.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_codex_164_head_echo_large_second() {
    staged(50, 200, 5, Hooks::Trusted);
}

/// Scenario: The same five staged rings on real Codex 0.164.0 at 50 × 200 with trusted hooks, in a third fresh session.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_codex_164_head_echo_large_third() {
    staged(50, 200, 5, Hooks::Trusted);
}

/// Scenario: The same five staged rings on real Codex 0.164.0 at 50 × 200 with trusted hooks, in a fourth fresh session.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_codex_164_head_echo_large_fourth() {
    staged(50, 200, 5, Hooks::Trusted);
}

/// Scenario: Five staged rings, as in the 50 × 200 runs, on real Codex 0.164.0 at 24 × 80 with trusted hooks, where the line wraps over two composer rows.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_codex_164_head_echo_small_first() {
    staged(24, 80, 5, Hooks::Trusted);
}

/// Scenario: Five more staged rings on real Codex 0.164.0 at 24 × 80 with trusted hooks, in a second fresh session.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_codex_164_head_echo_small_second() {
    staged(24, 80, 5, Hooks::Trusted);
}

/// Scenario: Five staged rings on real Codex 0.164.0 at 50 × 200 with every one of the deck's hooks switched off in Codex's own trust records, so Codex runs none of them; each must read exactly as with trusted hooks.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_codex_164_head_echo_hooks_off_first() {
    staged(50, 200, 5, Hooks::Off);
}

/// Scenario: Five more staged rings on real Codex 0.164.0 at 50 × 200 with the deck's hooks switched off, in a second fresh session.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_codex_164_head_echo_hooks_off_second() {
    staged(50, 200, 5, Hooks::Off);
}

/// Scenario: Start real Codex 0.164.0 at 50 × 200 through a shell launcher the deck's wrapper cannot see into, in a Codex home where the deck's hooks are installed and nothing trusts them, answer Codex's hooks review with "Continue without trusting", and ring it five times as in the trusted runs; each must read exactly as with trusted hooks.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_codex_164_head_echo_untrusted_first() {
    staged(50, 200, 5, Hooks::Untrusted);
}

/// Scenario: Five more staged rings on real Codex 0.164.0 at 50 × 200 through a shell launcher with the deck's hooks untrusted and Codex's hooks review answered "Continue without trusting", in a second fresh session.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_codex_164_head_echo_untrusted_second() {
    staged(50, 200, 5, Hooks::Untrusted);
}

fn controls(rows: u16, cols: u16) {
    skip_unless!(common::check_codex_available());
    preflight();
    let capture = record("controls", rows, cols, 0, Hooks::Trusted);
    // The same verdicts the fast tier pins on the committed capture.
    for control in head_echo::checked_controls(&capture) {
        eprintln!("{control:?}");
    }
}

/// Scenario: On real Codex 0.164.0 at 50 × 200 with trusted hooks, type six token heads alone and delete each with eight deletes; type the first one to seven characters of a head and delete them; type a head and then none, one, ten or forty characters of the rest and delete all of it; type a doorbell-shaped line with another token and an earlier attempt's line and delete them; type three whole lines with the Enter straight after them; and type six staged lines with the Enter asked for 50, 100 or 120 ms after the rest and written once the line has echoed. Read back through the deck's evidence state and screen profile, no write opens a dialog or submits anything but those Enters; each typed text is held exactly before its deletes, where the strict withdrawal recogniser counts the deletes for a whole head and refuses a partial one, a head plus a space and another line, and every delete leaves the placeholder; the three burst Enters are kept and refused for withdrawal, the six after the echo submit, and Codex's own record holds each submitted line once and nothing else doorbell-shaped.
#[spec("inbox/doorbell/036")]
#[test]
fn doorbell_036_live_codex_164_head_echo_controls() {
    controls(50, 200);
}
