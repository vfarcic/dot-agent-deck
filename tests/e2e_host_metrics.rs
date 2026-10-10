#![cfg(all(feature = "e2e", unix))]

//! Synthetic socket coverage of the daemon's host metrics; no agent is spawned.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use dot_agent_deck::agent_pty::AgentPtyRegistry;
use dot_agent_deck::daemon_protocol::{
    AttachRequest, AttachResponse, CAP_HOST_METRICS, KIND_REQ, KIND_RESP, bind_attach_listener,
    serve_attach,
};
use dot_agent_deck::host_metrics::HOST_METRICS_MAX_AGE;
use spec::spec;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

#[path = "common/child_lifetime_bound.rs"]
mod child_lifetime_bound;
#[path = "../src/test_temp.rs"]
mod test_temp;

static BIND_LOCK: Mutex<()> = Mutex::new(());

struct Server {
    _root: tempfile::TempDir,
    socket: std::path::PathBuf,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn server() -> Server {
    child_lifetime_bound::arm();
    let (root, socket, listener) = {
        let _guard = BIND_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let root = test_temp::tempdir().expect("isolated socket root");
        let socket = root.path().join("attach.sock");
        let listener = bind_attach_listener(&socket).expect("bind synthetic daemon");
        (root, socket, listener)
    };
    let task = tokio::spawn(async move {
        let (events, _) = tokio::sync::broadcast::channel(16);
        serve_attach(listener, Arc::new(AgentPtyRegistry::new()), events)
            .await
            .expect("serve synthetic daemon");
    });
    Server {
        _root: root,
        socket,
        task,
    }
}

async fn request(server: &Server, request: &AttachRequest) -> (AttachResponse, serde_json::Value) {
    let mut stream = UnixStream::connect(&server.socket).await.expect("connect");
    let payload = serde_json::to_vec(request).expect("encode request");
    stream.write_u8(KIND_REQ).await.unwrap();
    stream.write_u32(payload.len() as u32).await.unwrap();
    stream.write_all(&payload).await.unwrap();
    assert_eq!(stream.read_u8().await.unwrap(), KIND_RESP);
    let length = stream.read_u32().await.unwrap() as usize;
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload).await.unwrap();
    let response: AttachResponse = serde_json::from_slice(&payload).expect("decode response");
    assert!(response.ok, "request failed: {:?}", response.error);
    (response, serde_json::from_slice(&payload).unwrap())
}

fn assert_no_absolute_paths(value: &serde_json::Value) {
    match value {
        serde_json::Value::String(text) => assert!(
            !std::path::Path::new(text).is_absolute(),
            "host metrics must contain role names, not host paths: {text}"
        ),
        serde_json::Value::Array(values) => values.iter().for_each(assert_no_absolute_paths),
        serde_json::Value::Object(fields) => fields.values().for_each(assert_no_absolute_paths),
        _ => {}
    }
}

/// Scenario: Ask a socket-backed daemon for its capabilities, then request its
/// host metrics. The reply contains bounded disk figures for three named roles,
/// CPU and memory readings where supported, and sample age without host paths.
#[spec("protocol/host-metrics/001")]
#[tokio::test]
async fn protocol_host_metrics_001_socket_reports_host_numbers_without_paths() {
    let server = server();
    let (hello, _) = request(
        &server,
        &AttachRequest::Hello {
            client_version: dot_agent_deck::daemon_protocol::PROTOCOL_VERSION,
            client_build_version: None,
        },
    )
    .await;
    assert_eq!(CAP_HOST_METRICS, "host-metrics");
    assert!(
        hello
            .capabilities
            .unwrap()
            .iter()
            .any(|c| c == CAP_HOST_METRICS)
    );
    assert_eq!(
        serde_json::to_value(AttachRequest::HostMetrics).unwrap(),
        serde_json::json!({"op": "host-metrics"})
    );

    let (response, wire) = request(&server, &AttachRequest::HostMetrics).await;
    assert_no_absolute_paths(&wire);
    let metrics = response.host_metrics.expect("host-metrics payload");
    let mut roles = metrics
        .disks
        .iter()
        .map(|d| d.role.as_str())
        .collect::<Vec<_>>();
    roles.sort_unstable();
    assert_eq!(roles, ["temp_root", "working_root", "worktree_parent"]);
    for disk in &metrics.disks {
        let free = disk
            .free_bytes
            .expect("readable local filesystem free bytes");
        let total = disk
            .total_bytes
            .expect("readable local filesystem total bytes");
        assert!(total > 0 && free <= total, "invalid disk sample: {disk:?}");
    }
    assert!(metrics.cpu_count.is_some_and(|count| count > 0));
    if let Some(load) = metrics.load_per_cpu {
        assert!(load.is_finite() && load >= 0.0);
    }
    #[cfg(target_os = "linux")]
    {
        assert!(
            metrics.load_per_cpu.is_some(),
            "Linux load must be measurable here"
        );
        assert!(metrics.memory_used_bytes.is_some(), "Linux memory used");
        assert!(
            metrics.memory_available_bytes.is_some(),
            "Linux memory available"
        );
    }
    assert!(metrics.sample_age_ms <= HOST_METRICS_MAX_AGE.as_millis() as u64);
    assert!(wire["host_metrics"]["sampled_at_ms"].is_u64());
    assert!(wire["host_metrics"]["sample_age_ms"].is_u64());
}

/// Scenario: Await the first metrics reply before advancing a paused clock and
/// asking over a fresh connection; the cached sample stays identical while its
/// age increases, and a request after the freshness bound obtains a fresh sample.
#[spec("protocol/host-metrics/002")]
#[tokio::test(start_paused = true)]
async fn protocol_host_metrics_002_socket_reuses_sample_until_max_age() {
    let server = server();
    // Prevent paused Tokio time from auto-advancing while Unix I/O is pending.
    let clock_guard = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    let (first, mut first_wire) = request(&server, &AttachRequest::HostMetrics).await;
    let first = first.host_metrics.expect("first completed sample");
    let elapsed = Duration::from_millis(250);
    assert!(HOST_METRICS_MAX_AGE > elapsed);
    tokio::time::advance(elapsed).await;
    let (second, mut second_wire) = request(&server, &AttachRequest::HostMetrics).await;
    let second = second.host_metrics.expect("second completed sample");
    assert_eq!(
        second.sampled_at_ms, first.sampled_at_ms,
        "reuse the cached sample across connections"
    );
    assert_eq!(second.sample_age_ms, first.sample_age_ms + 250);
    first_wire["host_metrics"]
        .as_object_mut()
        .unwrap()
        .remove("sample_age_ms");
    second_wire["host_metrics"]
        .as_object_mut()
        .unwrap()
        .remove("sample_age_ms");
    assert_eq!(
        first_wire, second_wire,
        "only sample age changes on a cache hit"
    );

    tokio::time::advance(HOST_METRICS_MAX_AGE + Duration::from_millis(1)).await;
    let (fresh, _) = request(&server, &AttachRequest::HostMetrics).await;
    assert!(
        fresh.host_metrics.unwrap().sample_age_ms < 250,
        "expired samples must be refreshed on demand"
    );
    clock_guard.abort();
}
