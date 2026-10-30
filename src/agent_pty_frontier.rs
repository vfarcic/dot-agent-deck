//! PRD #1666 part 3 (Q35-20a; amendment 4 §7.2, amendment 5 §3.2,
//! amendment 6 §3): the Linux PTY reader, which also takes drain frontiers.
//!
//! The reader polls `[master POLLIN, wake POLLIN]` on a duplicate of the
//! master's descriptor and reads through the same reader as before, so a
//! read behaves exactly as the blocking loop's did: `EIO` is the end of the
//! stream, a signal is retried, and the bytes go to [`AgentBus::push_timed`]
//! unchanged. With no frontier asked for, the only difference is a `poll`
//! before each `read`.
//!
//! **One iteration** (A5 §3.2): the master first, then the wake, then, while
//! the pending set isn't empty, the zero-poll step. The fail-closed table
//! (A4 §7.2), row by row:
//!
//! | Event | Action |
//! | --- | --- |
//! | `poll` → `EINTR` | retry; no frontier decision |
//! | `poll` → any other error | one blocking `read`, as before; the pending set stays |
//! | master `POLLIN`, or `POLLHUP`/`POLLERR` | `read` (`0`/`EIO` ends the stream), then push |
//! | master `POLLNVAL` | the stream ends with an error |
//! | wake `POLLIN` | drain it (`EAGAIN` counts as drained), snapshot the queue into the pending set |
//! | wake `POLLERR`/`POLLHUP`/`POLLNVAL`, or a drain error | retire the wake: every outstanding request `Missing`, later ones `Missing` at once, the wake left out of the poll |
//! | zero-poll step | `poll([master], 0)`: **only `rc == 0`** takes a frontier, `Taken{o}` for every pending request of this generation, published under the bus lock. Anything else, or an error, takes none and keeps the pending set for the step after the next read |
//! | a request for another generation | `Missing` |
//! | the reader's end | every outstanding request `Missing`, and every later one at once (here, and in `pump_reader` for every platform) |
//! | the fed count can't advance | `Missing`, sticky (the output hook's count) |
//! | after `rc == 0`, a process other than the daemon holds the master, or one that could can't be read (E-MF, [`master_holders`]) | `Missing` for the pending set |
//!
//! **Publication.** `o` is the output hook's fed count, read under the bus
//! lock in the same critical section that sets `Taken{o}`, and this thread
//! is the only production caller of `push_timed`, so no byte reaches the
//! hook between the zero poll and the publication. Records learn the
//! outcome after the bus lock is released ([`GenerationInput::settle_frontier`]),
//! and before this thread's next push.
//!
//! **Ready, then a blocking read** (A5 §3.2): if a concurrent discard
//! empties the master between `POLLIN` and the `read`, the read blocks as
//! the old loop's did, and pending requests wait behind it (late, which
//! holds). `O_NONBLOCK` isn't set: the reader shares its open file
//! description with the input writer.
//!
//! [`GenerationInput::settle_frontier`]: crate::composer_probe::epoch::GenerationInput::settle_frontier

use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;

use super::{AgentBus, PushCost, ReadEnd};
use crate::composer_probe::frontier::{EventFd, Request, Retired};
use crate::composer_probe::master_holders::{self, Holders, MasterId};
use crate::composer_probe::record::FrontierOutcome;

const POLLIN: i16 = libc::POLLIN;
const POLLHUP: i16 = libc::POLLHUP;
const POLLERR: i16 = libc::POLLERR;
const POLLNVAL: i16 = libc::POLLNVAL;

/// What the reader polls and reads.
pub(super) trait ReaderIo {
    /// `poll([master POLLIN, wake POLLIN], -1)`, the wake left out when
    /// `wake` is false: the master's and the wake's `revents`.
    fn wait(&mut self, wake: bool) -> io::Result<(i16, i16)>;
    /// `poll([master POLLIN], 0)`: the master's `revents`, `0` when the poll
    /// returned `0`.
    fn zero_poll(&mut self) -> io::Result<i16>;
    /// One read of the master.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>;
    /// Read the wake back to zero; nothing pending counts as drained.
    fn drain_wake(&mut self) -> io::Result<()>;
    /// Whether a process other than the daemon holds the master (E-MF).
    fn holders(&mut self) -> Holders;
}

/// The production [`ReaderIo`]: the reader portable-pty gave, a duplicate of
/// the master to poll, and the wake.
struct PtyIo {
    reader: Box<dyn Read + Send>,
    master: OwnedFd,
    wake: Arc<EventFd>,
    /// The master's identity and the tick before its `openpty`; `None` when
    /// either couldn't be read, and then no scan can establish S-M.
    owner: Option<(MasterId, u64)>,
}

fn poll(fds: &mut [libc::pollfd], timeout: libc::c_int) -> io::Result<libc::c_int> {
    // SAFETY: `fds` is a live, writable slice of `pollfd`s of that length.
    let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

impl ReaderIo for PtyIo {
    fn wait(&mut self, wake: bool) -> io::Result<(i16, i16)> {
        let mut fds = [
            libc::pollfd {
                fd: self.master.as_raw_fd(),
                events: POLLIN,
                revents: 0,
            },
            libc::pollfd {
                // A negative descriptor is skipped, with `revents` zero.
                fd: if wake { self.wake.as_raw_fd() } else { -1 },
                events: POLLIN,
                revents: 0,
            },
        ];
        poll(&mut fds, -1)?;
        Ok((fds[0].revents, fds[1].revents))
    }

    fn zero_poll(&mut self) -> io::Result<i16> {
        let mut fds = [libc::pollfd {
            fd: self.master.as_raw_fd(),
            events: POLLIN,
            revents: 0,
        }];
        match poll(&mut fds, 0)? {
            0 => Ok(0),
            _ => Ok(fds[0].revents),
        }
    }

    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buf)
    }

    fn drain_wake(&mut self) -> io::Result<()> {
        self.wake.drain()
    }

    fn holders(&mut self) -> Holders {
        match &self.owner {
            Some((master, since)) => master_holders::holders(master, *since),
            None => Holders::Unknown {
                pid: 0,
                step: "identity",
                errno: None,
            },
        }
    }
}

/// A generation's frontier reader, set up before its thread starts.
pub(super) struct Frontier {
    master: OwnedFd,
    wake: Arc<EventFd>,
    owner: Option<(MasterId, u64)>,
    generation: u64,
    control: Control,
}

impl Frontier {
    /// Attach a wake to `bus`'s frontier queue and duplicate the master to
    /// poll. `None` when any step fails: the reader then reads as before,
    /// and every frontier request is `Missing`, since nothing attached.
    /// `opened` is the boot tick read before the master's `openpty`.
    pub(super) fn setup(
        bus: &AgentBus,
        agent_id: &str,
        master: Option<RawFd>,
        opened: Option<u64>,
        env: &[(String, String)],
    ) -> Option<Self> {
        let generation = agent_id.parse::<u64>().ok()?;
        let setup = || -> io::Result<(OwnedFd, EventFd)> {
            let master = master.ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;
            // SAFETY: fcntl(2) on a descriptor the caller's master owns; a
            // non-negative return is a new descriptor this reader owns.
            let dup = unsafe { libc::fcntl(master, libc::F_DUPFD_CLOEXEC, 0) };
            if dup < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `dup` was just returned and nothing else owns it.
            let dup = unsafe { OwnedFd::from_raw_fd(dup) };
            Ok((dup, EventFd::new()?))
        };
        match setup() {
            Ok((master, wake)) => {
                let wake = Arc::new(wake);
                let owner = master_holders::master_id(master.as_raw_fd())
                    .ok()
                    .zip(opened);
                if owner.is_none() {
                    tracing::warn!(
                        target: "composer_frontier",
                        agent_id,
                        "the master's identity or open time is unknown; the generation's frontiers will be missing"
                    );
                }
                bus.input().frontier().attach(generation, wake.clone());
                Some(Self {
                    master,
                    wake,
                    owner,
                    generation,
                    control: Control::from_env(env),
                })
            }
            Err(error) => {
                tracing::warn!(
                    target: "composer_frontier",
                    agent_id,
                    error = %error,
                    "frontier reader setup failed; the generation takes no frontier"
                );
                None
            }
        }
    }

    /// Read the master to its end, taking the frontiers asked for.
    pub(super) fn run(
        self,
        reader: Box<dyn Read + Send>,
        bus: &AgentBus,
        pushed: impl FnMut(PushCost),
    ) -> ReadEnd {
        let Self {
            master,
            wake,
            owner,
            generation,
            mut control,
        } = self;
        let mut io = PtyIo {
            reader,
            master,
            wake,
            owner,
        };
        run(&mut io, bus, generation, &mut control, pushed)
    }
}

/// The `e2e` build's controls and the unit tests' seams. Empty otherwise.
#[derive(Default)]
pub(super) struct Control {
    /// `DOT_AGENT_DECK_E2E_FRONTIER_GATE`: pause before each zero poll.
    #[cfg(feature = "e2e")]
    pub(super) gate: Option<Gate>,
    /// `DOT_AGENT_DECK_E2E_FRONTIER_NO_ZERO_POLL=1`: the RED mutant, which
    /// skips the zero poll and takes the offset as it is.
    #[cfg(feature = "e2e")]
    pub(super) no_zero_poll: bool,
    /// Run between the zero poll's `rc == 0` and the publication (A6).
    #[cfg(test)]
    pub(super) after_zero: Option<Box<dyn FnMut() + Send>>,
    /// Run with what a publication set, before its records learn it.
    #[cfg(test)]
    pub(super) published: Option<Published>,
}

/// [`Control::published`]'s seam.
#[cfg(test)]
pub(super) type Published = Box<dyn FnMut(&[Arc<Request>]) + Send>;

impl Control {
    #[cfg_attr(not(feature = "e2e"), allow(unused_variables))]
    fn from_env(env: &[(String, String)]) -> Self {
        #[cfg(feature = "e2e")]
        {
            let value = |name: &str| {
                env.iter()
                    .rev()
                    .find(|(k, _)| k == name)
                    .map(|(_, v)| v.clone())
            };
            let no_zero_poll =
                value("DOT_AGENT_DECK_E2E_FRONTIER_NO_ZERO_POLL").as_deref() == Some("1");
            if no_zero_poll {
                tracing::warn!(
                    target: "composer_frontier",
                    "e2e mutant: this reader takes frontiers without the zero poll"
                );
            }
            Self {
                gate: value("DOT_AGENT_DECK_E2E_FRONTIER_GATE").map(|dir| Gate {
                    dir: dir.into(),
                    attempt: 0,
                }),
                no_zero_poll,
                #[cfg(test)]
                after_zero: None,
                #[cfg(test)]
                published: None,
            }
        }
        #[cfg(not(feature = "e2e"))]
        Self::default()
    }

    fn before_zero_poll(&mut self) {
        #[cfg(feature = "e2e")]
        if let Some(gate) = self.gate.as_mut() {
            gate.pause();
        }
    }

    fn skips_zero_poll(&self) -> bool {
        #[cfg(feature = "e2e")]
        return self.no_zero_poll;
        #[cfg(not(feature = "e2e"))]
        false
    }

    fn after_zero(&mut self) {
        #[cfg(test)]
        if let Some(seam) = self.after_zero.as_mut() {
            seam();
        }
    }

    #[cfg_attr(not(test), allow(unused_variables))]
    fn published(&mut self, settled: &[Arc<Request>]) {
        #[cfg(test)]
        if let Some(seam) = self.published.as_mut() {
            seam(settled);
        }
    }
}

/// The `e2e` gate: before each zero poll a request reaches, write the
/// attempt's number to `<dir>/arrived` and wait until `<dir>/release` holds a
/// number at least as large.
#[cfg(feature = "e2e")]
pub(super) struct Gate {
    dir: std::path::PathBuf,
    attempt: u64,
}

#[cfg(feature = "e2e")]
impl Gate {
    /// How long a pause waits for its release before going on regardless,
    /// so a test that died can't park a sandbox daemon's reader for good.
    const LIMIT: std::time::Duration = std::time::Duration::from_secs(120);

    fn pause(&mut self) {
        self.attempt += 1;
        let temporary = self.dir.join("arrived.tmp");
        if std::fs::write(&temporary, self.attempt.to_string()).is_err()
            || std::fs::rename(&temporary, self.dir.join("arrived")).is_err()
        {
            return;
        }
        let start = std::time::Instant::now();
        while start.elapsed() < Self::LIMIT {
            let released = std::fs::read_to_string(self.dir.join("release"))
                .ok()
                .and_then(|value| value.trim().parse::<u64>().ok());
            if released.is_some_and(|released| released >= self.attempt) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
}

/// The reader's loop, to the end of the stream, after which every
/// outstanding request is `Missing` (the generation is retiring). See the
/// module documentation.
pub(super) fn run(
    io: &mut impl ReaderIo,
    bus: &AgentBus,
    generation: u64,
    control: &mut Control,
    pushed: impl FnMut(PushCost),
) -> ReadEnd {
    let end = read_to_end(io, bus, generation, control, pushed);
    let input = bus.input();
    input.settle_frontier(&input.frontier().retire(Retired::Reader));
    end
}

fn read_to_end(
    io: &mut impl ReaderIo,
    bus: &AgentBus,
    generation: u64,
    control: &mut Control,
    mut pushed: impl FnMut(PushCost),
) -> ReadEnd {
    let input = Arc::clone(bus.input());
    let queue = input.frontier();
    let mut pending: Vec<Arc<Request>> = Vec::new();
    let mut wake = true;
    let mut buf = [0u8; 8192];
    loop {
        let (master, rung) = match io.wait(wake) {
            Ok(revents) => revents,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                tracing::debug!(
                    target: "composer_frontier",
                    generation,
                    error = %error,
                    "reader poll failed; one blocking read"
                );
                if let Some(end) = read_once(io, bus, &mut buf, &mut pushed) {
                    return end;
                }
                zero_step(io, bus, generation, &mut pending, control);
                continue;
            }
        };
        if master & POLLNVAL != 0 {
            return ReadEnd::Error(io::Error::from_raw_os_error(libc::EBADF).kind());
        }
        if master & (POLLIN | POLLHUP | POLLERR) != 0
            && let Some(end) = read_once(io, bus, &mut buf, &mut pushed)
        {
            return end;
        }
        if wake && rung != 0 {
            let drained = if rung & (POLLERR | POLLHUP | POLLNVAL) != 0 {
                Err(io::Error::other(format!("wake revents {rung:#x}")))
            } else {
                io.drain_wake()
            };
            match drained {
                Ok(()) => pending.extend(queue.take_queued()),
                Err(error) => {
                    tracing::warn!(
                        target: "composer_frontier",
                        generation,
                        error = %error,
                        "frontier wake retired; the generation's frontier requests are missing"
                    );
                    wake = false;
                    pending.clear();
                    input.settle_frontier(&queue.retire(Retired::Wake));
                }
            }
        }
        if !pending.is_empty() {
            zero_step(io, bus, generation, &mut pending, control);
        }
    }
}

/// One read, pushed. `Some` when the stream ended.
fn read_once(
    io: &mut impl ReaderIo,
    bus: &AgentBus,
    buf: &mut [u8],
    pushed: &mut impl FnMut(PushCost),
) -> Option<ReadEnd> {
    match io.read(buf) {
        Ok(0) => Some(ReadEnd::Eof),
        Ok(n) => {
            pushed(bus.push_timed(buf[..n].to_vec()));
            None
        }
        // A read a signal interrupted is retried, not taken for the end of
        // the stream (round 5-wake, unchanged).
        Err(error) if error.kind() == io::ErrorKind::Interrupted => None,
        Err(error) => Some(ReadEnd::Error(error.kind())),
    }
}

/// The zero-poll step: take a frontier for the pending set only when the
/// master has nothing left to read.
fn zero_step(
    io: &mut impl ReaderIo,
    bus: &AgentBus,
    generation: u64,
    pending: &mut Vec<Arc<Request>>,
    control: &mut Control,
) {
    let input = bus.input();
    // A retirement may have settled some already: they're done.
    pending.retain(|r| r.outcome().is_none());
    let stale: Vec<Arc<Request>> = pending
        .iter()
        .filter(|r| r.generation() != generation && r.settle(FrontierOutcome::Missing))
        .cloned()
        .collect();
    pending.retain(|r| r.generation() == generation);
    input.settle_frontier(&stale);
    if pending.is_empty() {
        return;
    }
    control.before_zero_poll();
    let revents = if control.skips_zero_poll() {
        0
    } else {
        loop {
            match io.zero_poll() {
                Ok(revents) => break revents,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                // Takes none; the next read retries.
                Err(_) => return,
            }
        }
    };
    if revents != 0 {
        return;
    }
    control.after_zero();
    // E-MF: S-M checked, not assumed. A holder outside the daemon, or one
    // that could be and can't be read, and P-DRN doesn't apply: missing.
    let holders = io.holders();
    if holders != Holders::Sole {
        tracing::warn!(
            target: "composer_frontier",
            generation,
            holders = ?holders,
            "another process may hold the master; the frontier is missing"
        );
        let refused: Vec<Arc<Request>> = pending
            .drain(..)
            .filter(|r| r.settle(FrontierOutcome::Missing))
            .collect();
        input.settle_frontier(&refused);
        return;
    }
    let (settled, exhausted) = bus.publish_frontier(pending);
    pending.clear();
    if exhausted {
        tracing::warn!(
            target: "composer_frontier",
            generation,
            "output count exhausted; the generation's frontiers are missing"
        );
    }
    control.published(&settled);
    input.settle_frontier(&settled);
}
