#![cfg(feature = "e2e")]

//! L2 tests for PRD #196 — the new-agent (new-pane) Command field seeds from the
//! last command you spawned when no `default_command` is configured.
//!
//! Fallback chain for the Command-field seed (`src/ui.rs:4125`):
//!   1. `default_command` (when non-empty) — explicit config always wins.
//!   2. the last command the user spawned an interactive agent with, if any.
//!   3. blank (fresh install / never spawned).
//!
//! These are PTY-attached L2 tests: the new-pane form's renderer
//! (`render_new_pane_form`) and its state (`NewPaneFormState`) are private and
//! there is no public L1 render seam (same constraint as `prompt/new-pane/007`),
//! so the real form is driven through a PTY — Ctrl+n → dir-picker → Space → the
//! new-pane form — and asserted on the rendered vt100 grid.
//!
//! `cat` is the runnable stand-in command for every spawn: it is a real binary,
//! so the spawn succeeds and records a last command, and it blocks on stdin so
//! the spawned pane stays alive (Agent: none — no LLM tokens are spent).
//!
//! - `prompt/new-pane/011` — RED until the feature lands: reopening the form
//!   after spawning `cat` renders the Command field blank because nothing reads
//!   the recorded last command back.
//! - `prompt/new-pane/012` — GREEN now AND after the feature lands: an explicit
//!   `default_command` must keep winning over the recorded last command.

mod common;

use common::TuiDeck;
use spec::spec;

/// Read the new-pane form's Command-field text from a rendered vt100 grid.
///
/// The field renders as `│  Command: <value padded to inner width> │`; this
/// isolates the `Command:` row, drops everything up to and including the label,
/// strips the modal's right border (`│`, U+2502) and the field padding, and
/// returns the trimmed value (`""` when the field is blank).
fn command_field_value(grid: &str) -> String {
    let line = grid
        .lines()
        .find(|l| l.contains("Command:"))
        .unwrap_or_else(|| {
            panic!("the new-pane form must render a `Command:` field line.\nGrid:\n{grid}")
        });
    let after = line.split("Command:").nth(1).unwrap_or("");
    // Cut at the FIRST border bar (`│`, U+2502) after the label — the modal's
    // right border. (The dashboard underneath has its own border bars further
    // right, so taking the LAST bar would wrongly keep the modal border in the
    // value.) Trimming the head then drops the field's padding, leaving only the
    // field's actual text.
    let value = after
        .split_once('\u{2502}')
        .map(|(head, _)| head)
        .unwrap_or(after);
    value.trim().to_string()
}

/// Scenario: Launch the deck with an EMPTY `default_command`, open the new-pane
/// form (Ctrl+n → Space confirms the dir) and assert the Command field starts
/// BLANK. Then navigate to the Command field, type `cat`, and submit — spawning a
/// pane (`cat` is a real binary that blocks on stdin). Reopen the new-pane form
/// (Ctrl+d → Ctrl+n → Space) and assert the Command field is now PRE-FILLED with
/// `cat`, seeded from the recorded last command. RED today: nothing reads the
/// last command back, so the reopened field renders blank.
#[spec("prompt/new-pane/011")]
#[test]
fn new_pane_011_seed_from_last_command() {
    // `default_command` is empty: the minimal fixture sets no value and we point
    // DOT_AGENT_DECK_CONFIG nowhere, so the only seed left is the last-command
    // fallback this PRD adds.
    let deck = TuiDeck::launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");

    // (1) Open the new-pane form. With no prior spawn the Command field is BLANK.
    deck.send_keys(b"\x0e"); // Ctrl+n → directory picker
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" "); // Space → confirm dir → new-pane form
    deck.wait_for_string("Tab: switch"); // footer is the last line → form fully painted

    let first = deck.snapshot_grid();
    assert_eq!(
        command_field_value(&first),
        "",
        "with an empty default_command and no prior spawn, the new-pane Command field \
         must start BLANK.\nGrid:\n{first}"
    );

    // (2) Mode → Name → Command, type `cat`, submit. `cat` is a real binary that
    // blocks on stdin, so the spawn succeeds and the pane stays running.
    deck.send_keys(b"\r"); // Mode → Name
    deck.send_keys(b"\r"); // Name → Command
    deck.send_keys(b"cat");
    deck.send_keys(b"\r"); // submit
    deck.wait_for_string("[Command Mode Ctrl+D]"); // pane spawned & auto-focused

    // (3) Reopen the form. The Command field must now be PRE-FILLED with `cat`,
    // seeded from the recorded last command. RED today: nothing reads the last
    // command back, so the field renders blank.
    deck.send_keys(b"\x04"); // Ctrl+d → Normal mode
    deck.send_keys(b"\x0e"); // Ctrl+n → directory picker
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" "); // Space → confirm dir → new-pane form
    deck.wait_for_string("Tab: switch"); // form fully painted

    let second = deck.snapshot_grid();
    assert_eq!(
        command_field_value(&second),
        "cat",
        "after spawning `cat`, reopening the new-pane form must PRE-FILL the Command \
         field with the last command `cat` (default_command is empty). RED today: the \
         field renders blank because nothing seeds it from the recorded last command.\n\
         Grid:\n{second}"
    );
}

/// Scenario: Launch the deck with `default_command = "configured-default-cmd"`
/// (via a config.toml pointed at by DOT_AGENT_DECK_CONFIG), open the new-pane
/// form (Ctrl+n → Space) and assert the Command field pre-fills with
/// `configured-default-cmd`. Then navigate to the Command field, CLEAR it, type
/// `cat`, and submit — recording `cat` as the last command. Reopen the form and
/// assert the Command field is STILL `configured-default-cmd`: an explicit
/// `default_command` wins over the recorded last command. GREEN today and after
/// the feature lands — this guards that last-command seeding never overrides an
/// explicit `default_command`.
#[spec("prompt/new-pane/012")]
#[test]
fn new_pane_012_default_command_precedence() {
    // A distinctive `default_command` we only ever pre-fill, never submit.
    let cfg_dir = tempfile::tempdir().expect("config tempdir");
    let cfg_path = cfg_dir.path().join("config.toml");
    std::fs::write(&cfg_path, "default_command = \"configured-default-cmd\"\n")
        .expect("write config.toml");

    let deck = TuiDeck::builder()
        .with_env("DOT_AGENT_DECK_CONFIG", cfg_path.to_string_lossy())
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");

    // (1) Open the form: the Command field pre-fills from `default_command`.
    deck.send_keys(b"\x0e"); // Ctrl+n → directory picker
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" "); // Space → confirm dir → new-pane form
    deck.wait_for_string("Tab: switch"); // form fully painted

    let first = deck.snapshot_grid();
    assert_eq!(
        command_field_value(&first),
        "configured-default-cmd",
        "with default_command set, the new-pane Command field must pre-fill from it.\n\
         Grid:\n{first}"
    );

    // (2) Mode → Name → Command, CLEAR the pre-filled value, type `cat`, submit —
    // recording last_command = `cat`. (The form has no clear-line key, so the
    // field is cleared with a run of Backspace (0x7f) bytes; extra pops on an
    // empty field are no-ops.)
    deck.send_keys(b"\r"); // Mode → Name
    deck.send_keys(b"\r"); // Name → Command
    deck.send_keys(&[0x7fu8; 32]); // Backspace ×32 → clear `configured-default-cmd`
    deck.send_keys(b"cat");
    deck.send_keys(b"\r"); // submit
    deck.wait_for_string("[Command Mode Ctrl+D]"); // pane spawned & auto-focused

    // (3) Reopen the form. `default_command` STILL wins over the recorded last
    // command `cat` — the field must show `configured-default-cmd`, not `cat`.
    deck.send_keys(b"\x04"); // Ctrl+d → Normal mode
    deck.send_keys(b"\x0e"); // Ctrl+n → directory picker
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" "); // Space → confirm dir → new-pane form
    deck.wait_for_string("Tab: switch"); // form fully painted

    let second = deck.snapshot_grid();
    assert_eq!(
        command_field_value(&second),
        "configured-default-cmd",
        "an explicit default_command must win over the recorded last command: even \
         though `cat` was the last spawned command, reopening the form must still \
         pre-fill `configured-default-cmd`.\nGrid:\n{second}"
    );

    drop(cfg_dir);
}
