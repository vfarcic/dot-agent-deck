//! Effective build ID used for the local-attach build-version handshake.
//!
//! Production callers read the compile-time `env!("DAD_BUILD_ID")` baked
//! in by `build.rs` (PRD #103 M1.0). This helper layers a **test-only**
//! runtime override on top: when `DOT_AGENT_DECK_BUILD_ID_OVERRIDE` is
//! set in the process's environment, [`local_build_id`] returns that
//! value instead.
//!
//! The override exists exclusively so the M4.2 integration tests
//! (`tests/build_version_handshake.rs`) can simulate same-tag /
//! different-commit skew between the TUI and the daemon subprocess
//! without rebuilding the binary at a synthetic `DAD_BUILD_ID`. Both
//! sides honour the same variable so each subprocess can be pinned to
//! its own build_id independently:
//!
//! - [`crate::daemon_protocol::AttachResponse::hello`] uses it to
//!   advertise the daemon's `build_version` on the wire.
//! - [`crate::build_version_handshake::ensure_compatible_daemon_or_die`]
//!   uses it for the laptop's own comparison value and for the
//!   `client_build_version` it sends on `Hello`.
//!
//! Production code must never set this env var — there is no scenario
//! in which honest builds need to lie about their `DAD_BUILD_ID`. The
//! variable is grep-ably named so a future audit can confirm nothing
//! outside tests sets it.

/// Return the effective build_id for the local handshake.
///
/// `env!("DAD_BUILD_ID")` in production; the value of
/// `DOT_AGENT_DECK_BUILD_ID_OVERRIDE` when set under
/// `cfg(any(test, debug_assertions))`. Release builds (which set
/// `debug_assertions = false`) compile the override branch out entirely,
/// so the test hook cannot be reached from a shipped binary even if the
/// env var happens to be set in the operator's environment.
pub fn local_build_id() -> String {
    #[cfg(any(test, debug_assertions))]
    if let Ok(v) = std::env::var("DOT_AGENT_DECK_BUILD_ID_OVERRIDE") {
        return v;
    }
    env!("DAD_BUILD_ID").to_string()
}
