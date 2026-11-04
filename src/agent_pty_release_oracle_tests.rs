//! PRD #1666 part 3 (Q35-20b, plan §3.7, X-3): the release-authority oracle,
//! composed with the production driver (`inbox/probe/010`).
//!
//! **What runs is production code.** A real [`PaneWriter`] over a scripted
//! PTY, its gate and writer thread; the generation's real input
//! ([`GenerationInput`]: records, segments, epoch, schedule) and its real
//! output hook ([`OutputHook`]), fed from before the segments attach; the
//! production release driver ([`super::segment_driver::drive_segments`]) as
//! its own task, woken as production wakes it; a probe queued through
//! [`PaneWriter::queue_probe`] with production's arm section
//! ([`super::segment_driver::probe_section`]) closing its fence, and its
//! job's end reported to its segment by production's
//! [`super::segment_driver::end_probe_segment`]. What the observer would see
//! (a helper's records, the daemon's read of an `Armed` export, its editor
//! running, its report and exit) reaches the input through the same calls
//! the observer makes. The generated scripts leave admission out: every
//! probe there is admitted, and the section raises the liability as
//! `arm_segment` does. One rig asks it ([`Rig::probe_admitted`], A-20d-2):
//! its output goes through a real `AgentBus` with Claude Code's screen
//! profile, the probe is scheduled through the generation's own admission
//! with `composer_probe`'s reads, and its arm section asks again with the
//! writer's own facts (`writer_facts`), raising the liability in the same
//! hold, as the seam's arm does.
//!
//! **What judges it is not.** [`Model`] is the release table of core §4.6
//! and plan §3.7, written from the table, over the generated script alone:
//! which record matches which segment, which row releases what and when, and
//! what each trigger may carry. It never reads the implementation's state.
//! After every step, once the driver has had its chance, the bytes the PTY
//! took must be exactly what the model allows by then: a byte early is a
//! release without authority, a byte missing is a release that never came.
//! Every Ctrl+G must be written alone, after its number was published, and
//! nothing after retirement. The probe's settlement is judged the same way:
//! the schedule must stop being due exactly when the model settles it.
//!
//! The generated scripts carry the six traces of the Q35-20 review (R-U2-01
//! to 06) as ordinary steps: output before the segments attach (02), a
//! return and an editor read while a trigger's report is on its way (01),
//! typing after a release with nothing else moving (03), a deferred
//! settlement whose segment completes (05) and an unknown wait after a
//! release to a running editor (06). R-U2-04's paste-pair accounting has its
//! own generated model below ([`pairs`]). Each trace also has a fixed script,
//! and so does R-B-U2-02's: a drain that lands inside a driver pass, between
//! its read of the front segment and its extraction of what is held, forced
//! there by a test-only seam on the writer. That seam also lands any other
//! observation inside a pass (A-20d-3, O-20.3, [`Step::MidPass`]): a
//! helper's decision, its keymap re-read (A-20d-1a, registered on those
//! rigs), a second sighting of one helper, an exit or a return marker, each
//! script run with it before, inside and after the pass and required to end
//! the same.
//!
//! **A liability out with no segment (A-20d-5).** The model counts the
//! liability (one per probe armed, one less per owned `Armed` decision while
//! one is out), and every check compares it with the records'. With one out
//! and no segment open (a phantom raised for it, or Claude's lapsed probe
//! superseded by a retry that completed), a person's Ctrl+G cuts their op
//! and opens a `HumanG` whose number is published before its trigger write,
//! the bytes through the candidate (core §4.3, §4.5), the rest held behind
//! it. Both halves run: its authority half ([`Judged::NothingEarly`]) and
//! its exact half, the release once the `HumanG`'s helper answers. The
//! generator scripts phantoms too, and a Ctrl+G typed while a liability is
//! out, so its seeds reach the cut-open row (a floor counts it).
//!
//! **A probe byte the PTY fails (O-20.5).** A fixed script per agent makes
//! the PTY fail a probe's byte, with an error and by taking none of it,
//! while a person types behind its fence, and ends the probe with
//! production's own handling ([`super::segment_driver::probe_ended`]): the
//! arm undone, the segment gone, the failure counted, the typing released
//! with nothing outstanding and nothing left in flight. Retirement landing
//! between a live arm section and its byte is not forced here (the
//! activation list carries it).
//!
//! **An editor that ended before it was seen running (A-20d-15).** The
//! model releases a Claude `HumanG` whose `Unarmed` helper's clean report
//! was in when its exit drained, as production does. The generated scripts
//! seldom reach that row (the front segment's helper is rarely an undrained
//! `Unarmed` one when a report is scripted), so no floor requires it; the
//! fixed script [`probe_010_typing_behind_an_editor_that_ended_unseen_follows_the_release_table`]
//! reaches it on Claude and holds on Codex, whose release also needs its
//! return past the drain frontier, which this rig never takes.
//!
//! **A later Ctrl+G behind a segment that can never complete** (R-HOLD's
//! bounded refusal, the user's ruling of 2026-11-03). Once a released
//! segment's writer scope is open, nothing completes it, so the model
//! refuses the Ctrl+G first in what is held behind it, with everything held
//! after it, once the release has gone up to it: those bytes are never
//! expected. Its own fixed script is
//! [`probe_087_a_later_ctrl_g_behind_an_editor_segment_is_refused_and_typing_goes_on`],
//! which also checks the notice, the ticket and the doorbell's tick; the
//! generated runs reach the row too, which a floor requires
//! ([`Stats::refused`]: 43 in the fast tier's 40 seeds and 185 in lane 1's
//! 240 when it was added).
//!
//! **The policy it assumes (the audit's U3-2).** Every generation here runs
//! [`EcPolicy::Hold`], production's policy, under which `Records::complete`
//! refuses the EC route: the model has no EC or frontier rows, and no
//! Authority-policy EC completion is exercised (R-B-U3-02). If the policy
//! changes (Q76), the model needs those rows and `Records::complete`'s EC
//! route before this oracle says anything about it.
//!
//! **Where its power ends.** It is serial: each step settles before the
//! next, so a race between the observer and a driver pass in flight is found
//! only where a fixed script forces it. Its floors require a selection of the
//! release table's tuples ([`ROWS`], 14 when written), each settlement kind
//! and each kind of generated step (a keymap re-read, a mid-pass step and a
//! lapse retry are the fixed scripts' only), not every combination and not the whole table: no
//! Claude failed-start tuple and no tuple without a decision is required,
//! and a tuple's key does not tell an ending by a drain or an unknown wait
//! from a running editor (the cut-off has its own floor). Each is counted
//! once per segment that reached it, not per evaluation. Not exercised here, each its own obligation
//! and not evidence that it passed (R-B-U3-03): admission beyond
//! [`Rig::probe_admitted`]'s Claude rig (its own tests and S-20's
//! `e2e_probe_admission`; `composer_probe` itself and the observer's seam
//! are not called, and what lands after the job is queued and before the
//! writer thread reaches its section is not forced), the observer's `/proc`
//! scan and editor identity, more than one probe in a script beyond Claude's
//! lapse retry, other Ctrl+G forms and multi-candidate frames (the planner's
//! own oracle, Q35-16) and Codex timing beyond its markers (M-PF1).
//! Linux only, as the driver is.

use super::segment_driver::{
    PROBE_WRITE_FAILED, ProbeAsk, ProbeEnd, ProbeOutcome, ProbeRefusal, drive_segments,
    end_probe_segment, probe_ended, probe_section, scheduling_reads, writer_facts,
};
use super::*;
use crate::composer_probe::epoch::GenerationInput;
use crate::composer_probe::paste_mode::OutputHook;
use crate::composer_probe::query_ledger::T_TRIGGER;
use crate::composer_probe::record::{
    Attribution, Channel, Decision, EcPolicy, NotOwned, Observation, RecordKey, Records, Report,
    Tree,
};
use crate::composer_probe::segments::{Agent, Stage};
use crate::ingress_ledger::IngressLedger;
use spec::spec;
use std::collections::VecDeque;

const G: u64 = 5;
const OFF: &[u8] = b"\x1b[?2004l";
const ON: &[u8] = b"\x1b[?2004h";
const FRAME: &[u8] = b"\x1b[?2026h";
const CTRL_G: u8 = 0x07;

/// Q147b, from the line discipline's defaults (`termios(3)`), not from the
/// implementation: a byte a terminal out of raw mode may act on rather than
/// queue as typed. Every C0 control but Tab, Line Feed and Escape (among
/// them the signal keys, Enter under `ICRNL`, the erase, kill, word-erase,
/// reprint, literal-next, end-of-file and flow-control keys), and Delete
/// (the erase key).
fn line_discipline_key(byte: u8) -> bool {
    matches!(byte, 0x00..=0x08 | 0x0b..=0x1a | 0x1c..=0x1f | 0x7f)
}

/// What the PTY and the arm page saw, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seen {
    /// One write the PTY took.
    Took(Vec<u8>),
    /// A segment number published before its trigger.
    Published(u64),
}

/// What the agent does, at once, when the PTY takes a Ctrl+G (R-U2-01): it
/// reads the trigger and answers before the writer's report can reach the
/// segments.
type React = Box<dyn FnMut(&Rig) + Send>;

/// The PTY: takes everything, logs each write, and runs the agent's reaction
/// to a Ctrl+G inside the write, before the writer thread hears it went.
struct Pty {
    rig: Arc<std::sync::OnceLock<std::sync::Weak<Rig>>>,
}

/// How the PTY fails a probe's byte (O-20.5). A probe's job is its one byte,
/// so a partial write is one that took none of it.
#[derive(Debug, Clone, Copy)]
enum ProbeFault {
    /// `write` fails.
    Error,
    /// `write` takes nothing.
    Zero,
}

impl std::io::Write for Pty {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let Some(rig) = self.rig.get().and_then(std::sync::Weak::upgrade) else {
            return Ok(buf.len());
        };
        if buf == [CTRL_G] {
            let fault = rig.fault.lock().unwrap().take();
            if let Some((fault, typed)) = fault {
                // A person's keys, typed while the probe's byte is at the
                // PTY: behind the fence its arm section closed. None for an
                // empty `typed` (the audit's U3-8).
                if !typed.is_empty() {
                    let held = Arc::clone(&rig);
                    rig.runtime.block_on(async move { held.typed(typed).await });
                }
                return match fault {
                    ProbeFault::Error => Err(std::io::Error::from_raw_os_error(libc::EIO)),
                    ProbeFault::Zero => Ok(0),
                };
            }
        }
        rig.seen.lock().unwrap().push(Seen::Took(buf.to_vec()));
        if buf.contains(&CTRL_G) {
            let react = rig.react.lock().unwrap().take();
            if let Some(mut react) = react {
                react(&rig);
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// One generation on the real writer, input, hook and driver.
struct Rig {
    writer: Arc<AsyncMutex<PaneWriter>>,
    input: Arc<GenerationInput>,
    /// The output hook, fed directly; `None` on a rig whose output goes
    /// through [`Self::bus`].
    hook: Mutex<Option<OutputHook>>,
    /// A-20d-2: the generation's real output bus, with Claude Code's screen
    /// profile, when the rig asks admission as `composer_probe` does: its
    /// output is pushed through the bus, whose own output hook feeds the
    /// input, and its screen is read from the bus's marked Clean frame.
    bus: Option<Arc<AgentBus>>,
    seen: Mutex<Vec<Seen>>,
    react: Mutex<Option<React>>,
    ledger: Arc<IngressLedger>,
    retired: Arc<AtomicBool>,
    /// How many mid-pass facts fired inside a driver pass (R-B-U2-02).
    mid_fired: std::sync::atomic::AtomicUsize,
    /// The next probe byte's fault, and what a person types meanwhile
    /// (O-20.5).
    fault: Mutex<Option<(ProbeFault, &'static [u8])>>,
    /// The test's runtime, for what the PTY's writer thread types.
    runtime: tokio::runtime::Handle,
}

impl Rig {
    /// A generation of `agent` whose output ran `prefix` before its
    /// segments attached (R-U2-02), with its driver running.
    fn new(agent: Agent, prefix: &[u8]) -> Arc<Rig> {
        Self::build(agent, prefix, false)
    }

    /// [`Self::new`], its output through a real [`AgentBus`] with Claude
    /// Code's screen profile when `on_bus` (A-20d-2).
    fn build(agent: Agent, prefix: &[u8], on_bus: bool) -> Arc<Rig> {
        let bus = on_bus.then(|| {
            Arc::new(AgentBus::profiled(
                Some(&crate::doorbell_screen::CLAUDE_PROFILE),
                24,
                80,
            ))
        });
        let input = bus.as_ref().map_or_else(
            || Arc::new(GenerationInput::default()),
            |bus| Arc::clone(bus.input()),
        );
        assert!(input.attach_records(Records::new(G, EcPolicy::Hold, Instant::now())));
        let hook = match &bus {
            Some(bus) => {
                bus.push(prefix.to_vec());
                None
            }
            None => {
                let mut hook = OutputHook::new(Arc::clone(&input));
                hook.on_output(prefix);
                Some(hook)
            }
        };
        assert!(input.attach_segments(G, agent));
        let slot = Arc::new(std::sync::OnceLock::new());
        let retired = Arc::new(AtomicBool::new(false));
        let writer = PaneWriter::new(
            Box::new(Pty {
                rig: Arc::clone(&slot),
            }),
            None,
            Arc::new(Mutex::new(PaneInputState::default())),
            Arc::clone(&retired),
            Arc::new(AtomicBool::new(false)),
            None,
            Arc::clone(&input),
        );
        let rig = Arc::new(Rig {
            writer: Arc::new(AsyncMutex::new(writer)),
            input,
            hook: Mutex::new(hook),
            bus,
            seen: Mutex::new(Vec::new()),
            react: Mutex::new(None),
            ledger: IngressLedger::new(),
            retired,
            mid_fired: std::sync::atomic::AtomicUsize::new(0),
            fault: Mutex::new(None),
            runtime: tokio::runtime::Handle::current(),
        });
        let _ = slot.set(Arc::downgrade(&rig));
        let log = Arc::downgrade(&rig);
        tokio::spawn(drive_segments(
            Arc::clone(&rig.writer),
            Arc::clone(&rig.input),
            Box::new(move |n| {
                if let Some(rig) = log.upgrade() {
                    rig.seen.lock().unwrap().push(Seen::Published(n));
                }
            }),
        ));
        rig
    }

    /// The bytes the PTY took, in order.
    fn taken(&self) -> Vec<u8> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter_map(|s| match s {
                Seen::Took(b) => Some(b.clone()),
                Seen::Published(_) => None,
            })
            .flatten()
            .collect()
    }

    /// A probe, admitted: its segment opened, its `0x07` queued with
    /// production's arm section ([`probe_section`]) closing its fence, and
    /// its job's end reported to its segment as `composer_probe` reports it
    /// ([`end_probe_segment`]). Admission is left out: at the section, the
    /// liability is raised as the observer's `arm_segment` raises it.
    /// `before` runs between the job's end and that report (R-U2-01).
    async fn probe(&self, before: impl FnOnce(&Rig)) -> u64 {
        self.probe_opened_at(Instant::now(), before).await
    }

    /// [`Self::probe`], its segment opened as if at `now`: a `now` past
    /// [`T_TRIGGER`] since a lone Claude probe's byte opens its lapse retry,
    /// which supersedes it (core §4.7) while its liability stays out.
    async fn probe_opened_at(&self, now: Instant, before: impl FnOnce(&Rig)) -> u64 {
        let (number, job) = self.input.segment_open_probe(false, now).unwrap();
        let claim = Arc::new(crate::doorbell_txn::JobClaim::default());
        let w = self.writer.lock().await;
        let section = {
            let input = Arc::clone(&self.input);
            probe_section(
                Arc::clone(&w.gate.gate),
                Arc::clone(&self.input),
                Arc::clone(&claim),
                || true,
                move |ctx, _, _| {
                    input
                        .liability_raise(G, ctx.now)
                        .map(drop)
                        .map_err(|_| ProbeRefusal::Retired)
                },
            )
        };
        let probe = w.queue_probe(job, claim, section, || {}).unwrap();
        drop(w);
        let end = probe.end().await;
        assert!(
            matches!(end, ProbeEnd::Written { .. }),
            "the probe wasn't written: {end:?}"
        );
        before(self);
        end_probe_segment(&self.input, number, end);
        number
    }

    /// A probe as [`Self::probe`] queues it, whose byte the PTY fails with
    /// `fault` while a person types `typed`, its end handled as
    /// `composer_probe` handles it ([`probe_ended`]): the arm undone by
    /// lowering the liability, as the observer's `disarm` does.
    async fn probe_faulted(&self, fault: ProbeFault, typed: &'static [u8]) -> ProbeOutcome {
        *self.fault.lock().unwrap() = Some((fault, typed));
        let now = Instant::now();
        let (number, job) = self.input.segment_open_probe(false, now).unwrap();
        let claim = Arc::new(crate::doorbell_txn::JobClaim::default());
        let w = self.writer.lock().await;
        let gate = Arc::clone(&w.gate.gate);
        let section = {
            let input = Arc::clone(&self.input);
            probe_section(
                Arc::clone(&gate),
                Arc::clone(&self.input),
                Arc::clone(&claim),
                || true,
                move |ctx, _, _| {
                    input
                        .liability_raise(G, ctx.now)
                        .map(drop)
                        .map_err(|_| ProbeRefusal::Retired)
                },
            )
        };
        let probe = w.queue_probe(job, claim, section, || {}).unwrap();
        drop(w);
        let end = probe.end().await;
        let input = Arc::clone(&self.input);
        probe_ended(
            &self.input,
            &gate,
            "oracle",
            number,
            end,
            ProbeAsk::First,
            move || {
                let _ = input.liability_lower(G, Instant::now());
            },
        )
    }

    /// A probe asked as `composer_probe` asks it, on a rig whose output goes
    /// through its bus (A-20d-2): its scheduling reads
    /// ([`scheduling_reads`], the bindings unchanged) asking the
    /// generation's own admission with the facts read under this writer's
    /// lock, then again with the frame and its mark read in one hold, a
    /// person's ticket as the ledger shows it, the gate's fence and the
    /// writer's boundary; the segment opened; the arm section
    /// ([`probe_section`]) with the writer's own facts ([`writer_facts`]:
    /// the screen's freshness against the mark, the ticket read under the
    /// gate's lock), asking admission again and raising the liability in
    /// one hold ([`GenerationInput::arm_probe`], the seam's arm); and its
    /// end handled by [`probe_ended`]. `between` runs after the scheduled
    /// admission and before the job is queued, the writer's lock held, as
    /// `composer_probe` holds it: what lands there lands between
    /// scheduling and the arm section. Answers the outcome, or the
    /// scheduled refusal.
    async fn probe_admitted(&self, between: impl FnOnce(&Rig)) -> ProbeOutcome {
        use crate::composer_probe::admission::{Attempt, Outside};
        let bus = Arc::clone(self.bus.as_ref().expect("a rig on a bus"));
        let gate = Arc::clone(&self.writer.lock().await.gate.gate);
        let outside = |w: &PaneWriter, surface: bool, now: Instant| {
            let status = gate.status();
            let (fence, _) = self
                .input
                .admission_fence(status.fenced, status.held_ops, now);
            Outside {
                attempt: Attempt::First,
                permission: crate::composer_probe::Permission::Armable,
                fence,
                human_ticket: self.ledger.any_outstanding(),
                boundary: w.physical_boundary(),
                keymap: Ok(()),
                surface,
            }
        };
        let (this, on_bus, asked) = (self, &bus, &outside);
        let (w, ()) = scheduling_reads(
            None::<std::future::Ready<()>>,
            || std::future::ready(()),
            move |()| async move {
                let surface = shows_composer(on_bus.clean_frame());
                let w = this.writer.lock().await;
                let now = Instant::now();
                let estimate = match this.input.admit_probe(&asked(&w, surface, now), now) {
                    Err(crate::composer_probe::admission::Refusal::Estimate(at)) => Some(at),
                    _ => None,
                };
                (w, estimate)
            },
        )
        .await;
        let marked = bus.clean_frame_marked();
        let mark = marked.as_ref().map(|(_, mark)| *mark);
        let now = Instant::now();
        let scheduled = outside(&w, shows_composer(marked.map(|(grid, _)| grid)), now);
        if let Err(refusal) = self.input.admit_probe(&scheduled, now) {
            self.input.probe_failed(now);
            return ProbeOutcome::Refused(format!("{refusal:?}"));
        }
        between(self);
        let held = gate.status().held_ops > 0;
        let (number, job) = self.input.segment_open_probe(held, now).unwrap();
        let claim = Arc::new(crate::doorbell_txn::JobClaim::default());
        let section = {
            let input = Arc::clone(&self.input);
            let (fresh, facts) =
                writer_facts(Arc::clone(&bus), mark, Arc::clone(&self.ledger), scheduled);
            probe_section(
                Arc::clone(&gate),
                Arc::clone(&self.input),
                Arc::clone(&claim),
                fresh,
                move |ctx, status, screen_unchanged| {
                    input
                        .arm_probe(G, &facts(ctx, status, screen_unchanged), ctx.now, |_| {})
                        .map(drop)
                        .map_err(ProbeRefusal::Admission)
                },
            )
        };
        let disarm = {
            let input = Arc::clone(&self.input);
            let gate = Arc::clone(&gate);
            move || {
                let _ = input.liability_lower(G, Instant::now());
                let _ = input.segment_withdrawn(number);
                gate.open_if_empty();
            }
        };
        let probe = w.queue_probe(job, claim, section, disarm).unwrap();
        drop(w);
        let end = probe.end().await;
        let input = Arc::clone(&self.input);
        probe_ended(
            &self.input,
            &gate,
            "oracle",
            number,
            end,
            ProbeAsk::First,
            move || {
                let _ = input.liability_lower(G, Instant::now());
            },
        )
    }

    async fn typed(&self, bytes: &[u8]) {
        self.typed_with(bytes, None).await;
    }

    /// [`Self::typed`], its connection told of a refusal through `notice`,
    /// as the attach loop's rejection route tells it.
    async fn typed_with(&self, bytes: &[u8], notice: Option<tokio::sync::mpsc::Sender<Vec<u8>>>) {
        let frame = UserFrame::new(bytes.to_vec(), Some(self.ledger.admit()), notice);
        let accepted = self.writer.lock().await.accept(frame, None).await;
        let what = match accepted {
            Accepted::Held | Accepted::Written(Ok(())) => return,
            Accepted::Written(Err(e)) => format!("a failed write: {e}"),
            Accepted::Full(..) => "no room".to_string(),
            Accepted::Retired(_) => "retired".to_string(),
        };
        panic!("{bytes:?} wasn't accepted: {what}");
    }

    fn out(&self, bytes: &[u8]) {
        match &self.bus {
            Some(bus) => bus.push(bytes.to_vec()),
            None => self
                .hook
                .lock()
                .unwrap()
                .as_mut()
                .expect("a rig with no bus has its own output hook")
                .on_output(bytes),
        }
    }

    /// A liability out with no segment for it (A-20d-5): the phantom
    /// liability core §4.7 says a lapsed Claude probe leaves once its retry
    /// superseded it (CL-10), raised here as the arm raises one.
    fn phantom(&self) {
        self.input
            .liability_raise(G, Instant::now())
            .expect("a live generation raises");
    }

    /// From now on an `Armed` empty read's settlement waits for its keymap
    /// re-read (A-20d-1a), as `Observer::start` registers it; the re-read
    /// is the script's [`Step::Recheck`].
    fn recheck_at_settlement(&self) {
        assert!(self.input.recheck_keymap_at_settlement(Arc::new(|| {})));
    }

    /// The observer's keymap re-read with no lock held, as
    /// `Observer::recheck_keymap` runs it: the tickets parked so far, then
    /// the verdict (the bindings unchanged) under the input's lock.
    fn recheck(&self) {
        if let Some(through) = self.input.keymap_recheck_due() {
            self.input
                .settle_keymap_recheck(through, Ok(()), Instant::now());
        }
    }

    /// A step that can land inside a driver pass, run as the observer or
    /// the output hook would run it there: no await, no writer lock.
    fn apply_sync(&self, step: &Step) {
        match step {
            Step::Observe(r) => self.observe(r),
            Step::Editor(pid) => self.editor(*pid),
            Step::Report(pid, report) => self.report(*pid, *report),
            Step::Exit(pid) => self.exit(*pid),
            Step::Out(b) => self.out(b),
            Step::Recheck => self.recheck(),
            other => panic!("{other:?} can't land inside a driver pass"),
        }
    }

    /// Arm `step` to land inside the next driver pass that finds the op
    /// `on_held` held, between its read of the front segment and its
    /// extraction of what is held (A-20d-3, O-20.3; R-B-U2-02's seam).
    async fn mid_pass(self: &Arc<Self>, on_held: Vec<u8>, step: Step) {
        let weak = Arc::downgrade(self);
        let w = self.writer.lock().await;
        let gate = w.gate();
        let mut step = Some(step);
        w.set_mid_pass(Box::new(move || {
            if !gate.gate.holds_any(|frame| frame.bytes == on_held) {
                return false;
            }
            if let (Some(rig), Some(step)) = (weak.upgrade(), step.take()) {
                rig.apply_sync(&step);
                rig.mid_fired.fetch_add(1, Ordering::SeqCst);
            }
            true
        }));
    }

    /// What the generation shows once a script is over, for comparing
    /// orders (A-20d-3): the bytes taken and numbers published, the
    /// liability count, the schedule (due, failures, broken), the open
    /// segments, the epoch and its foreign count, and the gate.
    async fn snapshot(&self) -> Snapshot {
        let now = Instant::now();
        let status = self.writer.lock().await.gate.gate.status();
        let published = self
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter_map(|s| match s {
                Seen::Published(n) => Some(*n),
                Seen::Took(_) => None,
            })
            .collect();
        Snapshot {
            taken: self.taken(),
            published,
            liability: self.input.liability_count(),
            schedule: self.input.advance_schedule_for_e2e(Duration::ZERO, now),
            segments: self
                .input
                .segments_view(now)
                .iter()
                .map(|v| (v.number, v.stage, v.record))
                .collect(),
            epoch: (
                self.input.epoch().epoch(),
                self.input.epoch().last_foreign(),
            ),
            gate: (status.fenced, status.held_ops, status.held_bytes),
        }
    }

    fn observe(&self, r: &MRecord) {
        let now = Instant::now();
        // The daemon's read of an `Armed` export arrives with the helper's
        // invocation on IPC, before its record (as `observe.rs` notes it).
        if let (Channel::Ipc, Some(Decision::Armed), Some(empty)) = (r.channel, r.decision, r.read)
        {
            self.input.note_read(key(r.pid), empty);
        }
        let _ = self.input.record_observe(
            &Observation {
                generation: G,
                key: key(r.pid),
                channel: r.channel,
                attribution: if r.owned {
                    Attribution::Owned
                } else {
                    Attribution::NotOwned(NotOwned::Tool)
                },
                claim: None,
                decision: r.decision,
                seg_seen: r.seg,
                native_seen: None,
                birth_floor: now,
            },
            now,
        );
    }

    fn editor(&self, pid: u32) {
        self.input.segment_editor_running(key(pid), Instant::now());
    }

    fn report(&self, pid: u32, report: Report) {
        let _ = self
            .input
            .record_report(G, key(pid), report, Instant::now());
    }

    fn exit(&self, pid: u32) {
        let now = Instant::now();
        let _ = self.input.record_exit(G, key(pid), now, now);
        let _ = self.input.record_drained(G, key(pid), now);
    }

    /// Arm helper `pid`'s drain with no report to land inside the next
    /// driver pass that finds typing held, between its read of the front
    /// segment and its extraction of what is held (R-B-U2-02).
    async fn exit_mid_pass(self: &Arc<Self>, pid: u32) {
        let weak = Arc::downgrade(self);
        let w = self.writer.lock().await;
        let gate = w.gate();
        w.set_mid_pass(Box::new(move || {
            if !gate.gate.holds_any(|_| true) {
                return false;
            }
            if let Some(rig) = weak.upgrade() {
                rig.exit(pid);
                rig.mid_fired.fetch_add(1, Ordering::SeqCst);
            }
            true
        }));
    }

    async fn retire(&self) {
        self.retired.store(true, Ordering::SeqCst);
        self.writer.lock().await.gate.retire();
        self.input.retire();
    }

    /// Wait for the driver and the writer to settle on `expected`: the PTY
    /// has taken that much, and no trigger's report is still on its way.
    /// Then a moment more, so a late release shows.
    async fn settle(&self, expected: &[u8]) {
        self.settle_within(expected, Duration::from_secs(3)).await;
    }

    /// [`Self::settle`], waiting at most `patience` for the bytes.
    async fn settle_within(&self, expected: &[u8], patience: Duration) {
        let deadline = Instant::now() + patience;
        loop {
            let submitted = self
                .input
                .segments_view(Instant::now())
                .iter()
                .any(|v| v.stage == Stage::Submitted);
            if !submitted && self.taken().len() >= expected.len() {
                break;
            }
            if Instant::now() > deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        tokio::time::sleep(Duration::from_millis(3)).await;
    }
}

fn key(pid: u32) -> RecordKey {
    RecordKey { pid, start: 1 }
}

/// What a rig showed at the end of a script ([`Rig::snapshot`]).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Snapshot {
    taken: Vec<u8>,
    published: Vec<u64>,
    liability: Option<u32>,
    schedule: Option<(bool, u32, bool)>,
    segments: Vec<(Option<u64>, Stage, Option<RecordKey>)>,
    epoch: (u64, u64),
    gate: (bool, usize, usize),
}

/// Whether `frame` shows Claude Code's main composer, as admission's screen
/// conjunct reads it: a test copy of the driver's private
/// `surface_shows_composer` for Claude Code (its positive identity: the
/// ready screen, no dialog over it, at a measured width), which
/// `probe_059`'s arm rig runs itself.
fn shows_composer(frame: Option<crate::doorbell_screen::Grid>) -> bool {
    let profile = &crate::doorbell_screen::CLAUDE_PROFILE;
    frame.is_some_and(|grid| {
        grid.cols >= profile.min_cols
            && (profile.dialog)(&grid).is_none()
            && (profile.ready)(&grid)
            && (profile.composer)(&grid).is_some()
    })
}

/// A Claude Code frame at `cols`: the full-width rules around an empty
/// composer, after Claude Code's full-redraw marker (as `probe_059`'s).
fn claude_frame(cols: u16) -> Vec<u8> {
    let rule = "─".repeat(cols as usize);
    format!(
        "\x1b[2J\x1b[H\x1b[20;1H{rule}\x1b[21;1H❯\u{a0}\x1b[22;1H{rule}\x1b[23;3Hstatus\x1b[21;3H"
    )
    .into_bytes()
}

// ---------------------------------------------------------------------------
// The model: core §4.6's release table over the script alone.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum MKind {
    Probe,
    HumanG,
}

/// A helper's record as the script made it.
#[derive(Debug, Clone)]
struct MRecord {
    pid: u32,
    owned: bool,
    channel: Channel,
    seg: Option<u64>,
    decision: Option<Decision>,
    report: Report,
    /// The daemon's read of an `Armed` export on IPC: whether it was empty.
    read: Option<bool>,
    drained: bool,
    /// Its report, when its exit drained, said the helper reaped its editor
    /// and accounted for the tree (`Done`, `Clean` or `Survivors`): the
    /// editor's end is known (A-20d-15). A report after the drain doesn't
    /// set it.
    done_at_drain: bool,
    /// Its editor child was seen running (the observer's one report).
    editor: bool,
    matched: bool,
    /// When it was born and when it exited, on the model's own clock.
    born: u64,
    exited: Option<u64>,
    /// Its completion is no longer pending: a marker route completed it, or
    /// a later owned invocation subsumed it (A2 §3.1, as `record.rs`'s
    /// "Completion" section states it: an `Owned` record born after its
    /// exit, its editor known to have ended), after which its segment holds
    /// until retirement (`segments.rs`'s header).
    completed: bool,
    subsumed: bool,
}

#[derive(Debug, Clone, Default)]
struct Watch {
    off: bool,
    on: bool,
    frame: bool,
}

#[derive(Debug, Clone)]
struct MSeg {
    kind: MKind,
    number: u64,
    /// The output offset its trigger was written at.
    written: u64,
    record: Option<usize>,
    released: bool,
    cut_off: bool,
    e1: Watch,
    /// The release-table rows it reached ([`Stats::rows`]), once each.
    rows: Vec<Row>,
}

/// The release table's view of one generation.
#[derive(Debug, Clone)]
struct Model {
    agent: Agent,
    records: Vec<MRecord>,
    segs: VecDeque<MSeg>,
    last: u64,
    /// Typed bytes the gate holds, in order.
    held: VecDeque<u8>,
    /// What the PTY may have taken by now, in order.
    expected: Vec<u8>,
    /// Each HumanG trigger: its number.
    triggers: Vec<u64>,
    /// Each HumanG trigger's write, in order: the Ctrl+G alone when the
    /// driver writes it, the bytes through the candidate when a cut opens it
    /// (core §4.3).
    trigger_writes: Vec<Vec<u8>>,
    /// The probes written, each one Ctrl+G before any trigger's.
    probes: usize,
    /// The liability count: one per probe armed and per phantom, one less
    /// per owned `Armed` decision while one is out (core §2.4).
    liability: u32,
    /// An `Armed` empty read's settlement waits for its keymap re-read
    /// (A-20d-1a): the rig registered it.
    parks: bool,
    /// The probe's empty read is parked, waiting for that re-read.
    parked: bool,
    /// A step armed to land inside the driver pass that finds `.0` held,
    /// which the model applies right after the step that typed it.
    deferred: Option<(Vec<u8>, Step)>,
    /// The writer scope opened for good (A3 §4.3, A5 §1.4, A6 §1.3, as
    /// `record.rs`'s "writer scope" section states it): a decision that
    /// isn't helper-only (`Unarmed`, a tool, `Unknown`), a decisionless exit
    /// drain, or an editor-bearing exit with no clean report. Its cause is
    /// sticky until retirement, so under `Hold` nothing completes after it.
    scope_open: bool,
    /// Output bytes fed so far: the offset the next byte is at.
    fed: u64,
    retired: bool,
    /// The probe's settlement: `None` until it settles.
    settled: Option<Settled>,
    stats: Stats,
    /// The model's clock: one tick per birth or exit.
    clock: u64,
}

/// How a probe settled the schedule (R3 §3.4): an `Armed` empty read of
/// the deck's own claim; another `Armed`; anything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Settled {
    Empty,
    NonEmpty,
    Failed,
}

/// One row of the release table a matched segment reached (R-B-U3-02):
/// the agent, the segment's kind, its record's decision, whether its editor
/// was seen running and whether its start failed, and whether the row
/// released.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Row {
    agent: &'static str,
    kind: MKind,
    decision: &'static str,
    editor: bool,
    spawn_failed: bool,
    releases: bool,
}

/// How much of the table a run reached: a generator that never releases,
/// cuts or completes proves nothing.
#[derive(Debug, Clone, Default)]
struct Stats {
    released: usize,
    passed: usize,
    cut_off: usize,
    completed: usize,
    /// `HumanG`s a cut opened with a liability out and no segment
    /// (A-20d-5).
    cut_opened: usize,
    /// Held Ctrl+G presses refused, with what followed them, behind a
    /// released segment that can never complete (R-HOLD's bounded refusal).
    refused: usize,
    /// Releases that stopped at a line-discipline key, Claude being out of
    /// raw mode (Q147b).
    cooked_held: usize,
    /// Each row reached, and by how many segments.
    rows: std::collections::BTreeMap<Row, usize>,
    /// Each kind of settlement reached.
    settled: std::collections::BTreeMap<Settled, usize>,
}

impl Stats {
    fn add(&mut self, other: &Stats) {
        self.released += other.released;
        self.passed += other.passed;
        self.cut_off += other.cut_off;
        self.completed += other.completed;
        self.cut_opened += other.cut_opened;
        self.refused += other.refused;
        self.cooked_held += other.cooked_held;
        for (row, n) in &other.rows {
            *self.rows.entry(*row).or_default() += n;
        }
        for (kind, n) in &other.settled {
            *self.settled.entry(*kind).or_default() += n;
        }
    }
}

impl Model {
    fn new(agent: Agent, prefix: usize) -> Self {
        Self {
            agent,
            records: Vec::new(),
            segs: VecDeque::new(),
            last: 0,
            held: VecDeque::new(),
            expected: Vec::new(),
            triggers: Vec::new(),
            trigger_writes: Vec::new(),
            probes: 0,
            liability: 0,
            parks: false,
            parked: false,
            deferred: None,
            scope_open: false,
            fed: prefix as u64,
            retired: false,
            settled: None,
            stats: Stats::default(),
            clock: 0,
        }
    }

    /// Whether the gate holds: a segment is open or typing is held.
    fn fenced(&self) -> bool {
        !self.segs.is_empty() || !self.held.is_empty()
    }

    fn probe(&mut self) {
        self.probes += 1;
        self.liability += 1;
        self.last += 1;
        self.segs.push_back(MSeg {
            kind: MKind::Probe,
            number: self.last,
            written: self.fed,
            record: None,
            released: false,
            cut_off: false,
            e1: Watch::default(),
            rows: Vec::new(),
        });
        self.expected.push(CTRL_G);
        self.step();
    }

    fn typed(&mut self, bytes: &[u8]) {
        if self.retired {
            return;
        }
        let candidate = bytes.iter().position(|&b| b == CTRL_G);
        if self.fenced() {
            self.held.extend(bytes);
        } else if let Some(at) = candidate.filter(|_| self.liability > 0) {
            // A-20d-5, core §4.3 and §4.5 (A1 §3.3: a person's Ctrl+G with
            // a liability out opens a `HumanG`, and isn't native): with no
            // segment open and a liability out, the op is cut after its
            // first candidate, which opens a `HumanG` segment. Its number is
            // published before its trigger write, the bytes through the
            // candidate; the rest is held behind it.
            self.last += 1;
            self.triggers.push(self.last);
            self.trigger_writes.push(bytes[..=at].to_vec());
            self.expected.extend(&bytes[..=at]);
            self.segs.push_back(MSeg {
                kind: MKind::HumanG,
                number: self.last,
                written: self.fed,
                record: None,
                released: false,
                cut_off: false,
                e1: Watch::default(),
                rows: Vec::new(),
            });
            self.held.extend(&bytes[at + 1..]);
            self.stats.cut_opened += 1;
        } else {
            self.expected.extend(bytes);
        }
        self.step();
    }

    /// The lone, unmatched Claude probe lapsed and its retry superseded it
    /// (core §4.7): it ends, its liability still out, and the retry is the
    /// next segment.
    fn lapse_retry(&mut self) {
        let lapsed = self.segs.pop_front().expect("a lapsed probe");
        assert!(
            self.agent == Agent::Claude
                && lapsed.kind == MKind::Probe
                && lapsed.record.is_none()
                && self.segs.is_empty()
                && self.held.is_empty(),
            "only a lone, unmatched Claude probe with nothing held lapses"
        );
        self.probe();
    }

    /// A liability out with no segment for it ([`Rig::phantom`]).
    fn phantom(&mut self) {
        self.liability += 1;
        self.step();
    }

    /// The observer's keymap re-read, the bindings unchanged: a parked
    /// empty read settles as one.
    fn recheck(&mut self) {
        if self.parked {
            self.parked = false;
            self.settled = Some(Settled::Empty);
            *self.stats.settled.entry(Settled::Empty).or_default() += 1;
        }
        self.step();
    }

    fn out(&mut self, bytes: &[u8]) {
        for (i, chunk) in [OFF, ON, FRAME].iter().enumerate() {
            if bytes == *chunk {
                let start = self.fed;
                if let Some(seg) = self.segs.front_mut()
                    && start >= seg.written
                {
                    let w = &mut seg.e1;
                    match i {
                        0 => {
                            *w = Watch {
                                off: true,
                                ..Watch::default()
                            }
                        }
                        1 if w.off && !w.on => w.on = true,
                        2 if w.on && !w.frame => w.frame = true,
                        _ => {}
                    }
                }
            }
        }
        self.fed += bytes.len() as u64;
        self.step();
    }

    fn observe(&mut self, mut r: MRecord) {
        // The same helper seen again, on another channel: one record, its
        // first decision kept; a second changes nothing (core §2.4).
        if self.records.iter().any(|known| known.pid == r.pid) {
            self.step();
            return;
        }
        // Settlement (core §2.4): an owned `Armed` decision consumes a
        // liability while one is out.
        if r.owned && r.decision == Some(Decision::Armed) && self.liability > 0 {
            self.liability -= 1;
        }
        self.clock += 1;
        r.born = self.clock;
        if let Some(d) = r.decision
            && matches!(d, Decision::Unarmed | Decision::Tool | Decision::Unknown)
        {
            self.scope_open = true;
        }
        self.records.push(r);
        self.step();
    }

    fn editor(&mut self, pid: u32) {
        if let Some(r) = self.records.iter_mut().find(|r| r.pid == pid) {
            r.editor = true;
        }
        self.step();
    }

    fn report(&mut self, pid: u32, report: Report) {
        if let Some(r) = self.records.iter_mut().find(|r| r.pid == pid)
            && r.report == Report::None
        {
            r.report = report;
            // `Records::report`: a tree the helper left running or couldn't
            // account for opens the scope when it is reported, before any
            // exit (which the refusal above reads; completion reads it only
            // after the drain, where the exit's rule below agrees).
            if matches!(report, Report::Done(Tree::Survivors | Tree::Unknown)) {
                self.scope_open = true;
            }
        }
        self.step();
    }

    fn exit(&mut self, pid: u32) {
        self.clock += 1;
        if let Some(r) = self.records.iter_mut().find(|r| r.pid == pid) {
            if !r.drained {
                r.done_at_drain = matches!(r.report, Report::Done(Tree::Clean | Tree::Survivors));
            }
            r.drained = true;
            r.exited = Some(self.clock);
            let editor_bearing = r.decision != Some(Decision::Armed);
            let ended = matches!(
                r.report,
                Report::Done(Tree::Clean) | Report::SpawnFailed { .. }
            );
            if r.decision.is_none() || (editor_bearing && !ended) {
                self.scope_open = true;
            }
        }
        self.step();
    }

    fn retire(&mut self) {
        self.retired = true;
        self.held.clear();
        self.segs.clear();
    }

    /// Match, release and complete what the script now allows, then what
    /// the held typing may do: core §4.5–§4.6 from the table.
    fn step(&mut self) {
        if self.retired {
            return;
        }
        // A2 §3.1 (`record.rs`, "Completion"): an exited, pending record
        // whose editor is known to have ended is subsumed by any other owned
        // record born after its exit.
        for i in 0..self.records.len() {
            let r = &self.records[i];
            let ended = r.decision == Some(Decision::Armed)
                || matches!(
                    r.report,
                    Report::SpawnFailed { .. } | Report::Done(Tree::Clean | Tree::Survivors)
                );
            if let Some(exit) = r.exited
                && r.owned
                && ended
                && !r.completed
                && !r.subsumed
                && self
                    .records
                    .iter()
                    .any(|s| s.pid != r.pid && s.owned && s.born > exit)
            {
                self.records[i].subsumed = true;
            }
        }
        while let Some(seg) = self.segs.front() {
            let number = seg.number;
            if seg.record.is_none() {
                // The one owned, unmatched record that read this number.
                let candidates: Vec<usize> = (0..self.records.len())
                    .filter(|&i| {
                        let r = &self.records[i];
                        r.owned && !r.matched && r.seg == Some(number)
                    })
                    .collect();
                if let [i] = candidates[..] {
                    self.records[i].matched = true;
                    self.segs.front_mut().unwrap().record = Some(i);
                }
            }
            let seg = self.segs.front().unwrap();
            let Some(ri) = seg.record else {
                break;
            };
            let r = self.records[ri].clone();
            let spawn_failed = matches!(r.report, Report::SpawnFailed { .. });
            let pass = !spawn_failed
                && match (self.agent, seg.kind, r.decision) {
                    (Agent::Claude, MKind::Probe, Some(Decision::Armed | Decision::Unarmed)) => {
                        true
                    }
                    (Agent::Claude, MKind::HumanG, Some(Decision::Armed)) => true,
                    // Once its editor runs, or, on Claude, once its helper
                    // reported the editor's end and drained (A-20d-15).
                    // Codex's needs its return past the drain frontier,
                    // which this rig never takes (no frontier rows), so
                    // there it holds.
                    (Agent::Claude, MKind::HumanG, Some(Decision::Unarmed)) => {
                        r.editor || r.done_at_drain
                    }
                    (_, MKind::HumanG, Some(Decision::Unarmed)) => r.editor,
                    _ => false,
                };
            let unknown_end = r.drained
                && r.decision != Some(Decision::Armed)
                && matches!(r.report, Report::None | Report::Unrecognised);
            let row = Row {
                agent: agent_name(self.agent),
                kind: seg.kind,
                decision: decision_name(r.decision),
                editor: r.editor,
                spawn_failed,
                releases: pass,
            };
            let seg = self.segs.front_mut().unwrap();
            if !seg.rows.contains(&row) {
                seg.rows.push(row);
                *self.stats.rows.entry(row).or_default() += 1;
            }
            if pass && !seg.released {
                seg.released = true;
                self.stats.released += 1;
            }
            if seg.released && unknown_end && !seg.cut_off {
                seg.cut_off = true;
                self.stats.cut_off += 1;
            }
            // The probe's settlement (R3 §3.4): its matched decision, once
            // an `Armed` one's read is known or its record drained. Every
            // matched record is owned with no other claim, so an empty read
            // is the deck's own.
            if seg.kind == MKind::Probe
                && self.settled.is_none()
                && !self.parked
                && let Some(d) = r.decision
                && (d != Decision::Armed || r.drained || r.read.is_some())
            {
                let kind = match (d, r.read) {
                    (Decision::Armed, Some(true)) => Settled::Empty,
                    (Decision::Armed, _) => Settled::NonEmpty,
                    _ => Settled::Failed,
                };
                if kind == Settled::Empty && self.parks {
                    self.parked = true;
                } else {
                    self.settled = Some(kind);
                    *self.stats.settled.entry(kind).or_default() += 1;
                }
            }
            let helper_only = r.decision == Some(Decision::Armed) || spawn_failed;
            let returned = match self.agent {
                Agent::Claude => seg.e1.on,
                Agent::Codex => seg.e1.frame,
            };
            if r.drained && helper_only && returned && !self.scope_open && !r.subsumed {
                self.records[ri].completed = true;
                self.segs.pop_front();
                self.stats.completed += 1;
                continue;
            }
            break;
        }
        // What the held typing may do now.
        let candidate = self.held.iter().position(|&b| b == CTRL_G);
        match self.segs.front() {
            None => {
                let upto = candidate.unwrap_or(self.held.len());
                self.expected.extend(self.held.drain(..upto));
                if candidate.is_none() {
                    return;
                }
                // The candidate alone, as the next segment's trigger.
                self.held.pop_front();
                self.expected.push(CTRL_G);
                self.last += 1;
                self.triggers.push(self.last);
                self.trigger_writes.push(vec![CTRL_G]);
                self.segs.push_back(MSeg {
                    kind: MKind::HumanG,
                    number: self.last,
                    written: self.fed,
                    record: None,
                    released: false,
                    cut_off: false,
                    e1: Watch::default(),
                    rows: Vec::new(),
                });
            }
            Some(seg) if seg.released && !seg.cut_off => {
                // Q147b: a Claude segment released on an `Armed` decision
                // goes to a Claude whose terminal is out of raw mode until
                // it returns (its helper drained and its `?2004h` after the
                // trigger read): there a byte the cooked line discipline
                // acts on (a Ctrl+C is a SIGINT, an Enter becomes a line
                // feed, a Backspace erases) waits, with everything after
                // it, for that return.
                let cooked = self.agent == Agent::Claude
                    && seg.record.is_some_and(|ri| {
                        let r = &self.records[ri];
                        r.decision == Some(Decision::Armed) && !(r.drained && seg.e1.on)
                    });
                let key = cooked
                    .then(|| self.held.iter().position(|&b| line_discipline_key(b)))
                    .flatten();
                let upto = match (candidate, key) {
                    (Some(c), Some(k)) => c.min(k),
                    (c, k) => c.or(k).unwrap_or(self.held.len()),
                };
                if upto > 0 {
                    self.stats.passed += 1;
                }
                if key.is_some_and(|k| k == upto) {
                    self.stats.cooked_held += 1;
                }
                self.expected.extend(self.held.drain(..upto));
                // R-HOLD's bounded refusal (the user's ruling, 2026-11-03):
                // with the scope open this segment never completes, so the
                // Ctrl+G now first in what is held, and everything after it,
                // is refused and never written: once the release has gone
                // up to it, which a held line-discipline key before it
                // defers until Claude's return.
                if candidate.is_some() && self.scope_open && !cooked {
                    self.held.clear();
                    self.stats.refused += 1;
                }
            }
            Some(_) => {}
        }
    }

    /// The number the next helper should read to match the front segment.
    fn front(&self) -> Option<u64> {
        self.segs.front().map(|s| s.number)
    }
}

fn agent_name(agent: Agent) -> &'static str {
    match agent {
        Agent::Claude => "Claude",
        Agent::Codex => "Codex",
    }
}

fn decision_name(decision: Option<Decision>) -> &'static str {
    match decision {
        None => "none",
        Some(Decision::Armed) => "Armed",
        Some(Decision::Unarmed) => "Unarmed",
        Some(Decision::Tool) => "Tool",
        Some(Decision::Unknown) => "Unknown",
    }
}

impl Judged {
    /// How long a step waits for the bytes the model allows: a byte the
    /// table allows that never comes fails `Exactly` once that wait is over,
    /// and costs `NothingEarly` that wait at every step.
    fn patience(self) -> Duration {
        match self {
            Judged::Exactly => Duration::from_secs(3),
            Judged::NothingEarly => Duration::from_millis(100),
        }
    }
}

/// How [`check_as`] judges what the PTY took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Judged {
    /// Exactly what the table allows, each trigger after its own number:
    /// nothing early and nothing missing.
    Exactly,
    /// Authority alone: nothing the table doesn't allow yet. A byte still
    /// missing, or a trigger whose number was never published, passes.
    NothingEarly,
}

/// The checks after every step: the PTY took what the model allows, as
/// `judged` says, each trigger as the model writes it and after its number
/// (either way a Ctrl+G is written only as the end of a write: alone, or as
/// the end of a cut's trigger write); the liability count is the model's;
/// the schedule is due while the probe is unsettled (and not while its
/// empty read is parked for its keymap re-read), and as the step that
/// settles it ends, shows how it settled (U3-5): an empty read makes it due
/// again at once, a non-empty one waits with no failure counted, a failed
/// check waits as a retry. Later steps can move the epoch, which may make it
/// due again.
fn check_as(rig: &Rig, model: &Model, settled_before: Option<Settled>, judged: Judged, what: &str) {
    let taken = rig.taken();
    let allowed = match judged {
        Judged::Exactly => taken == model.expected,
        Judged::NothingEarly => model.expected.starts_with(&taken),
    };
    assert!(
        allowed,
        "{what}: the PTY took {:?} where the release table allows {:?} ({judged:?})\n\
         segments {:#?}\ngate {:?}\nmodel {:#?}",
        String::from_utf8_lossy(&taken),
        String::from_utf8_lossy(&model.expected),
        rig.input.segments_view(Instant::now()),
        rig.input.segments_front(),
        model.segs,
    );
    let seen = rig.seen.lock().unwrap().clone();
    let mut published = Vec::new();
    let mut writes = Vec::new();
    for s in &seen {
        match s {
            Seen::Published(n) => published.push(*n),
            Seen::Took(b) if b.contains(&CTRL_G) => {
                assert!(
                    b.iter().filter(|&&c| c == CTRL_G).count() == 1 && b.ends_with(&[CTRL_G]),
                    "{what}: a Ctrl+G went with bytes after it: {b:?}"
                );
                writes.push((b.clone(), published.last().copied()));
            }
            Seen::Took(_) => {}
        }
    }
    // The first Ctrl+Gs are the probes' own, each alone; each after them is
    // a trigger, written as the model says, after its own number was
    // published.
    for (probe, _) in writes.iter().take(model.probes) {
        assert_eq!(probe, &[CTRL_G], "{what}: a probe's byte goes alone");
    }
    let humang: Vec<_> = writes.iter().skip(model.probes).cloned().collect();
    let want: Vec<_> = model
        .trigger_writes
        .iter()
        .cloned()
        .zip(model.triggers.iter().map(|&n| Some(n)))
        .collect();
    if judged == Judged::Exactly {
        assert_eq!(humang, want, "{what}: each trigger after its own number");
    }
    if model.retired {
        return;
    }
    assert_eq!(
        rig.input.liability_count(),
        Some(model.liability),
        "{what}: the liability count"
    );
    match (settled_before, model.settled) {
        (_, None) if model.parked => assert!(
            !rig.input.probe_due(Instant::now()) && rig.input.settling(),
            "{what}: an empty read parked for its keymap re-read is a check outstanding"
        ),
        (_, None) => assert!(
            rig.input.probe_due(Instant::now()),
            "{what}: the probe is unsettled, so its schedule is as it was"
        ),
        (None, Some(kind)) => {
            let due = rig.input.probe_due(Instant::now());
            let retrying = rig.input.probe_retrying();
            let want = match kind {
                Settled::Empty => (true, false),
                Settled::NonEmpty => (false, false),
                Settled::Failed => (false, true),
            };
            assert_eq!(
                (due, retrying),
                want,
                "{what}: the probe settled {kind:?}: (due, retrying)"
            );
        }
        (Some(_), Some(_)) => {}
    }
}

// ---------------------------------------------------------------------------
// The script.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Step {
    Type(Vec<u8>),
    Observe(MRecord),
    Editor(u32),
    Report(u32, Report),
    Exit(u32),
    /// Helper `pid` drains with no report inside the next driver pass that
    /// finds typing held, after the pass read the front segment and before
    /// it takes what is held (R-B-U2-02). For the model, it drains now.
    ExitMidPass(u32),
    Out(&'static [u8]),
    /// The agent answers the next trigger inside its write: its return, and
    /// when `helper` is set a helper that reads its number, decides
    /// `Unarmed` and runs the user's editor (R-U2-01).
    Fast {
        helper: Option<u32>,
    },
    Retire,
    /// A liability out with no segment for it (A-20d-5, [`Rig::phantom`]).
    Phantom,
    /// The lone Claude probe has lapsed: its retry is opened past
    /// [`T_TRIGGER`] and written, superseding it (core §4.7), its liability
    /// still out ([`Rig::probe_opened_at`]).
    LapseRetry,
    /// The observer's keymap re-read settles the empty reads parked so far
    /// (A-20d-1a, [`Rig::recheck`]).
    Recheck,
    /// `step` lands inside the next driver pass that finds the op `on_held`
    /// held, between its read of the front segment and its extraction of
    /// what is held (A-20d-3). For the model, it lands right after the step
    /// that typed `on_held`.
    MidPass {
        on_held: Vec<u8>,
        step: Box<Step>,
    },
}

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

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

/// A script of `len` steps, each chosen against the model as it stands, so
/// that its records mostly target the open segment (positive rows) and
/// sometimes a stale number or another owner (negatives).
fn generate(rng: &mut Rng, model: &mut Model, len: usize) -> Vec<Step> {
    let mut steps = Vec::new();
    let mut next_pid = 100;
    let mut pending = None;
    // How often a helper opens the user's editor (or a tool's): either
    // opens the writer scope for good under `Hold`, after which nothing
    // completes, so some scripts keep to `Armed` helpers.
    let editor_rate: u64 = [0, 15, 45, 70][rng.below(4) as usize];
    // Most scripts open with a directed prelude, since a random script
    // seldom completes its probe before an editor opens the writer scope:
    // a Ctrl+G held behind the probe, the probe's own `Armed` helper and
    // return completing it, so the Ctrl+G becomes the next segment's
    // trigger, and then that trigger's own helper, deciding `Armed`,
    // `Unarmed` or a tool, its editor seen running or not, its start failed
    // or not. Others open with the probe's own helper deciding, for the
    // probe's rows and its settlements. The model judges every step of
    // either as any other.
    let prelude = rng.below(100);
    if prelude < 25
        && let Some(probe) = model.front()
    {
        next_pid += 1;
        let decision = [Decision::Armed, Decision::Unarmed, Decision::Tool][rng.below(3) as usize];
        let mut own = helper(next_pid, probe, decision);
        if let Step::Observe(r) = &mut own {
            r.read = (decision == Decision::Armed)
                .then(|| daemon_read(rng))
                .flatten();
        }
        let mut directed = vec![own];
        if decision == Decision::Unarmed && rng.chance(50) {
            directed.push(Step::Editor(next_pid));
        }
        for step in directed {
            apply_model(model, &step, &mut pending);
            steps.push(step);
        }
    } else if prelude < 70
        && let Some(probe) = model.front()
    {
        next_pid += 1;
        let mut own = helper(next_pid, probe, Decision::Armed);
        if let Step::Observe(r) = &mut own {
            r.read = daemon_read(rng);
        }
        let mut prelude = vec![
            Step::Type(vec![b'a', CTRL_G, b'b']),
            own,
            Step::Exit(next_pid),
        ];
        prelude.extend(fast_return(model.agent).into_iter().map(Step::Out));
        for step in prelude {
            apply_model(model, &step, &mut pending);
            steps.push(step);
        }
        if let Some(trigger) = model.front() {
            next_pid += 1;
            let decision = [
                Decision::Armed,
                Decision::Unarmed,
                Decision::Unarmed,
                Decision::Tool,
            ][rng.below(4) as usize];
            let mut directed = vec![helper(next_pid, trigger, decision)];
            if decision == Decision::Unarmed && rng.chance(60) {
                directed.push(Step::Editor(next_pid));
            }
            if rng.chance(25) {
                directed.push(Step::Report(
                    next_pid,
                    Report::SpawnFailed { errno: Some(2) },
                ));
            }
            directed.push(Step::Type(b"cd".to_vec()));
            for step in directed {
                apply_model(model, &step, &mut pending);
                steps.push(step);
            }
        }
    }
    for i in 0..len {
        if model.retired {
            break;
        }
        // Retirement only in a script's last quarter, so most scripts run
        // long enough to reach a held Ctrl+G's segment.
        let pick = if i < len * 3 / 4 {
            rng.below(98)
        } else {
            rng.below(100)
        };
        if pick < 12
            && let Some(front) = model.front()
        {
            // The front segment's own helper decides `Armed`, exits, and
            // the agent returns: a positive control for completion.
            next_pid += 1;
            let mut own = helper(next_pid, front, Decision::Armed);
            if let Step::Observe(r) = &mut own {
                r.read = daemon_read(rng);
            }
            let mut batch = vec![own, Step::Exit(next_pid)];
            batch.extend(fast_return(model.agent).into_iter().map(Step::Out));
            // The return before the exit, or after it.
            if rng.chance(50) {
                let exit = batch.remove(1);
                batch.push(exit);
            }
            for step in batch {
                apply_model(model, &step, &mut pending);
                steps.push(step);
            }
            continue;
        }
        let step = if pick < 30 {
            let n = 1 + rng.below(4) as usize;
            // Three letters stand for keys the cooked line discipline acts
            // on (Q147b): a Ctrl+C, an Enter and a Backspace. Mapped, not
            // drawn, so every seed's draws are what they were before.
            let mut bytes: Vec<u8> = (0..n)
                .map(|_| match b'a' + rng.below(26) as u8 {
                    b'q' => 0x03,
                    b'r' => b'\r',
                    b'z' => 0x7f,
                    letter => letter,
                })
                .collect();
            // At most one candidate per frame, and only while the gate holds
            // (a segment open or typing held) or a liability is out (the
            // cut-open path, A-20d-5): a Ctrl+G typed with nothing out is
            // native and written whole (the model's first correction),
            // which this generator doesn't script.
            if (model.fenced() || model.liability > 0) && rng.chance(50) {
                let at = rng.below(n as u64 + 1) as usize;
                bytes.insert(at, CTRL_G);
            }
            Step::Type(bytes)
        } else if pick < 52 {
            next_pid += 1;
            let front = model.front();
            let seg = match rng.below(10) {
                0 => front.map(|n| n.saturating_sub(1)),
                1 => Some(999),
                _ => front,
            };
            let decision = match rng.below(100) {
                n if n < editor_rate.saturating_sub(5) => Decision::Unarmed,
                n if n < editor_rate => Decision::Tool,
                _ => Decision::Armed,
            };
            let channel = if rng.chance(50) {
                Channel::Ipc
            } else {
                Channel::Marker
            };
            Step::Observe(MRecord {
                pid: next_pid,
                owned: !rng.chance(10),
                channel,
                seg,
                decision: Some(decision),
                report: Report::None,
                read: (channel == Channel::Ipc && decision == Decision::Armed)
                    .then(|| daemon_read(rng))
                    .flatten(),
                drained: false,
                done_at_drain: false,
                editor: false,
                matched: false,
                born: 0,
                exited: None,
                completed: false,
                subsumed: false,
            })
        } else if pick < 60 {
            let waiting: Vec<u32> = model
                .records
                .iter()
                .filter(|r| r.decision == Some(Decision::Unarmed) && !r.drained && !r.editor)
                .map(|r| r.pid)
                .collect();
            match waiting.last() {
                Some(&pid) => Step::Editor(pid),
                None => continue,
            }
        } else if pick < 67 {
            let open: Vec<u32> = model
                .records
                .iter()
                .filter(|r| !r.drained && r.report == Report::None)
                .map(|r| r.pid)
                .collect();
            match open.last() {
                Some(&pid) => {
                    let report = match rng.below(3) {
                        0 => Report::SpawnFailed { errno: Some(2) },
                        _ => Report::Done(Tree::Clean),
                    };
                    Step::Report(pid, report)
                }
                None => continue,
            }
        } else if pick < 77 {
            // Half the time, the released front segment's own helper drains
            // with no report while its editor may still run: the unknown
            // wait that cuts the release off (R-U2-06), generated directly.
            let unknown = model
                .segs
                .front()
                .filter(|s| s.released && !s.cut_off)
                .and_then(|s| s.record)
                .map(|i| &model.records[i])
                .filter(|r| {
                    !r.drained && r.report == Report::None && r.decision != Some(Decision::Armed)
                })
                .map(|r| r.pid);
            let open: Vec<u32> = model
                .records
                .iter()
                .filter(|r| !r.drained)
                .map(|r| r.pid)
                .collect();
            match unknown {
                Some(pid) if rng.chance(50) => Step::Exit(pid),
                _ if open.is_empty() => continue,
                _ => Step::Exit(open[rng.below(open.len() as u64) as usize]),
            }
        } else if (92..95).contains(&pick) {
            // A phantom: a liability out with no segment for it (A-20d-5),
            // so a later Ctrl+G typed with no segment open cuts its op.
            Step::Phantom
        } else if pick < 95 {
            Step::Out(match rng.below(4) {
                0 => OFF,
                1 => ON,
                2 => FRAME,
                _ => b"text",
            })
        } else if pick < 98 {
            next_pid += 1;
            Step::Fast {
                helper: rng.chance(50).then_some(next_pid),
            }
        } else {
            Step::Retire
        };
        apply_model(model, &step, &mut pending);
        steps.push(step);
    }
    steps
}

/// The daemon's read of an `Armed` export on IPC, when it arrives: empty
/// or not.
fn daemon_read(rng: &mut Rng) -> Option<bool> {
    rng.chance(70).then(|| rng.chance(50))
}

/// The model's side of `step`. A `Fast` step waits for the next trigger:
/// `pending` carries it until one is cut.
fn apply_model(model: &mut Model, step: &Step, pending: &mut Option<Option<u32>>) {
    let before = model.triggers.len();
    match step {
        Step::Type(b) => model.typed(b),
        Step::Observe(r) => model.observe(r.clone()),
        Step::Editor(pid) => model.editor(*pid),
        Step::Report(pid, report) => model.report(*pid, *report),
        Step::Exit(pid) | Step::ExitMidPass(pid) => model.exit(*pid),
        Step::Out(b) => model.out(b),
        Step::Fast { helper } => *pending = Some(*helper),
        Step::Retire => model.retire(),
        Step::Phantom => model.phantom(),
        Step::LapseRetry => model.lapse_retry(),
        Step::Recheck => model.recheck(),
        Step::MidPass { on_held, step } => {
            model.deferred = Some((on_held.clone(), (**step).clone()));
        }
    }
    fast_answers(model, before, pending);
    if let Step::Type(typed) = step
        && model.deferred.as_ref().is_some_and(|(on, _)| on == typed)
        && let Some((_, inner)) = model.deferred.take()
    {
        apply_model(model, &inner, pending);
    }
}

/// A trigger cut while a `Fast` answer waits: the agent's return and helper,
/// as the PTY's reaction runs them.
fn fast_answers(model: &mut Model, before: usize, pending: &mut Option<Option<u32>>) {
    if model.triggers.len() > before
        && let Some(helper) = pending.take()
    {
        let number = *model.triggers.last().unwrap();
        if let Some(pid) = helper {
            model.observe(fast_helper(pid, number));
            model.editor(pid);
        }
        for m in fast_return(model.agent) {
            model.out(m);
        }
    }
}

fn fast_helper(pid: u32, number: u64) -> MRecord {
    MRecord {
        pid,
        owned: true,
        channel: Channel::Ipc,
        seg: Some(number),
        decision: Some(Decision::Unarmed),
        report: Report::None,
        read: None,
        drained: false,
        done_at_drain: false,
        editor: false,
        matched: false,
        born: 0,
        exited: None,
        completed: false,
        subsumed: false,
    }
}

fn fast_return(agent: Agent) -> Vec<&'static [u8]> {
    match agent {
        Agent::Claude => vec![OFF, ON],
        Agent::Codex => vec![OFF, ON, FRAME],
    }
}

/// Run `steps` on a fresh rig and model, checking after each.
async fn run(agent: Agent, prefix: &[u8], fast_probe: bool, steps: &[Step], what: &str) -> Model {
    run_with(Opts::default(), agent, prefix, fast_probe, steps, what)
        .await
        .0
}

/// How [`run_with`] sets a rig up and judges it.
#[derive(Debug, Clone, Copy)]
struct Opts {
    /// An empty read's settlement waits for its keymap re-read (A-20d-1a).
    recheck: bool,
    judged: Judged,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            recheck: false,
            judged: Judged::Exactly,
        }
    }
}

/// [`run`] as `opts` says, answering the model and what the rig showed at
/// the end.
async fn run_with(
    opts: Opts,
    agent: Agent,
    prefix: &[u8],
    fast_probe: bool,
    steps: &[Step],
    what: &str,
) -> (Model, Snapshot) {
    if std::env::var_os("DAD_ORACLE_TRACE").is_some() {
        eprintln!("{what}: fast probe {fast_probe}, prefix {prefix:?}");
        for (i, step) in steps.iter().enumerate() {
            eprintln!("  {i}: {step:?}");
        }
    }
    let rig = Rig::new(agent, prefix);
    let mut model = Model::new(agent, prefix.len());
    if opts.recheck {
        rig.recheck_at_settlement();
        model.parks = true;
    }
    let mut pending: Option<Option<u32>> = None;
    // The probe; with `fast_probe`, Claude's return is read before its
    // written report (R-U2-01).
    let return_first = fast_return(agent);
    rig.probe(|rig| {
        if fast_probe {
            for m in &return_first {
                rig.out(m);
            }
        }
    })
    .await;
    model.probe();
    if fast_probe {
        for m in &return_first {
            model.out(m);
        }
    }
    rig.settle(&model.expected).await;
    check_as(
        &rig,
        &model,
        None,
        opts.judged,
        &format!("{what}: the probe"),
    );
    for (i, step) in steps.iter().enumerate() {
        match step {
            Step::Type(b) => rig.typed(b).await,
            Step::Observe(r) => rig.observe(r),
            Step::Editor(pid) => rig.editor(*pid),
            Step::Report(pid, report) => rig.report(*pid, *report),
            Step::Exit(pid) => rig.exit(*pid),
            Step::ExitMidPass(pid) => rig.exit_mid_pass(*pid).await,
            Step::Out(b) => rig.out(b),
            Step::Fast { helper } => {
                let helper = *helper;
                let answer = fast_return(agent);
                *rig.react.lock().unwrap() = Some(Box::new(move |rig: &Rig| {
                    let number = rig
                        .seen
                        .lock()
                        .unwrap()
                        .iter()
                        .rev()
                        .find_map(|s| match s {
                            Seen::Published(n) => Some(*n),
                            Seen::Took(_) => None,
                        })
                        .unwrap();
                    if let Some(pid) = helper {
                        rig.observe(&fast_helper(pid, number));
                        rig.editor(pid);
                    }
                    for m in &answer {
                        rig.out(m);
                    }
                }));
            }
            Step::Retire => rig.retire().await,
            Step::Phantom => rig.phantom(),
            Step::LapseRetry => {
                rig.probe_opened_at(
                    Instant::now() + T_TRIGGER + Duration::from_millis(1),
                    |_| {},
                )
                .await;
            }
            Step::Recheck => rig.recheck(),
            Step::MidPass { on_held, step } => {
                rig.mid_pass(on_held.clone(), (**step).clone()).await;
            }
        }
        let settled = model.settled;
        apply_model(&mut model, step, &mut pending);
        rig.settle_within(&model.expected, opts.judged.patience())
            .await;
        check_as(
            &rig,
            &model,
            settled,
            opts.judged,
            &format!("{what}: step {i} {step:?}"),
        );
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
    check_as(
        &rig,
        &model,
        model.settled,
        opts.judged,
        &format!("{what}: at rest"),
    );
    let armed = steps
        .iter()
        .filter(|s| matches!(s, Step::ExitMidPass(_) | Step::MidPass { .. }))
        .count();
    assert_eq!(
        rig.mid_fired.load(Ordering::SeqCst),
        armed,
        "{what}: each mid-pass step landed inside a driver pass with its op held"
    );
    let snapshot = rig.snapshot().await;
    (model, snapshot)
}

/// What a run of seeds reached.
#[derive(Debug, Default)]
struct Reached {
    stats: Stats,
    triggers: usize,
    retired: usize,
}

/// Generate and run one script per seed in `seeds`, alternating Claude and
/// Codex.
async fn run_seeds(seeds: std::ops::RangeInclusive<u64>) -> Reached {
    let mut reached = Reached::default();
    for seed in seeds {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ seed.wrapping_mul(0x2545_f491_4f6c_dd1d));
        let agent = if seed % 2 == 0 {
            Agent::Codex
        } else {
            Agent::Claude
        };
        let prefix_len = rng.below(64) as usize;
        let mut prefix: Vec<u8> = (0..prefix_len).map(|_| b'x').collect();
        // Output that ends part-way through an escape (R-U2-02).
        if rng.chance(50) {
            prefix.extend_from_slice(b"\x1b[?20");
        }
        let fast_probe = rng.chance(30);
        let mut model = Model::new(agent, prefix.len());
        model.probe();
        if fast_probe {
            for m in fast_return(agent) {
                model.out(m);
            }
        }
        let steps = generate(&mut rng, &mut model, 40);
        let what = format!("seed {seed} {agent:?}");
        let m = run(agent, &prefix, fast_probe, &steps, &what).await;
        reached.stats.add(&m.stats);
        reached.triggers += m.triggers.len();
        reached.retired += usize::from(m.retired);
    }
    reached
}

/// The release-table tuples a run must reach (R-B-U3-02): `(agent, kind,
/// decision, editor seen, start failed, releases)`, a selection of the
/// table, not all of it (see the module documentation). Under `EcPolicy::Hold`
/// no row completes by EC, and the Authority policy's EC rows are out of
/// this rig's reach (see the module documentation).
const ROWS: &[(&str, MKind, &str, bool, bool, bool)] = &[
    // Claude: a probe releases at either decision; a held Ctrl+G at
    // `Armed`, and at `Unarmed` once its editor runs.
    ("Claude", MKind::Probe, "Armed", false, false, true),
    ("Claude", MKind::Probe, "Unarmed", false, false, true),
    ("Claude", MKind::HumanG, "Armed", false, false, true),
    ("Claude", MKind::HumanG, "Unarmed", false, false, false),
    ("Claude", MKind::HumanG, "Unarmed", true, false, true),
    // Codex: only a held Ctrl+G whose editor runs releases before E1F.
    ("Codex", MKind::Probe, "Armed", false, false, false),
    ("Codex", MKind::Probe, "Unarmed", false, false, false),
    ("Codex", MKind::HumanG, "Armed", false, false, false),
    ("Codex", MKind::HumanG, "Unarmed", false, false, false),
    ("Codex", MKind::HumanG, "Unarmed", true, false, true),
    // A tool's editor releases nothing, on either agent.
    ("Claude", MKind::HumanG, "Tool", false, false, false),
    ("Codex", MKind::HumanG, "Tool", false, false, false),
    // A failed start releases nothing before its completion, even with the
    // editor seen running before the failure was reported.
    ("Codex", MKind::HumanG, "Unarmed", true, true, false),
    ("Codex", MKind::HumanG, "Tool", false, true, false),
];

/// Assert that `reached` reached each tuple of [`ROWS`], each kind of step
/// and each kind of settlement at least `floor` times.
fn assert_reached(reached: &Reached, floor: usize) {
    let r = &reached.stats;
    eprintln!(
        "reached: {} releases, {} passed, {} cut off, {} completions, {} triggers, \
         {} cut-opened, {} refused, {} line-discipline holds, {} retirements, settled {:?}",
        r.released,
        r.passed,
        r.cut_off,
        r.completed,
        reached.triggers,
        r.cut_opened,
        r.refused,
        r.cooked_held,
        reached.retired,
        r.settled
    );
    for (row, n) in &r.rows {
        eprintln!("  row {row:?}: {n}");
    }
    for &(agent, kind, decision, editor, spawn_failed, releases) in ROWS {
        let row = Row {
            agent,
            kind,
            decision,
            editor,
            spawn_failed,
            releases,
        };
        let n = r.rows.get(&row).copied().unwrap_or(0);
        assert!(n >= floor, "only {n} segments reached {row:?}");
    }
    for (what, n) in [
        ("releases", r.released),
        ("bytes passed to a released segment", r.passed),
        ("unknown endings cutting a release off", r.cut_off),
        ("completions", r.completed),
        ("triggers", reached.triggers),
        ("HumanGs a cut opened", r.cut_opened),
        (
            "Ctrl+G presses refused behind a segment that can never complete",
            r.refused,
        ),
        (
            "releases stopped at a line-discipline key on a Claude out of raw mode",
            r.cooked_held,
        ),
        ("retirements", reached.retired),
    ] {
        assert!(n >= floor, "only {n} {what} across the seeds");
    }
    for kind in [Settled::Empty, Settled::NonEmpty, Settled::Failed] {
        let n = r.settled.get(&kind).copied().unwrap_or(0);
        assert!(n >= floor, "only {n} probes settled {kind:?}");
    }
}

/// Scenario: Generate scripts against the release table, one generation each, Claude and Codex: a probe whose fence closes in production's arm section, then typing with and without a Ctrl+G, a liability raised with no segment for it (a phantom) so that a Ctrl+G typed with no segment open opens a `HumanG` from its cut, helpers that read the open segment's number, a stale one or none, decide `Armed` (with the daemon's read of the box, empty or not, or none), `Unarmed` or a tool, run the user's editor, report, and exit, return markers, an agent that answers a trigger before its written report arrives, and retirement, after output that ran before the segments attached. After every step, once the production driver has had its chance, the PTY has taken exactly what the table allows: nothing early, nothing missing, each Ctrl+G alone after its number, nothing after retirement, and the probe's schedule shows how its decision settled exactly when it does. This is the fast tier's share; lane 1 runs a longer one.
#[spec("inbox/probe/010")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_010_releases_follow_the_release_table_on_the_production_driver() {
    let reached = run_seeds(1..=FAST_SEEDS).await;
    assert_reached(&reached, 1);
}

/// Scenario: The generated release-table scripts of `probe_010_releases_follow_the_release_table_on_the_production_driver`, over a longer, different run of seeds, each release-table tuple the oracle requires reached at least three times.
#[cfg(feature = "e2e")]
#[spec("inbox/probe/010")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_010_the_long_run_follows_the_release_table() {
    let reached = run_seeds(1_001..=1_000 + LONG_SEEDS).await;
    assert_reached(&reached, 3);
}

/// The fast tier's seeds, and lane 1's (the audit's U3-9: each seed takes
/// about 0.15 s, most of it the settle waits).
const FAST_SEEDS: u64 = 40;
#[cfg(feature = "e2e")]
const LONG_SEEDS: u64 = 240;

/// An owned helper `pid` that read `seg` and decided `decision`.
fn helper(pid: u32, seg: u64, decision: Decision) -> Step {
    Step::Observe(MRecord {
        pid,
        owned: true,
        channel: Channel::Ipc,
        seg: Some(seg),
        decision: Some(decision),
        report: Report::None,
        read: None,
        drained: false,
        done_at_drain: false,
        editor: false,
        matched: false,
        born: 0,
        exited: None,
        completed: false,
        subsumed: false,
    })
}

/// Scenario: On Claude, the deck's check writes its Ctrl+G and a person types a letter and then a Ctrl+C, an Enter or a Backspace (or a letter alone) at each point of the check: before the helper is seen, after it decides `Armed`, after it exits, and after Claude's paste-mode disable but before its enable. The letter goes as the release table lets it, but the Ctrl+C, Enter or Backspace (Claude's terminal being out of raw mode until it returns) stays held until the helper has exited and Claude's enable is read, and is then written once, in order, after the letter; the check completes and settles either way (Q147b, the activation's lane-2 failure).
#[spec("inbox/probe/010")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_010_a_line_discipline_key_waits_for_claudes_return() {
    // Where the typing lands in the check's sequence: before step `at` of
    // the helper's decision, its exit, the disable and the enable.
    let points = [
        "before the helper",
        "after its decision",
        "after its exit",
        "after the disable",
    ];
    for key in [0x03u8, b'\r', 0x7f, b'y'] {
        for (at, point) in points.iter().enumerate() {
            let mut steps = vec![
                helper(11, 1, Decision::Armed),
                Step::Exit(11),
                Step::Out(OFF),
                Step::Out(ON),
            ];
            steps.insert(at, Step::Type(vec![b'x', key]));
            // Typing after the check is the pane's at once.
            steps.push(Step::Type(b"z\x03".to_vec()));
            let what = format!("key {key:#04x} {point}");
            let model = run(Agent::Claude, b"", false, &steps, &what).await;
            assert!(model.segs.is_empty(), "{what}: the check completed");
            assert_eq!(model.settled, Some(Settled::NonEmpty), "{what}: settled");
            assert_eq!(
                model.expected,
                [&[CTRL_G, b'x', key][..], b"z\x03"].concat(),
                "{what}: everything typed, once, in order"
            );
            assert_eq!(
                model.stats.cooked_held > 0,
                key != b'y',
                "{what}: the model held the key for the return"
            );
        }
    }
}

/// Scenario: The Q35-20 review's six traces as fixed scripts on the production driver, each judged by the release table: a return read before a probe's written report and a trigger's (R-U2-01), output that ran before the segments attached and ended inside an escape (R-U2-02), typing after a release to a running editor with nothing else moving (R-U2-03), a probe's `Armed` decision whose read never comes while its return and exit complete it (R-U2-05), and an unknown wait after a release to a running editor, with typing before and after it (R-U2-06).
#[spec("inbox/probe/010")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_010_the_review_traces_follow_the_release_table() {
    for agent in [Agent::Claude, Agent::Codex] {
        let ret: Vec<Step> = fast_return(agent).into_iter().map(Step::Out).collect();
        // R-U2-01 and 02: early output, then a probe answered before its
        // report, completing; a held Ctrl+G's trigger answered inside its
        // write by a helper and its editor.
        let u01 = vec![
            Step::Type(b"ab\x07cd".to_vec()),
            Step::Fast { helper: Some(12) },
            helper(11, 1, Decision::Armed),
            Step::Exit(11),
            Step::Type(b"ef".to_vec()),
        ];
        run(
            agent,
            b"xxxx\x1b[?20",
            true,
            &u01,
            &format!("R-U2-01/02 {agent:?}"),
        )
        .await;

        // R-U2-03 and 06: a held Ctrl+G released to the user's running
        // editor; typing after it goes on to the editor; once its helper
        // drains with no report, what is still held stays.
        let mut u03 = vec![
            Step::Type(b"\x07".to_vec()),
            helper(21, 1, Decision::Armed),
            Step::Exit(21),
        ];
        u03.extend(ret.clone());
        u03.extend([
            helper(22, 2, Decision::Unarmed),
            Step::Editor(22),
            Step::Type(b"gh".to_vec()),
            Step::Type(b"ij".to_vec()),
            Step::Exit(22),
            Step::Type(b"kl".to_vec()),
        ]);
        run(agent, b"", false, &u03, &format!("R-U2-03/06 {agent:?}")).await;

        // R-U2-05: `Armed` on a marker, no read, then the return before the
        // exit: the settlement still runs.
        let mut u05 = vec![Step::Observe(MRecord {
            pid: 31,
            owned: true,
            channel: Channel::Marker,
            seg: Some(1),
            decision: Some(Decision::Armed),
            report: Report::None,
            read: None,
            drained: false,
            done_at_drain: false,
            editor: false,
            matched: false,
            born: 0,
            exited: None,
            completed: false,
            subsumed: false,
        })];
        u05.extend(ret.clone());
        u05.push(Step::Exit(31));
        run(agent, b"", false, &u05, &format!("R-U2-05 {agent:?}")).await;
    }
}

/// Scenario: For Claude and Codex, a held Ctrl+G's trigger is answered by a helper that runs the person's editor, which ends before it is seen running: typing is held behind it, its helper reports the editor ended clean while more is typed, then exits, and more is typed after. Judged step by step by the release table: on Claude what was held goes at the helper's drained exit and later typing follows it; on Codex, whose return past the drain frontier this rig never takes, it all stays held (A-20d-15).
#[spec("inbox/probe/010")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_010_typing_behind_an_editor_that_ended_unseen_follows_the_release_table() {
    for agent in [Agent::Claude, Agent::Codex] {
        let mut steps = vec![
            Step::Type(b"\x07".to_vec()),
            helper(71, 1, Decision::Armed),
            Step::Exit(71),
        ];
        steps.extend(fast_return(agent).into_iter().map(Step::Out));
        steps.extend([
            helper(72, 2, Decision::Unarmed),
            Step::Type(b"gh".to_vec()),
            Step::Report(72, Report::Done(Tree::Clean)),
            Step::Type(b"ij".to_vec()),
            Step::Exit(72),
            Step::Type(b"kl".to_vec()),
        ]);
        let model = run(agent, b"", false, &steps, &format!("A-20d-15 {agent:?}")).await;
        let released = Row {
            agent: agent_name(agent),
            kind: MKind::HumanG,
            decision: "Unarmed",
            editor: false,
            spawn_failed: false,
            releases: true,
        };
        assert_eq!(
            model.stats.rows.contains_key(&released),
            agent == Agent::Claude,
            "{agent:?}: the release at the drain is Claude's alone here"
        );
    }
}

/// Scenario: A held Ctrl+G's trigger is answered by a helper that runs the user's editor, so the segment releases and typing goes on to the editor; then, for Claude and Codex, more typing is held and the release driver is stopped between reading that the segment released and taking what is held, while the helper drains with no report (an unknown wait). The bytes still held when the unknown wait became known stay held: the PTY takes nothing more (R-B-U2-02).
#[spec("inbox/probe/010")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_010_an_unknown_wait_inside_a_driver_pass_keeps_what_is_held() {
    for agent in [Agent::Claude, Agent::Codex] {
        let mut steps = vec![
            Step::Type(b"\x07".to_vec()),
            helper(41, 1, Decision::Armed),
            Step::Exit(41),
        ];
        steps.extend(fast_return(agent).into_iter().map(Step::Out));
        steps.extend([
            helper(42, 2, Decision::Unarmed),
            Step::Editor(42),
            Step::Type(b"gh".to_vec()),
            Step::ExitMidPass(42),
            Step::Type(b"ij".to_vec()),
            Step::Type(b"kl".to_vec()),
        ]);
        run(agent, b"", false, &steps, &format!("R-B-U2-02 {agent:?}")).await;
    }
}

/// R-U2-04's model: paste pairs and the records that account for them.
mod pairs {
    use super::*;

    /// One unit of a script: an editor session the deck saw (its record and
    /// its pair, in either order), or a pair no record accounts for.
    #[derive(Debug, Clone, Copy)]
    enum Unit {
        Session { pair_first: bool },
        Foreign,
    }

    /// Scenario: Generate sequences of editor sessions the deck saw (a helper's record and its bracketed-paste pair, in either order) and pairs no record accounts for, on a generation's real output hook and input. A foreign pair always moves the epoch as an external edit, once; a session whose record came first moves nothing; one whose pair came first moves it once and lends nothing to a later pair (R-U2-04).
    #[spec("inbox/probe/048")]
    #[test]
    fn probe_048_paste_pairs_follow_their_records() {
        for seed in 1..=200u64 {
            let mut rng = Rng(0xd1b5_4a32_d192_ed03 ^ seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
            let input = Arc::new(GenerationInput::default());
            assert!(input.attach_records(Records::new(G, EcPolicy::Hold, Instant::now())));
            assert!(input.attach_segments(G, Agent::Claude));
            let mut hook = OutputHook::new(Arc::clone(&input));
            let mut want = 0u64;
            let mut pid = 0;
            let units: Vec<Unit> = (0..12)
                .map(|_| match rng.below(3) {
                    0 => Unit::Foreign,
                    _ => Unit::Session {
                        pair_first: rng.chance(50),
                    },
                })
                .collect();
            for unit in &units {
                let pair = [OFF, ON].concat();
                match *unit {
                    Unit::Foreign => {
                        hook.on_output(&pair);
                        want += 1;
                    }
                    Unit::Session { pair_first } => {
                        pid += 1;
                        // Born before its own pair ends, after every earlier
                        // pair.
                        let born = Instant::now();
                        let record = Observation {
                            generation: G,
                            key: key(pid),
                            channel: Channel::Ipc,
                            attribution: Attribution::Owned,
                            claim: None,
                            decision: Some(Decision::Unarmed),
                            seg_seen: None,
                            native_seen: None,
                            birth_floor: born,
                        };
                        if pair_first {
                            hook.on_output(&pair);
                            input.record_observe(&record, Instant::now()).unwrap();
                            want += 1;
                        } else {
                            input.record_observe(&record, Instant::now()).unwrap();
                            hook.on_output(&pair);
                        }
                    }
                }
                assert_eq!(
                    input.epoch().last_foreign(),
                    want,
                    "seed {seed}: after {unit:?} in {units:?}"
                );
            }
        }
    }

    fn generation() -> (Arc<GenerationInput>, OutputHook) {
        let input = Arc::new(GenerationInput::default());
        assert!(input.attach_records(Records::new(G, EcPolicy::Hold, Instant::now())));
        assert!(input.attach_segments(G, Agent::Claude));
        let hook = OutputHook::new(Arc::clone(&input));
        (input, hook)
    }

    fn session(pid: u32, born: Instant) -> Observation {
        Observation {
            generation: G,
            key: key(pid),
            channel: Channel::Ipc,
            attribution: Attribution::Owned,
            claim: None,
            decision: Some(Decision::Unarmed),
            seg_seen: None,
            native_seen: None,
            birth_floor: born,
        }
    }

    /// Scenario: One more editor session than the accounting remembers pairs for: each session's bracketed-paste pair is read before its record, so each moves the epoch, and then all their records arrive late, every helper born before the first pair ended. A later, unrelated pair still moves the epoch: no late record banks credit for a pair that already moved it (R-B-U2-01, the audit's U3-4).
    #[spec("inbox/probe/048")]
    #[test]
    fn probe_048_a_backlog_past_the_cap_lends_nothing_to_a_later_pair() {
        let (input, mut hook) = generation();
        let pair = [OFF, ON].concat();
        let born = Instant::now();
        let sessions = u32::from(crate::composer_probe::epoch::EXPLAINED_CAP) + 1;
        for _ in 0..sessions {
            hook.on_output(&pair);
        }
        assert_eq!(input.epoch().last_foreign(), u64::from(sessions));
        for pid in 1..=sessions {
            input
                .record_observe(&session(pid, born), Instant::now())
                .unwrap();
        }
        assert_eq!(
            input.epoch().last_foreign(),
            u64::from(sessions),
            "a late record moves nothing"
        );
        hook.on_output(&pair);
        assert_eq!(
            input.epoch().last_foreign(),
            u64::from(sessions) + 1,
            "an unrelated pair after the late records moves the epoch"
        );
    }

    /// Scenario: One more editor session than the accounting remembers pairs for, with each helper born after the previous session's bracketed-paste pair ended and every record arriving late, oldest first. Each pair moves the epoch; the late records move nothing; and an unrelated pair read after them still moves the epoch, because the oldest record may be the forgotten pair's session and so spends no newer session's debt (R-B-U2-01, staggered births).
    #[spec("inbox/probe/048")]
    #[test]
    fn probe_048_staggered_births_past_the_cap_lend_nothing_to_a_later_pair() {
        let (input, _hook) = generation();
        let pair = pair_markers();
        let base = Instant::now();
        let at = |n: u64| base + Duration::from_millis(n);
        let sessions = u64::from(crate::composer_probe::epoch::EXPLAINED_CAP) + 1;
        // b1 < p1 < b2 < p2 < … : helper `s` is born at 2s, its pair ends at
        // 2s + 1, and no record has arrived yet.
        for s in 1..=sessions {
            input.on_return_markers(&pair, at(2 * s + 1));
        }
        assert_eq!(input.epoch().last_foreign(), sessions);
        let late = at(2 * sessions + 10);
        for s in 1..=sessions {
            input
                .record_observe(&session(s as u32, at(2 * s)), late)
                .unwrap();
        }
        assert_eq!(
            input.epoch().last_foreign(),
            sessions,
            "a late record moves nothing"
        );
        input.on_return_markers(&pair, at(2 * sessions + 11));
        assert_eq!(
            input.epoch().last_foreign(),
            sessions + 1,
            "an unrelated pair after the staggered late records moves the epoch"
        );
    }

    /// The return markers of one whole bracketed-paste pair.
    fn pair_markers() -> [crate::composer_probe::segments::Seen; 2] {
        use crate::composer_probe::segments::{Marker, Seen};
        [
            Seen {
                marker: Marker::PasteOff,
                start: 0,
            },
            Seen {
                marker: Marker::PasteOn,
                start: 8,
            },
        ]
    }

    /// Scenario: A helper's record is made, so its session may account for one bracketed-paste pair, but its pair never comes (the agent dropped it). While the helper runs, and for a grace after its drain, a pair read is still taken as its session's; once the grace is over, its credit is gone, and an unrelated pair moves the epoch (the audit's U3-4). A session's own pair delivered after the grace (a paused or backlogged output reader) also moves the epoch, once: the grace is a conservative policy, not a bound on output delivery. A session that never drains keeps its credit however long its editor runs.
    #[spec("inbox/probe/048")]
    #[test]
    fn probe_048_a_drained_record_whose_pair_never_came_lends_nothing() {
        use crate::composer_probe::epoch::CREDIT_AFTER_DRAIN;
        let pair = pair_markers();
        let past = |at: Instant| at + CREDIT_AFTER_DRAIN + Duration::from_millis(1);

        // Drained, and the grace is over: the pair is an external edit.
        let (input, _hook) = generation();
        let born = Instant::now();
        input.record_observe(&session(1, born), born).unwrap();
        let drained = Instant::now();
        input.record_exit(G, key(1), drained, drained).unwrap();
        input.record_drained(G, key(1), drained).unwrap();
        input.on_return_markers(&pair, past(Instant::now()));
        assert_eq!(
            input.epoch().last_foreign(),
            1,
            "a credit outlives its drained record by the grace only"
        );

        // Control: drained, the pair read inside the grace is its session's.
        let (input, _hook) = generation();
        let born = Instant::now();
        input.record_observe(&session(1, born), born).unwrap();
        let drained = Instant::now();
        input.record_exit(G, key(1), drained, drained).unwrap();
        input.record_drained(G, key(1), drained).unwrap();
        input.on_return_markers(&pair, Instant::now());
        assert_eq!(input.epoch().last_foreign(), 0, "its own pair, just after");

        // Control: still running, however long: its pair is still to come.
        let (input, _hook) = generation();
        let born = Instant::now();
        input.record_observe(&session(1, born), born).unwrap();
        input.on_return_markers(&pair, past(past(Instant::now())));
        assert_eq!(input.epoch().last_foreign(), 0, "a long editor session");

        // Control: its own pair, delayed past the grace after its drain, is
        // read as an external edit, once; an unrelated pair after it moves
        // the epoch too, so nothing is left banked by the late one.
        let (input, _hook) = generation();
        let born = Instant::now();
        input.record_observe(&session(1, born), born).unwrap();
        let drained = Instant::now();
        input.record_exit(G, key(1), drained, drained).unwrap();
        input.record_drained(G, key(1), drained).unwrap();
        let late = past(drained);
        input.on_return_markers(&pair, late);
        assert_eq!(
            input.epoch().last_foreign(),
            1,
            "its own pair, delivered after the grace, costs one more epoch"
        );
        input.on_return_markers(&pair, late + Duration::from_millis(1));
        assert_eq!(
            input.epoch().last_foreign(),
            2,
            "an unrelated pair after it"
        );
    }

    /// One event of an interleaved pair script.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Event {
        /// Session `s`'s helper is born.
        Born(usize),
        /// Its record is made.
        Record(usize),
        /// Its pair is read.
        Pair(usize),
        /// Its helper exits and drains.
        Drain(usize),
        /// A pair no record accounts for.
        Foreign,
        /// The clock moves past [`CREDIT_AFTER_DRAIN`].
        ///
        /// [`CREDIT_AFTER_DRAIN`]: crate::composer_probe::epoch::CREDIT_AFTER_DRAIN
        Wait,
    }

    /// The script shapes [`probe_048_interleaved_sessions_and_backlogs_keep_every_foreign_pair`]
    /// generates, and how many scripts of each it ran.
    #[derive(Debug, Default)]
    struct Shapes {
        /// At some moment, more sessions whose pair was read and whose record
        /// had not yet come than the accounting remembers pairs for: a peak
        /// outstanding late-session debt past the cap.
        backlogs: usize,
        /// At such a moment, the outstanding sessions' floors staggered: one
        /// of them born after another's pair had ended (R-B-U2-01's shape).
        staggered: usize,
        /// The most outstanding sessions born after the oldest outstanding
        /// pair ended, at a moment past the cap, over the run.
        staggered_floors_peak: usize,
        /// A session whose pair never comes.
        pairless: usize,
        /// Ended with every session drained, the grace over, and a final
        /// unrelated pair.
        quiescent: usize,
        /// A session's own pair read after its credit's grace, counted from
        /// its drain on the same clock, was over.
        late_own: usize,
        /// R-C-U2-01's interleave: a session's own pair read inside its
        /// grace, its drain after the clock passed a grace once already,
        /// while the reference still held its credit.
        own_after_wait: usize,
        /// The bounded accounting held fewer credits than the reference.
        fewer_credits: usize,
        /// The bounded accounting forgot a debt.
        forgot: usize,
    }

    /// The pair accounting that forgets nothing, the reference of the
    /// invariant on `Live::explain`: every pair that moved the epoch and no
    /// record has repaid, and every credit, with no cap on either, and the
    /// same grace after a drain.
    #[derive(Debug, Default)]
    struct Reference {
        credits: Vec<(RecordKey, Option<Instant>)>,
        unpaid: Vec<Instant>,
    }

    impl Reference {
        /// A new record of a helper born no earlier than `floor`: it repays
        /// the oldest unrepaid pair that ended at or after `floor`, or banks.
        fn record(&mut self, key: RecordKey, floor: Instant) {
            match self.unpaid.iter().position(|&ended| ended >= floor) {
                Some(at) => {
                    self.unpaid.remove(at);
                }
                None => self.credits.push((key, None)),
            }
        }

        /// Record `key` drained at `now`.
        fn drain(&mut self, key: RecordKey, now: Instant) {
            for credit in &mut self.credits {
                if credit.0 == key && credit.1.is_none() {
                    credit.1 = Some(now);
                }
            }
        }

        /// A pair is read at `now`: first the credits whose grace is over go.
        fn expire(&mut self, now: Instant) {
            use crate::composer_probe::epoch::CREDIT_AFTER_DRAIN;
            self.credits.retain(|c| {
                c.1.is_none_or(|d| now.saturating_duration_since(d) < CREDIT_AFTER_DRAIN)
            });
        }

        /// Then it takes the oldest credit, or moves the epoch and is owed:
        /// whether it moved the epoch.
        fn take(&mut self, now: Instant) -> bool {
            if self.credits.is_empty() {
                self.unpaid.push(now);
                true
            } else {
                self.credits.remove(0);
                false
            }
        }
    }

    /// The invariant on `Live::explain`, read from both accountings: the
    /// bounded credits are an ordered subsequence of the reference's, each
    /// with the same drain stamp, and every pair the reference still owes
    /// that ended after the watermark is remembered, in order.
    fn included(
        bounded: &crate::composer_probe::epoch::PairAccounting,
        reference: &Reference,
        base: Instant,
    ) -> Result<(), String> {
        let at = |i: Instant| i.saturating_duration_since(base);
        let mut theirs = reference.credits.iter();
        for credit in &bounded.credits {
            if !theirs.any(|r| r == credit) {
                return Err(format!(
                    "a credit for {:?} drained at {:?} that the reference does not bank, in \
                     that order with that stamp",
                    credit.0,
                    credit.1.map(at)
                ));
            }
        }
        let mut kept = bounded.unexplained.iter();
        for &ended in reference
            .unpaid
            .iter()
            .filter(|&&e| bounded.forgotten.is_none_or(|f| e > f))
        {
            if !kept.any(|&k| k == ended) {
                return Err(format!(
                    "a debt for a pair that ended at {:?}, after the watermark {:?}, is not \
                     remembered",
                    at(ended),
                    bounded.forgotten.map(at)
                ));
            }
        }
        Ok(())
    }

    /// One generated script: its events, and which sessions have no pair.
    fn script(rng: &mut Rng, cap: usize) -> (Vec<Event>, Vec<bool>) {
        let shape = rng.below(3);
        let sessions = if shape == 2 {
            cap + 1 + rng.below(5) as usize
        } else {
            rng.below(10) as usize
        };
        let foreign = rng.below(4) as usize;
        // In one script in three, one or two sessions whose pair never comes
        // (the agent exited, or the pair was lost).
        let mut pairless = vec![false; sessions];
        if sessions > 0 && rng.chance(33) {
            for _ in 0..=usize::from(rng.chance(30)) {
                pairless[rng.below(sessions as u64) as usize] = true;
            }
        }
        let mut events = match shape {
            0 => {
                // Every pair, the foreign ones among them, before any record.
                let mut pairs: Vec<Event> = (0..sessions)
                    .filter(|&s| !pairless[s])
                    .map(Event::Pair)
                    .collect();
                for _ in 0..foreign {
                    let at = rng.below(pairs.len() as u64 + 1) as usize;
                    pairs.insert(at, Event::Foreign);
                }
                let mut events: Vec<Event> = (0..sessions).map(Event::Born).collect();
                events.extend(pairs);
                for s in 0..sessions {
                    events.push(Event::Record(s));
                    if rng.chance(50) {
                        events.push(Event::Drain(s));
                    }
                }
                events
            }
            1 => {
                // Each session's own order, interleaved at random with the
                // others'.
                let mut lanes: Vec<VecDeque<Event>> = (0..sessions)
                    .map(|s| {
                        let mut lane = VecDeque::from([Event::Born(s)]);
                        let mut rest = if pairless[s] {
                            vec![Event::Record(s)]
                        } else if rng.chance(50) {
                            vec![Event::Record(s), Event::Pair(s)]
                        } else {
                            vec![Event::Pair(s), Event::Record(s)]
                        };
                        if rng.chance(60) {
                            // Its drain, after its record, before or after
                            // its pair.
                            let record = rest.iter().position(|e| *e == Event::Record(s));
                            let at = record.unwrap() + 1;
                            let at = at + rng.below((rest.len() - at) as u64 + 1) as usize;
                            rest.insert(at, Event::Drain(s));
                        }
                        lane.extend(rest);
                        lane
                    })
                    .collect();
                lanes.extend((0..foreign).map(|_| VecDeque::from([Event::Foreign])));
                interleave(rng, lanes)
            }
            _ => {
                // R-B-U2-01's shape: births staggered across earlier pairs'
                // endings (each helper born, then its pair, interleaved with
                // the other sessions'), every record late, read after its own
                // pair and interleaved with later births and pairs.
                let mut lanes: Vec<VecDeque<Event>> = (0..sessions)
                    .map(|s| {
                        if pairless[s] {
                            VecDeque::from([Event::Born(s)])
                        } else {
                            VecDeque::from([Event::Born(s), Event::Pair(s)])
                        }
                    })
                    .collect();
                lanes.extend((0..foreign).map(|_| VecDeque::from([Event::Foreign])));
                let mut events = interleave(rng, lanes);
                for (s, &missing) in pairless.iter().enumerate() {
                    let after = events
                        .iter()
                        .position(|e| {
                            *e == if missing {
                                Event::Born(s)
                            } else {
                                Event::Pair(s)
                            }
                        })
                        .unwrap()
                        + 1;
                    // Late, and most often after every pair.
                    let at = if rng.chance(60) {
                        events.len()
                    } else {
                        after + rng.below((events.len() - after) as u64 + 1) as usize
                    };
                    events.insert(at, Event::Record(s));
                    if rng.chance(50) {
                        let after = at + 1;
                        let at = after + rng.below((events.len() - after) as u64 + 1) as usize;
                        events.insert(at, Event::Drain(s));
                    }
                }
                events
            }
        };
        // The clock passes the grace now and then, mid-script.
        if rng.chance(30) {
            for _ in 0..=rng.below(2) {
                let at = rng.below(events.len() as u64 + 1) as usize;
                events.insert(at, Event::Wait);
            }
        }
        (events, pairless)
    }

    /// Lanes popped from the front in a random order.
    fn interleave(rng: &mut Rng, mut lanes: Vec<VecDeque<Event>>) -> Vec<Event> {
        let mut events = Vec::new();
        while lanes.iter().any(|l| !l.is_empty()) {
            let open: Vec<usize> = (0..lanes.len()).filter(|&i| !lanes[i].is_empty()).collect();
            let lane = open[rng.below(open.len() as u64) as usize];
            events.push(lanes[lane].pop_front().unwrap());
        }
        events
    }

    /// One run of a generated script against the bounded accounting and the
    /// reference: each pair as read (whether no record accounts for it,
    /// whether the reference moved the epoch, whether production did), and
    /// before each pair, whether the script had settled (below), and the
    /// shapes it reached; or the first step at which the invariant broke.
    struct Run {
        pairs: Vec<(bool, bool, bool)>,
        settled: Vec<bool>,
        shapes: Shapes,
    }

    fn run(
        events: &[Event],
        sessions: usize,
        pairless: &[bool],
        cap: usize,
    ) -> Result<Run, String> {
        use crate::composer_probe::epoch::CREDIT_AFTER_DRAIN;
        // A virtual clock for every stamp: the pairs' `now`, the records'
        // floors and steps, and, through the step clock, the instant a
        // drain stamps a credit with (R-C-U2-01).
        let (input, _hook) = generation();
        let pair = pair_markers();
        let base = Instant::now();
        let mut clock = base;
        let mut reference = Reference::default();
        let mut born = vec![clock; sessions];
        let mut pair_end: Vec<Option<Instant>> = vec![None; sessions];
        let mut recorded = vec![false; sessions];
        let mut drained_at: Vec<Option<Instant>> = vec![None; sessions];
        let mut waited = false;
        let mut wait_before_drain = vec![false; sessions];
        let mut outstanding: Vec<usize> = Vec::new();
        let mut shapes = Shapes::default();
        let (mut backlog, mut staggered, mut late_own, mut own_after_wait) =
            (false, false, false, false);
        let (mut fewer, mut forgot) = (false, false);
        let mut out = Run {
            pairs: Vec::new(),
            settled: Vec::new(),
            shapes: Shapes::default(),
        };
        for (i, &event) in events.iter().enumerate() {
            clock += Duration::from_millis(1);
            input.set_step_clock(Some(clock));
            let before = input.epoch().last_foreign();
            let mut theirs = None;
            match event {
                Event::Born(s) => born[s] = clock,
                Event::Record(s) => {
                    recorded[s] = true;
                    outstanding.retain(|&o| o != s);
                    input
                        .record_observe(&session(s as u32 + 1, born[s]), clock)
                        .unwrap();
                    reference.record(key(s as u32 + 1), born[s]);
                }
                Event::Drain(s) => {
                    drained_at[s] = Some(clock);
                    wait_before_drain[s] = waited;
                    input
                        .record_exit(G, key(s as u32 + 1), clock, clock)
                        .unwrap();
                    input.record_drained(G, key(s as u32 + 1), clock).unwrap();
                    reference.drain(key(s as u32 + 1), clock);
                }
                Event::Pair(_) | Event::Foreign => {
                    reference.expire(clock);
                    // Settled: no credit live, every pairless session's
                    // record made, and every session whose pair was read
                    // has its record. Nothing a pairless session's record
                    // displaced can still be lent from here on.
                    out.settled.push(
                        reference.credits.is_empty()
                            && (0..sessions)
                                .all(|s| recorded[s] || (!pairless[s] && pair_end[s].is_none())),
                    );
                    if let Event::Pair(s) = event {
                        pair_end[s] = Some(clock);
                        if !recorded[s] {
                            outstanding.push(s);
                        }
                        let in_grace =
                            drained_at[s].map(|d| clock.duration_since(d) < CREDIT_AFTER_DRAIN);
                        late_own |= in_grace == Some(false);
                        own_after_wait |= in_grace == Some(true)
                            && wait_before_drain[s]
                            && reference.credits.iter().any(|c| c.0 == key(s as u32 + 1));
                    }
                    theirs = Some(reference.take(clock));
                    input.on_return_markers(&pair, clock);
                }
                Event::Wait => {
                    clock += CREDIT_AFTER_DRAIN;
                    waited = true;
                }
            }
            let moved = input.epoch().last_foreign() - before;
            if moved > 1 {
                return Err(format!(
                    "step {i}, {event:?}, moved the epoch {moved} times"
                ));
            }
            match theirs {
                Some(theirs) => {
                    if theirs && moved == 0 {
                        return Err(format!(
                            "step {i}, {event:?}: the reference moved the epoch and production \
                             did not"
                        ));
                    }
                    out.pairs
                        .push((event == Event::Foreign, theirs, moved == 1));
                }
                None if moved != 0 => {
                    return Err(format!("step {i}, {event:?}, moved the epoch"));
                }
                None => {}
            }
            let bounded = input.pair_accounting().unwrap();
            included(&bounded, &reference, base)
                .map_err(|e| format!("after step {i}, {event:?}: {e}"))?;
            fewer |= bounded.credits.len() < reference.credits.len();
            forgot |= bounded.forgotten.is_some();
            if outstanding.len() > cap {
                backlog = true;
                let oldest = outstanding
                    .iter()
                    .filter_map(|&o| pair_end[o])
                    .min()
                    .unwrap();
                let floors = outstanding.iter().filter(|&&o| born[o] > oldest).count();
                staggered |= floors > 0;
                shapes.staggered_floors_peak = shapes.staggered_floors_peak.max(floors);
            }
        }
        shapes.backlogs = usize::from(backlog);
        shapes.staggered = usize::from(staggered);
        shapes.late_own = usize::from(late_own);
        shapes.own_after_wait = usize::from(own_after_wait);
        shapes.fewer_credits = usize::from(fewer);
        shapes.forgot = usize::from(forgot);
        shapes.pairless = usize::from(pairless.iter().any(|&p| p));
        out.shapes = shapes;
        Ok(out)
    }

    /// Scenario: Generate scripts in which editor sessions overlap rather than following one another: each helper is born, then its record and its bracketed-paste pair arrive in either order, interleaved with other sessions' and with pairs no record accounts for; some read a backlog of pairs past what the accounting remembers before their records, some stagger the helpers' births across earlier pairs' endings with every record late, some have a session whose pair never comes, the clock passes the credit's grace now and then, and most end with an unrelated pair after every session drained and the grace passed. Every stamp is on one virtual clock, the drains' included. After every step the accounting banks no credit, and forgets no debt after its watermark, that an accounting forgetting nothing would not, and it moves the epoch at every pair where that reference does; from any point at which nothing is left to lend, and from any point in a script whose every session's pair comes, the epoch moves at least once per pair no record accounts for (R-B-U2-01, R-C-U2-01, the audit's U3-1).
    #[spec("inbox/probe/048")]
    #[test]
    fn probe_048_interleaved_sessions_and_backlogs_keep_every_foreign_pair() {
        let cap = usize::from(crate::composer_probe::epoch::EXPLAINED_CAP);
        let mut shapes = Shapes::default();
        for seed in 1..=600u64 {
            let mut rng = Rng(0x2545_f491_4f6c_dd1d ^ seed.wrapping_mul(0xd1b5_4a32_d192_ed03));
            let (mut events, pairless) = script(&mut rng, cap);
            let sessions = pairless.len();
            let missing = pairless.iter().filter(|&&p| p).count();
            let quiescent = missing > 0 || rng.chance(30);
            if quiescent {
                // Every helper exits, the grace passes, and an unrelated
                // pair is read: no credit is left to explain it.
                let drained: Vec<usize> = events
                    .iter()
                    .filter_map(|e| match e {
                        Event::Drain(s) => Some(*s),
                        _ => None,
                    })
                    .collect();
                events.extend(
                    (0..sessions)
                        .filter(|s| !drained.contains(s))
                        .map(Event::Drain),
                );
                events.extend([Event::Wait, Event::Foreign]);
            } else if rng.chance(70) {
                events.push(Event::Foreign);
            }
            let run = run(&events, sessions, &pairless, cap)
                .unwrap_or_else(|e| panic!("seed {seed}: {e}\nevents {events:?}"));
            let pairs = &run.pairs;
            // An absolute count over suffixes, on the reference (production
            // moves at least where it does): credits are consumed oldest
            // first, so a foreign pair read while another session's credit
            // is live moves the epoch at that session's own pair instead
            // (the audit's U3-1). Only a session whose pair never comes, or
            // what its record displaced, can leave a foreign pair unmoved,
            // and only until nothing is left to lend: from a settled point,
            // or in a script with no such session, no allowance.
            for from in 0..pairs.len() {
                let foreign = pairs[from..].iter().filter(|p| p.0).count();
                let moves = pairs[from..].iter().filter(|p| p.1).count();
                if missing == 0 || run.settled[from] {
                    assert!(
                        moves >= foreign,
                        "seed {seed}: from pair {from} on, {foreign} pairs no record accounts \
                         for moved the reference only {moves} times\nevents {events:?}\n\
                         pairs {pairs:?}\nsettled {:?}",
                        run.settled
                    );
                }
            }
            if quiescent {
                shapes.quiescent += 1;
                assert_eq!(
                    pairs.last(),
                    Some(&(true, true, true)),
                    "seed {seed}: an unrelated pair after every session drained and the grace \
                     passed moved nothing\nevents {events:?}\npairs {pairs:?}"
                );
                assert_eq!(
                    run.settled.last(),
                    Some(&true),
                    "seed {seed}: the quiescent end is settled\nevents {events:?}"
                );
            }
            shapes.add(&run.shapes);
        }
        eprintln!("probe_048 interleaved shapes over 600 seeds: {shapes:?}");
        assert!(
            shapes.backlogs >= 20,
            "too few backlogs past the cap: {shapes:?}"
        );
        assert!(
            shapes.staggered >= 20,
            "too few staggered backlogs: {shapes:?}"
        );
        assert!(
            shapes.staggered_floors_peak >= cap,
            "no staggered backlog with a floor per remembered pair: {shapes:?}"
        );
        assert!(
            shapes.pairless >= 20,
            "too few pairless scripts: {shapes:?}"
        );
        assert!(shapes.quiescent >= 20, "too few quiescent ends: {shapes:?}");
        assert!(shapes.late_own >= 5, "too few late own pairs: {shapes:?}");
        assert!(
            shapes.own_after_wait >= 5,
            "too few own pairs inside a grace after a wait: {shapes:?}"
        );
        assert!(
            shapes.fewer_credits >= 20 && shapes.forgot >= 20,
            "too few scripts where the bounds bit: {shapes:?}"
        );
    }

    impl Shapes {
        fn add(&mut self, run: &Shapes) {
            self.backlogs += run.backlogs;
            self.staggered += run.staggered;
            self.staggered_floors_peak = self.staggered_floors_peak.max(run.staggered_floors_peak);
            self.pairless += run.pairless;
            self.late_own += run.late_own;
            self.own_after_wait += run.own_after_wait;
            self.fewer_credits += run.fewer_credits;
            self.forgot += run.forgot;
        }
    }

    /// Scenario: On one virtual clock, a helper is born and its record made, the clock passes a whole grace, then its helper drains and its bracketed-paste pair is read a millisecond later: inside the grace counted from the drain, so the pair is its session's and moves nothing. An unrelated pair read after it moves the epoch once (R-C-U2-01).
    #[spec("inbox/probe/048")]
    #[test]
    fn probe_048_an_own_pair_inside_the_grace_after_a_wait_moves_nothing() {
        use crate::composer_probe::epoch::CREDIT_AFTER_DRAIN;
        let (input, _hook) = generation();
        let pair = pair_markers();
        let born = Instant::now();
        input.set_step_clock(Some(born));
        input.record_observe(&session(1, born), born).unwrap();
        let drained = born + CREDIT_AFTER_DRAIN + Duration::from_millis(1);
        input.set_step_clock(Some(drained));
        input.record_exit(G, key(1), drained, drained).unwrap();
        input.record_drained(G, key(1), drained).unwrap();
        assert_eq!(
            input.pair_accounting().unwrap().credits,
            vec![(key(1), Some(drained))],
            "the credit's grace starts at the drain's step, on the test's clock"
        );
        input.on_return_markers(&pair, drained + Duration::from_millis(1));
        assert_eq!(
            input.epoch().last_foreign(),
            0,
            "its own pair, read inside the grace counted from its drain"
        );
        input.on_return_markers(&pair, drained + Duration::from_millis(2));
        assert_eq!(
            input.epoch().last_foreign(),
            1,
            "an unrelated pair after it"
        );
    }
}

/// Scenario: For Claude and Codex, a probe is admitted and its fence closed in production's arm section; a person types while its byte is at the PTY, and the PTY fails that byte, once with an error and once by taking none of it (a probe's job is its one byte, so a partial write takes nothing). The probe ends refused for its failed write, not for an admission refusal; its liability is lowered, its segment is gone and its schedule backs off; the PTY took no probe byte, and the release driver lets the held typing go with nothing outstanding, the fence opening behind it and nothing left in flight (O-20.5).
#[spec("inbox/probe/010")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_010_a_failed_probe_write_undoes_its_arm_and_releases_what_it_held() {
    for agent in [Agent::Claude, Agent::Codex] {
        for fault in [ProbeFault::Error, ProbeFault::Zero] {
            let what = format!("{} {fault:?}", agent_name(agent));
            let rig = Rig::new(agent, b"");
            let outcome = rig.probe_faulted(fault, b"ab").await;
            assert_eq!(
                outcome,
                ProbeOutcome::Refused(PROBE_WRITE_FAILED.into()),
                "{what}"
            );
            rig.settle(b"ab").await;
            assert_eq!(
                rig.taken(),
                b"ab",
                "{what}: no probe byte, the typing released"
            );
            assert_eq!(
                rig.input.liability_count(),
                Some(0),
                "{what}: the arm undone"
            );
            assert_eq!(
                rig.input.segments_front(),
                Some(crate::composer_probe::segments::Front::None),
                "{what}: its segment gone"
            );
            assert!(
                !rig.input.probe_due(Instant::now()),
                "{what}: a failure's backoff"
            );
            let w = rig.writer.lock().await;
            let status = w.gate.gate.status();
            assert!(
                !status.fenced && status.held_ops == 0,
                "{what}: the fence open, nothing held: {status:?}"
            );
            assert_eq!(
                w.pty.in_flight.count.load(Ordering::SeqCst),
                0,
                "{what}: nothing left in flight"
            );
        }
    }
}

/// Scenario: For Claude and Codex, a probe is admitted and its fence closed in production's arm section, and the PTY fails its byte, with an error and by taking none of it, while nobody types. The probe ends refused for its failed write, its arm undone and its schedule backing off, and its fence opens at once with nothing held behind it: no release is coming to open it (the audit's U3-8, O-20.5).
#[spec("inbox/probe/010")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_010_a_failed_probe_write_with_nothing_held_opens_its_fence() {
    for agent in [Agent::Claude, Agent::Codex] {
        for fault in [ProbeFault::Error, ProbeFault::Zero] {
            let what = format!("{} {fault:?}", agent_name(agent));
            let rig = Rig::new(agent, b"");
            let outcome = rig.probe_faulted(fault, b"").await;
            assert_eq!(
                outcome,
                ProbeOutcome::Refused(PROBE_WRITE_FAILED.into()),
                "{what}"
            );
            // Its fence opened by the probe's own end, not by a release.
            let w = rig.writer.lock().await;
            let status = w.gate.gate.status();
            assert!(
                !status.fenced && status.held_ops == 0,
                "{what}: the fence open, nothing held: {status:?}"
            );
            drop(w);
            rig.settle(b"").await;
            assert!(rig.taken().is_empty(), "{what}: no probe byte");
            assert_eq!(
                rig.input.liability_count(),
                Some(0),
                "{what}: the arm undone"
            );
            assert!(
                !rig.input.probe_due(Instant::now()),
                "{what}: a failure's backoff"
            );
            let w = rig.writer.lock().await;
            assert!(!w.gate.gate.status().fenced, "{what}: still open");
            assert_eq!(
                w.pty.in_flight.count.load(Ordering::SeqCst),
                0,
                "{what}: nothing left in flight"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// A-20d-2: the writer's facts against what lands between scheduling and the
// arm section, through the generation's own admission.
// ---------------------------------------------------------------------------

/// A Claude Code generation on a bus showing its main composer, and one
/// probe asked as `composer_probe` asks it ([`Rig::probe_admitted`]), with
/// `between` run between its scheduled admission and its arm section. The
/// rig, the probe's outcome, and the ticket `between` admitted, if any,
/// still outstanding.
async fn arm_after(
    between: impl FnOnce(&Rig) -> Option<crate::ingress_ledger::TicketGuard>,
) -> (
    Arc<Rig>,
    ProbeOutcome,
    Option<crate::ingress_ledger::TicketGuard>,
) {
    let rig = Rig::build(Agent::Claude, b"", true);
    rig.out(&claude_frame(80));
    let mut kept = None;
    let outcome = rig.probe_admitted(|rig| kept = between(rig)).await;
    (rig, outcome, kept)
}

/// Scenario: A Claude Code generation shows its main composer on a real bus, and a probe is asked as the deck's probe request asks it: scheduled through the generation's own admission, its arm section on the real writer thread asking admission again with the writer's own reads. Between scheduling and the arm section the agent writes output, or a person's ticket is admitted, or both: the arm refuses (the screen is no longer the frame it was scheduled from, or a person is typing), the PTY takes no byte, no fence closes and no liability is raised; a person's ticket counts one failure and the screen alone counts none (A-20d-11), and the person's typing then goes straight through. With nothing between, the same probe is written and closes its fence (A-20d-2).
#[spec("inbox/probe/084")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_084_what_lands_between_scheduling_and_the_arm_refuses_it_and_writes_nothing() {
    let (rig, written, _) = arm_after(|_| None).await;
    assert_eq!(
        written,
        ProbeOutcome::Written,
        "nothing between: the control"
    );
    rig.settle(&[CTRL_G]).await;
    assert_eq!(rig.taken(), [CTRL_G], "the control's probe byte");
    let status = rig.writer.lock().await.gate.gate.status();
    assert!(status.fenced, "the control closes its fence");
    assert_eq!(
        rig.input.liability_count(),
        Some(1),
        "and raises its liability"
    );

    type Between = fn(&Rig) -> Option<crate::ingress_ledger::TicketGuard>;
    let cases: [(&str, Between, &str); 3] = [
        (
            "output taken",
            |rig| {
                rig.out(b"x");
                None
            },
            "Admission(Surface)",
        ),
        (
            "a ticket admitted",
            |rig| Some(rig.ledger.admit()),
            "Admission(HumanTicket)",
        ),
        (
            "output taken and a ticket admitted",
            |rig| {
                rig.out(b"x");
                Some(rig.ledger.admit())
            },
            "Admission(HumanTicket)",
        ),
    ];
    for (what, between, refusal) in cases {
        let (rig, outcome, ticket) = arm_after(between).await;
        assert_eq!(
            outcome,
            ProbeOutcome::Refused(refusal.to_string()),
            "{what} between scheduling and the arm section"
        );
        assert!(rig.taken().is_empty(), "{what}: no byte written");
        let status = rig.writer.lock().await.gate.gate.status();
        assert!(!status.fenced, "{what}: no fence closed");
        assert_eq!(rig.input.liability_count(), Some(0), "{what}: none raised");
        assert!(
            rig.input.segments_view(Instant::now()).is_empty(),
            "{what}: the probe's segment withdrawn"
        );
        let schedule = rig
            .input
            .advance_schedule_for_e2e(Duration::ZERO, Instant::now());
        // A-20d-11: the screen's freshness refusal counts none; a ticket's
        // counts one.
        let counted = u32::from(refusal != "Admission(Surface)");
        assert_eq!(
            schedule.map(|(_, failures, _)| failures),
            Some(counted),
            "{what}: {counted} failure(s) counted"
        );
        // The person's typing, behind no fence, goes straight to the PTY.
        let frame = UserFrame::new(
            b"ab".to_vec(),
            ticket.or_else(|| Some(rig.ledger.admit())),
            None,
        );
        let accepted = rig.writer.lock().await.accept(frame, None).await;
        assert!(
            matches!(accepted, Accepted::Written(Ok(()))),
            "{what}: typing after the refusal is written"
        );
        assert_eq!(rig.taken(), b"ab", "{what}: and only it");
    }
}

/// Scenario: A Claude Code generation shows its main composer, and a probe is asked as the deck's probe request asks it. The agent writes output between its scheduling and its arm section, so the writer refuses the screen and nothing is written; that refusal counts no failure, so the schedule is due again at once, not broken, and once the agent redraws its composer the next probe, with nothing between, is written. A person's ticket admitted between scheduling and the arm still counts its one failure (A-20d-11).
#[spec("inbox/probe/084")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_084_a_screen_that_moved_before_the_arm_counts_no_failure() {
    let schedule = |rig: &Rig| {
        rig.input
            .advance_schedule_for_e2e(Duration::ZERO, Instant::now())
    };
    let (rig, outcome, _) = arm_after(|rig| {
        rig.out(b"x");
        None
    })
    .await;
    assert_eq!(
        outcome,
        ProbeOutcome::Refused("Admission(Surface)".to_string()),
        "output between scheduling and the arm"
    );
    assert!(rig.taken().is_empty(), "no byte written");
    assert_eq!(
        schedule(&rig),
        Some((true, 0, false)),
        "due again at once, no failure counted, not broken"
    );
    rig.out(&claude_frame(80));
    assert_eq!(
        rig.probe_admitted(|_| {}).await,
        ProbeOutcome::Written,
        "the next probe, from the redrawn composer"
    );
    rig.settle(&[CTRL_G]).await;
    assert_eq!(rig.taken(), [CTRL_G], "its byte alone");

    // The control: a person's ticket says the box may have changed.
    let (rig, outcome, _ticket) = arm_after(|rig| Some(rig.ledger.admit())).await;
    assert_eq!(
        outcome,
        ProbeOutcome::Refused("Admission(HumanTicket)".to_string())
    );
    assert_eq!(
        schedule(&rig).map(|(_, failures, _)| failures),
        Some(1),
        "a ticket's refusal still counts"
    );
}

// ---------------------------------------------------------------------------
// A-20d-3 (O-20.3): an observation landing inside a driver pass.
// ---------------------------------------------------------------------------

/// The three orders of one script: `step` before the op `on_held` is
/// typed, inside the driver pass that typing wakes (between its read of
/// the front segment and its extraction of what is held), and after it.
fn three_orders(
    head: &[Step],
    on_held: &[u8],
    step: Step,
    tail: &[Step],
) -> [(&'static str, Vec<Step>); 3] {
    let typed = Step::Type(on_held.to_vec());
    let with = |middle: Vec<Step>| {
        let mut steps = head.to_vec();
        steps.extend(middle);
        steps.extend_from_slice(tail);
        steps
    };
    [
        ("before", with(vec![step.clone(), typed.clone()])),
        (
            "inside",
            with(vec![
                Step::MidPass {
                    on_held: on_held.to_vec(),
                    step: Box::new(step.clone()),
                },
                typed.clone(),
            ]),
        ),
        ("after", with(vec![typed, step])),
    ]
}

/// A helper `pid` that read `seg` and decided `decision` on `channel`, with
/// the daemon's read of an `Armed` export (`read`).
fn seen(pid: u32, seg: u64, decision: Decision, channel: Channel, read: Option<bool>) -> Step {
    let Step::Observe(mut r) = helper(pid, seg, decision) else {
        unreachable!()
    };
    r.channel = channel;
    r.read = read;
    Step::Observe(r)
}

/// One of [`probe_085_an_observation_inside_a_driver_pass_ends_as_before_or_after_it`]'s
/// scripts: its name, the steps before the held op, the observation, the
/// steps after, how the probe settles and the failures it counts.
type Interleaved = (&'static str, Vec<Step>, Step, Vec<Step>, Settled, u32);

/// Scenario: For Claude and Codex, run each of six scripts three ways, an observation landing before a held op's driver pass, inside it (between the pass's read of the front segment and its extraction of what is held) or after it: the probe's own helper deciding `Armed` with an empty box (matching, consuming the liability and settling, its keymap re-read parked), that re-read itself, the same helper seen again on its marker deciding `Armed` or `Unarmed`, its exit completing the segment while typing with a Ctrl+G is held, and the agent's return marker completing it. Each order is judged step by step by the release table, and all three end the same: the same bytes taken and numbers published, the same liability count, the same schedule with no failure counted twice and no settlement lost, the same segments, epoch and gate (A-20d-3, O-20.3).
#[spec("inbox/probe/085")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_085_an_observation_inside_a_driver_pass_ends_as_before_or_after_it() {
    let zz = b"zz";
    for agent in [Agent::Claude, Agent::Codex] {
        let ret: Vec<Step> = fast_return(agent).into_iter().map(Step::Out).collect();
        let (last, first) = ret.split_last().unwrap();
        let empty = seen(61, 1, Decision::Armed, Channel::Ipc, Some(true));
        let full = seen(61, 1, Decision::Armed, Channel::Ipc, Some(false));
        let done = {
            let mut tail = vec![Step::Exit(61)];
            tail.extend(ret.clone());
            tail.push(Step::Type(b"cd".to_vec()));
            tail
        };
        let next = {
            let mut tail = vec![helper(62, 2, Decision::Armed), Step::Exit(62)];
            tail.extend(ret.clone());
            tail.push(Step::Type(b"cd".to_vec()));
            tail
        };
        let mut completing = vec![full.clone(), Step::Exit(61)];
        completing.extend(first.iter().cloned());
        completing.push(Step::Type(b"a\x07b".to_vec()));
        let mut released = vec![full];
        released.extend(ret.clone());
        released.push(Step::Type(b"a\x07b".to_vec()));
        let scripts: [Interleaved; 6] = [
            (
                "matching and settlement",
                vec![Step::Type(b"ab".to_vec())],
                empty.clone(),
                [vec![Step::Recheck], done.clone()].concat(),
                Settled::Empty,
                0,
            ),
            (
                "the keymap re-read",
                vec![Step::Type(b"ab".to_vec()), empty.clone()],
                Step::Recheck,
                done.clone(),
                Settled::Empty,
                0,
            ),
            (
                "the same `Armed` helper on its marker",
                vec![Step::Type(b"ab".to_vec()), empty, Step::Recheck],
                seen(61, 1, Decision::Armed, Channel::Marker, None),
                done.clone(),
                Settled::Empty,
                0,
            ),
            (
                "the same `Unarmed` helper on its marker",
                vec![
                    Step::Type(b"ab".to_vec()),
                    seen(61, 1, Decision::Unarmed, Channel::Ipc, None),
                ],
                seen(61, 1, Decision::Unarmed, Channel::Marker, None),
                vec![Step::Type(b"cd".to_vec())],
                Settled::Failed,
                1,
            ),
            (
                "the exit that completes",
                released,
                Step::Exit(61),
                next.clone(),
                Settled::NonEmpty,
                0,
            ),
            (
                "the return that completes",
                completing,
                last.clone(),
                next,
                Settled::NonEmpty,
                0,
            ),
        ];
        for (name, head, step, tail, settled, failures) in scripts {
            let mut ends = Vec::new();
            for (order, steps) in three_orders(&head, zz, step.clone(), &tail) {
                let what = format!("{agent:?}, {name}, {order}");
                let opts = Opts {
                    recheck: true,
                    judged: Judged::Exactly,
                };
                let (model, snapshot) = run_with(opts, agent, b"", false, &steps, &what).await;
                assert_eq!(model.settled, Some(settled), "{what}: settled once, as");
                assert_eq!(
                    model.stats.settled.values().sum::<usize>(),
                    1,
                    "{what}: one settlement"
                );
                assert_eq!(
                    snapshot.schedule.map(|(_, f, _)| f),
                    Some(failures),
                    "{what}: failures counted"
                );
                ends.push((order, model.expected.clone(), snapshot));
            }
            let (_, expected, end) = &ends[0];
            for (order, other_expected, other) in &ends[1..] {
                assert_eq!(
                    other_expected, expected,
                    "{agent:?}, {name}: the table allows the same bytes {order} as before"
                );
                assert_eq!(other, end, "{agent:?}, {name}: {order} ends as before");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// A-20d-5: the cut-open path, typing with a liability out and no segment.
// ---------------------------------------------------------------------------

/// The cut-open scripts for `agent`, each with its name: a liability out
/// with no segment, reached two ways (a phantom raised once the probe
/// completed; and, for Claude, production's own route, a lapsed probe
/// whose retry completed while its liability stays out, core §4.7's
/// CL-10), then typing with no candidate, then with one, then more; and the
/// `HumanG` its candidate opens ending two ways: its helper decides
/// `Armed` (the phantom's notice path), or `Unarmed` and runs the user's
/// editor.
fn cut_open_scripts(agent: Agent) -> Vec<(String, Vec<Step>)> {
    let ret: Vec<Step> = fast_return(agent).into_iter().map(Step::Out).collect();
    let mut routes = vec![("a phantom", {
        let mut steps = vec![helper(51, 1, Decision::Armed), Step::Exit(51)];
        steps.extend(ret.clone());
        steps.push(Step::Phantom);
        (steps, 2)
    })];
    if agent == Agent::Claude {
        routes.push(("a lapsed probe's", {
            let mut steps = vec![
                Step::LapseRetry,
                helper(51, 2, Decision::Armed),
                Step::Exit(51),
            ];
            steps.extend(ret.clone());
            (steps, 3)
        }));
    }
    let mut scripts = Vec::new();
    for (route, (head, n)) in routes {
        let typed = [
            Step::Type(b"xy".to_vec()),
            Step::Type(b"a\x07b".to_vec()),
            Step::Type(b"cd".to_vec()),
        ];
        let mut armed = vec![helper(52, n, Decision::Armed), Step::Exit(52)];
        armed.extend(ret.clone());
        armed.push(Step::Type(b"ef".to_vec()));
        let editor = vec![
            helper(52, n, Decision::Unarmed),
            Step::Editor(52),
            Step::Type(b"ef".to_vec()),
        ];
        for (end, stage2) in [("`Armed`", armed), ("the user's editor", editor)] {
            let steps = [head.clone(), typed.to_vec(), stage2].concat();
            scripts.push((format!("{agent:?}, {route} liability, {end}"), steps));
        }
    }
    scripts
}

/// Scenario: With a liability out and no segment open, reached by raising a phantom liability after the probe completed and, for Claude, by a lapsed probe whose retry completed while its liability stayed out, a person types with no Ctrl+G, then with one, then more, and the `HumanG` that Ctrl+G opens is answered by a helper deciding `Armed` or running the user's editor. After every step the PTY has taken nothing the release table doesn't allow yet: typing with no candidate goes straight through, the bytes after a candidate stay held, and no Ctrl+G is written with bytes after it; the liability count is the table's at every step (A-20d-5, its authority half).
#[spec("inbox/probe/086")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_086_a_cut_with_a_liability_out_and_no_segment_releases_nothing_early() {
    for agent in [Agent::Claude, Agent::Codex] {
        for (what, steps) in cut_open_scripts(agent) {
            let opts = Opts {
                recheck: false,
                judged: Judged::NothingEarly,
            };
            run_with(opts, agent, b"", false, &steps, &what).await;
        }
    }
}

/// Scenario: The scripts of `probe_086_a_cut_with_a_liability_out_and_no_segment_releases_nothing_early`, judged exactly: the Ctrl+G that the cut opens a `HumanG` with is written as the bytes through it, after its number was published, and once that segment's helper decides `Armed` or runs the user's editor, what was held behind it goes on, so nothing stays held for good (core §4.3, §4.5 and §4.6, A-20d-5's release half).
#[spec("inbox/probe/086")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_086_a_cut_with_a_liability_out_and_no_segment_opens_a_humang() {
    for agent in [Agent::Claude, Agent::Codex] {
        for (what, steps) in cut_open_scripts(agent) {
            run_with(Opts::default(), agent, b"", false, &steps, &what).await;
        }
    }
}

// ---------------------------------------------------------------------------
// R-HOLD's limit as a bounded refusal (the user's ruling, 2026-11-03).
// ---------------------------------------------------------------------------

/// Helper `pid`'s record as [`helper`] scripts it.
fn record(pid: u32, seg: u64, decision: Decision) -> MRecord {
    match helper(pid, seg, decision) {
        Step::Observe(r) => r,
        other => unreachable!("{other:?}"),
    }
}

/// Scenario: For Claude and Codex, a person's Ctrl+G is held behind the deck's check and, once the check ends, written as its own trigger, which its helper answers by running the person's editor: the segment releases and the person's typing goes on to the pane, while the editor runs or after its helper reported it ended and exited. Under `Hold` that segment can never complete, so the person then presses Ctrl+G again with more typing after it, in one write: that Ctrl+G and what follows it are refused at once, their connection told `ctrl-g-refused`, none of it reaches the PTY and nothing stays held; what the person types next goes straight on, and the doorbell's tick no longer reads a check outstanding.
#[spec("inbox/probe/087")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_087_a_later_ctrl_g_behind_an_editor_segment_is_refused_and_typing_goes_on() {
    use super::segment_driver::{TickProbe, tick_probe};
    for agent in [Agent::Claude, Agent::Codex] {
        for ended in [false, true] {
            let what = format!("{} ended={ended}", agent_name(agent));
            let rig = Rig::new(agent, b"");
            rig.probe(|_| {}).await;
            rig.typed(b"\x07").await;
            rig.observe(&record(71, 1, Decision::Armed));
            rig.exit(71);
            for m in fast_return(agent) {
                rig.out(m);
            }
            rig.settle(&[CTRL_G, CTRL_G]).await;
            assert_eq!(
                rig.taken(),
                [CTRL_G, CTRL_G],
                "{what}: the probe, then the person's Ctrl+G as its own trigger"
            );
            rig.observe(&record(72, 2, Decision::Unarmed));
            rig.editor(72);
            if ended {
                rig.report(72, Report::Done(Tree::Clean));
                rig.exit(72);
            }
            rig.typed(b"ab").await;
            rig.settle(b"\x07\x07ab").await;
            assert_eq!(
                rig.taken(),
                b"\x07\x07ab",
                "{what}: typing goes to the pane behind the person's editor"
            );
            let (tx, mut notice) = tokio::sync::mpsc::channel(4);
            rig.typed_with(b"\x07cd", Some(tx)).await;
            let told = tokio::time::timeout(Duration::from_secs(3), notice.recv()).await;
            let status = rig.writer.lock().await.gate.gate.status();
            assert_eq!(
                status.held_ops, 0,
                "{what}: the later Ctrl+G and what follows it aren't held: {status:?}"
            );
            assert_eq!(
                told.ok().flatten().as_deref(),
                Some(&b"ctrl-g-refused"[..]),
                "{what}: its connection told why"
            );
            rig.typed(b"ef").await;
            rig.settle(b"\x07\x07abef").await;
            assert_eq!(
                rig.taken(),
                b"\x07\x07abef",
                "{what}: nothing of the refused write, and what is typed after it goes on"
            );
            assert!(
                !rig.ledger.any_outstanding(),
                "{what}: the refused write's ticket settled"
            );
            let status = rig.writer.lock().await.gate.gate.status();
            assert_eq!(
                tick_probe(&rig.input, status, Instant::now()),
                TickProbe::NotDue,
                "{what}: no check outstanding, so the doorbell's tick goes on"
            );
            assert_eq!(
                rig.writer
                    .lock()
                    .await
                    .pty
                    .in_flight
                    .count
                    .load(Ordering::SeqCst),
                0,
                "{what}: nothing left in flight"
            );
        }
    }
}

/// Scenario: A Claude Code generation shows its main composer on a real bus, and a probe is asked as the deck's probe request asks it; each time, the agent writes output between the probe's scheduling and its arm section, so the writer refuses it for the screen (`Admission(Surface)`). The first three refusals in a row count no failure and leave the schedule due; the fourth counts one, so the schedule backs off (A-20d-11 bounded, the revd21 review's U3-1). A counted failure starts the run over: after a person's ticket refuses one, three more refusals for the screen count nothing more.
#[spec("inbox/probe/084")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_084_a_screen_that_keeps_moving_before_the_arm_backs_off() {
    let schedule = |rig: &Rig| {
        rig.input
            .advance_schedule_for_e2e(Duration::ZERO, Instant::now())
    };
    let moved = |rig: &Arc<Rig>| {
        let rig = Arc::clone(rig);
        async move {
            rig.out(&claude_frame(80));
            rig.probe_admitted(|rig| rig.out(b"x")).await
        }
    };
    let rig = Rig::build(Agent::Claude, b"", true);
    for i in 1..=4u32 {
        assert_eq!(
            moved(&rig).await,
            ProbeOutcome::Refused("Admission(Surface)".to_string()),
            "refusal {i}"
        );
        let expected = if i < 4 {
            Some((true, 0, false))
        } else {
            Some((false, 1, false))
        };
        assert_eq!(schedule(&rig), expected, "after refusal {i} in a row");
    }
    assert!(rig.taken().is_empty(), "no byte written");

    // A ticket's refusal counts, and starts the run over.
    let rig = Rig::build(Agent::Claude, b"", true);
    rig.out(&claude_frame(80));
    let mut kept = None;
    let outcome = rig
        .probe_admitted(|rig| kept = Some(rig.ledger.admit()))
        .await;
    assert_eq!(
        outcome,
        ProbeOutcome::Refused("Admission(HumanTicket)".to_string())
    );
    // The person's write settles, so the next probes see no ticket.
    drop(kept);
    assert_eq!(schedule(&rig).map(|(_, failures, _)| failures), Some(1));
    for i in 1..=3u32 {
        assert_eq!(
            moved(&rig).await,
            ProbeOutcome::Refused("Admission(Surface)".to_string()),
            "refusal {i} after the ticket"
        );
        assert_eq!(
            schedule(&rig).map(|(_, failures, _)| failures),
            Some(1),
            "refusal {i} after the ticket counts nothing more"
        );
    }
}
