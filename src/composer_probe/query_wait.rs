//! PRD #1666 part 3 (Q35-18a; A1 §4.1–§4.2, A2 §6, A3 §6.4 of Q35-core):
//! how long an agent may take to redraw by itself after a size change, as its
//! screen profile declares ([`RedrawTiming`]), and one generation's running
//! estimate of when the waits those size changes and its own queries started
//! have run out ([`QueryWaitEstimate`]).
//!
//! The estimate is advice, not exact deduplication: a query observed late
//! can extend an earlier edge's wait, a wait can end early, and continuing
//! activity keeps moving it. For a finite history of size changes and
//! queries followed by quiescence it stays where the last update put it. It
//! ignores frames, Clean ones included: an intermediate frame at the final
//! width isn't proof the remaining waits drained (A1 §4.2). Every addition is
//! checked; an update whose arithmetic overflows leaves no estimate, which a
//! caller reads as due at its next evaluation (A3 §6.4).
//!
//! Who feeds and reads it, as of this commit: the generation's input
//! (`super::epoch::GenerationInput`) keeps it under its leaf lock, fed by the
//! bus at each applied size change and nudge edge and by the output hook's
//! query notes; nothing reads it to decide. O1's nudge (Q35-18b) and O5's
//! admission hint (Q35-19) are its callers, through [`nudge_due`] and
//! [`QueryWaitEstimate::estimate`]. Reading it writes nothing.

use std::time::{Duration, Instant};

use super::query_ledger::QueryKind;
use crate::evidence_state::EvidenceStatus;

/// How long the agent may take to redraw by itself after a size change, and
/// which output queries its own input loop waits on (A1 §4.1).
#[derive(Debug, PartialEq, Eq)]
pub struct RedrawTiming {
    /// The longest one size change is expected to keep the agent from
    /// redrawing, when its query goes unanswered (serial: waits chain). A
    /// budget, not a bound: a wait can end early, and the agent's reader can
    /// delay one.
    pub per_size_change: Duration,
    pub margin: Duration,
    /// The estimate never runs further ahead of the update that moved it
    /// than this.
    pub cap: Duration,
    /// Query kinds whose waits block the agent's input loop.
    pub waits_on: &'static [QueryKind],
}

/// One generation's estimate of when its query waits have run out (A1 §4.2).
/// See the module documentation.
#[derive(Debug, Clone)]
pub struct QueryWaitEstimate {
    /// The profile's timing, once the generation has one.
    timing: Option<&'static RedrawTiming>,
    /// When the waits counted so far end; `None` once an update overflowed,
    /// until the next update that doesn't.
    end: Option<Instant>,
    /// The latest size change applied, profiled or not: what adoption counts
    /// of the history before it (A2 §6).
    last_size_change: Option<Instant>,
}

impl QueryWaitEstimate {
    /// A generation spawned at `spawn`, with its profile's timing if it has
    /// one: nothing to wait for yet.
    pub fn new(timing: Option<&'static RedrawTiming>, spawn: Instant) -> Self {
        Self {
            timing,
            end: Some(spawn),
            last_size_change: None,
        }
    }

    /// The daemon applied a size change to the generation's PTY at `t`: a
    /// dimension change or a nudge edge. Serial waits fold:
    /// `end = min(max(end, t) + per_size_change, t + cap)`. Unprofiled, it is
    /// only recorded.
    pub fn size_change(&mut self, t: Instant) {
        self.last_size_change = Some(self.last_size_change.map_or(t, |last| last.max(t)));
        let Some(timing) = self.timing else {
            return;
        };
        let Some(ceiling) = t.checked_add(timing.cap) else {
            self.end = None;
            return;
        };
        let from = self.end.map_or(t, |end| end.max(t));
        self.end = Some(
            from.checked_add(timing.per_size_change)
                .map_or(ceiling, |end| end.min(ceiling)),
        );
    }

    /// The output asked a query of `kind` at `q`. A kind the profile's input
    /// loop waits on extends the estimate to `q + per_size_change`; any other,
    /// or any query before a profile, changes nothing.
    pub fn query(&mut self, kind: QueryKind, q: Instant) {
        let Some(timing) = self.timing else {
            return;
        };
        if !timing.waits_on.contains(&kind) {
            return;
        }
        self.end = q
            .checked_add(timing.per_size_change)
            .map(|wait| self.end.map_or(wait, |end| end.max(wait)));
    }

    /// An unprofiled generation's profile is learned at `adoption`: the last
    /// size change before it still counts (A2 §6),
    /// `end = min(max(end, last_size_change + per_size_change), adoption + cap)`.
    /// Queries noted before adoption don't. `false`, changing nothing, when
    /// the generation already has a timing.
    pub fn adopt(&mut self, timing: &'static RedrawTiming, adoption: Instant) -> bool {
        if self.timing.is_some() {
            return false;
        }
        self.timing = Some(timing);
        let Some(ceiling) = adoption.checked_add(timing.cap) else {
            self.end = None;
            return true;
        };
        // Unprofiled, nothing moved `end` from the spawn.
        let mut end = self.end.unwrap_or(adoption);
        if let Some(t) = self.last_size_change {
            end = end.max(t.checked_add(timing.per_size_change).unwrap_or(ceiling));
        }
        self.end = Some(end.min(ceiling));
        true
    }

    /// `end + margin`: the instant the generation's counted waits are
    /// expected to have run out. `None` when there's no estimate: before a
    /// profile, or after an update or this sum overflowed. A caller treats
    /// `None` as due now.
    pub fn estimate(&self) -> Option<Instant> {
        self.end?.checked_add(self.timing?.margin)
    }
}

/// O1's timing conjunct, pure (A1 §4.3): whether the evidence-refresh nudge
/// may fire on `evidence` at `now`, given the generation's `estimate`. Only
/// `Dirty` and `Pending` evidence is refreshed by a nudge, and either one
/// waits for the agent's own redraw until `now` reaches the estimate; with
/// no estimate it's due at once. Every row of A1 §4.3's table is that rule:
/// a nudge's own edges move the estimate, and a start-up or a learned
/// profile with no size change since spawn has its estimate at spawn plus
/// the margin. The other conjuncts (idle, not held, once per episode) stay
/// with the doorbell tick. No caller yet (Q35-18b).
pub fn nudge_due(evidence: &EvidenceStatus, estimate: Option<Instant>, now: Instant) -> bool {
    matches!(
        evidence,
        EvidenceStatus::Dirty { .. } | EvidenceStatus::Pending { .. }
    ) && estimate.is_none_or(|at| now >= at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composer_probe::epoch::{GenerationInput, OutputNote};
    use crate::composer_probe::query_ledger::{
        Backlog, CprWaitEnd, EntryView, HazardPop, Pop, QueryKind,
    };
    use crate::composer_probe::record::{EcPolicy, Records};
    use crate::doorbell_screen::{CLAUDE_PROFILE, CODEX_PROFILE};
    use crate::evidence_state::EdgeReason;
    use spec::spec;

    const CODEX: &RedrawTiming = &CODEX_PROFILE.redraw;
    const CLAUDE: &RedrawTiming = &CLAUDE_PROFILE.redraw;

    const fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Scenario: Read the redraw timing each measured profile declares. Codex waits up to 2000 ms per size change on its CPR, with a 500 ms margin and a 120 s cap; Claude Code redraws within 50 ms, waits on no query and caps at 5 s; the e2e stand-in follows Claude Code's values.
    #[spec("inbox/probe/054")]
    #[test]
    fn probe_054_each_profile_declares_its_redraw_timing() {
        assert_eq!(
            *CODEX,
            RedrawTiming {
                per_size_change: ms(2000),
                margin: ms(500),
                cap: Duration::from_secs(120),
                waits_on: &[QueryKind::Cpr],
            }
        );
        assert_eq!(
            *CLAUDE,
            RedrawTiming {
                per_size_change: ms(50),
                margin: Duration::ZERO,
                cap: Duration::from_secs(5),
                waits_on: &[],
            }
        );
        #[cfg(feature = "e2e")]
        assert_eq!(crate::doorbell_screen::STANDIN_PROFILE.redraw, *CLAUDE);
    }

    /// Scenario: On Codex's timing, apply one size change a second after spawn, then separately a burst of five 300 ms apart, and five alternating sizes (80, 79, 80, 79, 80 columns) 100 ms apart. One change ends 2 s after it plus the margin; the waits of a burst chain, ending 2 s per change after the first; each alternating size counts as a change of its own. A fresh generation's estimate is its spawn plus the margin.
    #[spec("inbox/probe/054")]
    #[test]
    fn probe_054_size_changes_chain_their_waits() {
        let t0 = Instant::now();
        let fresh = QueryWaitEstimate::new(Some(CODEX), t0);
        assert_eq!(fresh.estimate(), Some(t0 + ms(500)));

        let mut one = QueryWaitEstimate::new(Some(CODEX), t0);
        one.size_change(t0 + ms(1000));
        assert_eq!(one.estimate(), Some(t0 + ms(1000 + 2000 + 500)));

        let mut burst = QueryWaitEstimate::new(Some(CODEX), t0);
        for i in 0..5 {
            burst.size_change(t0 + ms(300 * i));
        }
        assert_eq!(burst.estimate(), Some(t0 + ms(5 * 2000 + 500)));

        // The estimate counts applied changes, not distinct widths: the
        // daemon applied five, so five waits chain.
        let mut alternating = QueryWaitEstimate::new(Some(CODEX), t0);
        for (i, _cols) in [80u16, 79, 80, 79, 80].into_iter().enumerate() {
            alternating.size_change(t0 + ms(100 * i as u64));
        }
        assert_eq!(alternating.estimate(), Some(t0 + ms(5 * 2000 + 500)));

        // A change after the previous waits ran out starts from itself.
        let mut later = QueryWaitEstimate::new(Some(CODEX), t0);
        later.size_change(t0);
        later.size_change(t0 + ms(5000));
        assert_eq!(later.estimate(), Some(t0 + ms(5000 + 2000 + 500)));
    }

    /// Scenario: On Codex's timing, note CPR queries with no size change, a DA1 and an OSC 11 query, a CPR 10 ms after a size change, and a size change stamped earlier than an already counted one. A CPR alone extends the estimate to 2 s after it, other kinds change nothing, and the CPR after an edge extends that edge's wait by its 10 ms rather than adding a second wait; Claude Code's timing ignores a CPR.
    #[spec("inbox/probe/054")]
    #[test]
    fn probe_054_a_waited_on_query_extends_without_a_second_wait() {
        let t0 = Instant::now();
        let mut queries = QueryWaitEstimate::new(Some(CODEX), t0);
        queries.query(QueryKind::Da1, t0 + ms(100));
        queries.query(QueryKind::OscColour { slot: 11 }, t0 + ms(100));
        assert_eq!(queries.estimate(), Some(t0 + ms(500)), "not waited on");
        queries.query(QueryKind::Cpr, t0 + ms(100));
        assert_eq!(queries.estimate(), Some(t0 + ms(100 + 2000 + 500)));
        queries.query(QueryKind::Cpr, t0 + ms(50));
        assert_eq!(
            queries.estimate(),
            Some(t0 + ms(100 + 2000 + 500)),
            "an earlier query never shortens it"
        );

        let mut after_edge = QueryWaitEstimate::new(Some(CODEX), t0);
        after_edge.size_change(t0 + ms(1000));
        after_edge.query(QueryKind::Cpr, t0 + ms(1010));
        assert_eq!(
            after_edge.estimate(),
            Some(t0 + ms(1010 + 2000 + 500)),
            "advice: extended by the late query, not doubled"
        );

        let mut reordered = QueryWaitEstimate::new(Some(CODEX), t0);
        reordered.size_change(t0 + ms(1000));
        reordered.size_change(t0 + ms(400));
        assert_eq!(reordered.estimate(), Some(t0 + ms(1000 + 4000 + 500)));

        let mut claude = QueryWaitEstimate::new(Some(CLAUDE), t0);
        claude.query(QueryKind::Cpr, t0 + ms(100));
        assert_eq!(claude.estimate(), Some(t0));
        claude.size_change(t0 + ms(100));
        assert_eq!(claude.estimate(), Some(t0 + ms(150)));
    }

    /// Scenario: Apply two hundred size changes 1 ms apart on Codex's timing and on Claude Code's, whose chained waits (400 s, 10 s) run past each cap. Each estimate stops at the last change plus its profile's cap (120 s, 5 s) plus the margin, never further.
    #[spec("inbox/probe/054")]
    #[test]
    fn probe_054_the_cap_bounds_one_update() {
        let t0 = Instant::now();
        for (timing, label) in [(CODEX, "codex"), (CLAUDE, "claude")] {
            let mut e = QueryWaitEstimate::new(Some(timing), t0);
            let mut last = t0;
            for i in 0..200 {
                last = t0 + ms(i);
                e.size_change(last);
            }
            assert_eq!(
                e.estimate(),
                Some(last + timing.cap + timing.margin),
                "{label}"
            );
        }
    }

    /// Scenario: Before any profile, apply size changes and note CPR queries; then learn Codex's profile, either long after a resize, two seconds after one, or with no resize at all, and once more on a generation that already has a timing. Unprofiled there's no estimate and only the last size change is kept; adoption counts that change's wait (capped from the adoption), not the queries, and a generation with no size change starts at its spawn plus the margin; a second adoption changes nothing.
    #[spec("inbox/probe/054")]
    #[test]
    fn probe_054_adoption_counts_the_last_size_change_before_it() {
        let t0 = Instant::now();
        let mut e = QueryWaitEstimate::new(None, t0);
        e.size_change(t0 + ms(1000));
        e.size_change(t0 + ms(1200));
        e.query(QueryKind::Cpr, t0 + ms(1300));
        assert_eq!(e.estimate(), None, "no timing, no estimate");
        assert!(e.adopt(CODEX, t0 + ms(3000)));
        assert_eq!(e.estimate(), Some(t0 + ms(1200 + 2000 + 500)));
        assert!(!e.adopt(CLAUDE, t0 + ms(4000)), "adopted once");
        assert_eq!(e.estimate(), Some(t0 + ms(1200 + 2000 + 500)));

        let mut late = QueryWaitEstimate::new(None, t0);
        late.size_change(t0 + ms(100));
        assert!(late.adopt(CODEX, t0 + Duration::from_secs(60)));
        assert_eq!(
            late.estimate(),
            Some(t0 + ms(100 + 2000 + 500)),
            "already in the past"
        );

        let mut none = QueryWaitEstimate::new(None, t0);
        none.query(QueryKind::Cpr, t0 + ms(100));
        assert!(none.adopt(CODEX, t0 + ms(800)));
        assert_eq!(none.estimate(), Some(t0 + ms(500)));

        const TIGHT: RedrawTiming = RedrawTiming {
            per_size_change: ms(2000),
            margin: Duration::ZERO,
            cap: ms(300),
            waits_on: &[QueryKind::Cpr],
        };
        let mut capped = QueryWaitEstimate::new(None, t0);
        capped.size_change(t0 + ms(1000));
        assert!(capped.adopt(&TIGHT, t0 + ms(1100)));
        assert_eq!(capped.estimate(), Some(t0 + ms(1100 + 300)));

        let mut profiled = QueryWaitEstimate::new(Some(CODEX), t0);
        assert!(!profiled.adopt(CLAUDE, t0));
    }

    /// Scenario: Drive estimates whose arithmetic overflows `Instant`: a cap, a per-change wait, a margin and an adoption cap of `Duration::MAX`, and a query whose wait overflows followed by a size change that doesn't. An overflowing cap or query wait leaves no estimate, which the nudge reads as due at once; an overflowing per-change wait under a finite cap is clamped to the cap; an overflowing margin leaves no estimate; and the next update that doesn't overflow gives one again.
    #[spec("inbox/probe/054")]
    #[test]
    fn probe_054_checked_overflow_leaves_no_estimate() {
        const HUGE_CAP: RedrawTiming = RedrawTiming {
            per_size_change: ms(2000),
            margin: ms(500),
            cap: Duration::MAX,
            waits_on: &[QueryKind::Cpr],
        };
        const HUGE_WAIT: RedrawTiming = RedrawTiming {
            per_size_change: Duration::MAX,
            margin: Duration::ZERO,
            cap: ms(300),
            waits_on: &[QueryKind::Cpr],
        };
        const HUGE_MARGIN: RedrawTiming = RedrawTiming {
            per_size_change: ms(2000),
            margin: Duration::MAX,
            cap: ms(5000),
            waits_on: &[],
        };
        let t0 = Instant::now();
        let dirty = EvidenceStatus::Dirty {
            edge_seq: 0,
            reason: EdgeReason::Resize,
        };

        let mut cap = QueryWaitEstimate::new(Some(&HUGE_CAP), t0);
        cap.size_change(t0);
        assert_eq!(cap.estimate(), None);
        assert!(nudge_due(&dirty, cap.estimate(), t0), "evaluated at once");

        let mut wait = QueryWaitEstimate::new(Some(&HUGE_WAIT), t0);
        wait.size_change(t0 + ms(10));
        assert_eq!(wait.estimate(), Some(t0 + ms(310)), "clamped to the cap");
        wait.query(QueryKind::Cpr, t0 + ms(20));
        assert_eq!(wait.estimate(), None);
        wait.size_change(t0 + ms(30));
        assert_eq!(wait.estimate(), Some(t0 + ms(330)), "the next update");

        let margin = QueryWaitEstimate::new(Some(&HUGE_MARGIN), t0);
        assert_eq!(margin.estimate(), None);

        let mut adopted = QueryWaitEstimate::new(None, t0);
        adopted.size_change(t0);
        assert!(adopted.adopt(&HUGE_CAP, t0 + ms(10)));
        assert_eq!(adopted.estimate(), None);
        let mut adopted = QueryWaitEstimate::new(None, t0);
        adopted.size_change(t0);
        assert!(adopted.adopt(&HUGE_WAIT, t0 + ms(10)));
        assert_eq!(adopted.estimate(), Some(t0 + ms(310)), "clamped");
    }

    /// Scenario: Ask whether the evidence-refresh nudge is due for each evidence state A1 §4.3's table lists (a resize's Dirty, a nudge edge's Dirty, Pending after an edge and at start-up, a learned profile's Dirty) and for the states a nudge doesn't refresh, before, at and after the estimate and with no estimate. A Dirty or Pending state is due from the estimate on, and at once with no estimate; Clean, Unprofiled, an unknown geometry and either budget state never are.
    #[spec("inbox/probe/054")]
    #[test]
    fn probe_054_the_nudge_waits_for_the_estimate() {
        let t0 = Instant::now();
        let at = t0 + ms(2500);
        let refreshed = [
            EvidenceStatus::Dirty {
                edge_seq: 1,
                reason: EdgeReason::Resize,
            },
            EvidenceStatus::Dirty {
                edge_seq: 1,
                reason: EdgeReason::NudgeShrink,
            },
            EvidenceStatus::Dirty {
                edge_seq: 1,
                reason: EdgeReason::NudgeRestore,
            },
            EvidenceStatus::Dirty {
                edge_seq: 1,
                reason: EdgeReason::ProfileLearned,
            },
            EvidenceStatus::Pending {
                since_seq: 1,
                after_edge: Some(1),
            },
            EvidenceStatus::Pending {
                since_seq: 0,
                after_edge: None,
            },
        ];
        for evidence in refreshed {
            assert!(!nudge_due(&evidence, Some(at), t0), "{evidence:?}: before");
            assert!(!nudge_due(&evidence, Some(at), at - ms(1)), "{evidence:?}");
            assert!(nudge_due(&evidence, Some(at), at), "{evidence:?}: at");
            assert!(nudge_due(&evidence, Some(at), at + ms(1)), "{evidence:?}");
            assert!(nudge_due(&evidence, None, t0), "{evidence:?}: none");
        }
        let never = [
            EvidenceStatus::Unprofiled,
            EvidenceStatus::Clean { since_seq: 1 },
            EvidenceStatus::GeometryUnknown { since_seq: 1 },
            EvidenceStatus::OverBudget { since_seq: 1 },
            EvidenceStatus::WorkOverBudget { since_seq: 1 },
        ];
        for evidence in never {
            assert!(!nudge_due(&evidence, Some(t0), at), "{evidence:?}");
            assert!(!nudge_due(&evidence, None, at), "{evidence:?}");
        }
        // Start-up with no size change since spawn: the estimate is the
        // spawn plus the margin, so the nudge is as it was once that passes.
        let startup = QueryWaitEstimate::new(Some(CODEX), t0);
        let pending = EvidenceStatus::Pending {
            since_seq: 0,
            after_edge: None,
        };
        assert!(nudge_due(&pending, startup.estimate(), t0 + ms(500)));
        assert!(!nudge_due(&pending, startup.estimate(), t0 + ms(499)));
    }

    /// A Codex generation with a history in every structure the lock keeps:
    /// queries of each kind, a CPR wait, a frame, a native candidate, a size
    /// change and records attached.
    fn busy_codex(t0: Instant) -> GenerationInput {
        let input = GenerationInput::profiled(CprWaitEnd::SyncBegin, Some(CODEX), t0);
        assert!(input.attach_records(Records::new(1, EcPolicy::Hold, t0)));
        input.size_change(t0 + ms(10));
        input.on_output(
            &[
                OutputNote::Query(QueryKind::Da1),
                OutputNote::Query(QueryKind::Cpr),
            ],
            t0 + ms(20),
        );
        assert_eq!(input.accept_native(t0 + ms(30)), Some(1));
        input.on_output(&[OutputNote::SyncBegin], t0 + ms(40));
        input.on_output(&[OutputNote::Query(QueryKind::Cpr)], t0 + ms(50));
        input
    }

    /// Scenario: Give a Codex generation queries, a CPR wait, a frame, a latched native Ctrl+G, a size change and attached records, then evaluate its estimate and the nudge's timing conjunct many times. The ledger, `B`, the hazard FIFO, the records, the epoch and the estimate itself read the same before and after: evaluating writes nothing.
    #[spec("inbox/probe/054")]
    #[test]
    fn probe_054_evaluating_the_estimate_changes_no_safety_fact() {
        let t0 = Instant::now();
        let input = busy_codex(t0);
        let before = input.state_text();
        let (b, history, cpr) = (
            input.backlog(),
            input.history(),
            input.reply_class(QueryKind::Cpr),
        );
        let estimate = input.query_wait_estimate();
        assert_eq!(estimate, Some(t0 + ms(50 + 2000 + 500)));
        let dirty = EvidenceStatus::Dirty {
            edge_seq: 1,
            reason: EdgeReason::Resize,
        };
        for step in 0..10 {
            let now = t0 + Duration::from_secs(step * 30);
            let _ = nudge_due(&dirty, input.query_wait_estimate(), now);
        }
        assert_eq!(input.query_wait_estimate(), estimate);
        assert_eq!(input.state_text(), before, "nothing the lock keeps moved");
        assert_eq!(input.backlog(), b);
        assert_eq!(input.history(), history);
        assert_eq!(input.reply_class(QueryKind::Cpr), cpr);
    }

    /// Scenario: On an unprofiled generation, note a CPR query, then a `?2026h` frame, accept a native Ctrl+G, and learn Codex's profile; then note a frame, a CPR and a frame. `B` holds through the pre-adoption frame and across adoption, the native candidate stays latched and the CPR stays solicited, and the first frame after adoption ends `B`; afterwards the frame rule runs as on a generation born Codex. With the count at zero, adoption leaves `B` false; a saturated count becomes one wait that the next frame ends.
    #[spec("inbox/probe/054")]
    #[test]
    fn probe_054_b_keeps_its_value_when_codex_is_adopted() {
        let t0 = Instant::now();
        let input = GenerationInput::profiled(CprWaitEnd::Counted, None, t0);
        input.on_output(&[OutputNote::Query(QueryKind::Cpr)], t0);
        input.on_output(&[OutputNote::SyncBegin], t0 + ms(10));
        assert!(input.backlog(), "the counted rule ignores the frame");
        assert_eq!(input.accept_native(t0 + ms(20)), Some(1));
        let hazards = input.hazards(t0 + ms(30));
        assert_eq!(hazards.0, [EntryView::Latched { seq: 1 }]);
        assert!(input.adopt_profile(CprWaitEnd::SyncBegin, CODEX, t0 + ms(40)));
        assert!(input.backlog(), "no earlier query taken as answered");
        assert_eq!(input.hazards(t0 + ms(40)), hazards, "nothing reset");
        assert_eq!(input.reply_class(QueryKind::Cpr), (true, false));
        assert!(
            !input.adopt_profile(CprWaitEnd::Counted, CLAUDE, t0 + ms(45)),
            "once"
        );
        input.on_output(&[OutputNote::SyncBegin], t0 + ms(50));
        assert!(!input.backlog(), "a frame after adoption ends it");
        assert_eq!(
            input.hazards(t0 + ms(60)).0,
            [EntryView::Latched { seq: 1 }],
            "the frame pops nothing"
        );
        input.on_output(&[OutputNote::Query(QueryKind::Cpr)], t0 + ms(70));
        assert!(input.backlog());
        input.on_output(&[OutputNote::SyncBegin], t0 + ms(80));
        assert!(!input.backlog());

        let answered = GenerationInput::profiled(CprWaitEnd::Counted, None, t0);
        answered.on_output(&[OutputNote::Query(QueryKind::Cpr)], t0);
        answered.enqueue(crate::composer_probe::epoch::Writer::Human, b"\x1b[3;7R");
        assert!(!answered.backlog());
        assert!(answered.adopt_profile(CprWaitEnd::SyncBegin, CODEX, t0 + ms(10)));
        assert!(!answered.backlog());
        assert_eq!(answered.accept_native(t0 + ms(20)), Some(1));
        assert_eq!(
            answered.hazards(t0 + ms(20)).0,
            [EntryView::Quiet {
                seq: 1,
                paused: false
            }]
        );

        let mut saturated = Backlog::new(CprWaitEnd::Counted);
        for _ in 0..3 {
            saturated.cpr_query();
        }
        saturated.cpr_reply();
        saturated.adopt(CprWaitEnd::SyncBegin);
        assert!(saturated.holds(), "two outstanding are one wait");
        saturated.sync_begin();
        assert!(!saturated.holds());
    }

    /// Scenario: Move `B` between rules in each direction and to the same rule, with and without a wait outstanding. `B` reads the same before and after every change; leaving the frame rule mid-wait keeps a counted wait that only a CPR reply ends.
    #[spec("inbox/probe/054")]
    #[test]
    fn probe_054_b_adoption_is_continuous_in_every_direction() {
        for from in [CprWaitEnd::Counted, CprWaitEnd::SyncBegin] {
            for to in [CprWaitEnd::Counted, CprWaitEnd::SyncBegin] {
                for waiting in [false, true] {
                    let mut b = Backlog::new(from);
                    if waiting {
                        b.cpr_query();
                    }
                    b.adopt(to);
                    assert_eq!(b.holds(), waiting, "{from:?} -> {to:?}");
                }
            }
        }
        let mut b = Backlog::new(CprWaitEnd::SyncBegin);
        b.cpr_query();
        b.adopt(CprWaitEnd::Counted);
        b.sync_begin();
        assert!(b.holds(), "the counted rule ignores frames");
        b.cpr_reply();
        assert!(!b.holds());
    }

    /// Scenario: Retire a Codex generation and an unprofiled one, then apply a size change to the first and learn Codex's profile on the second. Neither is noted: the first's estimate stays where it was and the second adopts nothing.
    #[spec("inbox/probe/054")]
    #[test]
    fn probe_054_a_retired_generation_takes_nothing() {
        let t0 = Instant::now();
        let codex = GenerationInput::profiled(CprWaitEnd::SyncBegin, Some(CODEX), t0);
        codex.size_change(t0);
        codex.retire();
        codex.size_change(t0 + ms(10));
        assert_eq!(codex.query_wait_estimate(), Some(t0 + ms(2500)));
        let unprofiled = GenerationInput::profiled(CprWaitEnd::Counted, None, t0);
        unprofiled.retire();
        assert!(!unprofiled.adopt_profile(CprWaitEnd::SyncBegin, CODEX, t0 + ms(20)));
        assert_eq!(unprofiled.query_wait_estimate(), None);
        let mut receipt = HazardPop::Unspent;
        assert_eq!(unprofiled.pop_reported(&mut receipt, 1, t0), Pop::Nothing);
    }
}
