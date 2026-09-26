#![cfg(all(feature = "e2e", feature = "e2e-live"))]

//! Credentialed (lane 2) half of the Ctrl+W pane coverage — see the sibling
//! `e2e_pane_input.rs` for the deterministic half.
//!
//! Issue #502 split this out at TEST level rather than leaving the whole file
//! in lane 2: `prompt/pane-input/021` drives a real bash/readline pane and
//! needs no agent credential, while `prompt/pane-input/022` needs a live
//! Claude. The two shared no helper functions and no constants, so the split
//! duplicated nothing. Catalog ids are unaffected — `tests/CATALOG.md` and
//! `.dot-agent-deck/recordings/` are keyed by the `#[spec]` id, never by the
//! file it lives in.

mod common;

use std::time::Duration;

use common::TuiDeck;
use spec::spec;

const HAIKU_MODEL: &str = "claude-haiku-4-5-20251001";
const CLAUDE_PANE_NAME_SUFFIX: &str = "safe-close-claude";
const CTRL_W_KEPT_WORD: &str = "ctrlw_keep_6f3a";
const CTRL_W_DELETED_WORD: &str = "ctrlw_delete_91b2";
const ERASE_PANE_NAME_SUFFIX: &str = "erase-burst-claude";
const ERASE_KEPT_WORD: &str = "erase_keep_4c71";
const ERASE_PAYLOAD_WORD: &str = "erase_payload_8d20";
const ERASE_TAIL_WORD: &str = "erase_tail_9e33";
/// Issue #876: a burst the size of a real one-shot payload. The production
/// idle-worker prompt encodes to ~524 bytes, so a drain of it is an erase burst
/// of that many keypresses arriving as ONE write — which is the shape a TUI's
/// bulk-input heuristic is most likely to misread. `MAX_DRAINABLE_STRANDED_BYTES`
/// is 1024, so this sits between the real payload and the cap.
const ERASE_BULK_LEN: usize = 600;

/// Scenario: Runtime-skip unless Claude credentials are available, then launch a genuine interactive Claude Haiku pane with project trust and allowed tools configured. Type two sentinel words at the live agent prompt, press Ctrl+W, and verify the second word disappears before returning to command mode; the same Claude pane and daemon agent must still exist.
#[spec("prompt/pane-input/022")]
#[test]
fn pane_input_022_ctrl_w_does_not_tear_down_interactive_claude() {
    skip_unless!(common::check_claude_available());

    let deck = TuiDeck::builder()
        .with_pty_size(160, 45)
        .with_imported_claude_credentials()
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");

    let cwd = deck.workdir().to_path_buf();
    let mut trust_paths = vec![cwd.to_string_lossy().into_owned()];
    if let Ok(canonical) = cwd.canonicalize() {
        let canonical = canonical.to_string_lossy().into_owned();
        if !trust_paths.contains(&canonical) {
            trust_paths.push(canonical);
        }
    }
    common::seed_claude_trust_in_home(deck.home_dir(), &trust_paths)
        .expect("seed Claude onboarding and per-folder trust");

    deck.send_keys(b"\x0e");
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" ");
    deck.wait_for_string("New Agent");
    deck.send_keys(b"\t");
    deck.send_keys(CLAUDE_PANE_NAME_SUFFIX.as_bytes());
    deck.send_keys(b"\t");
    deck.send_keys(format!("claude --model {HAIKU_MODEL} --allowedTools Bash Read").as_bytes());
    let (submit_col, submit_row) = deck.wait_for_in_grid("[Submit]");
    deck.click(submit_col, submit_row);

    assert!(
        deck.wait_for_grid_string_within("Claude Code v", Duration::from_secs(45)),
        "the genuine interactive Claude UI must render before typing; grid:\n{}",
        deck.snapshot_grid()
    );
    deck.wait_for_string("[Command Mode Ctrl+D]");

    // The Name field starts with the selected directory's basename, so typing
    // the test label appends it to a nondeterministic tempdir prefix. Once the
    // genuine UI has rendered, match that stable suffix in the daemon snapshot
    // and retain the identity instead of baking the prefix into the test.
    let records = common::agent_records_on(deck.attach_socket_path());
    let claude_agent_id = records
        .iter()
        .find(|record| {
            record
                .display_name
                .as_deref()
                .is_some_and(|name| name.ends_with(CLAUDE_PANE_NAME_SUFFIX))
        })
        .unwrap_or_else(|| {
            panic!(
                "the real interactive Claude pane whose name ends with \
                 {CLAUDE_PANE_NAME_SUFFIX:?} must be registered before exercising Ctrl+W; \
                 records={records:?}"
            )
        })
        .id
        .clone();

    deck.send_keys(format!("{CTRL_W_KEPT_WORD} {CTRL_W_DELETED_WORD}").as_bytes());
    deck.wait_until_grid("both Ctrl+W sentinel words in the Claude prompt", |grid| {
        grid.contains(CTRL_W_KEPT_WORD) && grid.contains(CTRL_W_DELETED_WORD)
    });
    deck.send_keys(b"\x17");
    deck.wait_until_grid(
        "Ctrl+W forwarded to Claude and deleted the final word",
        |grid| grid.contains(CTRL_W_KEPT_WORD) && !grid.contains(CTRL_W_DELETED_WORD),
    );
    deck.send_keys(b"\x04");

    deck.wait_until_grid(
        "the Claude pane still visible after returning to command mode",
        |grid| !grid.contains("No active agents"),
    );
    let grid = deck.snapshot_grid();
    assert!(
        !grid.contains("No active agents"),
        "Ctrl+W must not tear down the real Claude pane\nFinal grid:\n{grid}"
    );
    assert!(
        common::agent_records_on(deck.attach_socket_path())
            .iter()
            .any(|record| record.id == claude_agent_id),
        "the same daemon-side Claude agent must still exist after Ctrl+W"
    );
}

/// Scenario: Runtime-skip unless Claude credentials are available, then launch a genuine interactive Claude Haiku pane with project trust and allowed tools configured. Type a sentinel word, a space and a stand-in for a daemon payload at the live agent prompt, then send exactly as many `DEL` bytes as that payload has characters — the byte sequence issue #876's drain writes. Type a third sentinel straight after and verify the prompt now reads the first word, one space and the third, which holds only if exactly the payload was erased. Then type 600 filler characters — the size of a real one-shot payload — erase exactly that many in one write, and verify the prompt collapses back to the same two words.
#[spec("prompt/pane-input/038")]
#[test]
fn pane_input_038_erase_burst_undoes_a_payload_in_a_live_claude_prompt() {
    skip_unless!(common::check_claude_available());

    let deck = TuiDeck::builder()
        .with_pty_size(160, 45)
        .with_imported_claude_credentials()
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");

    let cwd = deck.workdir().to_path_buf();
    let mut trust_paths = vec![cwd.to_string_lossy().into_owned()];
    if let Ok(canonical) = cwd.canonicalize() {
        let canonical = canonical.to_string_lossy().into_owned();
        if !trust_paths.contains(&canonical) {
            trust_paths.push(canonical);
        }
    }
    common::seed_claude_trust_in_home(deck.home_dir(), &trust_paths)
        .expect("seed Claude onboarding and per-folder trust");

    deck.send_keys(b"\x0e");
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" ");
    deck.wait_for_string("New Agent");
    deck.send_keys(b"\t");
    deck.send_keys(ERASE_PANE_NAME_SUFFIX.as_bytes());
    deck.send_keys(b"\t");
    deck.send_keys(format!("claude --model {HAIKU_MODEL} --allowedTools Bash Read").as_bytes());
    let (submit_col, submit_row) = deck.wait_for_in_grid("[Submit]");
    deck.click(submit_col, submit_row);

    assert!(
        deck.wait_for_grid_string_within("Claude Code v", Duration::from_secs(45)),
        "the genuine interactive Claude UI must render before typing; grid:\n{}",
        deck.snapshot_grid()
    );
    deck.wait_for_string("[Command Mode Ctrl+D]");

    let records = common::agent_records_on(deck.attach_socket_path());
    let claude_agent_id = records
        .iter()
        .find(|record| {
            record
                .display_name
                .as_deref()
                .is_some_and(|name| name.ends_with(ERASE_PANE_NAME_SUFFIX))
        })
        .unwrap_or_else(|| {
            panic!(
                "the real interactive Claude pane whose name ends with \
                 {ERASE_PANE_NAME_SUFFIX:?} must be registered before exercising the erase \
                 burst; records={records:?}"
            )
        })
        .id
        .clone();

    // The user's own draft, then the stand-in for a daemon payload appended to
    // it — the exact state an `Ambiguous` guarded submit leaves behind.
    deck.send_keys(format!("{ERASE_KEPT_WORD} {ERASE_PAYLOAD_WORD}").as_bytes());
    deck.wait_until_grid("both erase sentinels in the live Claude prompt", |grid| {
        grid.contains(ERASE_KEPT_WORD) && grid.contains(ERASE_PAYLOAD_WORD)
    });

    // Issue #876's drain, byte for byte: ONE write carrying exactly one `DEL`
    // per payload character. The deck forwards `KeyCode::Backspace` as `0x7f`
    // (`ui::keyevent_to_bytes`), which is the same byte
    // `agent_pty::PANE_ERASE_BYTE` puts on the PTY, so what Claude's editor
    // receives here is what the daemon writes.
    deck.send_keys(&vec![0x7fu8; ERASE_PAYLOAD_WORD.len()]);
    deck.wait_until_grid("the payload gone from the live prompt", |grid| {
        grid.contains(ERASE_KEPT_WORD) && !grid.contains(ERASE_PAYLOAD_WORD)
    });

    // Typing a third sentinel is what makes the count EXACT in both directions
    // rather than only in one. Asserting the payload's absence alone cannot see
    // an UNDER-erase: one erase short leaves a single leading payload character,
    // which no substring test for the whole word would notice. With a tail word
    // typed straight after the burst, the line reads `<kept> <tail>` contiguously
    // only when exactly the payload was removed — one erase short wedges a
    // leftover character between them, and one erase too many eats the space and
    // then the kept word itself.
    deck.send_keys(ERASE_TAIL_WORD.as_bytes());
    let joined = format!("{ERASE_KEPT_WORD} {ERASE_TAIL_WORD}");
    deck.wait_until_grid(
        "the erase burst removed exactly the payload — no more and no less",
        |grid| grid.contains(&joined),
    );

    let grid = deck.snapshot_grid();
    assert!(
        grid.contains(&joined),
        "exactly {} erases must remove exactly the {} payload characters: the live prompt has \
         to read {joined:?} with nothing between the two words. Too few leaves a payload \
         character behind; too many eat the space and then the word the user typed. \
         grid:\n{grid}",
        ERASE_PAYLOAD_WORD.len(),
        ERASE_PAYLOAD_WORD.len()
    );
    assert!(
        !grid.contains(ERASE_PAYLOAD_WORD),
        "every payload character must be gone from the live prompt; grid:\n{grid}"
    );

    // A burst the size of a REAL drain. The short one above proves the count is
    // exact; this proves the SHAPE survives — `ERASE_BULK_LEN` erases arriving
    // as one write, which is what draining a production idle prompt looks like
    // and the case where a TUI that classifies bulk input heuristically could
    // read the burst as a paste and insert it as text instead.
    deck.send_keys(&vec![b'x'; ERASE_BULK_LEN]);
    deck.wait_until_grid("the bulk filler in the live Claude prompt", |grid| {
        grid.contains(&"x".repeat(40))
    });
    deck.send_keys(&vec![0x7fu8; ERASE_BULK_LEN]);
    deck.wait_until_grid(
        "a real-sized erase burst collapsed the prompt back to what preceded it",
        |grid| grid.contains(&joined) && !grid.contains(&"x".repeat(40)),
    );

    deck.send_keys(b"\x04");
    deck.wait_until_grid(
        "the Claude pane still visible after returning to command mode",
        |grid| !grid.contains("No active agents"),
    );
    assert!(
        common::agent_records_on(deck.attach_socket_path())
            .iter()
            .any(|record| record.id == claude_agent_id),
        "the same daemon-side Claude agent must still exist after the erase burst"
    );
}
