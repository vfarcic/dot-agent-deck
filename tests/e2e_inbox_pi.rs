#![cfg(all(feature = "e2e", unix))]

//! PRD #1666 G3 through the attached TUI: a stand-in Pi
//! (`fixtures/pi-inbox/fake-pi.mjs`) runs the real extension and its receipt
//! receiver, with no model, and plays Pi's event contract so the deck's
//! not-started window can be watched from the card and the picker.

mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;

use common::TuiDeck;
use inbox::*;
use serde_json::json;
use spec::spec;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The not-started window these tests run the deck with.
const WINDOW_MS: u64 = 2000;
/// The start of the not-started reason, which the card's notice row shows
/// (its headline is cut to `✉1` at this card width).
const LINE: &str = "the agent has been idle for a few";

/// The `node` that can run the extension's TypeScript (23.6+), or `None`
/// after printing `SKIP:`.
fn node() -> Option<PathBuf> {
    let found = std::process::Command::new("sh")
        .args(["-c", "command -v node"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()));
    let Some(node) = found else {
        eprintln!("SKIP: node is not available for the Pi stand-in");
        return None;
    };
    let version = std::process::Command::new(&node)
        .arg("--version")
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default();
    let mut parts = version.trim_start_matches('v').split('.');
    let major: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let minor: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    if (major, minor) < (23, 6) {
        eprintln!("SKIP: node {version} cannot strip TypeScript types");
        return None;
    }
    Some(node)
}

/// A deck whose not-started window is [`WINDOW_MS`], with `extra` in the
/// daemon's environment.
fn deck(extra: &[(&str, &str)]) -> TuiDeck {
    let mut builder = TuiDeck::builder()
        .with_pty_size(160, 40)
        .with_env("DOT_AGENT_DECK_INBOX_NOT_STARTED_MS", WINDOW_MS.to_string());
    for (key, value) in extra {
        builder = builder.with_env(*key, *value);
    }
    let deck = builder.launch_with_fixture("minimal");
    assert!(
        common::wait_until(common::child_boot_budget(), || deck
            .attach_socket_path()
            .exists()),
        "the deck's daemon must listen"
    );
    deck
}

/// Start the stand-in Pi in `mode` as a pane of `deck`, wait until its
/// receiver registered, and return the pane and its hand-over log.
fn pi(deck: &TuiDeck, node: &Path, mode: &str, extra_env: &[(&str, &str)]) -> (Pane, PathBuf) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let base = deck.home().join(format!("fake-pi-{mode}"));
    let quote = |path: &Path| path.to_string_lossy().replace('\'', "'\\''");
    let command = format!(
        "exec '{}' '{}' '{}' {mode} '{}'",
        quote(node),
        quote(&root.join("tests/fixtures/pi-inbox/fake-pi.mjs")),
        quote(&root.join("pi-extension/src/index.ts")),
        quote(&base)
    );
    let mut env = vec![json!(["DOT_AGENT_DECK_PANE_ID", "pi-standin"])];
    for (key, value) in extra_env {
        env.push(json!([key, value]));
    }
    let reply = attach(
        deck,
        json!({"op":"start-agent", "command": common::capability_export_command(&command),
            "cwd": deck.cwd(), "rows": 40, "cols": 140, "env": env, "display_name": "pi-standin"}),
    );
    assert_eq!(reply["ok"], true, "stand-in spawn: {reply}");
    let agent = reply["id"].as_str().unwrap().to_string();
    let token = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(common::recorded_hook_capability(deck.cwd(), &agent))
        .expect("stand-in capability");
    let boot = common::child_boot_budget();
    let pid_path = base.with_extension("pid");
    assert!(
        common::wait_until(boot, || std::fs::read_to_string(&pid_path)
            .ok()
            .and_then(|text| text.parse::<u32>().ok())
            .is_some()),
        "the stand-in Pi must start within {boot:?}"
    );
    let pane = Pane {
        id: "pi-standin".into(),
        agent,
        token,
        pid: std::fs::read_to_string(&pid_path).unwrap().parse().unwrap(),
        capture: base.with_extension("bytes"),
        attach_socket: deck.attach_socket_path().to_path_buf(),
    };
    receipt_registered(deck, &pane);
    (pane, base.with_extension("log"))
}

/// The stand-in's hand-over log lines.
fn log(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn wait_log(path: &Path, line: &str) {
    assert!(
        common::wait_until(Duration::from_secs(20), || log(path)
            .iter()
            .any(|l| l == line)),
        "the stand-in must log {line:?}: {:?}",
        log(path)
    );
}

/// The card shows no not-started line for all of `span`.
fn never_shown(deck: &TuiDeck, span: Duration) {
    deck.wait_until_grid_then_hold("no not-started line", span, |grid| !grid.contains(LINE));
}

/// Scenario: A stand-in Pi running the real extension is handed a deck message and never starts on it while idle, with a two-second window. The attached TUI's card says the message may not have reached the agent; the picker explains that Retry may make the agent get it twice, and `r` hands it over once more as a marked repeat. A second run presses `c` instead, and the line goes.
#[spec("inbox/pi/023")]
#[test]
fn pi_023_a_message_the_agent_never_starts_on_shows_on_the_card_and_retry_repeats_it() {
    let Some(node) = node() else {
        return;
    };
    for retry in [true, false] {
        let deck = deck(&[]);
        let (pane, path) = pi(&deck, &node, "never", &[]);
        let id = enqueue(&deck, &pane, "PI-NOT-STARTED-1666");
        wait_log(&path, &format!("handed {id}"));
        deck.wait_for_string(LINE);
        // The deck is in command mode: the stand-in was started over the
        // attach socket, so no pane took focus.
        deck.wait_for_string("[Back to Pane d/Ctrl+D]");
        deck.send_keys(b"i");
        deck.wait_for_string("Retry sends it again");
        if retry {
            deck.send_keys(b"r");
            wait_log(&path, &format!("handed repeat {id}"));
            let handed = log(&path)
                .iter()
                .filter(|l| l.ends_with(id.as_str()) && l.starts_with("handed"))
                .count();
            assert_eq!(handed, 2, "one marked repeat: {:?}", log(&path));
        } else {
            deck.send_keys(b"c");
            deck.send_keys(b"\x1b");
            deck.wait_for_absence(LINE);
            // A cancelled message never returns.
            never_shown(&deck, Duration::from_millis(2 * WINDOW_MS));
        }
    }
}

/// Scenario: A stand-in Pi starts on the deck message at once, commits it and then stays in a silent model call for more than twice the window. The card never says the message may not have reached the agent.
#[spec("inbox/pi/023")]
#[test]
fn pi_023a_a_committed_message_in_a_long_silent_turn_shows_nothing() {
    let Some(node) = node() else {
        return;
    };
    let deck = deck(&[]);
    let (pane, path) = pi(&deck, &node, "silent", &[]);
    let id = enqueue(&deck, &pane, "PI-SILENT-TURN-1666");
    wait_log(&path, &format!("committed {id}"));
    never_shown(&deck, Duration::from_millis(2 * WINDOW_MS + 1000));
}

/// Scenario: A stand-in Pi runs a long turn on one deck message while a second is handed to it as a follow-up, which stays queued without starting. Nothing shows while the turn runs, for more than twice the window; after the turn ends with the original follow-up still queued, the card says it may not have reached the agent and that message still has one delivery.
#[spec("inbox/pi/023")]
#[test]
fn pi_023b_a_follow_up_behind_a_long_turn_shows_only_after_the_turn_ends() {
    let Some(node) = node() else {
        return;
    };
    let turn = 3 * WINDOW_MS;
    let deck = deck(&[]);
    let (pane, path) = pi(
        &deck,
        &node,
        "behind",
        &[("FAKE_PI_TURN_MS", &turn.to_string())],
    );
    let first = enqueue(&deck, &pane, "PI-LONG-TURN-1666");
    wait_log(&path, &format!("committed {first}"));
    let second = enqueue(&deck, &pane, "PI-FOLLOW-UP-1666");
    wait_log(&path, &format!("handed {second}"));
    never_shown(&deck, Duration::from_millis(2 * WINDOW_MS));
    deck.wait_for_string(LINE);
    let messages = inbox::listing(&deck, &pane)["messages"]
        .as_array()
        .unwrap()
        .clone();
    let follow_up = messages
        .iter()
        .find(|message| message["id"] == second)
        .unwrap();
    assert_eq!(follow_up["deliveries"], 1, "{follow_up}");
    assert!(
        !log(&path)
            .iter()
            .any(|line| line == &format!("handed repeat {second}")),
        "the notice concerns the original queued follow-up"
    );
}

/// Scenario: A stand-in Pi takes longer than the window to start on a deck message, as a slow preflight would. The card says the message may not have reached the agent, and the line goes once the turn starts, with no Retry.
#[spec("inbox/pi/023")]
#[test]
fn pi_023c_a_slow_start_shows_the_line_until_the_turn_starts() {
    let Some(node) = node() else {
        return;
    };
    let start = 3 * WINDOW_MS;
    let deck = deck(&[]);
    let (pane, path) = pi(
        &deck,
        &node,
        "slow",
        &[("FAKE_PI_START_MS", &start.to_string())],
    );
    let id = enqueue(&deck, &pane, "PI-SLOW-START-1666");
    wait_log(&path, &format!("handed {id}"));
    deck.wait_for_string(LINE);
    wait_log(&path, &format!("committed {id}"));
    deck.wait_for_absence(LINE);
    let handed = log(&path)
        .iter()
        .filter(|l| l.starts_with("handed"))
        .count();
    assert_eq!(handed, 1, "no repeat: {:?}", log(&path));
}
