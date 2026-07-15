#![cfg(unix)]

//! Fast subprocess coverage for non-interactive wrapper stream fidelity.

use std::io::Write as _;
use std::process::{Command, Output, Stdio};

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

/// Scenario: Run `dot-agent-deck wrap` with redirected non-interactive streams.
/// Stdout and stderr must remain separate, a stdout-only pipe must not receive
/// stderr, and binary stdin including terminal EOF bytes must reach an
/// EOF-sensitive child byte-for-byte.
#[test]
fn wrap_io_001_noninteractive_streams_are_transparent() {
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
}
