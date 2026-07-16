#![cfg(unix)]

//! Fast subprocess coverage for wrapper stream fidelity and signal cleanup.

mod common;

use std::fs::File;
use std::io::{Read as _, Write as _};
use std::os::fd::FromRawFd as _;
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use spec::spec;

fn run_wrap(script: &str, stdin: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args(["wrap", "--agent", "codex", "--", "/bin/sh", "-c", script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn non-interactive wrapper");
    child
        .stdin
        .take()
        .expect("wrapper stdin")
        .write_all(stdin)
        .expect("write wrapper stdin");
    child.wait_with_output().expect("wait for wrapper")
}

fn open_pty() -> (File, File) {
    let mut master = -1;
    let mut slave = -1;
    // SAFETY: `openpty` initializes both owned descriptors on success. The
    // resulting `File`s take ownership exactly once.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(
        rc,
        0,
        "open outer pseudo-terminal: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: successful `openpty` returned two fresh, valid descriptors.
    unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) }
}

fn read_pty(mut master: File) -> Vec<u8> {
    let mut observed = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        match master.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => observed.extend_from_slice(&buffer[..count]),
            // Linux PTY masters report EIO after the final slave closes.
            Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
            Err(error) => panic!("read outer pseudo-terminal: {error}"),
        }
    }
    observed
}

fn run_with_stderr_redirected() -> (bool, Vec<u8>, Vec<u8>) {
    let fixture = tempfile::tempdir().expect("create stderr-only redirect fixture");
    let stderr_path = fixture.path().join("stderr.log");
    let stderr_file = File::create(&stderr_path).expect("create redirected stderr");
    let (master, slave) = open_pty();
    let status = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args([
            "wrap",
            "--agent",
            "codex",
            "--",
            "/bin/sh",
            "-c",
            "printf 'mixed-stderr-marker\\n' >&2",
        ])
        .stdin(Stdio::from(
            slave.try_clone().expect("clone PTY slave for stdin"),
        ))
        .stdout(Stdio::from(slave))
        .stderr(Stdio::from(stderr_file))
        .status()
        .expect("run wrapper with stderr-only redirect");
    let terminal_output = read_pty(master);
    let redirected_stderr = std::fs::read(stderr_path).expect("read redirected stderr");
    (status.success(), terminal_output, redirected_stderr)
}

fn run_with_stdout_redirected() -> (bool, Vec<u8>) {
    let fixture = tempfile::tempdir().expect("create stdout-only redirect fixture");
    let stdout_path = fixture.path().join("stdout.log");
    let stdout_file = File::create(&stdout_path).expect("create redirected stdout");
    let (master, slave) = open_pty();
    let status = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args([
            "wrap",
            "--agent",
            "codex",
            "--",
            "/bin/sh",
            "-c",
            "if [ -t 0 ]; then input=tty; else input=pipe; fi; \
             if [ -t 2 ]; then error=tty; else error=pipe; fi; \
             printf 'stdin=%s stderr=%s\\n' \"$input\" \"$error\"",
        ])
        .stdin(Stdio::from(
            slave.try_clone().expect("clone PTY slave for stdin"),
        ))
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(slave))
        .status()
        .expect("run wrapper with stdout-only redirect");
    drop(master);
    let redirected_stdout = std::fs::read(stdout_path).expect("read redirected stdout");
    (status.success(), redirected_stdout)
}

fn bytes_contain(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Scenario: Run `dot-agent-deck wrap` with redirected non-interactive streams.
/// Wholly non-interactive streams remain separate and byte-exact, while stderr-only
/// and stdout-only redirects preserve each unaffected descriptor's TTY identity.
#[spec("codex/wrap/003")]
#[test]
fn codex_wrap_003_each_descriptor_preserves_its_original_semantics() {
    let separate = run_wrap("printf 'out\\n'; printf 'err\\n' >&2", b"");

    let pipe_dir = tempfile::tempdir().expect("create stdout-only pipe fixture");
    let pipe_stderr_path = pipe_dir.path().join("stderr.log");
    let pipe_stderr = std::fs::File::create(&pipe_stderr_path).expect("create stderr capture");
    let pipe = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args([
            "wrap",
            "--agent",
            "codex",
            "--",
            "/bin/sh",
            "-c",
            "printf 'pipe-out\\n'; printf 'pipe-err\\n' >&2",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(pipe_stderr))
        .output()
        .expect("run wrapper with stdout-only pipe");
    let pipe_stderr = std::fs::read(&pipe_stderr_path).expect("read separate pipe stderr");

    let binary_dir = tempfile::tempdir().expect("create binary stdin fixture");
    let binary_record = binary_dir.path().join("stdin.bin");
    let binary_payload = b"\x04\x00A\nB";
    let mut binary_child = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args([
            "wrap",
            "--agent",
            "codex",
            "--",
            "/bin/sh",
            "-c",
            "cat > \"$WRAP_STDIN_RECORD\"",
        ])
        .env("WRAP_STDIN_RECORD", &binary_record)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn wrapper binary stdin probe");
    binary_child
        .stdin
        .take()
        .expect("binary wrapper stdin")
        .write_all(binary_payload)
        .expect("write binary wrapper stdin");
    let binary = binary_child
        .wait_with_output()
        .expect("wait for binary stdin probe");
    let binary_observed = std::fs::read(&binary_record).unwrap_or_default();

    let (stderr_status, stderr_terminal, redirected_stderr) = run_with_stderr_redirected();
    let (stdout_status, redirected_stdout) = run_with_stdout_redirected();

    assert_eq!(
        (
            separate.status.success(),
            separate.stdout,
            separate.stderr,
            pipe.status.success(),
            pipe.stdout,
            pipe_stderr,
            binary.status.success(),
            binary_observed,
        ),
        (
            true,
            b"out\n".to_vec(),
            b"err\n".to_vec(),
            true,
            b"pipe-out\n".to_vec(),
            b"pipe-err\n".to_vec(),
            true,
            binary_payload.to_vec(),
        ),
        "non-interactive wrapping must preserve independent stdout/stderr pipes and byte-exact stdin through EOF"
    );
    assert_eq!(
        (
            stderr_status,
            redirected_stderr,
            bytes_contain(&stderr_terminal, b"mixed-stderr-marker"),
            stdout_status,
            redirected_stdout,
        ),
        (
            true,
            b"mixed-stderr-marker\n".to_vec(),
            false,
            true,
            b"stdin=tty stderr=tty\n".to_vec(),
        ),
        "wrapping must preserve every descriptor independently: non-interactive streams stay separate and byte-exact, stderr-only redirection reaches only stderr, and stdout-only redirection leaves stdin/stderr attached to their TTY"
    );
}

#[derive(Debug)]
struct SignalOutcome {
    path: &'static str,
    signal: &'static str,
    wrapper_exited: bool,
    child_gone: bool,
}

fn terminate(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn run_signal_case(
    signal: libc::c_int,
    signal_name: &'static str,
    interactive: bool,
) -> SignalOutcome {
    let fixture = tempfile::tempdir().expect("create signal fixture");
    let pid_path = fixture.path().join("child.pid");
    let mut command = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"));
    command
        .args([
            "wrap",
            "--agent",
            "codex",
            "--",
            "/bin/sh",
            "-c",
            "printf '%s\\n' \"$$\" > \"$WRAP_CHILD_PID_FILE\"; exec /bin/sleep 60",
        ])
        .env("WRAP_CHILD_PID_FILE", &pid_path);

    let _master = if interactive {
        let (master, slave) = open_pty();
        command
            .stdin(Stdio::from(
                slave.try_clone().expect("clone PTY slave for stdin"),
            ))
            .stdout(Stdio::from(
                slave.try_clone().expect("clone PTY slave for stdout"),
            ))
            .stderr(Stdio::from(slave));
        Some(master)
    } else {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        None
    };

    let mut wrapper = command.spawn().expect("spawn wrapper signal probe");
    let wrapper_pid = wrapper.id() as libc::pid_t;
    let read_child_pid = || -> Option<libc::pid_t> {
        std::fs::read_to_string(&pid_path)
            .ok()
            .and_then(|contents| contents.trim().parse().ok())
    };
    if !common::wait_until(Duration::from_secs(5), || read_child_pid().is_some()) {
        terminate(&mut wrapper);
        panic!(
            "{} wrapper never recorded its child pid",
            if interactive { "PTY" } else { "pipe" }
        );
    }
    let child_pid = read_child_pid().expect("child pid recorded");
    assert!(
        common::process_running(child_pid),
        "precondition: wrapped child pid {child_pid} must be running before {signal_name}"
    );

    // SAFETY: the wrapper pid came from this test's live `Child`; signaling it
    // is the behavior under test.
    let signal_result = unsafe { libc::kill(wrapper_pid, signal) };
    assert_eq!(
        signal_result, 0,
        "deliver {signal_name} to wrapper pid {wrapper_pid}"
    );

    let wrapper_exited = common::wait_until(Duration::from_secs(5), || {
        !common::process_running(wrapper_pid)
    });
    let child_gone = common::wait_until(Duration::from_secs(3), || {
        !common::process_running(child_pid)
    });

    if common::process_running(child_pid) {
        // SAFETY: best-effort cleanup of this test's recorded child.
        unsafe {
            libc::kill(child_pid, libc::SIGKILL);
        }
    }
    if common::process_running(wrapper_pid) {
        terminate(&mut wrapper);
    } else {
        let _ = wrapper.wait();
    }

    SignalOutcome {
        path: if interactive { "pty" } else { "pipe" },
        signal: signal_name,
        wrapper_exited,
        child_gone,
    }
}

/// Scenario: Start a lingering wrapped child once through an interactive PTY and
/// once through non-interactive pipes, then deliver SIGTERM and SIGHUP to each
/// wrapper. Every wrapper must forward the signal, reap its child, and exit.
#[spec("codex/wrap/004")]
#[test]
fn codex_wrap_004_termination_signals_reap_children_on_every_path() {
    let outcomes = [
        run_signal_case(libc::SIGTERM, "SIGTERM", true),
        run_signal_case(libc::SIGTERM, "SIGTERM", false),
        run_signal_case(libc::SIGHUP, "SIGHUP", true),
        run_signal_case(libc::SIGHUP, "SIGHUP", false),
    ];
    let all_cases_present = ["SIGTERM", "SIGHUP"].into_iter().all(|signal| {
        ["pty", "pipe"].into_iter().all(|path| {
            outcomes
                .iter()
                .any(|outcome| outcome.signal == signal && outcome.path == path)
        })
    });
    assert!(
        all_cases_present
            && outcomes
                .iter()
                .all(|outcome| outcome.wrapper_exited && outcome.child_gone),
        "wrapper must forward SIGTERM and SIGHUP and reap its child on both paths; outcomes: {outcomes:#?}"
    );
}
