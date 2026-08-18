//! IPC transport abstraction (PRD #42 M2).
//!
//! Hides the Unix-domain-socket / Windows-named-pipe split behind a single
//! `cfg`-dispatched API so the daemon, attach protocol, and the hook/ui sync
//! clients are transport-agnostic. The Unix backend is a behavior-preserving
//! **lift** of today's `tokio::net::UnixListener`/`UnixStream` and
//! `std::os::unix::net::UnixStream` usage; the Windows backend is the native
//! named-pipe implementation (byte mode, per-instance accept loop).
//!
//! Three types make up the surface:
//!
//! - [`IpcListener`] — `bind(endpoint)` + async `accept() -> IpcStream`.
//!   Replaces the hook listener in `daemon.rs` and the attach listener in
//!   `daemon_protocol.rs`.
//! - [`IpcStream`] — `AsyncRead + AsyncWrite + Unpin + Send`, with async
//!   `connect(endpoint)` and `into_split()`. Callers only ever name the halves
//!   through the [`IpcReadHalf`] / [`IpcWriteHalf`] aliases, so each backend is
//!   free to choose the concrete half type that matches its transport. The
//!   **Unix** backend uses `tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf}`
//!   (via [`tokio::net::UnixStream::into_split`]) so that dropping the write
//!   half alone performs `shutdown(SHUT_WR)` — the native half-close the attach
//!   protocol depends on and that `main` had. (An earlier draft used
//!   [`tokio::io::split`] here, but its generic write half does *not* `SHUT_WR`
//!   on drop, silently regressing the attach half-close on Linux/macOS.) The
//!   **Windows** named-pipe backend has no per-half half-close primitive, so it
//!   keeps [`tokio::io::split`]. `daemon_client.rs`'s protocol helpers were
//!   written against `tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf}` and
//!   stay compatible with both.
//! - [`IpcClient`] — a blocking, single-shot connect handle (`std::io::Read +
//!   Write`) for `hook::send_to_socket` and `ui::send_daemon_request_blocking`.
//!   It has two connect entry points, and the distinction is load-bearing:
//!   `connect` is unbounded, while `connect_timeout` honours a caller deadline
//!   (issue #435). Any caller that advertises a budget must use the latter —
//!   `connect` blocks uninterruptibly on Unix when the daemon's accept queue is
//!   full, so a deadline applied only *after* connecting bounds every step of
//!   the exchange except the one that can hang.
//!
//! Endpoint resolution lives in [`crate::platform::paths`] (a socket path on
//! Unix, a `\\.\pipe\dot-agent-deck-{user}-{hook|attach}` name on Windows);
//! callers pass the resolved [`std::path::Path`] in and this layer consumes it
//! opaquely.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::{IpcClient, IpcListener, IpcReadHalf, IpcStream, IpcWriteHalf};
#[cfg(windows)]
pub use windows::{IpcClient, IpcListener, IpcReadHalf, IpcStream, IpcWriteHalf};

/// Whether an endpoint's *presence* can be observed on the filesystem.
///
/// `true` on Unix: the endpoint is a socket file, so `Path::exists()` is a
/// meaningful (if not authoritative) liveness hint — the daemon does not unlink
/// its socket on exit, but a missing file definitely means no daemon.
///
/// `false` on Windows: a `\\.\pipe\…` name has no filesystem presence at all
/// (`GetFileAttributesW` answers `ERROR_BAD_PATHNAME`), so `exists()` is
/// permanently `false` whether or not a daemon is serving. Any code that treats
/// "path missing" as "daemon gone" must therefore consult this first and fall
/// back to an actual connect attempt — see
/// [`crate::build_version_handshake`]'s `poll_daemon_gone`, where an
/// unconditional `exists()` check would declare *every* live Windows daemon gone
/// on the first poll (PRD #163: "`poll_daemon_gone` path-existence check").
pub const ENDPOINT_IS_FILESYSTEM_PATH: bool = cfg!(unix);

/// Is there a **filesystem artifact** at `endpoint` that a bind would collide
/// with — i.e. is the Unix stale-inode dance applicable at all? (PRD #163 M4,
/// "stale-endpoint dance on Windows".)
///
/// Unix: `true` when the socket file exists. The daemon does not unlink its
/// socket on exit, so an inode there may be a live daemon *or* a crash leftover;
/// the caller distinguishes them with a probe-connect and unlinks the leftover
/// before rebinding.
///
/// Windows: always `false`. A `\\.\pipe\…` name has no inode — nothing to `stat`,
/// nothing to `remove_file` — and the singleton guard is
/// `first_pipe_instance(true)` inside [`IpcListener::bind`], which fails with
/// `AddrInUse` if the name is taken. Running the Unix dance there would be
/// semantically wrong twice over: `exists()` is permanently `false` (so the probe
/// never fires), and if it somehow did, `remove_file` on a pipe name would error
/// rather than clear anything.
///
/// A compile-time platform split, so the Unix code path is bit-identical to
/// before.
pub fn stale_endpoint_artifact(endpoint: &std::path::Path) -> bool {
    ENDPOINT_IS_FILESYSTEM_PATH && endpoint.exists()
}

/// Remove a stale endpoint artifact before binding, where the platform has one.
/// The unconditional-unlink half of the dance described on
/// [`stale_endpoint_artifact`]; used by
/// [`crate::daemon_protocol::bind_attach_listener`], whose singleton protection
/// comes from the caller's spawn lock rather than a probe.
pub fn remove_stale_endpoint(endpoint: &std::path::Path) -> std::io::Result<()> {
    if stale_endpoint_artifact(endpoint) {
        return std::fs::remove_file(endpoint);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use tokio::io::AsyncReadExt;

    use super::*;

    /// A unique endpoint for the transport tests, plus (on Unix) the tempdir it
    /// lives in. Deliberately per-platform *naming* only — every test below drives
    /// the same seam API on both.
    struct TestEndpoint {
        _dir: Option<tempfile::TempDir>,
        path: PathBuf,
    }

    fn unique_endpoint(tag: &str) -> TestEndpoint {
        #[cfg(unix)]
        {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join(format!("{tag}.sock"));
            TestEndpoint {
                _dir: Some(dir),
                path,
            }
        }
        #[cfg(windows)]
        {
            static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            TestEndpoint {
                _dir: None,
                path: PathBuf::from(format!(
                    r"\\.\pipe\dot-agent-deck-test-{tag}-{}-{n}",
                    std::process::id()
                )),
            }
        }
    }

    /// Bind a listener for the transport tests.
    ///
    /// Windows uses the real [`IpcListener::bind`], because there it *is* the
    /// subject under test: that call installs the owner-only pipe security
    /// descriptor. Unix goes through the test-only `from_tokio_listener` instead,
    /// because `IpcListener::bind` flips the process-global umask for the duration
    /// of `bind(2)` and under single-process `cargo test` that can hand a sibling
    /// test's freshly-created tempdir a 0o600 mode — the same hazard the `tests/`
    /// harnesses hold a bind lock for. The subject of these tests on Unix is the
    /// client, not the inode mode, so skipping the dance costs nothing.
    fn bind_for_test(path: &std::path::Path) -> IpcListener {
        #[cfg(unix)]
        {
            IpcListener::from_tokio_listener(
                tokio::net::UnixListener::bind(path).expect("bind the test socket"),
            )
        }
        #[cfg(windows)]
        {
            IpcListener::bind(path).expect("bind the test named pipe")
        }
    }

    /// The synchronous client's read/write deadline — PRD #163 M4's "sync IPC
    /// client timeouts (TUI hang-risk)". A peer that accepts the connection and
    /// then says nothing must NOT hang the caller; it must fail within the budget.
    ///
    /// Un-gated on purpose, so this asserts the contract on all three CI jobs. On
    /// Unix it pins the `SO_RCVTIMEO`/`SO_SNDTIMEO` behavior (`WouldBlock`); on
    /// `windows-latest` it is the live check of the overlapped-I/O implementation
    /// (`TimedOut`), which used to be a signature-stable no-op — the thing that
    /// let a wedged daemon hang the TUI key path forever.
    #[tokio::test]
    async fn sync_client_read_has_a_deadline_instead_of_hanging() {
        let endpoint = unique_endpoint("timeout");
        let _listener = bind_for_test(&endpoint.path);

        let mut client = IpcClient::connect(&endpoint.path).expect("connect the sync client");
        client
            .set_timeouts(Duration::from_millis(250))
            .expect("set the read/write deadline");

        // Writing is not the hazard: the endpoint's kernel buffer absorbs a small
        // frame even with nobody reading. It has to keep working under a deadline.
        client
            .write_all(b"{\"probe\":true}\n")
            .expect("a small write must not block on an unread endpoint");
        client.flush().expect("flush");

        let started = Instant::now();
        let mut buf = [0u8; 1];
        let err = client
            .read(&mut buf)
            .expect_err("a silent peer must not hang the read");
        assert!(
            matches!(
                err.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ),
            "expected a deadline error, got {err:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the deadline must bound the read, took {:?}",
            started.elapsed()
        );

        // The deadline is per-operation, not one-shot: a second read must also
        // return rather than block forever.
        assert!(client.read(&mut buf).is_err());
    }

    /// A full round-trip against our *own* endpoint, twice.
    ///
    /// On `windows-latest` this is the live proof of the [BLOCKER] fix, and the
    /// reason it runs twice: the first connect exercises the instance
    /// [`IpcListener::bind`] created, the second exercises the replacement
    /// [`IpcListener::accept`] creates — and each pipe instance carries its own
    /// security descriptor, so an accept path that forgot the DACL would leave
    /// every connection after the first one world-readable while still passing a
    /// single-connect test. It also proves the owner-only DACL does not lock *us*
    /// out and that `verify_pipe_server_is_current_user` accepts a genuinely
    /// self-owned pipe (the affirmative half; the foreign-owner denial is decided
    /// by `fsperm::endpoint_owner_is_trusted`, tested as pure data because a second
    /// local account is not available in CI).
    ///
    /// On Unix it is a straightforward "the sync client and the async listener
    /// still talk to each other" regression test.
    #[tokio::test]
    async fn own_endpoint_round_trips_on_every_instance() {
        let endpoint = unique_endpoint("roundtrip");
        let listener = bind_for_test(&endpoint.path);

        for round in 0..2u8 {
            let path = endpoint.path.clone();
            let expected = format!("hello-{round}\n");
            let payload = expected.clone();
            let client = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                let mut client = IpcClient::connect(&path)?;
                client.set_timeouts(Duration::from_secs(5))?;
                client.write_all(payload.as_bytes())?;
                client.flush()
            });

            let mut stream = listener.accept().await.expect("accept the client");
            let mut buf = vec![0u8; 32];
            let read = stream
                .read(&mut buf)
                .await
                .expect("read the client's frame");
            client
                .await
                .expect("the client task must not panic")
                .expect("the client's write must succeed");

            assert_eq!(
                &buf[..read],
                expected.as_bytes(),
                "round {round} did not round-trip"
            );
        }
    }

    /// The deadline-aware connect (issue #435) is a drop-in for the plain one
    /// in the two cases every caller meets in practice: a live endpoint still
    /// connects, and an absent one still fails with the same `NotFound` the
    /// unbounded entry point produced — the kind `hook`/`ui` fold into "no
    /// daemon". Neither of those is a timeout, which is exactly the point: the
    /// budget must cost nothing when the daemon is healthy or plainly gone.
    ///
    /// Un-gated on purpose, so both backends' `connect_timeout` are asserted on
    /// all three CI jobs. The *queue-full* case the deadline exists for is
    /// necessarily platform-specific — it needs a listener with a saturated
    /// `listen(2)` backlog — and lives beside the Unix backend in
    /// `unix::tests`, which is where the defect was.
    #[tokio::test]
    async fn connect_timeout_is_a_drop_in_for_the_unbounded_connect() {
        let endpoint = unique_endpoint("connect-timeout");
        let listener = bind_for_test(&endpoint.path);

        let path = endpoint.path.clone();
        let client = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            let mut client = IpcClient::connect_timeout(&path, Duration::from_secs(5))?;
            client.write_all(b"bounded\n")?;
            client.flush()
        });
        let mut stream = listener.accept().await.expect("accept the client");
        let mut buf = vec![0u8; 32];
        let read = stream
            .read(&mut buf)
            .await
            .expect("read the client's frame");
        client
            .await
            .expect("the client task must not panic")
            .expect("a bounded connect to a live endpoint must succeed");
        assert_eq!(&buf[..read], b"bounded\n");

        let absent = unique_endpoint("connect-timeout-absent");
        let started = Instant::now();
        let err = IpcClient::connect_timeout(&absent.path, Duration::from_secs(5))
            .err()
            .map(|e| e.kind())
            .expect("connecting to an endpoint nothing is serving must fail");
        assert_eq!(
            err,
            std::io::ErrorKind::NotFound,
            "an absent endpoint must keep reporting NotFound, got {err:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "an absent endpoint must fail immediately, not burn the budget — took {:?}",
            started.elapsed()
        );
    }

    /// The stale-endpoint predicate keys off the *platform*, not the path shape:
    /// on Unix a real file is an artifact to be cleared, on Windows nothing is.
    /// Runs un-gated on both CI jobs, so the Windows short-circuit is asserted on
    /// `windows-latest` rather than merely `cargo check`'d.
    #[test]
    fn stale_endpoint_artifact_follows_the_platform_not_the_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real_file = dir.path().join("dot-agent-deck.sock");
        std::fs::write(&real_file, b"").expect("create the stand-in endpoint file");

        assert_eq!(stale_endpoint_artifact(&real_file), cfg!(unix));
        assert!(!stale_endpoint_artifact(&dir.path().join("absent.sock")));
        // A pipe name is never an artifact — including on Unix, where it is just
        // an odd relative path that does not exist.
        assert!(!stale_endpoint_artifact(std::path::Path::new(
            r"\\.\pipe\dot-agent-deck-S-1-5-21-1-2-3-4-hook"
        )));
    }

    /// Removal is a no-op wherever there is no artifact, and never an error — the
    /// property `bind_attach_listener` needs so a Windows bind is not preceded by
    /// a doomed `remove_file` on a pipe name.
    #[test]
    fn remove_stale_endpoint_clears_a_file_and_is_a_no_op_otherwise() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real_file = dir.path().join("dot-agent-deck.sock");
        std::fs::write(&real_file, b"").expect("create the stand-in endpoint file");

        remove_stale_endpoint(&real_file).expect("clearing an artifact must succeed");
        assert_eq!(real_file.exists(), !cfg!(unix));

        remove_stale_endpoint(&dir.path().join("absent.sock")).expect("absent is not an error");
        remove_stale_endpoint(std::path::Path::new(
            r"\\.\pipe\dot-agent-deck-S-1-5-21-1-2-3-4-hook",
        ))
        .expect("a pipe name must never be unlinked");
    }
}
