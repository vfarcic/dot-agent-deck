//! Issue #1884: a bounded retry of PTY allocation on macOS.
//!
//! macOS `openpty` has failed intermittently with `ENXIO` ("Device not
//! configured"). CI met it as errno `6` in tests that spawn a stand-in agent,
//! each passing on rerun, and an allocation-only stress probe on a macOS runner
//! saw errno `-6` from the same call; whether the two share a cause is not
//! established, and neither is the cause itself. This is a mitigation: the
//! allocation alone is tried up to `ATTEMPTS` times with `DELAY` between
//! retryable failures, and what a caller does with the pair afterwards, spawning
//! the child included, still happens once. Every other error, and the last
//! attempt's, reaches the caller unchanged.
//!
//! A failed attempt is not closed by the caller: it owns neither descriptor
//! of a call that returned an error, so the retry drops nothing but the error
//! value. Closing the output integers of a failed call would be wrong, since
//! they may be unset or already reused by another thread.
//!
//! Other platforms allocate exactly once, as before.

use portable_pty::{NativePtySystem, PtyPair, PtySize, PtySystem};
#[cfg(any(target_os = "macos", all(test, unix)))]
use std::time::Duration;

/// Allocation attempts in total, the first included.
#[cfg(any(target_os = "macos", all(test, unix)))]
const ATTEMPTS: u32 = 3;

/// Pause after a retryable failure; there is none after the last attempt.
#[cfg(any(target_os = "macos", all(test, unix)))]
const DELAY: Duration = Duration::from_millis(5);

/// [`NativePtySystem`] whose `openpty` retries a transient failure on macOS.
/// Only allocation is retried: everything a caller does with the pair stays
/// single-shot.
#[derive(Default)]
pub struct RetryingPtySystem(pub NativePtySystem);

impl PtySystem for RetryingPtySystem {
    fn openpty(&self, size: PtySize) -> anyhow::Result<PtyPair> {
        #[cfg(test)]
        let attempt = || injected::next().map_or_else(|| self.0.openpty(size), Err);
        #[cfg(not(test))]
        let attempt = || self.0.openpty(size);
        #[cfg(target_os = "macos")]
        {
            retry(attempt, is_transient_portable, std::thread::sleep)
        }
        #[cfg(not(target_os = "macos"))]
        {
            attempt()
        }
    }
}

/// `open` is one raw allocation of a pair, as `wrap`'s inner PTY makes it with
/// `libc::openpty`; on macOS it is retried on a transient failure.
#[cfg(unix)]
pub fn open_raw<T>(open: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    #[cfg(target_os = "macos")]
    {
        retry(open, is_transient_io, std::thread::sleep)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let mut open = open;
        open()
    }
}

/// Call `attempt` until it succeeds, fails with an error `is_transient` refuses,
/// or has been called [`ATTEMPTS`] times, sleeping [`DELAY`] through `sleep`
/// after each retryable failure. Returns the last result as `attempt` gave it.
#[cfg(any(target_os = "macos", all(test, unix)))]
fn retry<T, E: std::fmt::Display>(
    mut attempt: impl FnMut() -> Result<T, E>,
    is_transient: impl Fn(&E) -> bool,
    mut sleep: impl FnMut(Duration),
) -> Result<T, E> {
    let mut made = 1;
    loop {
        match attempt() {
            Err(e) if made < ATTEMPTS && is_transient(&e) => {
                tracing::debug!(
                    attempt = made,
                    error = %e,
                    "PTY allocation failed transiently; retrying (issue #1884)"
                );
                sleep(DELAY);
                made += 1;
            }
            result => return result,
        }
    }
}

/// `ENXIO` as CI met it (`6`) and as the macOS stress probe met it (`-6`).
#[cfg(all(unix, any(target_os = "macos", test)))]
fn is_enxio(code: i32) -> bool {
    code == libc::ENXIO || code == -libc::ENXIO
}

#[cfg(all(unix, any(target_os = "macos", test)))]
fn is_transient_io(e: &std::io::Error) -> bool {
    e.raw_os_error().is_some_and(is_enxio)
}

/// portable-pty 0.8.1 reports a failed `openpty` as
/// `bail!("failed to openpty: {:?}", io::Error::last_os_error())`: the error is
/// a message, with no `io::Error` left in its chain to downcast. So the code is
/// read back from exactly that message, and a message of any other shape — a
/// `cloexec` failure after a successful allocation among them — is not retried.
/// A typed cause is preferred, should a later portable-pty carry one.
#[cfg(all(unix, any(target_os = "macos", test)))]
fn is_transient_portable(e: &anyhow::Error) -> bool {
    if let Some(io) = e.downcast_ref::<std::io::Error>() {
        return is_transient_io(io);
    }
    format!("{e}")
        .strip_prefix("failed to openpty: Os { code: ")
        .and_then(|rest| rest.split_once(", kind: "))
        .and_then(|(code, _)| code.parse().ok())
        .is_some_and(is_enxio)
}

/// Test seam: errors a test queues here are returned, in order, by the calling
/// thread's next [`RetryingPtySystem::openpty`] attempts in place of a real
/// allocation, so a test can drive a production call site through the retry.
#[cfg(test)]
pub(crate) mod injected {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    thread_local! {
        static QUEUE: RefCell<VecDeque<anyhow::Error>> = const { RefCell::new(VecDeque::new()) };
    }

    #[cfg(unix)]
    pub(crate) fn push(errors: impl IntoIterator<Item = anyhow::Error>) {
        QUEUE.with(|q| q.borrow_mut().extend(errors));
    }

    pub(super) fn next() -> Option<anyhow::Error> {
        QUEUE.with(|q| q.borrow_mut().pop_front())
    }

    /// Empty the queue, returning how many queued errors no attempt consumed.
    #[cfg(unix)]
    pub(crate) fn drain() -> usize {
        QUEUE.with(|q| q.borrow_mut().drain(..).count())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::agent_pty::{AgentPtyError, SpawnOptions, spawn};
    use std::cell::Cell;
    use std::io;

    /// The error portable-pty 0.8.1 builds for a failed `openpty`, byte for
    /// byte: the same format string over an `io::Error` carrying `code`.
    fn portable_failure(code: i32) -> anyhow::Error {
        anyhow::anyhow!(
            "failed to openpty: {:?}",
            io::Error::from_raw_os_error(code)
        )
    }

    /// Run [`retry`] over `results`, one per attempt, with a sleeper that only
    /// counts. Returns the outcome, the attempts made and the sleeps taken.
    fn run(
        results: Vec<Result<&'static str, anyhow::Error>>,
    ) -> (Result<&'static str, anyhow::Error>, usize, usize) {
        let mut results = results.into_iter();
        let attempts = Cell::new(0);
        let sleeps = Cell::new(0);
        let outcome = retry(
            || {
                attempts.set(attempts.get() + 1);
                results
                    .next()
                    .expect("retry made more attempts than scripted")
            },
            is_transient_portable,
            |d| {
                assert_eq!(d, DELAY);
                sleeps.set(sleeps.get() + 1);
            },
        );
        (outcome, attempts.get(), sleeps.get())
    }

    #[test]
    fn portable_ptys_failure_message_is_classified_from_its_actual_text() {
        // The two shapes seen on macOS, as the deck receives them.
        assert!(is_transient_portable(&portable_failure(6)));
        assert!(is_transient_portable(&portable_failure(-6)));
        // What the classifier relies on: std's `Debug` of an OS error opens
        // with its code, and portable-pty puts nothing before that but its prefix.
        assert!(
            format!("{}", portable_failure(-6))
                .starts_with("failed to openpty: Os { code: -6, kind: "),
            "{}",
            portable_failure(-6)
        );
        // Neither another errno, nor a `6` elsewhere, nor another failure.
        assert!(!is_transient_portable(&portable_failure(libc::ENOMEM)));
        assert!(!is_transient_portable(&portable_failure(66)));
        assert!(!is_transient_portable(&portable_failure(-66)));
        assert!(!is_transient_portable(&anyhow::anyhow!(
            "fcntl to read flags failed: {:?}",
            io::Error::from_raw_os_error(6)
        )));
        assert!(!is_transient_portable(
            &portable_failure(6).context("while spawning")
        ));
        // A typed cause, should one ever arrive.
        assert!(is_transient_portable(&anyhow::Error::from(
            io::Error::from_raw_os_error(-6)
        )));
        assert!(!is_transient_portable(&anyhow::Error::from(
            io::Error::from_raw_os_error(libc::EMFILE)
        )));
        assert!(is_transient_io(&io::Error::from_raw_os_error(6)));
        assert!(!is_transient_io(&io::Error::other(
            "failed to openpty: Os { code: 6, kind: x }"
        )));
    }

    #[test]
    fn a_first_success_makes_one_attempt_and_no_sleep() {
        let (outcome, attempts, sleeps) = run(vec![Ok("pair")]);
        assert_eq!(outcome.unwrap(), "pair");
        assert_eq!((attempts, sleeps), (1, 0));
    }

    #[test]
    fn one_transient_failure_then_success_succeeds_after_one_retry() {
        for code in [6, -6] {
            let (outcome, attempts, sleeps) = run(vec![Err(portable_failure(code)), Ok("pair")]);
            assert_eq!(outcome.unwrap(), "pair", "code {code}");
            assert_eq!((attempts, sleeps), (2, 1), "code {code}");
        }
    }

    #[test]
    fn two_transient_failures_then_success_succeeds_on_the_third_attempt() {
        let (outcome, attempts, sleeps) = run(vec![
            Err(portable_failure(6)),
            Err(portable_failure(-6)),
            Ok("pair"),
        ]);
        assert_eq!(outcome.unwrap(), "pair");
        assert_eq!((attempts, sleeps), (3, 2));
    }

    #[test]
    fn three_transient_failures_return_the_last_error_unchanged() {
        let (outcome, attempts, sleeps) = run(vec![
            Err(portable_failure(6)),
            Err(portable_failure(6)),
            Err(portable_failure(-6)),
        ]);
        assert_eq!(
            format!("{}", outcome.unwrap_err()),
            format!("{}", portable_failure(-6))
        );
        assert_eq!(
            (attempts, sleeps),
            (3, 2),
            "no fourth attempt, no sleep after the last"
        );
    }

    #[test]
    fn a_non_retryable_error_is_returned_at_once() {
        let (outcome, attempts, sleeps) = run(vec![Err(portable_failure(libc::ENOMEM))]);
        assert_eq!(
            format!("{}", outcome.unwrap_err()),
            format!("{}", portable_failure(libc::ENOMEM))
        );
        assert_eq!((attempts, sleeps), (1, 0));

        let (outcome, attempts, sleeps) = run(vec![
            Err(portable_failure(6)),
            Err(anyhow::anyhow!("fcntl to set CLOEXEC failed")),
        ]);
        assert_eq!(
            format!("{}", outcome.unwrap_err()),
            "fcntl to set CLOEXEC failed"
        );
        assert_eq!(
            (attempts, sleeps),
            (2, 1),
            "a transient then a fatal stops at the fatal"
        );
    }

    /// The spawn path allocates through [`RetryingPtySystem`]: errors queued in
    /// the seam reach `spawn` as its allocation's own. On macOS one transient
    /// failure is retried away and three surface as today's `Open` error, the
    /// last one's text; elsewhere the first surfaces at once, as before.
    #[test]
    fn the_spawn_path_allocates_through_the_retry() {
        let true_opts = || SpawnOptions {
            command: Some("/usr/bin/true"),
            ..SpawnOptions::default()
        };
        let sentinel = || anyhow::anyhow!("sentinel: never consumed");

        // A non-retryable error surfaces at once, on every platform.
        injected::push([portable_failure(libc::ENOMEM), sentinel()]);
        let Err(AgentPtyError::Open(msg)) = spawn(true_opts()) else {
            panic!("a non-retryable allocation error must fail the spawn");
        };
        assert_eq!(msg, portable_failure(libc::ENOMEM).to_string());
        assert_eq!(injected::drain(), 1, "exactly one attempt");

        injected::push([
            portable_failure(6),
            portable_failure(-6),
            portable_failure(-6),
            sentinel(),
        ]);
        let outcome = spawn(true_opts());
        if cfg!(target_os = "macos") {
            let Err(AgentPtyError::Open(msg)) = outcome else {
                panic!("three transient failures must fail the spawn");
            };
            assert_eq!(
                msg,
                portable_failure(-6).to_string(),
                "the last attempt's error"
            );
            assert_eq!(injected::drain(), 1, "exactly three attempts");

            injected::push([portable_failure(6)]);
            let mut pty = spawn(true_opts()).expect("one transient failure is retried away");
            assert_eq!(injected::drain(), 0);
            let _ = pty.child.kill();
            let _ = pty.child.wait();
        } else {
            let Err(AgentPtyError::Open(msg)) = outcome else {
                panic!("off macOS an allocation failure fails the spawn, as before");
            };
            assert_eq!(msg, portable_failure(6).to_string());
            assert_eq!(injected::drain(), 3, "exactly one attempt");
        }
    }
}
