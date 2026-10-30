//! PRD #1666 part 3 (Q35-19; core §4.7–§4.8, R5 §3.2, amendments 1–6 of
//! Q35-core): whether the deck may send a probe's Ctrl+G to a generation now,
//! as one function, [`admit`].
//!
//! **One function, every attempt (A4 §4).** The deck writes a byte to the
//! pane for a check in three places: the first probe of an attempt, a
//! scheduled retry after a refusal ([`Attempt::First`], [`Attempt::Retry`]),
//! and Claude's lapse retry ([`Attempt::Lapse`], core §4.7's one exception).
//! Each asks [`admit`], through `GenerationInput::admit_probe`, which reads
//! the generation's half under its input lock and writes nothing; the arm
//! page's count goes up only in `super::observe::Seam::arm`, which asks it
//! again first, on the writer thread, in the section that publishes the arm
//! (A5 §1.5). So a refusal writes no probe byte and changes no arm word.
//! That holds for a retirement racing the arm too: the arm's check, page
//! store and record are one critical section of the input lock
//! (`GenerationInput::arm_probe`), which retirement also takes, so it lands
//! before the check, which refuses, or after the record, when the arm is
//! complete. A refusal of the section's own raise (the count's bound, a
//! stale generation) is decided before the store as well.
//!
//! **The conjuncts, in the order they are asked** (each refusal names the
//! first that failed):
//! 1. the generation is live and its permission is `Armable` (plan §1.2);
//! 2. the records admit (`super::record::Records::admits`): every record
//!    whose completion is required is `Complete` or `Subsumed` (A2 §3.4,
//!    A4 conjunct 1), the writer scope is closed (A4 conjunct 2: pending or
//!    open refuses, A6), and neither record cap is reached (core §2.6);
//! 3. neither hazard state is `Exhausted` (A3 §6.1, A4 conjunct 3);
//! 4. the native hazard FIFO is clear (A1 §3.4): no `Latched` entry, no
//!    `Quiet` one inside D5's quiet period, nothing counted past its slots;
//! 5. fewer than two liabilities stand (R5 §3.2's cap), and the gate is open,
//!    except for a Claude lapse retry, whose gate holds only its lapsed probe
//!    with nothing behind it (core §4.7–§4.8);
//! 6. no human ticket is outstanding (core §4.8);
//! 7. the written stream is in **strict** ground (core §4.8): a lone ESC, a
//!    paste or an unfinished sequence gets no probe. This is not the
//!    doorbell's start predicate, whose lone-ESC exception (Q35-16's review,
//!    `super::planner::Boundary::Escape`) is never probe permission;
//! 8. for a lapse retry, nothing entered the box since the lapsed probe's own
//!    byte (`epoch == E0 + 1`, R5 §3.2) and `B` doesn't hold (A1 §3.5);
//! 9. the keymap sources still leave Ctrl+G on the external editor
//!    (`super::keymap`), read by the caller with no lock held;
//! 10. the screen shows the agent's main composer by its positive identity
//!     (Claude, R3 §4.1.1) or its placeholder (Codex, R3 §2.3 item 4), as the
//!     caller's recogniser reads it;
//! 11. **O5** (A1 §4.4, A2 §6): `now` has reached the generation's
//!     query-wait estimate (`super::query_wait`). Availability only: it
//!     reads the estimate, writes nothing, and a wrong estimate costs a
//!     later probe or a longer hold behind an admitted one, never a key. No
//!     estimate is due now. The delay it adds is not bounded by the profile's
//!     value, since a backlog can outlast it.
//!
//! **What doesn't refuse.** A saturated exemption ledger alone (A1 §3.2:
//! saturation makes every reply touch and none pass ahead, and admits as
//! before), and `B` alone outside a lapse retry: `B` reaches admission through
//! the hazard FIFO's latch, which conjunct 4 reads. C1's CPR input is not
//! read: C1 isn't built, and nothing here special-cases a deck reply.
//!
//! **What rests on P-WE** (Codex's `B` ends at its frame after a CPR, A2
//! §4.2, unverified on the real binaries until M-WE): conjunct 4's
//! `Latched` entries and conjunct 8's `B` are read from a `B` that, for a
//! Codex profile, turns false at a `?2026h` after the last CPR query. If P-WE
//! fails, a native Ctrl+G accepted during a wait could be taken as `Quiet`
//! and leave under D5. Nothing in production admits a Codex probe before
//! activation (no generation is `Armable` there, and EC is `Hold`).
//!
//! The screen and keymap facts and the gate, ticket and boundary are the
//! caller's ([`Outside`]); this module reads no screen, file or lock.

use std::time::Instant;

use super::Permission;
use super::keymap;
use super::planner::Boundary;
use super::record::{self, Hold};

/// Which of the deck's check writes is asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attempt {
    /// The first probe of an attempt.
    First,
    /// A retry the schedule brought after a refusal or a failure.
    Retry,
    /// Claude's lapse retry (core §4.7), superseding the lapsed probe whose
    /// own byte moved the epoch to `lapsed_e0 + 1`.
    Lapse { lapsed_e0: u64 },
}

/// The fence gate as admission reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fence {
    Open,
    /// Closed, and its only segment is a Claude probe that lapsed with
    /// nothing held behind it (core §4.7).
    Lapsed,
    Closed,
}

/// What admission reads outside the generation's input lock: the caller's
/// reads of the permission, gate, ticket, written stream, keymap and screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outside {
    pub attempt: Attempt,
    pub permission: Permission,
    pub fence: Fence,
    /// A person's ticket is outstanding.
    pub human_ticket: bool,
    /// Where the bytes the PTY took leave the input.
    pub boundary: Boundary,
    pub keymap: Result<(), keymap::Hold>,
    /// The screen shows the main composer (Claude) or its placeholder
    /// (Codex), as the caller's recogniser reads it.
    pub surface: bool,
}

/// What admission reads under the generation's input lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inside {
    pub retired: bool,
    /// [`record::Records::admits`], [`Hold::Retired`] with no records.
    pub records: Result<(), Hold>,
    /// Either hazard state is `Exhausted`.
    pub exhausted: bool,
    /// The hazard FIFO's clear conjunct.
    pub hazards_clear: bool,
    /// The liabilities standing.
    pub count: u32,
    pub epoch: u64,
    /// `B`.
    pub backlog: bool,
    /// The query-wait estimate, `None` for none.
    pub estimate: Option<Instant>,
}

/// Why no probe may be sent now: the first conjunct that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Retired,
    NotArmable(Permission),
    /// The records: an incomplete required record, a writer scope that
    /// isn't closed, or a cap.
    Records(Hold),
    Exhausted,
    /// The native hazard FIFO isn't clear.
    Hazard,
    /// Two liabilities stand.
    LiabilityCap,
    /// The gate is closed, other than for a lapse retry over its lapsed
    /// probe.
    Fenced,
    /// A lapse retry with no lapsed probe to supersede.
    NotLapsed,
    HumanTicket,
    /// The written stream isn't in ground.
    OffGround(Boundary),
    /// A lapse retry after something entered the box since the lapsed byte.
    Touched,
    /// A lapse retry while `B` holds.
    Backlog,
    Keymap(keymap::Hold),
    /// The screen isn't the main composer.
    Surface,
    /// O5: the query-wait estimate is still ahead.
    Estimate(Instant),
    /// The records refused the arm's raise after admission (retired since,
    /// or the count can't move).
    Raise(record::Refusal),
}

/// The one admission predicate. See the module documentation.
pub fn admit(inside: &Inside, outside: &Outside, now: Instant) -> Result<(), Refusal> {
    if inside.retired {
        return Err(Refusal::Retired);
    }
    if outside.permission != Permission::Armable {
        return Err(Refusal::NotArmable(outside.permission));
    }
    inside.records.map_err(Refusal::Records)?;
    if inside.exhausted {
        return Err(Refusal::Exhausted);
    }
    if !inside.hazards_clear {
        return Err(Refusal::Hazard);
    }
    if inside.count >= 2 {
        return Err(Refusal::LiabilityCap);
    }
    match (outside.attempt, outside.fence) {
        (Attempt::Lapse { .. }, Fence::Lapsed) => {}
        (Attempt::Lapse { .. }, _) => return Err(Refusal::NotLapsed),
        (_, Fence::Open) => {}
        (_, Fence::Lapsed | Fence::Closed) => return Err(Refusal::Fenced),
    }
    if outside.human_ticket {
        return Err(Refusal::HumanTicket);
    }
    if outside.boundary != Boundary::Ground {
        return Err(Refusal::OffGround(outside.boundary));
    }
    if let Attempt::Lapse { lapsed_e0 } = outside.attempt {
        if lapsed_e0.checked_add(1) != Some(inside.epoch) {
            return Err(Refusal::Touched);
        }
        if inside.backlog {
            return Err(Refusal::Backlog);
        }
    }
    outside.keymap.map_err(Refusal::Keymap)?;
    if !outside.surface {
        return Err(Refusal::Surface);
    }
    o5(inside.estimate, now)
}

/// O5 alone: `Ok` once `now` reaches `estimate`, or with none.
pub fn o5(estimate: Option<Instant>, now: Instant) -> Result<(), Refusal> {
    match estimate {
        Some(at) if now < at => Err(Refusal::Estimate(at)),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composer_probe::epoch::{GenerationInput, OutputNote};
    use crate::composer_probe::planner::{Physical, PhysicalTracker};
    use crate::composer_probe::query_ledger::{CprWaitEnd, LEDGER_SLOTS, QueryKind, T_TRIGGER};
    use crate::composer_probe::record::{
        Attribution, Cause, Channel, Decision, EcPolicy, NotOwned, Observation, RecordKey, Records,
        ScopeState,
    };
    use crate::composer_probe::{InertCause, Permission};
    use crate::doorbell_screen::CODEX_PROFILE;
    use spec::spec;
    use std::time::Duration;

    const G: u64 = 7;

    fn inside() -> Inside {
        Inside {
            retired: false,
            records: Ok(()),
            exhausted: false,
            hazards_clear: true,
            count: 0,
            epoch: 4,
            backlog: false,
            estimate: None,
        }
    }

    fn outside() -> Outside {
        Outside {
            attempt: Attempt::First,
            permission: Permission::Armable,
            fence: Fence::Open,
            human_ticket: false,
            boundary: Boundary::Ground,
            keymap: Ok(()),
            surface: true,
        }
    }

    /// A live generation with its records attached under `Hold`.
    fn generation() -> GenerationInput {
        let input = GenerationInput::new(CprWaitEnd::Counted);
        assert!(input.attach_records(Records::new(G, EcPolicy::Hold, Instant::now())));
        input
    }

    fn ipc(pid: u32, attribution: Attribution, decision: Decision, at: Instant) -> Observation {
        Observation {
            generation: G,
            key: RecordKey { pid, start: 1 },
            channel: Channel::Ipc,
            attribution,
            claim: None,
            decision: Some(decision),
            seg_seen: None,
            native_seen: None,
            birth_floor: at,
        }
    }

    /// Scenario: Ask admission with every conjunct met, for a first probe and a scheduled retry, then change one conjunct at a time: retirement, each permission short of armable, an incomplete record, a scope pending or open, each record cap, an exhausted hazard state, an uncleared hazard FIFO, two standing liabilities, a closed or lapsed gate, a person's ticket, a stream off ground, a held keymap, a screen that isn't the main composer and an estimate still ahead. The met case is admitted, and each change alone is refused with its own reason.
    #[spec("inbox/probe/077")]
    #[test]
    fn probe_077_each_conjunct_alone_refuses_with_its_reason() {
        let now = Instant::now();
        assert_eq!(admit(&inside(), &outside(), now), Ok(()));
        let retry = Outside {
            attempt: Attempt::Retry,
            ..outside()
        };
        assert_eq!(admit(&inside(), &retry, now), Ok(()));
        assert_eq!(
            admit(
                &Inside {
                    count: 1,
                    ..inside()
                },
                &outside(),
                now
            ),
            Ok(()),
            "one standing liability is under the cap"
        );

        let refuse_in = |change: Inside| admit(&change, &outside(), now);
        let refuse_out = |change: Outside| admit(&inside(), &change, now);
        assert_eq!(
            refuse_in(Inside {
                retired: true,
                ..inside()
            }),
            Err(Refusal::Retired)
        );
        for permission in [
            Permission::Pending,
            Permission::Inert(InertCause::UnmeasuredRelease),
            Permission::Inert(InertCause::Unbound),
        ] {
            assert_eq!(
                refuse_out(Outside {
                    permission,
                    ..outside()
                }),
                Err(Refusal::NotArmable(permission))
            );
        }
        for hold in [
            Hold::Incomplete(RecordKey { pid: 9, start: 1 }),
            Hold::Scope(ScopeState::Pending),
            Hold::Scope(ScopeState::Open(Cause::EditorLeftWriter)),
            Hold::RecordCap,
            Hold::HistoryCap,
            Hold::Retired,
        ] {
            assert_eq!(
                refuse_in(Inside {
                    records: Err(hold),
                    ..inside()
                }),
                Err(Refusal::Records(hold))
            );
        }
        assert_eq!(
            refuse_in(Inside {
                exhausted: true,
                ..inside()
            }),
            Err(Refusal::Exhausted)
        );
        assert_eq!(
            refuse_in(Inside {
                hazards_clear: false,
                ..inside()
            }),
            Err(Refusal::Hazard)
        );
        assert_eq!(
            refuse_in(Inside {
                count: 2,
                ..inside()
            }),
            Err(Refusal::LiabilityCap)
        );
        for fence in [Fence::Closed, Fence::Lapsed] {
            assert_eq!(
                refuse_out(Outside { fence, ..outside() }),
                Err(Refusal::Fenced)
            );
            assert_eq!(refuse_out(Outside { fence, ..retry }), Err(Refusal::Fenced));
        }
        assert_eq!(
            refuse_out(Outside {
                human_ticket: true,
                ..outside()
            }),
            Err(Refusal::HumanTicket)
        );
        for boundary in [Boundary::Inside, Boundary::Escape] {
            assert_eq!(
                refuse_out(Outside {
                    boundary,
                    ..outside()
                }),
                Err(Refusal::OffGround(boundary))
            );
        }
        for hold in [keymap::Hold::Changed, keymap::Hold::Rebound] {
            assert_eq!(
                refuse_out(Outside {
                    keymap: Err(hold),
                    ..outside()
                }),
                Err(Refusal::Keymap(hold))
            );
        }
        assert_eq!(
            refuse_out(Outside {
                surface: false,
                ..outside()
            }),
            Err(Refusal::Surface)
        );
        let ahead = now + Duration::from_millis(1);
        assert_eq!(
            refuse_in(Inside {
                estimate: Some(ahead),
                ..inside()
            }),
            Err(Refusal::Estimate(ahead))
        );
        assert_eq!(
            admit(
                &Inside {
                    estimate: Some(now),
                    ..inside()
                },
                &outside(),
                now
            ),
            Ok(()),
            "an estimate reached is due"
        );
    }

    /// Scenario: Ask admission for Claude's lapse retry over a gate holding only its lapsed probe, with nothing entered since that probe's byte and `B` false; then with the gate open, closed, the epoch moved since the lapsed byte, the lapsed byte at the epoch's last value, `B` true and a person's ticket. The first is admitted; each of the others is refused with its reason, and the same `B` that refuses the lapse retry refuses no first probe.
    #[spec("inbox/probe/077")]
    #[test]
    fn probe_077_the_lapse_retry_needs_its_lapsed_byte_last_and_no_backlog() {
        let now = Instant::now();
        let lapse = Outside {
            attempt: Attempt::Lapse { lapsed_e0: 3 },
            fence: Fence::Lapsed,
            ..outside()
        };
        let standing = Inside {
            count: 1,
            ..inside()
        };
        assert_eq!(admit(&standing, &lapse, now), Ok(()));
        for fence in [Fence::Open, Fence::Closed] {
            assert_eq!(
                admit(&standing, &Outside { fence, ..lapse }, now),
                Err(Refusal::NotLapsed)
            );
        }
        assert_eq!(
            admit(
                &Inside {
                    epoch: 5,
                    ..standing.clone()
                },
                &lapse,
                now
            ),
            Err(Refusal::Touched)
        );
        assert_eq!(
            admit(
                &Inside {
                    epoch: u64::MAX,
                    ..standing.clone()
                },
                &Outside {
                    attempt: Attempt::Lapse {
                        lapsed_e0: u64::MAX
                    },
                    ..lapse
                },
                now
            ),
            Err(Refusal::Touched),
            "no epoch follows the last"
        );
        let backlog = Inside {
            backlog: true,
            ..standing.clone()
        };
        assert_eq!(admit(&backlog, &lapse, now), Err(Refusal::Backlog));
        assert_eq!(
            admit(&backlog, &outside(), now),
            Ok(()),
            "B alone refuses no first probe"
        );
        assert_eq!(
            admit(
                &standing,
                &Outside {
                    human_ticket: true,
                    ..lapse
                },
                now
            ),
            Err(Refusal::HumanTicket)
        );
        assert_eq!(
            admit(
                &Inside {
                    count: 2,
                    ..standing
                },
                &lapse,
                now
            ),
            Err(Refusal::LiabilityCap),
            "a third liability never"
        );
    }

    /// Scenario: Feed the writer thread's own tracker a lone ESC, then `ESC [`, then the CSI's final byte, and ask admission with the boundary it reports after each. After the lone ESC, where a doorbell line may start, and inside the CSI, no probe is admitted; once the CSI ends in ground, it is.
    #[spec("inbox/probe/077")]
    #[test]
    fn probe_077_a_lone_esc_gets_no_probe() {
        let now = Instant::now();
        let physical = Physical::default();
        let mut tracker = PhysicalTracker::default();
        let ask = |physical: &Physical| {
            admit(
                &inside(),
                &Outside {
                    boundary: physical.boundary(),
                    ..outside()
                },
                now,
            )
        };
        tracker.written(b"\x1b", &physical);
        assert!(
            physical.boundary().line_may_start(),
            "a doorbell line may start here"
        );
        assert_eq!(ask(&physical), Err(Refusal::OffGround(Boundary::Escape)));
        tracker.written(b"[", &physical);
        assert_eq!(ask(&physical), Err(Refusal::OffGround(Boundary::Inside)));
        tracker.written(b"A", &physical);
        assert_eq!(ask(&physical), Ok(()));
    }

    /// Scenario: Arm through the generation's input with a refusal the raise itself makes after admission admits: the arm names a generation other than its records', and, separately, the generation retires before the arm. Each refuses before the page is stored, so the publication is never called; a matching arm calls it once with the raised count, then records it.
    #[spec("inbox/probe/077")]
    #[test]
    fn probe_077_an_arm_refused_after_admission_publishes_nothing() {
        let now = Instant::now();
        let input = generation();
        let mut stored = Vec::new();
        assert_eq!(
            input.arm_probe(G + 1, &outside(), now, |n| stored.push(n)),
            Err(Refusal::Raise(
                crate::composer_probe::record::Refusal::Stale
            ))
        );
        assert!(stored.is_empty(), "nothing stored for a refused raise");
        assert_eq!(
            input.arm_probe(G, &outside(), now, |n| stored.push(n)),
            Ok(1)
        );
        assert_eq!(stored, [1]);
        assert_eq!(input.liability_count(), Some(1));
        input.retire();
        assert_eq!(
            input.arm_probe(G, &outside(), now, |n| stored.push(n)),
            Err(Refusal::Retired)
        );
        assert_eq!(stored, [1]);
    }

    /// Scenario: On a live generation, note more D3′ queries than the exemption ledger keeps, so it saturates, with no native Ctrl+G accepted; then note a CPR so `B` holds. Admission admits a first probe in both states, and refuses only the lapse retry once `B` holds.
    #[spec("inbox/probe/077")]
    #[test]
    fn probe_077_a_saturated_ledger_alone_doesnt_refuse() {
        let now = Instant::now();
        let input = generation();
        let notes = vec![OutputNote::Query(QueryKind::Da1); LEDGER_SLOTS + 1];
        input.on_output(&notes, now);
        assert_eq!(
            input.history(),
            crate::composer_probe::query_ledger::QueryHistory::Unknown
        );
        assert_eq!(input.admit_probe(&outside(), now), Ok(()));
        input.on_output(&[OutputNote::Query(QueryKind::Cpr)], now);
        assert!(input.backlog());
        assert_eq!(input.admit_probe(&outside(), now), Ok(()));
        // The lapsed probe's own byte is the last thing that entered.
        input.liability_raise(G, now).unwrap();
        input.enqueue(crate::composer_probe::epoch::Writer::UnnamedDeck, b"\x07");
        let lapse = Outside {
            attempt: Attempt::Lapse {
                lapsed_e0: input.epoch().epoch() - 1,
            },
            fence: Fence::Lapsed,
            ..outside()
        };
        assert_eq!(input.admit_probe(&lapse, now), Err(Refusal::Backlog));
    }

    /// Scenario: On a live generation, accept a native Ctrl+G with `B` false and ask admission during D5's quiet period and after it; accept another while a CPR is outstanding and ask long after. The quiet entry refuses until its period has run and then leaves; the latched entry refuses however long it waits. Each refusal is the hazard's, and the read-only clear agrees with the clearing one.
    #[spec("inbox/probe/077")]
    #[test]
    fn probe_077_the_hazard_fifo_refuses_through_its_quiet_period_and_latch() {
        let t0 = Instant::now();
        let input = generation();
        input.accept_native(t0).unwrap();
        let during = t0 + T_TRIGGER;
        assert_eq!(input.admit_probe(&outside(), during), Err(Refusal::Hazard));
        let after = during + Duration::from_millis(1);
        assert_eq!(input.admit_probe(&outside(), after), Ok(()));
        assert!(input.hazards_clear(after), "the clearing read agrees");

        let latched = generation();
        latched.on_output(&[OutputNote::Query(QueryKind::Cpr)], t0);
        latched.accept_native(t0).unwrap();
        let later = t0 + T_TRIGGER * 100;
        assert_eq!(latched.admit_probe(&outside(), later), Err(Refusal::Hazard));
        assert!(!latched.hazards_clear(later));
    }

    /// Scenario: On a live generation under `Hold`, record a tool's helper deciding `Tool` on IPC, which leaves its record not required but opens the writer scope; separately, an `Owned` helper deciding `Armed`, whose record is required and incomplete while the scope stays closed. Admission refuses the first for the scope alone and the second for its record alone; with nothing recorded it admits.
    #[spec("inbox/probe/077")]
    #[test]
    fn probe_077_an_open_scope_or_an_incomplete_record_refuses() {
        let now = Instant::now();
        let clean = generation();
        assert_eq!(clean.admit_probe(&outside(), now), Ok(()));

        let tool = generation();
        tool.record_observe(
            &ipc(
                41,
                Attribution::NotOwned(NotOwned::Tool),
                Decision::Tool,
                now,
            ),
            now,
        )
        .unwrap();
        assert!(!tool.writer_scope().unwrap().is_closed());
        assert_eq!(
            tool.admit_probe(&outside(), now),
            Err(Refusal::Records(Hold::Scope(
                tool.writer_scope().unwrap().state()
            )))
        );

        let owned = generation();
        owned
            .record_observe(&ipc(42, Attribution::Owned, Decision::Armed, now), now)
            .unwrap();
        assert!(owned.writer_scope().unwrap().is_closed());
        assert_eq!(
            owned.admit_probe(&outside(), now),
            Err(Refusal::Records(Hold::Incomplete(RecordKey {
                pid: 42,
                start: 1
            })))
        );
    }

    /// Scenario: On a live Codex-profiled generation holding a resize's estimate, a noted query, a latched native Ctrl+G and a saturated ledger, evaluate O5 alone and whole admission before and after the estimate, at instants earlier than the generation has seen as well as later. Every fact the generation keeps (ledger, `B`, the hazard FIFO and its clock, records, estimate) reads byte-identical after each evaluation; and a generation holding only a resize's estimate is refused by O5 until it and admitted at it.
    #[spec("inbox/probe/077")]
    #[test]
    fn probe_077_o5_and_admission_write_nothing() {
        let spawn = Instant::now();
        let input =
            GenerationInput::profiled(CprWaitEnd::SyncBegin, Some(&CODEX_PROFILE.redraw), spawn);
        assert!(input.attach_records(Records::new(G, EcPolicy::Hold, spawn)));
        let t = spawn + Duration::from_secs(1);
        input.size_change(t);
        let notes = vec![OutputNote::Query(QueryKind::Da1); LEDGER_SLOTS + 2];
        input.on_output(&notes, t);
        input.on_output(&[OutputNote::Query(QueryKind::Cpr)], t);
        input.accept_native(t).unwrap();
        let estimate = input.query_wait_estimate().expect("an estimate");
        let before = input.state_text();
        for now in [
            spawn,
            t,
            estimate - Duration::from_millis(1),
            estimate,
            estimate + T_TRIGGER * 10,
        ] {
            let _ = o5(input.query_wait_estimate(), now);
            let _ = input.admit_probe(&outside(), now);
            assert_eq!(input.state_text(), before, "at {now:?}");
        }
        assert_eq!(
            o5(Some(estimate), estimate - Duration::from_millis(1)),
            Err(Refusal::Estimate(estimate))
        );
        assert_eq!(o5(Some(estimate), estimate), Ok(()));
        assert_eq!(o5(None, spawn), Ok(()), "no estimate is due now");

        let resized =
            GenerationInput::profiled(CprWaitEnd::SyncBegin, Some(&CODEX_PROFILE.redraw), spawn);
        assert!(resized.attach_records(Records::new(G, EcPolicy::Hold, spawn)));
        resized.size_change(t);
        let due = resized.query_wait_estimate().expect("an estimate");
        assert_eq!(
            resized.admit_probe(&outside(), due - Duration::from_millis(1)),
            Err(Refusal::Estimate(due)),
            "the generation's own estimate"
        );
        assert_eq!(resized.admit_probe(&outside(), due), Ok(()));
    }

    /// Scenario: Retire a live generation, and take another that never had records attached. Admission refuses the first as retired and the second as the records' retirement, reading no records in either.
    #[spec("inbox/probe/077")]
    #[test]
    fn probe_077_retirement_and_missing_records_refuse() {
        let now = Instant::now();
        let retired = generation();
        retired.retire();
        assert_eq!(retired.admit_probe(&outside(), now), Err(Refusal::Retired));
        let no_records = GenerationInput::new(CprWaitEnd::Counted);
        assert_eq!(
            no_records.admit_probe(&outside(), now),
            Err(Refusal::Records(Hold::Retired)),
            "a generation with no records attached admits nothing"
        );
    }
}
