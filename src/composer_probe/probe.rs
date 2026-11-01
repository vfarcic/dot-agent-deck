//! PRD #1666 part 3 (Q35-19; R5 §4.2, R2 §6.7–§6.8, A4 §4, A5 §1.5): the
//! probe job's start on the writer thread ([`start`]) and when the deck tries
//! a probe again ([`Schedule`]). Pure: Q35-20 wires both into the tick and
//! the pane writer.
//!
//! **The start.** A probe's `0x07` is queued as a job with a claim
//! (`crate::doorbell_txn::JobClaim`). When the writer thread comes to it, it
//! asks the job's start condition through `JobClaim::start_if`, and the
//! condition is the arm section: admission asked again, then the liability
//! and `seg_seq` published (`super::observe::Seam::arm`), with anything
//! Q35-20 adds to the section (the fence) after that `Ok`. So the arm word
//! moves only for a job the writer is about to write, and a refusal writes
//! no byte, moves no arm word and closes no fence: the job ends refused.
//!
//! **The take-back (R5 §4.2).** A person's op accepted while the probe's job
//! is still queued takes it back by the claim's compare-and-swap. Before the
//! section runs, that leaves nothing to undo: nothing was published. A
//! withdrawal that wins while the section runs (the claim's swap from
//! queued fails after the section published the arm) gets the section's
//! work undone by `disarm`, so the liability leaves with the job. A
//! condition the claim doesn't ask (taken back or started already) arms
//! nothing.
//!
//! **The schedule (R2 §6.8).** The first attempt is the caller's to make when
//! it wants one; after a read that found the box non-empty, the next waits
//! until the epoch moves or [`NON_EMPTY_RECHECK`] passes; after a failure
//! (a refusal, an uncertain or lapsed check) the next waits [`BACKOFF`]'s
//! step, then every [`BACKOFF_LAST`]. After [`BROKEN_AFTER`] failures in a
//! row the generation's observer is broken, which its caller shows and logs;
//! the waits go on as above, and a person's op makes one fresh attempt due
//! at once, so a repaired daemon or a transient failure recovers. R2 §6.8's
//! rule that three failures make the observer broken until a person retries
//! is superseded by R5's continuing retries (R5's reconciliation table), and
//! the waits here continue. The `broken` bit is kept only as presentation:
//! shown and logged, read by no wait and by no admission conjunct. Q35-20
//! must not feed it back as an "observer isn't broken" admission condition,
//! which would stop the continuing retries at the third failure. A due
//! attempt is still only an attempt: admission decides. Arithmetic past
//! `Instant`'s range leaves the attempt undue, failing toward fewer probes.

use std::time::{Duration, Instant};

use crate::doorbell_txn::{JobClaim, Start, StartCtx};

/// The writer thread's start of a probe's job: `arm` is the section, asked
/// only while the job is queued; `disarm` undoes it when a withdrawal won
/// the claim after `arm` succeeded. See the module documentation.
pub fn start<R>(
    claim: &JobClaim,
    ctx: &StartCtx,
    arm: impl FnOnce(&StartCtx) -> Result<(), R>,
    disarm: impl FnOnce(),
) -> Start<R> {
    let mut armed = false;
    let started = claim.start_if(ctx, |ctx| {
        arm(ctx)?;
        armed = true;
        Ok(())
    });
    if armed && matches!(started, Start::NotQueued) {
        disarm();
    }
    started
}

/// After a read that found the box non-empty, the next attempt waits for the
/// epoch to move or for this long with mail waiting.
pub const NON_EMPTY_RECHECK: Duration = Duration::from_secs(30 * 60);

/// The waits after the first failures in a row.
pub const BACKOFF: [Duration; 3] = [
    Duration::from_secs(30),
    Duration::from_secs(2 * 60),
    Duration::from_secs(10 * 60),
];

/// The wait after every later failure.
pub const BACKOFF_LAST: Duration = Duration::from_secs(30 * 60);

/// Failures in a row after which the observer is broken.
pub const BROKEN_AFTER: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wait {
    /// Due now.
    None,
    /// Not before this instant.
    Until(Instant),
    /// Until the epoch moves from this value, or this instant.
    Epoch { from: u64, until: Instant },
    /// Never by itself: an instant past the clock's range.
    Never,
}

/// One generation's probe schedule. See the module documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schedule {
    wait: Wait,
    failures: u32,
    broken: bool,
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            wait: Wait::None,
            failures: 0,
            broken: false,
        }
    }
}

impl Schedule {
    /// Whether an attempt is due at `now` with the epoch at `epoch`.
    pub fn due(&self, now: Instant, epoch: u64) -> bool {
        match self.wait {
            Wait::None => true,
            Wait::Until(at) => now >= at,
            Wait::Epoch { from, until } => epoch != from || now >= until,
            Wait::Never => false,
        }
    }

    /// Whether the observer is broken.
    pub fn broken(&self) -> bool {
        self.broken
    }

    /// The failures in a row so far.
    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// An attempt failed at `now`: refused, uncertain, or lapsed.
    pub fn failed(&mut self, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        if self.failures >= BROKEN_AFTER {
            self.broken = true;
        }
        let step = usize::try_from(self.failures - 1)
            .ok()
            .and_then(|i| BACKOFF.get(i))
            .copied()
            .unwrap_or(BACKOFF_LAST);
        self.wait = now.checked_add(step).map_or(Wait::Never, Wait::Until);
    }

    /// A probe read the box non-empty at `now`, with the epoch at `epoch`.
    pub fn non_empty(&mut self, now: Instant, epoch: u64) {
        self.failures = 0;
        self.broken = false;
        self.wait = now
            .checked_add(NON_EMPTY_RECHECK)
            .map_or(Wait::Never, |until| Wait::Epoch { from: epoch, until });
    }

    /// A probe read the box empty: the next attempt is the caller's to want.
    pub fn empty(&mut self) {
        *self = Self::default();
    }

    /// The test and `e2e` builds' clock offset (Q105b.2): the pending wait
    /// ends `by` earlier, as if `by` had passed. Nothing else moves: the
    /// failures, `broken` and an epoch wait's epoch stay as they are.
    #[cfg(any(test, feature = "e2e"))]
    pub fn advance(&mut self, by: std::time::Duration) {
        let earlier = |at: Instant| at.checked_sub(by).unwrap_or_else(Instant::now);
        self.wait = match self.wait {
            Wait::Until(at) => Wait::Until(earlier(at)),
            Wait::Epoch { from, until } => Wait::Epoch {
                from,
                until: earlier(until),
            },
            other => other,
        };
    }

    /// A person's op: a broken observer gets one fresh attempt, due at once.
    /// It stays broken until an attempt reads the box.
    pub fn person(&mut self) {
        if self.broken {
            self.wait = Wait::None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composer_probe::Permission;
    use crate::composer_probe::admission::{Attempt, Fence, Outside, Refusal};
    use crate::composer_probe::epoch::GenerationInput;
    use crate::composer_probe::gate::Gate;
    use crate::composer_probe::observe::{ControlOp, Page, Seam};
    use crate::composer_probe::planner::Boundary;
    use crate::composer_probe::query_ledger::CprWaitEnd;
    use crate::composer_probe::record::{
        Attribution, Channel, Decision, EcPolicy, Hold, NotOwned, Observation, RecordKey, Records,
        ScopeState,
    };
    use spec::spec;
    use std::sync::{Arc, Mutex};

    const G: u64 = 7;

    /// A page that keeps every publication, as `(count, seg_seq)`; a plain
    /// count publication keeps the segment number it last showed.
    #[derive(Debug, Default, Clone)]
    struct Book(Arc<Mutex<Vec<(u32, u64)>>>);

    impl Book {
        fn seen(&self) -> Vec<(u32, u64)> {
            self.0.lock().unwrap().clone()
        }
    }

    impl Page for Book {
        fn publish(&mut self, count: u32) {
            let mut book = self.0.lock().unwrap();
            let seg = book.last().map_or(0, |&(_, seg)| seg);
            book.push((count, seg));
        }

        fn publish_probe(&mut self, count: u32, seg_seq: u64) {
            self.0.lock().unwrap().push((count, seg_seq));
        }
    }

    fn fresh() -> (Seam<Book>, Book) {
        let input = Arc::new(GenerationInput::new(CprWaitEnd::Counted));
        assert!(input.attach_records(Records::new(G, EcPolicy::Hold, Instant::now())));
        let book = Book::default();
        let mut seam = Seam::new(G, input, book.clone());
        seam.set_permission(Permission::Armable);
        (seam, book)
    }

    fn outside() -> Outside {
        Outside {
            attempt: Attempt::First,
            // The seam's own permission replaces this.
            permission: Permission::Pending,
            fence: Fence::Open,
            human_ticket: false,
            boundary: Boundary::Ground,
            keymap: Ok(()),
            surface: true,
        }
    }

    fn ctx(now: Instant) -> StartCtx {
        StartCtx {
            boundary: Boundary::Ground,
            retired: false,
            now,
        }
    }

    /// The writer thread's outside facts for the job: its own boundary.
    fn at_writer(ctx: &StartCtx) -> Outside {
        Outside {
            boundary: ctx.boundary,
            ..outside()
        }
    }

    fn tool_helper(seam: &mut Seam<Book>, now: Instant) {
        seam.observe(
            &Observation {
                generation: G,
                key: RecordKey { pid: 41, start: 1 },
                channel: Channel::Ipc,
                attribution: Attribution::NotOwned(NotOwned::Tool),
                claim: None,
                decision: Some(Decision::Tool),
                seg_seen: None,
                native_seen: None,
                birth_floor: now,
            },
            now,
        )
        .unwrap();
    }

    /// Scenario: Queue a probe's job and let the writer thread come to it with every conjunct met: the section asks admission again, publishes count 1 with the probe's segment number in one publication and records the raise, and the job starts. A second probe with segment number 2 raises the count to 2, and a third is refused at the cap, publishing nothing.
    #[spec("inbox/probe/078")]
    #[test]
    fn probe_078_an_admitted_probe_publishes_its_liability_and_number_then_starts() {
        let now = Instant::now();
        let (mut seam, book) = fresh();
        assert_eq!(seam.admits(&outside(), now), Ok(()));
        let claim = JobClaim::default();
        let started = start(
            &claim,
            &ctx(now),
            |ctx| seam.arm(1, &at_writer(ctx), ctx.now).map(drop),
            || unreachable!("nothing withdrew it"),
        );
        assert_eq!(started, Start::Started);
        assert_eq!(book.seen(), [(1, 1)]);
        assert_eq!(seam.input().liability_count(), Some(1));

        assert_eq!(seam.arm(2, &outside(), now), Ok(2));
        assert_eq!(book.seen(), [(1, 1), (2, 2)]);
        assert_eq!(
            seam.arm(3, &outside(), now),
            Err(Refusal::LiabilityCap),
            "never a third"
        );
        assert_eq!(book.seen(), [(1, 1), (2, 2)]);
        assert_eq!(seam.input().liability_count(), Some(2));
    }

    /// Scenario: Admit a probe when it is scheduled, then, before the writer thread comes to its job, let a tool's helper open the writer scope; the writer comes to the job with a fence gate composed after the arm. The job is refused for the scope, no arm word is published, the count stays 0, no fence closes and the writer writes nothing; the same holds for a job that finds the stream off ground at the writer after a ground check when scheduled.
    #[spec("inbox/probe/078")]
    #[test]
    fn probe_078_the_writer_thread_revalidates_and_a_refusal_writes_nothing() {
        let now = Instant::now();
        let gate: Gate<()> = Gate::default();
        let (mut seam, book) = fresh();
        assert_eq!(seam.admits(&outside(), now), Ok(()), "scheduled");
        tool_helper(&mut seam, now);
        let claim = JobClaim::default();
        let started = start(
            &claim,
            &ctx(now),
            |ctx| {
                seam.arm(1, &at_writer(ctx), ctx.now)?;
                gate.fence(0);
                Ok(())
            },
            || unreachable!(),
        );
        let scope = seam.input().writer_scope().unwrap().state();
        assert!(matches!(scope, ScopeState::Open(_)));
        assert_eq!(
            started,
            Start::Refused(Refusal::Records(Hold::Scope(scope)))
        );
        assert_eq!(
            claim.state(),
            crate::doorbell_txn::ClaimState::Refused,
            "the writer drops it unwritten"
        );
        assert!(book.seen().is_empty(), "no arm word");
        assert_eq!(seam.input().liability_count(), Some(0));
        assert!(!gate.status().fenced, "no fence");

        let (mut seam, book) = fresh();
        assert_eq!(seam.admits(&outside(), now), Ok(()), "scheduled at ground");
        let claim = JobClaim::default();
        let escape = StartCtx {
            boundary: Boundary::Escape,
            ..ctx(now)
        };
        let started = start(
            &claim,
            &escape,
            |ctx| seam.arm(1, &at_writer(ctx), ctx.now).map(drop),
            || unreachable!(),
        );
        assert_eq!(
            started,
            Start::Refused(Refusal::OffGround(Boundary::Escape))
        );
        assert!(book.seen().is_empty());
        assert_eq!(seam.input().liability_count(), Some(0));
    }

    /// Scenario: Take a probe's job back before the writer comes to it; then race a take-back against the section, landing after the section published the arm and before the claim's swap. The first never asks the section and publishes nothing; the second loses the job, and the liability leaves with it: the count returns to 0 and the page shows 0 again with the probe's number.
    #[spec("inbox/probe/078")]
    #[test]
    fn probe_078_a_take_back_takes_the_liability_with_the_job() {
        let now = Instant::now();
        let (mut seam, book) = fresh();
        let claim = JobClaim::default();
        assert!(claim.withdraw());
        let started = start(
            &claim,
            &ctx(now),
            |_| -> Result<(), Refusal> { unreachable!("not asked once taken back") },
            || unreachable!(),
        );
        assert_eq!(started, Start::NotQueued);
        assert!(book.seen().is_empty());

        let claim = JobClaim::default();
        let seam = Mutex::new(&mut seam);
        let mut disarmed = None;
        let started = start(
            &claim,
            &ctx(now),
            |ctx| -> Result<(), Refusal> {
                seam.lock().unwrap().arm(5, &at_writer(ctx), ctx.now)?;
                assert!(claim.withdraw(), "the take-back lands mid-section");
                Ok(())
            },
            || disarmed = Some(seam.lock().unwrap().disarm()),
        );
        assert_eq!(started, Start::NotQueued);
        assert_eq!(disarmed, Some(Ok(0)));
        assert_eq!(book.seen(), [(1, 5), (0, 5)]);
        assert_eq!(seam.lock().unwrap().input().liability_count(), Some(0));
    }

    /// Scenario: Read the crate's production sources. The records' raise is recorded in exactly one place, the input's locked raise, which only the arm and the `e2e` control verb's raise reach; the arm asks admission first in the same hold of the input lock, and the raise without a publication has no production caller. Admission's predicate is called only by the generation's locked read of it, which the admission check and the arm share.
    #[spec("inbox/probe/078")]
    #[test]
    fn probe_078_the_arm_increment_has_one_call_site() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        let mut dirs = vec![src];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    dirs.push(path);
                } else if path.extension().is_some_and(|e| e == "rs")
                    && !path.to_string_lossy().ends_with("_tests.rs")
                {
                    files.push(path);
                }
            }
        }
        // (file, line) of each production line naming `needle`, a module's
        // `#[cfg(test)] mod tests` and everything after it left out.
        let sites = |needle: &str| -> Vec<(String, String)> {
            let mut found = Vec::new();
            for path in &files {
                let text = std::fs::read_to_string(path).unwrap();
                let production = text.split("#[cfg(test)]\nmod tests").next().unwrap();
                for line in production.lines() {
                    let code = line.trim_start();
                    if code.starts_with("//") {
                        continue;
                    }
                    if code.contains(needle) {
                        let name = path.file_name().unwrap().to_string_lossy().into_owned();
                        found.push((name, code.to_owned()));
                    }
                }
            }
            found.sort();
            found
        };
        let names = |found: Vec<(String, String)>| -> Vec<String> {
            found.into_iter().map(|(file, _)| file).collect()
        };

        assert_eq!(
            names(sites(".liability_raise(")),
            Vec::<String>::new(),
            "the raise without a publication has no production caller"
        );
        assert_eq!(
            names(sites("records.raise(")),
            ["epoch.rs"],
            "the records' raise is recorded in one place, the locked raise"
        );
        assert_eq!(
            names(sites("Self::raise_locked(")),
            ["epoch.rs", "epoch.rs"],
            "the locked raise: the arm and the control verb's raise"
        );
        assert_eq!(names(sites(".arm_probe(")), ["observe.rs"]);
        assert_eq!(names(sites(".raise_published(")), ["observe.rs"]);
        assert_eq!(
            names(sites("self.raise(")),
            ["observe.rs"],
            "the seam's raise: the control verb"
        );
        assert_eq!(names(sites("admission::admit(")), ["epoch.rs"]);
        assert_eq!(
            names(sites("Self::admit_locked(")),
            ["epoch.rs", "epoch.rs"],
            "admission: the check alone and the arm"
        );
        assert_eq!(names(sites(".admit_probe(")), ["observe.rs"]);

        let read = |file: &str| {
            std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("src/composer_probe")
                    .join(file),
            )
            .unwrap()
        };
        let section = |text: &str, head: &str| -> String {
            text.split(head)
                .nth(1)
                .and_then(|rest| rest.split("\n    }\n").next())
                .unwrap_or_else(|| panic!("{head}"))
                .to_owned()
        };
        let arm = section(&read("epoch.rs"), "pub fn arm_probe(");
        let asked = arm
            .find("Self::admit_locked(")
            .expect("the arm asks admission");
        let raised = arm.find("Self::raise_locked(").expect("the arm raises");
        assert!(asked < raised, "admission is asked before the raise");
        let lock = arm.find("lock_recovering()").expect("one hold of the lock");
        assert!(lock < asked, "in the same hold");
        assert!(
            section(&read("observe.rs"), "pub fn arm(").contains(".arm_probe("),
            "the seam's arm is that section"
        );
        assert!(
            section(&read("observe.rs"), "pub fn control(").contains("self.raise("),
            "the other raise"
        );
    }

    /// Scenario: Publish and withdraw the control verb's test liability after a probe published segment number 9. The count goes 1 and back to 0 through the same raise, and the page keeps showing 9.
    #[spec("inbox/probe/078")]
    #[test]
    fn probe_078_the_control_verb_keeps_the_segment_number() {
        let now = Instant::now();
        let (mut seam, book) = fresh();
        assert_eq!(seam.arm(9, &outside(), now), Ok(1));
        assert_eq!(seam.disarm(), Ok(0));
        assert_eq!(seam.control(G, ControlOp::Publish), Ok(1));
        assert_eq!(seam.control(G, ControlOp::Withdraw), Ok(0));
        assert!(
            book.seen().iter().all(|&(_, seg)| seg == 9),
            "{:?}",
            book.seen()
        );
    }

    /// Scenario: Drive a probe schedule through failures, a non-empty read, a person's op and an empty read. The waits are R2 §6.8's values. Failures wait 30 s, 2 min and 10 min, then 30 min each, and the third in a row marks the observer broken without stopping the waits; a person's op makes a broken observer's next attempt due at once; a non-empty read waits for the epoch to move or 30 minutes; an empty read clears everything; and a wait past the clock's range is never due.
    #[spec("inbox/probe/078")]
    #[test]
    fn probe_078_the_retry_schedule() {
        assert_eq!(
            BACKOFF.map(|d| d.as_secs()),
            [30, 120, 600],
            "R2 §6.8's first three waits"
        );
        assert_eq!(BACKOFF_LAST.as_secs(), 30 * 60);
        assert_eq!(NON_EMPTY_RECHECK.as_secs(), 30 * 60);
        assert_eq!(BROKEN_AFTER, 3);
        let t = Instant::now();
        let mut s = Schedule::default();
        assert!(s.due(t, 0), "the first attempt is the caller's");
        let mut at = t;
        for (n, wait) in [
            BACKOFF[0],
            BACKOFF[1],
            BACKOFF[2],
            BACKOFF_LAST,
            BACKOFF_LAST,
        ]
        .into_iter()
        .enumerate()
        {
            s.failed(at);
            assert!(
                !s.due(at + wait - Duration::from_millis(1), 0),
                "failure {n}"
            );
            assert!(s.due(at + wait, 0), "failure {n}");
            assert_eq!(s.broken(), n + 1 >= BROKEN_AFTER as usize, "failure {n}");
            at += wait;
        }
        s.failed(at);
        assert!(!s.due(at, 0));
        s.person();
        assert!(s.due(at, 0), "one fresh attempt, at once");
        assert!(s.broken(), "still broken until a read");

        s.non_empty(at, 4);
        assert!(!s.broken());
        assert!(!s.due(at + NON_EMPTY_RECHECK - Duration::from_millis(1), 4));
        assert!(s.due(at, 5), "the epoch moved");
        assert!(s.due(at + NON_EMPTY_RECHECK, 4));
        s.person();
        assert!(
            !s.due(at, 4),
            "a person's op hurries only a broken observer"
        );

        s.failed(at);
        s.empty();
        assert_eq!(s, Schedule::default());

        let mut far = Schedule::default();
        let mut end = t;
        while let Some(next) = end.checked_add(Duration::from_secs(1 << 40)) {
            end = next;
        }
        far.failed(end);
        assert!(!far.due(end, 0), "past the clock's range");
    }

    /// Q105b.2: the e2e clock offset moves only the pending wait, `Until`
    /// and `Epoch` alike, by exactly what it is given; the failures,
    /// `broken`, an epoch wait's epoch and the other waits stay as they are.
    #[test]
    fn advance_moves_only_the_pending_wait() {
        let t = Instant::now();
        let ms = Duration::from_millis;

        let mut s = Schedule::default();
        s.failed(t);
        assert!(!s.due(t, 0));
        s.advance(BACKOFF[0] - ms(1));
        assert!(!s.due(t, 0), "a millisecond short of the backoff");
        assert_eq!((s.failures(), s.broken()), (1, false));
        s.advance(ms(1));
        assert!(s.due(t, 0), "the whole backoff");
        assert_eq!(s.wait, Wait::Until(t));

        let mut broken = Schedule::default();
        for _ in 0..BROKEN_AFTER {
            broken.failed(t);
        }
        broken.advance(BACKOFF_LAST);
        assert!(broken.due(t, 0));
        assert_eq!((broken.failures(), broken.broken()), (BROKEN_AFTER, true));

        let mut epoch = Schedule::default();
        epoch.non_empty(t, 5);
        epoch.advance(NON_EMPTY_RECHECK - ms(1));
        assert!(!epoch.due(t, 5), "same epoch, a millisecond short");
        assert!(epoch.due(t, 6), "the epoch moving still ends it");
        epoch.advance(ms(1));
        assert!(epoch.due(t, 5));
        assert_eq!(epoch.wait, Wait::Epoch { from: 5, until: t });

        for wait in [Wait::None, Wait::Never] {
            let mut other = Schedule {
                wait,
                failures: 2,
                broken: false,
            };
            other.advance(BACKOFF_LAST);
            assert_eq!(other.wait, wait, "{wait:?} is no pending instant");
            assert_eq!(other.failures(), 2);
        }

        let mut far = Schedule::default();
        far.failed(t);
        far.advance(Duration::MAX);
        assert!(
            far.due(Instant::now(), 0),
            "an offset past the clock is now"
        );
    }
}
