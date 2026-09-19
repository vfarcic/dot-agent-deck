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
//! So the deck's event loop wedges inside library code on the first poll of a
//! terminal with nothing left to deliver — on an idle deck, immediately. (Not
//! *instantly* in every case: with bytes still buffered, crossterm parses and
//! returns those events first, and the wedge is the poll after the buffer
//! drains. One reproduction in ten ended that way, by a `?` on terminal I/O
//! that did fail before the loop got there.) Once it has wedged, nothing placed
//! after a `poll`/`read` call runs, so detection has to happen on **another
//! thread** — which is what this module is.
//!
//! [the mio one]: https://docs.rs/crossterm/0.29.0/src/crossterm/event/source/unix/mio.rs.html
//!
//! ## What it does
//!
//! One thread blocks in `poll(2)` on the same descriptor crossterm reads, and
//! classifies a hangup from `POLLHUP | POLLERR | POLLNVAL`. It never reads, so
//! it cannot steal a keystroke from the event loop.
//!
//! **The `events` mask is `POLLIN`, and the reason is a portability bug this
//! got wrong first.** POSIX says `POLLHUP`, `POLLERR` and `POLLNVAL` are set in
//! `revents` whether or not they were requested, so an `events` mask of **zero**
//! looks ideal: it would wake on a hangup and stay asleep through ordinary
//! typing. On Linux it does exactly that — measured, a live pseudo-terminal
//! slave returns `0` (timeout) with `revents == 0` and a hung-up one returns at
//! once with `POLLERR | POLLHUP`. **On macOS it does not.** `build-macos`
//! measured `Live` against a slave whose master had closed, so with a zero mask
//! the watchdog would simply never fire there and this would have shipped as a
//! Linux-only fix — Apple's `poll(2)` is kqueue-backed and famously "does not
//! support devices" well, and with nothing requested it registers no filter and
//! so reports no end-of-file. Requesting `POLLIN` makes the hangup observable
//! on both; it was already the verified-on-Linux spelling, since the original
//! reproduction of this defect polled with `POLLIN` and read back
//! `POLLIN | POLLERR | POLLHUP`.
//!
//! The cost of that mask is that the probe now also wakes on ordinary unread
//! input, which would be a spin — the very symptom this module exists to stop —
//! if the loop went straight back into `poll`. [`iteration_pause`] is the floor
//! that stops it, and `watch_does_not_spin_on_a_terminal_with_unread_input`
//! is the regression test.
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
///
/// `pub` alongside the other three, rather than private: [`HangupWatch`]'s
/// contract is stated in terms of it, and on a non-Unix build nothing reads it,
/// which a private constant reports as dead code under `-D warnings`.
pub const PROBE_INTERVAL: Duration = Duration::from_millis(250);

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
/// 128 + `SIGHUP`, the conventional code for "died of a hangup" — and measured
/// to be what a shell reports for the *other* shape of this scenario, where the
/// deck is the session leader and the kernel's SIGHUP ends it on the default
/// disposition. Both shapes therefore report the same thing.
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
/// it would call back, which narrows — but does not close — the window in which
/// a hangup racing a clean shutdown fires the callback of a `run_tui` that has
/// already returned. Both outcomes end the process, so what the residual race
/// costs is the exit status, not the shutdown.
pub struct HangupWatch {
    stop: Arc<AtomicBool>,
    probes: Arc<std::sync::atomic::AtomicUsize>,
}

impl HangupWatch {
    /// How many times the watchdog has polled the terminal so far.
    ///
    /// Observability, and the only way to assert the thing [`iteration_pause`]
    /// protects: a watchdog that polls thousands of times a second is spinning,
    /// whether or not it ever calls back.
    pub fn probe_count(&self) -> usize {
        self.probes.load(Ordering::SeqCst)
    }
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

/// One `poll(2)` on `fd`, returning its result and the `revents` it produced.
///
/// Split out from [`probe`] so a failing test can report the numbers the kernel
/// actually gave it rather than only the classification — which is what a
/// platform whose `poll` disagrees with this one's assumptions looks like, and
/// exactly how the macOS behaviour in the module docs was missed the first time.
#[cfg(unix)]
fn poll_once(fd: std::os::fd::RawFd, timeout: Duration) -> std::io::Result<(i32, libc::c_short)> {
    let mut pollfd = libc::pollfd {
        fd,
        // Not zero — see the module docs. macOS reports no hangup for an
        // unrequested event on a pseudo-terminal.
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    // SAFETY: `pollfd` is a single initialised struct and the count matches.
    // `poll` writes only `revents`, and borrows nothing past the call.
    let rc = unsafe { libc::poll(&mut pollfd, 1, millis) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((rc, pollfd.revents))
}

/// Whether `revents` from a `poll` that returned `rc` describes a hangup.
#[cfg(unix)]
fn classify(rc: i32, revents: libc::c_short) -> TtyState {
    if rc > 0 && revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
        TtyState::HungUp
    } else {
        TtyState::Live
    }
}

/// Probe `fd` for a hangup, blocking for at most `timeout`.
///
/// Non-destructive: this polls and never reads, so it consumes no input. It
/// does *wake* on input, which is what [`iteration_pause`] exists to absorb.
#[cfg(unix)]
pub fn probe(fd: std::os::fd::RawFd, timeout: Duration) -> std::io::Result<TtyState> {
    let (rc, revents) = poll_once(fd, timeout)?;
    Ok(classify(rc, revents))
}

/// How long a watchdog iteration should pause after a `Live` probe.
///
/// With `POLLIN` requested a terminal holding unread input wakes the poll
/// immediately, so without this floor the loop would spin at full tilt — the
/// exact symptom issue #1138 is about, reintroduced in the code that fixes it.
/// A hangup is unaffected: that path calls back and returns rather than pausing.
///
/// `pub` for the same reason [`PROBE_INTERVAL`] is: the watchdog loop that calls
/// it is Unix-only, so a private item here is dead code on a non-Unix build and
/// `build-windows` lints with `-D warnings`.
pub fn iteration_pause(elapsed: Duration) -> Duration {
    PROBE_INTERVAL.saturating_sub(elapsed)
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
    let probes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let thread_probes = Arc::clone(&probes);
    let spawned = std::thread::Builder::new()
        .name("dad-tty-hangup".to_string())
        .spawn(move || {
            let fd = tty.as_raw_fd();
            while !thread_stop.load(Ordering::SeqCst) {
                let started = std::time::Instant::now();
                thread_probes.fetch_add(1, Ordering::SeqCst);
                match probe(fd, PROBE_INTERVAL) {
                    // Including "live with unread input", which the `POLLIN`
                    // mask wakes on at once. Sleeping out the rest of the
                    // interval is what keeps that from being a spin.
                    Ok(TtyState::Live) => std::thread::sleep(iteration_pause(started.elapsed())),
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
    HangupWatch { stop, probes }
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
        // The newline is load-bearing: a pseudo-terminal slave starts in
        // CANONICAL mode, so bytes written to the master are held by the line
        // discipline and the slave does not become readable until a line
        // terminator arrives. Without it this case polls a slave with nothing
        // to read and proves nothing about readability at all.
        master.write_all(b"hello\n").expect("write to the master");
        master.flush().expect("flush the master");
        let (rc, revents) = poll_once(slave.as_raw_fd(), Duration::from_millis(50))
            .expect("probe a slave with input pending");
        assert_eq!(
            classify(rc, revents),
            TtyState::Live,
            "pending input is not a hangup, and must not be classified as one — \
             the probe requests POLLIN (see the module docs), so it WAKES on \
             input and only the error bits may end the deck. \
             poll rc={rc} revents={revents:#06x} \
             (POLLIN={:#06x} POLLHUP={:#06x} POLLERR={:#06x} POLLNVAL={:#06x})",
            libc::POLLIN,
            libc::POLLHUP,
            libc::POLLERR,
            libc::POLLNVAL,
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
        let (rc, revents) =
            poll_once(slave.as_raw_fd(), Duration::from_secs(5)).expect("probe a hung-up slave");
        assert_eq!(
            classify(rc, revents),
            TtyState::HungUp,
            "this platform's `poll` did not report a hangup for a pseudo-terminal \
             slave whose master has closed, so the watchdog would never fire here \
             and issue #1138 would be fixed on Linux only. That is not \
             hypothetical — it is what `build-macos` reported for an `events` \
             mask of zero. poll rc={rc} revents={revents:#06x} \
             (POLLIN={:#06x} POLLHUP={:#06x} POLLERR={:#06x} POLLNVAL={:#06x})",
            libc::POLLIN,
            libc::POLLHUP,
            libc::POLLERR,
            libc::POLLNVAL,
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

    /// Issue #1138 — a terminal holding unread input must not turn the
    /// watchdog into a spin. The probe requests `POLLIN`, so such a terminal
    /// wakes every poll instantly; without [`iteration_pause`] the loop would
    /// burn a core, which is the symptom this whole module exists to remove.
    #[test]
    fn watch_does_not_spin_on_a_terminal_with_unread_input() {
        use std::io::Write as _;

        let (mut master, slave) = open_pty();
        // Input that nothing will ever consume: the watchdog never reads, and
        // there is no event loop here to drain it. The trailing newline is what
        // makes the slave genuinely READABLE — a pseudo-terminal starts in
        // canonical mode and holds a partial line, so without it this test
        // polls an idle terminal and passes whether or not the floor exists.
        // Measured: it did exactly that before the newline was added.
        master.write_all(b"unconsumed input\n").expect("write");
        master.flush().expect("flush");

        let (tx, rx) = mpsc::channel();
        let watch = spawn_watch(WatchedTty::Owned(slave), move || {
            let _ = tx.send(());
        });

        let window = PROBE_INTERVAL * 3;
        assert!(
            rx.recv_timeout(window).is_err(),
            "unread input is not a hangup and must not end the deck"
        );

        // Generous: the floor allows about one probe per interval, so four over
        // three intervals is slack for scheduling, while a spin would be in the
        // hundreds of thousands.
        let probes = watch.probe_count();
        assert!(
            probes <= 4,
            "the watchdog polled {probes} times in {window:?} — with `POLLIN` \
             requested, a terminal with unread input wakes every poll at once, \
             so a missing iteration floor turns this loop into exactly the \
             100%-of-a-core spin issue #1138 was filed about"
        );
    }

    /// Issue #1138 — the iteration floor's arithmetic: a probe that returned
    /// instantly pauses for the whole interval, one that used the interval up
    /// pauses for nothing, and an overrun never underflows into a long sleep.
    #[test]
    fn iteration_pause_fills_out_the_interval_without_underflowing() {
        assert_eq!(iteration_pause(Duration::ZERO), PROBE_INTERVAL);
        assert_eq!(iteration_pause(PROBE_INTERVAL), Duration::ZERO);
        assert_eq!(iteration_pause(PROBE_INTERVAL * 2), Duration::ZERO);
        assert_eq!(
            iteration_pause(PROBE_INTERVAL / 4),
            PROBE_INTERVAL - PROBE_INTERVAL / 4
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
