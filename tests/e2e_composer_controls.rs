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

fn observer(f: &Fixture) -> serde_json::Value {
    f.control("observer-status")["composer_observer"].clone()
}

fn schedule(f: &Fixture, operation: &str) -> serde_json::Value {
    f.control(operation)["composer_schedule"].clone()
}

/// Scenario: Start a native Claude stand-in under the daemon and, before any observation, set its generation's EC policy to `authority` and its record cap to 1, and read both back; refuse a cap of zero and any control addressed to another generation. Hold the observer's passes: the hold answers with no pass running, and the finished count stays put until the go, then moves. Fail every PTY write and request a probe: its write fails, the schedule waits its first backoff and is not due a millisecond short of it, and is due once advanced by the rest, its failure kept. Hold the PTY writer, type, and see the keys parked unwritten until the go; then set a fault plan letting the PTY take one byte, type two, and see one written and the write failed.
#[spec("inbox/probe/079")]
#[test]
fn probe_079_the_composer_controls_reach_a_live_generation() {
    use dot_agent_deck::composer_probe::probe::BACKOFF;
    let f = Fixture::start("claude");

    // Generation-bound records override, before the first observation, read
    // back from the generation's records.
    let fresh = observer(&f);
    assert_eq!(fresh["records_observed"], false, "{fresh}");
    assert_eq!(fresh["records_policy"], "hold", "production's: {fresh}");
    f.control("records-policy:authority");
    f.control("records-cap:1");
    let set = observer(&f);
    assert_eq!(set["records_policy"], "authority", "{set}");
    assert_eq!(set["records_cap"], 1, "{set}");
    assert_eq!(set["records_retained"], 0, "{set}");
    let zero = f.control_for(f.generation, "records-cap:0");
    assert_eq!(zero["ok"], false, "a zero cap is refused: {zero}");
    let other = f.control_for(f.generation + 1, "writer-hold");
    assert_eq!(other["ok"], false, "another generation is refused: {other}");

    // The observer's pass barrier: the hold answers once no pass is running,
    // and none starts until the go.
    let held = f.control("observer-hold")["composer_observer"].clone();
    assert_eq!(held["passes_held"], true, "{held}");
    assert_eq!(held["pass_running"], false, "acknowledged quiet: {held}");
    let finished = held["passes"].as_u64().unwrap();
    let still = observer(&f);
    assert_eq!(
        (still["passes"].as_u64().unwrap(), &still["pass_running"]),
        (finished, &serde_json::Value::Bool(false)),
        "no pass starts while held: {still}"
    );
    f.control("observer-go");
    assert!(
        common::wait_until(WAIT, || observer(&f)["passes"].as_u64().unwrap() > finished),
        "passes resume after the go"
    );

    // The writer hold: typed keys are dequeued and parked, unwritten, until
    // the go.
    let before = f.input();
    f.control("writer-hold");
    let mut human = f.connect();
    human.write("HELD-KEYS");
    assert!(
        common::wait_until(WAIT, || writer(&f)["parked"] == true),
        "the writer parks with the job: {}",
        writer(&f)
    );
    assert_eq!(f.input(), before, "nothing written while parked");
    f.control("writer-go");
    let typed = [before.as_slice(), b"HELD-KEYS"].concat();
    f.expect(&typed);
    assert_eq!(writer(&f)["held"], false);

    // The fault plan: the PTY takes one byte, then fails.
    let failed = writer(&f)["failed_writes"].as_u64().unwrap();
    f.control("writer-fault:jobs=any,skip=0,accept=1");
    human.write("XY");
    let partial = [typed.as_slice(), b"X"].concat();
    f.expect(&partial);
    assert!(
        common::wait_until(WAIT, || writer(&f)["failed_writes"].as_u64().unwrap()
            > failed),
        "the second byte's write failed: {}",
        writer(&f)
    );
    f.control("writer-fault:off");
    let bad = f.control_for(f.generation, "writer-fault:jobs=probe");
    assert_eq!(bad["ok"], false, "an unknown plan is refused: {bad}");

    // The schedule, last, so a retry its tick may start once due writes
    // after every byte this test checks: a failed attempt's backoff,
    // advanced.
    f.hook_event("session_start");
    f.hook_event("idle");
    let first = schedule(&f, "schedule-status");
    assert_eq!(
        (&first["due"], &first["failures"]),
        (&true.into(), &0.into()),
        "{first}"
    );
    f.control("writer-fault:jobs=any,skip=0,accept=0");
    let probe = f.probe();
    assert_eq!(probe["composer_probe"]["outcome"], "refused", "{probe}");
    assert!(
        common::wait_until(WAIT, || schedule(&f, "schedule-status")["failures"] == 1),
        "the failed write counts: {}",
        schedule(&f, "schedule-status")
    );
    f.control("writer-fault:off");
    let waiting = schedule(&f, "schedule-status");
    assert_eq!(waiting["due"], false, "the first backoff: {waiting}");
    let backoff = u64::try_from(BACKOFF[0].as_millis()).unwrap();
    // Half the backoff leaves it waiting, since far less than half of it
    // passed between the failure and here; the other half makes it due.
    let short = schedule(&f, &format!("schedule-advance:{}", backoff / 2));
    assert_eq!(short["due"], false, "half the backoff: {short}");
    let due = schedule(&f, &format!("schedule-advance:{}", backoff / 2));
    assert_eq!(due["due"], true, "the whole backoff: {due}");
    assert_eq!(
        (&due["failures"], &due["broken"]),
        (&1.into(), &false.into()),
        "only the wait moved: {due}"
    );
}
