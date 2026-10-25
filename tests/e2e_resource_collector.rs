#![cfg(all(feature = "e2e", target_os = "linux"))]

//! Synthetic processes exercise the per-machine collector through its binary.

mod common;
#[allow(dead_code)]
mod resource_collector;

use common::TuiDeck;
use dot_agent_deck::daemon_client::{DaemonClient, StartAgentOptions};
use dot_agent_deck::telemetry::resource::parse_resource_attributes;
use resource_collector::{
    Receiver, assert_figures, assert_pss, attribute, bounded_output, close, collector, cpu,
    processes, run_resource, start_collector, start_collector_with_paths,
};
use serde_json::Value;
use spec::spec;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

// The launcher exits, and the intermediate parent exits after setsid+fork.
// Only the grandchild burns CPU after the file-gated trigger. Its parent
// sleeps, so a before/after run delta isolates the detached contribution.
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
    deadline = time.monotonic() + 120
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
    while not os.path.exists(base + '.burn') and time.monotonic() < deadline:
        time.sleep(0.01)
    before = time.process_time()
    while time.process_time() - before < 0.6 and time.monotonic() < deadline:
        pass
    with open(base + '.burned', 'w') as f:
        json.dump({'cpu': time.process_time() - before}, f)
    time.sleep(max(0, deadline - time.monotonic()))
    os._exit(0)
os.waitpid(first, 0)
if mode == 'agent':
    print('RESOURCE_AGENT_READY', flush=True)
    time.sleep(120)
"#;

struct Detached {
    base: PathBuf,
}

impl Detached {
    fn burn(&self) -> f64 {
        std::fs::write(self.base.with_extension("burn"), "go").unwrap();
        let burned = self.base.with_extension("burned");
        assert!(
            common::wait_until(Duration::from_secs(45), || std::fs::read(&burned)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                .is_some()),
            "reparented fixture must complete its controlled burn"
        );
        let value: Value = serde_json::from_slice(&std::fs::read(burned).unwrap()).unwrap();
        let seconds = value["cpu"].as_f64().unwrap();
        assert!(
            seconds >= 0.5,
            "controlled burn must consume measurable CPU"
        );
        seconds
    }

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

/// Scenario: Open a stand-in pane that double-forks a grandchild, leaving it reparented but still carrying the deck's run identity. Sample before and after triggering only the grandchild to burn CPU, require the run's CPU growth to cover that burn, then join its PSS and identity to the actual agent.run span; paths appear only after explicit opt-in.
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
    let burn = detached.burn();
    let opted_in = receiver.sample(true);
    let growth = cpu(run_resource(&opted_in, &run_id)) - cpu(run);
    assert!(
        growth >= burn - 0.02,
        "run CPU growth {growth} must cover only the detached grandchild's controlled burn {burn} (two clock-tick tolerance)"
    );
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

/// Scenario: Double-fork a CPU burner under a cleared environment so it leaves every run's process tree, then sample before and after a file-triggered burn. The shared unattributed bucket must have CPU growth covering the measured burn, live processes, positive anonymous PSS and named anon/file/shmem points, without a run identity; unrelated host work can also contribute to this bucket.
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
    let _collector = start_collector(&receiver);
    assert!(common::wait_until(Duration::from_secs(25), || !receiver
        .metrics()
        .is_empty()));
    let requests = receiver.requests();
    let metrics = requests.last().unwrap().body["resourceMetrics"]
        .as_array()
        .unwrap();
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
    let before_cpu = cpu(resource);
    assert!(
        before_cpu >= ready["cpu"].as_f64().unwrap(),
        "unattributed CPU must include the detached burn"
    );
    assert!(
        processes(resource) >= 1,
        "unattributed bucket must contain live processes"
    );
    assert_pss(resource, true);
    let burn = detached.burn();
    let after_burn = receiver.metrics().len();
    assert!(
        common::wait_until(Duration::from_secs(25), || receiver.metrics()[after_burn..]
            .iter()
            .any(
                |entry| attribute(&entry["resource"], "deck.attribution") == "unattributed"
            )),
        "collector must sample again after the controlled burn"
    );
    let samples = receiver.metrics();
    let after = samples[after_burn..]
        .iter()
        .find(|entry| attribute(&entry["resource"], "deck.attribution") == "unattributed")
        .unwrap();
    let growth = cpu(after) - before_cpu;
    assert!(
        growth >= burn - 0.02,
        "shared unattributed CPU growth {growth} must cover measured burn {burn} (two clock-tick tolerance); other host processes may contribute"
    );
    assert_pss(after, true);
}

fn standin(command_file: &str) -> String {
    format!(
        "/bin/sh -c 'printf %s \"$OTEL_RESOURCE_ATTRIBUTES\" > {command_file}; echo RESOURCE_SHARED_READY; exec cat'"
    )
}

/// Scenario: Start two stand-in runs in the same tiny git worktree and sample their resources at one receiver. Both runs must share the worktree id printed by the collector CLI and exactly one disk series; adding a ballast file must grow that series by the file's allocated bytes.
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
    let disk_bytes = |metrics: &[Value]| {
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
        points[0]["asInt"].as_str().unwrap().parse::<u64>().unwrap()
    };
    let before = disk_bytes(&metrics);
    std::fs::write(&ballast, vec![0x5a; 8 * 1024 * 1024]).unwrap();
    let after = disk_bytes(&receiver.sample(false));
    use std::os::unix::fs::MetadataExt;
    let allocated = std::fs::metadata(ballast).unwrap().blocks() * 512;
    assert!(
        allocated >= 8 * 1024 * 1024,
        "ballast must really allocate blocks"
    );
    assert!(
        after >= before + allocated,
        "disk growth {} must include ballast's {allocated} allocated bytes",
        after.saturating_sub(before)
    );
}

/// Scenario: Start a deck run in a git worktree and collect live metrics with and without path opt-in, then close the pane and await its terminal sample. Its complete resource attributes must stay identical, process and all PSS gauges must be zero, and cumulative CPU must not decrease.
#[spec("telemetry/resources/005")]
#[test]
fn resources_005_terminal_sample_preserves_live_resource_identity() {
    let receiver = Receiver::start();
    let deck = TuiDeck::builder()
        .without_agent_credentials()
        .with_env("OTEL_EXPORTER_OTLP_ENDPOINT", &receiver.endpoint)
        .with_env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json")
        .with_continue_session("resource-terminal", standin("collector-terminal.env"))
        .launch_with_fixture("minimal");
    deck.wait_for_string("RESOURCE_SHARED_READY");
    std::fs::create_dir_all(deck.workdir().join(".git")).unwrap();
    let run_id = environment_run(&deck.workdir().join("collector-terminal.env"));
    let agent = common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .next()
        .unwrap();
    // Separate receivers distinguish both collectors' resources without
    // introducing attributes that could hide an identity mismatch.
    let paths_receiver = Receiver::start();
    let _collector = start_collector(&receiver);
    let _paths_collector = start_collector_with_paths(&paths_receiver, true);
    let find = |receiver: &Receiver, ended: bool| {
        receiver.metrics().into_iter().find(|entry| {
            attribute(&entry["resource"], "deck.run.id") == run_id
                && (processes(entry) == 0) == ended
        })
    };
    for source in [&receiver, &paths_receiver] {
        assert!(
            common::wait_until(Duration::from_secs(25), || find(source, false).is_some()),
            "collector must sample the live worktree run"
        );
    }
    let live = find(&receiver, false).unwrap();
    let paths_live = find(&paths_receiver, false).unwrap();
    assert!(!attribute(&live["resource"], "deck.worktree.id").is_empty());
    assert!(attribute(&live["resource"], "deck.worktree.path").is_empty());
    assert_eq!(
        attribute(&paths_live["resource"], "deck.worktree.path"),
        deck.workdir().canonicalize().unwrap().to_string_lossy()
    );
    close(&deck, &agent);
    for (source, before) in [(&receiver, live), (&paths_receiver, paths_live)] {
        assert!(
            common::wait_until(Duration::from_secs(25), || find(source, true).is_some()),
            "collector must export a terminal sample"
        );
        let ended = find(source, true).unwrap();
        assert_eq!(processes(&ended), 0);
        assert_pss(&ended, false);
        assert!(
            cpu(&ended) >= cpu(&before),
            "terminal cumulative CPU must not decrease"
        );
        assert_eq!(
            ended["resource"]["attributes"], before["resource"]["attributes"],
            "terminal resource attributes must equal the live resource attributes, including worktree identity"
        );
    }
}

/// Scenario: Answer the collector's JSON and protobuf metric requests with HTTP 200 and an OTLP partial success rejecting 100 points. The once-only CLI must fail, report the rejection count, and keep the backend's sentinel error message out of stderr.
#[spec("telemetry/resources/006")]
#[test]
fn resources_006_partial_success_rejection_fails_without_backend_text() {
    const SENTINEL: &str = "RESOURCE_REJECTION_PRIVATE_84c713";
    let json = serde_json::to_vec(&serde_json::json!({"partialSuccess": {"rejectedDataPoints": "100", "errorMessage": SENTINEL}})).unwrap();
    // ExportMetricsServiceResponse.partial_success (field 1) contains
    // rejected_data_points (field 1, varint) and error_message (field 2).
    let mut partial = vec![0x08, 100, 0x12, SENTINEL.len() as u8];
    partial.extend_from_slice(SENTINEL.as_bytes());
    let mut protobuf = vec![0x0a, partial.len() as u8];
    protobuf.extend_from_slice(&partial);
    let mut failures = Vec::new();
    for (protocol, kind, response) in [
        ("http/json", "application/json", json),
        ("http/protobuf", "application/x-protobuf", protobuf),
    ] {
        let receiver = Receiver::with_metrics_response(kind, response);
        let mut command = collector(&receiver);
        command.env("OTEL_EXPORTER_OTLP_PROTOCOL", protocol).args([
            "--once",
            "--disk-interval",
            "0",
        ]);
        let output = bounded_output(command);
        let requests = receiver.requests();
        assert_eq!(requests.len(), 1, "partial rejection must not be retried");
        assert_eq!(requests[0].path, "/v1/metrics");
        assert_eq!(requests[0].content_type, kind);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains(SENTINEL),
            "backend errorMessage leaked into stderr for {protocol}: {stderr}"
        );
        if output.status.success()
            || !stderr.to_lowercase().contains("rejected")
            || !stderr.contains("100")
        {
            failures.push(format!(
                "{protocol}: status={}; stderr={stderr:?}",
                output.status
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "partial-success rejection must exit non-zero and report rejected count 100: {}",
        failures.join("; ")
    );
}
