//! PRD #385: a panic the deck contains leaves the terminal as the TUI set it
//! up, under the whole panic-hook stack `run_tui` installs.
//!
//! Each case runs in a child process: the outer test re-executes this test
//! binary filtered to itself with [`CASE`] set, on a fresh pseudo-terminal in
//! a session of its own, so the child's raw mode and alternate screen are that
//! pseudo-terminal's and never the terminal running the suite. The child sets
//! the terminal up with [`enter_tui_terminal`], the function `run_tui` calls,
//! causes one panic, then reports whether its terminal is still in raw mode
//! and prints [`PROBE`]. The outer test reads everything the child wrote to
//! the terminal, and checks the probe and whether the alternate screen was
//! left before it.
//!
//! No clipboard is opened: the clipboard's opener is a fake that panics.

use std::io::Read as _;
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::time::{Duration, Instant};

use spec::spec;

use super::enter_tui_terminal;
use crate::clipboard::{NativeClipboard, NativeWrite};

const CASE: &str = "DOT_AGENT_DECK_TEST_PANIC_TEARDOWN_CASE";
const PROBE: &str = "panic-teardown-probe raw=";
const ENTER_ALTERNATE_SCREEN: &str = "\x1b[?1049h";
const LEAVE_ALTERNATE_SCREEN: &str = "\x1b[?1049l";
const TEST: &str = "a_contained_panic_leaves_the_terminal_set_up_and_any_other_hands_it_back";

/// A pseudo-terminal pair, 80 columns by 24 rows.
fn open_pty() -> (std::fs::File, std::fs::File) {
    let mut master = -1;
    let mut slave = -1;
    let mut size = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `openpty` initialises both descriptors on success, and each is
    // turned into an owning `File` exactly once below.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            // `*const` on Linux and `*mut` on macOS.
            std::ptr::addr_of_mut!(size) as _,
        )
    };
    assert_eq!(
        rc,
        0,
        "open pseudo-terminal: {}",
        std::io::Error::last_os_error()
    );
    // `openpty` opens both without close-on-exec, so a process another test
    // thread spawns would inherit them, and an inherited slave keeps the
    // master readable past the child's exit. Mark both close-on-exec at once;
    // the child still gets the slave as its stdio, which `Command` installs
    // with `dup2` (that clears the flag on the copy). A spawn in the few
    // instructions between `openpty` and these calls can still inherit them,
    // which is why `run_child` also bounds the drain by its deadline.
    for fd in [master, slave] {
        // SAFETY: `fd` was opened above and is still open.
        let rc = unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        assert_ne!(
            rc,
            -1,
            "set close-on-exec on the pseudo-terminal: {}",
            std::io::Error::last_os_error()
        );
    }
    // SAFETY: both descriptors are freshly opened and owned by this test.
    unsafe {
        (
            std::fs::File::from_raw_fd(master),
            std::fs::File::from_raw_fd(slave),
        )
    }
}

/// Run `read` on a thread of its own, and hand back a receiver for its result,
/// so the caller can wait for it with a deadline rather than without one.
fn read_on_thread<T: Send + 'static>(
    read: impl FnOnce() -> T + Send + 'static,
) -> std::sync::mpsc::Receiver<T> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(read());
    });
    receiver
}

/// What a child run left behind.
struct Run {
    /// Everything the child wrote to its terminal.
    terminal: String,
    stderr: String,
}

impl Run {
    /// The terminal output up to the probe line, and the raw-mode state the
    /// probe reported.
    fn before_probe(&self) -> (&str, bool) {
        let at = self
            .terminal
            .find(PROBE)
            .unwrap_or_else(|| panic!("the child printed no probe:\n{self}"));
        let raw = self.terminal[at + PROBE.len()..].starts_with("true");
        (&self.terminal[..at], raw)
    }
}

impl std::fmt::Display for Run {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "terminal: {:?}\nstderr:\n{}", self.terminal, self.stderr)
    }
}

/// Run [`TEST`] in a child on a fresh pseudo-terminal with `case`.
fn run_child(case: &str) -> Run {
    let (mut master, slave) = open_pty();
    let mut command =
        std::process::Command::new(std::env::current_exe().expect("the test binary has a path"));
    command
        .args([
            "--exact",
            &format!("ui::panic_teardown_tests::{TEST}"),
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CASE, case)
        .stdin(slave.try_clone().expect("the slave for stdin"))
        .stdout(slave.try_clone().expect("the slave for stdout"))
        .stderr(std::process::Stdio::piped());
    // SAFETY: `setsid` is async-signal-safe. A session of its own leaves the
    // child no controlling terminal, so crossterm's `/dev/tty` lookups fall
    // back to its standard streams, which are the pseudo-terminal.
    unsafe {
        use std::os::unix::process::CommandExt as _;
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    // One deadline for the child, its terminal and its stderr together.
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut child = command.spawn().expect("start the child");
    // The parent's copies of the slave: the command's two and this one.
    drop(command);
    drop(slave);

    // Read the terminal until the child's side closes, which on Linux reads
    // as an error once the buffered output is drained.
    let reader = read_on_thread(move || {
        let mut out = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match master.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => out.extend_from_slice(&chunk[..n]),
            }
        }
        out
    });
    let mut stderr = child.stderr.take().expect("the child's stderr");
    let stderr = read_on_thread(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });

    let status = loop {
        if let Some(status) = child.try_wait().expect("poll the child") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("case {case}: the child did not finish within 60s");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    // Something other than the child can still hold its terminal or stderr
    // open; past the deadline that fails the test instead of hanging it.
    let left = || deadline.saturating_duration_since(Instant::now());
    let terminal = reader.recv_timeout(left()).unwrap_or_else(|error| {
        panic!("case {case}: the child's terminal was not drained within 60s: {error}")
    });
    let stderr = stderr.recv_timeout(left()).unwrap_or_else(|error| {
        panic!("case {case}: the child's stderr was not drained within 60s: {error}")
    });
    let run = Run {
        terminal: String::from_utf8_lossy(&terminal).into_owned(),
        stderr,
    };
    assert!(
        status.success(),
        "case {case}: the child failed ({status:?}):\n{run}"
    );
    assert!(
        run.terminal.contains("1 passed"),
        "case {case}: the child did not run {TEST}:\n{run}"
    );
    run
}

/// Whether the terminal on stdin is in raw mode (no canonical input, no echo).
fn stdin_is_raw() -> bool {
    // SAFETY: `termios` is plain data that `tcgetattr` fills in.
    let mut termios: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: stdin is open for the whole call and `termios` is writable.
    let rc = unsafe { libc::tcgetattr(std::io::stdin().as_raw_fd(), &mut termios) };
    assert_eq!(rc, 0, "tcgetattr: {}", std::io::Error::last_os_error());
    termios.c_lflag & (libc::ICANON | libc::ECHO) == 0
}

/// The child's half: set the terminal up as `run_tui` does, cause the
/// `case`'s panic, and print the probe.
fn child(case: &str) {
    let (_modes, _terminal) = enter_tui_terminal().expect("set the terminal up");
    assert!(stdin_is_raw(), "raw mode is on before the panic");
    match case {
        "clipboard" => {
            let mut native = NativeClipboard::with_opener(
                Box::new(|| panic!("deliberate panic in a fake clipboard open")),
                Duration::from_secs(5),
            );
            assert_eq!(native.write("text"), NativeWrite::Unavailable);
        }
        "parser" => {
            let fed = crate::embedded_pane::guarded_parser_feed(|| {
                panic!("deliberate panic in a guarded parser feed")
            });
            assert!(fed.is_err(), "the guarded feed caught its panic");
        }
        "ordinary" => {
            let joined = std::thread::Builder::new()
                .name("not-contained".into())
                .spawn(|| panic!("deliberate panic on an ordinary thread"))
                .expect("spawn")
                .join();
            assert!(joined.is_err());
        }
        other => panic!("unknown case {other}"),
    }
    use std::io::Write as _;
    let mut out = std::io::stdout();
    write!(out, "{PROBE}{}\r\n", stdin_is_raw()).expect("print the probe");
    out.flush().expect("flush the probe");
}

/// Scenario: The deck sets its terminal up as it does at start, with its whole panic-hook stack, in a pseudo-terminal, and then the native clipboard's worker panics (a fake opener) or a guarded parser feed panics. The terminal stays in raw mode on the alternate screen for the TUI that keeps running, while a panic on an ordinary thread, the control, hands the terminal back.
#[spec("clipboard/native/004")]
#[test]
fn a_contained_panic_leaves_the_terminal_set_up_and_any_other_hands_it_back() {
    if let Ok(case) = std::env::var(CASE) {
        return child(&case);
    }
    for case in ["clipboard", "parser"] {
        let run = run_child(case);
        let (before, raw) = run.before_probe();
        assert!(
            before.contains(ENTER_ALTERNATE_SCREEN),
            "case {case}: the terminal was set up:\n{run}"
        );
        assert!(
            !before.contains(LEAVE_ALTERNATE_SCREEN),
            "case {case}: a contained panic left the alternate screen:\n{run}"
        );
        assert!(
            raw,
            "case {case}: a contained panic turned raw mode off:\n{run}"
        );
    }
    let run = run_child("ordinary");
    let (before, raw) = run.before_probe();
    assert!(
        before.contains(LEAVE_ALTERNATE_SCREEN),
        "control: an ordinary panic left the alternate screen:\n{run}"
    );
    assert!(
        !raw,
        "control: an ordinary panic turned raw mode off:\n{run}"
    );
}
