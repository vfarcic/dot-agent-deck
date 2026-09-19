//! The two pieces of endpoint **I/O** that issue #1121 needed, kept out of
//! [`crate::platform::paths`] so that module's resolvers stay pure.
//!
//! - [`ensure_endpoint_dir`] — create the owner-only fallback directory. Called
//!   by the side that is about to **bind**, and by nothing else.
//! - [`client_socket_path`] / [`client_attach_socket_path`] — the **connect**
//!   side's resolution, which additionally consults the pre-#1121 endpoint
//!   spelling so a newer build still finds an older build's running daemon.
//!
//! # Why the connect side looks in two places
//!
//! Issue #1121 moved the `XDG_RUNTIME_DIR`-less fallback endpoints from
//! `<temp dir>/dot-agent-deck[-attach]-{uid}.sock` into
//! `<temp dir>/dot-agent-deck-{uid}/{hook,attach}.sock`. Without a compatibility
//! read the upgrade is silent in the worst way: an older daemon binds the old
//! path, a newer TUI looks only at the new one, the two never meet, and the TUI
//! lazy-spawns a *second* daemon with the first one's agents stranded under it.
//! PRD #103/#161's build-version handshake would normally catch exactly this
//! kind of skew and prompt — but it runs over a connection that, in that story,
//! is never made. Reading the legacy path restores the connection, so the
//! handshake fires and the user gets the prompt they get from any other version
//! skew.
//!
//! **The legacy path is read-only for us: never bound, never created, never
//! unlinked.** That is what stops the squatting problem this issue fixes from
//! simply moving to the compatibility path. A foreign entry planted there can
//! at worst fail [`crate::platform::fsperm::verify_endpoint_trusted`]'s `lstat`
//! (issue #1020), and we fall through to the new path and lazy-spawn there.
//!
//! The `$XDG_RUNTIME_DIR` and explicit-override spellings are untouched and
//! cost nothing here: the legacy consultation runs only when the resolved path
//! *is* the fallback one.

#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;

/// How long the connect-side liveness probe waits.
///
/// Both candidates are Unix-domain sockets on this machine, where a connect
/// either completes in microseconds or fails outright; the bound exists for the
/// one case that neither does — `connect(2)` on `AF_UNIX` parks indefinitely
/// when the listener's accept queue is full (see
/// [`crate::platform::ipc::IpcClient::connect`]). A wedged daemon must not turn
/// endpoint resolution into a hang, and a quarter second is the same budget
/// [`crate::ui`]'s interactive daemon hint gives itself.
#[cfg(unix)]
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

/// Make sure the directory `endpoint` lives in exists and is ours alone, when
/// that directory is [`crate::platform::paths::fallback_endpoint_dir`].
///
/// Call this immediately before a `bind(2)`, and **only** there. It is the
/// fallible half of the split DECISION 3 of issue #1121 draws: path resolution
/// stays pure and infallible because hook clients in a scrubbed environment,
/// `daemon status`, the desktop bridge and read-only diagnostics all call it,
/// while directory creation belongs to the one caller that is about to put a
/// socket inside.
///
/// A **no-op for every other endpoint**, which is what keeps it safe to call
/// unconditionally at a bind site: an explicit `DOT_AGENT_DECK_SOCKET` /
/// `DOT_AGENT_DECK_ATTACH_SOCKET` override, an `$XDG_RUNTIME_DIR` endpoint, a
/// test's `tempfile::tempdir()` socket and a Windows named pipe all resolve to
/// a parent this function does not own, and it creates nothing for any of them.
/// Deciding by comparing the parent — rather than by re-reading the
/// environment — is deliberate: there is then no second copy of the resolution
/// rules to drift out of step with [`crate::platform::paths::socket_path`].
///
/// Shaped after [`crate::remote_tunnel::tunnel_socket_dir_in`], which already
/// does this for ssh-forwarded sockets: resolve the directory, hand it to
/// [`crate::platform::fsperm::ensure_owner_only_dir`], and map the failure into
/// something the operator can act on.
pub fn ensure_endpoint_dir(endpoint: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let dir = crate::platform::paths::fallback_endpoint_dir();
        if endpoint.parent() != Some(dir.as_path()) {
            return Ok(());
        }
        crate::platform::fsperm::ensure_owner_only_dir(&dir).map_err(|source| {
            std::io::Error::new(
                source.kind(),
                format!(
                    "{source}\nThis is the endpoint directory the deck falls back to when \
                     XDG_RUNTIME_DIR is unset. To use a different one, set \
                     DOT_AGENT_DECK_SOCKET and DOT_AGENT_DECK_ATTACH_SOCKET to socket paths \
                     under a directory only you can write."
                ),
            )
        })
    }
    #[cfg(windows)]
    {
        // Windows endpoints are named pipes (`\\.\pipe\dot-agent-deck-{user}-…`)
        // with no filesystem presence and therefore no directory to own. There
        // is no `fallback_endpoint_dir` on this platform and nothing for this
        // function to create.
        let _ = endpoint;
        Ok(())
    }
}

/// The hook-ingestion endpoint a **client** in this process should connect to.
///
/// [`crate::platform::paths::socket_path`], except that when that resolved to
/// the fallback endpoint and nothing is answering there, a daemon from a
/// pre-#1121 build still listening at
/// [`crate::platform::paths::legacy_socket_path`] is used instead. See the
/// module docs for why, and for what "read-only" means here.
///
/// **Never use this to decide where to bind** — `daemon serve` resolves
/// [`crate::platform::paths::socket_path`] directly, so the new spelling is the
/// only one anything creates.
pub fn client_socket_path() -> PathBuf {
    let primary = crate::platform::paths::socket_path();
    #[cfg(unix)]
    let primary = with_legacy_fallback(
        primary,
        crate::platform::paths::fallback_socket_path(),
        crate::platform::paths::legacy_socket_path(),
    );
    primary
}

/// The streaming-attach endpoint a **client** in this process should connect
/// to. [`client_socket_path`]'s sibling, over
/// [`crate::platform::paths::attach_socket_path`] and
/// [`crate::platform::paths::legacy_attach_socket_path`].
pub fn client_attach_socket_path() -> PathBuf {
    let primary = crate::platform::paths::attach_socket_path();
    #[cfg(unix)]
    let primary = with_legacy_fallback(
        primary,
        crate::platform::paths::fallback_attach_socket_path(),
        crate::platform::paths::legacy_attach_socket_path(),
    );
    primary
}

/// The resolution order both client accessors share.
///
/// `primary` is what the pure resolver returned and `fallback` is the fallback
/// spelling it would have returned had nothing overridden it; they are equal
/// exactly when we are in the fallback case, which is the only case with a
/// legacy path to consult. Then: the new path if a daemon answers there,
/// otherwise the legacy path if one answers there, otherwise the new path —
/// so a host with no daemon at all lazy-spawns at the new spelling, which is
/// what makes this a transition rather than a second home.
#[cfg(unix)]
fn with_legacy_fallback(primary: PathBuf, fallback: PathBuf, legacy: PathBuf) -> PathBuf {
    if primary != fallback || endpoint_answers(&primary) {
        return primary;
    }
    if endpoint_answers(&legacy) {
        return legacy;
    }
    primary
}

/// Is a daemon we are willing to talk to listening at `endpoint` right now?
///
/// Trust first, then liveness. The trust check is
/// [`crate::platform::fsperm::verify_endpoint_trusted`] — the same `lstat`
/// issue #1020 put in front of every other endpoint connect — so a symlink, a
/// regular file, a foreign-owned socket or a loose mode all answer `false`
/// here without a `connect(2)` being attempted and without the entry being
/// touched. That is the whole of what makes a squatted legacy path harmless:
/// it costs one refused probe and we carry on at the new path.
#[cfg(unix)]
fn endpoint_answers(endpoint: &Path) -> bool {
    crate::platform::fsperm::verify_endpoint_trusted(endpoint).is_ok()
        && crate::platform::ipc::IpcClient::connect_timeout(endpoint, PROBE_TIMEOUT).is_ok()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A live, trusted socket at the primary path is used and the legacy path
    /// is not consulted — the ordinary case on a host that has already been
    /// through one launch on the new build.
    #[test]
    fn a_live_primary_wins_over_a_live_legacy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let primary = dir.path().join("primary.sock");
        let legacy = dir.path().join("legacy.sock");
        let _p = bind_trusted(&primary);
        let _l = bind_trusted(&legacy);

        assert_eq!(
            with_legacy_fallback(primary.clone(), primary.clone(), legacy),
            primary
        );
    }

    /// Nothing at the primary and a live daemon at the legacy path: the older
    /// build's daemon is found, which is the entire point of the compatibility
    /// read.
    #[test]
    fn an_absent_primary_falls_through_to_a_live_legacy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let primary = dir.path().join("primary.sock");
        let legacy = dir.path().join("legacy.sock");
        let _l = bind_trusted(&legacy);

        assert_eq!(
            with_legacy_fallback(primary.clone(), primary, legacy.clone()),
            legacy
        );
    }

    /// An entry at the legacy path that is not a trusted socket of ours — the
    /// squatter case — is refused by the probe and resolution continues at the
    /// new path. The planted entry is left exactly as it was found.
    #[test]
    fn an_untrusted_legacy_entry_is_refused_and_left_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let primary = dir.path().join("primary.sock");
        let legacy = dir.path().join("legacy.sock");
        std::fs::write(&legacy, b"squatter").expect("plant a regular file");

        assert_eq!(
            with_legacy_fallback(primary.clone(), primary.clone(), legacy.clone()),
            primary
        );
        assert_eq!(
            std::fs::read(&legacy).expect("the planted entry survives"),
            b"squatter"
        );
    }

    /// An override or an `$XDG_RUNTIME_DIR` endpoint never consults the legacy
    /// path, however live a daemon there is: `primary != fallback` short-
    /// circuits before any probe.
    #[test]
    fn a_non_fallback_primary_never_consults_the_legacy_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let overridden = dir.path().join("overridden.sock");
        let fallback = dir.path().join("fallback.sock");
        let legacy = dir.path().join("legacy.sock");
        let _l = bind_trusted(&legacy);

        assert_eq!(
            with_legacy_fallback(overridden.clone(), fallback, legacy),
            overridden,
            "an endpoint that is not the fallback one has no older spelling to look for"
        );
    }

    /// [`ensure_endpoint_dir`] creates nothing for an endpoint outside the
    /// fallback directory, which is what makes it safe at a bind site that may
    /// have been handed an override or a test path.
    #[test]
    fn ensure_endpoint_dir_is_inert_outside_the_fallback_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let elsewhere = dir.path().join("nested").join("attach.sock");
        ensure_endpoint_dir(&elsewhere).expect("a foreign endpoint is a no-op, not an error");
        assert!(
            !dir.path().join("nested").exists(),
            "ensure_endpoint_dir must not create a directory it does not own"
        );
    }

    /// The arm that does act: an endpoint inside the fallback directory gets
    /// that directory created at mode 0o700.
    #[test]
    fn ensure_endpoint_dir_creates_the_fallback_directory_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = crate::platform::paths::fallback_endpoint_dir();
        ensure_endpoint_dir(&crate::platform::paths::fallback_attach_socket_path())
            .expect("the fallback endpoint directory is ours to create");
        let mode = std::fs::metadata(&dir)
            .expect("the fallback endpoint directory now exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "{} must be owner-only", dir.display());
    }

    fn bind_trusted(path: &Path) -> std::os::unix::net::UnixListener {
        let listener = std::os::unix::net::UnixListener::bind(path).expect("bind");
        crate::platform::fsperm::set_endpoint_mode_owner_only(path).expect("0o600");
        listener
    }
}
