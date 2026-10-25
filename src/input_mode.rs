//! PRD #1666 part 3, round 5-wake (plan §1.3): evidence that an agent reads
//! its input byte by byte with no kernel echo, taken from the line discipline
//! of the PTY the agent actually reads.
//!
//! For a wrapped agent (Codex) that PTY is the wrapper's inner one, whose
//! master the wrapper holds. So the wrapper samples `ICANON` and `ECHO` there on
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

/// What one supervisory tick of [`InputModeWatch`] produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tick {
    /// A report to send.
    Report(ModeSample),
    /// A sample taken; the bits did not change and no re-assertion is due.
    Unchanged,
    /// The two stamps were out of order or further apart than
    /// [`MODE_MAX_AGE_NS`]: discarded, since it would arrive stale anyway.
    WideSample(ModeSample),
    /// A clock read failed: no sample.
    ClockFailed,
    /// The `tcgetattr` failed: no sample.
    TermiosFailed,
}

/// The wrapper's producer (plan §1.3): on each supervisory tick, read the
/// clock, the inner PTY's two bits, and the clock again, and report on every
/// change of either bit and every [`MODE_REASSERT`] whatever the bits.
/// `seq` is assigned when the sample is taken, so a higher one is newer.
#[derive(Debug, Default)]
pub struct InputModeWatch {
    next_seq: u64,
    /// The bits and first stamp of the last report produced.
    reported: Option<(bool, bool, u64)>,
}

impl InputModeWatch {
    /// One tick: `clock` is read before and after `bits`, which returns
    /// `(icanon, echo)` from the inner master, or `None` when it fails.
    pub fn tick(
        &mut self,
        clock: &mut impl FnMut() -> Option<u64>,
        bits: impl FnOnce() -> Option<(bool, bool)>,
    ) -> Tick {
        let Some(sampled) = clock() else {
            return Tick::ClockFailed;
        };
        let Some((icanon, echo)) = bits() else {
            return Tick::TermiosFailed;
        };
        let Some(done) = clock() else {
            return Tick::ClockFailed;
        };
        self.next_seq += 1;
        let sample = ModeSample {
            seq: self.next_seq,
            icanon,
            echo,
            sampled_mono_ns: sampled,
            sample_done_mono_ns: done,
        };
        if done < sampled || done - sampled > MODE_MAX_AGE_NS {
            return Tick::WideSample(sample);
        }
        let due = match self.reported {
            None => true,
            Some((was_icanon, was_echo, at)) => {
                was_icanon != icanon
                    || was_echo != echo
                    || sampled.saturating_sub(at) >= MODE_REASSERT.as_nanos() as u64
            }
        };
        if !due {
            return Tick::Unchanged;
        }
        self.reported = Some((icanon, echo, sampled));
        Tick::Report(sample)
    }
}

/// The daemon at the other end of one connection: the listening process's
/// pid, from the connection's credentials, and its start time, from the
/// process table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DaemonIdentity {
    pub pid: u32,
    pub start: u64,
}

/// Whether a daemon stores `input_mode` reports, as its probe answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Supported,
    Unsupported,
}

/// How many daemon identities [`VerdictCache`] remembers.
pub const VERDICT_CACHE_LEN: usize = 8;

/// The wrapper's verdicts per daemon identity, the least recently used
/// evicted past [`VERDICT_CACHE_LEN`].
#[derive(Debug, Default)]
pub struct VerdictCache {
    /// Least recently used first.
    entries: Vec<(DaemonIdentity, Verdict)>,
}

impl VerdictCache {
    /// `identity`'s verdict, marking it used.
    pub fn get(&mut self, identity: DaemonIdentity) -> Option<Verdict> {
        let at = self.entries.iter().position(|(id, _)| *id == identity)?;
        let entry = self.entries.remove(at);
        self.entries.push(entry);
        Some(entry.1)
    }

    /// Record `verdict` for `identity`; the identity evicted to make room.
    pub fn record(&mut self, identity: DaemonIdentity, verdict: Verdict) -> Option<DaemonIdentity> {
        self.entries.retain(|(id, _)| *id != identity);
        self.entries.push((identity, verdict));
        (self.entries.len() > VERDICT_CACHE_LEN).then(|| self.entries.remove(0).0)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// What one connection carried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exchange {
    /// The report was written, after a probe on this connection when
    /// `probed`; the daemon's answer, when one came before the deadline.
    Sent { probed: bool, ack: Option<String> },
    /// The daemon's verdict is Unsupported: no report written.
    Withheld { probed: bool },
    /// The connection ended (EOF, a reset, a failed write) or the deadline
    /// passed before a whole answer. A probe was written unless the probe's
    /// own write failed (`probe_write_failed`). This connection recorded no
    /// verdict, except that a report whose write failed after its probe was
    /// answered (`report_write_failed`) leaves the Supported verdict that
    /// answer recorded.
    Ended { why: &'static str },
    /// The daemon's identity could not be read: nothing written.
    IdentityUnreadable,
    /// The process at the other end runs as another user: nothing written.
    ForeignPeer,
    /// The connection's next read or write could not be bounded by the
    /// deadline, and nothing more was written. When that was the probe's
    /// write or the read of its answer, no verdict was recorded; when it was
    /// the report's write, the Supported verdict that let the report go stays
    /// recorded (answered after a probe written on this connection, or
    /// remembered from an earlier one). A failed bound on the
    /// acknowledgement's read is not this: the report was written, and the
    /// exchange is [`Exchange::Sent`] with no acknowledgement.
    Unbounded,
}

/// The longest answer line [`exchange`] reads.
const EXCHANGE_MAX_LINE: usize = 64 * 1024;

/// How long the daemon must have stayed silent on a probe, with the
/// connection open, for that to record Unsupported: a fail-closed trade of
/// availability, so the wrapper does not judge a daemon after too little
/// observation. It does not tell a slow live daemon from one that does not
/// support reports: a supporting daemon whose answer's first byte comes after
/// the deadline is still recorded Unsupported, and stays so while its
/// identity is remembered. A probe sent with less than this
/// left (a slow connect used the rest) records nothing, so a silent daemon
/// whose connects keep using the rest is probed again on each connection,
/// each probe bounded by its send's deadline.
pub const MODE_PROBE_SILENCE: std::time::Duration = std::time::Duration::from_millis(250);

/// A connection whose next read and write [`exchange`] can bound.
pub trait BoundedConn: std::io::Read + std::io::Write {
    /// Make the next read and the next write give up after `left`.
    fn bound(&mut self, left: std::time::Duration) -> std::io::Result<()>;
}

#[cfg(unix)]
impl BoundedConn for std::os::unix::net::UnixStream {
    fn bound(&mut self, left: std::time::Duration) -> std::io::Result<()> {
        self.set_read_timeout(Some(left))?;
        self.set_write_timeout(Some(left))
    }
}

/// Why a read or write of one line stopped.
#[derive(Debug)]
enum LineFail {
    /// The deadline passed (or the socket's timeout, which is what was left
    /// of it), after `got` bytes of the line.
    Deadline { got: usize },
    /// The bound could not be installed.
    Unbounded,
    /// The line was longer than [`EXCHANGE_MAX_LINE`].
    TooLong,
    /// Any other failure: a reset, a failed write.
    Io,
}

/// Bound `conn`'s next read and write by what is left until `deadline`.
fn arm<C: BoundedConn>(conn: &mut C, deadline: std::time::Instant) -> Result<(), LineFail> {
    match deadline.checked_duration_since(std::time::Instant::now()) {
        Some(left) if !left.is_zero() => conn.bound(left).map_err(|_| LineFail::Unbounded),
        _ => Err(LineFail::Deadline { got: 0 }),
    }
}

/// One line from `conn` by `deadline`: `Ok(None)` at EOF before a newline.
/// Each read is bounded by what is left of the deadline, so a peer that
/// drips its answer cannot stretch the whole line past it.
fn read_line<C: BoundedConn>(
    conn: &mut C,
    deadline: std::time::Instant,
) -> Result<Option<String>, LineFail> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        arm(conn, deadline).map_err(|fail| match fail {
            LineFail::Deadline { .. } => LineFail::Deadline { got: line.len() },
            other => other,
        })?;
        match conn.read(&mut byte) {
            Ok(0) => return Ok(None),
            Ok(_) if byte[0] == b'\n' => {
                return Ok(Some(String::from_utf8_lossy(&line).into_owned()));
            }
            Ok(_) if line.len() >= EXCHANGE_MAX_LINE => return Err(LineFail::TooLong),
            Ok(_) => line.push(byte[0]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err(LineFail::Deadline { got: line.len() });
            }
            Err(_) => return Err(LineFail::Io),
        }
    }
}

/// Write `line` and a newline to `conn` by `deadline`, each write bounded by
/// what is left of it.
fn write_line<C: BoundedConn>(
    conn: &mut C,
    line: &str,
    deadline: std::time::Instant,
) -> Result<(), LineFail> {
    let bytes = format!("{line}\n");
    let mut rest = bytes.as_bytes();
    while !rest.is_empty() {
        arm(conn, deadline)?;
        match conn.write(rest) {
            Ok(0) => return Err(LineFail::Io),
            Ok(n) => rest = &rest[n..],
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err(LineFail::Deadline { got: 0 });
            }
            Err(_) => return Err(LineFail::Io),
        }
    }
    conn.flush().map_err(|_| LineFail::Io)
}

/// One connection's part of the carrier (plan §1.3), with the daemon at its
/// other end already identified (`None`: unreadable), every read and write
/// done by `deadline`. A cached verdict decides at once; otherwise one
/// `probe` line asks, and the answer is recorded and decides, with the
/// `report` line written on the same connection only for
/// `input_mode_report: Some(1)`. A reply without it, a refusal, a malformed
/// line, and silence through the deadline after a probe written with at
/// least [`MODE_PROBE_SILENCE`] of it left record Unsupported. A connection
/// that ends before the probe is answered, an answer cut off by the
/// deadline, silence after a probe written later than that, and a bound that
/// fails before the answer is read record nothing; after a Supported answer
/// the verdict stays recorded whatever happens to the report and its
/// acknowledgement. Returns what happened and the identity evicted to record
/// a verdict.
pub fn exchange<C: BoundedConn>(
    conn: &mut C,
    identity: Option<DaemonIdentity>,
    cache: &mut VerdictCache,
    probe: &str,
    report: &str,
    deadline: std::time::Instant,
) -> (Exchange, Option<DaemonIdentity>) {
    let Some(identity) = identity else {
        return (Exchange::IdentityUnreadable, None);
    };
    let send = |conn: &mut C, probed| match write_line(conn, report, deadline) {
        Err(LineFail::Unbounded) => Exchange::Unbounded,
        Err(_) => Exchange::Ended {
            why: "report_write_failed",
        },
        Ok(()) => Exchange::Sent {
            probed,
            ack: read_line(conn, deadline).ok().flatten(),
        },
    };
    match cache.get(identity) {
        Some(Verdict::Supported) => return (send(conn, false), None),
        Some(Verdict::Unsupported) => return (Exchange::Withheld { probed: false }, None),
        None => {}
    }
    match write_line(conn, probe, deadline) {
        Ok(()) => {}
        Err(LineFail::Unbounded) => return (Exchange::Unbounded, None),
        Err(_) => {
            return (
                Exchange::Ended {
                    why: "probe_write_failed",
                },
                None,
            );
        }
    }
    let asked = std::time::Instant::now();
    let supported = match read_line(conn, deadline) {
        Ok(Some(answer)) => {
            serde_json::from_str::<crate::event::HookCapabilitiesReply>(answer.trim())
                .ok()
                .is_some_and(|reply| {
                    reply.code.is_none() && reply.input_mode_report == Some(INPUT_MODE_REPORT)
                })
        }
        Ok(None) => return (Exchange::Ended { why: "eof" }, None),
        Err(LineFail::Deadline { got: 0 })
            if deadline.saturating_duration_since(asked) >= MODE_PROBE_SILENCE =>
        {
            false
        }
        Err(LineFail::Deadline { .. }) => return (Exchange::Ended { why: "deadline" }, None),
        Err(LineFail::TooLong) => false,
        Err(LineFail::Unbounded) => return (Exchange::Unbounded, None),
        Err(LineFail::Io) => return (Exchange::Ended { why: "reset" }, None),
    };
    if supported {
        let evicted = cache.record(identity, Verdict::Supported);
        (send(conn, true), evicted)
    } else {
        let evicted = cache.record(identity, Verdict::Unsupported);
        (Exchange::Withheld { probed: true }, evicted)
    }
}

/// How the sender reaches and identifies the daemon at the other end of a
/// connection, injectable for tests.
#[cfg(unix)]
pub struct IdentityLookups {
    /// Connect within the budget. The platform's is the checked IPC client
    /// ([`crate::platform::ipc::IpcClient::connect_timeout`]), which bounds
    /// the connect and refuses a listener another user runs.
    pub connect: fn(
        &std::path::Path,
        std::time::Duration,
    ) -> std::io::Result<std::os::unix::net::UnixStream>,
    /// The connected peer's uid, read again before the first byte is written.
    pub peer_uid: fn(&std::os::unix::net::UnixStream) -> Option<u32>,
    pub peer_pid: fn(&std::os::unix::net::UnixStream) -> Option<u32>,
    pub started: fn(u32) -> Option<u64>,
}

#[cfg(unix)]
impl IdentityLookups {
    pub const PLATFORM: Self = Self {
        connect: |path, budget| {
            crate::platform::ipc::IpcClient::connect_timeout(path, budget)
                .map(crate::platform::ipc::IpcClient::into_std)
        },
        peer_uid: |stream| {
            use std::os::fd::AsRawFd;
            crate::platform::peercred::peer_uid_raw(stream.as_raw_fd()).ok()
        },
        peer_pid: |stream| {
            use std::os::fd::AsRawFd;
            crate::platform::peercred::peer_pid_raw(stream.as_raw_fd()).ok()
        },
        started: crate::platform::proc::process_start_of,
    };

    fn identify(&self, stream: &std::os::unix::net::UnixStream) -> Option<DaemonIdentity> {
        let pid = (self.peer_pid)(stream)?;
        let start = (self.started)(pid)?;
        Some(DaemonIdentity { pid, start })
    }
}

/// What the wrapper's sender reports for: its pane and the pane's token.
#[derive(Debug, Clone)]
pub struct ReportTarget {
    pub socket: PathBuf,
    pub pane_id: String,
    pub token: Option<String>,
}

#[cfg(unix)]
impl ReportTarget {
    fn probe_line(&self) -> String {
        serde_json::to_string(&crate::event::DaemonMessage::HookCapabilities(
            crate::event::HookCapabilitiesRequest {
                pane_id: self.pane_id.clone(),
                token: self.token.clone(),
            },
        ))
        .unwrap_or_default()
    }

    fn report_line(&self, sample: &ModeSample) -> String {
        serde_json::to_string(&crate::event::DaemonMessage::InputMode(
            crate::event::InputModeReport {
                pane_id: self.pane_id.clone(),
                token: self.token.clone(),
                seq: sample.seq,
                icanon: sample.icanon,
                echo: sample.echo,
                sampled_mono_ns: sample.sampled_mono_ns,
                sample_done_mono_ns: sample.sample_done_mono_ns,
            },
        ))
        .unwrap_or_default()
    }

    /// Connect, check the peer is this user, identify the daemon and carry
    /// `sample`, the whole of it (connect, probe, report and answers) bounded
    /// by one [`MODE_SEND_TIMEOUT`] deadline. `None` when no daemon answered
    /// the connect within it (a missing or stale socket, a full accept
    /// queue, a listener another user runs).
    pub fn send(
        &self,
        sample: &ModeSample,
        cache: &mut VerdictCache,
        lookups: &IdentityLookups,
    ) -> Option<(Exchange, Option<DaemonIdentity>, Option<DaemonIdentity>)> {
        let deadline = std::time::Instant::now() + MODE_SEND_TIMEOUT;
        let mut stream = (lookups.connect)(
            &self.socket,
            deadline.saturating_duration_since(std::time::Instant::now()),
        )
        .ok()?;
        if (lookups.peer_uid)(&stream) != Some(crate::platform::paths::current_uid()) {
            return Some((Exchange::ForeignPeer, None, None));
        }
        let identity = lookups.identify(&stream);
        let (outcome, evicted) = exchange(
            &mut stream,
            identity,
            cache,
            &self.probe_line(),
            &self.report_line(sample),
            deadline,
        );
        Some((outcome, identity, evicted))
    }
}

/// The wrapper's sender thread with its latest-wins slot (the `LatestSend`
/// pattern): the tick posts, the thread carries the newest sample waiting,
/// dropping one already older than [`MODE_MAX_AGE_NS`] when its turn comes.
/// Each send ends by its deadline, so a closed sender's thread ends within
/// one [`MODE_SEND_TIMEOUT`] of [`Self::close`].
#[cfg(unix)]
pub struct ModeSender {
    slot: std::sync::Mutex<Option<ModeSample>>,
    ready: std::sync::Condvar,
    closed: std::sync::atomic::AtomicBool,
    /// Set as the thread ends.
    ended: std::sync::atomic::AtomicBool,
    /// A test's hold on the thread's next turn: it says which sample it
    /// took, then waits to be let go before sending it.
    #[cfg(test)]
    turn_gate:
        std::sync::Mutex<Option<(std::sync::mpsc::Sender<u64>, std::sync::mpsc::Receiver<()>)>>,
}

#[cfg(unix)]
impl ModeSender {
    pub fn spawn(target: ReportTarget, lookups: IdentityLookups) -> std::sync::Arc<Self> {
        let sender = std::sync::Arc::new(Self {
            slot: std::sync::Mutex::new(None),
            ready: std::sync::Condvar::new(),
            closed: std::sync::atomic::AtomicBool::new(false),
            ended: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            turn_gate: std::sync::Mutex::new(None),
        });
        let worker = std::sync::Arc::clone(&sender);
        std::thread::spawn(move || {
            let mut cache = VerdictCache::default();
            let mut unreadable_streak = 0u64;
            let mut foreign_streak = 0u64;
            while let Some(sample) = worker.next() {
                let fresh = monotonic_ns().is_some_and(|now| {
                    now.saturating_sub(sample.sampled_mono_ns) <= MODE_MAX_AGE_NS
                });
                if !fresh {
                    tracing::debug!(
                        seq = sample.seq,
                        "wrap input mode: report stale at its turn, dropped"
                    );
                    continue;
                }
                #[cfg(test)]
                {
                    let gate = worker
                        .turn_gate
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .take();
                    if let Some((at, go)) = gate {
                        let _ = at.send(sample.seq);
                        let _ = go.recv();
                    }
                }
                let Some((outcome, identity, evicted)) = target.send(&sample, &mut cache, &lookups)
                else {
                    continue;
                };
                if let Some(evicted) = evicted {
                    tracing::info!(
                        pid = evicted.pid,
                        start = evicted.start,
                        "wrap input mode: daemon verdict evicted"
                    );
                }
                if !matches!(outcome, Exchange::ForeignPeer) && foreign_streak > 0 {
                    foreign_streak = 0;
                }
                match (&outcome, identity) {
                    (Exchange::ForeignPeer, _) => {
                        if foreign_streak == 0 {
                            tracing::warn!(
                                "wrap input mode: the process at the daemon's socket runs as another user; report withheld"
                            );
                        }
                        foreign_streak += 1;
                        continue;
                    }
                    (Exchange::Unbounded, _) => {
                        tracing::info!(
                            "wrap input mode: the connection could not be bounded; report withheld"
                        );
                    }
                    (Exchange::IdentityUnreadable, _) => {
                        if unreadable_streak == 0 {
                            tracing::info!(
                                "wrap input mode: the daemon's identity could not be read; report withheld"
                            );
                        }
                        unreadable_streak += 1;
                        continue;
                    }
                    (Exchange::Sent { probed: true, .. }, Some(id)) => {
                        tracing::info!(
                            pid = id.pid,
                            start = id.start,
                            verdict = "supported",
                            probed = true,
                            "wrap input mode: daemon identity"
                        );
                    }
                    (Exchange::Withheld { probed }, Some(id)) => {
                        if *probed {
                            tracing::info!(
                                pid = id.pid,
                                start = id.start,
                                verdict = "unsupported",
                                probed = true,
                                "wrap input mode: daemon identity"
                            );
                        } else {
                            tracing::debug!(
                                pid = id.pid,
                                seq = sample.seq,
                                "wrap input mode: report withheld"
                            );
                        }
                    }
                    (Exchange::Ended { why }, Some(id)) => {
                        tracing::info!(
                            pid = id.pid,
                            start = id.start,
                            why,
                            "wrap input mode: connection ended before an answer"
                        );
                    }
                    _ => {}
                }
                if unreadable_streak > 0 {
                    tracing::info!(
                        streak = unreadable_streak,
                        "wrap input mode: the daemon's identity is readable again"
                    );
                    unreadable_streak = 0;
                }
            }
            worker
                .ended
                .store(true, std::sync::atomic::Ordering::SeqCst);
        });
        sender
    }

    /// Whether the thread has ended.
    pub fn ended(&self) -> bool {
        self.ended.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Hold the thread's next turn: the returned receiver says which sample
    /// it took (once fresh), and it sends nothing until the returned sender
    /// lets it go.
    #[cfg(test)]
    fn hold_next_turn(&self) -> (std::sync::mpsc::Receiver<u64>, std::sync::mpsc::Sender<()>) {
        let (at_tx, at_rx) = std::sync::mpsc::channel();
        let (go_tx, go_rx) = std::sync::mpsc::channel();
        *self.turn_gate.lock().unwrap_or_else(|p| p.into_inner()) = Some((at_tx, go_rx));
        (at_rx, go_tx)
    }

    /// Wait for the next sample, or `None` once closed.
    fn next(&self) -> Option<ModeSample> {
        let mut slot = self.slot.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
                return None;
            }
            if let Some(sample) = slot.take() {
                return Some(sample);
            }
            slot = self.ready.wait(slot).unwrap_or_else(|p| p.into_inner());
        }
    }

    /// Replace whatever is waiting with `sample`, unless closed.
    pub fn post(&self, sample: ModeSample) {
        let mut slot = self.slot.lock().unwrap_or_else(|p| p.into_inner());
        if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        *slot = Some(sample);
        drop(slot);
        self.ready.notify_one();
    }

    /// Drop whatever is waiting and every later post; the thread ends.
    pub fn close(&self) {
        let mut slot = self.slot.lock().unwrap_or_else(|p| p.into_inner());
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        *slot = None;
        drop(slot);
        self.ready.notify_one();
    }
}

#[cfg(all(test, unix))]
#[path = "input_mode_tests.rs"]
mod tests;
