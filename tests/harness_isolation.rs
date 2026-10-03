//! The test harness must detach from any real deck before it spawns anything.
//!
//! Running the suite from inside a deck pane means this process inherits that
//! pane's `DOT_AGENT_DECK_SOCKET` / `_PANE_ID`. Anything spawned inherits them
//! too, and its hooks then post into the developer's LIVE dashboard — a card
//! appears under a fixture pane id and vanishes again. `ff5170d` scrubs these in
//! `agent_pty::spawn`, which is necessary but not sufficient: real `deck.log`
//! evidence shows four of five leaked fixture pane ids arriving from a tree that
//! already had that fix, through other spawn paths. Clearing the vars from the
//! test process covers every spawn path at once, including ones added later.
//!
//! The clearing happens before `main`, in a constructor in `tests/common/mod.rs`
//! (issue #1473), and that is also the only place the harness writes these
//! variables: a write at run time races any thread already reading the
//! environment (issue #678). So the tests below put the "live deck" into a
//! re-executed child's environment at exec rather than into this process's.

mod common;

/// The five deck endpoint and identity variables, with values that mimic a live
/// deck's.
const LIVE_DECK_VARS: [(&str, &str); 5] = [
    (
        "DOT_AGENT_DECK_SOCKET",
        "/run/user/1000/dot-agent-deck.sock",
    ),
    (
        "DOT_AGENT_DECK_ATTACH_SOCKET",
        "/run/user/1000/dot-agent-deck-attach.sock",
    ),
    ("DOT_AGENT_DECK_PANE_ID", "8"),
    ("DOT_AGENT_DECK_AGENT_ID", "8"),
    (
        "DOT_AGENT_DECK_PANE_CAPABILITY",
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    ),
];

/// Set only on the re-executed child of
/// [`harness_clears_inherited_deck_endpoints`].
const CLEARS_CHILD_ENV: &str = "DAD_TEST_HARNESS_CLEARS_CHILD";

/// What that child prints once both of its checks have passed, so the parent
/// can tell a real pass from a filter that matched nothing.
const CLEARS_CHILD_DONE: &str = "harness-clears-child-checked";

/// Scenario: Start a fresh copy of this test binary with all five deck endpoint
/// variables set to values that mimic a live deck, and in that child assert
/// every one is already gone on the first line of the test body, call the
/// harness setup hook, and assert they are still gone and the hook named them —
/// so no child the process spawns can inherit a route to a real daemon.
#[test]
fn harness_clears_inherited_deck_endpoints() {
    if std::env::var_os(CLEARS_CHILD_ENV).is_some() {
        for (var, _) in LIVE_DECK_VARS {
            assert!(
                std::env::var_os(var).is_none(),
                "{var} reached the test body — the before-main detach did not clear it"
            );
        }
        common::init_test_env();
        for (var, _) in LIVE_DECK_VARS {
            assert!(
                std::env::var_os(var).is_none(),
                "{var} survived harness setup — a spawned child would inherit it and \
                 could post hook events into a live deck"
            );
        }
        println!("{CLEARS_CHILD_DONE}");
        return;
    }

    let name = "harness_clears_inherited_deck_endpoints";
    let output = std::process::Command::new(std::env::current_exe().expect("test binary path"))
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(CLEARS_CHILD_ENV, "1")
        .envs(LIVE_DECK_VARS)
        .output()
        .expect("re-execute the test binary");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "the harness left a deck variable behind:\n{stdout}\n{stderr}"
    );
    // libtest exits 0 when the filter matches nothing, so prove the child
    // actually ran its half.
    assert!(
        stdout.contains(CLEARS_CHILD_DONE),
        "the child did not run {name}:\n{stdout}\n{stderr}"
    );
    assert!(
        stderr.contains("note: detaching this test process from a live deck"),
        "the harness must report what it cleared:\n{stderr}"
    );
}

// ---------------------------------------------------------------------------
// Issue #1473 — clearing the variables does not close the endpoint FALLBACK
// ---------------------------------------------------------------------------
//
// With `DOT_AGENT_DECK_SOCKET` / `_ATTACH_SOCKET` cleared, every resolver falls
// back to `$XDG_RUNTIME_DIR/dot-agent-deck{,-attach}.sock` — which on a
// developer's machine IS the live deck — or, with `XDG_RUNTIME_DIR` unset, to
// `${TMPDIR:-/tmp}/dot-agent-deck-<uid>/` and then the legacy `/tmp` spellings.
// Measured on `main` at 608ed56e: `orchestration/delegate/039`'s wrapped worker
// posted its fork-time `SessionStart` there (the ghost "Codex" card), and
// `dashboard/selection/016` asked the live daemon for a close preview.
//
// The reproduction has to put the stand-in "live deck" in the child's
// environment BEFORE the child process starts — that is what inheriting it from
// a deck pane means, and the guard runs before `main`. So the parent below
// re-executes this test binary, and the child does what the two leakers did.
#[cfg(unix)]
mod live_deck_fallback {
    use std::io::{Read, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use dot_agent_deck::agent_pty::{AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, SpawnOptions};

    use super::common;

    /// Set only on the re-executed child; its value is the control endpoint.
    const PROBE_CONTROL_ENV: &str = "DAD_TEST_LIVE_DECK_PROBE_CONTROL";

    const CHILD_TEST: &str = "live_deck_fallback::reexec_child_probes_for_a_live_deck";

    /// Pane ids the child spawns under, so a capture names which spawn it was.
    const LEAK_PANE: &str = "live-deck-probe-leak";
    const CONTROL_PANE: &str = "live-deck-probe-control";

    /// Set only on the re-executed child; the file it creates once both
    /// workers are past their fork-time post.
    const PROBE_READY_ENV: &str = "DAD_TEST_LIVE_DECK_PROBE_READY";

    /// What the stand-in `codex` prints before it becomes a byte sink. The
    /// wrapper starts relaying the inner command's output only AFTER its
    /// fork-time `SessionStart` send has returned (`run_wrap_pty`: the send is
    /// synchronous and the output pump is spawned after it), so this showing up
    /// in a worker's PTY means that worker's post has already been attempted —
    /// for the unpinned worker too, whose post has no other observable trace
    /// once the guard works.
    const STUB_SENTINEL: &str = "live-deck-probe-stub-up";

    /// How long the child may take to see both workers past their post, and
    /// the parent to see the child report it. Generous: under a loaded tier the
    /// deck binary's cold start is the slow part, and a miss fails loudly.
    const PROBE_DEADLINE: Duration = Duration::from_secs(90);

    /// A listening Unix socket standing in for a daemon, made the way the
    /// daemon makes its own: owner-only, in an owner-only directory, so a
    /// client's trust check cannot be the reason nothing arrived.
    fn stand_in(path: &Path) -> UnixListener {
        let dir = path.parent().expect("endpoint has a parent");
        std::fs::create_dir_all(dir).expect("create stand-in endpoint dir");
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .expect("chmod stand-in endpoint dir");
        let listener = UnixListener::bind(path)
            .unwrap_or_else(|e| panic!("bind stand-in {}: {e}", path.display()));
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .expect("chmod stand-in endpoint");
        listener
            .set_nonblocking(true)
            .expect("nonblocking stand-in");
        listener
    }

    /// Everything that connected to `listener` so far, one string per
    /// connection, with whatever it sent.
    fn drain(listener: &UnixListener) -> Vec<String> {
        let mut seen = Vec::new();
        while let Ok((mut stream, _)) = listener.accept() {
            stream.set_nonblocking(false).ok();
            stream
                .set_read_timeout(Some(Duration::from_millis(500)))
                .ok();
            let mut buf = Vec::new();
            let _ = stream.read_to_end(&mut buf);
            seen.push(String::from_utf8_lossy(&buf).into_owned());
        }
        seen
    }

    fn uid() -> u32 {
        // SAFETY: `geteuid` takes no arguments and cannot fail.
        unsafe { libc::geteuid() }
    }

    /// Where the developer's live deck sits, for each way it can be reached
    /// once the explicit endpoint variables are gone.
    enum Rung {
        /// `$XDG_RUNTIME_DIR/dot-agent-deck{,-attach}.sock` — a desktop session.
        RuntimeDir,
        /// `$TMPDIR/dot-agent-deck-<uid>/{hook,attach}.sock` — `XDG_RUNTIME_DIR`
        /// unset, e.g. an ssh session. The legacy `/tmp/dot-agent-deck-<uid>.sock`
        /// spellings are consulted under exactly the same condition (the
        /// resolved address came from this rung), so closing this one closes
        /// them too; they are not planted here because `/tmp` is a literal and
        /// the real one may hold the developer's own alias.
        TempDir,
    }

    fn run_case(rung: Rung) {
        let root = common::race_safe_tempdir();
        let (live_hook, live_attach) = match rung {
            Rung::RuntimeDir => (
                root.path().join("run").join("dot-agent-deck.sock"),
                root.path().join("run").join("dot-agent-deck-attach.sock"),
            ),
            Rung::TempDir => {
                let dir = root
                    .path()
                    .join("tmp")
                    .join(format!("dot-agent-deck-{}", uid()));
                (dir.join("hook.sock"), dir.join("attach.sock"))
            }
        };
        let hook = stand_in(&live_hook);
        let attach = stand_in(&live_attach);
        let control_path = root.path().join("control").join("hook.sock");
        let control = stand_in(&control_path);
        let ready_path = root.path().join("probe-ready");

        let mut child = Command::new(std::env::current_exe().expect("test binary path"));
        child
            .args(["--exact", CHILD_TEST, "--nocapture", "--test-threads=1"])
            .env(PROBE_CONTROL_ENV, &control_path)
            .env(PROBE_READY_ENV, &ready_path)
            // What the existing detach already clears — #1473 is the half that
            // is left once these are gone, so start from there.
            .env_remove("DOT_AGENT_DECK_SOCKET")
            .env_remove("DOT_AGENT_DECK_ATTACH_SOCKET")
            .env_remove("DOT_AGENT_DECK_PANE_CAPABILITY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        match rung {
            Rung::RuntimeDir => {
                child.env("XDG_RUNTIME_DIR", root.path().join("run"));
            }
            Rung::TempDir => {
                child
                    .env_remove("XDG_RUNTIME_DIR")
                    .env("TMPDIR", root.path().join("tmp"));
            }
        }
        let mut child = child.spawn().expect("re-execute the probe child");

        // Wait for the child to report both workers past their fork-time post
        // (or to exit, which it does early only by failing).
        let deadline = Instant::now() + PROBE_DEADLINE;
        while !ready_path.exists() && Instant::now() < deadline {
            if let Ok(Some(_)) = child.try_wait() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // Closing the child's stdin is its signal to shut its agents down.
        drop(child.stdin.take());
        let output = child.wait_with_output().expect("wait for the probe child");
        let child_log = format!(
            "child status {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let control_seen = drain(&control);

        assert!(
            output.status.success(),
            "the probe child failed\n{child_log}"
        );
        assert!(
            ready_path.exists(),
            "the probe child never saw both wrapped workers past their fork-time post within \
             {PROBE_DEADLINE:?}, so an empty live-deck listener would prove nothing\n{child_log}"
        );
        assert!(
            control_seen.iter().any(|line| line.contains(CONTROL_PANE)),
            "control: the wrapped worker pinned at an explicit endpoint got past its fork-time \
             post without anything reaching its endpoint, so the wrapper is not posting and the \
             absence below would prove nothing; control saw {control_seen:?}\n{child_log}"
        );

        let leaked_hook = drain(&hook);
        let leaked_attach = drain(&attach);
        assert!(
            leaked_hook.is_empty() && leaked_attach.is_empty(),
            "a test process reached the stand-in LIVE deck through the endpoint fallback at {} \
             (issue #1473) — on a developer's machine this is their running dashboard, and a \
             wrapped worker's SessionStart there is a ghost card.\n  hook endpoint {} received \
             {leaked_hook:?}\n  attach endpoint {} received {leaked_attach:?}",
            live_hook.parent().unwrap().display(),
            live_hook.display(),
            live_attach.display(),
        );
    }

    /// Scenario: Stand a fake "live deck" up at `$XDG_RUNTIME_DIR/dot-agent-deck.sock`
    /// and `…-attach.sock`, start a test process that inherits that runtime dir
    /// but none of the deck endpoint variables, and have it do what
    /// `orchestration/delegate/039` and `dashboard/selection/016` did — resolve
    /// and connect to the deck endpoints in-process, and spawn a wrapped Codex
    /// worker from a bare registry. A second worker pinned at a control socket
    /// proves the wrapper does post, and both workers are seen past their post
    /// before anything is checked; nothing may arrive at the fake live deck.
    /// Then the same with `XDG_RUNTIME_DIR` unset and the fake deck under
    /// `$TMPDIR/dot-agent-deck-<uid>/`.
    #[test]
    fn a_test_cannot_reach_a_live_deck_through_the_endpoint_fallback() {
        run_case(Rung::RuntimeDir);
        run_case(Rung::TempDir);
    }

    /// The re-executed half of
    /// [`a_test_cannot_reach_a_live_deck_through_the_endpoint_fallback`]. A no-op
    /// unless that test started this process.
    ///
    /// Scenario: Only when re-executed by the parent above: call the harness
    /// setup hook as `orchestration/delegate/039` does, connect to the hook and
    /// attach endpoints the client resolvers return as `dashboard/selection/016`
    /// does, spawn a wrapped Codex worker from a registry with no hook socket and
    /// a second one pinned at the parent's control endpoint, wait until each
    /// worker's stub output has come through its wrapper (proof its fork-time
    /// post was attempted), tell the parent, and hold them until it closes stdin.
    #[test]
    fn reexec_child_probes_for_a_live_deck() {
        let Some(control) = std::env::var_os(PROBE_CONTROL_ENV) else {
            return;
        };
        let control = PathBuf::from(control);
        let ready = PathBuf::from(std::env::var_os(PROBE_READY_ENV).expect("ready path is set"));
        common::init_test_env();

        // `dashboard/selection/016`'s shape: this process resolves the deck
        // endpoints for itself and connects.
        for endpoint in [
            dot_agent_deck::endpoint_resolve::client_socket_path(),
            dot_agent_deck::endpoint_resolve::client_attach_socket_path(),
        ] {
            if let Ok(mut stream) = UnixStream::connect(&endpoint) {
                let _ = writeln!(stream, "in-process probe from {}", std::process::id());
            }
        }

        // `orchestration/delegate/039`'s shape: a bare registry respawning a
        // `codex` under a real `dot-agent-deck wrap`.
        let cwd = common::race_safe_tempdir();
        let bin_dir = cwd.path().join("bin");
        std::fs::create_dir_all(&bin_dir).expect("create probe bin dir");
        let codex = bin_dir.join("codex");
        std::fs::write(
            &codex,
            format!(
                "#!/bin/sh\n[ \"$1\" = app-server ] && exit 1\necho {STUB_SENTINEL}\nexec cat\n"
            ),
        )
        .expect("write probe codex");
        std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755))
            .expect("chmod probe codex");
        let deck_dir = Path::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
            .parent()
            .expect("built deck binary has a parent directory");
        let path = format!(
            "{}:{}:{}",
            bin_dir.display(),
            deck_dir.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let cwd_str = cwd.path().to_string_lossy().into_owned();
        let registry = Arc::new(AgentPtyRegistry::new());
        let mut agents = Vec::new();
        for (pane, pin) in [
            (LEAK_PANE, None),
            (CONTROL_PANE, Some(control.to_string_lossy().into_owned())),
        ] {
            let mut env = vec![
                (DOT_AGENT_DECK_PANE_ID.to_string(), pane.to_string()),
                ("PATH".to_string(), path.clone()),
            ];
            if let Some(pin) = pin {
                env.push(("DOT_AGENT_DECK_SOCKET".to_string(), pin));
            }
            let agent = registry
                .spawn_agent(SpawnOptions {
                    command: Some("codex"),
                    cwd: Some(&cwd_str),
                    env,
                    ..SpawnOptions::default()
                })
                .unwrap_or_else(|e| panic!("spawn the {pane} worker: {e}"));
            agents.push((pane, agent));
        }

        // Each worker past its fork-time post — see `STUB_SENTINEL`.
        let deadline = Instant::now() + PROBE_DEADLINE;
        for (pane, agent) in &agents {
            loop {
                let snapshot = registry.snapshot(agent).unwrap_or_default();
                if String::from_utf8_lossy(&snapshot).contains(STUB_SENTINEL) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "the {pane} worker's stub never printed through its wrapper; PTY so far: {:?}",
                    String::from_utf8_lossy(&snapshot)
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        std::fs::write(&ready, b"both workers past their fork-time post\n")
            .expect("report readiness to the parent");

        let _ = std::io::stdin().read_to_end(&mut Vec::new());
        registry.shutdown_all();
    }
}

// ---------------------------------------------------------------------------
// PR #805 audit blocker 3 — a stale recording must not survive a run
// ---------------------------------------------------------------------------
//
// `TuiDeck` writes its artifacts only from `Drop`, and only on a panic or under
// `DOT_AGENT_DECK_RECORD`. Every route that ends a run without reaching `Drop` —
// SIGKILL, a nextest timeout, Ctrl-C, a runtime skip before launch, a failed
// re-recording — used to leave the PREVIOUS run's `full-stream.cast` in place,
// and `.claude/skills/demo-reel-adapter` selects a cast on path existence alone.
// A cast from a revision predating the redaction fixes could therefore be
// stitched into a video and uploaded to YouTube with its link in the PR body and
// the public release notes. (The upload is private by default now, so widening
// it beyond the channel owner and the accounts they deliberately share it with
// is a human step — that bounds the blast radius; it does not make a stale cast
// correct.)
//
// Both call sites of the discard are covered here: the runtime-skip one by
// actually taking it, and the launch one by a source guard, because observing it
// needs a real PTY launch and this file is in the fast tier.
//
// Issue #808 added the other half, and the guards for it live below the discard
// ones in this file. Clearing is keyed to REACHING one of those two call sites,
// and two routes do not: a FILTERED run never selects the test at all, and
// `skip_unless!` evaluates its preflight before `_skip_if_err` is entered. The
// answer to both is that the adapter checks PROVENANCE rather than existence, so
// what has to hold here is that the harness writes a sidecar the adapter can
// read, and that the two ends of that contract cannot drift apart silently.

/// The artifacts the harness dumps — the set the discard has to clear. Mirrors
/// `RECORDING_ARTIFACTS` in `tests/common/mod.rs`; the guard below proves the two
/// lists and the dump itself still agree.
const RECORDING_ARTIFACTS: [&str; 6] = [
    "provenance.json",
    "final-grid.txt",
    "final-grid.svg",
    "full-stream.cast",
    "fixture.toml",
    "daemon.log",
];

fn adapter_build_script() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".claude")
        .join("skills")
        .join("demo-reel-adapter")
        .join("build.sh");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn harness_source() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("common")
        .join("mod.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Scenario: Plant a stale `full-stream.cast` — plus every other recording
/// artifact and the paired `test.md` — in this test's own recordings directory,
/// then take the runtime-skip path through `skip_unless!`'s helper, which is one
/// of the routes that used to leave that cast behind while nextest reported the
/// test as passed. Afterwards every recording artifact is gone — including the
/// `provenance.json` sidecar that would otherwise still vouch for the stale cast
/// — and the paired doc is untouched.
#[test]
fn a_runtime_skip_discards_the_previous_recording() {
    // The helper PANICS instead of skipping when this is set, which would make
    // the test's outcome depend on the developer's environment.
    // SAFETY: the first statement of a synchronous test body, so no thread this
    // test or the harness starts exists yet. What else can exist is libtest's
    // runner thread, waiting for this test to finish — and, under plain `cargo
    // test` rather than the nextest every gate here uses, sibling tests sharing
    // this process, which would make this a race (issues #245, #678).
    unsafe { std::env::remove_var("DOT_AGENT_DECK_REQUIRE_REAL_E2E") };

    let dir = common::current_test_recordings_dir();
    std::fs::create_dir_all(&dir).expect("create this test's recordings dir");
    for name in RECORDING_ARTIFACTS {
        std::fs::write(dir.join(name), b"STALE-ARTIFACT-FROM-AN-EARLIER-REVISION")
            .expect("plant a stale artifact");
    }
    std::fs::write(dir.join("test.md"), b"# generated from the test source\n")
        .expect("plant the paired doc");

    let skipped = common::_skip_if_err(Err("no credential on this host".to_string()));
    assert!(
        skipped,
        "an Err must produce a skip, or this proves nothing"
    );

    for name in RECORDING_ARTIFACTS {
        let path = dir.join(name);
        assert!(
            !path.exists(),
            "{} survived a runtime skip, so an interrupted or skipped run can \
             still hand the demo reel an artifact it did not produce",
            path.display()
        );
    }
    assert_eq!(
        std::fs::read_to_string(dir.join("test.md")).expect("the paired doc must survive"),
        "# generated from the test source\n",
        "the paired `.md` is regenerated from the test source, carries no \
         credential and no run identity, and must not be deleted with the \
         artifacts"
    );

    // This test's recordings directory is its own fixture, so it takes it away
    // again — but only on success, so a failure leaves the evidence in place.
    std::fs::remove_dir_all(&dir).expect("remove this test's fixture recordings dir");
}

/// Scenario: Plant a stale recording artifact the discard CANNOT delete — a
/// directory where `full-stream.cast` should be — and take the runtime-skip path.
/// The harness panics instead of warning and carrying on, so the test fails
/// rather than running to a green finish with an artifact it did not produce.
///
/// PR #805's second audit named the old warn-and-continue a fail-open and it was
/// right: a warning only helps if somebody reads it, the run that printed it was
/// still reported as PASSED, and the artifact it could not remove still satisfies
/// the demo-reel adapter's existence check. Unix-only, because "a deletion that
/// fails for a reason other than NotFound" is arranged here through `EISDIR`.
#[cfg(unix)]
#[test]
fn a_discard_that_cannot_delete_a_stale_artifact_fails_the_run() {
    // SAFETY: as in `a_runtime_skip_discards_the_previous_recording` — the first
    // statement of a synchronous test body, with the same stated residual.
    unsafe { std::env::remove_var("DOT_AGENT_DECK_REQUIRE_REAL_E2E") };

    let dir = common::current_test_recordings_dir();
    std::fs::create_dir_all(&dir).expect("create this test's recordings dir");
    // `remove_file` on a directory is EISDIR, not NotFound — an undeletable
    // stale artifact, without having to make the tree unwritable (which would
    // also stop the harness from cleaning up after itself).
    let undeletable = dir.join("full-stream.cast");
    std::fs::create_dir_all(&undeletable).expect("plant an undeletable stale artifact");

    let outcome =
        std::panic::catch_unwind(|| common::_skip_if_err(Err("no credential".to_string())));

    // Cleaned up before the assertions, so a failure does not leave a directory
    // named `full-stream.cast` behind to break every later run of this test.
    std::fs::remove_dir_all(&dir).expect("remove this test's fixture recordings dir");

    let payload = outcome.expect_err(
        "the discard must FAIL the run when it cannot remove a stale artifact — a \
         warning leaves the run green with a recording it did not produce, which \
         is exactly what the demo-reel adapter then publishes",
    );
    let message = payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
        .unwrap_or_default();
    assert!(
        message.contains("could not discard the previous recording")
            && message.contains("full-stream.cast"),
        "the panic must name the artifact it could not remove: {message}"
    );
}

/// Scenario: Read the harness source and assert that `TuiDeck::try_launch_inner`
/// discards the previous recording, that it does so before it spawns anything,
/// and that the artifact list the discard walks still names every file
/// `dump_recordings` writes.
///
/// A source guard rather than an observation, because observing it needs a real
/// PTY launch and this file is in the fast tier — the launch path itself is
/// exercised by every `tests/e2e_*.rs` file that launches a `TuiDeck`, which is
/// most of the tier; the rest drive a headless or in-process daemon and never
/// launch one. No counts here on purpose — the sentence this replaced named a
/// split, and both of its numbers had rotted as e2e files were added.
#[test]
fn tui_deck_launch_discards_the_previous_recording_before_it_spawns() {
    let source = harness_source();
    let body = source
        .split_once("fn try_launch_inner(")
        .expect("try_launch_inner must exist")
        .1;

    let discard = body.find("discard_previous_recording(&test_name)").expect(
        "try_launch_inner no longer discards the previous recording — an \
             interrupted run can leave a stale cast for the demo reel to \
             publish (PR #805 audit blocker 3)",
    );
    let spawn = body
        .find("slave.spawn_command")
        .or_else(|| body.find(".spawn_command("))
        .expect("try_launch_inner must spawn the deck somewhere");
    assert!(
        discard < spawn,
        "the discard must run BEFORE the deck is spawned: from the spawn onwards \
         the run can be killed at any instant, and whatever it has not \
         overwritten is what a later reel build picks up"
    );

    // The dump and the discard must agree on the file names, or the discard
    // silently stops covering whatever the dump added.
    let dump = source
        .split_once("fn dump_recordings(")
        .expect("dump_recordings must exist")
        .1;
    let dump = dump.split_once("\n    fn ").map_or(dump, |(head, _)| head);
    for name in RECORDING_ARTIFACTS {
        assert!(
            source.contains(&format!("\"{name}\",")),
            "{name} is missing from RECORDING_ARTIFACTS in tests/common/mod.rs"
        );
    }
    let mut written: Vec<String> = Vec::new();
    let mut rest = dump;
    while let Some((_, after)) = rest.split_once("dir.join(\"") {
        let (name, tail) = after
            .split_once('"')
            .expect("unterminated dir.join literal");
        written.push(name.to_string());
        rest = tail;
    }
    assert!(
        !written.is_empty(),
        "found no `dir.join(\"…\")` artifact writes in dump_recordings — this \
         guard has stopped guarding anything"
    );
    for name in &written {
        assert!(
            RECORDING_ARTIFACTS.contains(&name.as_str()),
            "dump_recordings writes `{name}`, which the launch-time discard does \
             not remove — add it to RECORDING_ARTIFACTS in tests/common/mod.rs, \
             or an interrupted run leaves it behind (PR #805 audit blocker 3)"
        );
    }
}

// ---------------------------------------------------------------------------
// Issue #808 — the cast provenance sidecar
// ---------------------------------------------------------------------------

/// Scenario: Feed `recording_build_commit` every `DAD_BUILD_ID` shape
/// `build.rs` can compose plus the ones an operator can inject, and assert it
/// extracts the commit and the dirty flag from the real ones while yielding no
/// commit for anything that cannot identify a revision.
///
/// The commit in `provenance.json` is a publish gate, so a parse that returns a
/// plausible-looking non-commit is worse than one that returns nothing: an empty
/// `commit` makes the adapter refuse the clip, which is the safe direction.
#[test]
fn the_build_id_parse_yields_a_commit_only_when_one_is_really_there() {
    for (build_id, want_commit, want_dirty) in [
        // The ordinary shapes `resolve_build_id` composes.
        ("0.39.2-g5a56361", Some("5a56361"), false),
        ("0.39.2-g5a56361-dirty", Some("5a56361"), true),
        // A SemVer prerelease keeps its own hyphens, and one of them can begin
        // with `g` — which is why the sha is taken from the LAST `-g` and not
        // the first.
        ("0.25.0-gamma.1-g5a56361", Some("5a56361"), false),
        ("0.25.0-gamma.1-g5a56361-dirty", Some("5a56361"), true),
        // Longer abbreviations are fine; the floor is a minimum, not a length.
        ("0.39.2-gdeadbeefcafe1234", Some("deadbeefcafe1234"), false),
        // The `-unknown` sentinel a git-less or shallow build composes. No
        // commit, so the adapter refuses anything recorded by such a build.
        ("0.39.2-unknown", None, false),
        // A prerelease that merely LOOKS like it carries a sha.
        ("0.1.0-gamma", None, false),
        // An injected DAD_BUILD_ID (issue #250) that says nothing about a
        // revision, with and without the operator adding the dirty suffix.
        ("ci-build-1234", None, false),
        ("ci-build-1234-dirty", None, true),
        // Too short to be evidence: git's auto-abbreviation floor is 7, so a
        // 6-character prefix is a `core.abbrev` setting, not a commit.
        ("0.39.2-gabc123", None, false),
        // Not hex, so not a sha however long it is.
        ("0.39.2-gnothexatall", None, false),
    ] {
        let (commit, dirty) = common::recording_build_commit(build_id);
        assert_eq!(
            commit, want_commit,
            "recording_build_commit({build_id:?}) extracted the wrong commit"
        );
        assert_eq!(
            dirty, want_dirty,
            "recording_build_commit({build_id:?}) got the dirty flag wrong"
        );
    }
}

/// Scenario: Read the harness source and assert the provenance sidecar is
/// written LAST of the dump's artifacts while sitting FIRST in the list the
/// launch-time discard walks.
///
/// Both orders are fail-closed and they point opposite ways, which is why
/// neither is an accident worth leaving unguarded. Written last: a dump that
/// dies partway leaves a cast with no sidecar, and the adapter refuses a cast
/// with no sidecar — so a torn dump produces nothing publishable rather than
/// something publishable. Discarded first: a discard that panics partway has
/// already removed the sidecar that would have vouched for whatever survives.
#[test]
fn the_provenance_sidecar_is_written_last_and_discarded_first() {
    let source = harness_source();

    let dump = source
        .split_once("fn dump_recordings(")
        .expect("dump_recordings must exist")
        .1;
    let dump_body = dump.split_once("\n    /// ").map_or(dump, |(head, _)| head);
    let cast = dump_body
        .find("full-stream.cast")
        .expect("dump_recordings must write the cast");
    let provenance = dump_body.find("write_provenance(").expect(
        "dump_recordings no longer writes the provenance sidecar — the demo-reel \
         adapter then has nothing to check and falls back to publishing on path \
         existence (issue #808)",
    );
    assert!(
        cast < provenance,
        "the provenance sidecar must be written AFTER the cast it vouches for: a \
         dump that dies in between must leave a cast the adapter refuses, not one \
         it accepts"
    );

    let list = source
        .split_once("const RECORDING_ARTIFACTS")
        .expect("RECORDING_ARTIFACTS must exist")
        .1;
    let list = list.split_once("];").expect("unterminated array").0;
    let prov_pos = list
        .find("\"provenance.json\"")
        .expect("provenance.json must be in RECORDING_ARTIFACTS, or an interrupted run leaves a sidecar vouching for a cast it did not produce");
    let cast_pos = list
        .find("\"full-stream.cast\"")
        .expect("full-stream.cast must be in RECORDING_ARTIFACTS");
    assert!(
        prov_pos < cast_pos,
        "provenance.json must be discarded BEFORE full-stream.cast: the discard \
         panics on the first failure, so removing the sidecar first means a \
         partial discard leaves nothing publishable"
    );
}

/// Scenario: Read every field the harness's `write_provenance` puts in
/// `provenance.json` and every field the adapter's `build.sh` reads out of it,
/// then assert the adapter's read set is a subset of the harness's write set and
/// that both sides agree on the schema number.
///
/// The two halves of this contract live in different languages and neither
/// compiles the other, so drift is invisible until a reel build refuses every
/// clip. That failure is at least safe — an unreadable field reads as absent and
/// the adapter refuses — but it is also silent until somebody tries to publish,
/// which for a lane-2-only artifact can be weeks. Cheap to check here instead.
///
/// Deliberately one-directional: a field the harness writes and the adapter
/// ignores (`test`, the recording's own name) breaks nothing, while a field the
/// adapter requires and the harness stopped writing breaks everything.
#[test]
fn the_provenance_contract_the_adapter_reads_is_the_one_the_harness_writes() {
    let source = harness_source();
    let script = adapter_build_script();

    let literal = source
        .split_once("let provenance = serde_json::json!({")
        .expect("write_provenance must build the sidecar with a json! literal")
        .1;
    let literal = literal
        .split_once("});")
        .expect("unterminated json! literal")
        .0;
    let mut written: Vec<String> = Vec::new();
    let mut rest = literal;
    while let Some((_, after)) = rest.split_once('"') {
        let (name, tail) = after.split_once('"').expect("unterminated field name");
        written.push(name.to_string());
        rest = tail;
    }
    assert!(
        written.len() >= 8,
        "found only {} field names in the provenance literal — this guard has \
         stopped guarding anything: {written:?}",
        written.len()
    );

    // The adapter's read set, taken from the one jq filter that destructures the
    // sidecar: each field appears there as `(.<name> // …)`.
    let filter = script
        .split_once("if ! fields=\"$(jq -r '")
        .expect("build.sh must read the sidecar through a single jq filter")
        .1;
    let filter = filter
        .split_once("' \"$file\"")
        .expect("unterminated jq filter")
        .0;
    let mut read: Vec<String> = Vec::new();
    let mut rest = filter;
    while let Some((_, after)) = rest.split_once("(.") {
        let name: String = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if !name.is_empty() && after[name.len()..].starts_with(" //") {
            read.push(name);
        }
        rest = after;
    }
    assert!(
        read.len() >= 8,
        "found only {} fields in the adapter's jq filter — this guard has stopped \
         guarding anything: {read:?}",
        read.len()
    );

    for field in &read {
        assert!(
            written.iter().any(|w| w == field),
            "`.claude/skills/demo-reel-adapter/build.sh` reads provenance field \
             `{field}`, which `write_provenance` in tests/common/mod.rs does not \
             write. The adapter treats an absent field as a refusal, so the reel \
             would silently stop selecting every clip. Write the field, or stop \
             reading it. (harness writes: {written:?})"
        );
    }

    // The schema number gates the whole sidecar, so a bump on one side alone
    // refuses every clip.
    let harness_schema = source
        .split_once("const RECORDING_PROVENANCE_SCHEMA: u32 = ")
        .expect("RECORDING_PROVENANCE_SCHEMA must exist")
        .1
        .split_once(';')
        .expect("unterminated const")
        .0
        .trim()
        .to_string();
    let adapter_schema = script
        .split_once("\nPROVENANCE_SCHEMA=")
        .expect("build.sh must declare PROVENANCE_SCHEMA")
        .1
        .lines()
        .next()
        .expect("PROVENANCE_SCHEMA has no value")
        .trim()
        .to_string();
    assert_eq!(
        harness_schema, adapter_schema,
        "the harness writes provenance schema {harness_schema} and the adapter \
         only accepts {adapter_schema} — every clip would be refused. Bump both \
         in the same commit."
    );
}
