#![cfg(all(feature = "e2e", feature = "e2e-live"))]

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
//! lines inside the box, on CONSECUTIVE rows, bracketed by the box's own
//! horizontal rules. So "line two is exactly one row below line one, and both
//! sit inside one rule-delimited box" is simultaneously the "a newline was
//! inserted" assertion and the "no submission happened" assertion, and neither
//! depends on what the model decides to say. A second, independent negative
//! backs it up: line one is a directive that would create a uniquely-named
//! sentinel file if it were ever submitted, and that file must not exist — with
//! a GATING positive control at the end proving the directive was executable, so
//! the absence cannot pass vacuously.
//!
//! ## What pins M2 (the startup protocol push)
//! Step 1 above injects an ALREADY-CSI-u-encoded keypress, which crossterm
//! decodes even with no enhancement flags pushed. So the draft assertions cover
//! the M1 encoder and would still pass with M2 deleted. `001` therefore also
//! asserts the push appears in the deck's output stream, and `002` covers the
//! push/pop lifecycle on its own — deterministically, with no agent at all.
//!
//! Decision 23 cost: `001`'s draft assertions submit NOTHING, so they cost zero
//! LLM tokens; only its positive control (one plain Enter, one `touch`) spends a
//! short Haiku turn, and `002` spends none. Local-only (Decision 8 / rule 5):
//! gated on the `e2e` feature so CI's `cargo test-fast` never compiles it.
//! Flaky-tolerant (real agent) per rule 4 — run once, not looped.
//!
//! Bug fix, not a showcase: NO ` [reel]` marker on the catalog entries (PRD #180).

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
/// plain Enter) is the GATING positive control proving the directive was
/// executable all along, without which the absence would prove nothing.
const SENTINEL: &str = "shiftnl-7f3c.txt";

/// Shift+Enter as a kitty CSI-u keypress: `CSI 13 ; 2 u` (keycode 13 = Enter,
/// modifier param `1 + shift(1)` = 2). Written to the DECK's PTY, i.e. the deck
/// sees it as an incoming keypress exactly as a kitty-capable terminal would
/// deliver it.
const SHIFT_ENTER_CSI_U: &[u8] = b"\x1b[13;2u";

/// The agent's own input-box affordance — present once its prompt editor is
/// mounted and accepting keystrokes. Waiting on it (rather than typing blind
/// into a booting TUI) is what keeps the keystroke sequence below deterministic.
///
/// UNPINNED third-party UI copy: this is Claude Code's own footer hint, so a
/// Claude release that rewords it breaks the readiness wait (the test would then
/// fail on the 120 s timeout below, with the grid in its diagnostic). It is
/// isolated here as a single named constant precisely so that breakage is a
/// ONE-LINE fix. A structural signal was considered instead and rejected: the
/// only structural candidate is the input box's rules ([`is_rule_row`]), which
/// the *deck's* own chrome also draws, so it would report ready before the agent
/// has mounted anything.
const AGENT_INPUT_READY: &str = "? for shortcuts";

/// What crossterm 0.28 writes for
/// `PushKeyboardEnhancementFlags(DISAMBIGUATE_ESCAPE_CODES)` — `CSI > 1 u`
/// (`csi!(">")` + flag bits + `u`; DISAMBIGUATE is bit `0b0000_0001`). Seeing
/// this in the deck's OUTPUT stream is the only direct evidence M2 ran, which is
/// what stops this file from passing with M2 deleted: the CSI-u keypress injected
/// below is decoded by crossterm even with no flags pushed, so it exercises the
/// M1 encoder alone. Pinned against a crossterm upgrade by the L1 test
/// `ui::tests::keyboard_enhancement_wire_bytes_are_the_ones_the_e2e_asserts`.
const PUSH_ENHANCEMENT: &str = "\x1b[>1u";
/// What crossterm 0.28 writes for `PopKeyboardEnhancementFlags` — `CSI < 1 u`.
/// Note the `<`: it is NOT the `>` form the push uses.
const POP_ENHANCEMENT: &str = "\x1b[<1u";

/// Minimum run of `─` (U+2500) that marks a row as a horizontal RULE — a region
/// boundary — rather than a row that merely contains incidental box-drawing.
const RULE_MIN_RUN: usize = 20;

/// Does `line` carry a long horizontal rule? The agent's prompt editor is
/// bracketed by exactly two of these (its top and bottom edge), which is the
/// structural signature [`draft_geometry`] uses to prove a row is draft content
/// rather than transcript content.
fn is_rule_row(line: &str) -> bool {
    let mut run = 0usize;
    for ch in line.chars() {
        run = if ch == '─' { run + 1 } else { 0 };
        if run >= RULE_MIN_RUN {
            return true;
        }
    }
    false
}

/// Rows of the two draft markers, plus whether the prompt-editor box's rules sit
/// immediately above the first and immediately below the second. `None` while
/// either marker is still missing, so the settle wait can distinguish "not
/// painted yet" from "painted, wrong geometry".
///
/// The rule checks are what scope the markers to the INPUT BOX. Row adjacency on
/// its own reads any two consecutive rows alike, so a submitted draft that the
/// agent repainted into the transcript as two consecutive lines would satisfy it
/// vacuously. A two-line draft *inside the box* has the box's own rules directly
/// bracketing it; the same two lines in the transcript have ordinary output or
/// blank rows around them, with the box's top rule further down the grid.
fn draft_geometry(grid: &str) -> Option<(usize, usize, bool, bool)> {
    let lines: Vec<&str> = grid.lines().collect();
    let row_of = |needle: &str| lines.iter().position(|line| line.contains(needle));
    let one = row_of(MARKER_ONE)?;
    let two = row_of(MARKER_TWO)?;
    let rule_above = one
        .checked_sub(1)
        .and_then(|r| lines.get(r))
        .is_some_and(|l| is_rule_row(l));
    let rule_below = lines.get(two + 1).is_some_and(|l| is_rule_row(l));
    Some((one, two, rule_above, rule_below))
}

/// Is the draft two lines of ONE prompt-editor box — consecutive rows, both
/// inside the box's rules? `None` while either marker is still missing.
fn draft_is_two_lines_of_one_box(grid: &str) -> Option<bool> {
    let (one, two, rule_above, rule_below) = draft_geometry(grid)?;
    Some(two == one + 1 && rule_above && rule_below)
}

/// Scenario: Launch the real `dot-agent-deck` through the vt100 `TuiDeck`
/// harness with one auto-focused pane running a REAL interactive `claude` on
/// Haiku, then type `Run: touch shiftnl-7f3c.txt`, inject `ESC[13;2u` (the CSI-u
/// Shift+Enter a kitty-capable terminal emits) into the deck's PTY, and type
/// `Then stop. marker bravo-7f3c`. Assert the deck negotiated the enhanced
/// keyboard protocol at startup, and that the draft became two lines of ONE
/// prompt-editor box — the second marker on the row immediately below the first,
/// both bracketed by the box's rules — which is simultaneously the newline proof
/// and the no-submission proof, backed by the directive's sentinel file
/// `shiftnl-7f3c.txt` being absent until a deliberate plain Enter creates it.
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

    // M2 is live for THIS run. Without this the file would pass with the startup
    // protocol push deleted outright — the CSI-u keypress injected below is
    // decoded by crossterm with no flags pushed, so the rest of the test pins
    // only the M1 encoder. `embed/key-forwarding/002` covers the push/pop
    // lifecycle on its own; this asserts the fix under test was actually in
    // effect while the forwarding behavior below was measured.
    assert!(
        deck.wait_for_stream_string_within(PUSH_ENHANCEMENT, Duration::from_secs(30)),
        "the deck never pushed the enhanced keyboard protocol (no {PUSH_ENHANCEMENT:?} in \
         its output stream), so M2 is not in effect and the Shift+Enter behavior measured \
         below would not reproduce for a user who has no terminal keybind configured."
    );

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
    deck.wait_for_grid_predicate_within(Duration::from_secs(15), |g| {
        draft_is_two_lines_of_one_box(g) == Some(true)
    });

    // --- Load-bearing assertion: the draft is TWO LINES of ONE input box. ---
    let grid = deck.snapshot_grid();
    let (row_one, row_two, rule_above, rule_below) = draft_geometry(&grid).unwrap_or_else(|| {
        panic!("a draft line left the rendered grid entirely.\nFinal grid:\n{grid}")
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

    // Adjacency alone cannot tell draft rows from transcript rows — a submitted
    // draft the agent repainted into the transcript is two consecutive rows too.
    // The prompt-editor box brackets its content with horizontal rules, so
    // requiring one directly above the first line and one directly below the
    // second is what proves these rows are the LIVE DRAFT rather than an echo of
    // a submitted one.
    assert!(
        rule_above && rule_below,
        "the two draft lines are on consecutive rows {row_one}/{row_two} but are NOT bracketed \
         by the prompt editor's rules (rule directly above = {rule_above}, rule directly below \
         = {rule_below}), so they are not the live draft inside one input box — most likely a \
         SUBMITTED draft repainted into the transcript, which would mean Shift+Enter \
         submitted.\nFinal grid:\n{grid}"
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

    // --- GATING positive control: plain Enter must still submit, and the
    // directive must actually be executable. This is what makes the sentinel's
    // absence above mean "nothing was submitted" rather than "nothing was
    // submitted, or the agent was merely slow, or it declined the tool call".
    // A non-gating control cannot distinguish those, so the negative it backs
    // could pass vacuously — the whole point of asserting it.
    //
    // It is the one step that depends on a live model turn, so it carries the
    // widest ceiling in the file (120 s). That is the deliberate trade: this file
    // lives in the flaky-tolerant pre-PR e2e tier (rule 4/5), and a control that
    // never fails is not a control. ---
    deck.send_bytes(b"\r");
    assert!(
        common::wait_for_path(&sentinel, Duration::from_secs(120)),
        "positive control FAILED: after a plain Enter the agent never created {SENTINEL:?} \
         within 120s. Plain Enter must still submit, and the first line's directive must be \
         executable — without both, the sentinel's absence asserted above proves nothing \
         about Shift+Enter (an agent that was slow or declined the tool call would leave the \
         sentinel missing for entirely the wrong reason).\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
}

/// Scenario: Launch the real `dot-agent-deck` through the vt100 `TuiDeck`
/// harness with no pane at all, and assert its OUTPUT stream carries
/// `ESC[>1u` — the enhanced-keyboard-protocol push (M2) — once the dashboard is
/// up, since the harness answers the capability probe like a kitty-capable
/// terminal. Then quit cleanly with Ctrl+C twice, wait for the process to
/// actually exit, and assert the matching `ESC[<1u` pop appears exactly once,
/// after the push, so the deck cannot leave the user's shell in the enhanced
/// mode.
#[spec("embed/key-forwarding/002")]
#[test]
fn key_forwarding_002_enhanced_keyboard_protocol_pushed_and_popped() {
    // No agent, no credentials, no LLM: the whole behavior is the deck's own
    // terminal negotiation, so this is fully deterministic and costs nothing.
    //
    // `mut` for `wait_for_exit_within` below, which reaps the child.
    let mut deck = TuiDeck::launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");

    // --- The push (Gap 1). The harness's `answer_terminal_queries` replies to
    // the `ESC[?u` / `ESC[c` probe, so `supports_keyboard_enhancement()` returns
    // true here and the gated push fires — modelling exactly the kitty-capable
    // terminal the fix targets. ---
    assert!(
        deck.wait_for_stream_string_within(PUSH_ENHANCEMENT, Duration::from_secs(30)),
        "the deck never pushed the enhanced keyboard protocol: no {PUSH_ENHANCEMENT:?} in its \
         output stream. Without that push a kitty-capable terminal stays in legacy mode, where \
         Shift+Enter has no distinct encoding and arrives as a bare CR — so the modifier is \
         gone before any deck code runs and the M1 encoder never sees it. This is the M2 half \
         of PRD #227, and it is what makes the fix work with NO user-side terminal \
         configuration (docs/troubleshooting.md, changelog.d/227.feature.md).\nStream:\n{:?}",
        deck.stream_text()
    );

    // --- The pop. Ctrl+C opens the quit-confirm dialog; a second Ctrl+C from
    // inside it exits (`Action::Quit`), which is the deck's ordinary clean-exit
    // path through the teardown that pops. ---
    deck.send_bytes(b"\x03");
    deck.wait_for_string("Quit dot-agent-deck?");
    deck.send_bytes(b"\x03");

    // Wait for the PROCESS to be gone (and its output drained) before measuring,
    // not merely for the first pop to appear. Two reasons, both load-bearing:
    //
    //  * MULTIPLICITY. `wait_for_stream_string_within` returns the instant the
    //    first pop shows up, which is mid-shutdown — a SECOND pop written later
    //    (the RAII guard's `Drop`, or the panic hook) would land after that
    //    snapshot, so `count() == 1` would pass against an implementation that
    //    genuinely double-pops. Given how much of this milestone is guard /
    //    teardown / panic-hook restructuring, double-popping is exactly the
    //    regression this assertion exists to catch, and it has to be measured on
    //    the FINAL stream.
    //  * CLEAN EXIT. A zero status proves the Ctrl+C path returned normally
    //    rather than the pop being a side effect of the process dying.
    let exited_cleanly = deck.wait_for_exit_within(Duration::from_secs(30));
    assert_eq!(
        exited_cleanly,
        Some(true),
        "the deck did not exit cleanly after two Ctrl+C ({exited_cleanly:?}; None = still \
         running at the 30s ceiling). Terminal-mode restoration is only meaningful on an \
         orderly exit, and the pop counts below are only trustworthy once the process is \
         gone.\nFinal grid:\n{}\nStream:\n{:?}",
        deck.snapshot_grid(),
        deck.stream_text()
    );

    // The FINAL stream: the process is gone, so nothing more can be appended.
    let stream = deck.stream_text();

    assert_eq!(
        stream.matches(PUSH_ENHANCEMENT).count(),
        1,
        "expected exactly ONE {PUSH_ENHANCEMENT:?} push. Zero means M2 never ran (a \
         kitty-capable terminal then stays in legacy mode, where Shift+Enter has no distinct \
         encoding and arrives as a bare CR — the modifier is gone before any deck code runs \
         and the M1 encoder never sees it). More than one means the deck stacked pushes it \
         cannot symmetrically pop, leaking the enhanced mode into the user's \
         shell.\nStream:\n{stream:?}"
    );
    assert_eq!(
        stream.matches(POP_ENHANCEMENT).count(),
        1,
        "expected exactly ONE {POP_ENHANCEMENT:?} pop for the single push. Zero means the \
         deck exited leaving the enhanced mode set — not cosmetic, it outlives the process \
         and corrupts key delivery in the shell the user is dropped back into. More than one \
         means the test-and-clear on KEYBOARD_ENHANCEMENT_PUSHED stopped making the pop \
         idempotent, and the extra pop discards whatever sits UNDER ours on the terminal's \
         stack — a flag set some other program owns. Both the explicit teardown and the RAII \
         guard's `Drop` run on this exit path, so exactly one is the whole \
         property.\nStream:\n{stream:?}"
    );

    // Ordering, which a count cannot express: a pop before its push would be
    // popping some other program's flags.
    let push_at = stream
        .find(PUSH_ENHANCEMENT)
        .expect("push counted exactly once above");
    let pop_at = stream
        .find(POP_ENHANCEMENT)
        .expect("pop counted exactly once above");
    assert!(
        push_at < pop_at,
        "the pop ({POP_ENHANCEMENT:?} at byte {pop_at}) precedes the push \
         ({PUSH_ENHANCEMENT:?} at byte {push_at}), so the deck popped flags it had not pushed."
    );
}
