#![cfg(all(unix, feature = "e2e"))]

//! L2 coverage for issue #1138 — the TUI must end when its terminal hangs up.
//!
//! The defect: with the deck running as a CHILD of the session leader — `ssh -t
//! host task run`, or a tmux pane whose command is `task`, where Task 3.53
//! catches SIGHUP and forwards nothing — closing the terminal left the deck
//! running at 100% of a core indefinitely. The kernel's SIGHUP goes to the
//! session leader alone, so the deck was never signalled, and it could not
//! notice for itself: crossterm's Unix event source has no `Ok(0)` break, so
//! `event::poll` does not return `false` on a hung-up terminal — it does not
//! return at all (`src/terminal_hangup.rs` has the upstream code and the
//! `strace` counts).
//!
//! That is why this is L2 and not only L1. `terminal_hangup`'s unit tests prove
//! the probe and the watchdog against a real pseudo-terminal in milliseconds,
//! but they cannot prove the thing the issue is actually about: that the
//! shipped binary, with its event loop wedged inside a library, still goes away.
//! Only driving the real deck on a real terminal and taking that terminal away
//! can fail if someone deletes the watchdog from `run_tui`.
//!
//! Lane 1 — no credential and no real agent: the deck is launched with no panes
//! at all, because the hangup is a property of the event loop rather than of
//! anything running under it.
//!
//! **Linux only in practice.** Nothing `cfg`s this off for macOS, but no macOS
//! job enables the `e2e` feature — `e2e-deterministic` is a Linux job — so the
//! real-binary path is exercised there only by a developer running it. The L1
//! tests in `crate::terminal_hangup` are what `build-macos` runs, and they are
//! what caught Apple's `poll` reporting no hangup for an unrequested event.

mod common;

use std::io::Read as _;
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::time::Duration;

use spec::spec;

/// A pseudo-terminal pair, as `tests/wrap_io.rs::open_pty` builds one.
fn open_pty() -> (std::fs::File, std::fs::File) {
    let mut master = -1;
    let mut slave = -1;
    // SAFETY: `openpty` initialises both descriptors on success, and each is
    // turned into an owning `File` exactly once below.
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
    // `openpty` marks neither descriptor close-on-exec (issue #668's test-side
    // note): without this the deck would inherit a spare master of its own
    // terminal and nothing could ever hang it up.
    for fd in [master, slave] {
        // SAFETY: both descriptors are open and owned by this test.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }
    // SAFETY: both descriptors are freshly opened and owned by this test.
    unsafe {
        (
            std::fs::File::from_raw_fd(master),
            std::fs::File::from_raw_fd(slave),
        )
    }
}

/// Drain whatever the deck has painted so far, without blocking.
///
/// The master must be drained while waiting for the deck to come up: a full
/// pseudo-terminal buffer would block the deck in a `write` instead of leaving
/// it in the event loop, which is a different state than the one under test.
fn drain(master: &mut std::fs::File, sink: &mut Vec<u8>) {
    let mut buf = [0u8; 8192];
    loop {
        match master.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => sink.extend_from_slice(&buf[..n]),
            Err(_) => return, // WouldBlock, or the peer is gone
        }
    }
}

/// Everything this test started, reaped on every exit path including a panic.
///
/// Greptile P2 on PR #1164: the first version cleaned up inline, after the
/// waits, so an earlier assertion failure left the deck and its session leader
/// running — and it never reaped the daemon at all. The deck **lazy-spawns its
/// daemon detached** (its own session, parent PID 1 from birth), so nothing
/// above it can signal it, and with idle shutdown disabled it would otherwise
/// sit out the 300s `DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS` backstop — five
/// minutes of leaked daemon per run. An RAII drop is what makes all three
/// reliable, because a failed assertion unwinds through it.
struct Sandbox {
    bin: &'static str,
    leader: std::process::Child,
    deck_pid: Option<i32>,
    daemon_env: Vec<(&'static str, std::path::PathBuf)>,
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if let Some(pid) = self.deck_pid
            && common::process_running(pid)
        {
            // SAFETY: best-effort cleanup of a pid this test created. By PID,
            // never by a `pkill` pattern — a pattern that also matches a
            // production deck is how nine live panes were stopped on
            // 2026-09-15 (CLAUDE.md rule 12, issue #428 occurrence #5).
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        let _ = self.leader.kill();
        let _ = self.leader.wait();

        // Then the detached daemon, through the product's own verb rather than
        // a signal. It is scoped to THIS sandbox by the socket paths below, so
        // it cannot reach the developer's own daemon; and it needs no `--force`
        // because this test spawns no agents for the refusal (issue #770) to
        // protect.
        let mut stop = std::process::Command::new(self.bin);
        stop.arg("daemon").arg("stop").env_clear();
        for (key, value) in &self.daemon_env {
            stop.env(key, value);
        }
        let _ = stop
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// Scenario: Launch the real deck on a pseudo-terminal as the CHILD of a `trap
/// "" HUP` session leader — the `ssh -t host task run` shape, where SIGHUP
/// reaches the leader and is forwarded to nothing — wait for it to paint its
/// dashboard, confirm it is still running while the terminal is attached, then
/// close the pseudo-terminal master so the terminal hangs up. Assert the deck
/// process is gone within seconds instead of spinning on a terminal it can
/// neither read from nor write to.
#[spec("error/hangup/001")]
#[test]
fn hangup_001_deck_exits_when_its_terminal_hangs_up() {
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let dir = common::race_safe_tempdir();
    let work = dir.path();
    let home = work.join("home");
    std::fs::create_dir_all(&home).expect("create HOME");
    let pidfile = work.join("deck.pid");

    let (master, slave) = open_pty();
    // Non-blocking, so `drain` can be called from the wait loop below without
    // ever parking this test on a deck that has stopped painting.
    // SAFETY: the master is open and owned by this test.
    unsafe {
        let flags = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    // The session leader stands in for `task`: it ignores SIGHUP and forwards
    // nothing, so the deck below it is signalled by nobody and has to notice
    // the hangup for itself. `</dev/tty` is load-bearing — POSIX assigns an
    // asynchronous list's stdin to /dev/null before explicit redirections, so
    // without it the deck would read EOF from /dev/null at startup rather than
    // from the terminal this test takes away.
    let script = "trap '' HUP\n\"$0\" </dev/tty &\necho $! > \"$1\"\nwait\n";

    let path_env = std::env::var("PATH").unwrap_or_default();
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c")
        .arg(script)
        .arg(bin)
        .arg(&pidfile)
        .env_clear()
        .env("PATH", &path_env)
        .env("TERM", "xterm-256color")
        .env("LC_ALL", "C.UTF-8")
        .env("SHELL", "/bin/sh")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("DOT_AGENT_DECK_SOCKET", work.join("hook.sock"))
        .env("DOT_AGENT_DECK_ATTACH_SOCKET", work.join("attach.sock"))
        .env("DOT_AGENT_DECK_STATE_DIR", work.join("state"))
        .env("DOT_AGENT_DECK_LOG", work.join("deck.log"))
        // Pin the feature flags rather than inheriting whatever the operator's
        // real `~/.dot-agent-deck.toml` sets: project-config discovery walks up
        // from cwd, not from HOME.
        .env("DOT_AGENT_DECK_EXPERIMENTAL", "0")
        // The deck must not be ended by an idle daemon or a lifetime cap while
        // this test is waiting — only by the hangup.
        .env("DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS", "0")
        .env("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "300")
        .stdin(std::process::Stdio::from(
            slave.try_clone().expect("clone slave for stdin"),
        ))
        .stdout(std::process::Stdio::from(
            slave.try_clone().expect("clone slave for stdout"),
        ))
        .stderr(std::process::Stdio::from(
            slave.try_clone().expect("clone slave for stderr"),
        ));
    // SAFETY: `setsid` and `ioctl` are async-signal-safe and touch no memory
    // this closure shares with the parent. Rust dup2s the stdio descriptors
    // before running `pre_exec`, so fd 0 is already the pseudo-terminal slave.
    unsafe {
        use std::os::unix::process::CommandExt as _;
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Claim the terminal, so the kernel has a session leader to deliver
            // SIGHUP to — the one the `trap` above then swallows. Without this
            // the hangup would reach nobody at all, which is a weaker scenario
            // than the reported one.
            if libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    // Adopted by the guard immediately, so every path below — including a
    // failed assertion — reaps the leader, the deck and the detached daemon.
    let mut sandbox = Sandbox {
        bin,
        leader: cmd.spawn().expect("spawn the session leader"),
        deck_pid: None,
        daemon_env: vec![
            ("HOME", home.clone()),
            ("DOT_AGENT_DECK_SOCKET", work.join("hook.sock")),
            ("DOT_AGENT_DECK_ATTACH_SOCKET", work.join("attach.sock")),
            ("DOT_AGENT_DECK_STATE_DIR", work.join("state")),
            ("DOT_AGENT_DECK_LOG", work.join("deck.log")),
        ],
    };
    // The child holds its own copies; this test must not keep the terminal
    // alive from its side.
    drop(slave);

    let read_pid = || -> Option<i32> {
        std::fs::read_to_string(&pidfile)
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
    };

    // Ready when the daemon is listening AND the deck has painted: both, so the
    // hangup lands on a deck that is in its event loop rather than still
    // starting up. Decision 21: the bounded polling is `common::wait_until`, so
    // the terminal drain has to ride inside its `Fn` predicate — hence the
    // cells, which is the whole reason they exist.
    let attach_socket = work.join("attach.sock");
    let master = std::cell::RefCell::new(master);
    let painted = std::cell::RefCell::new(Vec::<u8>::new());
    let pump = || drain(&mut master.borrow_mut(), &mut painted.borrow_mut());
    let came_up = common::wait_until(
        common::child_boot_budget() + Duration::from_secs(20),
        || {
            pump();
            read_pid().is_some() && attach_socket.exists() && painted.borrow().len() > 1024
        },
    );
    let deck_pid = read_pid().expect("the session leader never recorded the deck's pid");
    sandbox.deck_pid = Some(deck_pid);
    assert!(
        came_up,
        "the deck never came up: attach socket {}, {} bytes painted",
        attach_socket.exists(),
        painted.borrow().len()
    );

    // Control: while the terminal is still attached the deck must stay up. A
    // watchdog that fired on an ordinary idle terminal — or on one with input
    // pending — would satisfy the assertion below without fixing anything.
    // Expressed as a wait that must TIME OUT, so the deck is watched for the
    // whole window rather than sampled once at the end of a sleep.
    let exited_while_attached = common::wait_until(Duration::from_millis(1500), || {
        pump();
        !common::process_running(deck_pid)
    });
    assert!(
        !exited_while_attached,
        "the deck exited while its terminal was still attached, so the assertion \
         below would prove nothing about hangups"
    );

    // Hang the terminal up. This is what closing an ssh session or a terminal
    // window does to the process on the other end.
    drop(master.into_inner());

    let gone = common::wait_until(Duration::from_secs(20), || {
        !common::process_running(deck_pid)
    });

    // No inline cleanup: `Sandbox`'s `Drop` reaps the deck, the leader and the
    // detached daemon whether this assertion passes or unwinds.
    assert!(
        gone,
        "the deck (pid {deck_pid}) was still running after its terminal hung up \
         (issue #1138). Its session leader ignores SIGHUP and forwards nothing, \
         so no signal reaches the deck — and crossterm's event source never \
         returns from a poll on a hung-up terminal, so nothing inside the event \
         loop can notice either. The deck spins at ~100% of one core until it \
         is killed by hand: measured over 10s before the fix, never below 99%."
    );
}
