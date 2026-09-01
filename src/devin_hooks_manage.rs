//! Install the deck's native hooks into Devin CLI's user config.
//!
//! Devin CLI ships a Claude-Code-compatible hooks engine: its command hooks post
//! the same stdin JSON shape Claude does, so they are ingested by the existing
//! [`crate::hook::handle_hook`] `"devin"` arm. This module writes the hook
//! DEFINITIONS — a `"hooks"` object whose every command shells
//! `dot-agent-deck hook --agent devin` — into Devin's user config, so a live
//! session's prompt / tool / turn events ride the deck's existing
//! raw-`AgentEvent` hook socket (no new wire, no `PROTOCOL_VERSION` bump —
//! rule 12).
//!
//! It is the [`IntegrationStrategy::NativeHooks`] analog of
//! [`crate::hooks_manage`] (Claude), but it borrows its SAFETY discipline from
//! [`crate::codex_hooks_manage`] rather than Claude's, because the target file is
//! materially more dangerous to write:
//!
//! - Claude's `~/.claude/settings.json` is a settings file the deck has always
//!   rewritten wholesale, and `hooks_manage` treats ANY read/parse failure as an
//!   empty config (`unwrap_or_else(|_| json!({}))`).
//! - Devin's user config is a **shared** file holding the user's `agent` (model),
//!   `permissions`, `mcpServers`, `theme_mode`, `read_config_from`, … AND Devin
//!   documents it as JSON *with comment support*. `serde_json` cannot parse
//!   comments, so Claude's parse-failure fallback would silently discard a
//!   perfectly valid user config the first time anyone wrote a `//` comment in
//!   it.
//!
//! So: only `NotFound` is treated as empty; malformed/JSONC content is backed up
//! and the install ERRORS rather than clobbering; a structurally-incompatible
//! shape errors without touching the file; the read-modify-write is serialized by
//! an in-process mutex and published atomically (temp file + `rename(2)`); and
//! only the `"hooks"` key is touched, so every unrelated setting survives.
//!
//! Deck-authored entries are identified by the EXACT command signature
//! [`HOOK_COMMAND_SUFFIX`] (`… hook --agent devin`), never a loose
//! `dot-agent-deck` substring, so re-installs are idempotent and a user hook that
//! merely mentions `dot-agent-deck` in an argument is preserved.
//!
//! **On Devin's Claude import.** Devin documents that it also reads hooks from
//! Claude's files (`~/.claude/settings.json`, `~/.claude.json`) when
//! `read_config_from.claude` is enabled, which it is by default — and that is
//! exactly where [`crate::hooks_manage`] installs the deck's CLAUDE hooks. On
//! paper that should mean two hook invocations per lifecycle event from one
//! Devin session, one stamped [`AgentType::Devin`] and one
//! [`AgentType::ClaudeCode`].
//!
//! Measured against devin 3000.3.27, it does not: with BOTH the deck's Claude
//! and Devin hooks installed and the import left at its default, a real session
//! emits exactly one event per lifecycle step, all stamped `devin` — in print
//! mode and in an interactive pane alike. An earlier revision of this module
//! detected the "conflict" and warned users to set
//! `"read_config_from": { "claude": false }`; that advice was dropped because it
//! fired on the deck's own default configuration and would have had users
//! disable their Claude rules, skills, commands and MCP imports to fix a symptom
//! that never occurred. If duplicate events are ever actually observed, add the
//! detection back then.
//!
//! [`IntegrationStrategy::NativeHooks`]: crate::agent_registry::IntegrationStrategy::NativeHooks
//! [`AgentType::Devin`]: crate::event::AgentType::Devin
//! [`AgentType::ClaudeCode`]: crate::event::AgentType::ClaudeCode

use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{Value, json};

/// The fixed command signature that identifies a deck-authored Devin hook. Every
/// deck hook command is `<binary_path> hook --agent devin`, so a command ending
/// in this exact suffix is deck-owned.
const HOOK_COMMAND_SUFFIX: &str = "hook --agent devin";

/// The interpreter every Devin hook command is quoted for, and the reason it
/// is a constant rather than the host: `devin_config_dir` returns `None` off
/// Unix, so a Devin config can only ever be read where a POSIX shell runs it.
///
/// Named so the choice is assertable on Linux. Its two spellings produce
/// byte-identical output on a POSIX host, so a regression back to
/// `HookShell::Native` here is invisible to every test this project can run
/// locally — which is exactly how it reached `build-windows` last time.
const HOOK_SHELL: crate::agent_hook_config::HookShell = crate::agent_hook_config::HookShell::Posix;

/// Serializes the read-modify-write of Devin's user config across concurrent
/// in-process installs (the TUI's startup install racing a `daemon serve` one).
/// Combined with the atomic temp-file+rename publish, this closes the
/// concurrent-clobber / partial-write window on the user's real config.
static INSTALL_LOCK: Mutex<()> = Mutex::new(());

/// Devin hook events we install a command handler for.
///
/// This is deliberately NOT [`crate::hooks_manage`]'s Claude list: Devin's
/// lifecycle is a different set, and installing names Devin never fires would be
/// dead config while missing the ones it does fire would lose card states.
/// Devin documents `PreToolUse`, `PostToolUse`, `PermissionRequest`,
/// `UserPromptSubmit`, `Stop`, `PostCompaction`, `SessionStart`, and
/// `SessionEnd` — and every one of those maps to an
/// [`crate::event::EventType`] via [`crate::hook`]'s `map_event_type`.
///
/// Notably absent vs. Claude: `Notification` (Devin surfaces permission prompts
/// through `PermissionRequest` instead), `PreCompact` (Devin fires only the
/// POST-compaction event), and `SubagentStart`/`SubagentStop`.
const DEVIN_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PermissionRequest",
    "Stop",
    "PostCompaction",
];

/// Devin CLI's user config directory, resolved the way Devin itself resolves it:
/// `$XDG_CONFIG_HOME/devin` when that variable is set to an absolute path, and
/// `~/.config/devin` otherwise.
///
/// **`$XDG_CONFIG_HOME` must be honoured** — measured against devin 3000.3.27,
/// not inferred. With `XDG_CONFIG_HOME` set, Devin reads
/// `$XDG_CONFIG_HOME/devin/config.json` and never touches `~/.config/devin` at
/// all. Writing to the literal `~/.config/devin` in that case would install
/// hooks into a file Devin never reads — which is worse than not installing
/// them, because it looks like success and silently delivers nothing.
///
/// A relative `XDG_CONFIG_HOME` is ignored per the XDG base-directory spec,
/// which requires an absolute path and says to fall back to the default
/// otherwise.
///
/// **Windows: deliberately a no-op, not an oversight.** This returns `None` off
/// Unix, so every caller degrades to a documented skip — exactly what
/// [`crate::codex_hooks_manage`]'s `codex_home` does, and for the same reason:
/// the path we would have to guess belongs to a *third-party* tool, and Devin —
/// not this project — decides where its config lives on Windows. Writing hooks
/// into a location Devin does not read would look like success while delivering
/// nothing. (The second reason this used to give — that the hook command came
/// out POSIX-quoted, which is not what Windows command parsing expects — has
/// stopped being a reason in either direction as of #734, which made the
/// quoting follow the *interpreter*: `install_to` below asks
/// `agent_hook_config::build_command` for `HookShell::Posix` unconditionally,
/// precisely because this function is what confines Devin to Unix. The reason
/// above is the one that stands, and it is sufficient on its own.) Native
/// Windows support for the deck is itself still open (#42); today Windows users
/// run under WSL, where the Unix branch below is the correct one.
///
/// Returns `None` when no real home resolves, so a guarded caller never writes
/// into a throwaway `/tmp` config.
pub fn devin_config_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        config_dir_from(
            std::env::var_os("XDG_CONFIG_HOME").as_deref(),
            std::env::var("HOME").ok().as_deref(),
        )
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// The pure resolution behind [`devin_config_dir`], taking the two environment
/// values explicitly so it is testable without mutating process-global state.
#[cfg(unix)]
fn config_dir_from(
    xdg_config_home: Option<&std::ffi::OsStr>,
    home: Option<&str>,
) -> Option<PathBuf> {
    if let Some(xdg) = xdg_config_home {
        let xdg = Path::new(xdg);
        if xdg.is_absolute() {
            return Some(xdg.join("devin"));
        }
    }
    let home = home.filter(|h| !h.is_empty())?;
    Some(Path::new(home).join(".config").join("devin"))
}

/// Devin CLI's user config file inside [`devin_config_dir`].
fn config_path(config_dir: &Path) -> PathBuf {
    config_dir.join("config.json")
}

/// Whether an executable `devin` is on `PATH`, used to guard the startup
/// auto-install so the deck never creates a Devin config directory on a machine
/// without Devin.
///
/// Only Unix names are probed, matching [`devin_config_dir`]'s Unix-only scope:
/// off Unix there is no config dir to install into, so a Windows-shaped lookup
/// would be dead weight. The executable bit is checked rather than mere
/// existence, mirroring [`crate::codex_hooks_manage`]'s equivalent probe — a
/// non-executable file named `devin` on `PATH` is not an agent we can launch.
fn devin_present_on_path() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join("devin");
        let Ok(meta) = std::fs::metadata(&candidate) else {
            return false;
        };
        if !meta.is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            meta.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        {
            true
        }
    })
}

/// Whether a command string is a deck-authored Devin hook, by EXACT signature.
/// A user command that merely contains `dot-agent-deck` (e.g.
/// `audit-wrapper --watch dot-agent-deck`) is NOT deck-owned and is preserved.
fn command_is_deck_owned(command: &str) -> bool {
    command.trim_end().ends_with(HOOK_COMMAND_SUFFIX)
}

/// Whether a hook rule was authored by the deck — i.e. one of its command
/// handlers is a deck hook by [`command_is_deck_owned`].
fn rule_is_dot_agent_deck(rule: &Value) -> bool {
    rule.get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(command_is_deck_owned)
            })
        })
}

/// Merge the deck's command hooks for `command` into an existing config value
/// (or `{}`), preserving every unrelated setting and every user-authored hook,
/// and refreshing (not duplicating) prior deck entries.
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

    // Strip stale deck entries from EVERY event — including events no longer in
    // `DEVIN_HOOK_EVENTS` — so a re-install after this list changes leaves no
    // orphaned deck rule behind, and drop any event key left empty as a result.
    let keys: Vec<String> = hooks.keys().cloned().collect();
    for key in keys {
        if let Some(arr) = hooks.get_mut(&key).and_then(Value::as_array_mut) {
            arr.retain(|rule| !rule_is_dot_agent_deck(rule));
            if arr.is_empty() && !DEVIN_HOOK_EVENTS.contains(&key.as_str()) {
                hooks.remove(&key);
            }
        }
    }

    let entry = json!({
        "hooks": [ { "type": "command", "command": command } ]
    });
    for &event in DEVIN_HOOK_EVENTS {
        let arr = hooks.entry(event.to_string()).or_insert_with(|| json!([]));
        if !arr.is_array() {
            *arr = json!([]);
        }
        arr.as_array_mut()
            .expect("hook event value is an array")
            .push(entry.clone());
    }
}

/// Remove the deck's hooks from an existing config value, leaving user hooks and
/// every unrelated setting untouched. Returns the event names a deck rule was
/// removed from. An event key left empty is dropped entirely, and so is a
/// `"hooks"` object left empty — an uninstall should return the file to the shape
/// it had before the deck ever wrote to it.
fn uninstall_impl(root: &mut Value) -> Vec<String> {
    let Some(obj) = root.as_object_mut() else {
        return Vec::new();
    };
    let Some(hooks) = obj.get_mut("hooks").and_then(Value::as_object_mut) else {
        return Vec::new();
    };

    let mut removed = Vec::new();
    let keys: Vec<String> = hooks.keys().cloned().collect();
    for key in keys {
        if let Some(arr) = hooks.get_mut(&key).and_then(Value::as_array_mut) {
            let before = arr.len();
            arr.retain(|rule| !rule_is_dot_agent_deck(rule));
            if arr.len() < before {
                removed.push(key.clone());
            }
            if arr.is_empty() {
                hooks.remove(&key);
            }
        }
    }
    if hooks.is_empty() {
        obj.remove("hooks");
    }

    removed
}

/// Reject a structurally-incompatible existing config shape WITHOUT mutating it.
/// Accepts a missing `hooks` key (created on install) and an empty object, but
/// rejects a non-object root, a non-object `hooks`, or any event value that is
/// not an array — so a merge never silently replaces user content it doesn't
/// understand.
fn validate_structure(root: &Value) -> io::Result<()> {
    let incompatible = |what: &str| {
        io::Error::new(
            ErrorKind::InvalidData,
            format!("existing Devin config.json is structurally incompatible: {what}"),
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

/// Read the existing config at `path`, applying the safety contract: only a
/// MISSING file is an empty config. Malformed content (including the JSONC
/// comments Devin allows but `serde_json` cannot parse) is backed up to
/// `config.json.bak` and reported as an error so the caller never overwrites it;
/// unreadable content propagates its own error.
fn read_config(path: &Path) -> io::Result<Value> {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(value) => Ok(value),
            Err(parse_err) => {
                let backup = path.with_extension("json.bak");
                let _ = std::fs::write(&backup, &bytes);
                Err(io::Error::new(
                    ErrorKind::InvalidData,
                    format!(
                        "existing Devin config.json is not valid JSON — Devin allows \
                         comments, which cannot be edited in place (preserved at {}): \
                         {parse_err}",
                        backup.display()
                    ),
                ))
            }
        },
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e),
    }
}

/// Testable core: merge the deck's hooks into `<config_dir>/config.json`,
/// writing the file atomically (creating the dir if needed). `binary_path` is
/// the absolute `dot-agent-deck` path the hook command should invoke.
pub fn install_to(config_dir: &Path, binary_path: &str) -> io::Result<()> {
    std::fs::create_dir_all(config_dir)?;
    let path = config_path(config_dir);

    let _guard = INSTALL_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let mut root = read_config(&path)?;
    validate_structure(&root)?;

    let command =
        crate::agent_hook_config::build_command(binary_path, HOOK_COMMAND_SUFFIX, HOOK_SHELL);
    install_impl(&mut root, &command);
    let contents = serde_json::to_string_pretty(&root)?;
    crate::agent_hook_config::write_atomic(config_dir, &path, contents.as_bytes())
}

/// Testable core: remove the deck's hooks from `<config_dir>/config.json`.
/// A missing config is a no-op (nothing to remove); the same read safety
/// contract as [`install_to`] applies, so a config we cannot parse is never
/// rewritten.
pub fn uninstall_from(config_dir: &Path) -> io::Result<Vec<String>> {
    let path = config_path(config_dir);
    if !path.exists() {
        return Ok(Vec::new());
    }

    let _guard = INSTALL_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let mut root = read_config(&path)?;
    validate_structure(&root)?;

    let removed = uninstall_impl(&mut root);
    if removed.is_empty() {
        return Ok(removed);
    }
    let contents = serde_json::to_string_pretty(&root)?;
    crate::agent_hook_config::write_atomic(config_dir, &path, contents.as_bytes())?;
    Ok(removed)
}

/// The DURABLE absolute path to pin into Devin's hook commands, or the
/// resolver's refusal (PRD #381).
///
/// PRD #381's M2 list names six call sites and omits this one, but it is the
/// same shape as the other six and writes a path Devin later executes — so
/// "no remaining `current_exe()` feeding an external config write" is not
/// satisfiable without it. It used to be `current_exe()` with a
/// [`crate::platform::paths::DEFAULT_BINARY_NAME`] fallback, which is issue
/// #536's bug in miniature: a bare command name in a file another program
/// hands to a shell.
fn durable_binary_path() -> Result<String, String> {
    crate::platform::paths::durable_binary_path()
}

/// Startup entry: install the deck's Devin hooks into the user's Devin config,
/// ONCE, command-agnostically. Wired as `DEVIN.startup_auto_install`, so the TUI
/// runs it at launch like Claude's hooks and OpenCode's plugin — meaning Devin
/// hooks fire however Devin is launched (bare `devin`, an absolute path, or a
/// launcher like `devbox run devin-big`), with events reaching the right card
/// through the inherited `DOT_AGENT_DECK_PANE_ID`.
///
/// Guarded, idempotent, and best-effort: SKIPs unless `devin` is on `PATH` and a
/// real config dir resolves, and any failure is logged, never fatal. Never prints
/// to stdout (it runs on the dashboard startup path).
pub fn auto_install() {
    if !devin_present_on_path() {
        tracing::debug!("devin startup install: skipped (devin not on PATH)");
        return;
    }
    let Some(config_dir) = devin_config_dir() else {
        tracing::debug!("devin startup install: skipped (no config dir resolves)");
        return;
    };

    let binary_path = match durable_binary_path() {
        Ok(binary_path) => binary_path,
        Err(e) => {
            tracing::warn!("auto-install: {e}");
            return;
        }
    };

    match install_to(&config_dir, &binary_path) {
        Ok(()) => {
            tracing::info!(
                "auto-installed Devin hooks: {}",
                DEVIN_HOOK_EVENTS.join(", ")
            );
        }
        Err(e) => tracing::warn!("auto-install: failed to write Devin hooks: {e}"),
    }
}

/// `dot-agent-deck hooks install --agent devin` — the explicit, chatty install.
/// Unlike [`auto_install`] this does NOT require `devin` on `PATH`: the user
/// asked for it by name, so a missing binary (not yet installed, or installed
/// only inside a devbox/container shell) must not silently do nothing.
pub fn install() -> Result<(), String> {
    let config_dir = devin_config_dir()
        .ok_or_else(|| "no Devin config dir resolves (HOME is unset)".to_string())?;
    install_to(&config_dir, &durable_binary_path()?).map_err(|e| e.to_string())?;

    println!("Installed hooks: {}", DEVIN_HOOK_EVENTS.join(", "));
    println!("Settings file: {}", config_path(&config_dir).display());
    Ok(())
}

/// `dot-agent-deck hooks uninstall --agent devin` — remove only the deck's own
/// hooks, leaving the user's hooks and every unrelated Devin setting intact.
pub fn uninstall() -> Result<(), String> {
    let config_dir = devin_config_dir()
        .ok_or_else(|| "no Devin config dir resolves (HOME is unset)".to_string())?;
    let removed = uninstall_from(&config_dir).map_err(|e| e.to_string())?;

    if removed.is_empty() {
        println!("No dot-agent-deck hooks found to remove.");
    } else {
        println!("Removed hooks: {}", removed.join(", "));
    }
    println!("Settings file: {}", config_path(&config_dir).display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_back(dir: &Path) -> Value {
        let contents = std::fs::read_to_string(config_path(dir)).expect("read config.json");
        serde_json::from_str(&contents).expect("parse config.json")
    }

    fn deck_commands_for(root: &Value, event: &str) -> Vec<String> {
        root["hooks"][event]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .filter(|rule| rule_is_dot_agent_deck(rule))
            .map(|rule| rule["hooks"][0]["command"].as_str().unwrap().to_string())
            .collect()
    }

    /// Every documented Devin hook event gets exactly one deck command rule,
    /// shelling the pinned binary with the `--agent devin` signature.
    #[test]
    fn install_writes_one_command_hook_per_devin_event() {
        let dir = tempfile::tempdir().expect("config tempdir");
        install_to(dir.path(), "/abs/dot-agent-deck").expect("install");

        let root = read_back(dir.path());
        let hooks = root["hooks"].as_object().expect("hooks object");
        assert_eq!(
            hooks.len(),
            DEVIN_HOOK_EVENTS.len(),
            "no extra event keys: {hooks:?}"
        );
        for &event in DEVIN_HOOK_EVENTS {
            let rules = hooks[event].as_array().expect("event array");
            assert_eq!(rules.len(), 1, "one deck rule per event ({event})");
            assert_eq!(
                rules[0]["hooks"][0]["command"].as_str(),
                Some("/abs/dot-agent-deck hook --agent devin"),
                "event {event}"
            );
            assert_eq!(rules[0]["hooks"][0]["type"].as_str(), Some("command"));
        }
    }

    /// Devin's hook event set is NOT Claude's: installing `Notification`,
    /// `PreCompact`, or the subagent boundaries would be dead config Devin never
    /// fires, and `PermissionRequest`/`PostCompaction` are the events it does.
    #[test]
    fn installed_events_match_devins_documented_lifecycle() {
        for present in [
            "SessionStart",
            "SessionEnd",
            "UserPromptSubmit",
            "PreToolUse",
            "PostToolUse",
            "PermissionRequest",
            "Stop",
            "PostCompaction",
        ] {
            assert!(
                DEVIN_HOOK_EVENTS.contains(&present),
                "{present} must be installed"
            );
        }
        for absent in [
            "Notification",
            "PreCompact",
            "SubagentStart",
            "SubagentStop",
        ] {
            assert!(
                !DEVIN_HOOK_EVENTS.contains(&absent),
                "{absent} is not a Devin hook event and must not be installed"
            );
        }
    }

    /// The user's own settings and hooks survive an install byte-for-byte, and a
    /// re-install refreshes the deck's rule rather than accumulating duplicates.
    #[test]
    fn install_merges_and_is_idempotent() {
        let dir = tempfile::tempdir().expect("config tempdir");
        let user_config = json!({
            "agent": { "model": "opus" },
            "theme_mode": "dark",
            "permissions": { "deny": ["exec"] },
            "hooks": {
                "PreToolUse": [
                    { "matcher": "^exec$", "hooks": [
                        { "type": "command", "command": "./scripts/audit.sh dot-agent-deck" }
                    ] }
                ]
            }
        });
        std::fs::write(
            config_path(dir.path()),
            serde_json::to_vec_pretty(&user_config).unwrap(),
        )
        .unwrap();

        install_to(dir.path(), "/abs/dot-agent-deck").expect("first install");
        install_to(dir.path(), "/abs/dot-agent-deck").expect("second install");

        let root = read_back(dir.path());
        // Unrelated settings untouched.
        assert_eq!(root["agent"]["model"].as_str(), Some("opus"));
        assert_eq!(root["theme_mode"].as_str(), Some("dark"));
        assert_eq!(root["permissions"]["deny"][0].as_str(), Some("exec"));
        // The user's hook survives — a command that merely MENTIONS
        // dot-agent-deck is not a deck entry.
        let pre = root["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 2, "user hook + one deck hook: {pre:?}");
        assert!(
            pre.iter().any(|rule| rule["hooks"][0]["command"]
                .as_str()
                .is_some_and(|c| c == "./scripts/audit.sh dot-agent-deck")),
            "user hook must be preserved: {pre:?}"
        );
        // Exactly one deck rule despite two installs.
        assert_eq!(deck_commands_for(&root, "PreToolUse").len(), 1);
    }

    /// A re-install after `DEVIN_HOOK_EVENTS` shrinks must not orphan a deck rule
    /// under an event we no longer install, and must not leave an empty key.
    #[test]
    fn install_cleans_up_deck_rules_for_retired_events() {
        let dir = tempfile::tempdir().expect("config tempdir");
        let stale = json!({
            "hooks": {
                "RetiredEvent": [
                    { "hooks": [
                        { "type": "command", "command": "/old/deck hook --agent devin" }
                    ] }
                ]
            }
        });
        std::fs::write(
            config_path(dir.path()),
            serde_json::to_vec_pretty(&stale).unwrap(),
        )
        .unwrap();

        install_to(dir.path(), "/abs/dot-agent-deck").expect("install");

        let root = read_back(dir.path());
        assert!(
            root["hooks"].get("RetiredEvent").is_none(),
            "an emptied retired event key must be dropped: {root:?}"
        );
    }

    /// Devin documents its config as JSON *with comment support*, which
    /// `serde_json` cannot parse. Claude's installer treats any parse failure as
    /// an empty config, which here would silently destroy the user's model,
    /// permissions and MCP servers. So: back the bytes up, error, write nothing.
    #[test]
    fn install_refuses_to_clobber_a_config_with_comments() {
        let dir = tempfile::tempdir().expect("config tempdir");
        let jsonc = "{\n  // the model I always use\n  \"agent\": { \"model\": \"opus\" }\n}\n";
        std::fs::write(config_path(dir.path()), jsonc).unwrap();

        let err = install_to(dir.path(), "/abs/dot-agent-deck")
            .expect_err("a config we cannot parse must not be rewritten");
        assert_eq!(err.kind(), ErrorKind::InvalidData);

        // The original bytes are still there, and backed up.
        assert_eq!(
            std::fs::read_to_string(config_path(dir.path())).unwrap(),
            jsonc,
            "the user's config must be left byte-for-byte intact"
        );
        let backup = dir.path().join("config.json.bak");
        assert_eq!(std::fs::read_to_string(backup).unwrap(), jsonc);
    }

    /// A structurally-incompatible shape errors WITHOUT touching the file.
    #[test]
    fn install_rejects_incompatible_structure_without_writing() {
        let dir = tempfile::tempdir().expect("config tempdir");
        let hostile = r#"{"hooks": {"PreToolUse": "not-an-array"}}"#;
        std::fs::write(config_path(dir.path()), hostile).unwrap();

        let err = install_to(dir.path(), "/abs/dot-agent-deck").expect_err("must reject");
        assert_eq!(err.kind(), ErrorKind::InvalidData);
        assert_eq!(
            std::fs::read_to_string(config_path(dir.path())).unwrap(),
            hostile
        );
    }

    /// Devin's hook commands are quoted for a POSIX shell on EVERY host, not
    /// for the host's own shell.
    ///
    /// `install_to` writes a Devin config without passing through
    /// `devin_config_dir()`, the gate that confines Devin to Unix, so taking the
    /// dialect from `cfg!(windows)` produced `cmd.exe` quoting on a Windows
    /// runner and turned `build-windows` red (PR #782). The constant is asserted
    /// directly because on this POSIX host the two spellings emit identical
    /// bytes: `install_quotes_a_binary_path_with_spaces` below would stay green
    /// through the regression, and only a Windows runner would notice.
    #[test]
    fn hook_commands_are_quoted_for_a_posix_shell_on_every_host() {
        assert_eq!(
            HOOK_SHELL,
            crate::agent_hook_config::HookShell::Posix,
            "Devin runs only where a POSIX shell does, so its hook command must \
             never be quoted for the host's shell"
        );
    }

    /// A path with whitespace is quoted so Devin parses the intended argv, and
    /// the resulting command is still recognized as deck-owned.
    #[test]
    fn install_quotes_a_binary_path_with_spaces() {
        let dir = tempfile::tempdir().expect("config tempdir");
        install_to(dir.path(), "/Applications/My Deck/dot-agent-deck").expect("install");

        let root = read_back(dir.path());
        let command = root["hooks"]["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert_eq!(
            command,
            "'/Applications/My Deck/dot-agent-deck' hook --agent devin"
        );
        assert!(command_is_deck_owned(command));
    }

    /// Uninstall removes only the deck's rules, drops the keys they emptied, and
    /// leaves user hooks plus unrelated settings alone.
    #[test]
    fn uninstall_removes_only_deck_rules() {
        let dir = tempfile::tempdir().expect("config tempdir");
        let user_config = json!({
            "agent": { "model": "opus" },
            "hooks": {
                "PreToolUse": [
                    { "hooks": [ { "type": "command", "command": "./mine.sh" } ] }
                ]
            }
        });
        std::fs::write(
            config_path(dir.path()),
            serde_json::to_vec_pretty(&user_config).unwrap(),
        )
        .unwrap();
        install_to(dir.path(), "/abs/dot-agent-deck").expect("install");

        let removed = uninstall_from(dir.path()).expect("uninstall");
        assert_eq!(removed.len(), DEVIN_HOOK_EVENTS.len());

        let root = read_back(dir.path());
        assert_eq!(root["agent"]["model"].as_str(), Some("opus"));
        let pre = root["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 1);
        assert_eq!(pre[0]["hooks"][0]["command"].as_str(), Some("./mine.sh"));
        // Every event key the deck alone occupied is gone.
        for &event in DEVIN_HOOK_EVENTS {
            if event != "PreToolUse" {
                assert!(
                    root["hooks"].get(event).is_none(),
                    "emptied key {event} must be dropped"
                );
            }
        }
    }

    /// With no user hooks at all, uninstall returns the file to its pre-deck
    /// shape: the whole `hooks` key disappears rather than lingering as `{}`.
    #[test]
    fn uninstall_drops_an_emptied_hooks_object() {
        let dir = tempfile::tempdir().expect("config tempdir");
        std::fs::write(config_path(dir.path()), br#"{"theme_mode":"dark"}"#).unwrap();
        install_to(dir.path(), "/abs/dot-agent-deck").expect("install");

        uninstall_from(dir.path()).expect("uninstall");

        let root = read_back(dir.path());
        assert!(root.get("hooks").is_none(), "leftover hooks key: {root:?}");
        assert_eq!(root["theme_mode"].as_str(), Some("dark"));
    }

    /// Uninstalling when nothing is installed is a no-op, not an error.
    #[test]
    fn uninstall_with_no_config_is_a_noop() {
        let dir = tempfile::tempdir().expect("config tempdir");
        assert!(uninstall_from(dir.path()).expect("no-op").is_empty());
        assert!(!config_path(dir.path()).exists());
    }

    /// Devin resolves its config the standard XDG way, so the deck must too.
    /// Measured against devin 3000.3.27: with `XDG_CONFIG_HOME` set it reads
    /// `$XDG_CONFIG_HOME/devin/config.json` and never touches `~/.config/devin`,
    /// so writing to the literal `~/.config` there would install hooks into a
    /// file Devin never reads — a silent no-op that still reports success.
    #[cfg(unix)]
    #[test]
    fn config_dir_follows_xdg_config_home_like_devin_does() {
        use std::ffi::OsStr;

        // XDG set to an absolute path wins over HOME.
        assert_eq!(
            config_dir_from(Some(OsStr::new("/xdg/cfg")), Some("/home/u")),
            Some(PathBuf::from("/xdg/cfg/devin"))
        );
        // Unset XDG falls back to ~/.config/devin.
        assert_eq!(
            config_dir_from(None, Some("/home/u")),
            Some(PathBuf::from("/home/u/.config/devin"))
        );
        // A relative XDG_CONFIG_HOME is invalid per the XDG spec — ignore it
        // rather than resolving a path against the process cwd.
        assert_eq!(
            config_dir_from(Some(OsStr::new("relative/cfg")), Some("/home/u")),
            Some(PathBuf::from("/home/u/.config/devin"))
        );
        // An empty XDG_CONFIG_HOME is likewise not absolute, so it falls back.
        assert_eq!(
            config_dir_from(Some(OsStr::new("")), Some("/home/u")),
            Some(PathBuf::from("/home/u/.config/devin"))
        );
        // No usable HOME and no XDG resolves to nothing, so a guarded caller
        // never writes into a throwaway location.
        assert_eq!(config_dir_from(None, None), None);
        assert_eq!(config_dir_from(None, Some("")), None);
    }

    /// The atomic publish must never widen the config's permissions. Devin ships
    /// `config.json` at 0600 and it holds `devin.org_id`, MCP server entries and
    /// whatever else the user keeps there, so a `File::create` default of 0644
    /// (under a typical 022 umask) would expose it to every local account the
    /// first time the deck installed its hooks.
    #[cfg(unix)]
    #[test]
    fn install_never_widens_config_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let mode_of = |dir: &Path| {
            std::fs::metadata(config_path(dir))
                .expect("stat config.json")
                .permissions()
                .mode()
                & 0o777
        };

        // A config the deck creates itself is owner-only, not umask-dependent.
        let fresh = tempfile::tempdir().expect("config tempdir");
        install_to(fresh.path(), "/abs/dot-agent-deck").expect("install");
        assert_eq!(
            mode_of(fresh.path()),
            0o600,
            "a deck-created Devin config must be owner-only"
        );

        // An existing owner-only config stays owner-only across install AND
        // uninstall — both go through the same atomic publish.
        let existing = tempfile::tempdir().expect("config tempdir");
        std::fs::write(config_path(existing.path()), br#"{"theme_mode":"dark"}"#).unwrap();
        std::fs::set_permissions(
            config_path(existing.path()),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();

        install_to(existing.path(), "/abs/dot-agent-deck").expect("install");
        assert_eq!(
            mode_of(existing.path()),
            0o600,
            "install must not widen an owner-only Devin config"
        );

        uninstall_from(existing.path()).expect("uninstall");
        assert_eq!(
            mode_of(existing.path()),
            0o600,
            "uninstall must not widen an owner-only Devin config"
        );
    }

    /// PRD #381: the seventh write site — the one the PRD's M2 list omits.
    /// Driving `install_to` with the REAL resolver and a build-artifact
    /// `current_exe()` must never put that artifact into Devin's config, and
    /// must never put a bare command name there either (issue #536, which this
    /// site used to reproduce with its `DEFAULT_BINARY_NAME` fallback).
    #[test]
    fn install_never_writes_a_build_artifact_or_a_bare_name_into_devin_config() {
        let dir = crate::test_temp::tempdir().expect("devin fixture tempdir");
        let home = dir.path().join("home");
        // The crate name plus the platform's executable suffix, which is what
        // the resolver searches `~/.local/bin` for. Seeding the bare name left
        // this candidate invisible on Windows, so the resolver refused and this
        // test failed on `build-windows` alone (PR #733).
        let name = format!(
            "{}{}",
            crate::platform::paths::DEFAULT_BINARY_NAME,
            std::env::consts::EXE_SUFFIX
        );
        let durable = home.join(".local").join("bin").join(&name);
        let artifact = dir
            .path()
            .join("checkout")
            .join("target")
            .join("debug")
            .join(&name);
        for candidate in [&durable, &artifact] {
            std::fs::create_dir_all(candidate.parent().expect("parent")).expect("create dir");
            std::fs::write(candidate, b"#!/bin/sh\nexit 0\n").expect("write candidate");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(candidate, std::fs::Permissions::from_mode(0o755))
                    .expect("chmod");
            }
        }

        let resolved =
            crate::platform::paths::durable_binary_path_with(Ok(artifact.clone()), &home, None)
                .expect("a seeded ~/.local/bin candidate must resolve");
        let config_dir = dir.path().join("config").join("devin");
        install_to(&config_dir, &resolved).expect("install Devin hooks");

        let body = std::fs::read_to_string(config_path(&config_dir)).expect("read config.json");
        // Both separator spellings, and the Windows one as JSON writes it
        // (`target\debug` is escaped to `target\\debug`): a single-separator
        // needle passes vacuously on Windows.
        for marker in [
            "target/debug",
            "target/release",
            r"target\\debug",
            r"target\\release",
        ] {
            assert!(
                !body.contains(marker),
                "a build artifact reached Devin's config as `{marker}`:\n{body}"
            );
        }
        let root = read_back(&config_dir);
        // Compared against the writer's OWN command builder rather than a
        // hand-spelled `<path> <suffix>`: this writer asks for `HookShell::Posix`,
        // so a Windows path (backslashes are outside the POSIX safe set) comes
        // back single-quoted on every host. The value under test here is the PATH
        // the resolver produced, not the quoting, and this way the assertion stays
        // exact on both platforms instead of pinning one platform's spelling.
        let expected = crate::agent_hook_config::build_command(
            durable.to_str().expect("durable is UTF-8"),
            HOOK_COMMAND_SUFFIX,
            HOOK_SHELL,
        );
        for &event in DEVIN_HOOK_EVENTS {
            let commands = deck_commands_for(&root, event);
            assert!(!commands.is_empty(), "no deck rule for {event}");
            for command in commands {
                assert_eq!(
                    command, expected,
                    "{event} names {command}, not the durable path"
                );
            }
        }
    }

    /// The refusal reaches the caller rather than degrading to a bare name.
    /// `durable_binary_path` is a thin wrapper, so this pins the one property
    /// that matters at this site: its error type is propagated by `install`,
    /// which returns `Result<(), String>`.
    #[test]
    fn a_refused_resolution_is_an_error_not_a_bare_command_name() {
        let dir = crate::test_temp::tempdir().expect("devin fixture tempdir");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).expect("create home");
        let artifact = dir
            .path()
            .join("checkout")
            .join("target")
            .join("debug")
            .join("dot-agent-deck");
        std::fs::create_dir_all(artifact.parent().expect("parent")).expect("create dir");
        std::fs::write(&artifact, b"x").expect("write artifact");
        let empty = dir.path().join("empty-bin");
        std::fs::create_dir_all(&empty).expect("create empty PATH dir");
        let path_value = std::env::join_paths([empty]).expect("join PATH");

        let err = crate::platform::paths::durable_binary_path_with(
            Ok(artifact),
            &home,
            Some(path_value.as_os_str()),
        )
        .expect_err("no durable candidate must refuse");
        assert_ne!(
            err,
            crate::platform::paths::DEFAULT_BINARY_NAME,
            "issue #536: the bare crate name is never the answer here"
        );
        assert!(
            err.contains("cargo install --path ."),
            "not actionable: {err}"
        );
    }
}
