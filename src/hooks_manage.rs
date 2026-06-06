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
        let expected = make_rule(binary_path, hook_type);

        let already_current = rules.iter().any(|rule| rule == &expected);
        let before = rules.len();

        // Always normalize dot-agent-deck entries down to a single fresh rule.
        rules.retain(|rule| !rule_contains_dot_agent_deck(rule));
        let removed = before - rules.len();
        rules.push(expected);

        if already_current && removed == 1 {
            skipped.push(hook_type);
        } else {
            installed.push(hook_type);
        }
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

/// Silently install hooks if Claude Code is detected.
/// Intended for dashboard startup — never prints to stdout.
pub fn auto_install() {
    let path = settings_path();
    if path.parent().is_none_or(|p| !p.exists()) {
        return;
    }

    let binary_path = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "dot-agent-deck".into());

    let mut settings = read_settings(&path);
    let (installed, _skipped) = install_impl(&mut settings, &binary_path);

    if installed.is_empty() {
        return;
    }

    if let Err(e) = write_settings(&path, &settings) {
        tracing::warn!("auto-install: failed to write Claude Code hooks: {e}");
        return;
    }

    tracing::info!("auto-installed Claude Code hooks: {}", installed.join(", "));
}

/// Auto-install to a custom settings path (for testing).
pub fn auto_install_to(path: &PathBuf) {
    if path.parent().is_none_or(|p| !p.exists()) {
        return;
    }

    let binary_path = "dot-agent-deck".to_string();
    let mut settings = read_settings(path);
    let (installed, _skipped) = install_impl(&mut settings, &binary_path);

    if installed.is_empty() {
        return;
    }

    write_settings(path, &settings).expect("failed to write settings");
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
