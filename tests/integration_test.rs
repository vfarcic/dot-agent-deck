use std::io::Write as _;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::RwLock;

use dot_agent_deck::daemon::run_daemon;
use dot_agent_deck::state::{AppState, SessionStatus, new_permission_responders};

fn event_json(session_id: &str, event_type: &str) -> String {
    format!(
        r#"{{"session_id":"{}","agent_type":"claude_code","event_type":"{}","timestamp":"2026-03-22T10:00:00Z"}}"#,
        session_id, event_type
    )
}

fn opencode_event_json(session_id: &str, event_type: &str) -> String {
    format!(
        r#"{{"session_id":"{}","agent_type":"open_code","event_type":"{}","timestamp":"2026-03-22T10:00:00Z"}}"#,
        session_id, event_type
    )
}

fn prompt_event_json(session_id: &str, prompt: &str) -> String {
    format!(
        r#"{{"session_id":"{}","agent_type":"claude_code","event_type":"thinking","user_prompt":"{}","timestamp":"2026-03-22T10:00:00Z"}}"#,
        session_id, prompt
    )
}

fn tool_event_json(session_id: &str, tool_name: &str) -> String {
    format!(
        r#"{{"session_id":"{}","agent_type":"claude_code","event_type":"tool_start","tool_name":"{}","timestamp":"2026-03-22T10:00:00Z"}}"#,
        session_id, tool_name
    )
}

#[tokio::test]
async fn single_session_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("test.sock");
    let state = Arc::new(RwLock::new(AppState::default()));

    let daemon_state = state.clone();
    let daemon_sock = sock_path.clone();
    let handle = tokio::spawn(async move {
        run_daemon(&daemon_sock, daemon_state, new_permission_responders())
            .await
            .unwrap();
    });

    // Wait for socket to be ready
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let mut stream = UnixStream::connect(&sock_path).await.unwrap();

    // Send session start
    let msg = format!("{}\n", event_json("s1", "session_start"));
    stream.write_all(msg.as_bytes()).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    {
        let s = state.read().await;
        assert!(s.sessions.contains_key("s1"));
        assert_eq!(s.sessions["s1"].status, SessionStatus::Idle);
    }

    // Send tool start
    let msg = format!("{}\n", tool_event_json("s1", "Read"));
    stream.write_all(msg.as_bytes()).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    {
        let s = state.read().await;
        assert_eq!(s.sessions["s1"].status, SessionStatus::Working);
    }

    // Send tool end
    let msg = format!("{}\n", event_json("s1", "tool_end"));
    stream.write_all(msg.as_bytes()).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    {
        let s = state.read().await;
        assert_eq!(s.sessions["s1"].status, SessionStatus::Working);
    }

    // Send session end
    let msg = format!("{}\n", event_json("s1", "session_end"));
    stream.write_all(msg.as_bytes()).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    {
        let s = state.read().await;
        assert!(!s.sessions.contains_key("s1"));
    }

    handle.abort();
}

#[tokio::test]
async fn multiple_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("test.sock");
    let state = Arc::new(RwLock::new(AppState::default()));

    let daemon_state = state.clone();
    let daemon_sock = sock_path.clone();
    let handle = tokio::spawn(async move {
        run_daemon(&daemon_sock, daemon_state, new_permission_responders())
            .await
            .unwrap();
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let mut stream = UnixStream::connect(&sock_path).await.unwrap();

    let msg = format!(
        "{}\n{}\n",
        event_json("s1", "session_start"),
        event_json("s2", "session_start")
    );
    stream.write_all(msg.as_bytes()).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    {
        let s = state.read().await;
        assert_eq!(s.sessions.len(), 2);
        assert!(s.sessions.contains_key("s1"));
        assert!(s.sessions.contains_key("s2"));
    }

    handle.abort();
}

#[tokio::test]
async fn hook_handler_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("test.sock");
    let state = Arc::new(RwLock::new(AppState::default()));

    let daemon_state = state.clone();
    let daemon_sock = sock_path.clone();
    let handle = tokio::spawn(async move {
        run_daemon(&daemon_sock, daemon_state, new_permission_responders())
            .await
            .unwrap();
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Point the hook at our test socket
    // SAFETY: This test runs in isolation; no other thread reads this env var concurrently.
    unsafe {
        std::env::set_var("DOT_AGENT_DECK_SOCKET", sock_path.to_str().unwrap());
    }

    // Simulate hook by writing directly to socket (since handle_hook reads stdin)
    // Instead, construct an AgentEvent and send it like the hook would
    {
        let mut stream = std::os::unix::net::UnixStream::connect(&sock_path).unwrap();
        let event = serde_json::json!({
            "session_id": "hook-test-1",
            "agent_type": "claude_code",
            "event_type": "session_start",
            "timestamp": "2026-03-22T10:00:00Z"
        });
        writeln!(stream, "{}", serde_json::to_string(&event).unwrap()).unwrap();
    }

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    {
        let s = state.read().await;
        assert!(s.sessions.contains_key("hook-test-1"));
        assert_eq!(s.sessions["hook-test-1"].status, SessionStatus::Idle);
    }

    // Send a tool_start event
    {
        let mut stream = std::os::unix::net::UnixStream::connect(&sock_path).unwrap();
        let event = serde_json::json!({
            "session_id": "hook-test-1",
            "agent_type": "claude_code",
            "event_type": "tool_start",
            "tool_name": "Bash",
            "tool_detail": "cargo test",
            "timestamp": "2026-03-22T10:00:01Z"
        });
        writeln!(stream, "{}", serde_json::to_string(&event).unwrap()).unwrap();
    }

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    {
        let s = state.read().await;
        assert_eq!(s.sessions["hook-test-1"].status, SessionStatus::Working);
        let tool = s.sessions["hook-test-1"].active_tool.as_ref().unwrap();
        assert_eq!(tool.name, "Bash");
    }

    handle.abort();
}

#[tokio::test]
async fn malformed_json_resilience() {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("test.sock");
    let state = Arc::new(RwLock::new(AppState::default()));

    let daemon_state = state.clone();
    let daemon_sock = sock_path.clone();
    let handle = tokio::spawn(async move {
        run_daemon(&daemon_sock, daemon_state, new_permission_responders())
            .await
            .unwrap();
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let mut stream = UnixStream::connect(&sock_path).await.unwrap();

    // Send garbage, then a valid event
    let msg = format!("not json\n{}\n", event_json("s1", "session_start"));
    stream.write_all(msg.as_bytes()).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    {
        let s = state.read().await;
        assert!(s.sessions.contains_key("s1"));
    }

    handle.abort();
}

#[tokio::test]
async fn user_prompt_flows_through_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("test.sock");
    let state = Arc::new(RwLock::new(AppState::default()));

    let daemon_state = state.clone();
    let daemon_sock = sock_path.clone();
    let handle = tokio::spawn(async move {
        run_daemon(&daemon_sock, daemon_state, new_permission_responders())
            .await
            .unwrap();
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let mut stream = UnixStream::connect(&sock_path).await.unwrap();

    // Start session then send thinking event with prompt
    let msg = format!(
        "{}\n{}\n",
        event_json("s1", "session_start"),
        prompt_event_json("s1", "fix the login bug")
    );
    stream.write_all(msg.as_bytes()).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    {
        let s = state.read().await;
        assert_eq!(s.sessions["s1"].status, SessionStatus::Thinking);
        assert_eq!(
            s.sessions["s1"].last_user_prompt.as_deref(),
            Some("fix the login bug")
        );
    }

    // Send another event without prompt — prompt should persist
    let msg = format!("{}\n", event_json("s1", "idle"));
    stream.write_all(msg.as_bytes()).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    {
        let s = state.read().await;
        assert_eq!(
            s.sessions["s1"].last_user_prompt.as_deref(),
            Some("fix the login bug")
        );
    }

    handle.abort();
}

#[tokio::test]
async fn opencode_session_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("test.sock");
    let state = Arc::new(RwLock::new(AppState::default()));

    let daemon_state = state.clone();
    let daemon_sock = sock_path.clone();
    let handle = tokio::spawn(async move {
        run_daemon(&daemon_sock, daemon_state, new_permission_responders())
            .await
            .unwrap();
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let mut stream = UnixStream::connect(&sock_path).await.unwrap();

    // Send OpenCode session start
    let msg = format!("{}\n", opencode_event_json("oc1", "session_start"));
    stream.write_all(msg.as_bytes()).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    {
        let s = state.read().await;
        assert!(s.sessions.contains_key("oc1"));
        assert_eq!(s.sessions["oc1"].status, SessionStatus::Idle);
        assert_eq!(
            s.sessions["oc1"].agent_type,
            dot_agent_deck::event::AgentType::OpenCode
        );
    }

    // Send tool start
    let msg = r#"{"session_id":"oc1","agent_type":"open_code","event_type":"tool_start","tool_name":"Bash","timestamp":"2026-03-22T10:00:01Z"}"#
        .to_string();
    stream
        .write_all(format!("{msg}\n").as_bytes())
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    {
        let s = state.read().await;
        assert_eq!(s.sessions["oc1"].status, SessionStatus::Working);
    }

    // Send session end
    let msg = format!("{}\n", opencode_event_json("oc1", "session_end"));
    stream.write_all(msg.as_bytes()).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    {
        let s = state.read().await;
        assert!(!s.sessions.contains_key("oc1"));
    }

    handle.abort();
}

#[tokio::test]
async fn mixed_agent_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("test.sock");
    let state = Arc::new(RwLock::new(AppState::default()));

    let daemon_state = state.clone();
    let daemon_sock = sock_path.clone();
    let handle = tokio::spawn(async move {
        run_daemon(&daemon_sock, daemon_state, new_permission_responders())
            .await
            .unwrap();
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let mut stream = UnixStream::connect(&sock_path).await.unwrap();

    // Start both Claude Code and OpenCode sessions
    let msg = format!(
        "{}\n{}\n",
        event_json("cc1", "session_start"),
        opencode_event_json("oc1", "session_start")
    );
    stream.write_all(msg.as_bytes()).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    {
        let s = state.read().await;
        assert_eq!(s.sessions.len(), 2);
        assert_eq!(
            s.sessions["cc1"].agent_type,
            dot_agent_deck::event::AgentType::ClaudeCode
        );
        assert_eq!(
            s.sessions["oc1"].agent_type,
            dot_agent_deck::event::AgentType::OpenCode
        );
    }

    handle.abort();
}

fn permission_event_json(session_id: &str, tool_use_id: &str, tool_name: &str) -> String {
    format!(
        r#"{{"session_id":"{}","agent_type":"claude_code","event_type":"permission_request","tool_name":"{}","tool_detail":"cargo test","timestamp":"2026-03-22T10:00:00Z","metadata":{{"tool_use_id":"{}","permission_state":"pending"}}}}"#,
        session_id, tool_name, tool_use_id
    )
}

#[tokio::test]
async fn permission_request_approve_sends_allow_decision() {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("test.sock");
    let state = Arc::new(RwLock::new(AppState::default()));
    let responders = new_permission_responders();

    let daemon_state = state.clone();
    let daemon_sock = sock_path.clone();
    let daemon_responders = responders.clone();
    let handle = tokio::spawn(async move {
        run_daemon(&daemon_sock, daemon_state, daemon_responders)
            .await
            .unwrap();
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // First, start a session on one connection
    {
        let mut stream = UnixStream::connect(&sock_path).await.unwrap();
        let msg = format!("{}\n", event_json("perm-s1", "session_start"));
        stream.write_all(msg.as_bytes()).await.unwrap();
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Send permission request on a separate connection (daemon keeps it open for the response)
    let mut perm_stream = UnixStream::connect(&sock_path).await.unwrap();
    let msg = format!("{}\n", permission_event_json("perm-s1", "tui-123", "Bash"));
    perm_stream.write_all(msg.as_bytes()).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Verify pending permission appears in state
    {
        let s = state.read().await;
        assert_eq!(s.sessions["perm-s1"].status, SessionStatus::WaitingForInput);
        let perm = s.sessions["perm-s1"].next_pending_permission().unwrap();
        assert_eq!(perm.tool_use_id, "tui-123");
        assert_eq!(perm.tool_name.as_deref(), Some("Bash"));
    }

    // Verify responder is registered
    assert!(responders.lock().unwrap().contains_key("tui-123"));

    // Simulate TUI pressing 'y': send "allow" through the oneshot channel
    {
        let tx = responders.lock().unwrap().remove("tui-123").unwrap();
        tx.send("allow".to_string()).unwrap();
    }

    // Read the response from the socket — the daemon writes the decision back
    let (read_half, _write_half) = perm_stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut response_line = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        reader.read_line(&mut response_line),
    )
    .await
    .expect("timed out waiting for permission response")
    .expect("failed to read response");

    let response: serde_json::Value = serde_json::from_str(response_line.trim()).unwrap();
    assert_eq!(response["decision"], "allow");

    // Resolve the permission from state (as the TUI would)
    {
        let mut s = state.write().await;
        s.resolve_permission("perm-s1", "tui-123");
    }

    // Verify state is cleaned up
    {
        let s = state.read().await;
        assert!(s.sessions["perm-s1"].next_pending_permission().is_none());
    }

    handle.abort();
}

#[tokio::test]
async fn permission_request_deny_sends_deny_decision() {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("test.sock");
    let state = Arc::new(RwLock::new(AppState::default()));
    let responders = new_permission_responders();

    let daemon_state = state.clone();
    let daemon_sock = sock_path.clone();
    let daemon_responders = responders.clone();
    let handle = tokio::spawn(async move {
        run_daemon(&daemon_sock, daemon_state, daemon_responders)
            .await
            .unwrap();
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Start session
    {
        let mut stream = UnixStream::connect(&sock_path).await.unwrap();
        let msg = format!("{}\n", event_json("deny-s1", "session_start"));
        stream.write_all(msg.as_bytes()).await.unwrap();
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Send permission request
    let mut perm_stream = UnixStream::connect(&sock_path).await.unwrap();
    let msg = format!("{}\n", permission_event_json("deny-s1", "tui-456", "Write"));
    perm_stream.write_all(msg.as_bytes()).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Send deny decision
    {
        let tx = responders.lock().unwrap().remove("tui-456").unwrap();
        tx.send("deny".to_string()).unwrap();
    }

    // Read response
    let (read_half, _write_half) = perm_stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut response_line = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        reader.read_line(&mut response_line),
    )
    .await
    .expect("timed out waiting for permission response")
    .expect("failed to read response");

    let response: serde_json::Value = serde_json::from_str(response_line.trim()).unwrap();
    assert_eq!(response["decision"], "deny");

    handle.abort();
}

#[tokio::test]
async fn multiple_concurrent_permission_requests() {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("test.sock");
    let state = Arc::new(RwLock::new(AppState::default()));
    let responders = new_permission_responders();

    let daemon_state = state.clone();
    let daemon_sock = sock_path.clone();
    let daemon_responders = responders.clone();
    let handle = tokio::spawn(async move {
        run_daemon(&daemon_sock, daemon_state, daemon_responders)
            .await
            .unwrap();
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Start two sessions
    {
        let mut stream = UnixStream::connect(&sock_path).await.unwrap();
        let msg = format!(
            "{}\n{}\n",
            event_json("multi-s1", "session_start"),
            event_json("multi-s2", "session_start")
        );
        stream.write_all(msg.as_bytes()).await.unwrap();
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Send permission requests on separate connections
    let mut perm_stream1 = UnixStream::connect(&sock_path).await.unwrap();
    let msg = format!(
        "{}\n",
        permission_event_json("multi-s1", "multi-perm-1", "Bash")
    );
    perm_stream1.write_all(msg.as_bytes()).await.unwrap();

    let mut perm_stream2 = UnixStream::connect(&sock_path).await.unwrap();
    let msg = format!(
        "{}\n",
        permission_event_json("multi-s2", "multi-perm-2", "Edit")
    );
    perm_stream2.write_all(msg.as_bytes()).await.unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Both sessions should have pending permissions
    {
        let s = state.read().await;
        assert!(s.sessions["multi-s1"].next_pending_permission().is_some());
        assert!(s.sessions["multi-s2"].next_pending_permission().is_some());
    }

    // Both responders should be registered
    {
        let map = responders.lock().unwrap();
        assert!(map.contains_key("multi-perm-1"));
        assert!(map.contains_key("multi-perm-2"));
    }

    // Approve first, deny second
    {
        let tx1 = responders.lock().unwrap().remove("multi-perm-1").unwrap();
        tx1.send("allow".to_string()).unwrap();
    }
    {
        let tx2 = responders.lock().unwrap().remove("multi-perm-2").unwrap();
        tx2.send("deny".to_string()).unwrap();
    }

    // Verify responses
    let (read1, _) = perm_stream1.into_split();
    let mut reader1 = BufReader::new(read1);
    let mut resp1 = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        reader1.read_line(&mut resp1),
    )
    .await
    .unwrap()
    .unwrap();
    let r1: serde_json::Value = serde_json::from_str(resp1.trim()).unwrap();
    assert_eq!(r1["decision"], "allow");

    let (read2, _) = perm_stream2.into_split();
    let mut reader2 = BufReader::new(read2);
    let mut resp2 = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        reader2.read_line(&mut resp2),
    )
    .await
    .unwrap()
    .unwrap();
    let r2: serde_json::Value = serde_json::from_str(resp2.trim()).unwrap();
    assert_eq!(r2["decision"], "deny");

    handle.abort();
}
