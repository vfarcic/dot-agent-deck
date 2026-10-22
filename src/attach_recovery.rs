//! Issue #1794: keep a daemon reachable over attach, or let it go.
//!
//! One `EMFILE` on the attach listener used to end the attach server while the
//! hook endpoint kept serving. No client could reach the daemon, and every
//! lazy-spawned replacement refused to start because the old daemon's hook
//! socket was still live. Two layers close that:
//!
//! - [`AcceptBackoff`] keeps the accept loop in `daemon_protocol` serving
//!   through errors that belong to one connection or to a moment, such as
//!   running out of descriptors, by waiting a short, capped delay and accepting
//!   again.
//! - [`supervise`] runs the attach server and, when it ends anyway, binds a
//!   fresh listener at the same path. Restarting keeps every agent and every
//!   orchestration role, which exist only in this daemon's memory. Only when
//!   the listener cannot be put back does it call its `lost` action, which in
//!   the daemon is [`stop_after_attach_lost`]: an unreachable daemon is worse
//!   than none, because the next client can start a fresh one.
//!
//! On Windows a rebind creates the pipe's first instance, which fails while a
//! connection the previous server accepted is still open, so there a daemon
//! whose attach server ended stops unless its clients have disconnected by the
//! last rebind. Windows builds do not ship yet.

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use tracing::{error, info, warn};

use crate::agent_pty::AgentPtyRegistry;
use crate::platform::ipc::IpcListener;
use crate::state::SharedState;

/// The delay after the first failed accept of a streak; each further failure
/// doubles it up to [`ACCEPT_RETRY_CAP`].
pub const ACCEPT_RETRY_FIRST: Duration = Duration::from_millis(5);

/// The longest wait between two accepts in a streak of failures, so a
/// persistent error costs one attempt a second rather than a spinning core.
pub const ACCEPT_RETRY_CAP: Duration = Duration::from_secs(1);

/// A streak logs its first failure, then one line per this many, which at the
/// cap is about once a minute.
const ACCEPT_FAILURES_PER_LOG: u32 = 60;

/// Whether a failed `accept` concerns one connection or a passing shortage
/// rather than the listener itself, so accepting again later can succeed.
pub fn is_transient_accept_error(e: &io::Error) -> bool {
    #[cfg(unix)]
    if let Some(code) = e.raw_os_error()
        && [
            libc::EMFILE,
            libc::ENFILE,
            libc::ENOBUFS,
            libc::ENOMEM,
            libc::EPROTO,
        ]
        .contains(&code)
    {
        return true;
    }
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::Interrupted
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::TimedOut
            | io::ErrorKind::OutOfMemory
    )
}

/// The accept loop's retry state: how many accepts in a row have failed.
#[derive(Debug, Default)]
pub struct AcceptBackoff {
    failures: u32,
}

impl AcceptBackoff {
    /// How long to wait before accepting again after `e`, or `None` when `e`
    /// is not transient and the loop should end.
    pub fn after_error(&mut self, e: &io::Error) -> Option<Duration> {
        if !is_transient_accept_error(e) {
            return None;
        }
        self.failures = self.failures.saturating_add(1);
        let delay = ACCEPT_RETRY_FIRST
            .saturating_mul(1u32.checked_shl(self.failures - 1).unwrap_or(u32::MAX))
            .min(ACCEPT_RETRY_CAP);
        if self.failures % ACCEPT_FAILURES_PER_LOG == 1 {
            warn!(
                failures = self.failures,
                "attach accept failed: {e}; accepting again in {delay:?}"
            );
        }
        Some(delay)
    }

    /// An accept succeeded: end the streak.
    pub fn after_success(&mut self) {
        if self.failures > 0 {
            info!(
                failures = self.failures,
                "attach accept recovered after failing"
            );
            self.failures = 0;
        }
    }
}

/// Waits before each attempt to bind a fresh listener once the attach server
/// has ended. Every attempt failing means the listener cannot be put back.
pub const REBIND_DELAYS: [Duration; 5] = [
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
    Duration::from_millis(1600),
];

/// More than this many restarts within [`RESTART_WINDOW`] means each fresh
/// listener ends as soon as it starts, so restarting is given up.
pub const MAX_RESTARTS_IN_WINDOW: usize = 5;

/// See [`MAX_RESTARTS_IN_WINDOW`].
pub const RESTART_WINDOW: Duration = Duration::from_secs(60);

/// Serve attach on `listener` with `serve`, and every time the server ends,
/// bind a fresh listener at `path` and serve again. Calls `lost` and returns
/// when no listener can be bound after [`REBIND_DELAYS`], or when the server
/// ended more than [`MAX_RESTARTS_IN_WINDOW`] times within [`RESTART_WINDOW`].
///
/// Each run of `serve` is its own task, so a panic in the accept loop ends that
/// run like an error does rather than ending supervision. That task is aborted
/// when this future is dropped, so aborting supervision stops the listener too.
///
/// `state_dir` holds `spawn.lock`, which a rebind takes (see
/// [`bind_under_spawn_lock`]).
pub async fn supervise<S, F, L, LF>(
    path: PathBuf,
    state_dir: PathBuf,
    listener: IpcListener,
    mut serve: S,
    lost: L,
) where
    S: FnMut(IpcListener) -> F,
    F: Future<Output = io::Result<()>> + Send + 'static,
    L: FnOnce() -> LF,
    LF: Future<Output = ()>,
{
    let mut listener = listener;
    let mut restarts: Vec<Instant> = Vec::new();
    loop {
        let mut server = AbortOnDrop(tokio::spawn(serve(listener)));
        match (&mut server.0).await {
            Ok(Ok(())) => error!("attach protocol server stopped"),
            Ok(Err(e)) => error!("attach protocol server error: {e}"),
            Err(e) => error!("attach protocol server failed: {e}"),
        }
        let now = Instant::now();
        restarts.retain(|at| now.duration_since(*at) < RESTART_WINDOW);
        if restarts.len() >= MAX_RESTARTS_IN_WINDOW {
            error!(
                restarts = restarts.len(),
                window_secs = RESTART_WINDOW.as_secs(),
                "the attach server keeps ending; no longer restarting it"
            );
            lost().await;
            return;
        }
        restarts.push(now);
        match rebind(&path, &state_dir).await {
            Some(fresh) => listener = fresh,
            None => {
                lost().await;
                return;
            }
        }
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<io::Result<()>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn rebind(path: &Path, state_dir: &Path) -> Option<IpcListener> {
    for delay in REBIND_DELAYS {
        tokio::time::sleep(delay).await;
        match bind_under_spawn_lock(path, state_dir).await {
            Ok(listener) => {
                // Not the startup line `Attach protocol listening on`, which the
                // cross-version harness counts to tell one daemon from two.
                warn!("attach listener re-established on {}", path.display());
                return Some(listener);
            }
            Err(e) => warn!("could not listen on {} again: {e}", path.display()),
        }
    }
    error!(
        attempts = REBIND_DELAYS.len(),
        "could not listen on {} again; no client can reach this daemon",
        path.display()
    );
    None
}

/// Bind a fresh attach listener at `path` while holding `spawn.lock` in
/// `state_dir`. A client that finds the endpoint dead removes it and starts a
/// daemon under that lock (`daemon_attach::ensure_daemon_running`), so binding
/// outside it could land between that client's probe and its unlink, and the
/// client would unlink the listener just bound. The wait for the lock is
/// bounded past a client's own wait for a daemon to start; if it runs out, or
/// the lock cannot be taken, the bind goes ahead without it.
async fn bind_under_spawn_lock(path: &Path, state_dir: &Path) -> io::Result<IpcListener> {
    let wait = crate::daemon_attach::daemon_start_poll_timeout() + SPAWN_LOCK_EXTRA_WAIT;
    let lock = match crate::platform::fsperm::ensure_owner_only_dir(state_dir) {
        Ok(()) => tokio::time::timeout(
            wait,
            crate::platform::lock::acquire_spawn_lock(&state_dir.join("spawn.lock")),
        )
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "waited too long"))
        .and_then(|taken| taken),
        Err(e) => Err(e),
    };
    if let Err(e) = &lock {
        warn!("binding the attach listener without the spawn lock: {e}");
    }
    let path = path.to_path_buf();
    let bound =
        tokio::task::spawn_blocking(move || crate::daemon_protocol::bind_attach_listener(&path))
            .await
            .map_err(io::Error::other)?;
    drop(lock);
    bound
}

/// How much longer than a client's own wait for a daemon to start a rebind
/// waits for the spawn lock.
const SPAWN_LOCK_EXTRA_WAIT: Duration = Duration::from_secs(5);

/// The daemon's `lost` action for [`supervise`]: stop the way a termination
/// signal does, so the hook endpoint is released and the next client can start
/// a fresh daemon. Names what it stops before the drain (issue #1109), drains
/// the agents with the usual grace, then releases the hook loop. Does nothing
/// when the daemon has already committed to a restart, which is exiting anyway.
pub async fn stop_after_attach_lost(
    shutdown: Arc<Notify>,
    registry: Arc<AgentPtyRegistry>,
    state: SharedState,
    restart: Arc<crate::daemon_restart::RestartControl>,
) {
    if !restart.stop_wins("attach-lost") {
        return;
    }
    error!(
        "shutting the daemon down because no client can reach it over attach \
         (every managed agent will be stopped); the next client starts a fresh daemon"
    );
    crate::daemon_stop::log_teardown_inventory(&state, &registry, "attach-lost").await;
    let _ = tokio::task::spawn_blocking(move || {
        registry.shutdown_all_graceful(crate::agent_pty::AGENT_TERMINATE_GRACE);
    })
    .await;
    shutdown.notify_one();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os_error(code: i32) -> io::Error {
        io::Error::from_raw_os_error(code)
    }

    #[cfg(unix)]
    #[test]
    fn running_out_of_descriptors_and_aborted_connections_are_transient() {
        for code in [
            libc::EMFILE,
            libc::ENFILE,
            libc::ENOBUFS,
            libc::ENOMEM,
            libc::EPROTO,
            libc::ECONNABORTED,
            libc::EINTR,
        ] {
            assert!(is_transient_accept_error(&os_error(code)), "{code}");
        }
        for code in [libc::EBADF, libc::EINVAL, libc::ENOTSOCK] {
            assert!(!is_transient_accept_error(&os_error(code)), "{code}");
        }
    }

    #[test]
    fn an_error_about_the_listener_itself_is_not_retried() {
        let mut backoff = AcceptBackoff::default();
        assert_eq!(
            backoff.after_error(&io::Error::other("no pending pipe")),
            None
        );
    }

    #[test]
    fn the_delay_doubles_to_the_cap_and_a_success_starts_over() {
        let mut backoff = AcceptBackoff::default();
        let aborted = io::Error::from(io::ErrorKind::ConnectionAborted);
        let delays: Vec<Duration> = (0..12)
            .map(|_| backoff.after_error(&aborted).expect("transient"))
            .collect();
        assert_eq!(delays[0], ACCEPT_RETRY_FIRST);
        assert_eq!(delays[1], ACCEPT_RETRY_FIRST * 2);
        assert!(delays.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!(*delays.last().unwrap(), ACCEPT_RETRY_CAP);
        backoff.after_success();
        assert_eq!(backoff.after_error(&aborted), Some(ACCEPT_RETRY_FIRST));
    }

    #[test]
    fn a_streak_of_any_length_never_waits_past_the_cap() {
        let mut backoff = AcceptBackoff {
            failures: u32::MAX - 1,
        };
        let aborted = io::Error::from(io::ErrorKind::ConnectionAborted);
        assert_eq!(backoff.after_error(&aborted), Some(ACCEPT_RETRY_CAP));
        assert_eq!(backoff.after_error(&aborted), Some(ACCEPT_RETRY_CAP));
    }
}
