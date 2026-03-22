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
        assert_eq!(s.sessions["s1"].status, SessionStatus::Idle);
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
