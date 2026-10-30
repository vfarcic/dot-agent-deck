//! PRD #1666 part 3 (Q35-16): the pre-write planner and the physical tracker
//! on the real pane writer and writer thread, over a scripted PTY writer
//! (`inbox/probe/010`, `019`, `075`, `076`).

use super::*;
use crate::composer_probe::epoch::{GenerationInput, Plan, Writer};
use crate::composer_probe::planner::Boundary;
use crate::composer_probe::record::{EcPolicy, Records};
use crate::doorbell_txn::{ClaimState, JobClaim, Start, StartCtx};
use crate::ingress_ledger::{IngressLedger, Settlement};
use spec::spec;

const G: u64 = 7;

/// A PTY writer that keeps every `write` call's bytes, one chunk per call.
/// While shut it takes nothing, after saying a write arrived; with a budget
/// it takes that many bytes in all and then fails every write. With a
/// per-call cap a call takes at most that many (a short write), and with
/// `shut_after` the call after that many calls finds it shut.
struct Scripted {
    chunks: Arc<Mutex<Vec<Vec<u8>>>>,
    open: Arc<(Mutex<bool>, std::sync::Condvar)>,
    entered: std::sync::mpsc::Sender<()>,
    budget: Option<usize>,
    per_call: Option<usize>,
    shut_after: Option<usize>,
    calls: usize,
}

impl std::io::Write for Scripted {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let (open, wake) = &*self.open;
        if self.shut_after == Some(self.calls) {
            *open.lock().unwrap() = false;
        }
        self.calls += 1;
        let _ = self.entered.send(());
        let mut guard = open.lock().unwrap();
        while !*guard {
            guard = wake.wait(guard).unwrap();
        }
        drop(guard);
        let buf = &buf[..self.per_call.map_or(buf.len(), |cap| cap.min(buf.len()))];
        let n = match &mut self.budget {
            None => buf.len(),
            Some(0) => return Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)),
            Some(left) => {
                let n = (*left).min(buf.len());
                *left -= n;
                n
            }
        };
        self.chunks.lock().unwrap().push(buf[..n].to_vec());
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A pane writer over a [`Scripted`] PTY writer, with records attached so a
/// liability can be raised, and a page that keeps what is published to it.
struct Rig {
    writer: PaneWriter,
    input: Arc<GenerationInput>,
    ledger: Arc<IngressLedger>,
    retired: Arc<AtomicBool>,
    chunks: Arc<Mutex<Vec<Vec<u8>>>>,
    open: Arc<(Mutex<bool>, std::sync::Condvar)>,
    entered: Mutex<std::sync::mpsc::Receiver<()>>,
    notices: tokio::sync::mpsc::Receiver<Vec<u8>>,
    notice: tokio::sync::mpsc::Sender<Vec<u8>>,
    /// Every native number published, in order.
    page: Arc<Mutex<Vec<u64>>>,
    hook: Mutex<crate::composer_probe::paste_mode::OutputHook>,
    /// The pane's input state, where a test arms a write pause.
    state: Arc<Mutex<PaneInputState>>,
}

/// How a [`Rig`]'s scripted PTY behaves: see [`Scripted`]. `pane` names the
/// pane its writer records into, which a write pause needs.
#[derive(Default)]
struct Script {
    budget: Option<usize>,
    per_call: Option<usize>,
    shut_after: Option<usize>,
    pane: bool,
}

/// The pane a [`Script`] with `pane` records into.
const PANE: &str = "planner-pane";

impl Rig {
    fn new(open: bool, budget: Option<usize>) -> Self {
        Self::scripted(
            open,
            Script {
                budget,
                ..Script::default()
            },
        )
    }

    fn scripted(open: bool, script: Script) -> Self {
        let input = Arc::new(GenerationInput::default());
        assert!(input.attach_records(Records::new(G, EcPolicy::Hold, Instant::now())));
        let page = Arc::new(Mutex::new(Vec::new()));
        let kept = Arc::clone(&page);
        assert!(input.set_native_page(Box::new(move |seq| kept.lock().unwrap().push(seq))));
        let chunks = Arc::new(Mutex::new(Vec::new()));
        let open = Arc::new((Mutex::new(open), std::sync::Condvar::new()));
        let (tx, entered) = std::sync::mpsc::channel();
        let retired = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(PaneInputState::default()));
        let writer = PaneWriter::new(
            Box::new(Scripted {
                chunks: Arc::clone(&chunks),
                open: Arc::clone(&open),
                entered: tx,
                budget: script.budget,
                per_call: script.per_call,
                shut_after: script.shut_after,
                calls: 0,
            }),
            script.pane.then(|| PANE.to_string()),
            Arc::clone(&state),
            Arc::clone(&retired),
            Arc::new(AtomicBool::new(false)),
            None,
            Arc::clone(&input),
        );
        let (notice, notices) = tokio::sync::mpsc::channel(64);
        let hook = Mutex::new(crate::composer_probe::paste_mode::OutputHook::new(
            Arc::clone(&input),
        ));
        Self {
            writer,
            input,
            ledger: IngressLedger::new(),
            retired,
            chunks,
            open,
            entered: Mutex::new(entered),
            notices,
            notice,
            page,
            hook,
            state,
        }
    }

    /// Park the writer thread after its next PTY write returns, before what
    /// it took is accounted: the "reached" receiver and the "resume" sender.
    /// Needs a rig with a pane.
    fn pause_after_write(&self) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (reached, reached_rx) = std::sync::mpsc::channel();
        let (resume, release) = std::sync::mpsc::channel();
        self.state.lock().unwrap().write_pause = Some(PtyWritePauseArm {
            pane_id_env: PANE.to_string(),
            at: PtyWritePause::AfterWrite,
            reached,
            release,
        });
        (reached_rx, resume)
    }

    /// Forget every write call reported so far.
    fn drain_entered(&self) {
        while self.entered.lock().unwrap().try_recv().is_ok() {}
    }

    /// Wait until a write call arrives at the PTY.
    async fn write_entered(&self) {
        let deadline = Instant::now() + Duration::from_secs(8);
        while self.entered.lock().unwrap().try_recv().is_err() {
            assert!(Instant::now() < deadline, "no write arrived at the PTY");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    /// Shut the PTY: a write arriving now waits until [`Self::open`].
    fn shut(&self) {
        *self.open.0.lock().unwrap() = false;
    }

    /// Make the writer thread leave, handing its PTY writer back, once it
    /// comes to this job: jobs queued after it are dropped unrun.
    fn surrender(&self) -> oneshot::Receiver<(Box<dyn std::io::Write + Send>, InputRecorder)> {
        let (tx, rx) = oneshot::channel();
        self.writer
            .pty
            .submit(
                PtyOp::Surrender(tx),
                PtyReply::Blocking(std::sync::mpsc::sync_channel(1).0),
            )
            .expect("the writer thread is running");
        rx
    }

    /// A liability out: the planner cuts what the gate lets through.
    fn liability(&self) {
        self.input
            .liability_raise(G, Instant::now())
            .expect("raised");
    }

    fn fence(&self) {
        assert!(self.writer.gate().fence());
    }

    fn release(&self) -> usize {
        self.writer.release_fence(Ok(()))
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

    fn chunks(&self) -> Vec<Vec<u8>> {
        self.chunks.lock().unwrap().clone()
    }

    fn taken(&self) -> Vec<u8> {
        self.chunks().concat()
    }

    fn in_flight(&self) -> usize {
        self.writer.pty.in_flight.count.load(Ordering::SeqCst)
    }

    fn held(&self) -> (bool, usize, usize) {
        let s = self.writer.gate().status();
        (s.fenced, s.held_ops, s.held_bytes)
    }

    fn page(&self) -> Vec<u64> {
        self.page.lock().unwrap().clone()
    }

    fn natives(&self) -> Vec<u64> {
        use crate::composer_probe::query_ledger::EntryView;
        self.input
            .hazards(Instant::now())
            .0
            .into_iter()
            .map(|e| match e {
                EntryView::Latched { seq } | EntryView::Quiet { seq, .. } => seq,
            })
            .collect()
    }

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

    async fn idle(&self) {
        self.writer.until_idle().await;
    }

    /// Wait until `n` jobs or held ops are in flight: a cut op's held rest
    /// stays in flight until it is written or refused.
    async fn flight(&self, n: usize) {
        let deadline = Instant::now() + Duration::from_secs(8);
        while self.in_flight() != n {
            assert!(
                Instant::now() < deadline,
                "{} in flight, not {n}",
                self.in_flight()
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    /// The agent's output asks OSC `slot`'s colour now.
    fn colour_query(&self, slot: u8) {
        self.hook
            .lock()
            .unwrap()
            .on_output(format!("\x1b]{slot};?\x1b\\").as_bytes());
    }
}

fn reply(slot: u8) -> Vec<u8> {
    format!("\x1b]{slot};rgb:1/22/333\x07").into_bytes()
}

/// Every Ctrl+G candidate form of core §4.4, each alone in a frame.
const FORMS: &[&[u8]] = &[
    b"\x07",
    b"\x1b\x07",
    b"\x1b[103;5u",
    b"\x1b[71;5:3u",
    b"\x1b[27;5;103~",
    b"\x1b[27;5;71~",
    b"\x1b[1\x07;5u",
    b"\x1bPq\x07\x1b\\",
    b"\x1b]10;rgb:1/22/333\x07",
];

// ------------------------------------------------------------------------
// inbox/probe/010: the planner, through the real writer.

/// Scenario: With a liability out and a check open, hold `ab`, BEL and `cd` in one frame, then release twice. The first release writes only through the BEL and keeps `cd` held behind the fence; the second writes `cd`; the frame's ticket settles once, as five bytes written, only after the last sub-op, and the epoch moved once.
#[spec("inbox/probe/010")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_010_a_held_frame_is_written_through_its_candidate_and_its_rest_held() {
    let rig = Rig::new(true, None);
    rig.liability();
    rig.fence();
    let (accepted, ticket) = rig.offer(b"ab\x07cd").await;
    assert!(matches!(accepted, Accepted::Held));
    assert_eq!(rig.held(), (true, 1, 5));
    assert_eq!(rig.release(), 1);
    rig.taken_becomes(b"ab\x07").await;
    rig.flight(1).await;
    assert_eq!(rig.held(), (true, 1, 2), "the rest is held behind the cut");
    assert!(
        rig.settled(ticket).is_empty(),
        "the ticket waits for the rest"
    );
    assert_eq!(rig.release(), 1);
    rig.taken_becomes(b"ab\x07cd").await;
    assert_eq!(
        rig.settles(ticket).await,
        [Settlement::Written { bytes: 5 }]
    );
    rig.idle().await;
    assert_eq!(rig.in_flight(), 0);
    assert_eq!(rig.held(), (false, 0, 0));
    assert_eq!(rig.input.epoch().epoch(), 1, "one op, one move");
    assert!(
        rig.page().is_empty() && rig.natives().is_empty(),
        "no native"
    );
}

/// Scenario: Hold, behind a check, a frame of three BELs, a frame ending in BEL and a plain frame, then release until the fence opens. Each release writes through the next candidate only, a candidate at a frame's last byte keeps the next frame held, and the bytes reach the PTY once, in order, with each ticket settled once.
#[spec("inbox/probe/010")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_010_every_held_candidate_cuts_one_release_at_a_time() {
    let rig = Rig::new(true, None);
    rig.fence();
    let mut tickets = Vec::new();
    for bytes in [b"\x07\x07\x07".as_slice(), b"x\x07", b"tail"] {
        let (accepted, ticket) = rig.offer(bytes).await;
        assert!(matches!(accepted, Accepted::Held));
        tickets.push(ticket);
    }
    let expected: [&[u8]; 5] = [
        b"\x07",
        b"\x07\x07",
        b"\x07\x07\x07",
        b"\x07\x07\x07x\x07",
        b"\x07\x07\x07x\x07tail",
    ];
    for (i, want) in expected.iter().enumerate() {
        assert!(rig.held().0, "release {i}: the fence is closed");
        rig.release();
        rig.taken_becomes(want).await;
    }
    rig.idle().await;
    assert_eq!(rig.held(), (false, 0, 0));
    assert_eq!(
        rig.settles(tickets[0]).await,
        [Settlement::Written { bytes: 3 }]
    );
    assert_eq!(
        rig.settles(tickets[1]).await,
        [Settlement::Written { bytes: 2 }]
    );
    assert_eq!(
        rig.settles(tickets[2]).await,
        [Settlement::Written { bytes: 4 }]
    );
    assert_eq!(rig.in_flight(), 0);
}

/// Scenario: With a liability out and no check open, offer a frame holding a CSI-u Ctrl+G between two letters, then a plain frame; after a release, lower the liability and type a BEL. The first frame is accepted and held: its prefix through the key reaches the PTY at once, and its rest and the next frame wait behind the fence it closed until a release; the cut key took no native number, so the BEL is numbered 1.
#[spec("inbox/probe/010")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_010_with_a_liability_out_an_open_frame_is_cut_at_its_candidate() {
    let rig = Rig::new(true, None);
    rig.liability();
    let (accepted, first) = rig.offer(b"x\x1b[103;5uy").await;
    assert!(matches!(accepted, Accepted::Held), "the gate owns it now");
    rig.taken_becomes(b"x\x1b[103;5u").await;
    let (accepted, second) = rig.offer(b"z").await;
    assert!(matches!(accepted, Accepted::Held), "behind the new segment");
    rig.flight(2).await;
    assert_eq!(rig.held(), (true, 2, 2));
    assert!(
        rig.page().is_empty(),
        "a candidate under a liability isn't native"
    );
    assert_eq!(rig.release(), 2);
    rig.taken_becomes(b"x\x1b[103;5uyz").await;
    assert_eq!(
        rig.settles(first).await,
        [Settlement::Written { bytes: 10 }]
    );
    assert_eq!(
        rig.settles(second).await,
        [Settlement::Written { bytes: 1 }]
    );
    rig.idle().await;
    assert_eq!(rig.in_flight(), 0);
    // The cut candidate took no native number: once the liability is
    // lowered, the next candidate is the first one numbered.
    rig.input.liability_lower(G, Instant::now()).unwrap();
    rig.offer(b"\x07").await;
    assert_eq!(rig.page(), [1]);
}

/// Scenario: With a liability out, a check open and a PTY that takes two bytes and then fails, hold `ab`, BEL and `cd`, then release twice. The first sub-op writes `ab` and fails, its connection is told once; the rest ends unwritten at its dequeue, and the ticket settles once, as two bytes written in part.
#[spec("inbox/probe/010")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_010_a_partial_sub_op_ends_the_rest_gone_and_settles_the_ticket_once() {
    let mut rig = Rig::new(true, Some(2));
    rig.liability();
    rig.fence();
    let (_, ticket) = rig.offer(b"ab\x07cd").await;
    rig.release();
    rig.taken_becomes(b"ab").await;
    rig.flight(1).await;
    assert!(rig.settled(ticket).is_empty());
    rig.release();
    assert_eq!(
        rig.settles(ticket).await,
        [Settlement::Partial { accepted: 2 }]
    );
    rig.idle().await;
    assert_eq!(rig.taken(), b"ab", "nothing of the rest is written");
    assert_eq!(rig.in_flight(), 0);
    assert_eq!(
        rig.notices.recv().await.as_deref(),
        Some(crate::agent_pty::WRITE_FAILED.as_bytes())
    );
    assert!(rig.notices.try_recv().is_err(), "told once");
}

/// Scenario: Retire a generation whose fence holds the rest of a frame cut after its prefix was written. The rest is refused, its connection told `exited`, and its ticket settles once as written in part.
#[spec("inbox/probe/010")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_010_retirement_refuses_a_cut_rest_as_written_in_part() {
    let mut rig = Rig::new(true, None);
    rig.fence();
    let (_, ticket) = rig.offer(b"ab\x07cd").await;
    rig.release();
    rig.taken_becomes(b"ab\x07").await;
    rig.flight(1).await;
    rig.writer.gate().retire();
    assert_eq!(
        rig.settles(ticket).await,
        [Settlement::Partial { accepted: 3 }]
    );
    assert_eq!(
        rig.notices.recv().await.as_deref(),
        Some(b"exited".as_slice())
    );
    assert_eq!(rig.in_flight(), 0);
}

/// Scenario: Behind a check, hold `ab`, BEL and `cd`, release once and park the writer just after the PTY took the prefix, before it counted it; retire the generation there. Nothing settles while the prefix is unaccounted; once the writer resumes, the ticket settles once, as three bytes written in part, and the connection is told `exited` once.
#[spec("inbox/probe/010")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_010_retirement_waits_for_a_prefix_the_pty_took_but_hasnt_counted() {
    let mut rig = Rig::scripted(
        true,
        Script {
            pane: true,
            ..Script::default()
        },
    );
    rig.fence();
    let (_, ticket) = rig.offer(b"ab\x07cd").await;
    let (reached, resume) = rig.pause_after_write();
    rig.release();
    tokio::task::spawn_blocking(move || reached.recv().expect("the writer parks"))
        .await
        .unwrap();
    assert_eq!(rig.taken(), b"ab\x07", "the PTY took the prefix");
    rig.writer.gate().retire();
    assert!(
        rig.settled(ticket).is_empty(),
        "no settlement while the prefix's count is outstanding"
    );
    resume.send(()).unwrap();
    assert_eq!(
        rig.settles(ticket).await,
        [Settlement::Partial { accepted: 3 }]
    );
    rig.idle().await;
    assert_eq!(rig.settled(ticket).len(), 1, "settled once");
    assert_eq!(rig.in_flight(), 0);
    assert_eq!(
        rig.notices.recv().await.as_deref(),
        Some(b"exited".as_slice())
    );
    assert!(rig.notices.try_recv().is_err(), "told once");
}

/// Scenario: Behind a check, hold `ab`, BEL and `cd` on a PTY that takes two bytes a call and then stops taking any; release once, so the prefix's write took `ab` and waits inside the PTY for its BEL, and retire the generation there. Nothing settles while the prefix is inside the PTY; once the PTY takes the BEL, the ticket settles once, as three bytes written in part.
#[spec("inbox/probe/010")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_010_retirement_waits_for_a_prefix_still_inside_the_pty_after_a_short_write() {
    let mut rig = Rig::scripted(
        true,
        Script {
            per_call: Some(2),
            shut_after: Some(1),
            ..Script::default()
        },
    );
    rig.fence();
    let (_, ticket) = rig.offer(b"ab\x07cd").await;
    rig.release();
    rig.taken_becomes(b"ab").await;
    rig.write_entered().await;
    rig.write_entered().await;
    rig.writer.gate().retire();
    assert!(
        rig.settled(ticket).is_empty(),
        "no settlement while the prefix is inside the PTY"
    );
    rig.open();
    rig.taken_becomes(b"ab\x07").await;
    assert_eq!(
        rig.settles(ticket).await,
        [Settlement::Partial { accepted: 3 }]
    );
    rig.idle().await;
    assert_eq!(rig.settled(ticket).len(), 1, "settled once");
    assert_eq!(rig.in_flight(), 0);
    assert_eq!(
        rig.notices.recv().await.as_deref(),
        Some(b"exited".as_slice())
    );
    assert!(rig.notices.try_recv().is_err(), "told once");
}

/// Scenario: Behind a check, hold three BELs on a shut PTY and release twice, so two of the frame's sub-ops are with the writer, then retire the generation while the third is held. When the PTY opens and takes both, the ticket settles once, as two bytes written in part, only after the second sub-op reported, and the connection is told `exited` once.
#[spec("inbox/probe/010")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_010_retirement_waits_for_every_running_sub_op() {
    let mut rig = Rig::new(false, None);
    rig.fence();
    let (_, ticket) = rig.offer(b"\x07\x07\x07").await;
    assert_eq!(rig.release(), 1);
    assert_eq!(rig.release(), 1);
    rig.write_entered().await;
    rig.writer.gate().retire();
    assert!(rig.settled(ticket).is_empty());
    rig.open();
    rig.taken_becomes(b"\x07\x07").await;
    assert_eq!(
        rig.settles(ticket).await,
        [Settlement::Partial { accepted: 2 }]
    );
    rig.idle().await;
    assert_eq!(rig.settled(ticket).len(), 1, "settled once");
    assert_eq!(rig.in_flight(), 0);
    assert_eq!(
        rig.notices.recv().await.as_deref(),
        Some(b"exited".as_slice())
    );
    assert!(rig.notices.try_recv().is_err(), "told once");
}

/// Scenario: Behind a check, hold three BELs on a shut PTY and release twice, so one sub-op is inside the PTY and the next is queued; the generation then retires, refusing the held rest. When the PTY opens, the first sub-op's BEL is written, the queued one is reached after the retirement and ends unwritten, the connection is told `exited` once, not once per part, and the ticket settles once, as one byte written in part.
#[spec("inbox/probe/010")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_010_a_sub_op_reached_after_retirement_doesnt_tell_the_connection_again() {
    let mut rig = Rig::new(false, None);
    rig.fence();
    let (_, ticket) = rig.offer(b"\x07\x07\x07").await;
    assert_eq!(rig.release(), 1);
    assert_eq!(rig.release(), 1);
    rig.write_entered().await;
    rig.retired.store(true, Ordering::SeqCst);
    rig.writer.gate().retire();
    rig.open();
    assert_eq!(
        rig.settles(ticket).await,
        [Settlement::Partial { accepted: 1 }]
    );
    rig.idle().await;
    assert_eq!(rig.taken(), b"\x07", "the queued sub-op wasn't written");
    assert_eq!(rig.settled(ticket).len(), 1, "settled once");
    assert_eq!(rig.in_flight(), 0);
    assert_eq!(
        rig.notices.recv().await.as_deref(),
        Some(b"exited".as_slice())
    );
    assert!(rig.notices.try_recv().is_err(), "told once");
}

/// Scenario: Behind a check, hold `ab`, BEL and `cd` on a PTY that takes two bytes and then fails; release once, so the prefix writes `ab` and fails, then retire the generation while the rest is held. The connection is told of the failed write once and not again for the retirement, and the ticket settles once, as two bytes written in part.
#[spec("inbox/probe/010")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_010_a_failed_prefix_then_retirement_tells_the_connection_once() {
    let mut rig = Rig::new(true, Some(2));
    rig.fence();
    let (_, ticket) = rig.offer(b"ab\x07cd").await;
    rig.release();
    rig.taken_becomes(b"ab").await;
    rig.flight(1).await;
    rig.writer.gate().retire();
    assert_eq!(
        rig.settles(ticket).await,
        [Settlement::Partial { accepted: 2 }]
    );
    rig.idle().await;
    assert_eq!(rig.in_flight(), 0);
    assert_eq!(
        rig.notices.recv().await.as_deref(),
        Some(crate::agent_pty::WRITE_FAILED.as_bytes())
    );
    assert!(rig.notices.try_recv().is_err(), "told once");
}

/// Scenario: Behind a check, hold `ab`, BEL and `cd` and release the prefix, which the PTY takes. Then the writer thread leaves: once with the rest's job queued behind its leaving, so the job is dropped unrun, and once before the rest is released, so its job can't be queued. Either way the ticket settles once as three bytes written in part, not as a failure that forgets the prefix.
#[spec("inbox/probe/010")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_010_a_last_sub_op_dropped_unrun_settles_with_what_the_prefix_took() {
    for queued in [true, false] {
        let rig = Rig::new(true, None);
        rig.fence();
        let (_, ticket) = rig.offer(b"ab\x07cd").await;
        rig.release();
        rig.taken_becomes(b"ab\x07").await;
        rig.flight(1).await;
        if queued {
            // The thread is busy with a write around the gate while it is
            // told to leave and the rest is queued behind that.
            rig.shut();
            rig.drain_entered();
            let busy = rig.writer.write_user(b"z");
            tokio::pin!(busy);
            tokio::select! {
                _ = &mut busy => panic!("the PTY is shut"),
                () = rig.write_entered() => {}
            }
            let left = rig.surrender();
            assert_eq!(rig.release(), 1);
            rig.open();
            busy.await.unwrap();
            left.await.expect("the writer thread left");
        } else {
            rig.surrender().await.expect("the writer thread left");
            assert_eq!(rig.release(), 1);
        }
        assert_eq!(
            rig.settles(ticket).await,
            [Settlement::Partial { accepted: 3 }],
            "queued behind the leaving: {queued}"
        );
    }
}

/// Scenario: Through the blocking `Write` path, with a liability out, write `ab`, BEL and `cd`. The call returns 3, a short count at the cut, and the PTY took exactly those bytes; the caller's next call carries `cd` and returns 2. With no liability the same bytes return 5 in one call.
#[spec("inbox/probe/010")]
#[test]
fn probe_010_a_write_returns_a_short_count_at_its_cut() {
    use std::io::Write as _;
    let mut rig = Rig::new(true, None);
    assert_eq!(rig.writer.write(b"ab\x07cd").unwrap(), 5);
    rig.liability();
    assert_eq!(rig.writer.write(b"ab\x07cd").unwrap(), 3);
    assert_eq!(rig.writer.write(b"cd").unwrap(), 2);
    assert_eq!(rig.taken(), b"ab\x07cdab\x07cd");
    assert_eq!(
        rig.chunks().last().map(Vec::as_slice),
        Some(b"cd".as_slice())
    );
    // Each call is one op to the planner: three moves.
    assert_eq!(rig.input.epoch().epoch(), 3);
}

/// Scenario: Offer a voice answer whose keys hold a BEL while a liability is out. It is refused at acceptance with core's words, writes nothing, settles its ticket refused and calls its queued hook; with no liability out, the same answer is written.
#[spec("inbox/probe/010")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_010_an_answer_holding_a_candidate_is_refused_while_a_liability_is_out() {
    let rig = Rig::new(true, None);
    let gate = || AnswerGate {
        raised: Arc::new(Mutex::new(HashMap::from([(
            "p".to_string(),
            RaisedQuestion {
                id: "q".into(),
                revision: None,
                input_seq: 0,
            },
        )]))),
        pane_id: "p".into(),
        question_id: "q".into(),
        revision: None,
        pause: None,
    };
    rig.liability();
    let ticket = rig.ledger.admit();
    let id = ticket.ticket();
    let mut queued = false;
    let out = rig
        .writer
        .write_answer(b"1\x07", gate(), || queued = true, Some(ticket))
        .await
        .unwrap();
    assert!(matches!(out, AnswerWrite::Refused));
    assert!(queued);
    assert_eq!(
        rig.settled(id),
        [Settlement::Refused(crate::agent_pty::ANSWER_HOLDS_CTRL_G)]
    );
    assert_eq!(rig.taken(), b"");
    assert_eq!(rig.input.epoch().epoch(), 0, "refused before acceptance");
    rig.input.liability_lower(G, Instant::now()).unwrap();
    let out = rig
        .writer
        .write_answer(b"1\x07", gate(), || {}, None)
        .await
        .unwrap();
    assert!(matches!(out, AnswerWrite::Written(_)));
    assert_eq!(rig.taken(), b"1\x07");
}

/// Scenario: With no liability and no check, offer a frame with two BELs through a PTY that is shut. Both are numbered at acceptance and the last number is on the page before the writer has the job, while the hazard FIFO is still empty; once the PTY takes the bytes, both enter the FIFO, in order. A later frame continues the numbering.
#[spec("inbox/probe/010")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_010_native_candidates_are_published_before_the_write_and_entered_as_taken() {
    let rig = Arc::new(Rig::new(false, None));
    let offer = tokio::spawn({
        let rig = Arc::clone(&rig);
        async move { rig.offer(b"a\x07b\x07").await.0 }
    });
    let entered = Arc::clone(&rig);
    tokio::task::spawn_blocking(move || entered.entered.lock().unwrap().recv().unwrap())
        .await
        .unwrap();
    assert_eq!(rig.page(), [2], "published before the PTY took a byte");
    assert!(
        rig.natives().is_empty(),
        "entered only as the PTY takes them"
    );
    rig.open();
    assert!(matches!(offer.await.unwrap(), Accepted::Written(Ok(()))));
    assert_eq!(rig.natives(), [1, 2]);
    rig.offer(b"\x1b[71;5u").await;
    assert_eq!(rig.page(), [2, 3]);
    assert_eq!(rig.natives(), [1, 2, 3]);
}

/// Scenario: Send a colour query, then answer it whole with a BEL ending, then send the same reply unsolicited and split. With no check the solicited reply numbers nothing; the unsolicited whole reply and the split one's final BEL each number one native candidate. Behind a check, a reply whose query came before the check is held whole and not cut, and an unsolicited one is cut at its BEL.
#[spec("inbox/probe/010")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_010_an_osc_reply_is_a_candidate_unless_a_kept_query_solicited_it_whole() {
    let rig = Rig::new(true, None);
    rig.colour_query(10);
    rig.offer(&reply(10)).await;
    assert!(rig.page().is_empty(), "a solicited whole reply");
    rig.offer(&reply(10)).await;
    assert_eq!(rig.page(), [1], "its query is answered already");
    let split = reply(11);
    rig.colour_query(11);
    for byte in &split {
        rig.offer(&[*byte]).await;
    }
    assert_eq!(rig.page(), [1, 2], "a split reply's BEL");
    rig.idle().await;
    let held = Rig::new(true, None);
    held.colour_query(10);
    held.fence();
    held.offer(&[reply(10), b"t1".to_vec()].concat()).await;
    held.offer(&reply(10)).await;
    held.offer(b"t2").await;
    // The first frame isn't one whole reply, so its BEL cuts.
    held.release();
    held.taken_becomes(&reply(10)).await;
    held.release();
    held.taken_becomes(&[reply(10), b"t1".to_vec(), reply(10)].concat())
        .await;
    held.flight(1).await;
    assert!(held.held().0, "the second reply's BEL cut too: unsolicited");
    held.release();
    held.taken_becomes(&[reply(10), b"t1".to_vec(), reply(10), b"t2".to_vec()].concat())
        .await;
}

/// Scenario: Behind a check, hold one 16 MiB frame of BELs and release three times; then, with no liability or check and a cursor position query outstanding, write another. Each release writes one byte and leaves one held node; the open frame numbers all its candidates as one range, published once, and the hazard FIFO keeps sixteen latched entries and counts the rest.
#[spec("inbox/probe/010")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_010_a_16_mib_candidate_frame_is_cut_lazily_and_numbered_as_one_range() {
    const MIB16: usize = 16 << 20;
    let rig = Rig::new(true, None);
    rig.fence();
    let frame = vec![0x07u8; MIB16];
    let (frame, _) = rig.frame(&frame);
    // Offered with no room check: one op larger than the cap is held alone.
    assert!(matches!(
        rig.writer.accept(frame, None).await,
        Accepted::Held
    ));
    for i in 1..=3usize {
        rig.release();
        rig.taken_becomes(&vec![0x07; i]).await;
        rig.flight(1).await;
        assert_eq!(
            rig.held(),
            (true, 1, MIB16 - i),
            "one node, not one per candidate"
        );
    }
    rig.writer.gate().retire();
    let open = Rig::new(true, None);
    // A cursor position query the agent waits on: `B` holds, so each entry
    // is `Latched` and none leaves under D5's quiet period while a slow
    // machine is still writing the frame.
    open.hook.lock().unwrap().on_output(b"\x1b[6n");
    let (accepted, _) = open.offer(&vec![0x07u8; MIB16]).await;
    assert!(matches!(accepted, Accepted::Written(Ok(()))));
    assert_eq!(open.page(), [MIB16 as u64], "one publication for the range");
    let (kept, extra) = open.input.hazards(Instant::now());
    assert_eq!(
        kept.len(),
        crate::composer_probe::query_ledger::HAZARD_SLOTS
    );
    assert!(kept.iter().all(|e| matches!(
        e,
        crate::composer_probe::query_ledger::EntryView::Latched { .. }
    )));
    assert_eq!(extra as usize, MIB16 - kept.len());
}

/// Scenario: Two clients interleave: one sends `ESC [ 1 0 3 ;`, the other a BEL, then the first finishes `5 u`. Behind a check, the planner reads them in acceptance order as one stream: the BEL inside the unfinished CSI is a candidate and cuts there, and the finished CSI-u Ctrl+G cuts again.
#[spec("inbox/probe/010")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_010_two_clients_interleaving_a_sequence_and_a_bel_are_read_as_one_stream() {
    let rig = Rig::new(true, None);
    rig.fence();
    for bytes in [b"\x1b[103;".as_slice(), b"\x07", b"5u", b"after"] {
        rig.offer(bytes).await;
    }
    rig.release();
    rig.taken_becomes(b"\x1b[103;\x07").await;
    rig.release();
    rig.taken_becomes(b"\x1b[103;\x075u").await;
    rig.release();
    rig.taken_becomes(b"\x1b[103;\x075uafter").await;
}

/// One framing case: its frames, in acceptance order, and where each is cut
/// behind a check.
type FramingCase<'a> = (&'a [&'a [u8]], &'a [Option<usize>]);

/// Scenario: Plan, through the generation's planner, each core §4.11 framing case behind a check and with neither check nor liability: every candidate form alone; Ctrl+G with text; a paste split over frames with a BEL inside, whatever the agent's paste mode; ESC then, later, BEL; CAN and SUB abandoning a sequence; and an overlong CSI ending in `u`. Behind a check each candidate cuts right after itself, an abandoned or plain op doesn't; with nothing out each is numbered instead and nothing cuts.
#[spec("inbox/probe/010")]
#[test]
fn probe_010_the_framing_cases_cut_behind_a_check_and_number_with_nothing_out() {
    let cases: &[FramingCase<'_>] = &[
        (&[b"ab\x07cd"], &[Some(3)]),
        (
            &[b"\x1b[200~a", b"\x07b", b"\x1b[201~"],
            &[None, Some(1), None],
        ),
        (&[b"\x1b", b"\x07"], &[None, Some(1)]),
        (&[b"\x1b[1\x18", b"x"], &[None, None]),
        (&[b"\x1b[1\x1a\x07"], &[Some(5)]),
        (
            &[&[b"\x1b[".as_slice(), &[b'1'; 70], b"u"].concat()],
            &[Some(73)],
        ),
    ];
    for &(frames, cuts) in cases {
        let held = GenerationInput::default();
        let open = GenerationInput::default();
        let mut numbered = 0;
        for (frame, cut) in frames.iter().zip(cuts) {
            let planned = held.enqueue_planned(Writer::Human, frame, Plan::Held);
            assert_eq!(planned.cut.map(|c| c.at), *cut, "{frames:?}: {frame:?}");
            assert!(planned.natives.is_none());
            let planned = open.enqueue_planned(Writer::Human, frame, Plan::Open);
            assert!(planned.cut.is_none(), "nothing cuts with nothing out");
            if let Some(n) = planned.natives {
                numbered += n.last - n.first + 1;
            }
        }
        let expected = cuts.iter().filter(|c| c.is_some()).count() as u64;
        assert_eq!(numbered, expected, "{frames:?}");
    }
    // A BEL inside an unfinished CSI or a DCS string cuts right after the
    // BEL; every other form after its last byte.
    let ends = [1, 2, 8, 9, 11, 10, 4, 4, 18];
    for (form, end) in FORMS.iter().zip(ends) {
        let planned = GenerationInput::default().enqueue_planned(Writer::Human, form, Plan::Held);
        assert_eq!(planned.cut.map(|c| c.at), Some(end), "{form:?}");
    }
    // The deck's own bytes are never cut or numbered.
    let deck = GenerationInput::default().enqueue_planned(Writer::UnnamedDeck, b"\x07", Plan::Open);
    assert!(deck.cut.is_none() && deck.natives.is_none());
}

// ------------------------------------------------------------------------
// inbox/probe/010: §3.7's accounting oracle, as a property test.

/// A tiny deterministic generator, so a failing seed replays exactly.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// One whole token a generated frame is built of. The oracle's own model of
/// a candidate is this enum, not the tokenizer: frames are whole tokens, so
/// no sequence spans two frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tok {
    Text(u8),
    Bel,
    CsiU,
    /// A whole colour reply to OSC `slot`, ended by BEL.
    Reply(u8),
}

impl Tok {
    fn bytes(self) -> Vec<u8> {
        match self {
            Tok::Text(b) => vec![b],
            Tok::Bel => vec![0x07],
            Tok::CsiU => b"\x1b[103;5u".to_vec(),
            Tok::Reply(slot) => reply(slot),
        }
    }
}

/// The oracle's model of a held op: its remaining tokens, and whether its
/// candidates cut.
#[derive(Debug, Clone)]
struct ModelOp {
    id: usize,
    toks: Vec<Tok>,
    /// A whole reply to a query kept at acceptance: no candidate.
    solicited: bool,
}

impl ModelOp {
    /// Bytes through the first candidate, and whether there was one.
    fn through_first(&self) -> (usize, bool) {
        for (i, tok) in self.toks.iter().enumerate() {
            let candidate = matches!(tok, Tok::Bel | Tok::CsiU)
                || (matches!(tok, Tok::Reply(_)) && !self.solicited);
            if candidate {
                return (i + 1, true);
            }
        }
        (self.toks.len(), false)
    }
}

/// The oracle: computed from the generated script alone (core §3.7).
#[derive(Default)]
struct Model {
    fenced: bool,
    liability: bool,
    held: std::collections::VecDeque<ModelOp>,
    /// Each slot's outstanding queries, oldest first: whether each was
    /// asked after the open segment's trigger.
    queries: [std::collections::VecDeque<bool>; 2],
    written: Vec<u8>,
    /// Bytes each op has had written.
    op_written: Vec<usize>,
    op_len: Vec<usize>,
    natives: u64,
}

impl Model {
    fn slot(slot: u8) -> usize {
        usize::from(slot - 10)
    }

    /// A new segment opens: every query asked so far is before its trigger.
    fn trigger(&mut self) {
        for q in &mut self.queries {
            q.iter_mut().for_each(|after| *after = false);
        }
    }

    fn write(&mut self, id: usize, toks: &[Tok]) {
        let bytes: Vec<u8> = toks.iter().flat_map(|t| t.bytes()).collect();
        self.op_written[id] += bytes.len();
        self.written.extend(bytes);
    }

    fn accept(&mut self, id: usize, toks: Vec<Tok>) -> bool {
        let len: usize = toks.iter().map(|t| t.bytes().len()).sum();
        self.op_len.push(len);
        self.op_written.push(0);
        let whole_reply = match toks.as_slice() {
            [Tok::Reply(slot)] => Some(*slot),
            _ => None,
        };
        let kept = whole_reply.map(|slot| self.queries[Self::slot(slot)].front().copied());
        let solicited = matches!(kept, Some(Some(_)));
        // I-5: a whole reply passes a closed fence when the query it
        // answers was asked after the segment's trigger.
        let passes = self.fenced && matches!(kept, Some(Some(true)));
        // Every reply token answers its slot's oldest query, at acceptance.
        for tok in &toks {
            if let Tok::Reply(slot) = tok {
                self.queries[Self::slot(*slot)].pop_front();
            }
        }
        let op = ModelOp {
            id,
            toks,
            solicited,
        };
        if passes {
            let toks = op.toks.clone();
            self.write(id, &toks);
            return true;
        }
        if self.fenced {
            self.held.push_back(op);
            return false;
        }
        let (through, cut) = op.through_first();
        if self.liability && cut {
            let toks = op.toks.clone();
            self.write(id, &toks[..through]);
            self.fenced = true;
            self.trigger();
            if through < toks.len() || toks.is_empty() {
                self.held.push_back(ModelOp {
                    id,
                    toks: toks[through..].to_vec(),
                    solicited: false,
                });
            }
            return false;
        }
        if !self.liability {
            self.natives += op
                .toks
                .iter()
                .filter(|t| {
                    matches!(t, Tok::Bel | Tok::CsiU) || (matches!(t, Tok::Reply(_)) && !solicited)
                })
                .count() as u64;
        }
        let toks = op.toks.clone();
        self.write(id, &toks);
        true
    }

    fn release(&mut self) {
        if !self.fenced {
            return;
        }
        while let Some(mut op) = self.held.pop_front() {
            let (through, cut) = op.through_first();
            let toks = op.toks.clone();
            self.write(op.id, &toks[..through]);
            if cut {
                self.trigger();
                if through < toks.len() {
                    op.toks = toks[through..].to_vec();
                    op.solicited = false;
                    self.held.push_front(op);
                }
                return;
            }
        }
        self.fenced = false;
    }
}

/// Scenario: For 64 seeded scripts, drive the real writer with random frames of text, BELs, CSI-u Ctrl+G and colour replies, checks opened and released, liabilities raised and lowered and colour queries asked, then release until no check is open. An oracle computed from the script alone predicts the exact bytes the PTY took, in order, each frame's single settlement, the native numbers published, and the pass-ahead of a reply only when its query was asked after the open check's trigger.
#[spec("inbox/probe/010")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_010_the_accounting_oracle_holds_over_generated_scripts() {
    for seed in 1..=64u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let rig = Rig::new(true, None);
        let mut model = Model::default();
        let mut tickets = Vec::new();
        let mut queried = 0;
        let mut script = Vec::new();
        for _ in 0..40 {
            match rng.below(10) {
                0 if !model.fenced => {
                    script.push("fence".to_string());
                    rig.fence();
                    model.fenced = true;
                    model.trigger();
                }
                1 => {
                    script.push("release".to_string());
                    rig.release();
                    model.release();
                }
                2 => {
                    script.push(format!("liability {}", !model.liability));
                    if model.liability {
                        rig.input.liability_lower(G, Instant::now()).unwrap();
                    } else {
                        rig.liability();
                    }
                    model.liability = !model.liability;
                }
                3 if queried < 6 => {
                    queried += 1;
                    let slot = 10 + rng.below(2) as u8;
                    script.push(format!("query {slot}"));
                    rig.colour_query(slot);
                    model.queries[Model::slot(slot)].push_back(true);
                }
                _ => {
                    let toks: Vec<Tok> = if rng.below(4) == 0 {
                        vec![Tok::Reply(10 + rng.below(2) as u8)]
                    } else {
                        (0..1 + rng.below(4))
                            .map(|_| match rng.below(8) {
                                0 => Tok::Bel,
                                1 => Tok::CsiU,
                                2 => Tok::Reply(10 + rng.below(2) as u8),
                                n => Tok::Text(b'a' + n as u8),
                            })
                            .collect()
                    };
                    script.push(format!("type {toks:?}"));
                    let bytes: Vec<u8> = toks.iter().flat_map(|t| t.bytes()).collect();
                    let id = tickets.len();
                    let written_now = model.accept(id, toks);
                    let (accepted, ticket) = rig.offer(&bytes).await;
                    tickets.push(ticket);
                    assert_eq!(
                        matches!(accepted, Accepted::Written(Ok(()))),
                        written_now,
                        "seed {seed}: {script:?}"
                    );
                }
            }
            rig.taken_becomes(&model.written).await;
        }
        while model.fenced {
            script.push("release".to_string());
            rig.release();
            model.release();
            rig.taken_becomes(&model.written).await;
        }
        rig.idle().await;
        assert_eq!(rig.taken(), model.written, "seed {seed}: {script:?}");
        assert_eq!(rig.held(), (false, 0, 0), "seed {seed}: pending is empty");
        assert_eq!(rig.in_flight(), 0, "seed {seed}");
        for (id, ticket) in tickets.iter().enumerate() {
            assert_eq!(
                model.op_written[id], model.op_len[id],
                "seed {seed}: op {id}"
            );
            let settled = rig.settles(*ticket).await;
            let want = if model.op_len[id] == 0 {
                Settlement::Written { bytes: 0 }
            } else {
                Settlement::Written {
                    bytes: model.op_len[id],
                }
            };
            assert_eq!(settled, [want], "seed {seed}: op {id}, {script:?}");
        }
        assert_eq!(
            rig.page().last().copied().unwrap_or(0),
            model.natives,
            "seed {seed}: {script:?}"
        );
    }
}

// ------------------------------------------------------------------------
// inbox/probe/019: with no probe, nothing changes.

/// Scenario: With no liability and no check, write every Ctrl+G candidate form, text around them and a paste holding them, through the gate, the blocking `Write` path and a voice answer. Nothing is held or cut: the PTY gets each op's bytes as one write, in order, with job boundaries identical to the input, and every write returns its full count.
#[spec("inbox/probe/019")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_019_with_no_probe_the_planner_never_cuts() {
    let mut rig = Rig::new(true, None);
    let mut frames: Vec<Vec<u8>> = FORMS.iter().map(|f| f.to_vec()).collect();
    frames.push(b"text\x07more\x1b[103;5utail".to_vec());
    frames.push(b"\x1b[200~\x07\x1b[71;5u\x1b[201~".to_vec());
    for frame in &frames {
        let (accepted, _) = rig.offer(frame).await;
        assert!(matches!(accepted, Accepted::Written(Ok(()))), "{frame:?}");
    }
    assert_eq!(rig.chunks(), frames, "one write per frame, unchanged");
    assert_eq!(rig.held(), (false, 0, 0));
    use std::io::Write as _;
    for frame in &frames {
        assert_eq!(rig.writer.write(frame).unwrap(), frame.len());
    }
    assert_eq!(rig.chunks(), [frames.clone(), frames.clone()].concat());
}

// ------------------------------------------------------------------------
// inbox/probe/075: a job's start condition, and jobs dropped at dequeue.

#[derive(Debug, PartialEq, Eq)]
struct Mine(&'static str);

fn ctx(boundary: Boundary, retired: bool) -> StartCtx {
    StartCtx {
        boundary,
        retired,
        now: Instant::now(),
    }
}

/// Scenario: Ask a queued claim to start under a caller-defined reason type. A refusal returns that reason unchanged and leaves the claim refused, never also withdrawn or started; an acceptance starts it; a claim taken back first is not queued, and its condition is never asked.
#[spec("inbox/probe/075")]
#[test]
fn probe_075_start_if_passes_a_callers_reason_through_and_refuses_once() {
    let claim = JobClaim::default();
    assert_eq!(
        claim.start_if(&ctx(Boundary::Ground, false), |_| Err(Mine("why"))),
        Start::Refused(Mine("why"))
    );
    assert_eq!(claim.state(), ClaimState::Refused);
    assert!(!claim.withdraw(), "a refused job isn't taken back");
    assert!(!claim.start());
    let claim = JobClaim::default();
    assert_eq!(
        claim.start_if(&ctx(Boundary::Ground, false), |c| {
            if c.boundary == Boundary::Ground {
                Ok(())
            } else {
                Err(Mine("off"))
            }
        }),
        Start::Started
    );
    assert_eq!(claim.state(), ClaimState::Started);
    let claim = JobClaim::default();
    assert!(claim.withdraw());
    let mut asked = false;
    assert_eq!(
        claim.start_if(&ctx(Boundary::Ground, false), |_| {
            asked = true;
            Ok::<(), Mine>(())
        }),
        Start::NotQueued
    );
    assert!(!asked, "a withdrawn job's condition isn't asked");
    assert_eq!(claim.state(), ClaimState::Withdrawn);
    // A take-back landing while the condition runs wins.
    let claim = JobClaim::default();
    assert_eq!(
        claim.start_if(&ctx(Boundary::Ground, false), |_| {
            assert!(claim.withdraw());
            Err(Mine("late"))
        }),
        Start::NotQueued
    );
    assert_eq!(claim.state(), ClaimState::Withdrawn);
}

/// Scenario: Write a person's unfinished CSI, then queue a deck line; then finish the CSI and queue another; then retire the generation, as a successor's publication does, and queue a third. The first is refused off ground and the third retired, each writing nothing, waking its submitter and leaving the in-flight count; the second is written.
#[spec("inbox/probe/075")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_075_a_deck_line_starts_only_at_a_boundary_of_a_live_generation() {
    let rig = Rig::new(true, None);
    rig.offer(b"\x1b[1").await;
    assert_eq!(rig.writer.physical_boundary(), Boundary::Inside);
    let mut job = rig
        .writer
        .queue_deck_job(b"LINE".to_vec(), Arc::default())
        .expect("queued");
    assert_eq!(job.done().await, None, "refused jobs never answer");
    assert_eq!(job.refused(), Some(DeckLineRefusal::OffGround));
    assert!(!job.started());
    assert!(!job.reconcile_take_back(), "the writer accounted for it");
    rig.idle().await;
    assert_eq!(rig.in_flight(), 0);
    rig.offer(b"~").await;
    assert_eq!(rig.writer.physical_boundary(), Boundary::Ground);
    let mut job = rig
        .writer
        .queue_deck_job(b"LINE".to_vec(), Arc::default())
        .unwrap();
    assert_eq!(job.done().await, Some((4, true)));
    assert_eq!(job.refused(), None);
    // The Escape key alone: a line may start after it, as before.
    rig.offer(b"\x1b").await;
    assert_eq!(rig.writer.physical_boundary(), Boundary::Escape);
    let mut job = rig
        .writer
        .queue_deck_job(b"ESC".to_vec(), Arc::default())
        .unwrap();
    assert_eq!(job.done().await, Some((3, true)));
    rig.retired.store(true, Ordering::SeqCst);
    let mut job = rig
        .writer
        .queue_deck_job(b"LATE".to_vec(), Arc::default())
        .unwrap();
    assert_eq!(job.done().await, None);
    assert_eq!(job.refused(), Some(DeckLineRefusal::Retired));
    rig.idle().await;
    assert_eq!(rig.taken(), b"\x1b[1~LINE\x1bESC");
    assert_eq!(rig.in_flight(), 0);
}

/// Scenario: Queue a job behind one the shut PTY is blocking, take the queued one back, and also queue a deck line the writer will refuse; then open the PTY. When the writer comes to them, each is dropped at its dequeue: its reply channel closes and nothing else holds its claim.
#[spec("inbox/probe/075")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_075_a_job_the_writer_does_not_start_is_dropped_at_its_dequeue() {
    let rig = Arc::new(Rig::new(false, None));
    let blocker = tokio::spawn({
        let rig = Arc::clone(&rig);
        async move { rig.offer(b"\x1b[").await.0 }
    });
    let entered = Arc::clone(&rig);
    tokio::task::spawn_blocking(move || entered.entered.lock().unwrap().recv().unwrap())
        .await
        .unwrap();
    let withdrawn = Arc::new(JobClaim::default());
    let mut taken_back = rig
        .writer
        .queue_deck_job(b"BACK".to_vec(), Arc::clone(&withdrawn))
        .unwrap();
    assert!(!taken_back.reconcile_take_back());
    assert!(withdrawn.withdraw());
    assert!(taken_back.reconcile_take_back());
    let refused = Arc::new(JobClaim::default());
    let mut off = rig
        .writer
        .queue_deck_job(b"OFF".to_vec(), Arc::clone(&refused))
        .unwrap();
    assert_eq!(
        Arc::strong_count(&withdrawn),
        3,
        "the job, its handle and us"
    );
    rig.open();
    assert!(matches!(blocker.await.unwrap(), Accepted::Written(Ok(()))));
    assert_eq!(taken_back.done().await, None, "closed at its dequeue");
    assert_eq!(off.done().await, None);
    assert_eq!(off.refused(), Some(DeckLineRefusal::OffGround));
    rig.idle().await;
    drop(taken_back);
    drop(off);
    assert_eq!(Arc::strong_count(&withdrawn), 1, "nothing else holds it");
    assert_eq!(Arc::strong_count(&refused), 1);
    assert_eq!(rig.taken(), b"\x1b[");
    assert_eq!(rig.in_flight(), 0);
}

// ------------------------------------------------------------------------
// inbox/probe/076: the ground wake (I-9).

/// Scenario: On the real writer thread, write a person's `ESC [`, start a ground wait, then write `A`. The tracker reads the written bytes: off ground after the first write, and the wait completes once the PTY took the second.
#[spec("inbox/probe/076")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_076_the_writer_thread_drives_the_boundary_and_its_wake() {
    let rig = Arc::new(Rig::new(true, None));
    rig.offer(b"\x1b[").await;
    let physical = rig.writer.gate().physical();
    assert_eq!(physical.boundary(), Boundary::Inside);
    let waiter = tokio::spawn({
        let physical = Arc::clone(&physical);
        async move { physical.until_line().await }
    });
    tokio::task::yield_now().await;
    assert!(!waiter.is_finished());
    rig.offer(b"A").await;
    tokio::time::timeout(Duration::from_secs(8), waiter)
        .await
        .expect("woken")
        .unwrap();
    assert_eq!(rig.writer.physical_boundary(), Boundary::Ground);
}
