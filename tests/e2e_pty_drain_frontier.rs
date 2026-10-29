#![cfg(all(feature = "e2e", target_os = "linux"))]

//! Real slave writes and the daemon's real master reader; no agent credential.
//! Assumed Q35-20a e2e surface (not implemented by this test slice):
//! `e2e-composer-control` operations `frontier-enqueue` and
//! `frontier-status:<request_id>` return `composer_frontier` with `request_id`,
//! `state` (`pending`, `taken`, `missing`) and `offset` for `taken`. Enqueue
//! uses the same shared outstanding request and reader as production, never
//! an alternate drain or a fixture-computed offset. Status is read-only.
//!
//! With DOT_AGENT_DECK_E2E_FRONTIER_GATE set, only a requested frontier pauses
//! immediately before each zero poll: publish its monotonically increasing
//! attempt in <dir>/arrived; wait until <dir>/release >= that attempt. A
//! release of u64::MAX opens subsequent attempts. This pause precedes poll,
//! not A6's separate after-zero/before-publication seam (coder-owned).
//!
//! Preserved RED mutant: DAD_TEST_FRONTIER_MUTANT=1 forwards
//! DOT_AGENT_DECK_E2E_FRONTIER_NO_ZERO_POLL=1 to this sandbox daemon. The
//! implementation's e2e-only mutant skips the zero poll and publishes the
//! current offset. The nonempty-master test below must fail on that mutant;
//! the mutation is not part of production and is not claimed executed yet.

mod common;
#[path = "support/pty_frontier.rs"]
mod fixture;
#[path = "support/inbox_doorbell.rs"]
mod inbox;

use fixture::{Peer, WAIT, WireCapture, listener, quote, script};
use serde_json::{Value, json};
use spec::spec;
use std::path::PathBuf;
use std::time::Instant;

struct Fixture {
    capture: WireCapture,
    agent: Peer,
    daemon: common::DaemonProc,
    generation: u64,
    gate: PathBuf,
}

impl Fixture {
    fn start(gated: bool) -> Self {
        // The gate directory is test-owned and lives until after the daemon.
        let daemon = common::spawn_daemon_serve_with_env(
            None,
            "0",
            &[("DOT_AGENT_DECK_E2E_ASSUME_EDITOR_PROBE", "claude@2.1.310")],
        );
        let gate = daemon.home.join("frontier-gate");
        std::fs::create_dir(&gate).unwrap();
        let listen = listener(&daemon.home, "agent");
        let script = script(&daemon.home);
        let mut env = vec![json!(["DOT_AGENT_DECK_PANE_ID", "frontier-pane"])];
        // The e2e reader captures these per-spawn values. They are never set
        // in the test process or inherited by a production daemon.
        if gated {
            env.push(json!(["DOT_AGENT_DECK_E2E_FRONTIER_GATE", gate]));
        }
        if std::env::var("DAD_TEST_FRONTIER_MUTANT").as_deref() == Ok("1") {
            env.push(json!(["DOT_AGENT_DECK_E2E_FRONTIER_NO_ZERO_POLL", "1"]));
        }
        let started = inbox::attach(
            &daemon,
            json!({
                "op":"start-agent", "command":format!("python3 {} {} agent", quote(&script), quote(&daemon.home)),
                "agent_type":"claude_code", "cwd":daemon.home, "env":env,
            }),
        );
        assert_eq!(
            started["ok"], true,
            "start only our Python stand-in: {started}"
        );
        let agent = Peer::accept(&listen, false);
        let generation = agent.ready["generation"]
            .as_str()
            .expect("installed generation")
            .parse()
            .unwrap();
        let capture = WireCapture::new(&daemon.attach_socket, started["id"].as_str().unwrap());
        Self {
            capture,
            agent,
            daemon,
            generation,
            gate,
        }
    }

    fn control(&self, operation: &str) -> Value {
        let reply = inbox::attach(
            &self.daemon,
            json!({
                "op":"e2e-composer-control", "pane_id":"frontier-pane",
                "generation":self.generation, "operation":operation,
            }),
        );
        assert_eq!(
            reply["ok"], true,
            "Q35-20a must expose the generation-bound {operation} reader control: {reply}"
        );
        reply["composer_frontier"].clone()
    }

    fn enqueue(&self) -> Value {
        let reply = self.control("frontier-enqueue");
        assert!(
            reply["request_id"].as_u64().is_some(),
            "shared request identity: {reply}"
        );
        reply
    }

    fn status(&self, request: &Value) -> Value {
        self.control(&format!(
            "frontier-status:{}",
            request["request_id"].as_u64().unwrap()
        ))
    }

    fn taken(&self, request: &Value) -> u64 {
        let answer = std::cell::RefCell::new(Value::Null);
        assert!(
            common::wait_until(WAIT, || {
                let state = self.status(request);
                assert_ne!(
                    state["state"], "missing",
                    "a live, quiet reader must take this frontier: {state}"
                );
                let done = state["state"] == "taken";
                *answer.borrow_mut() = state;
                done
            }),
            "frontier pending set must progress without a fresh wake"
        );
        answer.into_inner()["offset"]
            .as_u64()
            .expect("taken byte offset")
    }

    fn release(&self, attempt: u64) {
        let temporary = self.gate.join("release.tmp");
        std::fs::write(&temporary, attempt.to_string()).unwrap();
        std::fs::rename(temporary, self.gate.join("release")).unwrap();
    }

    fn arrived(&self, attempt: u64, request: &Value) {
        assert!(
            common::wait_until(WAIT, || {
                assert_eq!(
                    self.status(request)["state"],
                    "pending",
                    "frontier settled while the next zero poll still had a slave burst waiting"
                );
                std::fs::read_to_string(self.gate.join("arrived"))
                    .is_ok_and(|value| value.trim().parse::<u64>().unwrap() >= attempt)
            }),
            "reader must reach zero-poll attempt {attempt} without needing another wake"
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Release on assertion failure too: never leave the fixture reader
        // parked while its sandbox daemon is being torn down.
        self.release(u64::MAX);
    }
}

fn burst(trial: usize) -> Vec<u8> {
    let mut bytes = format!("\x1b[?2004hREAPED-CHILD-{trial:04}-").into_bytes();
    bytes.extend(std::iter::repeat_n(b'X', 32 * 1024));
    bytes.extend_from_slice(format!("-END-{trial:04}\x1b[?2026h").as_bytes());
    bytes
}

fn trials(check_frontier: bool) {
    let mut fixture = Fixture::start(false);
    if check_frontier {
        let initial = fixture.enqueue();
        assert_eq!(
            fixture.taken(&initial),
            0,
            "quiet empty master starts at offset zero"
        );
    }
    let start = Instant::now();
    let mut expected = Vec::new();
    for trial in 0..1000 {
        let data = burst(trial);
        let reaped = fixture.agent.command("burst", &data);
        assert_eq!(
            reaped["status"], 0,
            "child reaped after all three split writes"
        );
        assert_eq!(reaped["count"], data.len());
        expected.extend_from_slice(&data);
        if check_frontier {
            let request = fixture.enqueue(); // strictly after waitpid, not a timer
            let offset = fixture.taken(&request);
            assert_eq!(
                offset,
                expected.len() as u64,
                "trial {trial}: every completed slave byte must be behind frontier o"
            );
        }
    }
    let barrier = b"FINAL-PRODUCTION-MASTER-CAPTURE-BARRIER";
    fixture.agent.command("write", barrier);
    fixture.capture.through(barrier);
    expected.extend_from_slice(barrier);
    let actual = fixture.capture.bytes();
    assert!(
        actual == expected,
        "raw master capture differs: actual length {}, expected {}, first mismatch {:?}",
        actual.len(),
        expected.len(),
        actual.iter().zip(&expected).position(|(a, b)| a != b)
    );
    eprintln!(
        "1000 reaped-child split-write trials, frontier={check_frontier}: {:?}",
        start.elapsed()
    );
}

/// Scenario: Ask a quiet synthetic pane for its frontier, then reap a child that writes more than the line-discipline buffer in three split writes on each of 1,000 trials. Each request follows its child's reap and must place every captured byte behind its offset, with an exact raw master-stream comparison after a final capture barrier.
#[spec("inbox/probe/062")]
#[test]
fn probe_062_reaped_split_bursts_are_behind_the_reader_frontier() {
    trials(true);
}

/// Scenario: Run the same 1,000 reaped-child, large split-write trials through the existing master reader without requesting a frontier. Compare every output byte after a final capture barrier and report the fixture loop's time; this is a fixture and runtime-budget control, not frontier evidence.
#[spec("inbox/probe/062")]
#[test]
fn probe_062_burst_fixture_and_capture_control() {
    trials(false);
}

/// Scenario: Pause each zero-poll attempt and let a foreign slave writer fill the master until its actual write returns EAGAIN. Three nonempty attempts must keep the request pending; when that writer stops adding bytes, the original request must settle after the reader drains, without another wake.
#[spec("inbox/probe/062")]
#[test]
fn probe_062_a_backpressured_foreign_writer_delays_the_frontier() {
    let mut fixture = Fixture::start(true);
    let listen = listener(&fixture.daemon.home, "writer");
    let spawned = fixture.agent.command("retain", b"");
    let mut foreign = Peer::accept(&listen, true);
    assert_eq!(foreign.ready["pid"], spawned["writer"]);
    let request = fixture.enqueue();
    for attempt in 1..=3 {
        fixture.arrived(attempt, &request);
        let written = foreign.command("fill", &burst(attempt as usize));
        assert_eq!(written["errno"], libc::EAGAIN);
        assert!(
            written["count"].as_u64().unwrap() > 0,
            "positive foreign-write control"
        );
        assert_eq!(fixture.status(&request)["state"], "pending");
        fixture.release(attempt);
    }
    fixture.arrived(4, &request);
    fixture.release(u64::MAX);
    let offset = fixture.taken(&request);
    let barrier = b"FOREIGN-WRITER-STOPPED-CAPTURE-BARRIER";
    fixture.agent.command("write", barrier);
    fixture.capture.through(barrier);
    assert_eq!(
        offset as usize + barrier.len(),
        fixture.capture.len(),
        "frontier follows every actually written foreign byte"
    );
}
