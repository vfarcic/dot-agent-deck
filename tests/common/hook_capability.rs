//! Opt-in capability capture for synthetic pane processes only.

/// Let a stand-in retain its own injected capability in its private test cwd.
/// The registry's token reader is intentionally in-crate-only.
#[allow(dead_code)]
pub fn capability_export_command(command: &str) -> String {
    format!(
        "(umask 077; printf '%s' \"$DOT_AGENT_DECK_PANE_CAPABILITY\" > \".test-capability-$DOT_AGENT_DECK_AGENT_ID\"); {command}"
    )
}

/// Read only a token exported by the selected stand-in; never log its value.
#[allow(dead_code)]
pub async fn recorded_hook_capability(cwd: &std::path::Path, agent_id: &str) -> Option<String> {
    let path = cwd.join(format!(".test-capability-{agent_id}"));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        if let Ok(token) = std::fs::read_to_string(&path)
            && token.len() == 64
            && token.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Some(token);
        }
        assert!(
            std::time::Instant::now() < deadline,
            "stand-in did not export its capability"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}
