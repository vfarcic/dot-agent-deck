// Unix-only: the harness binds Unix sockets and drives `RLIMIT_NOFILE` and
// `shutdown(2)` through `libc`.
#![cfg(unix)]
//! Issue #1794: a daemon must never be left half-alive after its attach
//! listener fails to accept. One `EMFILE` used to end the attach server while
//! the hook endpoint kept serving, so no client could reach the daemon and no
//! replacement could start (its single-instance guard saw the live hook
//! socket).
//!
//! The first test drives a real in-process daemon into `EMFILE`; the others
//! drive the attach supervisor directly with a stand-in server. The first one
//! lowers the process's descriptor limit, which would starve any test running
//! beside it, so every test holds [`SERIAL`] for its whole run: under
//! `cargo test` they share a process, and only under nextest does each get its
//! own.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use spec::spec;
use tempfile::TempDir;
use tokio::net::UnixStream;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use dot_agent_deck::agent_pty::AgentPtyRegistry;
use dot_agent_deck::attach_recovery;
use dot_agent_deck::daemon::{Daemon, run_daemon_with};
use dot_agent_deck::daemon_client::DaemonClient;
use dot_agent_deck::platform::ipc::IpcListener;
use dot_agent_deck::state::{AppState, SharedState};

mod common;

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct DaemonHandle {
    attach_path: PathBuf,
    pty_registry: Arc<AgentPtyRegistry>,
    handle: JoinHandle<()>,
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        self.handle.abort();
        self.pty_registry.shutdown_all();
    }
}

/// Start a daemon on `hook_path` and `attach_path` and wait until its attach
/// socket answers a request.
async fn spawn_daemon(hook_path: &Path, attach_path: &Path) -> DaemonHandle {
    common::init_test_env();
    let state: SharedState = Arc::new(RwLock::new(AppState::default()));
    let daemon = Daemon::with_attach(state, attach_path.to_path_buf())
        .with_idle_shutdown(None)
        .with_lock_dir_override(common::lock_dir_path());
    let pty_registry = daemon.pty_registry.clone();
    let hook_for_daemon = hook_path.to_path_buf();
    let handle = tokio::spawn(async move {
        let _ = run_daemon_with(&hook_for_daemon, daemon).await;
    });
    let budget = common::daemon_task_start_budget().max(common::INPROC_DAEMON_READY_TIMEOUT);
    assert!(
        attach_answers_within(attach_path, budget).await,
        "attach socket {} was not answering within {budget:?}",
        attach_path.display()
    );
    DaemonHandle {
        attach_path: attach_path.to_path_buf(),
        pty_registry,
        handle,
    }
}

/// Whether a client reaches the daemon at `attach_path` — what `dot-agent-deck`
/// does first — within `budget`.
async fn attach_answers_within(attach_path: &Path, budget: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if UnixStream::connect(attach_path).await.is_ok()
            && DaemonClient::new(attach_path.to_path_buf())
                .list_agents()
                .await
                .is_ok()
        {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Two sibling directories, so a test can make the attach endpoint's directory
/// read-only without touching the hook endpoint's.
fn endpoint_dirs() -> (TempDir, PathBuf, PathBuf) {
    let root = common::race_safe_tempdir();
    let hook_dir = root.path().join("hook");
    let attach_dir = root.path().join("attach");
    for dir in [&hook_dir, &attach_dir] {
        std::fs::create_dir(dir).expect("create endpoint dir");
        set_mode(dir, 0o700);
    }
    let hook = hook_dir.join("hook.sock");
    let attach = attach_dir.join("attach.sock");
    (root, hook, attach)
}

fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

/// Lowers the soft descriptor limit and holds every free descriptor under it,
/// so the next `accept` fails with `EMFILE`. Dropping it closes them and
/// restores the limit.
struct FdExhaustion {
    held: Vec<libc::c_int>,
    original: libc::rlimit,
}

impl FdExhaustion {
    fn start() -> Self {
        let mut original = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: `getrlimit` writes into the struct we pass and nothing else.
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut original) },
            0
        );
        let open = std::fs::read_dir("/dev/fd").expect("list /dev/fd").count() as libc::rlim_t;
        let lowered = libc::rlimit {
            rlim_cur: (open + 32).min(original.rlim_cur),
            rlim_max: original.rlim_max,
        };
        // SAFETY: lowering the soft limit below the hard one is always allowed.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lowered) }, 0);
        let mut exhaustion = Self {
            held: Vec::new(),
            original,
        };
        loop {
            // SAFETY: `dup` of stderr either returns a fresh descriptor this
            // struct then owns, or -1.
            let fd = unsafe { libc::dup(2) };
            if fd < 0 {
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::EMFILE),
                    "filling the descriptor table must end at EMFILE"
                );
                break;
            }
            exhaustion.held.push(fd);
        }
        assert!(
            !exhaustion.held.is_empty(),
            "no descriptor was free to hold"
        );
        exhaustion
    }

    /// Close one held descriptor, so exactly one is free.
    fn release_one(&mut self) -> bool {
        match self.held.pop() {
            Some(fd) => {
                // SAFETY: `fd` came from `dup` above and is closed only here.
                unsafe { libc::close(fd) };
                true
            }
            None => false,
        }
    }
}

impl Drop for FdExhaustion {
    fn drop(&mut self) {
        for fd in self.held.drain(..) {
            // SAFETY: as in `release_one`.
            unsafe { libc::close(fd) };
        }
        // SAFETY: restoring the limit this process had before `start`.
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &self.original) };
    }
}

/// Scenario: a daemon is running with its attach socket answering. Every free
/// file descriptor in the process is taken and a client connects with the last
/// one, so the daemon's next `accept` fails with "Too many open files"; then
/// the descriptors are released. A client connecting afterwards must reach the
/// daemon, as `dot-agent-deck` would, and the one that connected during the
/// shortage must still be connected.
#[spec("daemon/attach-accept/001")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_transient_emfile_on_accept_does_not_end_the_attach_server() {
    let _serial = SERIAL.lock().await;
    let (_root, hook, attach) = endpoint_dirs();
    let daemon = spawn_daemon(&hook, &attach).await;

    let pending = {
        let mut exhaustion = FdExhaustion::start();
        // Free exactly one descriptor and connect with it, so the client end
        // exists and the daemon's accept has none left for the server end. A
        // daemon task can take the freed slot first; free another and retry.
        let pending = loop {
            assert!(exhaustion.release_one(), "ran out of descriptors to free");
            match std::os::unix::net::UnixStream::connect(&attach) {
                Ok(stream) => break stream,
                Err(e) if e.raw_os_error() == Some(libc::EMFILE) => continue,
                Err(e) => panic!("connecting to the attach socket failed: {e}"),
            }
        };
        // The daemon is woken by that connection at once; this window is what
        // gives its accept time to run, and fail, while nothing is free.
        tokio::time::sleep(Duration::from_millis(500)).await;
        pending
    };

    let budget = common::daemon_task_start_budget().max(Duration::from_secs(10));
    assert!(
        attach_answers_within(&daemon.attach_path, budget).await,
        "after one transient EMFILE the attach socket {} stopped answering: the attach \
         server ended while the daemon stayed up (issue #1794)",
        daemon.attach_path.display()
    );
    assert!(!daemon.handle.is_finished(), "the daemon must keep running");
    // The client that connected during the shortage is still connected: the
    // daemon accepted it once descriptors were free, rather than closing the
    // listener under it. Nothing was sent, so a read waits.
    pending
        .set_read_timeout(Some(Duration::from_millis(300)))
        .expect("set read timeout");
    let mut byte = [0u8; 1];
    match std::io::Read::read(&mut &pending, &mut byte) {
        Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
        other => panic!(
            "the client that connected while descriptors ran out was dropped ({other:?}); \
             the attach server must keep its listener and accept it once they are free"
        ),
    }
}

/// A stand-in for the daemon's attach server, for driving
/// [`attach_recovery::supervise`] directly: no daemon-level fault reaches the
/// "server ended with a non-transient error" branch on demand. The first
/// `fail_first` runs end at once with an error about the listener itself; every
/// later one serves until the test ends, accepting and dropping connections.
fn stand_in_server(
    fail_first: usize,
    runs: Arc<AtomicUsize>,
) -> impl FnMut(IpcListener) -> BoxedServe {
    move |listener| {
        let run = runs.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if run < fail_first {
                drop(listener);
                return Err(std::io::Error::other("the listener is broken"));
            }
            loop {
                let _ = listener.accept().await?;
            }
        })
    }
}

type BoxedServe = std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<()>> + Send>>;

fn bind_attach(attach: &Path) -> IpcListener {
    dot_agent_deck::daemon_protocol::bind_attach_listener(attach).expect("bind attach")
}

/// Run [`attach_recovery::supervise`] over `listener`, bound at `attach`, with
/// a `lost` action that records that it ran.
fn spawn_supervisor(
    attach: &Path,
    listener: IpcListener,
    fail_first: usize,
) -> (JoinHandle<()>, Arc<AtomicUsize>, Arc<AtomicBool>) {
    let runs = Arc::new(AtomicUsize::new(0));
    let lost = Arc::new(AtomicBool::new(false));
    let lost_flag = lost.clone();
    let handle = tokio::spawn(attach_recovery::supervise(
        attach.to_path_buf(),
        listener,
        stand_in_server(fail_first, runs.clone()),
        move || async move { lost_flag.store(true, Ordering::SeqCst) },
    ));
    (handle, runs, lost)
}

async fn connects_within(path: &Path, budget: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + budget;
    while tokio::time::Instant::now() < deadline {
        if UnixStream::connect(path).await.is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

/// Scenario: the attach server ends with an error about its listener rather
/// than a passing one. A client connecting to the same attach path shortly
/// after must get through, because the daemon listens again instead of
/// stopping, and the daemon is not shut down.
#[spec("daemon/attach-accept/002")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_attach_server_that_ends_is_replaced_on_the_same_path() {
    let _serial = SERIAL.lock().await;
    let (_root, _hook, attach) = endpoint_dirs();
    let (handle, runs, lost) = spawn_supervisor(&attach, bind_attach(&attach), 1);

    // Wait for the second run first: until the first one ends, the original
    // listener still answers a connect.
    let budget = common::daemon_task_start_budget().max(Duration::from_secs(10));
    let deadline = tokio::time::Instant::now() + budget;
    while runs.load(Ordering::SeqCst) < 2 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        runs.load(Ordering::SeqCst),
        2,
        "the attach server was not started again within {budget:?}"
    );
    assert!(
        connects_within(&attach, budget).await,
        "after the attach server ended, nothing listened at {} again",
        attach.display()
    );
    assert!(
        !lost.load(Ordering::SeqCst),
        "a listener that could be put back must not stop the daemon"
    );
    assert!(!handle.is_finished(), "supervision must go on");
    handle.abort();
}

/// Scenario: the attach server ends and the attach socket's directory is
/// read-only, so no listener can be bound there again. The daemon's stop
/// action must run, so it does not stay up with its hook endpoint live and no
/// client able to reach it.
#[spec("daemon/attach-accept/003")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_attach_server_that_cannot_listen_again_stops_the_daemon() {
    let _serial = SERIAL.lock().await;
    // SAFETY: `geteuid` has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("SKIP: root ignores the read-only directory this test relies on");
        return;
    }
    let (_root, _hook, attach) = endpoint_dirs();
    let attach_dir = attach.parent().expect("attach dir").to_path_buf();
    let listener = bind_attach(&attach);
    set_mode(&attach_dir, 0o500);
    let (handle, runs, lost) = spawn_supervisor(&attach, listener, usize::MAX);

    let budget = attach_recovery::REBIND_DELAYS.iter().sum::<Duration>()
        + common::daemon_task_start_budget();
    let ended = tokio::time::timeout(budget, handle).await;
    set_mode(&attach_dir, 0o700);
    assert!(
        ended.is_ok(),
        "supervision kept going for {budget:?} with no listener to serve"
    );
    assert!(
        lost.load(Ordering::SeqCst),
        "the daemon's stop action must run once no listener can be bound"
    );
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "nothing to serve on was bound"
    );
}

/// Scenario: the attach server ends every time it is started, right away. The
/// daemon must stop restarting it after a bounded number of tries and run its
/// stop action, rather than loop for ever.
#[spec("daemon/attach-accept/004")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_attach_server_that_keeps_ending_is_not_restarted_for_ever() {
    let _serial = SERIAL.lock().await;
    let (_root, _hook, attach) = endpoint_dirs();
    let (handle, runs, lost) = spawn_supervisor(&attach, bind_attach(&attach), usize::MAX);

    let budget = attach_recovery::REBIND_DELAYS[0] * 10 + common::daemon_task_start_budget();
    let ended = tokio::time::timeout(budget, handle).await;
    assert!(
        ended.is_ok(),
        "supervision kept restarting a server that always ends for {budget:?}"
    );
    assert!(lost.load(Ordering::SeqCst), "the stop action must run");
    assert_eq!(
        runs.load(Ordering::SeqCst),
        attach_recovery::MAX_RESTARTS_IN_WINDOW + 1,
        "the first run plus the bounded restarts"
    );
}
