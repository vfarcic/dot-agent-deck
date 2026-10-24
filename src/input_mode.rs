//! PRD #1666 part 3, round 5-wake (plan §1.3): evidence that an agent reads
//! its input byte by byte with no kernel echo, taken from the line discipline
//! of the PTY the agent actually reads.
//!
//! For a wrapped agent (Codex) that PTY is the wrapper's inner one, which only
//! the wrapper can read. So the wrapper samples `ICANON` and `ECHO` there on
//! every supervisory tick, stamping each sample with `CLOCK_MONOTONIC` before
//! and after the read, and reports it on the hook socket (`input_mode`). The
//! daemon keeps, per generation, an [`InputModeStore`]: the observer the first
//! accepted report bound (its pid and start time), the newest sample by the
//! observer's own sequence number, and the settle anchor. A report is accepted
//! only from the bound observer, in sequence, with a sample whose two stamps
//! are ordered and close together and none later than the daemon's own clock.
//!
//! Freshness is the sample's age, never when the report arrived: a sample is
//! usable for [`MODE_MAX_AGE`] from the stamp taken before its read.

use std::path::PathBuf;

/// How often the wrapper samples the inner PTY.
pub const MODE_TICK: std::time::Duration = std::time::Duration::from_millis(50);

/// How often the wrapper re-reports the bits when they have not changed.
pub const MODE_REASSERT: std::time::Duration = std::time::Duration::from_secs(1);

/// How old a sample may be, from its first stamp, to count: the re-assertion
/// interval plus 250 ms for the tick and the send.
pub const MODE_MAX_AGE_NS: u64 = 1_250_000_000;

/// How long the wrapper waits on one send.
pub const MODE_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// The capability version the daemon advertises for `input_mode` reports
/// (`HookCapabilitiesReply::input_mode_report`).
pub const INPUT_MODE_REPORT: u32 = 1;

/// One sample, as the wrapper reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeSample {
    pub seq: u64,
    pub icanon: bool,
    pub echo: bool,
    /// `CLOCK_MONOTONIC` read before the `tcgetattr`.
    pub sampled_mono_ns: u64,
    /// `CLOCK_MONOTONIC` read after it.
    pub sample_done_mono_ns: u64,
}

impl ModeSample {
    pub fn both_clear(&self) -> bool {
        !self.icanon && !self.echo
    }
}

/// The process a report speaks for: its pid and when it started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observer {
    pub pid: u32,
    pub start: u64,
}

/// Why the evidence cannot be used now, as the card's `cannot_type` clause
/// names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeClause {
    Unverified,
    Cooked,
    Stale,
    Conflict,
    ClockNamespace,
    ClockNamespaceUnreadable,
}

impl ModeClause {
    pub fn as_str(self) -> &'static str {
        match self {
            ModeClause::Unverified => "input_mode_unverified",
            ModeClause::Cooked => "input_mode_cooked",
            ModeClause::Stale => "input_mode_stale",
            ModeClause::Conflict => "input_mode_conflict",
            ModeClause::ClockNamespace => "clock_namespace",
            ModeClause::ClockNamespaceUnreadable => "clock_namespace_unreadable",
        }
    }

    /// Whether the clause holds for the rest of the generation (Q18).
    pub fn sticky(self) -> bool {
        matches!(
            self,
            ModeClause::Conflict
                | ModeClause::ClockNamespace
                | ModeClause::ClockNamespaceUnreadable
        )
    }
}

/// Why a report was not stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The two stamps are out of order, or further apart than
    /// [`MODE_MAX_AGE_NS`].
    WideSample,
    /// The sample was stamped after the daemon's own clock.
    ClockMismatch,
    /// A second process reported for the generation.
    Conflict,
    /// The generation's evidence is already held by a sticky clause.
    Held(ModeClause),
}

impl Refusal {
    pub fn code(self) -> &'static str {
        match self {
            Refusal::WideSample => "wide_sample",
            Refusal::ClockMismatch => "clock_mismatch",
            Refusal::Conflict => "input_mode_conflict",
            Refusal::Held(clause) => clause.as_str(),
        }
    }
}

/// What happened to an accepted report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stored {
    /// The newest sample now.
    Newer,
    /// A sequence number already stored: ignored and counted.
    Duplicate,
    /// An older sequence number: ignored and counted.
    Regression,
}

/// One generation's input-mode evidence. See the module documentation.
#[derive(Debug, Default, Clone)]
pub struct InputModeStore {
    observer: Option<Observer>,
    /// The two observers, when a second one reported.
    conflict: Option<(Observer, Observer)>,
    /// A sticky clause other than the conflict: the clock checks.
    held: Option<ModeClause>,
    latest: Option<ModeSample>,
    anchor: Option<u64>,
    pub duplicates: u64,
    pub regressions: u64,
    pub refused: u64,
}

impl InputModeStore {
    pub fn observer(&self) -> Option<Observer> {
        self.observer
    }

    pub fn conflict(&self) -> Option<(Observer, Observer)> {
        self.conflict
    }

    pub fn latest(&self) -> Option<ModeSample> {
        self.latest
    }

    /// The settle anchor: the after-read stamp of the first accepted
    /// both-clear sample, set once.
    pub fn anchor(&self) -> Option<u64> {
        self.anchor
    }

    /// The sticky clause holding the generation, if any.
    pub fn sticky(&self) -> Option<ModeClause> {
        if self.conflict.is_some() {
            Some(ModeClause::Conflict)
        } else {
            self.held
        }
    }

    /// A report the daemon refused for a sticky reason it found itself (the
    /// time-namespace checks): the generation holds it from now on.
    pub fn hold(&mut self, clause: ModeClause) {
        debug_assert!(clause.sticky());
        if self.held.is_none() {
            self.held = Some(clause);
        }
    }

    /// Store `sample` from `peer`, at the daemon's `now` (`CLOCK_MONOTONIC`).
    pub fn accept(
        &mut self,
        peer: Observer,
        sample: ModeSample,
        now: u64,
    ) -> Result<Stored, Refusal> {
        if let Some(clause) = self.sticky() {
            self.refused += 1;
            return Err(Refusal::Held(clause));
        }
        if sample.sample_done_mono_ns < sample.sampled_mono_ns
            || sample.sample_done_mono_ns - sample.sampled_mono_ns > MODE_MAX_AGE_NS
        {
            self.refused += 1;
            return Err(Refusal::WideSample);
        }
        if sample.sample_done_mono_ns > now {
            self.refused += 1;
            self.held = Some(ModeClause::ClockNamespace);
            return Err(Refusal::ClockMismatch);
        }
        match self.observer {
            None => self.observer = Some(peer),
            Some(bound) if bound != peer => {
                self.refused += 1;
                self.conflict = Some((bound, peer));
                return Err(Refusal::Conflict);
            }
            Some(_) => {}
        }
        if let Some(latest) = self.latest {
            if sample.seq == latest.seq {
                self.duplicates += 1;
                return Ok(Stored::Duplicate);
            }
            if sample.seq < latest.seq {
                self.regressions += 1;
                return Ok(Stored::Regression);
            }
        }
        self.latest = Some(sample);
        if self.anchor.is_none() && sample.both_clear() {
            self.anchor = Some(sample.sample_done_mono_ns);
        }
        Ok(Stored::Newer)
    }

    /// The newest sample, if it is usable at `now`: an observer bound, no
    /// sticky clause, both bits clear, its stamps ordered and no later than
    /// `now`, and at most [`MODE_MAX_AGE_NS`] old from its first stamp.
    pub fn usable(&self, now: u64) -> Result<ModeSample, ModeClause> {
        if let Some(clause) = self.sticky() {
            return Err(clause);
        }
        let Some(sample) = self.latest else {
            return Err(ModeClause::Unverified);
        };
        if !sample.both_clear() {
            return Err(ModeClause::Cooked);
        }
        if sample.sampled_mono_ns > sample.sample_done_mono_ns || sample.sample_done_mono_ns > now {
            return Err(ModeClause::ClockNamespace);
        }
        if now - sample.sampled_mono_ns > MODE_MAX_AGE_NS {
            return Err(ModeClause::Stale);
        }
        Ok(sample)
    }
}

/// A read of a process's time-namespace link (`/proc/<pid>/ns/time`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamespaceRead {
    Link(PathBuf),
    /// `ENOENT`: for the daemon's own link, the kernel has no time namespaces.
    NotFound,
    /// Any other failure (`ESRCH`, `EACCES`, …).
    Unreadable(std::io::ErrorKind),
}

impl NamespaceRead {
    pub fn of(result: std::io::Result<PathBuf>) -> Self {
        match result {
            Ok(link) => NamespaceRead::Link(link),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => NamespaceRead::NotFound,
            Err(e) => NamespaceRead::Unreadable(e.kind()),
        }
    }
}

/// Whether a peer's samples are on the daemon's clock (Linux): the daemon's
/// own link `own`, read once at its start, against the peer's `peer`. Only
/// when the daemon's own link is absent (no time namespaces) is an absent
/// peer link accepted; any other failure of the daemon's own read refuses
/// every report; when the daemon's link exists, the peer's must name the same
/// namespace.
pub fn namespace_verdict(own: &NamespaceRead, peer: &NamespaceRead) -> Result<(), ModeClause> {
    match (own, peer) {
        (NamespaceRead::Unreadable(_), _) => Err(ModeClause::ClockNamespaceUnreadable),
        (NamespaceRead::NotFound, NamespaceRead::NotFound) => Ok(()),
        (NamespaceRead::NotFound, _) => Err(ModeClause::ClockNamespace),
        (NamespaceRead::Link(own), NamespaceRead::Link(peer)) if own == peer => Ok(()),
        (NamespaceRead::Link(_), _) => Err(ModeClause::ClockNamespace),
    }
}

/// The time-namespace link of `pid` (`None` for the daemon itself).
#[cfg(target_os = "linux")]
pub fn read_time_namespace(pid: Option<u32>) -> NamespaceRead {
    let path = match pid {
        Some(pid) => format!("/proc/{pid}/ns/time"),
        None => "/proc/self/ns/time".to_string(),
    };
    NamespaceRead::of(std::fs::read_link(path))
}

/// The daemon's own time-namespace link, read once.
#[cfg(target_os = "linux")]
pub fn own_time_namespace() -> &'static NamespaceRead {
    static OWN: std::sync::OnceLock<NamespaceRead> = std::sync::OnceLock::new();
    OWN.get_or_init(|| read_time_namespace(None))
}

/// `CLOCK_MONOTONIC`, in nanoseconds, the clock both the wrapper's stamps and
/// the daemon's checks read. `None` if the clock cannot be read.
#[cfg(unix)]
pub fn monotonic_ns() -> Option<u64> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a stack-allocated `timespec` the call writes into.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    (rc == 0).then(|| ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64)
}
