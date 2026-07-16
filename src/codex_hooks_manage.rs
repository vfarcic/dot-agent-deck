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
//! **guarded, silent, and SAFE for the user's real `~/.codex`**. Deliberate
//! choices vs. Claude:
//!
//! - We write a SEPARATE `hooks.json` — Codex's highest-precedence,
//!   auto-discovered hook source (`$CODEX_HOME/hooks.json`) — rather than
//!   editing `config.toml`, so the user's real `~/.codex/config.toml` (auth
//!   references, model, project trust, skills, history) is never touched.
//! - We MERGE, never clobber: pre-existing user hooks are preserved; only prior
//!   deck-authored entries (identified by the EXACT command signature
//!   [`HOOK_COMMAND_SUFFIX`], not a loose `dot-agent-deck` substring) are
//!   refreshed, so re-installs never accumulate duplicates and a user hook that
//!   merely mentions `dot-agent-deck` is never deleted (finding #14).
//! - The write is ATOMIC (temp file in the same dir + `rename(2)`) and guarded
//!   by an in-process mutex, so a crash mid-write can't truncate the file and two
//!   panes launching Codex concurrently can't clobber each other (finding #1/M-2).
//! - We treat ONLY `NotFound` as an empty config. Unreadable, malformed, or
//!   structurally-incompatible existing content is NEVER silently discarded:
//!   malformed JSON is backed up to `hooks.json.bak` and the install errors;
//!   a structurally-incompatible shape errors WITHOUT touching the file
//!   (findings #1, L-2).
//!
//! Trust (finding #2 / M-1): Codex requires non-managed command hooks to be
//! trusted before they run. `--dangerously-bypass-hook-trust` exists, but it is
//! INVOCATION-GLOBAL — it trusts every enabled hook in the active `CODEX_HOME`,
//! including the user's own untrusted third-party hooks. Codex 0.144.4 exposes
//! no user-space *scoped* pre-trust the deck can safely use (managed/pre-trusted
//! hooks require a root-owned `/etc/codex/{managed_config,requirements}.toml` or
//! MDM/cloud source; per-hook trust is a SHA-256 over the exact definition that
//! only Codex itself should record). So the wrapper injects the global bypass
//! ONLY when the active `CODEX_HOME` contains no non-deck command hooks — i.e.
//! when the only thing the bypass would trust is the deck's own vetted entry
//! (see [`foreign_command_hooks_present`] and [`crate::wrap::codex_spawn_prep`]).
//! When a third-party hook is present the deck does NOT bypass, and its events
//! degrade to the coarse stdout classification rather than silently trusting an
//! unreviewed hook.

use std::io::{self, ErrorKind, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{Value, json};

/// The fixed command signature that identifies a deck-authored Codex hook. Every
/// deck hook command is `<binary_path> hook --agent codex`, so a command ending
/// in this exact suffix is deck-owned. Matching the full verb (rather than the
/// `dot-agent-deck` substring) means a user hook that merely mentions
/// `dot-agent-deck` in an argument is never mistaken for a deck entry
/// (finding #14).
const HOOK_COMMAND_SUFFIX: &str = "hook --agent codex";

/// Serializes the read-modify-write of `hooks.json` across concurrent in-process
/// Codex spawns (two panes launching `codex` at once). Combined with the atomic
/// temp-file+rename publish, this closes the concurrent-clobber / partial-write
/// window on the user's real `~/.codex/hooks.json` (finding #1/M-2).
static INSTALL_LOCK: Mutex<()> = Mutex::new(());

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

/// Whether a command string is a deck-authored hook, by EXACT signature: it ends
/// with [`HOOK_COMMAND_SUFFIX`] (`… hook --agent codex`). A user command that
/// merely contains `dot-agent-deck` (e.g. `audit-wrapper --watch dot-agent-deck`)
/// is NOT deck-owned and is preserved (finding #14).
fn command_is_deck_owned(command: &str) -> bool {
    command.trim_end().ends_with(HOOK_COMMAND_SUFFIX)
}

/// Whether a hooks.json rule was authored by the deck — i.e. one of its command
/// handlers is a deck hook by [`command_is_deck_owned`]. Used to strip stale deck
/// entries before re-adding fresh ones, so re-installs are idempotent and never
/// touch a user's own hooks.
fn rule_is_dot_agent_deck(rule: &Value) -> bool {
    rule.get("hooks")
        .and_then(|h| h.as_array())
        .is_some_and(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("command")
                    .and_then(|c| c.as_str())
                    .is_some_and(command_is_deck_owned)
            })
        })
}

/// Build the deck's hook command for `binary_path`, robustly quoting the
/// executable path so a path containing whitespace or shell metacharacters still
/// produces a valid command that Codex parses to the intended argv (finding #14 /
/// L-1). A "safe" path (only path-typical characters) is emitted verbatim so the
/// common case stays human-readable and stable; anything else is single-quoted
/// with embedded single quotes escaped.
fn build_command(binary_path: &str) -> String {
    format!(
        "{} {HOOK_COMMAND_SUFFIX}",
        shell_quote_if_needed(binary_path)
    )
}

/// Single-quote `path` for a POSIX shell only when it contains a character
/// outside a conservative safe set; otherwise return it unchanged.
fn shell_quote_if_needed(path: &str) -> String {
    fn is_safe(b: u8) -> bool {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'/' | b'.' | b'_' | b'-' | b'+' | b'=' | b':' | b'@' | b'%' | b','
            )
    }
    if !path.is_empty() && path.bytes().all(is_safe) {
        path.to_string()
    } else {
        format!("'{}'", path.replace('\'', r"'\''"))
    }
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

/// Reject a structurally-incompatible existing `hooks.json` shape without
/// mutating it. Accepts a missing `hooks` key (created on install) and an empty
/// object, but rejects a non-object root, a non-object `hooks`, or any event
/// value that is not an array — so a merge never silently replaces user content
/// it doesn't understand (finding #1).
fn validate_structure(root: &Value) -> io::Result<()> {
    let incompatible = |what: &str| {
        io::Error::new(
            ErrorKind::InvalidData,
            format!("existing hooks.json is structurally incompatible: {what}"),
        )
    };
    if !root.is_object() {
        return Err(incompatible("root is not a JSON object"));
    }
    let Some(hooks) = root.get("hooks") else {
        return Ok(()); // missing `hooks` is fine — install creates it
    };
    let Some(hooks) = hooks.as_object() else {
        return Err(incompatible("`hooks` is not a JSON object"));
    };
    for (event, value) in hooks {
        if !value.is_array() {
            return Err(incompatible(&format!(
                "hook event `{event}` is not an array"
            )));
        }
    }
    Ok(())
}

/// Atomically publish `bytes` to `dest` by writing a temp file in the SAME
/// directory (so `rename(2)` stays on one filesystem and is atomic) and renaming
/// over `dest`. A crash mid-write leaves either the old file or the temp file
/// intact — never a truncated `dest` (finding #1/M-2).
fn write_atomic(dir: &Path, dest: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = dir.join(format!(".hooks.json.tmp.{}", std::process::id()));
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    match std::fs::rename(&tmp, dest) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Testable core: merge the deck's hooks into `<codex_home>/hooks.json`, writing
/// the file atomically (creating the home dir if needed). `binary_path` is the
/// absolute `dot-agent-deck` path the hook command should invoke.
///
/// Safety contract (findings #1, #14, L-2, M-2): the read-modify-write is
/// serialized by [`INSTALL_LOCK`] and published atomically. Only a missing file
/// is treated as empty. Malformed JSON is backed up to `hooks.json.bak` and the
/// call errors (never discarded); a structurally-incompatible shape errors
/// WITHOUT touching the file; unreadable content propagates its error unwritten.
pub fn install_to(codex_home: &Path, binary_path: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(codex_home)?;
    let path = codex_home.join("hooks.json");

    let _guard = INSTALL_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let mut root = match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(value) => value,
            Err(parse_err) => {
                // Preserve the user's bytes (best-effort backup) and refuse to
                // overwrite — never discard content we couldn't parse.
                let backup = codex_home.join("hooks.json.bak");
                let _ = std::fs::write(&backup, &bytes);
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    format!(
                        "existing hooks.json is not valid JSON (preserved at {}): {parse_err}",
                        backup.display()
                    ),
                ));
            }
        },
        Err(e) if e.kind() == ErrorKind::NotFound => json!({}),
        // Unreadable (permissions, etc.): propagate rather than overwrite.
        Err(e) => return Err(e),
    };

    validate_structure(&root)?;

    let command = build_command(binary_path);
    install_impl(&mut root, &command);
    let contents = serde_json::to_string_pretty(&root)?;
    write_atomic(codex_home, &path, contents.as_bytes())
}

/// Whether the active `CODEX_HOME`'s `hooks.json` declares any command hook NOT
/// authored by the deck. The wrapper consults this before injecting the
/// invocation-global `--dangerously-bypass-hook-trust`: the bypass is only safe
/// when every command hook it would trust is deck-owned. Returns:
/// - `Ok(false)` when the file is absent or contains only deck-owned command
///   hooks (bypass is safe);
/// - `Ok(true)` when a non-deck command hook is present (do NOT bypass — it would
///   silently trust the user's unreviewed third-party hook, finding #2/M-1), or
///   when no `CODEX_HOME` resolves (conservative default);
/// - `Err` when the file exists but is unreadable/malformed (caller treats any
///   error as "do not bypass").
///
/// It inspects `CODEX_HOME/hooks.json` only. Project-local (`<repo>/.codex`),
/// plugin, and `config.toml`-defined hooks are NOT inspected here; the residual
/// is documented in `docs/develop/agent-adapters.md`.
pub fn foreign_command_hooks_present() -> std::io::Result<bool> {
    let Some(home) = codex_home() else {
        return Ok(true);
    };
    let path = home.join("hooks.json");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let root: Value =
        serde_json::from_slice(&bytes).map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;
    Ok(any_foreign_command_hook(&root))
}

/// Walk `root.hooks.<event>[].hooks[]` and report whether any `type == "command"`
/// handler's command is NOT deck-owned (per [`command_is_deck_owned`]).
fn any_foreign_command_hook(root: &Value) -> bool {
    let Some(hooks) = root.get("hooks").and_then(Value::as_object) else {
        return false;
    };
    for rules in hooks.values() {
        let Some(rules) = rules.as_array() else {
            continue;
        };
        for rule in rules {
            let Some(handlers) = rule.get("hooks").and_then(Value::as_array) else {
                continue;
            };
            for handler in handlers {
                if handler.get("type").and_then(Value::as_str) != Some("command") {
                    continue;
                }
                let foreign = handler
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|command| !command_is_deck_owned(command));
                if foreign {
                    return true;
                }
            }
        }
    }
    false
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
