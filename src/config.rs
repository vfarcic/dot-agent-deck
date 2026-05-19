use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::state::SessionStatus;
use crate::theme::Theme;

pub const CONFIG_KEYS: &[(&str, &str)] = &[
    ("default_command", "Default shell command for new panes"),
    ("theme", "Color theme: auto, light, dark (default: auto)"),
    (
        "auto_config_prompt",
        "Enable/disable the config generation prompt (default: true)",
    ),
    (
        "bell.enabled",
        "Enable/disable terminal bell (default: true)",
    ),
    (
        "bell.on_waiting_for_input",
        "Bell when agent waits for input (default: true)",
    ),
    (
        "bell.on_idle",
        "Bell when session goes idle (default: false)",
    ),
    ("bell.on_error", "Bell on agent error (default: true)"),
    (
        "idle_art.enabled",
        "Enable ASCII art in dashboard idle cards (default: false)",
    ),
    (
        "idle_art.provider",
        "LLM provider: anthropic (ANTHROPIC_API_KEY), openai (OPENAI_API_KEY), ollama (no key needed) (default: anthropic)",
    ),
    ("idle_art.model", "LLM model (default: claude-haiku-4-5)"),
    (
        "idle_art.timeout_secs",
        "Seconds idle before triggering art (default: 300)",
    ),
];

pub fn config_keys_help() -> String {
    let mut help = String::from("Available keys:\n");
    for (key, desc) in CONFIG_KEYS {
        help.push_str(&format!("  {key:<30} {desc}\n"));
    }
    help
}

pub fn socket_path() -> PathBuf {
    if let Ok(path) = std::env::var("DOT_AGENT_DECK_SOCKET") {
        return PathBuf::from(path);
    }

    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime_dir).join("dot-agent-deck.sock");
    }

    PathBuf::from("/tmp/dot-agent-deck.sock")
}

/// Path of the M1.2 streaming-attach Unix socket. Separate from the existing
/// hook-ingestion socket (PRD #76 line 219) so the two protocols have
/// disjoint, clearly-typed wire formats: hook ingestion is line-delimited
/// JSON, attach is a binary frame protocol (see `daemon_protocol`). Same
/// XDG-aware resolution pattern as `socket_path`, with `DOT_AGENT_DECK_ATTACH_SOCKET`
/// as the explicit override.
pub fn attach_socket_path() -> PathBuf {
    if let Ok(path) = std::env::var("DOT_AGENT_DECK_ATTACH_SOCKET") {
        return PathBuf::from(path);
    }

    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime_dir).join("dot-agent-deck-attach.sock");
    }

    PathBuf::from("/tmp/dot-agent-deck-attach.sock")
}

/// Per-user state directory. Used by lazy-spawn (PRD #76 M4.3) for the
/// detached daemon log and the spawn mutex (`spawn.lock`). Resolution order:
///
/// 1. `DOT_AGENT_DECK_STATE_DIR` — explicit override (tests use this).
/// 2. `$XDG_STATE_HOME/dot-agent-deck` — freedesktop spec default.
/// 3. `$HOME/.local/state/dot-agent-deck` — XDG fallback when the env var is
///    unset (per the spec).
pub fn state_dir() -> PathBuf {
    if let Ok(path) = std::env::var("DOT_AGENT_DECK_STATE_DIR") {
        return PathBuf::from(path);
    }
    match std::env::var("XDG_STATE_HOME") {
        Ok(state_home) if !state_home.is_empty() => {
            PathBuf::from(state_home).join("dot-agent-deck")
        }
        _ => dirs_home().join(".local/state/dot-agent-deck"),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BellConfig {
    pub enabled: bool,
    pub on_waiting_for_input: bool,
    pub on_idle: bool,
    pub on_error: bool,
}

impl Default for BellConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            on_waiting_for_input: true,
            on_idle: false,
            on_error: true,
        }
    }
}

impl BellConfig {
    pub fn should_bell(&self, status: &SessionStatus) -> bool {
        if !self.enabled {
            return false;
        }
        match status {
            SessionStatus::WaitingForInput => self.on_waiting_for_input,
            SessionStatus::Idle => self.on_idle,
            SessionStatus::Error => self.on_error,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct IdleArtConfig {
    pub enabled: bool,
    pub provider: String,
    pub model: String,
    pub timeout_secs: u64,
}

const MAX_IDLE_ART_TIMEOUT_SECS: u64 = i64::MAX as u64;

impl Default for IdleArtConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: "anthropic".to_string(),
            model: "claude-haiku-4-5".to_string(),
            timeout_secs: 300,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DashboardConfig {
    pub default_command: String,
    pub bell: BellConfig,
    pub theme: Theme,
    pub idle_art: IdleArtConfig,
    pub auto_config_prompt: bool,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            default_command: String::new(),
            bell: BellConfig::default(),
            theme: Theme::default(),
            idle_art: IdleArtConfig::default(),
            auto_config_prompt: true,
        }
    }
}

impl DashboardConfig {
    pub fn load() -> Self {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(contents) => match toml::from_str(&contents) {
                Ok(config) => config,
                Err(err) => {
                    eprintln!("Invalid config at {}: {err}", path.display());
                    Self::default()
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(err) => {
                eprintln!("Failed to read config at {}: {err}", path.display());
                Self::default()
            }
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create config directory: {e}"))?;
        }
        let contents =
            toml::to_string_pretty(self).map_err(|e| format!("Failed to serialize config: {e}"))?;
        std::fs::write(&path, contents)
            .map_err(|e| format!("Failed to write config at {}: {e}", path.display()))
    }

    pub fn get_field(&self, key: &str) -> Result<String, String> {
        match key {
            "default_command" => Ok(self.default_command.clone()),
            "theme" => Ok(self.theme.to_string()),
            "bell.enabled" => Ok(self.bell.enabled.to_string()),
            "bell.on_waiting_for_input" => Ok(self.bell.on_waiting_for_input.to_string()),
            "bell.on_idle" => Ok(self.bell.on_idle.to_string()),
            "bell.on_error" => Ok(self.bell.on_error.to_string()),
            "idle_art.enabled" => Ok(self.idle_art.enabled.to_string()),
            "idle_art.provider" => Ok(self.idle_art.provider.clone()),
            "idle_art.model" => Ok(self.idle_art.model.clone()),
            "idle_art.timeout_secs" => Ok(self.idle_art.timeout_secs.to_string()),
            "auto_config_prompt" => Ok(self.auto_config_prompt.to_string()),
            _ => Err(format!("Unknown config key: {key}\n{}", config_keys_help())),
        }
    }

    pub fn set_field(&mut self, key: &str, value: &str) -> Result<(), String> {
        let parse_bool = |v: &str| -> Result<bool, String> {
            v.parse().map_err(|_| format!("Invalid boolean: {v}"))
        };
        match key {
            "default_command" => {
                self.default_command = value.to_string();
                Ok(())
            }
            "theme" => {
                self.theme = value.parse().map_err(|e: String| e)?;
                Ok(())
            }
            "bell.enabled" => {
                self.bell.enabled = parse_bool(value)?;
                Ok(())
            }
            "bell.on_waiting_for_input" => {
                self.bell.on_waiting_for_input = parse_bool(value)?;
                Ok(())
            }
            "bell.on_idle" => {
                self.bell.on_idle = parse_bool(value)?;
                Ok(())
            }
            "bell.on_error" => {
                self.bell.on_error = parse_bool(value)?;
                Ok(())
            }
            "idle_art.enabled" => {
                self.idle_art.enabled = parse_bool(value)?;
                Ok(())
            }
            "idle_art.provider" => {
                self.idle_art.provider = value.to_string();
                Ok(())
            }
            "idle_art.model" => {
                self.idle_art.model = value.to_string();
                Ok(())
            }
            "idle_art.timeout_secs" => {
                let secs: u64 = value
                    .parse()
                    .map_err(|_| format!("Invalid number: {value}"))?;
                if secs > MAX_IDLE_ART_TIMEOUT_SECS {
                    return Err(format!(
                        "idle_art.timeout_secs must be <= {MAX_IDLE_ART_TIMEOUT_SECS}"
                    ));
                }
                self.idle_art.timeout_secs = secs;
                Ok(())
            }
            "auto_config_prompt" => {
                self.auto_config_prompt = value
                    .parse()
                    .map_err(|_| "Expected 'true' or 'false'".to_string())?;
                Ok(())
            }
            _ => Err(format!("Unknown config key: {key}\n{}", config_keys_help())),
        }
    }
}

fn config_path() -> PathBuf {
    if let Ok(dir) = std::env::var("DOT_AGENT_DECK_CONFIG") {
        return PathBuf::from(dir);
    }
    dirs_home().join(".config/dot-agent-deck/config.toml")
}

fn session_path() -> PathBuf {
    if let Ok(dir) = std::env::var("DOT_AGENT_DECK_SESSION") {
        return PathBuf::from(dir);
    }
    dirs_home().join(".config/dot-agent-deck/session.toml")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedPane {
    pub dir: String,
    pub name: String,
    pub command: String,
    /// When set, this pane was the agent pane of a mode tab.
    /// The value is the mode name from the project config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SavedSession {
    #[serde(default)]
    pub panes: Vec<SavedPane>,
}

impl SavedSession {
    pub fn load() -> Self {
        let path = session_path();
        match std::fs::read_to_string(&path) {
            Ok(contents) => match toml::from_str(&contents) {
                Ok(session) => session,
                Err(err) => {
                    eprintln!("Invalid session at {}: {err}", path.display());
                    Self::default()
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(err) => {
                eprintln!("Failed to read session at {}: {err}", path.display());
                Self::default()
            }
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let path = session_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create session directory: {e}"))?;
        }
        let contents = toml::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize session: {e}"))?;
        std::fs::write(&path, contents)
            .map_err(|e| format!("Failed to write session at {}: {e}", path.display()))
    }

    pub fn clear() -> Result<(), std::io::Error> {
        let path = session_path();
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Build a `SavedSession` snapshot from the live UI state.
    ///
    /// Must be called *before* tearing down mode/orchestration tabs — i.e., while
    /// `live_panes` (the authoritative `state.managed_pane_ids`) still contains
    /// every pane, including mode-tab agent panes that carry `mode = Some(...)`.
    /// `retain` here only prunes panes the user externally closed before exit;
    /// running it after teardown would also drop the mode-tab agent pane and lose
    /// the mode field, breaking `--continue` restoration (PRD #69).
    pub fn snapshot(
        pane_metadata: &mut HashMap<String, SavedPane>,
        pane_display_names: &HashMap<String, String>,
        live_panes: &HashSet<String>,
    ) -> Self {
        pane_metadata.retain(|id, _| live_panes.contains(id));
        for (id, meta) in pane_metadata.iter_mut() {
            if let Some(name) = pane_display_names.get(id) {
                meta.name = name.clone();
            }
        }
        let mut ids: Vec<&String> = pane_metadata.keys().collect();
        ids.sort_by_key(|id| id.parse::<u64>().unwrap_or(0));
        Self {
            panes: ids
                .into_iter()
                .filter_map(|id| pane_metadata.get(id).cloned())
                .collect(),
        }
    }
}

const STAR_PROMPT_INTERVAL: u64 = 10;

fn star_prompt_path() -> PathBuf {
    if let Ok(p) = std::env::var("DOT_AGENT_DECK_STAR_PROMPT") {
        return PathBuf::from(p);
    }
    dirs_home().join(".config/dot-agent-deck/star-prompt-state.json")
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StarPromptState {
    pub launch_count: u64,
    pub permanently_dismissed: bool,
    pub last_prompt_at_launch: u64,
}

impl StarPromptState {
    pub fn load() -> Self {
        let path = star_prompt_path();
        match std::fs::read_to_string(&path) {
            Ok(contents) => match serde_json::from_str(&contents) {
                Ok(state) => state,
                Err(err) => {
                    eprintln!("Invalid star prompt state at {}: {err}", path.display());
                    Self::default()
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(err) => {
                eprintln!(
                    "Failed to read star prompt state at {}: {err}",
                    path.display()
                );
                Self::default()
            }
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let path = star_prompt_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create star prompt directory: {e}"))?;
        }
        let contents = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize star prompt state: {e}"))?;
        std::fs::write(&path, contents).map_err(|e| {
            format!(
                "Failed to write star prompt state at {}: {e}",
                path.display()
            )
        })
    }

    pub fn increment_and_check(&mut self) -> bool {
        self.launch_count += 1;
        let _ = self.save();
        !self.permanently_dismissed
            && self.launch_count - self.last_prompt_at_launch >= STAR_PROMPT_INTERVAL
    }

    pub fn snooze(&mut self) {
        self.last_prompt_at_launch = self.launch_count;
        let _ = self.save();
    }

    pub fn dismiss_permanently(&mut self) {
        self.permanently_dismissed = true;
        let _ = self.save();
    }
}

// ---------------------------------------------------------------------------
// Config generation state — tracks directories where the user chose "Never"
// for the auto-config-prompt modal.
// ---------------------------------------------------------------------------

fn config_gen_state_path() -> PathBuf {
    if let Ok(p) = std::env::var("DOT_AGENT_DECK_CONFIG_GEN_STATE") {
        return PathBuf::from(p);
    }
    dirs_home().join(".config/dot-agent-deck/config-gen-state.json")
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ConfigGenState {
    pub suppressed_dirs: Vec<String>,
}

impl ConfigGenState {
    pub fn load() -> Self {
        let path = config_gen_state_path();
        match std::fs::read_to_string(&path) {
            Ok(contents) => match serde_json::from_str(&contents) {
                Ok(state) => state,
                Err(err) => {
                    eprintln!("Invalid config gen state at {}: {err}", path.display());
                    Self::default()
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(err) => {
                eprintln!(
                    "Failed to read config gen state at {}: {err}",
                    path.display()
                );
                Self::default()
            }
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let path = config_gen_state_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create config gen state directory: {e}"))?;
        }
        let contents = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize config gen state: {e}"))?;
        std::fs::write(&path, contents).map_err(|e| {
            format!(
                "Failed to write config gen state at {}: {e}",
                path.display()
            )
        })
    }

    pub fn is_suppressed(&self, dir: &str) -> bool {
        self.suppressed_dirs.iter().any(|d| d == dir)
    }

    pub fn suppress_dir(&mut self, dir: &str) {
        if !self.is_suppressed(dir) {
            self.suppressed_dirs.push(dir.to_string());
            let _ = self.save();
        }
    }
}

/// Serializes tests that mutate `DOT_AGENT_DECK_STATE_DIR` /
/// `XDG_STATE_HOME` / `HOME`. Rust runs unit tests in parallel and these are
/// process-global, so any test that wants to observe a specific value of
/// `state_dir()` must hold this lock for the duration of its env-var fiddling.
#[cfg(test)]
pub static STATE_DIR_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Serializes tests that mutate `DOT_AGENT_DECK_CONFIG_GEN_STATE` or call
/// `ConfigGenState::save()` / `load()` (directly or through handlers like
/// `handle_config_gen_prompt_key`). Rust runs unit tests in parallel, so
/// without this lock those tests race on the shared env var and on whatever
/// state file each one points it at.
#[cfg(test)]
pub(crate) static CONFIG_GEN_STATE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Test-only RAII guard that sets `DOT_AGENT_DECK_CONFIG_GEN_STATE` and
/// restores its prior value on drop, even if the test panics. Callers must
/// hold `CONFIG_GEN_STATE_ENV_LOCK` for the guard's lifetime.
#[cfg(test)]
pub(crate) struct ConfigGenStateEnvGuard {
    prev: Option<String>,
}

#[cfg(test)]
impl ConfigGenStateEnvGuard {
    pub(crate) fn set(value: &str) -> Self {
        let prev = std::env::var("DOT_AGENT_DECK_CONFIG_GEN_STATE").ok();
        // SAFETY: callers must hold CONFIG_GEN_STATE_ENV_LOCK for the
        // duration of this guard, which serializes env-var access.
        unsafe {
            std::env::set_var("DOT_AGENT_DECK_CONFIG_GEN_STATE", value);
        }
        Self { prev }
    }
}

#[cfg(test)]
impl Drop for ConfigGenStateEnvGuard {
    fn drop(&mut self) {
        // SAFETY: see ConfigGenStateEnvGuard::set.
        unsafe {
            match self.prev.take() {
                Some(v) => std::env::set_var("DOT_AGENT_DECK_CONFIG_GEN_STATE", v),
                None => std::env::remove_var("DOT_AGENT_DECK_CONFIG_GEN_STATE"),
            }
        }
    }
}

pub(crate) fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bell_config_defaults() {
        let bc = BellConfig::default();
        assert!(bc.enabled);
        assert!(bc.on_waiting_for_input);
        assert!(!bc.on_idle);
        assert!(bc.on_error);
    }

    #[test]
    fn bell_config_deserialize_empty() {
        let bc: BellConfig = toml::from_str("").unwrap();
        assert!(bc.enabled);
        assert!(bc.on_waiting_for_input);
        assert!(!bc.on_idle);
        assert!(bc.on_error);
    }

    #[test]
    fn bell_config_deserialize_partial() {
        let bc: BellConfig = toml::from_str("on_idle = true").unwrap();
        assert!(bc.enabled);
        assert!(bc.on_idle);
    }

    #[test]
    fn dashboard_config_without_bell_section() {
        let dc: DashboardConfig = toml::from_str(r#"default_command = "echo hi""#).unwrap();
        assert_eq!(dc.default_command, "echo hi");
        assert!(dc.bell.enabled);
    }

    #[test]
    fn dashboard_config_with_bell_section() {
        let toml_str = r#"
default_command = "test"

[bell]
enabled = false
on_idle = true
"#;
        let dc: DashboardConfig = toml::from_str(toml_str).unwrap();
        assert!(!dc.bell.enabled);
        assert!(dc.bell.on_idle);
        assert!(dc.bell.on_waiting_for_input);
    }

    #[test]
    fn should_bell_respects_enabled() {
        let bc = BellConfig {
            enabled: false,
            ..Default::default()
        };
        assert!(!bc.should_bell(&SessionStatus::WaitingForInput));
        assert!(!bc.should_bell(&SessionStatus::Error));
    }

    #[test]
    fn theme_defaults_to_auto() {
        let dc: DashboardConfig = toml::from_str("").unwrap();
        assert_eq!(dc.theme, Theme::Auto);
    }

    #[test]
    fn theme_deserialize_light() {
        let dc: DashboardConfig = toml::from_str(r#"theme = "light""#).unwrap();
        assert_eq!(dc.theme, Theme::Light);
    }

    #[test]
    fn theme_get_set_field() {
        let mut dc = DashboardConfig::default();
        assert_eq!(dc.get_field("theme").unwrap(), "auto");
        dc.set_field("theme", "dark").unwrap();
        assert_eq!(dc.theme, Theme::Dark);
        assert!(dc.set_field("theme", "invalid").is_err());
    }

    #[test]
    fn saved_session_round_trip() {
        let session = SavedSession {
            panes: vec![
                SavedPane {
                    dir: "/repo/api".to_string(),
                    name: "api".to_string(),
                    command: "claude".to_string(),
                    mode: None,
                },
                SavedPane {
                    dir: "/repo/ui".to_string(),
                    name: "ui".to_string(),
                    command: "".to_string(),
                    mode: None,
                },
            ],
        };
        let toml_str = toml::to_string_pretty(&session).unwrap();
        let loaded: SavedSession = toml::from_str(&toml_str).unwrap();
        assert_eq!(loaded.panes.len(), 2);
        assert_eq!(loaded.panes[0].dir, "/repo/api");
        assert_eq!(loaded.panes[0].name, "api");
        assert_eq!(loaded.panes[0].command, "claude");
        assert_eq!(loaded.panes[1].command, "");
    }

    #[test]
    fn saved_session_empty_default() {
        let session = SavedSession::default();
        assert!(session.panes.is_empty());
    }

    #[test]
    fn saved_session_deserialize_empty() {
        let session: SavedSession = toml::from_str("").unwrap();
        assert!(session.panes.is_empty());
    }

    #[test]
    fn saved_session_load_save_clear() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.toml");
        let prev = std::env::var("DOT_AGENT_DECK_SESSION").ok();
        // SAFETY: test is single-threaded; no other code reads this var concurrently.
        unsafe {
            std::env::set_var("DOT_AGENT_DECK_SESSION", path.to_str().unwrap());
        }

        // Load returns default when file missing
        let session = SavedSession::load();
        assert!(session.panes.is_empty());

        // Save then load round-trips
        let session = SavedSession {
            panes: vec![SavedPane {
                dir: "/tmp/test".to_string(),
                name: "test".to_string(),
                command: "echo hi".to_string(),
                mode: None,
            }],
        };
        session.save().unwrap();
        let loaded = SavedSession::load();
        assert_eq!(loaded.panes.len(), 1);
        assert_eq!(loaded.panes[0].dir, "/tmp/test");

        // Clear removes the file
        SavedSession::clear().unwrap();
        assert!(!path.exists());

        // SAFETY: test cleanup — restore original env var.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DOT_AGENT_DECK_SESSION", v),
                None => std::env::remove_var("DOT_AGENT_DECK_SESSION"),
            }
        }
    }

    #[test]
    fn should_bell_per_status() {
        let bc = BellConfig::default();
        assert!(bc.should_bell(&SessionStatus::WaitingForInput));
        assert!(!bc.should_bell(&SessionStatus::Idle));
        assert!(bc.should_bell(&SessionStatus::Error));
        assert!(!bc.should_bell(&SessionStatus::Thinking));
        assert!(!bc.should_bell(&SessionStatus::Working));
        assert!(!bc.should_bell(&SessionStatus::Compacting));
    }

    #[test]
    fn star_prompt_default_values() {
        let state = StarPromptState::default();
        assert_eq!(state.launch_count, 0);
        assert!(!state.permanently_dismissed);
        assert_eq!(state.last_prompt_at_launch, 0);
    }

    #[test]
    fn star_prompt_serde_round_trip() {
        let state = StarPromptState {
            launch_count: 42,
            permanently_dismissed: true,
            last_prompt_at_launch: 30,
        };
        let json = serde_json::to_string(&state).unwrap();
        let loaded: StarPromptState = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.launch_count, 42);
        assert!(loaded.permanently_dismissed);
        assert_eq!(loaded.last_prompt_at_launch, 30);
    }

    #[test]
    fn star_prompt_serde_missing_fields() {
        let loaded: StarPromptState = serde_json::from_str("{}").unwrap();
        assert_eq!(loaded.launch_count, 0);
        assert!(!loaded.permanently_dismissed);
        assert_eq!(loaded.last_prompt_at_launch, 0);
    }

    #[test]
    fn star_prompt_increment_and_check_triggers_at_10() {
        // Test pure logic without file I/O — manually track state
        let mut state = StarPromptState::default();
        for i in 1..=9 {
            state.launch_count = i;
            let should_show = !state.permanently_dismissed
                && state.launch_count - state.last_prompt_at_launch >= STAR_PROMPT_INTERVAL;
            assert!(!should_show, "should not trigger at launch {i}");
        }
        state.launch_count = 10;
        let should_show = !state.permanently_dismissed
            && state.launch_count - state.last_prompt_at_launch >= STAR_PROMPT_INTERVAL;
        assert!(should_show, "should trigger at launch 10");
    }

    #[test]
    fn star_prompt_snooze_resets_window() {
        let mut state = StarPromptState::default();
        state.launch_count = 10;
        state.last_prompt_at_launch = state.launch_count; // snooze
        for i in 11..=19 {
            state.launch_count = i;
            let should_show = !state.permanently_dismissed
                && state.launch_count - state.last_prompt_at_launch >= STAR_PROMPT_INTERVAL;
            assert!(!should_show, "should not trigger at launch {i}");
        }
        state.launch_count = 20;
        let should_show = !state.permanently_dismissed
            && state.launch_count - state.last_prompt_at_launch >= STAR_PROMPT_INTERVAL;
        assert!(should_show, "should trigger at launch 20");
    }

    #[test]
    fn star_prompt_dismiss_permanently() {
        let mut state = StarPromptState {
            permanently_dismissed: true,
            ..StarPromptState::default()
        };
        for i in 1..=20 {
            state.launch_count = i;
            let should_show = !state.permanently_dismissed
                && state.launch_count - state.last_prompt_at_launch >= STAR_PROMPT_INTERVAL;
            assert!(!should_show, "dismissed state should never trigger");
        }
    }

    #[test]
    fn star_prompt_load_save_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("star.json");
        let prev = std::env::var("DOT_AGENT_DECK_STAR_PROMPT").ok();
        // SAFETY: test is single-threaded; no other code reads this var concurrently.
        unsafe {
            std::env::set_var("DOT_AGENT_DECK_STAR_PROMPT", path.to_str().unwrap());
        }

        let state = StarPromptState {
            launch_count: 15,
            permanently_dismissed: false,
            last_prompt_at_launch: 10,
        };
        state.save().unwrap();

        let loaded = StarPromptState::load();
        assert_eq!(loaded.launch_count, 15);
        assert!(!loaded.permanently_dismissed);
        assert_eq!(loaded.last_prompt_at_launch, 10);

        // Load from corrupted file returns default
        std::fs::write(&path, "not valid json!!!").unwrap();
        let loaded = StarPromptState::load();
        assert_eq!(loaded.launch_count, 0);

        // Load from missing file returns default
        std::fs::remove_file(&path).unwrap();
        let loaded = StarPromptState::load();
        assert_eq!(loaded.launch_count, 0);
        assert!(!loaded.permanently_dismissed);

        // SAFETY: test cleanup — restore original env var.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DOT_AGENT_DECK_STAR_PROMPT", v),
                None => std::env::remove_var("DOT_AGENT_DECK_STAR_PROMPT"),
            }
        }
    }

    #[test]
    fn idle_art_config_defaults() {
        let config = IdleArtConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.provider, "anthropic");
        assert_eq!(config.model, "claude-haiku-4-5");
        assert_eq!(config.timeout_secs, 300);
    }

    #[test]
    fn dashboard_config_without_idle_art() {
        let dc: DashboardConfig = toml::from_str("").unwrap();
        assert!(!dc.idle_art.enabled);
        assert_eq!(dc.idle_art.provider, "anthropic");
        assert_eq!(dc.idle_art.model, "claude-haiku-4-5");
    }

    #[test]
    fn dashboard_config_with_idle_art() {
        let toml_str = r#"
[idle_art]
enabled = true
provider = "openai"
model = "gpt-4o-mini"
timeout_secs = 600
"#;
        let dc: DashboardConfig = toml::from_str(toml_str).unwrap();
        assert!(dc.idle_art.enabled);
        assert_eq!(dc.idle_art.provider, "openai");
        assert_eq!(dc.idle_art.model, "gpt-4o-mini");
        assert_eq!(dc.idle_art.timeout_secs, 600);
    }

    #[test]
    fn idle_art_get_set_fields() {
        let mut dc = DashboardConfig::default();
        assert_eq!(dc.get_field("idle_art.enabled").unwrap(), "false");
        assert_eq!(dc.get_field("idle_art.provider").unwrap(), "anthropic");
        assert_eq!(dc.get_field("idle_art.model").unwrap(), "claude-haiku-4-5");
        assert_eq!(dc.get_field("idle_art.timeout_secs").unwrap(), "300");

        dc.set_field("idle_art.enabled", "true").unwrap();
        assert!(dc.idle_art.enabled);

        dc.set_field("idle_art.provider", "ollama").unwrap();
        assert_eq!(dc.idle_art.provider, "ollama");

        dc.set_field("idle_art.model", "llama3").unwrap();
        assert_eq!(dc.idle_art.model, "llama3");

        dc.set_field("idle_art.timeout_secs", "120").unwrap();
        assert_eq!(dc.idle_art.timeout_secs, 120);

        assert!(dc.set_field("idle_art.enabled", "notabool").is_err());
        assert!(dc.set_field("idle_art.timeout_secs", "notanumber").is_err());
    }

    #[test]
    fn auto_config_prompt_defaults_to_true() {
        let dc = DashboardConfig::default();
        assert!(dc.auto_config_prompt);
    }

    #[test]
    fn auto_config_prompt_deserialize_missing() {
        let dc: DashboardConfig = toml::from_str("").unwrap();
        assert!(dc.auto_config_prompt);
    }

    #[test]
    fn auto_config_prompt_deserialize_false() {
        let dc: DashboardConfig = toml::from_str("auto_config_prompt = false").unwrap();
        assert!(!dc.auto_config_prompt);
    }

    #[test]
    fn state_dir_uses_explicit_override_first() {
        let _guard = STATE_DIR_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev_state = std::env::var("DOT_AGENT_DECK_STATE_DIR").ok();
        let prev_xdg = std::env::var("XDG_STATE_HOME").ok();
        // SAFETY: env-var lock held; restored on the way out.
        unsafe {
            std::env::set_var("DOT_AGENT_DECK_STATE_DIR", "/tmp/explicit-state");
            std::env::set_var("XDG_STATE_HOME", "/should/be/ignored");
        }

        assert_eq!(state_dir(), PathBuf::from("/tmp/explicit-state"));

        // SAFETY: same lock held; restoring previous values.
        unsafe {
            match prev_state {
                Some(v) => std::env::set_var("DOT_AGENT_DECK_STATE_DIR", v),
                None => std::env::remove_var("DOT_AGENT_DECK_STATE_DIR"),
            }
            match prev_xdg {
                Some(v) => std::env::set_var("XDG_STATE_HOME", v),
                None => std::env::remove_var("XDG_STATE_HOME"),
            }
        }
    }

    #[test]
    fn state_dir_uses_xdg_state_home_when_set() {
        let _guard = STATE_DIR_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev_state = std::env::var("DOT_AGENT_DECK_STATE_DIR").ok();
        let prev_xdg = std::env::var("XDG_STATE_HOME").ok();
        // SAFETY: env-var lock held; restored on the way out.
        unsafe {
            std::env::remove_var("DOT_AGENT_DECK_STATE_DIR");
            std::env::set_var("XDG_STATE_HOME", "/var/lib/state");
        }

        assert_eq!(state_dir(), PathBuf::from("/var/lib/state/dot-agent-deck"));

        // SAFETY: same lock held; restoring previous values.
        unsafe {
            match prev_state {
                Some(v) => std::env::set_var("DOT_AGENT_DECK_STATE_DIR", v),
                None => std::env::remove_var("DOT_AGENT_DECK_STATE_DIR"),
            }
            match prev_xdg {
                Some(v) => std::env::set_var("XDG_STATE_HOME", v),
                None => std::env::remove_var("XDG_STATE_HOME"),
            }
        }
    }

    #[test]
    fn state_dir_falls_back_to_home_when_xdg_unset() {
        let _guard = STATE_DIR_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev_state = std::env::var("DOT_AGENT_DECK_STATE_DIR").ok();
        let prev_xdg = std::env::var("XDG_STATE_HOME").ok();
        let prev_home = std::env::var("HOME").ok();
        // SAFETY: env-var lock held; restored on the way out.
        unsafe {
            std::env::remove_var("DOT_AGENT_DECK_STATE_DIR");
            std::env::remove_var("XDG_STATE_HOME");
            std::env::set_var("HOME", "/home/test-user");
        }

        assert_eq!(
            state_dir(),
            PathBuf::from("/home/test-user/.local/state/dot-agent-deck")
        );

        // SAFETY: same lock held; restoring previous values.
        unsafe {
            match prev_state {
                Some(v) => std::env::set_var("DOT_AGENT_DECK_STATE_DIR", v),
                None => std::env::remove_var("DOT_AGENT_DECK_STATE_DIR"),
            }
            match prev_xdg {
                Some(v) => std::env::set_var("XDG_STATE_HOME", v),
                None => std::env::remove_var("XDG_STATE_HOME"),
            }
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[test]
    fn auto_config_prompt_get_set_field() {
        let mut dc = DashboardConfig::default();
        assert_eq!(dc.get_field("auto_config_prompt").unwrap(), "true");
        dc.set_field("auto_config_prompt", "false").unwrap();
        assert!(!dc.auto_config_prompt);
        assert_eq!(dc.get_field("auto_config_prompt").unwrap(), "false");
        assert!(dc.set_field("auto_config_prompt", "notbool").is_err());
    }

    #[test]
    fn config_gen_state_default_empty() {
        let state = ConfigGenState::default();
        assert!(state.suppressed_dirs.is_empty());
    }

    #[test]
    fn config_gen_state_suppress_and_check() {
        let mut state = ConfigGenState::default();
        assert!(!state.is_suppressed("/some/dir"));
        state.suppressed_dirs.push("/some/dir".to_string());
        assert!(state.is_suppressed("/some/dir"));
        assert!(!state.is_suppressed("/other/dir"));
    }

    #[test]
    fn config_gen_state_suppress_dir_deduplicates() {
        // suppress_dir() calls save(), which reads DOT_AGENT_DECK_CONFIG_GEN_STATE.
        // Hold the env-var lock and point at a temp path so we neither race
        // against load_save_cycle nor pollute the real home dir.
        let _guard = CONFIG_GEN_STATE_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config-gen-state.json");
        // Drop guard restores the env var even if an assertion below panics.
        let _env_restore = ConfigGenStateEnvGuard::set(path.to_str().unwrap());

        let mut state = ConfigGenState::default();
        state.suppressed_dirs.push("/dup".to_string());
        state.suppressed_dirs.push("/dup".to_string()); // manual dup
        // suppress_dir should not add again
        assert_eq!(state.suppressed_dirs.len(), 2);
        // But the method itself checks before adding
        let mut state2 = ConfigGenState::default();
        state2.suppressed_dirs.push("/dup".to_string());
        state2.suppress_dir("/dup");
        assert_eq!(state2.suppressed_dirs.len(), 1);
    }

    #[test]
    fn config_gen_state_serde_round_trip() {
        let state = ConfigGenState {
            suppressed_dirs: vec!["/a".to_string(), "/b".to_string()],
        };
        let json = serde_json::to_string(&state).unwrap();
        let loaded: ConfigGenState = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.suppressed_dirs.len(), 2);
        assert!(loaded.is_suppressed("/a"));
        assert!(loaded.is_suppressed("/b"));
    }

    #[test]
    fn config_gen_state_load_save_cycle() {
        // Serialize against any other test that touches this env var or calls
        // save()/load() — Rust runs unit tests in parallel.
        let _guard = CONFIG_GEN_STATE_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config-gen-state.json");
        let prev = std::env::var("DOT_AGENT_DECK_CONFIG_GEN_STATE").ok();
        // SAFETY: env-var lock held for the duration of this test.
        unsafe {
            std::env::set_var("DOT_AGENT_DECK_CONFIG_GEN_STATE", path.to_str().unwrap());
        }

        // Load returns default when file missing
        let state = ConfigGenState::load();
        assert!(state.suppressed_dirs.is_empty());

        // Save then load round-trips
        let mut state = ConfigGenState::default();
        state.suppressed_dirs.push("/test/dir".to_string());
        state.save().unwrap();
        let loaded = ConfigGenState::load();
        assert_eq!(loaded.suppressed_dirs.len(), 1);
        assert!(loaded.is_suppressed("/test/dir"));

        // Load from corrupted file returns default
        std::fs::write(&path, "not valid json!!!").unwrap();
        let loaded = ConfigGenState::load();
        assert!(loaded.suppressed_dirs.is_empty());

        // SAFETY: test cleanup — restore original env var.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DOT_AGENT_DECK_CONFIG_GEN_STATE", v),
                None => std::env::remove_var("DOT_AGENT_DECK_CONFIG_GEN_STATE"),
            }
        }
    }
}
