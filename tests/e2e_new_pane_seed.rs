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
//! - `prompt/new-pane/013` — RED until the exclusion is dropped: a spawn made
//!   through an AUTHORING mode (the built-in `schedule` option) now records a
//!   last command like any other form-launched spawn, so a freshly reopened
//!   regular form pre-fills with the authoring command.
//! - `prompt/new-pane/014` — regression guard: the recorded last command must
//!   survive a full deck RESTART (two launches sharing one HOME) — proving the
//!   value round-trips through the persisted `session.toml`, not just in-process.

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

/// Scenario: Launch the deck with an EMPTY `default_command` in the
/// `schedule-mode` fixture (so the regular form falls back to the recorded last
/// command, making the authoring recording observable). Open the new-pane form
/// (Ctrl+n → Space), cycle the Mode field to the built-in `schedule` AUTHORING
/// option, navigate to the Command field, CLEAR it so the submitted command is
/// unambiguously `cat`, type `cat`, and submit — dispatching an authoring-mode
/// spawn. Then reopen a FRESH regular form (Ctrl+d → Ctrl+n → Space) WITHOUT
/// cycling Mode and assert the Command field is PRE-FILLED with `cat` — proving
/// an authoring-mode spawn now records a last command like any other
/// form-launched spawn (the exclusion was dropped for consistency), so the
/// regular form seeds from it. RED against the current implementation, which
/// still excludes authoring via `is_authoring_selected()`, so the reopened field
/// renders blank.
#[spec("prompt/new-pane/013")]
#[test]
fn new_pane_013_authoring_spawn_records_last_command() {
    // `default_command` is empty (the schedule-mode fixture sets no value and we
    // point DOT_AGENT_DECK_CONFIG nowhere), so the regular form's only possible
    // seed is the last-command fallback — exactly the channel the authoring spawn
    // now feeds.
    let deck = TuiDeck::launch_with_fixture("schedule-mode");
    deck.wait_for_string("No active sessions");

    // (1) Authoring spawn: open the form, cycle Mode to the built-in `schedule`
    // authoring option, type `cat`, and submit. `cat` is a real binary so the
    // spawn deterministically succeeds and auto-focuses the card. The
    // exclusion is dropped, so this authoring spawn now records `cat` as the last
    // command like any plain spawn.
    deck.send_keys(b"\x0e"); // Ctrl+n → directory picker
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" "); // Space → confirm dir → new-pane form
    deck.wait_for_string("No mode"); // Mode cycler is up at "No mode"
    deck.send_keys(b"\x1b[C\x1b[C\x1b[C\x1b[C\x1b[C\x1b[C\x1b[C\x1b[C"); // Right ×8 → schedule (caps at last)
    deck.wait_for_string("schedule mode"); // selection landed on the schedule authoring mode
    deck.send_keys(b"\r"); // Mode → Name
    deck.send_keys(b"\r"); // Name → Command
    // CLEAR the Command field first so the recorded value is unambiguously `cat`
    // regardless of any pre-fill (the form has no clear-line key, so we pop with a
    // run of Backspace (0x7f) bytes; extra pops on an empty field are no-ops —
    // same technique as `012`).
    deck.send_keys(&[0x7fu8; 32]); // Backspace ×32 → clear any pre-filled command
    deck.send_keys(b"cat");
    deck.send_keys(b"\r"); // submit the authoring spawn
    deck.wait_for_string("[Command Mode Ctrl+D]"); // authoring card spawned & auto-focused

    // (2) Reopen a FRESH regular form — do NOT cycle Mode, so it is the ordinary
    // "No mode" form whose Command field seeds from `default_command` (empty) →
    // recorded last command → `cat`.
    deck.send_keys(b"\x04"); // Ctrl+d → Normal mode
    deck.send_keys(b"\x0e"); // Ctrl+n → directory picker
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" "); // Space → confirm dir → new-pane form
    deck.wait_for_string("Tab: switch"); // form fully painted

    // (3) The Command field must be PRE-FILLED with `cat` — the authoring spawn's
    // command is now recorded and seeds the later regular form. RED against the
    // current code, which still excludes authoring, so the field renders blank.
    let grid = deck.snapshot_grid();
    assert_eq!(
        command_field_value(&grid),
        "cat",
        "an authoring-mode (schedule) spawn must now RECORD its command as the last command: \
         after spawning `cat` through the schedule authoring option, reopening a regular \
         new-pane form must PRE-FILL the Command field with `cat` (`default_command` is empty, \
         so the last command is the only seed). RED today: the authoring-exclusion gate still \
         drops the recording, so the field renders blank.\nGrid:\n{grid}"
    );
}

/// Scenario: Prove the recorded last command survives a full binary RESTART. Two
/// launches share ONE isolated HOME (so the persisted `session.toml` carries
/// over). Launch 1 (empty `default_command`): open the new-pane form (Ctrl+n →
/// Space), type `cat`, submit (spawning a pane), then quit cleanly (Ctrl+d →
/// Ctrl+c → Ctrl+c) so the session flushes to disk; wait until `session.toml`
/// records `last_command = "cat"`, then drop launch 1 (its temp working dir is
/// removed, so the saved pane is skipped on restore and the next launch starts
/// clean). Launch 2, same HOME: open the new-pane form and assert the Command
/// field is PRE-FILLED with `cat`, read back from the session file launch 1 wrote
/// — exercising the persist → reload → seed round-trip, not just in-process
/// state. GREEN against the current implementation; a RED here means the value
/// failed to persist or reload across the restart.
#[spec("prompt/new-pane/014")]
#[test]
fn new_pane_014_last_command_survives_restart() {
    // A single HOME shared by both launches so the persisted `session.toml`
    // (resolved under $HOME by `session_path()`) carries the recorded last
    // command from launch 1 into launch 2. Each launch still gets its own temp
    // working dir, sockets, and daemon — so this is a genuine restart, not a
    // warm hand-off.
    let shared_home = tempfile::tempdir().expect("shared HOME tempdir");
    let home = shared_home.path().to_path_buf();
    let home_arg = home.to_string_lossy().to_string();

    // --- Launch 1: spawn `cat`, then quit cleanly so the session flushes. ---
    {
        let deck = TuiDeck::builder()
            .with_env("HOME", home_arg.as_str())
            .launch_with_fixture("minimal");
        deck.wait_for_string("No active sessions");

        deck.send_keys(b"\x0e"); // Ctrl+n → directory picker
        deck.wait_for_string("Select Directory");
        deck.send_keys(b" "); // Space → confirm dir → new-pane form
        deck.wait_for_string("Tab: switch"); // form fully painted
        deck.send_keys(b"\r"); // Mode → Name
        deck.send_keys(b"\r"); // Name → Command
        deck.send_keys(b"cat");
        deck.send_keys(b"\r"); // submit → record last_command = `cat`
        deck.wait_for_string("[Command Mode Ctrl+D]"); // pane spawned & auto-focused

        // Quit cleanly so the session flushes to disk: Ctrl+d leaves the pane's
        // command mode back to Normal, the first Ctrl+c opens the quit dialog,
        // the second Ctrl+c confirms the quit (the deck writes its final session
        // snapshot — including last_command — on the way out).
        deck.send_keys(b"\x04"); // Ctrl+d → Normal mode
        deck.send_keys(b"\x03"); // Ctrl+c → quit-confirm dialog
        deck.send_keys(b"\x03"); // Ctrl+c → confirm quit

        // Wait until the persisted session actually carries the recorded command
        // before tearing launch 1 down — this is the disk hand-off launch 2 reads.
        // (The wait sleeps between reads, so it lives in the harness, not here —
        // Decision 21 forbids sleeping inside e2e test bodies.)
        let session_path = home.join(".config/dot-agent-deck/session.toml");
        common::wait_for_file_contains(&session_path, "last_command = \"cat\"");
        // `deck` drops here: the process is reaped and launch 1's temp working
        // dir is removed, so the saved `cat` pane (whose dir is now gone) is
        // skipped on launch 2's restore and the deck starts at "No active
        // sessions" — while the recorded last command persists in the shared HOME.
    }

    // --- Launch 2: SAME HOME — the Command field pre-fills from the persisted
    // last command written by launch 1. ---
    let deck = TuiDeck::builder()
        .with_env("HOME", home_arg.as_str())
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");

    deck.send_keys(b"\x0e"); // Ctrl+n → directory picker
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" "); // Space → confirm dir → new-pane form
    deck.wait_for_string("Tab: switch"); // form fully painted

    let grid = deck.snapshot_grid();
    assert_eq!(
        command_field_value(&grid),
        "cat",
        "the recorded last command must survive a full deck restart: launch 1 spawned `cat` \
         and quit, and launch 2 (sharing the same HOME) must PRE-FILL the new-pane Command \
         field with `cat`, read back from the persisted session file. A blank field here means \
         the value failed to persist or reload across the restart.\nGrid:\n{grid}"
    );

    drop(shared_home);
}
