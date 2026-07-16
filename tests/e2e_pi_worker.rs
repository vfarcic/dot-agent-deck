#![cfg(feature = "e2e")]

//! L2 REAL-`pi` WORKER proof (PRD #201 completeness — pi's second role).
//!
//! The existing pi e2es prove pi as an ORCHESTRATOR (`chain-smoke/pi/001`
//! headless, `pi/live/002` live, both native-seeded). None prove pi as a
//! **WORKER**: a real pi receiving its worker task NATIVELY, doing the work, and
//! signalling `work-done` back. "Pi is a first-class agent" should mean BOTH
//! roles work; this test pins the worker half.
//!
//! ## The worker chain under test (all four asserted)
//! 1. The pi worker **receives its task NATIVELY** (PRD #201) — on a `clear =
//!    true` delegate the daemon RESPAWNS the worker, stashes the
//!    `worker-task-<role>.md` pointer as the pane's seed, and the respawned pi's
//!    extension pulls it on `session_start` via `dot-agent-deck get-seed` →
//!    `pi.sendUserMessage` (NOT PTY keystroke injection). The daemon records the
//!    delivery as native; the test asserts that flag.
//! 2. The pi worker **does the task** — creates a uniquely-named sentinel file
//!    with known contents.
//! 3. The pi worker **signals `work-done`** — the daemon writes
//!    `.dot-agent-deck/work-done-<role>.md`. Whether pi shells the footer's
//!    `dot-agent-deck work-done` CLI or calls its extension's native `work_done`
//!    tool, both route the same `WorkDone` signal over the hook socket, so the
//!    file's appearance is a path-agnostic proof.
//! 4. The delivery was **native, not the injection fallback** — the daemon's
//!    per-pane `seed_delivered_native` flag is set only by the `get-seed` pull.
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
//! ## `clear = true` + native delivery dissolves the old 10s-fallback fragility
//! Workers default `clear = true`, so each delegate RESPAWNS the worker. The OLD
//! injection path then waited `SESSION_START_WAIT` (~10s) before typing the task
//! into the pane — and because pi maps `session_start → waiting` (it never emits
//! `EventType::SessionStart`), that wait always burned the full fallback, so a
//! slow pi boot could land the injected task before pi was input-ready. NATIVE
//! delivery (PRD #201) removes that entirely: the daemon stashes the task as the
//! respawned pane's seed and pi's extension pulls it on `session_start` via
//! `get-seed` → `sendUserMessage` ("always triggers a turn") — deterministic, no
//! keystroke timing. This test therefore exercises the real `clear = true`
//! respawn path (a `.dot-agent-deck.toml` `coder` role whose command is `pi` and
//! whose `clear` defaults to `true`) and proves the task arrives NATIVELY.
//!
//! The daemon's PTY-injection SAFETY NET (which delivers if the native pull never
//! comes) is deferred out of the test window via `DOT_AGENT_DECK_SEED_FALLBACK_SECS`
//! set high, so the ONLY route that can deliver in-window is the native pull —
//! making the `seed_delivered_native` assertion a clean native-vs-fallback
//! discriminator. A `clear = false` pi worker (no respawn → no `session_start`)
//! still uses the legacy injection; that mid-session case is a documented further
//! enhancement, out of scope here.
//!
//! ## Reuse of the real-pi machinery (from `e2e_pi_orchestrator.rs`)
//! An in-process daemon (`common::spawn_inprocess_daemon`, whose hook loop ingests
//! `work-done` over the socket and re-broadcasts), and a REAL `pi` worker PTY whose
//! HOME carries the bundled extension, staged into the location pi resolves from
//! that HOME (production materializes it at daemon startup; this in-process daemon
//! bypasses that entry, so the test stages it) — spawned with `--provider openrouter --model
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

/// Scenario: Bring up an in-process daemon, write a `.dot-agent-deck.toml`
/// declaring a `coder` role whose command is a REAL `pi` and whose `clear`
/// defaults to `true`, and spawn a REAL `pi` worker pane (IDLE) as that role. The
/// worker's HOME carries the bundled extension, staged (as the daemon-startup
/// seam would) into the location pi resolves from that HOME before pi boots.
/// Register the orchestration maps (a synthetic,
/// un-spawned orchestrator pane plus the real pi worker pane, sharing the
/// `pi-worker-orchestration` + cwd). Then, via the deterministic
/// synthetic-delegate path, call `handle_delegate` with a `DelegateSignal` from
/// the synthetic orchestrator delegating to the `coder` role a task to create the
/// uniquely-named sentinel `pi_worker_sentinel_9d2e.txt` (contents
/// `PI_WORKER_SENTINEL_OK`). Because the role config's `clear = true`, the daemon
/// RESPAWNS the worker and delivers the task NATIVELY (PRD #201): it stashes the
/// `worker-task-coder.md` pointer as the respawned pane's seed and pi's extension
/// pulls it on `session_start` via `dot-agent-deck get-seed` →
/// `pi.sendUserMessage` — NOT PTY keystroke injection (the injection safety net
/// is deferred out of the window via `DOT_AGENT_DECK_SEED_FALLBACK_SECS`). Assert
/// the full WORKER chain: the pi worker created the sentinel with the expected
/// contents (received AND did the task), the daemon wrote
/// `.dot-agent-deck/work-done-coder.md` (signalled work-done — footer CLI or the
/// native `work_done` tool), AND the daemon recorded the seed was delivered
/// NATIVELY (`seed_delivered_native`, proving the native pull ran, not the
/// injection fallback). Generous per-step timeouts sized to confidence, not token
/// cost (Design Decision #7). HEADLESS — a functional proof, not a reel clip.
#[spec("chain-smoke/pi/002")]
#[test]
fn chain_smoke_pi_002_worker_receives_delegate_and_signals_work_done() {
    skip_unless!(check_pi_available());
    // PRD #201: defer the daemon's PTY-injection safety net far past the test
    // window so the ONLY route that can deliver the task in-window is the native
    // `get-seed` pull — making the `seed_delivered_native` assertion a clean
    // native-vs-fallback discriminator (the fallback can't race in and win).
    //
    // SAFETY: set here, at the very top of the sync test entry point — BEFORE the
    // tokio runtime (and therefore any daemon worker thread) is created below —
    // so no concurrent `getenv` can race this `setenv`. nextest runs each test in
    // its own process, so this never leaks to another test.
    unsafe {
        std::env::set_var(
            dot_agent_deck::agent_pty::DOT_AGENT_DECK_SEED_FALLBACK_SECS,
            "600",
        );
    }
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
    // construction.
    let cwd = common::race_safe_tempdir();
    let cwd_str = cwd
        .path()
        .to_str()
        .expect("orchestration cwd is UTF-8")
        .to_string();
    let path_env = path_with_binary_dir();

    // PRD #201: a `.dot-agent-deck.toml` whose `coder` role runs the REAL `pi`
    // with `clear = true` (the default). `handle_delegate`'s role lookup finds
    // it, so the delegate takes the `clear = true` RESPAWN path — the respawned
    // pi's extension pulls the task NATIVELY via `get-seed`. The `orchestrator`
    // start role's command is a no-op (`true`): it is never spawned here (the
    // orchestrator pane is synthetic), it exists only so the config parses as a
    // well-formed orchestration with a start role.
    std::fs::write(
        cwd.path().join(".dot-agent-deck.toml"),
        format!(
            "[[orchestrations]]\n\
             name = \"pi-worker-orchestration\"\n\n\
             [[orchestrations.roles]]\n\
             name = \"orchestrator\"\n\
             command = \"true\"\n\
             start = true\n\n\
             [[orchestrations.roles]]\n\
             name = \"{WORKER_ROLE}\"\n\
             command = \"pi --provider openrouter --model {PI_MODEL} --approve\"\n\
             clear = true\n"
        ),
    )
    .expect("write worker orchestration .dot-agent-deck.toml");

    // --- WORKER: real pi whose HOME carries the bundled extension. In production
    // the extension is materialized ONCE at daemon startup
    // (`orchestrator_ext::auto_materialize`, from the `daemon serve` entry); this
    // in-process test daemon bypasses that entry, so we stage the extension here —
    // into the SAME location pi resolves from `pi_home` (`<home>/.pi/agent/
    // extensions/dot-agent-deck`) — mirroring the daemon-startup seam. `pi_home`
    // is propagated as the pi child's HOME via `opts.env` below, so the worker
    // loads our tools (incl. the native `work_done`) + status reporting exactly
    // like a production pi pane.
    let pi_home = common::race_safe_tempdir();
    dot_agent_deck::orchestrator_ext::materialize(
        &dot_agent_deck::orchestrator_ext::extension_dir_under(pi_home.path()),
    )
    .expect("stage the bundled pi extension into the worker HOME");

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

    // Spawn a cheap placeholder (`cat`, blocks on stdin) as the worker pane, NOT
    // pi. The `clear = true` delegate below RESPAWNS this pane into the REAL pi
    // (the toml `coder` role's command), reusing THIS spawn's captured env
    // (OPENROUTER_API_KEY + HOME etc.) — so the respawned pi is the one under
    // test and we pay for exactly one pi boot. The placeholder exists only to
    // give `respawn_agent_for_pane` a live target for `pane_id_env`.
    // The returned agent id is intentionally unused: the `clear = true` respawn
    // replaces this entry (and its id) with a fresh one, and the assertions
    // resolve the CURRENT worker agent by its stable `pane_id_env`.
    daemon
        .registry
        .spawn_agent(SpawnOptions {
            command: Some("cat"),
            cwd: Some(cwd_str.as_str()),
            rows: 40,
            cols: 120,
            env: pi_env,
            ..SpawnOptions::default()
        })
        .expect("spawn placeholder worker agent");

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

    // No readiness wait: the `cat` placeholder is a live registry entry the
    // instant `spawn_agent` returns, which is all `respawn_agent_for_pane` needs
    // as its target. Waiting for `cat` to "settle" would just burn the ceiling
    // (it emits no output), and the pane it produces is discarded by the respawn
    // anyway — the REAL pi boots fresh below, and only its `session_start` →
    // `get-seed` pull is what the assertions turn on.

    // --- Deterministic orchestrator side: a synthetic `DelegateSignal` routed
    // through the real `handle_delegate`. Directive task with the literal sentinel
    // tokens the worker must reproduce (robust to LLM phrasing). The task body
    // becomes the contents of `worker-task-coder.md`; the daemon appends the
    // work-done footer. Because the `coder` role is `clear = true`, the daemon
    // RESPAWNS the pi worker and stashes the single-line pointer as the fresh
    // pane's NATIVE seed (rather than typing it in) — pi's extension pulls it via
    // `get-seed` → `sendUserMessage`.
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

    // After a `clear = true` respawn the worker's registry AGENT id changes
    // (`worker_agent_id` above is now stale), so resolve the CURRENT agent for
    // WORKER_PANE (by its stable `pane_id_env`) for the pane-snapshot diagnostics.
    let current_worker_pane = || {
        daemon
            .registry
            .agent_records()
            .into_iter()
            .find(|r| r.pane_id_env.as_deref() == Some(WORKER_PANE))
            .and_then(|r| daemon.registry.snapshot(&r.id).ok())
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default()
    };

    // 1 + 2. The pi worker received the NATIVE task and did it: the sentinel
    //        exists with the expected contents (proves pi pulled the seed via
    //        get-seed, read its task file, and ran the work).
    let sentinel = cwd.path().join(SENTINEL_NAME);
    let sentinel_ok = common::wait_for_path_async(&sentinel, Duration::from_secs(240)).await;

    let worker_pane = current_worker_pane();

    // Self-diagnosing signals: did the delegated task text reach the pane, and did
    // pi surface an API/account error? Distinguishes "the model/quota is the
    // blocker" from "the pi-worker native-delivery path is broken".
    let worker_lower = worker_pane.to_lowercase();
    let task_reached = worker_pane.contains("worker-task-coder.md")
        || worker_lower.contains(&SENTINEL_NAME.to_lowercase());
    let api_errored = ["quota", "exceeded", "billing", "unauthorized", "rate limit"]
        .iter()
        .any(|k| worker_lower.contains(k));
    assert!(
        sentinel_ok,
        "the pi WORKER never created the sentinel {SENTINEL_NAME:?} within 240s. \
         task_reached_pane={task_reached} (if false, the native seed never reached the \
         respawned pi — a delivery failure, NOT the model). \
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
        current_worker_pane()
    );

    // 4. NATIVE-PATH PROOF (PRD #201, GAP-#1 discipline): the task arrived via the
    //    extension's `get-seed` pull → `pi.sendUserMessage`, NOT the PTY-injection
    //    safety net. `get-seed`'s take marks the delivery native, and the fallback
    //    was deferred 600s out of the window — so a completed chain can only mean
    //    the native pull delivered the seed. Assert the registry flag to make that
    //    explicit and bulletproof.
    assert!(
        daemon.registry.seed_delivered_native(WORKER_PANE),
        "the pi worker's task must have been delivered NATIVELY via `get-seed` → \
         `sendUserMessage` (the PTY-injection fallback was deferred out of the test \
         window), but the daemon did not record a native seed delivery for \
         {WORKER_PANE:?} — the respawned pi's session_start → get-seed pull did not run."
    );
}
