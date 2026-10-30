//! PRD #1666 part 3 (Q35-20a): the frontier reader's fail-closed table
//! (amendment 4 §7.2, amendment 5 §3.2, amendment 6 §3), on scripted polls
//! and reads and the real bus and queue (`inbox/probe/062`'s private rows).

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::AtomicUsize;

use super::frontier_reader::{Control, ReaderIo, run};
use super::*;
use crate::composer_probe::frontier::{EventFd, MAX_OUTSTANDING, Request, Retired, Wake};
use crate::composer_probe::master_holders::Holders;
use crate::composer_probe::record::{FrontierOutcome, RecordKey};
use crate::composer_probe::segments::{Marker, ReturnMarkers, ReturnWatch};
use spec::spec;

const GEN: u64 = 7;
const IN: i16 = libc::POLLIN;
const HUP: i16 = libc::POLLHUP;
const ERR: i16 = libc::POLLERR;
const NVAL: i16 = libc::POLLNVAL;

/// A poll and read script. When a script runs out, the master hangs up and
/// reads end the stream, so every run ends.
#[derive(Default)]
struct Script {
    waits: VecDeque<io::Result<(i16, i16)>>,
    zero: VecDeque<io::Result<i16>>,
    reads: VecDeque<io::Result<Vec<u8>>>,
    drains: VecDeque<io::Result<()>>,
    /// What each holder census finds, `NoneFound` once it runs out.
    holders: VecDeque<Holders>,
    /// Each `wait`'s `wake` argument, in order.
    waited: Vec<bool>,
    zero_polls: usize,
    drained: usize,
}

impl Script {
    fn wait(mut self, master: i16, wake: i16) -> Self {
        self.waits.push_back(Ok((master, wake)));
        self
    }
    fn wait_err(mut self, errno: i32) -> Self {
        self.waits
            .push_back(Err(io::Error::from_raw_os_error(errno)));
        self
    }
    fn zero(mut self, revents: i16) -> Self {
        self.zero.push_back(Ok(revents));
        self
    }
    fn zero_err(mut self, errno: i32) -> Self {
        self.zero
            .push_back(Err(io::Error::from_raw_os_error(errno)));
        self
    }
    fn read(mut self, bytes: &[u8]) -> Self {
        self.reads.push_back(Ok(bytes.to_vec()));
        self
    }
    fn read_err(mut self, errno: i32) -> Self {
        self.reads
            .push_back(Err(io::Error::from_raw_os_error(errno)));
        self
    }
    fn drain_err(mut self, errno: i32) -> Self {
        self.drains
            .push_back(Err(io::Error::from_raw_os_error(errno)));
        self
    }
}

impl ReaderIo for Script {
    fn wait(&mut self, wake: bool) -> io::Result<(i16, i16)> {
        self.waited.push(wake);
        assert!(self.waited.len() < 1_000, "the reader must not spin");
        self.waits.pop_front().unwrap_or(Ok((HUP, 0)))
    }
    fn zero_poll(&mut self) -> io::Result<i16> {
        self.zero_polls += 1;
        self.zero.pop_front().unwrap_or(Ok(0))
    }
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.reads.pop_front() {
            None => Ok(0),
            Some(Ok(bytes)) => {
                buf[..bytes.len()].copy_from_slice(&bytes);
                Ok(bytes.len())
            }
            Some(Err(e)) => Err(e),
        }
    }
    fn drain_wake(&mut self) -> io::Result<()> {
        self.drained += 1;
        self.drains.pop_front().unwrap_or(Ok(()))
    }
    fn holders(&mut self) -> Holders {
        self.holders.pop_front().unwrap_or(Holders::NoneFound)
    }
}

/// A wake that counts its rings and fails them with `errno` when set.
#[derive(Default)]
struct CountingWake {
    rings: AtomicUsize,
    fail: Option<i32>,
}

impl Wake for CountingWake {
    fn ring(&self) -> io::Result<()> {
        self.rings.fetch_add(1, Ordering::SeqCst);
        self.fail
            .map_or(Ok(()), |e| Err(io::Error::from_raw_os_error(e)))
    }
}

/// A bus whose queue a reader for [`GEN`] attached, woken by `wake`.
fn bus_with(wake: Arc<CountingWake>) -> Arc<AgentBus> {
    let bus = Arc::new(AgentBus::new());
    bus.input().frontier().attach(GEN, wake);
    bus
}

fn bus() -> Arc<AgentBus> {
    bus_with(Arc::default())
}

fn ask(bus: &AgentBus) -> Arc<Request> {
    bus.input().request_frontier(GEN, None)
}

fn reader(bus: &AgentBus, script: &mut Script) -> ReadEnd {
    run(script, bus, GEN, &mut Control::default(), |_| {})
}

fn taken(offset: u64) -> Option<FrontierOutcome> {
    Some(FrontierOutcome::Taken { offset })
}

const MISSING: Option<FrontierOutcome> = Some(FrontierOutcome::Missing);

/// Scenario: A request is queued and the master and the wake are ready together, with a burst behind the master. The reader reads and pushes the burst first, then drains the wake, and the zero poll takes the frontier after every byte it pushed, publishing the fed count.
#[spec("inbox/probe/062")]
#[test]
fn probe_062_combined_readiness_reads_the_master_before_the_frontier() {
    let bus = bus();
    let request = ask(&bus);
    let mut script = Script::default().wait(IN, IN).read(b"burst-bytes");
    assert_eq!(reader(&bus, &mut script), ReadEnd::Eof);
    assert_eq!(request.outcome(), taken(11));
    assert_eq!(bus.snapshot(), b"burst-bytes");
    assert_eq!(script.zero_polls, 1);
}

/// Scenario: The wake arrives while the master still has bytes, so the zero poll reports POLLIN and takes nothing. The reader reads those bytes on the next turn and the same request is taken after them, with no second wake.
#[spec("inbox/probe/062")]
#[test]
fn probe_062_a_nonempty_zero_poll_retries_after_the_next_read_without_a_wake() {
    let bus = bus();
    let request = ask(&bus);
    let mut script = Script::default()
        .wait(0, IN)
        .zero(IN)
        .wait(IN, 0)
        .read(b"late")
        .zero(0);
    reader(&bus, &mut script);
    assert_eq!(request.outcome(), taken(4));
    assert_eq!(script.drained, 1, "one wake for the request");
    assert_eq!(script.zero_polls, 2);
}

/// Scenario: The zero poll reports POLLHUP alone (rc 1), then POLLERR: neither takes a frontier. The master's hangup is then read as the end of the stream, and the request is missing.
#[spec("inbox/probe/062")]
#[test]
fn probe_062_hup_or_err_on_the_zero_poll_takes_nothing() {
    for revents in [HUP, ERR] {
        let bus = bus();
        let request = ask(&bus);
        let mut script = Script::default().wait(0, IN).zero(revents).wait(revents, 0);
        assert_eq!(reader(&bus, &mut script), ReadEnd::Eof, "{revents:#x}");
        assert_eq!(request.outcome(), MISSING, "{revents:#x}");
    }
}

/// Scenario: The master reports POLLHUP or POLLERR without POLLIN. The reader reads, as it always has: an end of stream or a read error ends it, and a queued request is missing.
#[spec("inbox/probe/062")]
#[test]
fn probe_062_master_hup_and_err_read_to_the_end() {
    let bus = bus();
    let request = ask(&bus);
    let mut script = Script::default().wait(HUP, 0);
    assert_eq!(reader(&bus, &mut script), ReadEnd::Eof);
    assert_eq!(request.outcome(), MISSING);

    let bus = super::frontier_reader_tests::bus();
    let request = ask(&bus);
    let mut script = Script::default().wait(ERR, 0).read_err(libc::EBADF);
    assert!(matches!(reader(&bus, &mut script), ReadEnd::Error(_)));
    assert_eq!(request.outcome(), MISSING);
}

/// Scenario: The master reports POLLNVAL. The stream ends with an error, as a failed read ends it, and the queued request is missing.
#[spec("inbox/probe/062")]
#[test]
fn probe_062_master_nval_ends_the_stream_with_an_error() {
    let bus = bus();
    let request = ask(&bus);
    let mut script = Script::default().wait(NVAL, IN);
    assert_eq!(
        reader(&bus, &mut script),
        ReadEnd::Error(io::Error::from_raw_os_error(libc::EBADF).kind())
    );
    assert_eq!(request.outcome(), MISSING);
    assert_eq!(script.zero_polls, 0);
}

/// Scenario: The reader's poll is interrupted by a signal, then the zero poll is too. Each is retried and decides nothing; the frontier is taken by the zero poll that completes.
#[spec("inbox/probe/062")]
#[test]
fn probe_062_eintr_retries_and_decides_nothing() {
    let bus = bus();
    let request = ask(&bus);
    let mut script = Script::default()
        .wait_err(libc::EINTR)
        .wait(0, IN)
        .zero_err(libc::EINTR)
        .zero(0);
    reader(&bus, &mut script);
    assert_eq!(request.outcome(), taken(0));
    assert_eq!(script.zero_polls, 2);
}

/// Scenario: The reader's poll fails with another error. It reads once, blocking, as the old reader did, and the pending request stays pending through it; a failing zero poll takes nothing either. The request is taken after the read.
#[spec("inbox/probe/062")]
#[test]
fn probe_062_other_poll_errors_read_once_and_keep_the_request() {
    let bus = bus();
    let request = ask(&bus);
    let mut script = Script::default()
        .wait(0, IN)
        .zero_err(libc::ENOMEM)
        .wait_err(libc::ENOMEM)
        .read(b"xy")
        .zero(0);
    reader(&bus, &mut script);
    assert_eq!(request.outcome(), taken(2));
}

/// Scenario: A request names another generation than the reader's, and one is made to a queue no reader attached. Both are missing at once, without ringing a wake.
#[spec("inbox/probe/062")]
#[test]
fn probe_062_stale_generations_and_unattached_queues_are_missing() {
    let wake = Arc::new(CountingWake::default());
    let bus = bus_with(wake.clone());
    let stale = bus.input().request_frontier(GEN + 1, None);
    assert_eq!(stale.outcome(), MISSING);
    assert_eq!(wake.rings.load(Ordering::SeqCst), 0);

    let unattached = AgentBus::new();
    assert_eq!(
        unattached.input().request_frontier(GEN, None).outcome(),
        MISSING,
        "macOS, Windows and a failed setup take no frontier"
    );
}

/// Scenario: Requests are queued, one already in the reader's pending set and one not yet handed over, when the stream ends. Both are missing, a request made afterwards is missing at once, and the wake isn't rung for it.
#[spec("inbox/probe/062")]
#[test]
fn probe_062_teardown_with_queued_requests_makes_them_missing() {
    let wake = Arc::new(CountingWake::default());
    let bus = bus_with(wake.clone());
    let first = ask(&bus);
    let script_second = Arc::new(Mutex::new(None));
    let mut script = Script::default().wait(0, IN).zero(IN).wait(HUP, 0);
    // The second arrives after the reader's snapshot took the first.
    let pending_bus = bus.clone();
    let slot = script_second.clone();
    let mut control = Control::default();
    let end = {
        let mut io = Hooked {
            inner: &mut script,
            before_wait: Some(Box::new(move |n| {
                if n == 1 {
                    *slot.lock().unwrap() = Some(ask(&pending_bus));
                }
            })),
        };
        run(&mut io, &bus, GEN, &mut control, |_| {})
    };
    assert_eq!(end, ReadEnd::Eof);
    let second = script_second.lock().unwrap().take().unwrap();
    assert_eq!(first.outcome(), MISSING);
    assert_eq!(second.outcome(), MISSING);
    let rings = wake.rings.load(Ordering::SeqCst);
    let later = ask(&bus);
    assert_eq!(later.outcome(), MISSING);
    assert_eq!(wake.rings.load(Ordering::SeqCst), rings);
    assert_eq!(bus.input().frontier().retired(), Some(Retired::Reader));
}

/// A [`ReaderIo`] that runs a hook before each `wait`, with the number of
/// waits before it.
struct Hooked<'a> {
    inner: &'a mut Script,
    before_wait: Option<Box<dyn FnMut(usize) + 'a>>,
}

impl ReaderIo for Hooked<'_> {
    fn wait(&mut self, wake: bool) -> io::Result<(i16, i16)> {
        let n = self.inner.waited.len();
        if let Some(hook) = self.before_wait.as_mut() {
            hook(n);
        }
        self.inner.wait(wake)
    }
    fn zero_poll(&mut self) -> io::Result<i16> {
        self.inner.zero_poll()
    }
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
    fn drain_wake(&mut self) -> io::Result<()> {
        self.inner.drain_wake()
    }
    fn holders(&mut self) -> Holders {
        self.inner.holders()
    }
}

/// Scenario: The wake reports POLLNVAL, then, on another reader, POLLERR, then a drain fails with EBADF. Each retires the wake: the pending request is missing, the reader polls the master alone from then on and keeps pushing output, a later request is missing at once, and the reader ends.
#[spec("inbox/probe/062")]
#[test]
fn probe_062_a_failing_wake_retires_and_output_keeps_flowing() {
    let cases: [fn(Script) -> Script; 3] = [
        |s| s.wait(0, NVAL),
        |s| s.wait(0, ERR | IN),
        |s| s.wait(0, IN).drain_err(libc::EBADF),
    ];
    for (n, case) in cases.into_iter().enumerate() {
        let bus = bus();
        let request = ask(&bus);
        let mut script = case(Script::default()).wait(IN, 0).read(b"still");
        assert_eq!(reader(&bus, &mut script), ReadEnd::Eof, "case {n}");
        assert_eq!(request.outcome(), MISSING, "case {n}");
        assert_eq!(bus.snapshot(), b"still", "case {n}");
        assert!(script.waited[0], "case {n}");
        assert!(
            script.waited[1..].iter().all(|w| !w),
            "case {n}: the wake left the poll set: {:?}",
            script.waited
        );
        assert_eq!(
            script.zero_polls, 0,
            "case {n}: no frontier from a retired wake"
        );
        assert_eq!(
            bus.input().frontier().retired(),
            Some(Retired::Wake),
            "the first cause stays"
        );
        assert_eq!(ask(&bus).outcome(), MISSING, "case {n}");
    }
}

/// Scenario: A requester's ring fails with an error other than EAGAIN. The wake is retired: that request and every outstanding one are missing, and a later request is missing at once.
#[spec("inbox/probe/062")]
#[test]
fn probe_062_a_failed_ring_retires_the_wake() {
    let failing = Arc::new(CountingWake {
        fail: Some(libc::EBADF),
        ..Default::default()
    });
    let bus = bus_with(failing.clone());
    let queued = ask(&bus);
    assert_eq!(queued.outcome(), MISSING);
    assert_eq!(bus.input().frontier().retired(), Some(Retired::Wake));
    assert_eq!(ask(&bus).outcome(), MISSING, "missing at once, not rung");
    assert_eq!(failing.rings.load(Ordering::SeqCst), 1);
    assert!(bus.input().frontier().take_queued().is_empty());
}

/// Scenario: A real eventfd is filled to its maximum count, so the next ring gets EAGAIN, which counts as rung; draining it empties it, and draining an empty one (EAGAIN) counts as drained.
#[spec("inbox/probe/062")]
#[test]
fn probe_062_eventfd_eagain_is_coalesced() {
    use std::os::fd::AsRawFd;
    let wake = EventFd::new().unwrap();
    let full = u64::MAX - 1;
    // SAFETY: an 8-byte write from a `u64` we own to the eventfd we own.
    let n = unsafe {
        libc::write(
            wake.as_raw_fd(),
            std::ptr::from_ref(&full).cast(),
            std::mem::size_of::<u64>(),
        )
    };
    assert_eq!(n, 8);
    wake.ring().expect("EAGAIN is a pending wake");
    wake.drain().unwrap();
    wake.drain().expect("nothing pending is drained");
    wake.ring().unwrap();
    wake.drain().unwrap();
}

/// Scenario: The output count starts two bytes below its bound and a three-byte push overflows it. The frontier is missing, and it stays missing for a later request after more output, rather than restarting the count.
#[spec("inbox/probe/062")]
#[test]
fn probe_062_an_exhausted_count_makes_every_frontier_missing() {
    let bus = bus();
    bus.state.lock().unwrap().output_hook.set_fed(u64::MAX - 2);
    let first = ask(&bus);
    let later = Arc::new(Mutex::new(None));
    let publications = Arc::new(Mutex::new(Vec::new()));
    let mut control = Control {
        published: Some(Box::new({
            let publications = publications.clone();
            move |settled: &[Arc<Request>]| {
                publications
                    .lock()
                    .unwrap()
                    .extend(settled.iter().map(|r| (r.id(), r.outcome())));
            }
        })),
        ..Control::default()
    };
    // The overflow, a later push, then a later request, woken on its own.
    let mut script = Script::default()
        .wait(IN, IN)
        .read(b"abc")
        .wait(IN, 0)
        .read(b"d")
        .wait(0, IN);
    let slot = later.clone();
    let asking = bus.clone();
    let mut io = Hooked {
        inner: &mut script,
        before_wait: Some(Box::new(move |n| {
            if n == 2 {
                *slot.lock().unwrap() = Some(ask(&asking));
            }
        })),
    };
    run(&mut io, &bus, GEN, &mut control, |_| {});
    let later = later.lock().unwrap().take().unwrap();
    assert_eq!(
        *publications.lock().unwrap(),
        [(first.id(), MISSING), (later.id(), MISSING)],
        "each publication itself answered missing"
    );
    assert_eq!(bus.state.lock().unwrap().output_hook.fed(), None, "sticky");
}

/// Scenario: Pause the reader between the zero poll's rc 0 and the publication, and retire the wake there (A6). The retirement's Missing is the request's one outcome, the publication sets nothing, and exactly one settlement happens. In the other order, the publication's Taken stands and the retirement after it settles nothing.
#[spec("inbox/probe/062")]
#[test]
fn probe_062_retirement_and_publication_settle_once_in_either_order() {
    // Retirement first.
    let bus = bus();
    let request = ask(&bus);
    let retired = Arc::new(Mutex::new(Vec::new()));
    let published = Arc::new(Mutex::new(None));
    let mut control = Control {
        after_zero: Some(Box::new({
            let bus = bus.clone();
            let retired = retired.clone();
            move || {
                *retired.lock().unwrap() = bus.input().frontier().retire(Retired::Wake);
            }
        })),
        published: Some(Box::new({
            let published = published.clone();
            move |settled: &[Arc<Request>]| {
                *published.lock().unwrap() = Some(settled.len());
            }
        })),
        ..Control::default()
    };
    let mut script = Script::default().wait(0, IN).zero(0);
    run(&mut script, &bus, GEN, &mut control, |_| {});
    assert_eq!(request.outcome(), MISSING);
    assert_eq!(
        retired.lock().unwrap().len(),
        1,
        "the retirement settled it"
    );
    assert_eq!(
        *published.lock().unwrap(),
        Some(0),
        "the losing publisher settles nothing"
    );

    // Publication first.
    let bus = super::frontier_reader_tests::bus();
    let request = ask(&bus);
    let late = Arc::new(Mutex::new(None));
    let mut control = Control {
        after_zero: None,
        published: Some(Box::new({
            let bus = bus.clone();
            let late = late.clone();
            move |settled: &[Arc<Request>]| {
                assert_eq!(settled.len(), 1);
                *late.lock().unwrap() = Some(bus.input().frontier().retire(Retired::Wake).len());
            }
        })),
        ..Control::default()
    };
    let mut script = Script::default().wait(0, IN).zero(0);
    run(&mut script, &bus, GEN, &mut control, |_| {});
    assert_eq!(request.outcome(), taken(0));
    assert_eq!(
        *late.lock().unwrap(),
        Some(0),
        "the losing retirement settles nothing"
    );
}

/// Scenario: When a publication's requests are handed to be settled, the bus lock is already released, so a record's lock is never taken under it (A5 §3.2's lock order).
#[spec("inbox/probe/062")]
#[test]
fn probe_062_settlement_runs_after_the_bus_lock_is_released() {
    let bus = bus();
    let request = ask(&bus);
    let free = Arc::new(Mutex::new(None));
    let mut control = Control {
        after_zero: None,
        published: Some(Box::new({
            let bus = bus.clone();
            let free = free.clone();
            move |_: &[Arc<Request>]| {
                *free.lock().unwrap() = Some(bus.state.try_lock().is_ok());
            }
        })),
        ..Control::default()
    };
    let mut script = Script::default().wait(IN, IN).read(b"x");
    run(&mut script, &bus, GEN, &mut control, |_| {});
    assert_eq!(request.outcome(), taken(1));
    assert_eq!(*free.lock().unwrap(), Some(true));
}

/// Scenario: Fill a generation's queue to its cap of unsettled requests: the next is missing at once. A record asking twice gets its one request back, and nothing more is queued.
#[spec("inbox/probe/062")]
#[test]
fn probe_062_the_queue_is_bounded_and_one_request_per_record() {
    let bus = bus();
    let key = RecordKey { pid: 42, start: 9 };
    let first = bus.input().request_frontier(GEN, Some(key));
    let again = bus.input().request_frontier(GEN, Some(key));
    assert_eq!(first.id(), again.id(), "a duplicate is ignored");
    let held: Vec<_> = (1..MAX_OUTSTANDING).map(|_| ask(&bus)).collect();
    assert!(held.iter().all(|r| r.outcome().is_none()));
    assert_eq!(ask(&bus).outcome(), MISSING, "past the cap");
    assert_eq!(bus.input().frontier().take_queued().len(), MAX_OUTSTANDING);
}

/// Scenario: Push output in which a `?2004h` starts before the frontier and ends after it, take the frontier between the two pushes, then feed the same bytes to the return-marker recogniser. Its count agrees with the frontier, and the straddling sequence doesn't qualify while one that starts at the frontier does.
#[spec("inbox/probe/062")]
#[test]
fn probe_062_a_sequence_started_before_the_frontier_does_not_qualify() {
    let bus = bus();
    let before: &[u8] = b"head\x1b[?20";
    let after: &[u8] = b"04h\x1b[?2004h";
    let request = ask(&bus);
    let mut script = Script::default()
        .wait(IN, IN)
        .read(before)
        .wait(IN, 0)
        .read(after);
    reader(&bus, &mut script);
    let Some(FrontierOutcome::Taken { offset }) = request.outcome() else {
        panic!("{:?}", request.outcome());
    };
    assert_eq!(offset, before.len() as u64);
    let mut markers = ReturnMarkers::default();
    let mut seen = Vec::new();
    markers.feed(before, &mut seen);
    assert_eq!(
        markers.fed(),
        offset,
        "one count for the frontier and the markers"
    );
    markers.feed(after, &mut seen);
    let mut watch = ReturnWatch::ec(offset);
    let starts: Vec<_> = seen.iter().map(|s| (s.marker, s.start)).collect();
    assert_eq!(
        starts,
        [(Marker::PasteOn, 4), (Marker::PasteOn, offset + 3)],
        "the first starts before o"
    );
    watch.feed(seen[0]);
    assert!(!watch.enabled(), "a sequence begun before o");
    watch.feed(seen[1]);
    assert!(watch.enabled(), "a sequence begun at or after o");
}

/// Scenario: An installed generation's seam records a clean editor report, which asks the generation's reader for a drain frontier instead of recording it missing. The reader pushes the pending output and takes the frontier after it, and the record learns `Taken` at that offset once the bus lock is released.
#[spec("inbox/probe/062")]
#[test]
fn probe_062_a_clean_report_learns_the_readers_frontier() {
    use crate::composer_probe::Permission;
    use crate::composer_probe::observe::{Evidence, Page, Seam};
    use crate::composer_probe::record::{Attribution, EcPolicy, Frontier, Records};
    use crate::event::{
        ComposerClaim, ComposerDecision, ComposerInvocationRequest, EditorReport, EditorTree,
    };

    #[derive(Debug, Default)]
    struct Nowhere;
    impl Page for Nowhere {
        fn publish(&mut self, _count: u32) {}
    }

    let bus = bus();
    let now = std::time::Instant::now();
    assert!(
        bus.input()
            .attach_records(Records::new(GEN, EcPolicy::Hold, now))
    );
    let mut seam = Seam::new(GEN, Arc::clone(bus.input()), Nowhere);
    seam.set_permission(Permission::Armable);
    let key = RecordKey {
        pid: 5,
        start: 9_005,
    };
    seam.invocation(
        &ComposerInvocationRequest {
            pane_id: "p".into(),
            token: None,
            generation: GEN,
            decision: ComposerDecision::Unarmed,
            claim: Some(ComposerClaim::Owned),
            seg_seen: Some(0),
            native_seen: Some(0),
            argv: vec!["/tmp/claude-prompt-1.md".into()],
        },
        Some(Evidence {
            key,
            birth_floor: now,
            attribution: Attribution::Owned,
            read: None,
        }),
        now,
    );
    seam.report(
        key,
        EditorReport::EditorDone {
            tree: EditorTree::Clean,
        },
        now,
    );
    assert_eq!(
        bus.input().record(key).unwrap().frontier,
        Frontier::Requested
    );
    let mut script = Script::default().wait(IN, IN).read(b"after-edit");
    reader(&bus, &mut script);
    assert_eq!(
        bus.input().record(key).unwrap().frontier,
        Frontier::Taken { offset: 10 }
    );
}

/// Scenario: The zero poll finds the master empty, but the holder scan finds another process holding the master, then, on another request, one it couldn't read. Neither request is taken: each is missing, and the output keeps flowing.
#[spec("inbox/probe/062")]
#[test]
fn probe_062_another_holder_of_the_master_makes_the_frontier_missing() {
    for found in [
        Holders::Other(4242),
        Holders::Unknown {
            pid: 4243,
            step: "fd",
            errno: Some(libc::EACCES),
        },
    ] {
        let bus = bus();
        let request = ask(&bus);
        let mut script = Script::default()
            .wait(IN, IN)
            .read(b"out")
            .wait(IN, 0)
            .read(b"more");
        script.holders.push_back(found.clone());
        let published = Arc::new(Mutex::new(0));
        let mut control = Control {
            published: Some(Box::new({
                let published = published.clone();
                move |_: &[Arc<Request>]| *published.lock().unwrap() += 1
            })),
            ..Control::default()
        };
        run(&mut script, &bus, GEN, &mut control, |_| {});
        assert_eq!(request.outcome(), MISSING, "{found:?}");
        assert_eq!(
            *published.lock().unwrap(),
            0,
            "{found:?}: nothing published"
        );
        assert_eq!(bus.snapshot(), b"outmore", "{found:?}");
    }
}
