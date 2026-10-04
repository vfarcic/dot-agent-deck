#![cfg(feature = "e2e")]

//! L2 checks on the harness's own teardown — what `TuiDeck`'s `Drop` does when a
//! test panics with a deck still alive.
//!
//! Issue #1566: a test that panicked about 5s in lived on until nextest's 180s
//! slow-timeout killed it, so CI reported a timeout instead of the panic. The
//! whole linger was the failure dump regenerating the paired `test.md`, which
//! syn-parses every `#[spec]`-bearing source file in a debug build: measured
//! 8–24s on a loaded 16-core box (2026-10-04), against 0.4s for the rest of the
//! dump, and minutes on CI's starved 4-CPU runners. No failure consumer reads that
//! doc — CI uploads no recordings, and the demo-reel adapter refuses any
//! recording whose provenance is not `passed` — so the panic path no longer
//! regenerates it.

mod common;

use common::TuiDeck;
use spec::spec;

/// Scenario: On a thread named after this test, start the deck on the minimal
/// fixture, wait for the dashboard, and panic with the deck still alive, as a
/// failing e2e test does. The failure dump is still written (with a `failed`
/// provenance), but the paired `test.md` is not regenerated, which is the step
/// that kept a panicked test's process alive for minutes.
#[test]
#[spec("harness/teardown/001")]
fn teardown_001_a_panicking_test_skips_the_paired_doc_regeneration() {
    let recordings = common::current_test_recordings_dir();
    let paired_doc = recordings.join("test.md");
    // A previous `DOT_AGENT_DECK_RECORD=1` run or `cargo xtask docs --tests`
    // may have left one; the assertion below is about THIS drop writing it.
    match std::fs::remove_file(&paired_doc) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => panic!("remove stale {}: {e}", paired_doc.display()),
    }

    // The harness names the deck, and so its recordings directory and the
    // `#[spec]` function the regeneration looks up, after the current thread.
    // Naming the panicking thread after this test makes the regeneration, if it
    // runs, find this test's own `#[spec]` and write `test.md` — an observation
    // that does not depend on how loaded the machine is.
    let thread_name = std::thread::current()
        .name()
        .expect("libtest names the test thread")
        .to_string();
    let joined = std::thread::Builder::new()
        .name(thread_name)
        .spawn(|| {
            let deck = TuiDeck::builder().launch_with_fixture("minimal");
            deck.wait_for_string("No active agents");
            panic!("deliberate panic with a live deck (issue #1566)");
        })
        .expect("spawn the panicking thread")
        .join();
    assert!(
        joined.is_err(),
        "the deck thread must have panicked, or its drop never took the failure path"
    );

    // Control: the drop really ran its failure dump, so the doc's absence below
    // is the regeneration being skipped rather than the dump never happening.
    let provenance = std::fs::read_to_string(recordings.join("provenance.json"))
        .expect("a panicking drop still writes the failure dump");
    let parsed: serde_json::Value =
        serde_json::from_str(&provenance).expect("provenance.json is JSON");
    assert_eq!(
        parsed["outcome"], "failed",
        "the dump must record the failure it was written for: {provenance}"
    );
    assert!(
        recordings.join("final-grid.txt").is_file(),
        "a panicking drop still writes the final grid"
    );

    assert!(
        !paired_doc.exists(),
        "the panicking drop regenerated {}: that step syn-parses every #[spec] \
         source file and kept a failed test's process alive until nextest's \
         180s timeout on starved CI runners (issue #1566)",
        paired_doc.display()
    );
}
