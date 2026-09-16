//! Fast-tier coverage of the **CLI half** of the hook-socket provenance gate
//! (issue #1077): that the real `dot-agent-deck` binary forwards the per-spawn
//! capability token out of its own environment onto the wire, and omits the key
//! entirely when it has none.
//!
//! This is the one seam neither the unit tests nor the daemon's hook-loop tests
//! can see. `crate::hook_provenance` decides the policy, `crate::daemon`'s
//! `hook_provenance_*` tests drive that decision over a real socket, and both
//! build the message themselves — so a CLI that never read
//! `DOT_AGENT_DECK_HOOK_TOKEN` would leave every one of them green while every
//! real signal was refused. Here the daemon is a **stub** that captures the raw
//! line, which is what makes the assertion about the bytes the CLI emits rather
//! than about anything the daemon decides.
//!
//! The omission half is the cross-version half, and it is as load-bearing as the
//! presence half: a CLI with no token must send the payload an older daemon has
//! always received, byte for byte, or every mixed-version pair breaks on the
//! upgrade rather than on the downgrade.

#![cfg(unix)]

mod common;

use std::io::BufRead;
use std::os::unix::net::UnixListener;

/// Run the real CLI against a stub listener that captures one line, and return
/// what it captured.
///
/// The stub accepts, reads a line and closes without replying — which for
/// `work-done` is the ordinary case (the verb reads nothing back) and for
/// `delegate` is `SocketReply::NoReply`, a documented success.
fn capture_one_line(args: &[&str], token: Option<&str>) -> String {
    let dir = common::harness_tempdir().expect("create temp dir for the stub hook socket");
    let socket_path = dir.path().join("hook.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind stub hook socket");

    let captured = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut reader = std::io::BufReader::new(stream);
        let mut line = String::new();
        let _ = reader.read_line(&mut line);
        line
    });

    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"));
    cmd.args(args)
        .env("DOT_AGENT_DECK_SOCKET", &socket_path)
        .env("DOT_AGENT_DECK_PANE_ID", "cli-pane");
    match token {
        Some(t) => {
            cmd.env("DOT_AGENT_DECK_HOOK_TOKEN", t);
        }
        // Explicitly removed rather than merely unset: this test binary can be
        // run from inside a real deck pane, which would otherwise hand the
        // child a genuine token and make the omission case vacuous.
        None => {
            cmd.env_remove("DOT_AGENT_DECK_HOOK_TOKEN");
        }
    }
    let output = cmd.output().expect("run the real CLI");
    let line = captured.join().expect("stub thread");
    assert!(
        output.status.success(),
        "{args:?} exited {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    line
}

/// A token of the shape the daemon mints — 64 lowercase hex characters.
const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

#[test]
fn work_done_forwards_the_hook_token_from_its_environment() {
    let line = capture_one_line(&["work-done", "--task", "done"], Some(TOKEN));
    assert!(
        line.contains(&format!("\"token\":\"{TOKEN}\"")),
        "the work-done CLI did not forward DOT_AGENT_DECK_HOOK_TOKEN: {line}"
    );
}

#[test]
fn delegate_forwards_the_hook_token_from_its_environment() {
    let line = capture_one_line(
        &["delegate", "--to", "worker", "--task", "do it"],
        Some(TOKEN),
    );
    assert!(
        line.contains(&format!("\"token\":\"{TOKEN}\"")),
        "the delegate CLI did not forward DOT_AGENT_DECK_HOOK_TOKEN: {line}"
    );
}

/// The cross-version half. A CLI with no token must not put the key on the wire
/// at all, so a daemon that predates the field receives exactly the JSON it
/// always received.
#[test]
fn a_cli_with_no_token_omits_the_key_entirely() {
    for args in [
        vec!["work-done", "--task", "done"],
        vec!["delegate", "--to", "worker", "--task", "do it"],
    ] {
        let line = capture_one_line(&args, None);
        assert!(
            !line.contains("token"),
            "{args:?} put a token key on the wire with nothing to put in it, which is not the \
             payload a pre-#1077 daemon has always received: {line}"
        );
    }
}

/// An empty or whitespace-only value in the environment is "no token", not a
/// token. Presenting one would be refused as malformed and would read in the
/// daemon's log as a forgery rather than as an environment that never carried
/// one.
#[test]
fn a_blank_hook_token_is_treated_as_absent() {
    let line = capture_one_line(&["work-done", "--task", "done"], Some("   "));
    assert!(
        !line.contains("token"),
        "a blank DOT_AGENT_DECK_HOOK_TOKEN must be omitted, not presented: {line}"
    );
}
