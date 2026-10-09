#![cfg(all(feature = "e2e", unix))]

//! L2 coverage for issue #1493: a deck-launched Codex card shows the status
//! the agent is actually in, the way a Claude Code card does, driven through
//! the real deck, daemon and `dot-agent-deck wrap` around a stand-in that
//! paints TUI-shaped output and reports its turn through Codex's native hooks.

mod common;

use std::path::Path;
use std::time::Duration;

use common::TuiDeck;
use serde_json::json;
use spec::spec;

/// Every status label a dashboard card can render (`ui::status_label`), so a
/// check for one can also require the absence of the others.
const STATUS_LABELS: [&str; 6] = [
    "Thinking",
    "Working",
    "Needs Input",
    "Idle",
    "Compacting",
    "Error",
];

/// The deck's own Codex hooks as `codex app-server`'s `hooks/list` reports
/// them, every one switched on, so the wrapper's trust step records trust for
/// all of them. Same shape and placeholders as `tests/e2e_codex_seed_delivery.rs`.
fn deck_hook_response() -> String {
    let entry = |event: &str, camel: &str| {
        json!({
            "key": format!("__CODEX_HOME__/hooks.json:{event}:0:0"),
            "eventName": camel,
            "handlerType": "command",
            "matcher": null,
            "command": "__DECK_COMMAND__",
            "timeoutSec": 600,
            "statusMessage": null,
            "sourcePath": "__CODEX_HOME__/hooks.json",
            "source": "user",
            "pluginId": null,
            "displayOrder": 0,
            "enabled": true,
            "isManaged": false,
            "currentHash": format!("sha256:{event}"),
            "trustStatus": "untrusted"
        })
    };
    json!({
        "id": 2,
        "result": {"data": [{
            "cwd": "/workspace",
            "hooks": [
                entry("session_start", "sessionStart"),
                entry("user_prompt_submit", "userPromptSubmit"),
                entry("pre_tool_use", "preToolUse"),
                entry("post_tool_use", "postToolUse"),
                entry("permission_request", "permissionRequest"),
                entry("stop", "stop"),
            ],
            "warnings": [],
            "errors": []
        }]}
    })
    .to_string()
}

/// A deck whose one restored pane runs `command` under `dot-agent-deck wrap
/// --agent codex`, detached to the dashboard. `codex_on_path` decides the
/// wrapper's trust step: with the `codex-synthetic` app-server stand-in on
/// `PATH` it records trust for the deck's hooks; without it — no `codex`
/// anywhere, the `devbox run codex-big` host — it cannot run at all. Never the
/// inherited `PATH`, since a host `codex` would decide the trust step.
fn wrapped_codex_deck(command: &str, codex_on_path: bool) -> TuiDeck {
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let bin_dir = Path::new(bin)
        .parent()
        .expect("test binary has a parent dir")
        .display()
        .to_string();
    let path = if codex_on_path {
        let stand_in = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-synthetic");
        format!("{}:{bin_dir}:/usr/bin:/bin", stand_in.display())
    } else {
        format!("{bin_dir}:/usr/bin:/bin")
    };
    let deck = TuiDeck::builder()
        .with_pty_size(180, 45)
        .with_env("PATH", path)
        .with_env("CODEX_HOOK_LIST_RESPONSE", deck_hook_response())
        .with_continue_session(
            "",
            format!("dot-agent-deck wrap --agent codex -- {command}"),
        )
        .launch_with_fixture("codex-synthetic");
    deck.wait_for_string("[Command Mode Ctrl+D]");
    deck.send_bytes(b"\x04");
    deck.wait_for_string("Dir:");
    deck
}

/// Whether the grid shows `label` as the card's status and no other status.
fn shows_only(grid: &str, label: &str) -> bool {
    grid.contains(label)
        && STATUS_LABELS
            .iter()
            .filter(|other| **other != label)
            .all(|other| !grid.contains(other))
}

fn assert_card_status(deck: &TuiDeck, label: &str, moment: &str) {
    assert!(
        deck.wait_for_grid_predicate_within(Duration::from_secs(15), |grid| shows_only(
            grid, label
        )),
        "{moment}: the Codex card should read {label:?} and nothing else:\n{}",
        deck.snapshot_grid()
    );
}

/// The card keeps reading `label` for the whole of `window`, so a status that
/// arrives late cannot slip past a check that happened to look early.
fn assert_card_holds(deck: &TuiDeck, label: &str, window: Duration, moment: &str) {
    let left = deck.wait_for_grid_predicate_within(window, |grid| !shows_only(grid, label));
    assert!(
        !left,
        "{moment}: the Codex card should keep reading {label:?}:\n{}",
        deck.snapshot_grid()
    );
}

fn go(deck: &TuiDeck, step: &str) {
    std::fs::write(deck.workdir().join(format!("go-{step}")), "")
        .unwrap_or_else(|e| panic!("create go-{step}: {e}"));
}

/// Scenario: Restore a pane running a wrapped Codex stand-in whose hooks the
/// deck trusted, and detach to the dashboard. While it paints its boot screen
/// with no prompt sent the card reads Idle; it then walks one turn through
/// Codex's native hooks — Thinking, Working on the tool, Needs Input on a
/// permission prompt, Idle on Stop — and stays Idle while the TUI redraws.
#[spec("codex/status/001")]
#[test]
fn codex_status_001_trusted_codex_card_follows_its_hooks_not_its_output() {
    let deck = wrapped_codex_deck("/bin/sh codex-status-standin.sh", true);
    let work = deck.workdir().to_path_buf();
    assert!(
        common::wait_for_path(&work.join("booted.log"), Duration::from_secs(20)),
        "the wrapped Codex stand-in never painted its boot screen:\n{}",
        deck.snapshot_grid()
    );
    assert_card_status(&deck, "Idle", "started, no prompt yet");
    assert_card_holds(
        &deck,
        "Idle",
        Duration::from_secs(2),
        "started, no prompt yet",
    );

    go(&deck, "prompt");
    assert_card_status(&deck, "Thinking", "prompt submitted");
    go(&deck, "tool");
    assert_card_status(&deck, "Working", "tool running");
    assert!(
        deck.wait_for_grid_string_within("sentinel_dir", Duration::from_secs(10)),
        "the Working card should name its tool:\n{}",
        deck.snapshot_grid()
    );
    go(&deck, "permission");
    assert_card_status(&deck, "Needs Input", "permission prompt on screen");
    go(&deck, "tooldone");
    assert_card_status(&deck, "Thinking", "permission answered, tool finished");
    go(&deck, "stop");
    assert_card_status(&deck, "Idle", "turn finished");
    assert!(
        common::wait_for_path(&work.join("redrawn.log"), Duration::from_secs(15)),
        "the stand-in never redrew after its turn:\n{}",
        deck.snapshot_grid()
    );
    assert_card_holds(&deck, "Idle", Duration::from_secs(2), "redraw after Stop");
}

/// Scenario: Restore a pane running a wrapped Codex stand-in on a host with no
/// `codex` on `PATH`, so the deck cannot get Codex's hooks trusted, and detach
/// to the dashboard. The card reads Thinking while the stand-in paints, Idle
/// once its output has stayed quiet, Thinking again for the next burst of
/// output and Idle after it — never Working or Thinking for the pane's whole life.
#[spec("codex/status/003")]
#[test]
fn codex_status_003_untrusted_codex_card_settles_when_its_output_goes_quiet() {
    let deck = wrapped_codex_deck("/bin/sh codex-quiet-standin.sh", false);
    let work = deck.workdir().to_path_buf();
    assert!(
        common::wait_for_path(&work.join("booted.log"), Duration::from_secs(20)),
        "the wrapped Codex stand-in never painted its boot screen:\n{}",
        deck.snapshot_grid()
    );
    assert_card_status(&deck, "Idle", "boot paint over, output quiet");
    assert_card_holds(&deck, "Idle", Duration::from_secs(2), "output still quiet");

    go(&deck, "paint-1");
    assert_card_status(&deck, "Thinking", "Codex drawing again");
    assert!(
        common::wait_for_path(&work.join("painted.log"), Duration::from_secs(15)),
        "the stand-in never finished its second paint:\n{}",
        deck.snapshot_grid()
    );
    assert_card_status(&deck, "Idle", "second paint over, output quiet");
}
