//! PRD #20 W1 — install Codex's native hooks into the active `CODEX_HOME`.
//!
//! Codex 0.144.4 ships a Claude-Code-compatible hooks engine, so its command
//! hooks POST the same stdin JSON shape Claude does and are ingested by the
//! existing [`crate::hook::handle_hook`] `"codex"` arm. This module writes the
//! hook DEFINITIONS — a `hooks.json` whose every command shells
//! `dot-agent-deck hook --agent codex` — into the Codex home the spawned `codex`
//! child reads, so a live interactive session's prompt / tool / turn events ride
//! the deck's existing raw-`AgentEvent` hook socket (no new wire, no
//! `PROTOCOL_VERSION` bump — rule 12).
//!
//! It is the Codex analog of [`crate::hooks_manage::auto_install`] (Claude):
//! **idempotent-overwrite, guarded, silent**. Two deliberate choices vs. Claude:
//!
//! - We write a SEPARATE `hooks.json` — Codex's highest-precedence,
//!   auto-discovered hook source (`$CODEX_HOME/hooks.json`) — rather than
//!   editing `config.toml`, so the user's real `~/.codex/config.toml` (auth
//!   references, model, project trust, skills, history) is never touched.
//! - We MERGE: any pre-existing user hooks are preserved; only prior
//!   deck-authored (`dot-agent-deck …`) entries are refreshed, so re-installs
//!   never accumulate duplicates.
//!
//! Trust: Codex requires command hooks to be trusted before they run. The deck
//! authors its OWN hook definition (it vets the source — itself), so the wrapper
//! launches `codex` with `--dangerously-bypass-hook-trust` (see [`crate::wrap`]).

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// Codex hook events we install a command handler for. Every name maps to an
/// [`crate::event::EventType`] via [`crate::hook`]'s `map_event_type`, and Codex
/// fires these at the engine level (shared by the interactive TUI and
/// `codex exec`). Covers the lifecycle (`SessionStart`/`Stop`), prompt
/// (`UserPromptSubmit`), tool (`Pre`/`PostToolUse`), permission, compaction, and
/// subagent boundaries — the same class Claude delivers.
const CODEX_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PermissionRequest",
    "Stop",
    "PreCompact",
    "PostCompact",
    "SubagentStart",
    "SubagentStop",
];

/// Resolve the active Codex home the way Codex itself does: `$CODEX_HOME` when
/// set (and non-empty), else `$HOME/.codex`. Returns `None` when neither is
/// available so a guarded caller never falls back to a throwaway/`/tmp` home —
/// in production this is the user's REAL `~/.codex`, preserving auth/skills/
/// history (per the PRD design).
fn codex_home() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CODEX_HOME")
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var("HOME").ok().filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join(".codex"))
}

/// Whether a hooks.json rule was authored by the deck — i.e. one of its command
/// handlers shells `dot-agent-deck`. Used to strip stale deck entries before
/// re-adding fresh ones, so re-installs are idempotent and never touch a user's
/// own hooks.
fn rule_is_dot_agent_deck(rule: &Value) -> bool {
    rule.get("hooks")
        .and_then(|h| h.as_array())
        .is_some_and(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("command")
                    .and_then(|c| c.as_str())
                    .is_some_and(|cmd| cmd.contains("dot-agent-deck"))
            })
        })
}

/// Merge the deck's command hooks for `command` into an existing `hooks.json`
/// value (or `{}`), preserving any user-authored hooks and refreshing (not
/// duplicating) prior deck entries.
fn install_impl(root: &mut Value, command: &str) {
    if !root.is_object() {
        *root = json!({});
    }
    let obj = root.as_object_mut().expect("root is an object");
    if !obj.get("hooks").is_some_and(Value::is_object) {
        obj.insert("hooks".into(), json!({}));
    }
    let hooks = obj
        .get_mut("hooks")
        .and_then(Value::as_object_mut)
        .expect("hooks is an object");

    // Strip any stale deck-authored entries from every event so a re-install
    // normalizes down to exactly one fresh deck rule per event.
    let keys: Vec<String> = hooks.keys().cloned().collect();
    for key in keys {
        if let Some(arr) = hooks.get_mut(&key).and_then(Value::as_array_mut) {
            arr.retain(|rule| !rule_is_dot_agent_deck(rule));
        }
    }

    let entry = json!({
        "hooks": [ { "type": "command", "command": command } ]
    });
    for &event in CODEX_HOOK_EVENTS {
        let arr = hooks.entry(event.to_string()).or_insert_with(|| json!([]));
        if !arr.is_array() {
            *arr = json!([]);
        }
        arr.as_array_mut()
            .expect("hook event value is an array")
            .push(entry.clone());
    }
}

/// Testable core: merge the deck's hooks into `<codex_home>/hooks.json`, writing
/// the file (creating the home dir if needed). `binary_path` is the absolute
/// `dot-agent-deck` path the hook command should invoke.
pub fn install_to(codex_home: &Path, binary_path: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(codex_home)?;
    let path = codex_home.join("hooks.json");
    let mut root = match std::fs::read_to_string(&path) {
        Ok(contents) => serde_json::from_str(&contents).unwrap_or_else(|_| json!({})),
        Err(_) => json!({}),
    };
    let command = format!("{binary_path} hook --agent codex");
    install_impl(&mut root, &command);
    let contents = serde_json::to_string_pretty(&root)?;
    std::fs::write(&path, contents)
}

/// Silently install the Codex hooks into the active `CODEX_HOME`. Guarded
/// (`CODEX_HOME`/`HOME` must resolve), idempotent-overwrite, never a
/// throwaway/`/tmp` write. Invoked from the wrapper ([`crate::wrap::run_wrap`])
/// just before a `codex` child is spawned, so the hooks are on disk before Codex
/// boots and discovers them. Failures are swallowed (best-effort, like Claude's
/// `auto_install`): a missing home or unwritable dir degrades to the coarse
/// stdout fallback rather than blocking the spawn.
pub fn auto_install() {
    let Some(home) = codex_home() else {
        return;
    };
    let binary_path = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "dot-agent-deck".into());
    if let Err(e) = install_to(&home, &binary_path) {
        tracing::warn!("auto-install: failed to write Codex hooks.json: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_writes_command_hooks_for_every_event() {
        let dir = tempfile::tempdir().expect("codex home tempdir");
        install_to(dir.path(), "/abs/dot-agent-deck").expect("install hooks.json");

        let contents =
            std::fs::read_to_string(dir.path().join("hooks.json")).expect("read hooks.json");
        let root: Value = serde_json::from_str(&contents).expect("parse hooks.json");
        let hooks = root
            .get("hooks")
            .and_then(Value::as_object)
            .expect("hooks object");

        for &event in CODEX_HOOK_EVENTS {
            let arr = hooks
                .get(event)
                .and_then(Value::as_array)
                .unwrap_or_else(|| panic!("event {event} present"));
            assert_eq!(arr.len(), 1, "one deck rule per event ({event})");
            let cmd = arr[0]["hooks"][0]["command"].as_str().expect("command str");
            assert_eq!(cmd, "/abs/dot-agent-deck hook --agent codex");
            assert_eq!(arr[0]["hooks"][0]["type"].as_str(), Some("command"));
        }
    }

    #[test]
    fn reinstall_is_idempotent_and_preserves_user_hooks() {
        let dir = tempfile::tempdir().expect("codex home tempdir");
        // A pre-existing user hook the deck must never clobber.
        let user = json!({
            "hooks": {
                "PreToolUse": [
                    { "hooks": [ { "type": "command", "command": "/user/own-hook" } ] }
                ]
            }
        });
        std::fs::write(
            dir.path().join("hooks.json"),
            serde_json::to_string_pretty(&user).unwrap(),
        )
        .unwrap();

        install_to(dir.path(), "/abs/dot-agent-deck").expect("first install");
        install_to(dir.path(), "/abs/dot-agent-deck").expect("second install");

        let contents = std::fs::read_to_string(dir.path().join("hooks.json")).unwrap();
        let root: Value = serde_json::from_str(&contents).unwrap();
        let pre = root["hooks"]["PreToolUse"]
            .as_array()
            .expect("PreToolUse array");
        // The user's hook survives; the deck's is present exactly once (no dupes).
        let user_rules = pre
            .iter()
            .filter(|r| r["hooks"][0]["command"] == json!("/user/own-hook"))
            .count();
        let deck_rules = pre.iter().filter(|r| rule_is_dot_agent_deck(r)).count();
        assert_eq!(user_rules, 1, "user hook preserved");
        assert_eq!(
            deck_rules, 1,
            "deck hook present exactly once after re-install"
        );
    }
}
