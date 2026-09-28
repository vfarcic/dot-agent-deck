#![cfg(all(feature = "e2e", unix))]

//! L2 coverage for issue #559: what a wrapped Codex pane's automatic prompt
//! does when the pane cannot confirm it, driven through the real deck, daemon
//! and `dot-agent-deck wrap`.

mod common;

use std::path::Path;
use std::time::Duration;

use common::TuiDeck;
use serde_json::json;
use spec::spec;

const SEED_MARKER: &str = "CODEXSEEDMARKER559";

/// The deck's own `pre_tool_use` and `user_prompt_submit` hooks as Codex lists
/// them, prompt hook switched on. `__DECK_COMMAND__` because the harness HOME —
/// and so the command the install writes — does not exist at builder time; the
/// `codex-synthetic` app-server stand-in substitutes it out of the installed
/// `hooks.json`, as `codex/hooks/003` does.
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
                entry("pre_tool_use", "preToolUse"),
                entry("user_prompt_submit", "userPromptSubmit"),
            ],
            "warnings": [],
            "errors": []
        }]}
    })
    .to_string()
}

/// Launch a deck whose one mode declares Codex and carries a seed prompt, spawn
/// that mode around a recorder, and return how many copies of the seed the
/// recorder received once a retry would have had time to land.
///
/// `codex_on_path` is the whole difference between the two runs: with the
/// `codex-synthetic` app-server stand-in on the deck's `PATH` the wrapper's
/// trust step records trust for the prompt hook; without it — no `codex`
/// anywhere, the `devbox run codex-big` host — the step cannot run at all.
/// Nothing on either pane ever reports a submitted prompt, so the only question
/// is whether the deck retypes the seed.
fn seed_copies(codex_on_path: bool) -> (usize, String) {
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let bin_dir = Path::new(bin)
        .parent()
        .expect("test binary has a parent dir")
        .display()
        .to_string();
    // Never the inherited PATH: a host `codex` (this project's dev box has one
    // in /usr/local/bin) would decide the trust step instead of the fixture.
    let path = if codex_on_path {
        let stand_in = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-synthetic");
        format!("{}:{bin_dir}:/usr/bin:/bin", stand_in.display())
    } else {
        format!("{bin_dir}:/usr/bin:/bin")
    };
    let deck = TuiDeck::builder()
        .with_pty_size(160, 42)
        .with_env("PATH", path)
        .with_env("CODEX_HOOK_LIST_RESPONSE", deck_hook_response())
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");
    let work = deck.workdir().to_path_buf();
    std::fs::write(
        work.join(".dot-agent-deck.toml"),
        format!(
            "[[modes]]\n\
             name = \"codex-seeded\"\n\
             agent = \"codex\"\n\
             reactive_panes = 0\n\
             seed_prompt = \"{SEED_MARKER}\"\n"
        ),
    )
    .expect("write Codex seeded mode");
    // Paints one line, as Codex's composer does, so the wrapper's classifier
    // gives the pane a producer; then records every submission and reports
    // none of them, as a Codex whose prompt hook never runs would not.
    let script = work.join("codex-standin.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\n\
         echo started >> started.log\n\
         printf '%s\\n' 'Ask Codex to do anything'\n\
         while IFS= read -r l; do printf '%s\\n' \"$l\" >> record.log; done\n",
    )
    .expect("write Codex stand-in");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod Codex stand-in");
    }

    deck.send_keys(b"\x0e"); // Ctrl+n → directory picker
    deck.send_keys(b" "); // confirm cwd → new-pane form
    deck.wait_for_string("No mode");
    deck.send_keys(b"\x1b[C"); // Right → `codex-seeded`
    deck.send_keys(b"\r"); // Mode → Name
    deck.send_keys(b"\r"); // Name → Command
    deck.send_keys(&[0x7fu8; 64]); // clear any pre-filled command
    deck.send_keys(b"./codex-standin.sh");
    deck.send_keys(b"\r");

    assert!(
        common::wait_for_path(&work.join("started.log"), Duration::from_secs(15)),
        "the Codex mode pane never ran its command (codex_on_path={codex_on_path}):\n{}",
        deck.snapshot_grid()
    );
    let record = work.join("record.log");
    assert!(
        common::wait_for_file_substr_count(&record, SEED_MARKER, 1, Duration::from_secs(30)),
        "the seed never reached the pane (codex_on_path={codex_on_path}):\n{}",
        deck.snapshot_grid()
    );
    // The retry floor for a Codex producer is 10 s from the write
    // (`SLOW_CONFIRMATION_LATENCY`); the replacement payload lands then. Wait
    // for a second copy with margin for a loaded box, which the healthy pane
    // reaches and the degraded one must not.
    common::wait_for_file_substr_count(&record, SEED_MARKER, 2, Duration::from_secs(25));
    let recorded = std::fs::read_to_string(&record).unwrap_or_default();
    (recorded.matches(SEED_MARKER).count(), deck.snapshot_grid())
}

/// Scenario: Launch the deck with a mode that declares Codex and carries a seed prompt, and spawn it around a stand-in that paints a composer line and records every submission while reporting none. With a `codex` app-server stand-in on the deck's PATH the wrapper records trust for Codex's prompt hook, so the unconfirmed seed is typed in a second time after the retry floor; with no `codex` on the PATH at all, as on a host where it lives only inside `devbox run codex-big`, the same pane must receive the seed exactly once.
#[spec("codex/wrap/007")]
#[test]
fn codex_wrap_007_untrusted_codex_pane_receives_its_seed_once() {
    let (healthy, healthy_grid) = seed_copies(true);
    assert_eq!(
        healthy, 2,
        "control: a wrapped Codex pane whose prompt hook was trusted keeps the retry that \
         recovers a swallowed seed — without it this test proves nothing about the other \
         run:\n{healthy_grid}"
    );
    let (degraded, degraded_grid) = seed_copies(false);
    assert_eq!(
        degraded, 1,
        "a wrapped Codex pane whose wrapper could not get Codex's hooks trusted received its \
         seed {degraded} times — nothing on it can confirm a prompt, so a delivered one was \
         typed in again (issue #559):\n{degraded_grid}"
    );
}
