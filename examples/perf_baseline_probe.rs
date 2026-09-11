//! PRD #741 M4 performance probe.
//!
//! Spins up a REAL attach server (production `run_attach_server_with_counter`)
//! backed by a registry + `AppState`, spawns N real `cat` PTY agents through the
//! attach protocol, seeds a realistic live `Thinking` session for each, then:
//!   1. measures the exact serialized `ListAgents` response payload (bytes/refresh)
//!      at several fleet sizes;
//!   2. measures one pushed `BroadcastMsg::Event` frame, which is what M4(b)
//!      pays instead of that payload in steady state;
//!   3. times a tight sequential `list_agents()` loop (connect + req + resp +
//!      teardown) to derive achievable connections/sec and per-connection cost.
//!
//! This produced the baseline table in the PRD's performance section and is
//! committed so the M4(b) comparison is reproducible rather than reconstructed —
//! the provenance standard #819 set and the reason the baseline was taken from
//! unmodified code in the first place.
//!
//! **Scope of what it measures: the LIBRARY side.** It drives `DaemonClient`
//! directly, so it reports what a bare client costs per operation. It says
//! nothing about how many operations the desktop performs per refresh — that is
//! `desktop/src-tauri/src`'s business and is measured by the tests named in the
//! closing section below.
//!
//! Run: cargo run --release --example perf_baseline_probe
//! (release so timing is representative; the byte counts are build-independent.)

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use dot_agent_deck::agent_pty::{AgentPtyRegistry, AgentRecord, DOT_AGENT_DECK_PANE_ID};
use dot_agent_deck::daemon_client::{DaemonClient, StartAgentOptions};
use dot_agent_deck::daemon_protocol::{AttachResponse, run_attach_server_with_counter};
use dot_agent_deck::event::{AgentEvent, AgentType, BroadcastMsg, EventType};
use dot_agent_deck::state::AppState;
use tokio::sync::{RwLock, broadcast};

fn realistic_thinking_event(pane_id: &str, agent_id: &str, cwd: &str, n: usize) -> AgentEvent {
    let mut metadata = std::collections::HashMap::new();
    metadata.insert("card_title".to_string(), format!("worker-{n}"));
    AgentEvent {
        session_id: format!("{pane_id}-session"),
        agent_type: AgentType::ClaudeCode,
        event_type: EventType::ToolStart,
        tool_name: Some("Bash".to_string()),
        tool_detail: Some(format!(
            "cargo nextest run --workspace --features e2e -p dot-agent-deck agent_{n}"
        )),
        cwd: Some(cwd.to_string()),
        timestamp: chrono::Utc::now(),
        user_prompt: Some(format!(
            "Please investigate the failing test in module {n} and report the sentinel filename you find in the fixture directory."
        )),
        metadata,
        pane_id: Some(pane_id.to_string()),
        agent_id: Some(agent_id.to_string()),
        agent_version: Some("claude-code 1.2.3".to_string()),
        schema_version: Some(1),
        live_target: None,
    }
}

async fn wire_bytes(records: &[AgentRecord]) -> usize {
    // The exact JSON payload the daemon writes in the KIND_RESP frame for
    // `ListAgents` is `serde_json::to_vec(&AttachResponse::agent_records(records))`.
    // Frame overhead on the wire is +5 bytes (1 kind byte + 4-byte big-endian len).
    let resp = AttachResponse::agent_records(records.to_vec());
    serde_json::to_vec(&resp).unwrap().len()
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("attach.sock");
    let registry = Arc::new(AgentPtyRegistry::new());
    let (event_tx, _rx) = broadcast::channel(256);
    let state: Arc<RwLock<AppState>> = Arc::new(RwLock::new(AppState::default()));
    let counter = Arc::new(AtomicUsize::new(0));

    let server = {
        let sock = sock.clone();
        let registry = registry.clone();
        let state = state.clone();
        let counter = counter.clone();
        tokio::spawn(async move {
            let _ = run_attach_server_with_counter(&sock, registry, event_tx, counter, state).await;
        })
    };

    // Wait for bind.
    let deadline = Instant::now() + Duration::from_secs(5);
    while tokio::net::UnixStream::connect(&sock).await.is_err() {
        assert!(Instant::now() < deadline, "attach socket never came up");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let client = DaemonClient::new(sock.clone());
    let fleet_sizes = [2usize, 8, 12, 15];
    let max_fleet = *fleet_sizes.iter().max().unwrap();

    println!("=== PRD #741 perf baseline probe ===");
    println!("socket: {}", sock.display());

    let mut spawned = 0usize;
    let mut byte_rows: Vec<(usize, usize)> = Vec::new();

    for target in fleet_sizes {
        while spawned < target {
            let n = spawned;
            let pane_id = format!("pane-{n}");
            let cwd = format!("/home/dev/projects/service-{n}");
            let agent_id = client
                .start_agent(StartAgentOptions {
                    command: Some("cat".to_string()),
                    cwd: Some(cwd.clone()),
                    display_name: Some(format!("worker-{n}")),
                    env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane_id.clone())],
                    ..StartAgentOptions::default()
                })
                .await
                .expect("spawn cat agent");
            {
                let mut guard = state.write().await;
                guard.apply_event(realistic_thinking_event(&pane_id, &agent_id, &cwd, n));
            }
            spawned += 1;
        }

        let records = client.list_agents().await.expect("list_agents");
        let live = records.iter().filter(|r| r.live.is_some()).count();
        let bytes = wire_bytes(&records).await;
        byte_rows.push((records.len(), bytes));
        println!(
            "fleet={:>2}  records={:>2}  live_joined={:>2}  wire_payload_bytes={:>6}  bytes/agent={:>4}",
            target,
            records.len(),
            live,
            bytes,
            bytes / records.len().max(1)
        );
    }

    // Timing: tight sequential list_agents loop at the max fleet size.
    let iters = 400usize;
    // warmup
    for _ in 0..20 {
        let _ = client.list_agents().await.unwrap();
    }
    let mut samples: Vec<u128> = Vec::with_capacity(iters);
    let start = Instant::now();
    for _ in 0..iters {
        let t = Instant::now();
        let _ = client.list_agents().await.unwrap();
        samples.push(t.elapsed().as_micros());
    }
    let wall = start.elapsed();
    samples.sort_unstable();
    let median = samples[samples.len() / 2];
    let p95 = samples[(samples.len() * 95) / 100];
    let min = samples[0];
    let seq_per_sec = iters as f64 / wall.as_secs_f64();
    let final_counter = counter.load(Ordering::SeqCst);

    println!("\n--- list_agents() sequential timing (fleet={max_fleet}, {iters} iters) ---");
    println!("min    = {min:>6} us/call");
    println!("median = {median:>6} us/call");
    println!("p95    = {p95:>6} us/call");
    println!("wall   = {:.3} s", wall.as_secs_f64());
    println!("sequential throughput = {seq_per_sec:.0} list_agents/s (1 connection each)");
    println!("concurrent client_count at end = {final_counter} (should be 0)");

    // What ONE pushed event frame costs, which is what M4(b) pays in steady
    // state instead of the whole listing above.
    let event_payload = serde_json::to_vec(&BroadcastMsg::Event(realistic_thinking_event(
        "pane-0",
        "0",
        "/home/dev/projects/service-0",
        0,
    )))
    .unwrap()
    .len();

    println!("\n--- one pushed event frame (what M4(b) pays per change) ---");
    println!("BroadcastMsg::Event payload = {event_payload} B (+5 B frame header)");
    for (agents, bytes) in &byte_rows {
        println!(
            "  vs a full ListAgents at {agents:>2} agents: {bytes:>6} B  =>  {:.1}x",
            *bytes as f64 / event_payload as f64
        );
    }

    // NOT a desktop model. This binary measures the library client, and the
    // desktop's per-refresh connection count is a property of
    // `desktop/src-tauri/src/daemon_bridge.rs`, not of this code — it was 2 at
    // the baseline, 1 after M4(a) held the handshake, and ~0 in steady state
    // after M4(b) folded the events. The earlier version of this section printed
    // "connections/refresh (Unix) = 2" as though it were a property of the
    // library, and it was stale the moment M4(a) landed.
    println!("\n--- library-side derived model (NOT the desktop's refresh cost) ---");
    println!(
        "one DaemonClient operation = 1 connection (the daemon answers one request per connection)"
    );
    println!(
        "so a client doing hello + list_agents per refresh pays 2; one holding the handshake pays 1;"
    );
    println!("one folding the pushed events pays 0 until its reconciliation floor.");
    println!("the desktop's actual figure is measured in desktop/src-tauri/src/daemon_bridge.rs:");
    println!(
        "  tests::ten_refreshes_cost_one_handshake_and_ten_listings      (M4(a): 11 connections / 10 refreshes)"
    );
    println!(
        "  tests::ten_folded_refreshes_cost_one_handshake_and_one_listing (M4(b):  2 connections / 10 refreshes)"
    );
    println!(
        "watcher coalesce floor = 150 ms => <= 6.667 refresh/s (unchanged; it caps EMITS, not fetches)"
    );
    let median_ms = median as f64 / 1000.0;
    println!(
        "at median list_agents latency {median_ms:.3} ms, a refresh that does fetch costs ~{median_ms:.3} ms of the 150 ms budget"
    );

    registry.shutdown_all();
    server.abort();
    println!("\nOK");
}
