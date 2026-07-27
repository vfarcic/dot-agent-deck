#![cfg(feature = "e2e")]

//! L2 PTY-attached REAL-agent test for PRD #227 M4 — Shift+Enter in an embedded
//! agent pane inserts a NEWLINE instead of submitting the draft.
//!
//! This is the CLAUDE.md rule 4 "AS A USER ACTUALLY USES AND SEES IT" test for
//! the modifier-aware key-forwarding fix: the REAL `dot-agent-deck` binary
//! driven through the vt100 `TuiDeck` harness, with a REAL interactive `claude`
//! (cheap Haiku model, NO `-p`) booted live in the pane, typed into with real
//! keystroke bytes.
//!
//! ## Why a stand-in cannot cover this
//! `cat` — the usual pane stand-in — has no notion of a draft, so it cannot
//! distinguish "inserted a newline" from "submitted". Newline-vs-submit IS the
//! behavior under test, so only an agent with a real prompt editor can prove it.
//! The PRD says exactly this (Technical Approach M4).
//!
//! ## The chain this exercises, end to end
//!   1. The harness writes `ESC[13;2u` to the DECK's PTY — what a kitty-capable
//!      terminal emits for Shift+Enter once the enhanced protocol is active
//!      (M2), and also what the previously-documented Ghostty
//!      `keybind = shift+enter=csi:13;2u` workaround emits.
//!   2. The deck's crossterm decodes it as `Enter + SHIFT` (verified to happen
//!      even with no flags pushed — PRD Verification Notes).
//!   3. `keyevent_to_bytes` (M1) re-encodes it as `ESC[13;2u` rather than
//!      collapsing it to a bare `\r` — the whole bug — and `write_raw_bytes`
//!      forwards it to the agent's PTY.
//!   4. The real agent inserts a newline and does NOT submit.
//!
//! Before the fix step 3 emitted `\r`, so the agent submitted the draft. The
//! NEGATIVE half of the assertion below is what pins that regression.
//!
//! ## How "no submission" is proved deterministically (no LLM judgement)
//! In a real agent's prompt editor a submitted draft LEAVES the input box (it is
//! repainted into the transcript, several rows up, and the box is cleared before
//! the next characters land). A draft that merely gained a newline keeps BOTH
//! lines inside the box, on CONSECUTIVE rows. So the row of line two being
//! exactly one below the row of line one is simultaneously the "a newline was
//! inserted" assertion and the "no submission happened" assertion, and neither
//! depends on what the model decides to say. A second, independent negative
//! backs it up: line one is a directive that would create a uniquely-named
//! sentinel file if it were ever submitted, and that file must not exist.
//!
//! Decision 23 cost: the load-bearing assertions submit NOTHING, so they cost
//! zero LLM tokens — only the soft positive control at the end (one plain Enter,
//! one `touch`) spends a short Haiku turn. Local-only (Decision 8 / rule 5):
//! gated on the `e2e` feature so CI's `cargo test-fast` never compiles it.
//! Flaky-tolerant (real agent) per rule 4 — run once, not looped.
//!
//! Bug fix, not a showcase: NO ` [reel]` marker on the catalog entry (PRD #180).

mod common;

use std::time::Duration;

use common::TuiDeck;
use spec::spec;

/// Cheapest Claude tier that runs a directive turn (same pin the other
/// real-`claude` e2es use).
const PINNED_MODEL: &str = "claude-haiku-4-5-20251001";

/// First line of the draft. Doubles as the submit tripwire: if Shift+Enter ever
/// submits again, the agent receives this directive and creates [`SENTINEL`].
/// Kept short so it cannot wrap inside the pane's input box (a wrapped line
/// would occupy two rows and make the adjacency assertion meaningless).
const LINE_ONE: &str = "Run: touch shiftnl-7f3c.txt";
/// Second line of the same draft, typed AFTER the Shift+Enter injection.
const LINE_TWO: &str = "Then stop. marker bravo-7f3c";

/// Substring located on the rendered grid to find the draft's first row.
/// Uniquely-named so it survives LLM phrasing variance and cannot collide with
/// deck chrome.
const MARKER_ONE: &str = "shiftnl-7f3c.txt";
/// Substring located on the rendered grid to find the draft's second row.
const MARKER_TWO: &str = "bravo-7f3c";

/// The file [`LINE_ONE`] directs the agent to create. Its ABSENCE is the
/// independent no-submission signal; its later appearance (after a deliberate
/// plain Enter) is the soft positive control proving the directive was
/// executable all along.
const SENTINEL: &str = "shiftnl-7f3c.txt";

/// Shift+Enter as a kitty CSI-u keypress: `CSI 13 ; 2 u` (keycode 13 = Enter,
/// modifier param `1 + shift(1)` = 2). Written to the DECK's PTY, i.e. the deck
/// sees it as an incoming keypress exactly as a kitty-capable terminal would
/// deliver it.
const SHIFT_ENTER_CSI_U: &[u8] = b"\x1b[13;2u";

/// The agent's own input-box affordance — present once its prompt editor is
/// mounted and accepting keystrokes. Waiting on it (rather than typing blind
/// into a booting TUI) is what keeps the keystroke sequence below deterministic.
const AGENT_INPUT_READY: &str = "? for shortcuts";

/// Are both draft markers on `grid` with the second on the row immediately
/// below the first? `None` while either marker is still missing, so the settle
/// wait can distinguish "not painted yet" from "painted, wrong geometry".
fn draft_rows(grid: &str) -> Option<bool> {
    let row_of = |needle: &str| grid.lines().position(|line| line.contains(needle));
    let one = row_of(MARKER_ONE)?;
    let two = row_of(MARKER_TWO)?;
    Some(two == one + 1)
}

/// Scenario: Launch the REAL `dot-agent-deck` binary through the vt100 `TuiDeck`
/// harness with a restored saved session whose one pane runs a REAL interactive
/// `claude` on Haiku (`--allowedTools Bash`, NO `-p`), with the per-test HOME
/// carrying imported credentials and pre-seeded project trust so the first-run
/// onboarding and trust gates clear without a keystroke. The restored pane
/// auto-focuses, so keys typed at the deck are forwarded to the embedded agent.
/// Once the agent's prompt editor is up, type the draft line `Run: touch
/// shiftnl-7f3c.txt`, then inject `ESC[13;2u` — the CSI-u encoding of
/// Shift+Enter that a kitty-capable terminal emits — into the deck's PTY, then
/// type a second line `Then stop. marker bravo-7f3c`. Assert on the rendered
/// vt100 grid that the draft became TWO lines: both markers are on screen and
/// the second sits on the row IMMEDIATELY BELOW the first, which is only true
/// while both lines live inside the same input box. That adjacency is also the
/// no-submission proof — a submitted first line would have been repainted into
/// the transcript far above the box before the second line was typed. Back it
/// with an independent negative: the directive's uniquely-named sentinel file
/// `shiftnl-7f3c.txt` must NOT exist in the pane's cwd. Finally, best-effort
/// (logged, not gating): press plain Enter and report whether the sentinel then
/// appears — the positive control showing the directive was executable and that
/// plain Enter still submits. PTY-attached, so it records a `full-stream.cast`;
/// bug fix, so NOT reel-marked. Flaky-tolerant (real agent) — run once, not
/// looped.
#[spec("embed/key-forwarding/001")]
#[test]
fn key_forwarding_001_shift_enter_inserts_newline_without_submitting() {
    // Decision 26 runtime-skip: a missing CLI / credentials is an environmental
    // condition, not a broken test.
    skip_unless!(common::check_claude_available());

    // A FULLY INTERACTIVE agent (no `-p`): print mode has no prompt editor, so
    // it could not show newline-vs-submit at all. `--allowedTools Bash`
    // whitelists the one tool the directive needs so no permission prompt can
    // block the pane (we deliberately avoid `--dangerously-skip-permissions`,
    // which Claude Code refuses under root).
    let agent_command = format!("claude --model {PINNED_MODEL} --allowedTools Bash");

    let deck = TuiDeck::builder()
        // Real credentials so the daemon-spawned interactive agent authenticates.
        .with_imported_claude_credentials()
        // Pre-trust the pane's cwd (the per-test work dir) so the first-run
        // onboarding + trust gates clear with no human keystroke — otherwise the
        // trust dialog, not the prompt editor, would eat our keystrokes.
        .with_claude_trust_workdir()
        // Auto-open one pane running the real interactive agent against the
        // fixture cwd. The restored pane auto-focuses, which is what routes our
        // keystrokes into the embedded pane (UiMode::PaneInput).
        .with_continue_session("shift-enter", &agent_command)
        .launch_with_fixture("minimal");

    // The restored pane is focused: the bottom bar shows the focused-pane
    // Command-Mode affordance. From here every key we send is forwarded to the
    // embedded agent rather than handled by the dashboard.
    deck.wait_for_string("[Command Mode Ctrl+D]");

    // Wait for the agent's prompt editor to mount before typing. Generous
    // ceiling (Design Decision #7): a real agent's boot is seconds, not
    // milliseconds, and typing into a still-booting TUI would be the classic
    // flake.
    assert!(
        deck.wait_for_grid_string_within(AGENT_INPUT_READY, Duration::from_secs(120)),
        "the embedded agent's prompt editor never became ready (no {AGENT_INPUT_READY:?} \
         affordance within 120s), so the keystroke sequence under test could not be \
         delivered.\nFinal grid:\n{}",
        deck.snapshot_grid()
    );

    // --- The keystroke sequence a user performs: type, Shift+Enter, type. ---

    deck.send_bytes(LINE_ONE.as_bytes());
    assert!(
        deck.wait_for_grid_string_within(MARKER_ONE, Duration::from_secs(30)),
        "the first draft line never appeared in the embedded pane — plain characters are \
         not reaching the agent, so the Shift+Enter behavior below cannot be \
         measured.\nFinal grid:\n{}",
        deck.snapshot_grid()
    );

    // THE KEY UNDER TEST. The deck's crossterm decodes this as `Enter + SHIFT`;
    // the M1 encoder must forward it as CSI-u rather than collapsing it to the
    // bare `\r` that made the agent submit.
    deck.send_bytes(SHIFT_ENTER_CSI_U);

    deck.send_bytes(LINE_TWO.as_bytes());
    assert!(
        deck.wait_for_grid_string_within(MARKER_TWO, Duration::from_secs(30)),
        "the second draft line never appeared in the embedded pane after the Shift+Enter \
         injection.\nFinal grid:\n{}",
        deck.snapshot_grid()
    );

    // Let the agent's prompt editor finish its repaint before measuring rows —
    // it paints optimistically at the cursor first, then re-lays out the whole
    // input box (which grows by a row and shifts up), so only the settled frame
    // carries the real geometry. NOT `wait_until_quiescent`: a live agent
    // animates its spinner, so the deck's byte stream never goes idle and
    // quiescence would simply time out. This wait does not decide the outcome —
    // the assertion below re-checks the settled grid either way, so a layout
    // that never settles still produces the detailed diagnostic.
    deck.wait_for_grid_predicate_within(Duration::from_secs(15), |g| draft_rows(g) == Some(true));

    // --- Load-bearing assertion: the draft is TWO LINES of ONE input box. ---
    let grid = deck.snapshot_grid();
    let (_, row_one) = deck.find_in_grid(MARKER_ONE).unwrap_or_else(|| {
        panic!("the first draft line left the rendered grid entirely.\nFinal grid:\n{grid}")
    });
    let (_, row_two) = deck.find_in_grid(MARKER_TWO).unwrap_or_else(|| {
        panic!("the second draft line left the rendered grid entirely.\nFinal grid:\n{grid}")
    });

    assert_eq!(
        row_two,
        row_one + 1,
        "Shift+Enter did not insert a newline into the agent's draft. Expected the second \
         line ({MARKER_TWO:?}, row {row_two}) to sit on the row IMMEDIATELY BELOW the first \
         ({MARKER_ONE:?}, row {row_one}) — the two consecutive rows of a single two-line \
         input box. A row gap this large means the first line was SUBMITTED (repainted into \
         the transcript above the box, with the second line typed into a freshly-cleared \
         box); an equal row means no newline was inserted at all. Either way the deck \
         collapsed `Enter + SHIFT` to a bare CR instead of forwarding \
         `ESC[13;2u`.\nFinal grid:\n{grid}"
    );

    // --- Independent negative: nothing was submitted, so the directive on the
    // first line never ran. `wait_for_path` is the harness's bounded poll (no
    // raw sleep in an e2e body, Decision 21); a submit would have executed the
    // `touch` well inside this window, since the assertions above already gave
    // the agent tens of seconds. ---
    let sentinel = deck.workdir().join(SENTINEL);
    assert!(
        !common::wait_for_path(&sentinel, Duration::from_secs(10)),
        "the sentinel {SENTINEL:?} was created in the pane cwd, which means the draft's \
         first line was SUBMITTED and executed — Shift+Enter submitted instead of inserting \
         a newline.\nFinal grid:\n{}",
        deck.snapshot_grid()
    );

    // --- Soft positive control (logged, NOT gating): plain Enter must still
    // submit, and the directive must be executable — which is what makes the
    // sentinel's absence above meaningful rather than vacuous. Left non-gating
    // because it is the only step that depends on a live model turn, and rule 4
    // keeps LLM variance out of load-bearing assertions. ---
    deck.send_bytes(b"\r");
    let submitted = common::wait_for_path(&sentinel, Duration::from_secs(120));
    eprintln!(
        "positive control (soft): plain Enter submitted the two-line draft and the agent \
         created {SENTINEL:?} = {submitted}"
    );
}
