//! Real-subprocess coverage for `dot-agent-deck daemon endpoint`, the remote
//! half of ssh endpoint discovery (issue #1174; see `tests/CATALOG.md`'s
//! `daemon/endpoint` section).
//!
//! The command exists so that the far host's answer to "where does your deck
//! listen" comes from a build of this program running over there, rather than
//! from the shell snippet (`remote_tunnel::REMOTE_SOCKET_PROBE`) that used to
//! restate the rules in `sh` and select a candidate with filesystem tests
//! alone. These tests pin the two refusals that snippet structurally cannot
//! make — a stale inode, and a socket whose mode is wrong — plus the one
//! acceptance, against a real daemon.
//!
//! **What these tests do not claim.** Printing a path is not an assertion that
//! the listener is the deck's daemon: a completed `Hello` proves something is
//! listening and speaks this wire, and an attacker already running as the same
//! user can satisfy every check here. `daemon/endpoint/002` pins the mode
//! refusal precisely because the issue's worst case names a `0666` listener,
//! not because `0600` proves authorship. The in-band step that would make a
//! forwarded endpoint self-describing is tracked separately; see
//! `docs/develop/remote-endpoint-discovery.md`.
//!
//! `/001` and `/002` bind their own listeners, so they need no daemon at all.
//! `/003` queries the in-process daemon `tests/common` provides, the same way
//! `daemon/status/001` does.

use std::path::Path;
use std::time::Duration;

#[cfg(unix)]
use spec::spec;

mod common;

/// What one real `dot-agent-deck daemon endpoint` subprocess produced.
#[cfg(unix)]
struct CliEndpointResult {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

/// Run the REAL built binary's `daemon endpoint` against `attach_socket`.
///
/// `DOT_AGENT_DECK_ATTACH_SOCKET` is the same override
/// `platform::paths::attach_socket_path` honours first, so the subprocess
/// resolves exactly the path under test — which is also the first rung of the
/// discovery snippet, and the one that was printed with no checks at all.
#[cfg(unix)]
fn run_daemon_endpoint_cli(attach_socket: &Path) -> CliEndpointResult {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"));
    cmd.arg("daemon").arg("endpoint");
    cmd.env("DOT_AGENT_DECK_ATTACH_SOCKET", attach_socket);
    let output = cmd
        .output()
        .expect("run the real `dot-agent-deck daemon endpoint` CLI as a subprocess");
    CliEndpointResult {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// The exit code for "something is here and it failed a trust check": the
/// caller must stop rather than fall through to an unchecked rung.
#[cfg(unix)]
const ENDPOINT_UNTRUSTED: i32 = 1;

/// The exit code for "nothing was learned": no inode, nothing answering, or a
/// timed-out handshake. The caller keeps looking.
#[cfg(unix)]
const ENDPOINT_UNDETERMINED: i32 = 3;

/// Assert a refusal is a *handled* one carrying exactly `expected_code`: no
/// panic, and not clap's own exit 2 / `Usage:` banner, which is what a build
/// lacking the subcommand produces.
///
/// The code is asserted rather than merely "non-zero" because the probe acts on
/// the difference — `1` binds and ends discovery, anything else falls through
/// to a rung that prints the path unchecked. A refusal that drifted from `1` to
/// `3` would silently make every trust check here advisory again (PR #1191
/// review, P1), with no test noticing.
///
/// Without the clap checks these tests would also pass on a build where
/// `daemon endpoint` does not exist at all — the same trap
/// `daemon/status/003` documents.
#[cfg(unix)]
fn assert_handled_refusal(result: &CliEndpointResult, expected_code: i32, what: &str) {
    assert!(
        !result.status.success(),
        "{what} must not report success; status={:?} stdout={:?} stderr={:?}",
        result.status,
        result.stdout,
        result.stderr
    );
    assert!(
        result.stdout.trim().is_empty(),
        "{what} must print no path at all — the caller reads stdout and would forward \
         whatever is there; stdout={:?}",
        result.stdout
    );
    assert!(
        !result.stderr.contains("panicked"),
        "{what} must be a controlled diagnostic, not a Rust panic; stderr={:?}",
        result.stderr
    );
    assert_ne!(
        result.status.code(),
        Some(2),
        "exit code 2 is clap's own usage/parse-error code; a refusal that collides with it \
         cannot be told apart from a build that does not have this subcommand; status={:?} \
         stderr={:?}",
        result.status,
        result.stderr
    );
    assert_eq!(
        result.status.code(),
        Some(expected_code),
        "{what} must exit exactly {expected_code}; the probe branches on this value, so a \
         drift here changes whether the refusal binds or is fallen through; status={:?} \
         stderr={:?}",
        result.status,
        result.stderr
    );
    assert!(
        !result.stderr.contains("Usage:"),
        "stderr carries clap's subcommand-usage banner, so `endpoint` was not recognised as a \
         real `daemon` subcommand and this test proved nothing; stderr={:?}",
        result.stderr
    );
}

/// Scenario: Bind a Unix socket at the attach path, drop the listener so only the inode is left, chmod it to 0o600, and run the REAL `dot-agent-deck daemon endpoint` CLI as a subprocess. Assert it refuses — printing no path and exiting non-zero for a handled reason rather than clap's usage error — because nothing is listening there, and assert it leaves the inode alone.
#[spec("daemon/endpoint/001")]
#[test]
#[cfg(unix)]
fn daemon_endpoint_001_refuses_a_stale_inode_no_daemon_is_listening_on() {
    use std::os::unix::fs::PermissionsExt;

    common::init_test_env();
    let scratch = common::race_safe_tempdir();
    let attach_path = scratch.path().join("stale.sock");

    // Bind and immediately drop: the listener is gone, the inode remains. This
    // is exactly what a daemon killed with SIGKILL leaves behind, and it is
    // the case the discovery snippet's filesystem tests cannot see — `-S`
    // answers about the inode, and a dead daemon's inode is still a socket.
    {
        let _listener =
            std::os::unix::net::UnixListener::bind(&attach_path).expect("bind the stale endpoint");
    }
    std::fs::set_permissions(&attach_path, std::fs::Permissions::from_mode(0o600))
        .expect("chmod the stale endpoint to the mode a real daemon binds");
    assert!(
        attach_path.exists(),
        "test precondition violated: the stale inode must still be present"
    );

    let result = run_daemon_endpoint_cli(&attach_path);
    assert_handled_refusal(
        &result,
        ENDPOINT_UNDETERMINED,
        "a stale inode with no listener",
    );

    assert!(
        attach_path.exists(),
        "the resolver is read-only: it must not unlink the inode it refused, which is the \
         daemon's own recovery to perform"
    );
    assert!(
        std::os::unix::net::UnixStream::connect(&attach_path).is_err(),
        "the resolver must not lazy-spawn a daemon at the endpoint it was asked about — \
         a read-only query that brings the thing it queried into existence is the defect \
         `daemon/status/003` pins for the sibling command"
    );
}

/// Scenario: Bind a Unix socket at the attach path, keep a live listener on it, and chmod it to 0o666 — the mode the issue's impersonation case names. Run the REAL `dot-agent-deck daemon endpoint` CLI as a subprocess and assert it refuses on the mode, naming it, even though something genuinely is listening.
#[spec("daemon/endpoint/002")]
#[test]
#[cfg(unix)]
fn daemon_endpoint_002_refuses_a_live_listener_whose_mode_is_not_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    common::init_test_env();
    let scratch = common::race_safe_tempdir();
    let attach_path = scratch.path().join("world-writable.sock");

    // Held for the whole test: this listener is LIVE, so a connect would
    // succeed and only the mode check can refuse it. That is the point — the
    // discovery snippet has no portable `test` spelling for a mode, so every
    // clause it can carry passes here.
    let _listener =
        std::os::unix::net::UnixListener::bind(&attach_path).expect("bind the wide-open endpoint");
    std::fs::set_permissions(&attach_path, std::fs::Permissions::from_mode(0o666))
        .expect("chmod the endpoint world-writable");

    let result = run_daemon_endpoint_cli(&attach_path);
    assert_handled_refusal(&result, ENDPOINT_UNTRUSTED, "a live listener at mode 0o666");

    assert!(
        result.stderr.contains("666"),
        "the refusal must name the mode it refused, so an operator can act on it; stderr={:?}",
        result.stderr
    );
}

/// Scenario: Start the in-process daemon, wait for its attach socket to accept, and run the REAL `dot-agent-deck daemon endpoint` CLI as a subprocess against it. Assert it exits 0 and prints exactly that socket path on a single line — the shape the discovery snippet forwards.
#[spec("daemon/endpoint/003")]
#[test]
#[cfg(unix)]
fn daemon_endpoint_003_prints_the_path_when_a_real_daemon_answers() {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build daemon-endpoint live runtime")
        .block_on(daemon_endpoint_003_prints_the_path_when_a_real_daemon_answers_inner());
}

#[cfg(unix)]
async fn daemon_endpoint_003_prints_the_path_when_a_real_daemon_answers_inner() {
    let daemon = common::spawn_inprocess_daemon().await;
    wait_for_attach_socket(&daemon.attach_path, Duration::from_secs(10)).await;

    let attach_path = daemon.attach_path.clone();
    let result = tokio::task::spawn_blocking(move || run_daemon_endpoint_cli(&attach_path))
        .await
        .expect("daemon endpoint CLI subprocess task did not panic");

    assert!(
        result.status.success(),
        "a live daemon must be discoverable; status={:?} stdout={:?} stderr={:?}",
        result.status,
        result.stdout,
        result.stderr
    );

    // Exactly one line, and that line is the path. The caller
    // (`endpoint_test::discover_socket`) takes the LAST non-empty line and
    // parses it, so extra output would silently change which value is
    // forwarded.
    let lines: Vec<&str> = result
        .stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "the resolver must print exactly one line; stdout={:?}",
        result.stdout
    );
    assert_eq!(
        lines[0],
        daemon.attach_path.to_string_lossy(),
        "the printed path must be the endpoint the daemon actually bound"
    );

    daemon.registry.shutdown_all();
}

/// Wait until the in-process daemon's attach endpoint accepts connections.
/// `spawn_inprocess_daemon` proves hook-socket readiness, but the attach bind
/// happens immediately afterward and the CLI subprocess must not race it.
#[cfg(unix)]
async fn wait_for_attach_socket(attach_socket: &Path, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if tokio::net::UnixStream::connect(attach_socket).await.is_ok() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "in-process daemon attach socket {} was not accepting connections within {timeout:?}",
            attach_socket.display()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
