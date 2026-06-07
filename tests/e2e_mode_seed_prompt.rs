#![cfg(feature = "e2e")]

//! L2 test for PRD #127 M3.1 — `seed_prompt` on `[[modes]]` with GATED
//! delivery to the agent pane (the enabling primitive for the Phase-3
//! "schedule" creation mode).
//!
//! A `[[modes]]` mode whose config carries a `seed_prompt` must auto-deliver
//! that prompt to its AGENT pane once the agent is ready — GATED exactly like
//! orchestrations (agent-ready signal + buffer), NOT ungated. A mode without a
//! `seed_prompt` must deliver nothing (no regression).
//!
//! ## Exercising the agent-ready gate
//! Delivery is gated on the agent's readiness signal (the `SessionStart` hook
//! event, as orchestrations use). The test's agent is a fixture script that
//! self-posts `SessionStart` via `dot-agent-deck hook claude-code` — the real
//! hook path — using the `DOT_AGENT_DECK_PANE_ID` injected per-pane and the
//! `DOT_AGENT_DECK_SOCKET` inherited from the daemon. After posting readiness
//! it records every line written into its PTY stdin to a cwd-relative file, so
//! a delivered prompt shows up as a recorded line (immune to PTY echo).
//!
//! RED today: `ModeConfig` has no `seed_prompt` field (serde silently drops the
//! fixture's value) and there is no agent-pane seed delivery for plain modes,
//! so the seeded mode's marker is never recorded.

mod common;

use std::time::Duration;

use common::TuiDeck;
use spec::spec;

const SEED_MARKER: &str = "SEEDPROMPTMARKER127";

/// Write a fixture "agent" script into `work`: it records that it started, posts
/// a synthetic `SessionStart` (the readiness signal the delivery gate waits on)
/// via the real `dot-agent-deck hook` path, then appends every stdin line it
/// receives to `record-<tag>.log` (cwd-relative → lands under `work`).
fn write_agent_script(work: &std::path::Path, tag: &str) -> String {
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let script_name = format!("agent-{tag}.sh");
    let body = format!(
        "#!/bin/sh\n\
         echo started >> started-{tag}.log\n\
         printf '%s' '{{\"hook_event_name\":\"SessionStart\",\"session_id\":\"seedtest-{tag}\"}}' \
         | \"{bin}\" hook claude-code >/dev/null 2>&1\n\
         while IFS= read -r l; do printf '%s\\n' \"$l\" >> record-{tag}.log; done\n"
    );
    let path = work.join(&script_name);
    std::fs::write(&path, body).expect("write agent script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod agent script");
    }
    format!("./{script_name}")
}

/// Drive the new-pane dialog to spawn `[[modes]]` entry at `mode_index`
/// (1-based: index 1 = first mode) with the agent `command`. Sequence:
/// Ctrl+n → dir-picker (Space confirms cwd) → form (Right ×mode_index selects
/// the mode, Enter → Name, Enter → Command, type command, Enter submits).
fn spawn_mode(deck: &TuiDeck, mode_index: usize, command: &str) {
    // The deck repaints on a periodic tick, so it never goes fully quiescent —
    // sync on rendered dialog text instead. Keys are processed in order by the
    // deck's event loop, so each state transition is synchronous per key.
    deck.send_keys(b"\x0e"); // Ctrl+n → directory picker
    deck.send_keys(b" "); // Space → confirm current dir → new-pane form
    deck.wait_for_string("No mode"); // form is up, Mode field focused at "No mode"
    let mut mode_keys = Vec::new();
    for _ in 0..mode_index {
        mode_keys.extend_from_slice(b"\x1b[C"); // Right → next mode option
    }
    deck.send_keys(&mode_keys);
    deck.send_keys(b"\r"); // Mode → Name
    deck.send_keys(b"\r"); // Name (default) → Command
    deck.send_keys(command.as_bytes());
    deck.send_keys(b"\r"); // submit
}

/// Scenario: Launch the deck in a fixture whose `.dot-agent-deck.toml` defines
/// a `seeded` mode (carrying `seed_prompt`) and a `plain` mode (none). Drive the
/// new-pane dialog to spawn the `seeded` mode with a recorder agent that
/// self-posts `SessionStart`; assert the seed prompt is delivered into the
/// agent pane once ready (its marker is recorded). Then spawn the `plain` mode
/// the same way and assert its agent started but received NO auto-delivered
/// prompt — the gated seed delivery fires only when a `seed_prompt` is present.
#[spec("tabs/mode/005")]
#[test]
fn mode_005_seed_prompt_gated_delivery_to_agent_pane() {
    let deck = TuiDeck::launch_with_fixture("mode-seed");
    deck.wait_for_string("No active sessions");
    let work = deck.workdir().to_path_buf();

    // --- Seeded mode: the seed_prompt must be delivered to the agent pane. ---
    let seeded_cmd = write_agent_script(&work, "seeded");
    spawn_mode(&deck, 1, &seeded_cmd); // mode index 1 = `seeded`

    assert!(
        common::wait_for_path(&work.join("started-seeded.log"), Duration::from_secs(10)),
        "the seeded mode's agent pane must spawn and run its command"
    );
    assert!(
        common::wait_for_file_substr_count(
            &work.join("record-seeded.log"),
            SEED_MARKER,
            1,
            Duration::from_secs(15),
        ),
        "the mode's seed_prompt must be delivered into the agent pane once it is ready"
    );

    // --- Plain mode: no seed_prompt → nothing delivered (no regression). ---
    let plain_cmd = write_agent_script(&work, "plain");
    spawn_mode(&deck, 2, &plain_cmd); // mode index 2 = `plain`

    assert!(
        common::wait_for_path(&work.join("started-plain.log"), Duration::from_secs(10)),
        "the plain mode's agent pane must spawn and run its command"
    );
    // Give a delivery, if one were (incorrectly) going to happen, time to land —
    // longer than the agent-ready buffer.
    assert!(
        !common::wait_for_path(&work.join("record-plain.log"), Duration::from_secs(3)),
        "a mode without seed_prompt must not auto-deliver any prompt to its agent pane"
    );
}
