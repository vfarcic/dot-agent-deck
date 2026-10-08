//! Pure-data contracts for replies sourced from an agent's finished turn.

use dot_agent_deck::daemon_protocol::{
    AttachRequest, CAP_TURN_REPLIES, DAEMON_CAPABILITIES, MAX_TURN_REPLY_BYTES,
};
use dot_agent_deck::hook::extract_turn_reply;
use dot_agent_deck::quota_signals::extract_codex_turn_reply;
use serde_json::{Value, json};

// Verbatim task_complete from a real local Codex rollout on 2026-10-02.
// Its reply is just "OK"; no tool output, project data or credentials are kept.
const CAPTURED_CODEX_COMPLETE: &str =
    include_str!("fixtures/turn-replies/codex-task-complete.json");

/// Scenario: Decode a Claude Stop payload containing its final assistant reply. The extracted text must retain the reply verbatim and identify a successful turn.
#[test]
fn claude_stop_reply_is_extracted() {
    let reply = extract_turn_reply(&json!({
        "session_id": "claude-reading-session",
        "hook_event_name": "Stop",
        "last_assistant_message": "REPLY_SENTINEL_1497: all tests passed."
    }))
    .expect("Stop's last_assistant_message is a final reply");
    assert_eq!(reply.text, "REPLY_SENTINEL_1497: all tests passed.");
    assert!(!reply.failed);
}

/// Scenario: Decode Stop payloads whose last assistant message is missing, null, or an unexpected JSON type. They must produce no reply without losing the hook to a panic or parse error.
#[test]
fn claude_stop_without_reply_is_tolerated() {
    for extra in [
        json!({}),
        json!({"last_assistant_message": null}),
        json!({"last_assistant_message": {"text": "unexpected"}}),
    ] {
        let mut payload =
            json!({"session_id": "claude-reading-session", "hook_event_name": "Stop"});
        payload
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(
            extract_turn_reply(&payload).is_none(),
            "no string reply in {payload}"
        );
    }
}

/// Scenario: Supply a last-assistant-message field on a tool, notification, and subagent hook, and on a Stop explicitly attributed to a subagent. None may be read as the pane's main turn ending.
#[test]
fn claude_reply_ignores_tool_and_subagent_payloads() {
    for event in ["PostToolUse", "Notification", "SubagentStop"] {
        assert!(
            extract_turn_reply(&json!({
                "hook_event_name": event, "last_assistant_message": "not the main reply"
            }))
            .is_none()
        );
    }
    assert!(
        extract_turn_reply(&json!({
            "hook_event_name": "Stop", "agent_id": "background-subagent",
            "last_assistant_message": "not the main reply"
        }))
        .is_none()
    );
}

/// Scenario: Extract an oversized Claude reply ending in multibyte characters. Its output must be the longest valid UTF-8 prefix within the fixed eight-KiB cap.
#[test]
fn claude_reply_is_utf8_bounded_to_eight_kib() {
    assert_eq!(MAX_TURN_REPLY_BYTES, 8192);
    let prefix = "x".repeat(MAX_TURN_REPLY_BYTES - 1);
    let oversized = format!("{prefix}🦀{}", "tail".repeat(4096));
    let reply = extract_turn_reply(&json!({
        "hook_event_name": "Stop", "last_assistant_message": oversized
    }))
    .expect("oversized reply is retained with truncation");
    assert_eq!(reply.text, prefix);
    assert!(reply.text.len() <= MAX_TURN_REPLY_BYTES);
}

/// Scenario: Extract a final assistant message on Claude's StopFailure hook. The reply must identify a failed turn rather than sounding like a normal completion.
#[test]
fn claude_failed_turn_reply_is_marked_failed() {
    let reply = extract_turn_reply(&json!({
        "hook_event_name": "StopFailure", "error": "api_error",
        "last_assistant_message": "I could not finish the request."
    }))
    .expect("failed turn's final reply");
    assert_eq!(reply.text, "I could not finish the request.");
    assert!(reply.failed);
}

/// Scenario: Decode an unmodified task_complete record captured from a real Codex rollout. Its last-agent-message and turn identifier must survive extraction, with a normal-turn outcome.
#[test]
fn codex_task_complete_captured_reply_is_extracted() {
    let record: Value = serde_json::from_str(CAPTURED_CODEX_COMPLETE).unwrap();
    let reply = extract_codex_turn_reply(&record).expect("captured task_complete final reply");
    assert_eq!(reply.text, "OK");
    assert_eq!(
        reply.turn_id.as_deref(),
        Some("01a0fd0c-4a03-7cf1-bd44-5dd82f4e99a9")
    );
    assert!(!reply.failed);
}

/// Scenario: Remove the reply from a captured completion and change its record or event type. These shapes must not produce a reply from tool output or other non-completion data.
#[test]
fn codex_reply_ignores_noncompletion_and_absent_reply() {
    let original: Value = serde_json::from_str(CAPTURED_CODEX_COMPLETE).unwrap();
    let mut absent = original.clone();
    absent["payload"]
        .as_object_mut()
        .unwrap()
        .remove("last_agent_message");
    assert!(extract_codex_turn_reply(&absent).is_none());
    absent["payload"]["last_agent_message"] = Value::Null;
    assert!(extract_codex_turn_reply(&absent).is_none());
    for event in ["task_started", "agent_message", "turn_aborted"] {
        let mut record = original.clone();
        record["payload"]["type"] = event.into();
        assert!(extract_codex_turn_reply(&record).is_none());
    }
    let mut wrong_record = original;
    wrong_record["type"] = "response_item".into();
    assert!(extract_codex_turn_reply(&wrong_record).is_none());
}

/// Scenario: Enlarge a captured Codex completion's reply and attach an error object. Extraction must bound the text at a UTF-8 boundary and identify the failed turn.
#[test]
fn codex_failed_reply_is_utf8_bounded() {
    let mut record: Value = serde_json::from_str(CAPTURED_CODEX_COMPLETE).unwrap();
    let prefix = "x".repeat(MAX_TURN_REPLY_BYTES - 1);
    record["payload"]["last_agent_message"] = format!("{prefix}🦀{}", "tail".repeat(4096)).into();
    record["payload"]["error"] = json!({"message": "request failed", "codex_error_info": "other"});
    let reply = extract_codex_turn_reply(&record).expect("bounded failed reply");
    assert_eq!(reply.text, prefix);
    assert!(reply.failed);
}

/// Scenario: Serialize the per-agent reply subscription and inspect the daemon's advertised capability. The verb must be additive and explicitly advertised for a client to send it.
#[test]
fn turn_reply_subscription_has_an_advertised_additive_verb() {
    assert_eq!(CAP_TURN_REPLIES, "turn-replies");
    assert!(DAEMON_CAPABILITIES.contains(&CAP_TURN_REPLIES));
    assert_eq!(
        serde_json::to_value(AttachRequest::SubscribeTurnReplies {
            id: "reading-agent".into()
        })
        .unwrap(),
        json!({"op": "subscribe-turn-replies", "id": "reading-agent"})
    );
}

#[cfg(unix)]
#[path = "../src/test_temp.rs"]
mod test_temp;

/// Scenario: Ask the production client library to subscribe against fake older daemons advertising no capabilities, an empty set, or an unrelated capability. Each must return Unsupported without sending the new request to the peer.
#[cfg(unix)]
#[tokio::test]
async fn client_refuses_turn_reply_subscription_when_capability_is_absent() {
    use dot_agent_deck::daemon_client::{DaemonClient, GatedQuery};
    use dot_agent_deck::daemon_protocol::{
        AttachResponse, KIND_REQ, PROTOCOL_VERSION, read_frame, write_resp,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;

    for advertised in [
        None,
        Some(Vec::<String>::new()),
        Some(vec!["focus-gained".into()]),
    ] {
        let temp = test_temp::tempdir().expect("private fake daemon endpoint");
        let path = temp.path().join("attach.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let non_hello = Arc::new(AtomicUsize::new(0));
        let observed = non_hello.clone();
        let server = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let Some((kind, bytes)) = read_frame(&mut stream).await.unwrap() else {
                    continue;
                };
                assert_eq!(kind, KIND_REQ);
                let request: Value = serde_json::from_slice(&bytes).unwrap();
                let response = if request["op"] == "hello" {
                    AttachResponse {
                        capabilities: advertised.clone(),
                        ..AttachResponse::hello(PROTOCOL_VERSION)
                    }
                } else {
                    observed.fetch_add(1, Ordering::SeqCst);
                    AttachResponse::err("unknown variant subscribe-turn-replies")
                };
                write_resp(&mut stream, &response).await.unwrap();
            }
        });
        let client = DaemonClient::new(path);
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            client.subscribe_turn_replies("reading-agent"),
        )
        .await
        .expect("capability refusal is prompt")
        .expect("unsupported is an outcome, not a server error");
        assert!(
            matches!(result, GatedQuery::Unsupported),
            "client must report reading unavailable on this daemon"
        );
        assert_eq!(
            non_hello.load(Ordering::SeqCst),
            0,
            "client library must withhold the new verb"
        );
        server.abort();
        let _ = server.await;
    }
}
