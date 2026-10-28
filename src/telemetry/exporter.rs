//! The daemon-facing half: [`Telemetry`] owns the run tracker, the task that
//! feeds it in order, and a background task that batches finished spans and
//! posts them over OTLP/HTTP.
//!
//! With telemetry off, [`Telemetry::start_in`] returns a handle that does nothing:
//! no task, no client, no environment for agents. With a `grpc` (or unknown)
//! protocol it warns once and only propagates. Export is asynchronous and
//! batched; a failure is logged at most once a minute, by status code or error
//! kind, never with a header or the endpoint's credentials, and never reaches
//! an agent, a pane or the daemon.
//!
//! **One order for everything a run sees.** The daemon's broadcast is read by
//! an async task, while agents start on the spawn path and end on their PTY
//! reader threads. So that a status the agent reported is never lost to that
//! race, the lifecycle reaches the tracker the same way the events do:
//!
//! - a run is *prepared* in the tracker before its agent starts, so an event
//!   the agent causes always finds it ([`super::tracker::RunTracker::prepare_run`]);
//! - committing, ending and human input are commands on one queue, read by the
//!   same task that reads the broadcast, and the task applies every event
//!   already broadcast before it applies a command — so an agent's last status,
//!   broadcast before its PTY closed, is applied before its run ends;
//! - human input waits with the run it was typed into, not only on the queue:
//!   the command is a reminder to count it, and a run ended first — by its
//!   pane's close broadcast, say — counts it as it ends;
//! - human input is counted in the order telemetry was told about it against
//!   the events: each write notes how far the daemon's broadcast had got when
//!   telemetry is told about it, just after the pane accepted it
//!   ([`BroadcastClock`]), and the task counts it before the first event
//!   broadcast after that once the tracker has opened its run, so it is read
//!   against the prompt the cards showed then rather than when the task takes
//!   it. A lone `ESC` takes part too: an event on its pane, or naming no
//!   pane, broadcast after it waits out its window, and what the `ESC`
//!   turns out to be — the Escape key, or the key whose sequence it begins
//!   when the next write completes that sequence — keeps the `ESC`'s place,
//!   so it counts before that event;
//! - a delegated task's post (issue #1981 M7) keeps its place the same way:
//!   it notes the broadcast's position as telemetry is told about it, waits
//!   in [`Shared::posts`], and is applied before the first message broadcast
//!   after that, so a pane closed after the post does not end the
//!   orchestrator's run before the post is applied. While that run is
//!   prepared and not opened yet, the post is held with the run's own held
//!   events instead, in the same place, and opened in it as the run opens
//!   ([`RunTracker::place_post`]);
//! - a run whose spawn is refused is forgotten when its [`PreparedRun`] is
//!   dropped uncommitted.
//!
//! **Delivery.** A failed send is retried with exponential backoff and jitter
//! when the failure is transient (a connection error, HTTP 429, 502, 503 or
//! 504), honouring `Retry-After`, until the export timeout or, at shutdown, the
//! remaining flush budget runs out. Redirects are not followed: a 3xx answer is
//! a failed export, so a header meant for the configured endpoint never goes
//! anywhere else. A success that reports spans the backend did not accept (the
//! OTLP `partial_success`) is logged like a failure and not retried. A log
//! line about an export carries its counts, the HTTP status and the endpoint's
//! scheme, host and port, never text the backend wrote or the error's own.
//!
//! **Shutdown.** An export in flight when shutdown begins gives way: its batch
//! goes back into the buffer, and the final flush sends it with the runs the
//! shutdown ends, within the shutdown's budget rather than after the export
//! timeout. The spans the shutdown ends wait for room in the export queue
//! within that budget instead of being dropped when it is full. The budget
//! also ends early enough for the daemon to exit inside the window its stop
//! client waits for: see [`Telemetry::shutdown`].

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::time::Instant;

use crate::event::BroadcastMsg;

use super::accounting::locate::TrustedDir;
use super::accounting::run_usage::{UsageDirs, UsageFile};
use super::accounting::usage_file::USAGE_FILE_ENV;
use super::accounting::{UsageSource, locate, usage_source};
use super::config::{EnvMap, TelemetryConfig, redacted_endpoint};
use super::input::{InputFilter, InterventionKind, LONE_ESCAPE_WINDOW, PASTE_IDLE_WINDOW};
use super::launch::LAUNCH_ENV_KEYS;
use super::propagation::{RunIdentity, agent_env};
use super::resource::{ResourceAttribute, deck_resource, host_name};
use super::run_context::{ProbeSettings, Probes};
use super::span::{SpanRecord, new_run_id};
use super::tracker::{EndReason, FinishingRun, RunGeneration, RunStart, RunTracker};
use super::{Protocol, now_unix_nanos, otlp};

/// Spans per request, at most.
pub const MAX_BATCH: usize = 512;
/// How long a finished span waits for company before it is sent anyway.
pub const BATCH_DELAY: Duration = Duration::from_secs(5);
/// Finished spans waiting for the exporter, at most. Past it new spans are
/// dropped and counted, so a backend that is down cannot grow the daemon.
pub const QUEUE_CAPACITY: usize = 4096;
/// Human input waiting for the tracker, per run, at most. Past it the input is
/// dropped: the run's intervention count is all it feeds.
pub const MAX_QUEUED_INPUT: usize = 1024;
/// Export failures are logged at most this often.
pub const ERROR_LOG_INTERVAL: Duration = Duration::from_secs(60);
/// The first wait before a retry; each further one doubles, up to
/// [`RETRY_MAX_BACKOFF`], and is jittered down by up to half.
pub const RETRY_INITIAL_BACKOFF: Duration = Duration::from_secs(1);
pub const RETRY_MAX_BACKOFF: Duration = Duration::from_secs(5);
/// How much of the shutdown budget cancelling the run-context probes may use.
const PROBE_SHUTDOWN_BUDGET: Duration = Duration::from_millis(500);
/// How long a stop client waits for the daemon to exit after asking it to
/// stop: `daemon stop`'s `STOP_GRACE_TIMEOUT` (`src/daemon_stop.rs:52`) and the
/// build-mismatch prompt's, whichever is tighter. Past it plain `daemon stop`
/// reports a timeout and `daemon stop --force` kills the daemon mid-teardown.
const STOP_CLIENT_WINDOW: Duration = crate::agent_pty::DAEMON_STOP_POLL_BUDGET;
/// What the final flush leaves of `STOP_CLIENT_WINDOW` for the rest of the
/// daemon's teardown after it: closing its sockets, stopping what is left and
/// exiting, while the client samples only every 100 ms.
pub const SHUTDOWN_HEADROOM: Duration = Duration::from_millis(500);
/// Bytes of a successful answer read, at most, for its partial-success report.
pub const MAX_RESPONSE_BODY: usize = 64 * 1024;
/// How often each open run's usage record is read (PRD #1858), as
/// `codex_rollout_tail` reads a rollout. A run's end reads the rest.
pub const USAGE_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// The directory, under the per-run configuration directory, that holds the
/// usage files the deck's Pi extension and OpenCode plugin append to.
pub(crate) const USAGE_DIR: &str = "usage";
/// How many tokens one attempt to take a usage lease tries
/// ([`take_usage_lease`]).
const LEASE_TOKENS: usize = 4;
/// How long after an attempt to take a usage lease failed a later run's
/// start tries again ([`LeaseSlot`]).
const LEASE_RETRY_AFTER: Duration = Duration::from_secs(10);
/// How old a lease file still being made is before a starting daemon's
/// cleanup removes it ([`remove_leftover_usage_files`]).
const ABANDONED_LEASE_AGE: Duration = Duration::from_secs(60 * 60);

/// The deck's usage directory, [`USAGE_DIR`] in `config_dir`, made if it is
/// not there and opened as a [`TrustedDir`] when it is a directory of this
/// user's — not a symlink — that no other user can enter: one with group or
/// other permissions is narrowed to the owner first. `config_dir` itself is
/// resolved once, as any trusted root is. `None` otherwise, and then no run
/// gets a usage file; elsewhere than Unix no run gets one either.
fn private_dir(config_dir: &Path) -> Option<TrustedDir> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(config_dir)
            .ok()?;
        let root = TrustedDir::open(config_dir)?;
        root.make_dir(std::ffi::OsStr::new(USAGE_DIR), 0o700).ok()?;
        let dir = root.subdir(Path::new(USAGE_DIR))?;
        if dir.metadata().ok()?.permissions().mode() & 0o077 != 0 {
            dir.set_mode(0o700).ok()?;
            if dir.metadata().ok()?.permissions().mode() & 0o077 != 0 {
                return None;
            }
        }
        Some(dir)
    }
    #[cfg(not(unix))]
    {
        let _ = config_dir;
        None
    }
}

/// Create run `run_id`'s usage file in `dir`, named for the daemon's lease
/// `lease`: a new regular file only this user can read or write, so a name
/// already there — a file, a symlink or a FIFO — is refused rather than
/// reused. `None` when it cannot be made.
fn new_usage_file(dir: TrustedDir, run_id: &str, lease: &str) -> Option<UsageFile> {
    let name = locate::usage_file_name(run_id, lease)?;
    #[cfg(unix)]
    {
        let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW;
        drop(dir.open_raw(Path::new(&name), flags, 0o600).ok()?);
    }
    Some(UsageFile { dir, name })
}

/// This daemon's lease on its usage files (PRD #1858): a file in the usage
/// directory, named by a token of its own, on which the daemon holds an
/// exclusive `flock` for as long as it runs — the lock goes when its
/// descriptor closes, a crash included. Its usage files carry the token
/// ([`locate::usage_file_name`]), so a daemon that starts later removes them
/// only once it can take the lock itself ([`remove_leftover_usage_files`]),
/// which no process id decides.
struct UsageLease {
    token: String,
    /// Holds the lock.
    #[cfg(unix)]
    _held: std::os::fd::OwnedFd,
}

/// A lease token: 16 bytes from the operating system's random source, as
/// hexadecimal. `None` when that source fails: there is no fallback, as a
/// token from a clock or a counter could be another daemon's.
fn lease_token() -> Option<String> {
    let mut token = [0u8; 16];
    getrandom::fill(&mut token).ok()?;
    Some(super::span::hex(&token))
}

/// Take a new lease in `dir`. Its file is made under the name of a lease
/// being made ([`locate::lease_being_made`]), which cleanup does not take as
/// a lease, locked, then renamed to the lease's own name only if no entry
/// has that name ([`TrustedDir::rename_new`]), so a lease another daemon
/// finds under its own name is already held and a held one is never
/// replaced. A token whose file cannot be made, locked or renamed is given
/// up for another, at most [`LEASE_TOKENS`] of them. `None` when none could
/// be, or the random source fails, and then no run gets a usage file.
#[cfg(unix)]
fn take_usage_lease(dir: &TrustedDir) -> Option<UsageLease> {
    take_usage_lease_with(dir, lease_token, |_| {})
}

/// [`take_usage_lease`], with its tokens from `token` and `made` called with
/// each token between making its file and locking it: a test's seam.
#[cfg(unix)]
fn take_usage_lease_with(
    dir: &TrustedDir,
    mut token: impl FnMut() -> Option<String>,
    mut made: impl FnMut(&str),
) -> Option<UsageLease> {
    use std::os::fd::AsRawFd as _;
    let flags = libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    for _ in 0..LEASE_TOKENS {
        let token = token()?;
        let making = locate::lease_file_name(&token, false);
        let lease = locate::lease_file_name(&token, true);
        // A name that is there already is not this daemon's to remove.
        let Ok(held) = dir.open_raw(Path::new(&making), flags, 0o600) else {
            continue;
        };
        made(&token);
        // SAFETY: `held` is an open descriptor.
        let locked = unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
        if locked && dir.rename_new(making.as_ref(), lease.as_ref()).is_ok() {
            return Some(UsageLease { token, _held: held });
        }
        dir.remove(making.as_ref());
    }
    None
}

#[cfg(not(unix))]
fn take_usage_lease(_dir: &TrustedDir) -> Option<UsageLease> {
    None
}

/// This daemon's usage lease, taken with its first usage file and then held
/// for as long as the daemon runs. An attempt that failed is not final: a
/// later run's start tries again, no sooner than [`LEASE_RETRY_AFTER`] after
/// it, so a passing failure does not leave the daemon without usage files.
#[derive(Default)]
struct LeaseSlot(Mutex<LeaseState>);

#[derive(Default)]
enum LeaseState {
    #[default]
    Untried,
    Failed(std::time::Instant),
    Held(UsageLease),
}

impl LeaseSlot {
    /// The lease's token, `take` called for a lease when there is none: the
    /// first time, and again once [`LEASE_RETRY_AFTER`] has passed since it
    /// last failed. `None` while there is no lease.
    fn token(
        &self,
        now: std::time::Instant,
        take: impl FnOnce() -> Option<UsageLease>,
    ) -> Option<String> {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        match &*state {
            LeaseState::Held(lease) => return Some(lease.token.clone()),
            LeaseState::Failed(at) if now.saturating_duration_since(*at) < LEASE_RETRY_AFTER => {
                return None;
            }
            LeaseState::Untried | LeaseState::Failed(_) => {}
        }
        *state = take().map_or(LeaseState::Failed(now), LeaseState::Held);
        match &*state {
            LeaseState::Held(lease) => Some(lease.token.clone()),
            LeaseState::Untried | LeaseState::Failed(_) => None,
        }
    }

    /// The lease held, if any.
    #[cfg(test)]
    fn with_held<T>(&self, f: impl FnOnce(&UsageLease) -> T) -> Option<T> {
        match &*self.0.lock().unwrap_or_else(|p| p.into_inner()) {
            LeaseState::Held(lease) => Some(f(lease)),
            LeaseState::Untried | LeaseState::Failed(_) => None,
        }
    }
}

/// Remove the usage files in the deck's usage directory under `config_dir`
/// left by a daemon that ended without removing them (PRD #1858): called as
/// a daemon starts, before it has a lease of its own. A file names the lease
/// of the daemon that made it ([`UsageLease`]). For each lease file there,
/// the lease's files and the lease file itself are removed only when its
/// lock can be taken at once, so the daemon that held it has ended; a lease
/// another daemon holds — on this machine, whatever process-id namespace it
/// runs in — keeps its files. A usage file whose lease file is not there is
/// kept. A lease file still being made ([`locate::lease_being_made`]) is not
/// taken as authority to remove usage files: only that file itself is
/// removed, once it is [`ABANDONED_LEASE_AGE`] old and its lock can be taken
/// at once. A daemon makes, locks and renames one within a single call
/// ([`take_usage_lease`]), so one that old and unlocked is eligible to go; it
/// does not prove its maker ended, which may be paused between making and
/// locking it, and a maker that then finds it gone takes another token. Where
/// the rename falls back to a link, a removal of the old name that fails
/// leaves both names on the held lease, and its usage files carry the token
/// they share; the held lock keeps both. Every open
/// and removal is made beneath the usage directory's descriptor, with no
/// symlink followed. Elsewhere than Unix no run has a usage file, and nothing
/// is removed.
pub(crate) fn remove_leftover_usage_files(config_dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd as _;
        let Some(dir) =
            TrustedDir::open(config_dir).and_then(|root| root.subdir(Path::new(USAGE_DIR)))
        else {
            return;
        };
        let names = dir.entry_names(usize::MAX);
        for lease_file in &names {
            let name = lease_file.as_encoded_bytes();
            // `None` for a lease file still being made.
            let lease = locate::lease_of_file(name);
            if lease.is_none() && locate::lease_being_made(name).is_none() {
                continue;
            }
            let flags = libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC;
            let Ok(held) = dir.open_raw(Path::new(lease_file), flags, 0) else {
                continue;
            };
            let Ok(meta) = held
                .try_clone()
                .map(std::fs::File::from)
                .and_then(|file| file.metadata())
            else {
                continue;
            };
            let abandoned = || {
                meta.modified()
                    .ok()
                    .and_then(|at| at.elapsed().ok())
                    .is_some_and(|age| age >= ABANDONED_LEASE_AGE)
            };
            if !meta.is_file() || (lease.is_none() && !abandoned()) {
                continue;
            }
            // SAFETY: `held` is an open descriptor.
            if unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                continue;
            }
            for name in &names {
                if lease.is_some()
                    && locate::usage_file_lease(name.as_encoded_bytes()) == lease
                    && dir.has_file(Path::new(name))
                {
                    dir.remove(name);
                }
            }
            dir.remove(lease_file);
        }
    }
    #[cfg(not(unix))]
    let _ = config_dir;
}

enum ExportMsg {
    Span(Box<SpanRecord>),
    /// Send everything held, by `deadline` at the latest, then acknowledge.
    Flush {
        ack: oneshot::Sender<()>,
        deadline: Instant,
    },
}

/// What the ingest task applies, in the order it was sent — see the module
/// docs.
enum Command {
    Subscribe(broadcast::Receiver<BroadcastMsg>),
    Commit {
        run_id: String,
    },
    /// Stamped when it is applied, after the events broadcast before it, so
    /// none of them lands after the run's end.
    End {
        pane_id: String,
        agent_id: Option<String>,
        /// The run whose input filter [`Telemetry::end_run`] marked ended.
        run_id: Option<String>,
        reason: EndReason,
        exit_code: Option<i32>,
    },
    /// `run_id` on `pane_id` has human input waiting ([`PaneInput::pending`]).
    HumanInput {
        pane_id: String,
        run_id: String,
    },
    /// Issue #1981 M7: the delegate path posted task `seq`, which waits in
    /// [`Shared::posts`] with its broadcast position: a reminder to apply
    /// it, and none posted after it, between the broadcast messages it was
    /// posted between, if no message after it does first
    /// ([`super::handoff`]).
    DelegatePosted {
        seq: u64,
    },
    /// Acknowledged once everything sent before it, broadcast included, is
    /// applied.
    #[cfg(test)]
    Barrier(oneshot::Sender<()>),
    /// End every run, stamped like [`Command::End`], and hand the spans that
    /// finishes back to [`Telemetry::shutdown`] to queue.
    Shutdown {
        ack: oneshot::Sender<Vec<SpanRecord>>,
    },
}

/// A test seam pausing a task at one point: signal the first half, then await
/// the second, which suspends the task without blocking the thread it runs on.
#[cfg(test)]
type Gate = (oneshot::Sender<()>, oneshot::Receiver<()>);

/// A run's input filter, from its commit until the tracker ends the run,
/// with the generation it is for.
struct PaneInput {
    run_id: String,
    agent_id: Option<String>,
    /// [`Telemetry::end_run`] was called for it: later input is not its.
    ended: bool,
    filter: InputFilter,
    /// When the lone `ESC` the filter holds arrived — monotonic, for the
    /// window, and wall-clock and on the broadcast, for the intervention it
    /// may become.
    escape: Option<(std::time::Instant, u64, u64)>,
    /// When the last write that left the filter inside a bracketed paste
    /// arrived, monotonic: the paste is read as ended once
    /// [`PASTE_IDLE_WINDOW`] passes with no write after it.
    paste: Option<std::time::Instant>,
    /// The human input the filter accepted and the tracker has not counted
    /// yet, oldest first. Taken, once, by whichever comes first: the first
    /// event broadcast after it, the [`Command::HumanInput`] sent with the
    /// first of them, or the run ending.
    pending: Vec<Written>,
}

/// Human input a run's filter accepted.
#[derive(Clone, Copy, Debug)]
struct Written {
    /// When, on the wall clock.
    at: u64,
    kind: InterventionKind,
    /// How many messages the daemon had broadcast when telemetry was told
    /// about it, just after the pane accepted it
    /// ([`BroadcastClock::position`]): it is counted before every message
    /// from that one on.
    position: u64,
}

impl PaneInput {
    /// Feed one accepted write, `now` on the wall clock and `at` on the
    /// monotonic one, that telemetry was told about at broadcast `position`:
    /// the human input it settles, oldest first, and whether it left a lone
    /// `ESC` held. Input a held lone `ESC` turns into — the Escape key, once
    /// its window has passed or another `ESC` follows it, or the key whose
    /// sequence it begins, with every byte of that key the write holds —
    /// keeps the time and position that `ESC` was written at, as one input;
    /// what follows the key has `now` and `position`. A sequence the write
    /// leaves undecided counts once, when a later write decides it, at that
    /// write's time and position. The rest of a key already decided does not
    /// count again in the write that completes it, except a bracketed paste's,
    /// which is paste written then.
    fn write(
        &mut self,
        bytes: &[u8],
        now: u64,
        position: u64,
        at: std::time::Instant,
    ) -> (Vec<Written>, bool) {
        let mut human = Vec::with_capacity(2);
        let mut rest = bytes;
        if let Some((since, written, escaped)) = self.escape {
            let kind = if at.saturating_duration_since(since) >= LONE_ESCAPE_WINDOW {
                // Its window has passed: the Escape key, whatever follows.
                self.escape = None;
                self.filter
                    .settle_lone_escape()
                    .then_some(InterventionKind::Interrupt)
            } else {
                let (kind, taken) = self.filter.decide_held(bytes);
                rest = &bytes[taken..];
                kind
            };
            if let Some(kind) = kind {
                human.push(Written {
                    at: written,
                    kind,
                    position: escaped,
                });
            }
        }
        // A paste with no input for its window lost its end marker.
        if self
            .paste
            .is_some_and(|since| at.saturating_duration_since(since) >= PASTE_IDLE_WINDOW)
        {
            self.filter.end_paste();
        }
        if let Some(kind) = self.filter.classify(rest) {
            human.push(Written {
                at: now,
                kind,
                position,
            });
        }
        if !self.filter.in_paste() {
            self.paste = None;
        } else if !bytes.is_empty() {
            self.paste = Some(at);
        }
        // A non-empty write leaving a lone ESC held ended with it.
        let mut held = false;
        if !self.filter.holds_lone_escape() {
            self.escape = None;
        } else if !bytes.is_empty() {
            self.escape = Some((at, now, position));
            held = true;
        }
        (human, held)
    }

    /// Everything this run's input still owes the tracker, oldest first:
    /// what is pending, then a held lone `ESC` when `settle` says it is the
    /// Escape key now.
    fn take_held(&mut self, settle: bool) -> Vec<Written> {
        let mut held = std::mem::take(&mut self.pending);
        if settle
            && let Some((_, at, position)) = self.escape.take()
            && self.filter.settle_lone_escape()
        {
            held.push(Written {
                at,
                kind: InterventionKind::Interrupt,
                position,
            });
        }
        held
    }

    /// The pending input written before the daemon broadcast its message
    /// `position`, oldest first, and the position of the last that stays.
    fn take_through(&mut self, position: u64) -> (Vec<Written>, Option<u64>) {
        let due = self.pending.partition_point(|w| w.position <= position);
        let taken = self.pending.drain(..due).collect();
        (taken, self.pending.last().map(|w| w.position))
    }

    /// Whether input written before broadcast message `position` is still
    /// held: pending, or a lone `ESC`.
    fn owes_before(&self, position: u64) -> bool {
        self.pending.first().is_some_and(|w| w.position <= position)
            || self.escape.is_some_and(|(_, _, at)| at <= position)
    }

    /// [`Self::take_through`], then a lone `ESC` written before broadcast
    /// message `position` as the Escape key when its window has passed by
    /// `now`. Everything pending was written before a held `ESC`: a later
    /// write would have settled it.
    fn take_before(&mut self, position: u64, now: std::time::Instant) -> Vec<Written> {
        let (mut taken, _) = self.take_through(position);
        if self.escape.is_some_and(|(since, _, at)| {
            at <= position && now.saturating_duration_since(since) >= LONE_ESCAPE_WINDOW
        }) {
            taken.extend(self.take_held(true));
        }
        taken
    }
}

/// How far the daemon's broadcast has got: the number of messages it has
/// sent since the ingest task's receiver was made, which numbers each message
/// that receiver reads. Read as telemetry is told about human input, just
/// after the pane accepted it, so the ingest task can count that input
/// between the events broadcast before it and the ones after, on the order
/// the daemon published them in.
///
/// It is a receiver of its own that reads nothing until it falls
/// [`CLOCK_CATCH_UP_AT`] behind: the broadcast does not number its messages,
/// but a receiver knows how many it has not read.
#[derive(Default)]
struct BroadcastClock {
    rx: Option<broadcast::Receiver<BroadcastMsg>>,
    /// Messages sent before `rx`'s next one.
    base: u64,
}

/// Issue #1981 M7: delegated tasks posted and not applied yet
/// ([`Shared::posts`]).
#[derive(Default)]
struct WaitingPosts {
    /// The number the next post is given: posts are numbered in the order
    /// they join `queue`, which is broadcast order.
    next_seq: u64,
    /// Oldest first, each with its number and how many messages the daemon
    /// had broadcast when telemetry was told about it
    /// ([`BroadcastClock::position`]): applied before every message from
    /// that one on.
    queue: std::collections::VecDeque<WaitingPost>,
}

struct WaitingPost {
    seq: u64,
    position: u64,
    posted: super::handoff::Posted,
}

/// How far behind the broadcast [`BroadcastClock`] falls before it reads
/// what it is behind by. A trigger, not a bound on what it keeps alive: it
/// catches up only when the ingest task applies a message, so while that
/// task is stalled it can hold as many as the broadcast's ring keeps.
const CLOCK_CATCH_UP_AT: usize = 256;

impl BroadcastClock {
    /// A clock numbering messages from `events`' next one, which has read
    /// nothing yet.
    fn numbering(events: &broadcast::Receiver<BroadcastMsg>) -> Self {
        loop {
            let sent = events.len();
            let rx = events.resubscribe();
            // Nothing was sent in between, so `rx` starts where `sent` ends.
            if events.len() == sent {
                return Self {
                    rx: Some(rx),
                    base: sent as u64,
                };
            }
        }
    }

    fn position(&self) -> u64 {
        self.base + self.rx.as_ref().map_or(0, |rx| rx.len() as u64)
    }

    /// Read what the clock is behind by, once that is [`CLOCK_CATCH_UP_AT`]
    /// or more, so it does not keep the messages alive longer than it must.
    fn catch_up(&mut self) {
        let Some(rx) = &mut self.rx else {
            return;
        };
        let mut left = rx.len();
        if left < CLOCK_CATCH_UP_AT {
            return;
        }
        while left > 0 {
            let read = match rx.try_recv() {
                Ok(_) => 1,
                Err(broadcast::error::TryRecvError::Lagged(missed)) => missed,
                Err(_) => break,
            };
            self.base += read;
            left = left.saturating_sub(usize::try_from(read).unwrap_or(usize::MAX));
        }
    }
}

type Filters = Mutex<HashMap<String, Vec<PaneInput>>>;

/// The name of the dispatched unit an open run's generation belongs to, from
/// the deck's record of dispatched units, or `None` when it belongs to none.
/// See [`Telemetry::set_dispatch_lookup`].
pub type DispatchLookup = Box<dyn Fn(&RunGeneration) -> Option<String> + Send + Sync>;

fn lock_filters(filters: &Filters) -> std::sync::MutexGuard<'_, HashMap<String, Vec<PaneInput>>> {
    filters.lock().unwrap_or_else(|p| p.into_inner())
}

/// The run `run_id` on `pane_id` has ended: drop its input filter, and return
/// when the input it accepted and the tracker has not counted yet was written,
/// with a lone `ESC` it still held last — the Escape key, since nothing can
/// follow it now. Called by the tracker, with the tracker locked.
fn retire_input(filters: &Filters, pane_id: &str, run_id: &str) -> Vec<(u64, InterventionKind)> {
    let mut filters = lock_filters(filters);
    let Some(inputs) = filters.get_mut(pane_id) else {
        return Vec::new();
    };
    let Some(i) = inputs.iter().position(|input| input.run_id == run_id) else {
        return Vec::new();
    };
    let mut input = inputs.remove(i);
    if inputs.is_empty() {
        filters.remove(pane_id);
    }
    input
        .take_held(true)
        .into_iter()
        .map(|w| (w.at, w.kind))
        .collect()
}

/// What the spawn path, the reader threads and the ingest task share.
struct Shared {
    tracker: Mutex<RunTracker>,
    queue: mpsc::Sender<ExportMsg>,
    dropped: Mutex<RateLimitedLog>,
    /// Issue #1981 M8: every span the queue refused since the exporter
    /// started (`deck.telemetry.spans.dropped`). Unlike [`Self::dropped`],
    /// which counts only between warnings, never reset.
    dropped_total: std::sync::atomic::AtomicU64,
    /// Issue #1981 M8: how often a `daemon.health` span is sent, when it is
    /// ([`super::health`]), and the hook latencies of the interval open now.
    health_interval: Option<Duration>,
    health: Mutex<super::health::Window>,
    /// Each run's input filter, by pane, the newest generation last. The
    /// tracker retires one as it ends that run ([`retire_input`]), so a held
    /// lone `ESC` is counted however the run ends. Locked after the tracker,
    /// never before it.
    filters: Arc<Filters>,
    /// Where the broadcast stands, for input to note as it is written. Locked
    /// after the filters and the posts, never before them.
    clock: Mutex<BroadcastClock>,
    /// Issue #1981 M7: delegated tasks posted and not applied yet, oldest
    /// first. Locked after the tracker, never before it.
    posts: Mutex<WaitingPosts>,
    /// Wakes the ingest task to time a lone `ESC` that a write left held.
    escape_held: tokio::sync::Notify,
    /// Turned true as shutdown begins: an export in flight gives way to the
    /// final flush.
    stopping: watch::Sender<bool>,
    /// The runtime ended runs are completed on ([`Shared::complete_later`]).
    runtime: tokio::runtime::Handle,
    /// This, for a run completed on another thread to queue its span.
    me: std::sync::Weak<Shared>,
    /// Batches of ended runs, one per [`Shared::complete_later`] call with
    /// any, whose task has not ended yet: not runs.
    finalizing: watch::Sender<usize>,
    /// See [`Telemetry::set_dispatch_lookup`].
    dispatch_lookup: OnceLock<DispatchLookup>,
    /// Test seam: pauses the ingest task once inside its `escape_held`
    /// branch, after signalling the first half, until the second resolves.
    #[cfg(test)]
    timer_gate: Mutex<Option<Gate>>,
    /// Test seam: pauses the ingest task once as it is about to wait out a
    /// lone `ESC` before an event ([`Shared::observe_in_order`]).
    #[cfg(test)]
    wait_gate: Mutex<Option<Gate>>,
    /// Test seam: pauses the ingest task once the command queue is first
    /// found empty, before any branch is polled ([`pause_at_command_gate`]).
    #[cfg(test)]
    command_gate: Mutex<Option<Gate>>,
    /// The ingest task resumed from [`Self::command_gate`]: the queue was
    /// empty when it paused, so the other branches are polled before it is
    /// again.
    #[cfg(test)]
    command_gate_resumed: std::sync::atomic::AtomicBool,
}

struct Exporting {
    shared: Arc<Shared>,
    commands: mpsc::UnboundedSender<Command>,
    probes: Probes,
    ingest: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// When the daemon began a teardown a stop client is timing — see
    /// [`Telemetry::teardown_began`].
    teardown_began: OnceLock<std::time::Instant>,
}

/// See the module docs. Cheap to share: clone the `Arc` it is held in.
pub struct Telemetry {
    config: Option<TelemetryConfig>,
    /// The daemon's `OTEL_*` variables (and Claude Code's switch), the base an
    /// agent's environment is computed from.
    otel_env: EnvMap,
    /// The daemon's [`LAUNCH_ENV_KEYS`]: its `PATH`, which the shim directory
    /// goes in front of, and the user's own say on Pi's export.
    launch_env: EnvMap,
    host: Option<String>,
    exporting: Option<Exporting>,
    /// Where per-run agent configuration files are written.
    config_dir: Option<PathBuf>,
    /// The `PATH` shim's directory, first on every agent's `PATH` when set
    /// ([`super::shim`]).
    shim_dir: Option<PathBuf>,
    /// The committed runs whose per-run agent configuration files the shim
    /// may write, removed as each run's agent ends ([`super::launch`]).
    run_files: Mutex<Vec<RunFiles>>,
    /// This daemon's lease on its usage files, taken with the first one.
    usage_lease: LeaseSlot,
}

/// A committed run whose agent may have per-run configuration files.
struct RunFiles {
    pane_id: String,
    agent_id: Option<String>,
    run_id: String,
}

/// A run minted for a spawn that has not started its agent yet — see
/// [`Telemetry::prepare_run`]. Dropping it without [`Self::commit`] forgets the
/// run: its agent never started.
pub struct PreparedRun {
    /// The variables to add to the agent's environment.
    pub env: Vec<(String, String)>,
    telemetry: Arc<Telemetry>,
    run_id: String,
    pane_id: String,
    agent_id: Option<String>,
    cwd: Option<PathBuf>,
    committed: bool,
}

impl PreparedRun {
    /// The agent started: open its run.
    pub fn commit(mut self) {
        self.committed = true;
        if self.telemetry.shim_dir.is_some() {
            self.telemetry.lock_run_files().push(RunFiles {
                pane_id: self.pane_id.clone(),
                agent_id: self.agent_id.clone(),
                run_id: self.run_id.clone(),
            });
        }
        self.telemetry.commit(&self);
    }
}

impl Drop for PreparedRun {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Some(exporting) = &self.telemetry.exporting {
            exporting.shared.with_tracker(|t| t.abort_run(&self.run_id));
        }
    }
}

/// A unit being spawned: see [`Telemetry::begin_dispatch`]. Dropping it ends
/// the naming of runs by the unit's worktree.
pub struct DispatchNaming {
    telemetry: Arc<Telemetry>,
    name: String,
    token: u64,
}

impl DispatchNaming {
    /// The spawn started the generations `agents`, by pane and agent id, and,
    /// for an orchestration, the instance `orchestration_id`: name their runs
    /// that are not named yet — one whose agent started outside the worktree
    /// — and every role the instance starts later.
    pub fn spawned(self, agents: &[(String, String)], orchestration_id: Option<&str>) {
        if let Some(exporting) = &self.telemetry.exporting {
            exporting
                .shared
                .with_tracker(|t| t.name_dispatched(&self.name, agents, orchestration_id));
        }
    }
}

impl Drop for DispatchNaming {
    fn drop(&mut self) {
        if let Some(exporting) = &self.telemetry.exporting {
            exporting
                .shared
                .with_tracker(|t| t.end_dispatch(self.token));
        }
    }
}

/// Where the daemon writes per-run agent configuration files: under its state
/// directory.
pub fn default_config_dir() -> PathBuf {
    crate::config::state_dir().join("telemetry")
}

/// How [`Telemetry`] runs, apart from the environment. Tests change it.
#[derive(Default)]
struct Settings {
    probes: ProbeSettings,
    retry: RetryPolicy,
    /// Test seam: where [`Telemetry::start_with`] writes per-run agent
    /// configuration files; the daemon's state directory when unset.
    #[cfg(test)]
    config_dir: Option<PathBuf>,
    /// Test seam: hand finished spans here instead of to an HTTP exporter.
    #[cfg(test)]
    sink: Option<mpsc::Sender<ExportMsg>>,
    /// Test seam: armed before the ingest task first runs; see
    /// [`Shared::command_gate`].
    #[cfg(test)]
    command_gate: Option<Gate>,
    /// Issue #1981 M8: send a `daemon.health` span this often; none when
    /// unset. Only the daemon's start sets it ([`Telemetry::start_in`]).
    health_interval: Option<Duration>,
}

impl Telemetry {
    /// A handle that does nothing — what [`Self::start_in`] returns when off.
    pub fn disabled() -> Self {
        Telemetry {
            config: None,
            otel_env: EnvMap::new(),
            launch_env: EnvMap::new(),
            host: None,
            exporting: None,
            config_dir: None,
            shim_dir: None,
            run_files: Mutex::new(Vec::new()),
            usage_lease: LeaseSlot::default(),
        }
    }

    /// [`Self::start_in`] with the daemon's [`default_config_dir`].
    #[cfg(test)]
    pub fn start(env: &EnvMap) -> Self {
        Self::start_with(env, Settings::default())
    }

    /// [`Self::start_in`] with test `settings`, into their `config_dir`, else
    /// the daemon's [`default_config_dir`].
    #[cfg(test)]
    fn start_with(env: &EnvMap, mut settings: Settings) -> Self {
        let config_dir = settings
            .config_dir
            .take()
            .unwrap_or_else(default_config_dir);
        Self::start_with_dir(env, config_dir, settings)
    }

    /// Read the configuration from `env` (the daemon's environment at start)
    /// and, when it asks for an export the deck can do, spawn the exporter on
    /// the current tokio runtime, with per-run agent configuration files
    /// written into `config_dir`.
    pub fn start_in(env: &EnvMap, config_dir: PathBuf) -> Self {
        let settings = Settings {
            health_interval: Some(super::health::interval(env)),
            ..Settings::default()
        };
        Self::start_with_dir(env, config_dir, settings)
    }

    fn start_with_dir(env: &EnvMap, config_dir: PathBuf, settings: Settings) -> Self {
        let Some(config) = TelemetryConfig::from_env(env) else {
            return Self::disabled();
        };
        let otel_env: EnvMap = env
            .iter()
            .filter(|(k, _)| k.starts_with("OTEL_") || k.starts_with("CLAUDE_CODE_"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let launch_env: EnvMap = env
            .iter()
            .filter(|(k, _)| LAUNCH_ENV_KEYS.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let host = host_name();
        let exporting = if !config.protocol.deck_can_export() {
            tracing::warn!(
                protocol = config.protocol.as_env_value(),
                "telemetry: the deck exports traces over http/protobuf or http/json only; \
                 nothing will be exported by the deck, agents still receive the OTLP configuration"
            );
            None
        } else if tokio::runtime::Handle::try_current().is_err() {
            tracing::warn!(
                "telemetry: no async runtime to export from; agent-run traces will not be \
                 exported, agents still receive the OTLP configuration"
            );
            None
        } else {
            match start_exporting(&config, host.as_deref(), settings) {
                Ok(exporting) => {
                    tracing::info!(
                        endpoint = %redacted_endpoint(&config.endpoint),
                        protocol = config.protocol.as_env_value(),
                        "telemetry: exporting agent-run traces over OTLP"
                    );
                    Some(exporting)
                }
                Err(e) => {
                    tracing::warn!(
                        "telemetry: the OTLP client could not be built ({e}); agent-run traces \
                         will not be exported, agents still receive the OTLP configuration"
                    );
                    None
                }
            }
        };
        Telemetry {
            config: Some(config),
            otel_env,
            launch_env,
            host,
            exporting,
            config_dir: Some(config_dir),
            shim_dir: None,
            run_files: Mutex::new(Vec::new()),
            usage_lease: LeaseSlot::default(),
        }
    }

    /// Put `dir`, the `PATH` shim's directory ([`super::shim::install`]),
    /// first on the `PATH` of every agent from now on.
    pub fn set_shim_dir(&mut self, dir: PathBuf) {
        self.shim_dir = Some(dir);
    }

    /// Name each open run's dispatched unit with `lookup`, asked with the
    /// tracker let go, as each poll of the runs' usage comes round: for a run
    /// [`Self::begin_dispatch`] did not name from its start, such as a
    /// restarted single unit's. The first lookup set stays. Nothing happens
    /// when not exporting.
    pub fn set_dispatch_lookup(&self, lookup: DispatchLookup) {
        if let Some(exporting) = &self.exporting {
            let _ = exporting.shared.dispatch_lookup.set(lookup);
        }
    }

    /// A unit named `name` is about to be spawned in `worktree`: each run
    /// prepared in it until the returned [`DispatchNaming`] is dropped is
    /// named after the unit as it is prepared, so it has the name however
    /// soon its agent exits or its pane closes. Call
    /// [`DispatchNaming::spawned`] once the spawn says which generations it
    /// started. `None` when not exporting.
    pub fn begin_dispatch(self: &Arc<Self>, name: &str, worktree: &Path) -> Option<DispatchNaming> {
        let exporting = self.exporting.as_ref()?;
        let token = exporting
            .shared
            .tracker
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .begin_dispatch(name, worktree);
        Some(DispatchNaming {
            telemetry: Arc::clone(self),
            name: name.to_string(),
            token,
        })
    }

    /// Whether telemetry is configured at all (export or propagation).
    pub fn is_enabled(&self) -> bool {
        self.config.is_some()
    }

    /// Whether the deck exports its own spans.
    #[cfg(test)]
    pub fn is_exporting(&self) -> bool {
        self.exporting.is_some()
    }

    /// An agent is about to start: mint its run, prepare it in the tracker, and
    /// compute the variables to add to its environment. Call it before the
    /// agent process exists and [`PreparedRun::commit`] once it does; a spawn
    /// refused in between drops the [`PreparedRun`], which forgets the run.
    /// `spawn_env` is the spawn's own overlay and is applied over the daemon's
    /// environment captured by [`Self::start_in`]: a value set in either counts as
    /// the user's. `cwd` is used only to resolve the run's git context, off
    /// this thread. The run id goes in as [`super::shim::RUN_ID_ENV`], and with
    /// the shim installed its directory goes first on the agent's `PATH` — the
    /// spawn's own, else the daemon's. `None` when telemetry is off.
    pub fn prepare_run(
        self: &Arc<Self>,
        mut start: RunStart,
        spawn_env: &[(String, String)],
        cwd: Option<PathBuf>,
    ) -> Option<PreparedRun> {
        let config = self.config.as_ref()?;
        if start.run_id.is_empty() {
            start.run_id = new_run_id();
        }
        let mut base = self.otel_env.clone();
        base.extend(self.launch_env.iter().map(|(k, v)| (k.clone(), v.clone())));
        base.extend(spawn_env.iter().cloned());
        let mut env = agent_env(
            Some(config),
            &base,
            &RunIdentity {
                run_id: &start.run_id,
                pane_id: &start.pane_id,
                agent: &start.agent,
                orchestration: start.orchestration.as_ref(),
            },
            self.host.as_deref(),
        );
        env.extend(super::launch::extra_env(&start.agent, &base));
        let usage_dirs = self.usage_dirs(&start, spawn_env);
        // This run's own usage file, else none: over any value the spawn
        // inherited, which names another run's file (a run started from
        // inside another agent's pane, or by a daemon started in one).
        let usage_file = usage_dirs
            .usage_file
            .as_ref()
            .and_then(|f| f.dir.path().join(&f.name).to_str().map(str::to_string))
            .unwrap_or_default();
        env.push((USAGE_FILE_ENV.to_string(), usage_file));
        // Whether or not the shim is installed: without it, an id the daemon
        // inherited from another deck's agent would reach this one (#1902).
        env.push((super::shim::RUN_ID_ENV.to_string(), start.run_id.clone()));
        if let Some(shim) = self.shim_dir.as_deref().and_then(Path::to_str)
            && let Some(path) = base.get("PATH")
        {
            // An empty `PATH` is the current directory, and stays so after
            // the shim: `<shim>:` ends in an empty entry.
            env.push(("PATH".to_string(), format!("{shim}:{path}")));
        }
        let prepared = PreparedRun {
            env,
            telemetry: Arc::clone(self),
            run_id: start.run_id.clone(),
            pane_id: start.pane_id.clone(),
            agent_id: start.agent_id.clone(),
            cwd,
            committed: false,
        };
        if let Some(exporting) = &self.exporting {
            let run_id = start.run_id.clone();
            exporting.shared.with_tracker(|t| {
                t.prepare_run_in(start, now_unix_nanos(), prepared.cwd.as_deref());
                t.set_usage_dirs(&run_id, usage_dirs);
            });
        }
        Some(prepared)
    }

    /// Where `start`'s agent keeps its usage record (PRD #1858), from the
    /// spawn's own environment, else the daemon's. A Pi or OpenCode run gets
    /// a usage file of its own, under the per-run configuration directory,
    /// when the deck exports traces and the directory can be made; nothing
    /// else would read it.
    fn usage_dirs(&self, start: &RunStart, spawn_env: &[(String, String)]) -> UsageDirs {
        let var = |key: &str| {
            spawn_env
                .iter()
                .rev()
                .find(|(k, _)| k == key)
                .map(|(_, v)| OsString::from(v))
                .or_else(|| std::env::var_os(key))
        };
        let home = var("HOME");
        let mut dirs = UsageDirs::default();
        match usage_source(&start.agent) {
            UsageSource::ClaudeTranscript => {
                dirs.claude_config =
                    locate::claude_config_dir(var("CLAUDE_CONFIG_DIR").as_deref(), home.as_deref());
            }
            UsageSource::CodexRollout => {
                dirs.codex_home = locate::codex_home(var("CODEX_HOME").as_deref(), home.as_deref());
            }
            UsageSource::UsageFile if self.exporting.is_some() => {
                dirs.usage_file =
                    self.config_dir
                        .as_deref()
                        .and_then(private_dir)
                        .and_then(|dir| {
                            let lease = self
                                .usage_lease
                                .token(std::time::Instant::now(), || take_usage_lease(&dir))?;
                            new_usage_file(dir, &start.run_id, &lease)
                        });
            }
            UsageSource::UsageFile | UsageSource::NoChannel => {}
        }
        dirs
    }

    fn commit(&self, run: &PreparedRun) {
        let Some(exporting) = &self.exporting else {
            return;
        };
        {
            let mut filters = exporting.shared.lock_filters();
            filters
                .entry(run.pane_id.clone())
                .or_default()
                .push(PaneInput {
                    run_id: run.run_id.clone(),
                    agent_id: run.agent_id.clone(),
                    ended: false,
                    filter: InputFilter::new(),
                    escape: None,
                    paste: None,
                    pending: Vec::new(),
                });
            // Sent before the filters are unlocked, so no notice of input to
            // this run can reach the tracker ahead of the run itself.
            let _ = exporting.commands.send(Command::Commit {
                run_id: run.run_id.clone(),
            });
        }
        // The git probes run off the spawn path; the run's context
        // attributes appear on its span once they have resolved.
        if let Some(cwd) = run.cwd.clone() {
            let shared = Arc::clone(&exporting.shared);
            let run_id = run.run_id.clone();
            exporting.probes.spawn(cwd, move |context| {
                shared.with_tracker(|t| t.set_context(&run_id, context));
            });
        }
    }

    /// An attached client wrote `bytes` into `pane_id`. Counted as a human
    /// intervention, of the kind its bytes say, unless it is only terminal
    /// reports, including a report split across writes
    /// ([`super::input::InputFilter`]). A lone `ESC` — the
    /// Escape key — is counted once [`LONE_ESCAPE_WINDOW`] passes without the
    /// rest of a report, when another `ESC` follows it, or when the run ends;
    /// it counts at the time and broadcast position it was written at, not
    /// those of the write that decided it, and so does the key whose sequence
    /// it begins when the next write completes that sequence. Input the deck
    /// injects itself (a delegated task, an inbox
    /// delivery) must not be passed here.
    pub fn observe_client_input(&self, pane_id: &str, bytes: &[u8]) {
        self.observe_client_input_at(pane_id, bytes, now_unix_nanos(), std::time::Instant::now());
    }

    /// [`Self::observe_client_input`] at a given time: `now` on the wall clock,
    /// `at` on the monotonic one.
    fn observe_client_input_at(
        &self,
        pane_id: &str,
        bytes: &[u8],
        now: u64,
        at: std::time::Instant,
    ) {
        let Some(exporting) = &self.exporting else {
            return;
        };
        let shared = &exporting.shared;
        let held = {
            let mut filters = shared.lock_filters();
            // No open run on this pane: nothing to count it against.
            let Some(input) = filters
                .get_mut(pane_id)
                .and_then(|inputs| inputs.last_mut())
                .filter(|input| !input.ended)
            else {
                return;
            };
            // Read under the filters' lock, so the run's input is noted in
            // the order of the broadcast. This is when telemetry is told
            // about the write, not when the PTY accepted it: the pane has
            // already handed the bytes to the agent, so an agent fast enough
            // to read the key and fire a hook before this line has that hook
            // broadcast first, and the key is counted after its own response
            // — against a prompt it already closed, say. Placing it by the
            // PTY's acceptance would take a position read at the write
            // itself, in the pane's writer.
            let position = shared.lock_clock().position();
            let (human, held) = input.write(bytes, now, position, at);
            // The input waits with its run; the first of it since the tracker
            // last took the run's input sends the reminder to take it.
            let wake = input.pending.is_empty() && !human.is_empty();
            let room = MAX_QUEUED_INPUT.saturating_sub(input.pending.len());
            input.pending.extend(human.into_iter().take(room));
            if wake {
                let _ = exporting.commands.send(Command::HumanInput {
                    pane_id: pane_id.to_string(),
                    run_id: input.run_id.clone(),
                });
            }
            held
        };
        if held {
            shared.escape_held.notify_one();
        }
    }

    /// The agent `agent_id` on `pane_id` is gone. Ignored when a newer
    /// generation already holds the pane. Applied after every event already
    /// broadcast, so the agent's last status is part of its run. A lone `ESC`
    /// still in its window is counted as the run ends, whether by this or by
    /// the pane's close broadcast arriving first.
    pub fn end_run(
        &self,
        pane_id: &str,
        agent_id: Option<&str>,
        reason: EndReason,
        exit_code: Option<i32>,
    ) {
        self.remove_run_files(|run| {
            run.pane_id == pane_id && (agent_id.is_none() || run.agent_id.as_deref() == agent_id)
        });
        let Some(exporting) = &self.exporting else {
            return;
        };
        let run_id = exporting
            .shared
            .lock_filters()
            .get_mut(pane_id)
            .and_then(|inputs| {
                inputs.iter_mut().rev().find(|input| {
                    !input.ended && (agent_id.is_none() || input.agent_id.as_deref() == agent_id)
                })
            })
            .map(|input| {
                input.ended = true;
                input.run_id.clone()
            });
        let _ = exporting.commands.send(Command::End {
            pane_id: pane_id.to_string(),
            agent_id: agent_id.map(str::to_string),
            run_id,
            reason,
            exit_code,
        });
    }

    /// Issue #1981 M7: the delegate path posted `posted`'s task: open its
    /// `agent.handoff`, stamped now, between the broadcast messages sent
    /// before and after this call. Nothing happens when not exporting.
    pub fn observe_delegate_posted(&self, posted: &super::DelegatePosted<'_>) {
        let Some(exporting) = &self.exporting else {
            return;
        };
        let shared = &exporting.shared;
        let seq = {
            // Read under the posts' lock, so the queue is in broadcast order.
            let mut posts = shared.lock_posts();
            let position = shared.lock_clock().position();
            let seq = posts.next_seq;
            posts.next_seq += 1;
            posts.queue.push_back(WaitingPost {
                seq,
                position,
                posted: super::handoff::Posted::of(posted, now_unix_nanos()),
            });
            seq
        };
        let _ = exporting.commands.send(Command::DelegatePosted { seq });
    }

    /// Feed the daemon's broadcast to the tracker until it closes. Nothing
    /// happens when not exporting, so an off deck spawns nothing.
    pub fn subscribe(&self, rx: broadcast::Receiver<BroadcastMsg>) {
        if let Some(exporting) = &self.exporting {
            *exporting.shared.lock_clock() = BroadcastClock::numbering(&rx);
            let _ = exporting.commands.send(Command::Subscribe(rx));
        }
    }

    /// Resolves once everything sent to the tracker before it — broadcast
    /// events included — has been applied. Immediately when not exporting.
    #[cfg(test)]
    pub async fn settled(&self) {
        let Some(exporting) = &self.exporting else {
            return;
        };
        let (ack, done) = oneshot::channel();
        if exporting.commands.send(Command::Barrier(ack)).is_ok() {
            let _ = done.await;
        }
    }

    /// The daemon was asked to stop at `at`: a stop client's window opened
    /// then, and its teardown — stopping every agent first — runs before
    /// [`Self::shutdown`] is called. The first call counts.
    pub fn teardown_began(&self, at: std::time::Instant) {
        if let Some(exporting) = &self.exporting {
            let _ = exporting.teardown_began.set(at);
        }
    }

    #[cfg(test)]
    pub(crate) fn teardown_began_at(&self) -> Option<std::time::Instant> {
        self.exporting
            .as_ref()
            .and_then(|e| e.teardown_began.get().copied())
    }

    /// Daemon shutdown, bounded in all by `budget`, or by what is left of the
    /// stop client's window when that is less — the window since
    /// [`Self::teardown_began`] (since now when it was not called), less
    /// [`SHUTDOWN_HEADROOM`] — so a slow backend cannot make the daemon's
    /// stop time out: what it has not taken by then is dropped, and logged.
    ///
    /// Make an export in flight give way, cancel the run-context probes
    /// (killing their processes), end every open run as `daemon_shutdown` once
    /// the events already broadcast are applied, and send what the exporter
    /// holds, retrying only while the budget lasts. Returns as soon as that is
    /// done, at once when nothing is pending.
    pub async fn shutdown(&self, budget: Duration) {
        self.remove_run_files(|_| true);
        let Some(exporting) = &self.exporting else {
            return;
        };
        let now = std::time::Instant::now();
        let began = exporting.teardown_began.get().copied().unwrap_or(now);
        let budget = budget.min(
            STOP_CLIENT_WINDOW
                .saturating_sub(now.saturating_duration_since(began))
                .saturating_sub(SHUTDOWN_HEADROOM),
        );
        let deadline = deadline_after(budget);
        exporting.shared.stopping.send_replace(true);
        exporting
            .probes
            .shutdown(budget.min(PROBE_SHUTDOWN_BUDGET))
            .await;
        let (ack, ended) = oneshot::channel();
        let mut last = Vec::new();
        if exporting.commands.send(Command::Shutdown { ack }).is_ok() {
            match tokio::time::timeout_at(deadline, ended).await {
                Ok(Ok(spans)) => last = spans,
                Ok(Err(_)) => {}
                Err(_) => {
                    tracing::warn!(
                        "telemetry: open runs could not be ended in time; spans dropped"
                    );
                }
            }
        }
        // Awaited, not tried: a queue still full of what a slow backend left
        // is drained by the exporter while the budget lasts.
        let queue = &exporting.shared.queue;
        let mut unqueued = last.len();
        let flushed = tokio::time::timeout_at(deadline, async {
            for span in last {
                queue.send(ExportMsg::Span(Box::new(span))).await.ok()?;
                unqueued -= 1;
            }
            let (ack, flushed) = oneshot::channel();
            queue.send(ExportMsg::Flush { ack, deadline }).await.ok()?;
            flushed.await.ok()
        })
        .await;
        if !matches!(flushed, Ok(Some(()))) {
            if unqueued > 0 {
                tracing::warn!(
                    spans = unqueued,
                    "telemetry: the export queue stayed full at shutdown; the final runs and \
                     the flush were dropped"
                );
            } else {
                tracing::warn!("telemetry: the final flush did not finish in time; spans dropped");
            }
        }
        if let Some(ingest) = exporting
            .ingest
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            ingest.abort();
        }
    }

    fn lock_run_files(&self) -> std::sync::MutexGuard<'_, Vec<RunFiles>> {
        self.run_files.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Remove the per-run configuration files of the runs `which` selects.
    fn remove_run_files(&self, which: impl Fn(&RunFiles) -> bool) {
        let removed: Vec<RunFiles> = {
            let mut runs = self.lock_run_files();
            let (removed, kept) = std::mem::take(&mut *runs).into_iter().partition(&which);
            *runs = kept;
            removed
        };
        let Some(dir) = self.config_dir.as_deref() else {
            return;
        };
        for run in removed {
            super::launch::remove_run_config_files(dir, &run.run_id);
        }
    }

    /// The committed runs whose configuration files are removed with them.
    #[cfg(test)]
    pub(crate) fn run_file_ids(&self) -> Vec<String> {
        self.lock_run_files()
            .iter()
            .map(|run| run.run_id.clone())
            .collect()
    }

    /// The descriptor holding this daemon's usage lease, once it has one:
    /// a test closes it to stand for the kernel closing it as a crashed
    /// daemon's process ends.
    #[cfg(all(test, unix))]
    pub(crate) fn usage_lease_fd(&self) -> Option<std::os::fd::RawFd> {
        use std::os::fd::AsRawFd as _;
        self.usage_lease.with_held(|lease| lease._held.as_raw_fd())
    }

    /// Run-context probes queued or running.
    #[cfg(test)]
    pub fn outstanding_probes(&self) -> usize {
        self.exporting
            .as_ref()
            .map_or(0, |exporting| exporting.probes.outstanding())
    }
}

impl Shared {
    /// Run `f` on the tracker, then queue the spans it finished. A run that
    /// ended with a usage record to read has it drained after the tracker is
    /// let go, on a blocking thread ([`Self::complete_later`]); one with
    /// nothing to read is among the spans queued here.
    fn with_tracker(&self, f: impl FnOnce(&mut RunTracker)) {
        let (finished, finishing) = {
            let mut tracker = self.tracker.lock().unwrap_or_else(|p| p.into_inner());
            f(&mut tracker);
            tracker.take_finished_deferred()
        };
        self.enqueue(finished);
        self.complete_later(finishing);
    }

    /// Drain the usage record of each run in `finishing` and queue its span
    /// once that is done ([`FinishingRun::complete`]), on a blocking thread,
    /// so the caller — the ingest task, mostly — goes on with other runs'
    /// events meanwhile. The batch is counted in [`Self::finalizing`] until
    /// its task ends. A run whose drain panics still gives its span, its
    /// usage incomplete ([`FinishingRun::complete`]), so one run's failure
    /// does not take the others' spans with it.
    fn complete_later(&self, finishing: Vec<FinishingRun>) {
        if finishing.is_empty() {
            return;
        }
        /// Counts the batch off however its task ends.
        struct Done(std::sync::Weak<Shared>);
        impl Drop for Done {
            fn drop(&mut self) {
                if let Some(shared) = self.0.upgrade() {
                    shared.finalizing.send_modify(|n| *n -= 1);
                }
            }
        }
        self.finalizing.send_modify(|n| *n += 1);
        let me = self.me.clone();
        self.runtime.spawn_blocking(move || {
            let done = Done(me);
            let spans: Vec<SpanRecord> =
                finishing.into_iter().map(FinishingRun::complete).collect();
            if let Some(shared) = done.0.upgrade() {
                shared.enqueue(spans);
            }
        });
    }

    /// Resolves once no batch of ended runs ([`Self::complete_later`]) begun
    /// before the call is still being completed. A batch's task ends having
    /// handed its spans to [`Self::enqueue`], which drops what a full queue
    /// has no room for, unless it panics outside the runs' drains — which
    /// [`FinishingRun::complete`] catches — and then its spans are lost.
    async fn finalized(&self) {
        let _ = self.finalizing.subscribe().wait_for(|n| *n == 0).await;
    }

    /// Poll every open run's usage record (PRD #1858) on a blocking thread,
    /// outside the tracker's lock, unless the last poll is still reading. No
    /// thread is taken while no open run has a record to read.
    fn poll_usage(&self, polling: &Arc<std::sync::atomic::AtomicBool>) {
        use std::sync::atomic::Ordering;
        if polling.swap(true, Ordering::AcqRel) {
            return;
        }
        let runs = self
            .tracker
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .usage_to_poll();
        if runs.is_empty() {
            polling.store(false, Ordering::Release);
            return;
        }
        let polling = Arc::clone(polling);
        tokio::task::spawn_blocking(move || {
            for run in runs {
                run.poll();
            }
            polling.store(false, Ordering::Release);
        });
    }

    /// Name the dispatched unit of every open run that has none yet, by the
    /// lookup [`Telemetry::set_dispatch_lookup`] set. The lookup is asked with
    /// the tracker let go, so it may take the deck's own locks; only the
    /// ingest task opens and ends runs, so a run it names is still the one it
    /// was asked about, or has ended and is not named.
    fn name_dispatched_runs(&self) {
        let Some(lookup) = self.dispatch_lookup.get() else {
            return;
        };
        let runs = self
            .tracker
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .runs_without_dispatch();
        let named: Vec<(String, String)> = runs
            .into_iter()
            .filter_map(|run| lookup(&run).map(|name| (run.run_id, name)))
            .collect();
        if !named.is_empty() {
            self.with_tracker(|t| {
                for (run_id, name) in &named {
                    t.set_dispatch_name(run_id, name);
                }
            });
        }
    }

    fn lock_filters(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<PaneInput>>> {
        lock_filters(&self.filters)
    }

    fn lock_clock(&self) -> std::sync::MutexGuard<'_, BroadcastClock> {
        self.clock.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn lock_posts(&self) -> std::sync::MutexGuard<'_, WaitingPosts> {
        self.posts.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Issue #1981 M7: place each task posted before broadcast message
    /// `position`, or every one with `None`, and numbered `seq` or earlier,
    /// or any with `None`, oldest first ([`RunTracker::place_post`]): its
    /// hand-off opens now, or, while its orchestrator's run is prepared and
    /// not opened yet, is held with that run's events in the same place.
    /// Returns the position of the first post within `seq` written after
    /// `position`.
    fn take_posts_before(
        &self,
        t: &mut RunTracker,
        position: Option<u64>,
        seq: Option<u64>,
    ) -> Option<u64> {
        let mut posts = self.lock_posts();
        // Numbers and positions both rise along the queue, so what is taken
        // is always a prefix of it.
        while let Some(first) = posts.queue.front() {
            if seq.is_some_and(|s| first.seq > s) {
                return None;
            }
            if position.is_some_and(|p| first.position > p) {
                return Some(first.position);
            }
            if let Some(first) = posts.queue.pop_front() {
                t.place_post(first.posted);
            }
        }
        None
    }

    /// [`Self::take_posts_before`], with the tracker locked for it.
    fn take_posts(&self, through: Option<u64>, seq: u64) -> Option<u64> {
        let mut left = None;
        self.with_tracker(|t| left = self.take_posts_before(t, through, Some(seq)));
        left
    }

    /// Count the human input waiting with `run_id` on `pane_id`, if the run
    /// has not ended and taken it already: what was written before broadcast
    /// message `through`, or all of it with `None`. Returns the broadcast
    /// position the last input left waiting was written at.
    fn take_input(&self, pane_id: &str, run_id: &str, through: Option<u64>) -> Option<u64> {
        let mut left = None;
        self.with_tracker(|t| {
            let held = self
                .lock_filters()
                .get_mut(pane_id)
                .and_then(|inputs| inputs.iter_mut().find(|input| input.run_id == run_id))
                .map(|input| match through {
                    Some(position) => input.take_through(position),
                    None => (input.take_held(false), None),
                });
            let Some((held, rest)) = held else {
                return;
            };
            left = rest;
            for w in held {
                t.observe_run_input(pane_id, run_id, w.at, w.kind);
            }
        });
        left
    }

    /// Count the input written before broadcast message `position` on
    /// `pane_id`, or on every pane with `None`, into each run the tracker
    /// takes input for, so it is read before that message is applied: what
    /// is pending, then a lone `ESC` whose window has passed by `now`.
    /// [`Self::escape_due_before`] says how long to wait for one still in its
    /// window first.
    fn take_input_before(
        &self,
        t: &mut RunTracker,
        pane_id: Option<&str>,
        position: u64,
        now: std::time::Instant,
    ) {
        let mut due = Vec::new();
        {
            let mut filters = self.lock_filters();
            for (pane, inputs) in filters.iter_mut() {
                if pane_id.is_some_and(|p| p != pane) {
                    continue;
                }
                for input in inputs.iter_mut() {
                    if input.owes_before(position) && t.takes_input(pane, &input.run_id) {
                        let taken = input.take_before(position, now);
                        due.push((pane.clone(), input.run_id.clone(), taken));
                    }
                }
            }
        }
        for (pane, run_id, taken) in due {
            for w in taken {
                t.observe_run_input(&pane, &run_id, w.at, w.kind);
            }
        }
    }

    /// Count every held lone `ESC` whose window has passed by `at` as human
    /// input at the time it was written, straight into its run, after any
    /// input still waiting with that run. Returns when the next one still
    /// held is due. One written after broadcast message `read`, the first the
    /// ingest task has not applied, is due now and left for the next call, so
    /// it is counted after the events broadcast before it.
    ///
    /// Any write after the ESC settles it on the writer's side instead. The
    /// tracker stays locked throughout, so no run ends between a filter
    /// giving its ESC up and the run counting it.
    ///
    /// A filter whose run the tracker has not opened yet — its
    /// [`Command::Commit`] still queued — is left as it is: the tracker would
    /// drop what it gave up. Its ESC is timed once the run opens: applying a
    /// Commit wakes the task again, because the wake the write sent can be
    /// taken by the task before the Commit is — `biased` orders the branches
    /// it polls, not a command's arrival against a wake.
    fn settle_escapes(&self, at: std::time::Instant, read: u64) -> Option<std::time::Instant> {
        let mut next_due: Option<std::time::Instant> = None;
        self.with_tracker(|t| {
            let mut filters = self.lock_filters();
            for (pane_id, inputs) in filters.iter_mut() {
                for input in inputs.iter_mut() {
                    let Some((since, _, position)) = input.escape else {
                        continue;
                    };
                    if t.run_id(pane_id) != Some(input.run_id.as_str()) {
                        continue;
                    }
                    let due = since + LONE_ESCAPE_WINDOW;
                    if at >= due && position > read {
                        next_due = Some(at);
                    } else if at >= due {
                        for w in input.take_held(true) {
                            t.observe_run_input(pane_id, &input.run_id, w.at, w.kind);
                        }
                    } else {
                        next_due = Some(next_due.map_or(due, |d| d.min(due)));
                    }
                }
            }
        });
        next_due
    }

    /// Drop `run_id`'s filter on `pane_id` if [`Telemetry::end_run`] ended it
    /// and the tracker did not retire it: its run never opened. Another
    /// generation's filter is left alone, whatever its End says.
    fn forget_ended_input(&self, pane_id: &str, run_id: &str) {
        let mut filters = self.lock_filters();
        if let Some(inputs) = filters.get_mut(pane_id) {
            inputs.retain(|input| !(input.ended && input.run_id == run_id));
            if inputs.is_empty() {
                filters.remove(pane_id);
            }
        }
    }

    /// When the last lone `ESC` written before broadcast message `position`
    /// on `pane_id`, or on any pane with `None`, into a run the tracker takes
    /// input for, leaves its window, if one is still in it at `now`: until
    /// then nobody can say whether it is the Escape key, and it has to be
    /// counted before that message is applied if it is.
    fn escape_due_before(
        &self,
        pane_id: Option<&str>,
        position: u64,
        now: std::time::Instant,
    ) -> Option<std::time::Instant> {
        let t = self.tracker.lock().unwrap_or_else(|p| p.into_inner());
        let filters = self.lock_filters();
        filters
            .iter()
            .filter(|(pane, _)| pane_id.is_none_or(|p| p == pane.as_str()))
            .flat_map(|(pane, inputs)| inputs.iter().map(move |input| (pane, input)))
            .filter_map(|(pane, input)| {
                let (since, _, at) = input.escape?;
                let due = since + LONE_ESCAPE_WINDOW;
                (at <= position && due > now && t.takes_input(pane, &input.run_id)).then_some(due)
            })
            .max()
    }

    /// Apply broadcast message `position`, after the input written before
    /// the daemon broadcast it. A lone `ESC` written before it is waited out
    /// first, so the Escape key counts in its place rather than after the
    /// message: until [`LONE_ESCAPE_WINDOW`] has passed since it was written,
    /// and never longer than that window from now, and only for events on its
    /// pane or naming no pane. A write that decides the `ESC` during the wait
    /// leaves what it decided in the `ESC`'s place, so that is counted before
    /// the message too. An `ESC` whose window has still not passed by then
    /// stays held, and counts after the message if it is the Escape key.
    async fn observe_in_order(&self, msg: &BroadcastMsg, position: u64) {
        let now = std::time::Instant::now();
        if let BroadcastMsg::Event(event) = msg {
            self.record_hook_latency(event);
        }
        if let BroadcastMsg::Event(event) = msg
            && let Some(due) = self.escape_due_before(event.pane_id.as_deref(), position, now)
        {
            let due = due.min(now + LONE_ESCAPE_WINDOW);
            #[cfg(test)]
            let gate = self.wait_gate.lock().unwrap().take();
            #[cfg(test)]
            if let Some((entered, resume)) = gate {
                let _ = entered.send(());
                let _ = resume.await;
            }
            tokio::time::sleep_until(Instant::from_std(due)).await;
        }
        self.observe(msg, position, std::time::Instant::now());
    }

    /// Apply broadcast message `position`, after the input written before
    /// the daemon broadcast it, a lone `ESC` among it once its window has
    /// passed by `now`.
    fn observe(&self, msg: &BroadcastMsg, position: u64, now: std::time::Instant) {
        match msg {
            BroadcastMsg::Event(event) => self.with_tracker(|t| {
                self.take_posts_before(t, Some(position), None);
                // An event naming no pane can be applied to any.
                self.take_input_before(t, event.pane_id.as_deref(), position, now);
                t.observe_event(event, now_unix_nanos());
            }),
            BroadcastMsg::Inbox(notice) => self.with_tracker(|t| {
                self.take_posts_before(t, Some(position), None);
                t.observe_inbox(notice, now_unix_nanos());
            }),
            _ => {
                if !self.lock_posts().queue.is_empty() {
                    self.with_tracker(|t| {
                        self.take_posts_before(t, Some(position), None);
                    });
                }
            }
        }
        self.lock_clock().catch_up();
    }

    /// Issue #1981 M8: count how long `event` took from its hook's stamp to
    /// the daemon receiving it ([`super::health`]), when the daemon admitted
    /// it from a producer: only those carry the daemon's receipt stamp
    /// ([`crate::event::RECEIVED_BY_DECK_AT_METADATA_KEY`]), so no event the
    /// daemon builds itself is counted, whatever its type.
    fn record_hook_latency(&self, event: &crate::event::AgentEvent) {
        if self.health_interval.is_none() {
            return;
        }
        let Some(received) = event
            .metadata
            .get(crate::event::RECEIVED_BY_DECK_AT_METADATA_KEY)
            .and_then(|v| v.parse::<u64>().ok())
        else {
            return;
        };
        let Some(stamped) = event
            .timestamp
            .timestamp_nanos_opt()
            .and_then(|n| u64::try_from(n).ok())
        else {
            return;
        };
        self.lock_health().record(stamped, received);
    }

    fn lock_health(&self) -> std::sync::MutexGuard<'_, super::health::Window> {
        self.health.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Issue #1981 M8: end the health interval at `now`, the daemon's
    /// resident memory then being `rss`, and queue its `daemon.health` span.
    fn sample_health(&self, now: u64, rss: Option<u64>) {
        let runs_open = self
            .tracker
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .open_runs();
        let dropped = self
            .dropped_total
            .load(std::sync::atomic::Ordering::Relaxed);
        let span = self.lock_health().close(now, runs_open, rss, dropped);
        self.enqueue(vec![span]);
    }

    fn enqueue(&self, spans: Vec<SpanRecord>) {
        for span in spans {
            if self
                .queue
                .try_send(ExportMsg::Span(Box::new(span)))
                .is_err()
            {
                self.dropped_total
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let mut log = self.dropped.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(suppressed) = log.record(std::time::Instant::now()) {
                    tracing::warn!(
                        suppressed,
                        "telemetry: the export queue is full; dropping spans"
                    );
                }
            }
        }
    }
}

fn start_exporting(
    config: &TelemetryConfig,
    host: Option<&str>,
    settings: Settings,
) -> Result<Exporting, reqwest::Error> {
    let (stopping, stop) = watch::channel(false);
    #[cfg(test)]
    let queue = match settings.sink {
        Some(sink) => sink,
        None => spawn_exporter(config, host, settings.retry, stop)?,
    };
    #[cfg(not(test))]
    let queue = spawn_exporter(config, host, settings.retry, stop)?;
    let health_interval = settings.health_interval;
    let filters: Arc<Filters> = Arc::default();
    let mut tracker = RunTracker::new();
    let held = Arc::clone(&filters);
    tracker.set_held_input(Box::new(move |pane_id, run_id| {
        retire_input(&held, pane_id, run_id)
    }));
    let shared = Arc::new_cyclic(|me| Shared {
        tracker: Mutex::new(tracker),
        queue,
        dropped: Mutex::new(RateLimitedLog::new(ERROR_LOG_INTERVAL)),
        dropped_total: std::sync::atomic::AtomicU64::new(0),
        health_interval,
        health: Mutex::new(super::health::Window::new(now_unix_nanos())),
        filters,
        clock: Mutex::default(),
        posts: Mutex::default(),
        escape_held: tokio::sync::Notify::new(),
        stopping,
        runtime: tokio::runtime::Handle::current(),
        me: me.clone(),
        finalizing: watch::Sender::new(0),
        dispatch_lookup: OnceLock::new(),
        #[cfg(test)]
        timer_gate: Mutex::new(None),
        #[cfg(test)]
        wait_gate: Mutex::new(None),
        #[cfg(test)]
        command_gate: Mutex::new(settings.command_gate),
        #[cfg(test)]
        command_gate_resumed: std::sync::atomic::AtomicBool::new(false),
    });
    let (commands, rx) = mpsc::unbounded_channel();
    let ingest = tokio::spawn(run_ingest(Arc::clone(&shared), rx));
    Ok(Exporting {
        shared,
        commands,
        probes: Probes::new(settings.probes),
        ingest: Mutex::new(Some(ingest)),
        teardown_began: OnceLock::new(),
    })
}

/// Events applied between two yields to the runtime while draining.
const DRAIN_YIELD_EVERY: usize = 256;

/// The ingest task: apply broadcast events as they come, each command after
/// every event broadcast before it, human input between the events broadcast
/// before and after it was written, and a held lone `ESC` as the Escape key
/// once its window has passed.
async fn run_ingest(shared: Arc<Shared>, mut commands: mpsc::UnboundedReceiver<Command>) {
    let mut events: Option<broadcast::Receiver<BroadcastMsg>> = None;
    // The broadcast messages `events` has passed, read or missed: the number
    // of the next one, as [`BroadcastClock`] counts.
    let mut read: u64 = 0;
    let mut escape_due: Option<std::time::Instant> = None;
    let mut usage_poll = tokio::time::interval(USAGE_POLL_INTERVAL);
    usage_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let usage_polling = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // `interval_at`: a plain interval's first tick is immediate, which would
    // send an empty first interval at once.
    let mut health = shared.health_interval.map(|every| {
        let mut tick = tokio::time::interval_at(Instant::now() + every, every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick
    });
    loop {
        #[cfg(test)]
        pause_at_command_gate(&shared, &commands).await;
        tokio::select! {
            biased;
            command = next_command(&shared, &mut commands) => {
                let Some(command) = command else { break };
                if let Command::Subscribe(rx) = command {
                    events = Some(rx);
                    read = 0;
                    continue;
                }
                drain(&shared, &mut events, &mut read).await;
                if let Command::HumanInput { pane_id, run_id } = &command {
                    take_input(&shared, &mut events, &mut read, pane_id, run_id).await;
                    continue;
                }
                if let Command::DelegatePosted { seq } = command {
                    take_posts(&shared, &mut events, &mut read, seq).await;
                    continue;
                }
                if !apply(&shared, command) {
                    break;
                }
            }
            () = shared.escape_held.notified() => {
                #[cfg(test)]
                let gate = shared.timer_gate.lock().unwrap().take();
                #[cfg(test)]
                if let Some((entered, resume)) = gate {
                    let _ = entered.send(());
                    let _ = resume.await;
                }
                drain(&shared, &mut events, &mut read).await;
                escape_due = shared.settle_escapes(std::time::Instant::now(), read);
            }
            () = sleep_until(escape_due) => {
                drain(&shared, &mut events, &mut read).await;
                escape_due = shared.settle_escapes(std::time::Instant::now(), read);
            }
            _ = usage_poll.tick() => {
                shared.poll_usage(&usage_polling);
                shared.name_dispatched_runs();
                // Issue #1981 M7: a hand-off past its age bound ends then,
                // not when the next inbox change or post happens to come.
                shared.with_tracker(|t| t.expire_handoffs(now_unix_nanos()));
            }
            () = next_tick(&mut health) => {
                drain(&shared, &mut events, &mut read).await;
                // The interval ends now; the memory read off this worker
                // after it is that end's. No event is read meanwhile, and no
                // lock is held across the read.
                let now = now_unix_nanos();
                let rss = super::health::rss_bytes().await;
                shared.sample_health(now, rss);
            }
            msg = next_event(&mut events) => match msg {
                Ok(msg) => {
                    shared.observe_in_order(&msg, read).await;
                    read += 1;
                }
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    tracing::debug!(missed, "telemetry: broadcast subscriber lagged");
                    read += missed;
                }
                Err(broadcast::error::RecvError::Closed) => events = None,
            },
        }
    }
}

#[cfg(not(test))]
async fn next_command(
    _shared: &Shared,
    commands: &mut mpsc::UnboundedReceiver<Command>,
) -> Option<Command> {
    commands.recv().await
}

/// [`mpsc::UnboundedReceiver::recv`], which once resumed from
/// [`Shared::command_gate`] first yields, so the other branches are polled
/// before the queue, as when it was polled empty.
#[cfg(test)]
async fn next_command(
    shared: &Shared,
    commands: &mut mpsc::UnboundedReceiver<Command>,
) -> Option<Command> {
    if shared
        .command_gate_resumed
        .swap(false, std::sync::atomic::Ordering::SeqCst)
    {
        tokio::task::yield_now().await;
    }
    commands.recv().await
}

/// Test seam: pause the ingest task at [`Shared::command_gate`] the first
/// time the command queue is found empty, before any branch is polled, by
/// awaiting its second half rather than blocking the thread.
#[cfg(test)]
async fn pause_at_command_gate(shared: &Shared, commands: &mpsc::UnboundedReceiver<Command>) {
    if !commands.is_empty() {
        return;
    }
    let Some((entered, resume)) = shared.command_gate.lock().unwrap().take() else {
        return;
    };
    let _ = entered.send(());
    let _ = resume.await;
    shared
        .command_gate_resumed
        .store(true, std::sync::atomic::Ordering::SeqCst);
}

async fn next_tick(tick: &mut Option<tokio::time::Interval>) {
    match tick {
        Some(tick) => {
            tick.tick().await;
        }
        None => std::future::pending().await,
    }
}

async fn sleep_until(due: Option<std::time::Instant>) {
    match due {
        Some(due) => tokio::time::sleep_until(Instant::from_std(due)).await,
        None => std::future::pending().await,
    }
}

async fn next_event(
    events: &mut Option<broadcast::Receiver<BroadcastMsg>>,
) -> Result<BroadcastMsg, broadcast::error::RecvError> {
    match events {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Apply every event already broadcast — the ones waiting when the drain
/// starts, not the ones published while it runs, so a producer that never
/// stops cannot hold a command back. Yields to the runtime every
/// [`DRAIN_YIELD_EVERY`] events.
async fn drain(
    shared: &Shared,
    events: &mut Option<broadcast::Receiver<BroadcastMsg>>,
    read: &mut u64,
) {
    // The messages between this receiver and the sender's tail right now.
    let waiting = events.as_ref().map_or(0, |rx| rx.len() as u64);
    drain_to(shared, events, read, *read + waiting).await;
}

/// Apply the broadcast up to, not including, message `to`, or as far as it
/// has been sent. Yields to the runtime every [`DRAIN_YIELD_EVERY`] events,
/// and waits out a lone `ESC` held before an event on its pane
/// ([`Shared::observe_in_order`]).
async fn drain_to(
    shared: &Shared,
    events: &mut Option<broadcast::Receiver<BroadcastMsg>>,
    read: &mut u64,
    to: u64,
) {
    let Some(rx) = events else {
        return;
    };
    let mut since_yield = 0usize;
    while *read < to {
        match rx.try_recv() {
            Ok(msg) => {
                shared.observe_in_order(&msg, *read).await;
                *read += 1;
            }
            // The receiver skipped `missed` of them, which it will not see.
            Err(broadcast::error::TryRecvError::Lagged(missed)) => {
                tracing::debug!(missed, "telemetry: broadcast subscriber lagged");
                *read += missed;
            }
            Err(_) => break,
        }
        since_yield += 1;
        if since_yield >= DRAIN_YIELD_EVERY {
            since_yield = 0;
            tokio::task::yield_now().await;
        }
    }
}

/// Count the human input waiting with `run_id` on `pane_id`, each after the
/// events broadcast before it was written: input written since the drain
/// before this began waits for the broadcast to be applied up to it. Input
/// the broadcast cannot be applied up to — it closed — is counted as it is.
async fn take_input(
    shared: &Shared,
    events: &mut Option<broadcast::Receiver<BroadcastMsg>>,
    read: &mut u64,
    pane_id: &str,
    run_id: &str,
) {
    while let Some(written) = shared.take_input(pane_id, run_id, Some(*read)) {
        drain_to(shared, events, read, written).await;
        if *read < written {
            shared.take_input(pane_id, run_id, None);
            break;
        }
    }
}

/// Place task `seq`, if it still waits, and any posted before it, each
/// after the broadcast messages sent before it was posted, as [`take_input`]
/// counts input; never one posted after it, whose own reminder may be queued
/// behind the commit of its orchestrator's run. Posts the broadcast cannot be
/// applied up to — it closed — are placed as they are.
async fn take_posts(
    shared: &Shared,
    events: &mut Option<broadcast::Receiver<BroadcastMsg>>,
    read: &mut u64,
    seq: u64,
) {
    while let Some(posted) = shared.take_posts(Some(*read), seq) {
        drain_to(shared, events, read, posted).await;
        if *read < posted {
            shared.take_posts(None, seq);
            break;
        }
    }
}

/// Apply one command; `false` once the task is to stop.
fn apply(shared: &Arc<Shared>, command: Command) -> bool {
    match command {
        Command::Subscribe(_) => {}
        Command::Commit { run_id } => {
            shared.with_tracker(|t| t.commit_run(&run_id));
            // An Escape written into the run before this may already be held,
            // its wake taken while the run was not open: time it now.
            shared.escape_held.notify_one();
        }
        Command::End {
            pane_id,
            agent_id,
            run_id,
            reason,
            exit_code,
        } => {
            let now = now_unix_nanos();
            shared.with_tracker(|t| {
                t.end_agent_run(&pane_id, agent_id.as_deref(), reason, exit_code, now)
            });
            if let Some(run_id) = run_id {
                shared.forget_ended_input(&pane_id, &run_id);
            }
        }
        // Counted and applied by `run_ingest`, in order with the broadcast.
        Command::HumanInput { .. } | Command::DelegatePosted { .. } => {}
        #[cfg(test)]
        Command::Barrier(ack) => {
            // Once the runs ended before it are completed too.
            let shared = Arc::clone(shared);
            tokio::spawn(async move {
                shared.finalized().await;
                let _ = ack.send(());
            });
        }
        Command::Shutdown { ack } => {
            // A run's last key, a lone ESC still in its window, is counted as
            // the tracker ends the run.
            let (mut finished, finishing) = {
                let mut tracker = shared.tracker.lock().unwrap_or_else(|p| p.into_inner());
                // A task posted as shutdown began is opened, to end with the
                // rest, as is one held for a run not opened yet.
                shared.take_posts_before(&mut tracker, None, None);
                tracker.end_all(EndReason::DaemonShutdown, now_unix_nanos());
                tracker.take_finished_deferred()
            };
            shared.lock_filters().clear();
            // The runs still open with a usage record to read are completed
            // on a blocking thread, after every run ended before them has
            // queued its span, so the final flush `shutdown` sends after
            // these finds those in the queue. With none, no thread is taken.
            let shared = Arc::clone(shared);
            tokio::spawn(async move {
                shared.finalized().await;
                if !finishing.is_empty() {
                    let completed = tokio::task::spawn_blocking(move || {
                        finishing
                            .into_iter()
                            .map(FinishingRun::complete)
                            .collect::<Vec<_>>()
                    })
                    .await
                    .unwrap_or_else(|_| {
                        tracing::warn!(
                            "completing the runs open at shutdown failed; their spans are lost"
                        );
                        Vec::new()
                    });
                    finished.extend(completed);
                }
                // Queued by `shutdown`, which can wait for room; when it
                // stopped waiting for these, as best it can.
                if let Err(finished) = ack.send(finished) {
                    shared.enqueue(finished);
                }
            });
            return false;
        }
    }
    true
}

/// Counts occurrences and says when one may be logged: the first, then at most
/// one per interval, carrying how many were suppressed in between.
#[derive(Debug)]
pub struct RateLimitedLog {
    interval: Duration,
    last: Option<std::time::Instant>,
    suppressed: u64,
}

impl RateLimitedLog {
    pub fn new(interval: Duration) -> Self {
        RateLimitedLog {
            interval,
            last: None,
            suppressed: 0,
        }
    }

    /// `Some(suppressed since the last log)` when this occurrence may be
    /// logged, `None` when it is to be counted silently.
    pub fn record(&mut self, now: std::time::Instant) -> Option<u64> {
        match self.last {
            Some(last) if now.duration_since(last) < self.interval => {
                self.suppressed += 1;
                None
            }
            _ => {
                self.last = Some(now);
                Some(std::mem::take(&mut self.suppressed))
            }
        }
    }
}

fn build_headers(headers: &[(String, String)]) -> reqwest::header::HeaderMap {
    let mut map = reqwest::header::HeaderMap::new();
    let mut invalid = 0usize;
    for (key, value) in headers {
        match (
            reqwest::header::HeaderName::from_bytes(key.as_bytes()),
            reqwest::header::HeaderValue::from_str(value),
        ) {
            (Ok(name), Ok(mut value)) => {
                value.set_sensitive(true);
                map.insert(name, value);
            }
            _ => invalid += 1,
        }
    }
    if invalid > 0 {
        tracing::warn!(
            invalid,
            "telemetry: skipped OTLP headers that are not valid HTTP headers"
        );
    }
    map
}

/// The OTLP client: the configured headers on every request, and no redirect
/// followed, so those headers reach the configured endpoint and nothing else.
fn build_client(config: &TelemetryConfig) -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .timeout(config.timeout)
        .redirect(reqwest::redirect::Policy::none())
        .default_headers(build_headers(&config.headers))
        .build()
}

/// `stop` is [`Shared::stopping`]'s receiving end.
fn spawn_exporter(
    config: &TelemetryConfig,
    host: Option<&str>,
    retry: RetryPolicy,
    stop: watch::Receiver<bool>,
) -> Result<mpsc::Sender<ExportMsg>, reqwest::Error> {
    let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
    let sender = Sender {
        client: build_client(config)?,
        endpoint: config.endpoint.clone(),
        protocol: config.protocol.clone(),
        resource: deck_resource(&config.service_name, host, &config.user_resource_attributes),
        timeout: config.timeout,
        retry,
        errors: RateLimitedLog::new(ERROR_LOG_INTERVAL),
        stop,
        #[cfg(test)]
        panic_on_send: false,
    };
    tokio::spawn(run_exporter(sender, rx));
    Ok(tx)
}

/// The waits between attempts of one export.
#[derive(Debug, Clone, Copy)]
struct RetryPolicy {
    initial: Duration,
    max: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            initial: RETRY_INITIAL_BACKOFF,
            max: RETRY_MAX_BACKOFF,
        }
    }
}

/// How one export ended.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SendOutcome {
    Delivered {
        attempts: u32,
    },
    /// Delivered, and the backend's answer reports spans it did not accept,
    /// or a warning (the OTLP `partial_success`). Not retried.
    PartiallyRejected {
        attempts: u32,
        rejected: i64,
    },
    /// The backend answered with a status that is not retried (a 3xx among
    /// them).
    Rejected {
        status: u16,
    },
    /// Every attempt failed transiently and the time ran out.
    GaveUp {
        attempts: u32,
    },
    /// A failure that is not retried, other than an HTTP status.
    Failed,
    /// Shutdown began first: the batch is back in the buffer, for the final
    /// flush to send.
    Deferred,
}

struct Sender {
    client: reqwest::Client,
    endpoint: String,
    protocol: Protocol,
    resource: Vec<ResourceAttribute>,
    timeout: Duration,
    retry: RetryPolicy,
    errors: RateLimitedLog,
    /// True once shutdown has begun ([`Shared::stopping`]).
    stop: watch::Receiver<bool>,
    /// Test seam: every send panics once it has taken its batch.
    #[cfg(test)]
    panic_on_send: bool,
}

/// Whether an HTTP status is a transient failure the OTLP specification
/// retries. Shared with PRD #1859's collector.
pub fn is_retryable_status(status: u16) -> bool {
    matches!(status, 429 | 502 | 503 | 504)
}

/// A `Retry-After` given in seconds. The HTTP-date form is not honoured; the
/// backoff applies instead.
pub fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

/// `Instant::now() + after`, or a day from now when that is not representable:
/// `OTEL_EXPORTER_OTLP_TIMEOUT` accepts any `u64` of milliseconds.
fn deadline_after(after: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(after)
        .unwrap_or_else(|| now + Duration::from_secs(86_400))
}

/// `backoff` scaled by a random factor in `[0.5, 1]`.
pub fn jittered(backoff: Duration) -> Duration {
    let mut bytes = [0u8; 4];
    let fraction = match getrandom::fill(&mut bytes) {
        Ok(()) => f64::from(u32::from_le_bytes(bytes)) / f64::from(u32::MAX),
        Err(_) => 1.0,
    };
    backoff.mul_f64(0.5 + 0.5 * fraction)
}

/// Resolves once `stop` is true; never, when its sender is gone first.
async fn stopped(stop: &mut watch::Receiver<bool>) {
    if stop.wait_for(|stopping| *stopping).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// What a failed request is, for a log line: a fixed description of its
/// kind. Never the error's own text, which can carry the endpoint.
fn request_failure(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        "the request timed out"
    } else if e.is_connect() {
        "the backend could not be reached"
    } else if e.is_request() {
        "the request could not be sent"
    } else if e.is_body() || e.is_decode() {
        "the backend's answer could not be read"
    } else {
        "the request failed"
    }
}

impl Sender {
    /// Send `batch`, retrying transient failures until the export timeout or
    /// `flush_deadline`, whichever comes first. A send that is not part of a
    /// flush gives way once shutdown begins, leaving `batch` as it was.
    async fn send(
        &mut self,
        batch: &mut Vec<SpanRecord>,
        flush_deadline: Option<Instant>,
    ) -> Option<SendOutcome> {
        if batch.is_empty() {
            return None;
        }
        if flush_deadline.is_none() && *self.stop.borrow() {
            return Some(SendOutcome::Deferred);
        }
        let spans = std::mem::take(batch);
        #[cfg(test)]
        assert!(!self.panic_on_send, "telemetry test: a send that panics");
        let (body, content_type) = match self.protocol {
            Protocol::HttpJson => (
                otlp::encode_json(&self.resource, &spans),
                "application/json",
            ),
            _ => (
                otlp::encode_protobuf(&self.resource, &spans),
                "application/x-protobuf",
            ),
        };
        let mut deadline = deadline_after(self.timeout);
        if let Some(flush) = flush_deadline {
            deadline = deadline.min(flush);
        }
        let attempts = self.deliver(body, content_type, deadline);
        let delivered = if flush_deadline.is_some() {
            Some(attempts.await)
        } else {
            let mut stop = self.stop.clone();
            tokio::select! {
                biased;
                () = stopped(&mut stop) => None,
                done = attempts => Some(done),
            }
        };
        let Some((outcome, attempts, failure)) = delivered else {
            *batch = spans;
            return Some(SendOutcome::Deferred);
        };
        let reportable = !matches!(outcome, SendOutcome::Delivered { .. });
        if reportable && let Some(suppressed) = self.errors.record(std::time::Instant::now()) {
            // The backend's own message is not logged: it can echo the
            // configured headers' values.
            if let SendOutcome::PartiallyRejected { rejected, .. } = outcome {
                tracing::warn!(
                    spans = spans.len(),
                    rejected,
                    suppressed_failures = suppressed,
                    endpoint = %redacted_endpoint(&self.endpoint),
                    "telemetry: the OTLP backend did not accept every span"
                );
            } else {
                tracing::warn!(
                    spans = spans.len(),
                    attempts,
                    suppressed_failures = suppressed,
                    endpoint = %redacted_endpoint(&self.endpoint),
                    "telemetry: OTLP export failed: {failure}"
                );
            }
        }
        Some(outcome)
    }

    /// The attempts of one export: how it ended, how many there were, and
    /// what to log about it — fixed text and the HTTP status, nothing the
    /// backend wrote.
    async fn deliver(
        &self,
        body: Vec<u8>,
        content_type: &str,
        deadline: Instant,
    ) -> (SendOutcome, u32, String) {
        let mut backoff = self.retry.initial;
        let mut attempts = 0u32;
        let mut failure = String::new();
        let outcome = loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break SendOutcome::GaveUp { attempts };
            }
            attempts += 1;
            let result = self
                .client
                .post(&self.endpoint)
                .timeout(remaining)
                .header(reqwest::header::CONTENT_TYPE, content_type)
                .body(body.clone())
                .send()
                .await;
            let wait = match result {
                Ok(response) if response.status().is_success() => {
                    match self.partial_success(response).await {
                        Some(partial)
                            if partial.rejected > 0 || !partial.error_message.is_empty() =>
                        {
                            break SendOutcome::PartiallyRejected {
                                attempts,
                                rejected: partial.rejected,
                            };
                        }
                        _ => break SendOutcome::Delivered { attempts },
                    }
                }
                Ok(response) if is_retryable_status(response.status().as_u16()) => {
                    failure = format!("the backend answered HTTP {}", response.status().as_u16());
                    retry_after(&response)
                }
                Ok(response) => {
                    let status = response.status().as_u16();
                    failure = format!("the backend answered HTTP {status}");
                    break SendOutcome::Rejected { status };
                }
                Err(e) if e.is_connect() || e.is_timeout() || e.is_request() => {
                    failure = request_failure(&e).to_string();
                    None
                }
                Err(e) => {
                    failure = request_failure(&e).to_string();
                    break SendOutcome::Failed;
                }
            };
            // A `Retry-After` can lengthen the wait, never shorten it below
            // the backoff: `Retry-After: 0` from an overloaded backend must
            // not turn into back-to-back retries.
            let backoff_wait = jittered(backoff);
            let wait = wait.map_or(backoff_wait, |asked| asked.max(backoff_wait));
            // Compared as durations: `now + wait` overflows `Instant` for a
            // `Retry-After` as large as `u64::MAX` seconds.
            if wait >= deadline.saturating_duration_since(Instant::now()) {
                break SendOutcome::GaveUp { attempts };
            }
            tokio::time::sleep(wait).await;
            backoff = (backoff * 2).min(self.retry.max);
        };
        (outcome, attempts, failure)
    }

    /// What a successful answer reports the backend did not accept, from at
    /// most [`MAX_RESPONSE_BODY`] bytes of it, in the configured protocol's
    /// format. `None` when it reports nothing, or cannot be read.
    async fn partial_success(&self, response: reqwest::Response) -> Option<otlp::PartialSuccess> {
        let body = bounded_body(response, MAX_RESPONSE_BODY).await;
        match self.protocol {
            Protocol::HttpJson => otlp::decode_partial_success_json(&body),
            _ => otlp::decode_partial_success_protobuf(&body),
        }
    }
}

/// At most `max` bytes of `response`'s body; what could not be read is left
/// out.
async fn bounded_body(mut response: reqwest::Response, max: usize) -> Vec<u8> {
    let mut body = Vec::new();
    while body.len() < max {
        let Ok(Some(chunk)) = response.chunk().await else {
            break;
        };
        let room = max - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(room)]);
    }
    body
}

/// Await one export, so that a panic inside it costs that batch only: the
/// exporter task, and every later export, carries on. The batch was taken
/// out of the task's buffer when the send began. `Err` when it panicked.
async fn contained<F: std::future::Future>(send: F) -> Result<F::Output, Panicked> {
    use futures_util::FutureExt;
    std::panic::AssertUnwindSafe(send)
        .catch_unwind()
        .await
        .map_err(|_| Panicked)
}

/// An export that panicked; see [`contained`].
#[derive(Debug)]
struct Panicked;

impl Sender {
    /// [`Self::send`], with a panic inside it contained and logged like any
    /// other failed export: at most once per [`ERROR_LOG_INTERVAL`].
    async fn export(&mut self, batch: &mut Vec<SpanRecord>, flush_deadline: Option<Instant>) {
        if contained(self.send(batch, flush_deadline)).await.is_err()
            && let Some(suppressed) = self.errors.record(std::time::Instant::now())
        {
            tracing::warn!(
                suppressed_failures = suppressed,
                "telemetry: an OTLP export failed unexpectedly; its spans are dropped"
            );
        }
    }
}

async fn run_exporter(mut sender: Sender, mut rx: mpsc::Receiver<ExportMsg>) {
    let mut batch: Vec<SpanRecord> = Vec::new();
    // `interval_at`: a plain interval's first tick is immediate, which would
    // send the first span alone.
    let mut tick = tokio::time::interval_at(Instant::now() + BATCH_DELAY, BATCH_DELAY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(ExportMsg::Span(span)) => {
                    batch.push(*span);
                    if batch.len() >= MAX_BATCH {
                        sender.export(&mut batch, None).await;
                    }
                }
                Some(ExportMsg::Flush { ack, deadline }) => {
                    // Everything queued before the flush, in batches.
                    while let Ok(ExportMsg::Span(span)) = rx.try_recv() {
                        batch.push(*span);
                    }
                    while !batch.is_empty() && Instant::now() < deadline {
                        let rest = batch.split_off(batch.len().min(MAX_BATCH));
                        sender.export(&mut batch, Some(deadline)).await;
                        batch = rest;
                    }
                    if !batch.is_empty() {
                        tracing::warn!(
                            spans = batch.len(),
                            "telemetry: the final flush ran out of time; spans dropped"
                        );
                        batch.clear();
                    }
                    let _ = ack.send(());
                }
                None => {
                    sender.export(&mut batch, None).await;
                    break;
                }
            },
            _ = tick.tick() => {
                sender.export(&mut batch, None).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AgentEvent, AgentType, EventType};
    use crate::telemetry::span::{AttrValue, Attribute};
    use crate::telemetry::tracker::tests::event;
    use crate::telemetry::{AttrKey, SPAN_AGENT_RUN, SPAN_AGENT_STATUS};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn env(pairs: &[(&str, &str)]) -> EnvMap {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn run_start(pane: &str) -> RunStart {
        RunStart {
            run_id: String::new(),
            pane_id: pane.to_string(),
            agent_id: None,
            agent: AgentType::ClaudeCode,
            orchestration: None,
            model: None,
            invoking_skill: None,
            issue_number: None,
        }
    }

    fn generation(pane: &str, agent_id: &str) -> RunStart {
        RunStart {
            agent_id: Some(agent_id.to_string()),
            ..run_start(pane)
        }
    }

    /// What the spawn path does: prepare, add the variables, commit.
    fn begin(
        telemetry: &Arc<Telemetry>,
        start: RunStart,
        spawn_env: &[(String, String)],
    ) -> Vec<(String, String)> {
        let run = telemetry.prepare_run(start, spawn_env, None).unwrap();
        let added = run.env.clone();
        run.commit();
        added
    }

    fn get<'a>(added: &'a [(String, String)], key: &str) -> Option<&'a str> {
        added
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    const ON: [(&str, &str); 1] = [("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:9")];

    /// A telemetry handle whose finished spans land in the returned receiver
    /// instead of an HTTP exporter.
    fn capturing(probes: ProbeSettings) -> (Arc<Telemetry>, mpsc::Receiver<ExportMsg>) {
        let (sink, rx) = mpsc::channel(QUEUE_CAPACITY);
        let telemetry = Telemetry::start_with(
            &env(&ON),
            Settings {
                probes,
                sink: Some(sink),
                ..Settings::default()
            },
        );
        assert!(telemetry.is_exporting());
        (Arc::new(telemetry), rx)
    }

    fn captured(rx: &mut mpsc::Receiver<ExportMsg>) -> Vec<SpanRecord> {
        let mut spans = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let ExportMsg::Span(span) = msg {
                spans.push(*span);
            }
        }
        spans
    }

    fn attr(span: &SpanRecord, key: AttrKey) -> Option<&AttrValue> {
        span.attributes
            .iter()
            .find(|a| a.key == key)
            .map(|a| &a.value)
    }

    fn statuses(spans: &[SpanRecord]) -> Vec<AttrValue> {
        spans
            .iter()
            .filter(|s| s.name == SPAN_AGENT_STATUS)
            .filter_map(|s| attr(s, AttrKey::Status).cloned())
            .collect()
    }

    fn of(pane: &str, agent_id: &str, event_type: EventType) -> BroadcastMsg {
        let mut e = event(pane, event_type);
        e.agent_id = Some(agent_id.to_string());
        BroadcastMsg::Event(e)
    }

    /// The daemon's own broadcast that it closed the pane (`spawn.rs`).
    fn pane_closed(pane: &str, agent_id: &str) -> BroadcastMsg {
        let mut e = event(pane, EventType::SessionEnd);
        e.agent_id = Some(agent_id.to_string());
        e.metadata.insert(
            crate::event::DAEMON_PANE_CLOSED_METADATA_KEY.into(),
            "1".into(),
        );
        BroadcastMsg::Event(e)
    }

    /// Wait until `done` holds. The outer bound is a watchdog for a hung
    /// test, far past anything a loaded runner needs, not a timing claim.
    async fn eventually(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while !done() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting until {what}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// A monotonic time an hour ahead: a lone `ESC` written "then" is still
    /// in its window whatever the scheduler does, so only the run ending can
    /// settle it.
    fn window_held_open() -> std::time::Instant {
        std::time::Instant::now() + Duration::from_secs(3600)
    }

    /// The docs promise that a Claude Code switch the user set is left alone.
    /// The spawn overlay rarely carries it; the daemon's own environment, where
    /// the user exports it, usually does — and the overlay wins in the child's
    /// environment, so a `1` computed from the overlay alone would override
    /// the user's `0`.
    #[test]
    fn a_switch_set_in_the_daemon_environment_or_the_overlay_stands() {
        let telemetry = Arc::new(Telemetry::start(&env(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:9"),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc"),
            ("CLAUDE_CODE_ENABLE_TELEMETRY", "0"),
            ("OTEL_METRICS_EXPORTER", "none"),
        ])));
        let overlay = vec![("OTEL_LOGS_EXPORTER".to_string(), "console".to_string())];
        let run = telemetry
            .prepare_run(run_start("p1"), &overlay, None)
            .unwrap();
        assert_eq!(get(&run.env, "CLAUDE_CODE_ENABLE_TELEMETRY"), None);
        assert_eq!(get(&run.env, "OTEL_METRICS_EXPORTER"), None);
        assert_eq!(get(&run.env, "OTEL_LOGS_EXPORTER"), None);
        assert_eq!(get(&run.env, "OTEL_EXPORTER_OTLP_PROTOCOL"), None);
        assert!(get(&run.env, "OTEL_RESOURCE_ATTRIBUTES").is_some());

        let unset = Arc::new(Telemetry::start(&env(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:9"),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc"),
        ])));
        let run = unset.prepare_run(run_start("p1"), &[], None).unwrap();
        assert_eq!(get(&run.env, "CLAUDE_CODE_ENABLE_TELEMETRY"), Some("1"));
        assert_eq!(get(&run.env, "OTEL_METRICS_EXPORTER"), Some("otlp"));
        assert_eq!(get(&run.env, "OTEL_LOGS_EXPORTER"), Some("otlp"));
    }

    /// Pi's extension is switched on only when the agent's own environment —
    /// the daemon's with the spawn's overlay over it — leaves the SDK and the
    /// logs exporter on.
    #[test]
    fn pi_is_not_switched_on_against_a_spawn_opt_out() {
        let pi = || RunStart {
            agent: AgentType::Pi,
            ..run_start("p1")
        };
        let flag = |telemetry: &Arc<Telemetry>, overlay: &[(&str, &str)]| {
            let overlay: Vec<_> = overlay
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            let run = telemetry.prepare_run(pi(), &overlay, None).unwrap();
            get(&run.env, super::super::launch::PI_TELEMETRY_ENV).map(str::to_string)
        };
        let telemetry = Arc::new(Telemetry::start(&env(&[(
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            "http://127.0.0.1:9",
        )])));
        assert_eq!(flag(&telemetry, &[]).as_deref(), Some("1"));
        for overlay in [
            &[("OTEL_SDK_DISABLED", "true")][..],
            &[("OTEL_SDK_DISABLED", "True")][..],
            &[("OTEL_LOGS_EXPORTER", "none")][..],
            &[("OTEL_LOGS_EXPORTER", "otlpx")][..],
        ] {
            assert_eq!(flag(&telemetry, overlay), None, "{overlay:?}");
        }
        let logs_off = Arc::new(Telemetry::start(&env(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:9"),
            ("OTEL_LOGS_EXPORTER", "none"),
        ])));
        assert_eq!(flag(&logs_off, &[]), None, "the daemon's own selector");
    }

    /// A switch the deck sets only when the user has not, set in the
    /// daemon's environment to a value that is not UTF-8, is the user's all
    /// the same: the deck adds none of it, for Claude Code or for Pi.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_switch_counts_as_set() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let start = |unreadable: &str| {
            Arc::new(Telemetry::start(&super::super::config::env_from([
                (
                    OsString::from("OTEL_EXPORTER_OTLP_ENDPOINT"),
                    OsString::from("http://127.0.0.1:9"),
                ),
                (
                    OsString::from(unreadable),
                    OsString::from_vec(vec![b'0', 0xff]),
                ),
            ])))
        };
        let pi = || RunStart {
            agent: AgentType::Pi,
            ..run_start("p1")
        };
        let pi_flag = super::super::launch::PI_TELEMETRY_ENV;
        // With the SDK switch unreadable the deck's own export is off too,
        // and there is no run to prepare.
        for switch in [
            pi_flag,
            "OTEL_SDK_DISABLED",
            "OTEL_LOGS_EXPORTER",
            "OTEL_EXPORTER_OTLP_LOGS_PROTOCOL",
        ] {
            let run = start(switch)
                .prepare_run(pi(), &[], None)
                .map(|run| run.env.clone())
                .unwrap_or_default();
            assert_eq!(get(&run, pi_flag), None, "{switch}");
        }
        for switch in [
            "CLAUDE_CODE_ENABLE_TELEMETRY",
            "OTEL_METRICS_EXPORTER",
            "OTEL_LOGS_EXPORTER",
            "OTEL_EXPORTER_OTLP_PROTOCOL",
        ] {
            let run = start(switch)
                .prepare_run(run_start("p1"), &[], None)
                .map(|run| run.env.clone())
                .unwrap_or_default();
            assert_eq!(get(&run, switch), None, "{switch}");
        }
        // Unreadable values the deck has no use for are still left out.
        let other = start("OTEL_SERVICE_NAME");
        let run = other.prepare_run(run_start("p1"), &[], None).unwrap();
        assert_eq!(get(&run.env, "CLAUDE_CODE_ENABLE_TELEMETRY"), Some("1"));
    }

    /// A Claude Code launch behind leading assignments or an `env` prefix,
    /// with no declared agent kind, is still Claude Code: its run says so and
    /// it gets the switches.
    #[test]
    fn a_wrapped_claude_launch_is_claude_and_gets_the_switches() {
        let telemetry = Arc::new(Telemetry::start(&env(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:9"),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc"),
        ])));
        for command in [
            "FOO=1 claude --model haiku",
            "env FOO=1 claude --model haiku",
        ] {
            let start = RunStart::for_spawn("p1", "a1", None, Some(command), None);
            assert_eq!(start.agent, AgentType::ClaudeCode, "{command}");
            let run = telemetry.prepare_run(start, &[], None).unwrap();
            assert_eq!(
                get(&run.env, "CLAUDE_CODE_ENABLE_TELEMETRY"),
                Some("1"),
                "{command}"
            );
            assert!(
                get(&run.env, "OTEL_RESOURCE_ATTRIBUTES")
                    .is_some_and(|attrs| attrs.contains("deck.agent=claude")),
                "{command}"
            );
        }
    }

    #[test]
    fn rate_limited_log_reports_the_first_then_one_per_interval() {
        let mut log = RateLimitedLog::new(Duration::from_secs(60));
        let t0 = std::time::Instant::now();
        assert_eq!(log.record(t0), Some(0));
        assert_eq!(log.record(t0 + Duration::from_secs(1)), None);
        assert_eq!(log.record(t0 + Duration::from_secs(59)), None);
        assert_eq!(log.record(t0 + Duration::from_secs(61)), Some(2));
        assert_eq!(log.record(t0 + Duration::from_secs(62)), None);
    }

    #[test]
    fn off_means_no_environment_and_nothing_recorded() {
        let telemetry = Arc::new(Telemetry::start(&env(&[("OTEL_SERVICE_NAME", "x")])));
        assert!(!telemetry.is_enabled());
        assert!(!telemetry.is_exporting());
        assert!(
            telemetry
                .prepare_run(run_start("p1"), &[], Some(PathBuf::from("/")))
                .is_none()
        );
    }

    /// With the shim installed every agent's `PATH` starts with its directory
    /// — in front of the spawn's own `PATH`, else the daemon's; without it the
    /// `PATH` is left alone. Every agent carries its run id either way (#1902).
    #[cfg(unix)]
    #[tokio::test]
    async fn the_shim_goes_first_on_every_agents_path() {
        let mut pairs = ON.to_vec();
        pairs.push(("PATH", "/usr/bin:/bin"));
        let mut telemetry = Telemetry::start(&env(&pairs));
        let plain = Arc::new(Telemetry::start(&env(&pairs)));
        telemetry.set_shim_dir(PathBuf::from("/state/telemetry/bin"));
        let telemetry = Arc::new(telemetry);
        for agent in [AgentType::None, AgentType::ClaudeCode, AgentType::Codex] {
            let start = RunStart {
                agent,
                ..run_start("p1")
            };
            let run = telemetry.prepare_run(start, &[], None).unwrap();
            assert_eq!(
                get(&run.env, "PATH"),
                Some("/state/telemetry/bin:/usr/bin:/bin")
            );
            assert_eq!(
                get(&run.env, super::super::shim::RUN_ID_ENV),
                Some(run.run_id.as_str())
            );
        }
        let own = [("PATH".to_string(), "/mine".to_string())];
        let run = telemetry.prepare_run(run_start("p2"), &own, None).unwrap();
        assert_eq!(get(&run.env, "PATH"), Some("/state/telemetry/bin:/mine"));
        // An empty PATH is the current directory, and still is after the shim.
        let empty = [("PATH".to_string(), String::new())];
        let run = telemetry
            .prepare_run(run_start("p4"), &empty, None)
            .unwrap();
        assert_eq!(get(&run.env, "PATH"), Some("/state/telemetry/bin:"));
        assert_eq!(
            get(&run.env, super::super::shim::RUN_ID_ENV),
            Some(run.run_id.as_str())
        );
        let run = plain.prepare_run(run_start("p3"), &[], None).unwrap();
        assert_eq!(get(&run.env, "PATH"), None, "no shim, no PATH");
        assert_eq!(
            get(&run.env, super::super::shim::RUN_ID_ENV),
            Some(run.run_id.as_str()),
            "no shim, still the run id"
        );
    }

    /// PRD #1858: a Pi or OpenCode run is handed a usage file of its own,
    /// made new and only this user's, in a directory no other user can
    /// enter, over one it inherited from the pane it was started from; a run
    /// of another agent is handed an empty value over the inherited one.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_pi_or_opencode_run_gets_its_own_usage_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("state");
        // A directory left open to others is narrowed before it is used.
        std::fs::create_dir_all(dir.join(USAGE_DIR)).unwrap();
        std::fs::set_permissions(dir.join(USAGE_DIR), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let telemetry = Arc::new(Telemetry::start_with(
            &env(&ON),
            Settings {
                config_dir: Some(dir.clone()),
                ..Settings::default()
            },
        ));
        let inherited = vec![(
            USAGE_FILE_ENV.to_string(),
            "/parent.usage.jsonl".to_string(),
        )];
        let usage_dir = std::fs::canonicalize(dir.join(USAGE_DIR)).unwrap();
        for agent in [AgentType::Pi, AgentType::OpenCode, AgentType::ClaudeCode] {
            let start = RunStart {
                agent: agent.clone(),
                ..run_start("p1")
            };
            let run = telemetry.prepare_run(start, &inherited, None).unwrap();
            let file = run
                .env
                .iter()
                .rev()
                .find(|(k, _)| k == USAGE_FILE_ENV)
                .map(|(_, v)| v.clone());
            if agent == AgentType::ClaudeCode {
                assert_eq!(file.as_deref(), Some(""));
                continue;
            }
            let file = PathBuf::from(file.unwrap());
            let lease = &telemetry
                .usage_lease
                .with_held(|lease| lease.token.clone())
                .unwrap();
            assert_eq!(
                file,
                usage_dir.join(format!("{}-{lease}.usage.jsonl", run.run_id)),
                "{agent:?}"
            );
            // The daemon's lease is there under its own name, and held.
            let lease_file = usage_dir.join(format!("{lease}.lease"));
            assert!(lease_held(&lease_file), "{agent:?}");
            let meta = std::fs::symlink_metadata(&file).unwrap();
            assert!(meta.is_file() && meta.len() == 0, "{agent:?}");
            assert_eq!(meta.permissions().mode() & 0o777, 0o600, "{agent:?}");
            // A run whose agent never started leaves no file behind.
            drop(run);
            assert!(!file.exists(), "{agent:?}");
        }
        let mode = std::fs::metadata(dir.join(USAGE_DIR))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    /// PRD #1858: a usage directory that is a symlink, or another user's,
    /// is not used: the run gets an empty value, never a path into it.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_unsafe_usage_directory_gives_no_usage_file() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("state");
        std::fs::create_dir_all(&dir).unwrap();
        let elsewhere = home.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, dir.join(USAGE_DIR)).unwrap();
        let telemetry = Arc::new(Telemetry::start_with(
            &env(&ON),
            Settings {
                config_dir: Some(dir.clone()),
                ..Settings::default()
            },
        ));
        let start = RunStart {
            agent: AgentType::Pi,
            ..run_start("p1")
        };
        let run = telemetry.prepare_run(start, &[], None).unwrap();
        assert_eq!(get(&run.env, USAGE_FILE_ENV), Some(""));
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
    }

    /// Whether another open file description holds `path`'s lock.
    #[cfg(unix)]
    fn lease_held(path: &Path) -> bool {
        use std::os::fd::AsRawFd as _;
        let file = std::fs::File::open(path).unwrap();
        // SAFETY: `file` is open.
        let free = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
        !free
    }

    /// PRD #1858: the usage files a daemon that ended without reading them
    /// left are removed as the next daemon starts, with their lease, when
    /// that lease's lock is free; a lease held through another open file
    /// description — another daemon's, whichever process-id namespace it is
    /// in, and whatever its process id is here — keeps its files. A file
    /// with no lease file, or not named as the deck names them, is kept. A
    /// lease file still being made is not a lease: it goes only once it is
    /// [`ABANDONED_LEASE_AGE`] old and unlocked, and takes no usage file
    /// with it.
    #[cfg(unix)]
    #[test]
    fn leftover_usage_files_go_unless_their_lease_is_held() {
        let home = tempfile::tempdir().unwrap();
        let usage = home.path().join(USAGE_DIR);
        std::fs::create_dir_all(&usage).unwrap();
        let (ended, running, unleased) = (new_run_id(), new_run_id(), new_run_id());
        let (abandoned, making, stuck) = (new_run_id(), new_run_id(), new_run_id());
        let file = |lease: &str| format!("{}-{lease}.usage.jsonl", new_run_id());
        let gone = [
            file(&ended),
            file(&ended),
            format!("{ended}.lease"),
            format!("{abandoned}.lease-new"),
        ];
        let kept = [
            file(&running),
            format!("{running}.lease"),
            file(&unleased),
            "notes.txt".to_string(),
            "x.usage.jsonl".to_string(),
            format!("x-{}.usage.jsonl", std::process::id()),
            format!("{making}.lease-new"),
            file(&making),
            format!("{stuck}.lease-new"),
            file(&abandoned),
        ];
        for name in gone.iter().chain(&kept) {
            std::fs::write(usage.join(name), "{}").unwrap();
        }
        let old = std::time::SystemTime::now() - ABANDONED_LEASE_AGE - Duration::from_secs(60);
        for lease in [&abandoned, &stuck] {
            std::fs::File::options()
                .write(true)
                .open(usage.join(format!("{lease}.lease-new")))
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        // Another daemon's lock on its lease, and on an old lease file it is
        // still making: open file descriptions this cleanup does not share.
        let held = lock(&usage.join(format!("{running}.lease")));
        let _stuck = lock(&usage.join(format!("{stuck}.lease-new")));
        remove_leftover_usage_files(home.path());
        for name in &gone {
            assert!(!usage.join(name).exists(), "left over: {name}");
        }
        for name in &kept {
            assert!(usage.join(name).exists(), "removed: {name}");
        }
        // Once that daemon has ended, its files go too.
        drop(held);
        remove_leftover_usage_files(home.path());
        assert!(!usage.join(&kept[0]).exists());
        assert!(!usage.join(&kept[1]).exists());
        assert!(usage.join(&kept[2]).exists());
        remove_leftover_usage_files(&home.path().join("absent"));
    }

    /// An exclusive lock on `path` through an open file description of its
    /// own, as another daemon holds its lease.
    #[cfg(unix)]
    fn lock(path: &Path) -> std::fs::File {
        use std::os::fd::AsRawFd as _;
        let file = std::fs::File::open(path).unwrap();
        // SAFETY: `file` is open.
        assert_eq!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        file
    }

    /// The usage directory under `home`, made, and opened as the daemon
    /// opens it.
    #[cfg(unix)]
    fn usage_dir(home: &Path) -> TrustedDir {
        private_dir(home).unwrap()
    }

    /// The inode of the entry `name` in `dir`.
    #[cfg(unix)]
    fn inode(dir: &Path, name: &str) -> u64 {
        use std::os::unix::fs::MetadataExt as _;
        std::fs::symlink_metadata(dir.join(name)).unwrap().ino()
    }

    /// PRD #1858: a daemon whose lease file is made but not yet locked when
    /// another daemon starts and cleans up — the order two daemons starting
    /// together can take — still ends with a lease, and the cleanup removes
    /// no file of a lease that is held. Then a daemon whose file went before
    /// it locked it, as cleanup takes an abandoned one, takes another token
    /// and still ends with a lease.
    #[cfg(unix)]
    #[test]
    fn a_lease_being_made_survives_another_daemons_cleanup() {
        use std::os::unix::fs::MetadataExt as _;
        let home = tempfile::tempdir().unwrap();
        let dir = usage_dir(home.path());
        let usage = home.path().join(USAGE_DIR);
        let running = new_run_id();
        let live = format!("{}-{running}.usage.jsonl", new_run_id());
        for name in [format!("{running}.lease"), live.clone()] {
            std::fs::write(usage.join(name), "").unwrap();
        }
        let _running = lock(&usage.join(format!("{running}.lease")));

        let mut paused = Vec::new();
        let lease = take_usage_lease_with(&dir, lease_token, |token| {
            paused.push(token.to_string());
            remove_leftover_usage_files(home.path());
        })
        .expect("a lease");
        assert_eq!(paused, std::slice::from_ref(&lease.token));
        let name = format!("{}.lease", lease.token);
        assert!(lease_held(&usage.join(&name)));
        let held = std::fs::File::from(lease._held.try_clone().unwrap());
        assert_eq!(held.metadata().unwrap().ino(), inode(&usage, &name));
        assert!(!usage.join(format!("{}.lease-new", lease.token)).exists());
        assert!(usage.join(&live).exists());
        assert!(usage.join(format!("{running}.lease")).exists());
        // Its own files outlive the next daemon's cleanup too.
        let own = format!("{}-{}.usage.jsonl", new_run_id(), lease.token);
        std::fs::write(usage.join(&own), "").unwrap();
        remove_leftover_usage_files(home.path());
        assert!(usage.join(&own).exists() && usage.join(&live).exists());

        let mut first = true;
        let again = take_usage_lease_with(&dir, lease_token, |token| {
            if std::mem::take(&mut first) {
                dir.remove(locate::lease_file_name(token, false).as_ref());
            }
        })
        .expect("a lease under another token");
        assert!(lease_held(&usage.join(format!("{}.lease", again.token))));
    }

    /// PRD #1858: a token that names a lease already held — two daemons
    /// drawing the same one — never replaces that lease: the second daemon
    /// gives the token up, leaves nothing of it behind, and takes the next.
    /// With no other token it takes no lease at all, and neither does a
    /// daemon whose random source fails.
    #[cfg(unix)]
    #[test]
    fn a_repeated_token_never_replaces_a_held_lease() {
        use std::os::unix::fs::MetadataExt as _;
        let home = tempfile::tempdir().unwrap();
        let dir = usage_dir(home.path());
        let usage = home.path().join(USAGE_DIR);
        let (same, next) = (new_run_id(), new_run_id());
        let tokens = |tokens: Vec<String>| {
            let mut tokens = tokens.into_iter();
            move || tokens.next()
        };
        let first = take_usage_lease_with(&dir, tokens(vec![same.clone()]), |_| {}).unwrap();
        let name = format!("{same}.lease");
        let held = std::fs::File::from(first._held.try_clone().unwrap());
        assert_eq!(held.metadata().unwrap().ino(), inode(&usage, &name));

        let second =
            take_usage_lease_with(&dir, tokens(vec![same.clone(), next.clone()]), |_| {}).unwrap();
        assert_eq!(second.token, next);
        assert_eq!(held.metadata().unwrap().ino(), inode(&usage, &name));
        assert!(lease_held(&usage.join(&name)));
        assert!(!usage.join(format!("{same}.lease-new")).exists());

        assert!(take_usage_lease_with(&dir, tokens(vec![same.clone()]), |_| {}).is_none());
        assert!(take_usage_lease_with(&dir, || None, |_| unreachable!()).is_none());
        assert_eq!(held.metadata().unwrap().ino(), inode(&usage, &name));
        let mut names: Vec<_> = std::fs::read_dir(&usage)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        let mut want = vec![name, format!("{next}.lease")];
        want.sort();
        assert_eq!(names, want);
    }

    /// PRD #1858: a daemon that could not take its usage lease tries again on
    /// a later run's start once [`LEASE_RETRY_AFTER`] has passed, not before,
    /// and keeps the lease it then takes.
    #[cfg(unix)]
    #[test]
    fn a_failed_lease_is_taken_again_on_a_later_run() {
        let home = tempfile::tempdir().unwrap();
        let dir = usage_dir(home.path());
        let slot = LeaseSlot::default();
        let start = std::time::Instant::now();
        assert_eq!(slot.token(start, || None), None);
        assert_eq!(
            slot.token(start + LEASE_RETRY_AFTER / 2, || unreachable!()),
            None
        );
        let later = start + LEASE_RETRY_AFTER;
        let token = slot
            .token(later, || take_usage_lease(&dir))
            .expect("a lease");
        assert!(lease_held(
            &home.path().join(USAGE_DIR).join(format!("{token}.lease"))
        ));
        assert_eq!(
            slot.token(later + LEASE_RETRY_AFTER, || unreachable!()),
            Some(token)
        );
    }

    /// The Devin configurations the shim writes for a run go when that run's
    /// agent ends, and only that run's; a stopping daemon removes the rest.
    /// Without the shim no run is tracked.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_runs_configurations_go_with_its_agent() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("state");
        let mut telemetry = Telemetry::start_with(
            &env(&ON),
            Settings {
                config_dir: Some(dir.clone()),
                ..Settings::default()
            },
        );
        telemetry.set_shim_dir(dir.join("bin"));
        let telemetry = Arc::new(telemetry);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        let write = |run: &PreparedRun, pid: u32| {
            let path = dir.join(format!("devin-{}-{pid}.json", run.run_id));
            std::fs::write(&path, "{}").unwrap();
            path
        };
        let start = |pane: &str, agent: &str| {
            telemetry
                .prepare_run(generation(pane, agent), &[], None)
                .unwrap()
        };
        let first = start("p1", "a1");
        let first_files = [write(&first, 1), write(&first, 2)];
        first.commit();
        let second = start("p2", "a2");
        let second_file = write(&second, 1);
        second.commit();
        assert_eq!(telemetry.run_file_ids().len(), 2);
        telemetry.end_run("p1", Some("a-other"), EndReason::Exited, None);
        assert!(
            first_files.iter().all(|p| p.exists()),
            "another generation's end"
        );
        telemetry.end_run("p1", Some("a1"), EndReason::Exited, None);
        assert!(first_files.iter().all(|p| !p.exists()), "p1's went");
        assert!(second_file.exists(), "p2's stays");
        assert_eq!(telemetry.run_file_ids().len(), 1);

        telemetry.shutdown(Duration::from_secs(1)).await;
        assert!(!second_file.exists(), "shutdown removes the rest");
        assert!(dir.join("bin").is_dir(), "the shim directory stays");

        let plain = Arc::new(Telemetry::start(&env(&ON)));
        plain
            .prepare_run(run_start("p1"), &[], None)
            .unwrap()
            .commit();
        assert!(plain.run_file_ids().is_empty());
    }

    #[tokio::test]
    async fn grpc_propagates_but_exports_nothing() {
        let telemetry = Arc::new(Telemetry::start(&env(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:9"),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc"),
        ])));
        assert!(telemetry.is_enabled());
        assert!(!telemetry.is_exporting());
        let (tx, rx) = broadcast::channel(4);
        telemetry.subscribe(rx);
        drop(tx);
        let added = begin(&telemetry, run_start("p1"), &[]);
        assert!(added.iter().any(|(k, _)| k == "OTEL_RESOURCE_ATTRIBUTES"));
    }

    // ---- R1: lifecycle and broadcast in one order ----

    /// A fast agent's first status is broadcast — and read by the ingest task
    /// — before the spawn path commits its run. It is kept.
    #[tokio::test]
    async fn a_status_broadcast_before_the_commit_survives() {
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let (tx, events) = broadcast::channel(16);
        telemetry.subscribe(events);
        let prepared = telemetry
            .prepare_run(generation("p1", "a1"), &[], None)
            .unwrap();
        tx.send(of("p1", "a1", EventType::SessionStart)).unwrap();
        tx.send(of("p1", "a1", EventType::ToolStart)).unwrap();
        telemetry.settled().await;
        // The events have been applied, and the run is not committed yet.
        prepared.commit();
        telemetry.end_run("p1", Some("a1"), EndReason::Exited, None);
        telemetry.settled().await;
        let spans = captured(&mut rx);
        let seen = statuses(&spans);
        assert_eq!(
            seen.last(),
            Some(&AttrValue::Str("working".into())),
            "{seen:?}"
        );
        let run = spans.iter().find(|s| s.name == SPAN_AGENT_RUN).unwrap();
        assert_eq!(
            attr(run, AttrKey::StatusChangesCount),
            Some(&AttrValue::Int(seen.len() as i64))
        );
    }

    /// An agent's last status is broadcast just before its PTY closes, and the
    /// reader thread ends the run before the ingest task has read it. It is
    /// part of the run all the same.
    #[tokio::test]
    async fn a_status_broadcast_before_the_end_survives() {
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let (tx, events) = broadcast::channel(16);
        telemetry.subscribe(events);
        begin(&telemetry, generation("p1", "a1"), &[]);
        tx.send(of("p1", "a1", EventType::SessionStart)).unwrap();
        telemetry.settled().await;
        let before = statuses(&captured(&mut rx));
        tx.send(of("p1", "a1", EventType::ToolStart)).unwrap();
        tx.send(of("p1", "a1", EventType::Idle)).unwrap();
        // No await between the broadcasts and the end: the task has not run.
        telemetry.end_run("p1", Some("a1"), EndReason::Exited, None);
        telemetry.settled().await;
        let spans = captured(&mut rx);
        let mut seen = before;
        seen.extend(statuses(&spans));
        assert!(seen.contains(&AttrValue::Str("working".into())), "{seen:?}");
        assert_eq!(
            seen.last(),
            Some(&AttrValue::Str("idle".into())),
            "{seen:?}"
        );
        let run = spans.iter().find(|s| s.name == SPAN_AGENT_RUN).unwrap();
        assert_eq!(
            attr(run, AttrKey::EndReason),
            Some(&AttrValue::Str("exited".into()))
        );
    }

    /// The same, for daemon shutdown: events broadcast before it are applied
    /// before every run is ended.
    #[tokio::test]
    async fn a_status_broadcast_before_shutdown_survives() {
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let (tx, events) = broadcast::channel(16);
        telemetry.subscribe(events);
        begin(&telemetry, generation("p1", "a1"), &[]);
        tx.send(of("p1", "a1", EventType::SessionStart)).unwrap();
        tx.send(of("p1", "a1", EventType::ToolStart)).unwrap();
        telemetry.shutdown(Duration::from_secs(2)).await;
        let spans = captured(&mut rx);
        let seen = statuses(&spans);
        assert_eq!(
            seen.last(),
            Some(&AttrValue::Str("working".into())),
            "{seen:?}"
        );
        let run = spans.iter().find(|s| s.name == SPAN_AGENT_RUN).unwrap();
        assert_eq!(
            attr(run, AttrKey::EndReason),
            Some(&AttrValue::Str("daemon_shutdown".into()))
        );
    }

    /// A fast agent's PTY can end before the spawn path commits its run. The
    /// end is kept for the run, which opens and ends at once: one run, ended
    /// `exited`, and nothing of it left in the handle.
    #[tokio::test]
    async fn an_agent_that_exits_before_its_commit_leaves_no_run_open() {
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let (tx, events) = broadcast::channel(16);
        telemetry.subscribe(events);
        let prepared = telemetry
            .prepare_run(generation("p1", "a1"), &[], None)
            .unwrap();
        tx.send(of("p1", "a1", EventType::SessionStart)).unwrap();
        // Not yet registered when its PTY ended, so the seam reads `closed`.
        telemetry.end_run("p1", Some("a1"), EndReason::Closed, None);
        telemetry.settled().await;
        prepared.commit();
        telemetry.settled().await;
        let spans = captured(&mut rx);
        let runs: Vec<_> = spans.iter().filter(|s| s.name == SPAN_AGENT_RUN).collect();
        assert_eq!(runs.len(), 1, "{spans:?}");
        assert_eq!(
            attr(runs[0], AttrKey::EndReason),
            Some(&AttrValue::Str("exited".into()))
        );
        let shared = &telemetry.exporting.as_ref().unwrap().shared;
        assert_eq!(shared.tracker.lock().unwrap().held(), (0, 0));
        assert!(shared.lock_filters().is_empty());
    }

    /// A spawn refused after `prepare_run` drops its `PreparedRun`: the run is
    /// forgotten, with the events it was holding.
    #[tokio::test]
    async fn a_refused_spawn_leaves_no_run() {
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let (tx, events) = broadcast::channel(16);
        telemetry.subscribe(events);
        let prepared = telemetry
            .prepare_run(generation("p1", "a1"), &[], None)
            .unwrap();
        tx.send(of("p1", "a1", EventType::SessionStart)).unwrap();
        telemetry.settled().await;
        drop(prepared);
        telemetry.settled().await;
        let shared = &telemetry.exporting.as_ref().unwrap().shared;
        assert_eq!(shared.tracker.lock().unwrap().held(), (0, 0));
        telemetry.shutdown(Duration::from_secs(2)).await;
        assert!(captured(&mut rx).is_empty());
    }

    /// R3 / R6: what the handle keeps per run — the tracker's runs and the
    /// input filters — is gone once the runs end.
    #[tokio::test]
    async fn many_runs_return_the_handle_to_its_baseline() {
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let (tx, events) = broadcast::channel(1024);
        telemetry.subscribe(events);
        for i in 0..100 {
            let pane = format!("pane-{i}");
            let agent = format!("agent-{i}");
            begin(&telemetry, generation(&pane, &agent), &[]);
            tx.send(of(&pane, &agent, EventType::SessionStart)).unwrap();
            telemetry.observe_client_input(&pane, b"\x1b[24;");
            telemetry.end_run(&pane, Some(&agent), EndReason::Exited, None);
            telemetry.settled().await;
            captured(&mut rx);
        }
        let shared = &telemetry.exporting.as_ref().unwrap().shared;
        assert_eq!(shared.tracker.lock().unwrap().held(), (0, 0));
        assert_eq!(shared.tracker.lock().unwrap().live_shadow_states(), 0);
        assert!(shared.lock_filters().is_empty());
    }

    /// The run span of `spans`, its intervention count and the times of its
    /// intervention events.
    fn interventions(spans: &[SpanRecord]) -> (Option<AttrValue>, Vec<u64>) {
        let run = spans.iter().find(|s| s.name == SPAN_AGENT_RUN).unwrap();
        let times = run
            .events
            .iter()
            .filter(|e| e.name == crate::telemetry::EVENT_INTERVENTION)
            .map(|e| e.time_unix_nanos)
            .collect();
        (attr(run, AttrKey::InterventionsCount).cloned(), times)
    }

    /// N2: pressing Escape alone and then nothing else — the way to interrupt
    /// an agent — is an intervention, whether the window passes before the
    /// run ends, the run ends inside the window, or the daemon shuts down.
    #[tokio::test]
    async fn the_escape_key_alone_is_an_intervention() {
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let shared = Arc::clone(&telemetry.exporting.as_ref().unwrap().shared);

        // The window passes while the run is idle: the ingest task settles it.
        // Its timer is not a command, so wait for the settlement itself — the
        // filter giving the ESC up, which happens with the tracker locked, in
        // one step with the run counting it.
        begin(&telemetry, generation("p1", "a1"), &[]);
        let written = now_unix_nanos();
        telemetry.observe_client_input_at("p1", b"\x1b", written, std::time::Instant::now());
        eventually("the ingest task settled the Escape", || {
            shared.lock_filters()["p1"]
                .iter()
                .all(|input| input.escape.is_none() && !input.filter.is_holding())
        })
        .await;
        telemetry.end_run("p1", Some("a1"), EndReason::Exited, None);
        telemetry.settled().await;
        let (count, times) = interventions(&captured(&mut rx));
        assert_eq!(count, Some(AttrValue::Int(1)));
        assert_eq!(times, vec![written], "counted at the time it was written");

        // The run ends inside the window.
        begin(&telemetry, generation("p1", "a2"), &[]);
        telemetry.observe_client_input("p1", b"\x1b");
        telemetry.end_run("p1", Some("a2"), EndReason::Exited, None);
        telemetry.settled().await;
        assert_eq!(interventions(&captured(&mut rx)).0, Some(AttrValue::Int(1)));

        // The daemon shuts down inside the window.
        begin(&telemetry, generation("p1", "a3"), &[]);
        telemetry.observe_client_input("p1", b"\x1b");
        telemetry.shutdown(Duration::from_secs(2)).await;
        assert_eq!(interventions(&captured(&mut rx)).0, Some(AttrValue::Int(1)));
    }

    /// N7: the daemon's pane-close broadcast ends the run while a lone ESC is
    /// still in its window — read by the ingest task before the reader
    /// thread's `end_run`, or already broadcast when `end_run` is called — and
    /// a new generation replacing the run does the same. The Escape is counted
    /// in the run it was pressed in, at the time it was written, every time.
    #[tokio::test]
    async fn the_escape_key_counts_however_the_run_ends() {
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let shared = Arc::clone(&telemetry.exporting.as_ref().unwrap().shared);
        let (tx, events) = broadcast::channel(16);
        telemetry.subscribe(events);
        let run_of = |spans: &[SpanRecord], reason: &str| {
            let runs: Vec<_> = spans.iter().filter(|s| s.name == SPAN_AGENT_RUN).collect();
            assert_eq!(runs.len(), 1, "one run span");
            assert_eq!(
                attr(runs[0], AttrKey::EndReason),
                Some(&AttrValue::Str(reason.into()))
            );
            interventions(spans)
        };

        // The close broadcast is read first; the reader's end_run comes later.
        begin(&telemetry, generation("p1", "a1"), &[]);
        telemetry.settled().await;
        let written = now_unix_nanos();
        telemetry.observe_client_input_at("p1", b"\x1b", written, window_held_open());
        tx.send(pane_closed("p1", "a1")).unwrap();
        telemetry.settled().await;
        let (count, times) = run_of(&captured(&mut rx), "closed");
        assert_eq!(count, Some(AttrValue::Int(1)));
        assert_eq!(times, vec![written]);
        telemetry.end_run("p1", Some("a1"), EndReason::Exited, None);
        telemetry.settled().await;
        assert!(captured(&mut rx).is_empty(), "the run ended once");

        // The close is already broadcast, unread, when end_run is called.
        begin(&telemetry, generation("p1", "a2"), &[]);
        telemetry.settled().await;
        let written = now_unix_nanos();
        telemetry.observe_client_input_at("p1", b"\x1b", written, window_held_open());
        tx.send(pane_closed("p1", "a2")).unwrap();
        // No await in between: the ingest task has read neither.
        telemetry.end_run("p1", Some("a2"), EndReason::Exited, None);
        telemetry.settled().await;
        let (count, times) = run_of(&captured(&mut rx), "closed");
        assert_eq!(count, Some(AttrValue::Int(1)));
        assert_eq!(times, vec![written]);

        // A new generation replaces the run before its end is seen: the run
        // waits for its own end.
        begin(&telemetry, generation("p1", "a3"), &[]);
        telemetry.settled().await;
        telemetry.observe_client_input_at("p1", b"\x1b", now_unix_nanos(), window_held_open());
        begin(&telemetry, generation("p1", "a4"), &[]);
        telemetry.settled().await;
        assert!(
            captured(&mut rx).is_empty(),
            "the replaced run is still open"
        );

        // Its close, read late, ends it with its Escape, and leaves the newer
        // run's Escape where it is.
        telemetry.observe_client_input_at("p1", b"\x1b", now_unix_nanos(), window_held_open());
        tx.send(pane_closed("p1", "a3")).unwrap();
        telemetry.settled().await;
        let (count, _) = run_of(&captured(&mut rx), "closed");
        assert_eq!(count, Some(AttrValue::Int(1)));
        assert!(shared.lock_filters()["p1"][0].escape.is_some());
        telemetry.end_run("p1", Some("a3"), EndReason::Exited, None);
        telemetry.settled().await;
        assert!(captured(&mut rx).is_empty(), "the replaced run ended once");
        telemetry.end_run("p1", Some("a4"), EndReason::Exited, None);
        telemetry.settled().await;
        let (count, _) = run_of(&captured(&mut rx), "exited");
        assert_eq!(count, Some(AttrValue::Int(1)));
        assert!(shared.lock_filters().is_empty());
    }

    /// N10: input already accepted from a person is counted in its run when
    /// the pane's close broadcast is applied before the notice of it — an
    /// ordinary key, and an Escape the writer settled as its window passed —
    /// and counted once, whichever of the notice and the close comes first.
    #[tokio::test]
    async fn accepted_input_counts_when_the_close_overtakes_it() {
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let shared = Arc::clone(&telemetry.exporting.as_ref().unwrap().shared);
        let (tx, events) = broadcast::channel(16);
        telemetry.subscribe(events);
        let closed = |spans: &[SpanRecord]| {
            let runs: Vec<_> = spans.iter().filter(|s| s.name == SPAN_AGENT_RUN).collect();
            assert_eq!(runs.len(), 1, "one run span");
            assert_eq!(
                attr(runs[0], AttrKey::EndReason),
                Some(&AttrValue::Str("closed".into()))
            );
            interventions(spans)
        };

        // A key, then the close, with no await between: the ingest task reads
        // the close while draining the broadcast for the input's notice.
        begin(&telemetry, generation("p1", "a1"), &[]);
        telemetry.settled().await;
        let written = now_unix_nanos();
        telemetry.observe_client_input_at("p1", b"a", written, std::time::Instant::now());
        tx.send(pane_closed("p1", "a1")).unwrap();
        telemetry.settled().await;
        let (count, times) = closed(&captured(&mut rx));
        assert_eq!(count, Some(AttrValue::Int(1)));
        assert_eq!(times, vec![written]);
        telemetry.end_run("p1", Some("a1"), EndReason::Exited, None);
        telemetry.settled().await;
        assert!(captured(&mut rx).is_empty(), "the run ended once");
        assert!(shared.lock_filters().is_empty());

        // An Escape the next write finds past its window — a focus report, no
        // key — is settled by the writer into the same waiting input.
        begin(&telemetry, generation("p1", "a2"), &[]);
        telemetry.settled().await;
        let at = window_held_open();
        let written = now_unix_nanos();
        telemetry.observe_client_input_at("p1", b"\x1b", written, at);
        telemetry.observe_client_input_at("p1", b"\x1b[I", written + 1, at + LONE_ESCAPE_WINDOW);
        assert!(
            shared.lock_filters()["p1"][0].escape.is_none(),
            "the writer settled the Escape"
        );
        tx.send(pane_closed("p1", "a2")).unwrap();
        telemetry.settled().await;
        let (count, times) = closed(&captured(&mut rx));
        assert_eq!(count, Some(AttrValue::Int(1)));
        assert_eq!(times, vec![written]);

        // The notice applied before the close: counted then, not again as the
        // run ends.
        begin(&telemetry, generation("p1", "a3"), &[]);
        telemetry.settled().await;
        telemetry.observe_client_input("p1", b"a");
        telemetry.settled().await;
        tx.send(pane_closed("p1", "a3")).unwrap();
        telemetry.settled().await;
        assert_eq!(closed(&captured(&mut rx)).0, Some(AttrValue::Int(1)));
        assert!(shared.lock_filters().is_empty());
    }

    /// N12: a timer the ingest task picked before a new run's Commit was sent
    /// leaves that run's input alone — a key, and an Escape already past its
    /// window — and the Escape is still settled once the run opens, before
    /// anything else touches the run.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_timer_picked_before_a_commit_leaves_the_new_runs_input_alone() {
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let shared = Arc::clone(&telemetry.exporting.as_ref().unwrap().shared);
        begin(&telemetry, generation("p0", "old"), &[]);
        telemetry.settled().await;

        // An Escape on another pane takes the task into its timer branch,
        // where it waits.
        let (entered, picked) = oneshot::channel();
        let (resume, paused) = oneshot::channel();
        *shared.timer_gate.lock().unwrap() = Some((entered, paused));
        telemetry.observe_client_input("p0", b"\x1b");
        picked.await.unwrap();

        // A new run is committed and typed into, its Escape already past its
        // window, while the Commit waits behind the branch.
        begin(&telemetry, generation("p1", "new"), &[]);
        let at = std::time::Instant::now() - LONE_ESCAPE_WINDOW;
        let written = now_unix_nanos();
        telemetry.observe_client_input_at("p1", b"a", written, at);
        telemetry.observe_client_input_at("p1", b"\x1b", written + 6_000_000_000, at);
        resume.send(()).unwrap();

        eventually("the ingest task settled the new run's Escape", || {
            shared.lock_filters()["p1"]
                .iter()
                .all(|input| input.escape.is_none() && input.pending.is_empty())
        })
        .await;
        telemetry.end_run("p1", Some("new"), EndReason::Exited, None);
        telemetry.settled().await;
        let (count, times) = interventions(&captured(&mut rx));
        assert_eq!(count, Some(AttrValue::Int(2)));
        assert_eq!(times, vec![written, written + 6_000_000_000]);
    }

    /// The kinds of the intervention events on `spans`' run span.
    fn intervention_kinds(spans: &[SpanRecord]) -> Vec<AttrValue> {
        let run = spans.iter().find(|s| s.name == SPAN_AGENT_RUN).unwrap();
        run.events
            .iter()
            .filter(|e| e.name == crate::telemetry::EVENT_INTERVENTION)
            .filter_map(|e| {
                e.attributes
                    .iter()
                    .find(|a| a.key == AttrKey::InterventionKind)
                    .map(|a| a.value.clone())
            })
            .collect()
    }

    /// Input is written, then a status change is broadcast and applied before
    /// the ingest task takes the input: the input's kind follows the prompt
    /// in force when it was written, in both directions.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_status_applied_before_the_input_is_taken_does_not_change_its_kind() {
        // Stamped when sent, as a producer stamps them.
        let now = |agent: &str, event_type| {
            let mut msg = of("p1", agent, event_type);
            if let BroadcastMsg::Event(e) = &mut msg {
                e.timestamp = chrono::Utc::now();
            }
            msg
        };
        for (agent, before, after, expected) in [
            (
                "a1",
                EventType::PermissionRequest,
                EventType::Thinking,
                "answer",
            ),
            (
                "a2",
                EventType::Thinking,
                EventType::PermissionRequest,
                "text",
            ),
        ] {
            let (telemetry, mut rx) = capturing(ProbeSettings::default());
            let shared = Arc::clone(&telemetry.exporting.as_ref().unwrap().shared);
            let (tx, events) = broadcast::channel(16);
            telemetry.subscribe(events);
            begin(&telemetry, generation("p0", "other"), &[]);
            begin(&telemetry, generation("p1", agent), &[]);
            tx.send(now(agent, EventType::SessionStart)).unwrap();
            tx.send(now(agent, before.clone())).unwrap();
            telemetry.settled().await;

            // An Escape on another pane holds the ingest task in its timer
            // branch while the key is written and the status broadcast.
            let (entered, picked) = oneshot::channel();
            let (resume, paused) = oneshot::channel();
            *shared.timer_gate.lock().unwrap() = Some((entered, paused));
            telemetry.observe_client_input("p0", b"\x1b");
            picked.await.unwrap();
            telemetry.observe_client_input("p1", b"y");
            tx.send(now(agent, after.clone())).unwrap();
            resume.send(()).unwrap();

            telemetry.settled().await;
            telemetry.end_run("p1", Some(agent), EndReason::Exited, None);
            telemetry.settled().await;
            assert_eq!(
                intervention_kinds(&captured(&mut rx)),
                vec![AttrValue::Str(expected.into())],
                "{before:?} then {after:?}"
            );
        }
    }

    /// The receiver handed to [`Telemetry::subscribe`] already has a prompt
    /// opening waiting on it, which the ingest task reads first and numbers
    /// from 0; input is written after it and before the prompt closes, all of
    /// it applied in one pass. The input is an answer: the input clock starts
    /// after the waiting messages, where the ingest task's numbering of the
    /// next one does, so the input is counted after the prompt opened and
    /// before it closed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn input_after_messages_waiting_at_subscribe_is_placed_among_them() {
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        begin(&telemetry, generation("p0", "other"), &[]);
        begin(&telemetry, generation("p1", "a1"), &[]);
        let (tx, events) = broadcast::channel(16);
        tx.send(of("p1", "a1", EventType::SessionStart)).unwrap();
        tx.send(of("p1", "a1", EventType::PermissionRequest))
            .unwrap();
        telemetry.settled().await;

        let resume = hold_ingest(&telemetry, "p0").await;
        telemetry.subscribe(events);
        telemetry.observe_client_input("p1", b"y");
        tx.send(of("p1", "a1", EventType::Thinking)).unwrap();
        resume.send(()).unwrap();

        telemetry.settled().await;
        telemetry.end_run("p1", Some("a1"), EndReason::Exited, None);
        telemetry.settled().await;
        assert_eq!(
            intervention_kinds(&captured(&mut rx)),
            vec![AttrValue::Str("answer".into())]
        );
    }

    /// A paste whose end marker never comes is read as ended once
    /// [`PASTE_IDLE_WINDOW`] passes with no input: what follows is its own
    /// kind. Writes that keep coming within the window keep the paste going,
    /// however long it runs in all.
    #[tokio::test]
    async fn a_paste_with_no_input_for_its_window_is_read_as_ended() {
        for (agent, gap, expected) in [
            ("a1", PASTE_IDLE_WINDOW, ["paste", "text", "interrupt"]),
            (
                "a2",
                PASTE_IDLE_WINDOW - Duration::from_millis(1),
                ["paste", "paste", "paste"],
            ),
        ] {
            let (telemetry, mut rx) = capturing(ProbeSettings::default());
            begin(&telemetry, generation("p1", agent), &[]);
            telemetry.settled().await;
            let at = std::time::Instant::now();
            // Written in separate bursts, so each write's kind is its own.
            let written = now_unix_nanos();
            let apart = 2 * crate::telemetry::tracker::INTERVENTION_COALESCE_NANOS;
            telemetry.observe_client_input_at("p1", b"\x1b[200~abc", written, at);
            telemetry.observe_client_input_at("p1", b"def", written + apart, at + gap);
            telemetry.observe_client_input_at("p1", b"\x03", written + 2 * apart, at + 2 * gap);
            telemetry.settled().await;
            telemetry.end_run("p1", Some(agent), EndReason::Exited, None);
            telemetry.settled().await;
            assert_eq!(
                intervention_kinds(&captured(&mut rx)),
                expected.map(|kind| AttrValue::Str(kind.into())),
                "{gap:?}"
            );
        }
    }

    /// The intervention events on `spans`' run span: when, and of what kind.
    fn timed_kinds(spans: &[SpanRecord]) -> Vec<(u64, AttrValue)> {
        let run = spans.iter().find(|s| s.name == SPAN_AGENT_RUN).unwrap();
        run.events
            .iter()
            .filter(|e| e.name == crate::telemetry::EVENT_INTERVENTION)
            .map(|e| {
                let kind = e
                    .attributes
                    .iter()
                    .find(|a| a.key == AttrKey::InterventionKind);
                (e.time_unix_nanos, kind.unwrap().value.clone())
            })
            .collect()
    }

    /// Holds the ingest task in its timer branch, by an Escape on `pane`,
    /// until the returned sender is sent to; returns once it is held.
    async fn hold_ingest(telemetry: &Telemetry, pane: &str) -> oneshot::Sender<()> {
        let shared = &telemetry.exporting.as_ref().unwrap().shared;
        let (entered, picked) = oneshot::channel();
        let (resume, paused) = oneshot::channel();
        *shared.timer_gate.lock().unwrap() = Some((entered, paused));
        telemetry.observe_client_input(pane, b"\x1b");
        picked.await.unwrap();
        resume
    }

    /// A prompt opens, closes, and opens again with a producer stamp behind
    /// both; the person types after it is broadcast, and the ingest task
    /// applies it in the same pass as the keystroke. The keystroke came after
    /// the prompt opened, on the order the deck recorded them in, so it is an
    /// answer, whatever the producer stamped.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_prompt_reopened_with_an_older_stamp_is_open_for_input_after_it() {
        let stamped = |event_type, seconds_ago: i64| {
            let mut msg = of("p1", "a1", event_type);
            if let BroadcastMsg::Event(e) = &mut msg {
                e.timestamp = chrono::Utc::now() - chrono::Duration::seconds(seconds_ago);
            }
            msg
        };
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let (tx, events) = broadcast::channel(16);
        telemetry.subscribe(events);
        begin(&telemetry, generation("p0", "other"), &[]);
        begin(&telemetry, generation("p1", "a1"), &[]);
        tx.send(stamped(EventType::SessionStart, 0)).unwrap();
        tx.send(stamped(EventType::PermissionRequest, 0)).unwrap();
        tx.send(stamped(EventType::Thinking, 0)).unwrap();
        telemetry.settled().await;

        let resume = hold_ingest(&telemetry, "p0").await;
        tx.send(stamped(EventType::PermissionRequest, 60)).unwrap();
        telemetry.observe_client_input("p1", b"y");
        resume.send(()).unwrap();

        telemetry.settled().await;
        telemetry.end_run("p1", Some("a1"), EndReason::Exited, None);
        telemetry.settled().await;
        assert_eq!(
            intervention_kinds(&captured(&mut rx)),
            vec![AttrValue::Str("answer".into())]
        );
    }

    /// Text written at 1 s is still waiting when the deck's answer, accepted
    /// at 11 s, is applied in the same pass: the text counts first, at 1 s,
    /// and the answer at 11 s, as two interventions. Input at 12 s joins the
    /// answer's burst rather than starting one, since the last input never
    /// moves back to 1 s.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn input_waiting_when_a_deck_answer_is_applied_counts_before_it() {
        let second = 1_000_000_000;
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let (tx, events) = broadcast::channel(16);
        telemetry.subscribe(events);
        begin(&telemetry, generation("p0", "other"), &[]);
        begin(&telemetry, generation("p1", "a1"), &[]);
        tx.send(of("p1", "a1", EventType::SessionStart)).unwrap();
        telemetry.settled().await;
        let zero = now_unix_nanos() - 20 * second;

        let resume = hold_ingest(&telemetry, "p0").await;
        telemetry.observe_client_input_at("p1", b"x", zero + second, std::time::Instant::now());
        let mut answered = of("p1", "a1", EventType::Thinking);
        if let BroadcastMsg::Event(e) = &mut answered {
            e.metadata.insert(
                crate::event::QUESTION_ANSWERED_BY_DECK_METADATA_KEY.into(),
                crate::event::QUESTION_ANSWERED_BY_DECK_METADATA_VALUE.into(),
            );
            e.timestamp = chrono::DateTime::from_timestamp_nanos((zero + 11 * second) as i64);
        }
        tx.send(answered).unwrap();
        resume.send(()).unwrap();
        telemetry.settled().await;
        telemetry.observe_client_input_at(
            "p1",
            b"z",
            zero + 12 * second,
            std::time::Instant::now(),
        );

        telemetry.settled().await;
        telemetry.end_run("p1", Some("a1"), EndReason::Exited, None);
        telemetry.settled().await;
        assert_eq!(
            timed_kinds(&captured(&mut rx)),
            vec![
                (zero + second, AttrValue::Str("text".into())),
                (zero + 11 * second, AttrValue::Str("answer".into())),
            ]
        );
    }

    /// An Escape pressed at 1 s is still held when the deck's answer,
    /// accepted at 11 s, is applied, with its window over (`expired`) or
    /// still running: it counts first, as an interrupt at 1 s, and the answer
    /// at 11 s, as two interventions, rather than one interrupt at 11 s.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_escape_held_when_a_deck_answer_is_applied_counts_before_it() {
        let second = 1_000_000_000;
        for expired in [true, false] {
            let (telemetry, mut rx) = capturing(ProbeSettings::default());
            let (tx, events) = broadcast::channel(16);
            telemetry.subscribe(events);
            begin(&telemetry, generation("p0", "other"), &[]);
            begin(&telemetry, generation("p1", "a1"), &[]);
            tx.send(of("p1", "a1", EventType::SessionStart)).unwrap();
            telemetry.settled().await;
            let zero = now_unix_nanos() - 20 * second;

            let resume = hold_ingest(&telemetry, "p0").await;
            let pressed = if expired {
                std::time::Instant::now() - LONE_ESCAPE_WINDOW
            } else {
                std::time::Instant::now()
            };
            telemetry.observe_client_input_at("p1", b"\x1b", zero + second, pressed);
            let mut answered = of("p1", "a1", EventType::Thinking);
            if let BroadcastMsg::Event(e) = &mut answered {
                e.metadata.insert(
                    crate::event::QUESTION_ANSWERED_BY_DECK_METADATA_KEY.into(),
                    crate::event::QUESTION_ANSWERED_BY_DECK_METADATA_VALUE.into(),
                );
                e.timestamp = chrono::DateTime::from_timestamp_nanos((zero + 11 * second) as i64);
            }
            tx.send(answered).unwrap();
            resume.send(()).unwrap();

            telemetry.settled().await;
            telemetry.end_run("p1", Some("a1"), EndReason::Exited, None);
            telemetry.settled().await;
            assert_eq!(
                timed_kinds(&captured(&mut rx)),
                vec![
                    (zero + second, AttrValue::Str("interrupt".into())),
                    (zero + 11 * second, AttrValue::Str("answer".into())),
                ],
                "expired: {expired}"
            );
        }
    }

    /// An Escape pressed at 1 s is still held when a permission prompt
    /// broadcast after it is applied, with its window over or still running:
    /// the run records the interrupt before the status change the prompt
    /// made, at 1 s.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_escape_held_when_a_prompt_opens_counts_before_it() {
        let second = 1_000_000_000;
        for expired in [true, false] {
            let (telemetry, mut rx) = capturing(ProbeSettings::default());
            let (tx, events) = broadcast::channel(16);
            telemetry.subscribe(events);
            begin(&telemetry, generation("p0", "other"), &[]);
            begin(&telemetry, generation("p1", "a1"), &[]);
            tx.send(of("p1", "a1", EventType::SessionStart)).unwrap();
            telemetry.settled().await;
            let zero = now_unix_nanos() - 20 * second;

            let resume = hold_ingest(&telemetry, "p0").await;
            let pressed = if expired {
                std::time::Instant::now() - LONE_ESCAPE_WINDOW
            } else {
                std::time::Instant::now()
            };
            telemetry.observe_client_input_at("p1", b"\x1b", zero + second, pressed);
            tx.send(of("p1", "a1", EventType::PermissionRequest))
                .unwrap();
            resume.send(()).unwrap();

            telemetry.settled().await;
            telemetry.end_run("p1", Some("a1"), EndReason::Exited, None);
            telemetry.settled().await;
            let spans = captured(&mut rx);
            assert_eq!(
                timed_kinds(&spans),
                vec![(zero + second, AttrValue::Str("interrupt".into()))],
                "expired: {expired}"
            );
            let run = spans.iter().find(|s| s.name == SPAN_AGENT_RUN).unwrap();
            let names: Vec<_> = run
                .events
                .iter()
                .map(|e| e.name)
                .filter(|&name| {
                    name == crate::telemetry::EVENT_STATUS_CHANGE
                        || name == crate::telemetry::EVENT_INTERVENTION
                })
                .collect();
            assert_eq!(
                names,
                vec![
                    crate::telemetry::EVENT_STATUS_CHANGE,
                    crate::telemetry::EVENT_INTERVENTION,
                    crate::telemetry::EVENT_STATUS_CHANGE,
                ],
                "expired: {expired}"
            );
        }
    }

    /// Text typed at 1 s, then an Escape at 5.99 s still held when the deck's
    /// answer, accepted at 6.01 s, is applied; a second Escape at 6.02 s,
    /// written while the ingest task waits out the first one's window,
    /// decides the first as the Escape key. The first counts where it was
    /// pressed, before the answer: one interrupt from 1 s, rather than the
    /// text at 1 s and an interrupt at 6.01 s that the answer overtook.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_escape_decided_during_the_wait_for_it_counts_before_the_event() {
        let second = 1_000_000_000;
        let ms = 1_000_000;
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let (tx, events) = broadcast::channel(16);
        telemetry.subscribe(events);
        begin(&telemetry, generation("p0", "other"), &[]);
        begin(&telemetry, generation("p1", "a1"), &[]);
        tx.send(of("p1", "a1", EventType::SessionStart)).unwrap();
        telemetry.settled().await;
        let zero = now_unix_nanos() - 20 * second;
        let shared = &telemetry.exporting.as_ref().unwrap().shared;

        let resume = hold_ingest(&telemetry, "p0").await;
        let pressed = std::time::Instant::now();
        telemetry.observe_client_input_at("p1", b"a", zero + second, pressed);
        telemetry.observe_client_input_at("p1", b"\x1b", zero + 5990 * ms, pressed);
        let mut answered = of("p1", "a1", EventType::Thinking);
        if let BroadcastMsg::Event(e) = &mut answered {
            e.metadata.insert(
                crate::event::QUESTION_ANSWERED_BY_DECK_METADATA_KEY.into(),
                crate::event::QUESTION_ANSWERED_BY_DECK_METADATA_VALUE.into(),
            );
            e.timestamp = chrono::DateTime::from_timestamp_nanos((zero + 6010 * ms) as i64);
        }
        tx.send(answered).unwrap();
        let (entered, waiting) = oneshot::channel();
        let (go, paused) = oneshot::channel();
        *shared.wait_gate.lock().unwrap() = Some((entered, paused));
        resume.send(()).unwrap();
        waiting.await.unwrap();
        telemetry.observe_client_input_at(
            "p1",
            b"\x1b",
            zero + 6020 * ms,
            pressed + LONE_ESCAPE_WINDOW / 2,
        );
        go.send(()).unwrap();

        telemetry.settled().await;
        telemetry.end_run("p1", Some("a1"), EndReason::Exited, None);
        telemetry.settled().await;
        assert_eq!(
            timed_kinds(&captured(&mut rx)),
            vec![(zero + second, AttrValue::Str("interrupt".into()))]
        );
    }

    /// A lone `ESC` that a later write decides — as the Escape key, by
    /// another `ESC` or by its window passing, or as the start of a key's
    /// sequence — counts at the time and broadcast position it was written
    /// at; the rest of that write counts at its own. A sequence the next
    /// write leaves undecided counts when a later write decides it.
    #[test]
    fn a_decided_escape_keeps_the_time_and_position_it_was_written_at() {
        use InterventionKind::{Interrupt, Navigate, Text};
        let pane_input = || PaneInput {
            run_id: "r1".into(),
            agent_id: None,
            ended: false,
            filter: InputFilter::new(),
            escape: None,
            paste: None,
            pending: Vec::new(),
        };
        let timed = |human: Vec<Written>| -> Vec<_> {
            human.iter().map(|w| (w.at, w.kind, w.position)).collect()
        };
        let pressed = std::time::Instant::now();
        let soon = pressed + LONE_ESCAPE_WINDOW / 2;
        let late = pressed + LONE_ESCAPE_WINDOW;
        let cases: [(&[u8], _, Vec<_>, bool); 8] = [
            (b"\x1b", soon, vec![(1, Interrupt, 3)], true),
            (
                b"\x1bab",
                soon,
                vec![(1, Interrupt, 3), (2, Text, 5)],
                false,
            ),
            (b"x", soon, vec![(1, Navigate, 3)], false),
            (b"\r", soon, vec![(1, Text, 3)], false),
            (b"[Ahi", soon, vec![(1, Navigate, 3), (2, Text, 5)], false),
            (b"[I", soon, vec![], false),
            (b"x", late, vec![(1, Interrupt, 3), (2, Text, 5)], false),
            (b"\x1b", late, vec![(1, Interrupt, 3)], true),
        ];
        for (next, at, expected, held) in cases {
            let mut input = pane_input();
            assert_eq!(input.write(b"\x1b", 1, 3, pressed).0.len(), 0);
            let (human, still) = input.write(next, 2, 5, at);
            assert_eq!(timed(human), expected, "{next:?}");
            assert_eq!(still, held, "{next:?}");
            let escape = input
                .escape
                .map(|(_, written, position)| (written, position));
            assert_eq!(escape, held.then_some((2, 5)), "{next:?}");
        }

        let mut input = pane_input();
        input.write(b"\x1b", 1, 3, pressed);
        assert_eq!(timed(input.write(b"[1;", 2, 5, soon).0), vec![]);
        assert_eq!(
            timed(input.write(b"5A", 4, 7, soon).0),
            vec![(4, Navigate, 7)]
        );
    }

    /// The keys a held `ESC` can begin with bytes of their own after the one
    /// that decides it: an SS3 cursor key, Alt with a multibyte character, an
    /// X10 mouse report. With how many bytes after the `ESC` decide each.
    const KEYS_WITH_TAILS: [(&[u8], usize); 3] =
        [(b"OA", 1), ("\u{e9}".as_bytes(), 1), (b"[Mabc", 2)];

    /// A held `ESC`, then the rest of its key, alone or with a `z` after it,
    /// in two writes split at every byte. A key the first write decides
    /// counts once, at the `ESC`'s time and position, wherever the split
    /// falls inside it: the second write adds nothing for the rest of it. The
    /// `z` counts at the time of the write it is in. A key split before it is
    /// decided counts once, when the second write decides it, as the docs say.
    #[test]
    fn a_decided_key_counts_its_own_bytes_once() {
        use InterventionKind::{Navigate, Text};
        for ((key, decided), with_z) in KEYS_WITH_TAILS
            .into_iter()
            .flat_map(|key| [(key, false), (key, true)])
        {
            let mut rest = key.to_vec();
            if with_z {
                rest.push(b'z');
            }
            let z = |at, position| with_z.then_some((at, Text, position));
            let pressed = std::time::Instant::now();
            let soon = pressed + LONE_ESCAPE_WINDOW / 4;
            let sooner = pressed + LONE_ESCAPE_WINDOW / 2;
            for at in 0..=rest.len() {
                let mut input = PaneInput {
                    run_id: "r1".into(),
                    agent_id: None,
                    ended: false,
                    filter: InputFilter::new(),
                    escape: None,
                    paste: None,
                    pending: Vec::new(),
                };
                let timed = |human: Vec<Written>| -> Vec<_> {
                    human.iter().map(|w| (w.at, w.kind, w.position)).collect()
                };
                assert!(input.write(b"\x1b", 1, 3, pressed).0.is_empty());
                let first = timed(input.write(&rest[..at], 2, 5, soon).0);
                let second = timed(input.write(&rest[at..], 4, 7, sooner).0);
                let (want_first, want_second) = if at == 0 {
                    (vec![], [Some((1, Navigate, 3)), z(4, 7)].to_vec())
                } else if at < decided {
                    let kind = if with_z { Text } else { Navigate };
                    (vec![], vec![Some((4, kind, 7))])
                } else if at <= key.len() {
                    (vec![Some((1, Navigate, 3))], vec![z(4, 7)])
                } else {
                    (vec![Some((1, Navigate, 3)), z(2, 5)], vec![])
                };
                let want_first: Vec<_> = want_first.into_iter().flatten().collect();
                let want_second: Vec<_> = want_second.into_iter().flatten().collect();
                let what = format!("{rest:?} split at {at}");
                assert_eq!(first, want_first, "{what}: first write");
                assert_eq!(second, want_second, "{what}: second write");
                assert!(!input.filter.is_holding(), "{what}");
            }
        }
    }

    /// An `ESC` at 1 s, the rest of its key at 1.01 s and a `z` at 6.005 s:
    /// the key is one navigation from 1 s, so the `z`, more than the
    /// coalescing window after it, starts an intervention of its own. Counted
    /// again at 1.01 s, the key would pull the `z` into its burst as text.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_key_after_a_decided_one_starts_its_own_intervention() {
        let second = 1_000_000_000;
        let ms = 1_000_000;
        for (key, _) in KEYS_WITH_TAILS {
            let (telemetry, mut rx) = capturing(ProbeSettings::default());
            begin(&telemetry, generation("p0", "other"), &[]);
            begin(&telemetry, generation("p1", "a1"), &[]);
            telemetry.settled().await;
            let zero = now_unix_nanos() - 20 * second;

            // Held, so the ESC's window cannot pass on the ingest task's clock.
            let resume = hold_ingest(&telemetry, "p0").await;
            let pressed = std::time::Instant::now();
            telemetry.observe_client_input_at("p1", b"\x1b", zero + second, pressed);
            telemetry.observe_client_input_at(
                "p1",
                key,
                zero + 1010 * ms,
                pressed + LONE_ESCAPE_WINDOW / 10,
            );
            telemetry.observe_client_input_at(
                "p1",
                b"z",
                zero + 6005 * ms,
                pressed + Duration::from_millis(6005),
            );
            resume.send(()).unwrap();

            telemetry.settled().await;
            telemetry.end_run("p1", Some("a1"), EndReason::Exited, None);
            telemetry.settled().await;
            assert_eq!(
                timed_kinds(&captured(&mut rx)),
                vec![
                    (zero + second, AttrValue::Str("navigate".into())),
                    (zero + 6005 * ms, AttrValue::Str("text".into())),
                ],
                "{key:?}"
            );
        }
    }

    /// An `ESC` at 1 s, a permission prompt broadcast, then the rest of its
    /// key at 1.01 s: the key was pressed before the prompt opened, so it is
    /// navigation, not an answer. A `z` typed after the prompt, at 7 s, is
    /// the answer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_key_decided_after_a_prompt_keeps_the_kind_it_had_before_it() {
        let second = 1_000_000_000;
        let ms = 1_000_000;
        for (key, _) in KEYS_WITH_TAILS {
            let (telemetry, mut rx) = capturing(ProbeSettings::default());
            let (tx, events) = broadcast::channel(16);
            telemetry.subscribe(events);
            begin(&telemetry, generation("p0", "other"), &[]);
            begin(&telemetry, generation("p1", "a1"), &[]);
            tx.send(of("p1", "a1", EventType::SessionStart)).unwrap();
            telemetry.settled().await;
            let zero = now_unix_nanos() - 20 * second;

            let resume = hold_ingest(&telemetry, "p0").await;
            let pressed = std::time::Instant::now();
            telemetry.observe_client_input_at("p1", b"\x1b", zero + second, pressed);
            tx.send(of("p1", "a1", EventType::PermissionRequest))
                .unwrap();
            telemetry.observe_client_input_at(
                "p1",
                key,
                zero + 1010 * ms,
                pressed + LONE_ESCAPE_WINDOW / 10,
            );
            telemetry.observe_client_input_at(
                "p1",
                b"z",
                zero + 7 * second,
                pressed + Duration::from_secs(7),
            );
            resume.send(()).unwrap();

            telemetry.settled().await;
            telemetry.end_run("p1", Some("a1"), EndReason::Exited, None);
            telemetry.settled().await;
            assert_eq!(
                timed_kinds(&captured(&mut rx)),
                vec![
                    (zero + second, AttrValue::Str("navigate".into())),
                    (zero + 7 * second, AttrValue::Str("answer".into())),
                ],
                "{key:?}"
            );
        }
    }

    /// An `ESC` at 1 s, its key's deciding bytes at 1.005 s, a permission
    /// prompt broadcast, then the rest of the key at 1.01 s: the key was
    /// counted, as navigation, before the prompt opened, and the rest of it
    /// is not input of its own, so it does not answer the prompt. A `z` typed
    /// after the prompt, at 7 s, is the answer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_key_split_around_a_prompt_keeps_the_kind_it_had_before_it() {
        let second = 1_000_000_000;
        let ms = 1_000_000;
        for (key, decided) in KEYS_WITH_TAILS {
            let (telemetry, mut rx) = capturing(ProbeSettings::default());
            let (tx, events) = broadcast::channel(16);
            telemetry.subscribe(events);
            begin(&telemetry, generation("p0", "other"), &[]);
            begin(&telemetry, generation("p1", "a1"), &[]);
            tx.send(of("p1", "a1", EventType::SessionStart)).unwrap();
            telemetry.settled().await;
            let zero = now_unix_nanos() - 20 * second;

            let resume = hold_ingest(&telemetry, "p0").await;
            let pressed = std::time::Instant::now();
            telemetry.observe_client_input_at("p1", b"\x1b", zero + second, pressed);
            telemetry.observe_client_input_at(
                "p1",
                &key[..decided],
                zero + 1005 * ms,
                pressed + LONE_ESCAPE_WINDOW / 20,
            );
            tx.send(of("p1", "a1", EventType::PermissionRequest))
                .unwrap();
            telemetry.observe_client_input_at(
                "p1",
                &key[decided..],
                zero + 1010 * ms,
                pressed + LONE_ESCAPE_WINDOW / 10,
            );
            telemetry.observe_client_input_at(
                "p1",
                b"z",
                zero + 7 * second,
                pressed + Duration::from_secs(7),
            );
            resume.send(()).unwrap();

            telemetry.settled().await;
            telemetry.end_run("p1", Some("a1"), EndReason::Exited, None);
            telemetry.settled().await;
            assert_eq!(
                timed_kinds(&captured(&mut rx)),
                vec![
                    (zero + second, AttrValue::Str("navigate".into())),
                    (zero + 7 * second, AttrValue::Str("answer".into())),
                ],
                "{key:?}"
            );
        }
    }

    /// Messages already waiting on the ingest task's receiver when the clock
    /// is made are before it: input written then counts after all of them.
    #[test]
    fn the_clock_numbers_messages_queued_before_it() {
        let (tx, events) = broadcast::channel(16);
        for _ in 0..3 {
            tx.send(of("p1", "a1", EventType::Thinking)).unwrap();
        }
        let clock = BroadcastClock::numbering(&events);
        assert_eq!(clock.position(), 3);
        tx.send(of("p1", "a1", EventType::Thinking)).unwrap();
        // The ingest task numbers what it reads from 0: the fourth message,
        // number 3, is the first after input written at position 3.
        assert_eq!(clock.position(), 4);
        assert_eq!(events.len() as u64, clock.position());
    }

    /// Messages a small ring overwrote before the clock read them still
    /// count, as the ingest task counts the ones it missed, before and after
    /// the clock catches up.
    #[test]
    fn the_clock_counts_messages_a_small_ring_dropped() {
        let (tx, mut events) = broadcast::channel(4);
        let mut clock = BroadcastClock::numbering(&events);
        for _ in 0..10 {
            tx.send(of("p1", "a1", EventType::Thinking)).unwrap();
        }
        assert_eq!(clock.position(), 10);
        let mut read = 0;
        while let Ok(n) = match events.try_recv() {
            Ok(_) => Ok(1),
            Err(broadcast::error::TryRecvError::Lagged(missed)) => Ok(missed),
            Err(e) => Err(e),
        } {
            read += n;
        }
        assert_eq!(read, clock.position(), "the ingest task's count");

        for _ in 0..CLOCK_CATCH_UP_AT {
            tx.send(of("p1", "a1", EventType::Thinking)).unwrap();
        }
        let behind = 10 + CLOCK_CATCH_UP_AT as u64;
        assert_eq!(clock.position(), behind);
        clock.catch_up();
        assert_eq!(clock.position(), behind);
        assert_eq!(clock.base, behind, "read through the overwritten ones");
    }

    /// The clock reads nothing until it is [`CLOCK_CATCH_UP_AT`] behind, then
    /// reads all of it, and its position moves with neither.
    #[test]
    fn the_clock_catches_up_at_its_trigger_without_moving() {
        let (tx, events) = broadcast::channel(1024);
        let mut clock = BroadcastClock::numbering(&events);
        for _ in 0..CLOCK_CATCH_UP_AT - 1 {
            tx.send(of("p1", "a1", EventType::Thinking)).unwrap();
        }
        clock.catch_up();
        assert_eq!(clock.base, 0, "not behind enough to read");
        assert_eq!(clock.position(), CLOCK_CATCH_UP_AT as u64 - 1);
        tx.send(of("p1", "a1", EventType::Thinking)).unwrap();
        clock.catch_up();
        assert_eq!(clock.base, CLOCK_CATCH_UP_AT as u64);
        assert_eq!(clock.rx.as_ref().unwrap().len(), 0);
        assert_eq!(clock.position(), CLOCK_CATCH_UP_AT as u64);
        tx.send(of("p1", "a1", EventType::Thinking)).unwrap();
        assert_eq!(clock.position(), CLOCK_CATCH_UP_AT as u64 + 1);
    }

    /// With the ingest task held and hundreds of messages on another pane
    /// either side, text typed at 1 s, before a permission prompt is
    /// broadcast, and a key typed at 11 s, after it, with the clock catching
    /// up in between, are read against the prompt as it stood before each:
    /// text, then an answer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn input_either_side_of_a_clock_catch_up_is_read_against_the_prompt_before_it() {
        let second = 1_000_000_000;
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let (tx, events) = broadcast::channel(1024);
        telemetry.subscribe(events);
        begin(&telemetry, generation("p0", "other"), &[]);
        begin(&telemetry, generation("p1", "a1"), &[]);
        tx.send(of("p1", "a1", EventType::SessionStart)).unwrap();
        telemetry.settled().await;
        let zero = now_unix_nanos() - 20 * second;
        let shared = &telemetry.exporting.as_ref().unwrap().shared;
        let filler = |n: usize| {
            for _ in 0..n {
                tx.send(of("p2", "a2", EventType::Thinking)).unwrap();
            }
        };

        let resume = hold_ingest(&telemetry, "p0").await;
        filler(CLOCK_CATCH_UP_AT - 56);
        let now = std::time::Instant::now();
        telemetry.observe_client_input_at("p1", b"a", zero + second, now);
        tx.send(of("p1", "a1", EventType::PermissionRequest))
            .unwrap();
        filler(100);
        {
            let mut clock = shared.lock_clock();
            let before = clock.position();
            clock.catch_up();
            assert_eq!(clock.position(), before);
            assert_eq!(clock.rx.as_ref().unwrap().len(), 0, "caught up");
        }
        telemetry.observe_client_input_at("p1", b"b", zero + 11 * second, now);
        resume.send(()).unwrap();

        telemetry.settled().await;
        telemetry.end_run("p1", Some("a1"), EndReason::Exited, None);
        telemetry.settled().await;
        assert_eq!(
            timed_kinds(&captured(&mut rx)),
            vec![
                (zero + second, AttrValue::Str("text".into())),
                (zero + 11 * second, AttrValue::Str("answer".into())),
            ]
        );
    }

    /// Threads 4 / 10: a run's end is stamped when the ingest task applies
    /// it, after the statuses broadcast before it, so no status starts or ends
    /// after its run, nor does any event on the run.
    #[tokio::test]
    async fn no_status_outlives_its_run() {
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let (tx, events) = broadcast::channel(16);
        telemetry.subscribe(events);
        begin(&telemetry, generation("p1", "a1"), &[]);
        telemetry.settled().await;
        tx.send(of("p1", "a1", EventType::SessionStart)).unwrap();
        tx.send(of("p1", "a1", EventType::ToolStart)).unwrap();
        telemetry.end_run("p1", Some("a1"), EndReason::Exited, None);
        // The ingest task, on this thread, reads the broadcasts only after
        // this, a clock tick at least after the end_run call.
        std::thread::sleep(Duration::from_millis(2));
        telemetry.settled().await;
        let spans = captured(&mut rx);
        let run = spans.iter().find(|s| s.name == SPAN_AGENT_RUN).unwrap();
        let end = run.end_unix_nanos;
        assert!(!statuses(&spans).is_empty());
        for status in spans.iter().filter(|s| s.name == SPAN_AGENT_STATUS) {
            assert!(
                status.start_unix_nanos <= end && status.end_unix_nanos <= end,
                "{} .. {} past the run's end {end}",
                status.start_unix_nanos,
                status.end_unix_nanos
            );
        }
        for event in &run.events {
            assert!(event.time_unix_nanos <= end, "{} past {end}", event.name);
        }
    }

    /// PRD #1858: a run's final drain runs off the ingest task. While one
    /// run's usage record is held up — a slow disk — the ingest task goes on
    /// applying other runs' commands, the ended run's span waits for its
    /// drain, and is queued, usage and all, once the drain is done.
    #[tokio::test]
    async fn a_slow_final_drain_does_not_hold_up_other_runs() {
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let shared = Arc::clone(&telemetry.exporting.as_ref().unwrap().shared);
        begin(&telemetry, generation("p1", "a1"), &[]);
        telemetry.settled().await;
        let usage = shared.tracker.lock().unwrap().usage_to_poll();
        assert_eq!(usage.len(), 1);
        let held = usage[0].hold_readers();
        telemetry.end_run("p1", Some("a1"), EndReason::Closed, None);
        begin(&telemetry, generation("p2", "a2"), &[]);
        eventually("the next run was applied", || {
            shared.tracker.lock().unwrap().run_id("p2").is_some()
        })
        .await;
        let ended = |spans: &[SpanRecord]| {
            spans
                .iter()
                .filter(|s| s.name == SPAN_AGENT_RUN)
                .filter(|s| attr(s, AttrKey::EndReason).is_some())
                .count()
        };
        assert_eq!(ended(&captured(&mut rx)), 0, "queued before its drain");
        drop(held);
        telemetry.settled().await;
        let spans = captured(&mut rx);
        assert_eq!(ended(&spans), 1);
        let run = spans
            .iter()
            .find(|s| s.name == SPAN_AGENT_RUN && attr(s, AttrKey::EndReason).is_some())
            .unwrap();
        assert!(
            run.attributes.iter().any(|a| matches!(
                a.key,
                AttrKey::Accounting(crate::telemetry::accounting::AccountingKey::TokensProvenance)
            )),
            "the drain's usage is on the span"
        );
    }

    /// PRD #1858: a run whose final drain panics — a bug in a reader — is
    /// still sent, its usage incomplete, and the run ended with it in the
    /// same batch is sent with its usage as read.
    #[tokio::test]
    async fn a_drain_that_panics_loses_no_span() {
        use crate::telemetry::accounting::AccountingKey;
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let shared = Arc::clone(&telemetry.exporting.as_ref().unwrap().shared);
        begin(&telemetry, generation("p1", "a1"), &[]);
        begin(&telemetry, generation("p2", "a2"), &[]);
        telemetry.settled().await;
        let (failing, kept) = {
            let tracker = shared.tracker.lock().unwrap();
            tracker.usage_of("p1").unwrap().panic_in_finish();
            (
                tracker.run_id("p1").unwrap().to_string(),
                tracker.run_id("p2").unwrap().to_string(),
            )
        };
        let finishing = {
            let mut tracker = shared.tracker.lock().unwrap();
            tracker.end_all(EndReason::Closed, now_unix_nanos());
            let (finished, finishing) = tracker.take_finished_deferred();
            shared.enqueue(finished);
            finishing
        };
        assert_eq!(finishing.len(), 2);
        shared.complete_later(finishing);
        telemetry.settled().await;
        let spans = captured(&mut rx);
        let run = |id: &str| {
            spans
                .iter()
                .find(|s| {
                    s.name == SPAN_AGENT_RUN
                        && attr(s, AttrKey::EndReason).is_some()
                        && attr(s, AttrKey::RunId) == Some(&AttrValue::Str(id.to_string()))
                })
                .unwrap_or_else(|| panic!("no span for {id}"))
        };
        let reason = |id: &str| {
            attr(
                run(id),
                AttrKey::Accounting(AccountingKey::TokensUnavailableReason),
            )
            .cloned()
        };
        assert_eq!(
            reason(&failing),
            Some(AttrValue::Str("incomplete".to_string()))
        );
        assert_ne!(
            reason(&kept),
            Some(AttrValue::Str("incomplete".to_string()))
        );
        assert!(
            attr(
                run(&kept),
                AttrKey::Accounting(AccountingKey::TokensProvenance)
            )
            .is_some()
        );
    }

    /// A current-thread runtime whose only blocking thread is taken until
    /// this is dropped: the thread is let go first, then the runtime, so an
    /// assertion that fails inside it still lets the thread go.
    struct BlockingThreadTaken {
        _release: std::sync::mpsc::Sender<()>,
        runtime: tokio::runtime::Runtime,
    }

    impl BlockingThreadTaken {
        fn new() -> Self {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .max_blocking_threads(1)
                .enable_all()
                .build()
                .unwrap();
            let (release, released) = std::sync::mpsc::channel::<()>();
            let (taken, is_taken) = std::sync::mpsc::channel();
            runtime.spawn_blocking(move || {
                let _ = taken.send(());
                // Returns once the sender is dropped.
                let _ = released.recv();
            });
            is_taken.recv().unwrap();
            BlockingThreadTaken {
                _release: release,
                runtime,
            }
        }
    }

    /// A run of an arbitrary command, which the deck has no usage channel for.
    fn command_run(pane: &str, agent_id: &str) -> RunStart {
        RunStart {
            agent: AgentType::None,
            ..generation(pane, agent_id)
        }
    }

    /// Issue #1981 M6: a dispatched unit's run is named as it is prepared,
    /// so the name survives the run ending before anything else could name
    /// it. With the ingest task paused, the run is prepared in the unit's
    /// worktree and committed, its pane's close is broadcast, and the spawn
    /// ends without saying which generations it started — as when the close
    /// also removed the unit from the deck's record. Resumed, the ingest task
    /// takes the close before the commit, and the run still ends named; a run
    /// prepared outside the worktree does not.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dispatched_run_closed_before_ingest_keeps_its_name() {
        let (entered, polled_empty) = oneshot::channel();
        let (resume, paused) = oneshot::channel();
        let (sink, mut rx) = mpsc::channel(QUEUE_CAPACITY);
        let telemetry = Arc::new(Telemetry::start_with(
            &env(&ON),
            Settings {
                sink: Some(sink),
                command_gate: Some((entered, paused)),
                ..Settings::default()
            },
        ));
        polled_empty.await.unwrap();
        let (tx, events) = broadcast::channel(16);
        telemetry.subscribe(events);
        let naming = telemetry
            .begin_dispatch("My Unit: exact", Path::new("/w/my-unit"))
            .unwrap();
        for (pane, agent, cwd) in [("p1", "a1", "/w/my-unit"), ("p2", "a2", "/w/other")] {
            telemetry
                .prepare_run(command_run(pane, agent), &[], Some(PathBuf::from(cwd)))
                .unwrap()
                .commit();
            tx.send(pane_closed(pane, agent)).unwrap();
        }
        drop(naming);
        resume.send(()).unwrap();
        let mut spans = Vec::new();
        eventually("both runs' spans were queued", || {
            spans.extend(captured(&mut rx));
            ended_runs(&spans).len() == 2
        })
        .await;
        let run = |pane: &str| {
            *ended_runs(&spans)
                .iter()
                .find(|s| attr(s, AttrKey::PaneId) == Some(&AttrValue::Str(pane.into())))
                .unwrap_or_else(|| panic!("no run span for {pane}"))
        };
        assert_eq!(
            attr(run("p1"), AttrKey::EndReason),
            Some(&AttrValue::Str("closed".into()))
        );
        assert_eq!(
            attr(run("p1"), AttrKey::DispatchName),
            Some(&AttrValue::Str("My Unit: exact".into()))
        );
        assert_eq!(attr(run("p2"), AttrKey::DispatchName), None);
    }

    fn ended_runs(spans: &[SpanRecord]) -> Vec<&SpanRecord> {
        spans
            .iter()
            .filter(|s| s.name == SPAN_AGENT_RUN && attr(s, AttrKey::EndReason).is_some())
            .collect()
    }

    /// PRD #1858: a run the deck has no usage channel for — an arbitrary
    /// command, Devin — is completed as it ends, on no blocking thread. With
    /// the runtime's only one taken, two command runs end, the second after
    /// its pull request's checks went pending, failing, pending and passing,
    /// and both spans reach the sink with their unavailable usage, how they
    /// ended, and the second's four CI changes, two rounds and one failure.
    #[test]
    fn a_run_with_nothing_to_read_takes_no_blocking_thread() {
        use crate::telemetry::accounting::AccountingKey;
        use crate::telemetry::tracker::tests::checks_report;
        let taken = BlockingThreadTaken::new();
        taken.runtime.block_on(async {
            let (telemetry, mut rx) = capturing(ProbeSettings::default());
            let (tx, events) = broadcast::channel(16);
            telemetry.subscribe(events);
            begin(&telemetry, command_run("p1", "a1"), &[]);
            begin(&telemetry, command_run("p2", "a2"), &[]);
            tx.send(of("p2", "a2", EventType::SessionStart)).unwrap();
            for (revision, state, counts) in [
                (1, "pending", (0, 0, 0)),
                (2, "failing", (5, 1, 0)),
                (3, "pending", (5, 0, 0)),
                (4, "passing", (6, 0, 0)),
            ] {
                let mut report = checks_report("p2", 9, revision, state, counts);
                report.agent_id = Some("a2".to_string());
                tx.send(BroadcastMsg::Event(report)).unwrap();
            }
            telemetry.end_run("p1", Some("a1"), EndReason::Closed, None);
            telemetry.end_run("p2", Some("a2"), EndReason::Exited, Some(0));
            let mut spans = Vec::new();
            eventually("both runs' spans were queued", || {
                spans.extend(captured(&mut rx));
                ended_runs(&spans).len() == 2
            })
            .await;
            let run = |pane: &str| {
                *ended_runs(&spans)
                    .iter()
                    .find(|s| attr(s, AttrKey::PaneId) == Some(&AttrValue::Str(pane.into())))
                    .unwrap_or_else(|| panic!("no run span for {pane}"))
            };
            let text = |v: &str| Some(AttrValue::Str(v.to_string()));
            let accounting = |pane: &str, key| attr(run(pane), AttrKey::Accounting(key)).cloned();
            for (pane, ended) in [("p1", "interrupted"), ("p2", "completed")] {
                for key in [
                    AccountingKey::TokensUnavailableReason,
                    AccountingKey::CostUnavailableReason,
                    AccountingKey::QuotaUnavailableReason,
                ] {
                    assert_eq!(accounting(pane, key), text("no_channel"), "{pane} {key:?}");
                }
                assert_eq!(
                    accounting(pane, AccountingKey::Termination),
                    text(ended),
                    "{pane}"
                );
                assert_eq!(
                    accounting(pane, AccountingKey::TerminationProvenance),
                    text("measured"),
                    "{pane}"
                );
            }
            let changes = run("p2")
                .events
                .iter()
                .filter(|e| e.name == crate::telemetry::EVENT_CI_CHANGE)
                .count();
            assert_eq!(changes, 4);
            assert_eq!(
                attr(run("p2"), AttrKey::CiRoundsCount),
                Some(&AttrValue::Int(2))
            );
            assert_eq!(
                attr(run("p2"), AttrKey::CiFailuresCount),
                Some(&AttrValue::Int(1))
            );
        });
    }

    /// PRD #1858: a shutdown with no open run to read the usage of takes no
    /// blocking thread: with the runtime's only one taken, it ends the open
    /// command run, queues its span and the final flush, and returns.
    #[test]
    fn a_shutdown_with_nothing_to_read_takes_no_blocking_thread() {
        let taken = BlockingThreadTaken::new();
        taken.runtime.block_on(async {
            let (telemetry, mut rx) = capturing(ProbeSettings::default());
            begin(&telemetry, command_run("p1", "a1"), &[]);
            telemetry.settled().await;
            let mut shutting = tokio::spawn({
                let telemetry = Arc::clone(&telemetry);
                async move { telemetry.shutdown(Duration::from_secs(60)).await }
            });
            let mut spans = Vec::new();
            let mut flushed = false;
            loop {
                tokio::select! {
                    msg = rx.recv() => match msg {
                        Some(ExportMsg::Span(span)) => spans.push(*span),
                        Some(ExportMsg::Flush { ack, .. }) => {
                            let _ = ack.send(());
                            flushed = true;
                        }
                        None => break,
                    },
                    _ = &mut shutting => break,
                }
            }
            spans.extend(captured(&mut rx));
            assert!(flushed, "the final flush was queued");
            let reasons: Vec<_> = ended_runs(&spans)
                .iter()
                .filter_map(|s| attr(s, AttrKey::EndReason).cloned())
                .collect();
            assert_eq!(reasons, vec![AttrValue::Str("daemon_shutdown".into())]);
        });
    }

    /// Threads 2 / 9: shutdown with the export queue full — a backend slow
    /// enough to have left it so — waits for room within its budget for the
    /// final runs and the flush, instead of dropping both.
    #[tokio::test]
    async fn the_final_runs_wait_for_room_in_a_full_queue() {
        let (sink, mut rx) = mpsc::channel(1);
        let telemetry = Arc::new(Telemetry::start_with(
            &env(&ON),
            Settings {
                sink: Some(sink.clone()),
                ..Settings::default()
            },
        ));
        let shared = Arc::clone(&telemetry.exporting.as_ref().unwrap().shared);
        begin(&telemetry, generation("p1", "a1"), &[]);
        telemetry.settled().await;
        for span in one_span() {
            sink.send(ExportMsg::Span(Box::new(span))).await.unwrap();
        }
        let mut shutting = tokio::spawn({
            let telemetry = Arc::clone(&telemetry);
            async move { telemetry.shutdown(Duration::from_secs(60)).await }
        });
        eventually("the shutdown ended the run", || {
            shared.tracker.lock().unwrap().held() == (0, 0)
        })
        .await;
        // The exporter catches up only now.
        let mut spans = Vec::new();
        let mut flushed = false;
        let mut take = |msg: ExportMsg| match msg {
            ExportMsg::Span(span) => spans.push(*span),
            ExportMsg::Flush { ack, .. } => {
                let _ = ack.send(());
                flushed = true;
            }
        };
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(msg) => take(msg),
                    None => break,
                },
                _ = &mut shutting => break,
            }
        }
        while let Ok(msg) = rx.try_recv() {
            take(msg);
        }
        assert!(flushed, "the final flush was queued");
        let reasons: Vec<_> = spans
            .iter()
            .filter(|s| s.name == SPAN_AGENT_RUN)
            .filter_map(|s| attr(s, AttrKey::EndReason).cloned())
            .collect();
        assert!(
            reasons.contains(&AttrValue::Str("daemon_shutdown".into())),
            "{reasons:?}"
        );
    }

    /// Threads 2 / 9: a queue that stays full for the whole budget costs the
    /// final runs, and the log says so; the shutdown still keeps its budget.
    #[tokio::test]
    async fn a_queue_that_stays_full_at_shutdown_is_reported() {
        use tracing_subscriber::layer::SubscriberExt;
        let logs = Captured::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::new(
                "dot_agent_deck::telemetry=trace",
            ))
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(move || writer.clone()),
            );
        let _guard = crate::test_isolation::capture_tracing_on_this_thread(subscriber);

        let (sink, _never_read) = mpsc::channel(1);
        let telemetry = Arc::new(Telemetry::start_with(
            &env(&ON),
            Settings {
                sink: Some(sink.clone()),
                ..Settings::default()
            },
        ));
        begin(&telemetry, generation("p1", "a1"), &[]);
        telemetry.settled().await;
        for span in one_span() {
            sink.send(ExportMsg::Span(Box::new(span))).await.unwrap();
        }
        let started = std::time::Instant::now();
        telemetry.shutdown(Duration::from_millis(300)).await;
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        let logged = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        assert!(
            logged.contains("the export queue stayed full at shutdown"),
            "{logged}"
        );
    }

    /// N13: the ingest task can find the command queue empty, a new run be
    /// committed and a lone ESC written into it, and the task then take the
    /// write's wake before the run's Commit — `biased` orders the polls, not
    /// the Commit's arrival against the wake. The Escape is still settled by
    /// its window, with nothing else written and the run still open.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_escape_whose_wake_beats_its_runs_commit_still_settles() {
        let (entered, polled_empty) = oneshot::channel();
        let (resume, paused) = oneshot::channel();
        let (sink, mut rx) = mpsc::channel(QUEUE_CAPACITY);
        let telemetry = Arc::new(Telemetry::start_with(
            &env(&ON),
            Settings {
                sink: Some(sink),
                command_gate: Some((entered, paused)),
                ..Settings::default()
            },
        ));
        let shared = Arc::clone(&telemetry.exporting.as_ref().unwrap().shared);
        polled_empty.await.unwrap();
        begin(&telemetry, generation("p1", "new"), &[]);
        telemetry.observe_client_input("p1", b"\x1b");
        resume.send(()).unwrap();
        eventually("the Escape settled by its window", || {
            shared.lock_filters()["p1"]
                .iter()
                .all(|input| input.escape.is_none() && !input.filter.is_holding())
        })
        .await;
        assert!(shared.tracker.lock().unwrap().run_id("p1").is_some());
        telemetry.end_run("p1", Some("new"), EndReason::Exited, None);
        telemetry.settled().await;
        assert_eq!(interventions(&captured(&mut rx)).0, Some(AttrValue::Int(1)));
    }

    /// N11: an End that names no agent cleans up the run it ended and no
    /// other — not a newer generation's held Escape whose own End is queued
    /// behind it.
    #[tokio::test]
    async fn an_end_naming_no_agent_leaves_the_next_generation_alone() {
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let shared = Arc::clone(&telemetry.exporting.as_ref().unwrap().shared);
        begin(&telemetry, generation("p1", "a"), &[]);
        telemetry.settled().await;
        telemetry.observe_client_input_at("p1", b"\x1b", now_unix_nanos(), window_held_open());
        telemetry.end_run("p1", None, EndReason::Exited, None);
        begin(&telemetry, generation("p1", "b"), &[]);
        telemetry.observe_client_input_at("p1", b"\x1b", now_unix_nanos(), window_held_open());
        telemetry.end_run("p1", None, EndReason::Exited, None);
        telemetry.settled().await;
        let counts: Vec<_> = captured(&mut rx)
            .iter()
            .filter(|s| s.name == SPAN_AGENT_RUN)
            .map(|s| attr(s, AttrKey::InterventionsCount).cloned())
            .collect();
        assert_eq!(
            counts,
            vec![Some(AttrValue::Int(1)), Some(AttrValue::Int(1))]
        );
        assert!(shared.lock_filters().is_empty());
    }

    /// N2: Escape followed by another key. Inside the window the two are one
    /// sequence (Alt+key), pressed when the Escape was; past it the Escape
    /// counts at its own time, and the key joins its burst or starts a new
    /// one by the usual rule.
    #[tokio::test]
    async fn the_escape_key_then_another_key_counts_by_when_each_was_pressed() {
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let second = 1_000_000_000u64;
        let cases: [(Duration, i64); 3] = [
            // Alt+a: one intervention, at the Escape that began it.
            (Duration::from_millis(10), 1),
            // Past the window, inside a burst: one, at the Escape.
            (Duration::from_secs(1), 1),
            // Past a burst: two.
            (Duration::from_secs(6), 2),
        ];
        for (i, (later, count)) in cases.into_iter().enumerate() {
            let agent = format!("a{i}");
            begin(&telemetry, generation("p1", &agent), &[]);
            let t0 = 100 * second;
            let at = std::time::Instant::now();
            let t1 = t0 + later.as_nanos() as u64;
            telemetry.observe_client_input_at("p1", b"\x1b", t0, at);
            telemetry.observe_client_input_at("p1", b"a", t1, at + later);
            telemetry.end_run("p1", Some(&agent), EndReason::Exited, None);
            telemetry.settled().await;
            let (got, times) = interventions(&captured(&mut rx));
            assert_eq!(got, Some(AttrValue::Int(count)), "{later:?}");
            assert_eq!(times[0], t0, "{later:?}");
            if count == 2 {
                assert_eq!(times, vec![t0, t1]);
            }
        }

        // A report whose remainder arrives inside the window is still a report.
        begin(&telemetry, generation("p1", "r"), &[]);
        let at = std::time::Instant::now();
        telemetry.observe_client_input_at("p1", b"\x1b", 1, at);
        telemetry.observe_client_input_at("p1", b"[24;80R", 2, at + Duration::from_millis(5));
        telemetry.end_run("p1", Some("r"), EndReason::Exited, None);
        telemetry.settled().await;
        assert_eq!(interventions(&captured(&mut rx)).0, Some(AttrValue::Int(0)));
    }

    /// N3: with events published without pause — faster than the task applies
    /// them, so it also lags — an End and a Shutdown still take effect in
    /// bounded time.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_producer_that_never_stops_does_not_hold_a_command_back() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        // Acknowledge the final flush, so the shutdown returns once it has
        // been applied rather than at the end of its budget.
        let (spans_tx, mut spans_rx) = mpsc::unbounded_channel();
        let exporter = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                match msg {
                    ExportMsg::Span(span) => {
                        let _ = spans_tx.send(*span);
                    }
                    ExportMsg::Flush { ack, .. } => {
                        let _ = ack.send(());
                    }
                }
            }
        });
        let (tx, events) = broadcast::channel(8);
        telemetry.subscribe(events);
        begin(&telemetry, generation("p1", "a1"), &[]);
        begin(&telemetry, generation("p2", "a2"), &[]);
        // Runs whose agents are still starting: every event is checked against
        // each, which makes applying one far slower than publishing one.
        let starting: Vec<PreparedRun> = (0..20_000)
            .map(|i| {
                let id = format!("s{i}");
                telemetry
                    .prepare_run(generation(&id, &id), &[], None)
                    .unwrap()
            })
            .collect();
        telemetry.settled().await;
        let stop = Arc::new(AtomicBool::new(false));
        let running = Arc::new(AtomicUsize::new(0));
        // One status, over and over: no status change, so no span floods the
        // export queue.
        let msg = of("p1", "a1", EventType::ToolStart);
        let producers: Vec<_> = (0..3)
            .map(|_| {
                let (tx, stop, msg) = (tx.clone(), Arc::clone(&stop), msg.clone());
                let running = Arc::clone(&running);
                std::thread::spawn(move || {
                    let _ = tx.send(msg.clone());
                    running.fetch_add(1, Ordering::SeqCst);
                    while !stop.load(Ordering::Relaxed) {
                        let _ = tx.send(msg.clone());
                    }
                })
            })
            .collect();
        // The pressure the commands must get through: every producer is
        // publishing, and the task is a whole channel behind.
        eventually("every producer is publishing", || {
            running.load(Ordering::SeqCst) == 3
        })
        .await;
        eventually("the ingest task lags a full channel", || tx.len() >= 8).await;
        // Watchdogs, not timing claims: a drain that follows the producers
        // never finishes, whatever the bound.
        let watchdog = Duration::from_secs(60);
        telemetry.end_run("p1", Some("a1"), EndReason::Exited, None);
        let ended = tokio::time::timeout(watchdog, telemetry.settled()).await;
        let shut =
            tokio::time::timeout(watchdog, telemetry.shutdown(Duration::from_secs(30))).await;
        stop.store(true, Ordering::Relaxed);
        for producer in producers {
            producer.join().unwrap();
        }
        drop(starting);
        assert!(
            ended.is_ok(),
            "the End was applied while publication went on"
        );
        assert!(shut.is_ok(), "the shutdown returned");
        exporter.abort();
        let spans: Vec<SpanRecord> = std::iter::from_fn(|| spans_rx.try_recv().ok()).collect();
        let reasons: Vec<_> = spans
            .iter()
            .filter(|s| s.name == SPAN_AGENT_RUN)
            .filter_map(|s| attr(s, AttrKey::EndReason).cloned())
            .collect();
        assert!(
            reasons.contains(&AttrValue::Str("exited".into())),
            "{reasons:?}"
        );
        assert!(
            reasons.contains(&AttrValue::Str("daemon_shutdown".into())),
            "the Shutdown was applied, not timed out: {reasons:?}"
        );
    }

    /// R6: a terminal report split across two writes is not an intervention;
    /// a key is.
    #[tokio::test]
    async fn a_split_report_is_not_an_intervention() {
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        begin(&telemetry, generation("p1", "a1"), &[]);
        telemetry.observe_client_input("p1", b"\x1b[");
        telemetry.observe_client_input("p1", b"24;80R");
        telemetry.observe_client_input("p1", b"\x1b]11;rgb:0");
        telemetry.observe_client_input("p1", b"000/0000/0000\x07");
        telemetry.end_run("p1", Some("a1"), EndReason::Exited, None);
        telemetry.settled().await;
        let spans = captured(&mut rx);
        let run = spans.iter().find(|s| s.name == SPAN_AGENT_RUN).unwrap();
        assert_eq!(
            attr(run, AttrKey::InterventionsCount),
            Some(&AttrValue::Int(0))
        );

        begin(&telemetry, generation("p1", "a2"), &[]);
        telemetry.observe_client_input("p1", b"\x1b[24;80R");
        telemetry.observe_client_input("p1", b"y");
        telemetry.end_run("p1", Some("a2"), EndReason::Exited, None);
        telemetry.settled().await;
        let spans = captured(&mut rx);
        let run = spans.iter().find(|s| s.name == SPAN_AGENT_RUN).unwrap();
        assert_eq!(
            attr(run, AttrKey::InterventionsCount),
            Some(&AttrValue::Int(1))
        );
    }

    /// A.3: shutdown with a run-context probe hanging returns within its
    /// budget, having killed and reaped the probe.
    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_is_bounded_with_a_hanging_probe() {
        let scratch = crate::test_temp::tempdir().expect("scratch tempdir");
        let pid_file = scratch.path().join("pids");
        let script = scratch.path().join("stalled-git.sh");
        std::fs::write(
            &script,
            format!("echo $$ >> '{}'\nexec sleep 30\n", pid_file.display()),
        )
        .unwrap();
        let (telemetry, mut rx) = capturing(ProbeSettings {
            program: Some((
                PathBuf::from("/bin/sh"),
                vec![script.to_string_lossy().into_owned()],
            )),
            deadline: Duration::from_secs(60),
            ..ProbeSettings::default()
        });
        let run = telemetry
            .prepare_run(
                generation("p1", "a1"),
                &[],
                Some(scratch.path().to_path_buf()),
            )
            .unwrap();
        run.commit();
        let until = std::time::Instant::now() + Duration::from_secs(10);
        while !pid_file.exists() {
            assert!(std::time::Instant::now() < until, "the probe never started");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let started = std::time::Instant::now();
        telemetry.shutdown(Duration::from_secs(2)).await;
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(telemetry.outstanding_probes(), 0);
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        let alive = std::process::Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(!alive, "the probe was killed and reaped");
        let spans = captured(&mut rx);
        let run = spans.iter().find(|s| s.name == SPAN_AGENT_RUN).unwrap();
        assert!(attr(run, AttrKey::InstructionsCommit).is_none());
        assert!(attr(run, AttrKey::RepositoryName).is_none());
    }

    // ---- HTTP ----

    /// One request a loopback receiver took: its head (lowercased) and body.
    #[derive(Debug, Clone)]
    struct Request {
        head: String,
        body: Vec<u8>,
    }

    async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<Request> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let head_end = loop {
            let n = socket.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
        let length: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .map(|v| v.trim().parse().unwrap())
            .unwrap_or(0);
        while buf.len() < head_end + length {
            let n = socket.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        Some(Request {
            head,
            body: buf[head_end..head_end + length].to_vec(),
        })
    }

    /// A loopback receiver that answers each connection's one request with
    /// the next of `responses` (the last repeating), and reports every request
    /// it took.
    async fn receiver(
        responses: Vec<String>,
    ) -> (std::net::SocketAddr, mpsc::UnboundedReceiver<Request>) {
        receiver_answering(responses.into_iter().map(|r| (r, Vec::new())).collect()).await
    }

    /// [`receiver`], each answer with a body.
    async fn receiver_answering(
        responses: Vec<(String, Vec<u8>)>,
    ) -> (std::net::SocketAddr, mpsc::UnboundedReceiver<Request>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut served = 0usize;
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let Some(request) = read_request(&mut socket).await else {
                    continue;
                };
                let (response, body) = &responses[served.min(responses.len() - 1)];
                served += 1;
                let _ = tx.send(request);
                let mut answer = format!(
                    "{response}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                answer.extend_from_slice(body);
                let _ = socket.write_all(&answer).await;
                let _ = socket.shutdown().await;
            }
        });
        (addr, rx)
    }

    const OK: &str = "HTTP/1.1 200 OK";
    const UNAVAILABLE: &str = "HTTP/1.1 503 Service Unavailable";

    fn sender_for(endpoint: String, headers: &str, timeout: Duration) -> Sender {
        let mut pairs = vec![("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", endpoint.as_str())];
        if !headers.is_empty() {
            pairs.push(("OTEL_EXPORTER_OTLP_HEADERS", headers));
        }
        let config = TelemetryConfig::from_env(&env(&pairs)).unwrap();
        Sender {
            client: build_client(&config).unwrap(),
            endpoint: config.endpoint.clone(),
            protocol: config.protocol.clone(),
            resource: deck_resource(&config.service_name, Some("h"), &[]),
            timeout,
            retry: RetryPolicy {
                initial: Duration::from_millis(20),
                max: Duration::from_millis(80),
            },
            errors: RateLimitedLog::new(ERROR_LOG_INTERVAL),
            stop: watch::channel(false).1,
            panic_on_send: false,
        }
    }

    fn one_span() -> Vec<SpanRecord> {
        let mut tracker = RunTracker::new();
        tracker.begin_run(crate::telemetry::tracker::tests::start("p1"), 1);
        tracker.end_run("p1", EndReason::Exited, None, 2);
        tracker.take_finished()
    }

    /// R5: a 503 is retried, and the retry delivers the same batch.
    #[tokio::test]
    async fn a_transient_failure_is_retried_until_it_delivers() {
        let (addr, mut requests) = receiver(vec![
            UNAVAILABLE.into(),
            "HTTP/1.1 429 Too Many".into(),
            OK.into(),
        ])
        .await;
        let mut sender = sender_for(
            format!("http://{addr}/v1/traces"),
            "",
            Duration::from_secs(5),
        );
        let outcome = sender.send(&mut one_span(), None).await;
        assert_eq!(outcome, Some(SendOutcome::Delivered { attempts: 3 }));
        let bodies: Vec<Vec<u8>> = std::iter::from_fn(|| requests.try_recv().ok())
            .map(|r| r.body)
            .collect();
        assert_eq!(bodies.len(), 3);
        assert!(
            bodies.windows(2).all(|w| w[0] == w[1]),
            "the same batch each time"
        );
    }

    /// R5: a backend that never recovers is given up on within the export
    /// timeout, after more than one attempt.
    #[tokio::test]
    async fn a_backend_that_never_recovers_is_given_up_on_in_time() {
        let (addr, _requests) = receiver(vec![UNAVAILABLE.into()]).await;
        let mut sender = sender_for(
            format!("http://{addr}/v1/traces"),
            "",
            Duration::from_millis(600),
        );
        let started = std::time::Instant::now();
        let outcome = sender.send(&mut one_span(), None).await;
        assert!(
            started.elapsed() < Duration::from_millis(1500),
            "{:?}",
            started.elapsed()
        );
        assert!(
            matches!(outcome, Some(SendOutcome::GaveUp { attempts }) if attempts >= 2),
            "{outcome:?}"
        );
        // A refused connection is transient too, and bounded the same way.
        let mut sender = sender_for(
            "http://127.0.0.1:9/v1/traces".into(),
            "",
            Duration::from_millis(300),
        );
        let started = std::time::Instant::now();
        assert!(matches!(
            sender.send(&mut one_span(), None).await,
            Some(SendOutcome::GaveUp { .. })
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    /// R5: the remaining shutdown budget bounds the retries, not only the
    /// export timeout.
    #[tokio::test]
    async fn the_flush_deadline_bounds_the_retries() {
        let (addr, _requests) = receiver(vec![UNAVAILABLE.into()]).await;
        let mut sender = sender_for(
            format!("http://{addr}/v1/traces"),
            "",
            Duration::from_secs(30),
        );
        let started = std::time::Instant::now();
        let outcome = sender
            .send(
                &mut one_span(),
                Some(Instant::now() + Duration::from_millis(300)),
            )
            .await;
        assert!(
            matches!(outcome, Some(SendOutcome::GaveUp { .. })),
            "{outcome:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    /// R5: `Retry-After` sets the wait; one past the deadline ends the export
    /// at once.
    #[tokio::test]
    async fn retry_after_is_honoured() {
        let (addr, _requests) =
            receiver(vec![format!("{UNAVAILABLE}\r\nretry-after: 1"), OK.into()]).await;
        let mut sender = sender_for(
            format!("http://{addr}/v1/traces"),
            "",
            Duration::from_secs(5),
        );
        let started = std::time::Instant::now();
        let outcome = sender.send(&mut one_span(), None).await;
        assert_eq!(outcome, Some(SendOutcome::Delivered { attempts: 2 }));
        assert!(
            started.elapsed() >= Duration::from_millis(900),
            "{:?}",
            started.elapsed()
        );

        let (addr, mut requests) =
            receiver(vec![format!("{UNAVAILABLE}\r\nretry-after: 30")]).await;
        let mut sender = sender_for(
            format!("http://{addr}/v1/traces"),
            "",
            Duration::from_secs(2),
        );
        let started = std::time::Instant::now();
        let outcome = sender.send(&mut one_span(), None).await;
        assert_eq!(outcome, Some(SendOutcome::GaveUp { attempts: 1 }));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(requests.try_recv().is_ok());
        assert!(requests.try_recv().is_err(), "no second attempt");
    }

    /// N1: a `Retry-After` of any value ends in bounded time without a panic:
    /// one too large to add to a clock (`u64::MAX` seconds) or merely past the
    /// deadline gives up at once; one that is not a number of seconds falls
    /// back to the backoff; zero still waits for the backoff.
    #[tokio::test]
    async fn any_retry_after_ends_in_bounded_time() {
        let cases: [(&str, SendOutcome); 6] = [
            ("18446744073709551615", SendOutcome::GaveUp { attempts: 1 }),
            ("1000000000", SendOutcome::GaveUp { attempts: 1 }),
            (
                "99999999999999999999999",
                SendOutcome::Delivered { attempts: 2 },
            ),
            ("-5", SendOutcome::Delivered { attempts: 2 }),
            ("0", SendOutcome::Delivered { attempts: 2 }),
            (
                "Wed, 21 Oct 2015 07:28:00 GMT",
                SendOutcome::Delivered { attempts: 2 },
            ),
        ];
        for (value, expected) in cases {
            let (addr, _requests) = receiver(vec![
                format!("{UNAVAILABLE}\r\nretry-after: {value}"),
                OK.into(),
            ])
            .await;
            let mut sender = sender_for(
                format!("http://{addr}/v1/traces"),
                "",
                Duration::from_secs(2),
            );
            let started = std::time::Instant::now();
            let outcome = sender.send(&mut one_span(), None).await;
            assert_eq!(outcome, Some(expected), "retry-after: {value}");
            assert!(
                started.elapsed() < Duration::from_secs(1),
                "retry-after: {value}: {:?}",
                started.elapsed()
            );
            if value == "0" {
                // The jittered first backoff is at least half of 20 ms.
                assert!(
                    started.elapsed() >= Duration::from_millis(10),
                    "retry-after: 0 retried without the backoff: {:?}",
                    started.elapsed()
                );
            }
        }
    }

    /// A backend that answers every send with `Retry-After: 0` is not hit
    /// back to back: the backoff still sets the pauses, so the attempts within
    /// the export timeout stay as few as the backoff allows.
    #[tokio::test]
    async fn a_zero_retry_after_does_not_shorten_the_backoff() {
        let (addr, mut requests) = receiver(vec![format!("{UNAVAILABLE}\r\nretry-after: 0")]).await;
        let mut sender = sender_for(
            format!("http://{addr}/v1/traces"),
            "",
            Duration::from_millis(400),
        );
        let outcome = sender.send(&mut one_span(), None).await;
        let Some(SendOutcome::GaveUp { attempts }) = outcome else {
            panic!("{outcome:?}");
        };
        // Pauses of at least 10, 20, then 40 ms each fit at most 12 attempts
        // in 400 ms; back-to-back retries over loopback make far more.
        assert!((2..=12).contains(&attempts), "{attempts} attempts");
        let taken = std::iter::from_fn(|| requests.try_recv().ok()).count();
        assert!(taken <= 12, "{taken} requests");
    }

    /// A final flush whose deadline passes with spans still unsent drops them
    /// and says how many in the log, rather than acknowledging in silence.
    #[tokio::test]
    async fn spans_left_at_the_flush_deadline_are_counted_in_a_warning() {
        use tracing_subscriber::layer::SubscriberExt;
        let logs = Captured::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::new(
                "dot_agent_deck::telemetry=trace",
            ))
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(move || writer.clone()),
            );
        let _guard = crate::test_isolation::capture_tracing_on_this_thread(subscriber);

        let sender = sender_for(
            "http://127.0.0.1:9/v1/traces".into(),
            "",
            Duration::from_secs(1),
        );
        let (queue, rx) = mpsc::channel(QUEUE_CAPACITY);
        let exporter = tokio::spawn(run_exporter(sender, rx));
        for _ in 0..3 {
            for span in one_span() {
                queue.send(ExportMsg::Span(Box::new(span))).await.unwrap();
            }
        }
        let (ack, done) = oneshot::channel();
        queue
            .send(ExportMsg::Flush {
                ack,
                // Already passed: nothing can be sent.
                deadline: Instant::now(),
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(60), done)
            .await
            .expect("the flush finished")
            .expect("the exporter task is alive to acknowledge it");
        drop(queue);
        exporter.await.unwrap();
        let logged = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        let warning = logged
            .lines()
            .find(|l| l.contains("the final flush ran out of time; spans dropped"))
            .unwrap_or_else(|| panic!("no drop warning: {logged}"));
        assert!(warning.contains("spans=3"), "{warning}");
    }

    /// N1: after a `Retry-After` of `u64::MAX` seconds, the exporter task is
    /// still there: a later batch is sent and delivered through it.
    #[tokio::test]
    async fn the_exporter_outlives_an_extreme_retry_after() {
        let (addr, mut requests) = receiver(vec![
            format!("{UNAVAILABLE}\r\nretry-after: 18446744073709551615"),
            OK.into(),
        ])
        .await;
        let config = TelemetryConfig::from_env(&env(&[(
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
            &format!("http://{addr}/v1/traces"),
        )]))
        .unwrap();
        let queue = spawn_exporter(
            &config,
            Some("h"),
            RetryPolicy::default(),
            watch::channel(false).1,
        )
        .unwrap();
        for _ in 0..2 {
            for span in one_span() {
                queue.send(ExportMsg::Span(Box::new(span))).await.unwrap();
            }
            let (ack, done) = oneshot::channel();
            queue
                .send(ExportMsg::Flush {
                    ack,
                    deadline: Instant::now() + Duration::from_secs(5),
                })
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), done)
                .await
                .expect("the flush finished in time")
                .expect("the exporter task is alive to acknowledge it");
        }
        let taken: Vec<Request> = std::iter::from_fn(|| requests.try_recv().ok()).collect();
        assert_eq!(taken.len(), 2, "the first batch once, the later one once");
    }

    /// An export timeout too large to add to a clock (the variable takes any
    /// number of milliseconds) still sends.
    #[tokio::test]
    async fn an_extreme_export_timeout_still_sends() {
        let (addr, _requests) = receiver(vec![OK.into()]).await;
        let config = TelemetryConfig::from_env(&env(&[
            (
                "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
                &format!("http://{addr}/v1/traces"),
            ),
            ("OTEL_EXPORTER_OTLP_TIMEOUT", "18446744073709551615"),
        ]))
        .unwrap();
        let mut sender = sender_for(config.endpoint.clone(), "", config.timeout);
        assert_eq!(
            sender.send(&mut one_span(), None).await,
            Some(SendOutcome::Delivered { attempts: 1 })
        );
    }

    /// A panic inside one export is contained: the exporter task carries on.
    #[tokio::test]
    async fn a_panicking_export_is_contained() {
        assert!(
            contained(async { panic!("an export that panics") })
                .await
                .is_err()
        );
        let mut ran = false;
        assert!(contained(async { ran = true }).await.is_ok());
        assert!(ran);
    }

    /// N9: an export that panics batch after batch, through the exporter
    /// task itself, is logged like any failed export — once, then not again
    /// within the interval — and the task carries on acknowledging flushes.
    #[tokio::test]
    async fn repeated_export_panics_are_logged_at_the_error_rate() {
        use tracing_subscriber::layer::SubscriberExt;
        let logs = Captured::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::new(
                "dot_agent_deck::telemetry=trace",
            ))
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(move || writer.clone()),
            );
        let _guard = crate::test_isolation::capture_tracing_on_this_thread(subscriber);

        let mut sender = sender_for(
            "http://127.0.0.1:9/v1/traces".into(),
            "",
            Duration::from_secs(1),
        );
        sender.panic_on_send = true;
        let (queue, rx) = mpsc::channel(QUEUE_CAPACITY);
        let exporter = tokio::spawn(run_exporter(sender, rx));
        for _ in 0..5 {
            for span in one_span() {
                queue.send(ExportMsg::Span(Box::new(span))).await.unwrap();
            }
            let (ack, done) = oneshot::channel();
            queue
                .send(ExportMsg::Flush {
                    ack,
                    deadline: Instant::now() + Duration::from_secs(60),
                })
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(60), done)
                .await
                .expect("the flush finished")
                .expect("the exporter task is alive to acknowledge it");
        }
        drop(queue);
        exporter.await.unwrap();
        let logged = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        let warned = logged.matches("failed unexpectedly").count();
        assert_eq!(warned, 1, "five panics inside one interval: {logged}");
    }

    /// A loopback receiver that takes the first request and never answers
    /// it, and answers every later one with 200.
    async fn stalling_receiver() -> (std::net::SocketAddr, mpsc::UnboundedReceiver<Request>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut held = None;
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let Some(request) = read_request(&mut socket).await else {
                    continue;
                };
                let _ = tx.send(request);
                if held.is_none() {
                    held = Some(socket);
                    continue;
                }
                let _ = socket
                    .write_all(
                        format!("{OK}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                            .as_bytes(),
                    )
                    .await;
                let _ = socket.shutdown().await;
            }
        });
        (addr, rx)
    }

    /// Threads 1 / 15: an export the backend leaves hanging when shutdown
    /// begins gives way, and the final flush sends both its batch and the
    /// final run within the flush's budget, not after the export timeout.
    #[tokio::test]
    async fn an_export_in_flight_gives_way_to_the_final_flush() {
        let (addr, mut requests) = stalling_receiver().await;
        let mut sender = sender_for(
            format!("http://{addr}/v1/traces"),
            "",
            Duration::from_secs(600),
        );
        let (stopping, stop) = watch::channel(false);
        sender.stop = stop;
        let (queue, rx) = mpsc::channel(QUEUE_CAPACITY);
        let exporter = tokio::spawn(run_exporter(sender, rx));
        // A full batch is sent at once; the backend takes it and never answers.
        for _ in 0..MAX_BATCH {
            for span in one_span() {
                queue.send(ExportMsg::Span(Box::new(span))).await.unwrap();
            }
        }
        let hung = requests.recv().await.unwrap();

        stopping.send_replace(true);
        let mut tracker = RunTracker::new();
        tracker.begin_run(crate::telemetry::tracker::tests::start("final"), 1);
        tracker.end_run("final", EndReason::DaemonShutdown, None, 2);
        for span in tracker.take_finished() {
            queue.send(ExportMsg::Span(Box::new(span))).await.unwrap();
        }
        let (ack, done) = oneshot::channel();
        queue
            .send(ExportMsg::Flush {
                ack,
                deadline: Instant::now() + Duration::from_secs(30),
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(60), done)
            .await
            .expect("the flush finished inside its budget")
            .unwrap();
        let flushed: Vec<Request> = std::iter::from_fn(|| requests.try_recv().ok()).collect();
        let has = |body: &[u8], needle: &[u8]| body.windows(needle.len()).any(|w| w == needle);
        assert!(
            flushed.iter().any(|r| r.body == hung.body),
            "the batch in flight was sent again"
        );
        assert!(
            flushed.iter().any(|r| has(&r.body, b"run-final")),
            "the final run was sent"
        );
        exporter.abort();
    }

    /// A telemetry handle exporting over HTTP to `addr`, with the default
    /// export timeout.
    fn exporting_to(addr: std::net::SocketAddr) -> Arc<Telemetry> {
        let endpoint = format!("http://{addr}");
        let telemetry = Telemetry::start(&env(&[("OTEL_EXPORTER_OTLP_ENDPOINT", &endpoint)]));
        assert!(telemetry.is_exporting());
        Arc::new(telemetry)
    }

    /// Qodo on PR #1814: the daemon stopped late in a stop client's window —
    /// its agents took their grace — and the backend stalls the final flush.
    /// The shutdown keeps to what is left of the window, not to the budget it
    /// was given, and says the spans were dropped.
    #[tokio::test]
    async fn a_stalled_flush_keeps_to_what_is_left_of_the_stop_window() {
        use tracing_subscriber::layer::SubscriberExt;
        let logs = Captured::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::new(
                "dot_agent_deck::telemetry=trace",
            ))
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(move || writer.clone()),
            );
        let _guard = crate::test_isolation::capture_tracing_on_this_thread(subscriber);

        let (addr, mut requests) = stalling_receiver().await;
        let telemetry = exporting_to(addr);
        begin(&telemetry, generation("p1", "a1"), &[]);
        telemetry.settled().await;
        let left = Duration::from_millis(300);
        telemetry.teardown_began(
            std::time::Instant::now() - (STOP_CLIENT_WINDOW - SHUTDOWN_HEADROOM - left),
        );

        let started = std::time::Instant::now();
        tokio::time::timeout(
            Duration::from_secs(30),
            telemetry.shutdown(Duration::from_secs(60)),
        )
        .await
        .expect("the shutdown kept to the stop window");
        let took = started.elapsed();
        assert!(took < left + Duration::from_millis(700), "{took:?}");
        assert!(
            requests.try_recv().is_ok(),
            "the final flush reached the backend, which held it"
        );
        let logged = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        assert!(logged.contains("spans dropped"), "{logged}");
    }

    /// With nothing to send, the shutdown returns at once, whatever its
    /// budget, even with the backend stalling.
    #[tokio::test]
    async fn a_shutdown_with_nothing_pending_returns_at_once() {
        let (addr, mut requests) = stalling_receiver().await;
        let telemetry = exporting_to(addr);
        let started = std::time::Instant::now();
        tokio::time::timeout(
            Duration::from_secs(30),
            telemetry.shutdown(Duration::from_secs(60)),
        )
        .await
        .expect("the shutdown returned");
        let took = started.elapsed();
        assert!(took < Duration::from_millis(500), "{took:?}");
        assert!(requests.try_recv().is_err(), "nothing was sent");
    }

    /// Thread 3: a 200 that reports spans the backend did not accept, in
    /// either encoding, is logged at the error rate, with the count it
    /// rejected and none of the backend's own text, and not retried. A
    /// backend's message can echo the configured header's value.
    #[tokio::test]
    async fn a_partial_success_is_logged_and_not_retried() {
        use tracing_subscriber::layer::SubscriberExt;
        let logs = Captured::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::new(
                "dot_agent_deck::telemetry=trace",
            ))
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(move || writer.clone()),
            );
        let _guard = crate::test_isolation::capture_tracing_on_this_thread(subscriber);

        let key = "sentinel-header-value-1741";
        let json = format!(
            r#"{{"partialSuccess":{{"rejectedSpans":"1","errorMessage":"invalid key {key}"}}}}"#
        );
        let (addr, mut requests) = receiver_answering(vec![(OK.into(), json.into_bytes())]).await;
        let mut sender = sender_for(
            format!("http://{addr}/v1/traces"),
            &format!("x-api-key={key}"),
            Duration::from_secs(5),
        );
        sender.protocol = Protocol::HttpJson;
        for _ in 0..2 {
            assert_eq!(
                sender.send(&mut one_span(), None).await,
                Some(SendOutcome::PartiallyRejected {
                    attempts: 1,
                    rejected: 1
                })
            );
        }
        assert_eq!(
            std::iter::from_fn(|| requests.try_recv().ok()).count(),
            2,
            "one request per send"
        );

        // ExportTraceServiceResponse { partial_success: { rejected_spans: 2,
        // error_message: "late" } }.
        let protobuf = vec![0x0a, 0x08, 0x08, 0x02, 0x12, 0x04, b'l', b'a', b't', b'e'];
        let (addr, _requests) = receiver_answering(vec![(OK.into(), protobuf)]).await;
        let mut sender = sender_for(
            format!("http://{addr}/v1/traces"),
            "",
            Duration::from_secs(5),
        );
        assert_eq!(
            sender.send(&mut one_span(), None).await,
            Some(SendOutcome::PartiallyRejected {
                attempts: 1,
                rejected: 2
            })
        );

        // Nothing reported, or a report of nothing, is plain delivery.
        for body in [Vec::new(), br#"{"partialSuccess":{}}"#.to_vec()] {
            let (addr, _requests) = receiver_answering(vec![(OK.into(), body)]).await;
            let mut sender = sender_for(
                format!("http://{addr}/v1/traces"),
                "",
                Duration::from_secs(5),
            );
            sender.protocol = Protocol::HttpJson;
            assert_eq!(
                sender.send(&mut one_span(), None).await,
                Some(SendOutcome::Delivered { attempts: 1 })
            );
        }

        let logged = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        assert_eq!(
            logged.matches("did not accept every span").count(),
            2,
            "once per sender inside the interval: {logged}"
        );
        assert!(logged.contains("rejected=1"), "{logged}");
        assert!(logged.contains("rejected=2"), "{logged}");
        for backend_text in [key, "invalid key", "late"] {
            assert!(!logged.contains(backend_text), "{backend_text}: {logged}");
        }
    }

    /// A rejection that is not transient is not retried.
    #[tokio::test]
    async fn a_bad_request_is_not_retried() {
        let (addr, mut requests) = receiver(vec!["HTTP/1.1 400 Bad Request".into()]).await;
        let mut sender = sender_for(
            format!("http://{addr}/v1/traces"),
            "",
            Duration::from_secs(5),
        );
        assert_eq!(
            sender.send(&mut one_span(), None).await,
            Some(SendOutcome::Rejected { status: 400 })
        );
        assert!(requests.try_recv().is_ok());
        assert!(requests.try_recv().is_err());
    }

    /// A.1: a redirect is a failed export. The location it names receives
    /// neither the request nor the configured header.
    #[tokio::test]
    async fn a_redirect_is_not_followed() {
        let elsewhere = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let elsewhere_addr = elsewhere.local_addr().unwrap();
        for status in ["307 Temporary Redirect", "308 Permanent Redirect"] {
            let (addr, mut requests) = receiver(vec![format!(
                "HTTP/1.1 {status}\r\nlocation: http://{elsewhere_addr}/v1/traces"
            )])
            .await;
            let mut sender = sender_for(
                format!("http://{addr}/v1/traces"),
                "x-secret=SENTINEL-HEADER-VALUE",
                Duration::from_secs(5),
            );
            let outcome = sender.send(&mut one_span(), None).await;
            let code: u16 = status[..3].parse().unwrap();
            assert_eq!(outcome, Some(SendOutcome::Rejected { status: code }));
            let first = requests.try_recv().unwrap();
            assert!(first.head.contains("x-secret: sentinel-header-value"));
            assert!(requests.try_recv().is_err(), "not retried either");
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(500), elsewhere.accept())
                .await
                .is_err(),
            "the redirect target was never contacted"
        );
    }

    async fn export_one_run(protocol: &str) -> (String, Vec<u8>, String) {
        let (addr, mut requests) = receiver(vec![OK.into()]).await;
        let telemetry = Arc::new(Telemetry::start(&env(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", &format!("http://{addr}")),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", protocol),
            ("OTEL_EXPORTER_OTLP_HEADERS", "x-api-key=k%20v"),
        ])));
        assert!(telemetry.is_exporting());
        let added = begin(&telemetry, run_start("p1"), &[]);
        let attrs = super::super::resource::parse_resource_attributes(
            get(&added, "OTEL_RESOURCE_ATTRIBUTES").unwrap(),
        );
        let run_id = attrs
            .iter()
            .find(|(k, _)| k == "deck.run.id")
            .unwrap()
            .1
            .clone();
        telemetry.observe_client_input("p1", b"\x1b[I");
        telemetry.observe_client_input("p1", b"y\r");
        telemetry.shutdown(Duration::from_secs(10)).await;
        let request = tokio::time::timeout(Duration::from_secs(10), requests.recv())
            .await
            .expect("the exporter posted within the budget")
            .unwrap();
        (request.head, request.body, run_id)
    }

    #[tokio::test]
    async fn exports_protobuf_to_the_traces_path_with_headers() {
        let (head, body, run_id) = export_one_run("http/protobuf").await;
        assert!(head.starts_with("post /v1/traces "), "{head}");
        assert!(head.contains("content-type: application/x-protobuf"));
        assert!(head.contains("x-api-key: k v"));
        let contains = |needle: &[u8]| body.windows(needle.len()).any(|w| w == needle);
        assert!(
            contains(run_id.as_bytes()),
            "the agent's run id is the span's"
        );
        assert!(contains(b"daemon_shutdown"));
        assert!(contains(b"agent.run"));
    }

    #[tokio::test]
    async fn exports_json_when_asked() {
        let (head, body, run_id) = export_one_run("http/json").await;
        assert!(head.contains("content-type: application/json"));
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let spans = &v["resourceSpans"][0]["scopeSpans"][0]["spans"];
        let run = &spans[0];
        assert_eq!(run["name"], "agent.run");
        let attr = |key: &str| {
            run["attributes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|kv| kv["key"] == key)
                .map(|kv| kv["value"].clone())
        };
        assert_eq!(attr("deck.run.id").unwrap()["stringValue"], run_id);
        assert_eq!(attr("deck.interventions.count").unwrap()["intValue"], "1");
        assert_eq!(
            attr("deck.run.end_reason").unwrap()["stringValue"],
            "daemon_shutdown"
        );
    }

    // ---- A.4: the never-sent rule, through the exporter ----

    /// A `MakeWriter` collecting log output.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Sentinels in everything a run's input has — event fields, a valid pull
    /// request report, human input, the spawn's command, title and
    /// orchestration directory, the working directory and its git remote, an
    /// environment variable and the OTLP header — go through the whole path:
    /// spawn preparation, the broadcast, the ingest task, the git probes, the
    /// exporter (one failed request, one delivered) and its logs. None comes
    /// out in a request body's bytes, in a request head other than the
    /// header's own line, or in a log line the telemetry module wrote — over
    /// either protocol. The delivered body, decoded, carries the pull request's
    /// number and the repository's name, so the path really was taken.
    #[cfg(unix)]
    #[tokio::test]
    async fn no_sentinel_reaches_a_request_or_a_log_line_over_json() {
        no_sentinel_reaches_a_request_or_a_log_line("http/json").await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn no_sentinel_reaches_a_request_or_a_log_line_over_protobuf() {
        no_sentinel_reaches_a_request_or_a_log_line("http/protobuf").await;
    }

    #[cfg(unix)]
    async fn no_sentinel_reaches_a_request_or_a_log_line(protocol: &str) {
        use crate::telemetry::otlp::tests::{
            SENTINEL_AGENT_ID, SENTINELS, json_values, loaded_event, protobuf_values,
        };
        use tracing_subscriber::layer::SubscriberExt;

        let logs = Captured::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::new(
                "dot_agent_deck::telemetry=trace",
            ))
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(move || writer.clone()),
            );
        let _guard = crate::test_isolation::capture_tracing_on_this_thread(subscriber);

        let scratch = crate::test_temp::tempdir().expect("scratch tempdir");
        let cwd = scratch.path().join("SENTINEL-CWD");
        std::fs::create_dir_all(&cwd).unwrap();
        let git = |args: &[&str]| {
            assert!(
                crate::git_env::fixture_git(&cwd, scratch.path())
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        git(&["init", "--quiet"]);
        git(&[
            "remote",
            "add",
            "origin",
            "https://SENTINEL-ENV-VALUE@example.com/SENTINEL-BRANCH/repo.git",
        ]);

        let (addr, mut requests) =
            receiver(vec!["HTTP/1.1 400 Bad Request".into(), OK.into()]).await;
        let telemetry = Arc::new(Telemetry::start(&env(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", &format!("http://{addr}")),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", protocol),
            (
                "OTEL_EXPORTER_OTLP_HEADERS",
                "x-secret=SENTINEL-HEADER-VALUE",
            ),
            ("SOME_TOKEN", "SENTINEL-ENV-VALUE"),
        ])));
        let (tx, events) = broadcast::channel(64);
        telemetry.subscribe(events);
        let membership = crate::agent_pty::TabMembership::Orchestration {
            name: "tdd".into(),
            role_index: 0,
            role_name: "coder".into(),
            is_start_role: false,
            orchestration_cwd: Some("/x/SENTINEL-CWD".into()),
            display_title: Some("SENTINEL-TITLE".into()),
            orchestration_id: Some("orch-1".into()),
        };
        let start = |pane: &str| {
            RunStart::for_spawn(
                pane,
                SENTINEL_AGENT_ID,
                None,
                Some("claude --append SENTINEL-COMMAND SENTINEL-PROMPT"),
                Some(&membership),
            )
        };
        let spawn_env = vec![("SOME_TOKEN".to_string(), "SENTINEL-ENV-VALUE".to_string())];

        // Run 1: its export is rejected, so the failure is logged.
        let run = telemetry
            .prepare_run(start("pane-1"), &spawn_env, Some(cwd.clone()))
            .unwrap();
        for (k, v) in &run.env {
            for sentinel in SENTINELS {
                assert!(!v.contains(sentinel) && !k.contains(sentinel), "{k}={v}");
            }
        }
        run.commit();
        for event_type in [
            EventType::SessionStart,
            EventType::ToolStart,
            EventType::PullRequest,
        ] {
            tx.send(BroadcastMsg::Event(loaded_event("pane-1", event_type)))
                .unwrap();
        }
        telemetry.observe_client_input("pane-1", b"SENTINEL-INPUT\r");
        while telemetry.outstanding_probes() > 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        telemetry.end_run("pane-1", Some(SENTINEL_AGENT_ID), EndReason::Exited, None);
        telemetry.settled().await;
        let exporting = telemetry.exporting.as_ref().unwrap();
        let (ack, done) = oneshot::channel();
        exporting
            .shared
            .queue
            .send(ExportMsg::Flush {
                ack,
                deadline: Instant::now() + Duration::from_secs(5),
            })
            .await
            .unwrap();
        done.await.unwrap();

        // Run 2: delivered at shutdown.
        let run = telemetry
            .prepare_run(start("pane-2"), &spawn_env, Some(cwd.clone()))
            .unwrap();
        run.commit();
        let events: Vec<AgentEvent> = [
            EventType::SessionStart,
            EventType::PullRequest,
            EventType::Idle,
        ]
        .into_iter()
        .map(|t| loaded_event("pane-2", t))
        .collect();
        for e in events {
            tx.send(BroadcastMsg::Event(e)).unwrap();
        }
        telemetry.observe_client_input("pane-2", b"more SENTINEL-INPUT");
        while telemetry.outstanding_probes() > 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        telemetry.shutdown(Duration::from_secs(5)).await;

        let taken: Vec<Request> = std::iter::from_fn(|| requests.try_recv().ok()).collect();
        assert_eq!(taken.len(), 2, "one rejected request, one delivered");
        let expected_type = match protocol {
            "http/json" => "content-type: application/json",
            _ => "content-type: application/x-protobuf",
        };
        assert!(taken[1].head.contains(expected_type), "{}", taken[1].head);
        let values = match protocol {
            "http/json" => json_values(&taken[1].body),
            _ => protobuf_values(&taken[1].body),
        };
        let has = |v: &str| values.iter().any(|x| x == v);
        assert!(
            has("4242"),
            "the valid report's number was sent: {values:?}"
        );
        assert!(has("repo"), "the repository name was resolved: {values:?}");
        for request in &taken {
            let body = &request.body;
            let head: String = request
                .head
                .lines()
                .filter(|l| !l.starts_with("x-secret:"))
                .collect::<Vec<_>>()
                .join("\n");
            for sentinel in SENTINELS {
                assert!(
                    !body
                        .windows(sentinel.len())
                        .any(|w| w == sentinel.as_bytes()),
                    "{sentinel} in a request body: {}",
                    String::from_utf8_lossy(body)
                );
                assert!(
                    !head.contains(&sentinel.to_ascii_lowercase()),
                    "{sentinel} in a request head: {head}"
                );
            }
        }
        let logged = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        assert!(
            logged.contains("OTLP export failed"),
            "the failure path logged: {logged}"
        );
        for sentinel in SENTINELS {
            assert!(
                !logged.contains(sentinel),
                "{sentinel} in the log: {logged}"
            );
        }
        let _ = Attribute::str(AttrKey::RunId, "");
    }

    fn health_attr(span: &SpanRecord, key: AttrKey) -> Option<i64> {
        match attr(span, key)? {
            AttrValue::Int(v) => Some(*v),
            _ => None,
        }
    }

    fn some_span() -> SpanRecord {
        SpanRecord {
            trace_id: [1; 16],
            span_id: [1; 8],
            parent_span_id: None,
            name: SPAN_AGENT_RUN,
            start_unix_nanos: 1,
            end_unix_nanos: 2,
            attributes: Vec::new(),
            events: Vec::new(),
        }
    }

    /// Issue #1981 M8: `deck.telemetry.spans.dropped` is every span the
    /// queue refused since the start, the health spans it refused included,
    /// and is not reset between warnings or between samples.
    #[tokio::test]
    async fn dropped_spans_are_counted_exactly_across_health_samples() {
        let (sink, mut rx) = mpsc::channel(2);
        let telemetry = Telemetry::start_with(
            &env(&ON),
            Settings {
                sink: Some(sink),
                ..Settings::default()
            },
        );
        let shared = &telemetry.exporting.as_ref().unwrap().shared;
        // Two fit; three are refused.
        shared.enqueue((0..5).map(|_| some_span()).collect());
        // A health span sampled now says three, and is refused itself.
        shared.sample_health(now_unix_nanos(), None);
        let held = captured(&mut rx);
        assert_eq!(held.len(), 2);
        assert!(held.iter().all(|s| s.name == SPAN_AGENT_RUN));
        // Room again: two samples in a row, each counting the refused one.
        shared.sample_health(now_unix_nanos(), None);
        shared.sample_health(now_unix_nanos(), None);
        // The queue is full again: this sample is refused.
        shared.sample_health(now_unix_nanos(), None);
        let samples = captured(&mut rx);
        let dropped: Vec<_> = samples
            .iter()
            .map(|s| {
                assert_eq!(s.name, crate::telemetry::SPAN_DAEMON_HEALTH);
                health_attr(s, AttrKey::SpansDropped)
            })
            .collect();
        assert_eq!(dropped, vec![Some(4), Some(4)]);
        shared.sample_health(now_unix_nanos(), None);
        let next = captured(&mut rx);
        assert_eq!(next.len(), 1);
        assert_eq!(health_attr(&next[0], AttrKey::SpansDropped), Some(5));
        // The interval of each sample starts where the last one ended.
        assert_eq!(samples[1].start_unix_nanos, samples[0].end_unix_nanos);
    }

    /// Issue #1981 M8: the ingest task sends `daemon.health` on its interval,
    /// counting the open runs and the latency of the hook events an agent
    /// produced, never of one the daemon made itself.
    #[tokio::test]
    async fn health_is_sampled_on_its_interval_from_producer_events_only() {
        let (sink, mut rx) = mpsc::channel(QUEUE_CAPACITY);
        let telemetry = Arc::new(Telemetry::start_with(
            &env(&ON),
            Settings {
                sink: Some(sink),
                health_interval: Some(Duration::from_millis(50)),
                ..Settings::default()
            },
        ));
        let (tx, events) = broadcast::channel(16);
        telemetry.subscribe(events);
        begin(&telemetry, generation("p1", "a1"), &[]);
        let mut produced = event("p1", EventType::Thinking);
        produced.agent_id = Some("a1".into());
        produced.timestamp = chrono::Utc::now() - chrono::Duration::milliseconds(1500);
        tx.send(BroadcastMsg::Event(received_after(produced, 1500)))
            .unwrap();
        // The daemon's own: a shell's busy state.
        tx.send(of("p1", "a1", EventType::ShellBusy)).unwrap();
        telemetry.settled().await;
        let applied = now_unix_nanos();
        let mut health = Vec::new();
        // Once an interval opened after the events were applied has been
        // sent, the one they fell in has been too.
        eventually("an interval after the events", || {
            health.extend(
                captured(&mut rx)
                    .into_iter()
                    .filter(|s| s.name == crate::telemetry::SPAN_DAEMON_HEALTH),
            );
            health.iter().any(|s| s.start_unix_nanos >= applied)
        })
        .await;
        let count: i64 = health
            .iter()
            .map(|s| health_attr(s, AttrKey::HookLatencyCount).unwrap())
            .sum();
        assert_eq!(count, 1, "{health:?}");
        let sampled = health
            .iter()
            .find(|s| health_attr(s, AttrKey::HookLatencyCount) == Some(1))
            .unwrap();
        assert!(health_attr(sampled, AttrKey::HookLatencyP50).unwrap() >= 1500);
        assert!(
            health
                .iter()
                .all(|s| health_attr(s, AttrKey::DaemonRunsOpen) == Some(1))
        ); // The memory read off the ingest task's worker reaches each sample.
        #[cfg(target_os = "linux")]
        assert!(
            health
                .iter()
                .all(|s| health_attr(s, AttrKey::DaemonMemoryRss).is_some_and(|b| b > 0)),
            "{health:?}"
        );
    }

    /// Issue #1981 M8: `event` as the daemon admits it from a producer,
    /// received `ms` milliseconds after its hook stamped it
    /// (`crate::daemon::admit_producer_event`).
    fn received_after(mut event: AgentEvent, ms: i64) -> AgentEvent {
        let at = event.timestamp + chrono::Duration::milliseconds(ms);
        event.metadata.insert(
            crate::event::RECEIVED_BY_DECK_AT_METADATA_KEY.to_string(),
            at.timestamp_nanos_opt().unwrap().to_string(),
        );
        event
    }

    /// Issue #1981 M8 (PR #2010 review): hook latency counts only the events
    /// the daemon admitted from a producer, measured to when the daemon
    /// received each. The events the daemon builds itself carry no receipt
    /// stamp and are not counted, whatever their type: the question it turns
    /// keyboard-only after typing part of an answer, a Codex turn's failure
    /// read from its rollout, and a shell's busy state.
    #[tokio::test]
    async fn only_events_admitted_from_a_producer_are_hook_latency_samples() {
        let (sink, mut rx) = mpsc::channel(QUEUE_CAPACITY);
        let telemetry = Arc::new(Telemetry::start_with(
            &env(&ON),
            Settings {
                sink: Some(sink),
                // Sampled by hand below, not by the interval.
                health_interval: Some(Duration::from_secs(3600)),
                ..Settings::default()
            },
        ));
        let (tx, events) = broadcast::channel(16);
        telemetry.subscribe(events);
        begin(&telemetry, generation("p1", "a1"), &[]);
        // Stamped by its hook a minute ago and received 1.5 s later: the
        // sample is the 1.5 s, not the minute until the broadcast was read.
        let mut produced = event("p1", EventType::Thinking);
        produced.agent_id = Some("a1".into());
        produced.timestamp = chrono::Utc::now() - chrono::Duration::seconds(60);
        tx.send(BroadcastMsg::Event(received_after(produced, 1500)))
            .unwrap();
        let question: crate::question::PendingQuestion =
            serde_json::from_value(serde_json::json!({
                "id": "q1",
                "kind": "permission",
                "questions": [{"prompt": "Allow?", "options": [
                    {"index": 1, "label": "Yes", "role": "allow_once"},
                    {"index": 2, "label": "No", "role": "deny"},
                ]}],
                "raised_at_ms": 1,
                "channel": "unsupported",
                "revision": 2,
            }))
            .unwrap();
        let mut keyboard_only = crate::daemon::deck_keyboard_only_event(
            "p1-session".into(),
            AgentType::ClaudeCode,
            Some("p1".into()),
            Some("a1".into()),
            &question,
        );
        keyboard_only.timestamp = chrono::Utc::now() - chrono::Duration::milliseconds(9000);
        tx.send(BroadcastMsg::Event(keyboard_only)).unwrap();
        let mut rollout_error = crate::daemon::codex_rollout_failure_event(
            crate::codex_rollout_tail::CodexTurnFailure {
                pane_id: "p1".into(),
                agent_id: "a1".into(),
                session_id: "p1-session".into(),
                turn_id: "t1".into(),
                outcome: crate::quota_signals::FailureOutcome::Error,
                message: None,
            },
        );
        assert_eq!(rollout_error.event_type, EventType::Error, "precondition");
        rollout_error.timestamp = chrono::Utc::now() - chrono::Duration::milliseconds(9000);
        tx.send(BroadcastMsg::Event(rollout_error)).unwrap();
        tx.send(of("p1", "a1", EventType::ShellBusy)).unwrap();
        telemetry.settled().await;
        let shared = &telemetry.exporting.as_ref().unwrap().shared;
        shared.sample_health(now_unix_nanos(), None);
        let health: Vec<_> = captured(&mut rx)
            .into_iter()
            .filter(|s| s.name == crate::telemetry::SPAN_DAEMON_HEALTH)
            .collect();
        assert_eq!(health.len(), 1, "{health:?}");
        assert_eq!(health_attr(&health[0], AttrKey::HookLatencyCount), Some(1));
        assert_eq!(health_attr(&health[0], AttrKey::HookLatencyMax), Some(1500));
        assert_eq!(health_attr(&health[0], AttrKey::HookLatencyP50), Some(1500));
    }

    /// Issue #1981 M8: an event carrying no receipt stamp, or one that is not
    /// a number, is no hook latency sample, whatever its type or age.
    #[tokio::test]
    async fn an_event_without_a_readable_receipt_stamp_is_no_hook_latency_sample() {
        let (sink, mut rx) = mpsc::channel(QUEUE_CAPACITY);
        let telemetry = Arc::new(Telemetry::start_with(
            &env(&ON),
            Settings {
                sink: Some(sink),
                health_interval: Some(Duration::from_secs(3600)),
                ..Settings::default()
            },
        ));
        let (tx, events) = broadcast::channel(16);
        telemetry.subscribe(events);
        begin(&telemetry, generation("p1", "a1"), &[]);
        let mut unstamped = event("p1", EventType::Thinking);
        unstamped.agent_id = Some("a1".into());
        tx.send(BroadcastMsg::Event(unstamped.clone())).unwrap();
        unstamped.metadata.insert(
            crate::event::RECEIVED_BY_DECK_AT_METADATA_KEY.to_string(),
            "soon".to_string(),
        );
        tx.send(BroadcastMsg::Event(unstamped)).unwrap();
        telemetry.settled().await;
        let shared = &telemetry.exporting.as_ref().unwrap().shared;
        shared.sample_health(now_unix_nanos(), None);
        let health: Vec<_> = captured(&mut rx)
            .into_iter()
            .filter(|s| s.name == crate::telemetry::SPAN_DAEMON_HEALTH)
            .collect();
        assert_eq!(health.len(), 1, "{health:?}");
        assert_eq!(health_attr(&health[0], AttrKey::HookLatencyCount), Some(0));
    }

    /// Issue #1981 M7 review: a post keeps its place on the broadcast. With
    /// the ingest task paused, a task is posted and then the orchestrator's
    /// pane-close is broadcast; resumed, the post is applied before the
    /// close, so the hand-off is the orchestrator run's child and ends
    /// `unknown` at that run's end rather than parentless and open.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_post_is_applied_before_a_pane_close_broadcast_after_it() {
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let (tx, events) = broadcast::channel(16);
        telemetry.subscribe(events);
        begin(&telemetry, generation("orch", "oa"), &[]);
        begin(&telemetry, generation("work", "wa"), &[]);
        telemetry.settled().await;
        let shared = &telemetry.exporting.as_ref().unwrap().shared;
        let (entered, polled_empty) = oneshot::channel();
        let (resume, paused) = oneshot::channel();
        *shared.command_gate.lock().unwrap() = Some((entered, paused));
        // A barrier brings the task round to the gate, unless its usage poll
        // already did, with the barrier queued behind it.
        let nudge = {
            let telemetry = Arc::clone(&telemetry);
            tokio::spawn(async move { telemetry.settled().await })
        };
        polled_empty.await.unwrap();
        crate::telemetry::observe_delegate_posted(
            Some(Arc::clone(&telemetry)),
            &crate::telemetry::DelegatePosted {
                message_id: "m-t",
                orchestrator_pane: "orch",
                orchestrator_agent_id: Some("oa"),
                worker_pane: "work",
                worker_agent_id: "wa",
                role: "coder",
            },
        );
        tx.send(pane_closed("orch", "oa")).unwrap();
        resume.send(()).unwrap();
        nudge.await.unwrap();
        telemetry.settled().await;
        let spans = captured(&mut rx);
        let handoffs: Vec<_> = spans
            .iter()
            .filter(|s| s.name == crate::telemetry::SPAN_AGENT_HANDOFF)
            .collect();
        assert_eq!(handoffs.len(), 1, "{spans:?}");
        let handoff = handoffs[0];
        let run = ended_runs(&spans)
            .into_iter()
            .find(|s| attr(s, AttrKey::PaneId) == Some(&AttrValue::Str("orch".into())))
            .expect("the orchestrator's run ended");
        assert_eq!(handoff.trace_id, run.trace_id);
        assert_eq!(handoff.parent_span_id, Some(run.span_id));
        assert_eq!(attr(handoff, AttrKey::RunId), attr(run, AttrKey::RunId));
        assert_eq!(
            attr(handoff, AttrKey::HandoffOutcome),
            Some(&AttrValue::Str("unknown".into()))
        );
        assert_eq!(handoff.end_unix_nanos, run.end_unix_nanos);
    }

    /// Issue #1981 M7 review: the ingest task ends a hand-off past its age
    /// bound on its own timer, with no inbox change or post to prompt it,
    /// at the moment the bound was reached.
    #[tokio::test]
    async fn the_ingest_timer_ends_a_handoff_past_its_age_bound() {
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        begin(&telemetry, generation("orch", "oa"), &[]);
        telemetry.settled().await;
        let posted_at = now_unix_nanos() - crate::telemetry::handoff::MAX_HANDOFF_AGE_NANOS - 1;
        let shared = &telemetry.exporting.as_ref().unwrap().shared;
        shared.with_tracker(|t| {
            t.delegate_posted(crate::telemetry::handoff::Posted {
                message_id: "m-old".into(),
                orchestrator_pane: "orch".into(),
                orchestrator_agent_id: Some("oa".into()),
                worker_pane: "work".into(),
                worker_agent_id: "wa".into(),
                role: "coder".into(),
                at: posted_at,
            });
        });
        let mut handoffs = Vec::new();
        eventually("the timer ended the hand-off", || {
            handoffs.extend(
                captured(&mut rx)
                    .into_iter()
                    .filter(|s| s.name == crate::telemetry::SPAN_AGENT_HANDOFF),
            );
            !handoffs.is_empty()
        })
        .await;
        assert_eq!(
            attr(&handoffs[0], AttrKey::HandoffOutcome),
            Some(&AttrValue::Str("unknown".into()))
        );
        assert_eq!(
            handoffs[0].end_unix_nanos,
            posted_at + crate::telemetry::handoff::MAX_HANDOFF_AGE_NANOS
        );
    }

    /// Pause the ingest task at its command gate, so what is sent next is
    /// queued and broadcast without being applied. The sender resumes it; the
    /// handle is the barrier that brought it round, done once it is through.
    async fn pause_ingest(
        telemetry: &Arc<Telemetry>,
    ) -> (oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
        let shared = &telemetry.exporting.as_ref().unwrap().shared;
        let (entered, polled_empty) = oneshot::channel();
        let (resume, paused) = oneshot::channel();
        *shared.command_gate.lock().unwrap() = Some((entered, paused));
        // A barrier brings the task round to the gate, unless its usage poll
        // already did, with the barrier queued behind it.
        let nudge = {
            let telemetry = Arc::clone(telemetry);
            tokio::spawn(async move { telemetry.settled().await })
        };
        polled_empty.await.unwrap();
        (resume, nudge)
    }

    /// The delegate path posting task `message_id` from the orchestrator
    /// `orch`/`agent` to the worker `work`/`wa`.
    fn post_task(telemetry: &Arc<Telemetry>, message_id: &str, orch: &str, agent: &str) {
        crate::telemetry::observe_delegate_posted(
            Some(Arc::clone(telemetry)),
            &crate::telemetry::DelegatePosted {
                message_id,
                orchestrator_pane: orch,
                orchestrator_agent_id: Some(agent),
                worker_pane: "work",
                worker_agent_id: "wa",
                role: "coder",
            },
        );
    }

    fn open_handoffs(telemetry: &Telemetry) -> usize {
        let mut open = 0;
        let shared = &telemetry.exporting.as_ref().unwrap().shared;
        shared.with_tracker(|t| open = t.handoffs_held().0);
        open
    }

    fn handoff_spans(spans: &[SpanRecord]) -> Vec<&SpanRecord> {
        spans
            .iter()
            .filter(|s| s.name == crate::telemetry::SPAN_AGENT_HANDOFF)
            .collect()
    }

    fn handoff<'a>(spans: &'a [SpanRecord], message_id: &str) -> &'a SpanRecord {
        handoff_spans(spans)
            .into_iter()
            .find(|s| attr(s, AttrKey::HandoffId) == Some(&AttrValue::Str(message_id.into())))
            .unwrap_or_else(|| panic!("no hand-off for {message_id}: {spans:?}"))
    }

    fn run_on<'a>(spans: &'a [SpanRecord], pane: &str) -> &'a SpanRecord {
        ended_runs(spans)
            .into_iter()
            .find(|s| attr(s, AttrKey::PaneId) == Some(&AttrValue::Str(pane.into())))
            .unwrap_or_else(|| panic!("no run ended on {pane}: {spans:?}"))
    }

    /// `handoff` is in `run`'s trace, as its child, naming it.
    fn assert_child_of(handoff: &SpanRecord, run: &SpanRecord) {
        assert_eq!(handoff.trace_id, run.trace_id, "{handoff:?}");
        assert_eq!(handoff.parent_span_id, Some(run.span_id), "{handoff:?}");
        assert_eq!(attr(handoff, AttrKey::RunId), attr(run, AttrKey::RunId));
    }

    /// Issue #1981 M7 review (the reviewer's
    /// `review_prepared_post_survives_later_close`): the orchestrator's run
    /// is prepared and its commit queued when it posts a task, and its pane's
    /// close is broadcast after the post. Applied as the run opens, the post
    /// keeps its place before the close, so the hand-off is the run's child
    /// and ends `unknown` with it rather than parentless.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_post_against_a_prepared_run_is_applied_before_a_later_close() {
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let (tx, events) = broadcast::channel(16);
        telemetry.subscribe(events);
        begin(&telemetry, generation("work", "wa"), &[]);
        telemetry.settled().await;
        let (resume, nudge) = pause_ingest(&telemetry).await;
        let orch = telemetry
            .prepare_run(generation("orch", "oa"), &[], None)
            .unwrap();
        orch.commit();
        post_task(&telemetry, "m-o", "orch", "oa");
        tx.send(pane_closed("orch", "oa")).unwrap();
        resume.send(()).unwrap();
        nudge.await.unwrap();
        telemetry.settled().await;
        let spans = captured(&mut rx);
        assert_eq!(handoff_spans(&spans).len(), 1, "{spans:?}");
        let handoff = handoff(&spans, "m-o");
        let run = run_on(&spans, "orch");
        assert_child_of(handoff, run);
        assert_eq!(
            attr(handoff, AttrKey::HandoffOutcome),
            Some(&AttrValue::Str("unknown".into()))
        );
        assert_eq!(handoff.end_unix_nanos, run.end_unix_nanos);
    }

    /// Issue #1981 M7 review (the reviewer's
    /// `review_one_post_reminder_does_not_steal_later_prepared_post`): with
    /// A's run open, A posts, B's run is prepared and its commit queued, then
    /// B posts, with nothing broadcast in between. A's reminder places A's
    /// post only, so B's is placed after B's run opens, in B's trace.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_post_reminder_does_not_take_a_later_post_against_a_prepared_run() {
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let (_tx, events) = broadcast::channel::<BroadcastMsg>(16);
        telemetry.subscribe(events);
        begin(&telemetry, generation("orch-a", "oa"), &[]);
        begin(&telemetry, generation("work", "wa"), &[]);
        telemetry.settled().await;
        let (resume, nudge) = pause_ingest(&telemetry).await;
        post_task(&telemetry, "m-a", "orch-a", "oa");
        let b = telemetry
            .prepare_run(generation("orch-b", "ob"), &[], None)
            .unwrap();
        b.commit();
        post_task(&telemetry, "m-b", "orch-b", "ob");
        resume.send(()).unwrap();
        nudge.await.unwrap();
        telemetry.shutdown(Duration::from_secs(2)).await;
        let spans = captured(&mut rx);
        assert_eq!(handoff_spans(&spans).len(), 2, "{spans:?}");
        assert_child_of(handoff(&spans, "m-a"), run_on(&spans, "orch-a"));
        assert_child_of(handoff(&spans, "m-b"), run_on(&spans, "orch-b"));
    }

    /// Issue #1981 M7 review: a post placed while its orchestrator's run is
    /// prepared — by a broadcast message on another pane, with no close —
    /// waits in that run and is opened as the run opens, as its child; the
    /// run's later close ends it `unknown`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_post_held_for_a_prepared_run_keeps_the_run_as_parent() {
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let (tx, events) = broadcast::channel(16);
        telemetry.subscribe(events);
        begin(&telemetry, generation("work", "wa"), &[]);
        telemetry.settled().await;
        let (resume, nudge) = pause_ingest(&telemetry).await;
        let orch = telemetry
            .prepare_run(generation("orch", "oa"), &[], None)
            .unwrap();
        orch.commit();
        post_task(&telemetry, "m-o", "orch", "oa");
        tx.send(of("work", "wa", EventType::SessionStart)).unwrap();
        resume.send(()).unwrap();
        nudge.await.unwrap();
        telemetry.settled().await;
        assert_eq!(open_handoffs(&telemetry), 1);
        tx.send(pane_closed("orch", "oa")).unwrap();
        telemetry.settled().await;
        let spans = captured(&mut rx);
        let handoff = handoff(&spans, "m-o");
        let run = run_on(&spans, "orch");
        assert_child_of(handoff, run);
        assert_eq!(
            attr(handoff, AttrKey::HandoffOutcome),
            Some(&AttrValue::Str("unknown".into()))
        );
    }

    /// Issue #1981 M7 review: tasks posted from two orchestrators whose runs
    /// are both prepared, interleaved, each run's close broadcast after
    /// every post: each hand-off opens in its own orchestrator's run, before
    /// that run's close ends it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn posts_against_two_prepared_runs_each_open_in_their_own_run() {
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let (tx, events) = broadcast::channel(16);
        telemetry.subscribe(events);
        begin(&telemetry, generation("work", "wa"), &[]);
        telemetry.settled().await;
        let (resume, nudge) = pause_ingest(&telemetry).await;
        let one = telemetry
            .prepare_run(generation("orch-1", "o1"), &[], None)
            .unwrap();
        one.commit();
        post_task(&telemetry, "m-1a", "orch-1", "o1");
        let two = telemetry
            .prepare_run(generation("orch-2", "o2"), &[], None)
            .unwrap();
        two.commit();
        post_task(&telemetry, "m-2a", "orch-2", "o2");
        post_task(&telemetry, "m-1b", "orch-1", "o1");
        post_task(&telemetry, "m-2b", "orch-2", "o2");
        tx.send(pane_closed("orch-1", "o1")).unwrap();
        tx.send(pane_closed("orch-2", "o2")).unwrap();
        resume.send(()).unwrap();
        nudge.await.unwrap();
        telemetry.settled().await;
        let spans = captured(&mut rx);
        assert_eq!(handoff_spans(&spans).len(), 4, "{spans:?}");
        for (id, pane) in [
            ("m-1a", "orch-1"),
            ("m-1b", "orch-1"),
            ("m-2a", "orch-2"),
            ("m-2b", "orch-2"),
        ] {
            let handoff = handoff(&spans, id);
            let run = run_on(&spans, pane);
            assert_child_of(handoff, run);
            assert_eq!(
                attr(handoff, AttrKey::HandoffOutcome),
                Some(&AttrValue::Str("unknown".into())),
                "{id}"
            );
            assert_eq!(handoff.end_unix_nanos, run.end_unix_nanos, "{id}");
        }
    }

    /// Issue #1981 M7 review: the daemon shuts down while a post waits in
    /// its orchestrator's run, prepared and never opened. The hand-off is
    /// not lost: it ends `daemon_shutdown`, naming the run, with no run span
    /// to parent it.
    #[tokio::test]
    async fn shutdown_ends_a_post_waiting_on_a_prepared_run() {
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let (_tx, events) = broadcast::channel::<BroadcastMsg>(16);
        telemetry.subscribe(events);
        begin(&telemetry, generation("work", "wa"), &[]);
        let orch = telemetry
            .prepare_run(generation("orch", "oa"), &[], None)
            .unwrap();
        post_task(&telemetry, "m-o", "orch", "oa");
        telemetry.settled().await;
        assert_eq!(
            open_handoffs(&telemetry),
            0,
            "the post waits in the prepared run"
        );
        telemetry.shutdown(Duration::from_secs(2)).await;
        let spans = captured(&mut rx);
        assert_eq!(handoff_spans(&spans).len(), 1, "{spans:?}");
        let handoff = handoff(&spans, "m-o");
        assert_eq!(
            attr(handoff, AttrKey::HandoffOutcome),
            Some(&AttrValue::Str("daemon_shutdown".into()))
        );
        assert_eq!(
            attr(handoff, AttrKey::RunId),
            Some(&AttrValue::Str(orch.run_id.clone()))
        );
        assert_eq!(handoff.parent_span_id, None);
        drop(orch);
    }

    /// An inbox change to message `id` in `pane`'s inbox, as broadcast.
    fn inbox(pane: &str, id: &str, state: crate::inbox::StateName) -> BroadcastMsg {
        BroadcastMsg::Inbox(crate::event::InboxNotice {
            pane_id: pane.into(),
            agent_id: None,
            message_id: id.into(),
            subject: String::new(),
            from_label: String::new(),
            state,
            reason: None,
            returned: false,
            wake_stalled: false,
            wake_update: false,
            wake_hold: None,
            hold_detail: None,
            attention_update: false,
            requeued: false,
            provenance: None,
            termination: None,
            notices: Default::default(),
        })
    }

    /// Report `report` naming task `task`, queued in `orch`'s inbox by the
    /// worker `work`/`wa`.
    fn report_for(orch: &str, report: &str, task: &str) -> BroadcastMsg {
        let BroadcastMsg::Inbox(mut notice) = inbox(orch, report, crate::inbox::StateName::Queued)
        else {
            unreachable!()
        };
        notice.provenance = Some(crate::report_recovery::ReportProvenance::Task {
            task_id: task.into(),
            report_id: "r".into(),
            producer: crate::report_recovery::Producer {
                pane_id: "work".into(),
                agent_id: "wa".into(),
                role: Some("coder".into()),
            },
        });
        BroadcastMsg::Inbox(notice)
    }

    /// With the ingest task paused, the orchestrator's run is prepared and
    /// its commit queued, task `m-o` is posted, and `before` then the
    /// orchestrator's pane-close then `after` are broadcast. The spans once
    /// it resumes.
    async fn around_a_held_close(
        before: Vec<BroadcastMsg>,
        after: Vec<BroadcastMsg>,
    ) -> Vec<SpanRecord> {
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let (tx, events) = broadcast::channel(16);
        telemetry.subscribe(events);
        begin(&telemetry, generation("work", "wa"), &[]);
        telemetry.settled().await;
        let (resume, nudge) = pause_ingest(&telemetry).await;
        let orch = telemetry
            .prepare_run(generation("orch", "oa"), &[], None)
            .unwrap();
        orch.commit();
        post_task(&telemetry, "m-o", "orch", "oa");
        for msg in before {
            tx.send(msg).unwrap();
        }
        tx.send(pane_closed("orch", "oa")).unwrap();
        for msg in after {
            tx.send(msg).unwrap();
        }
        resume.send(()).unwrap();
        nudge.await.unwrap();
        telemetry.settled().await;
        captured(&mut rx)
    }

    /// PR #2010 review: with the ingest task paused, the orchestrator's run
    /// is prepared and its commit queued, and a report naming no task then
    /// reaches the orchestrator, before its pane-close. Resumed, the report
    /// is counted on the run, in its place before the close.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_report_naming_no_task_before_a_queued_commit_counts_on_the_run() {
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let (tx, events) = broadcast::channel(16);
        telemetry.subscribe(events);
        begin(&telemetry, generation("work", "wa"), &[]);
        telemetry.settled().await;
        let (resume, nudge) = pause_ingest(&telemetry).await;
        let orch = telemetry
            .prepare_run(generation("orch", "oa"), &[], None)
            .unwrap();
        orch.commit();
        let BroadcastMsg::Inbox(mut notice) = inbox("orch", "m-n", crate::inbox::StateName::Queued)
        else {
            unreachable!()
        };
        notice.agent_id = Some("oa".into());
        notice.provenance = Some(crate::report_recovery::ReportProvenance::Uncorrelated {
            report_id: "r".into(),
            producer: crate::report_recovery::Producer {
                pane_id: "work".into(),
                agent_id: "wa".into(),
                role: None,
            },
        });
        tx.send(BroadcastMsg::Inbox(notice)).unwrap();
        tx.send(pane_closed("orch", "oa")).unwrap();
        resume.send(()).unwrap();
        nudge.await.unwrap();
        telemetry.settled().await;
        let spans = captured(&mut rx);
        assert_eq!(
            attr(run_on(&spans, "orch"), AttrKey::HandoffsUncorrelatedCount),
            Some(&AttrValue::Int(1)),
            "{spans:?}"
        );
    }

    fn event_names(span: &SpanRecord) -> Vec<&'static str> {
        span.events.iter().map(|e| e.name).collect()
    }

    /// Issue #1981 M7 review (the reviewer's
    /// `review_prepared_post_ignores_report_after_orchestrator_close`): a
    /// task posted while its orchestrator's run is prepared, the run's
    /// pane-close, then the worker's report naming the task, all applied as
    /// the run opens. The report came after the run ended, so the hand-off
    /// ends `unknown` with the run, with no report event.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_report_after_a_prepared_runs_held_close_finds_the_handoff_ended() {
        let spans = around_a_held_close(vec![], vec![report_for("orch", "m-r", "m-o")]).await;
        assert_eq!(handoff_spans(&spans).len(), 1, "{spans:?}");
        let handoff = handoff(&spans, "m-o");
        let run = run_on(&spans, "orch");
        assert_child_of(handoff, run);
        assert_eq!(
            attr(handoff, AttrKey::HandoffOutcome),
            Some(&AttrValue::Str("unknown".into()))
        );
        assert_eq!(handoff.end_unix_nanos, run.end_unix_nanos);
        assert_eq!(event_names(handoff), Vec::<&str>::new());
    }

    /// Issue #1981 M7 review: `notice`, about task `m-o`, broadcast after the
    /// prepared orchestrator run's held close, is applied after that close:
    /// the hand-off ends `unknown` with the run, with nothing taken and no
    /// other outcome.
    async fn assert_a_task_notice_after_a_held_close_finds_the_handoff_ended(notice: BroadcastMsg) {
        let spans = around_a_held_close(vec![], vec![notice]).await;
        assert_eq!(handoff_spans(&spans).len(), 1, "{spans:?}");
        let handoff = handoff(&spans, "m-o");
        let run = run_on(&spans, "orch");
        assert_child_of(handoff, run);
        assert_eq!(
            attr(handoff, AttrKey::HandoffOutcome),
            Some(&AttrValue::Str("unknown".into()))
        );
        assert_eq!(handoff.end_unix_nanos, run.end_unix_nanos);
        assert_eq!(event_names(handoff), Vec::<&str>::new());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_fetch_after_a_prepared_runs_held_close_finds_the_handoff_ended() {
        assert_a_task_notice_after_a_held_close_finds_the_handoff_ended(inbox(
            "work",
            "m-o",
            crate::inbox::StateName::Fetched,
        ))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_acknowledgement_after_a_prepared_runs_held_close_finds_the_handoff_ended() {
        assert_a_task_notice_after_a_held_close_finds_the_handoff_ended(inbox(
            "work",
            "m-o",
            crate::inbox::StateName::Acknowledged,
        ))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_undeliverable_after_a_prepared_runs_held_close_finds_the_handoff_ended() {
        let BroadcastMsg::Inbox(mut notice) =
            inbox("work", "m-o", crate::inbox::StateName::Undeliverable)
        else {
            unreachable!()
        };
        notice.reason = Some(crate::inbox::UndeliverableReason::AgentExited);
        assert_a_task_notice_after_a_held_close_finds_the_handoff_ended(BroadcastMsg::Inbox(
            notice,
        ))
        .await;
    }

    /// Issue #1981 M7 review: the worker's report on a task held for a
    /// prepared orchestrator run arrives before the run's held close: the
    /// run had not ended, so the hand-off ends `reported`, with the report's
    /// event, at the run's end; the orchestrator's acknowledgement after the
    /// close does not move that end.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_report_before_a_prepared_runs_held_close_ends_the_handoff_reported() {
        use crate::inbox::StateName;
        let spans = around_a_held_close(
            vec![
                inbox("work", "m-o", StateName::Acknowledged),
                report_for("orch", "m-r", "m-o"),
            ],
            vec![inbox("orch", "m-r", StateName::Acknowledged)],
        )
        .await;
        assert_eq!(handoff_spans(&spans).len(), 1, "{spans:?}");
        let handoff = handoff(&spans, "m-o");
        let run = run_on(&spans, "orch");
        assert_child_of(handoff, run);
        assert_eq!(
            attr(handoff, AttrKey::HandoffOutcome),
            Some(&AttrValue::Str("reported".into()))
        );
        assert_eq!(handoff.end_unix_nanos, run.end_unix_nanos);
        assert_eq!(
            event_names(handoff),
            vec![
                crate::telemetry::EVENT_HANDOFF_TAKEN,
                crate::telemetry::EVENT_HANDOFF_REPORTED
            ]
        );
        assert!(
            handoff
                .events
                .iter()
                .all(|e| e.time_unix_nanos <= run.end_unix_nanos),
            "{handoff:?}"
        );
    }

    /// Issue #1981 M7 review: a report that overtakes its task's post, then
    /// the post held for its prepared orchestrator run, the run's close, and
    /// the orchestrator acknowledging the report: the acknowledgement came
    /// after the close, so the hand-off ends `reported` at the run's end,
    /// not at the acknowledgement.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_acknowledgement_after_a_held_close_of_a_report_that_overtook_its_post_stays_after_it()
     {
        use crate::inbox::StateName;
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let (tx, events) = broadcast::channel(16);
        telemetry.subscribe(events);
        begin(&telemetry, generation("work", "wa"), &[]);
        telemetry.settled().await;
        let (resume, nudge) = pause_ingest(&telemetry).await;
        let orch = telemetry
            .prepare_run(generation("orch", "oa"), &[], None)
            .unwrap();
        orch.commit();
        tx.send(report_for("orch", "m-r", "m-o")).unwrap();
        post_task(&telemetry, "m-o", "orch", "oa");
        tx.send(pane_closed("orch", "oa")).unwrap();
        tx.send(inbox("orch", "m-r", StateName::Acknowledged))
            .unwrap();
        resume.send(()).unwrap();
        nudge.await.unwrap();
        telemetry.settled().await;
        let spans = captured(&mut rx);
        assert_eq!(handoff_spans(&spans).len(), 1, "{spans:?}");
        let handoff = handoff(&spans, "m-o");
        let run = run_on(&spans, "orch");
        assert_child_of(handoff, run);
        assert_eq!(
            attr(handoff, AttrKey::HandoffOutcome),
            Some(&AttrValue::Str("reported".into()))
        );
        assert_eq!(handoff.end_unix_nanos, run.end_unix_nanos);
    }

    /// Issue #1981 M7: the delegate seam and the inbox's broadcast make one
    /// `agent.handoff` through the whole exporter, in the orchestrator's
    /// trace, carrying none of the task's text.
    #[tokio::test]
    async fn a_posted_task_becomes_a_handoff_through_the_broadcast() {
        use crate::inbox::StateName;
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let (tx, events) = broadcast::channel(16);
        telemetry.subscribe(events);
        begin(&telemetry, generation("orch", "oa"), &[]);
        begin(&telemetry, generation("work", "wa"), &[]);
        let notice = |pane: &str, id: &str, state| crate::event::InboxNotice {
            pane_id: pane.into(),
            agent_id: None,
            message_id: id.into(),
            subject: "SENTINEL-subject".into(),
            from_label: "SENTINEL-label".into(),
            state,
            reason: None,
            returned: false,
            wake_stalled: false,
            wake_update: false,
            wake_hold: None,
            hold_detail: None,
            attention_update: false,
            requeued: false,
            provenance: None,
            termination: None,
            notices: Default::default(),
        };
        // The worker takes the task before telemetry hears of the post.
        tx.send(BroadcastMsg::Inbox(notice(
            "work",
            "m-t",
            StateName::Acknowledged,
        )))
        .unwrap();
        crate::telemetry::observe_delegate_posted(
            Some(Arc::clone(&telemetry)),
            &crate::telemetry::DelegatePosted {
                message_id: "m-t",
                orchestrator_pane: "orch",
                orchestrator_agent_id: Some("oa"),
                worker_pane: "work",
                worker_agent_id: "wa",
                role: "coder",
            },
        );
        let mut report = notice("orch", "m-r", StateName::Queued);
        report.provenance = Some(crate::report_recovery::ReportProvenance::Task {
            task_id: "m-t".into(),
            report_id: "r".into(),
            producer: crate::report_recovery::Producer {
                pane_id: "work".into(),
                agent_id: "wa".into(),
                role: Some("coder".into()),
            },
        });
        tx.send(BroadcastMsg::Inbox(report)).unwrap();
        tx.send(BroadcastMsg::Inbox(notice(
            "orch",
            "m-r",
            StateName::Acknowledged,
        )))
        .unwrap();
        telemetry.settled().await;
        let spans = captured(&mut rx);
        let handoffs: Vec<_> = spans
            .iter()
            .filter(|s| s.name == crate::telemetry::SPAN_AGENT_HANDOFF)
            .collect();
        assert_eq!(handoffs.len(), 1, "{spans:?}");
        let handoff = handoffs[0];
        assert_eq!(
            attr(handoff, AttrKey::HandoffOutcome),
            Some(&AttrValue::Str("reported".into()))
        );
        assert_eq!(
            handoff.events.iter().map(|e| e.name).collect::<Vec<_>>(),
            vec![
                crate::telemetry::EVENT_HANDOFF_TAKEN,
                crate::telemetry::EVENT_HANDOFF_REPORTED
            ]
        );
        telemetry.end_run("orch", Some("oa"), EndReason::Exited, None);
        telemetry.settled().await;
        let spans = captured(&mut rx);
        let run = spans.iter().find(|s| s.name == SPAN_AGENT_RUN).unwrap();
        assert_eq!(handoff.trace_id, run.trace_id);
        assert_eq!(handoff.parent_span_id, Some(run.span_id));
        assert!(!format!("{handoff:?}").contains("SENTINEL"));
    }
}
