use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::state::SessionStatus;

pub const CONFIG_KEYS: &[(&str, &str)] = &[
    ("default_command", "Default shell command for new panes"),
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

    // PRD #93 reviewer REV-2: the `/tmp` fallback must include the uid so
    // two users on the same host can't collide on the same socket path
    // (the daemon is per-user; the 0o600 mode is on the socket inode, but
    // the *path* still has to be unique, otherwise the loser's `bind(2)`
    // sees `EADDRINUSE` against the winner's inode). Same rationale as
    // `attach_socket_path` below.
    PathBuf::from(format!("/tmp/dot-agent-deck-{}.sock", current_uid()))
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

    // PRD #93 reviewer REV-2: include the uid in the `/tmp` fallback path so
    // two users on the same host get disjoint sockets (each daemon's
    // `bind(2)` would otherwise collide with the other user's inode), and
    // so the path itself can't be observed by another user to figure out
    // *which* deck process to target. The 0o600 mode on the inode is
    // already enforced; the per-user path is the missing half.
    PathBuf::from(format!("/tmp/dot-agent-deck-attach-{}.sock", current_uid()))
}

/// Current OS uid, used to namespace the `/tmp` fallback sockets per user.
/// Wraps `libc::getuid` so the unsafe is centralized in one place.
fn current_uid() -> u32 {
    // SAFETY: `getuid(2)` is async-signal-safe and has no failure mode; it
    // simply returns the calling process's real uid.
    unsafe { libc::getuid() }
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
    pub idle_art: IdleArtConfig,
    pub auto_config_prompt: bool,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            default_command: String::new(),
            bell: BellConfig::default(),
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
    /// When set, this pane was the orchestrator pane of an orchestration
    /// tab; the snapshot carries enough metadata to rebuild the whole tab
    /// (orchestrator + role panes, prompt, role order, start cursor) on the
    /// daemon-empty restore path. `Option` + `#[serde(default)]` so older
    /// `session.toml` files (no `orchestration` key) still parse with
    /// `orchestration == None`. See [`OrchestrationSnapshot`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orchestration: Option<OrchestrationSnapshot>,
}

/// PRD #89 M2b.2 — orchestration metadata captured on a saved pane so the
/// daemon-empty restore path (fresh machine / crash recovery, when there is
/// no warm daemon to hydrate from) can rebuild the orchestration tab. Schema
/// ported from the closed PRD #74 design.
///
/// Carried as `SavedPane::orchestration: Option<OrchestrationSnapshot>` with
/// `#[serde(default)]`, so a `session.toml` written before this field existed
/// (no `[panes.orchestration]` table) still parses, yielding
/// `orchestration == None`. A `version` field is present from day one so a
/// future schema change can be migrated rather than silently dropped. No
/// `#[serde(deny_unknown_fields)]` — forward-compat with snapshots a newer
/// binary may write with extra keys.
///
/// This struct ONLY captures the metadata + its (de)serialization; the
/// restore branch that rebuilds the tab from it is M2b.3 (a separate step).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OrchestrationSnapshot {
    /// Schema version, for future migration. `1` is the initial format.
    pub version: u32,
    /// Role names in DISPLAY order — the same order as the tab's
    /// `role_pane_ids`, so the restore branch can recreate the role panes
    /// in the order the user saw them.
    pub roles: Vec<String>,
    /// Index into `roles` of the start (orchestrator) role — restores the
    /// "next role to start" cursor.
    pub start_role_index: usize,
    /// Pre-built prompt injected into the start (orchestrator) role on
    /// restore. Empty string when the orchestration had no prompt.
    pub orchestrator_prompt: String,
    /// Resolved orchestration config NAME — half of the reference used to
    /// re-resolve the `OrchestrationConfig` from disk on restore.
    pub config_name: String,
    /// Project PATH the orchestration was resolved from (the directory that
    /// holds `.dot-agent-deck.toml`) — the other half of the re-resolution
    /// reference.
    pub project_path: String,
    /// Which roles had been started, by index into `roles`. Optional —
    /// snapshots that predate this field load with an empty list.
    #[serde(default)]
    pub started_role_indices: Vec<usize>,
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
    /// the mode field, breaking auto-restore of the mode tab (PRD #69).
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

/// PRD #89 M1.2 — leading-edge throttle that coalesces saved-session snapshot
/// writes so a burst of meaningful state changes (e.g. orchestration setup
/// spawning many panes) produces one or two disk writes, not one per change.
///
/// Behaviour: the first pending change writes immediately (leading edge), then
/// writes are throttled to at most one per `interval`; a single trailing write
/// flushes whatever accumulated while the throttle was closed. So a tight burst
/// collapses to ≤2 writes (one leading + one trailing), and sustained activity
/// is bounded to ~one write per `interval` regardless of how many changes occur.
///
/// Pure data + logic: the caller supplies the clock as a monotonic [`Duration`]
/// from an arbitrary epoch (in production, `epoch.elapsed()`; in tests, any
/// value), so it is fully unit-testable without wall-clock sleeps.
#[derive(Debug, Clone)]
pub struct SnapshotCoalescer {
    /// Minimum spacing between disk writes.
    interval: Duration,
    /// A change is pending (recorded but not yet flushed to disk).
    dirty: bool,
    /// Clock value of the last write; `None` until the first write happens.
    last_write: Option<Duration>,
}

impl SnapshotCoalescer {
    /// Create a coalescer that allows at most one write per `interval`.
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            dirty: false,
            last_write: None,
        }
    }

    /// Record that a meaningful state change occurred. Does not write — the
    /// actual coalesced write happens when the caller next sees [`Self::is_due`]
    /// return `true`.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Whether a write is due at `now`: a change is pending AND either nothing
    /// has been written yet (leading edge) or at least `interval` has elapsed
    /// since the last write (trailing edge / throttle release).
    pub fn is_due(&self, now: Duration) -> bool {
        if !self.dirty {
            return false;
        }
        match self.last_write {
            None => true,
            Some(last) => now.saturating_sub(last) >= self.interval,
        }
    }

    /// Mark a write as completed at `now`, clearing the pending flag and arming
    /// the throttle so the next write waits a full `interval`.
    pub fn record_write(&mut self, now: Duration) {
        self.dirty = false;
        self.last_write = Some(now);
    }
}

// ---------------------------------------------------------------------------
// Scheduled tasks — global, daemon-owned config (PRD #127, M1.2)
// ---------------------------------------------------------------------------

/// One `[[scheduled_tasks]]` entry from the global
/// `~/.config/dot-agent-deck/schedules.toml`. The daemon's job list. See PRD
/// #127 "Configuration: global, daemon-owned".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScheduledTask {
    /// Reuse-registry key; unique per daemon. Renaming is forbidden via the
    /// edit path (it orphans the reused tab) — treat as remove + add.
    pub name: String,
    /// Cron expression (5-field POSIX or 6/7-field). Validated by the
    /// scheduler / CLI before write. Evaluated in local time.
    pub cron: String,
    /// Spawn target directory. `~` and `$VAR` are expanded at load time
    /// (see [`expand_path`]); relative paths resolve against `$HOME`.
    pub working_dir: String,
    /// Single-agent command (mirrors the new-deck dialog); ignored when the
    /// target dir defines `[[orchestrations]]`. Required: a missing or blank
    /// value is rejected at load time (see [`validate_task`]) — there is no
    /// `$SHELL` fallback for scheduled tasks. Kept `Option` only so the file
    /// shape round-trips and the absence can be reported as a load error rather
    /// than a parse failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// The prompt delivered to the spawned agent / orchestrator role.
    pub prompt: String,
    /// Open a fresh tab on every fire instead of reusing one. Default false
    /// (reuse — the dominant access pattern; see PRD "Tab lifecycle").
    #[serde(default)]
    pub new_tab_per_fire: bool,
    /// Whether the daemon registers and fires this task. Default true.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

/// Internal mirror of the file shape so a well-formed file deserializes in one
/// shot; the robust loader below falls back to per-entry parsing when the
/// strict parse fails, so one bad entry can't block the rest.
#[derive(Debug, Default, Deserialize)]
struct SchedulesFile {
    #[serde(default)]
    scheduled_tasks: Vec<ScheduledTask>,
}

/// A per-entry (or file-level) load failure. `entry` is the array index when
/// the failure is attributable to a single `[[scheduled_tasks]]` block, `None`
/// for a file-level error. The caller surfaces these via the scheduler's
/// notification seam (PRD #126) — a malformed entry never crashes the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleLoadError {
    pub entry: Option<usize>,
    pub message: String,
}

/// Result of loading the global schedules config: the entries that parsed
/// (with paths expanded), plus any per-entry / file-level errors.
#[derive(Debug, Default, Clone)]
pub struct LoadedSchedules {
    pub tasks: Vec<ScheduledTask>,
    pub errors: Vec<ScheduleLoadError>,
}

/// Global schedules path: `$XDG_CONFIG_HOME/dot-agent-deck/schedules.toml`,
/// falling back to `~/.config/...`. `DOT_AGENT_DECK_SCHEDULES` overrides it so
/// tests never touch the real home dir.
pub fn schedules_path() -> PathBuf {
    if let Ok(p) = std::env::var("DOT_AGENT_DECK_SCHEDULES") {
        return PathBuf::from(p);
    }
    match std::env::var("XDG_CONFIG_HOME") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir).join("dot-agent-deck/schedules.toml"),
        _ => dirs_home().join(".config/dot-agent-deck/schedules.toml"),
    }
}

impl LoadedSchedules {
    /// Load from the global [`schedules_path`].
    pub fn load() -> Self {
        Self::load_from(&schedules_path())
    }

    /// Load from an explicit path (tests, and any future supervised-mode
    /// override). A missing file is not an error — it yields an empty set.
    pub fn load_from(path: &std::path::Path) -> Self {
        let contents = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Self::default();
            }
            Err(err) => {
                return Self {
                    tasks: Vec::new(),
                    errors: vec![ScheduleLoadError {
                        entry: None,
                        message: format!("failed to read {}: {err}", path.display()),
                    }],
                };
            }
        };
        Self::parse(&contents)
    }

    /// Parse schedules from a TOML string with robust per-entry handling: a
    /// single malformed `[[scheduled_tasks]]` entry is reported as an error and
    /// skipped without blocking the valid entries.
    pub fn parse(contents: &str) -> Self {
        // Fast path: the whole file is well-formed TOML. Each entry still has to
        // clear the semantic check below (a command-less entry is rejected), so
        // we validate per-entry even on this path rather than blindly collecting.
        if let Ok(file) = toml::from_str::<SchedulesFile>(contents) {
            let mut out = Self::default();
            for (i, task) in file.scheduled_tasks.into_iter().enumerate() {
                match validate_task(task, i) {
                    Ok(task) => out.tasks.push(task),
                    Err(err) => out.errors.push(err),
                }
            }
            return out;
        }

        // Slow path: parse to a generic table, then deserialize each
        // `[[scheduled_tasks]]` entry individually so one bad entry doesn't
        // take the others down with it.
        let table: toml::Table = match contents.parse() {
            Ok(t) => t,
            Err(err) => {
                return Self {
                    tasks: Vec::new(),
                    errors: vec![ScheduleLoadError {
                        entry: None,
                        message: format!("malformed TOML: {err}"),
                    }],
                };
            }
        };

        let mut out = Self::default();
        let Some(value) = table.get("scheduled_tasks") else {
            return out;
        };
        let Some(entries) = value.as_array() else {
            out.errors.push(ScheduleLoadError {
                entry: None,
                message: "`scheduled_tasks` must be an array of tables".to_string(),
            });
            return out;
        };

        for (i, entry) in entries.iter().enumerate() {
            match entry.clone().try_into::<ScheduledTask>() {
                Ok(task) => match validate_task(task, i) {
                    Ok(task) => out.tasks.push(task),
                    Err(err) => out.errors.push(err),
                },
                Err(err) => out.errors.push(ScheduleLoadError {
                    entry: Some(i),
                    message: err.to_string(),
                }),
            }
        }
        out
    }
}

/// Validate a freshly-parsed task and apply load-time path expansion. A
/// hand-edited entry with no (or blank) `command` is REJECTED here (PRD #127
/// follow-up, USER DECISION): a scheduled task needs an agent command to act on
/// its prompt, and there is no silent `$SHELL` fallback. Rejection mirrors the
/// malformed-entry path — the error is surfaced via the daemon's notification
/// seam (PRD #126) and the entry is skipped, without blocking valid siblings or
/// crashing the daemon.
fn validate_task(task: ScheduledTask, index: usize) -> Result<ScheduledTask, ScheduleLoadError> {
    match &task.command {
        Some(cmd) if !cmd.trim().is_empty() => Ok(expand_task(task)),
        _ => Err(ScheduleLoadError {
            entry: Some(index),
            message: format!(
                "scheduled task {:?} has no `command`; a command is required \
                 (a scheduled task needs an agent command to act on its prompt — \
                 there is no $SHELL fallback)",
                task.name
            ),
        }),
    }
}

/// Apply load-time path expansion to a task's `working_dir`.
fn expand_task(mut task: ScheduledTask) -> ScheduledTask {
    task.working_dir = expand_path(&task.working_dir);
    task
}

/// Expand `~` and `$VAR` / `${VAR}` in a path, then resolve a relative result
/// against `$HOME` (NOT any agent cwd — the authoring agent's cwd is
/// irrelevant for a global daemon). PRD #127 Open Q7.
pub fn expand_path(input: &str) -> String {
    let home = dirs_home();

    // `~` / `~/...` → home.
    let after_tilde = if input == "~" {
        return home.to_string_lossy().into_owned();
    } else if let Some(rest) = input.strip_prefix("~/") {
        format!("{}/{}", home.to_string_lossy(), rest)
    } else {
        input.to_string()
    };

    let expanded = expand_env_vars(&after_tilde);

    // Resolve a still-relative path against $HOME.
    if expanded.starts_with('/') {
        expanded
    } else {
        home.join(&expanded).to_string_lossy().into_owned()
    }
}

/// Substitute `$VAR` and `${VAR}` with their environment values. An undefined
/// variable expands to the empty string (matching common shell-ish behavior
/// without failing the whole load). A `$` that does not begin a valid variable
/// reference is left untouched.
fn expand_env_vars(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            // ${VAR}
            Some('{') => {
                chars.next(); // consume '{'
                let mut name = String::new();
                let mut closed = false;
                for nc in chars.by_ref() {
                    if nc == '}' {
                        closed = true;
                        break;
                    }
                    name.push(nc);
                }
                if closed && !name.is_empty() {
                    out.push_str(&std::env::var(&name).unwrap_or_default());
                } else {
                    // Not a well-formed reference — emit verbatim.
                    out.push('$');
                    out.push('{');
                    out.push_str(&name);
                }
            }
            // $VAR — name is [A-Za-z_][A-Za-z0-9_]*
            Some(&first) if first == '_' || first.is_ascii_alphabetic() => {
                let mut name = String::new();
                while let Some(&nc) = chars.peek() {
                    if nc == '_' || nc.is_ascii_alphanumeric() {
                        name.push(nc);
                        chars.next();
                    } else {
                        break;
                    }
                }
                out.push_str(&std::env::var(&name).unwrap_or_default());
            }
            // Lone `$` — leave it.
            _ => out.push('$'),
        }
    }
    out
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

// ---------------------------------------------------------------------------
// Experimental feature flag — `[features]` table in `.dot-agent-deck.toml`
// (PRD #139). The flag plumbing lives in `crate::features`; this module owns
// only the parse + env-merge + file-load helpers it builds on.
// ---------------------------------------------------------------------------

/// Env var that overrides the `[features] experimental` value. A
/// case-insensitive `1`/`true` forces the flag ON; any other set value
/// forces it OFF. Env WINS over the file (PRD #139 OQ3), so once it is set,
/// file edits to that field are ignored on reload.
pub const EXPERIMENTAL_ENV: &str = "DOT_AGENT_DECK_EXPERIMENTAL";

/// Internal mirror of the `.dot-agent-deck.toml` shape for the `[features]`
/// table only. Every other key (`[[modes]]`, `[[orchestrations]]`, …) is
/// ignored, so this loader is decoupled from `ProjectConfig`'s schema and an
/// absent `[features]` table deserializes to the default (experimental =
/// false).
#[derive(Debug, Default, Deserialize)]
struct FeaturesFile {
    #[serde(default)]
    features: crate::features::Features,
}

/// Parse the `[features]` table out of `.dot-agent-deck.toml` contents. An
/// absent table (or empty file) yields the default (`experimental = false`).
/// Returns `Err` on malformed TOML so the hot-reload path can keep the
/// previous value (PRD #139 M2.1).
pub fn parse_features(contents: &str) -> Result<crate::features::Features, toml::de::Error> {
    Ok(toml::from_str::<FeaturesFile>(contents)?.features)
}

/// Apply the env override to a file-derived value. `DOT_AGENT_DECK_EXPERIMENTAL`
/// WINS over the file when set (OQ3): a case-insensitive `1`/`true` forces ON,
/// any other set value forces OFF, and an unset var defers to `file`.
pub fn resolve_features(file: crate::features::Features) -> crate::features::Features {
    let experimental = match std::env::var(EXPERIMENTAL_ENV) {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            v == "1" || v == "true"
        }
        Err(_) => file.experimental,
    };
    crate::features::Features { experimental }
}

/// Path of the `.dot-agent-deck.toml` whose `[features]` table backs the
/// flag — the file in the current working directory, so the TUI and daemon
/// (launched in the same dir) read the same source of truth.
/// `DOT_AGENT_DECK_FEATURES_CONFIG` is an explicit override so tests never
/// touch the real cwd.
pub fn features_config_path() -> PathBuf {
    if let Ok(p) = std::env::var("DOT_AGENT_DECK_FEATURES_CONFIG") {
        return PathBuf::from(p);
    }
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(crate::project_config::CONFIG_FILE_NAME)
}

/// Upper bound on the `.dot-agent-deck.toml` the feature-flag loader will
/// read. A `[features]` table is a handful of bytes; this cap stops a
/// pathological `DOT_AGENT_DECK_FEATURES_CONFIG` target (a huge regular file)
/// from exhausting memory on the detached ~2s watcher thread (audit LOW-1).
const MAX_FEATURES_CONFIG_BYTES: u64 = 64 * 1024;

/// Load the `[features]` table from `path`. A missing file is the default
/// (OFF). A non-regular target (FIFO, device, …), an oversized file, or
/// malformed/partial TOML keeps `previous` — the partial-write tolerance the
/// watcher relies on (PRD #139 M2.1) plus the runaway-target guard from audit
/// LOW-1. Warnings never echo file content (audit INFO-2): only the path is
/// logged, so pointing the override at a sensitive file can't leak its bytes.
pub fn load_features_file(
    path: &std::path::Path,
    previous: crate::features::Features,
) -> crate::features::Features {
    use std::io::Read;

    // Stat first so a non-regular or oversized target is rejected before any
    // read. `metadata` follows symlinks, so a symlink to /dev/zero or a FIFO
    // is caught by the `is_file()` check on the resolved target (audit LOW-1).
    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return crate::features::Features::default();
        }
        Err(_) => {
            tracing::warn!(
                "failed to stat {}; keeping previous experimental={}",
                path.display(),
                previous.experimental
            );
            return previous;
        }
    };
    if !metadata.is_file() {
        tracing::warn!(
            "features config {} is not a regular file; keeping previous experimental={}",
            path.display(),
            previous.experimental
        );
        return previous;
    }
    if metadata.len() > MAX_FEATURES_CONFIG_BYTES {
        tracing::warn!(
            "features config {} exceeds {MAX_FEATURES_CONFIG_BYTES} bytes; keeping previous experimental={}",
            path.display(),
            previous.experimental
        );
        return previous;
    }

    // Read with a hard cap as defense-in-depth against a TOCTOU grow between
    // the stat above and this read.
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => {
            tracing::warn!(
                "failed to open {}; keeping previous experimental={}",
                path.display(),
                previous.experimental
            );
            return previous;
        }
    };
    let mut contents = String::new();
    if file
        .take(MAX_FEATURES_CONFIG_BYTES)
        .read_to_string(&mut contents)
        .is_err()
    {
        tracing::warn!(
            "failed to read {}; keeping previous experimental={}",
            path.display(),
            previous.experimental
        );
        return previous;
    }

    match parse_features(&contents) {
        Ok(features) => features,
        // audit INFO-2: never include the toml error's Display — it embeds a
        // snippet of the offending input, which could leak a sensitive file's
        // contents if the override path is pointed at one.
        Err(_) => {
            tracing::warn!(
                "invalid [features] table in {}: malformed TOML; keeping previous experimental={}",
                path.display(),
                previous.experimental
            );
            previous
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spec::spec;

    /// Scenario: Drive a `SnapshotCoalescer` (interval 500ms) synchronously with
    /// a 50-change burst all observed at the same instant — after each
    /// `mark_dirty` the simulated event loop checks `is_due(now)` and writes if
    /// due — then perform one trailing check after the interval has elapsed.
    /// The leading-edge write fires on the first change; the remaining 49 are
    /// throttled; the trailing check flushes the accumulated burst — so the
    /// burst collapses to at most two writes (here exactly two), never the 50 a
    /// naive write-per-change would produce.
    #[spec("session/save/003")]
    #[test]
    fn save_003_coalesces_burst_to_at_most_two_writes() {
        let interval = Duration::from_millis(500);
        let mut coalescer = SnapshotCoalescer::new(interval);
        let mut writes = 0usize;

        // A tight burst: 50 rapid changes, all at the same instant (now = 0),
        // each followed by the event loop's `is_due` check — exactly how the
        // main loop drives it. Only the leading-edge write should fire here.
        let now = Duration::ZERO;
        for _ in 0..50 {
            coalescer.mark_dirty();
            if coalescer.is_due(now) {
                writes += 1;
                coalescer.record_write(now);
            }
        }

        // The loop keeps ticking; once `interval` has elapsed with a change
        // still pending, the single trailing write flushes the coalesced burst.
        let after = interval;
        if coalescer.is_due(after) {
            writes += 1;
            coalescer.record_write(after);
        }

        assert!(
            writes <= 2,
            "a burst of 50 changes must coalesce to at most two disk writes, got {writes}"
        );
        assert!(
            writes >= 1,
            "the burst must still produce at least one write, got {writes}"
        );

        // After the trailing flush nothing is pending, so no further write is
        // due no matter how far the clock advances.
        assert!(
            !coalescer.is_due(after + interval + interval),
            "no write is due once the burst has been flushed and nothing new is dirty"
        );
    }

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
    fn saved_session_round_trip() {
        let session = SavedSession {
            panes: vec![
                SavedPane {
                    dir: "/repo/api".to_string(),
                    name: "api".to_string(),
                    command: "claude".to_string(),
                    mode: None,
                    orchestration: None,
                },
                SavedPane {
                    dir: "/repo/ui".to_string(),
                    name: "ui".to_string(),
                    command: "".to_string(),
                    mode: None,
                    orchestration: None,
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

    /// Scenario: Build a `SavedSession` whose single pane carries an
    /// `OrchestrationSnapshot` (3 roles in display order, a start-role cursor,
    /// an orchestrator prompt, the resolved config name + project path, and a
    /// started-roles list), serialize it to TOML and deserialize it back —
    /// asserting every orchestration field round-trips intact. Then deserialize
    /// a legacy `session.toml` string that has NO `orchestration` key and
    /// assert the pane still parses with `orchestration == None`, proving the
    /// `#[serde(default)]` forward-compat guarantee for old snapshots.
    #[spec("config/saved-session/001")]
    #[test]
    fn saved_session_001_orchestration_serde_round_trip_and_legacy_parse() {
        // (a) Round-trip a pane carrying an OrchestrationSnapshot.
        let session = SavedSession {
            panes: vec![SavedPane {
                dir: "/repo/app".to_string(),
                name: "orchestrator".to_string(),
                command: "claude".to_string(),
                mode: None,
                orchestration: Some(OrchestrationSnapshot {
                    version: 1,
                    roles: vec![
                        "orchestrator".to_string(),
                        "coder".to_string(),
                        "reviewer".to_string(),
                    ],
                    start_role_index: 0,
                    orchestrator_prompt: "Build the feature end to end".to_string(),
                    config_name: "tdd-cycle".to_string(),
                    project_path: "/repo/app".to_string(),
                    started_role_indices: vec![0, 1],
                }),
            }],
        };

        let toml_str = toml::to_string_pretty(&session).unwrap();
        let loaded: SavedSession = toml::from_str(&toml_str).unwrap();

        assert_eq!(loaded.panes.len(), 1);
        let pane = &loaded.panes[0];
        assert_eq!(pane.dir, "/repo/app");
        assert_eq!(pane.name, "orchestrator");
        assert_eq!(pane.command, "claude");
        assert_eq!(pane.mode, None);

        let orch = pane
            .orchestration
            .as_ref()
            .expect("orchestration must round-trip as Some");
        assert_eq!(orch.version, 1);
        assert_eq!(orch.roles, vec!["orchestrator", "coder", "reviewer"]);
        assert_eq!(orch.start_role_index, 0);
        assert_eq!(orch.orchestrator_prompt, "Build the feature end to end");
        assert_eq!(orch.config_name, "tdd-cycle");
        assert_eq!(orch.project_path, "/repo/app");
        assert_eq!(orch.started_role_indices, vec![0, 1]);

        // (b) A legacy session.toml predating the orchestration field still
        // parses, with orchestration == None (the #[serde(default)] guarantee).
        let legacy = r#"
[[panes]]
dir = "/repo/legacy"
name = "old-pane"
command = "vim"
"#;
        let legacy_loaded: SavedSession = toml::from_str(legacy).unwrap();
        assert_eq!(legacy_loaded.panes.len(), 1);
        assert_eq!(legacy_loaded.panes[0].dir, "/repo/legacy");
        assert!(
            legacy_loaded.panes[0].orchestration.is_none(),
            "a legacy snapshot with no orchestration key must parse with orchestration == None"
        );
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
                orchestration: None,
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
    fn attach_socket_fallback_is_per_user() {
        // PRD #93 round-2 reviewer REV-2: when XDG_RUNTIME_DIR is unset
        // *and* DOT_AGENT_DECK_ATTACH_SOCKET is unset, the fallback under
        // /tmp must include the uid so two users on the same host don't
        // collide. The old `/tmp/dot-agent-deck-attach.sock` would
        // sandwich two daemons onto one path and let the first binder
        // arbitrarily lock the rest of the host out.
        let _g = STATE_DIR_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev_attach = std::env::var("DOT_AGENT_DECK_ATTACH_SOCKET").ok();
        let prev_sock = std::env::var("DOT_AGENT_DECK_SOCKET").ok();
        let prev_xdg = std::env::var("XDG_RUNTIME_DIR").ok();
        // SAFETY: state-dir lock held, restored on the way out.
        unsafe {
            std::env::remove_var("DOT_AGENT_DECK_ATTACH_SOCKET");
            std::env::remove_var("DOT_AGENT_DECK_SOCKET");
            std::env::remove_var("XDG_RUNTIME_DIR");
        }

        // SAFETY: getuid is async-signal-safe and infallible.
        let uid = unsafe { libc::getuid() };
        let attach = attach_socket_path();
        let hook = socket_path();
        let attach_str = attach.to_string_lossy();
        let hook_str = hook.to_string_lossy();
        assert!(
            attach_str.contains(&format!("-{uid}.sock")),
            "attach fallback must embed uid: got {attach_str}"
        );
        assert!(
            hook_str.contains(&format!("-{uid}.sock")),
            "hook fallback must embed uid: got {hook_str}"
        );
        assert!(
            attach_str.starts_with("/tmp/"),
            "attach fallback should live under /tmp when XDG is unset: got {attach_str}"
        );

        // SAFETY: same lock; restoring previous values.
        unsafe {
            match prev_attach {
                Some(v) => std::env::set_var("DOT_AGENT_DECK_ATTACH_SOCKET", v),
                None => std::env::remove_var("DOT_AGENT_DECK_ATTACH_SOCKET"),
            }
            match prev_sock {
                Some(v) => std::env::set_var("DOT_AGENT_DECK_SOCKET", v),
                None => std::env::remove_var("DOT_AGENT_DECK_SOCKET"),
            }
            match prev_xdg {
                Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
                None => std::env::remove_var("XDG_RUNTIME_DIR"),
            }
        }
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

    // scheduler/config/001 — one valid + one malformed `[[scheduled_tasks]]`:
    // the valid entry loads, the malformed one is reported as an error, and
    // there is no panic.
    #[test]
    fn schedules_load_one_valid_one_malformed() {
        let toml_str = r#"
[[scheduled_tasks]]
name = "good"
cron = "0 9 * * *"
working_dir = "/tmp/good"
command = "claude"
prompt = "do the thing"

[[scheduled_tasks]]
name = "bad"
# `cron` is required but missing, and prompt is missing too → entry fails
working_dir = "/tmp/bad"
"#;
        let loaded = LoadedSchedules::parse(toml_str);
        assert_eq!(loaded.tasks.len(), 1, "valid entry still loads");
        assert_eq!(loaded.tasks[0].name, "good");
        assert_eq!(loaded.errors.len(), 1, "malformed entry reported");
        assert_eq!(loaded.errors[0].entry, Some(1));
    }

    // PRD #127 follow-up — a hand-edited entry with no `command` is REJECTED on
    // load (no silent $SHELL fallback): it is reported as an error and skipped,
    // while a sibling entry that DOES carry a command still loads. Mirrors the
    // malformed-entry handling so the daemon never crashes on a bad entry.
    #[test]
    fn schedules_reject_command_less_entry_keep_valid() {
        let toml_str = r#"
[[scheduled_tasks]]
name = "no-cmd"
cron = "0 9 * * *"
working_dir = "/tmp/no-cmd"
prompt = "do the thing"

[[scheduled_tasks]]
name = "has-cmd"
cron = "0 9 * * *"
working_dir = "/tmp/has-cmd"
command = "claude"
prompt = "do the thing"
"#;
        let loaded = LoadedSchedules::parse(toml_str);
        assert_eq!(
            loaded.tasks.len(),
            1,
            "only the command-bearing entry loads"
        );
        assert_eq!(loaded.tasks[0].name, "has-cmd");
        assert_eq!(loaded.errors.len(), 1, "the command-less entry is reported");
        assert_eq!(loaded.errors[0].entry, Some(0));
        assert!(
            loaded.errors[0].message.to_lowercase().contains("command"),
            "error must name the missing command, got: {}",
            loaded.errors[0].message
        );

        // A blank (whitespace-only) command is rejected the same way.
        let blank = r#"
[[scheduled_tasks]]
name = "blank-cmd"
cron = "0 9 * * *"
working_dir = "/tmp/blank-cmd"
command = "   "
prompt = "do the thing"
"#;
        let loaded = LoadedSchedules::parse(blank);
        assert!(loaded.tasks.is_empty(), "a blank command is not a command");
        assert_eq!(loaded.errors.len(), 1);
    }

    #[test]
    fn schedules_missing_file_is_empty_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.toml");
        let loaded = LoadedSchedules::load_from(&path);
        assert!(loaded.tasks.is_empty());
        assert!(loaded.errors.is_empty());
    }

    // scheduler/config/002 — a minimal entry applies the documented defaults
    // (`new_tab_per_fire=false`, `enabled=true`) and `~`/`$VAR` in `working_dir`
    // are expanded at load time. `command` is required (PRD #127 follow-up) so
    // each entry carries one.
    #[test]
    fn schedules_defaults_and_path_expansion() {
        let _guard = STATE_DIR_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev_home = std::env::var("HOME").ok();
        let prev_var = std::env::var("DAD_TEST_DIR").ok();
        // SAFETY: env-var lock held; restored on the way out.
        unsafe {
            std::env::set_var("HOME", "/home/tester");
            std::env::set_var("DAD_TEST_DIR", "projects/digest");
        }

        let toml_str = r#"
[[scheduled_tasks]]
name = "minimal"
cron = "0 9 * * *"
working_dir = "~/scheduled/morning"
command = "claude"
prompt = "hi"

[[scheduled_tasks]]
name = "with-var"
cron = "0 9 * * *"
working_dir = "$DAD_TEST_DIR"
command = "claude"
prompt = "hi"
"#;
        let loaded = LoadedSchedules::parse(toml_str);
        assert!(loaded.errors.is_empty());
        assert_eq!(loaded.tasks.len(), 2);

        let minimal = &loaded.tasks[0];
        assert!(!minimal.new_tab_per_fire, "new_tab_per_fire defaults false");
        assert!(minimal.enabled, "enabled defaults true");
        assert_eq!(minimal.command.as_deref(), Some("claude"));
        assert_eq!(minimal.working_dir, "/home/tester/scheduled/morning");

        // Relative result (from $VAR) resolves against $HOME.
        let with_var = &loaded.tasks[1];
        assert_eq!(with_var.working_dir, "/home/tester/projects/digest");

        // SAFETY: same lock held; restore previous values.
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match prev_var {
                Some(v) => std::env::set_var("DAD_TEST_DIR", v),
                None => std::env::remove_var("DAD_TEST_DIR"),
            }
        }
    }

    #[test]
    fn schedules_round_trip_explicit_fields() {
        let toml_str = r#"
[[scheduled_tasks]]
name = "full"
cron = "0 9 * * MON-FRI"
working_dir = "/abs/path"
command = "claude"
prompt = "multi\nline"
new_tab_per_fire = true
enabled = false
"#;
        let loaded = LoadedSchedules::parse(toml_str);
        assert!(loaded.errors.is_empty());
        let t = &loaded.tasks[0];
        assert_eq!(t.command.as_deref(), Some("claude"));
        assert!(t.new_tab_per_fire);
        assert!(!t.enabled);
        assert_eq!(t.working_dir, "/abs/path");
    }

    #[test]
    fn expand_path_handles_braced_and_lone_dollar() {
        let _guard = STATE_DIR_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev_home = std::env::var("HOME").ok();
        let prev_var = std::env::var("DAD_BRACE").ok();
        // SAFETY: env-var lock held; restored below.
        unsafe {
            std::env::set_var("HOME", "/home/tester");
            std::env::set_var("DAD_BRACE", "braced");
        }

        assert_eq!(expand_path("/a/${DAD_BRACE}/b"), "/a/braced/b");
        assert_eq!(expand_path("~"), "/home/tester");
        // A lone `$` and an undefined var don't panic.
        assert_eq!(expand_path("/lit/$"), "/lit/$");
        assert_eq!(expand_path("/x/$DAD_UNDEFINED/y"), "/x//y");

        // SAFETY: same lock held; restore previous values.
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match prev_var {
                Some(v) => std::env::set_var("DAD_BRACE", v),
                None => std::env::remove_var("DAD_BRACE"),
            }
        }
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
