//! PRD #1666 part 3 (Q35-15): the fence gate on its own, over plain values.

use dot_agent_deck::composer_probe::gate::{Admission, FENCE_CAP, FENCE_OPS_CAP, Gate, Waiter};
use spec::spec;
use std::cell::Cell;
use std::sync::Arc;
use std::time::Duration;

type G = Arc<Gate<&'static str>>;

/// Offer `op` of `len` bytes; `reply` says whether it is a reply that passes
/// ahead of any fence. How many times `hold` ran, beside the admission.
fn offer(
    gate: &G,
    op: &'static str,
    len: usize,
    waiter: Option<Waiter<&'static str>>,
) -> (Admission<&'static str>, u32) {
    let held = Cell::new(0);
    let admitted = gate.admit(op, len, waiter, |_, _| false, |_| held.set(held.get() + 1));
    (admitted, held.get())
}

fn full(admitted: Admission<&'static str>) -> Waiter<&'static str> {
    match admitted {
        Admission::Full(_, waiter) => waiter,
        other => panic!("expected full, got {other:?}"),
    }
}

/// Scenario: Offer ops to an open gate, then close it and offer ops up to its op cap, empty ops, and ops past its byte cap; then release it and retire it. An open gate hands every op back to be written; a closed one holds ops (running `hold` once each) until 1,024 ops or 1 MiB, settles empty ops without holding them, refuses more as full, releases what it held in order, and once retired accepts nothing and hands back what it held once.
#[spec("inbox/probe/011")]
#[test]
fn probe_011_the_gate_holds_within_its_bounds_and_releases_in_order() {
    assert_eq!(
        (FENCE_OPS_CAP, FENCE_CAP),
        (1024, 1_048_576),
        "R6 §5.1's bounds"
    );
    let gate: G = Arc::default();
    let (admitted, held) = offer(&gate, "open", 4, None);
    assert!(matches!(admitted, Admission::Open("open")));
    assert_eq!(held, 0);
    assert_eq!(gate.release(), Vec::<&str>::new(), "nothing to release");
    assert!(gate.fence(7));
    assert!(gate.fence(9), "closing a closed fence is no change");
    let (admitted, held) = offer(&gate, "empty", 0, None);
    assert!(matches!(admitted, Admission::Settled("empty")));
    assert_eq!(held, 0, "an empty op isn't held");
    for _ in 0..FENCE_OPS_CAP {
        let (admitted, held) = offer(&gate, "op", 1, None);
        assert!(matches!(admitted, Admission::Held));
        assert_eq!(held, 1);
    }
    let status = gate.status();
    assert!(status.fenced);
    assert_eq!(
        (status.held_ops, status.held_bytes, status.full_waiters),
        (FENCE_OPS_CAP, FENCE_OPS_CAP, 0)
    );
    let (admitted, held) = offer(&gate, "over", 1, None);
    assert_eq!(held, 0);
    let waiter = full(admitted);
    assert_eq!(gate.status().full_waiters, 1);
    drop(waiter);
    assert_eq!(gate.status().full_waiters, 0);
    let released = gate.release();
    assert_eq!(released.len(), FENCE_OPS_CAP);
    assert!(!gate.status().fenced);
    assert_eq!(gate.status().held_ops, 0);

    for (first, second, fits) in [
        (FENCE_CAP, 1, false),
        (FENCE_CAP - 1, 1, true),
        (FENCE_CAP + 1, 1, false),
        (FENCE_CAP - 1, 2, false),
    ] {
        let gate: G = Arc::default();
        gate.fence(0);
        assert!(matches!(offer(&gate, "a", first, None).0, Admission::Held));
        let (admitted, _) = offer(&gate, "b", second, None);
        assert_eq!(
            matches!(admitted, Admission::Held),
            fits,
            "{first} then {second}"
        );
        assert_eq!(
            gate.status().held_bytes,
            first + if fits { second } else { 0 }
        );
    }

    let gate: G = Arc::default();
    gate.fence(0);
    for op in ["one", "two", "three"] {
        assert!(matches!(offer(&gate, op, 3, None).0, Admission::Held));
    }
    assert_eq!(gate.release(), ["one", "two", "three"]);
    assert!(gate.fence(0));
    assert!(matches!(offer(&gate, "four", 4, None).0, Admission::Held));
    assert!(!gate.retired());
    assert_eq!(gate.retire(), ["four"]);
    assert!(gate.retired());
    assert_eq!(gate.retire(), Vec::<&str>::new(), "handed back once");
    assert!(!gate.fence(0));
    assert!(matches!(
        offer(&gate, "late", 1, None).0,
        Admission::Retired("late")
    ));
    assert_eq!(gate.release(), Vec::<&str>::new());
    assert!(!gate.status().fenced);
}

/// Scenario: Close a fence at offset 10 and offer a reply the classifier passes, then one it doesn't, with the gate full and with a waiter queued, and offer the same reply to an open gate. The classifier is asked only while fenced, with the fence's own trigger; a passing reply is handed back to be written at once without `hold` or a held op, even when the gate is full or waiters are queued; one that doesn't pass is held or queued like any op.
#[spec("inbox/probe/011")]
#[test]
fn probe_011_a_passing_reply_skips_the_held_queue() {
    let gate: G = Arc::default();
    let asked = Cell::new(None);
    let admitted = gate.admit(
        "open-reply",
        9,
        None,
        |_, t| {
            asked.set(Some(t));
            true
        },
        |_| panic!("not held"),
    );
    assert!(matches!(admitted, Admission::Open(_)));
    assert_eq!(asked.get(), None, "an open gate classifies nothing");
    gate.fence(10);
    assert!(matches!(
        offer(&gate, "fill", FENCE_CAP, None).0,
        Admission::Held
    ));
    let _waiter = full(offer(&gate, "queued", 1, None).0);
    let admitted = gate.admit(
        "reply",
        9,
        None,
        |op, t| {
            asked.set(Some(t));
            *op == "reply"
        },
        |_| panic!("not held"),
    );
    assert!(matches!(admitted, Admission::PassAhead("reply")));
    assert_eq!(asked.get(), Some(10), "asked with the fence's trigger");
    let status = gate.status();
    assert_eq!((status.held_ops, status.held_bytes), (1, FENCE_CAP));
    let admitted = gate.admit("not-reply", 9, None, |_, _| false, |_| {});
    let _second = full(admitted);
    assert_eq!(gate.status().full_waiters, 2);
}

/// Scenario: Fill a fence and queue three full waiters, then offer a fresh op and the later waiters' ops before the head's, release, and drop a waiter. Neither a fresh op nor a later waiter takes freed space or an open gate ahead of the head; the head is admitted, which leaves the queue, then the next; a dropped waiter leaves the queue; and each waiter's wait ends when the gate changes, not before.
#[spec("inbox/probe/011")]
#[tokio::test]
async fn probe_011_full_waiters_keep_their_order() {
    let gate: G = Arc::default();
    gate.fence(0);
    assert!(matches!(
        offer(&gate, "fill", FENCE_CAP, None).0,
        Admission::Held
    ));
    let mut first = full(offer(&gate, "first", 1, None).0);
    let second = full(offer(&gate, "second", 1, None).0);
    let third = full(offer(&gate, "third", 1, None).0);
    assert_eq!(gate.status().full_waiters, 3);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), first.changed())
            .await
            .is_err(),
        "nothing changed yet"
    );
    let second = full(offer(&gate, "second", 1, Some(second)).0);
    let fresh = full(offer(&gate, "fresh", 1, None).0);
    assert_eq!(gate.status().full_waiters, 4);
    let waiting = tokio::spawn(async move {
        first.changed().await;
        first
    });
    tokio::task::yield_now().await;
    assert_eq!(gate.release(), ["fill"]);
    let first = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("the release wakes the waiter")
        .unwrap();
    let second = full(offer(&gate, "second", 1, Some(second)).0);
    assert!(matches!(
        offer(&gate, "first", 1, Some(first)).0,
        Admission::Open("first")
    ));
    assert_eq!(gate.status().full_waiters, 3, "the head left the queue");
    drop(third);
    assert_eq!(gate.status().full_waiters, 2, "a dropped waiter leaves");
    let fresh = full(offer(&gate, "fresh", 1, Some(fresh)).0);
    assert!(matches!(
        offer(&gate, "second", 1, Some(second)).0,
        Admission::Open("second")
    ));
    assert!(matches!(
        offer(&gate, "fresh", 1, Some(fresh)).0,
        Admission::Open("fresh")
    ));
    assert_eq!(gate.status().full_waiters, 0);
    assert!(matches!(
        offer(&gate, "next", 1, None).0,
        Admission::Open(_)
    ));
}

/// Scenario: Queue a full waiter, then retire the gate; and queue another behind a head that is dropped. Retirement wakes the waiter, whose op is then refused as retired; the head's drop wakes the waiter behind it.
#[spec("inbox/probe/011")]
#[tokio::test]
async fn probe_011_a_waiter_wakes_at_retirement_and_when_the_head_leaves() {
    let gate: G = Arc::default();
    gate.fence(0);
    offer(&gate, "fill", FENCE_CAP, None);
    let mut waiter = full(offer(&gate, "waiting", 1, None).0);
    let woken = tokio::spawn(async move {
        waiter.changed().await;
        waiter
    });
    tokio::task::yield_now().await;
    gate.retire();
    let waiter = tokio::time::timeout(Duration::from_secs(5), woken)
        .await
        .expect("retirement wakes it")
        .unwrap();
    assert!(matches!(
        offer(&gate, "waiting", 1, Some(waiter)).0,
        Admission::Retired(_)
    ));

    let gate: G = Arc::default();
    gate.fence(0);
    offer(&gate, "fill", FENCE_CAP, None);
    let head = full(offer(&gate, "head", 1, None).0);
    let mut behind = full(offer(&gate, "behind", 1, None).0);
    let woken = tokio::spawn(async move {
        behind.changed().await;
        behind
    });
    tokio::task::yield_now().await;
    drop(head);
    let behind = tokio::time::timeout(Duration::from_secs(5), woken)
        .await
        .expect("the head's leaving wakes it")
        .unwrap();
    let again = offer(&gate, "behind", 1, Some(behind)).0;
    assert!(
        matches!(again, Admission::Full(..)),
        "still full, but now the head"
    );
    assert_eq!(gate.status().full_waiters, 1, "the same place, kept");
    drop(again);
    assert_eq!(gate.status().full_waiters, 0);
}
