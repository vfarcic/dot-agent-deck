#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]

//! Real Pi worker receives a clear=true delegate through its native inbox,
//! writes a sentinel, and reports work-done. A synthetic orchestrator routes
//! the task through the real daemon; only the worker spends a model credential.
//! Scratch HOME and PI_CODING_AGENT_DIR contain the bundled extension.

use std::time::{Duration, Instant};

use dot_agent_deck::agent_pty::{DOT_AGENT_DECK_PANE_ID, SpawnOptions};
use dot_agent_deck::event::DelegateSignal;

mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/pi_inbox_live.rs"]
mod live;

use spec::spec;

/// The synthetic orchestrator pane id. No agent is spawned for it — it exists
/// only in the orchestration maps so `handle_delegate` accepts the delegate as
/// coming from a registered orchestrator pane (same as
/// `e2e_delegate_work_done_chain.rs`).
const ORCH_PANE: &str = "synthetic-orchestrator-pane";
const WORKER_PANE: &str = "pi-worker-pane";
const WORKER_ROLE: &str = "coder";
/// The sentinel the pi worker must create. Distinctive + lands in a fresh tempdir
/// cwd, so it is unique by construction and unambiguous. DISTINCT from the
/// `chain-smoke/pi/001` (`7c3f`), `pi/live/001` (`4b1a`), and `pi/live/002`
/// (`5e8c`) sentinels.
const SENTINEL_NAME: &str = "pi_worker_sentinel_9d2e.txt";
const SENTINEL_CONTENT: &str = "PI_WORKER_SENTINEL_OK";

// ---------------------------------------------------------------------------
// chain-smoke/pi/002 — real-pi WORKER receives a delegate → work-done
// ---------------------------------------------------------------------------

/// Scenario: Configure a clear=true Pi worker, optionally delay its replacement boot by 35 seconds, and delegate a shell command appending to an absolute sentinel. The worker must stay free of not-armed notices, register before any doorbell intercept, acknowledge one delivery, append once and signal work-done; its default watcher argv remains intact.
#[spec("chain-smoke/pi/002")]
#[test]
fn chain_smoke_pi_002_worker_receives_delegate_and_signals_work_done() {
    skip_unless!(common::check_pi_available());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("build multi-thread runtime");
    rt.block_on(chain_smoke_pi_002_worker_receives_delegate_and_signals_work_done_inner());
}

async fn chain_smoke_pi_002_worker_receives_delegate_and_signals_work_done_inner() {
    let daemon = common::spawn_inprocess_daemon().await;
    let diagnostics = live::diagnostics_dir();
    eprintln!("chain-smoke Pi diagnostics: {}", diagnostics.display());
    let cli_observer = live::write_cli_observer(&diagnostics);

    // Shared orchestration cwd: the pi worker runs here, and the sentinel +
    // work-done file land here. A fresh tempdir → the sentinel name is unique by
    // construction.
    let cwd = common::race_safe_tempdir();
    let cwd_str = cwd
        .path()
        .to_str()
        .expect("orchestration cwd is UTF-8")
        .to_string();
    let sentinel = cwd.path().join(SENTINEL_NAME);
    let sentinel_arg = sentinel.to_string_lossy().replace('\'', "'\\''");
    let sentinel_command = format!("printf '%s' '{SENTINEL_CONTENT}' >> '{sentinel_arg}'");
    let path_env = common::path_with_binary_dir();

    // PRD #201: a `.dot-agent-deck.toml` whose `coder` role runs the REAL `pi`
    // clear=true exercises the replacement Pi process, seeded through its inbox.
    let pi_model = common::pi_test_model().cli_args();
    let boot_delay = std::env::var("DOT_AGENT_DECK_TEST_PI_BOOT_DELAY_SECS")
        .map(|value| value.parse::<u64>().expect("Pi boot delay must be seconds"))
        .unwrap_or(0);
    let worker_command = if boot_delay == 0 {
        format!("pi {pi_model} --approve")
    } else {
        format!("sleep {boot_delay}; exec pi {pi_model} --approve")
    };
    eprintln!("chain-smoke: real Pi replacement boot delay: {boot_delay}s");
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
             command = {worker_command:?}\n\
             agent = \"pi\"\n\
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
    common::import_pi_credentials(&pi_home.path().join(".pi/agent"))
        .expect("seed the Pi login into the worker's isolated agent dir");
    dot_agent_deck::orchestrator_ext::materialize(
        &dot_agent_deck::orchestrator_ext::extension_dir_under(pi_home.path()),
    )
    .expect("stage the bundled pi extension into the worker HOME");
    std::fs::write(
        pi_home
            .path()
            .join(".pi/agent/extensions/inbox-test-observer.ts"),
        include_str!("fixtures/pi-inbox/observe-native.ts"),
    )
    .unwrap();

    // CRITICAL (harness caveat): this spawn does not go through
    // `TuiDeckBuilder::launch` — it builds `SpawnOptions.env` for an in-process
    // daemon — so the pi child gets exactly this environment. Pi authenticates
    // from the login seeded into `PI_CODING_AGENT_DIR` above, or from the
    // provider's env key when `common::pi_test_model` names one
    // (`common::pi_env_key`). Explicitly propagate HOME (+ pane/socket/PATH)
    // into the pi child. Never print a key. The pi worker
    // runs `dot-agent-deck work-done` from its task-file footer (or the extension's
    // `work_done` tool), so it needs the built binary on PATH and the hook socket
    // via DOT_AGENT_DECK_SOCKET; DOT_AGENT_DECK_PANE_ID tags its work-done signal
    // with this worker pane (this is the minimal env the proven `claude` worker in
    // `e2e_delegate_work_done_chain.rs` uses, plus pi's HOME and agent dir).
    let mut pi_env = vec![
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
        (
            "PI_CODING_AGENT_DIR".to_string(),
            pi_home.path().join(".pi/agent").display().to_string(),
        ),
    ];
    pi_env.extend(common::pi_env_key());
    pi_env.extend([
        (
            "DOT_AGENT_DECK_BIN".into(),
            cli_observer.display().to_string(),
        ),
        (
            "DECK_TEST_REAL_BIN".into(),
            common::contained_deck_binary().display().to_string(),
        ),
        (
            "DECK_TEST_PI_TIMELINE".into(),
            diagnostics.join("timeline.jsonl").display().to_string(),
        ),
        (
            "DECK_TEST_PI_EVENTS".into(),
            diagnostics.join("native.jsonl").display().to_string(),
        ),
    ]);

    // Spawn a cheap placeholder (`cat`, blocks on stdin) as the worker pane, NOT
    // pi. The `clear = true` delegate below RESPAWNS this pane into the REAL pi
    // (the toml `coder` role's command), reusing THIS spawn's captured env
    // (HOME, PI_CODING_AGENT_DIR etc.) — so the respawned pi is the one under
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
        let orch = dot_agent_deck::state::OrchestrationIdentity {
            id: "orch-test-0".to_string(),
            name: "pi-worker-orchestration".to_string(),
        };
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
    // anyway. The real Pi boots fresh below and receives its task through
    // the bundled extension's inbox stream.

    // --- Deterministic orchestrator side: a synthetic `DelegateSignal` routed
    // through the real `handle_delegate`. Directive task with the literal sentinel
    // tokens the worker must reproduce (robust to LLM phrasing). The task body
    // becomes the contents of `worker-task-coder.md`; the daemon appends the
    // work-done footer. Because the `coder` role is `clear = true`, the daemon
    // RESPAWNS the pi worker and stashes the single-line pointer as the fresh
    // pane's NATIVE seed (rather than typing it in) — pi's extension pulls it via
    // the native inbox stream.
    // The closing clauses are load-bearing, not politeness. An earlier version
    // ended "That is the entire task — do not do anything else", which
    // CONTRADICTED the work-done footer the daemon appends below it: the worker
    // created the sentinel and then stopped, and assertion 3 timed out on a
    // signal the task text had just told it not to send. The pointer to "When
    // done" removes that conflict. The no-questions clause removes the other
    // observed stall — the footer asks the worker to invent a `<summary-slug>`,
    // and a small model answered by asking the user to supply one, parking the
    // turn forever ("provide the summary slug ... and I can write the report").
    // A third run composed the exact completion command but only offered to run it; offering to act is not asking a follow-up question, so the no-questions clause did not cover that conversational stall, and the no-offers clause now requires signalling before any reply while explicitly rejecting the "next step" framing and waiting for confirmation.
    // None of these clauses dictates HOW to signal, so the real footer stays under test.
    let task = format!(
        "Use your shell tool to run exactly this command: `{sentinel_command}`. \
         Preserve the literal path, including every leading dot. Do not cd, change the command \
         or substitute a file-writing tool. That is the only work to do — then follow the \"When done\" instructions below to \
         signal completion. Do not ask any follow-up questions before doing both steps: you \
         have everything you need, so choose any value the instructions leave up to you. Do \
         not stop or reply until you have followed the \"When done\" instructions yourself \
         and signalled completion. Never describe signalling as a next step, offer to do it \
         later, or wait for confirmation."
    );
    let signal = DelegateSignal {
        pane_id: ORCH_PANE.to_string(),
        task,
        to: vec![WORKER_ROLE.to_string()],
        supersede: false,
        timestamp: chrono::Utc::now(),
        token: None,
    };
    let delegated_at = Instant::now();
    let mut observations =
        GenerationSamples::start(daemon.registry.clone(), diagnostics.join("samples.jsonl"));
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
    //        exists with the expected contents (proves Pi received the inbox task and ran the work).
    // Poll the CONTENT, not existence: a shell redirect creates the file before
    // the write lands, so an existence wait can read it empty (issue #244).
    eprintln!("chain-smoke: waiting up to 120s for the exact sentinel path {sentinel:?}");
    let sentinel_result = common::wait_for_file_containing_async(
        &sentinel,
        SENTINEL_CONTENT,
        Duration::from_secs(120),
    )
    .await;

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
    if let Err(observed) = sentinel_result {
        panic!(
            "the pi WORKER never wrote {SENTINEL_CONTENT:?} into the sentinel {sentinel:?} \
             within 120s; observed: {observed}. \
             task_reached_pane={task_reached} (if false, the native seed never reached the \
             respawned pi — a delivery failure, NOT the model). \
             api_error_in_pane={api_errored} (if true, an account/quota is the blocker, not the \
             pi-worker path).\n=== pi worker pane ===\n{worker_pane}\n=== end ==="
        );
    }

    // 3. The pi worker signalled work-done: the daemon wrote the per-role summary
    //    file (`handle_work_done`), proving the pi worker ran `dot-agent-deck
    //    work-done` from its task-file footer (or called the extension's native
    //    `work_done` tool) and the signal reached the daemon over the hook socket.
    let work_done = cwd
        .path()
        .join(".dot-agent-deck")
        .join(format!("work-done-{WORKER_ROLE}.md"));
    eprintln!(
        "chain-smoke: exact sentinel verified at {sentinel:?}; waiting up to 60s for {work_done:?}"
    );
    let work_done_ok = common::wait_for_path_async(&work_done, Duration::from_secs(60)).await;
    assert!(
        work_done_ok,
        "the pi worker never signalled work-done (no {work_done:?}). The sentinel was created, so \
         the pi worker received AND did the task, but the work-done signal did not reach the \
         daemon (neither the footer `dot-agent-deck work-done` CLI nor the extension's `work_done` \
         tool).\n=== pi worker pane ===\n{}\n=== end ===",
        current_worker_pane()
    );

    let messages = daemon.registry.inbox().list(WORKER_PANE);
    let delivered = messages
        .iter()
        .find(|message| message.subject == "task from the orchestrator for coder")
        .expect("delegation remains in the Pi worker inbox history");
    assert_eq!(
        delivered.state,
        dot_agent_deck::inbox::StateName::Acknowledged,
        "{delivered:?}"
    );
    assert_eq!(
        delivered.ack_how,
        Some(dot_agent_deck::inbox::AckHow::ExtensionConfirmed),
        "{delivered:?}"
    );
    assert_eq!(delivered.deliveries, 1, "{delivered:?}");
    assert_eq!(
        std::fs::read_to_string(&sentinel).unwrap(),
        SENTINEL_CONTENT,
        "the append-only action sentinel must record exactly one execution"
    );
    assert!(
        delegated_at.elapsed() >= Duration::from_secs(boot_delay),
        "the real replacement must run after its configured boot delay"
    );
    let samples = observations.finish();
    assert!(
        samples.len() > 1,
        "current-generation observer must advance"
    );
    assert!(
        samples.iter().all(|s| s["notice"] != "not_armed"),
        "no not_armed through ack + work-done: {samples:?}"
    );
    let timeline = live::read_jsonl(&diagnostics.join("timeline.jsonl"));
    let id = delivered.id.as_str();
    let registration = timeline
        .iter()
        .position(|v| !v["registered"].is_null())
        .expect("real watcher registration observed");
    let fetches = timeline
        .iter()
        .enumerate()
        .filter(|(_, v)| v["doorbell"] == id)
        .collect::<Vec<_>>();
    for (position, fetch) in &fetches {
        assert!(
            registration < *position,
            "registration precedes intercept: {timeline:?}"
        );
        assert_eq!(
            fetch["fetchRx"].as_str().unwrap().parse::<u64>().unwrap(),
            timeline[registration]["registered"]["rx"].as_u64().unwrap()
        );
    }
    let possessed = timeline
        .iter()
        .position(|v| {
            v["possess"]["held"]
                .as_array()
                .is_some_and(|a| a.iter().any(|r| r["id"] == id))
        })
        .expect("native possession observed");
    let handed = timeline
        .iter()
        .position(|v| {
            v["handed"]
                .as_array()
                .is_some_and(|a| a.iter().any(|r| r["id"] == id))
        })
        .expect("native hand-over observed");
    let committed = timeline
        .iter()
        .position(|v| {
            v["committed"]
                .as_array()
                .is_some_and(|a| a.iter().any(|r| r["id"] == id))
        })
        .expect("native commit observed");
    assert!(
        possessed < handed && handed < committed,
        "possession -> handed -> commit: {timeline:?}"
    );
    assert!(
        timeline
            .iter()
            .filter(|v| v["verb"] == serde_json::json!(["inbox", "wait"]))
            .all(|v| v["doorbellOnly"] == false),
        "default watcher argv intact"
    );
    let users = live::read_jsonl(&diagnostics.join("native.jsonl"))
        .into_iter()
        .filter(|v| {
            v["type"] == "message_start"
                && v["event"]["message"]["role"] == "user"
                && v["event"].to_string().contains(id)
        })
        .collect::<Vec<_>>();
    assert_eq!(users.len(), 1, "one tested native opener: {users:?}");
    std::fs::write(
        diagnostics.join("pane.txt"),
        common::redact_credentials_for_output(&current_worker_pane()),
    )
    .unwrap();
    std::fs::write(diagnostics.join("summary.json"),serde_json::json!({"boot_delay":boot_delay,"elapsed_ms":delegated_at.elapsed().as_millis(),"intercepts":fetches.len(),"delivery":delivered,"samples":samples.len()}).to_string()).unwrap();
    eprintln!(
        "chain-smoke: samples={} intercepted_fetches={} diagnostics={}",
        samples.len(),
        fetches.len(),
        diagnostics.display()
    );
    eprintln!("chain-smoke: acknowledged=1 acted=1 deliveries=1 boot_delay={boot_delay}s");
}

/// Observe the replacement by stable pane id, including the boot interval.
/// Drop joins the sampler before the daemon tears down, even on assertion panic.
struct GenerationSamples {
    stop: std::sync::mpsc::Sender<()>,
    worker: Option<std::thread::JoinHandle<Vec<serde_json::Value>>>,
}
impl GenerationSamples {
    fn start(
        registry: std::sync::Arc<dot_agent_deck::agent_pty::AgentPtyRegistry>,
        path: std::path::PathBuf,
    ) -> Self {
        let (stop, halted) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            use std::io::Write;
            let start = Instant::now();
            let mut samples = Vec::new();
            let mut file = std::fs::File::create(path).unwrap();
            loop {
                let current = registry
                    .agent_records()
                    .into_iter()
                    .find(|r| r.pane_id_env.as_deref() == Some(WORKER_PANE));
                let notice = current
                    .as_ref()
                    .and_then(|r| registry.inbox().integration_notice(WORKER_PANE, &r.id));
                let sample = serde_json::json!({"elapsed_ms":start.elapsed().as_millis(),"agent":current.as_ref().map(|r|&r.id),"notice":notice,"messages":registry.inbox().list(WORKER_PANE)});
                writeln!(
                    file,
                    "{}",
                    common::redact_credentials_for_output(&sample.to_string())
                )
                .unwrap();
                samples.push(sample);
                match halted.recv_timeout(Duration::from_millis(100)) {
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    _ => break,
                }
            }
            samples
        });
        Self {
            stop,
            worker: Some(worker),
        }
    }
    fn finish(&mut self) -> Vec<serde_json::Value> {
        let _ = self.stop.send(());
        self.worker
            .take()
            .unwrap()
            .join()
            .expect("current-generation observer must stay healthy")
    }
}
impl Drop for GenerationSamples {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
