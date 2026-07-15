#![cfg(feature = "e2e")]

//! L2 REAL-`pi` orchestrator proof + scheduled/unattended Pi parity (PRD #201
//! M4.1 + M4.2, test-plan rows 13-14). This is the flagship: a REAL `pi` agent,
//! driving a real model, loads our bundled orchestrator extension, calls the
//! native `delegate` tool, the daemon routes the task to a REAL worker PTY, the
//! worker creates a uniquely-named sentinel file and signals `work-done`, and the
//! Pi pane's status is tracked entirely through the extension's `agent-event`
//! path with NO hook installed and no `~/.claude/settings.json` mutation.
//!
//! ## Why real agents (Design Decision #7)
//! The behavior under test IS real agent behavior: does a real orchestrator
//! *decide* to delegate, call the native tool correctly, and does the daemon
//! route that into a real worker that does the work and reports back. The
//! synthetic harness (`status/agent-event/003`, `orchestration/delegate/005`)
//! already pins the plumbing deterministically; this test pins the thing a
//! stand-in never can. Per Design Decision #7 the real-agent tier is sized to
//! CONFIDENCE, not token cost — generous per-step timeouts, a directive prompt,
//! and a distinctive sentinel — and is the flaky-tolerant pre-PR tier (rule 4/5),
//! run once, never looped.
//!
//! ## Structure (mirrors `e2e_delegate_work_done_chain.rs`)
//! An in-process daemon (`common::spawn_inprocess_daemon` — its hook loop ingests
//! `delegate` / `work-done` / `agent-event` over the socket and re-broadcasts
//! `AgentEvent`s), a REAL `claude` (Haiku) worker pane spawned + ready BEFORE the
//! orchestrator delegates (so the daemon-injected pointer lands in a live input
//! box), and a REAL `pi` orchestrator pane whose HOME carries the bundled
//! extension, staged into the location pi resolves from that HOME (production
//! materializes it at daemon startup; this in-process daemon bypasses that entry,
//! so the test stages it, PRD #201). The
//! ORCHESTRATOR role is swapped to `pi`; the worker stays a black-box `claude`
//! (its hooks/CLI are unchanged — the workaround-dissolution is Pi-only by
//! construction, Design Decision #4).
//!
//! All sleeping/polling lives in the `common` harness (Decision 21); the `#[spec]`
//! entry points are sync `#[test]`s that `block_on` an async body (the
//! linkage-check scanner links `#[spec]` to the next PLAIN `fn`, so an
//! `#[tokio::test] async fn` would misbind — same pattern as `session/live/007`).
//!
//! ## pi model + worker agent
//! - **Orchestrator:** real `pi` (0.80.6) via `--provider openrouter --model
//!   openai/gpt-5-nano` — the cheapest GPT-5.x tier that reliably tool-calls;
//!   `--approve` clears pi's project-local trust so the `delegate` tool executes
//!   without a permission prompt (pi is YOLO/no-permission by default). Confirmed
//!   out-of-band that gpt-5-nano loads the extension and calls `delegate` within
//!   ~10s. (`--thinking off` is rejected by this endpoint — "Reasoning is
//!   mandatory" — so thinking is left at the model default.)
//! - **Worker:** real `claude` Haiku (`claude-haiku-4-5-20251001`) — the proven
//!   chain-test worker; directive-following and cheap. Chosen over OpenCode
//!   because OpenCode's tool-permission sandbox gates `.dot-agent-deck` reads /
//!   shell runs (see the OpenCode arm of `e2e_delegate_work_done_chain.rs`),
//!   which would block the full work-done loop for reasons orthogonal to M4.1.
//!
//! ## Credentials (Design Decision #5, harness caveat)
//! pi authenticates to OpenRouter via `OPENROUTER_API_KEY` (vals-sourced in the
//! devbox shell). `AgentPtyRegistry::spawn_agent` inherits the parent env and
//! overlays `opts.env`; M4.1 EXPLICITLY propagates `OPENROUTER_API_KEY` + `HOME`
//! (+ the pane/socket/PATH vars) into the pi child's `opts.env`, and M4.2 passes
//! them into the daemon (which the scheduler-spawned pi inherits) — so pi starts
//! with the key regardless of how the runner scrubs the environment. The key is
//! NEVER printed (only checked non-empty for the runtime-skip).
//!
//! Tier: e2e (`#[cfg(feature = "e2e")]`) — spawns real agents, hits a real model.
//! Runtime-skipped (Decision 26) when `pi`/`claude`/credentials/`OPENROUTER_API_KEY`
//! are absent.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tempfile::TempDir;

use dot_agent_deck::agent_pty::{DOT_AGENT_DECK_AGENT_ID, DOT_AGENT_DECK_PANE_ID, SpawnOptions};
use dot_agent_deck::event::{AgentType, EventType};

mod common;

use spec::spec;

const ORCH_PANE: &str = "pi-orchestrator-pane";
const WORKER_PANE: &str = "worker-pane";
const WORKER_ROLE: &str = "coder";
const PINNED_CLAUDE_MODEL: &str = "claude-haiku-4-5-20251001";
/// Cheapest GPT-5.x tier on OpenRouter that reliably tool-calls (verified
/// out-of-band: loads the extension + calls `delegate` in ~10s).
const PI_MODEL: &str = "openai/gpt-5-nano";
/// The sentinel the worker must create. Distinctive + lands in a fresh tempdir
/// cwd, so it is unique by construction and unambiguous on the worker pane.
const SENTINEL_NAME: &str = "pi_orch_sentinel_7c3f.txt";
const SENTINEL_CONTENT: &str = "PI_ORCH_SENTINEL_OK";

// ---------------------------------------------------------------------------
// Non-polling helpers (kept in-file; all sleeping/polling is in `common`).
// ---------------------------------------------------------------------------

/// `pi` on PATH AND a non-empty `OPENROUTER_API_KEY`. The key is only checked
/// for presence — never printed or logged (it is a secret; the auto-mode
/// classifier blocks any substring of it).
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
        _ => Err(
            "OPENROUTER_API_KEY not set — real-pi orchestrator e2e needs OpenRouter auth".into(),
        ),
    }
}

/// Build an isolated `HOME` for the Claude worker that (a) carries the host
/// credentials/onboarding so auth works without a fresh login, and (b) pre-marks
/// `worker_cwd` as trusted so Claude's first-run "Is this a project you trust?"
/// dialog never appears and the injected delegate pointer lands in the input box
/// rather than being consumed answering the dialog. The returned TempDir must be
/// kept alive for the worker's lifetime. Ported from
/// `e2e_delegate_work_done_chain.rs::prepare_claude_home`.
fn prepare_claude_home(worker_cwd: &str) -> TempDir {
    let host_home = std::env::var("HOME").expect("HOME is set");
    let home = common::race_safe_tempdir();

    std::fs::create_dir_all(home.path().join(".claude")).expect("mk .claude");
    std::fs::copy(
        Path::new(&host_home)
            .join(".claude")
            .join(".credentials.json"),
        home.path().join(".claude").join(".credentials.json"),
    )
    .expect("copy claude credentials");

    let host_cfg_path = Path::new(&host_home).join(".claude.json");
    let mut cfg: serde_json::Value = std::fs::read_to_string(&host_cfg_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({ "hasCompletedOnboarding": true }));
    if !cfg["projects"].is_object() {
        cfg["projects"] = serde_json::json!({});
    }
    cfg["projects"][worker_cwd] = serde_json::json!({
        "hasTrustDialogAccepted": true,
        "hasCompletedProjectOnboarding": true,
        "projectOnboardingSeenCount": 1,
    });
    std::fs::write(
        home.path().join(".claude.json"),
        serde_json::to_string(&cfg).expect("serialize .claude.json"),
    )
    .expect("write isolated .claude.json");

    home
}

/// One `[[scheduled_tasks]]` block whose `command` launches a REAL `pi` (cheap,
/// non-interactive) with the bundled extension loaded from the daemon's HOME. The
/// cron never fires on its own (Jan 1 00:00) — the fire is driven by `RunNow`.
/// A tiny `-p` prompt keeps the model turn cheap; the status we observe
/// (session_start → waiting) fires on boot, before/independent of the turn.
fn pi_schedule_toml(working_dir: &str) -> String {
    format!(
        "[[scheduled_tasks]]\n\
         name = \"pi-unattended\"\n\
         cron = \"0 0 1 1 *\"\n\
         working_dir = \"{working_dir}\"\n\
         command = \"pi --provider openrouter --model {PI_MODEL} --approve -p ready\"\n\
         prompt = \"ready\"\n\
         enabled = true\n\n"
    )
}

/// The freshly-built test binary's dir, prepended to PATH so an agent's
/// `dot-agent-deck delegate` / `work-done` / `agent-event` resolves.
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
// M4.1 — real-pi orchestrator delegate → work-done (test-plan ROW 13)
// ---------------------------------------------------------------------------

/// Scenario: Bring up an in-process daemon, spawn a REAL Claude Code (Haiku)
/// worker as a long-running interactive `coder`-role pane and wait until it is
/// input-ready, then spawn a REAL `pi` orchestrator pane IDLE (no CLI-arg
/// prompt) whose HOME carries the bundled extension, staged (as the
/// daemon-startup seam would) into the location pi resolves from that HOME before
/// pi boots — and whose env explicitly propagates `OPENROUTER_API_KEY` + `HOME`.
/// Deliver pi's directive NATIVELY (PRD #201): stash it in the daemon seed store
/// (`set_pending_seed`), and pi's extension pulls it on `session_start` via
/// `dot-agent-deck get-seed` → `pi.sendUserMessage` (NOT a CLI arg, NOT PTY
/// keystroke injection). The directive tells pi to call the native `delegate`
/// tool once, delegating to the `coder` role a task that creates the
/// uniquely-named sentinel `pi_orch_sentinel_7c3f.txt` (contents
/// `PI_ORCH_SENTINEL_OK`). Assert the FULL chain: the sentinel file exists with
/// the expected contents (the real worker ran the delegated task via the daemon
/// route), the daemon wrote `.dot-agent-deck/work-done-coder.md` (work-done
/// returned to the orchestrator), a `Pi`-typed `AgentEvent` for the orchestrator
/// pane was observed on the daemon's broadcast (status via the extension's
/// `agent-event` path, NO hook), AND the daemon recorded that the seed was
/// delivered NATIVELY via the `get-seed` pull (proving the native path ran, not
/// an injection fallback).
#[spec("chain-smoke/pi/001")]
#[test]
fn chain_smoke_pi_001_orchestrator_delegates_to_real_worker() {
    skip_unless!(common::check_claude_available());
    skip_unless!(check_pi_available());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("build multi-thread runtime");
    rt.block_on(chain_smoke_pi_001_orchestrator_delegates_to_real_worker_inner());
}

async fn chain_smoke_pi_001_orchestrator_delegates_to_real_worker_inner() {
    let daemon = common::spawn_inprocess_daemon().await;

    // Shared orchestration cwd: the worker + orchestrator run here, and the
    // sentinel + work-done file land here. A fresh tempdir → the sentinel name
    // is unique by construction.
    let cwd = common::race_safe_tempdir();
    let cwd_str = cwd
        .path()
        .to_str()
        .expect("orchestration cwd is UTF-8")
        .to_string();
    let path_env = path_with_binary_dir();

    // --- WORKER: real Claude Haiku, spawned + ready before the orchestrator
    // delegates (so the daemon-injected pointer lands in a live input box).
    let claude_home = prepare_claude_home(&cwd_str);
    let worker_command =
        format!("claude --model {PINNED_CLAUDE_MODEL} --allowedTools Bash Read Write");
    let worker_env = vec![
        (DOT_AGENT_DECK_PANE_ID.to_string(), WORKER_PANE.to_string()),
        (
            "DOT_AGENT_DECK_SOCKET".to_string(),
            daemon.hook_path.display().to_string(),
        ),
        ("PATH".to_string(), path_env.clone()),
        (
            "HOME".to_string(),
            claude_home
                .path()
                .to_str()
                .expect("claude home UTF-8")
                .to_string(),
        ),
    ];
    let worker_agent_id = daemon
        .registry
        .spawn_agent(SpawnOptions {
            command: Some(worker_command.as_str()),
            cwd: Some(cwd_str.as_str()),
            rows: 40,
            cols: 120,
            env: worker_env,
            ..SpawnOptions::default()
        })
        .expect("spawn worker agent");

    // Register the orchestration maps `handle_delegate` / `handle_work_done`
    // read, exactly as StartAgent would for a live orchestration tab. Both panes
    // share one orchestration + cwd; only the orchestrator pane is in the
    // orchestrator set, so the orchestrator's `delegate` routes to the worker.
    {
        let mut st = daemon.state.write().await;
        st.pane_role_map
            .insert(ORCH_PANE.to_string(), "orchestrator".to_string());
        st.pane_role_map
            .insert(WORKER_PANE.to_string(), WORKER_ROLE.to_string());
        st.orchestrator_pane_ids.insert(ORCH_PANE.to_string());
        let orch = ("pi-orchestration".to_string(), cwd_str.clone());
        st.pane_orchestration_map
            .insert(ORCH_PANE.to_string(), orch.clone());
        st.pane_orchestration_map
            .insert(WORKER_PANE.to_string(), orch);
        st.pane_cwd_map
            .insert(WORKER_PANE.to_string(), cwd_str.clone());
        st.pane_cwd_map
            .insert(ORCH_PANE.to_string(), cwd_str.clone());
    }

    common::wait_until_agent_output_settled(
        &daemon.registry,
        &worker_agent_id,
        Duration::from_millis(1500),
        Duration::from_secs(45),
    )
    .await;

    // --- ORCHESTRATOR: real pi whose HOME carries the bundled extension. In
    // production the extension is materialized ONCE at daemon startup
    // (`orchestrator_ext::auto_materialize`, from the `daemon serve` entry); this
    // in-process test daemon bypasses that entry, so we stage the extension here —
    // into the SAME location pi resolves from `pi_home` — mirroring the
    // daemon-startup seam. `pi_home` is propagated as the pi child's HOME via
    // `opts.env` below.
    let pi_home = common::race_safe_tempdir();
    dot_agent_deck::orchestrator_ext::materialize(
        &dot_agent_deck::orchestrator_ext::extension_dir_under(pi_home.path()),
    )
    .expect("stage the bundled pi extension into the orchestrator HOME");

    // Collect the extension's `agent-event` broadcasts BEFORE spawning pi, so its
    // session_start (→ waiting) status report can't be missed.
    let event_log = common::BroadcastEventLog::start(&daemon.event_tx);

    // CRITICAL (harness caveat): explicitly propagate OPENROUTER_API_KEY + HOME
    // (+ pane/socket/PATH) into the pi child. Never print the key.
    let openrouter_key =
        std::env::var("OPENROUTER_API_KEY").expect("checked non-empty by check_pi_available");
    let pi_env = vec![
        (DOT_AGENT_DECK_PANE_ID.to_string(), ORCH_PANE.to_string()),
        (
            DOT_AGENT_DECK_AGENT_ID.to_string(),
            "pi-orchestrator-agent".to_string(),
        ),
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

    // Directive prompt: instruct pi to call `delegate` ONCE with role `coder` and
    // a task that creates the distinctive sentinel. Robust to LLM phrasing — the
    // sentinel filename + content are literal tokens the worker must reproduce.
    // PRD #201: this directive is delivered NATIVELY — NOT as a CLI arg. It is
    // stashed in the daemon seed store (`set_pending_seed`) and the pi extension
    // pulls it on `session_start` via `dot-agent-deck get-seed` →
    // `pi.sendUserMessage`, which "always triggers a turn". No PTY keystroke
    // injection, no CLI positional arg.
    let directive = format!(
        "You are the orchestrator in a dot-agent-deck orchestration. A worker role named \
         \"coder\" is available. Call the delegate tool EXACTLY ONCE with role \"coder\" and \
         this task text: Create a file named {SENTINEL_NAME} in the current working directory \
         whose entire contents are exactly the text {SENTINEL_CONTENT}. Use the Bash tool to \
         create it. Do NOT do the work yourself — only call the delegate tool, then stop."
    );
    // Spawn pi IDLE — NO CLI-arg prompt. The only route the directive can reach
    // pi is the native `get-seed` pull, so a successful delegate PROVES native
    // delivery (there is no injection fallback armed on this direct-registry
    // spawn path).
    let pi_command = format!("pi --provider openrouter --model {PI_MODEL} --approve");

    let orch_agent_id = daemon
        .registry
        .spawn_agent(SpawnOptions {
            command: Some(pi_command.as_str()),
            cwd: Some(cwd_str.as_str()),
            rows: 40,
            cols: 120,
            env: pi_env,
            ..SpawnOptions::default()
        })
        .expect("spawn pi orchestrator agent");

    // PRD #201 native prompt delivery: stash the directive as the orchestrator
    // pane's seed (keyed by DOT_AGENT_DECK_PANE_ID = ORCH_PANE). pi's extension
    // pulls it on `session_start` — well after this returns, since pi's Node/Bun
    // boot + extension load takes seconds.
    daemon.registry.set_pending_seed(ORCH_PANE, &directive);

    // --- Assert the FULL chain. ---

    // 1. The worker created the sentinel via the delegated task (proves: pi
    //    called `delegate` → daemon routed → worker ran the real task). Generous
    //    per-step timeout (Design Decision #7): pi boot + model + delegate, then
    //    the worker reads its task file and does the work.
    let sentinel = cwd.path().join(SENTINEL_NAME);
    let sentinel_ok = common::wait_for_path_async(&sentinel, Duration::from_secs(240)).await;

    let orch_pane =
        String::from_utf8_lossy(&daemon.registry.snapshot(&orch_agent_id).unwrap_or_default())
            .into_owned();
    let worker_pane = String::from_utf8_lossy(
        &daemon
            .registry
            .snapshot(&worker_agent_id)
            .unwrap_or_default(),
    )
    .into_owned();

    // Self-diagnosing signal: distinguish "the model/quota is the blocker" from
    // "the delegate path is broken".
    let orch_lower = orch_pane.to_lowercase();
    let worker_lower = worker_pane.to_lowercase();
    let api_errored = ["quota", "exceeded", "billing", "unauthorized", "rate limit"]
        .iter()
        .any(|k| orch_lower.contains(k) || worker_lower.contains(k));
    assert!(
        sentinel_ok,
        "the delegated worker never created the sentinel {SENTINEL_NAME:?} within 240s. \
         api_error_in_a_pane={api_errored} (if true, an account/quota is the blocker, not the \
         delegate path).\n=== pi orchestrator pane ===\n{orch_pane}\n\
         === worker pane ===\n{worker_pane}\n=== end ==="
    );

    let contents = std::fs::read_to_string(&sentinel).expect("read sentinel file");
    assert!(
        contents.contains(SENTINEL_CONTENT),
        "sentinel {SENTINEL_NAME:?} exists but does not contain {SENTINEL_CONTENT:?}; \
         got:\n{contents}"
    );

    // 2. work-done returned to the orchestrator: the daemon wrote the per-role
    //    summary file (handle_work_done), proving the worker ran
    //    `dot-agent-deck work-done` from its task-file footer and the daemon
    //    ingested it over the socket.
    let work_done = cwd
        .path()
        .join(".dot-agent-deck")
        .join(format!("work-done-{WORKER_ROLE}.md"));
    let work_done_ok = common::wait_for_path_async(&work_done, Duration::from_secs(120)).await;
    assert!(
        work_done_ok,
        "the worker never signalled work-done (no {work_done:?}). The sentinel was created, so \
         the worker ran, but the work-done CLI did not reach the daemon.\n\
         === worker pane ===\n{}\n=== end ===",
        String::from_utf8_lossy(
            &daemon
                .registry
                .snapshot(&worker_agent_id)
                .unwrap_or_default()
        )
    );

    // 3. The Pi pane's status was tracked via the extension's `agent-event` path
    //    (no hook): a `Pi`-typed AgentEvent for the orchestrator pane rode the
    //    daemon's broadcast. Match ONLY the extension's mapped states
    //    (WaitingForInput / Thinking / Idle from `agent-event --type
    //    waiting|running|finished`), NOT a `SessionStart` — so a Pi status here
    //    can ONLY have come from the real extension shelling `agent-event`, never
    //    a daemon-side `from_command` spawn-time guess. (This pane is spawned via
    //    the low-level registry, which does not synthesize a spawn-time event, but
    //    the filter makes the intent explicit and bulletproof.)
    let pi_status = event_log
        .wait_for(
            |e| {
                e.agent_type == AgentType::Pi
                    && e.pane_id.as_deref() == Some(ORCH_PANE)
                    && matches!(
                        e.event_type,
                        EventType::WaitingForInput | EventType::Thinking | EventType::Idle
                    )
            },
            Duration::from_secs(30),
        )
        .await;
    assert!(
        pi_status.is_some(),
        "no Pi-typed extension status (WaitingForInput/Thinking/Idle) was observed for the \
         orchestrator pane {ORCH_PANE:?} — the extension's `agent-event` status path did not \
         report (expected it to shell `dot-agent-deck agent-event` on session_start/agent_start).\n\
         === pi orchestrator pane ===\n{orch_pane}\n=== end ==="
    );

    // 4. NATIVE-PATH PROOF (PRD #201, GAP-#1 discipline): the directive was
    //    delivered by the extension's `get-seed` pull → `pi.sendUserMessage`,
    //    NOT by a CLI arg and NOT by PTY keystroke injection. `get-seed`'s take
    //    marks the delivery native, and there is no injection fallback on this
    //    direct-registry spawn — so a delegate could only have happened if the
    //    native pull delivered the seed. Asserting the registry flag makes that
    //    explicit and bulletproof.
    assert!(
        daemon.registry.seed_delivered_native(ORCH_PANE),
        "the pi orchestrator's directive must have been delivered NATIVELY via \
         `get-seed` → `sendUserMessage` (the pane was spawned idle with no CLI-arg \
         prompt and no PTY injection), but the daemon did not record a native seed \
         delivery for {ORCH_PANE:?} — the extension's session_start → get-seed pull \
         did not run."
    );
}

// ---------------------------------------------------------------------------
// M4.2 — scheduled real-pi, UNATTENDED status (test-plan ROW 14)
// ---------------------------------------------------------------------------

/// Scenario: Start the real `daemon serve` headlessly (no TUI client attached)
/// with a CLEAN HOME and register one enabled schedule whose command is a REAL
/// `pi` (cheap `-p`, cheap GPT-5.x). Propagate `OPENROUTER_API_KEY` + the
/// built-binary PATH into the daemon (which the scheduler-spawned pi inherits,
/// along with `HOME` + `DOT_AGENT_DECK_SOCKET`); the daemon-startup
/// auto-materialize puts the bundled extension into that HOME as the daemon
/// boots — no manual setup. Subscribe as an UNATTENDED `SubscribeEvents` consumer
/// and fire the schedule via `RunNow`. Assert the scheduled pi boots and its real
/// extension reports a `Pi`-typed `AgentEvent` that the daemon re-broadcasts on
/// the event stream — the real-agent, unattended, no-client status path (the
/// value M4.2 adds over the synthetic `status/agent-event/003`).
#[spec("scheduler/pi/001")]
#[test]
fn scheduler_pi_001_scheduled_unattended_status_via_extension() {
    skip_unless!(check_pi_available());

    let scratch = tempfile::tempdir().expect("scratch tempdir");
    let work = scratch.path().join("pi-work");
    std::fs::create_dir_all(&work).expect("create pi working_dir");

    let toml = pi_schedule_toml(&work.to_string_lossy());

    // The scheduler-spawned pi inherits the daemon's env: propagate the OpenRouter
    // key (never printed) and prepend the freshly-built binary dir to PATH so pi's
    // extension resolves `dot-agent-deck agent-event`. HOME + DOT_AGENT_DECK_SOCKET
    // are set by the daemon-serve harness and inherited by the child.
    let openrouter_key =
        std::env::var("OPENROUTER_API_KEY").expect("checked non-empty by check_pi_available");
    let path_env = path_with_binary_dir();

    let daemon = common::spawn_daemon_serve_with_env(
        Some(&toml),
        "0",
        &[
            ("OPENROUTER_API_KEY", openrouter_key.as_str()),
            ("PATH", path_env.as_str()),
        ],
    );

    // No manual `orchestrator_ext::materialize` here: this is a REAL
    // `daemon serve`, so the daemon-startup auto-materialize (PRD #201, the
    // `daemon serve` entry) fires as the daemon boots, materializing the bundled
    // extension into the daemon's HOME (`daemon.home`) — which the
    // scheduler-spawned pi inherits — BEFORE pi launches. The daemon's HOME
    // therefore starts CLEAN and this test exercises the production path, in
    // parity with the other real-daemon pi tests (pi/live/*). (The in-process
    // daemon tests — chain-smoke/pi/00{1,2} — bypass that entry, so they stage
    // the extension explicitly.)

    // Subscribe as an unattended consumer BEFORE firing, so the scheduled pi's
    // first status report can't be missed.
    let sub = daemon.subscribe_events();

    daemon
        .run_now("pi-unattended")
        .expect("run-now pi-unattended");

    // The real extension reports the Pi pane's status on boot (session_start →
    // waiting) and the daemon re-broadcasts it. Generous window to absorb the pi
    // spawn + Node/Bun boot + extension load, unattended.
    //
    // CRITICAL: match ONLY the extension's mapped states — WaitingForInput /
    // Thinking / Idle (the `agent-event --type waiting|running|finished`
    // mappings) — and explicitly NOT `SessionStart`. The scheduler's spawn path
    // (`surface_spawned_pane`) broadcasts a synthetic `SessionStart` carrying the
    // `from_command`-guessed `Pi` type the instant the pane spawns, BEFORE pi's
    // Node runtime boots or the extension loads. Matching that guess would be a
    // false pass (it proves nothing about the real extension). Only a
    // non-`SessionStart` Pi event can have originated from the real extension
    // shelling `dot-agent-deck agent-event` — which is exactly M4.2's claim.
    let ev = sub.wait_for(
        |e| {
            e.agent_type == AgentType::Pi
                && matches!(
                    e.event_type,
                    EventType::WaitingForInput | EventType::Thinking | EventType::Idle
                )
        },
        Duration::from_secs(90),
    );
    assert_eq!(
        ev.agent_type,
        AgentType::Pi,
        "a scheduled, unattended real pi must report a Pi-typed status via its extension \
         (WaitingForInput/Thinking/Idle from `agent-event`) — not merely the daemon's \
         spawn-time `SessionStart` guess — re-broadcast by the daemon with no TUI client attached"
    );
    assert_ne!(
        ev.event_type,
        EventType::SessionStart,
        "the observed Pi status must come from the extension's `agent-event`, not the \
         spawn-time `from_command` SessionStart guess"
    );

    drop(sub);
}
