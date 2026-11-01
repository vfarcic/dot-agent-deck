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

/// Scenario: Start a native Claude stand-in under the daemon, set its generation's EC policy to `authority` and its record cap to 1 before any observation and read both back, and see a cap of zero and a control addressed to another generation refused. Hold the observer's passes and then the PTY writer: no pass finishes and typed keys stay parked unwritten until each go, and a fault plan letting the PTY take one byte writes one of two typed keys and fails the write. Fail every PTY write and request a probe: its write fails, an advance short of the backoff left keeps the schedule waiting, and a whole backoff more makes it due with its failure kept.
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
    // The failure is at or after this, so the wait it leaves is at least
    // the backoff less what has passed since.
    let before_failure = std::time::Instant::now();
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
    // An advance short of the least the wait can have left keeps it
    // waiting, with a margin for the control's own round trip; a whole
    // backoff more makes it due, however long this test has taken.
    let margin = std::time::Duration::from_secs(5);
    let short_by = BACKOFF[0]
        .checked_sub(before_failure.elapsed() + margin)
        .expect("the test reached the short advance within the backoff less its margin");
    let short = schedule(&f, &format!("schedule-advance:{}", short_by.as_millis()));
    assert_eq!(short["due"], false, "short of the backoff left: {short}");
    let due = schedule(&f, &format!("schedule-advance:{}", BACKOFF[0].as_millis()));
    assert_eq!(due["due"], true, "a whole backoff more: {due}");
    assert_eq!(
        (&due["failures"], &due["broken"]),
        (&1.into(), &false.into()),
        "only the wait moved: {due}"
    );
}
