//! Fast-tier coverage of the **CLI half** of the hook-socket provenance gate
//! (issue #1077): that the real `dot-agent-deck` binary forwards the per-spawn
//! capability token out of its own environment onto the wire, and omits the key
//! entirely when it has none.
//!
//! This is the one seam neither the unit tests nor the daemon's hook-loop tests
//! can see. `crate::hook_provenance` decides the policy, `crate::daemon`'s
//! `hook_provenance_*` tests drive that decision over a real socket, and both
//! build the message themselves — so a CLI that never read
//! `DOT_AGENT_DECK_PANE_CAPABILITY` would leave every one of them green while every
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

use std::io::{BufRead, Write};
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
            cmd.env("DOT_AGENT_DECK_PANE_CAPABILITY", t);
        }
        // Explicitly removed rather than merely unset: this test binary can be
        // run from inside a real deck pane, which would otherwise hand the
        // child a genuine token and make the omission case vacuous.
        None => {
            cmd.env_remove("DOT_AGENT_DECK_PANE_CAPABILITY");
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
        "the work-done CLI did not forward DOT_AGENT_DECK_PANE_CAPABILITY: {line}"
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
        "the delegate CLI did not forward DOT_AGENT_DECK_PANE_CAPABILITY: {line}"
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
        "a blank DOT_AGENT_DECK_PANE_CAPABILITY must be omitted, not presented: {line}"
    );
}

// ---------------------------------------------------------------------------
// Issue #1129: the acknowledgement half.
//
// `work-done` and `dispatch` used to write their line and exit 0 whatever the
// daemon then did with it, so a message the provenance gate refused was
// indistinguishable from one it acted on. The daemon now answers both verbs with
// a `SignalAck` at the gate. These tests own the CLI end of that exchange — what
// the binary does with the four lines it can get back — against a stub, which is
// the only way to drive a refusal without also building the daemon state that
// produces one.
// ---------------------------------------------------------------------------

/// Run the real CLI against a stub that reads one line and then writes `reply`
/// (nothing at all when `reply` is `None`, which is a daemon predating the ack).
/// Returns the process output.
fn cli_against_stub_reply(args: &[&str], reply: Option<&str>) -> std::process::Output {
    let dir = common::harness_tempdir().expect("create temp dir for the stub hook socket");
    let socket_path = dir.path().join("hook.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind stub hook socket");
    let reply = reply.map(str::to_string);

    let stub = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut reader = std::io::BufReader::new(stream);
        let mut line = String::new();
        let _ = reader.read_line(&mut line);
        if let Some(reply) = reply {
            let mut stream = reader.into_inner();
            let _ = stream.write_all(format!("{reply}\n").as_bytes());
            let _ = stream.flush();
        }
    });

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args(args)
        .env("DOT_AGENT_DECK_SOCKET", &socket_path)
        .env("DOT_AGENT_DECK_PANE_ID", "cli-pane")
        .env_remove("DOT_AGENT_DECK_PANE_CAPABILITY")
        .output()
        .expect("run the real CLI");
    stub.join().expect("stub thread");
    output
}

/// The refusal line the daemon writes for a `work-done` or `dispatch` it would
/// not admit — the `missing_token` case, which is the one a LEGITIMATE sender
/// reaches (a `dot-agent-deck` in the pane older than the daemon).
const REFUSAL: &str = r#"{"kind":"signal_ack","accepted":false,"reason":"missing_token","error":"refused: this pane was issued a hook capability token and the message presented none."}"#;

/// The verbs under test, with the arguments that make each one send its signal.
fn fire_and_forget_invocations() -> Vec<Vec<&'static str>> {
    vec![
        vec!["work-done", "--task", "done"],
        vec!["dispatch", "unit", "--task", "do it"],
    ]
}

/// The point of the whole change: a refused signal is no longer a clean exit 0.
#[test]
fn a_refused_signal_is_reported_to_the_sender() {
    for args in fire_and_forget_invocations() {
        let out = cli_against_stub_reply(&args, Some(REFUSAL));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success(),
            "{args:?} exited 0 on a signal the daemon refused, which is issue #1129 exactly;              stderr: {stderr}"
        );
        assert!(
            stderr.contains("hook capability token"),
            "{args:?} failed without passing on the daemon's reason: {stderr}"
        );
        assert!(
            stderr.contains("missing_token"),
            "{args:?} dropped the greppable code the daemon also put in its own warn line, so              an operator cannot match the two: {stderr}"
        );
    }
}

/// The admission is a success and says nothing — an agent running `work-done`
/// at the end of every task must not have its output decorated on the happy
/// path.
#[test]
fn an_admitted_signal_exits_zero_and_is_silent() {
    for args in fire_and_forget_invocations() {
        let out = cli_against_stub_reply(&args, Some(r#"{"kind":"signal_ack","accepted":true}"#));
        assert!(
            out.status.success(),
            "{args:?} failed on an admitted signal; stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            out.stderr.is_empty(),
            "{args:?} wrote to stderr on the happy path: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// The cross-version half, and the one that decides whether this change is safe
/// to ship: a daemon that predates the ack writes nothing back, and that must
/// stay a success. `delegate` made the same call for the same reason — the verb
/// was fire-and-forget before the daemon answered it, so a daemon that does not
/// answer must not become a phantom failure on every mixed-version pair.
#[test]
fn a_daemon_that_writes_no_ack_is_still_a_success() {
    for args in fire_and_forget_invocations() {
        let out = cli_against_stub_reply(&args, None);
        assert!(
            out.status.success(),
            "{args:?} failed against a daemon that predates the acknowledgement; stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// And the direction that would be easy to get wrong. Every field of `SignalAck`
/// is `#[serde(default)]` over an `accepted: bool`, so a line that is not an ack
/// parses into `accepted: false`. Without the affirmative-marker check that
/// would turn a daemon we do not understand — or another verb's reply arriving
/// on this connection — into a reported failure on a signal that was delivered.
#[test]
fn a_reply_that_is_not_an_ack_is_not_read_as_a_refusal() {
    for line in ["{}", r#"{"seed":null}"#, "not json at all", ""] {
        for args in fire_and_forget_invocations() {
            let out = cli_against_stub_reply(&args, Some(line));
            assert!(
                out.status.success(),
                "{args:?} treated {line:?} as a refusal; stderr: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}
