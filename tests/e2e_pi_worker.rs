#![cfg(feature = "e2e")]

//! L2 REAL-`pi` WORKER proof (PRD #201 completeness — pi's second role).
//!
//! The existing pi e2es prove pi as an ORCHESTRATOR (`chain-smoke/pi/001`
//! headless, `pi/live/002` live + injection-seeded). None prove pi as a
//! **WORKER**: a real pi receiving the daemon-injected worker task, doing the
//! work, and signalling `work-done` back. "Pi is a first-class agent" should mean
//! BOTH roles work; this test pins the worker half.
//!
//! ## The worker chain under test (all three asserted)
//! 1. The pi worker **receives the daemon-injected task** — the agent-agnostic
//!    `worker-task-<role>.md` footer path: the daemon writes the task file and
//!    injects the single-line pointer via `write_to_pane_and_submit`, and pi
//!    AUTO-SUBMITS + acts on it (same injection primitive proven for the pi
//!    ORCHESTRATOR by `pi/live/002`, here exercised for the pi WORKER role).
//! 2. The pi worker **does the task** — creates a uniquely-named sentinel file
//!    with known contents.
//! 3. The pi worker **signals `work-done`** — the daemon writes
//!    `.dot-agent-deck/work-done-<role>.md`. Whether pi shells the footer's
//!    `dot-agent-deck work-done` CLI or calls its extension's native `work_done`
//!    tool, both route the same `WorkDone` signal over the hook socket, so the
//!    file's appearance is a path-agnostic proof.
//!
//! ## Orchestrator side: the deterministic synthetic-delegate path (chosen)
//! The thing under test is the pi WORKER. A real `claude`/`pi` orchestrator would
//! add orchestrator-LLM flakiness (does it decide to delegate, call the tool
//! correctly) WITHOUT adding to the worker proof — and the genuine real-pi-
//! orchestrator ⇄ real-worker mix is already pinned by `chain-smoke/pi/001` +
//! `pi/live/002`. So the orchestrator side here is the deterministic
//! `AppState::handle_delegate` call with a synthetic `DelegateSignal` (the exact
//! pattern of `e2e_delegate_work_done_chain.rs`), routing the delegate into the
//! real pi worker.
//!
//! ## Latency: `clear = false` sidesteps the 10s `SESSION_START_WAIT` fallback
//! Workers default `clear = true`, so in a real orchestration each delegate
//! RESPAWNS the worker and the daemon waits `SESSION_START_WAIT` (~10s) before
//! injecting — and because pi maps `session_start → waiting` (it never emits
//! `EventType::SessionStart`), that wait always burns the full fallback, so a slow
//! pi boot could land the injected task before pi is input-ready. This test
//! deliberately isolates the WORKER proof from that (separately-tracked) fragility
//! by using the `clear = false` path: with NO `.dot-agent-deck.toml` role config
//! in the worker cwd, `handle_delegate`'s role lookup returns `None`, so there is
//! NO respawn — the pi worker is spawned ONCE, and the delegate injects only after
//! we have polled it to genuine input-readiness (`wait_until_agent_output_settled`,
//! sized generously for pi's Bun/Node boot + extension load). Per the brief,
//! `clear = false` for the worker role is the sanctioned way to isolate this proof;
//! the `clear = true`-respawn + 10s-fallback fragility is out of scope here (it is
//! tracked for the companion PRD).
//!
//! ## Reuse of the real-pi machinery (from `e2e_pi_orchestrator.rs`)
//! An in-process daemon (`common::spawn_inprocess_daemon`, whose hook loop ingests
//! `work-done` over the socket and re-broadcasts), and a REAL `pi` worker PTY whose
//! HOME starts WITHOUT the extension — the deck's spawn-time auto-materialize
//! (the `spawn_agent` seam, PRD #201) puts the bundled extension into that HOME
//! before pi boots — spawned with `--provider openrouter --model
//! openai/gpt-5-nano --approve`. `OPENROUTER_API_KEY` + `HOME` (+ the pane/socket/
//! PATH vars) are explicitly propagated into the pi child's `opts.env`; the key is
//! NEVER printed (only checked non-empty for the runtime-skip). HEADLESS — a
//! functional proof, not a reel clip (the reel already showcases pi-as-orchestrator
//! via `pi/live/002`).
//!
//! All sleeping/polling lives in the `common` harness (Decision 21); the `#[spec]`
//! entry point is a sync `#[test]` that `block_on`s an async body (the
//! linkage-check scanner links `#[spec]` to the next PLAIN `fn`, so an
//! `#[tokio::test] async fn` would misbind — same pattern as `chain-smoke/pi/001`).
//!
//! Tier: e2e (`#[cfg(feature = "e2e")]`) — spawns a real agent, hits a real model.
//! Flaky-tolerant pre-PR tier (real LLM), run once, never looped. Runtime-skipped
//! (Decision 26) when `pi` / `OPENROUTER_API_KEY` is absent.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use dot_agent_deck::agent_pty::{DOT_AGENT_DECK_PANE_ID, SpawnOptions};
use dot_agent_deck::event::DelegateSignal;

mod common;

use spec::spec;

/// The synthetic orchestrator pane id. No agent is spawned for it — it exists
/// only in the orchestration maps so `handle_delegate` accepts the delegate as
/// coming from a registered orchestrator pane (same as
/// `e2e_delegate_work_done_chain.rs`).
const ORCH_PANE: &str = "synthetic-orchestrator-pane";
const WORKER_PANE: &str = "pi-worker-pane";
const WORKER_ROLE: &str = "coder";
/// Cheapest GPT-5.x tier on OpenRouter that reliably runs a directive turn (the
/// same model `chain-smoke/pi/001` / `pi/live/002` pin).
const PI_MODEL: &str = "openai/gpt-5-nano";
/// The sentinel the pi worker must create. Distinctive + lands in a fresh tempdir
/// cwd, so it is unique by construction and unambiguous. DISTINCT from the
/// `chain-smoke/pi/001` (`7c3f`), `pi/live/001` (`4b1a`), and `pi/live/002`
/// (`5e8c`) sentinels.
const SENTINEL_NAME: &str = "pi_worker_sentinel_9d2e.txt";
const SENTINEL_CONTENT: &str = "PI_WORKER_SENTINEL_OK";

// ---------------------------------------------------------------------------
// Non-polling helpers (kept in-file; all sleeping/polling is in `common`).
// ---------------------------------------------------------------------------

/// `pi` on PATH AND a non-empty `OPENROUTER_API_KEY`. The key is only checked for
/// presence — never printed or logged (it is a secret). Mirrors
/// `e2e_pi_orchestrator.rs::check_pi_available`.
fn check_pi_available() -> Result<(), String> {
    let ok = std::process::Command::new("pi")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        return Err("pi CLI not installed (could not invoke `pi --version`)".into());
    }
    match std::env::var("OPENROUTER_API_KEY") {
        Ok(k) if !k.trim().is_empty() => Ok(()),
        _ => Err("OPENROUTER_API_KEY not set — real-pi worker e2e needs OpenRouter auth".into()),
    }
}

/// The freshly-built test binary's dir, prepended to PATH so the pi worker's
/// `dot-agent-deck work-done` (footer CLI) / the extension's `work_done` tool
/// resolve. `CARGO_BIN_EXE_dot-agent-deck` is set by Cargo at integration-test
/// build time to the binary under test.
fn path_with_binary_dir() -> String {
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let bin_dir = Path::new(bin)
        .parent()
        .expect("test binary has a parent dir")
        .to_str()
        .expect("bin dir is UTF-8");
    format!("{bin_dir}:{}", std::env::var("PATH").unwrap_or_default())
}

// ---------------------------------------------------------------------------
// chain-smoke/pi/002 — real-pi WORKER receives a delegate → work-done
// ---------------------------------------------------------------------------

/// Scenario: Bring up an in-process daemon and spawn a REAL `pi` worker IDLE
/// (no CLI-arg prompt — it boots to an interactive input box) as a `coder`-role
/// worker pane whose env propagates `OPENROUTER_API_KEY` + `HOME` (+ the
/// pane/socket/PATH vars). The worker's HOME starts WITHOUT the extension; the
/// deck's spawn-time auto-materialize (the `spawn_agent` seam) detects the `pi`
/// command and puts the bundled orchestrator extension into that HOME before pi
/// boots. Register the orchestration maps (a synthetic, un-spawned orchestrator
/// pane plus the real pi worker pane, sharing one orchestration + cwd) and wait
/// until the pi worker is input-ready. Then, via the deterministic
/// synthetic-delegate path, call `handle_delegate` with a `DelegateSignal` from
/// the synthetic orchestrator delegating to the `coder` role a task to create the
/// uniquely-named sentinel `pi_worker_sentinel_9d2e.txt` (contents
/// `PI_WORKER_SENTINEL_OK`). Because no `.dot-agent-deck.toml` role config exists,
/// the delegate takes the `clear = false` no-respawn path and injects the
/// single-line worker-task pointer straight into the live pi pane. Assert the full
/// WORKER chain: the pi worker auto-submitted the injected pointer, read its task
/// file, created the sentinel with the expected contents (proves it received AND
/// did the task), and the daemon wrote `.dot-agent-deck/work-done-coder.md`
/// (proves it signalled work-done — via the footer CLI or the extension's native
/// `work_done` tool). Generous per-step timeouts sized to confidence, not token
/// cost (Design Decision #7). HEADLESS — a functional proof, not a reel clip.
#[spec("chain-smoke/pi/002")]
#[test]
fn chain_smoke_pi_002_worker_receives_delegate_and_signals_work_done() {
    skip_unless!(check_pi_available());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("build multi-thread runtime");
    rt.block_on(chain_smoke_pi_002_worker_receives_delegate_and_signals_work_done_inner());
}

async fn chain_smoke_pi_002_worker_receives_delegate_and_signals_work_done_inner() {
    let daemon = common::spawn_inprocess_daemon().await;

    // Shared orchestration cwd: the pi worker runs here, and the sentinel +
    // work-done file land here. A fresh tempdir → the sentinel name is unique by
    // construction, and there is NO `.dot-agent-deck.toml`, so the delegate's role
    // lookup returns `None` (⇒ `clear = false`, no respawn).
    let cwd = common::race_safe_tempdir();
    let cwd_str = cwd
        .path()
        .to_str()
        .expect("orchestration cwd is UTF-8")
        .to_string();
    let path_env = path_with_binary_dir();

    // --- WORKER: real pi whose HOME starts WITHOUT the extension. The deck's
    // spawn-time auto-materialize (PRD #201, `spawn_agent` seam) detects the
    // `pi` command and materializes the bundled extension into the HOME the pi
    // child inherits (here `pi_home`, propagated via `opts.env` below) BEFORE pi
    // boots — so the pi worker loads our tools (incl. the native `work_done`) +
    // status reporting exactly like a production pi pane, via the real
    // production flow rather than a hand-set-up shortcut. `pi_home` starts
    // clean; no manual `orchestrator_ext::materialize` call.
    let pi_home = common::race_safe_tempdir();

    // CRITICAL (harness caveat): explicitly propagate OPENROUTER_API_KEY + HOME
    // (+ pane/socket/PATH) into the pi child. Never print the key. The pi worker
    // runs `dot-agent-deck work-done` from its task-file footer (or the extension's
    // `work_done` tool), so it needs the built binary on PATH and the hook socket
    // via DOT_AGENT_DECK_SOCKET; DOT_AGENT_DECK_PANE_ID tags its work-done signal
    // with this worker pane (this is the minimal env the proven `claude` worker in
    // `e2e_delegate_work_done_chain.rs` uses, plus pi's HOME + OpenRouter key).
    let openrouter_key =
        std::env::var("OPENROUTER_API_KEY").expect("checked non-empty by check_pi_available");
    let pi_env = vec![
        (DOT_AGENT_DECK_PANE_ID.to_string(), WORKER_PANE.to_string()),
        (
            "DOT_AGENT_DECK_SOCKET".to_string(),
            daemon.hook_path.display().to_string(),
        ),
        ("PATH".to_string(), path_env),
        (
            "HOME".to_string(),
            pi_home.path().to_str().expect("pi home UTF-8").to_string(),
        ),
        ("OPENROUTER_API_KEY".to_string(), openrouter_key),
    ];

    // Spawn pi IDLE (no CLI-arg prompt) so it boots to an interactive input box —
    // the daemon-injected worker-task pointer is what seeds it, exactly as a
    // production worker pane is seeded.
    let pi_command = format!("pi --provider openrouter --model {PI_MODEL} --approve");
    let worker_agent_id = daemon
        .registry
        .spawn_agent(SpawnOptions {
            command: Some(pi_command.as_str()),
            cwd: Some(cwd_str.as_str()),
            rows: 40,
            cols: 120,
            env: pi_env,
            ..SpawnOptions::default()
        })
        .expect("spawn pi worker agent");

    // Register the orchestration maps `handle_delegate` / `handle_work_done` read,
    // exactly as StartAgent would for a live orchestration tab. The synthetic
    // orchestrator pane is registered but NOT spawned (it exists only so the
    // delegate is accepted as coming from an orchestrator pane); only the pi worker
    // is a real agent. Both share one orchestration + cwd, so the delegate routes
    // to the worker, and the worker's work-done file lands in the shared cwd.
    {
        let mut st = daemon.state.write().await;
        st.pane_role_map
            .insert(ORCH_PANE.to_string(), "orchestrator".to_string());
        st.pane_role_map
            .insert(WORKER_PANE.to_string(), WORKER_ROLE.to_string());
        st.orchestrator_pane_ids.insert(ORCH_PANE.to_string());
        let orch = ("pi-worker-orchestration".to_string(), cwd_str.clone());
        st.pane_orchestration_map
            .insert(ORCH_PANE.to_string(), orch.clone());
        st.pane_orchestration_map
            .insert(WORKER_PANE.to_string(), orch);
        st.pane_cwd_map
            .insert(WORKER_PANE.to_string(), cwd_str.clone());
        st.pane_cwd_map
            .insert(ORCH_PANE.to_string(), cwd_str.clone());
    }

    // Let the real pi worker reach genuine input-readiness BEFORE delegating, so
    // the daemon-injected pointer lands in a live input box (not mid-boot, where it
    // would be dropped). Generous ceiling for pi's Bun/Node boot + extension load
    // (Design Decision #7). This — not a fixed SESSION_START_WAIT — is what gates
    // the injection, since the `clear = false` path does no respawn/wait.
    common::wait_until_agent_output_settled(
        &daemon.registry,
        &worker_agent_id,
        Duration::from_millis(1500),
        Duration::from_secs(60),
    )
    .await;

    // --- Deterministic orchestrator side: a synthetic `DelegateSignal` routed
    // through the real `handle_delegate`. Directive task with the literal sentinel
    // tokens the worker must reproduce (robust to LLM phrasing). The task body
    // becomes the contents of `worker-task-coder.md`; the daemon appends the
    // work-done footer, and injects only the single-line pointer into the pi pane.
    let task = format!(
        "Create a file named {SENTINEL_NAME} in the current working directory whose entire \
         contents are exactly the text {SENTINEL_CONTENT}. Use your shell tool to create it. \
         That is the entire task — do not do anything else."
    );
    let signal = DelegateSignal {
        pane_id: ORCH_PANE.to_string(),
        task,
        to: vec![WORKER_ROLE.to_string()],
        timestamp: chrono::Utc::now(),
    };
    daemon
        .state
        .read()
        .await
        .handle_delegate(signal, &daemon.registry, &daemon.event_tx)
        .await;

    // --- Assert the FULL worker chain. ---

    // 1 + 2. The pi worker received the injected task and did it: the sentinel
    //        exists with the expected contents (proves pi auto-submitted the
    //        injected pointer, read its task file, and ran the work).
    let sentinel = cwd.path().join(SENTINEL_NAME);
    let sentinel_ok = common::wait_for_path_async(&sentinel, Duration::from_secs(240)).await;

    let worker_pane = String::from_utf8_lossy(
        &daemon
            .registry
            .snapshot(&worker_agent_id)
            .unwrap_or_default(),
    )
    .into_owned();

    // Self-diagnosing signals: did the injected pointer reach the pane, and did pi
    // surface an API/account error? Distinguishes "the model/quota is the blocker"
    // from "the pi-worker task-injection path is broken".
    let worker_lower = worker_pane.to_lowercase();
    let pointer_reached = worker_pane.contains("worker-task-coder.md");
    let api_errored = ["quota", "exceeded", "billing", "unauthorized", "rate limit"]
        .iter()
        .any(|k| worker_lower.contains(k));
    assert!(
        sentinel_ok,
        "the pi WORKER never created the sentinel {SENTINEL_NAME:?} within 240s. \
         injected_pointer_reached_pane={pointer_reached} (if false, the daemon-injected \
         worker-task pointer never rendered — a task-injection failure, NOT the model). \
         api_error_in_pane={api_errored} (if true, an account/quota is the blocker, not the \
         pi-worker path).\n=== pi worker pane ===\n{worker_pane}\n=== end ==="
    );

    let contents = std::fs::read_to_string(&sentinel).expect("read sentinel file");
    assert!(
        contents.contains(SENTINEL_CONTENT),
        "sentinel {SENTINEL_NAME:?} exists but does not contain {SENTINEL_CONTENT:?}; \
         got:\n{contents}"
    );

    // 3. The pi worker signalled work-done: the daemon wrote the per-role summary
    //    file (`handle_work_done`), proving the pi worker ran `dot-agent-deck
    //    work-done` from its task-file footer (or called the extension's native
    //    `work_done` tool) and the signal reached the daemon over the hook socket.
    let work_done = cwd
        .path()
        .join(".dot-agent-deck")
        .join(format!("work-done-{WORKER_ROLE}.md"));
    let work_done_ok = common::wait_for_path_async(&work_done, Duration::from_secs(120)).await;
    assert!(
        work_done_ok,
        "the pi worker never signalled work-done (no {work_done:?}). The sentinel was created, so \
         the pi worker received AND did the task, but the work-done signal did not reach the \
         daemon (neither the footer `dot-agent-deck work-done` CLI nor the extension's `work_done` \
         tool).\n=== pi worker pane ===\n{}\n=== end ===",
        String::from_utf8_lossy(
            &daemon
                .registry
                .snapshot(&worker_agent_id)
                .unwrap_or_default()
        )
    );
}
