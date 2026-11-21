//! Install the deck's hooks into Google's Antigravity CLI (`agy`, PRD #211).
//!
//! `agy` reads hooks from the **global** `~/.gemini/config/hooks.json`, which
//! maps a hook NAME to that hook's event arrays. The Antigravity App and IDE
//! read the same file, and `agy`'s own `/hooks` command rewrites it. So the
//! deck owns exactly one name, [`HOOK_NAME`], and nothing else in the file:
//!
//! - every other top-level key is written back byte for byte, in its place
//!   (the file is parsed into ordered raw values, never round-tripped through
//!   [`serde_json::Value`], which would reorder keys and respell numbers);
//! - a re-install replaces only the deck key's event arrays, and keeps the
//!   `enabled` the user set on it (through `/hooks`): that is their switch, and
//!   a fresh install leaves it to `agy`'s default of on;
//! - a re-install is decided on structure, not bytes, because `agy` respells
//!   the file when it saves it: when the deck key already holds what would be
//!   written, nothing is written;
//! - uninstall removes the deck key and nothing else.
//!
//! The write discipline is [`crate::devin_hooks_manage`]'s: only a missing file
//! counts as empty; content that is not JSON is backed up beside the file and
//! the install errors without writing; a shape the deck does not understand
//! errors without writing; the read-modify-write holds an in-process lock and
//! the cross-process config lock, and is published atomically through
//! [`crate::agent_hook_config`]. `agy` accepts a UTF-8 BOM (1.5.8), so one is
//! skipped on read.
//!
//! `agy` runs each hook as `sh -c "<command>"` with its cwd in the folder
//! holding `hooks.json`, so the command is POSIX-quoted and names the deck by
//! absolute path. The hook it runs ([`crate::hook`]'s `"antigravity"` arm)
//! answers and does nothing else outside a deck pane, which is what keeps the
//! App and IDE, which run the same hooks, unaffected.

use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::de::{Deserialize, Deserializer, MapAccess, Visitor};
use serde_json::value::RawValue;
use serde_json::{Map, Value, json};

use crate::agent_hook_config::{InstallMode, KeptDeckEntry};

/// The one top-level hook name the deck owns in `hooks.json`.
pub const HOOK_NAME: &str = "dot-agent-deck";

/// The fixed command signature that identifies a deck-authored Antigravity
/// hook command: `<binary_path> hook --agent antigravity`.
const HOOK_COMMAND_SUFFIX: &str = "hook --agent antigravity";

/// `agy` runs hooks through `sh -c`, and [`antigravity_config_dir`] confines the
/// install to Unix, so the command is always quoted for a POSIX shell.
const HOOK_SHELL: crate::agent_hook_config::HookShell = crate::agent_hook_config::HookShell::Posix;

/// Serializes the read-modify-write across concurrent in-process installs.
static INSTALL_LOCK: Mutex<()> = Mutex::new(());

/// Events whose handlers are a flat list of commands.
const FLAT_EVENTS: &[&str] = &["SessionStart", "PreInvocation", "Stop"];

/// Events whose handlers sit under a tool `matcher`.
const TOOL_EVENTS: &[&str] = &["PreToolUse", "PostToolUse"];

/// Every event the deck installs, in the order `agy` fires them.
/// `PostInvocation` is not installed: the card reads nothing from it, and each
/// hook is a process `agy` starts on its hot path.
const INSTALLED_EVENTS: &[&str] = &[
    "SessionStart",
    "PreInvocation",
    "PreToolUse",
    "PostToolUse",
    "Stop",
];

/// `~/.gemini/config`, where `agy` reads its global `hooks.json`. `None` off
/// Unix, where the deck does not yet run `agy`, and when `HOME` is unset or
/// empty, so a guarded caller never writes into a throwaway location.
pub fn antigravity_config_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        let home = std::env::var("HOME").ok().filter(|h| !h.is_empty())?;
        Some(Path::new(&home).join(".gemini").join("config"))
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// `hooks.json` inside [`antigravity_config_dir`].
fn hooks_path(config_dir: &Path) -> PathBuf {
    config_dir.join("hooks.json")
}

/// Whether an executable `agy` is on `PATH`, which guards the automatic install
/// so the deck never creates `~/.gemini` on a machine without Antigravity.
fn agy_present_on_path() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let Ok(meta) = std::fs::metadata(dir.join("agy")) else {
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

/// The executable a deck-authored command names, unquoted, or `None` when the
/// command is not one.
fn deck_command_executable(command: &str) -> Option<String> {
    crate::agent_hook_config::command_executable(command, HOOK_COMMAND_SUFFIX)
        .map(|exe| crate::agent_hook_config::unquote_if_needed(exe).into_owned())
}

/// Whether `command` is a deck install sharing the installing binary's name.
fn command_is_deck_install(command: &str, binary_path: &str) -> bool {
    deck_command_executable(command)
        .is_some_and(|exe| crate::agent_hook_config::is_replaceable_deck_install(&exe, binary_path))
}

/// The top-level entries of `hooks.json`, in file order, each value kept as
/// the exact text it was written with.
struct Entries(Vec<(String, Box<RawValue>)>);

impl<'de> Deserialize<'de> for Entries {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct EntriesVisitor;
        impl<'de> Visitor<'de> for EntriesVisitor {
            type Value = Entries;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a JSON object of named hooks")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Entries, A::Error> {
                let mut entries = Vec::new();
                while let Some(entry) = map.next_entry::<String, Box<RawValue>>()? {
                    entries.push(entry);
                }
                Ok(Entries(entries))
            }
        }
        deserializer.deserialize_map(EntriesVisitor)
    }
}

/// An error for a file the deck understands as JSON but will not edit.
fn incompatible(what: &str) -> io::Error {
    io::Error::new(
        ErrorKind::InvalidData,
        format!("existing Antigravity hooks.json is structurally incompatible: {what}"),
    )
}

/// Read `hooks.json`: a missing file is empty; content that is not JSON is
/// backed up beside it and is an error; JSON whose root is not an object is
/// an error without a backup.
fn read_hooks(path: &Path) -> io::Result<Vec<(String, Box<RawValue>)>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let parsed = std::str::from_utf8(&bytes)
        .map_err(|e| e.to_string())
        .map(|text| text.strip_prefix('\u{feff}').unwrap_or(text))
        .and_then(|text| {
            serde_json::from_str::<Value>(text)
                .map(|value| (text, value))
                .map_err(|e| e.to_string())
        });
    let (text, value) = match parsed {
        Ok(parsed) => parsed,
        Err(parse_err) => {
            let backup = crate::agent_hook_config::backup_malformed(path, &bytes);
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                format!(
                    "existing Antigravity hooks.json is not valid JSON, so it was left as \
                     it is ({}): {parse_err}",
                    crate::agent_hook_config::preserved_phrase(&backup)
                ),
            ));
        }
    };
    if !value.is_object() {
        return Err(incompatible("root is not a JSON object"));
    }
    let Entries(entries) = serde_json::from_str(text)?;
    Ok(entries)
}

/// The deck key's current value, from the first entry named [`HOOK_NAME`].
fn deck_entry(entries: &[(String, Box<RawValue>)]) -> io::Result<Option<Value>> {
    let Some((_, raw)) = entries.iter().find(|(name, _)| name == HOOK_NAME) else {
        return Ok(None);
    };
    let value: Value = serde_json::from_str(raw.get())?;
    if !value.is_object() {
        return Err(incompatible(&format!("`{HOOK_NAME}` is not a JSON object")));
    }
    Ok(Some(value))
}

/// Every command string under the deck key's events, in either handler shape.
fn deck_entry_commands(ours: &Value) -> Vec<&str> {
    ours.as_object()
        .into_iter()
        .flat_map(Map::values)
        .filter_map(Value::as_array)
        .flat_map(|rules| crate::agent_hook_config::rule_command_strs(rules))
        .collect()
}

/// The deck key as an install by `binary_path` writes it, given what it holds
/// now. Under [`InstallMode::Automatic`] a deck command naming another live,
/// durable install is kept rather than replaced (PRD #1487). Returns the new
/// value and the binary it names.
fn install_impl(
    existing: Option<&Value>,
    command: &str,
    binary_path: &str,
    mode: InstallMode,
) -> (Value, String) {
    let keeper = match mode {
        InstallMode::Explicit => None,
        InstallMode::Automatic => existing.and_then(|ours| {
            let handlers: Vec<Value> = deck_entry_commands(ours)
                .into_iter()
                .map(|command| json!({ "command": command }))
                .collect();
            crate::agent_hook_config::auto_install_kept_entry(
                &[json!({ "hooks": handlers })],
                binary_path,
                |cmd| command_is_deck_install(cmd, binary_path),
                deck_command_executable,
                |exe| crate::agent_hook_config::build_command(exe, HOOK_COMMAND_SUFFIX, HOOK_SHELL),
            )
        }),
    };
    let command = KeptDeckEntry::command_for(None, keeper.as_ref(), command);
    let handler = json!({ "type": "command", "command": command });
    let mut ours = Map::new();
    if let Some(enabled) = existing.and_then(|ours| ours.get("enabled")) {
        ours.insert("enabled".into(), enabled.clone());
    }
    for &event in FLAT_EVENTS {
        ours.insert(event.into(), json!([handler.clone()]));
    }
    for &event in TOOL_EVENTS {
        ours.insert(
            event.into(),
            json!([{ "matcher": "*", "hooks": [handler.clone()] }]),
        );
    }
    (
        Value::Object(ours),
        KeptDeckEntry::named_binary(keeper.as_ref(), binary_path),
    )
}

/// `entries` as a file: one top-level entry per line, each value exactly as it
/// was read (or, for the deck key, pretty-printed at that depth).
fn render(entries: &[(String, Box<RawValue>)]) -> io::Result<String> {
    if entries.is_empty() {
        return Ok("{}\n".to_string());
    }
    let mut out = String::from("{\n");
    for (index, (name, raw)) in entries.iter().enumerate() {
        out.push_str("  ");
        out.push_str(&serde_json::to_string(name)?);
        out.push_str(": ");
        out.push_str(raw.get());
        if index + 1 < entries.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str("}\n");
    Ok(out)
}

/// `value` as the raw text of a top-level entry, indented for that depth.
fn nested_raw(value: &Value) -> io::Result<Box<RawValue>> {
    let text = serde_json::to_string_pretty(value)?.replace('\n', "\n  ");
    Ok(RawValue::from_string(text)?)
}

/// Write the deck key into `<config_dir>/hooks.json` for `binary_path`. The
/// testable core of `hooks install --agent antigravity`.
pub fn install_to(config_dir: &Path, binary_path: &str) -> io::Result<()> {
    install_to_reporting(config_dir, binary_path, InstallMode::Explicit).map(|_| ())
}

/// [`install_to`] under [`InstallMode::Automatic`]. Returns whether the file
/// changed and the binary the deck key names.
pub(crate) fn auto_install_to(config_dir: &Path, binary_path: &str) -> io::Result<(bool, String)> {
    install_to_reporting(config_dir, binary_path, InstallMode::Automatic)
}

fn install_to_reporting(
    config_dir: &Path,
    binary_path: &str,
    mode: InstallMode,
) -> io::Result<(bool, String)> {
    let path = hooks_path(config_dir);
    crate::agent_hook_config::ensure_hook_path_is_shell_safe(binary_path)?;
    crate::config_write_guard::ensure_config_write_allowed(&path)?;
    std::fs::create_dir_all(config_dir)?;

    let _guard = INSTALL_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let _config_lock = crate::agent_hook_config::lock_config(&path)?;

    let mut entries = read_hooks(&path)?;
    let existing = deck_entry(&entries)?;
    let command =
        crate::agent_hook_config::build_command(binary_path, HOOK_COMMAND_SUFFIX, HOOK_SHELL);
    let (ours, named) = install_impl(existing.as_ref(), &command, binary_path, mode);
    let deck_entries = entries.iter().filter(|(name, _)| name == HOOK_NAME).count();
    if existing.as_ref() == Some(&ours) && deck_entries == 1 {
        return Ok((false, named));
    }
    let raw = nested_raw(&ours)?;
    match entries.iter().position(|(name, _)| name == HOOK_NAME) {
        Some(first) => {
            entries[first].1 = raw;
            let mut index = 0;
            entries.retain(|(name, _)| {
                let keep = name != HOOK_NAME || index == first;
                index += 1;
                keep
            });
        }
        None => entries.push((HOOK_NAME.to_string(), raw)),
    }
    crate::agent_hook_config::write_atomic(config_dir, &path, render(&entries)?.as_bytes())?;
    Ok((true, named))
}

/// Remove the deck key from `<config_dir>/hooks.json`, leaving every other key
/// as it is. Returns whether there was one. A missing file is a no-op, and a
/// file that cannot be parsed is never rewritten.
pub fn uninstall_from(config_dir: &Path) -> io::Result<bool> {
    let path = hooks_path(config_dir);
    if !path.exists() {
        return Ok(false);
    }
    let _guard = INSTALL_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let _config_lock = crate::agent_hook_config::lock_config(&path)?;

    let mut entries = read_hooks(&path)?;
    let before = entries.len();
    entries.retain(|(name, _)| name != HOOK_NAME);
    if entries.len() == before {
        return Ok(false);
    }
    crate::agent_hook_config::write_atomic(config_dir, &path, render(&entries)?.as_bytes())?;
    Ok(true)
}

/// Startup entry, wired as `ANTIGRAVITY.startup_auto_install`: install the
/// deck key once, command-agnostically, when `agy` is on `PATH`. Best-effort:
/// a failure is logged, never fatal, and nothing is printed.
pub fn auto_install() -> Vec<crate::hook_binary::HookPin> {
    match startup_install() {
        Some(Ok(pin)) => vec![pin],
        Some(Err(e)) => {
            tracing::warn!("auto-install: {e}");
            Vec::new()
        }
        None => Vec::new(),
    }
}

/// [`auto_install`] for `hooks refresh`: whether Antigravity is installed here,
/// and what the install could not write.
pub fn refresh_install() -> crate::hooks_refresh::InstallAttempt {
    let attempt = startup_install();
    if let Some(Err(e)) = &attempt {
        tracing::warn!("auto-install: {e}");
    }
    crate::hooks_refresh::InstallAttempt::of(attempt)
}

/// [`auto_install`]'s work: `None` when Antigravity is not installed here,
/// else the pin or why the install wrote nothing.
fn startup_install() -> Option<Result<crate::hook_binary::HookPin, String>> {
    if !agy_present_on_path() {
        tracing::debug!("antigravity startup install: skipped (agy not on PATH)");
        return None;
    }
    let Some(config_dir) = antigravity_config_dir() else {
        tracing::debug!("antigravity startup install: skipped (no config dir resolves)");
        return None;
    };
    Some(install_into(&config_dir))
}

/// The automatic install into `config_dir`, from this copy's durable path.
fn install_into(config_dir: &Path) -> Result<crate::hook_binary::HookPin, String> {
    let binary_path = crate::platform::paths::durable_binary_path()?;
    let (changed, named) = auto_install_to(config_dir, &binary_path)
        .map_err(|e| format!("failed to write Antigravity hooks: {e}"))?;
    if changed {
        crate::agent_hook_config::log_auto_install_change(
            "antigravity",
            &hooks_path(config_dir),
            &named,
            "antigravity startup auto-install",
        );
    }
    Ok(crate::hook_binary::HookPin {
        agent: crate::event::AgentType::Antigravity,
        config: hooks_path(config_dir),
        binary: named,
    })
}

/// What the deck key's commands name now, read back without installing
/// anything (`ANTIGRAVITY.configured_pins`). An empty list when the file or the
/// key is missing; `None` under [`auto_install`]'s guards or when the file
/// cannot be read.
pub fn configured_pins() -> Option<Vec<crate::hook_binary::HookPin>> {
    if !agy_present_on_path() {
        return None;
    }
    configured_pins_in(&antigravity_config_dir()?)
}

/// [`configured_pins`] for an explicit config dir.
fn configured_pins_in(config_dir: &Path) -> Option<Vec<crate::hook_binary::HookPin>> {
    let path = hooks_path(config_dir);
    let Some(root) = crate::agent_hook_config::read_json_config(&path)? else {
        return Some(Vec::new());
    };
    let Some(ours) = root.get(HOOK_NAME) else {
        return Some(Vec::new());
    };
    Some(
        crate::agent_hook_config::configured_deck_executables(
            &json!({ "hooks": ours }),
            deck_command_executable,
        )
        .into_iter()
        .map(|binary| crate::hook_binary::HookPin {
            agent: crate::event::AgentType::Antigravity,
            config: path.clone(),
            binary,
        })
        .collect(),
    )
}

/// `dot-agent-deck hooks install --agent antigravity`. Unlike [`auto_install`]
/// it does not require `agy` on `PATH`: the user asked for it by name.
pub fn install() -> Result<(), String> {
    let config_dir = antigravity_config_dir()
        .ok_or_else(|| "no Antigravity config dir resolves (HOME is unset)".to_string())?;
    install_to(&config_dir, &crate::platform::paths::durable_binary_path()?)
        .map_err(|e| e.to_string())?;
    println!(
        "Installed hook `{HOOK_NAME}`: {}",
        INSTALLED_EVENTS.join(", ")
    );
    println!("Hooks file: {}", hooks_path(&config_dir).display());
    Ok(())
}

/// `dot-agent-deck hooks uninstall --agent antigravity`: remove the deck key,
/// leaving the user's own hooks as they are.
pub fn uninstall() -> Result<(), String> {
    let config_dir = antigravity_config_dir()
        .ok_or_else(|| "no Antigravity config dir resolves (HOME is unset)".to_string())?;
    if uninstall_from(&config_dir).map_err(|e| e.to_string())? {
        println!("Removed hook `{HOOK_NAME}`.");
    } else {
        println!("No dot-agent-deck hooks found to remove.");
    }
    println!("Hooks file: {}", hooks_path(&config_dir).display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DECK: &str = "/opt/deck/dot-agent-deck";

    fn install(dir: &Path) -> Value {
        install_to(dir, DECK).unwrap();
        serde_json::from_slice(&std::fs::read(hooks_path(dir)).unwrap()).unwrap()
    }

    #[test]
    fn install_writes_one_named_hook_with_five_events() {
        let dir = tempfile::tempdir().unwrap();
        let doc = install(dir.path());
        let object = doc.as_object().unwrap();
        assert_eq!(object.keys().collect::<Vec<_>>(), vec![HOOK_NAME]);
        let ours = object[HOOK_NAME].as_object().unwrap();
        let mut events: Vec<&str> = ours.keys().map(String::as_str).collect();
        events.sort_unstable();
        let mut expected = INSTALLED_EVENTS.to_vec();
        expected.sort_unstable();
        assert_eq!(events, expected);
        assert!(ours.get("enabled").is_none());
        let commands = deck_entry_commands(&object[HOOK_NAME]);
        assert_eq!(commands.len(), 5);
        for command in commands {
            assert_eq!(deck_command_executable(command).as_deref(), Some(DECK));
        }
    }

    #[test]
    fn install_keeps_key_order_bytes_and_a_bom_free_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = hooks_path(dir.path());
        let user_a = r#"{"Stop":[{"type":"command","command":"a"}], "n": 1E2}"#;
        let user_b = r#"[ "not a hook, still kept" ]"#;
        std::fs::write(
            &path,
            format!("\u{feff}{{\"b\": {user_b},\n \"a\":{user_a}}}"),
        )
        .unwrap();
        install(dir.path());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.starts_with('\u{feff}'));
        let b = text.find(user_b).expect("b kept verbatim");
        let a = text.find(user_a).expect("a kept verbatim");
        let ours = text.find(HOOK_NAME).unwrap();
        assert!(b < a && a < ours, "{text}");
        // A second install changes nothing, byte for byte.
        install(dir.path());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    }

    #[test]
    fn duplicate_deck_keys_collapse_into_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = hooks_path(dir.path());
        std::fs::write(
            &path,
            r#"{"dot-agent-deck":{"enabled":false},"u":{},"dot-agent-deck":{}}"#,
        )
        .unwrap();
        install(dir.path());
        let entries = read_hooks(&path).unwrap();
        let names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec![HOOK_NAME, "u"]);
        assert_eq!(
            deck_entry(&entries).unwrap().unwrap()["enabled"],
            Value::Bool(false)
        );
    }

    #[test]
    fn an_unusable_shape_is_refused_without_writing() {
        for body in ["[]", r#"{"dot-agent-deck": []}"#] {
            let dir = tempfile::tempdir().unwrap();
            let path = hooks_path(dir.path());
            std::fs::write(&path, body).unwrap();
            assert!(install_to(dir.path(), DECK).is_err(), "{body}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
        }
    }

    #[test]
    fn uninstall_removes_only_the_deck_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = hooks_path(dir.path());
        assert!(!uninstall_from(dir.path()).unwrap(), "missing file");
        std::fs::write(&path, r#"{"u": {"Stop": []}}"#).unwrap();
        install(dir.path());
        assert!(uninstall_from(dir.path()).unwrap());
        let doc: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(doc, json!({"u": {"Stop": []}}));
        assert!(
            !uninstall_from(dir.path()).unwrap(),
            "nothing left to remove"
        );
        std::fs::write(&path, r#"{"dot-agent-deck": {}}"#).unwrap();
        assert!(uninstall_from(dir.path()).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}\n");
    }

    #[test]
    fn configured_pins_read_the_deck_key_only() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(configured_pins_in(dir.path()), Some(Vec::new()));
        let path = hooks_path(dir.path());
        std::fs::write(
            &path,
            r#"{"u":{"Stop":[{"type":"command","command":"/other/dot-agent-deck hook --agent antigravity"}]}}"#,
        )
        .unwrap();
        assert_eq!(configured_pins_in(dir.path()), Some(Vec::new()));
        install(dir.path());
        let pins = configured_pins_in(dir.path()).unwrap();
        assert_eq!(pins.len(), 1);
        assert_eq!(pins[0].binary, DECK);
        assert_eq!(pins[0].agent, crate::event::AgentType::Antigravity);
    }
}
