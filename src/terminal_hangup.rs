//! Issue #1138 — end the TUI when its terminal hangs up, instead of spinning
//! forever on a descriptor with nothing left to read.
//!
//! ## The defect this exists for
//!
//! When the terminal hangs up, the kernel signals the **session leader** and
//! nobody else. That is fine when the deck IS the session leader — SIGHUP's
//! default disposition ends it, measured at ~1s. It is the other shape that
//! costs a core: `ssh -t host task run`, or a tmux pane whose command is
//! `task`, leaves an intermediate process as the session leader, and Task 3.53
//! catches SIGHUP and forwards nothing. The deck is then a child that receives
//! **no signal at all** and has to notice for itself.
//!
//! It did not. Reproduced on Linux against the real binary: with the deck a
//! child of a `trap "" HUP` session leader, closing the pseudo-terminal master
//! left it running at **100% of a core indefinitely** — measured over 10s, ten
//! samples, never below 99%.
//!
//! ## Why the event loop cannot notice this by itself
//!
//! `run_tui` propagates terminal I/O errors with `?`, so the obvious theory is
//! that a hangup surfaces as an `Err` from `crossterm::event::poll` and the
//! existing error path handles it. It does not, and the reason is worth writing
//! down because it rules out every in-loop fix.
//!
//! Crossterm's Unix event source is [the mio one] unless `use-dev-tty` is
//! enabled, and on a readable descriptor it runs:
//!
//! ```text
//! loop {
//!     match self.tty_fd.read(&mut self.tty_buffer) {
//!         Ok(read_count) => { if read_count > 0 { self.parser.advance(..) } }
//!         Err(e) => { if WouldBlock { break } else if Interrupted { continue } }
//!     };
//!     if let Some(event) = self.parser.next() { return Ok(Some(event)); }
//! }
//! ```
//!
//! There is **no break on `Ok(0)`**. A hung-up terminal polls readable forever
//! and reads 0 bytes forever, so that loop never terminates: `event::poll` does
//! not return `false`, and does not return an error — it does not return.
//! Measured with `strace` on the reproduction above: 19,499 `read(0, "", 1024)
//! = 0` calls in a single 20,000-line window, with no `poll` between them.
//!
//! So the deck's event loop is wedged inside library code the moment the
//! terminal goes away, and no check placed after a `poll`/`read` call can run.
//! Detection has to happen on **another thread**, which is what this module is.
//!
//! [the mio one]: https://docs.rs/crossterm/0.29.0/src/crossterm/event/source/unix/mio.rs.html
//!
//! ## What it does
//!
//! One thread blocks in `poll(2)` on the same descriptor crossterm reads, with
//! an `events` mask of **zero**. That mask is the whole trick: `POLLHUP`,
//! `POLLERR` and `POLLNVAL` are reported whether or not they were requested,
//! while `POLLIN` is not — so the watchdog wakes on a hangup and stays asleep
//! through ordinary typing. It never reads, so it cannot steal a keystroke from
//! the event loop. Measured on the pseudo-terminal this module's tests build: a
//! live slave returns `0` (timeout) with `revents == 0`, and one whose master
//! has closed returns immediately with `POLLERR | POLLHUP`.
//!
//! The policy on detection lives in [`crate::ui::run_tui`], not here, because
//! it ends the process and so cannot be unit-tested: set a flag the event loop
//! checks, wait [`ACK_WINDOW`] for it to acknowledge, and end the process
//! either way.
//!
//! **Be clear about which half fires.** On crossterm 0.29's mio source the
//! event loop is wedged and the clean half does not win: measured over ten
//! reproductions, with and without input bursts, the loop acknowledged **zero**
//! times and nine ended on the watchdog's own exit. (The tenth ended in 20ms by
//! a different route — a `?` on terminal I/O that did fail, which is the
//! pre-existing error path issue #1162 covers.) The flag check is kept anyway
//! because it costs one relaxed load per iteration and it is the only thing
//! that would end a deck-LEVEL spin: a crossterm that breaks on `Ok(0)`, or the
//! `use-dev-tty` source, returns `false` forever instead of not returning, and
//! nothing else in the loop would notice.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// How long one probe blocks before looping to re-check the stop flag.
///
/// This is the watchdog's idle cost and its shutdown latency, nothing else — a
/// hangup wakes the poll immediately rather than waiting this out.
const PROBE_INTERVAL: Duration = Duration::from_millis(250);

/// How long the watchdog waits for the event loop to **acknowledge** the
/// hangup before it gives up on it and ends the process.
///
/// This is the bound on how long the deck keeps spinning after its terminal
/// disappears, so it is the number the reported symptom is measured against.
/// The loop checks its flag once per iteration — ~16ms when idle — so 500ms is
/// about thirty chances to take a flag it will take on the first one if it is
/// going to take it at all, and a loop that is measurably wedged is not worth
/// burning a core for any longer than that.
pub const ACK_WINDOW: Duration = Duration::from_millis(500);

/// How long an event loop that DID acknowledge the hangup is then given to
/// finish its teardown.
///
/// Spent only after an acknowledgement, so it is never the cost of the wedged
/// case. An acknowledged teardown writes the session snapshot and drops the
/// attach sockets, which is worth waiting on; it is not worth waiting longer,
/// because the terminal it would render to is gone.
pub const UNWIND_GRACE: Duration = Duration::from_secs(2);

/// The status the deck exits with when a hangup ends it.
///
/// 128 + `SIGHUP`, the conventional code for "died of a hangup" — which is
/// exactly what the shell reports for the *other* shape of this scenario, where
/// the deck is the session leader and the kernel's SIGHUP ends it. Both shapes
/// therefore report the same thing.
pub const HANGUP_EXIT_CODE: i32 = 129;

/// What one probe of the terminal found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtyState {
    /// Still attached — including "attached with input pending", which this
    /// probe deliberately cannot distinguish from "attached and idle".
    Live,
    /// Hung up: `POLLHUP`, `POLLERR` or `POLLNVAL`.
    HungUp,
}

/// Stops the watchdog thread when dropped.
///
/// Dropping does **not** join: the thread sits in a `poll(2)` of up to
/// [`PROBE_INTERVAL`], and making every ordinary quit pay that is a worse trade
/// than letting a thread with nothing left to do outlive the teardown by a
/// quarter second. It re-reads the stop flag after the poll returns and before
/// it would call back, so a hangup racing a clean shutdown does not fire the
/// callback of a `run_tui` that has already returned.
pub struct HangupWatch {
    stop: Arc<AtomicBool>,
}

impl Drop for HangupWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Wait up to `timeout` for `flag` to be set, and report whether it was.
///
/// Polled rather than parked on a condvar: the two sides are a watchdog thread
/// and an event loop that must not take a lock per iteration, and the whole
/// wait is bounded by [`ACK_WINDOW`].
pub fn wait_for(flag: &AtomicBool, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if flag.load(Ordering::SeqCst) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Probe `fd` for a hangup, blocking for at most `timeout`.
///
/// Non-destructive by construction: the `events` mask is zero, so this consumes
/// no input and does not even wake on any. See the module docs for why that
/// matters — the event loop is reading the same descriptor.
#[cfg(unix)]
pub fn probe(fd: std::os::fd::RawFd, timeout: Duration) -> std::io::Result<TtyState> {
    let mut pollfd = libc::pollfd {
        fd,
        events: 0,
        revents: 0,
    };
    let millis = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    // SAFETY: `pollfd` is a single initialised struct and the count matches.
    // `poll` writes only `revents`, and borrows nothing past the call.
    let rc = unsafe { libc::poll(&mut pollfd, 1, millis) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if rc > 0 && pollfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
        return Ok(TtyState::HungUp);
    }
    Ok(TtyState::Live)
}

/// The descriptor crossterm's Unix event source reads from.
///
/// Chosen exactly the way `crossterm::terminal::sys::file_descriptor::tty_fd`
/// chooses it — stdin when it is a terminal, `/dev/tty` otherwise — because
/// watching a *different* descriptor than the one that wedges would be watching
/// the wrong thing. The `/dev/tty` arm owns its `File` so the descriptor stays
/// open for as long as the watchdog thread holds it.
#[cfg(unix)]
enum WatchedTty {
    Stdin,
    Owned(std::fs::File),
}

#[cfg(unix)]
impl WatchedTty {
    fn open() -> std::io::Result<Self> {
        // SAFETY: `isatty` reads kernel state for a descriptor number and
        // touches no memory of ours.
        if unsafe { libc::isatty(libc::STDIN_FILENO) } == 1 {
            return Ok(Self::Stdin);
        }
        Ok(Self::Owned(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/tty")?,
        ))
    }

    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd as _;
        match self {
            Self::Stdin => libc::STDIN_FILENO,
            Self::Owned(file) => file.as_raw_fd(),
        }
    }
}

/// Watch `tty` until it hangs up, then call `on_hangup` exactly once.
///
/// The thread owns `tty`, so an `Owned` descriptor cannot be closed out from
/// under the poll.
#[cfg(unix)]
fn spawn_watch<F>(tty: WatchedTty, on_hangup: F) -> HangupWatch
where
    F: FnOnce() + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let spawned = std::thread::Builder::new()
        .name("dad-tty-hangup".to_string())
        .spawn(move || {
            let fd = tty.as_raw_fd();
            while !thread_stop.load(Ordering::SeqCst) {
                match probe(fd, PROBE_INTERVAL) {
                    Ok(TtyState::Live) => {}
                    Ok(TtyState::HungUp) => {
                        // Re-read the flag: `run_tui` may have returned between
                        // the poll waking and now, in which case there is
                        // nothing left to rescue and the callback ends a
                        // process that is already on its way out.
                        if thread_stop.load(Ordering::SeqCst) {
                            return;
                        }
                        on_hangup();
                        return;
                    }
                    // A signal interrupted the wait; that says nothing about
                    // the terminal, so go round again.
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    // Anything else means the probe itself is unusable. Stop
                    // rather than guess: ending the deck on an unreadable
                    // `poll` result would be worse than the spin it prevents.
                    Err(e) => {
                        tracing::warn!(error = %e, "terminal hangup watchdog stopping: poll failed");
                        return;
                    }
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "could not start the terminal hangup watchdog");
    }
    HangupWatch { stop }
}

/// Start the watchdog on the terminal the event loop reads.
///
/// Returns `None` when there is no terminal to watch — stdin is not one and
/// `/dev/tty` cannot be opened — which is the case for a deck whose input is
/// redirected, where the scenario this guards against cannot arise.
#[cfg(unix)]
pub fn watch_controlling_terminal<F>(on_hangup: F) -> Option<HangupWatch>
where
    F: FnOnce() + Send + 'static,
{
    match WatchedTty::open() {
        Ok(tty) => Some(spawn_watch(tty, on_hangup)),
        Err(e) => {
            tracing::debug!(error = %e, "no terminal to watch for hangup");
            None
        }
    }
}

/// Windows has no `poll(2)` on a console handle and no session-leader shape for
/// the defect to take: the L2 tier this was reproduced in is Unix-only, and the
/// deck's Windows console input does not wedge the way crossterm's Unix mio
/// source does. Nothing is watched there.
#[cfg(not(unix))]
pub fn watch_controlling_terminal<F>(_on_hangup: F) -> Option<HangupWatch>
where
    F: FnOnce() + Send + 'static,
{
    None
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::sync::mpsc;

    /// A pseudo-terminal pair, as `tests/wrap_io.rs::open_pty` builds one.
    fn open_pty() -> (std::fs::File, std::fs::File) {
        let mut master = -1;
        let mut slave = -1;
        // SAFETY: `openpty` initialises both descriptors on success, and each
        // is turned into an owning `File` exactly once below.
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(
            rc,
            0,
            "open pseudo-terminal: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: both descriptors are freshly opened and owned by this test.
        unsafe {
            (
                std::fs::File::from_raw_fd(master),
                std::fs::File::from_raw_fd(slave),
            )
        }
    }

    /// Issue #1138 — the detection primitive, against a real pseudo-terminal.
    /// A live slave must read as `Live` *including while input is pending*,
    /// since a probe that reported a hangup on unread bytes would end the deck
    /// on every keystroke.
    #[test]
    fn probe_reports_a_live_terminal_as_live() {
        let (mut master, slave) = open_pty();
        assert_eq!(
            probe(slave.as_raw_fd(), Duration::from_millis(50)).expect("probe an idle slave"),
            TtyState::Live
        );

        use std::io::Write as _;
        master.write_all(b"hello").expect("write to the master");
        master.flush().expect("flush the master");
        assert_eq!(
            probe(slave.as_raw_fd(), Duration::from_millis(50))
                .expect("probe a slave with input pending"),
            TtyState::Live,
            "pending input is not a hangup — an `events` mask of zero is what \
             keeps this probe blind to readability"
        );
    }

    /// Issue #1138 — the same probe against the same descriptor once the
    /// master has closed, which is what a terminal hanging up IS. It must
    /// report `HungUp` promptly rather than blocking out the timeout.
    #[test]
    fn probe_reports_a_hung_up_terminal_as_hung_up() {
        let (master, slave) = open_pty();
        drop(master);
        let started = std::time::Instant::now();
        assert_eq!(
            probe(slave.as_raw_fd(), Duration::from_secs(5)).expect("probe a hung-up slave"),
            TtyState::HungUp
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a hangup is already pending, so the probe must return at once \
             rather than wait its timeout out (took {:?})",
            started.elapsed()
        );
    }

    /// Issue #1138 — the watchdog calls back when the terminal it is watching
    /// hangs up, and does so from a thread, which is the whole point: the
    /// event loop is wedged in crossterm and cannot do this for itself.
    #[test]
    fn watch_calls_back_when_the_terminal_hangs_up() {
        let (master, slave) = open_pty();
        let (tx, rx) = mpsc::channel();
        let _watch = spawn_watch(WatchedTty::Owned(slave), move || {
            let _ = tx.send(());
        });

        // Still attached: nothing may fire.
        assert!(
            rx.recv_timeout(PROBE_INTERVAL * 3).is_err(),
            "the watchdog fired while the terminal was still attached"
        );

        drop(master);
        rx.recv_timeout(Duration::from_secs(10))
            .expect("the watchdog must call back once the master closes");
    }

    /// Issue #1138 — the acknowledgement wait returns as soon as the event
    /// loop sets its flag, and reports `false` rather than blocking forever
    /// when the loop is wedged and never will.
    #[test]
    fn wait_for_reports_whether_the_flag_was_taken() {
        let flag = Arc::new(AtomicBool::new(false));
        let started = std::time::Instant::now();
        assert!(
            !wait_for(&flag, Duration::from_millis(60)),
            "a flag nobody sets must time out"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(50),
            "and must actually have waited"
        );

        let setter = Arc::clone(&flag);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            setter.store(true, Ordering::SeqCst);
        });
        assert!(
            wait_for(&flag, Duration::from_secs(10)),
            "a flag set during the wait must be observed"
        );
    }

    /// Issue #1138 — dropping the handle stops the watchdog, so a hangup that
    /// arrives after `run_tui` has already returned cannot end a process that
    /// is on its way out cleanly.
    #[test]
    fn dropping_the_handle_stops_the_watchdog() {
        let (master, slave) = open_pty();
        let (tx, rx) = mpsc::channel();
        let watch = spawn_watch(WatchedTty::Owned(slave), move || {
            let _ = tx.send(());
        });
        drop(watch);
        // Give the thread time to observe the stop flag before the hangup.
        std::thread::sleep(PROBE_INTERVAL * 2);
        drop(master);
        assert!(
            rx.recv_timeout(PROBE_INTERVAL * 4).is_err(),
            "a stopped watchdog must not call back"
        );
    }
}
