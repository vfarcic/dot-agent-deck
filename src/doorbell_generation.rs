//! PRD #1666 part 3, round 5-wake (plan §1.3, §3.4′, §4.4, §4.6): one
//! generation's doorbell state, and what the doorbell's admission reads from
//! it.
//!
//! A generation (an agent id) owns its [`LatchCell`], its checked attempt
//! numbers and its attempt ring for as long as it lives. The hook arm moves
//! the cell on receipt of exact ring evidence ([`GenerationDoorbell::evidence`]),
//! the user's own submit or clear lifts a suspension
//! ([`GenerationDoorbell::user_cleared`]), and retirement makes the cell
//! absorbing. A successor in the same pane is a new generation with its own.
//!
//! The admission's other inputs that belong to the generation are here too:
//! the input-mode evidence of an unwrapped pane, read from the deck's own PTY
//! master; the first-ring settle anchor; and the activation verdict for the
//! generation's effective agent type and latched release, against the
//! recorded measurements and the tripwires set since the daemon started.

use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use semver::Version;

use crate::doorbell_activation::{
    Activation, LatchedRelease, NoGo, ReleaseRecord, activation, unrecorded_claude_releases,
    without_unrecorded_claude,
};
use crate::doorbell_txn::{AttemptCounter, AttemptRing, Latch, LatchCell, RingToken};
use crate::event::AgentType;

/// The releases on which the deck recognised a measured dialog since it
/// started (plan §4.6, Q13): set only on an inherited release, and from then
/// on every generation of that release is held as `release_tripwire` until
/// the daemon restarts.
#[derive(Debug, Default)]
pub struct Tripwires {
    set: Mutex<Vec<(AgentType, Version)>>,
}

impl Tripwires {
    /// Set the tripwire for `agent`'s `release`; `true` when it was not set
    /// before.
    pub fn trip(&self, agent: &AgentType, release: &Version) -> bool {
        let mut set = self.set.lock().unwrap();
        if set.iter().any(|(a, r)| a == agent && r == release) {
            return false;
        }
        set.push((agent.clone(), release.clone()));
        true
    }

    pub fn tripped(&self) -> Vec<(AgentType, Version)> {
        self.set.lock().unwrap().clone()
    }
}

/// The verdict for a generation of `agent` whose header latched `latch`
/// (plan §4.6): `records` with the e2e build's Claude Code exclusion applied,
/// then the activation table. With no effective agent type there is no
/// profile to type under, and an agent type no record measures is
/// `unmeasured_release` whatever its start-up screen showed: the deck reads
/// no release from a screen it has no record of, so `unknown_release` would
/// send the user after a start-up screen that was never the cause.
pub fn generation_activation<'a>(
    records: &[ReleaseRecord<'a>],
    agent: Option<&AgentType>,
    latch: &LatchedRelease,
    tripwires: &[(AgentType, Version)],
) -> Activation<'a> {
    let Some(agent) = agent.filter(|a| **a != AgentType::None) else {
        return Activation::NoGo(NoGo::UnmeasuredRelease);
    };
    let records = without_unrecorded_claude(records, &unrecorded_claude_releases());
    if !records.iter().any(|record| record.agent == *agent) {
        return Activation::NoGo(NoGo::UnmeasuredRelease);
    }
    activation(&records, agent, latch, tripwires)
}

/// The clause a verdict holds a generation with, `None` for `Go` and
/// `Inherited`.
pub fn refused_clause(verdict: &Activation<'_>) -> Option<&'static str> {
    match verdict {
        Activation::NoGo(no_go) => Some(no_go.clause()),
        Activation::Go { .. } | Activation::Inherited { .. } => None,
    }
}

/// One generation's doorbell state. See the module documentation.
pub struct GenerationDoorbell {
    cell: LatchCell,
    counter: AttemptCounter,
    ring: AttemptRing,
    /// The pane's child is the deck's wrapper, which reports the agent's
    /// input mode itself: the master's own termios say nothing about it.
    wrapped: bool,
    /// A duplicate of the pane's PTY master, for an unwrapped pane's
    /// input-mode reads (plan §1.3): the deck's PTY slave is the agent's
    /// terminal, so the master's line discipline is the agent's.
    #[cfg(unix)]
    master: Option<std::os::fd::OwnedFd>,
    /// An unwrapped generation's settle anchor: when its first Clean frame
    /// showed the empty ready composer, `CLOCK_MONOTONIC` nanoseconds. Set
    /// once.
    unwrapped_anchor: OnceLock<u64>,
    /// A first attempt was admitted: later rings do not wait for the settle
    /// window (plan §4.4).
    first_admitted: AtomicBool,
    /// The e2e build's stand-in editor's executable, when its profile was
    /// selected for this generation.
    #[cfg(feature = "e2e")]
    standin: Option<std::path::PathBuf>,
    /// What to do when a move outside a transaction (the hook arm's
    /// evidence, the user's own clear) changed the cell: the registry asks
    /// for the generation's re-projection.
    on_moved: OnceLock<Box<dyn Fn() + Send + Sync>>,
}

impl std::fmt::Debug for GenerationDoorbell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GenerationDoorbell")
            .field("cell", &self.cell.load())
            .field("wrapped", &self.wrapped)
            .field("unwrapped_anchor", &self.unwrapped_anchor.get())
            .finish()
    }
}

impl GenerationDoorbell {
    /// A generation's state at spawn. `master` is the raw descriptor of the
    /// pane's PTY master, duplicated here so a read never races the master's
    /// close.
    pub fn new(wrapped: bool, #[cfg(unix)] master: Option<std::os::fd::RawFd>) -> Self {
        Self {
            cell: LatchCell::default(),
            counter: AttemptCounter::default(),
            ring: AttemptRing::default(),
            wrapped,
            #[cfg(unix)]
            master: master.and_then(duplicate),
            unwrapped_anchor: OnceLock::new(),
            first_admitted: AtomicBool::new(false),
            #[cfg(feature = "e2e")]
            standin: None,
            on_moved: OnceLock::new(),
        }
    }

    /// Call `moved` whenever the hook arm's evidence or the user's own clear
    /// moves the cell. Set once.
    pub fn on_moved(&self, moved: impl Fn() + Send + Sync + 'static) {
        let _ = self.on_moved.set(Box::new(moved));
    }

    fn moved(&self) {
        if let Some(moved) = self.on_moved.get() {
            moved();
        }
    }

    /// The e2e build's stand-in profile was selected for this generation,
    /// for `binary`.
    #[cfg(feature = "e2e")]
    pub fn with_standin(self, binary: Option<std::path::PathBuf>) -> Self {
        Self {
            standin: binary,
            ..self
        }
    }

    /// The stand-in's executable, when its profile was selected.
    #[cfg(feature = "e2e")]
    pub fn standin(&self) -> Option<&std::path::Path> {
        self.standin.as_deref()
    }

    /// A generation whose attempt numbers start at `next` (tests reach the
    /// reserved number).
    #[cfg(test)]
    pub fn starting_at(next: u32) -> Self {
        Self {
            counter: AttemptCounter::starting_at(next),
            ..Self::new(
                false,
                #[cfg(unix)]
                None,
            )
        }
    }

    pub fn cell(&self) -> &LatchCell {
        &self.cell
    }

    pub fn counter(&self) -> &AttemptCounter {
        &self.counter
    }

    pub fn ring(&self) -> &AttemptRing {
        &self.ring
    }

    pub fn wrapped(&self) -> bool {
        self.wrapped
    }

    /// Exact hook evidence for `token`, applied to the cell on receipt
    /// (plan §3.4′): the ring maps the token to its attempt and the cell's
    /// exact-attempt guard decides. `Some` with the cell's state when it
    /// moved.
    pub fn evidence(&self, token: &str, exact: bool) -> Option<Latch> {
        let token = RingToken::parse(token)?;
        let moved = crate::doorbell_txn::hook_evidence(&self.cell, &self.ring, token, exact)?;
        self.moved();
        Some(moved)
    }

    /// The user's own submit or clear, read from their bytes: a suspended
    /// attempt is resolved as the user's. `Some` with the cell's state when
    /// it moved.
    pub fn user_cleared(&self) -> Option<Latch> {
        self.cell.user_cleared().ok()?;
        self.moved();
        Some(self.cell.load())
    }

    /// The generation retired: its cell refuses every later move.
    pub fn retire(&self) {
        self.cell.retire();
    }

    /// Whether the doorbell is suspended for the generation (plan §3.4′).
    pub fn suspended(&self) -> bool {
        self.cell.load().suspended()
    }

    /// An unwrapped pane's input-mode evidence (plan §1.3): both `ICANON` and
    /// `ECHO` clear on the deck's own PTY master, read now. A wrapped pane's
    /// evidence comes from its wrapper's reports instead.
    pub fn unwrapped_mode(&self) -> Result<(), &'static str> {
        if self.wrapped {
            return Err(crate::input_mode::ModeClause::Unverified.as_str());
        }
        #[cfg(unix)]
        {
            let Some(master) = &self.master else {
                return Err(crate::input_mode::ModeClause::Unverified.as_str());
            };
            use std::os::fd::AsRawFd;
            // SAFETY: zeroed `termios` is a valid out-parameter, and the
            // descriptor is owned by this generation for the call.
            let mut termios: libc::termios = unsafe { std::mem::zeroed() };
            if unsafe { libc::tcgetattr(master.as_raw_fd(), &mut termios) } != 0 {
                return Err(crate::input_mode::ModeClause::Unverified.as_str());
            }
            if termios.c_lflag & (libc::ICANON | libc::ECHO) != 0 {
                Err(crate::input_mode::ModeClause::Cooked.as_str())
            } else {
                Ok(())
            }
        }
        #[cfg(not(unix))]
        {
            Err(crate::input_mode::ModeClause::Unverified.as_str())
        }
    }

    /// An unwrapped generation's first Clean frame showing the empty ready
    /// composer, at `now_ns`: the settle anchor, set once.
    pub fn note_ready_frame(&self, now_ns: u64) {
        let _ = self.unwrapped_anchor.set(now_ns);
    }

    /// The settle anchor (plan §4.4): a wrapped generation's is its first
    /// accepted both-clear sample's after-read stamp (`store_anchor`); an
    /// unwrapped one's, its first Clean empty-composer frame.
    pub fn settle_anchor(&self, store_anchor: Option<u64>) -> Option<u64> {
        if self.wrapped {
            store_anchor
        } else {
            store_anchor.or_else(|| self.unwrapped_anchor.get().copied())
        }
    }

    /// Whether the first ring still waits for the settle window to end at
    /// `now_ns`: before the anchor, or inside `settle` after it. Later rings
    /// do not wait.
    pub fn settling(
        &self,
        store_anchor: Option<u64>,
        settle: std::time::Duration,
        now_ns: u64,
    ) -> bool {
        if self.first_admitted.load(Ordering::Acquire) || settle.is_zero() {
            return false;
        }
        match self.settle_anchor(store_anchor) {
            None => true,
            Some(anchor) => {
                let end =
                    anchor.saturating_add(u64::try_from(settle.as_nanos()).unwrap_or(u64::MAX));
                now_ns < end
            }
        }
    }

    /// An attempt was admitted.
    pub fn admitted(&self) {
        self.first_admitted.store(true, Ordering::Release);
    }
}

#[cfg(unix)]
fn duplicate(fd: std::os::fd::RawFd) -> Option<std::os::fd::OwnedFd> {
    use std::os::fd::BorrowedFd;
    // SAFETY: the descriptor is the pane's PTY master, open for the duration
    // of this call (the caller holds the master); the duplicate is owned here.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    borrowed.try_clone_to_owned().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doorbell_txn::{Cause, Resolution};

    #[test]
    fn evidence_on_receipt_resolves_the_open_attempt_by_its_token() {
        let generation = GenerationDoorbell::starting_at(1);
        let token = RingToken::parse("abcdfg").unwrap();
        let attempt = generation.counter().allocate().unwrap();
        generation.ring().record(attempt, token);
        generation.cell().open(attempt).unwrap();
        assert_eq!(generation.evidence("abcdfg", false), None, "not exact");
        assert_eq!(generation.evidence("zzzzzz", true), None, "another token");
        assert_eq!(generation.evidence("ABCDFG", true), None, "not a token");
        assert_eq!(
            generation.evidence("abcdfg", true),
            Some(Latch::Resolved(attempt, Resolution::Submitted))
        );
    }

    #[test]
    fn a_users_clear_lifts_only_a_suspension() {
        let generation = GenerationDoorbell::starting_at(1);
        assert_eq!(generation.user_cleared(), None, "nothing suspended");
        let attempt = generation.counter().allocate().unwrap();
        generation.cell().open(attempt).unwrap();
        generation
            .cell()
            .suspend(attempt, Cause::NoSubmitEvidence)
            .unwrap();
        assert!(generation.suspended());
        assert_eq!(
            generation.user_cleared(),
            Some(Latch::Resolved(attempt, Resolution::User))
        );
        assert!(!generation.suspended());
        generation.retire();
        assert_eq!(generation.cell().load(), Latch::Retired);
        assert_eq!(generation.user_cleared(), None);
    }

    #[test]
    fn the_first_ring_waits_out_the_window_and_later_ones_do_not() {
        let generation = GenerationDoorbell::new(
            false,
            #[cfg(unix)]
            None,
        );
        let settle = std::time::Duration::from_secs(10);
        assert!(generation.settling(None, settle, 5), "no anchor yet");
        generation.note_ready_frame(1_000);
        generation.note_ready_frame(9_000_000_000);
        assert_eq!(generation.settle_anchor(None), Some(1_000), "set once");
        assert!(generation.settling(None, settle, 1_000 + 9_999_999_999));
        assert!(!generation.settling(None, settle, 1_000 + 10_000_000_000));
        let wrapped = GenerationDoorbell::new(
            true,
            #[cfg(unix)]
            None,
        );
        wrapped.note_ready_frame(1_000);
        assert_eq!(
            wrapped.settle_anchor(None),
            None,
            "a wrapper's report anchors it"
        );
        assert!(wrapped.settling(Some(5), settle, 6));
        wrapped.admitted();
        assert!(
            !wrapped.settling(None, settle, 6),
            "a later ring does not wait"
        );
    }

    #[test]
    fn a_wrapped_panes_master_is_no_evidence_and_no_master_is_unverified() {
        let wrapped = GenerationDoorbell::new(
            true,
            #[cfg(unix)]
            None,
        );
        assert_eq!(wrapped.unwrapped_mode(), Err("input_mode_unverified"));
        let unwrapped = GenerationDoorbell::new(
            false,
            #[cfg(unix)]
            None,
        );
        assert_eq!(unwrapped.unwrapped_mode(), Err("input_mode_unverified"));
    }

    #[test]
    fn a_tripwire_is_set_once_per_release() {
        let wires = Tripwires::default();
        let v = Version::new(0, 165, 0);
        assert!(wires.trip(&AgentType::Codex, &v));
        assert!(!wires.trip(&AgentType::Codex, &v));
        assert!(wires.trip(&AgentType::ClaudeCode, &v));
        assert_eq!(wires.tripped().len(), 2);
    }

    #[test]
    fn a_generation_with_no_agent_type_has_no_profile_to_type_under() {
        let records = &crate::doorbell_activation::RELEASE_RECORDS;
        let latch = LatchedRelease::Known(Version::new(2, 1, 310));
        assert_eq!(
            refused_clause(&generation_activation(records, None, &latch, &[])),
            Some("unmeasured_release")
        );
        assert_eq!(
            refused_clause(&generation_activation(
                records,
                Some(&AgentType::None),
                &latch,
                &[]
            )),
            Some("unmeasured_release")
        );
        assert_eq!(
            refused_clause(&generation_activation(
                records,
                Some(&AgentType::ClaudeCode),
                &latch,
                &[]
            )),
            None
        );
    }

    #[test]
    fn an_agent_type_no_record_measures_is_unmeasured_whatever_its_latch() {
        let records = &crate::doorbell_activation::RELEASE_RECORDS;
        for agent in [AgentType::OpenCode, AgentType::Pi, AgentType::Devin] {
            for latch in [
                LatchedRelease::Unseen,
                LatchedRelease::Known(Version::new(1, 0, 0)),
            ] {
                assert_eq!(
                    refused_clause(&generation_activation(records, Some(&agent), &latch, &[])),
                    Some("unmeasured_release"),
                    "{agent:?} {latch:?}"
                );
            }
        }
        // An agent type with records still reads an unseen header as such.
        assert_eq!(
            refused_clause(&generation_activation(
                records,
                Some(&AgentType::ClaudeCode),
                &LatchedRelease::Unseen,
                &[]
            )),
            Some("unknown_release")
        );
    }
}
