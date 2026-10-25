#![cfg(all(feature = "e2e", target_os = "linux"))]

//! Synthetic processes exercise the per-machine collector through its binary.

mod common;
#[allow(dead_code)]
mod resource_collector;

use common::TuiDeck;
use dot_agent_deck::daemon_client::{DaemonClient, StartAgentOptions};
use dot_agent_deck::telemetry::resource::parse_resource_attributes;
use resource_collector::{
    Receiver, assert_figures, attribute, bounded_output, close, metric, run_resource,
};
use serde_json::Value;
use spec::spec;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

// The launcher exits, and the intermediate parent exits after setsid+fork.
// The remaining grandchild keeps its environment, burns measurable CPU, then
// sleeps. It has its own wall-clock deadline even if test cleanup is killed.
const DETACHED: &str = r#"import json, os, sys, time
base, mode = sys.argv[1:]
if mode == 'agent':
    with open(base + '.env', 'w') as f:
        f.write(os.environ.get('OTEL_RESOURCE_ATTRIBUTES', ''))
first = os.fork()
if first == 0:
    os.setsid()
    intermediate = os.getpid()
    if os.fork() != 0:
        os._exit(0)
    for fd in (0, 1, 2):
        os.close(fd)
    deadline = time.monotonic() + 60
    with open('/proc/self/stat') as f:
        start = f.read().rsplit(')', 1)[1].split()[19]
    with open(base + '.pid', 'w') as f:
        json.dump({'pid': os.getpid(), 'start': start}, f)
    while os.getppid() == intermediate and time.monotonic() < deadline:
        time.sleep(0.01)
    while time.process_time() < 0.3 and time.monotonic() < deadline - 20:
        pass
    with open('/proc/self/stat') as f:
        fields = f.read().rsplit(')', 1)[1].split()
    with open(base + '.ready', 'w') as f:
        json.dump({'pid': os.getpid(), 'parent': os.getppid(), 'intermediate': intermediate,
                   'cpu': (int(fields[11]) + int(fields[12])) / os.sysconf('SC_CLK_TCK')}, f)
    time.sleep(max(0, deadline - time.monotonic()))
    os._exit(0)
os.waitpid(first, 0)
if mode == 'agent':
    print('RESOURCE_AGENT_READY', flush=True)
    time.sleep(60)
"#;

struct Detached {
    base: PathBuf,
}

impl Detached {
    fn ready(&self) -> Value {
        let ready = self.base.with_extension("ready");
        assert!(
            common::wait_until(Duration::from_secs(45), || std::fs::read(&ready)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                .is_some()),
            "double-forked fixture did not finish its CPU burn"
        );
        let value: Value = serde_json::from_slice(&std::fs::read(ready).unwrap()).unwrap();
        assert_ne!(
            value["parent"], value["intermediate"],
            "fixture must actually be reparented"
        );
        assert!(
            value["cpu"].as_f64().unwrap() >= 0.25,
            "fixture must burn measurable CPU"
        );
        value
    }
}

impl Drop for Detached {
    fn drop(&mut self) {
        // Match starttime before signalling, so a reused pid is never killed.
        if let Ok(bytes) = std::fs::read(self.base.with_extension("pid"))
            && let Ok(identity) = serde_json::from_slice::<Value>(&bytes)
            && let Some(pid) = identity["pid"].as_u64()
            && let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            && let Some((_, tail)) = stat.rsplit_once(')')
            && tail.split_whitespace().nth(19) == identity["start"].as_str()
        {
            // SAFETY: positive fixture pid, verified against its starttime.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
        }
    }
}

fn shell_path(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
}

fn environment_run(path: &Path) -> String {
    let raw = std::fs::read_to_string(path).unwrap();
    let run = parse_resource_attributes(&raw)
        .into_iter()
        .find(|(key, _)| key == "deck.run.id")
        .map(|(_, value)| value)
        .unwrap();
    assert!(!run.is_empty());
    run
}

/// Scenario: Open a stand-in pane that double-forks a CPU-burning grandchild, leaving it reparented but still carrying the deck's run identity. Sample resource metrics at a local receiver, close the pane and join the figures to its actual agent.run span; PSS and the detached CPU must be present, and working-directory paths appear only after explicit opt-in.
#[spec("telemetry/resources/001")]
#[test]
fn resources_001_detached_cpu_joins_run_and_paths_are_opt_in() {
    let scratch = common::harness_tempdir().unwrap();
    let script = scratch.path().join("detach.py");
    std::fs::write(&script, DETACHED).unwrap();
    let detached = Detached {
        base: scratch.path().join("detached"),
    };
    let receiver = Receiver::start();
    let deck = TuiDeck::builder()
        .without_agent_credentials()
        .with_env("OTEL_EXPORTER_OTLP_ENDPOINT", &receiver.endpoint)
        .with_env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json")
        .with_continue_session(
            "resource-detach",
            format!(
                "/usr/bin/python3 {} {} agent",
                shell_path(&script),
                shell_path(&detached.base)
            ),
        )
        .launch_with_fixture("minimal");
    deck.wait_for_string("RESOURCE_AGENT_READY");
    std::fs::create_dir_all(deck.workdir().join(".git")).unwrap();
    let ready = detached.ready();
    let run_id = environment_run(&detached.base.with_extension("env"));
    let agent = common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .next()
        .unwrap();
    let metrics = receiver.sample(false);
    let run = run_resource(&metrics, &run_id);
    assert_figures(run, ready["cpu"].as_f64().unwrap());
    assert_eq!(
        attribute(&run["resource"], "deck.pane.id"),
        agent.pane_id_env.as_deref().unwrap()
    );
    let cwd = deck
        .workdir()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(
        !serde_json::to_string(&metrics).unwrap().contains(&cwd),
        "default metrics leaked cwd"
    );
    assert!(
        metrics
            .iter()
            .all(|entry| attribute(&entry["resource"], "deck.worktree.path").is_empty())
    );
    let opted_in = receiver.sample(true);
    assert_eq!(
        attribute(
            &run_resource(&opted_in, &run_id)["resource"],
            "deck.worktree.path"
        ),
        cwd
    );
    close(&deck, &agent);
    let span = receiver.run(agent.pane_id_env.as_deref().unwrap());
    assert_eq!(
        attribute(&span, "deck.run.id"),
        attribute(&run["resource"], "deck.run.id")
    );
}

/// Scenario: Double-fork a CPU burner under a cleared environment so it leaves every run's process tree. A collector sample must still report an unattributed bucket with CPU covering that burn, live processes and PSS, without inventing a run identity.
#[spec("telemetry/resources/002")]
#[test]
fn resources_002_cleared_detached_work_is_unattributed() {
    let scratch = common::harness_tempdir().unwrap();
    let script = scratch.path().join("detach.py");
    std::fs::write(&script, DETACHED).unwrap();
    let detached = Detached {
        base: scratch.path().join("unattributed"),
    };
    // env_clear is the execve equivalent of env -i, including removal of the
    // test runner's own deck.run.id. The final child also leaves the tree.
    let mut command = Command::new("/usr/bin/python3");
    command
        .env_clear()
        .arg(&script)
        .arg(&detached.base)
        .arg("cleared");
    assert!(bounded_output(command).status.success());
    let ready = detached.ready();
    let pid = ready["pid"].as_u64().unwrap();
    assert!(
        std::fs::read(format!("/proc/{pid}/environ"))
            .unwrap()
            .split(|byte| *byte == 0)
            .all(|entry| !entry.starts_with(b"OTEL_RESOURCE_ATTRIBUTES="))
    );
    let receiver = Receiver::start();
    let metrics = receiver.sample(false);
    let unattributed: Vec<_> = metrics
        .iter()
        .filter(|entry| attribute(&entry["resource"], "deck.attribution") == "unattributed")
        .collect();
    assert_eq!(
        unattributed.len(),
        1,
        "one unattributed resource per sample"
    );
    let resource = unattributed[0];
    assert!(attribute(&resource["resource"], "deck.run.id").is_empty());
    let cpu = metric(resource, "deck.run.cpu.time")["sum"]["dataPoints"][0]["asDouble"]
        .as_f64()
        .unwrap();
    assert!(
        cpu >= ready["cpu"].as_f64().unwrap(),
        "unattributed CPU must include the detached burn"
    );
    let count: u64 = metric(resource, "deck.run.processes")["gauge"]["dataPoints"][0]["asInt"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(count >= 1, "cleared detached process must not be dropped");
    assert_eq!(
        metric(resource, "deck.run.memory.pss")["gauge"]["dataPoints"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
}

fn standin(command_file: &str) -> String {
    format!(
        "/bin/sh -c 'printf %s \"$OTEL_RESOURCE_ATTRIBUTES\" > {command_file}; echo RESOURCE_SHARED_READY; exec cat'"
    )
}

/// Scenario: Start two stand-in runs in the same tiny git worktree and sample their resources at one receiver. Both run resources must share the worktree id printed by the collector CLI, while exactly one disk series reports that worktree's allocated usage.
#[spec("telemetry/resources/003")]
#[test]
fn resources_003_shared_worktree_has_one_disk_series() {
    let receiver = Receiver::start();
    let deck = TuiDeck::builder()
        .without_agent_credentials()
        .with_env("OTEL_EXPORTER_OTLP_ENDPOINT", &receiver.endpoint)
        .with_env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json")
        .with_continue_session("resource-first", standin("collector-first.env"))
        .launch_with_fixture("minimal");
    deck.wait_for_string("RESOURCE_SHARED_READY");
    std::fs::create_dir_all(deck.workdir().join(".git")).unwrap();
    let ballast = deck.workdir().join("allocated-fixture.bin");
    std::fs::write(&ballast, vec![0x5a; 16384]).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let client = DaemonClient::new(deck.attach_socket_path().to_path_buf());
    runtime
        .block_on(client.start_agent(StartAgentOptions {
            command: Some(standin("collector-second.env")),
            cwd: Some(deck.workdir().to_string_lossy().into_owned()),
            display_name: Some("resource-second".into()),
            env: vec![("DOT_AGENT_DECK_PANE_ID".into(), "resource-second".into())],
            ..StartAgentOptions::default()
        }))
        .unwrap();
    common::wait_for_file_contains(&deck.workdir().join("collector-second.env"), "deck.run.id=");
    let first = environment_run(&deck.workdir().join("collector-first.env"));
    let second = environment_run(&deck.workdir().join("collector-second.env"));
    assert_ne!(first, second, "two real deck run identities");
    let mut command = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck-collector"));
    command.env_clear().arg("worktree-id").arg(deck.workdir());
    let output = bounded_output(command);
    assert!(output.status.success());
    let worktree = String::from_utf8(output.stdout).unwrap().trim().to_string();
    assert_eq!(worktree.len(), 32);
    let metrics = receiver.sample(false);
    for run_id in [&first, &second] {
        assert_eq!(
            attribute(
                &run_resource(&metrics, run_id)["resource"],
                "deck.worktree.id"
            ),
            worktree
        );
    }
    let disks: Vec<_> = metrics
        .iter()
        .filter(|entry| attribute(&entry["resource"], "deck.worktree.id") == worktree)
        .flat_map(|entry| entry["scopeMetrics"].as_array().into_iter().flatten())
        .flat_map(|scope| scope["metrics"].as_array().into_iter().flatten())
        .filter(|metric| metric["name"] == "deck.worktree.disk.usage")
        .collect();
    assert_eq!(
        disks.len(),
        1,
        "shared worktree disk must not be repeated per run"
    );
    let points = disks[0]["gauge"]["dataPoints"].as_array().unwrap();
    assert_eq!(points.len(), 1, "one disk series per worktree");
    let bytes: u64 = points[0]["asInt"].as_str().unwrap().parse().unwrap();
    use std::os::unix::fs::MetadataExt;
    assert!(bytes >= std::fs::metadata(ballast).unwrap().blocks() * 512);
}
