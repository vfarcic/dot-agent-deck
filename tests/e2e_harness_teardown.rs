#![cfg(feature = "e2e")]

//! L2 checks on the harness's own teardown — what `TuiDeck`'s `Drop` does when a
//! test panics with a deck still alive.
//!
//! Issue #1566: a test that panicked about 5s in lived on until nextest's 180s
//! slow-timeout killed it, so CI reported a timeout instead of the panic. The
//! whole linger was the failure dump regenerating the paired `test.md`, which
//! syn-parses every `#[spec]`-bearing source file in a debug build: measured
//! 8–24s on a loaded 16-core box (2026-10-04), against 0.4s for the rest of the
//! dump, and minutes on CI's starved 4-CPU runners. No failure consumer reads that
//! doc — CI uploads no recordings, and the demo-reel adapter refuses any
//! recording whose provenance is not `passed` — so the panic path no longer
//! regenerates it.

mod common;

use std::path::PathBuf;

use common::TuiDeck;
use spec::spec;

const DELIBERATE_PANIC: &str = "deliberate panic with a live deck (issue #1566)";

/// Moves an existing paired `test.md` out of the way and restores it on drop,
/// so the test can observe whether the drop under test writes one without
/// destroying a doc a developer generated. Leaves the path untouched when there
/// was nothing to set aside.
struct RestorePairedDoc {
    path: PathBuf,
    saved: Option<Vec<u8>>,
}

impl RestorePairedDoc {
    fn set_aside(path: PathBuf) -> Self {
        let saved = match std::fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => panic!("read existing {}: {e}", path.display()),
        };
        if saved.is_some() {
            std::fs::remove_file(&path)
                .unwrap_or_else(|e| panic!("set aside {}: {e}", path.display()));
        }
        Self { path, saved }
    }
}

impl Drop for RestorePairedDoc {
    fn drop(&mut self) {
        if let Some(bytes) = &self.saved
            && let Err(e) = std::fs::write(&self.path, bytes)
        {
            eprintln!("could not restore {}: {e}", self.path.display());
        }
    }
}

/// Scenario: Start the deck and a sleeping stand-in agent on a named thread,
/// capture their sandbox daemon and agent pids, then panic as a failing test does.
/// Teardown must stop both before deleting their working directory, still write
/// the failed recording (including relative logs from a launch subdirectory and Claude metadata from an overridden sandbox HOME), and skip regeneration of the paired test document.
#[test]
#[spec("harness/teardown/001")]
fn teardown_001_a_panicking_test_skips_the_paired_doc_regeneration() {
    let recordings = common::current_test_recordings_dir();
    let effective_home = common::harness_tempdir().unwrap();
    let home_path = effective_home.path().to_path_buf();
    let paired_doc = recordings.join("test.md");
    // A previous `DOT_AGENT_DECK_RECORD=1` run or `cargo xtask docs --tests`
    // may have left one, and the launch-time discard keeps it on purpose so a
    // developer browsing it does not lose it. The assertion below is about THIS
    // drop writing it, so set it aside and put it back however the test ends.
    let _restore = RestorePairedDoc::set_aside(paired_doc.clone());

    // The harness names the deck, and so its recordings directory and the
    // `#[spec]` function the regeneration looks up, after the current thread.
    // Naming the panicking thread after this test makes the regeneration, if it
    // runs, find this test's own `#[spec]` and write `test.md` — an observation
    // that does not depend on how loaded the machine is.
    let thread_name = std::thread::current()
        .name()
        .expect("libtest names the test thread")
        .to_string();
    #[cfg(unix)]
    let (sender, receiver) = std::sync::mpsc::channel();
    let joined = std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let builder = TuiDeck::builder()
                .with_launch_subdir("nested-launch")
                .with_env("DOT_AGENT_DECK_LOG", "relative-deck.log")
                .with_env("HOME", home_path.to_str().unwrap());
            #[cfg(unix)]
            let builder = builder.with_continue_session(
                "teardown-standin",
                "sh -c 'echo $$ > teardown-agent.pid; exec sleep 180'",
            );
            let deck = builder.launch_with_fixture("minimal");
            let projects = home_path.join(".claude/projects/fixture");
            std::fs::create_dir_all(&projects).unwrap();
            std::fs::write(
                projects.join("observed.jsonl"),
                "{\"sessionId\":\"overridden-home-session\",\"permissionMode\":\"acceptEdits\"}\n",
            )
            .unwrap();
            std::fs::write(
                deck.workdir().join("nested-launch/relative-deck.log"),
                "relative launch log sentinel\n",
            )
            .unwrap();
            #[cfg(unix)]
            {
                deck.wait_for_string("teardown-standin");
                let pid_path = deck.workdir().join("teardown-agent.pid");
                assert!(common::wait_for_path(
                    &pid_path,
                    std::time::Duration::from_secs(10)
                ));
                let agent_pid: i32 = std::fs::read_to_string(pid_path)
                    .unwrap()
                    .trim()
                    .parse()
                    .unwrap();
                let runtime = tokio::runtime::Runtime::new().unwrap();
                let stream = runtime
                    .block_on(dot_agent_deck::platform::ipc::IpcStream::connect(
                        deck.attach_socket_path(),
                    ))
                    .expect("connect to this test's daemon");
                let daemon_pid = dot_agent_deck::platform::peercred::peer_pid(&stream)
                    .expect("capture sandbox daemon peer pid")
                    as i32;
                sender
                    .send((daemon_pid, agent_pid, deck.workdir().to_owned()))
                    .unwrap();
            }
            #[cfg(not(unix))]
            deck.wait_for_string("No active agents");
            panic!("{DELIBERATE_PANIC}");
        })
        .expect("spawn the panicking thread")
        .join();
    // The deliberate panic specifically: a launch or `wait_for_string` timeout
    // also panics with the deck alive and takes the same failure path, so
    // accepting any panic would let a deck that never started pass this test.
    let payload = joined.expect_err("the deck thread must have panicked");
    let message = payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_default();
    assert_eq!(
        message, DELIBERATE_PANIC,
        "the deck thread panicked before reaching the deliberate panic"
    );

    #[cfg(unix)]
    {
        let (daemon_pid, agent_pid, workdir) = receiver.recv().expect("captured sandbox pids");
        // Reap only captured processes even when this regression is red, so
        // proving the leak does not leave another test agent on the host.
        struct ReapCaptured(i32, i32);
        impl Drop for ReapCaptured {
            fn drop(&mut self) {
                for pid in [self.1, self.0] {
                    if common::process_running(pid) {
                        // SAFETY: each pid was captured from this test's sandbox.
                        unsafe { libc::kill(pid, libc::SIGKILL) };
                    }
                }
            }
        }
        let _reap = ReapCaptured(daemon_pid, agent_pid);
        assert!(
            common::wait_until(std::time::Duration::from_secs(2), || {
                !common::process_running(daemon_pid) && !common::process_running(agent_pid)
            }),
            "panicking TuiDeck teardown left sandbox processes alive after deleting cwd {}: daemon pid {daemon_pid} alive={}, agent pid {agent_pid} alive={}",
            workdir.display(),
            common::process_running(daemon_pid),
            common::process_running(agent_pid)
        );
    }

    // Control: the drop really ran its failure dump, so the doc's absence below
    // is the regeneration being skipped rather than the dump never happening.
    let provenance = std::fs::read_to_string(recordings.join("provenance.json"))
        .expect("a panicking drop still writes the failure dump");
    let parsed: serde_json::Value =
        serde_json::from_str(&provenance).expect("provenance.json is JSON");
    assert_eq!(
        parsed["outcome"], "failed",
        "the dump must record the failure it was written for: {provenance}"
    );
    assert!(
        recordings.join("final-grid.txt").is_file(),
        "a panicking drop still writes the final grid"
    );
    assert!(
        std::fs::read_to_string(recordings.join("deck.log"))
            .is_ok_and(|log| log.contains("relative launch log sentinel")),
        "teardown lost the relative deck log from its actual launch directory"
    );
    assert!(
        parsed["claude_sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|session| session["session_id"] == "overridden-home-session"),
        "teardown lost Claude metadata from the effective child HOME: {provenance}"
    );

    assert!(
        !paired_doc.exists(),
        "the panicking drop regenerated {}: that step syn-parses every #[spec] \
         source file and kept a failed test's process alive until nextest's \
         180s timeout on starved CI runners (issue #1566)",
        paired_doc.display()
    );
}

/// Scenario: Start two stand-in daemons, one for this fixture and one standing
/// for another deck, then give the fixture's attach-socket name to the other
/// daemon's socket — first as a hard link, then as a symlink — and run the
/// harness teardown stop. Both attempts must be refused with the other daemon
/// still running and answering; pointed at the other daemon's own fixture, the
/// same stop then stops it.
#[cfg(unix)]
#[test]
#[spec("harness/teardown/002")]
fn teardown_002_a_swapped_fixture_socket_cannot_stop_another_daemon() {
    use common::FixtureDaemonStop;

    let mut other = common::spawn_daemon_serve(None, "0");
    let mut own = common::spawn_daemon_serve(None, "0");
    let own_root = own.attach_socket.parent().unwrap().to_path_buf();
    let other_root = other.attach_socket.parent().unwrap().to_path_buf();
    let own_socket = own.attach_socket.clone();

    fn assert_other_untouched(
        other: &mut common::DaemonProc,
        how: &str,
        outcome: common::FixtureDaemonStop,
    ) {
        assert!(
            matches!(outcome, common::FixtureDaemonStop::Refused(_)),
            "a fixture socket {how} the other daemon's socket must be refused, got {outcome:?}"
        );
        assert!(
            other.is_alive_public(),
            "teardown through a fixture socket {how} the other daemon's socket stopped it"
        );
        assert!(
            common::attach_request_on(
                &other.attach_socket,
                &dot_agent_deck::daemon_protocol::AttachRequest::ListAgents,
            )
            .is_ok_and(|resp| resp.ok),
            "the other daemon stopped answering after teardown through a socket {how} it"
        );
    }

    // A hard link keeps the fixture's own pathname, even canonicalised, while
    // connecting to it reaches the other daemon's listener.
    std::fs::remove_file(&own_socket).expect("unlink the fixture's socket name");
    std::fs::hard_link(&other.attach_socket, &own_socket)
        .expect("hard-link the other daemon's socket into the fixture");
    assert_other_untouched(
        &mut other,
        "hard-linked to",
        common::stop_fixture_daemon(&own_root, &own_socket),
    );

    // A symlink, as a process swapping the name after a path check would leave.
    std::fs::remove_file(&own_socket).expect("unlink the hard link");
    std::os::unix::fs::symlink(&other.attach_socket, &own_socket)
        .expect("symlink the other daemon's socket into the fixture");
    assert_other_untouched(
        &mut other,
        "symlinked to",
        common::stop_fixture_daemon(&own_root, &own_socket),
    );
    assert!(
        own.is_alive_public(),
        "the fixture's own daemon stopped, though neither swapped name leads to it"
    );

    // Control: the refusals above are about identity, not an inability to
    // stop — the other daemon's own fixture stops it.
    let pid = other.pid();
    assert_eq!(
        common::stop_fixture_daemon(&other_root, &other.attach_socket),
        FixtureDaemonStop::Stopped { pid },
        "the daemon launched with that fixture's socket must be stopped by it"
    );
    assert!(
        other.wait_for_exit(std::time::Duration::from_secs(2)),
        "the stopped daemon is still running"
    );
}
