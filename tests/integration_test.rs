use std::io::Write as _;
use std::sync::Arc;

use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::RwLock;

use dot_agent_deck::daemon::run_daemon;
use dot_agent_deck::state::{AppState, SessionStatus};

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
        run_daemon(&daemon_sock, daemon_state).await.unwrap();
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
        run_daemon(&daemon_sock, daemon_state).await.unwrap();
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
        run_daemon(&daemon_sock, daemon_state).await.unwrap();
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
        run_daemon(&daemon_sock, daemon_state).await.unwrap();
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
        run_daemon(&daemon_sock, daemon_state).await.unwrap();
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
        run_daemon(&daemon_sock, daemon_state).await.unwrap();
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
        run_daemon(&daemon_sock, daemon_state).await.unwrap();
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
