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

/// Scenario: Runtime-skip unless Claude credentials are available, then launch a genuine interactive Claude Haiku pane with project trust and allowed tools configured. Type two sentinel words at the live agent prompt, press Ctrl+W, and verify the second word disappears before returning to command mode; the same Claude pane and daemon agent must still exist.
#[spec("prompt/pane-input/022")]
#[test]
fn pane_input_022_ctrl_w_does_not_tear_down_interactive_claude() {
    skip_unless!(common::check_claude_available());

    let deck = TuiDeck::builder()
        .with_pty_size(160, 45)
        .with_imported_claude_credentials()
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");

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
    let (submit_col, submit_row) = deck
        .find_in_grid("[Submit]")
        .expect("new-pane form should render [Submit]");
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
        |grid| !grid.contains("No active sessions"),
    );
    let grid = deck.snapshot_grid();
    assert!(
        !grid.contains("No active sessions"),
        "Ctrl+W must not tear down the real Claude pane\nFinal grid:\n{grid}"
    );
    assert!(
        common::agent_records_on(deck.attach_socket_path())
            .iter()
            .any(|record| record.id == claude_agent_id),
        "the same daemon-side Claude agent must still exist after Ctrl+W"
    );
}
