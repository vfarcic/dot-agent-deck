//! Pure-data unit tests for the file logger's `RUST_LOG` precedence (issue
//! #605): a user-supplied `dot_agent_deck=…` directive must win over the
//! deck's built-in `dot_agent_deck=info` default, while an absent one must
//! leave that default — and the global `error` floor for dependency crates —
//! exactly as they were.
//!
//! These are lib units, NOT `#[spec]` catalog tests — no scenario comments, no
//! catalog entries. They assert at the altitude the reporter sees: whether the
//! line actually lands in the log, not which directives the filter holds.

use std::io::Write;
use std::sync::{Arc, Mutex};

use dot_agent_deck::logging::env_filter;
use tracing_subscriber::fmt::MakeWriter;

/// A `MakeWriter` over a shared in-memory buffer. Hand-rolled because
/// `tracing-subscriber` implements `MakeWriter` for `Mutex<W>` (not shareable
/// once `with_writer` takes ownership) and for `Arc<W> where &W: Write` (which
/// `&Mutex<Vec<u8>>` is not).
#[derive(Clone)]
struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("capture buffer poisoned").write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CaptureWriter {
    type Writer = CaptureWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Install the filter `rust_log` produces on a thread-local subscriber, emit
/// one probe event per (target, level) of interest, and return everything the
/// subscriber wrote. `with_default` keeps this thread-local, so the tests stay
/// independent under `cargo test`'s threads as well as `nextest`'s processes.
fn capture(rust_log: Option<&str>) -> String {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(env_filter(rust_log))
        .with_writer(CaptureWriter(Arc::clone(&buf)))
        .with_ansi(false)
        // No timestamps, so two captures taken at different instants compare
        // byte for byte. Nothing under test here is time-dependent.
        .without_time()
        .finish();

    tracing::subscriber::with_default(subscriber, || {
        // A module inside this crate, i.e. what `dot_agent_deck=…` has to cover
        // by prefix — the deck never logs against the bare crate target.
        tracing::debug!(target: "dot_agent_deck::daemon", "crate-debug-line");
        tracing::info!(target: "dot_agent_deck::daemon", "crate-info-line");
        tracing::warn!(target: "dot_agent_deck::daemon", "crate-warn-line");
        // A dependency crate, to pin the global floor the deck inherits.
        tracing::error!(target: "some_dependency", "dependency-error-line");
        tracing::info!(target: "some_dependency", "dependency-info-line");
    });

    let bytes = buf.lock().expect("capture buffer poisoned").clone();
    String::from_utf8(bytes).expect("log output is not UTF-8")
}

/// Issue #605, the reported defect: `RUST_LOG=dot_agent_deck=debug` must turn
/// on crate-level debug logging.
#[test]
fn logging_filter_explicit_crate_directive_enables_debug() {
    let out = capture(Some("dot_agent_deck=debug"));
    assert!(
        out.contains("crate-debug-line"),
        "RUST_LOG=dot_agent_deck=debug did not enable crate debug logging; got:\n{out}"
    );
    assert!(out.contains("crate-info-line"), "got:\n{out}");
}

/// The control: with no `RUST_LOG` at all the crate default stays at `info`,
/// and dependency crates keep the global `error` floor.
#[test]
fn logging_filter_absent_rust_log_keeps_info_default() {
    let out = capture(None);
    assert!(
        !out.contains("crate-debug-line"),
        "crate debug logging leaked in with no RUST_LOG set; got:\n{out}"
    );
    assert!(out.contains("crate-info-line"), "got:\n{out}");
    assert!(out.contains("dependency-error-line"), "got:\n{out}");
    assert!(
        !out.contains("dependency-info-line"),
        "dependency info logging leaked past the error floor; got:\n{out}"
    );
}

/// An empty `RUST_LOG` is the same as an unset one — the directive string the
/// builder assembles must not grow a stray empty segment.
#[test]
fn logging_filter_empty_rust_log_behaves_like_unset() {
    assert_eq!(capture(Some("")), capture(None));
}

/// The dependency `error` floor survives a user directive too. Before the fix
/// it did not: any non-empty `RUST_LOG` suppressed
/// `EnvFilter::from_default_env`'s implicit default, so asking for crate debug
/// logging silently switched dependency errors off. Naming the floor makes it
/// unconditional.
#[test]
fn logging_filter_user_directive_keeps_the_dependency_error_floor() {
    let out = capture(Some("dot_agent_deck=debug"));
    assert!(out.contains("dependency-error-line"), "got:\n{out}");
    assert!(
        !out.contains("dependency-info-line"),
        "dependency info logging leaked past the error floor; got:\n{out}"
    );
}

/// The control that isolates the cause: a *more specific* target was never
/// affected by the defect, because the appended default only replaces an
/// exactly-equal target. It must keep working after the fix.
#[test]
fn logging_filter_more_specific_target_still_enables_debug() {
    let out = capture(Some("dot_agent_deck::daemon=debug"));
    assert!(out.contains("crate-debug-line"), "got:\n{out}");
}

/// A bare level in `RUST_LOG` names no target, so it does not constrain
/// `dot_agent_deck` — the crate default still applies and only the global floor
/// moves. Unchanged behavior, pinned so the fix cannot widen it by accident.
#[test]
fn logging_filter_bare_level_does_not_move_the_crate_default() {
    let out = capture(Some("debug"));
    assert!(
        !out.contains("crate-debug-line"),
        "a bare RUST_LOG level should not raise the crate default; got:\n{out}"
    );
    assert!(out.contains("dependency-info-line"), "got:\n{out}");
}

/// Precedence runs both ways: an explicit directive that is *quieter* than the
/// default must also be honored.
#[test]
fn logging_filter_explicit_crate_directive_can_quiet_the_crate() {
    let out = capture(Some("dot_agent_deck=warn"));
    assert!(
        !out.contains("crate-info-line"),
        "RUST_LOG=dot_agent_deck=warn did not quiet crate info logging; got:\n{out}"
    );
    assert!(out.contains("crate-warn-line"), "got:\n{out}");
}
