#![cfg(all(feature = "e2e", unix))]

//! Lane-1 coverage for the command word the deck writes into the `work-done`
//! instruction a delegated worker later runs (issue #549).
//!
//! The deck composes that instruction in its own process, but the worker runs
//! it in the WORKER's shell, with whatever `$PATH` that shell ended up with —
//! commonly a login shell that sourced profile files the deck never saw. So a
//! command word that is only correct under the deck's own `$PATH` is not a
//! command word the worker can rely on. This file runs the generated line the
//! way a worker does, under a `$PATH` the deck did not choose, and checks that
//! the signal reaches the daemon.
//!
//! No real agent: the worker pane is a shell stub that runs the line it is
//! handed, so this belongs in lane 1 and runs in CI. What the stub stands in
//! for is an agent copying the line out of its task file; the real-agent end
//! of that is `delegate_work_done_chain_claude`, in lane 2.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use dot_agent_deck::agent_pty::{DOT_AGENT_DECK_PANE_ID, SpawnOptions};
use dot_agent_deck::event::DelegateSignal;
use spec::spec;

const ORCH_PANE: &str = "orchestrator-pane";
const WORKER_PANE: &str = "worker-pane";
const WORKER_ROLE: &str = "coder";

/// A token unique to this test's report, so finding it in the daemon's
/// `work-done-coder.md` proves THIS report arrived.
const SENTINEL: &str = "delegate-020-report-5c1e";

/// Scenario: Put the built deck binary's own directory first on the deck's
/// `$PATH`, delegate a task to a shell-stub worker whose own `$PATH` puts a
/// different executable named `dot-agent-deck` first, and take the
/// `work-done --task-file` line from the worker task file the deck writes.
/// The worker runs that line from its own environment: the shadow must never
/// run, and the daemon must receive the worker's report.
#[spec("orchestration/delegate/020")]
#[test]
fn delegate_020_work_done_line_reaches_the_deck_under_the_workers_own_path() {
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let bin_dir = Path::new(bin)
        .parent()
        .expect("deck binary has a parent dir")
        .to_str()
        .expect("bin dir is UTF-8")
        .to_string();
    let inherited_path = std::env::var("PATH").unwrap_or_default();
    // The deck's own `$PATH`: its install directory first, which is the shape
    // in which a bare `dot-agent-deck` resolves to the deck *in this process*.
    // SAFETY: a stated residual, not a proof (issue #1516). Set at the top of
    // the sync entry point, before the tokio runtime (and so any daemon worker
    // thread) exists, and never written again. The threads that exist here:
    // this test's own and libtest's runner thread, which waits for it; nothing
    // above allocates a harness temp dir, so the `load-context` heartbeat has
    // not started. nextest runs each test in its own process, so the change
    // cannot reach another test.
    unsafe {
        std::env::set_var("PATH", format!("{bin_dir}:{inherited_path}"));
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build multi-thread runtime");
    rt.block_on(delegate_020_inner(&bin_dir, &inherited_path));
}

async fn delegate_020_inner(bin_dir: &str, inherited_path: &str) {
    let daemon = common::spawn_inprocess_daemon().await;

    let cwd = common::race_safe_tempdir();
    let cwd_str = cwd.path().to_str().expect("cwd is UTF-8").to_string();

    // The worker's `$PATH`: one the deck did not choose, whose first entry
    // holds a different executable named `dot-agent-deck`. The deck's real
    // directory is still on it, later — which is exactly what a first-match
    // lookup ignores.
    let shadow_dir = common::race_safe_tempdir();
    let marker = shadow_dir.path().join("shadow-ran");
    let shadow = shadow_dir.path().join("dot-agent-deck");
    std::fs::write(
        &shadow,
        format!("#!/bin/sh\necho \"$@\" > '{}'\nexit 0\n", marker.display()),
    )
    .expect("write shadow binary");
    std::fs::set_permissions(&shadow, std::fs::Permissions::from_mode(0o755))
        .expect("make shadow executable");
    let worker_path = format!("{}:{bin_dir}:{inherited_path}", shadow_dir.path().display());

    // The worker stands in for an agent following its task file: a shell that,
    // once handed the line, runs it from ITS OWN environment — the `$PATH`
    // above and the hook capability token the daemon minted into this pane —
    // and records how it went. `cat` afterwards keeps the pane alive.
    let worker = cwd.path().join("worker-stub.sh");
    std::fs::write(
        &worker,
        "#!/bin/sh\n\
         while [ ! -f run-me.sh ]; do sleep 0.05; done\n\
         sh ./run-me.sh > run-me.out 2>&1\n\
         echo $? > run-me.rc.tmp && mv run-me.rc.tmp run-me.rc\n\
         exec cat\n",
    )
    .expect("write worker stub");
    std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o755))
        .expect("make worker stub executable");
    let worker_command = worker.to_str().expect("worker path is UTF-8").to_string();

    let _worker_agent_id = daemon
        .registry
        .spawn_agent(SpawnOptions {
            command: Some(&worker_command),
            cwd: Some(cwd_str.as_str()),
            env: vec![
                (DOT_AGENT_DECK_PANE_ID.to_string(), WORKER_PANE.to_string()),
                (
                    "DOT_AGENT_DECK_SOCKET".to_string(),
                    daemon.hook_path.display().to_string(),
                ),
                ("PATH".to_string(), worker_path.clone()),
            ],
            ..SpawnOptions::default()
        })
        .expect("spawn worker stub");

    {
        let mut st = daemon.state.write().await;
        st.pane_role_map
            .insert(ORCH_PANE.to_string(), "orchestrator".to_string());
        st.pane_role_map
            .insert(WORKER_PANE.to_string(), WORKER_ROLE.to_string());
        st.orchestrator_pane_ids.insert(ORCH_PANE.to_string());
        let orch = dot_agent_deck::state::OrchestrationIdentity {
            id: "orch-test-0".to_string(),
            name: "test-orchestration".to_string(),
        };
        st.pane_orchestration_map
            .insert(ORCH_PANE.to_string(), orch.clone());
        st.pane_orchestration_map
            .insert(WORKER_PANE.to_string(), orch);
        st.pane_cwd_map
            .insert(WORKER_PANE.to_string(), cwd_str.clone());
    }

    let signal = DelegateSignal {
        pane_id: ORCH_PANE.to_string(),
        task: "List the files in the current directory.".to_string(),
        to: vec![WORKER_ROLE.to_string()],
        supersede: false,
        timestamp: chrono::Utc::now(),
        token: None,
    };
    daemon
        .state
        .read()
        .await
        .handle_delegate(signal, &daemon.registry, &daemon.event_tx)
        .await;

    let task_file = cwd
        .path()
        .join(".dot-agent-deck")
        .join("worker-task-coder.md");
    let ok = common::wait_for_path_async(&task_file, Duration::from_secs(5)).await;
    assert!(ok, "worker task file was never written at {task_file:?}");
    let body = std::fs::read_to_string(&task_file).expect("read worker task file");

    // The primary instruction, exactly as the worker would copy it out of the
    // fenced block, with the one placeholder it is told to fill in.
    let line = body
        .lines()
        .find(|l| l.contains(" work-done --task-file '"))
        .unwrap_or_else(|| panic!("no `work-done --task-file` line in the task file:\n{body}"))
        .replace("<summary-slug>", "delegate-020");
    // The report path is the single-quoted value AFTER `--task-file`: the
    // command word before it may itself be single-quoted (a path with a space).
    let report_rel = line
        .split_once("--task-file '")
        .and_then(|(_, rest)| rest.split_once('\''))
        .map(|(path, _)| path)
        .unwrap_or_else(|| panic!("no single-quoted --task-file path in {line:?}"));
    assert!(
        report_rel.starts_with(".dot-agent-deck/") && !report_rel.contains(".."),
        "the report path must be relative, under the worker's .dot-agent-deck/: {report_rel:?}"
    );
    let report = cwd.path().join(report_rel);
    std::fs::write(&report, format!("{SENTINEL}\n")).expect("write the worker's report");

    // Control: in the worker's `$PATH` a bare `dot-agent-deck` really is the
    // shadow. Without this, a shadow that was never reachable would make the
    // assertion below pass for the wrong reason.
    let control_path = worker_path.clone();
    let control = tokio::task::spawn_blocking(move || {
        std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("dot-agent-deck control")
            .env("PATH", control_path)
            .output()
    })
    .await
    .expect("join the control shell")
    .expect("run the control shell");
    assert!(
        control.status.success() && marker.exists(),
        "control failed: a shell with the worker's $PATH must resolve a bare \
         `dot-agent-deck` to the shadow, or this test proves nothing (status {:?}, stderr {})",
        control.status,
        String::from_utf8_lossy(&control.stderr)
    );
    std::fs::remove_file(&marker).expect("reset the shadow marker");

    // Hand the line to the worker, written atomically so it never runs half.
    std::fs::write(cwd.path().join("run-me.sh.tmp"), format!("{line}\n"))
        .expect("write the worker's command");
    std::fs::rename(
        cwd.path().join("run-me.sh.tmp"),
        cwd.path().join("run-me.sh"),
    )
    .expect("hand the command to the worker");
    let rc_path = cwd.path().join("run-me.rc");
    let ran = common::wait_for_path_async(&rc_path, Duration::from_secs(20)).await;
    let rc = std::fs::read_to_string(&rc_path).unwrap_or_default();
    let out = std::fs::read_to_string(cwd.path().join("run-me.out")).unwrap_or_default();
    assert!(
        ran,
        "the worker never finished running the line.\nline: {line}\noutput: {out}"
    );

    assert!(
        !marker.exists(),
        "the generated `work-done` line ran a DIFFERENT `dot-agent-deck` — the first one on \
         the worker's own $PATH — instead of the deck that wrote it, so the report never \
         reached the daemon.\nline: {line}\nshadow got: {:?}",
        std::fs::read_to_string(&marker).unwrap_or_default()
    );
    assert_eq!(
        rc.trim(),
        "0",
        "the generated `work-done` line failed in the worker's shell.\nline: {line}\n\
         output: {out}"
    );

    let work_done = cwd
        .path()
        .join(".dot-agent-deck")
        .join(format!("work-done-{WORKER_ROLE}.md"));
    let arrived = common::wait_for_path_async(&work_done, Duration::from_secs(10)).await;
    let summary = std::fs::read_to_string(&work_done).unwrap_or_default();
    assert!(
        arrived && summary.contains(SENTINEL),
        "the daemon never received the worker's report ({work_done:?} arrived={arrived}, \
         contents {summary:?})"
    );

    daemon.registry.shutdown_all();
}
