//! The test harness must detach from any real deck before it spawns anything.
//!
//! Running the suite from inside a deck pane means this process inherits that
//! pane's `DOT_AGENT_DECK_SOCKET` / `_PANE_ID`. Anything spawned inherits them
//! too, and its hooks then post into the developer's LIVE dashboard — a card
//! appears under a fixture pane id and vanishes again. `ff5170d` scrubs these in
//! `agent_pty::spawn`, which is necessary but not sufficient: real `deck.log`
//! evidence shows four of five leaked fixture pane ids arriving from a tree that
//! already had that fix, through other spawn paths. Clearing the vars from the
//! test process covers every spawn path at once, including ones added later.
//!
//! nextest runs each test in its own process, so mutating this process's
//! environment cannot affect another test.

mod common;

/// Scenario: Set all four deck endpoint variables to values that mimic a live
/// deck, call the harness setup hook, and assert every one of them is gone —
/// so no child this process spawns can inherit a route to a real daemon.
#[test]
fn harness_clears_inherited_deck_endpoints() {
    for (var, value) in [
        (
            "DOT_AGENT_DECK_SOCKET",
            "/run/user/1000/dot-agent-deck.sock",
        ),
        (
            "DOT_AGENT_DECK_ATTACH_SOCKET",
            "/run/user/1000/dot-agent-deck-attach.sock",
        ),
        ("DOT_AGENT_DECK_PANE_ID", "8"),
        ("DOT_AGENT_DECK_AGENT_ID", "8"),
    ] {
        // SAFETY: single-threaded test body, before the harness starts anything.
        unsafe { std::env::set_var(var, value) };
    }

    common::init_test_env();

    for var in [
        "DOT_AGENT_DECK_SOCKET",
        "DOT_AGENT_DECK_ATTACH_SOCKET",
        "DOT_AGENT_DECK_PANE_ID",
        "DOT_AGENT_DECK_AGENT_ID",
    ] {
        assert!(
            std::env::var_os(var).is_none(),
            "{var} survived harness setup — a spawned child would inherit it and \
             could post hook events into a live deck"
        );
    }
}

// ---------------------------------------------------------------------------
// PR #805 audit blocker 3 — a stale recording must not survive a run
// ---------------------------------------------------------------------------
//
// `TuiDeck` writes its artifacts only from `Drop`, and only on a panic or under
// `DOT_AGENT_DECK_RECORD`. Every route that ends a run without reaching `Drop` —
// SIGKILL, a nextest timeout, Ctrl-C, a runtime skip before launch, a failed
// re-recording — used to leave the PREVIOUS run's `full-stream.cast` in place,
// and `.claude/skills/demo-reel-adapter` selects a cast on path existence alone.
// A cast from a revision predating the redaction fixes could therefore be
// stitched into a video and uploaded to YouTube with its link in the PR body and
// the public release notes. (The upload is private by default now, so widening
// it beyond the channel owner and the accounts they deliberately share it with
// is a human step — that bounds the blast radius; it does not make a stale cast
// correct.)
//
// Both call sites of the discard are covered here: the runtime-skip one by
// actually taking it, and the launch one by a source guard, because observing it
// needs a real PTY launch and this file is in the fast tier.

/// The artifacts the harness dumps — the set the discard has to clear. Mirrors
/// `RECORDING_ARTIFACTS` in `tests/common/mod.rs`; the guard below proves the two
/// lists and the dump itself still agree.
const RECORDING_ARTIFACTS: [&str; 4] = [
    "final-grid.txt",
    "final-grid.svg",
    "full-stream.cast",
    "fixture.toml",
];

fn harness_source() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("common")
        .join("mod.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Scenario: Plant a stale `full-stream.cast` — plus the other three recording
/// artifacts and the paired `test.md` — in this test's own recordings directory,
/// then take the runtime-skip path through `skip_unless!`'s helper, which is one
/// of the routes that used to leave that cast behind while nextest reported the
/// test as passed. Afterwards all four artifacts are gone and the paired doc is
/// untouched.
#[test]
fn a_runtime_skip_discards_the_previous_recording() {
    // The helper PANICS instead of skipping when this is set, which would make
    // the test's outcome depend on the developer's environment.
    // SAFETY: single-threaded test body; nextest gives each test its own process.
    unsafe { std::env::remove_var("DOT_AGENT_DECK_REQUIRE_REAL_E2E") };

    let dir = common::current_test_recordings_dir();
    std::fs::create_dir_all(&dir).expect("create this test's recordings dir");
    for name in RECORDING_ARTIFACTS {
        std::fs::write(dir.join(name), b"STALE-ARTIFACT-FROM-AN-EARLIER-REVISION")
            .expect("plant a stale artifact");
    }
    std::fs::write(dir.join("test.md"), b"# generated from the test source\n")
        .expect("plant the paired doc");

    let skipped = common::_skip_if_err(Err("no credential on this host".to_string()));
    assert!(
        skipped,
        "an Err must produce a skip, or this proves nothing"
    );

    for name in RECORDING_ARTIFACTS {
        let path = dir.join(name);
        assert!(
            !path.exists(),
            "{} survived a runtime skip, so an interrupted or skipped run can \
             still hand the demo reel an artifact it did not produce",
            path.display()
        );
    }
    assert_eq!(
        std::fs::read_to_string(dir.join("test.md")).expect("the paired doc must survive"),
        "# generated from the test source\n",
        "the paired `.md` is regenerated from the test source, carries no \
         credential and no run identity, and must not be deleted with the \
         artifacts"
    );

    // This test's recordings directory is its own fixture, so it takes it away
    // again — but only on success, so a failure leaves the evidence in place.
    std::fs::remove_dir_all(&dir).expect("remove this test's fixture recordings dir");
}

/// Scenario: Plant a stale recording artifact the discard CANNOT delete — a
/// directory where `full-stream.cast` should be — and take the runtime-skip path.
/// The harness panics instead of warning and carrying on, so the test fails
/// rather than running to a green finish with an artifact it did not produce.
///
/// PR #805's second audit named the old warn-and-continue a fail-open and it was
/// right: a warning only helps if somebody reads it, the run that printed it was
/// still reported as PASSED, and the artifact it could not remove still satisfies
/// the demo-reel adapter's existence check. Unix-only, because "a deletion that
/// fails for a reason other than NotFound" is arranged here through `EISDIR`.
#[cfg(unix)]
#[test]
fn a_discard_that_cannot_delete_a_stale_artifact_fails_the_run() {
    // SAFETY: single-threaded test body; nextest gives each test its own process.
    unsafe { std::env::remove_var("DOT_AGENT_DECK_REQUIRE_REAL_E2E") };

    let dir = common::current_test_recordings_dir();
    std::fs::create_dir_all(&dir).expect("create this test's recordings dir");
    // `remove_file` on a directory is EISDIR, not NotFound — an undeletable
    // stale artifact, without having to make the tree unwritable (which would
    // also stop the harness from cleaning up after itself).
    let undeletable = dir.join("full-stream.cast");
    std::fs::create_dir_all(&undeletable).expect("plant an undeletable stale artifact");

    let outcome =
        std::panic::catch_unwind(|| common::_skip_if_err(Err("no credential".to_string())));

    // Cleaned up before the assertions, so a failure does not leave a directory
    // named `full-stream.cast` behind to break every later run of this test.
    std::fs::remove_dir_all(&dir).expect("remove this test's fixture recordings dir");

    let payload = outcome.expect_err(
        "the discard must FAIL the run when it cannot remove a stale artifact — a \
         warning leaves the run green with a recording it did not produce, which \
         is exactly what the demo-reel adapter then publishes",
    );
    let message = payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
        .unwrap_or_default();
    assert!(
        message.contains("could not discard the previous recording")
            && message.contains("full-stream.cast"),
        "the panic must name the artifact it could not remove: {message}"
    );
}

/// Scenario: Read the harness source and assert that `TuiDeck::try_launch_inner`
/// discards the previous recording, that it does so before it spawns anything,
/// and that the artifact list the discard walks still names every file
/// `dump_recordings` writes.
///
/// A source guard rather than an observation, because observing it needs a real
/// PTY launch and this file is in the fast tier — the launch path itself is
/// exercised by the 57 of the 71 `tests/e2e_*.rs` files that build a `TuiDeck`
/// (the other 14 drive a headless or in-process daemon and never launch one).
#[test]
fn tui_deck_launch_discards_the_previous_recording_before_it_spawns() {
    let source = harness_source();
    let body = source
        .split_once("fn try_launch_inner(")
        .expect("try_launch_inner must exist")
        .1;

    let discard = body.find("discard_previous_recording(&test_name)").expect(
        "try_launch_inner no longer discards the previous recording — an \
             interrupted run can leave a stale cast for the demo reel to \
             publish (PR #805 audit blocker 3)",
    );
    let spawn = body
        .find("slave.spawn_command")
        .or_else(|| body.find(".spawn_command("))
        .expect("try_launch_inner must spawn the deck somewhere");
    assert!(
        discard < spawn,
        "the discard must run BEFORE the deck is spawned: from the spawn onwards \
         the run can be killed at any instant, and whatever it has not \
         overwritten is what a later reel build picks up"
    );

    // The dump and the discard must agree on the file names, or the discard
    // silently stops covering whatever the dump added.
    let dump = source
        .split_once("fn dump_recordings(")
        .expect("dump_recordings must exist")
        .1;
    let dump = dump.split_once("\n    fn ").map_or(dump, |(head, _)| head);
    for name in RECORDING_ARTIFACTS {
        assert!(
            source.contains(&format!("\"{name}\",")),
            "{name} is missing from RECORDING_ARTIFACTS in tests/common/mod.rs"
        );
    }
    let mut written: Vec<String> = Vec::new();
    let mut rest = dump;
    while let Some((_, after)) = rest.split_once("dir.join(\"") {
        let (name, tail) = after
            .split_once('"')
            .expect("unterminated dir.join literal");
        written.push(name.to_string());
        rest = tail;
    }
    assert!(
        !written.is_empty(),
        "found no `dir.join(\"…\")` artifact writes in dump_recordings — this \
         guard has stopped guarding anything"
    );
    for name in &written {
        assert!(
            RECORDING_ARTIFACTS.contains(&name.as_str()),
            "dump_recordings writes `{name}`, which the launch-time discard does \
             not remove — add it to RECORDING_ARTIFACTS in tests/common/mod.rs, \
             or an interrupted run leaves it behind (PR #805 audit blocker 3)"
        );
    }
}
