//! Issue #868: `pane restart <role>` — a CLI-triggerable daemon verb that
//! restarts a worker role's pane on demand, riding the same unversioned
//! hook-socket `DaemonMessage` channel `Delegate`/`Dispatch`/`GetSeed`
//! already use.
//!
//! M1 gave a naturally-exited worker's `AgentRecord` a `crashed ==
//! Some(true)` marker, but the only recovery path was a human restarting the
//! pane from the TUI. This file pins the daemon-side contract for the new
//! verb — `RestartRoleSignal` in, `RestartRoleResponse` out, handled by
//! `handle_restart_role_with_state` — calling the handler directly the same
//! way `tests/delegate_respawn_recovery.rs`'s `delegate()` helper calls
//! `handle_delegate_with_state`, bypassing the CLI/socket layer entirely.

#![cfg(unix)]

use std::time::Duration;

use dot_agent_deck::agent_pty::{
    AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, PaneRecreateIdentity, SpawnOptions, TabMembership,
};
use dot_agent_deck::event::{
    DelegateResponse, DelegateSignal, RestartRoleResponse, RestartRoleSignal,
};
use dot_agent_deck::state::OrchestrationIdentity;
use spec::spec;

mod common;

const ORCH_PANE: &str = "restart-orchestrator";
const WORKER_PANE: &str = "restart-coder";
const WORKER_ROLE: &str = "coder";
const UNKNOWN_ROLE: &str = "no-such-role";
const ORCHESTRATION: &str = "restart-orchestration";
const ORCHESTRATION_ID: &str = "restart-instance-1";

fn config(worker_command: &str) -> String {
    format!(
        "[[orchestrations]]\nname = \"{ORCHESTRATION}\"\n\n\
         [[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"cat\"\nstart = true\n\n\
         [[orchestrations.roles]]\nname = \"{WORKER_ROLE}\"\ncommand = \"{worker_command}\"\n"
    )
}

fn membership(role_index: usize, role_name: &str, is_start_role: bool, cwd: &str) -> TabMembership {
    TabMembership::Orchestration {
        name: ORCHESTRATION.to_string(),
        role_index,
        role_name: role_name.to_string(),
        is_start_role,
        orchestration_cwd: Some(cwd.to_string()),
        display_title: None,
        orchestration_id: Some(ORCHESTRATION_ID.to_string()),
    }
}

struct Fixture {
    daemon: common::InProcDaemon,
    _dir: tempfile::TempDir,
    worker_agent_id: String,
}

/// Spawn an orchestrator + a worker running `worker_command`, register both
/// roles. `worker_command` decides whether the worker stays healthy for the
/// life of the test (`"cat"`) or exits on its own shortly after boot to
/// simulate a crash (a short `sleep`), mirroring the M1 precedent test's own
/// technique of letting a stand-in exit naturally rather than sending it a
/// signal, so `pump_reader`'s EOF branch is what marks it `crashed`.
async fn fixture(worker_command: &str) -> Fixture {
    let daemon = common::spawn_inprocess_daemon().await;
    let dir = common::race_safe_tempdir();
    std::fs::write(
        dir.path().join(".dot-agent-deck.toml"),
        config(worker_command),
    )
    .expect("write orchestration config");
    let cwd = dir.path().to_string_lossy().into_owned();

    daemon
        .registry
        .spawn_agent(SpawnOptions {
            command: Some("cat"),
            cwd: Some(&cwd),
            display_name: Some("orchestrator"),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), ORCH_PANE.to_string())],
            tab_membership: Some(membership(0, "orchestrator", true, &cwd)),
            ..SpawnOptions::default()
        })
        .expect("spawn orchestrator stand-in");
    let worker_agent_id = daemon
        .registry
        .spawn_agent(SpawnOptions {
            command: Some(worker_command),
            cwd: Some(&cwd),
            display_name: Some(WORKER_ROLE),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), WORKER_PANE.to_string())],
            tab_membership: Some(membership(1, WORKER_ROLE, false, &cwd)),
            ..SpawnOptions::default()
        })
        .expect("spawn worker stand-in");

    {
        let mut state = daemon.state.write().await;
        let identity = OrchestrationIdentity::Instance {
            id: ORCHESTRATION_ID.to_string(),
            name: ORCHESTRATION.to_string(),
        };
        state.register_orchestration_role(
            ORCH_PANE,
            "orchestrator",
            true,
            identity.clone(),
            Some(&cwd),
        );
        state.register_orchestration_role(WORKER_PANE, WORKER_ROLE, false, identity, Some(&cwd));
    }

    Fixture {
        daemon,
        _dir: dir,
        worker_agent_id,
    }
}

/// Poll until `agent_id`'s registry record is `crashed == Some(true)`, the
/// same bounded-deadline idiom every other waiter in this harness uses.
async fn wait_for_crashed(registry: &AgentPtyRegistry, agent_id: &str, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if registry
            .agent_record_any(agent_id)
            .is_some_and(|r| r.crashed == Some(true))
        {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Call `dot-agent-deck pane restart <role>`'s daemon-side handler directly,
/// the same way `delegate_respawn_recovery.rs`'s `delegate()` helper calls
/// `handle_delegate_with_state` — bypassing the CLI/socket layer, which is
/// mechanical wiring already covered by `main.rs`'s other CLI arms.
async fn restart_role(
    fx: &Fixture,
    caller_pane_id: &str,
    role: &str,
    force: bool,
) -> RestartRoleResponse {
    let signal = RestartRoleSignal {
        pane_id: caller_pane_id.to_string(),
        role: role.to_string(),
        force,
        timestamp: chrono::Utc::now(),
        token: None,
    };
    dot_agent_deck::state::handle_restart_role_with_state(
        signal,
        &fx.daemon.state,
        &fx.daemon.registry,
        &fx.daemon.event_tx,
    )
    .await
}

/// Scenario: a worker stand-in exits on its own shortly after boot (M1 marks
/// its record `crashed == Some(true)`), then the orchestrator pane calls the
/// new restart handler with `force: false`. The role must come back: the
/// response reports success with no error, a fresh agent id now owns the
/// worker's pane, and the role registration survives — a subsequent
/// `delegate_targets` lookup from the orchestrator still resolves `coder` to
/// the same pane.
#[tokio::test(flavor = "multi_thread")]
#[spec("pane/restart/001")]
async fn pane_restart_001_restarts_a_crashed_worker_and_role_stays_reachable() {
    let fx = fixture("sleep 0.2").await;

    let crashed = wait_for_crashed(
        &fx.daemon.registry,
        &fx.worker_agent_id,
        Duration::from_secs(5),
    )
    .await;
    assert!(
        crashed,
        "precondition: the worker stand-in never got marked crashed; record = {:?}",
        fx.daemon.registry.agent_record_any(&fx.worker_agent_id)
    );

    let response = restart_role(&fx, ORCH_PANE, WORKER_ROLE, false).await;
    assert!(
        response.restarted,
        "restarting a crashed worker without force must succeed; response = {response:?}"
    );
    assert!(
        response.error.is_none(),
        "a successful restart must carry no error; response = {response:?}"
    );

    let new_agent_id = fx
        .daemon
        .registry
        .pane_current_agent_id(WORKER_PANE)
        .expect("the worker pane must have a live agent after restart");
    assert_ne!(
        new_agent_id, fx.worker_agent_id,
        "restart must replace the crashed agent with a freshly spawned one"
    );

    let state = fx.daemon.state.read().await;
    let targets = state.delegate_targets(ORCH_PANE, &[WORKER_ROLE.to_string()]);
    assert_eq!(
        targets,
        vec![(WORKER_ROLE.to_string(), WORKER_PANE.to_string())],
        "the role registration must survive the restart so the next delegate still resolves it"
    );
}

/// Scenario: the worker is healthy (never crashed) and the orchestrator calls
/// restart without `force`. The handler must refuse: `restarted` is false, an
/// error names that the pane is not crashed, and nothing was actually
/// killed — the worker's agent id is unchanged.
#[tokio::test(flavor = "multi_thread")]
#[spec("pane/restart/002")]
async fn pane_restart_002_refuses_a_healthy_pane_without_force() {
    let fx = fixture("cat").await;

    let response = restart_role(&fx, ORCH_PANE, WORKER_ROLE, false).await;
    assert!(
        !response.restarted,
        "restarting a healthy pane without force must be refused; response = {response:?}"
    );
    assert!(
        response
            .error
            .as_deref()
            .is_some_and(|e| e.to_lowercase().contains("crash")),
        "the refusal must explain the pane is not crashed; response = {response:?}"
    );

    let agent_id = fx
        .daemon
        .registry
        .pane_current_agent_id(WORKER_PANE)
        .expect("the worker pane must still have its original agent");
    assert_eq!(
        agent_id, fx.worker_agent_id,
        "a refused restart must not touch the healthy worker's agent"
    );
}

/// Scenario: the worker is healthy, but the orchestrator calls restart with
/// `force: true`. The handler must restart it anyway: success, and a fresh
/// agent id replaces the healthy one despite it never having crashed.
#[tokio::test(flavor = "multi_thread")]
#[spec("pane/restart/003")]
async fn pane_restart_003_force_restarts_a_healthy_pane() {
    let fx = fixture("cat").await;

    let response = restart_role(&fx, ORCH_PANE, WORKER_ROLE, true).await;
    assert!(
        response.restarted,
        "force must restart a healthy pane; response = {response:?}"
    );
    assert!(
        response.error.is_none(),
        "a successful forced restart must carry no error; response = {response:?}"
    );

    let new_agent_id = fx
        .daemon
        .registry
        .pane_current_agent_id(WORKER_PANE)
        .expect("the worker pane must have a live agent after a forced restart");
    assert_ne!(
        new_agent_id, fx.worker_agent_id,
        "force must replace the healthy agent with a freshly spawned one"
    );
}

/// Scenario: the orchestrator asks to restart a role name that does not
/// exist anywhere in this orchestration's registration. The handler must
/// refuse and name the unknown role rather than silently doing nothing.
#[tokio::test(flavor = "multi_thread")]
#[spec("pane/restart/004")]
async fn pane_restart_004_refuses_an_unknown_role() {
    let fx = fixture("cat").await;

    let response = restart_role(&fx, ORCH_PANE, UNKNOWN_ROLE, false).await;
    assert!(
        !response.restarted,
        "restarting an unregistered role must be refused; response = {response:?}"
    );
    assert!(
        response
            .error
            .as_deref()
            .is_some_and(|e| e.contains(UNKNOWN_ROLE)),
        "the refusal must name the unknown role; response = {response:?}"
    );
}

/// Scenario: the caller is the WORKER's own pane, not the orchestrator.
/// Mirrors `handle_delegate_with_state`'s anti-spoofing check — only the
/// orchestrator pane of an orchestration may trigger a restart within it.
#[tokio::test(flavor = "multi_thread")]
#[spec("pane/restart/005")]
async fn pane_restart_005_refuses_from_a_non_orchestrator_pane() {
    let fx = fixture("cat").await;

    let response = restart_role(&fx, WORKER_PANE, WORKER_ROLE, false).await;
    assert!(
        !response.restarted,
        "a restart requested by a non-orchestrator pane must be refused; response = {response:?}"
    );
    assert!(
        response.error.is_some(),
        "the refusal must carry an error explaining the caller is not this orchestration's \
         orchestrator; response = {response:?}"
    );

    let agent_id = fx
        .daemon
        .registry
        .pane_current_agent_id(WORKER_PANE)
        .expect("the worker pane must still have its original agent");
    assert_eq!(
        agent_id, fx.worker_agent_id,
        "a refused restart must not touch the worker's agent"
    );
}

/// Scenario: two orchestration instances share the exact same orchestration
/// `name` and `cwd` — told apart only by their PRD #140 `Instance` token —
/// each running a role named `coder`. Instance A's orchestrator
/// force-restarts `coder`; only instance A's worker may be touched. Routing
/// goes through `delegate_targets`' `OrchestrationIdentity` equality, not a
/// bare `(cwd, name)` tuple, so this is a regression guard for already-correct
/// behavior rather than a defect pin.
#[tokio::test(flavor = "multi_thread")]
#[spec("pane/restart/006")]
async fn pane_restart_006_two_same_name_cwd_instances_do_not_cross_restart() {
    let daemon = common::spawn_inprocess_daemon().await;
    let dir = common::race_safe_tempdir();
    std::fs::write(dir.path().join(".dot-agent-deck.toml"), config("cat"))
        .expect("write orchestration config");
    let cwd = dir.path().to_string_lossy().into_owned();

    const ORCH_PANE_A: &str = "restart-iso-orch-a";
    const WORKER_PANE_A: &str = "restart-iso-worker-a";
    const ORCH_PANE_B: &str = "restart-iso-orch-b";
    const WORKER_PANE_B: &str = "restart-iso-worker-b";

    let worker_agent_id_a = daemon
        .registry
        .spawn_agent(SpawnOptions {
            command: Some("cat"),
            cwd: Some(&cwd),
            display_name: Some(WORKER_ROLE),
            env: vec![(
                DOT_AGENT_DECK_PANE_ID.to_string(),
                WORKER_PANE_A.to_string(),
            )],
            tab_membership: Some(membership(1, WORKER_ROLE, false, &cwd)),
            ..SpawnOptions::default()
        })
        .expect("spawn instance A's worker stand-in");
    let worker_agent_id_b = daemon
        .registry
        .spawn_agent(SpawnOptions {
            command: Some("cat"),
            cwd: Some(&cwd),
            display_name: Some(WORKER_ROLE),
            env: vec![(
                DOT_AGENT_DECK_PANE_ID.to_string(),
                WORKER_PANE_B.to_string(),
            )],
            tab_membership: Some(membership(1, WORKER_ROLE, false, &cwd)),
            ..SpawnOptions::default()
        })
        .expect("spawn instance B's worker stand-in");
    daemon
        .registry
        .spawn_agent(SpawnOptions {
            command: Some("cat"),
            cwd: Some(&cwd),
            display_name: Some("orchestrator"),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), ORCH_PANE_A.to_string())],
            tab_membership: Some(membership(0, "orchestrator", true, &cwd)),
            ..SpawnOptions::default()
        })
        .expect("spawn instance A's orchestrator stand-in");
    daemon
        .registry
        .spawn_agent(SpawnOptions {
            command: Some("cat"),
            cwd: Some(&cwd),
            display_name: Some("orchestrator"),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), ORCH_PANE_B.to_string())],
            tab_membership: Some(membership(0, "orchestrator", true, &cwd)),
            ..SpawnOptions::default()
        })
        .expect("spawn instance B's orchestrator stand-in");

    {
        let mut state = daemon.state.write().await;
        let identity_a = OrchestrationIdentity::Instance {
            id: "restart-iso-instance-a".to_string(),
            name: ORCHESTRATION.to_string(),
        };
        let identity_b = OrchestrationIdentity::Instance {
            id: "restart-iso-instance-b".to_string(),
            name: ORCHESTRATION.to_string(),
        };
        state.register_orchestration_role(
            ORCH_PANE_A,
            "orchestrator",
            true,
            identity_a.clone(),
            Some(&cwd),
        );
        state.register_orchestration_role(
            WORKER_PANE_A,
            WORKER_ROLE,
            false,
            identity_a,
            Some(&cwd),
        );
        state.register_orchestration_role(
            ORCH_PANE_B,
            "orchestrator",
            true,
            identity_b.clone(),
            Some(&cwd),
        );
        state.register_orchestration_role(
            WORKER_PANE_B,
            WORKER_ROLE,
            false,
            identity_b,
            Some(&cwd),
        );
    }

    let signal = RestartRoleSignal {
        pane_id: ORCH_PANE_A.to_string(),
        role: WORKER_ROLE.to_string(),
        force: true,
        timestamp: chrono::Utc::now(),
        token: None,
    };
    let response = dot_agent_deck::state::handle_restart_role_with_state(
        signal,
        &daemon.state,
        &daemon.registry,
        &daemon.event_tx,
    )
    .await;
    assert!(
        response.restarted,
        "instance A's force restart of its own `coder` must succeed; response = {response:?}"
    );

    let agent_id_b_after = daemon
        .registry
        .pane_current_agent_id(WORKER_PANE_B)
        .expect("instance B's worker pane must still have a live agent");
    assert_eq!(
        agent_id_b_after, worker_agent_id_b,
        "instance A's restart of its own same-named `coder` must NEVER touch instance B's \
         same-name/same-cwd worker pane; response = {response:?}"
    );

    let agent_id_a_after = daemon
        .registry
        .pane_current_agent_id(WORKER_PANE_A)
        .expect("instance A's worker pane must have a live agent after restart");
    assert_ne!(
        agent_id_a_after, worker_agent_id_a,
        "sanity: instance A's OWN worker must actually have been restarted (a no-op restart \
         would make the isolation assertion above meaningless)"
    );
}

// ---------------------------------------------------------------------------
// Fix round (upstream PR #918 review): CLI-level RED coverage for `pane
// restart`'s own handling of `SocketReply::NoReply` and an unparseable reply
// line. `src/main.rs`'s `PaneCmd::Restart` arm used to fold BOTH into `return
// ExitCode::SUCCESS`, which reads to an orchestrating agent as "restarted"
// when nothing happened — wrong for this verb: an old/broken daemon cannot
// possibly have restarted anything. These drive the REAL CLI binary against a
// stub Unix-socket "daemon" — exactly `src/hook.rs`'s
// `socket_006_silent_close_returns_no_reply_not_empty_line` stub-listener
// technique — rather than calling the handler directly like every test
// above.
// ---------------------------------------------------------------------------

/// This test file's own deliberate, documented contract for the fix's stderr
/// wording (the task spec leaves exact phrasing to the coder): whatever
/// message the CLI prints when an old daemon silently closes the connection
/// without replying must say, case-insensitively, that the daemon does not
/// support this command — distinguishing it from a plain "restart failed"
/// error. Mirrors this file's own existing convention of asserting a
/// deliberately chosen keyword rather than exact prose (e.g.
/// `pane_restart_002`'s `.contains("crash")`).
const OLD_DAEMON_STDERR_NEEDLE: &str = "does not support";

/// Same contract, for the "reply line does not parse as a
/// `RestartRoleResponse`" branch: the message must say, case-insensitively,
/// that the daemon's response was unexpected/unrecognized.
const MALFORMED_REPLY_STDERR_NEEDLE: &str = "unexpected";

/// Bind a stub Unix-socket "daemon" at a fresh temp path, run the REAL
/// `dot-agent-deck pane restart <role>` CLI as a subprocess against it, let
/// `stub_reply` decide what (if anything) the stub writes back once it has
/// read the CLI's one request line, and return the subprocess's output.
fn run_pane_restart_against_stub(
    role: &str,
    stub_reply: impl FnOnce(std::os::unix::net::UnixStream) + Send + 'static,
) -> std::process::Output {
    let tmp = common::harness_tempdir().expect("create temp dir for stub daemon socket");
    let socket_path = tmp.path().join("s.sock");
    let listener =
        std::os::unix::net::UnixListener::bind(&socket_path).expect("bind stub daemon socket");

    let daemon_thread = std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            let mut reader = std::io::BufReader::new(&stream);
            let mut line = String::new();
            let _ = std::io::BufRead::read_line(&mut reader, &mut line);
            stub_reply(stream);
        }
    });

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args(["pane", "restart", role])
        .env(DOT_AGENT_DECK_PANE_ID, "stub-restart-caller-pane")
        .env("DOT_AGENT_DECK_SOCKET", &socket_path)
        .output()
        .expect("run the real `dot-agent-deck pane restart <role>` CLI");

    let _ = daemon_thread.join();
    output
}

/// Scenario: an old/broken daemon accepts the connection, reads the CLI's
/// one request line, then closes without writing anything back at all —
/// `SocketReply::NoReply`. The real `dot-agent-deck pane restart <role>` CLI
/// must exit non-zero and say the daemon does not support this command —
/// today it silently exits 0 with no output, reading as a successful
/// restart that never happened.
#[spec("pane/restart/007")]
#[test]
fn pane_restart_007_cli_fails_when_an_old_daemon_never_replies() {
    let output = run_pane_restart_against_stub(WORKER_ROLE, drop);

    assert!(
        !output.status.success(),
        "`pane restart` against a daemon that silently closes without replying \
         must exit non-zero, not the current ExitCode::SUCCESS; status = {:?}, \
         stdout = {:?}, stderr = {:?}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
    assert!(
        stderr.contains(OLD_DAEMON_STDERR_NEEDLE),
        "stderr must name that the daemon does not support this command (needle \
         {OLD_DAEMON_STDERR_NEEDLE:?}); got stderr = {stderr:?}"
    );
}

/// Scenario: the daemon replies, but with one line of unrelated/malformed
/// JSON that does not parse as a `RestartRoleResponse`. The real CLI must
/// exit non-zero and say the daemon's response was unexpected — today it
/// silently exits 0 with no output.
#[spec("pane/restart/008")]
#[test]
fn pane_restart_008_cli_fails_when_the_reply_does_not_parse_as_a_restart_response() {
    use std::io::Write as _;

    let output = run_pane_restart_against_stub(WORKER_ROLE, |mut stream| {
        let _ = stream.write_all(b"{\"type\":\"unrelated-malformed-reply\"}\n");
    });

    assert!(
        !output.status.success(),
        "`pane restart` against a daemon replying with an unparseable line must \
         exit non-zero, not the current ExitCode::SUCCESS; status = {:?}, \
         stdout = {:?}, stderr = {:?}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
    assert!(
        stderr.contains(MALFORMED_REPLY_STDERR_NEEDLE),
        "stderr must name that the daemon's response was unexpected (needle \
         {MALFORMED_REPLY_STDERR_NEEDLE:?}); got stderr = {stderr:?}"
    );
}

// ---------------------------------------------------------------------------
// Fix-round (upstream PR #918 review, "Restart Can Kill Replacement"):
// `handle_restart_role_with_state` checks crash state early, inside its own
// short-lived read-guard block, which drops before the function acquires
// `pane_dispatch_lock` and respawns. `dispatch_one_owned` (a concurrent
// `clear = true` delegate) holds the SAME `pane_dispatch_lock` while it
// installs a healthy replacement agent into the pane, so the handler
// re-checks crash state again once the dispatch lock is actually held,
// immediately before the respawn — if a delegate won the lock race and
// installed a replacement in the meantime, this second check catches it and
// refuses, the same as if the pane had never been crashed at all.
// ---------------------------------------------------------------------------

/// Scenario: the worker crashes and is marked `crashed == Some(true)` (same
/// precondition as `pane_restart_001`). The test itself then takes
/// `AgentPtyRegistry::pane_dispatch_lock(WORKER_PANE)` — the exact lock
/// `handle_restart_role_with_state` and a concurrent `clear = true` delegate
/// (`dispatch_one_owned`) both acquire before respawning — and builds the
/// restart call as a plain (unspawned) future it drives BY HAND: a manual
/// `poll` with a no-op waker, no `tokio::spawn`, so nothing here rests on the
/// OS scheduler happening to run a background task before or after this
/// test's own next line (a real, observed failure mode of an earlier version
/// of this test — spawning the restart and racing it against a real OS
/// thread let the restart's own early check sometimes run AFTER the
/// replacement was already installed, silently testing nothing). The first
/// poll must already return `Pending`: the handler's read-guard phase
/// (caller check, target resolution, the crash check itself, the role config
/// lookup) contains no other `.await` that can suspend on an uncontended
/// lock, so reaching `Pending` here is only possible via the SAME
/// `pane_dispatch_lock` this test holds — proof the crash check already ran
/// and captured `crashed == true` before anything else happens. Only then
/// does the test call the same `respawn_or_recreate_agent_for_pane` a
/// concurrent delegate's `dispatch_one_owned` would use, installing a
/// healthy `cat` replacement into the pane while still holding the lock, and
/// only after that does it release the lock and drive the restart future to
/// completion. The restart must refuse — report `restarted: false` with a
/// "has not crashed" error, the same wording the early check uses — rather
/// than proceed and kill the replacement.
#[tokio::test(flavor = "multi_thread")]
#[spec("pane/restart/010")]
async fn pane_restart_010_recheck_under_dispatch_lock_prevents_killing_a_concurrent_replacement() {
    use std::future::Future;
    use std::task::{Context, Waker};

    let fx = fixture("sleep 0.2").await;

    let crashed = wait_for_crashed(
        &fx.daemon.registry,
        &fx.worker_agent_id,
        Duration::from_secs(5),
    )
    .await;
    assert!(
        crashed,
        "precondition: the worker stand-in never got marked crashed; record = {:?}",
        fx.daemon.registry.agent_record_any(&fx.worker_agent_id)
    );

    // Hold the same dispatch lock the handler and a concurrent delegate both
    // acquire before respawning, so everything below is ordered by the lock
    // rather than by timing.
    let dispatch_mutex = fx.daemon.registry.pane_dispatch_lock(WORKER_PANE);
    let dispatch_guard = dispatch_mutex.lock().await;

    let signal = RestartRoleSignal {
        pane_id: ORCH_PANE.to_string(),
        role: WORKER_ROLE.to_string(),
        force: false,
        timestamp: chrono::Utc::now(),
        token: None,
    };
    let mut restart_future = Box::pin(dot_agent_deck::state::handle_restart_role_with_state(
        signal,
        &fx.daemon.state,
        &fx.daemon.registry,
        &fx.daemon.event_tx,
    ));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(
        restart_future.as_mut().poll(&mut cx).is_pending(),
        "the first poll must be Pending — nothing else in the read-guard phase has an await \
         point that can suspend on an uncontended lock, so this is consistent with (not an \
         independent proof of) blocking on the dispatch lock this test holds; if it completed \
         (or needed a second poll) here, its early check never ran against the crashed state \
         this test is trying to pin, and the rest of this test would be racing nothing"
    );

    // Still holding the lock: simulate exactly what a concurrent
    // `clear = true` delegate's `dispatch_one_owned` does on the SAME lock in
    // production — install a healthy replacement via
    // `respawn_or_recreate_agent_for_pane`. The crashed worker's registry
    // record is still present (never closed), so this must resolve as an
    // ordinary respawn (`recreated == false`), not the recreate leg — the
    // identity fields below are therefore unused by this call and are filled
    // only for the type to construct.
    let recreate_identity = PaneRecreateIdentity {
        cwd: None,
        display_name: Some(WORKER_ROLE.to_string()),
        tab_membership: None,
        agent_type: None,
        env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), WORKER_PANE.to_string())],
    };
    let replacement = fx
        .daemon
        .registry
        .respawn_or_recreate_agent_for_pane(WORKER_PANE, "cat", &recreate_identity)
        .await
        .expect(
            "the simulated concurrent delegate must be able to install a healthy replacement \
             while the restart is queued behind the same dispatch lock",
        );
    assert!(
        !replacement.recreated,
        "the crashed worker's registry record was still present, so this must be an ordinary \
         respawn (recreated == false) — the same leg `dispatch_one_owned`'s common case takes; \
         replacement = {replacement:?}"
    );
    let replacement_agent_id = replacement.agent_id;

    // Release the lock: only now can the restart future's own
    // `pane_dispatch_lock` acquisition succeed and its respawn attempt run.
    drop(dispatch_guard);

    let response = restart_future.await;

    assert!(
        !response.restarted,
        "TOCTOU: the restart proceeded and reported restarted: true even though a healthy \
         replacement was installed into the pane while the restart was queued behind the \
         dispatch lock, without --force ever having been given; response = {response:?}"
    );
    assert!(
        response
            .error
            .as_deref()
            .is_some_and(|e| e.contains("has not crashed")),
        "the refusal must re-check crash state under the dispatch lock and give the same \
         \"has not crashed\" wording the early check uses; response = {response:?}"
    );

    let occupant = fx.daemon.registry.pane_current_agent_id(WORKER_PANE);
    assert_eq!(
        occupant.as_deref(),
        Some(replacement_agent_id.as_str()),
        "the healthy replacement installed by the simulated concurrent delegate must still be \
         the pane's live occupant afterward — proof the restart did not kill it; \
         occupant = {occupant:?}, replacement = {replacement_agent_id}"
    );
    assert!(
        fx.daemon
            .registry
            .agent_record_any(&replacement_agent_id)
            .is_some_and(|r| r.crashed != Some(true)),
        "the healthy replacement must still be alive (not marked crashed) after the restart \
         resolved; record = {:?}",
        fx.daemon.registry.agent_record_any(&replacement_agent_id)
    );
}

/// Scenario: the same lock-holding + `respawn_or_recreate_agent_for_pane`
/// simulation technique as `pane_restart_010` — a concurrent `clear = true`
/// delegate installs a healthy replacement into the pane while a queued
/// restart is blocked behind `pane_dispatch_lock` — except this restart
/// carries `force: true`. Every existing `--force` test (`pane_restart_003`)
/// predates this fix and only proves force bypasses the ORIGINAL early
/// check; this pins that force also bypasses the NEW post-dispatch-lock
/// recheck added by that fix. `--force` means "restart regardless of crash
/// state," so the restart must proceed and respawn even though the pane's
/// occupant is no longer crashed by the time the lock is held, replacing the
/// just-installed replacement rather than refusing.
#[tokio::test(flavor = "multi_thread")]
#[spec("pane/restart/011")]
async fn pane_restart_011_force_bypasses_the_post_dispatch_lock_recheck_too() {
    use std::future::Future;
    use std::task::{Context, Waker};

    let fx = fixture("sleep 0.2").await;

    let crashed = wait_for_crashed(
        &fx.daemon.registry,
        &fx.worker_agent_id,
        Duration::from_secs(5),
    )
    .await;
    assert!(
        crashed,
        "precondition: the worker stand-in never got marked crashed; record = {:?}",
        fx.daemon.registry.agent_record_any(&fx.worker_agent_id)
    );

    // Hold the same dispatch lock the handler and a concurrent delegate both
    // acquire before respawning, so everything below is ordered by the lock
    // rather than by timing.
    let dispatch_mutex = fx.daemon.registry.pane_dispatch_lock(WORKER_PANE);
    let dispatch_guard = dispatch_mutex.lock().await;

    let signal = RestartRoleSignal {
        pane_id: ORCH_PANE.to_string(),
        role: WORKER_ROLE.to_string(),
        force: true,
        timestamp: chrono::Utc::now(),
        token: None,
    };
    let mut restart_future = Box::pin(dot_agent_deck::state::handle_restart_role_with_state(
        signal,
        &fx.daemon.state,
        &fx.daemon.registry,
        &fx.daemon.event_tx,
    ));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(
        restart_future.as_mut().poll(&mut cx).is_pending(),
        "the first poll must be Pending — nothing else in the read-guard phase has an await \
         point that can suspend on an uncontended lock, so this is consistent with (not an \
         independent proof of) blocking on the dispatch lock this test holds; if it completed \
         (or needed a second poll) here, the rest of this test would be racing nothing"
    );

    // Still holding the lock: simulate exactly what a concurrent
    // `clear = true` delegate's `dispatch_one_owned` does on the SAME lock in
    // production — install a healthy replacement via
    // `respawn_or_recreate_agent_for_pane`. The crashed worker's registry
    // record is still present (never closed), so this must resolve as an
    // ordinary respawn (`recreated == false`), not the recreate leg — the
    // identity fields below are therefore unused by this call and are filled
    // only for the type to construct.
    let recreate_identity = PaneRecreateIdentity {
        cwd: None,
        display_name: Some(WORKER_ROLE.to_string()),
        tab_membership: None,
        agent_type: None,
        env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), WORKER_PANE.to_string())],
    };
    let replacement = fx
        .daemon
        .registry
        .respawn_or_recreate_agent_for_pane(WORKER_PANE, "cat", &recreate_identity)
        .await
        .expect(
            "the simulated concurrent delegate must be able to install a healthy replacement \
             while the restart is queued behind the same dispatch lock",
        );
    assert!(
        !replacement.recreated,
        "the crashed worker's registry record was still present, so this must be an ordinary \
         respawn (recreated == false) — the same leg `dispatch_one_owned`'s common case takes; \
         replacement = {replacement:?}"
    );
    let replacement_agent_id = replacement.agent_id;

    // Release the lock: only now can the restart future's own
    // `pane_dispatch_lock` acquisition succeed and its respawn attempt run.
    drop(dispatch_guard);

    let response = restart_future.await;

    assert!(
        response.restarted,
        "force must restart the pane regardless of crash state, even though the concurrent \
         replacement installed while this restart was queued is no longer crashed; \
         response = {response:?}"
    );
    assert!(
        response.error.is_none(),
        "a successful forced restart must carry no error; response = {response:?}"
    );

    let occupant = fx.daemon.registry.pane_current_agent_id(WORKER_PANE);
    assert_ne!(
        occupant.as_deref(),
        Some(replacement_agent_id.as_str()),
        "force must replace the concurrent delegate's healthy replacement with a freshly \
         spawned agent, not leave it in place; occupant = {occupant:?}, \
         replacement = {replacement_agent_id}"
    );
}

/// The pointer line `dispatch_one_owned` writes into the `coder` worker's PTY.
const POINTER: &[u8] = b"Read .dot-agent-deck/worker-task-coder.md for your task.";

/// Count `POINTER` in whatever agent currently owns the worker pane.
fn pointers_in_worker_pane(registry: &AgentPtyRegistry) -> usize {
    registry
        .pane_current_agent_id(WORKER_PANE)
        .and_then(|id| registry.snapshot(&id).ok())
        .unwrap_or_default()
        .windows(POINTER.len())
        .filter(|w| *w == POINTER)
        .count()
}

/// Poll until the worker pane's current agent has received at least `count`
/// pointers, or `timeout` passes. Returns whether it got there.
async fn wait_for_pointers(registry: &AgentPtyRegistry, count: usize, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if pointers_in_worker_pane(registry) >= count {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn delegate_to_worker(fx: &Fixture, supersede: bool) -> DelegateResponse {
    let signal = DelegateSignal {
        pane_id: ORCH_PANE.to_string(),
        task: "Probe task for the busy-worker refusal.".to_string(),
        to: vec![WORKER_ROLE.to_string()],
        supersede,
        timestamp: chrono::Utc::now(),
        token: None,
    };
    fx.daemon
        .state
        .read()
        .await
        .handle_delegate_with_state(
            signal,
            &fx.daemon.registry,
            &fx.daemon.event_tx,
            Some(&fx.daemon.state),
        )
        .await
}

/// Scenario: the orchestrator delegates to a healthy `cat` worker, waits for the
/// task pointer to land, and delegates to it again before any work-done — that
/// second delegate must be refused as busy and never reach the pane (issue
/// #580). The orchestrator then cancels the task with `pane restart --force`,
/// and a plain delegate to the restarted role must go through, because the
/// restart retired the commission only the replaced agent could have answered
/// (issue #590).
#[tokio::test(flavor = "multi_thread")]
#[spec("pane/restart/012")]
async fn pane_restart_012_force_restart_cancels_the_task_a_busy_refusal_names() {
    let fx = fixture("cat").await;
    // `clear = false`, so a delegate writes into the live `cat` rather than
    // respawning it and waiting out a `SessionStart` a `cat` never sends. The
    // config is read on every delegate, so rewriting it here is enough.
    std::fs::write(
        fx._dir.path().join(".dot-agent-deck.toml"),
        format!("{}clear = false\n", config("cat")),
    )
    .expect("rewrite the orchestration config with a clear = false worker");

    let first = delegate_to_worker(&fx, false).await;
    assert_eq!(
        first.delivered,
        vec![WORKER_ROLE.to_string()],
        "the first delegate to an idle worker must be dispatched; response = {first:?}"
    );
    assert!(
        wait_for_pointers(&fx.daemon.registry, 1, Duration::from_secs(20)).await,
        "precondition: the first task pointer never reached the worker"
    );
    // The pty echoes the line and `cat` writes it back, so one delivery can
    // show the pointer more than once. Let it settle and compare against that.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let delivered_once = pointers_in_worker_pane(&fx.daemon.registry);

    let refused = delegate_to_worker(&fx, false).await;
    assert!(
        refused.delivered.is_empty(),
        "a delegate to a worker that still owes a work-done must not be dispatched; \
         response = {refused:?}"
    );
    assert_eq!(
        refused
            .busy
            .iter()
            .map(|b| b.role.as_str())
            .collect::<Vec<_>>(),
        vec![WORKER_ROLE],
        "the busy worker must be named; response = {refused:?}"
    );
    assert!(
        refused
            .error
            .as_deref()
            .is_some_and(|e| e.contains("--supersede")),
        "the refusal must be an error naming the remedy; response = {refused:?}"
    );
    // Give a (wrongly) dispatched second pointer the time the first one took
    // and then some, before asserting it never came.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        pointers_in_worker_pane(&fx.daemon.registry),
        delivered_once,
        "the refused delegate must not have written a second pointer into the busy pane"
    );

    let restarted = restart_role(&fx, ORCH_PANE, WORKER_ROLE, true).await;
    assert!(
        restarted.restarted,
        "precondition: the forced restart must succeed; response = {restarted:?}"
    );

    let after_restart = delegate_to_worker(&fx, false).await;
    assert_eq!(
        after_restart.delivered,
        vec![WORKER_ROLE.to_string()],
        "after `pane restart --force` the role takes a plain delegate again — the cancelled \
         task's commission went with the agent that owed it; response = {after_restart:?}"
    );
    assert!(
        after_restart.busy.is_empty() && after_restart.superseded.is_empty(),
        "nothing was outstanding to refuse or supersede; response = {after_restart:?}"
    );
    assert!(
        wait_for_pointers(&fx.daemon.registry, 1, Duration::from_secs(20)).await,
        "the delegate after the restart must reach the replacement agent"
    );
}
