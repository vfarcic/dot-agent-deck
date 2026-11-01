#![cfg(all(feature = "e2e", target_os = "linux"))]

//! Q105b.2: the `e2e` build's composer controls reach a live generation
//! through `e2e-composer-control`, act on the production path they name and
//! refuse what they must. A smoke test of the controls themselves; the rows
//! that use them are S-20b's and S-20c's.

mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/live_segments.rs"]
mod live;

use live::{Fixture, WAIT};
use spec::spec;
use std::time::Duration;

ctor::declarative::ctor! {
    // The fixture child is selected by its private environment before libtest
    // parses Codex's injected arguments. The normal test process returns.
    #[ctor(unsafe)]
    fn native_fixture_entry() {
        live::native_entry();
    }
}

fn writer(f: &Fixture) -> serde_json::Value {
    f.control("writer-status")["composer_writer"].clone()
}

/// Scenario: Start a native Claude stand-in under the daemon and, before any probe, set its generation's EC policy and record cap, then refuse a cap of zero and any control addressed to another generation. Hold and release the observer's passes and see the pass count stop and move; read and advance the probe schedule; hold the PTY writer, type, see the keys queued but unwritten until the go; then set a fault plan letting the PTY take one byte, type two, and see one written and the write failed.
#[spec("inbox/probe/079")]
#[test]
fn probe_079_the_composer_controls_reach_a_live_generation() {
    let f = Fixture::start("claude");

    // Generation-bound records override, before the first observation.
    f.control("records-policy:authority");
    f.control("records-cap:1");
    let zero = f.control_for(f.generation, "records-cap:0");
    assert_eq!(zero["ok"], false, "a zero cap is refused: {zero}");
    let other = f.control_for(f.generation + 1, "writer-hold");
    assert_eq!(other["ok"], false, "another generation is refused: {other}");

    // The observer's pass barrier.
    f.control("observer-hold");
    let passes = || {
        f.control("observer-status")["composer_observer"]["passes"]
            .as_u64()
            .unwrap()
    };
    let status = f.control("observer-status")["composer_observer"].clone();
    assert_eq!(status["passes_held"], true, "{status}");
    // A pass already running when the hold came may still finish.
    let first = passes();
    common::wait_until(Duration::from_millis(200), || passes() != first);
    let held = passes();
    assert!(
        !common::wait_until(Duration::from_millis(300), || passes() != held),
        "no pass while held"
    );
    f.control("observer-go");
    assert!(
        common::wait_until(WAIT, || passes() > held),
        "passes resume after the go"
    );

    // The schedule: due before any attempt, and still due once advanced.
    let schedule = f.control("schedule-status")["composer_schedule"].clone();
    assert_eq!(schedule["due"], true, "{schedule}");
    assert_eq!(schedule["failures"], 0, "{schedule}");
    let advanced = f.control("schedule-advance:750000")["composer_schedule"].clone();
    assert_eq!(advanced["due"], true, "{advanced}");

    // The writer hold: typed keys are queued, unwritten, until the go.
    let before = f.input();
    f.control("writer-hold");
    let mut human = f.connect();
    human.write("HELD-KEYS");
    assert!(
        common::wait_until(WAIT, || writer(&f)["parked"] == true),
        "the writer parks with the job: {}",
        writer(&f)
    );
    assert!(
        !common::wait_until(Duration::from_millis(250), || f.input() != before),
        "nothing written while held"
    );
    f.control("writer-go");
    let typed = [before.as_slice(), b"HELD-KEYS"].concat();
    f.expect(&typed);
    assert_eq!(writer(&f)["held"], false);

    // The fault plan: the PTY takes one byte, then fails.
    f.control("writer-fault:jobs=any,skip=0,accept=1");
    human.write("XY");
    let partial = [typed.as_slice(), b"X"].concat();
    f.expect(&partial);
    assert!(
        common::wait_until(WAIT, || writer(&f)["failed_writes"].as_u64().unwrap() >= 1),
        "the second byte's write failed: {}",
        writer(&f)
    );
    f.control("writer-fault:off");
    let bad = f.control_for(f.generation, "writer-fault:jobs=probe");
    assert_eq!(bad["ok"], false, "an unknown plan is refused: {bad}");
}
