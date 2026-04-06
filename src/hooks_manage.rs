use std::path::PathBuf;

use serde_json::{Value, json};

const HOOK_TYPES: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "Notification",
    "Stop",
    "PreCompact",
    "SubagentStart",
    "SubagentStop",
];

fn settings_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".claude").join("settings.json")
}

fn read_settings(path: &PathBuf) -> Value {
    match std::fs::read_to_string(path) {
        Ok(contents) => serde_json::from_str(&contents).unwrap_or_else(|_| json!({})),
        Err(_) => json!({}),
    }
}

fn write_settings(path: &PathBuf, settings: &Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let contents = serde_json::to_string_pretty(settings)?;
    std::fs::write(path, contents)
}

/// Build a rule object in the new hooks format:
/// `{ "hooks": [{"type": "command", "command": "..."}] }`
/// For Notification, adds a matcher for permission_prompt.
fn make_rule(binary_path: &str, hook_type: &str) -> Value {
    let command_obj = json!({
        "type": "command",
        "command": format!("{binary_path} hook")
    });

    if hook_type == "Notification" {
        json!({
            "matcher": "permission_prompt",
            "hooks": [command_obj]
        })
    } else {
        json!({
            "hooks": [command_obj]
        })
    }
}

/// Ensure `settings["hooks"]` is an object and return a mutable reference to it.
fn ensure_hooks_object(settings: &mut Value) -> &mut serde_json::Map<String, Value> {
    let obj = settings
        .as_object_mut()
        .expect("settings must be an object");
    if !obj.contains_key("hooks") || !obj["hooks"].is_object() {
        obj.insert("hooks".into(), json!({}));
    }
    obj.get_mut("hooks").unwrap().as_object_mut().unwrap()
}

/// Ensure `hooks_obj[hook_type]` is an array and return a mutable reference.
fn ensure_hook_array<'a>(
    hooks_obj: &'a mut serde_json::Map<String, Value>,
    hook_type: &str,
) -> &'a mut Vec<Value> {
    if !hooks_obj.contains_key(hook_type) || !hooks_obj[hook_type].is_array() {
        hooks_obj.insert(hook_type.into(), json!([]));
    }
    hooks_obj
        .get_mut(hook_type)
        .unwrap()
        .as_array_mut()
        .unwrap()
}

/// Check if a rule is in the old flat format: `{"type":"command","command":"..."}` without a `hooks` key.
fn is_old_format(rule: &Value) -> bool {
    rule.get("command").is_some() && rule.get("hooks").is_none()
}

/// A rule is well-formed if it has no matcher or the matcher is a string.
fn is_well_formed_rule(rule: &Value) -> bool {
    match rule.get("matcher") {
        None => true,
        Some(m) => m.is_string(),
    }
}

fn has_valid_new_format_rule(rules: &[Value]) -> bool {
    rules.iter().any(|rule| {
        !is_old_format(rule) && is_well_formed_rule(rule) && rule_contains_dot_agent_deck(rule)
    })
}

fn install_impl(settings: &mut Value, binary_path: &str) -> (Vec<&'static str>, Vec<&'static str>) {
    let hooks_obj = ensure_hooks_object(settings);

    // Clean up dot-agent-deck entries for hook types no longer in HOOK_TYPES
    let all_keys: Vec<String> = hooks_obj.keys().cloned().collect();
    for key in all_keys {
        if !HOOK_TYPES.contains(&key.as_str()) {
            if let Some(arr) = hooks_obj.get_mut(&key).and_then(|v| v.as_array_mut()) {
                arr.retain(|rule| !rule_contains_dot_agent_deck(rule));
            }
            // Remove the key entirely if the array is now empty
            if hooks_obj
                .get(&key)
                .and_then(|v| v.as_array())
                .is_some_and(|a| a.is_empty())
            {
                hooks_obj.remove(&key);
            }
        }
    }

    let mut installed = Vec::new();
    let mut skipped = Vec::new();

    for &hook_type in HOOK_TYPES {
        let rules = ensure_hook_array(hooks_obj, hook_type);

        // If we already have a valid new-format entry, skip
        if has_valid_new_format_rule(rules) {
            skipped.push(hook_type);
            continue;
        }

        // Remove any old-format or malformed dot-agent-deck entries before adding
        rules.retain(|rule| !rule_contains_dot_agent_deck(rule));

        rules.push(make_rule(binary_path, hook_type));
        installed.push(hook_type);
    }

    (installed, skipped)
}

fn uninstall_impl(settings: &mut Value) -> Vec<&'static str> {
    let hooks = match settings.get_mut("hooks").and_then(|h| h.as_object_mut()) {
        Some(h) => h,
        None => return Vec::new(),
    };

    let mut removed = Vec::new();

    for &hook_type in HOOK_TYPES {
        if let Some(arr) = hooks.get_mut(hook_type).and_then(|v| v.as_array_mut()) {
            let before = arr.len();
            arr.retain(|rule| !rule_contains_dot_agent_deck(rule));
            if arr.len() < before {
                removed.push(hook_type);
            }
        }
    }

    removed
}

fn rule_contains_dot_agent_deck(rule: &Value) -> bool {
    // New format: { "hooks": [{"command": "...dot-agent-deck..."}] }
    let new_format = rule
        .get("hooks")
        .and_then(|h| h.as_array())
        .is_some_and(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("command")
                    .and_then(|c| c.as_str())
                    .is_some_and(|cmd| cmd.contains("dot-agent-deck"))
            })
        });
    // Old format: { "command": "...dot-agent-deck..." }
    let old_format = rule
        .get("command")
        .and_then(|c| c.as_str())
        .is_some_and(|cmd| cmd.contains("dot-agent-deck"));
    new_format || old_format
}

pub fn install() {
    let binary_path = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "dot-agent-deck".into());

    let path = settings_path();
    let mut settings = read_settings(&path);

    let (installed, skipped) = install_impl(&mut settings, &binary_path);

    if let Err(e) = write_settings(&path, &settings) {
        eprintln!("Error writing {}: {e}", path.display());
        return;
    }

    if !installed.is_empty() {
        println!("Installed hooks: {}", installed.join(", "));
    }
    if !skipped.is_empty() {
        println!("Already installed (skipped): {}", skipped.join(", "));
    }
    println!("Settings file: {}", path.display());
}

pub fn uninstall() {
    let path = settings_path();
    let mut settings = read_settings(&path);

    let removed = uninstall_impl(&mut settings);

    if let Err(e) = write_settings(&path, &settings) {
        eprintln!("Error writing {}: {e}", path.display());
        return;
    }

    if removed.is_empty() {
        println!("No dot-agent-deck hooks found to remove.");
    } else {
        println!("Removed hooks: {}", removed.join(", "));
    }
    println!("Settings file: {}", path.display());
}

// --- Testable versions that accept a custom path ---

pub fn install_to(path: &PathBuf, binary_path: &str) {
    let mut settings = read_settings(path);
    install_impl(&mut settings, binary_path);
    write_settings(path, &settings).expect("failed to write settings");
}

pub fn uninstall_from(path: &PathBuf) {
    let mut settings = read_settings(path);
    uninstall_impl(&mut settings);
    write_settings(path, &settings).expect("failed to write settings");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_settings() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        (dir, path)
    }

    #[test]
    fn install_adds_hooks_to_empty_settings() {
        let (_dir, path) = temp_settings();
        install_to(&path, "/usr/local/bin/dot-agent-deck");

        let settings: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let hooks = settings["hooks"].as_object().unwrap();

        for hook_type in HOOK_TYPES {
            let rules = hooks[*hook_type].as_array().unwrap();
            assert_eq!(rules.len(), 1, "Expected 1 rule for {hook_type}");

            let rule = &rules[0];
            let inner_hooks = rule["hooks"].as_array().unwrap();
            assert_eq!(inner_hooks.len(), 1);
            let cmd = inner_hooks[0]["command"].as_str().unwrap();
            assert!(cmd.contains("dot-agent-deck hook"));
        }

        // Notification rule should have a string matcher
        let notif_rule = &hooks["Notification"].as_array().unwrap()[0];
        assert_eq!(notif_rule["matcher"].as_str(), Some("permission_prompt"));
    }

    #[test]
    fn install_preserves_existing_hooks() {
        let (_dir, path) = temp_settings();

        // Existing rule in the new format
        let existing = json!({
            "hooks": {
                "SessionStart": [
                    {
                        "hooks": [
                            {"type": "command", "command": "my-other-tool start"}
                        ]
                    }
                ]
            }
        });
        std::fs::write(&path, serde_json::to_string(&existing).unwrap()).unwrap();

        install_to(&path, "/usr/local/bin/dot-agent-deck");

        let settings: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let session_start = settings["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(session_start.len(), 2);
        // First rule is the original
        assert_eq!(
            session_start[0]["hooks"][0]["command"].as_str(),
            Some("my-other-tool start")
        );
        // Second rule is ours
        assert!(
            session_start[1]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .contains("dot-agent-deck")
        );
    }

    #[test]
    fn install_is_idempotent() {
        let (_dir, path) = temp_settings();

        install_to(&path, "/usr/local/bin/dot-agent-deck");
        install_to(&path, "/usr/local/bin/dot-agent-deck");

        let settings: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        for hook_type in HOOK_TYPES {
            let rules = settings["hooks"][*hook_type].as_array().unwrap();
            assert_eq!(rules.len(), 1, "Duplicate rule for {hook_type}");
        }
    }

    #[test]
    fn uninstall_removes_only_dot_agent_deck_entries() {
        let (_dir, path) = temp_settings();

        let existing = json!({
            "hooks": {
                "SessionStart": [
                    {
                        "hooks": [
                            {"type": "command", "command": "my-other-tool start"}
                        ]
                    },
                    {
                        "hooks": [
                            {"type": "command", "command": "/usr/local/bin/dot-agent-deck hook"}
                        ]
                    }
                ],
                "PreToolUse": [
                    {
                        "hooks": [
                            {"type": "command", "command": "/usr/local/bin/dot-agent-deck hook"}
                        ]
                    }
                ]
            }
        });
        std::fs::write(&path, serde_json::to_string(&existing).unwrap()).unwrap();

        uninstall_from(&path);

        let settings: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let session_start = settings["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(session_start.len(), 1);
        assert_eq!(
            session_start[0]["hooks"][0]["command"].as_str(),
            Some("my-other-tool start")
        );

        let pre_tool = settings["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre_tool.len(), 0);
    }

    #[test]
    fn install_upgrades_old_format_to_new() {
        let (_dir, path) = temp_settings();

        // Old flat format
        let existing = json!({
            "hooks": {
                "SessionStart": [
                    {"type": "command", "command": "/usr/local/bin/dot-agent-deck hook"}
                ],
                "PreToolUse": [
                    {"type": "command", "command": "/usr/local/bin/dot-agent-deck hook"}
                ],
                "SessionEnd": [
                    {"type": "command", "command": "/usr/local/bin/dot-agent-deck hook"}
                ]
            }
        });
        std::fs::write(&path, serde_json::to_string(&existing).unwrap()).unwrap();

        install_to(&path, "/usr/local/bin/dot-agent-deck");

        let settings: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();

        // Old-format entries should be replaced with new-format
        for hook_type in HOOK_TYPES {
            let rules = settings["hooks"][*hook_type].as_array().unwrap();
            assert_eq!(rules.len(), 1, "Expected 1 rule for {hook_type}");
            // Must be new format (has "hooks" key)
            assert!(
                rules[0].get("hooks").is_some(),
                "Expected new format for {hook_type}"
            );
        }

        // SessionEnd old-format entry should have been upgraded to new format
        let session_end = settings["hooks"]["SessionEnd"].as_array().unwrap();
        assert_eq!(
            session_end.len(),
            1,
            "SessionEnd should have 1 upgraded rule"
        );
        assert!(
            session_end[0].get("hooks").is_some(),
            "SessionEnd should be new format"
        );
    }

    #[test]
    fn install_fixes_malformed_matcher() {
        let (_dir, path) = temp_settings();

        // Simulate the broken object matcher from the earlier bug
        let existing = json!({
            "hooks": {
                "Notification": [
                    {
                        "matcher": { "notification_type": "permission_prompt" },
                        "hooks": [
                            {"type": "command", "command": "/usr/local/bin/dot-agent-deck hook"}
                        ]
                    }
                ]
            }
        });
        std::fs::write(&path, serde_json::to_string(&existing).unwrap()).unwrap();

        install_to(&path, "/usr/local/bin/dot-agent-deck");

        let settings: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let notif = settings["hooks"]["Notification"].as_array().unwrap();
        assert_eq!(notif.len(), 1);
        // Matcher should now be a string
        assert_eq!(notif[0]["matcher"].as_str(), Some("permission_prompt"));
    }

    #[test]
    fn uninstall_noop_on_empty_settings() {
        let (_dir, path) = temp_settings();
        std::fs::write(&path, "{}").unwrap();
        uninstall_from(&path); // Should not panic
    }

    #[test]
    fn uninstall_noop_when_no_file() {
        let (_dir, path) = temp_settings();
        uninstall_from(&path); // Should not panic
    }
}
