//! PRD #92 F5 — descendant-process kill semantics.
//!
//! Before F5, the daemon's kill paths sent `kill(pid, SIGKILL)` to the
//! direct child only. Commands containing a space were launched via
//! `$SHELL -c <cmd>`, so the registered PID was the shell's — `kill`
//! tore down the shell, but every process the shell had spawned (the
//! actual agent, language servers, file watchers, etc.) was orphaned to
//! init and survived. The daemon and TUI both thought the kill
//! succeeded; the user found stale processes hanging around after
//! Ctrl+W.
//!
//! F5 switches the kill paths to `killpg(pgid, SIGKILL)` (and `killpg
//! SIGTERM` for the graceful escalation phase) so the entire process
//! group dies together. `portable-pty` already makes every PTY child a
//! session leader via `setsid()` in its `pre_exec`, so the child's PID
//! equals its session ID and process-group ID — no additional spawn-path
//! setup needed.
//!
//! This test launches a shell-wrapped agent whose shell spawns a
//! long-lived descendant (`sh -c 'sleep 30 & echo $! > pid_file ; wait
//! "$pid"'`), reads the descendant PID once the shell has written it,
//! closes the pane via the controller (which goes through `close_agent`
//! → `force_kill_child_and_wait`), and asserts that both the shell PID
//! and the descendant PID are dead (`kill(pid, 0)` returns ESRCH) within
//! a bounded wait.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serial_test::serial;
use tempfile::TempDir;
use tokio::task::JoinHandle;

use dot_agent_deck::agent_pty::AgentPtyRegistry;
use dot_agent_deck::daemon_protocol::{bind_attach_listener, serve_attach};
use dot_agent_deck::embedded_pane::EmbeddedPaneController;
use dot_agent_deck::pane::PaneController;

static HARNESS_BIND_LOCK: Mutex<()> = Mutex::new(());

struct Server {
    _dir: TempDir,
    path: PathBuf,
    registry: Arc<AgentPtyRegistry>,
    handle: JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn start_server() -> Server {
    let registry = Arc::new(AgentPtyRegistry::new());

    let (dir, path, listener) = {
        let _g = HARNESS_BIND_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("attach.sock");
        let listener = bind_attach_listener(&path).expect("bind attach listener");
        (dir, path, listener)
    };

    let registry_for_task = registry.clone();
    let (event_tx, _) = tokio::sync::broadcast::channel(16);
    let handle = tokio::spawn(async move {
        let _ = serve_attach(listener, registry_for_task, event_tx).await;
    });

    Server {
        _dir: dir,
        path,
        registry,
        handle,
    }
}

/// `kill(pid, 0)` returns 0 if the process exists (regardless of whether
/// the caller could actually signal it), and `-1` with ESRCH if it
/// doesn't. We use it as a non-destructive liveness probe for the
/// shell + descendant during the post-close polling loop.
fn pid_is_alive(pid: i32) -> bool {
    // SAFETY: `kill(pid, 0)` does not signal — it only probes. ESRCH
    // is the expected outcome once the kernel has reaped the process.
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        true
    } else {
        // EPERM means the process exists but we can't signal it — for
        // tests this never happens (same user owns everything) but
        // treat it as alive to be safe.
        std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
}

/// Poll a predicate until either it returns true or the timeout elapses.
async fn wait_for<F: FnMut() -> bool>(timeout: Duration, interval: Duration, mut pred: F) -> bool {
    let start = tokio::time::Instant::now();
    while tokio::time::Instant::now() - start < timeout {
        if pred() {
            return true;
        }
        tokio::time::sleep(interval).await;
    }
    pred()
}

/// PRD #92 F5: closing a shell-wrapped agent must reap both the shell
/// itself and every descendant the shell had spawned. Pre-F5 the daemon
/// sent `kill(shell_pid, SIGKILL)` only; the descendant survived as an
/// orphan re-parented to init. Post-F5 it sends `killpg(shell_pid,
/// SIGKILL)`, taking the whole group down together.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(process_group_kill)]
async fn close_pane_reaps_shell_descendants() {
    let server = start_server().await;
    let ctrl = Arc::new(EmbeddedPaneController::new(
        server.path.clone(),
        tokio::runtime::Handle::current(),
    ));

    // Use a shared tempdir for the pid-relay file. The shell writes its
    // `sleep` child's pid to this file via `echo $! > FILE` so the test
    // can capture it without relying on `/proc/<pid>/task/children` or
    // `pgrep -P` (both Linux-only and brittle in CI).
    let pid_dir = tempfile::tempdir().unwrap();
    let pid_file = pid_dir.path().join("descendant.pid");

    // `sh -c 'sleep 30 & echo $! > FILE; wait "$pid"'`
    //
    // Why this shape: `sleep 30 &` backgrounds the child; `$!` is the
    // child's PID and the shell writes it to FILE; then the shell
    // `wait`s on it so the shell process keeps the PTY open and the
    // daemon sees the registered (shell) PID stay alive — both the
    // shell and the descendant must die together when F5's killpg
    // signals the group.
    let cmd = format!("sleep 30 & echo $! > {} ; wait $!", pid_file.display());
    let cmd_for_pane = cmd.clone();
    let pane_id = {
        let ctrl = ctrl.clone();
        tokio::task::spawn_blocking(move || ctrl.create_pane(Some(&cmd_for_pane), None).unwrap())
            .await
            .unwrap()
    };

    // Capture the daemon-registered (shell) PID via the registry.
    let agent_ids = server.registry.agent_ids();
    assert_eq!(agent_ids.len(), 1, "exactly one agent should be registered");
    let shell_pid = server
        .registry
        .child_pid(&agent_ids[0])
        .expect("daemon-side child should expose a pid") as i32;

    // Wait for the shell to write the descendant pid into the relay file.
    // The shell does `echo $!` after backgrounding `sleep 30`, so the
    // file appears within a few PTY tick cycles. The earlier 3 s budget
    // proved insufficient on loaded CI runners (PRD #92 PR #105 — CI
    // hit the 'shell never wrote pid' branch repeatedly even though
    // the shell was fine). The shell + descendant otherwise live for
    // `sleep 30` so a 10 s ceiling is still well under the natural
    // lifetime and well over any realistic shell-startup cost.
    let pid_file_ready = wait_for(Duration::from_secs(10), Duration::from_millis(20), || {
        pid_file.exists()
            && std::fs::metadata(&pid_file)
                .map(|m| m.len() > 0)
                .unwrap_or(false)
    })
    .await;
    assert!(
        pid_file_ready,
        "shell never wrote the descendant pid to {} — was the shell still starting?",
        pid_file.display()
    );

    let descendant_pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .expect("descendant pid file should contain a numeric PID");

    // Sanity-check: both the shell and the descendant are alive before
    // the close. Otherwise the test would pass for the wrong reason
    // (a descendant that died on its own would look like a successful
    // F5 fix even on the pre-F5 code).
    assert!(
        pid_is_alive(shell_pid),
        "shell pid {shell_pid} should be alive before close"
    );
    assert!(
        pid_is_alive(descendant_pid),
        "descendant pid {descendant_pid} should be alive before close"
    );

    // Trigger the close path (Ctrl+W ➜ EmbeddedPaneController::close_pane
    // ➜ daemon StopAgent ➜ AgentPtyRegistry::close_agent ➜
    // force_kill_child_and_wait, which post-F5 uses `killpg`).
    let ctrl_for_close = ctrl.clone();
    let pane_id_for_close = pane_id.clone();
    tokio::task::spawn_blocking(move || ctrl_for_close.close_pane(&pane_id_for_close).unwrap())
        .await
        .unwrap();

    // Both PIDs must die together. Generous timeout because the OS
    // delivery of SIGKILL is async, but well under the descendant's
    // `sleep 30` so we know the kill is doing the work (not the timer).
    let shell_dead = wait_for(Duration::from_secs(3), Duration::from_millis(20), || {
        !pid_is_alive(shell_pid)
    })
    .await;
    assert!(
        shell_dead,
        "shell pid {shell_pid} should be dead within 3s after close — kill (or killpg) failed"
    );

    let descendant_dead = wait_for(Duration::from_secs(3), Duration::from_millis(20), || {
        !pid_is_alive(descendant_pid)
    })
    .await;
    assert!(
        descendant_dead,
        "descendant pid {descendant_pid} should be dead within 3s after close — PRD #92 F5 regression: killpg is not signalling the whole process group"
    );
}

/// PRD #92 F8: closing a well-behaved agent must deliver SIGTERM and
/// give the agent a window to run its own cleanup. Pre-F8 the Ctrl+W
/// path sent SIGKILL directly (uncatchable), so even an agent that
/// wanted to clean up its `setsid`'d sub-shells had no opportunity to
/// do so. Post-F8 the agent gets SIGTERM with a 3-second grace
/// before SIGKILL.
///
/// The well-behaved shape we test: a shell that traps SIGTERM, writes
/// a sentinel file, and exits. After the close we assert the sentinel
/// exists on disk — proof that SIGTERM was delivered and the trap ran.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(process_group_kill)]
async fn close_pane_well_behaved_agent_runs_sigterm_trap() {
    let server = start_server().await;
    let ctrl = Arc::new(EmbeddedPaneController::new(
        server.path.clone(),
        tokio::runtime::Handle::current(),
    ));

    // The sentinels live in a shared tempdir so the test can probe
    // them after the close. The shell traps TERM, writes the
    // `trapped` sentinel and exits 0 on SIGTERM — well-behaved
    // agent behavior. It also writes a `ready` sentinel **before**
    // entering `sleep 60` so the test can wait for the trap to be
    // installed without depending on a fixed sleep (F8 followup
    // auditor #4 — fixed-sleep races on a slow CI host).
    let sentinel_dir = tempfile::tempdir().unwrap();
    let trapped_path = sentinel_dir.path().join("trapped.flag");
    let ready_path = sentinel_dir.path().join("ready.flag");

    // `sh -c '
    //    trap "echo trapped > TRAPPED; exit 0" TERM
    //    echo ready > READY
    //    sleep 60
    // '`
    //
    // The `echo ready > READY` runs AFTER `trap ...`, so by the time
    // the test sees `ready.flag` on disk we know the trap is armed.
    // The trailing `sleep 60` keeps the shell alive long enough that
    // the test's close hits while the shell is parked in `sleep` —
    // SIGTERM lands while the trap is armed and runs before the
    // shell exits.
    let cmd = format!(
        "trap 'echo trapped > {} ; exit 0' TERM; echo ready > {} ; sleep 60",
        trapped_path.display(),
        ready_path.display(),
    );
    let cmd_for_pane = cmd.clone();
    let pane_id = {
        let ctrl = ctrl.clone();
        tokio::task::spawn_blocking(move || ctrl.create_pane(Some(&cmd_for_pane), None).unwrap())
            .await
            .unwrap()
    };

    // Wait for the `ready` sentinel — proves the trap is installed.
    // The shell's `trap` + `echo ready` runs in well under 100 ms on a
    // quiet host; budget is generous because loaded CI runners have
    // been observed to take seconds to schedule fork+exec+shell-init
    // (PRD #92 PR #105 — the 5 s budget here was hit deterministically
    // on a CI run even though the shell was fine). Combined with
    // `#[serial(process_group_kill)]` above, contention from sibling
    // PTY-heavy tests in this file is removed; the budget is the
    // last line of defense against runner-wide load.
    let ready_appeared = wait_for(Duration::from_secs(10), Duration::from_millis(20), || {
        ready_path.exists()
            && std::fs::metadata(&ready_path)
                .map(|m| m.len() > 0)
                .unwrap_or(false)
    })
    .await;
    assert!(
        ready_appeared,
        "shell never wrote the 'ready' sentinel at {} — trap may not be installed; aborting before close to avoid a false-negative pass",
        ready_path.display()
    );

    assert!(
        !trapped_path.exists(),
        "'trapped' sentinel must not exist before the close — the trap hasn't fired yet"
    );

    // Trigger the close path (Ctrl+W → EmbeddedPaneController::close_pane
    // → daemon StopAgent → AgentPtyRegistry::close_agent → F8
    // terminate_child_with_grace_and_wait, which sends SIGTERM first).
    let ctrl_for_close = ctrl.clone();
    let pane_id_for_close = pane_id.clone();
    tokio::task::spawn_blocking(move || ctrl_for_close.close_pane(&pane_id_for_close).unwrap())
        .await
        .unwrap();

    // The trap should have fired and written the sentinel. Poll up to
    // 2 s so a slightly-busy scheduler doesn't make the test flaky;
    // the trap itself is well under 100 ms on any reasonable host.
    let trapped_appeared = wait_for(Duration::from_secs(2), Duration::from_millis(20), || {
        trapped_path.exists()
    })
    .await;
    assert!(
        trapped_appeared,
        "'trapped' sentinel at {} must exist after close — SIGTERM trap did not run within the 3 s F8 grace window",
        trapped_path.display()
    );

    let contents = std::fs::read_to_string(&trapped_path).expect("read sentinel");
    assert!(
        contents.contains("trapped"),
        "'trapped' sentinel must contain the trap's payload, got {contents:?}"
    );
}

/// PRD #92 F8: an uncooperative agent that ignores SIGTERM at BOTH
/// shell layers must still be reaped after the grace window via the
/// SIGKILL backstop. Pre-F8 this would happen instantly (raw SIGKILL);
/// post-F8 it happens after the 3-second grace + a few hundred
/// milliseconds of SIGKILL delivery + reap.
///
/// PRD #92 F8 followup (reviewer + auditor #3): the earlier shape of
/// this test used `trap "" TERM; sleep 60` — only the parent shell
/// ignored SIGTERM, but `killpg(SIGTERM)` also reached the `sleep`
/// child which has no trap and dies on SIGTERM naturally; the shell's
/// `wait` then returned and the shell exited, so the test passed
/// WITHOUT the SIGKILL backstop firing. Worse, the assertion was only
/// an upper bound (`< 3.5 s`), so a regression to pre-F8
/// immediate-SIGKILL semantics would have passed too. The fix:
///   1. Trap SIGTERM at BOTH layers (parent shell AND the backgrounded
///      child) so `killpg(SIGTERM)` is ignored top-to-bottom and the
///      shell cannot exit via "child died on SIGTERM, wait returned."
///   2. Assert a **lower** bound on the close-path duration: it must
///      take at least ~2.8 s (the 3 s F8 grace, minus 200 ms slack for
///      scheduler / setup overhead). Pre-F8 SIGKILL-only would close
///      in milliseconds and fail this bound.
/// The combined shape proves both that the grace was waited out AND
/// that the SIGKILL backstop was actually needed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(process_group_kill)]
async fn close_pane_uncooperative_agent_killed_after_grace() {
    let server = start_server().await;
    let ctrl = Arc::new(EmbeddedPaneController::new(
        server.path.clone(),
        tokio::runtime::Handle::current(),
    ));

    // Both the parent shell AND the backgrounded subshell ignore
    // SIGTERM. The parent's `wait` parks on the subshell, and since
    // the subshell ignores SIGTERM too, `killpg(SIGTERM)` cannot
    // terminate either process. Only the daemon's SIGKILL backstop
    // (delivered after AGENT_TERMINATE_GRACE) can reap them.
    //
    // PRD #92 PR #105 followup: write a `ready` sentinel **after**
    // the parent shell's `trap '' TERM` runs, so the test can wait
    // for trap-installation before triggering the close. Without
    // this gate, close-pane could race shell startup — landing
    // before `trap` had run — and SIGTERM would kill the shell at
    // its (still-default) handler instead of being ignored. The
    // sibling test `close_pane_well_behaved_agent_runs_sigterm_trap`
    // uses the same ready-sentinel shape for the same reason.
    let sentinel_dir = tempfile::tempdir().unwrap();
    let ready_path = sentinel_dir.path().join("ready.flag");
    let cmd = format!(
        "trap '' TERM; echo ready > {} ; (trap '' TERM; sleep 60) & wait",
        ready_path.display(),
    );
    let cmd_for_pane = cmd.clone();
    let pane_id = {
        let ctrl = ctrl.clone();
        tokio::task::spawn_blocking(move || ctrl.create_pane(Some(&cmd_for_pane), None).unwrap())
            .await
            .unwrap()
    };

    // Capture the daemon-registered (shell) PID via the registry.
    let agent_ids = server.registry.agent_ids();
    assert_eq!(agent_ids.len(), 1);
    let shell_pid = server
        .registry
        .child_pid(&agent_ids[0])
        .expect("daemon-side child should expose a pid") as i32;

    // Wait for the `ready` sentinel — proves the parent shell's
    // `trap '' TERM` has actually been installed. Without this gate
    // the close below can land during shell startup (before the
    // trap runs), SIGTERM kills the shell at its default handler,
    // and the close returns in tens of milliseconds — failing the
    // ≥ 2.8 s lower bound for the wrong reason.
    let ready_appeared = wait_for(Duration::from_secs(10), Duration::from_millis(20), || {
        ready_path.exists()
            && std::fs::metadata(&ready_path)
                .map(|m| m.len() > 0)
                .unwrap_or(false)
    })
    .await;
    assert!(
        ready_appeared,
        "shell never wrote the 'ready' sentinel at {} — trap may not be installed; aborting before close to avoid a false-negative pass",
        ready_path.display()
    );

    // Sanity-check the shell is alive before the close.
    assert!(
        pid_is_alive(shell_pid),
        "shell pid {shell_pid} should be alive before close"
    );

    // Time the close so we can assert BOTH bounds. The lower bound
    // pins the grace was waited out; the upper bound pins the
    // SIGKILL backstop fires before the shell's 60-second sleep.
    let start = std::time::Instant::now();
    let ctrl_for_close = ctrl.clone();
    let pane_id_for_close = pane_id.clone();
    tokio::task::spawn_blocking(move || ctrl_for_close.close_pane(&pane_id_for_close).unwrap())
        .await
        .unwrap();
    let close_elapsed = start.elapsed();

    let shell_dead = wait_for(
        Duration::from_millis(500),
        Duration::from_millis(20),
        || !pid_is_alive(shell_pid),
    )
    .await;
    assert!(
        shell_dead,
        "shell pid {shell_pid} should be dead after close + 500 ms — SIGKILL fallback failed?"
    );

    // Lower bound: 2.8 s = 3 s F8 grace - 200 ms slack for scheduler
    // / spawn-blocking setup. Pre-F8 immediate-SIGKILL would close in
    // milliseconds and fail here.
    assert!(
        close_elapsed >= Duration::from_millis(2800),
        "F8 close path returned in {close_elapsed:?} for an uncooperative agent — too fast; the SIGTERM grace was not waited out (would pass pre-F8 with raw SIGKILL)"
    );
    // Upper bound: 4.5 s = 3 s grace + 1.5 s for SIGKILL delivery +
    // reap + spawn-blocking hop. (Tighter than the 3.5 s used to be
    // wouldn't survive the CTRL_W_STOP_TIMEOUT bookkeeping under
    // load.)
    assert!(
        close_elapsed < Duration::from_millis(4500),
        "F8 close path took {close_elapsed:?} for an uncooperative agent — exceeds the 4.5 s upper bound (grace was 3 s; SIGKILL should fire promptly after)"
    );
}

/// PRD #92 F1 followup (auditor #5): closing an agent must signal the
/// agent's process group, not the daemon's. This regression would have
/// shown up if a bad `pid_to_pgid` (e.g. accepting `0`) reached
/// `killpg(0, SIGKILL)`, which the kernel interprets as "signal every
/// process in the caller's process group" — the daemon itself plus
/// every attach client. Probes the daemon's own pgid before and after
/// the close to make sure `killpg` is hitting the right group.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(process_group_kill)]
async fn close_pane_does_not_signal_daemon_process_group() {
    // SAFETY: getpgrp(2) reads the caller's pgid and is async-signal-safe.
    let daemon_pgid = unsafe { libc::getpgrp() };
    assert!(
        daemon_pgid > 0,
        "test runner must have a valid process group, got {daemon_pgid}"
    );

    let server = start_server().await;
    let ctrl = Arc::new(EmbeddedPaneController::new(
        server.path.clone(),
        tokio::runtime::Handle::current(),
    ));

    let pane_id = {
        let ctrl = ctrl.clone();
        tokio::task::spawn_blocking(move || {
            ctrl.create_pane(Some("sh -c 'sleep 30'"), None).unwrap()
        })
        .await
        .unwrap()
    };

    // Trigger the close path — internally this goes through
    // force_kill_child_and_wait → killpg.
    let ctrl_for_close = ctrl.clone();
    let pane_id_for_close = pane_id.clone();
    tokio::task::spawn_blocking(move || ctrl_for_close.close_pane(&pane_id_for_close).unwrap())
        .await
        .unwrap();

    // The daemon's process group MUST still be alive. If `killpg` had
    // hit pgid=0 (the bug pid_to_pgid guards against), the daemon
    // itself would have died and `kill(daemon_pgid, 0)` would return
    // ESRCH. `kill(-pgid, 0)` is the canonical "is this process group
    // alive" probe; we pass `-daemon_pgid` to invoke the "every
    // process in the group" semantic.
    //
    // SAFETY: `kill(pid, 0)` does not signal — it only probes.
    let rc = unsafe { libc::kill(-daemon_pgid, 0) };
    assert!(
        rc == 0,
        "daemon's own process group {daemon_pgid} must still be alive after closing an agent — killpg targeted the wrong group? (kill returned rc={rc}, errno={:?})",
        std::io::Error::last_os_error().raw_os_error()
    );
}
