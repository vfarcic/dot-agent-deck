//! PRD #1666 part 3 (Q35-15): the fence gate on the real pane writer and
//! writer thread, over a scripted PTY writer (`inbox/probe/011`).

use super::*;
use crate::composer_probe::epoch::GenerationInput;
use crate::composer_probe::gate::{FENCE_CAP, FENCE_OPS_CAP};
use crate::composer_probe::query_ledger::{LEDGER_SLOTS, QueryKind};
use crate::ingress_ledger::{IngressLedger, Settlement};
use spec::spec;

const DA1_REPLY: &[u8] = b"\x1b[?62;22c";

/// A PTY writer that keeps what it takes. It takes nothing while shut,
/// after saying a write arrived; with a budget, it takes that many bytes in
/// all and then fails every write with `kind`.
struct Scripted {
    taken: Arc<Mutex<Vec<u8>>>,
    open: Arc<(Mutex<bool>, std::sync::Condvar)>,
    entered: std::sync::mpsc::Sender<()>,
    budget: Option<(usize, std::io::ErrorKind)>,
}

impl std::io::Write for Scripted {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = self.entered.send(());
        let (open, wake) = &*self.open;
        let mut guard = open.lock().unwrap();
        while !*guard {
            guard = wake.wait(guard).unwrap();
        }
        drop(guard);
        let n = match &mut self.budget {
            None => buf.len(),
            Some((0, kind)) => return Err(std::io::Error::from(*kind)),
            Some((left, _)) => {
                let n = (*left).min(buf.len());
                *left -= n;
                n
            }
        };
        self.taken.lock().unwrap().extend_from_slice(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A pane writer over a [`Scripted`] PTY writer, with its ledger.
struct Rig {
    writer: PaneWriter,
    input: Arc<GenerationInput>,
    ledger: Arc<IngressLedger>,
    retired: Arc<AtomicBool>,
    /// The generation's exit token, which its reader would set at the PTY's
    /// end.
    exited: Arc<AtomicBool>,
    taken: Arc<Mutex<Vec<u8>>>,
    open: Arc<(Mutex<bool>, std::sync::Condvar)>,
    entered: Mutex<std::sync::mpsc::Receiver<()>>,
    notices: tokio::sync::mpsc::Receiver<Vec<u8>>,
    notice: tokio::sync::mpsc::Sender<Vec<u8>>,
    /// The generation's real output hook, which the agent's output goes
    /// through.
    hook: Mutex<crate::composer_probe::paste_mode::OutputHook>,
}

impl Rig {
    fn new(open: bool, budget: Option<(usize, std::io::ErrorKind)>) -> Self {
        let input = Arc::new(GenerationInput::default());
        let taken = Arc::new(Mutex::new(Vec::new()));
        let open = Arc::new((Mutex::new(open), std::sync::Condvar::new()));
        let (tx, entered) = std::sync::mpsc::channel();
        let retired = Arc::new(AtomicBool::new(false));
        let exited = Arc::new(AtomicBool::new(false));
        let writer = PaneWriter::new(
            Box::new(Scripted {
                taken: Arc::clone(&taken),
                open: Arc::clone(&open),
                entered: tx,
                budget,
            }),
            None,
            Arc::new(Mutex::new(PaneInputState::default())),
            Arc::clone(&retired),
            Arc::clone(&exited),
            None,
            Arc::clone(&input),
        );
        let (notice, notices) = tokio::sync::mpsc::channel(16);
        let hook = Mutex::new(crate::composer_probe::paste_mode::OutputHook::new(
            Arc::clone(&input),
        ));
        Self {
            writer,
            input,
            ledger: IngressLedger::new(),
            retired,
            exited,
            taken,
            open,
            entered: Mutex::new(entered),
            notices,
            notice,
            hook,
        }
    }

    fn frame(&self, bytes: &[u8]) -> (UserFrame, u64) {
        let ticket = self.ledger.admit();
        let id = ticket.ticket();
        (
            UserFrame::new(bytes.to_vec(), Some(ticket), Some(self.notice.clone())),
            id,
        )
    }

    async fn offer(&self, bytes: &[u8]) -> (Accepted, u64) {
        let (frame, id) = self.frame(bytes);
        (self.writer.accept(frame, None).await, id)
    }

    fn open(&self) {
        let (open, wake) = &*self.open;
        *open.lock().unwrap() = true;
        wake.notify_all();
    }

    fn taken(&self) -> Vec<u8> {
        self.taken.lock().unwrap().clone()
    }

    fn in_flight(&self) -> usize {
        self.writer.pty.in_flight.count.load(Ordering::SeqCst)
    }

    fn fence(&self) {
        assert!(self.writer.gate().fence(), "the gate isn't retired");
    }

    /// The agent's output asks a query of `kind` now, through the real
    /// output hook.
    fn query(&self, kind: QueryKind) {
        let bytes: &[u8] = match kind {
            QueryKind::Da1 => b"\x1b[c",
            QueryKind::Cpr => b"\x1b[6n",
            QueryKind::OscColour { .. } => b"\x1b]11;?\x07",
        };
        self.hook.lock().unwrap().on_output(bytes);
    }

    /// Every settlement the ledger logged for `ticket`, in order.
    fn settled(&self, ticket: u64) -> Vec<Settlement> {
        self.ledger
            .settled()
            .into_iter()
            .filter(|(t, _)| *t == ticket)
            .map(|(_, how)| how)
            .collect()
    }

    async fn settles(&self, ticket: u64) -> Vec<Settlement> {
        let deadline = Instant::now() + Duration::from_secs(8);
        while self.settled(ticket).is_empty() {
            assert!(Instant::now() < deadline, "ticket {ticket} never settled");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        self.settled(ticket)
    }

    async fn taken_becomes(&self, expected: &[u8]) {
        let deadline = Instant::now() + Duration::from_secs(8);
        while self.taken() != expected {
            assert!(
                Instant::now() < deadline,
                "the PTY took {:?}, not {:?}",
                String::from_utf8_lossy(&self.taken()),
                String::from_utf8_lossy(expected)
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
}

fn written(accepted: &Accepted) -> bool {
    matches!(accepted, Accepted::Written(Ok(())))
}

/// Scenario: With no fence, hand the pane writer's gate a frame, an empty frame and another frame. Each is written and awaited as before, its ticket settles with what the PTY took, the empty one included, every one moves the epoch once, and the in-flight count is back to zero.
#[spec("inbox/probe/011")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_011_with_no_fence_a_frame_is_written_and_awaited_as_before() {
    let rig = Rig::new(true, None);
    let mut tickets = Vec::new();
    for bytes in [b"a".as_slice(), b"", b"b"] {
        let (accepted, ticket) = rig.offer(bytes).await;
        assert!(written(&accepted), "{bytes:?}");
        tickets.push(ticket);
    }
    assert_eq!(rig.taken(), b"ab");
    assert_eq!(rig.settled(tickets[0]), [Settlement::Written { bytes: 1 }]);
    assert_eq!(rig.settled(tickets[1]), [Settlement::Written { bytes: 0 }]);
    assert_eq!(rig.settled(tickets[2]), [Settlement::Written { bytes: 1 }]);
    assert_eq!(rig.input.epoch().epoch(), 3);
    // A job leaves the count just after it answers.
    rig.writer.until_idle().await;
    assert_eq!(rig.in_flight(), 0);
    assert_eq!(rig.writer.gate().status().held_ops, 0);
}

/// Scenario: With no fence, write a frame through a PTY that takes three bytes and then fails, and another through one that fails at once (the `inbox/probe/015` forced rows). The first answers the error after its prefix reached the PTY and settles partial; the second answers the error and settles failed; each reports once.
#[spec("inbox/probe/011")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_011_with_no_fence_a_short_or_failed_write_answers_as_before() {
    let partial = Rig::new(true, Some((3, std::io::ErrorKind::BrokenPipe)));
    let (accepted, ticket) = partial.offer(b"abcdef").await;
    assert!(matches!(accepted, Accepted::Written(Err(_))));
    assert_eq!(partial.taken(), b"abc");
    assert_eq!(
        partial.settled(ticket),
        [Settlement::Partial { accepted: 3 }]
    );
    let failed = Rig::new(true, Some((0, std::io::ErrorKind::Other)));
    let (accepted, ticket) = failed.offer(b"xyz").await;
    assert!(matches!(accepted, Accepted::Written(Err(_))));
    assert_eq!(failed.taken(), b"");
    assert_eq!(
        failed.settled(ticket),
        [Settlement::Failed(std::io::ErrorKind::Other)]
    );
    failed.writer.until_idle().await;
    assert_eq!(failed.in_flight(), 0);
}

/// Scenario: Close the fence and hand the gate three frames, whose callers then go away. Nothing reaches the PTY; each frame moved the epoch at acceptance, is in flight and keeps its ticket outstanding with no job queued. Releasing writes all three once, in acceptance order, settles each written, moves the epoch no further and empties the in-flight count.
#[spec("inbox/probe/011")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_011_held_frames_are_released_in_acceptance_order() {
    let rig = Rig::new(true, None);
    rig.fence();
    let mut tickets = Vec::new();
    for bytes in [b"a|".as_slice(), b"b|", b"c|"] {
        let (accepted, ticket) = rig.offer(bytes).await;
        assert!(matches!(accepted, Accepted::Held));
        tickets.push(ticket);
    }
    assert_eq!(rig.taken(), b"");
    assert_eq!(rig.input.epoch().epoch(), 3, "moved at acceptance");
    assert_eq!(rig.in_flight(), 3);
    let marks = rig.ledger.marks();
    assert_eq!((marks.outstanding, marks.job_mark), (3, 0));
    let status = rig.writer.gate().status();
    assert_eq!((status.held_ops, status.held_bytes), (3, 6));
    assert_eq!(rig.writer.release_fence(Ok(())), 3);
    rig.taken_becomes(b"a|b|c|").await;
    for ticket in tickets {
        assert_eq!(
            rig.settles(ticket).await,
            [Settlement::Written { bytes: 2 }]
        );
    }
    rig.writer.until_idle().await;
    assert_eq!(rig.in_flight(), 0);
    assert_eq!(rig.input.epoch().epoch(), 3, "not moved again at release");
    assert!(!rig.writer.gate().status().fenced);
    let (accepted, _) = rig.offer(b"d").await;
    assert!(written(&accepted), "the gate is open again");
}

/// Scenario: Close a fence and feed it two thousand empty frames, then one-byte frames until it is full; separately, fill an empty fence with exactly 1 MiB and with one frame larger than that. Empty frames settle at once, move the epoch as an unfenced one does and add no held op; the 1,025th one-byte frame is full; exactly 1 MiB is held and a byte more is full; a frame larger than the cap is held alone and anything after it is full.
#[spec("inbox/probe/011")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_011_the_fence_bounds_held_ops_and_bytes() {
    let rig = Rig::new(true, None);
    rig.fence();
    for _ in 0..2000 {
        let (accepted, ticket) = rig.offer(b"").await;
        assert!(matches!(accepted, Accepted::Held));
        assert_eq!(rig.settled(ticket), [Settlement::Written { bytes: 0 }]);
    }
    assert_eq!(
        rig.writer.gate().status().held_ops,
        0,
        "no node per empty frame"
    );
    assert_eq!(rig.in_flight(), 0);
    assert_eq!(
        rig.input.epoch().epoch(),
        2000,
        "each moved the epoch, as unfenced"
    );
    for _ in 0..FENCE_OPS_CAP {
        assert!(matches!(rig.offer(b"x").await.0, Accepted::Held));
    }
    assert!(matches!(rig.offer(b"x").await.0, Accepted::Full(..)));
    assert_eq!(rig.writer.gate().status().held_ops, FENCE_OPS_CAP);

    for (first, second_full) in [(FENCE_CAP, true), (FENCE_CAP + 1, true)] {
        let rig = Rig::new(true, None);
        rig.fence();
        assert!(matches!(
            rig.offer(&vec![b'p'; first]).await.0,
            Accepted::Held
        ));
        assert_eq!(
            matches!(rig.offer(b"y").await.0, Accepted::Full(..)),
            second_full,
            "after {first} bytes"
        );
    }
    let rig = Rig::new(true, None);
    rig.fence();
    assert!(matches!(
        rig.offer(&vec![b'p'; FENCE_CAP - 1]).await.0,
        Accepted::Held
    ));
    assert!(matches!(rig.offer(b"y").await.0, Accepted::Held), "fits");
}

/// Scenario: Fill a fence, then have two connections' frames come back full in turn, and a third, fresh frame arrive after them; abort a fourth waiter. A fresh frame and a later waiter don't overtake the head waiter even once the fence opens; after release the head is written first, then the next, then the fresh one; the aborted waiter's frame is never written and its place passes on.
#[spec("inbox/probe/011")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_011_full_waiters_are_served_in_arrival_order() {
    let rig = Rig::new(true, None);
    rig.fence();
    assert!(matches!(
        rig.offer(&vec![b'h'; FENCE_CAP]).await.0,
        Accepted::Held
    ));
    let take = |accepted: Accepted| match accepted {
        Accepted::Full(frame, waiter) => (frame, waiter),
        _ => panic!("expected full"),
    };
    let (x, wx) = take(rig.offer(b"X").await.0);
    let (y, wy) = take(rig.offer(b"Y").await.0);
    let (gone, wgone) = take(rig.offer(b"GONE").await.0);
    let (gone_ticket, gone) = (gone.ticket.as_ref().unwrap().ticket(), gone);
    assert_eq!(rig.writer.gate().status().full_waiters, 3);
    // A waiter whose task is cancelled: its frame and place are dropped.
    drop((gone, wgone));
    assert_eq!(rig.settled(gone_ticket), [Settlement::Cancelled]);
    assert_eq!(rig.writer.gate().status().full_waiters, 2);
    rig.writer.release_fence(Ok(()));
    // Open now, but the queue is still served in order.
    let (z, wz) = take(rig.offer(b"Z").await.0);
    let (y, wy) = take(rig.writer.accept(y, Some(wy)).await);
    assert!(written(&rig.writer.accept(x, Some(wx)).await));
    let (z, wz) = take(rig.writer.accept(z, Some(wz)).await);
    assert!(written(&rig.writer.accept(y, Some(wy)).await));
    assert!(written(&rig.writer.accept(z, Some(wz)).await));
    let mut expected = vec![b'h'; FENCE_CAP];
    expected.extend_from_slice(b"XYZ");
    rig.taken_becomes(&expected).await;
    assert_eq!(rig.writer.gate().status().full_waiters, 0);
}

/// Scenario: Hold frames from a connection and leave another full, then retire the generation. Each held frame is refused `exited` once, its connection told, nothing is written and the in-flight count is back to zero; the full waiter wakes, its frame is refused as retired when offered again, and a later release or fence does nothing.
#[spec("inbox/probe/011")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_011_retirement_refuses_held_and_full_frames_once() {
    let mut rig = Rig::new(true, None);
    rig.fence();
    let (_, first) = rig.offer(&vec![b'r'; FENCE_CAP]).await;
    let (full, mut place) = match rig.offer(b"waiting").await.0 {
        Accepted::Full(frame, waiter) => (frame, waiter),
        _ => panic!("expected full"),
    };
    assert_eq!(rig.in_flight(), 1);
    let woken = tokio::spawn(async move {
        place.changed().await;
        place
    });
    rig.writer.gate().retire();
    let place = tokio::time::timeout(Duration::from_secs(8), woken)
        .await
        .expect("the waiter wakes at retirement")
        .unwrap();
    assert_eq!(rig.settled(first), [Settlement::Refused("exited")]);
    assert_eq!(rig.notices.try_recv().unwrap(), b"exited");
    assert_eq!(rig.in_flight(), 0);
    let full_ticket = full.ticket.as_ref().unwrap().ticket();
    match rig.writer.accept(full, Some(place)).await {
        Accepted::Retired(frame) => frame.refuse("exited"),
        _ => panic!("a retired gate accepts nothing"),
    }
    assert_eq!(rig.settled(full_ticket), [Settlement::Refused("exited")]);
    assert_eq!(rig.writer.release_fence(Ok(())), 0);
    assert!(!rig.writer.gate().fence());
    rig.writer.gate().retire();
    assert_eq!(rig.settled(first).len(), 1, "refused once");
    assert_eq!(rig.taken(), b"");
}

/// Scenario: Hold a frame, release it behind a write the PTY isn't taking, retire the generation, then let the PTY take again; separately release with the captured target no longer the pane's owner. The released frame is refused `exited` on the writer thread, never written, and its connection is told `exited` once; the failed target check refuses every held frame with its reason and tells its connection; each reports once and nothing stays in flight.
#[spec("inbox/probe/011")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_011_a_release_reached_after_retirement_writes_nothing() {
    let mut rig = Rig::new(false, None);
    let (tx, _blocker) = std::sync::mpsc::sync_channel(1);
    rig.writer
        .pty
        .submit(
            PtyOp::Write(b"blocker".to_vec(), ByteSource::User),
            PtyReply::Blocking(tx),
        )
        .unwrap();
    rig.entered.lock().unwrap().recv().unwrap();
    rig.fence();
    let (_, ticket) = rig.offer(b"late").await;
    assert_eq!(rig.writer.release_fence(Ok(())), 1);
    rig.retired.store(true, Ordering::SeqCst);
    rig.open();
    assert_eq!(rig.settles(ticket).await, [Settlement::Refused("exited")]);
    rig.writer.until_idle().await;
    assert_eq!(rig.taken(), b"blocker");
    // R-U2-03: the refusal on the writer thread reaches the connection.
    assert_eq!(rig.notices.try_recv().unwrap(), b"exited");
    assert!(rig.notices.try_recv().is_err(), "told once");
    assert_eq!(rig.in_flight(), 0);

    let mut rig = Rig::new(true, None);
    rig.fence();
    let (_, first) = rig.offer(b"one").await;
    let (_, second) = rig.offer(b"two").await;
    assert_eq!(rig.writer.release_fence(Err("stale")), 0);
    for ticket in [first, second] {
        assert_eq!(rig.settled(ticket), [Settlement::Refused("stale")]);
        assert_eq!(rig.notices.try_recv().unwrap(), b"stale");
    }
    assert_eq!(rig.in_flight(), 0);
    assert_eq!(rig.taken(), b"");
}

/// Scenario: Hold a frame and mark its target exited, its generation not retired, before releasing; separately hold a frame, release it behind a write the PTY isn't taking, and mark the target exited before the PTY takes again. Neither frame is written: the first release submits nothing and refuses it `exited`, the second is refused `exited` on the writer thread; each ticket reports once, its connection is told `exited` once, and nothing stays in flight (R-U2-02, R-U2-03).
#[spec("inbox/probe/011")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_011_a_release_to_an_exited_target_writes_nothing() {
    let mut rig = Rig::new(true, None);
    rig.fence();
    let (_, ticket) = rig.offer(b"before").await;
    rig.exited.store(true, Ordering::SeqCst);
    assert_eq!(rig.writer.release_fence(Ok(())), 0, "refused at release");
    assert_eq!(rig.settled(ticket), [Settlement::Refused("exited")]);
    assert_eq!(rig.notices.try_recv().unwrap(), b"exited");
    assert!(rig.notices.try_recv().is_err(), "told once");
    assert!(!rig.retired.load(Ordering::SeqCst));
    assert_eq!(rig.in_flight(), 0);
    assert_eq!(rig.taken(), b"");

    let mut rig = Rig::new(false, None);
    let (tx, _blocker) = std::sync::mpsc::sync_channel(1);
    rig.writer
        .pty
        .submit(
            PtyOp::Write(b"blocker".to_vec(), ByteSource::User),
            PtyReply::Blocking(tx),
        )
        .unwrap();
    rig.entered.lock().unwrap().recv().unwrap();
    rig.fence();
    let (_, ticket) = rig.offer(b"after").await;
    assert_eq!(rig.writer.release_fence(Ok(())), 1);
    rig.exited.store(true, Ordering::SeqCst);
    rig.open();
    assert_eq!(rig.settles(ticket).await, [Settlement::Refused("exited")]);
    rig.writer.until_idle().await;
    assert_eq!(rig.taken(), b"blocker");
    assert_eq!(rig.notices.try_recv().unwrap(), b"exited");
    assert!(rig.notices.try_recv().is_err(), "told once");
    assert!(!rig.retired.load(Ordering::SeqCst));
    assert_eq!(rig.in_flight(), 0);
}

/// Scenario: Hold a frame and release it to a PTY that fails the write after taking three of its bytes, and another to a PTY that fails at once. Each ticket settles once with what the PTY took, partial or failed, and each frame's connection is told `exited` once: nobody awaits a released write, so that typed rejection is how its failure reaches the person (R-U2-03).
#[spec("inbox/probe/011")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_011_a_released_write_that_fails_tells_its_connection() {
    for (budget, expected) in [
        (3, Settlement::Partial { accepted: 3 }),
        (0, Settlement::Failed(std::io::ErrorKind::BrokenPipe)),
    ] {
        let mut rig = Rig::new(true, Some((budget, std::io::ErrorKind::BrokenPipe)));
        rig.fence();
        let (_, ticket) = rig.offer(b"abcdef").await;
        assert_eq!(rig.writer.release_fence(Ok(())), 1);
        assert_eq!(rig.settles(ticket).await, [expected]);
        assert_eq!(rig.notices.try_recv().unwrap(), b"exited");
        assert!(rig.notices.try_recv().is_err(), "told once");
        rig.writer.until_idle().await;
        assert_eq!(rig.in_flight(), 0);
    }
}

/// Scenario: Hold a frame while the writer thread is gone, then release it. The released frame's ticket settles failed with a broken pipe, once, and nothing stays in flight.
#[spec("inbox/probe/011")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_011_a_release_to_a_gone_writer_settles_once() {
    let rig = Rig::new(true, None);
    rig.fence();
    let (_, ticket) = rig.offer(b"orphan").await;
    let (tx, back) = oneshot::channel();
    rig.writer
        .pty
        .submit(
            PtyOp::Surrender(tx),
            PtyReply::Blocking(std::sync::mpsc::sync_channel(1).0),
        )
        .unwrap();
    let _displaced = back.await.unwrap();
    rig.writer.release_fence(Ok(()));
    assert_eq!(
        rig.settled(ticket),
        [Settlement::Failed(std::io::ErrorKind::BrokenPipe)]
    );
    assert_eq!(rig.in_flight(), 0);
}

/// Scenario: Close a fence, hold a key, have the agent's output ask a DA1 query, then hand the gate one whole DA1 reply while the PTY is taking writes slowly, and submit a deck job straight to the writer. The reply is written at once past the closed fence, is in flight while it is written, moves no epoch and takes no held op or byte; the deck job, which never came through acceptance, is written too and counts in flight but not in the fence; the key waits for the release (I-5).
#[spec("inbox/probe/011")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_011_a_reply_passes_ahead_of_a_closed_fence() {
    let rig = Arc::new(Rig::new(false, None));
    rig.fence();
    let (accepted, key) = rig.offer(b"key|").await;
    assert!(matches!(accepted, Accepted::Held));
    let epoch = rig.input.epoch().epoch();
    rig.query(QueryKind::Da1);
    let reply = tokio::spawn({
        let rig = Arc::clone(&rig);
        async move { rig.offer(DA1_REPLY).await }
    });
    rig.entered.lock().unwrap().recv().unwrap();
    assert_eq!(
        rig.in_flight(),
        2,
        "the held key and the reply being written"
    );
    let status = rig.writer.gate().status();
    assert_eq!((status.held_ops, status.held_bytes), (1, 4));
    rig.open();
    let (accepted, _) = reply.await.unwrap();
    assert!(written(&accepted));
    assert_eq!(rig.taken(), DA1_REPLY);
    assert_eq!(
        rig.input.epoch().epoch(),
        epoch,
        "an exempt reply moves none"
    );
    let mut job = rig
        .writer
        .queue_deck_job(b"deck|".to_vec(), Arc::default())
        .expect("the writer thread runs");
    job.done().await.expect("written past the fence");
    assert_eq!(rig.writer.gate().status().held_ops, 1);
    assert!(rig.settled(key).is_empty(), "the key is still held");
    rig.writer.release_fence(Ok(()));
    rig.taken_becomes(&[DA1_REPLY, b"deck|", b"key|"].concat())
        .await;
}

/// Scenario: With a CPR query and a DA1 query asked before the fence closed, the DA1 one in the very push the fence closes after, and a DA1 query after, hold a key and hand the gate a DA1 reply, a CPR reply and another DA1 reply; then two replies in one frame, a reply to nothing and half a reply, each against a fresh query asked after the trigger. Both DA1 replies are exempt, but only the second, answering the later query, passes ahead: the first answers the earlier query and is held; the CPR reply is never exempt; two replies in one frame, a reply to no query and a split reply are held. After release the PTY has the passing reply, then everything else in acceptance order.
#[spec("inbox/probe/011")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_011_only_a_reply_to_a_query_after_the_trigger_passes_ahead() {
    let rig = Rig::new(true, None);
    rig.query(QueryKind::Cpr);
    // Noted at the very offset the fence then closes at: before the trigger.
    rig.query(QueryKind::Da1);
    rig.fence();
    rig.query(QueryKind::Da1);
    assert_eq!(rig.input.reply_class(QueryKind::Da1), (true, true));
    let mut held = Vec::new();
    for bytes in [b"key|".as_slice(), DA1_REPLY, b"\x1b[3;7R"] {
        let (accepted, _) = rig.offer(bytes).await;
        assert!(matches!(accepted, Accepted::Held), "{bytes:?} is held");
        held.extend_from_slice(bytes);
    }
    assert_eq!(
        rig.taken(),
        b"",
        "the first reply answers the earlier query"
    );
    let (accepted, _) = rig.offer(DA1_REPLY).await;
    assert!(written(&accepted), "the later query's reply passes ahead");
    assert_eq!(rig.taken(), DA1_REPLY);
    let concatenated = [DA1_REPLY, DA1_REPLY].concat();
    for (query, bytes, label) in [
        (true, concatenated.as_slice(), "two replies in one frame"),
        (false, DA1_REPLY, "a reply to no outstanding query"),
        (true, b"\x1b[?62;".as_slice(), "half a reply"),
        (false, b"22c".as_slice(), "the rest of a split reply"),
    ] {
        if query {
            rig.query(QueryKind::Da1);
        }
        let (accepted, _) = rig.offer(bytes).await;
        assert!(matches!(accepted, Accepted::Held), "{label} is held");
        held.extend_from_slice(bytes);
    }
    rig.writer.release_fence(Ok(()));
    rig.taken_becomes(&[DA1_REPLY, &held].concat()).await;
}

/// Scenario: Close a fence, hold a key, and have the agent's output ask eight DA1 queries after the trigger, filling the query ledger. Hand the gate one whole DA1 reply and park it just after the gate decided it passes ahead; while it is parked, have the output ask two more queries, which saturates the ledger, then let it go; then offer a second DA1 reply to the saturated ledger. The reply decided before the saturation is written ahead and moves no epoch; the one offered after it is held and moves the epoch once; the key never reaches the PTY before the release, and after it the PTY has the passing reply, then the key, then the held reply (R-U2-04).
#[spec("inbox/probe/011")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_011_pass_ahead_and_its_epoch_are_one_decision() {
    let rig = Arc::new(Rig::new(true, None));
    rig.fence();
    let (accepted, key) = rig.offer(b"key|").await;
    assert!(matches!(accepted, Accepted::Held));
    for _ in 0..LEDGER_SLOTS {
        rig.query(QueryKind::Da1);
    }
    let epoch = rig.input.epoch().epoch();
    let (reached, release) = rig.input.park_next_acceptance();
    let offered = tokio::spawn({
        let rig = Arc::clone(&rig);
        async move { rig.offer(DA1_REPLY).await }
    });
    tokio::task::spawn_blocking(move || reached.recv_timeout(Duration::from_secs(8)))
        .await
        .unwrap()
        .expect("the reply parks after its decision");
    // The output hook saturates the ledger between the decision and the
    // job reaching the writer.
    rig.query(QueryKind::Da1);
    rig.query(QueryKind::Da1);
    assert!(
        !rig.input.reply_class(QueryKind::Da1).1,
        "saturated: a reply classified now is not exempt"
    );
    assert_eq!(rig.taken(), b"");
    release.send(()).unwrap();
    let (accepted, _) = offered.await.unwrap();
    assert!(
        written(&accepted),
        "decided before the saturation, it passes"
    );
    assert_eq!(rig.input.epoch().epoch(), epoch, "and moves no epoch");
    assert_eq!(rig.taken(), DA1_REPLY);
    assert_eq!(rig.writer.gate().status().held_ops, 1);
    assert!(rig.settled(key).is_empty(), "the key is still held");

    let (accepted, _) = rig.offer(DA1_REPLY).await;
    assert!(
        matches!(accepted, Accepted::Held),
        "offered after the saturation, it is held"
    );
    assert_eq!(rig.input.epoch().epoch(), epoch + 1, "and moves the epoch");
    assert_eq!(rig.taken(), DA1_REPLY);
    rig.writer.release_fence(Ok(()));
    rig.taken_becomes(&[DA1_REPLY, b"key|", DA1_REPLY].concat())
        .await;
}

/// Scenario: Hand the gate a frame with no fence and park its acceptance just after it moved the epoch. While parked, the epoch has moved and the PTY writer has been handed nothing; once the park lets go, the frame is written.
#[spec("inbox/probe/011")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_011_the_epoch_moves_before_the_job_is_visible() {
    let rig = Arc::new(Rig::new(true, None));
    let (reached, release) = rig.input.park_next_acceptance();
    let offered = tokio::spawn({
        let rig = Arc::clone(&rig);
        async move { rig.offer(b"parked").await }
    });
    tokio::task::spawn_blocking(move || reached.recv_timeout(Duration::from_secs(8)))
        .await
        .unwrap()
        .expect("the acceptance parks");
    assert_eq!(rig.input.epoch().epoch(), 1, "moved before the handoff");
    assert!(
        rig.entered
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_millis(100))
            .is_err(),
        "the writer has not been handed the job"
    );
    assert_eq!(rig.taken(), b"");
    release.send(()).unwrap();
    assert!(written(&offered.await.unwrap().0));
    assert_eq!(rig.taken(), b"parked");
}

/// Scenario: Take a queued job back, then close its generation's ledger while a started job is still inside the PTY. The taken-back job reports one physical outcome, cancelled, and the writer dropping it later reports nothing more; the started job leaves the outstanding set at retirement but stays unreported until the writer reports it written, once.
#[spec("inbox/probe/011")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_011_each_ticket_reports_one_physical_outcome() {
    let rig = Rig::new(false, None);
    // A job the writer starts: inside the PTY write until the PTY takes it.
    let started = rig.ledger.admit();
    let started_id2 = started.ticket();
    let (tx, _done) = oneshot::channel();
    rig.writer
        .pty
        .submit_ticketed(
            PtyOp::WriteAll(b"inside".to_vec(), ByteSource::User, true),
            PtyReply::Async(tx),
            Some(started),
        )
        .unwrap();
    rig.entered.lock().unwrap().recv().unwrap();
    let queued = rig.ledger.admit();
    let queued_id = queued.ticket();
    let outcome = rig
        .writer
        .pty
        .run_ticketed(
            PtyOp::WriteAll(b"queued".to_vec(), ByteSource::User, true),
            Some(Duration::from_millis(20)),
            Some(queued),
        )
        .await;
    assert!(matches!(outcome, PtyJobOutcome::Withdrawn));
    rig.ledger.close();
    assert_eq!(
        rig.ledger.marks().outstanding,
        0,
        "retired: none outstanding"
    );
    assert_eq!(
        rig.ledger.jobs_unreported(),
        1,
        "the started job hasn't reported"
    );
    rig.open();
    rig.writer.until_idle().await;
    // The writer drops the taken-back job when it reaches it: a job queued
    // after it ends only once that has happened.
    assert!(matches!(
        rig.writer.pty.run(PtyOp::Flush, None).await,
        PtyJobOutcome::Done(_)
    ));
    let physical = |ticket: u64| -> Vec<Settlement> {
        rig.settled(ticket)
            .into_iter()
            .filter(|how| *how != Settlement::Retired)
            .collect()
    };
    assert_eq!(physical(queued_id), [Settlement::Cancelled]);
    assert_eq!(physical(started_id2), [Settlement::Written { bytes: 6 }]);
    assert!(rig.settled(started_id2).contains(&Settlement::Retired));
    assert_eq!(rig.ledger.jobs_unreported(), 0);
}

/// Scenario: Close a generation's ledger while a job of its is queued behind a write the PTY isn't taking, then take that job back by dropping its caller, and let the PTY take again. The ticket is logged retired at the close and then reports one physical outcome, cancelled, at the take-back; the writer, reaching the taken-back job, reports nothing more, and nothing stays in flight or unreported.
#[spec("inbox/probe/011")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_011_a_take_back_after_the_close_reports_once() {
    let rig = Arc::new(Rig::new(false, None));
    let (tx, _blocker) = std::sync::mpsc::sync_channel(1);
    rig.writer
        .pty
        .submit(
            PtyOp::Write(b"blocker".to_vec(), ByteSource::User),
            PtyReply::Blocking(tx),
        )
        .unwrap();
    rig.entered.lock().unwrap().recv().unwrap();
    let queued = rig.ledger.admit();
    let queued_id = queued.ticket();
    let caller = tokio::spawn({
        let rig = Arc::clone(&rig);
        async move {
            rig.writer
                .pty
                .run_ticketed(
                    PtyOp::WriteAll(b"queued".to_vec(), ByteSource::User, true),
                    None,
                    Some(queued),
                )
                .await;
        }
    });
    let deadline = Instant::now() + Duration::from_secs(8);
    while rig.ledger.marks().job_mark == 0 {
        assert!(Instant::now() < deadline, "the job is queued");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    rig.ledger.close();
    assert_eq!(rig.ledger.jobs_unreported(), 1);
    caller.abort();
    let _ = caller.await;
    assert_eq!(
        rig.settled(queued_id),
        [Settlement::Retired, Settlement::Cancelled]
    );
    rig.open();
    rig.writer.until_idle().await;
    assert!(matches!(
        rig.writer.pty.run(PtyOp::Flush, None).await,
        PtyJobOutcome::Done(_)
    ));
    assert_eq!(
        rig.settled(queued_id),
        [Settlement::Retired, Settlement::Cancelled],
        "reported once"
    );
    assert_eq!(rig.ledger.jobs_unreported(), 0);
    assert_eq!(rig.taken(), b"blocker");
    // The flush leaves the count just after it answers.
    rig.writer.until_idle().await;
    assert_eq!(rig.in_flight(), 0);
}

/// Hold `bytes` from a person in `id`'s fence, closing it first.
#[cfg(unix)]
async fn hold_on(
    registry: &Arc<AgentPtyRegistry>,
    id: &str,
    bytes: &[u8],
) -> (Arc<IngressLedger>, u64) {
    let gate = Arc::clone(&registry.inner.lock().agents[id].gate);
    assert!(gate.fence());
    let ledger = registry.ingress_ledger(id).unwrap();
    let ticket = ledger.admit();
    let at = ticket.ticket();
    let writer = registry.agent_writer(id).unwrap();
    let accepted = writer
        .lock()
        .await
        .accept(UserFrame::new(bytes.to_vec(), Some(ticket), None), None)
        .await;
    assert!(matches!(accepted, Accepted::Held));
    (ledger, at)
}

/// Scenario: Through the registry, hold a person's frame in a fence on a `cat` pane and close the agent; on a second pane restart the agent; on a third spawn a new agent over one whose process already exited. Each way a generation retires refuses its held frame `exited` once, leaves nothing in flight and writes nothing, and its gate accepts nothing more.
#[cfg(unix)]
#[spec("inbox/probe/011")]
#[tokio::test]
async fn probe_011_every_retirement_path_retires_the_gate() {
    let registry = Arc::new(AgentPtyRegistry::new());
    let refused = |ledger: &IngressLedger, ticket: u64| {
        ledger
            .settled()
            .into_iter()
            .filter(|(t, how)| *t == ticket && *how != Settlement::Retired)
            .map(|(_, how)| how)
            .collect::<Vec<_>>()
    };
    let gate_of = |id: &str| Arc::clone(&registry.inner.lock().agents[id].gate);

    let closed = spawn_on(&registry, "probe-011-close", "/bin/cat");
    let gate = gate_of(&closed);
    let (ledger, ticket) = hold_on(&registry, &closed, b"never").await;
    registry.close_agent(&closed).expect("close");
    assert_eq!(
        refused(&ledger, ticket),
        [Settlement::Refused("exited")],
        "close"
    );
    assert!(gate.gate.retired());
    assert_eq!(gate.in_flight.count.load(Ordering::SeqCst), 0);

    let restarted = spawn_on(&registry, "probe-011-respawn", "/bin/cat");
    let gate = gate_of(&restarted);
    let (ledger, ticket) = hold_on(&registry, &restarted, b"never").await;
    registry
        .respawn_agent_for_pane("probe-011-respawn", "/bin/cat")
        .await
        .expect("respawn");
    assert_eq!(
        refused(&ledger, ticket),
        [Settlement::Refused("exited")],
        "respawn"
    );
    assert!(gate.gate.retired());

    let pane = "probe-011-exited";
    let exited = spawn_on(&registry, pane, "sh -c 'sleep 0.3'");
    let gate = gate_of(&exited);
    let (ledger, ticket) = hold_on(&registry, &exited, b"never").await;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !registry.pane_occupant_has_exited(pane) {
        assert!(Instant::now() < deadline, "the predecessor exits");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!gate.gate.retired(), "exiting alone retires nothing");
    let _successor = spawn_on(&registry, pane, "/bin/cat");
    assert_eq!(
        refused(&ledger, ticket),
        [Settlement::Refused("exited")],
        "spawn over"
    );
    assert!(gate.gate.retired());
    assert!(!gate.fence());
    registry.shutdown_all();
}

#[cfg(unix)]
fn spawn_on(registry: &Arc<AgentPtyRegistry>, pane: &str, command: &str) -> String {
    registry
        .spawn_agent(SpawnOptions {
            command: Some(command),
            env: crate::test_isolation::pin_unreachable_endpoints(vec![(
                DOT_AGENT_DECK_PANE_ID.to_string(),
                pane.to_string(),
            )]),
            ..SpawnOptions::default()
        })
        .expect("spawn on the pane")
}
