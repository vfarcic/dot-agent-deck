//! Issue #1993, OpenCode's side: text OpenCode adds to a user message itself
//! is marked `synthetic`, and the plugin does not report it as the user's
//! prompt.

use super::tests::run_plugin_driver;

/// Run `body` against the plugin and return the prompts it reported, plus the
/// prompt the card shows once the deck has applied every report.
fn reported(body: &str) -> Option<(Vec<String>, Option<String>)> {
    let payloads = run_plugin_driver(
        &format!(
            r#"
const hooks = await plugin({{ directory: "/work" }});
const send = (type, properties) => hooks.event({{ event: {{ type, properties }} }});
const user = (id) => send("message.updated", {{ sessionID: "root", info: {{ id, sessionID: "root", role: "user", directory: "/work" }} }});
const text = (messageID, n, t, synthetic) => send("message.part.updated", {{ sessionID: "root", part: {{ id: messageID + "-" + n, sessionID: "root", messageID, type: "text", text: t, synthetic }} }});
await send("session.created", {{ sessionID: "root", info: {{ id: "root", directory: "/work" }} }});
{body}
"#
        ),
        &[],
    )?;
    let prompts = payloads
        .iter()
        .filter(|p| p["event"] == "session.prompt")
        .map(|p| p["prompt"].as_str().unwrap_or_default().to_string())
        .collect();
    // The driver's exit sends `session.deleted`, which would close the card.
    let mut state = crate::state::AppState::default();
    for payload in payloads.iter().filter(|p| p["event"] != "session.deleted") {
        let event = crate::hook::build_opencode_event(
            serde_json::from_value(payload.clone()).expect("the deck decodes every payload"),
        )
        .expect("every payload maps to an event");
        state.apply_event(event);
    }
    let session = payloads
        .iter()
        .find_map(|p| p["session_id"].as_str())
        .expect("the plugin reported the session");
    let card = state.sessions[session].last_user_prompt.clone();
    Some((prompts, card))
}

/// A user message whose only text is synthetic — OpenCode's own turn after a
/// background task or a compaction — is not reported, and the card keeps the
/// prompt the user typed before it.
#[test]
fn opencode_plugin_does_not_report_its_own_synthetic_turn() {
    let Some((prompts, card)) = reported(
        r#"
await user("typed");
await text("typed", 1, "fix the login bug");
await user("injected");
await text("injected", 1, "Summarize the task tool output above and continue with your task.", true);
"#,
    ) else {
        return;
    };
    assert_eq!(prompts, vec!["fix the login bug"]);
    assert_eq!(card.as_deref(), Some("fix the login bug"));
}

/// A typed message that OpenCode gave synthetic text first (a file it read
/// for an attachment) still reports the text the user typed.
#[test]
fn opencode_plugin_reports_the_typed_text_after_a_synthetic_part() {
    let Some((prompts, card)) = reported(
        r#"
await user("attached");
await text("attached", 1, "Called the Read tool with the following input: {\"filePath\":\"/work/a.rs\"}", true);
await text("attached", 2, "explain this file");
"#,
    ) else {
        return;
    };
    assert_eq!(prompts, vec!["explain this file"]);
    assert_eq!(card.as_deref(), Some("explain this file"));
}
