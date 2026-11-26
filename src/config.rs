use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::state::SessionStatus;

pub const CONFIG_KEYS: &[(&str, &str)] = &[
    ("default_command", "Default shell command for new panes"),
    (
        "default_dir",
        "Absolute directory the desktop's New agent browser opens in (default: home)",
    ),
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
        "notifications.question_after_minutes",
        "Notify once when an agent's question has waited this many minutes; 0 turns it off (default: 10)",
    ),
    (
        "doorbell.claude",
        "Type the doorbell into Claude Code and check its input box (default: true)",
    ),
    (
        "doorbell.codex",
        "Type the doorbell into Codex (default: true)",
    ),
    (
        "doorbell.antigravity",
        "Type the doorbell into Antigravity (default: true)",
    ),
    (
        "host_metrics.disk_free_warn_percent",
        "Host disk shows a warning below this % free (default: 15)",
    ),
    (
        "host_metrics.disk_free_critical_percent",
        "Host disk shows critical below this % free (default: 5)",
    ),
    (
        "host_metrics.memory_available_warn_percent",
        "Host memory shows a warning below this % available (default: 20)",
    ),
    (
        "host_metrics.memory_available_critical_percent",
        "Host memory shows critical below this % available (default: 10)",
    ),
    (
        "host_metrics.load_per_core_warn",
        "Host load shows a warning above this load per core (default: 0.8)",
    ),
    (
        "host_metrics.load_per_core_critical",
        "Host load shows critical above this load per core (default: 1.5)",
    ),
];

pub fn config_keys_help() -> String {
    let mut help = String::from("Available keys:\n");
    for (key, desc) in CONFIG_KEYS {
        help.push_str(&format!("  {key:<30} {desc}\n"));
    }
    help
}

/// Hook-ingestion endpoint path. Delegates to [`crate::platform::paths`] (PRD
/// #42 M1): Unix resolves `$XDG_RUNTIME_DIR/dot-agent-deck.sock`, else
/// `<temp dir>/dot-agent-deck-{uid}/hook.sock` — the sibling of the attach
/// endpoint's `attach.sock` in that same per-uid directory. Windows resolves
/// the named-pipe name. `DOT_AGENT_DECK_SOCKET` overrides.
///
/// "per-uid-`/tmp` socket path" is what this used to say, and it is now wrong
/// twice over: the endpoints moved one level down into a directory the deck
/// owns (issue #1121), and the root is `std::env::temp_dir()` rather than a
/// literal `/tmp`, so it is not `/tmp` at all wherever `$TMPDIR` points
/// elsewhere — which on macOS is everywhere.
pub fn socket_path() -> PathBuf {
    crate::platform::paths::socket_path()
}

/// Path of the M1.2 streaming-attach endpoint. Separate from the existing
/// hook-ingestion socket (PRD #76 line 219) so the two protocols have
/// disjoint, clearly-typed wire formats: hook ingestion is line-delimited
/// JSON, attach is a binary frame protocol (see `daemon_protocol`). Delegates
/// to [`crate::platform::paths`]; `DOT_AGENT_DECK_ATTACH_SOCKET` overrides.
pub fn attach_socket_path() -> PathBuf {
    crate::platform::paths::attach_socket_path()
}

/// Per-user state directory. Used by lazy-spawn (PRD #76 M4.3) for the
/// detached daemon log and the spawn mutex (`spawn.lock`). Delegates to
/// [`crate::platform::paths`] (PRD #42 M1); `DOT_AGENT_DECK_STATE_DIR`
/// overrides, then `$XDG_STATE_HOME`/`$HOME` on Unix or `%LOCALAPPDATA%` on
/// Windows.
pub fn state_dir() -> PathBuf {
    crate::platform::paths::state_dir()
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
            // Issue #714: a quota-blocked agent needs a person just like an
            // erroring one, so it rings under the same switch.
            SessionStatus::Error | SessionStatus::Blocked => self.on_error,
            // Issue #1757: Waiting's work is not done, so it never rings, even
            // under `on_idle`; the bell rings on its later step to Idle.
            SessionStatus::Waiting => false,
            _ => false,
        }
    }
}

/// Issue #2075: the `[notifications]` table. `question_after_minutes`: once an
/// agent's question has waited this long unanswered, the TUI sends one
/// terminal notification for it; `0` turns that off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationsConfig {
    pub question_after_minutes: u64,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            question_after_minutes: 10,
        }
    }
}

impl NotificationsConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// How long a question waits before its notification, or `None` when
    /// notifications are off. A value too large for a duration (only a
    /// hand-edited file can hold one; `config set` refuses it) reads as off
    /// rather than panicking (PR #2208 review).
    pub fn question_after(&self) -> Option<chrono::Duration> {
        let minutes = i64::try_from(self.question_after_minutes).ok()?;
        if minutes <= 0 {
            return None;
        }
        chrono::Duration::try_minutes(minutes)
    }
}

/// PRD #1666 part 3: the `[doorbell]` table, one switch per agent the deck
/// types its doorbell line into. `false` turns the doorbell off for that
/// agent: the deck types nothing into it and installs no input-box check, and
/// its messages arrive by its hooks, native wake or `inbox fetch`. The daemon
/// reads it when it starts an agent ([`crate::doorbell_switch`]), where
/// `DOT_AGENT_DECK_DOORBELL_CLAUDE` / `DOT_AGENT_DECK_DOORBELL_CODEX` /
/// `DOT_AGENT_DECK_DOORBELL_ANTIGRAVITY` win over it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DoorbellConfig {
    pub claude: bool,
    pub codex: bool,
    pub antigravity: bool,
}

impl Default for DoorbellConfig {
    fn default() -> Self {
        Self {
            claude: true,
            codex: true,
            antigravity: true,
        }
    }
}

impl DoorbellConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Issue #519: the removed `[idle_art]` section is deliberately NOT declared
/// here as an accepted-and-ignored field. `DashboardConfig` sets no
/// `#[serde(deny_unknown_fields)]`, so serde drops unknown tables silently and
/// a `config.toml` still carrying `[idle_art]` keeps loading unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DashboardConfig {
    pub default_command: String,
    /// PRD #1223: a deck-level setting — the directory that creating an agent
    /// on this deck starts browsing in, typically the one folder most agents
    /// here are started under.
    ///
    /// Stored verbatim; empty means unset. Every reader goes through
    /// [`crate::new_agent_options::usable_default_dir`] — validated (absolute,
    /// the orchestration-cwd predicate), canonicalised and opened, or `None`.
    /// The desktop is TOLD it as
    /// [`crate::new_agent_options::NewAgentOptions::default_dir`], absent when
    /// it fails, so a bad setting never breaks the options query; the TUI,
    /// which runs on this host, reads it from its own copy of this struct for
    /// its directory picker and falls back to its cwd. Not serialised when empty, so a `config set` of an
    /// unrelated key does not write `default_dir = ""` into the file.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub default_dir: String,
    pub bell: BellConfig,
    /// PRD #1666 part 3: the per-agent doorbell switch ([`DoorbellConfig`]).
    /// Not serialised while both are on, so a `config set` of another key
    /// writes no `[doorbell]` table.
    #[serde(skip_serializing_if = "DoorbellConfig::is_default")]
    pub doorbell: DoorbellConfig,
    /// Issue #2075 ([`NotificationsConfig`]). Not serialised at its default, so
    /// a `config set` of another key writes no `[notifications]` table.
    #[serde(skip_serializing_if = "NotificationsConfig::is_default")]
    pub notifications: NotificationsConfig,
    /// PRD #2119: inert. Nothing opens the config-generation dialog on its
    /// own, so nothing reads this; the key stays accepted so existing configs
    /// and `config set auto_config_prompt …` keep working.
    pub auto_config_prompt: bool,
    /// PRD #1258: the `[host_metrics]` table — the thresholds this deck's
    /// daemon judges its host's disk, memory and load by
    /// ([`crate::host_metrics::HealthThresholds`], which documents the keys and
    /// defaults). Kept as written, and validated per value where it is read, so
    /// one bad value falls back to its own default with a warning instead of
    /// failing the whole file, and a `config set` of another key writes the
    /// table back unchanged. A `host_metrics` that is not a table reads as
    /// empty, likewise without failing the file; the daemon's read
    /// ([`DashboardConfig::load_bounded_reporting`]) reports it as a warning.
    #[serde(
        skip_serializing_if = "toml::Table::is_empty",
        deserialize_with = "deserialize_host_metrics_table"
    )]
    pub host_metrics: toml::Table,
}

/// Every top-level key [`DashboardConfig`] reads and writes. A save replaces
/// these and nothing else, so a table this struct does not own — the agent
/// cards' `[catalog]` (PRD #2374, `src/card_catalog.rs`), or one a newer build
/// added — survives `config set`, comments and all.
/// `dashboard_keys_name_every_field` fails to compile when a field is added
/// without being listed here.
const DASHBOARD_KEYS: &[&str] = &[
    "default_command",
    "default_dir",
    "bell",
    "doorbell",
    "notifications",
    "auto_config_prompt",
    "host_metrics",
];

/// The file [`DashboardConfig::save_to`] writes: `existing` (the file as it is
/// on disk, `None` when there is none) with each of [`DASHBOARD_KEYS`] set to
/// what `ours` (the struct's own serialization) says, or removed when `ours`
/// leaves it out. Everything else in `existing` is kept as written. `Err` when
/// `existing` is not valid TOML: there is nothing to merge into, and writing
/// `ours` alone would delete whatever the file held.
fn merge_dashboard_config(existing: Option<&str>, ours: &str) -> Result<String, String> {
    let Some(existing) = existing else {
        return Ok(ours.to_string());
    };
    let mut doc = existing
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| e.message().to_string())?;
    let mut ours = ours
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| format!("the deck's own serialization did not parse: {e}"))?;
    for key in DASHBOARD_KEYS {
        let Some(mut item) = ours.remove(key) else {
            doc.remove(key);
            continue;
        };
        clear_table_positions(&mut item);
        if let Some(slot) = doc.get_mut(key) {
            // Replaced in place, so the key keeps its own comments and place,
            // and a table its header's comments and its position in the file.
            if let (Some(old), Some(new)) = (slot.as_table(), item.as_table_mut()) {
                *new.decor_mut() = old.decor().clone();
                new.set_position(old.position());
            } else if let (Some(old), Some(new)) = (slot.as_value(), item.as_value_mut()) {
                *new.decor_mut() = old.decor().clone();
            }
            *slot = item;
        } else {
            doc.insert(key, item);
        }
    }
    Ok(doc.to_string())
}

/// Forget the positions `item`'s tables had in the document they were parsed
/// from, so each is written after the table before it in the file it is
/// moved into rather than at a position that means something else there.
fn clear_table_positions(item: &mut toml_edit::Item) {
    match item {
        toml_edit::Item::Table(table) => {
            table.set_position(None);
            for (_, child) in table.iter_mut() {
                clear_table_positions(child);
            }
        }
        toml_edit::Item::ArrayOfTables(tables) => {
            for table in tables.iter_mut() {
                table.set_position(None);
                for (_, child) in table.iter_mut() {
                    clear_table_positions(child);
                }
            }
        }
        _ => {}
    }
}

/// [`DashboardConfig::host_metrics`]'s lenient read: a table is kept as it is,
/// anything else reads as empty, so it never fails the whole config. Silent:
/// the warning is [`host_metrics_not_a_table`], raised by
/// [`DashboardConfig::load_bounded_reporting`], which hands it to its caller
/// rather than logging it on every read.
fn deserialize_host_metrics_table<'de, D>(deserializer: D) -> Result<toml::Table, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match toml::Value::deserialize(deserializer)? {
        toml::Value::Table(table) => table,
        _ => toml::Table::new(),
    })
}

/// PRD #1666 part 3: what [`DashboardConfig::load_bounded_status`] found at
/// the config path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFile {
    /// Opening the path found nothing, and an `lstat` right after it found
    /// nothing either, not even a symlink: [`BoundedLoad::config`] is the
    /// defaults, as intended.
    Absent,
    /// The file was read, and [`BoundedLoad::config`] is what it says.
    Read,
    /// There is a file, but it was not read (not a regular file, over
    /// [`MAX_DASHBOARD_CONFIG_BYTES`], unreadable, a link whose target is
    /// missing, or something appeared at the path while it was being read), is
    /// not valid TOML, or a setting has the wrong type; or the open found
    /// nothing and the `lstat` after it failed for another reason than
    /// `NotFound`: [`BoundedLoad::config`] is the defaults, which may not be
    /// what the user wrote.
    Failed,
}

/// [`DashboardConfig::load_bounded_status`]'s result.
#[derive(Debug, Clone)]
pub struct BoundedLoad {
    pub config: DashboardConfig,
    pub warnings: Vec<String>,
    pub file: ConfigFile,
}

/// The warning for a `host_metrics` in `config.toml` that is not a table.
fn host_metrics_not_a_table(value: &toml::Value) -> String {
    format!(
        "host_metrics in config.toml is a {}, not a table; using the default thresholds",
        value.type_str()
    )
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            default_command: String::new(),
            default_dir: String::new(),
            bell: BellConfig::default(),
            doorbell: DoorbellConfig::default(),
            notifications: NotificationsConfig::default(),
            auto_config_prompt: true,
            host_metrics: toml::Table::new(),
        }
    }
}

impl DashboardConfig {
    pub fn load() -> Self {
        let (config, warning) = Self::load_reporting_from(&config_path());
        if let Some(warning) = warning {
            eprintln!("{warning}");
        }
        config
    }

    /// [`Self::load`] from `path`, returning the line it prints on stderr
    /// instead of printing it: a file that does not parse, or one that cannot
    /// be read. Either way the config is [`Self::default`].
    ///
    /// Issue #2156 item 2: a `config.toml`, or a folder above it, that is a
    /// link whose target is missing is a file that cannot be read, not an
    /// absent one, decided as [`read_dashboard_config_bounded`] decides it.
    /// So `config get` says so on stderr where it used to print the default
    /// silently while the daemon held the doorbell off.
    fn load_reporting_from(path: &Path) -> (Self, Option<String>) {
        let unreadable = |reason: &dyn std::fmt::Display| {
            Some(format!(
                "Failed to read config at {}: {reason}",
                path.display()
            ))
        };
        match std::fs::read_to_string(path) {
            Ok(contents) => match toml::from_str(&contents) {
                Ok(config) => (config, None),
                Err(err) => (
                    Self::default(),
                    Some(format!("Invalid config at {}: {err}", path.display())),
                ),
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                match config_not_found_verdict(path) {
                    Ok(()) => (Self::default(), None),
                    Err(reason) => (Self::default(), unreadable(&reason)),
                }
            }
            Err(err) => (Self::default(), unreadable(&err)),
        }
    }

    /// PRD #1223 audit A4: [`Self::load`] for the daemon, which reads this file
    /// to answer a peer's `NewAgentOptions` query and so must not let the read
    /// block, or allocate, without bound.
    ///
    /// The same file ([`config_path`], `DOT_AGENT_DECK_CONFIG` included) and the
    /// same fallback — anything that is not a readable, parseable config yields
    /// [`Self::default`] — with three differences, all confined to what a
    /// pathological file does:
    ///
    /// * **The open cannot hang.** `O_NONBLOCK` on Unix, because a plain
    ///   `open(2)` of a FIFO with no writer blocks inside the open — the reason
    ///   [`crate::project_resolve::read_config_file`] sets it. A symlink IS
    ///   followed, unlike that reader: a dotfiles manager linking `config.toml`
    ///   into place is ordinary, and [`Self::load`] has always followed one.
    /// * **Only a regular file is read**, checked on the open handle, so a FIFO,
    ///   socket or device at the path (or behind a link) is refused unread.
    /// * **At most [`MAX_DASHBOARD_CONFIG_BYTES`]**, checked against the recorded
    ///   length and again by [`crate::bounded_read::read_capped`], which catches
    ///   a file growing between the two.
    ///
    /// A refusal is logged without the TOML error's `Display`, which embeds a
    /// snippet of the file (the `load_features_file` precedent).
    pub fn load_bounded() -> Self {
        Self::load_bounded_from(&config_path())
    }

    fn load_bounded_from(path: &Path) -> Self {
        let (config, warnings) = Self::load_bounded_reporting_from(path);
        for warning in warnings {
            tracing::warn!("{warning}");
        }
        config
    }

    /// [`Self::load_bounded`], returning its warnings instead of logging them,
    /// for a caller that reads the file repeatedly and logs each distinct
    /// warning once (PRD #1258: the host sampler reads its thresholds on every
    /// sample). The warnings are a file the reader could not read, a file that
    /// is not valid TOML or does not fit the config's shape — each of which
    /// yields [`Self::default`] — and a `host_metrics` that is not a table,
    /// which reads as empty while the rest of the file is kept.
    pub fn load_bounded_reporting() -> (Self, Vec<String>) {
        Self::load_bounded_reporting_from(&config_path())
    }

    fn load_bounded_reporting_from(path: &Path) -> (Self, Vec<String>) {
        let load = Self::load_bounded_status_from(path);
        (load.config, load.warnings)
    }

    /// [`Self::load_bounded`], also saying whether there was a file and
    /// whether it was read, for a caller that must not treat a broken file as
    /// an absent one (PRD #1666 part 3: the doorbell switch, which a broken
    /// file holds off rather than turning back on — see
    /// [`crate::doorbell_switch::at_spawn`]). Logs its warnings, as
    /// [`Self::load_bounded`] does.
    pub fn load_bounded_status() -> BoundedLoad {
        let load = Self::load_bounded_status_from(&config_path());
        for warning in &load.warnings {
            tracing::warn!("{warning}");
        }
        load
    }

    pub(crate) fn load_bounded_status_from(path: &Path) -> BoundedLoad {
        let failed = |warning: String| BoundedLoad {
            config: Self::default(),
            warnings: vec![warning],
            file: ConfigFile::Failed,
        };
        let contents = match read_dashboard_config_bounded(path) {
            Ok(None) => {
                return BoundedLoad {
                    config: Self::default(),
                    warnings: Vec::new(),
                    file: ConfigFile::Absent,
                };
            }
            Ok(Some(contents)) => contents,
            Err(reason) => {
                return failed(format!(
                    "config at {} not read ({reason}); using the defaults",
                    path.display()
                ));
            }
        };
        let Ok(mut table) = contents.parse::<toml::Table>() else {
            return failed(format!(
                "invalid config at {}: malformed TOML; using the defaults",
                path.display()
            ));
        };
        let mut warnings = Vec::new();
        if let Some(value) = table.get("host_metrics").filter(|v| !v.is_table()) {
            warnings.push(host_metrics_not_a_table(value));
            table.remove("host_metrics");
        }
        match toml::Value::Table(table).try_into() {
            Ok(config) => BoundedLoad {
                config,
                warnings,
                file: ConfigFile::Read,
            },
            // The error's `Display` is not used: it can quote the file.
            Err(_) => failed(format!(
                "invalid config at {}: a setting has the wrong type; using the defaults",
                path.display()
            )),
        }
    }

    pub fn save(&self) -> Result<(), String> {
        self.save_to(&config_path())
    }

    /// Save to `path`: read the file as it is now, merge this config's own
    /// keys into it ([`merge_dashboard_config`]) and replace it
    /// ([`write_config_atomically`]), all under the lock the card-action
    /// editor takes too ([`lock_config_edit`], issue #2399 re-audit 2), so a
    /// catalog edit in a daemon and this save never interleave. Nothing is
    /// saved, with the reason, when the other writer holds the lock for
    /// [`CONFIG_LOCK_WAIT`] or the lock file there is unsafe
    /// ([`ConfigLockProblem::Unsafe`]). Where there is no lock to take
    /// ([`ConfigLockProblem::Unavailable`]: no lock file and none can be
    /// made, or a file system that cannot lock) it keeps its existing writer
    /// and saves unlocked, as it did before the lock.
    pub(crate) fn save_to(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create config directory: {e}"))?;
        }
        let _locked = match lock_config_edit(path, CONFIG_LOCK_WAIT) {
            Ok(file) => Some(file),
            Err(ConfigLockProblem::Busy(why) | ConfigLockProblem::Unsafe(why)) => {
                return Err(format!("Not saving config at {}: {why}", path.display()));
            }
            Err(ConfigLockProblem::Unavailable(_)) => None,
        };
        let ours =
            toml::to_string_pretty(self).map_err(|e| format!("Failed to serialize config: {e}"))?;
        let existing = match std::fs::read_to_string(path) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(format!(
                    "Failed to read config at {} before saving it: {e}",
                    path.display()
                ));
            }
        };
        let contents = merge_dashboard_config(existing.as_deref(), &ours).map_err(|e| {
            format!(
                "Not saving config at {}: it is not valid TOML ({e}); fix it first, \
                 because saving would replace everything it holds",
                path.display()
            )
        })?;
        write_config_atomically(path, contents.as_bytes())
            .map_err(|e| format!("Failed to write config at {}: {e}", path.display()))
    }

    pub fn get_field(&self, key: &str) -> Result<String, String> {
        match key {
            "default_command" => Ok(self.default_command.clone()),
            "default_dir" => Ok(self.default_dir.clone()),
            "bell.enabled" => Ok(self.bell.enabled.to_string()),
            "bell.on_waiting_for_input" => Ok(self.bell.on_waiting_for_input.to_string()),
            "bell.on_idle" => Ok(self.bell.on_idle.to_string()),
            "bell.on_error" => Ok(self.bell.on_error.to_string()),
            "doorbell.claude" => Ok(self.doorbell.claude.to_string()),
            "doorbell.codex" => Ok(self.doorbell.codex.to_string()),
            "doorbell.antigravity" => Ok(self.doorbell.antigravity.to_string()),
            "notifications.question_after_minutes" => {
                Ok(self.notifications.question_after_minutes.to_string())
            }
            "auto_config_prompt" => Ok(self.auto_config_prompt.to_string()),
            _ => match host_metrics_key(key) {
                // The value in effect, so a reader sees the default for an
                // unset or invalid key rather than nothing.
                Some(name) => Ok(crate::host_metrics::HealthThresholds::from_table(
                    &self.host_metrics,
                )
                .0
                .value(name)
                .to_string()),
                None => Err(format!("Unknown config key: {key}\n{}", config_keys_help())),
            },
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
            "default_dir" => {
                // Refused here as well as at read time, so a typo is caught
                // by the command that made it rather than by a browser that
                // quietly opens in home. Empty unsets it.
                if !value.is_empty() && !crate::agent_pty::is_valid_orchestration_cwd(value) {
                    return Err(format!(
                        "Invalid default_dir: {value:?} is not an absolute path \
                         (empty unsets it)"
                    ));
                }
                self.default_dir = value.to_string();
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
            "doorbell.claude" => {
                self.doorbell.claude = parse_bool(value)?;
                Ok(())
            }
            "doorbell.codex" => {
                self.doorbell.codex = parse_bool(value)?;
                Ok(())
            }
            "doorbell.antigravity" => {
                self.doorbell.antigravity = parse_bool(value)?;
                Ok(())
            }
            "notifications.question_after_minutes" => {
                let invalid = || {
                    format!("Invalid {key}: {value:?} (a whole number of minutes; 0 turns it off)")
                };
                let minutes: u64 = value.trim().parse().map_err(|_| invalid())?;
                let in_range = i64::try_from(minutes)
                    .ok()
                    .is_some_and(|m| chrono::Duration::try_minutes(m).is_some());
                if !in_range {
                    return Err(invalid());
                }
                self.notifications.question_after_minutes = minutes;
                Ok(())
            }
            "auto_config_prompt" => {
                self.auto_config_prompt = value
                    .parse()
                    .map_err(|_| "Expected 'true' or 'false'".to_string())?;
                Ok(())
            }
            _ => match host_metrics_key(key) {
                Some(name) => {
                    // Refused here as well as at read time, so a typo is caught
                    // by the command that made it.
                    let number = value
                        .trim()
                        .parse::<f64>()
                        .ok()
                        .filter(|n| crate::host_metrics::HealthThresholds::in_range(name, *n))
                        .ok_or_else(|| format!("Invalid {key}: {value:?}"))?;
                    // A whole number stays an integer in the file, as a person
                    // would write it.
                    let stored = if number.fract() == 0.0 && number <= i64::MAX as f64 {
                        toml::Value::Integer(number as i64)
                    } else {
                        toml::Value::Float(number)
                    };
                    // Refused, too, when it would invert its pair: the deck
                    // would ignore it, and both of the pair, until fixed.
                    let mut candidate = self.host_metrics.clone();
                    candidate.insert(name.to_string(), stored.clone());
                    let (effective, _) =
                        crate::host_metrics::HealthThresholds::from_table(&candidate);
                    if effective.value(name) != number {
                        let other =
                            crate::host_metrics::HealthThresholds::paired_key(name).unwrap_or(name);
                        let other_value = threshold_as_read(&candidate, other);
                        return Err(format!(
                            "Invalid {key}: {value} with host_metrics.{other} = {other_value} \
                             would make the critical limit less strict than the warning, and \
                             the deck would use the defaults for both. Change \
                             host_metrics.{other} first."
                        ));
                    }
                    self.host_metrics.insert(name.to_string(), stored);
                    Ok(())
                }
                None => Err(format!("Unknown config key: {key}\n{}", config_keys_help())),
            },
        }
    }
}

/// The threshold `name` as [`crate::host_metrics::HealthThresholds::from_table`]
/// reads it from `table` before checking its pair: the written number when it
/// is one in range, else its default.
fn threshold_as_read(table: &toml::Table, name: &str) -> f64 {
    use crate::host_metrics::HealthThresholds;
    table
        .get(name)
        .and_then(|value| match value {
            toml::Value::Integer(n) => Some(*n as f64),
            toml::Value::Float(f) => Some(*f),
            _ => None,
        })
        .filter(|n| HealthThresholds::in_range(name, *n))
        .unwrap_or_else(|| HealthThresholds::default().value(name))
}

/// The threshold name in a `host_metrics.<name>` config key, when it is one.
fn host_metrics_key(key: &str) -> Option<&str> {
    key.strip_prefix("host_metrics.")
        .filter(|name| crate::host_metrics::HEALTH_THRESHOLD_KEYS.contains(name))
}

/// PRD #1223 audit A4: the most [`DashboardConfig::load_bounded`] reads.
///
/// **1 MiB**, the project-config reader's cap
/// ([`crate::project_resolve::MAX_PROJECT_CONFIG_BYTES`]) — the other file the
/// daemon reads on a peer's behalf — so the two bounds stay one number. A
/// `config.toml` is a few hundred bytes; anything near this is not a config.
pub const MAX_DASHBOARD_CONFIG_BYTES: u64 = crate::project_resolve::MAX_PROJECT_CONFIG_BYTES;

/// [`DashboardConfig::load_bounded`]'s read: `Ok(None)` when there is no file,
/// `Err` with a short reason (no file content) when there is one that is not a
/// regular file within [`MAX_DASHBOARD_CONFIG_BYTES`] or cannot be read.
///
/// Issue #2137 N2: a symlink at `path` whose target is missing (moved, or on a
/// volume that is not mounted) is a file that cannot be read, not an absent
/// one: the target may be the config that turned a doorbell off. `Ok(None)`
/// means the open failed with `NotFound` and a following `lstat` of `path`
/// failed with `NotFound` too; when the `lstat` finds anything (a link, or an
/// entry that appeared after the open) or fails another way, the read fails.
///
/// Issue #2156: the same holds for a folder above `config.toml` — typically
/// `~/.config/dot-agent-deck` itself — that is a link whose target is missing.
/// The open and the `lstat` of `path` both say `NotFound` then, because the
/// missing folder is on the path, so [`missing_config_folder_verdict`] looks at
/// the nearest folder above it that is there: a real folder (or a link to one)
/// is a first run whose config folder does not exist yet, and anything else
/// fails the read.
fn read_dashboard_config_bounded(path: &Path) -> Result<Option<String>, String> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return config_not_found_verdict(path).map(|()| None);
        }
        Err(e) => return Err(format!("open failed: {e}")),
    };
    let metadata = file.metadata().map_err(|e| format!("stat failed: {e}"))?;
    if !metadata.is_file() {
        return Err("not a regular file".into());
    }
    if metadata.len() > MAX_DASHBOARD_CONFIG_BYTES {
        return Err(format!(
            "exceeds the {MAX_DASHBOARD_CONFIG_BYTES}-byte limit"
        ));
    }
    crate::bounded_read::read_capped(file, MAX_DASHBOARD_CONFIG_BYTES, "config file").map(Some)
}

/// Whether an open or read of the config at `path` that failed with `NotFound`
/// found an absent config (`Ok`), or one that is there but cannot be read
/// (`Err`, with a short reason): [`after_open_not_found`] on the path itself,
/// then [`missing_config_folder_verdict`] on the folders above it. Shared by
/// [`read_dashboard_config_bounded`] and [`DashboardConfig::load`], so the
/// daemon and `config get` tell a first run from a dangling link the same way
/// (issue #2156).
pub(crate) fn config_not_found_verdict(path: &Path) -> Result<(), String> {
    after_open_not_found(std::fs::symlink_metadata(path).map(|m| m.file_type()))
        .and_then(|_| missing_config_folder_verdict(path))
        .map(|_| ())
}

/// [`read_dashboard_config_bounded`]'s verdict once the open failed with
/// `NotFound`, from what `lstat` (`symlink_metadata`) then said about the path.
/// `Ok(None)` only when `lstat` also says `NotFound`; anything else fails, so a
/// change at the path between the two calls is never read as an absent config.
fn after_open_not_found(
    lstat: std::io::Result<std::fs::FileType>,
) -> Result<Option<String>, String> {
    match lstat {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Ok(kind) if kind.is_symlink() => Err("it is a link whose target is missing".into()),
        Ok(_) => Err("it changed while it was being read".into()),
        Err(e) => Err(format!("stat after a failed open failed: {e}")),
    }
}

/// Issue #2156: [`read_dashboard_config_bounded`]'s verdict once neither the
/// open nor the `lstat` of `path` found anything, from the nearest folder
/// above `path` that `lstat` finds. Every entry between it and `path` is
/// missing, so it decides whether the config's folder simply does not exist
/// yet (`Ok(None)`, a first run) or sits behind a link whose target is
/// missing — moved, or on a volume that is not mounted — and may hold the
/// config that turned a doorbell off (`Err`). A relative `path` whose every
/// folder is missing is absent: the walk stops at the working directory.
fn missing_config_folder_verdict(path: &Path) -> Result<Option<String>, String> {
    for folder in path.ancestors().skip(1) {
        if folder.as_os_str().is_empty() {
            break;
        }
        match std::fs::symlink_metadata(folder) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            lstat => {
                return nearest_config_folder_verdict(folder, lstat.map(|m| m.file_type()), || {
                    std::fs::metadata(folder).map(|m| m.file_type())
                });
            }
        }
    }
    Ok(None)
}

/// [`missing_config_folder_verdict`]'s verdict on `folder`, the nearest entry
/// above the config path that is there, from its `lstat` and, for a link, the
/// `stat` that follows it. `Ok(None)` only for a folder, or a link that leads
/// to one; a link whose target is missing, anything that is not a folder, or a
/// call that fails another way fails the read.
fn nearest_config_folder_verdict(
    folder: &Path,
    lstat: std::io::Result<std::fs::FileType>,
    stat: impl FnOnce() -> std::io::Result<std::fs::FileType>,
) -> Result<Option<String>, String> {
    match lstat {
        Ok(kind) if kind.is_dir() => Ok(None),
        Ok(kind) if kind.is_symlink() => match stat() {
            Ok(target) if target.is_dir() => Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(format!(
                "the folder {} is a link whose target is missing",
                folder.display()
            )),
            Ok(_) => Err(format!(
                "the folder {} is a link to something that is not a folder",
                folder.display()
            )),
            Err(e) => Err(format!(
                "stat of the folder {} failed: {e}",
                folder.display()
            )),
        },
        Ok(_) => Err(format!("{} is not a folder", folder.display())),
        Err(e) => Err(format!(
            "stat of the folder {} failed: {e}",
            folder.display()
        )),
    }
}

/// Issue #2137 N1: publish `config.toml` through a temp file and a rename, so
/// a reader — the daemon checking the doorbell switch as it spawns an agent —
/// sees the old file or the new one and never the empty or partial file the
/// truncate-then-write of `std::fs::write` exposed.
///
/// Kept to what `std::fs::write` did in every other respect:
///
/// * **A symlinked `config.toml` stays a symlink.** `std::fs::write` wrote
///   through the link into its target, so this follows the link chain itself
///   and renames over the final target, leaving the links in place. A dangling
///   link resolves to its missing target, which is created, as before.
/// * **The file keeps its mode.** On unix the temp file is created with the
///   existing target's permission bits (narrowed by the umask), then given the
///   target's exact permissions before any byte is written; a new file gets
///   `0666 & !umask`, which is what `std::fs::write` created.
/// * **A config the user cannot write is still refused**, with the same error.
/// * **The file keeps its owner and its hard links.** On unix, when the swap
///   would give `config.toml` another owner or group (`config set` run as root
///   with the user's `HOME`, a file owned by someone else in the user's
///   folder) or the file has more than one hard link,
///   [`swap_keeps_owner_and_links`] sends the save to the in-place write.
///
/// The temp file is opened with `create_new` (`O_EXCL`), so a pre-planted entry
/// at its name — a symlink included — is never followed or truncated, only
/// skipped for the next name.
///
/// **Where the folder cannot take the swap, the save is the old in-place
/// write.** When [`falls_back_to_in_place_write`] says the temp file or the
/// rename failed for a reason in the environment — a writable `config.toml` in
/// a folder the user cannot write, a single-file bind mount — this removes any
/// temp file it made and does exactly what `std::fs::write` did, without the
/// protection. Any other failure (the write, the sync, a full disk) leaves the
/// old file as it was and is reported.
pub(crate) fn write_config_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_config_atomically_with(path, bytes, |file, bytes| {
        use std::io::Write as _;
        file.write_all(bytes)?;
        file.sync_all()
    })
}

/// Which step of the atomic save failed, for [`falls_back_to_in_place_write`].
#[derive(Clone, Copy, Debug)]
enum AtomicSaveStep {
    CreateTemp,
    Rename,
}

/// Issue #2137: whether an error at `step` means the swap cannot be done in
/// this place — so the save falls back to the in-place write `config set`
/// always did — rather than that the save itself failed.
fn falls_back_to_in_place_write(step: AtomicSaveStep, err: &std::io::Error) -> bool {
    use std::io::ErrorKind as K;
    match step {
        // The folder cannot take a new entry: the user cannot write it
        // (EACCES/EPERM), or it is on a read-only filesystem while the file
        // itself is writable, such as a file bind-mounted into a read-only
        // container (EROFS). The probe already found the file writable.
        AtomicSaveStep::CreateTemp => {
            matches!(err.kind(), K::PermissionDenied | K::ReadOnlyFilesystem)
        }
        // The entry cannot be replaced by a rename: it is a mount point, as a
        // single-file bind mount (docker `-v file:file`, a k8s `subPath`) is
        // (EBUSY); it sits on another mount than its folder (EXDEV); or the
        // folder is sticky and the user does not own the file (EPERM).
        AtomicSaveStep::Rename => matches!(
            err.kind(),
            K::ResourceBusy | K::CrossesDevices | K::PermissionDenied
        ),
    }
}

/// Who owns a file and how many names it has, for
/// [`swap_keeps_owner_and_links`].
#[cfg(unix)]
#[derive(Clone, Copy, Debug)]
struct FileOwnership {
    uid: u32,
    gid: u32,
    nlink: u64,
}

#[cfg(unix)]
impl FileOwnership {
    fn of(meta: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt as _;
        Self {
            uid: meta.uid(),
            gid: meta.gid(),
            nlink: meta.nlink(),
        }
    }
}

/// Issue #2137 (PR #2142 review): whether renaming the temp file
/// (`replacement`) over `target` leaves the config owned as before and keeps
/// its other names. A missing target has neither, so the swap is fine.
///
/// The renamed config takes the temp file's owner and group, so those are
/// compared with the target's directly rather than predicted from
/// `geteuid`/`getegid`: on macOS, and in a setgid folder on Linux, a new file
/// takes its folder's group, not the process's.
///
/// When this is false the save is the in-place write, which keeps the owner
/// and the links: `config set` run as root with the user's `HOME` (`sudo -E`,
/// or macOS `sudo`) would otherwise leave the user a root-owned config they
/// cannot save (and, if it is `0600`, the daemon cannot read), and a
/// hard-linked config would be split from its other names.
#[cfg(unix)]
fn swap_keeps_owner_and_links(target: Option<FileOwnership>, replacement: FileOwnership) -> bool {
    match target {
        None => true,
        Some(t) => t.uid == replacement.uid && t.gid == replacement.gid && t.nlink <= 1,
    }
}

/// [`write_config_atomically`] with the body write and sync supplied, so a test
/// can make them fail.
fn write_config_atomically_with(
    path: &Path,
    bytes: &[u8],
    write_body: impl FnOnce(&mut std::fs::File, &[u8]) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let target = resolve_symlink_chain(path);
    // `std::fs::write` opened the existing file for writing, so a config the
    // user cannot write (a read-only `chmod 444`) was refused. The rename
    // needs only the directory's permission, so ask the file first; the probe
    // neither creates nor truncates.
    match std::fs::OpenOptions::new().write(true).open(&target) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let target_meta = std::fs::metadata(&target).ok();
    let target_perms = target_meta.as_ref().map(|m| m.permissions());

    let (mut file, tmp) = match create_temp_beside(&target, target_perms.as_ref()) {
        Ok(created) => created,
        Err(e) if falls_back_to_in_place_write(AtomicSaveStep::CreateTemp, &e) => {
            return std::fs::write(&target, bytes);
        }
        Err(e) => return Err(e),
    };

    // A swap that would change who owns the config, or split it from its
    // other hard links, is not made: the save is the in-place write instead.
    #[cfg(unix)]
    match file.metadata() {
        Ok(replacement)
            if swap_keeps_owner_and_links(
                target_meta.as_ref().map(FileOwnership::of),
                FileOwnership::of(&replacement),
            ) => {}
        other => {
            drop(file);
            let _ = std::fs::remove_file(&tmp);
            return match other {
                Ok(_) => std::fs::write(&target, bytes),
                Err(e) => Err(e),
            };
        }
    }

    let written = (|| {
        if let Some(perms) = &target_perms {
            file.set_permissions(perms.clone())?;
        }
        write_body(&mut file, bytes)
    })();
    drop(file);
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    match std::fs::rename(&tmp, &target) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            if falls_back_to_in_place_write(AtomicSaveStep::Rename, &e) {
                std::fs::write(&target, bytes)
            } else {
                Err(e)
            }
        }
    }
}

/// Issue #2399 re-audit new 3: what a strict save must find unchanged about
/// the config it replaces — which file it is (device and inode), its
/// permission bits, its owner and group, and how many names it has. The
/// reader takes it from the handle it read through; [`write_config_strictly`]
/// compares it with the handle its write probe opens, and the caller's last
/// check compares it again before the rename. Outside Unix every file has the
/// same stamp, and the bytes are what tell a change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileStamp {
    dev: u64,
    ino: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    nlink: u64,
}

impl FileStamp {
    pub(crate) fn of(meta: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            Self {
                dev: meta.dev(),
                ino: meta.ino(),
                mode: meta.mode(),
                uid: meta.uid(),
                gid: meta.gid(),
                nlink: meta.nlink(),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = meta;
            Self {
                dev: 0,
                ino: 0,
                mode: 0,
                uid: 0,
                gid: 0,
                nlink: 0,
            }
        }
    }
}

/// Why a strict save refuses a config that is no longer what was read: the
/// card-action editor's words, shared by [`write_config_strictly`]'s probe and
/// the editor's last check.
pub(crate) const CONFIG_CHANGED_SINCE_READ: &str = "it changed since the editor read it (another program saved it); nothing was saved: \
     read the catalog again and redo the change";

/// Issue #2399 re-audit 2: the sidecar both cooperating writers of the user
/// config lock — the card-action editor ([`crate::card_catalog`]) and
/// `config set`'s [`DashboardConfig::save_to`] — for their whole read, merge
/// and save, so neither saves between the other's read and save. A fixed name,
/// like the temp file's, so a config whose own name is near the 255-byte limit
/// still has one.
pub(crate) const CONFIG_LOCK_NAME: &str = ".dot-agent-deck-config.lock";

/// How long a writer waits for the other to finish before giving up. The
/// card-action editor counts it from the start of its request, for its
/// in-process turn and the file lock together ([`lock_config_edit_by`]);
/// `config set` counts it from when it asks for the lock. Reading and writing
/// the files has no deadline of its own.
pub(crate) const CONFIG_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(3);
/// How often a waiting writer tries again.
pub(crate) const CONFIG_LOCK_POLL: std::time::Duration = std::time::Duration::from_millis(20);

/// Why [`lock_config_edit`] holds no lock.
#[derive(Debug)]
pub(crate) enum ConfigLockProblem {
    /// Another writer kept it until the deadline.
    Busy(String),
    /// Issue #2399 final review F2 / audit A3: there is a lock file, but not
    /// one this user's writers made for themselves ([`lock_file_unsafe`]):
    /// another user could replace it under a writer, or hold it. Both writers
    /// refuse; it is never `chmod`ed, `chown`ed or replaced.
    Unsafe(String),
    /// There is no lock to take here: no lock file and none can be made, or
    /// the file system cannot lock. `config set` then saves unlocked, as it
    /// did before the lock; the catalog editor refuses.
    Unavailable(String),
}

/// What [`lock_file_unsafe`] judges an open lock file by.
#[cfg(unix)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct LockFileFacts {
    pub(crate) regular: bool,
    pub(crate) uid: u32,
    pub(crate) mode: u32,
    pub(crate) nlink: u64,
}

#[cfg(unix)]
impl LockFileFacts {
    fn of(meta: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt as _;
        Self {
            regular: meta.is_file(),
            uid: meta.uid(),
            mode: meta.mode(),
            nlink: meta.nlink(),
        }
    }
}

/// Why an open lock file is not one the deck's writers for user `me` can
/// trust (final review F2 / audit A3): it must be a regular file, owned by
/// `me`, with no group or other permission bits and one link. Anything else
/// is what another user could have put there to replace under a writer or to
/// hold.
#[cfg(unix)]
pub(crate) fn lock_file_unsafe(facts: LockFileFacts, me: u32) -> Option<&'static str> {
    if !facts.regular {
        Some("it is not a regular file")
    } else if facts.uid != me {
        Some("it belongs to another user")
    } else if facts.mode & 0o077 != 0 {
        Some("other users have permissions on it")
    } else if facts.nlink != 1 {
        Some("it has another hard link")
    } else {
        None
    }
}

/// [`lock_config_edit_by`], waiting up to `wait` from now.
pub(crate) fn lock_config_edit(
    path: &Path,
    wait: std::time::Duration,
) -> Result<std::fs::File, ConfigLockProblem> {
    lock_config_edit_by(path, std::time::Instant::now() + wait)
}

/// Take the exclusive lock on [`CONFIG_LOCK_NAME`] beside the file `path`
/// resolves to (through its links, so two writers reaching one file by
/// different paths take one lock), trying until `deadline`; released when the
/// returned file is dropped. A missing lock file is created owner-only; an
/// existing one is used only when [`lock_file_unsafe`] finds nothing wrong
/// with it (Unix), and is never changed, replaced or deleted. It is opened
/// without following a link or waiting on a FIFO. It is an advisory `flock`:
/// it orders the deck's own writers, not an editor that does not take it,
/// which the writers' compare-before-rename checks are for.
pub(crate) fn lock_config_edit_by(
    path: &Path,
    deadline: std::time::Instant,
) -> Result<std::fs::File, ConfigLockProblem> {
    let target = resolve_symlink_chain(path);
    let dir = match target.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let lock_path = dir.join(CONFIG_LOCK_NAME);
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let unavailable = |why: String| {
        ConfigLockProblem::Unavailable(format!(
            "its lock file {} cannot be used ({why})",
            lock_path.display()
        ))
    };
    let unsafe_lock = |why: String| {
        ConfigLockProblem::Unsafe(format!(
            "its lock file {} is not safe to use ({why})",
            lock_path.display()
        ))
    };
    let file = match options.open(&lock_path) {
        Ok(file) => file,
        // Something is there that cannot be opened as a lock file — a link,
        // or a file this user may not open: not one this user's writers made.
        Err(e) => {
            return Err(match std::fs::symlink_metadata(&lock_path) {
                Ok(meta) if meta.file_type().is_symlink() => unsafe_lock("it is a link".into()),
                Ok(_) => unsafe_lock(e.to_string()),
                Err(_) => unavailable(e.to_string()),
            });
        }
    };
    match file.metadata() {
        #[cfg(unix)]
        Ok(meta) => {
            // SAFETY: `geteuid` takes no arguments and cannot fail.
            let me = unsafe { libc::geteuid() };
            if let Some(why) = lock_file_unsafe(LockFileFacts::of(&meta), me) {
                return Err(unsafe_lock(why.into()));
            }
        }
        #[cfg(not(unix))]
        Ok(meta) if meta.is_file() => {}
        #[cfg(not(unix))]
        Ok(_) => return Err(unsafe_lock("it is not a regular file".into())),
        Err(e) => return Err(unavailable(e.to_string())),
    }
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(CONFIG_LOCK_POLL);
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(ConfigLockProblem::Busy(format!(
                    "another program was saving it ({} stayed locked while this waited); \
                     try again",
                    lock_path.display()
                )));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(unavailable(e.to_string())),
        }
    }
}

/// Issue #2399 re-audit 5: a folder on the config's path that a user other
/// than this one could change — group- or world-writable without the sticky
/// bit, or owned by someone other than this user and root — named with the
/// reason. Such a folder lets another user swap the file between the save's
/// checks and its rename, so a strict save refuses there.
///
/// The path is resolved here, one component at a time from the root (a
/// relative one from the current folder), following each link where it is
/// met — in a folder component or in the final file's own chain — and `..`
/// against the folder reached so far, as the kernel does (final audit A2,
/// round 4). A judged folder is judged with its parents, up to the user's
/// home (as `$HOME` names it, or resolved) when the folder is under it, and
/// up to the root otherwise. Judged: the folder the final file is in, whether
/// or not the file exists and wherever the folder is (round 5, C1); every
/// folder holding a link that is followed, at any depth; and every other
/// folder a name is looked up in, except a folder strictly above the home
/// that the walk passes through on its way down, which is judged only where
/// it holds a followed link or holds the final file. Refused: more than
/// [`FOLDER_CHECK_MAX_LINKS`] links followed (a loop among them), a link that
/// cannot be read, and an entry that cannot be inspected; the final file may
/// be absent, a folder before it may not. Checked by pathname when the save
/// starts; a folder changed afterwards is not caught.
#[cfg(unix)]
fn folder_others_can_change(configured: &Path) -> Result<(), String> {
    use std::collections::{HashSet, VecDeque};
    use std::ffi::OsString;
    use std::os::unix::fs::MetadataExt as _;
    use std::path::Component;

    let home_as_named = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from);
    let home = home_as_named
        .as_deref()
        .and_then(|home| std::fs::canonicalize(home).ok());
    let is_home =
        |dir: &Path| home.as_deref() == Some(dir) || home_as_named.as_deref() == Some(dir);
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    let me = unsafe { libc::geteuid() };
    let mut judged: HashSet<PathBuf> = HashSet::new();
    let mut judge = |start: &Path| -> Result<(), String> {
        for dir in start.ancestors() {
            if !judged.insert(dir.to_path_buf()) {
                break;
            }
            let meta = std::fs::metadata(dir)
                .map_err(|e| format!("the folder {} cannot be inspected ({e})", dir.display()))?;
            let mode = meta.mode();
            let why = if mode & 0o022 != 0 && mode & 0o1000 == 0 {
                Some((
                    "it is group- or world-writable",
                    "make it writable only by you",
                ))
            } else if meta.uid() != me && meta.uid() != 0 {
                Some((
                    "it belongs to another user",
                    "keep the config in folders that are yours",
                ))
            } else {
                None
            };
            if let Some((why, remedy)) = why {
                return Err(format!(
                    "the folder {} can be changed by other users ({why}), so another user could \
                     replace the file while it is saved; {remedy}",
                    dir.display()
                ));
            }
            if is_home(dir) {
                break;
            }
        }
        Ok(())
    };
    // A folder strictly above the home: the walk passes through it on the way
    // to the home, and it is judged there only where it holds a followed link.
    // The folder the final file is in is judged wherever it is (round 5, C1).
    let above_home = |dir: &Path| {
        home.as_deref()
            .is_some_and(|home| home != dir && home.starts_with(dir))
    };

    let absolute = if configured.is_absolute() {
        configured.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(configured))
            .map_err(|e| {
                format!(
                    "the folder of {} cannot be resolved ({e})",
                    configured.display()
                )
            })?
    };
    let mut rest: VecDeque<OsString> = VecDeque::new();
    let push_front = |rest: &mut VecDeque<OsString>, path: &Path| -> bool {
        let mut parts = Vec::new();
        let mut rooted = false;
        for component in path.components() {
            match component {
                Component::RootDir | Component::Prefix(_) => rooted = true,
                Component::CurDir => {}
                Component::ParentDir => parts.push(OsString::from("..")),
                Component::Normal(name) => parts.push(name.to_os_string()),
            }
        }
        for part in parts.into_iter().rev() {
            rest.push_front(part);
        }
        rooted
    };
    push_front(&mut rest, &absolute);
    let mut current = PathBuf::from("/");
    let mut links = 0usize;
    while let Some(name) = rest.pop_front() {
        if name == ".." {
            if let Some(parent) = current.parent() {
                current = parent.to_path_buf();
            }
            continue;
        }
        let last = rest.is_empty();
        let entry = current.join(&name);
        let meta = match std::fs::symlink_metadata(&entry) {
            Ok(meta) => meta,
            Err(e) if last && e.kind() == std::io::ErrorKind::NotFound => {
                judge(&current)?;
                return Ok(());
            }
            Err(e) => {
                return Err(format!(
                    "the folder {} cannot be resolved ({e})",
                    entry.display()
                ));
            }
        };
        if meta.file_type().is_symlink() {
            links += 1;
            if links > FOLDER_CHECK_MAX_LINKS {
                return Err(format!(
                    "{} leads through more than {FOLDER_CHECK_MAX_LINKS} links, so the folders \
                     it passes through cannot be checked",
                    configured.display()
                ));
            }
            let target = std::fs::read_link(&entry)
                .map_err(|e| format!("the link {} cannot be read ({e})", entry.display()))?;
            judge(&current)?;
            if push_front(&mut rest, &target) {
                current = PathBuf::from("/");
            }
            continue;
        }
        if last || !above_home(&current) {
            judge(&current)?;
        }
        if last {
            return Ok(());
        }
        current = entry;
    }
    // The path named a folder (or the root), not a file in one.
    judge(&current)?;
    Ok(())
}

/// The most links [`folder_others_can_change`] follows on one path, as the
/// kernel's own limit for a path lookup.
#[cfg(unix)]
const FOLDER_CHECK_MAX_LINKS: usize = 40;

/// Issue #2399 re-audit new 1: open `target` for writing to learn that it can
/// be written and what it is, without waiting and without following a link:
/// `O_NONBLOCK`, because a FIFO put in its place with no reader would hold the
/// open (and the editor's lock) indefinitely, and `O_NOFOLLOW`, because
/// `target` is already the end of its link chain. The handle must be a regular
/// file; its metadata is answered. `None`: there is no file.
fn probe_for_write(target: &Path) -> Result<Option<std::fs::Metadata>, String> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    match options.open(target) {
        Ok(file) => {
            let meta = file
                .metadata()
                .map_err(|e| format!("it cannot be inspected: {e}"))?;
            if !meta.is_file() {
                return Err("it is not a regular file".to_string());
            }
            Ok(Some(meta))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        #[cfg(unix)]
        Err(e) if matches!(e.raw_os_error(), Some(libc::ENXIO | libc::ELOOP)) => Err(
            "it is not a regular file (a FIFO, a socket or a link is in its place now)".to_string(),
        ),
        Err(e) => Err(format!("it cannot be written: {e}")),
    }
}

/// Issue #2399 (review 1, audit 4): the strict sibling of
/// [`write_config_atomically`], for the card-action editor's writes through the
/// daemon. It publishes `bytes` at `target` only by renaming a temp file over
/// it, and where [`write_config_atomically`] falls back to an in-place write it
/// refuses instead, leaving the file as it was: a folder that will not take the
/// temp file, a swap that would change the file's owner or group or split it
/// from another hard link, a rename the file system will not do. `config set`
/// keeps its fallbacks; only this caller is strict.
///
/// `target` is the file itself, already resolved through any links (the caller
/// resolved it once, when it read the file), so nothing is resolved again here;
/// `configured` is the path it was reached by, for the folder check.
/// `expected` is the file as read ([`FileStamp`]; `None`: there was none).
/// Refused before anything is written: a folder on the path that other users
/// can change ([`folder_others_can_change`], Unix), a file the write probe
/// finds is not a regular file or is not the one read ([`probe_for_write`]).
/// The temp file is created owner-only (`0600` at the exclusive open, Unix)
/// and stays so while it is written; `still_as_read` runs after it is written
/// and synced: an `Err` from it (the caller's file changed since it was read)
/// removes the temp file and refuses. Then the temp file takes the mode the
/// file had when it was read (a config this creates: `0600`; the process
/// umask is not changed) and is renamed over the file. After the rename the folder is synced
/// too, best effort, so that the new name reaches the disk; a failure there is
/// not reported, because the file was replaced.
///
/// Every `Err` means the file at `target` was not replaced.
pub(crate) fn write_config_strictly(
    configured: &Path,
    target: &Path,
    bytes: &[u8],
    expected: Option<FileStamp>,
    still_as_read: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    write_config_strictly_with(
        configured,
        target,
        bytes,
        expected,
        |file, bytes| {
            use std::io::Write as _;
            file.write_all(bytes)?;
            file.sync_all()
        },
        still_as_read,
    )
}

/// A test's look at a staging file ([`STAGED`]).
#[cfg(test)]
type StagedHook = Box<dyn FnOnce(&std::fs::File)>;

#[cfg(test)]
thread_local! {
    /// A test's look at [`write_config_strictly`]'s staging file the moment
    /// it is created, before anything else is done to it.
    static STAGED: std::cell::RefCell<Option<StagedHook>> =
        const { std::cell::RefCell::new(None) };
}

/// [`write_config_strictly`] with the body write and sync supplied, so a test
/// can make them fail.
fn write_config_strictly_with(
    configured: &Path,
    target: &Path,
    bytes: &[u8],
    expected: Option<FileStamp>,
    write_body: impl FnOnce(&mut std::fs::File, &[u8]) -> std::io::Result<()>,
    still_as_read: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    #[cfg(unix)]
    folder_others_can_change(configured)?;
    #[cfg(not(unix))]
    let _ = configured;
    let target_meta = probe_for_write(target)?;
    if target_meta.as_ref().map(FileStamp::of) != expected {
        return Err(CONFIG_CHANGED_SINCE_READ.to_string());
    }
    // Owner-only from the exclusive open that creates it (final audit A1): a
    // later `chmod` would leave a descriptor someone opened before it able to
    // read the body.
    #[cfg(unix)]
    let staging_mode = {
        use std::os::unix::fs::PermissionsExt as _;
        Some(std::fs::Permissions::from_mode(0o600))
    };
    #[cfg(not(unix))]
    let staging_mode: Option<std::fs::Permissions> = None;
    let (mut file, tmp) = create_temp_beside(target, staging_mode.as_ref())
        .map_err(|e| format!("no file can be made beside it to replace it in one step ({e})"))?;
    #[cfg(test)]
    STAGED.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook(&file);
        }
    });
    let discard = |file: std::fs::File, reason: String| {
        drop(file);
        let _ = std::fs::remove_file(&tmp);
        Err(reason)
    };
    // The mode it is published with: the file's as read, or, for a config
    // this creates, owner-only (the staging file's own mode, set again here
    // in case the umask narrowed it further).
    let published = match (&target_meta, staging_mode) {
        (Some(meta), _) => meta.permissions(),
        (None, Some(private)) => private,
        (None, None) => match file.metadata() {
            Ok(created) => created.permissions(),
            Err(e) => {
                return discard(file, format!("the file made beside it cannot be read: {e}"));
            }
        },
    };
    #[cfg(unix)]
    {
        match file.metadata() {
            Ok(replacement) => {
                let kept = target_meta.as_ref().map(FileOwnership::of);
                if !swap_keeps_owner_and_links(kept, FileOwnership::of(&replacement)) {
                    let reason = if kept.is_some_and(|k| k.nlink > 1) {
                        "it has another hard link, which replacing it would split from it"
                    } else {
                        "replacing it would change its owner or group"
                    };
                    return discard(file, reason.to_string());
                }
            }
            Err(e) => {
                return discard(file, format!("the file made beside it cannot be read: {e}"));
            }
        }
    }
    if let Err(e) = write_body(&mut file, bytes) {
        return discard(file, format!("the new file could not be written: {e}"));
    }
    if let Err(reason) = still_as_read() {
        return discard(file, reason);
    }
    if let Err(e) = file.set_permissions(published) {
        return discard(
            file,
            format!("the new file could not be given its mode: {e}"),
        );
    }
    drop(file);
    if let Err(e) = std::fs::rename(&tmp, target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("it could not be replaced in one step ({e})"));
    }
    #[cfg(unix)]
    if let Some(parent) = target.parent() {
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        if let Ok(folder) = std::fs::File::open(parent) {
            let _ = folder.sync_all();
        }
    }
    Ok(())
}

/// Create the temp file [`write_config_atomically_with`] stages the new
/// `config.toml` in, in `target`'s folder so the rename stays on one
/// filesystem. Opened with `create_new` (`O_EXCL`); a name that is taken is
/// skipped for the next one, up to 16 times.
///
/// * **The name does not include the target's.** `.dot-agent-deck-config.
///   <pid>.<seq>.tmp` is at most 58 bytes, so a config whose own name is near
///   the 255-byte limit still gets a temp file beside it.
/// * **On unix the file is created with the target's permission bits**
///   (`target_perms`, further narrowed by the umask), so its permission bits
///   are never wider than the config's, not even before the caller copies the
///   exact mode across. With no existing target it gets `0666 & !umask`, as
///   `std::fs::write` gave a new file.
fn create_temp_beside(
    target: &Path,
    target_perms: Option<&std::fs::Permissions>,
) -> std::io::Result<(std::fs::File, PathBuf)> {
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    let dir = match target.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if let Some(perms) = target_perms {
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        options.mode(perms.mode() & 0o777);
    }
    #[cfg(not(unix))]
    let _ = target_perms;

    let mut attempt = 0;
    loop {
        let tmp = dir.join(format!(
            ".dot-agent-deck-config.{}.{}.tmp",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        match options.open(&tmp) {
            Ok(file) => return Ok((file, tmp)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempt < 16 => {
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Follow `path` through any symlinks to the entry a write through it would
/// land on, which need not exist. Stops at the first entry that is not a
/// symlink or cannot be inspected, and after 40 links (the kernel's own limit),
/// leaving the open or rename to report what is wrong there.
pub(crate) fn resolve_symlink_chain(path: &Path) -> PathBuf {
    let mut current = path.to_path_buf();
    for _ in 0..40 {
        let is_link = std::fs::symlink_metadata(&current)
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false);
        if !is_link {
            break;
        }
        let Ok(link) = std::fs::read_link(&current) else {
            break;
        };
        current = match current.parent() {
            Some(parent) => parent.join(link),
            None => link,
        };
    }
    current
}

/// PRD #2374: the user config file the agent-card catalog is read from — the
/// same file [`DashboardConfig::load`] reads.
pub(crate) fn user_config_path() -> PathBuf {
    config_path()
}

fn config_path() -> PathBuf {
    if let Ok(dir) = std::env::var("DOT_AGENT_DECK_CONFIG") {
        return PathBuf::from(dir);
    }
    config_dir().join("config.toml")
}

fn session_path() -> PathBuf {
    if let Ok(dir) = std::env::var("DOT_AGENT_DECK_SESSION") {
        return PathBuf::from(dir);
    }
    config_dir().join("session.toml")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedPane {
    pub dir: String,
    pub name: String,
    pub command: String,
    /// Deprecated by issue #1199 (workspace modes were removed). A
    /// `session.toml` written by an older build may still name the mode tab
    /// this pane was the agent pane of, so the key keeps DEserializing — an
    /// unknown key would otherwise be a silent drop, and the restore path needs
    /// to know to warn — but it is never written again. A pane that carries one
    /// restores as a plain dashboard pane with one session warning.
    #[serde(default, skip_serializing)]
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
///
/// PRD #89 review-fix F12: every sub-field carries `#[serde(default)]` so a
/// MALFORMED partial `[panes.orchestration]` block degrades to a defaulted
/// snapshot (which the restore path then drift-checks and falls back from)
/// rather than failing the WHOLE-file TOML parse and dropping ALL panes. A
/// zero-default `version`/empty `roles` is harmless — the restore path treats a
/// role-less / out-of-range snapshot as drift (F2) and restores a plain pane.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OrchestrationSnapshot {
    /// Schema version, for future migration. `1` is the initial format.
    #[serde(default)]
    pub version: u32,
    /// Role names in DISPLAY order — the same order as the tab's
    /// `role_pane_ids`, so the restore branch can recreate the role panes
    /// in the order the user saw them.
    #[serde(default)]
    pub roles: Vec<String>,
    /// Index into `roles` of the start (orchestrator) role — restores the
    /// "next role to start" cursor.
    #[serde(default)]
    pub start_role_index: usize,
    /// Pre-built prompt injected into the start (orchestrator) role on
    /// restore. Empty string when the orchestration had no prompt.
    #[serde(default)]
    pub orchestrator_prompt: String,
    /// Resolved orchestration config NAME — half of the reference used to
    /// re-resolve the `OrchestrationConfig` from disk on restore.
    #[serde(default)]
    pub config_name: String,
    /// Project PATH the orchestration was resolved from (the directory that
    /// holds `.dot-agent-deck.toml`) — the other half of the re-resolution
    /// reference.
    #[serde(default)]
    pub project_path: String,
    /// Which roles had been started, by index into `roles`. Optional —
    /// snapshots that predate this field load with an empty list.
    ///
    /// Issue #929: from [`ORCHESTRATION_SNAPSHOT_VERSION`] 2 on, the roles
    /// whose panes were live when the snapshot was written
    /// ([`crate::tab::TabManager::record_started_roles`]), and the daemon-empty
    /// restore starts only those ([`Self::roles_to_start_on_restore`]). A
    /// version-1 snapshot always holds `[start_role_index]`, whatever was
    /// running, so it is no record of what ran and restore ignores it.
    #[serde(default)]
    pub started_role_indices: Vec<usize>,
    /// PRD #89 review-fix F4: the user-typed orchestration tab TITLE
    /// (`Tab::Orchestration.name`), captured so the daemon-empty restore path
    /// rebuilds the tab under the user's title rather than the canonical
    /// config/cwd name. `None` when the user didn't name the orchestration
    /// (the title then falls back to the resolved canonical name on restore).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_title: Option<String>,
}

/// The [`OrchestrationSnapshot::version`] this build writes.
///
/// Issue #929 moved it from 1 to 2: a version-2 snapshot's
/// `started_role_indices` lists the roles that were live when it was written,
/// while a version-1 snapshot's was always `[start_role_index]`. Nothing else
/// in the format changed, so each build still reads the other's snapshots: an
/// older deck ignores the list and starts every role, as it always has.
pub const ORCHESTRATION_SNAPSHOT_VERSION: u32 = 2;

impl OrchestrationSnapshot {
    /// Issue #929: which of the re-resolved orchestration's `role_count` roles
    /// the daemon-empty restore starts, by index; a role it does not start
    /// comes back as a dead slot. A snapshot older than version 2 starts every
    /// role, as restore always has. A newer one starts the roles in
    /// `started_role_indices`, ignoring an index past `role_count`, plus the
    /// saved start role, which receives the replayed prompt and is the pane
    /// that may delegate.
    pub fn roles_to_start_on_restore(&self, role_count: usize) -> Vec<bool> {
        if self.version < 2 {
            return vec![true; role_count];
        }
        let mut start = vec![false; role_count];
        for &index in self
            .started_role_indices
            .iter()
            .chain(std::iter::once(&self.start_role_index))
        {
            if let Some(slot) = start.get_mut(index) {
                *slot = true;
            }
        }
        start
    }
}

/// Issue #949 — where the user was looking when the snapshot was taken, so a
/// detach/reattach puts them back there instead of on the deck's landing
/// default with every tab reset to its fallback role. A cold start against an
/// EMPTY daemon deliberately restores less — see [`Self::without_pane_ids`].
///
/// **Every locator here is a PANE ID, never a tab index or a tab name.** Tab
/// indices are not stable across a reattach — warm-daemon hydration rebuilds
/// tabs in whatever order the daemon's agent list partitions into buckets —
/// whereas a DAEMON-OWNED pane's id is: the daemon captures each agent's
/// `DOT_AGENT_DECK_PANE_ID` and echoes it back through `list_agents`, and
/// `EmbeddedPaneController::hydrate_from_daemon` reuses that value verbatim.
/// A pane also belongs to exactly one tab, so one id locates the tab *and* the
/// pane inside it, and no tab identity has to be invented or persisted.
/// Locally-spawned panes get counter ids that are NOT an identity across
/// processes, which is what [`Self::retain_pane_ids`] filters out on restore.
///
/// Focus is CLIENT-owned state, which is why it lives here rather than on the
/// wire: two clients attached to one daemon legitimately look at different
/// things, so no client's focus move may relocate another LIVE client's view,
/// and a snapshot also survives a daemon restart, which a daemon-held cursor
/// cannot. See [`SavedSession::focus`].
///
/// That is a statement about live clients and not about the FILE, which is
/// single-writer per `$HOME` (or `DOT_AGENT_DECK_SESSION`): two decks running
/// under one of those overwrite each other's snapshot wholesale, so the last
/// one to write decides where the NEXT attach lands. Position inherits that
/// from [`SavedSession::panes`] and [`SavedSession::last_command`] rather than
/// introducing it, and a daemon-held cursor would not fix it — it would replace
/// last-writer-wins with a deliberately shared cursor, which is the outcome the
/// client-owned choice is avoiding. Keying the file per client is the real fix
/// and belongs to the whole snapshot, not to this field.
///
/// Every field carries `#[serde(default)]` for the same reason
/// [`OrchestrationSnapshot`]'s do: a malformed or partial `[focus]` table
/// degrades to a defaulted value rather than failing the WHOLE-file TOML parse
/// and dropping every saved pane with it. A fully defaulted value reads as
/// "nothing remembered" and leaves the deck's own landing choice alone — which
/// is why `dashboard_active` is an explicit flag instead of being implied by an
/// absent `active_pane`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct SavedFocus {
    /// Schema version, for future migration. `1` is the initial format.
    #[serde(default)]
    pub version: u32,
    /// Whether the DASHBOARD tab was the active one. A remembered Dashboard is
    /// a real position, not an absent one — see
    /// `TabManager::apply_focus_snapshot`.
    #[serde(default)]
    pub dashboard_active: bool,
    /// The pane focused in the tab that was ACTIVE, which doubles as the
    /// locator for which tab that was. Taken from the pane controller rather
    /// than from the tab's own field, because an orchestration tab stores `None`
    /// to mean "the start role's pane" and a locator has to be a concrete id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_pane: Option<String>,
    /// The remembered focused pane of every tab that had one, the active tab's
    /// included. Restoring these is what stops a later `Tab`-away-and-back from
    /// dumping the user on each tab's start role — the active tab alone would
    /// fix only the first screen after a reattach.
    #[serde(default)]
    pub tab_panes: Vec<String>,
}

impl SavedFocus {
    /// Issue #949 — keep only the locators the DAEMON supplied on this startup,
    /// dropping every other pane id. The id-free part (which tab KIND was
    /// active) always survives, because it needs no ids at all.
    ///
    /// **A pane id is a real identity only for a pane the daemon owns.** For
    /// those it is genuinely stable: the daemon captured the agent's
    /// `DOT_AGENT_DECK_PANE_ID` at spawn and echoes it back on `list_agents`, so
    /// `EmbeddedPaneController::hydrate_from_daemon` reuses the very same value
    /// the previous TUI had. Every OTHER pane's id comes from `allocate_id`, a
    /// bare counter, and is reused across processes by coincidence rather than
    /// by identity. That is EVERY restored pane on the daemon-EMPTY rebuild
    /// path (pass an empty set there, and nothing id-shaped is honoured).
    ///
    /// Reusing a counter is worse than useless rather than merely unhelpful: a
    /// startup that recreates the same panes in the same order happens to
    /// reproduce the same numbers, but one that does not — a pane closed and
    /// another created before the snapshot was written — reassigns a
    /// remembered number to a DIFFERENT pane, and restores focus, and every
    /// keystroke after it, to the wrong terminal instead of failing to restore. A silent wrong answer is the one
    /// outcome worth engineering against here.
    ///
    /// [`crate::tab::TabManager::apply_focus_snapshot`] then still honours a
    /// remembered Dashboard and falls back to the startup's own landing choice
    /// for everything the filter removed.
    pub fn retain_pane_ids(&self, daemon_pane_ids: &HashSet<String>) -> Self {
        Self {
            active_pane: self
                .active_pane
                .as_ref()
                .filter(|id| daemon_pane_ids.contains(*id))
                .cloned(),
            tab_panes: self
                .tab_panes
                .iter()
                .filter(|id| daemon_pane_ids.contains(*id))
                .cloned()
                .collect(),
            ..self.clone()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SavedSession {
    #[serde(default)]
    pub panes: Vec<SavedPane>,
    /// PRD #196: the global "last command" — the most recent command the user
    /// spawned an INTERACTIVE agent with from the new-pane flow. Read back as the
    /// new-pane Command-field seed when `default_command` is empty (the fallback
    /// chain default → last → blank). `Option` + `#[serde(default)]` so an old
    /// `session.toml` written before this field existed loads as `None` (PRD:
    /// treat missing/unreadable as empty, never a hard failure). Authoring-mode
    /// fallback spawns (schedule / issue-dispatch) are excluded from recording.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_command: Option<String>,
    /// Issue #949: the user's last position — which tab was active and which
    /// pane each tab had focused. `Option` + `#[serde(default)]` so a
    /// `session.toml` written before this field existed (no `[focus]` table)
    /// loads as `None`, which means "nothing remembered" and leaves the deck's
    /// own landing choice untouched.
    ///
    /// Like [`Self::last_command`] this is GLOBAL state rather than something
    /// derived from the pane list, so [`Self::snapshot`] leaves it `None` and
    /// the caller overlays the runtime value before persisting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<SavedFocus>,
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
        use std::io::Write;

        let path = session_path();
        let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
        // PRD #163 auditor: through the fsperm seam rather than plain
        // `create_dir_all`, so the directory holding this snapshot is owner-only
        // too — parity with `schedules.toml`/`remotes.toml`. The file's own
        // mode/DACL protects the command lines and prompts inside; this protects
        // the directory metadata when the config dir is redirected somewhere
        // shared. Create-only: an existing directory keeps its mode (PRD #127 S2).
        crate::platform::fsperm::create_owner_only_dir(parent)
            .map_err(|e| format!("Failed to create session directory: {e}"))?;

        let contents = toml::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize session: {e}"))?;

        // PRD #89 review-fix F6/F7: write owner-only (0o600) AND atomically. The
        // snapshot now carries command lines, project paths, and prompts and is
        // written continuously, so (a) it must not be world-readable regardless
        // of the user's umask, and (b) a crash mid-write must never leave a
        // half-written `session.toml` for the next launch to choke on. Mirror
        // remote.rs: write a sibling temp file opened with mode 0o600, then
        // `rename(2)` it into place (atomic on a POSIX same-filesystem rename).
        // The pid suffix avoids collisions between concurrently-saving decks.
        let file_name = path
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| "session.toml".to_string());
        let tmp_path = parent.join(format!("{file_name}.{}.tmp", std::process::id()));

        // PRD #42 M1: owner-only (0o600) creation mode comes from the platform
        // seam — `.mode(0o600)` on Unix; on Windows (#163) the DACL cannot be
        // supplied at create time, so the seam instead puts `WRITE_DAC` on the
        // handle, which is what lets the `set_file_owner_only` call below apply it.
        let mut open_opts = std::fs::OpenOptions::new();
        open_opts.create(true).write(true).truncate(true);
        crate::platform::fsperm::set_create_mode_owner_only(&mut open_opts);
        let mut tmp_file = open_opts
            .open(&tmp_path)
            .map_err(|e| format!("Failed to open temp session at {}: {e}", tmp_path.display()))?;

        // Owner-only permissions BEFORE the first content byte (PRD #163 M4).
        // Defense in depth on Unix (a stale temp file from a crashed previous save
        // would keep its old mode, since the create-mode only applies on create),
        // and on Windows where the protected current-user-only DACL is applied at
        // all — `std::fs::OpenOptions` has no `SECURITY_ATTRIBUTES` hook, so the
        // create-mode seam can only pre-authorize this call (`WRITE_DAC`), not
        // stand in for it. Running it before the write means the
        // snapshot's command lines, project paths and prompts are never exposed
        // under a loose inherited ACL; the end state on Unix is identical.
        if let Err(e) = crate::platform::fsperm::set_file_owner_only(&tmp_file) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(format!(
                "Failed to set permissions on temp session at {}: {e}",
                tmp_path.display()
            ));
        }
        if let Err(e) = tmp_file.write_all(contents.as_bytes()) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(format!(
                "Failed to write temp session at {}: {e}",
                tmp_path.display()
            ));
        }
        drop(tmp_file);

        std::fs::rename(&tmp_path, &path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp_path);
            format!("Failed to write session at {}: {e}", path.display())
        })
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
    /// Must be called *before* tearing down orchestration tabs — i.e., while
    /// `live_panes` (the authoritative `state.managed_pane_ids`) still contains
    /// every pane. `retain` here only prunes panes the user externally closed
    /// before exit; running it after teardown would also drop the tabs' panes
    /// and lose their metadata, breaking auto-restore of those tabs (PRD #69).
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
            // PRD #196: the snapshot is built from live panes only; the caller
            // overlays the runtime `last_command` before persisting (it is global
            // state, not derived from the pane list).
            last_command: None,
            // Issue #949: same shape as `last_command` — the focused pane and
            // active tab are tab state, not pane state, so the caller overlays
            // them.
            focus: None,
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

/// PRD #120 GitHub issue-dispatch configuration, carried by a
/// `[[scheduled_tasks]]` entry whose `[scheduled_tasks.issue_dispatch]` table is
/// present. The table's **presence is the task-type discriminator**: a
/// [`ScheduledTask`] with `issue_dispatch == Some(_)` enumerates open issues for
/// `repo` and dispatches one agent per issue on fire, instead of the single
/// spawn behaviour of PRD #127. The shared scheduler fields — `name`, `cron`,
/// `working_dir` (the workspace root the repo clones under), `prompt` (the
/// per-issue template, see [`crate::issue_dispatch`]), and `enabled` — still come
/// from the enclosing [`ScheduledTask`]; only the GitHub-specific knobs live
/// here. One repo per task is a locked PRD decision (several repos → several
/// schedules), which is why this is a single `repo`, not a list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IssueDispatchConfig {
    /// Target repo in `owner/name` form (e.g. `vfarcic/dot-ai`).
    pub repo: String,
    /// Maximum number of issues dispatched per fire (the per-run cap). Omitting
    /// it defaults to 3 — the value documented in the changelog — so a
    /// hand-written `[scheduled_tasks.issue_dispatch]` table that leaves it out
    /// still deserializes (and the task still fires) instead of being rejected.
    #[serde(default = "default_max_per_run")]
    pub max_per_run: usize,
    /// Optional label filter (e.g. `agent-eligible`). `None` = no label gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Optional raw `gh` search-query override for advanced users. `None` = the
    /// default "all open issues up to `max_per_run`" listing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// PRD #421 Phase 3: the `claim_gating` / `claim_ttl_hours` keys, written
    /// flat on the `[scheduled_tasks.issue_dispatch]` table.
    #[serde(flatten)]
    pub claim: ClaimGatingConfig,
}

/// PRD #421 M3.4: whether a live claim on GitHub stops an `issue_dispatch`
/// task from dispatching an issue, and for how long a claim record stays live
/// after its last edit. File-only keys of `[scheduled_tasks.issue_dispatch]`.
/// The switch turns off the GATE only: the claim is written, and the owner's
/// record refreshed, either way.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClaimGatingConfig {
    /// `claim_gating`: honour other decks' live claims. Default `false`.
    #[serde(rename = "claim_gating", default, skip_serializing_if = "is_false")]
    pub gating: bool,
    /// `claim_ttl_hours`: how long after its last edit a claim record stays
    /// live. From 1 to [`crate::issue_dispatch::MAX_CLAIM_TTL_HOURS`] (checked
    /// at load); default [`crate::issue_dispatch::DEFAULT_CLAIM_TTL_HOURS`].
    /// The deck's claim records carry it, and other decks read them with it.
    #[serde(
        rename = "claim_ttl_hours",
        default = "default_claim_ttl_hours",
        skip_serializing_if = "is_default_claim_ttl_hours"
    )]
    pub ttl_hours: u32,
    /// Why the load turned `gating` off although the file turns it on: the
    /// TTL cannot be kept current by the task's cron (PRD #421 M3.1). Set at
    /// load and never written back, so a CLI edit that rewrites the file keeps
    /// the user's `claim_gating = true`.
    #[serde(skip)]
    pub gating_off: Option<String>,
}

impl Default for ClaimGatingConfig {
    fn default() -> Self {
        Self {
            gating: false,
            ttl_hours: default_claim_ttl_hours(),
            gating_off: None,
        }
    }
}

impl ClaimGatingConfig {
    /// Whether a fire should consult claims: switched on, and not turned off
    /// at load.
    pub fn gating_in_effect(&self) -> bool {
        self.gating && self.gating_off.is_none()
    }

    /// The claim TTL as a duration.
    pub fn ttl(&self) -> chrono::Duration {
        chrono::Duration::hours(i64::from(self.ttl_hours))
    }
}

fn default_claim_ttl_hours() -> u32 {
    crate::issue_dispatch::DEFAULT_CLAIM_TTL_HOURS
}

fn is_default_claim_ttl_hours(hours: &u32) -> bool {
    *hours == default_claim_ttl_hours()
}

fn is_false(value: &bool) -> bool {
    !*value
}

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
    /// Issue #835: the fire's SPAWN SHAPE, overriding what `working_dir`'s config
    /// would imply. `None` (the default, and every pre-#835 task) derives the
    /// shape from that dir's config exactly as before.
    ///
    /// Accepted values are [`crate::spawn::SpawnShapeOverride`]'s vocabulary,
    /// mirroring `dispatch --single` / `--orchestration <name>`:
    /// `"single"`, `"orchestration"`, `"orchestration:<name>"`. Validated at LOAD
    /// (see [`validate_task`]) so a hand-edited typo is a load error naming the
    /// task rather than a surprise on the next cron tick.
    ///
    /// Exists because the shape was previously derivable ONLY from the target
    /// dir, so a schedule pointed at a repo defining `[[orchestrations]]` fired a
    /// full team and silently ignored its own `command` — there was no way to say
    /// "fire one agent HERE", in the repo whose skills and git remote the task
    /// needs.
    ///
    /// Mutually exclusive with `issue_dispatch`: that task type resolves a shape
    /// per CLONED repo, so a task-level shape has nothing to apply to. The
    /// combination is REJECTED at load rather than ignored, matching how a
    /// malformed `repo` is handled — silently ignoring a field the author wrote
    /// is the defect class this whole field exists to remove.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape: Option<String>,
    /// PRD #120: when present, this is an `issue_dispatch` task — on fire it
    /// enumerates open issues for `issue_dispatch.repo` and dispatches an agent
    /// per issue, reusing `prompt` as the per-issue template (the
    /// `{{issue_number}}` placeholder substituted at fire time) and
    /// `working_dir` as the workspace root. Absent (`None`) → the original
    /// single-spawn task. The table's presence is the task-type discriminator.
    ///
    /// MUST stay the LAST field: TOML serializes it as a
    /// `[scheduled_tasks.issue_dispatch]` sub-table, which has to follow all the
    /// scalar fields of the entry to round-trip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_dispatch: Option<IssueDispatchConfig>,
}

fn default_enabled() -> bool {
    true
}

/// Default `issue_dispatch.max_per_run` when the field is omitted: 3, matching
/// the changelog's documented "enumeration cap, default 3". `pub` so the CLI
/// `schedule add` path can reuse it for `--max-per-run`'s default and not drift
/// from this serde default.
pub fn default_max_per_run() -> usize {
    3
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
/// falling back to `~/.config/...` (on Windows: `%APPDATA%\dot-agent-deck`,
/// which has no XDG stage — see
/// [`crate::platform::paths::xdg_config_home`]). `DOT_AGENT_DECK_SCHEDULES`
/// overrides it so tests never touch the real home dir.
pub fn schedules_path() -> PathBuf {
    if let Ok(p) = std::env::var("DOT_AGENT_DECK_SCHEDULES") {
        return PathBuf::from(p);
    }
    match crate::platform::paths::xdg_config_home() {
        Some(dir) => dir.join("dot-agent-deck/schedules.toml"),
        None => config_dir().join("schedules.toml"),
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
/// follow-up, USER DECISION): a schedule needs an agent command to act on
/// its prompt, and there is no silent `$SHELL` fallback. Rejection mirrors the
/// malformed-entry path — the error is surfaced via the daemon's notification
/// seam (PRD #126) and the entry is skipped, without blocking valid siblings or
/// crashing the daemon.
fn validate_task(
    mut task: ScheduledTask,
    index: usize,
) -> Result<ScheduledTask, ScheduleLoadError> {
    // Issue #835: validate the `shape` STRING here, next to the issue-dispatch
    // slug check and for the same reason — it arrives from hand-edited TOML and
    // is re-read unattended by the daemon on every fire, so a typo must be a load
    // error naming the task rather than a surprise cron tick. The parse is the
    // one shared with `dispatch` and the CLI, so a value any door accepts is a
    // value every door accepts.
    if let Some(raw) = &task.shape
        && let Err(message) = crate::spawn::SpawnShapeOverride::parse(raw)
    {
        return Err(ScheduleLoadError {
            entry: Some(index),
            message: format!("schedule {:?}: {message}", task.name),
        });
    }
    // PRD #120: an issue-dispatch task has no top-level `command` — the per-issue
    // spawn derives its command from each cloned repo's `.dot-agent-deck.toml`
    // (orchestration roles, or the single-agent default). Only the #127
    // single-spawn task type requires `command`.
    if let Some(disp) = &task.issue_dispatch {
        // M1: `repo`/`label`/`query` come from hand-edited TOML and flow into
        // `gh`/`git` argv (an argument-injection vector run unattended by the
        // daemon). Reject a malformed slug or a leading-`-` filter here rather
        // than at fire time.
        if let Err(message) = crate::issue_dispatch::validate_issue_dispatch_config(
            &disp.repo,
            disp.max_per_run,
            disp.label.as_deref(),
            disp.query.as_deref(),
        ) {
            return Err(ScheduleLoadError {
                entry: Some(index),
                message: format!("schedule {:?}: {message}", task.name),
            });
        }
        // Issue #835: `shape` has nothing to apply to on an issue-dispatch task —
        // that type resolves a shape per CLONED repo, from that clone's own
        // config. Reject the combination rather than ignoring the field, matching
        // how a malformed `repo` is handled: a silently-ignored field the author
        // deliberately wrote is exactly the defect #835 is about.
        if task.shape.is_some() {
            return Err(ScheduleLoadError {
                entry: Some(index),
                message: format!(
                    "schedule {:?}: `shape` cannot be combined with \
                     `issue_dispatch` (an issue-dispatch task resolves a shape per \
                     cloned repo, from that repo's own config); remove one of them",
                    task.name
                ),
            });
        }
        if let Err(message) = crate::issue_dispatch::validate_claim_ttl_hours(disp.claim.ttl_hours)
        {
            return Err(ScheduleLoadError {
                entry: Some(index),
                message: format!("schedule {:?}: {message}", task.name),
            });
        }
        // PRD #421 M3.1: a TTL the cron cannot keep current turns the GATE off
        // rather than rejecting the task — dispatching, and writing the claim,
        // carry on. The scheduler reports why on every (re)load.
        if disp.claim.gating {
            let gating_off = crate::issue_dispatch::check_claim_ttl_against_cron(
                &task.cron,
                disp.claim.ttl_hours,
                &chrono::Local::now(),
            )
            .err();
            if let Some(disp) = task.issue_dispatch.as_mut() {
                disp.claim.gating_off = gating_off;
            }
        }
        return Ok(expand_task(task));
    }
    match &task.command {
        Some(cmd) if !cmd.trim().is_empty() => Ok(expand_task(task)),
        _ => Err(ScheduleLoadError {
            entry: Some(index),
            message: format!(
                "schedule {:?} has no `command`; a command is required \
                 (a schedule needs an agent command to act on its prompt — \
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
///
/// The `~` expansion uses the host's own separator (issue #1136). On Windows
/// the home directory is native (`C:\Users\me`), so gluing the remainder onto
/// it with a literal `/` produced the mixed-separator `C:\Users\me/proj`. That
/// string resolves — Win32 accepts `/` — but it does not stay in memory: it is
/// persisted to `schedules.toml` by `schedule add`/`update`, applied to every
/// loaded task's `working_dir`, shown back to the user, and carried over the
/// protocol as a fire's `working_dir` or an orchestration cwd. So what it cost
/// was inconsistent stored and displayed text, not a failed spawn.
///
/// **Only the tilde expansion changes.** A path the user typed without a `~` is
/// passed through exactly as typed on both platforms, and the relative-vs-
/// absolute test stays the leading-`/` one rather than becoming
/// `Path::is_absolute`. On Windows those differ — `/work/space` is rooted on
/// whichever drive is current, so `is_absolute` is false and it would be
/// re-anchored as `C:\work\space` — but that is a change to text the user
/// wrote, not to text this function welded together, and several tests pin the
/// current behaviour. Whether a driveless rooted path should be anchored at all
/// belongs with native Windows support (#164), not here.
pub fn expand_path(input: &str) -> String {
    use crate::platform::paths::{join_home_and_rest, strip_tilde_prefix};

    let home = dirs_home();

    // `~` / `~/...` → home. `~\...` counts too on Windows, where `\` is a path
    // separator; on Unix it names a directory and is left alone.
    let after_tilde = if input == "~" {
        return home.to_string_lossy().into_owned();
    } else if let Some(rest) = strip_tilde_prefix(input) {
        join_home_and_rest(&home.to_string_lossy(), rest)
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

/// Where the star-prompt state lives: `star-prompt-state.json` in the
/// platform config dir, or the file `DOT_AGENT_DECK_STAR_PROMPT` names.
///
/// PRD #2174: the TUI and the desktop app both read and write this one file,
/// so a launch of either counts and a choice made in either holds in both.
pub fn star_prompt_path() -> PathBuf {
    if let Ok(p) = std::env::var("DOT_AGENT_DECK_STAR_PROMPT") {
        return PathBuf::from(p);
    }
    config_dir().join("star-prompt-state.json")
}

/// The star-repo reminder's state (PRD #37), shared by the TUI and the
/// desktop app (PRD #2174).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StarPromptState {
    pub launch_count: u64,
    pub permanently_dismissed: bool,
    pub last_prompt_at_launch: u64,
    /// The file this state is read from and saved to. Not serialised: it is
    /// where the state lives, not part of it.
    #[serde(skip)]
    path: Option<PathBuf>,
}

/// What the file at a path holds: a state, nothing yet, or something that
/// could not be read (said once, by [`StarPromptState::load_from`]).
enum StarPromptRead {
    State(StarPromptState),
    Missing,
    Unreadable(String),
}

fn read_star_prompt(path: &Path) -> StarPromptRead {
    match std::fs::read_to_string(path) {
        Ok(contents) => match serde_json::from_str::<StarPromptState>(&contents) {
            Ok(state) => StarPromptRead::State(state),
            Err(err) => StarPromptRead::Unreadable(format!(
                "Invalid star prompt state at {}: {err}",
                path.display()
            )),
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => StarPromptRead::Missing,
        Err(err) => StarPromptRead::Unreadable(format!(
            "Failed to read star prompt state at {}: {err}",
            path.display()
        )),
    }
}

impl StarPromptState {
    /// The state at [`star_prompt_path`].
    pub fn load() -> Self {
        Self::load_from(star_prompt_path())
    }

    /// The state at `path`; the default when the file is missing or cannot be
    /// read (saying why on stderr for the second).
    pub fn load_from(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut state = match read_star_prompt(&path) {
            StarPromptRead::State(state) => state,
            StarPromptRead::Missing => Self::default(),
            StarPromptRead::Unreadable(why) => {
                eprintln!("{why}");
                Self::default()
            }
        };
        state.path = Some(path);
        state
    }

    fn path(&self) -> PathBuf {
        self.path.clone().unwrap_or_else(star_prompt_path)
    }

    /// Write the state to its file.
    ///
    /// PRD #2174: through [`write_config_atomically`] — a uniquely named,
    /// exclusively created sibling file renamed over the old one, following a
    /// symlinked state file to its target — so a reader, the other client
    /// starting at the same moment, sees the old state or the new one, never a
    /// half-written file. An unreadable file reads as "never dismissed",
    /// which would ask again someone who had said "Don't ask again".
    pub fn save(&self) -> Result<(), String> {
        let path = self.path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create star prompt directory: {e}"))?;
        }
        let contents = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize star prompt state: {e}"))?;
        write_config_atomically(&path, contents.as_bytes()).map_err(|e| {
            format!(
                "Failed to write star prompt state at {}: {e}",
                path.display()
            )
        })
    }

    /// Fold in what the file holds now.
    ///
    /// PRD #2174: the other client may have written the file since this one
    /// read it — counted a launch, or been told "Don't ask again". Every
    /// field only ever grows (a count, a flag that is never unset, the launch
    /// a reminder was last shown at), so keeping the larger of each never
    /// undoes a choice made in the other client. Quiet on purpose: the TUI
    /// calls this while it owns the terminal, where stderr would draw over
    /// the screen, and the startup read has already said what it found.
    fn refresh(&mut self) {
        if let StarPromptRead::State(disk) = read_star_prompt(&self.path()) {
            self.launch_count = self.launch_count.max(disk.launch_count);
            self.permanently_dismissed |= disk.permanently_dismissed;
            self.last_prompt_at_launch = self.last_prompt_at_launch.max(disk.last_prompt_at_launch);
        }
    }

    /// Change the state and save it, as one step against the other client.
    ///
    /// PRD #2174: under an exclusive lock on a sidecar beside the state file
    /// ([`lock_star_prompt`]), the file is read and folded in
    /// ([`Self::refresh`]), `change` is applied, and the result saved — so a
    /// launch or a Later saved by one client cannot replace a Don't ask again
    /// the other saved a moment earlier, and two launches at once both count.
    fn update(&mut self, change: impl FnOnce(&mut Self)) -> Result<(), String> {
        let path = self.path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create star prompt directory: {e}"))?;
        }
        let _lock = lock_star_prompt(&path)?;
        self.refresh();
        change(self);
        self.save()
    }

    /// Whether this launch is one the reminder is shown at.
    pub fn is_due(&self) -> bool {
        !self.permanently_dismissed
            && self.launch_count.saturating_sub(self.last_prompt_at_launch) >= STAR_PROMPT_INTERVAL
    }

    /// Count this launch, save it, and answer whether to show the reminder.
    /// A launch that cannot be saved still counts once, in what this client
    /// answers from — whether the save failed before the count was made (the
    /// lock) or after it (the write; Qodo, PR #2321).
    pub fn increment_and_check(&mut self) -> bool {
        let mut counted = false;
        let _ = self.update(|state| {
            state.launch_count += 1;
            counted = true;
        });
        if !counted {
            self.launch_count += 1;
        }
        self.is_due()
    }

    /// Remind me later: ask again after another [`STAR_PROMPT_INTERVAL`]
    /// launches of either client. The error says why it could not be saved.
    pub fn snooze(&mut self) -> Result<(), String> {
        self.update(|state| state.last_prompt_at_launch = state.launch_count)
    }

    /// Star on GitHub, or Don't ask again: never ask again, in either client.
    /// The error says why it could not be saved.
    pub fn dismiss_permanently(&mut self) -> Result<(), String> {
        self.update(|state| state.permanently_dismissed = true)
    }
}

/// How long a save waits for the other client to finish its own.
const STAR_PROMPT_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(2);
const STAR_PROMPT_LOCK_POLL: std::time::Duration = std::time::Duration::from_millis(20);

/// Take the exclusive lock that serialises the two clients' updates of the
/// star-prompt state at `path`; released when the returned file is dropped.
///
/// The pattern `deck_list::acquire_edit_lock` uses, for its reasons: the state
/// file cannot carry the lock, because every save replaces it by rename, so an
/// empty, owner-only sidecar `.<name>.lock` that is never replaced or deleted
/// holds it. The sidecar sits beside the file a save actually replaces — a
/// symlinked state file's target — so two clients reaching one file by
/// different paths take one lock (Qodo, PR #2321).
///
/// Only the other client still holding the lock after
/// [`STAR_PROMPT_LOCK_WAIT`] is an error, and nothing is saved then. A sidecar
/// that cannot be opened, or a filesystem that cannot lock (`Unsupported`, or
/// an NFS home without a lock daemon's `ENOLCK`), goes ahead unlocked: the
/// update is still made against a fresh read, and refusing there would count
/// no launch and save no choice at all (Qodo, PR #2321).
fn lock_star_prompt(path: &Path) -> Result<Option<std::fs::File>, String> {
    lock_reminder_state(path, "star-prompt-state.json", "star prompt state")
}

/// The lock [`lock_star_prompt`] describes, for any reminder state shared by
/// the two clients: `default_name` names the sidecar when `path` has no file
/// name, and `what` names the state in the error.
fn lock_reminder_state(
    path: &Path,
    default_name: &str,
    what: &str,
) -> Result<Option<std::fs::File>, String> {
    let target = resolve_symlink_chain(path);
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| default_name.to_string());
    let lock_path = target.with_file_name(format!(".{name}.lock"));
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    crate::platform::fsperm::set_create_mode_owner_only(&mut options);
    let Ok(file) = options.open(&lock_path) else {
        return Ok(None);
    };
    let deadline = std::time::Instant::now() + STAR_PROMPT_LOCK_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Some(file)),
            Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(STAR_PROMPT_LOCK_POLL);
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(format!(
                    "Failed to save the {what}: {} stayed locked",
                    lock_path.display()
                ));
            }
            Err(std::fs::TryLockError::Error(_)) => return Ok(None),
        }
    }
}

/// Where the Antigravity experimental notice's state lives:
/// `antigravity-notice-state.json` in the platform config dir, or the file
/// `DOT_AGENT_DECK_ANTIGRAVITY_NOTICE` names.
///
/// PRD #211: like [`star_prompt_path`], one file the TUI and the desktop app
/// both read and write, so a start in either counts and Don't show again in
/// either holds in both.
pub fn antigravity_notice_path() -> PathBuf {
    if let Ok(p) = std::env::var("DOT_AGENT_DECK_ANTIGRAVITY_NOTICE") {
        return PathBuf::from(p);
    }
    config_dir().join("antigravity-notice-state.json")
}

/// The Antigravity experimental notice's state (PRD #211), shared by the TUI
/// and the desktop app. The words are [`crate::antigravity_notice`]'s.
///
/// Each update rereads the file under the lock [`lock_star_prompt`] describes
/// and changes what it read, so a start or a choice saved by the other client
/// a moment earlier is kept. What [`AntigravityNoticeSession`] could not save
/// it keeps as a change to the state it read ([`AntigravityNoticePending`]),
/// not as an absolute state, so its starts add to the other client's instead
/// of being merged away. A file that is corrupt is read as a fresh state and
/// replaced by the next save.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AntigravityNoticeState {
    /// Antigravity starts counted, from either client.
    pub start_count: u64,
    /// The start the notice was last shown at; `0` when it never was.
    pub last_prompt_at_start: u64,
    /// Don't show again, from either client.
    pub permanently_dismissed: bool,
}

/// What [`AntigravityNoticeSession::record_start`] answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AntigravityNoticeStart {
    /// Whether to show the notice for this start.
    pub show: bool,
    /// Why the state file could not be read or saved, the first time this
    /// session meets a problem; `None` after that. The start was still
    /// counted in what this session answers from.
    pub problem: Option<String>,
}

enum AntigravityNoticeRead {
    State(AntigravityNoticeState),
    Missing,
    Unreadable(String),
}

fn read_antigravity_notice(path: &Path) -> AntigravityNoticeRead {
    match std::fs::read_to_string(path) {
        Ok(contents) => match serde_json::from_str::<AntigravityNoticeState>(&contents) {
            Ok(state) => AntigravityNoticeRead::State(state),
            Err(err) => AntigravityNoticeRead::Unreadable(format!(
                "Invalid Antigravity notice state at {}: {err}",
                path.display()
            )),
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => AntigravityNoticeRead::Missing,
        Err(err) => AntigravityNoticeRead::Unreadable(format!(
            "Failed to read Antigravity notice state at {}: {err}",
            path.display()
        )),
    }
}

impl AntigravityNoticeState {
    /// The state in the file at `path`: the default when it is missing or
    /// cannot be read, with the reason for the second.
    pub fn load_from(path: &Path) -> (Self, Option<String>) {
        match read_antigravity_notice(path) {
            AntigravityNoticeRead::State(state) => (state, None),
            AntigravityNoticeRead::Missing => (Self::default(), None),
            AntigravityNoticeRead::Unreadable(why) => (Self::default(), Some(why)),
        }
    }

    /// Whether a start at this state's count is one the notice shows at: the
    /// first, and then the [`crate::antigravity_notice::NOTICE_INTERVAL`]th
    /// after the last showing, until Don't show again.
    pub fn is_due(&self) -> bool {
        !self.permanently_dismissed
            && self.start_count > 0
            && (self.last_prompt_at_start == 0
                || self.start_count.saturating_sub(self.last_prompt_at_start)
                    >= crate::antigravity_notice::NOTICE_INTERVAL)
    }

    fn save_to(&self, path: &Path) -> Result<(), String> {
        let contents = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize Antigravity notice state: {e}"))?;
        write_config_atomically(path, contents.as_bytes()).map_err(|e| {
            format!(
                "Failed to write Antigravity notice state at {}: {e}",
                path.display()
            )
        })
    }
}

/// What an [`AntigravityNoticeSession`] changed but could not save, relative
/// to the state it last read from the file (PRD #211).
///
/// Its starts are kept as a change rather than an absolute count so that the
/// other client's starts, saved in the meantime, are added to rather than
/// merged away: the file at 10, one unsaved start here and one saved start
/// there make 12, and this session's next start saves 13. Its showing is
/// kept as the total it happened at instead, so those later starts do not
/// push it later: a showing at the 151st start stays at 151 when the other
/// client saves five starts before this one can save, and the next is due at
/// 251, not 256.
///
/// What a merge holds to: no start is lost, either side's Don't show again
/// holds, and the showing is never past the merged total. While the file
/// only moves forward — the ordinary case — the showing is also never placed
/// after starts that came after it. A file that went back (deleted or reset
/// while this session could not save) gives that total no fixed meaning, so
/// the showing is clamped to the merged total; when two sessions both hold
/// an unsaved showing over such a file, where it lands depends on which of
/// them saves first, and the next notice comes a few starts earlier or
/// later. A reminder can afford that, so no record of the reset is kept.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct AntigravityNoticePending {
    /// Starts counted here and not yet saved.
    starts: u64,
    /// The showing among those starts, as the total it happened at: the
    /// file's count when this session read it for that start, plus this
    /// session's unsaved starts up to and including it. `None` when none
    /// showed it.
    shown_at: Option<u64>,
    /// Don't show again, chosen or read here; either side's holds.
    dismissed: bool,
}

impl AntigravityNoticePending {
    /// Apply this change to `state`, read from the file: its starts follow
    /// the file's, a showing among them keeps the total it happened at
    /// (clamped to the merged total, for a file that went back), the later
    /// of it and the file's showing holds, and either side's Don't show
    /// again holds. See [`AntigravityNoticePending`] for what that keeps
    /// and what, after a reset, it does not.
    fn apply_to(&self, state: &mut AntigravityNoticeState) {
        state.start_count = state.start_count.saturating_add(self.starts);
        if let Some(at) = self.shown_at {
            state.last_prompt_at_start = state.last_prompt_at_start.max(at.min(state.start_count));
        }
        state.permanently_dismissed |= self.dismissed;
    }

    /// The change that turns `read`, the file's state, into `changed`.
    fn between(read: &AntigravityNoticeState, changed: &AntigravityNoticeState) -> Self {
        Self {
            starts: changed.start_count.saturating_sub(read.start_count),
            // `apply_to` and a start only move the showing later, so one that
            // differs from the file's is this session's.
            shown_at: (changed.last_prompt_at_start > read.last_prompt_at_start)
                .then_some(changed.last_prompt_at_start),
            dismissed: changed.permanently_dismissed,
        }
    }
}

/// One client's use of the Antigravity notice's state for as long as it runs
/// (PRD #211): the TUI holds one in its UI state, the desktop app one per
/// process.
///
/// Every update reads the file at `path`, applies the change and saves it, as
/// one step against the other client. When a save fails, what changed is kept
/// here as an [`AntigravityNoticePending`] and applied to every later read,
/// so this session goes on answering from the file's state plus its own: a
/// start that could not be saved counts once the file can be written again,
/// on top of any the other client saved meanwhile, does not bring the
/// notice back at the next start, and a Don't show again that could not be
/// saved holds until the client restarts. The first problem met is said once;
/// later ones are not repeated.
#[derive(Debug, Clone)]
pub struct AntigravityNoticeSession {
    path: PathBuf,
    /// What this session could not save, applied to each later read.
    unsaved: Option<AntigravityNoticePending>,
    /// Whether a problem was already handed to the caller to say.
    problem_said: bool,
}

impl AntigravityNoticeSession {
    /// A session over the state file at `path`.
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            unsaved: None,
            problem_said: false,
        }
    }

    /// The state file this session reads and writes.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The state this session answers from now: the file's, with anything it
    /// could not save added.
    pub fn state(&self) -> AntigravityNoticeState {
        let (mut state, _) = AntigravityNoticeState::load_from(&self.path);
        if let Some(unsaved) = &self.unsaved {
            unsaved.apply_to(&mut state);
        }
        state
    }

    /// `problem`, the first time this session has one to say.
    fn say_once(&mut self, problem: Option<String>) -> Option<String> {
        problem.filter(|_| !std::mem::replace(&mut self.problem_said, true))
    }

    /// Read the file, add what was not saved, apply `change`, and save,
    /// under the lock [`lock_star_prompt`] describes. Answers whether it was
    /// saved, and why the file could not be read or saved, if it could not.
    fn update(
        &mut self,
        change: impl FnOnce(&mut AntigravityNoticeState),
    ) -> (bool, Option<String>) {
        let path = self.path.clone();
        let mut problem = None;
        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            problem = Some(format!(
                "Failed to create Antigravity notice directory: {e}"
            ));
        }
        let lock = lock_reminder_state(
            &path,
            "antigravity-notice-state.json",
            "Antigravity notice state",
        );
        let (read, unreadable) = AntigravityNoticeState::load_from(&path);
        let mut state = read.clone();
        if let Some(unsaved) = &self.unsaved {
            unsaved.apply_to(&mut state);
        }
        // `change` decides on the total: the file's starts and this
        // session's unsaved ones together.
        change(&mut state);
        let saved = match lock {
            // The other client kept the file locked: save nothing over it.
            Err(why) => Err(why),
            Ok(_lock) => state.save_to(&path),
        };
        let problem = problem.or(unreadable);
        match saved {
            Ok(()) => {
                self.unsaved = None;
                (true, problem)
            }
            Err(why) => {
                self.unsaved = Some(AntigravityNoticePending::between(&read, &state));
                (false, problem.or(Some(why)))
            }
        }
    }

    /// Count a user-started Antigravity start, and answer whether to show the
    /// notice for it. A showing is recorded with the count, so Later and Open
    /// issues need not save anything: the notice comes back at the next
    /// interval either way.
    pub fn record_start(&mut self) -> AntigravityNoticeStart {
        let mut show = false;
        let (_, problem) = self.update(|state| {
            state.start_count = state.start_count.saturating_add(1);
            show = state.is_due();
            if show {
                state.last_prompt_at_start = state.start_count;
            }
        });
        let problem = self.say_once(problem);
        AntigravityNoticeStart { show, problem }
    }

    /// Don't show again, in either client. It holds in this session whether
    /// or not it could be saved; the answer is why it could not, when this is
    /// the first problem the session has to say.
    pub fn dismiss_permanently(&mut self) -> Option<String> {
        let (saved, problem) = self.update(|state| state.permanently_dismissed = true);
        if saved {
            return None;
        }
        self.say_once(
            problem.or_else(|| Some("Failed to save the Antigravity notice state".to_string())),
        )
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
    config_dir().join("config-gen-state.json")
}

/// PRD #2119: the config-generation dialog no longer offers "Never", so the
/// TUI neither reads nor writes this file; the type stays so a file recorded by
/// an earlier build still loads (`dashboard/config-gen/002`).
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

/// Home directory used to anchor config/state/cache paths. Delegates to
/// [`crate::platform::paths::home_dir`] (PRD #42 M1): `$HOME` (fallback `/`) on
/// Unix, `%USERPROFILE%` on Windows.
pub(crate) fn dirs_home() -> PathBuf {
    crate::platform::paths::home_dir()
}

/// Directory holding the user's dot-agent-deck config files. Delegates to
/// [`crate::platform::paths::config_dir`] (PRD #163 M1): `~/.config/dot-agent-deck`
/// on Unix — byte-for-byte what every caller previously spelled inline —
/// `%APPDATA%\dot-agent-deck` on Windows.
///
/// Each per-file `DOT_AGENT_DECK_*` override is checked by its own resolver
/// *before* this is called, so overrides stay authoritative.
pub(crate) fn config_dir() -> PathBuf {
    crate::platform::paths::config_dir()
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
/// table only. Every other key (`[[orchestrations]]`, …) is
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
/// flag: the file in `project_dir`, the directory the CALLER has decided is
/// the project.
///
/// Issue #577: this used to join [`std::env::current_dir`] unconditionally,
/// making it the only config read in the deck keyed to the process's own
/// working directory rather than to a directory it was handed — every other
/// one goes through [`crate::project_config::load_project_config`] with an
/// explicit dir. A deck launched anywhere but its project therefore read the
/// `[features]` table from the launch directory's file (usually one that does
/// not exist), so every experimental surface silently resolved OFF and the
/// symptom was indistinguishable from the feature having been removed. The
/// cwd read now happens once at the entry point (`launch_project_dir` in
/// `main.rs`), where it is a deliberate choice rather than a hidden default.
///
/// `DOT_AGENT_DECK_FEATURES_CONFIG` names the file outright and still wins
/// over `project_dir`, so tests — and an operator pointing the flag at one
/// specific file — depend on no directory at all.
pub fn features_config_path(project_dir: &Path) -> PathBuf {
    if let Ok(p) = std::env::var("DOT_AGENT_DECK_FEATURES_CONFIG") {
        return PathBuf::from(p);
    }
    project_dir.join(crate::project_config::CONFIG_FILE_NAME)
}

/// Decide whether a candidate `.dot-agent-deck.toml` found by the ancestor
/// walk may be trusted, given the uid that owns it and the uid we are running
/// as (issue #577).
///
/// The pure-data core of the walk's trust check, split out so its rules are
/// unit-testable on every platform — the same shape as
/// [`crate::platform::fsperm::endpoint_owner_is_trusted`], and for the same
/// reason: a decision this small should not need a foreign-owned file (or
/// root) to exercise.
///
/// Walking upward means considering directories the operator did not name and
/// may not own — `/tmp/project` sits under a world-writable `/tmp`, where any
/// local user can create `/tmp/.dot-agent-deck.toml`. Ownership is the check
/// that matters: an attacker cannot create a file owned by *us*. It fails
/// closed, so an ancestor we cannot vouch for is skipped and the walk
/// continues past it rather than adopting it.
/// `cfg_attr` rather than a `#[cfg(unix)]` on the function: the rule is
/// platform-independent and its test runs everywhere, but the only production
/// caller is inside `config_candidate_is_trusted`'s `#[cfg(unix)]` arm, so on
/// Windows this is dead code to a build that does not compile the tests —
/// which `build-windows` is, since it runs bare `cargo clippy -- -D warnings`
/// without `--all-targets` (CLAUDE.md rule 2). Same shape, mirror-image
/// platform, as `fsperm::endpoint_owner_is_trusted`'s
/// `#[cfg_attr(not(windows), allow(dead_code))]`.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn config_owner_is_trusted(file_uid: u32, our_uid: u32) -> bool {
    file_uid == our_uid
}

/// The project directory the deck's process-global config reads key off:
/// `start` if it holds a trusted `.dot-agent-deck.toml`, else the nearest
/// ancestor that does, else `start` unchanged.
///
/// Issue #577: the deck used to key the `[features]` table to its own working
/// directory, so running it from anywhere below the project root — `repo/src`,
/// `repo/docs`, a nested crate — read a `.dot-agent-deck.toml` that is not
/// there and silently resolved every experimental surface OFF. Walking up is
/// how a project-scoped config is normally found, and it makes the flag depend
/// on which PROJECT you are in rather than on which of its directories you
/// happened to be standing in.
///
/// Two deliberate limits:
///
/// - A candidate must be a regular file owned by the current uid
///   ([`config_owner_is_trusted`]); anything else is skipped and the walk
///   continues. `metadata` follows symlinks, so the ownership answer is about
///   the resolved target, matching [`load_features_file`]'s own `is_file()`
///   check.
/// - When nothing qualifies the result is `start` itself, so the resulting
///   path is byte-identical to the pre-#577 behaviour and a deck launched
///   outside any project reads exactly what it read before.
///
/// This does NOT make the flag per-project — it stays one process-global
/// toggle (CLAUDE.md rule 9). A deck launched entirely outside its project,
/// with panes pointed into it, still reads the launch directory's file;
/// `DOT_AGENT_DECK_FEATURES_CONFIG` is the escape hatch there.
pub fn resolve_project_dir(start: &Path) -> PathBuf {
    // `ancestors()` yields `start` first, so a project whose config sits in
    // the launch directory itself resolves without walking anywhere and keeps
    // exactly its pre-#577 behaviour. It is lexical rather than symlink-
    // resolving, which is what we want: `current_dir()` already hands us the
    // physical path on Unix, and re-resolving would report a project root the
    // operator never typed.
    for dir in start.ancestors() {
        if config_candidate_is_trusted(&dir.join(crate::project_config::CONFIG_FILE_NAME)) {
            return dir.to_path_buf();
        }
    }
    start.to_path_buf()
}

/// Whether `path` is a `.dot-agent-deck.toml` the ancestor walk may stop at:
/// a regular file owned by the current user. Anything else — absent, a
/// directory, a foreign-owned file — is skipped so the walk continues past it.
///
/// `metadata` follows symlinks, so a symlink to a foreign-owned file is judged
/// by its target, and a dangling one is simply absent. This is the same
/// stat-then-read shape [`load_features_file`] already uses; it is not a
/// TOCTOU guarantee and does not need to be, because the loader re-validates
/// (regular file, size cap, parse) before it trusts a byte of the content.
/// This check decides only WHICH directory is the project.
fn config_candidate_is_trusted(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        config_owner_is_trusted(metadata.uid(), crate::platform::paths::current_uid())
    }
    // Windows has no uid, and the file analogue of the SID check in
    // `platform::fsperm` takes a kernel HANDLE rather than a path — it is
    // built for the named pipe and the spawn mutex, not for stat-ing a config.
    // The exposure it would close is also structurally smaller here: the walk
    // climbs through the per-user profile (`C:\Users\<me>\…`), which is
    // ACL'd to that user, before reaching `C:\`, which is admin-writable by
    // default — there is no world-writable `/tmp` equivalent on the way up.
    // So Windows accepts any regular file, and the property that differs is
    // recorded here rather than left to be inferred from the `cfg`.
    #[cfg(not(unix))]
    {
        true
    }
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

    /// Issue #2137 N1: a reader that opens `config.toml` while `config set`
    /// saves it — the daemon spawning an agent does exactly that — must see the
    /// old file or the new one, never an empty file or a prefix of one. A
    /// prefix can be valid TOML that ends before `[doorbell]`, which reads as
    /// the doorbell left on.
    #[test]
    fn a_config_save_never_shows_a_reader_an_empty_or_partial_file() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut config = DashboardConfig::default();
        config.doorbell.claude = false;
        config.save_to(&path).unwrap();
        let full = std::fs::read_to_string(&path).unwrap();
        assert!(full.contains("[doorbell]"), "{full}");

        let done = Arc::new(AtomicBool::new(false));
        let reader = {
            let (path, done, full) = (path.clone(), Arc::clone(&done), full.clone());
            std::thread::spawn(move || {
                let mut torn = Vec::new();
                while !done.load(Ordering::Relaxed) {
                    match std::fs::read_to_string(&path) {
                        Ok(seen) if seen != full => torn.push(seen.len()),
                        Ok(_) => {}
                        // The rename swaps the entry in one step, so the file
                        // is never missing either.
                        Err(_) => torn.push(usize::MAX),
                    }
                }
                torn
            })
        };
        // 200 saves: the old truncate-then-write tears most reads in this loop,
        // so this is plenty to go red on it, and each save pays a real fsync.
        for _ in 0..200 {
            config.save_to(&path).unwrap();
        }
        done.store(true, Ordering::Relaxed);
        let torn = reader.join().unwrap();
        assert!(
            torn.is_empty(),
            "a reader saw {} torn reads of {} bytes (lengths seen: {:?})",
            torn.len(),
            full.len(),
            &torn[..torn.len().min(10)]
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), full);
    }

    /// Issue #2137 N1: the atomic save keeps what `std::fs::write` did for a
    /// `config.toml` that is a (relative, chained) symlink — the links stay
    /// links and the final target gets the new content — keeps the target's
    /// mode, and leaves no temp file behind.
    #[cfg(unix)]
    #[test]
    fn a_config_save_writes_through_symlinks_keeps_the_mode_and_leaves_no_temp() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = crate::test_temp::tempdir().unwrap();
        let real_dir = dir.path().join("dotfiles");
        let link_dir = dir.path().join("config");
        std::fs::create_dir_all(&real_dir).unwrap();
        std::fs::create_dir_all(&link_dir).unwrap();
        let real = real_dir.join("deck.toml");
        std::fs::write(&real, "default_command = \"old\"\n").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o640)).unwrap();
        let hop = link_dir.join("hop.toml");
        std::os::unix::fs::symlink("../dotfiles/deck.toml", &hop).unwrap();
        let path = link_dir.join("config.toml");
        std::os::unix::fs::symlink("hop.toml", &path).unwrap();

        let config = DashboardConfig {
            default_command: "new".to_string(),
            ..DashboardConfig::default()
        };
        config.save_to(&path).unwrap();

        for link in [&path, &hop] {
            assert!(
                std::fs::symlink_metadata(link)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "{} must stay a symlink",
                link.display()
            );
        }
        assert!(
            std::fs::read_to_string(&real)
                .unwrap()
                .contains("default_command = \"new\"")
        );
        assert_eq!(
            std::fs::metadata(&real).unwrap().permissions().mode() & 0o777,
            0o640
        );
        for d in [&real_dir, &link_dir] {
            let strays: Vec<_> = std::fs::read_dir(d)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .filter(|n| n.to_string_lossy().ends_with(".tmp"))
                .collect();
            assert!(strays.is_empty(), "temp file left behind: {strays:?}");
        }
    }

    /// Issue #2137 N1: `std::fs::write` refused a `config.toml` the user made
    /// read-only, and the rename — which needs only the directory's
    /// permission — must not start overwriting it.
    #[cfg(unix)]
    #[test]
    fn a_config_save_still_refuses_a_read_only_config() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "default_command = \"old\"\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
        if std::fs::OpenOptions::new().write(true).open(&path).is_ok() {
            eprintln!("SKIP: this process can write a 0444 file (running as root)");
            return;
        }

        let config = DashboardConfig {
            default_command: "new".to_string(),
            ..DashboardConfig::default()
        };
        let err = config.save_to(&path).unwrap_err();
        assert!(err.starts_with("Failed to write config at "), "{err}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "default_command = \"old\"\n"
        );
    }

    fn temp_files_in(dir: &Path) -> Vec<std::ffi::OsString> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n.to_string_lossy().ends_with(".tmp"))
            .collect()
    }

    /// Issue #2137: `std::fs::write` saved a writable `config.toml` that sits
    /// in a folder the user cannot write. The atomic save cannot make its temp
    /// file there, so it must fall back to that in-place write rather than
    /// refuse a save that used to work.
    #[cfg(unix)]
    #[test]
    fn a_config_save_still_saves_a_writable_config_in_a_read_only_folder() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = crate::test_temp::tempdir().unwrap();
        let folder = dir.path().join("locked");
        std::fs::create_dir(&folder).unwrap();
        let path = folder.join("config.toml");
        std::fs::write(&path, "default_command = \"old\"\n").unwrap();
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o555)).unwrap();
        // Put the folder back however the test ends, so the tempdir can go.
        struct Unlock<'a>(&'a Path);
        impl Drop for Unlock<'_> {
            fn drop(&mut self) {
                let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o755));
            }
        }
        let _unlock = Unlock(&folder);
        if std::fs::File::create(folder.join("probe")).is_ok() {
            eprintln!("SKIP: this process can write a 0555 folder (running as root)");
            return;
        }

        let config = DashboardConfig {
            default_command: "new".to_string(),
            ..DashboardConfig::default()
        };
        config.save_to(&path).unwrap();

        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("default_command = \"new\"")
        );
        assert!(temp_files_in(&folder).is_empty());
    }

    /// Issue #2137: the fallback is for a place the swap cannot happen, not
    /// for a save that failed. A write that fails part-way must leave the old
    /// `config.toml` exactly as it was, report the error, and leave no temp
    /// file behind — the in-place write would have left a torn file.
    #[test]
    fn a_failed_config_write_leaves_the_old_file_and_no_temp_file() {
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "default_command = \"old\"\n").unwrap();

        let err = write_config_atomically_with(&path, b"default_command = \"new\"\n", |f, b| {
            use std::io::Write as _;
            f.write_all(&b[..b.len() / 2])?;
            Err(std::io::Error::from(std::io::ErrorKind::StorageFull))
        })
        .unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::StorageFull);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "default_command = \"old\"\n"
        );
        assert!(temp_files_in(dir.path()).is_empty());
    }

    /// Issue #2399 (review 1, audit 4): the strict save the card-action editor
    /// uses refuses, with the file untouched and no temp file left, where
    /// `config set`'s save would fall back to an in-place write — a hard-linked
    /// config, a folder that takes no new entry — and when the body write fails
    /// part-way or the caller's last check finds the file changed.
    #[cfg(unix)]
    #[test]
    fn a_strict_config_save_refuses_instead_of_writing_in_place() {
        use std::os::unix::fs::PermissionsExt as _;
        let old = "default_command = \"old\"\n";
        let new = b"default_command = \"new\"\n";

        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, old).unwrap();
        let stamp = |p: &Path| Some(FileStamp::of(&std::fs::metadata(p).unwrap()));
        let err = write_config_strictly_with(
            &path,
            &path,
            new,
            stamp(&path),
            |f, b| {
                use std::io::Write as _;
                f.write_all(&b[..b.len() / 2])?;
                Err(std::io::Error::from(std::io::ErrorKind::StorageFull))
            },
            || Ok(()),
        )
        .unwrap_err();
        assert!(err.contains("could not be written"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), old);
        assert!(temp_files_in(dir.path()).is_empty());

        let err =
            write_config_strictly(&path, &path, new, stamp(&path), || Err("it changed".into()))
                .unwrap_err();
        assert_eq!(err, "it changed");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), old);
        assert!(temp_files_in(dir.path()).is_empty());

        let twin = dir.path().join("twin.toml");
        std::fs::hard_link(&path, &twin).unwrap();
        let err = write_config_strictly(&path, &path, new, stamp(&path), || Ok(())).unwrap_err();
        assert!(err.contains("hard link"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), old);
        assert!(temp_files_in(dir.path()).is_empty());
        std::fs::remove_file(&twin).unwrap();

        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        let inside = locked.join("config.toml");
        std::fs::write(&inside, old).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
        let probe = locked.join("probe");
        if std::fs::write(&probe, "").is_ok() {
            let _ = std::fs::remove_file(&probe);
            eprintln!("SKIP (folder case): this process can write a 0555 folder (root)");
        } else {
            let err = write_config_strictly(&inside, &inside, new, stamp(&inside), || Ok(()))
                .unwrap_err();
            assert!(err.contains("beside it"), "{err}");
            assert_eq!(std::fs::read_to_string(&inside).unwrap(), old);
            // `config set`'s save keeps its fallback for the same folder.
            write_config_atomically(&inside, new).unwrap();
            assert_eq!(std::fs::read(&inside).unwrap(), new);
        }
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

        write_config_strictly(&path, &path, new, stamp(&path), || Ok(())).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), new);
        assert!(temp_files_in(dir.path()).is_empty());
    }

    /// Issue #2399 final review F2 / audit A3: the rule an existing lock file
    /// is judged by, as a unit, since a test cannot make a file owned by
    /// another user without privileges: a private regular file of this
    /// user's with one link is safe; another owner, any group or other
    /// permission bit, a second link or a file that is not regular is not.
    #[cfg(unix)]
    #[test]
    fn a_lock_file_is_safe_only_when_private_to_this_user() {
        let me = 1000;
        let safe = LockFileFacts {
            regular: true,
            uid: me,
            mode: 0o100600,
            nlink: 1,
        };
        assert_eq!(lock_file_unsafe(safe, me), None);
        let cases = [
            (LockFileFacts { uid: 1001, ..safe }, "another user"),
            (LockFileFacts { uid: 0, ..safe }, "another user"),
            (
                LockFileFacts {
                    mode: 0o100644,
                    ..safe
                },
                "permissions",
            ),
            (
                LockFileFacts {
                    mode: 0o100666,
                    ..safe
                },
                "permissions",
            ),
            (
                LockFileFacts {
                    mode: 0o100610,
                    ..safe
                },
                "permissions",
            ),
            (LockFileFacts { nlink: 2, ..safe }, "hard link"),
            (
                LockFileFacts {
                    regular: false,
                    ..safe
                },
                "not a regular file",
            ),
        ];
        for (facts, why) in cases {
            let found = lock_file_unsafe(facts, me);
            assert!(
                found.is_some_and(|f| f.contains(why)),
                "{facts:?}: {found:?}"
            );
        }
    }

    /// Issue #2399 final audit A2, round 4: the folder check follows the
    /// path's links itself, and refuses a path whose links it cannot follow
    /// to an end: a loop of folder links, a loop of file links, and a chain
    /// longer than its cap of 40 links (here 41, ending at a real file) are
    /// each refused, naming the path; a chain of 40 is followed.
    #[cfg(unix)]
    #[test]
    fn the_folder_check_refuses_a_link_loop_or_a_chain_past_its_cap() {
        let dir = crate::test_temp::tempdir().unwrap();
        // The scratch folder's own path may pass through a link (macOS's
        // `/var` is one to `/private/var`), which the check counts too, so
        // the chains below start from the path with its links resolved.
        let home = &std::fs::canonicalize(dir.path()).unwrap();
        std::os::unix::fs::symlink(home.join("loop-dir"), home.join("loop-dir")).unwrap();
        std::os::unix::fs::symlink(home.join("b.toml"), home.join("a.toml")).unwrap();
        std::os::unix::fs::symlink(home.join("a.toml"), home.join("b.toml")).unwrap();
        std::fs::write(home.join("real.toml"), "").unwrap();
        let chain = |len: usize, prefix: &str| {
            let mut next = home.join("real.toml");
            for i in 0..len {
                let link = home.join(format!("{prefix}{i}.toml"));
                std::os::unix::fs::symlink(&next, &link).unwrap();
                next = link;
            }
            next
        };
        let long = chain(41, "long-");
        let at_cap = chain(40, "cap-");

        for path in [
            home.join("loop-dir").join("config.toml"),
            home.join("a.toml"),
            long,
        ] {
            let refused = folder_others_can_change(&path)
                .expect_err("a loop or an over-long chain is refused");
            assert!(refused.contains("links"), "{refused}");
            assert!(refused.contains(&path.display().to_string()), "{refused}");
        }
        folder_others_can_change(&at_cap).expect("a chain of 40 links is followed");
    }

    /// Issue #2399 final audit A1: the strict writer's staging file is
    /// owner-only from the exclusive open that creates it, not made so by a
    /// later `chmod` — another user who opened it before that `chmod` would
    /// keep a descriptor that reads the config written into it. An existing
    /// config then ends on its own vetted mode (here `0600` and `0644`), and a
    /// config the strict writer creates is published `0600`, whatever the
    /// umask would have given it.
    #[cfg(unix)]
    #[test]
    fn the_strict_writers_staging_file_is_owner_only_when_it_is_created() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = crate::test_temp::tempdir().unwrap();

        // Under a umask that strips group and other bits, every new file is
        // owner-only and this test would pass without the creation mode.
        let reference = dir.path().join("reference.toml");
        std::fs::write(&reference, "").unwrap();
        let reference_mode = std::fs::metadata(&reference).unwrap().permissions().mode() & 0o777;
        std::fs::remove_file(&reference).unwrap();
        if reference_mode & 0o077 == 0 {
            eprintln!(
                "SKIP: the umask already creates files as {reference_mode:o}, so a staging \
                 file would be owner-only without its creation mode and this test proves \
                 nothing here"
            );
            return;
        }

        let staged_mode = |path: &Path, expected: Option<FileStamp>| {
            let seen = std::rc::Rc::new(std::cell::Cell::new(None));
            let into = seen.clone();
            STAGED.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move |file: &std::fs::File| {
                    into.set(Some(file.metadata().unwrap().permissions().mode() & 0o777));
                }));
            });
            write_config_strictly(path, path, b"default_command = \"new\"\n", expected, || {
                Ok(())
            })
            .unwrap();
            seen.get().expect("the strict writer staged the file")
        };
        let stamp = |p: &Path| Some(FileStamp::of(&std::fs::metadata(p).unwrap()));
        let mode_of = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;

        for existing in [0o600, 0o644] {
            let path = dir.path().join(format!("config-{existing:o}.toml"));
            std::fs::write(&path, "default_command = \"old\"\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(existing)).unwrap();
            let created = staged_mode(&path, stamp(&path));
            assert_eq!(
                created, 0o600,
                "a {existing:o} config's staging file was created {created:o}"
            );
            assert_eq!(mode_of(&path), existing, "the config keeps its own mode");
        }

        let fresh = dir.path().join("new.toml");
        let created = staged_mode(&fresh, None);
        assert_eq!(
            created, 0o600,
            "a new config's staging file was created {created:o}"
        );
        assert_eq!(
            mode_of(&fresh),
            0o600,
            "a config the editor creates is private"
        );
        assert!(temp_files_in(dir.path()).is_empty());
    }

    /// Issue #2137: `std::fs::write` saved a config whose file name is close to
    /// the filesystem's 255-byte limit (`DOT_AGENT_DECK_CONFIG` can name any
    /// file). The temp file beside it must not be named after it, or its name
    /// runs past the limit (ENAMETOOLONG) and the save fails.
    #[cfg(unix)]
    #[test]
    fn a_config_with_a_250_byte_file_name_still_saves() {
        use std::os::unix::ffi::OsStrExt as _;

        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join(format!("{}.toml", "c".repeat(245)));
        assert_eq!(path.file_name().unwrap().as_bytes().len(), 250);
        std::fs::write(&path, "default_command = \"old\"\n").unwrap();

        // The old temp name was `.<name>.<pid>.<seq>.tmp`; this only goes red
        // where that cannot fit, so say so where the limit is longer.
        let c_dir = std::ffi::CString::new(dir.path().as_os_str().as_bytes()).unwrap();
        // SAFETY: `c_dir` is a valid NUL-terminated string that outlives the
        // call, and `pathconf` only reads through it.
        let name_max = unsafe { libc::pathconf(c_dir.as_ptr(), libc::_PC_NAME_MAX) };
        if name_max < 0 || name_max as usize >= 250 + ".x.1.0.tmp".len() {
            eprintln!(
                "SKIP: {} allows {name_max}-byte names, so a temp name built from \
                 a 250-byte config name would fit and this test proves nothing here",
                dir.path().display()
            );
            return;
        }

        let config = DashboardConfig {
            default_command: "new".to_string(),
            ..DashboardConfig::default()
        };
        config.save_to(&path).unwrap();

        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("default_command = \"new\"")
        );
        assert!(temp_files_in(dir.path()).is_empty());
    }

    /// Issue #2137: the temp file that stages a `0600` config's new contents
    /// must never be readable by anyone else, not even in the moment between
    /// its creation and the `set_permissions` that copies the target's mode:
    /// in a folder others can search, another user could open it in that
    /// moment and keep the descriptor while the secret is written. So the
    /// file must come out of `open` with no group or other bits.
    #[cfg(unix)]
    #[test]
    fn the_temp_file_for_a_0600_config_is_created_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = crate::test_temp::tempdir().unwrap();

        // Under a umask that already strips group and other bits (077), any
        // new file comes out owner-only and this test would pass without the
        // creation mode, so say so instead of passing silently.
        let reference = dir.path().join("reference.toml");
        std::fs::write(&reference, "").unwrap();
        let reference_mode = std::fs::metadata(&reference).unwrap().permissions().mode() & 0o777;
        if reference_mode & 0o077 == 0 {
            eprintln!(
                "SKIP: the umask already creates files as {reference_mode:o}, so a temp \
                 file would be owner-only without its creation mode and this test \
                 proves nothing here"
            );
            return;
        }

        let path = dir.path().join("config.toml");
        std::fs::write(&path, "default_command = \"old\"\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let perms = std::fs::metadata(&path).unwrap().permissions();

        let (file, tmp) = create_temp_beside(&path, Some(&perms)).unwrap();
        let mode = file.metadata().unwrap().permissions().mode();
        drop(file);
        std::fs::remove_file(&tmp).unwrap();

        assert_eq!(
            mode & 0o077,
            0,
            "temp file created with mode {:o}, readable beyond its 0600 target",
            mode & 0o777
        );
    }

    /// Issue #2137: a new config gets what `std::fs::write` gave one, and an
    /// existing one keeps its own mode — the creation mode only narrows the
    /// window, the save still ends on the target's exact bits.
    #[cfg(unix)]
    #[test]
    fn a_config_save_ends_on_the_target_mode_and_a_new_config_gets_the_default() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = crate::test_temp::tempdir().unwrap();
        let config = DashboardConfig::default();

        let reference = dir.path().join("reference.toml");
        std::fs::write(&reference, "").unwrap();
        let reference_mode = std::fs::metadata(&reference).unwrap().permissions().mode() & 0o777;

        let existing = dir.path().join("existing.toml");
        std::fs::write(&existing, "").unwrap();
        std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o600)).unwrap();
        config.save_to(&existing).unwrap();
        assert_eq!(
            std::fs::metadata(&existing).unwrap().permissions().mode() & 0o777,
            0o600
        );

        // A mode the umask strips: the temp file is created narrower than
        // the target, so only the copy of the exact mode puts it back.
        if reference_mode == 0o666 {
            eprintln!(
                "SKIP: the umask strips nothing from 0666, so a 0666 config would \
                 keep its mode without the mode copy and that case proves nothing here"
            );
        } else {
            let open = dir.path().join("open.toml");
            std::fs::write(&open, "").unwrap();
            std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o666)).unwrap();
            config.save_to(&open).unwrap();
            assert_eq!(
                std::fs::metadata(&open).unwrap().permissions().mode() & 0o777,
                0o666,
                "a 0666 config must keep 0666 even though the umask narrows the temp file"
            );
        }

        let fresh = dir.path().join("fresh.toml");
        config.save_to(&fresh).unwrap();
        assert_eq!(
            std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777,
            std::fs::metadata(&reference).unwrap().permissions().mode() & 0o777,
            "a new config must get the mode std::fs::write gives a new file"
        );
    }

    /// Issue #2137 (PR #2142 review): the swap is used only when it leaves the
    /// config owned as before and with no other name. `replacement` is the
    /// temp file the save made, whose owner is what the renamed config gets.
    /// A different owner needs root to produce on a real file, so the decision
    /// is tested on its inputs.
    #[cfg(unix)]
    #[test]
    fn the_swap_is_used_only_when_it_keeps_the_owner_and_the_links() {
        let me = FileOwnership {
            uid: 1000,
            gid: 1000,
            nlink: 1,
        };
        let root = FileOwnership {
            uid: 0,
            gid: 0,
            nlink: 1,
        };
        let cases = [
            ("missing target", None, me, true),
            ("same owner, one link", Some(me), me, true),
            (
                "uid differs",
                Some(FileOwnership { uid: 0, ..me }),
                me,
                false,
            ),
            (
                "gid differs",
                Some(FileOwnership { gid: 20, ..me }),
                me,
                false,
            ),
            (
                "two links",
                Some(FileOwnership { nlink: 2, ..me }),
                me,
                false,
            ),
            ("root saving the user's file", Some(me), root, false),
            ("root saving root's file", Some(root), root, true),
        ];
        for (name, target, replacement, want) in cases {
            assert_eq!(
                swap_keeps_owner_and_links(target, replacement),
                want,
                "{name}"
            );
        }
    }

    /// Issue #2137 (PR #2142 review of 85f6a8c9): the owner/group check is wired
    /// to the real target, not only correct on its inputs. Scenario: give a
    /// config one of this user's supplementary groups, save it, and expect the
    /// same inode, the group kept and the new content written in place. A
    /// different owner (uid) needs root to produce and is not tested here.
    #[cfg(unix)]
    #[test]
    fn a_config_save_keeps_a_config_whose_group_was_changed() {
        use std::os::unix::fs::MetadataExt as _;

        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "default_command = \"old\"\n").unwrap();
        let before = std::fs::metadata(&path).unwrap();

        // SAFETY: `getgroups(0, NULL)` only returns the count, and the second
        // call writes at most `groups.len()` entries into the buffer.
        let mut groups: Vec<libc::gid_t> =
            vec![0; unsafe { libc::getgroups(0, std::ptr::null_mut()) }.max(0) as usize];
        let n = unsafe { libc::getgroups(groups.len() as libc::c_int, groups.as_mut_ptr()) };
        groups.truncate(n.max(0) as usize);
        // A usable group is one `chown` accepts: listed is not always enough
        // (macOS lists groups it will not hand a file to).
        let Some(gid) = groups
            .into_iter()
            .filter(|&g| g != before.gid())
            .find(|&g| std::os::unix::fs::chown(&path, None, Some(g)).is_ok())
        else {
            eprintln!(
                "SKIP: this user has no supplementary group other than the file's \
                 that chown accepts, so a config with a changed group cannot be made here"
            );
            return;
        };
        assert_eq!(std::fs::metadata(&path).unwrap().gid(), gid);

        let config = DashboardConfig {
            default_command: "new".to_string(),
            ..DashboardConfig::default()
        };
        config.save_to(&path).unwrap();

        let after = std::fs::metadata(&path).unwrap();
        assert_eq!(
            after.ino(),
            before.ino(),
            "a config whose group the swap would change must be saved in place"
        );
        assert_eq!(after.gid(), gid, "the config must keep its group");
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("default_command = \"new\"")
        );
        assert!(temp_files_in(dir.path()).is_empty());
    }

    /// Issue #2137 (PR #2142 review): `std::fs::write` wrote into the file, so
    /// every hard link to `config.toml` saw the new content. A rename would
    /// give `config.toml` a new inode and leave the other name on the old one,
    /// so a config with more than one link must be saved in place.
    #[cfg(unix)]
    #[test]
    fn a_config_save_keeps_a_hard_linked_config_one_file() {
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let other = dir.path().join("dotfiles-config.toml");
        std::fs::write(&path, "default_command = \"old\"\n").unwrap();
        std::fs::hard_link(&path, &other).unwrap();

        let config = DashboardConfig {
            default_command: "new".to_string(),
            ..DashboardConfig::default()
        };
        config.save_to(&path).unwrap();

        for name in [&path, &other] {
            assert!(
                std::fs::read_to_string(name)
                    .unwrap()
                    .contains("default_command = \"new\""),
                "{} must show the new content",
                name.display()
            );
        }
        assert!(temp_files_in(dir.path()).is_empty());
    }

    /// Issue #2137: which errors send the save to the old in-place write.
    /// EBUSY (a single-file bind mount) and EXDEV cannot be produced here
    /// without root, so the decision is tested on the errno itself.
    #[cfg(unix)]
    #[test]
    fn only_a_place_that_cannot_take_the_swap_falls_back_to_the_in_place_write() {
        use AtomicSaveStep::{CreateTemp, Rename};
        let errno = std::io::Error::from_raw_os_error;

        for (step, code) in [
            (CreateTemp, libc::EACCES),
            (CreateTemp, libc::EPERM),
            (CreateTemp, libc::EROFS),
            (Rename, libc::EBUSY),
            (Rename, libc::EXDEV),
            (Rename, libc::EPERM),
            (Rename, libc::EACCES),
        ] {
            assert!(
                falls_back_to_in_place_write(step, &errno(code)),
                "{step:?} errno {code} must fall back"
            );
        }
        for (step, code) in [
            (CreateTemp, libc::ENOSPC),
            (CreateTemp, libc::EDQUOT),
            (CreateTemp, libc::EIO),
            (CreateTemp, libc::ENOENT),
            (CreateTemp, libc::EBUSY),
            (Rename, libc::ENOSPC),
            (Rename, libc::EIO),
            (Rename, libc::ENOENT),
            (Rename, libc::EISDIR),
            (Rename, libc::EROFS),
        ] {
            assert!(
                !falls_back_to_in_place_write(step, &errno(code)),
                "{step:?} errno {code} must fail, not fall back"
            );
        }
    }

    /// PRD #1258 (PR #1672 review): the reporting read hands back every warning
    /// the plain bounded read would log — so the host sampler, which reads on
    /// every sample, can log each one once — and still keeps the rest of a file
    /// whose `host_metrics` is not a table.
    #[test]
    fn a_reporting_bounded_load_returns_its_warnings_instead_of_logging_them() {
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let (config, warnings) = DashboardConfig::load_bounded_reporting_from(&path);
        assert_eq!((config.default_command.as_str(), warnings.len()), ("", 0));

        std::fs::write(
            &path,
            "default_command = \"claude\"\n[host_metrics]\nload_per_core_warn = 1\n",
        )
        .unwrap();
        let (config, warnings) = DashboardConfig::load_bounded_reporting_from(&path);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(config.default_command, "claude");
        assert!(config.host_metrics.contains_key("load_per_core_warn"));

        std::fs::write(&path, "default_command = \"claude\"\nhost_metrics = 5\n").unwrap();
        let (config, warnings) = DashboardConfig::load_bounded_reporting_from(&path);
        assert_eq!(
            config.default_command, "claude",
            "the rest of the file survives"
        );
        assert!(config.host_metrics.is_empty());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("not a table"), "{warnings:?}");

        for (contents, says) in [
            ("default_command = [unclosed\n", "malformed TOML"),
            ("bell = 5\n", "wrong type"),
        ] {
            std::fs::write(&path, contents).unwrap();
            let (config, warnings) = DashboardConfig::load_bounded_reporting_from(&path);
            assert_eq!(config.default_command, "", "{contents}: the defaults");
            assert_eq!(warnings.len(), 1, "{contents}: {warnings:?}");
            assert!(warnings[0].contains(says), "{contents}: {warnings:?}");
            assert!(
                !warnings[0].contains("unclosed"),
                "no file content is quoted"
            );
        }

        let (_, warnings) = DashboardConfig::load_bounded_reporting_from(dir.path());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("not read"), "{warnings:?}");
    }

    /// PRD #1223 audit A4: the daemon's bounded read of `config.toml` answers
    /// what `load` answers for an ordinary file — including one reached through
    /// a symlink — and falls back to the defaults for no file at all.
    #[test]
    fn a_bounded_load_reads_an_ordinary_config_and_follows_a_link_to_one() {
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert_eq!(
            DashboardConfig::load_bounded_from(&path).default_command,
            "",
            "no file is the defaults"
        );

        std::fs::write(&path, "default_command = \"opencode --model x\"\n").unwrap();
        assert_eq!(
            DashboardConfig::load_bounded_from(&path).default_command,
            "opencode --model x"
        );

        #[cfg(unix)]
        {
            let link = dir.path().join("linked.toml");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            assert_eq!(
                DashboardConfig::load_bounded_from(&link).default_command,
                "opencode --model x",
                "a dotfiles-style link to a regular config is followed, as `load` follows it"
            );
        }

        std::fs::write(&path, "default_command = [\n").unwrap();
        assert_eq!(
            DashboardConfig::load_bounded_from(&path).default_command,
            "",
            "a malformed config is the defaults"
        );
    }

    /// Audit A4: a config over [`MAX_DASHBOARD_CONFIG_BYTES`] is not read into
    /// memory; the daemon answers with the defaults instead.
    #[test]
    fn a_bounded_load_refuses_an_oversized_config() {
        assert_eq!(
            MAX_DASHBOARD_CONFIG_BYTES,
            crate::project_resolve::MAX_PROJECT_CONFIG_BYTES
        );
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut body = String::from("default_command = \"claude\"\n#");
        body.push_str(&"x".repeat(MAX_DASHBOARD_CONFIG_BYTES as usize));
        std::fs::write(&path, body).unwrap();
        assert_eq!(
            DashboardConfig::load_bounded_from(&path).default_command,
            ""
        );
    }

    /// Audit A4: a FIFO at the config path — directly or behind a link — is
    /// refused without blocking in the open, where `load`'s plain read would
    /// wait for a writer forever.
    #[cfg(unix)]
    #[test]
    fn a_bounded_load_refuses_a_fifo_without_blocking() {
        let dir = crate::test_temp::tempdir().unwrap();
        let fifo = dir.path().join("config.toml");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).expect("cstring");
        // SAFETY: `c_path` is a valid NUL-terminated string that outlives the
        // call, and `mkfifo` only reads through it.
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo failed: {}", std::io::Error::last_os_error());
        let link = dir.path().join("linked.toml");
        std::os::unix::fs::symlink(&fifo, &link).unwrap();

        for path in [fifo, link] {
            let (tx, rx) = std::sync::mpsc::channel();
            let probe = path.clone();
            std::thread::spawn(move || {
                let _ = tx.send(DashboardConfig::load_bounded_from(&probe).default_command);
            });
            assert_eq!(
                rx.recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap_or_else(|_| panic!("{}: the bounded load blocked", path.display())),
                ""
            );
        }
    }

    /// Issue #2137 N2 review: once the open of `config.toml` fails with
    /// `NotFound`, only an `lstat` that also says `NotFound` is an absent
    /// config. Anything else at the path by then — a dangling link, or a file,
    /// folder or link that appeared between the two calls — or an `lstat` that
    /// fails another way is a failed read, so a race between the two calls
    /// holds the doorbell off rather than reading a new file as absent.
    #[cfg(unix)]
    #[test]
    fn a_config_open_that_finds_nothing_is_absent_only_when_lstat_agrees() {
        use std::io::{Error, ErrorKind};
        let dir = crate::test_temp::tempdir().unwrap();
        let file = dir.path().join("file.toml");
        std::fs::write(&file, "").unwrap();
        let folder = dir.path().join("folder");
        std::fs::create_dir(&folder).unwrap();
        let link = dir.path().join("link.toml");
        std::os::unix::fs::symlink(dir.path().join("missing.toml"), &link).unwrap();
        let kind = |p: &Path| Ok(std::fs::symlink_metadata(p).unwrap().file_type());

        assert_eq!(
            after_open_not_found(Err(Error::from(ErrorKind::NotFound))),
            Ok(None),
            "lstat NotFound: nothing at the path"
        );
        let symlink = after_open_not_found(kind(&link));
        assert_eq!(
            symlink,
            Err("it is a link whose target is missing".to_string())
        );
        for (name, lstat) in [
            ("regular file", kind(&file)),
            ("folder", kind(&folder)),
            (
                "lstat denied",
                Err(Error::from(ErrorKind::PermissionDenied)),
            ),
            ("lstat other error", Err(Error::other("io error"))),
        ] {
            let verdict = after_open_not_found(lstat);
            assert!(verdict.is_err(), "{name}: {verdict:?}");
        }
    }

    /// Issue #2156: the nearest folder above a config path that is not there
    /// decides whether the config is absent. Only a folder, or a link that
    /// leads to one, is a first run; a link whose target is missing, a link to
    /// a file, a file, and a call that fails another way each fail the read,
    /// so a race or an unreadable folder holds the doorbell off rather than
    /// reading as no config.
    #[cfg(unix)]
    #[test]
    fn a_missing_config_folder_is_absent_only_below_a_real_folder() {
        use std::io::{Error, ErrorKind};
        let dir = crate::test_temp::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, "").unwrap();
        let folder = dir.path().join("folder");
        std::fs::create_dir(&folder).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&folder, &link).unwrap();
        let lstat = |p: &Path| Ok(std::fs::symlink_metadata(p).unwrap().file_type());
        let stat = |p: &Path| {
            let p = p.to_path_buf();
            move || std::fs::metadata(&p).map(|m| m.file_type())
        };
        let never = || -> std::io::Result<std::fs::FileType> { panic!("stat of a non-link") };
        let at = Path::new("/somewhere");

        assert_eq!(
            nearest_config_folder_verdict(at, lstat(&folder), never),
            Ok(None)
        );
        assert_eq!(
            nearest_config_folder_verdict(at, lstat(&link), stat(&folder)),
            Ok(None),
            "a link to a folder"
        );
        assert_eq!(
            nearest_config_folder_verdict(at, lstat(&link), || Err(Error::from(
                ErrorKind::NotFound
            ))),
            Err("the folder /somewhere is a link whose target is missing".to_string())
        );
        for (name, verdict) in [
            (
                "link to a file",
                nearest_config_folder_verdict(at, lstat(&link), stat(&file)),
            ),
            (
                "link, stat denied",
                nearest_config_folder_verdict(at, lstat(&link), || {
                    Err(Error::from(ErrorKind::PermissionDenied))
                }),
            ),
            (
                "a file",
                nearest_config_folder_verdict(at, lstat(&file), never),
            ),
            (
                "lstat denied",
                nearest_config_folder_verdict(
                    at,
                    Err(Error::from(ErrorKind::PermissionDenied)),
                    never,
                ),
            ),
        ] {
            assert!(verdict.is_err(), "{name}: {verdict:?}");
        }

        // The walk itself: nothing above a relative path's missing folders
        // but the working directory, which is a first run.
        assert_eq!(
            missing_config_folder_verdict(Path::new("no-such-folder/config.toml")),
            Ok(None)
        );
    }

    /// PRD #2374 (review finding): `config set` of an unrelated key keeps the
    /// agent cards' `[catalog]` table, any other table this build does not
    /// know, and the user's comments; a key the struct owns is rewritten, and
    /// one it now leaves out (back at its default) is removed.
    #[test]
    fn a_config_save_keeps_the_catalog_and_other_tables_it_does_not_own() {
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "\
# my deck
default_command = \"claude\" # the usual

# about bells
[bell]
on_idle = true

[doorbell]
claude = false

[catalog]
remove = [\"merge\"] # not this one

[[catalog.fields]]
name = \"risk\"
description = \"How risky the change is.\"
type = \"text\"

[[catalog.actions]]
name = \"ship\"
description = \"Offer when it is ready.\"
prompt = \"Ship it.\"

[from_a_newer_build]
answer = 42
";
        std::fs::write(&path, original).unwrap();
        let mut config: DashboardConfig = toml::from_str(original).unwrap();
        config.set_field("bell.enabled", "false").unwrap();
        config.set_field("doorbell.claude", "true").unwrap();
        config.save_to(&path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            saved.starts_with("# my deck\ndefault_command = \"claude\" # the usual\n"),
            "{saved}"
        );
        assert!(saved.contains("# about bells\n[bell]\n"), "{saved}");
        assert!(
            saved.contains("remove = [\"merge\"] # not this one"),
            "{saved}"
        );
        let table: toml::Table = toml::from_str(&saved).unwrap();
        let before: toml::Table = toml::from_str(original).unwrap();
        assert_eq!(table["catalog"], before["catalog"], "{saved}");
        assert_eq!(
            table["from_a_newer_build"], before["from_a_newer_build"],
            "{saved}"
        );
        assert_eq!(table["bell"]["enabled"].as_bool(), Some(false), "{saved}");
        assert_eq!(table["bell"]["on_idle"].as_bool(), Some(true), "{saved}");
        assert!(
            !table.contains_key("doorbell"),
            "doorbell is back at its default, so the save drops it: {saved}"
        );
        let reread: DashboardConfig = toml::from_str(&saved).unwrap();
        assert!(!reread.bell.enabled && reread.doorbell.claude);
        assert_eq!(reread.default_command, "claude");

        // The catalog the daemon reads from that file is the user's, not the
        // defaults.
        let catalog = crate::card_catalog::resolve(Some(&saved));
        assert!(catalog.errors.is_empty(), "{:?}", catalog.errors);
        assert!(catalog.fields.iter().any(|f| f.name == "risk"));
        assert!(catalog.actions.iter().any(|a| a.name == "ship"));
        assert!(!catalog.actions.iter().any(|a| a.name == "merge"));
    }

    /// PRD #2374: a config that is not valid TOML is refused rather than
    /// replaced, so a typo never costs the user their catalog or any other
    /// setting the deck could not read.
    #[test]
    fn a_config_save_refuses_to_replace_a_file_that_is_not_toml() {
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "default_command = \n[catalog]\nremove = [\"merge\"]\n";
        std::fs::write(&path, original).unwrap();
        let err = DashboardConfig::default().save_to(&path).unwrap_err();
        assert!(err.starts_with("Not saving config at "), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    /// [`DASHBOARD_KEYS`] names every field of [`DashboardConfig`]: the
    /// destructuring fails to compile when a field is added, and the
    /// serialization of a config with every field away from its default
    /// writes exactly the listed keys.
    #[test]
    fn dashboard_keys_name_every_field() {
        let mut config = DashboardConfig::default();
        let DashboardConfig {
            default_command,
            default_dir,
            bell,
            doorbell,
            notifications,
            auto_config_prompt,
            host_metrics,
        } = &mut config;
        *default_command = "x".into();
        *default_dir = "/x".into();
        bell.enabled = !bell.enabled;
        doorbell.claude = !doorbell.claude;
        notifications.question_after_minutes += 1;
        *auto_config_prompt = !*auto_config_prompt;
        host_metrics.insert("disk_warn_percent".into(), toml::Value::Integer(80));
        let written: toml::Table = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
        let mut keys: Vec<&str> = written.keys().map(String::as_str).collect();
        keys.sort_unstable();
        let mut listed = DASHBOARD_KEYS.to_vec();
        listed.sort_unstable();
        assert_eq!(keys, listed);
    }

    /// Issue #2156 (what `docs/configuration.md` says of it): `config set`
    /// through a config folder that is a link whose target is missing fails,
    /// and creates nothing at the link or where it points.
    #[cfg(unix)]
    #[test]
    fn a_config_save_through_a_dangling_config_folder_fails() {
        let dir = crate::test_temp::tempdir().unwrap();
        let target = dir.path().join("unmounted/dot-agent-deck");
        let folder = dir.path().join("dot-agent-deck");
        std::os::unix::fs::symlink(&target, &folder).unwrap();
        let result = DashboardConfig::default().save_to(&folder.join("config.toml"));
        assert!(result.is_err(), "{result:?}");
        assert!(
            std::fs::symlink_metadata(&folder)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!dir.path().join("unmounted").exists());
    }

    /// Issue #2156 item 2: `config get` (and every other [`DashboardConfig::load`]
    /// caller) says a config it cannot read through a link whose target is
    /// missing — at `config.toml` or at a folder above it — rather than
    /// printing the default silently while the daemon's bounded read holds the
    /// doorbell off. Both readers agree on every case: a missing file in a
    /// real folder, and a missing folder below a real one (a first run), are
    /// absent to both and warned of by neither.
    #[cfg(unix)]
    #[test]
    fn a_config_load_says_a_dangling_link_cannot_be_read_as_the_daemon_does() {
        let dir = crate::test_temp::tempdir().unwrap();
        let file_link = dir.path().join("file-link.toml");
        std::os::unix::fs::symlink(dir.path().join("gone/config.toml"), &file_link).unwrap();
        let folder = dir.path().join("folder-link");
        std::os::unix::fs::symlink(dir.path().join("unmounted"), &folder).unwrap();
        let through_folder = folder.join("config.toml");

        for (path, reason) in [
            (
                file_link.clone(),
                "it is a link whose target is missing".to_string(),
            ),
            (
                through_folder.clone(),
                format!(
                    "the folder {} is a link whose target is missing",
                    folder.display()
                ),
            ),
        ] {
            let (config, warning) = DashboardConfig::load_reporting_from(&path);
            assert_eq!(
                warning,
                Some(format!(
                    "Failed to read config at {}: {reason}",
                    path.display()
                ))
            );
            assert_eq!(config.get_field("doorbell.claude").unwrap(), "true");
            assert_eq!(
                DashboardConfig::load_bounded_status_from(&path).file,
                ConfigFile::Failed,
                "{}",
                path.display()
            );
        }

        for absent in [
            dir.path().join("config.toml"),
            dir.path().join("first-run/dot-agent-deck/config.toml"),
        ] {
            assert_eq!(
                DashboardConfig::load_reporting_from(&absent).1,
                None,
                "{}",
                absent.display()
            );
            assert_eq!(
                DashboardConfig::load_bounded_status_from(&absent).file,
                ConfigFile::Absent
            );
        }

        // A file that does not parse is still reported as before.
        let invalid = dir.path().join("invalid.toml");
        std::fs::write(&invalid, "not = = toml").unwrap();
        let warning = DashboardConfig::load_reporting_from(&invalid).1.unwrap();
        assert!(warning.starts_with("Invalid config at "), "{warning}");
    }

    /// Issue #2156 item 3, the gap as it stands: a mount point whose volume
    /// is not mounted is a real, empty folder, which is what a test without
    /// root can stage. Both readers take it for a first run and `config set`
    /// writes the defaults plus the key into it, where the mounted volume
    /// will hide it. Comparing the folder's device with its parent's does not
    /// tell it apart: it is the same device while nothing is mounted, as for
    /// any ordinary folder; only a mounted volume shows a different one.
    #[cfg(unix)]
    #[test]
    fn an_unmounted_mount_point_reads_and_saves_as_a_first_run() {
        use std::os::unix::fs::MetadataExt as _;
        let dir = crate::test_temp::tempdir().unwrap();
        let mount_point = dir.path().join("dot-agent-deck");
        std::fs::create_dir(&mount_point).unwrap();
        let path = mount_point.join("config.toml");

        assert_eq!(DashboardConfig::load_reporting_from(&path).1, None);
        assert_eq!(
            DashboardConfig::load_bounded_status_from(&path).file,
            ConfigFile::Absent
        );
        assert_eq!(
            std::fs::metadata(&mount_point).unwrap().dev(),
            std::fs::metadata(dir.path()).unwrap().dev(),
            "an unmounted mount point is on its parent's device"
        );

        let mut config = DashboardConfig::default();
        config.set_field("doorbell.claude", "false").unwrap();
        config.save_to(&path).unwrap();
        let written = DashboardConfig::load_reporting_from(&path).0;
        assert_eq!(written.get_field("doorbell.claude").unwrap(), "false");
        assert_eq!(written.get_field("doorbell.codex").unwrap(), "true");
    }

    /// The pure trust rule the ancestor walk stops on (issue #577). Split out
    /// of the filesystem check so the deletion-unsafe direction — adopting an
    /// ancestor config we do not own — is exercised without needing a
    /// foreign-owned file or root, mirroring how
    /// `platform::fsperm::endpoint_owner_is_trusted` is tested. Ownership is
    /// the check that matters when walking upward: `/tmp/project` sits under a
    /// world-writable `/tmp` where any local user can drop a
    /// `/tmp/.dot-agent-deck.toml`, and an attacker cannot create a file owned
    /// by us.
    #[test]
    fn ancestor_config_is_trusted_only_when_we_own_it() {
        assert!(
            config_owner_is_trusted(1000, 1000),
            "our own file is trusted"
        );
        assert!(
            !config_owner_is_trusted(0, 1000),
            "a root-owned config planted above us is not adopted"
        );
        assert!(
            !config_owner_is_trusted(1001, 1000),
            "another local user's config planted above us is not adopted"
        );
    }

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

        // PRD #89 review-fix G2: assert the EXACT write count, not `<= 2` / `>= 1`.
        // For this scenario the count is fully determined — one leading-edge write
        // on the first change plus one trailing write after the interval elapses =
        // 2 — so any off-by-one in the coalescer (an extra leading write, a missed
        // trailing flush) flips this count and fails the test.
        assert_eq!(
            writes, 2,
            "a 50-change burst must coalesce to exactly two writes \
             (leading edge + one trailing flush), got {writes}"
        );

        // After the trailing flush nothing is pending, so no further write is
        // due no matter how far the clock advances.
        assert!(
            !coalescer.is_due(after + interval + interval),
            "no write is due once the burst has been flushed and nothing new is dirty"
        );
    }

    /// The bell's defaults (ring on waiting-for-input and on error, not on
    /// idle) hold for a built config, an empty table and a partial one, and a
    /// key that is present overrides only its own default.
    #[test]
    fn bell_config_defaults_hold_when_built_parsed_empty_or_partial() {
        let cases = [
            ("built", BellConfig::default(), false),
            ("empty", toml::from_str("").unwrap(), false),
            ("partial", toml::from_str("on_idle = true").unwrap(), true),
        ];
        for (name, bc, on_idle) in cases {
            assert!(bc.enabled, "{name}");
            assert!(bc.on_waiting_for_input, "{name}");
            assert_eq!(bc.on_idle, on_idle, "{name}");
            assert!(bc.on_error, "{name}");
        }
    }

    /// `auto_config_prompt` and the `[bell]` table default when a config omits
    /// them, and a config that sets them is read over the defaults, leaving the
    /// keys it does not set at theirs.
    #[test]
    fn dashboard_config_reads_auto_config_prompt_and_bell_over_their_defaults() {
        let built = DashboardConfig::default();
        assert!(built.auto_config_prompt);

        let empty: DashboardConfig = toml::from_str("").unwrap();
        assert!(empty.auto_config_prompt);

        let off: DashboardConfig = toml::from_str("auto_config_prompt = false").unwrap();
        assert!(!off.auto_config_prompt);

        let no_bell: DashboardConfig = toml::from_str(r#"default_command = "echo hi""#).unwrap();
        assert_eq!(no_bell.default_command, "echo hi");
        assert!(no_bell.bell.enabled);

        let with_bell: DashboardConfig = toml::from_str(
            r#"
default_command = "test"

[bell]
enabled = false
on_idle = true
"#,
        )
        .unwrap();
        assert!(!with_bell.bell.enabled);
        assert!(with_bell.bell.on_idle);
        assert!(with_bell.bell.on_waiting_for_input);
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
            last_command: None,
            focus: None,
        };
        let toml_str = toml::to_string_pretty(&session).unwrap();
        let loaded: SavedSession = toml::from_str(&toml_str).unwrap();
        assert_eq!(loaded.panes.len(), 2);
        assert_eq!(loaded.panes[0].dir, "/repo/api");
        assert_eq!(loaded.panes[0].name, "api");
        assert_eq!(loaded.panes[0].command, "claude");
        assert_eq!(loaded.panes[1].command, "");
    }

    /// PRD #196: the global `last_command` round-trips through serialize →
    /// deserialize, AND a session TOML written before the field existed (no
    /// `last_command` key) loads as `None` rather than failing — the
    /// `#[serde(default)]` forward-compat guarantee the form-seed fallback relies
    /// on (missing/unreadable → empty, never a hard failure).
    #[test]
    fn saved_session_last_command_round_trip_and_missing_key() {
        // (a) A set last_command round-trips intact.
        let session = SavedSession {
            panes: Vec::new(),
            last_command: Some("claude".to_string()),
            focus: None,
        };
        let toml_str = toml::to_string_pretty(&session).unwrap();
        let loaded: SavedSession = toml::from_str(&toml_str).unwrap();
        assert_eq!(loaded.last_command.as_deref(), Some("claude"));

        // (b) A legacy session.toml predating the field still parses, with
        // last_command == None (the #[serde(default)] guarantee).
        let legacy = r#"
[[panes]]
dir = "/repo/legacy"
name = "old-pane"
command = "vim"
"#;
        let legacy_loaded: SavedSession = toml::from_str(legacy).unwrap();
        assert_eq!(legacy_loaded.panes.len(), 1);
        assert!(
            legacy_loaded.last_command.is_none(),
            "a session.toml with no last_command key must load as None"
        );
    }

    /// Scenario: Round-trip a populated `[focus]` table through serialize →
    /// deserialize; parse a `session.toml` written before the field existed (no
    /// `[focus]` table at all) and assert it loads with `focus == None`, i.e.
    /// "nothing remembered"; and parse a PARTIAL `[focus]` table carrying only
    /// one key, asserting the whole file still parses — the saved panes survive
    /// — and that the missing keys default rather than aborting the parse. That
    /// third case is the one that matters most: `SavedSession::load` treats a
    /// TOML error as "no session at all", so a `[focus]` table a newer binary
    /// wrote with keys this one does not know must never cost the user every
    /// pane. Finally, check the OTHER compatibility direction — deserialize what
    /// this build writes into a struct carrying only the pre-issue-#949 fields,
    /// which must still recover the panes and `last_command`.
    #[spec("config/saved-session/002")]
    #[test]
    fn saved_session_002_focus_serde_round_trip_legacy_and_partial() {
        // (a) A populated position round-trips intact.
        let session = SavedSession {
            panes: Vec::new(),
            last_command: None,
            focus: Some(SavedFocus {
                version: 1,
                dashboard_active: true,
                active_pane: None,
                tab_panes: vec!["7".to_string(), "9".to_string()],
            }),
        };
        let toml_str = toml::to_string_pretty(&session).unwrap();
        let loaded: SavedSession = toml::from_str(&toml_str).unwrap();
        assert_eq!(loaded.focus, session.focus, "TOML: {toml_str}");

        // (b) A session.toml predating the field loads as "nothing remembered",
        // which leaves the deck's own landing choice alone.
        let legacy = r#"
[[panes]]
dir = "/repo/legacy"
name = "old-pane"
command = "vim"
"#;
        let legacy_loaded: SavedSession = toml::from_str(legacy).unwrap();
        assert_eq!(legacy_loaded.panes.len(), 1);
        assert!(
            legacy_loaded.focus.is_none(),
            "a session.toml with no [focus] table must load as None"
        );

        // (c) A PARTIAL table degrades to defaults for the absent keys and does
        // not take the panes down with it.
        let partial = r#"
[[panes]]
dir = "/repo/app"
name = "kept-pane"
command = "claude"

[focus]
active_pane = "12"
"#;
        let partial_loaded: SavedSession = toml::from_str(partial).unwrap();
        assert_eq!(
            partial_loaded.panes.len(),
            1,
            "a partial [focus] table must not fail the whole-file parse"
        );
        let focus = partial_loaded
            .focus
            .expect("a present [focus] table parses even when incomplete");
        assert_eq!(focus.active_pane.as_deref(), Some("12"));
        assert_eq!(focus.version, 0, "an absent version defaults");
        assert!(!focus.dashboard_active, "an absent flag defaults to false");
        assert!(
            focus.tab_panes.is_empty(),
            "an absent list defaults to empty"
        );

        // (d) The OTHER compatibility direction, and the one the struct's own
        // shape cannot demonstrate: an OLDER build reading a file THIS one
        // wrote. `SavedSession` sets no `deny_unknown_fields`, so an unknown
        // `[focus]` table is ignored rather than fatal — but that is exactly the
        // kind of claim worth checking rather than asserting, since `load()`
        // turns any TOML error into "no session at all" and would cost an older
        // build every pane. Modelled by a local struct with the pre-issue-#949
        // field set, deserialized from what the new writer emits.
        #[derive(Deserialize)]
        struct PreFocusSavedSession {
            #[serde(default)]
            panes: Vec<SavedPane>,
            #[serde(default)]
            last_command: Option<String>,
        }
        let new_writer_output = toml::to_string_pretty(&SavedSession {
            panes: vec![SavedPane {
                dir: "/repo/app".to_string(),
                name: "kept-pane".to_string(),
                command: "claude".to_string(),
                mode: None,
                orchestration: None,
            }],
            last_command: Some("claude".to_string()),
            focus: Some(SavedFocus {
                version: 1,
                dashboard_active: false,
                active_pane: Some("3".to_string()),
                tab_panes: vec!["3".to_string()],
            }),
        })
        .unwrap();
        let old_reader: PreFocusSavedSession = toml::from_str(&new_writer_output)
            .expect("an older build must still parse a session.toml carrying [focus]");
        assert_eq!(old_reader.panes.len(), 1, "TOML: {new_writer_output}");
        assert_eq!(old_reader.panes[0].name, "kept-pane");
        assert_eq!(old_reader.last_command.as_deref(), Some("claude"));
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
                    display_title: Some("My TDD Run".to_string()),
                }),
            }],
            last_command: None,
            focus: None,
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
        assert_eq!(orch.display_title.as_deref(), Some("My TDD Run"));

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

    /// Scenario: Build snapshots of a three-role orchestration and ask which
    /// roles a daemon-empty restore starts. An old (`version = 1`) snapshot,
    /// whose list was only ever `[start_role_index]`, starts every role however
    /// its list reads; a current one starts the roles it lists plus the start
    /// role, ignoring an index past the role count, and a current snapshot
    /// written by a newer deck under a higher version is read the same way.
    #[spec("config/saved-session/003")]
    #[test]
    fn saved_session_003_started_roles_are_honoured_only_from_a_current_snapshot() {
        let snapshot = |version: u32, start: usize, started: Vec<usize>| OrchestrationSnapshot {
            version,
            roles: vec!["orchestrator".into(), "coder".into(), "reviewer".into()],
            start_role_index: start,
            orchestrator_prompt: String::new(),
            config_name: "tdd-cycle".into(),
            project_path: "/repo/app".into(),
            started_role_indices: started,
            display_title: None,
        };

        // Old snapshots: every role, whatever the list says.
        for started in [vec![0], vec![0, 1], vec![]] {
            assert_eq!(
                snapshot(1, 0, started.clone()).roles_to_start_on_restore(3),
                vec![true, true, true],
                "a version-1 snapshot listing {started:?} must still start every role"
            );
        }
        assert_eq!(
            snapshot(0, 0, vec![0]).roles_to_start_on_restore(3),
            vec![true, true, true],
            "a malformed block that defaulted `version` to 0 is treated as old"
        );

        // Current snapshots: the listed roles.
        assert_eq!(ORCHESTRATION_SNAPSHOT_VERSION, 2);
        assert_eq!(
            snapshot(2, 0, vec![0, 1]).roles_to_start_on_restore(3),
            vec![true, true, false]
        );
        // The start role always starts — it is where the prompt goes and the
        // one pane that may delegate — even if the list leaves it out.
        assert_eq!(
            snapshot(2, 1, vec![2]).roles_to_start_on_restore(3),
            vec![false, true, true]
        );
        assert_eq!(
            snapshot(2, 0, vec![]).roles_to_start_on_restore(3),
            vec![true, false, false]
        );
        // An index past the roles is ignored rather than trusted.
        assert_eq!(
            snapshot(2, 0, vec![0, 7]).roles_to_start_on_restore(3),
            vec![true, false, false]
        );
        // A newer deck's snapshot is still an honest list.
        assert_eq!(
            snapshot(3, 0, vec![0, 2]).roles_to_start_on_restore(3),
            vec![true, false, true]
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
        // SAFETY: a stated residual, not a proof (issue #1516). This test starts
        // no runtime and no thread, so under nextest the threads that exist are
        // its own and libtest's runner thread, which waits for it. Under plain
        // `cargo test` it takes no lock and races any sibling test reading the
        // environment (issue #245).
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
            last_command: None,
            focus: None,
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
        assert!(bc.should_bell(&SessionStatus::Blocked));
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
            ..StarPromptState::default()
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
        // SAFETY: a stated residual, not a proof (issue #1516). This test starts
        // no runtime and no thread, so under nextest the threads that exist are
        // its own and libtest's runner thread, which waits for it. Under plain
        // `cargo test` it takes no lock and races any sibling test reading the
        // environment (issue #245).
        unsafe {
            std::env::set_var("DOT_AGENT_DECK_STAR_PROMPT", path.to_str().unwrap());
        }

        let state = StarPromptState {
            launch_count: 15,
            permanently_dismissed: false,
            last_prompt_at_launch: 10,
            ..StarPromptState::default()
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

    /// PRD #2174: a state written by a build before the shared reminder
    /// carries over unchanged — its count and its "Don't ask again" — and is
    /// saved back with the same three keys and nothing else.
    #[test]
    fn star_prompt_saved_state_from_an_older_build_carries_over() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("star-prompt-state.json");
        std::fs::write(
            &path,
            "{\n  \"launch_count\": 23,\n  \"permanently_dismissed\": true,\n  \"last_prompt_at_launch\": 20\n}",
        )
        .unwrap();

        let mut state = StarPromptState::load_from(&path);
        assert_eq!(state.launch_count, 23);
        assert!(state.permanently_dismissed);
        assert_eq!(state.last_prompt_at_launch, 20);
        assert!(!state.increment_and_check(), "a dismissed state never asks");

        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            saved,
            serde_json::json!({
                "launch_count": 24,
                "permanently_dismissed": true,
                "last_prompt_at_launch": 20
            })
        );
    }

    /// PRD #2174: a save replaces the file whole, through a sibling temporary
    /// file, and leaves no temporary file behind in the directory.
    #[test]
    fn star_prompt_save_replaces_the_file_and_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("star.json");
        let mut state = StarPromptState::load_from(&path);
        for _ in 0..3 {
            state.increment_and_check();
        }
        let mut names: Vec<String> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        // The state, and the empty lock sidecar that serialises the clients.
        assert_eq!(
            names,
            vec![".star.json.lock".to_string(), "star.json".to_string()]
        );
        assert_eq!(StarPromptState::load_from(&path).launch_count, 3);
    }

    /// PRD #211: a client's session over the notice state at `path`.
    fn notice_session(path: &Path) -> AntigravityNoticeSession {
        AntigravityNoticeSession::new(path.to_path_buf())
    }

    /// PRD #211: one start, by a client that has just started (a fresh
    /// session, which reads everything from the file).
    fn notice_start(path: &Path) -> AntigravityNoticeStart {
        notice_session(path).record_start()
    }

    /// PRD #211: the Antigravity notice shows on the first start, then on the
    /// hundredth start after the last showing, and the file records each.
    #[test]
    fn antigravity_notice_shows_first_then_every_hundredth_start() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notice.json");
        assert!(notice_start(&path).show, "first start");
        let (state, _) = AntigravityNoticeState::load_from(&path);
        assert_eq!(state.start_count, 1);
        assert_eq!(state.last_prompt_at_start, 1);
        for start in 2..=100 {
            assert!(!notice_start(&path).show, "start {start} showed");
        }
        assert!(
            notice_start(&path).show,
            "the hundredth start after the showing did not show"
        );
        let (state, _) = AntigravityNoticeState::load_from(&path);
        assert_eq!((state.start_count, state.last_prompt_at_start), (101, 101));
        assert!(!notice_start(&path).show);
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("\"last_prompt_at_start\": 101"), "{saved}");
    }

    /// PRD #211: Don't show again holds at every later start, however many.
    #[test]
    fn antigravity_notice_dont_show_again_is_permanent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notice.json");
        assert!(notice_start(&path).show);
        assert_eq!(notice_session(&path).dismiss_permanently(), None);
        for start in 2..=301 {
            assert!(!notice_start(&path).show, "start {start} showed");
        }
        let (state, _) = AntigravityNoticeState::load_from(&path);
        assert!(state.permanently_dismissed);
        assert_eq!(state.start_count, 301);
    }

    /// PRD #211: a missing file is a fresh state; missing fields default.
    #[test]
    fn antigravity_notice_missing_file_and_fields_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.json");
        assert_eq!(
            AntigravityNoticeState::load_from(&path),
            (AntigravityNoticeState::default(), None)
        );
        std::fs::write(&path, r#"{"permanently_dismissed":true}"#).unwrap();
        let (state, problem) = AntigravityNoticeState::load_from(&path);
        assert_eq!(problem, None);
        assert!(state.permanently_dismissed);
        assert_eq!((state.start_count, state.last_prompt_at_start), (0, 0));
        assert!(!notice_start(&path).show);
    }

    /// PRD #211: an unreadable file is reported, and the start still answers
    /// as a fresh state's first; the next save replaces the file.
    #[test]
    fn antigravity_notice_unreadable_file_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notice.json");
        std::fs::write(&path, "not json").unwrap();
        let start = notice_start(&path);
        assert!(start.show);
        let problem = start.problem.expect("the unreadable file is reported");
        assert!(
            problem.contains("Invalid Antigravity notice state"),
            "{problem}"
        );
        let (state, problem) = AntigravityNoticeState::load_from(&path);
        assert_eq!(problem, None, "the save replaced the unreadable file");
        assert_eq!(state.start_count, 1);
    }

    /// PRD #211: a file that cannot be saved is reported for the start and
    /// for Don't show again; the start still answers.
    #[test]
    fn antigravity_notice_unsaveable_file_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, "").unwrap();
        let path = blocker.join("notice.json");
        let start = notice_start(&path);
        assert!(start.show);
        assert!(start.problem.is_some());
        assert!(notice_session(&path).dismiss_permanently().is_some());
    }

    /// PRD #211: the two clients share the file: starts in either count
    /// towards the next showing, and Don't show again in either holds in the
    /// other, even when the other saves a start straight after it.
    #[test]
    fn antigravity_notice_the_two_clients_share_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notice.json");
        assert!(notice_start(&path).show);
        for start in 2..=100 {
            // Each client reads the file at its own start.
            assert!(!notice_start(&path).show, "{start}");
        }
        assert!(notice_start(&path).show, "start 101");
        assert_eq!(notice_session(&path).dismiss_permanently(), None);
        for start in 102..=500 {
            assert!(!notice_start(&path).show, "{start}");
        }
    }

    /// PRD #211: starts racing on the shared file lose no count and no
    /// dismissal, under the lock the star reminder uses.
    #[test]
    fn antigravity_notice_racing_starts_lose_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notice.json");
        let start = std::sync::Arc::new(std::sync::Barrier::new(17));
        let mut threads = Vec::new();
        for _ in 0..16 {
            let (path, start) = (path.clone(), start.clone());
            threads.push(std::thread::spawn(move || {
                start.wait();
                notice_start(&path).show
            }));
        }
        let dismiss = {
            let (path, start) = (path.clone(), start.clone());
            std::thread::spawn(move || {
                start.wait();
                assert_eq!(notice_session(&path).dismiss_permanently(), None);
            })
        };
        let shown = threads
            .into_iter()
            .map(|t| t.join().unwrap())
            .filter(|shown| *shown)
            .count();
        dismiss.join().unwrap();
        assert!(shown <= 1, "{shown} starts showed the notice");
        let (state, _) = AntigravityNoticeState::load_from(&path);
        assert_eq!(state.start_count, 16);
        assert!(state.permanently_dismissed);
    }

    /// PRD #211: a session whose state file cannot be saved keeps counting
    /// in memory, so the start after the first showing does not show the
    /// notice again, and the hundredth after it does. The problem is said
    /// once.
    #[test]
    fn antigravity_notice_session_keeps_unsaved_starts() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, "").unwrap();
        let mut session = notice_session(&blocker.join("notice.json"));
        let first = session.record_start();
        assert!(first.show, "first start");
        assert!(first.problem.is_some(), "the problem was not said");
        for start in 2..=100 {
            let next = session.record_start();
            assert!(!next.show, "start {start} showed");
            assert_eq!(next.problem, None, "start {start} said it again");
        }
        assert!(session.record_start().show, "start 101");
        assert_eq!(session.state().start_count, 101);
    }

    /// PRD #211: a Don't show again that cannot be saved holds for the rest
    /// of the session and says why once; a new session (the client
    /// restarted) reads the file again.
    #[test]
    fn antigravity_notice_session_holds_an_unsaved_dismissal() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, "").unwrap();
        let path = blocker.join("notice.json");
        let mut session = notice_session(&path);
        let problem = session.dismiss_permanently();
        assert!(
            problem
                .as_deref()
                .is_some_and(|p| p.contains("Antigravity notice")),
            "{problem:?}"
        );
        for start in 1..=300 {
            let next = session.record_start();
            assert!(!next.show, "start {start} showed after Don't show again");
            assert_eq!(next.problem, None, "start {start} said it again");
        }
        assert_eq!(session.dismiss_permanently(), None, "said twice");
        assert!(notice_start(&path).show, "a restarted client");
    }

    /// PRD #211: what a session could not save is saved with its next update
    /// once the file can be written again.
    #[test]
    fn antigravity_notice_session_saves_what_it_held_once_it_can() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("state");
        std::fs::write(&parent, "").unwrap();
        let path = parent.join("notice.json");
        let mut session = notice_session(&path);
        assert!(session.record_start().show);
        assert_eq!(session.dismiss_permanently(), None, "said twice");
        std::fs::remove_file(&parent).unwrap();
        assert!(!session.record_start().show);
        let (state, problem) = AntigravityNoticeState::load_from(&path);
        assert_eq!(problem, None);
        assert_eq!(
            state,
            AntigravityNoticeState {
                start_count: 2,
                last_prompt_at_start: 1,
                permanently_dismissed: true,
            }
        );

        // The other client saves a start while this session cannot: neither
        // start is lost. The file at 10; this session's start cannot be
        // saved (the other client holds the lock); the other client then
        // saves its own; this session's next start saves 13, not 12.
        let path = dir.path().join("shared.json");
        seed_notice(&path, 10, 1);
        let mut a = notice_session(&path);
        let held = hold_notice_lock(&path);
        let unsaved = a.record_start();
        assert!(!unsaved.show);
        assert!(unsaved.problem.is_some(), "the held lock was not said");
        drop(held);
        assert_eq!(AntigravityNoticeState::load_from(&path).0.start_count, 10);
        assert!(!notice_start(&path).show, "the other client's start");
        assert_eq!(AntigravityNoticeState::load_from(&path).0.start_count, 11);
        assert_eq!(a.state().start_count, 12, "the file's 11 and A's one");
        assert!(!a.record_start().show);
        let (state, _) = AntigravityNoticeState::load_from(&path);
        assert_eq!((state.start_count, state.last_prompt_at_start), (13, 1));
        assert!(!state.permanently_dismissed);
        assert_eq!(
            a.state().start_count,
            13,
            "the saved starts were kept twice"
        );
    }

    /// PRD #211: write the notice state `count` / `last` to `path`.
    fn seed_notice(path: &Path, count: u64, last: u64) {
        AntigravityNoticeState {
            start_count: count,
            last_prompt_at_start: last,
            permanently_dismissed: false,
        }
        .save_to(path)
        .unwrap();
    }

    /// PRD #211: the lock the other client holds while it saves, so a
    /// session's save at `path` fails until it is dropped.
    fn hold_notice_lock(path: &Path) -> std::fs::File {
        lock_reminder_state(path, "antigravity-notice-state.json", "test")
            .unwrap()
            .expect("this filesystem cannot lock")
    }

    /// PRD #211: several starts this session could not save are all added to
    /// the starts the other client saved meanwhile, and whether the notice is
    /// due is decided on that total. The file at 96 (last shown at 1); three
    /// unsaved starts here; one saved there; this session's next start is the
    /// 101st, the hundredth after the showing, and shows the notice.
    #[test]
    fn antigravity_notice_session_adds_several_unsaved_starts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notice.json");
        seed_notice(&path, 96, 1);
        let mut a = notice_session(&path);
        let held = hold_notice_lock(&path);
        for start in 97..=99 {
            let unsaved = a.record_start();
            assert!(!unsaved.show, "start {start} showed");
            assert_eq!(unsaved.problem.is_some(), start == 97, "start {start}");
        }
        drop(held);
        assert_eq!(AntigravityNoticeState::load_from(&path).0.start_count, 96);
        assert!(!notice_start(&path).show, "the other client's start");
        assert_eq!(a.state().start_count, 100);
        assert!(a.record_start().show, "the 101st start");
        let (state, _) = AntigravityNoticeState::load_from(&path);
        assert_eq!((state.start_count, state.last_prompt_at_start), (101, 101));
        assert!(!a.record_start().show, "102");
        assert!(!notice_start(&path).show, "103, the other client");
        let (state, _) = AntigravityNoticeState::load_from(&path);
        assert_eq!((state.start_count, state.last_prompt_at_start), (103, 101));
    }

    /// PRD #211 (Qodo finding on PR #2370): a showing this session could not
    /// save keeps the start it happened at when the other client saves starts
    /// before this session recovers. The file at 150 (last shown at 50); this
    /// session's 151st start shows the notice and cannot be saved; the other
    /// client then saves five starts (its first shows the notice too, since
    /// it cannot know of this one); this session's next start saves 157 with
    /// the showing at 151, not 156, and the notice is next due at 251.
    #[test]
    fn antigravity_notice_unsaved_showing_keeps_its_start() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notice.json");
        seed_notice(&path, 150, 50);
        let mut a = notice_session(&path);
        let held = hold_notice_lock(&path);
        let unsaved = a.record_start();
        assert!(unsaved.show, "the 151st start");
        assert!(unsaved.problem.is_some(), "the held lock was not said");
        drop(held);
        assert_eq!(
            AntigravityNoticeState::load_from(&path).0.start_count,
            150,
            "the showing was saved"
        );
        assert!(notice_start(&path).show, "the other client's 151st");
        for start in 152..=155 {
            assert!(!notice_start(&path).show, "the other client's {start}");
        }
        let (state, _) = AntigravityNoticeState::load_from(&path);
        assert_eq!((state.start_count, state.last_prompt_at_start), (155, 151));
        let merged = a.state();
        assert_eq!(
            (merged.start_count, merged.last_prompt_at_start),
            (156, 151),
            "the unsaved showing moved after the other client's starts"
        );
        assert!(!a.record_start().show, "157");
        let (state, _) = AntigravityNoticeState::load_from(&path);
        assert_eq!(
            (state.start_count, state.last_prompt_at_start),
            (157, 151),
            "the recovered save rebased the showing"
        );
        assert_eq!(a.state(), state, "the saved change was kept twice");
        for start in 158..=250 {
            assert!(!notice_start(&path).show, "start {start} showed");
        }
        assert!(notice_start(&path).show, "251, the hundredth after 151");
    }

    /// PRD #211: an unsaved showing applied to a file read later keeps the
    /// total it happened at whatever the file's own showing — earlier, the
    /// same or later — and never lands past the merged total when the file
    /// went back (it was replaced by an empty one).
    #[test]
    fn antigravity_notice_pending_showing_keeps_its_total() {
        let pending = AntigravityNoticePending {
            starts: 1,
            shown_at: Some(151),
            dismissed: false,
        };
        for (file_last, merged_last) in [(50, 151), (151, 151), (153, 153)] {
            let mut state = AntigravityNoticeState {
                start_count: 155,
                last_prompt_at_start: file_last,
                permanently_dismissed: false,
            };
            pending.apply_to(&mut state);
            assert_eq!(
                (state.start_count, state.last_prompt_at_start),
                (156, merged_last),
                "the file's showing at {file_last}"
            );
        }
        let mut state = AntigravityNoticeState::default();
        pending.apply_to(&mut state);
        assert_eq!((state.start_count, state.last_prompt_at_start), (1, 1));
        assert!(!state.is_due(), "the start after the showing");
    }

    /// PRD #211: Don't show again holds across a save this session could not
    /// make, in both directions. A dismissal here that could not be saved
    /// holds over the other client's later start and is saved with this
    /// session's next start; a dismissal the other client saved while this
    /// session held an unsaved start holds here and is kept by that save.
    /// Neither side's starts are lost.
    #[test]
    fn antigravity_notice_unsaved_dismissal_ors_both_ways() {
        let dir = tempfile::tempdir().unwrap();

        // This session's dismissal cannot be saved; the other client's start
        // is saved without it.
        let path = dir.path().join("here.json");
        seed_notice(&path, 10, 1);
        let mut a = notice_session(&path);
        let held = hold_notice_lock(&path);
        assert!(a.dismiss_permanently().is_some(), "the held lock was said");
        drop(held);
        assert!(!notice_start(&path).show, "the other client's start");
        let (state, _) = AntigravityNoticeState::load_from(&path);
        assert_eq!(state.start_count, 11);
        assert!(!state.permanently_dismissed, "precondition");
        assert!(a.state().permanently_dismissed, "the unsaved dismissal");
        assert!(!a.record_start().show);
        let (state, _) = AntigravityNoticeState::load_from(&path);
        assert_eq!(state.start_count, 12);
        assert!(state.permanently_dismissed, "this session's dismissal");

        // The other client's dismissal is saved while this session holds an
        // unsaved start.
        let path = dir.path().join("there.json");
        seed_notice(&path, 10, 1);
        let mut a = notice_session(&path);
        let held = hold_notice_lock(&path);
        assert!(!a.record_start().show);
        drop(held);
        assert_eq!(notice_session(&path).dismiss_permanently(), None);
        let merged = a.state();
        assert_eq!(merged.start_count, 11, "the unsaved start");
        assert!(merged.permanently_dismissed, "the other client's dismissal");
        assert!(!a.record_start().show);
        let (state, _) = AntigravityNoticeState::load_from(&path);
        assert_eq!(state.start_count, 12);
        assert!(state.permanently_dismissed, "the other client's dismissal");
    }

    /// PRD #211 (review of the fix for Qodo's finding on PR #2370): two
    /// sessions hold an unsaved showing over a file that went back, and
    /// recover in either order. The file at 150 (last shown at 50); session
    /// A's 151st start shows the notice and cannot be saved; the file is
    /// deleted; session B's two starts cannot be saved either, its first
    /// showing the notice. Once saving works, each records one more start.
    /// Whichever saves first, all five starts are kept, the showing is not
    /// past the total, and neither recovering start shows the notice. Where
    /// the showing lands may differ between the two orders, and is not
    /// asserted.
    #[test]
    fn antigravity_notice_two_unsaved_showings_over_a_reset_file() {
        for a_first in [true, false] {
            let order = if a_first { "A then B" } else { "B then A" };
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("notice.json");
            seed_notice(&path, 150, 50);
            let mut a = notice_session(&path);
            let mut b = notice_session(&path);
            let held = hold_notice_lock(&path);
            let start = a.record_start();
            assert!(start.show, "{order}: A's 151st start");
            assert!(start.problem.is_some(), "{order}: A saved");
            std::fs::remove_file(&path).unwrap();
            let start = b.record_start();
            assert!(start.show, "{order}: B's first start over the reset file");
            assert!(start.problem.is_some(), "{order}: B saved");
            assert!(!b.record_start().show, "{order}: B's second start");
            drop(held);
            assert!(
                !path.exists(),
                "{order}: a start was saved while the lock was held"
            );

            let (first, second) = if a_first {
                (&mut a, &mut b)
            } else {
                (&mut b, &mut a)
            };
            for (which, session) in [("first", first), ("second", second)] {
                let start = session.record_start();
                assert!(!start.show, "{order}: the {which} recovering start");
                assert!(start.problem.is_none(), "{order}: {:?}", start.problem);
            }
            let (state, unreadable) = AntigravityNoticeState::load_from(&path);
            assert!(unreadable.is_none(), "{order}: {unreadable:?}");
            assert_eq!(state.start_count, 5, "{order}: a start was lost");
            assert!(
                state.last_prompt_at_start <= state.start_count,
                "{order}: the showing is past the total: {state:?}"
            );
            assert!(!state.permanently_dismissed, "{order}");
            assert_eq!(a.state(), state, "{order}: A kept an unsaved change");
            assert_eq!(b.state(), state, "{order}: B kept an unsaved change");
        }
    }

    /// PRD #2174: two clients share one file. Launches of either count
    /// towards the reminder, which shows on the tenth whichever client it is.
    #[test]
    fn star_prompt_launches_of_either_client_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("star.json");
        for launch in 1..=9 {
            // Alternate the two clients; each reads the file at its start.
            let mut client = StarPromptState::load_from(&path);
            assert!(!client.increment_and_check(), "launch {launch} asks");
        }
        let mut tenth = StarPromptState::load_from(&path);
        assert!(tenth.increment_and_check(), "the tenth launch asks");
    }

    /// PRD #2174: a client that is showing the reminder when the other one
    /// is told "Don't ask again" keeps that answer when it is answered
    /// "Later" — a choice saves what the file holds now, not what it held
    /// when the client started.
    #[test]
    fn star_prompt_later_keeps_a_dismissal_the_other_client_saved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("star.json");
        let mut seed = StarPromptState::load_from(&path);
        for _ in 0..9 {
            seed.increment_and_check();
        }

        let mut tui = StarPromptState::load_from(&path);
        assert!(tui.increment_and_check(), "launch 10 asks the TUI");
        let mut desktop = StarPromptState::load_from(&path);
        assert!(
            desktop.increment_and_check(),
            "launch 11 asks the desktop app"
        );

        desktop.dismiss_permanently().unwrap();
        tui.snooze().unwrap();

        let after = StarPromptState::load_from(&path);
        assert!(after.permanently_dismissed, "Later undid Don't ask again");
        assert_eq!(after.launch_count, 11, "Later lost the desktop's launch");
        let mut next = StarPromptState::load_from(&path);
        for _ in 0..20 {
            assert!(!next.increment_and_check());
        }
    }

    /// PRD #2174 (Qodo and Greptile, PR #2321): launches and choices racing
    /// on the shared file lose nothing. Each of sixteen threads reads the
    /// file, then counts a launch and answers Later, while one more says
    /// Don't ask again; every launch is counted and the dismissal survives.
    #[test]
    fn star_prompt_racing_updates_lose_no_launch_and_no_dismissal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("star.json");
        let start = std::sync::Arc::new(std::sync::Barrier::new(17));
        let mut threads = Vec::new();
        for _ in 0..16 {
            let (path, start) = (path.clone(), start.clone());
            threads.push(std::thread::spawn(move || {
                let mut client = StarPromptState::load_from(&path);
                start.wait();
                client.increment_and_check();
                client.snooze().unwrap();
            }));
        }
        {
            let (path, start) = (path.clone(), start.clone());
            threads.push(std::thread::spawn(move || {
                let mut client = StarPromptState::load_from(&path);
                start.wait();
                client.dismiss_permanently().unwrap();
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
        let after = StarPromptState::load_from(&path);
        assert_eq!(after.launch_count, 16, "a racing launch was lost");
        assert!(
            after.permanently_dismissed,
            "a racing save undid Don't ask again"
        );
    }

    /// PRD #2174 (Qodo, PR #2321): a launch whose save fails after the count
    /// was made — the state file is read-only — counts once, not twice.
    #[cfg(unix)]
    #[test]
    fn star_prompt_a_launch_that_cannot_be_saved_counts_once() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("star.json");
        let mut seed = StarPromptState::load_from(&path);
        for _ in 0..8 {
            seed.increment_and_check();
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
        if std::fs::OpenOptions::new().write(true).open(&path).is_ok() {
            println!("SKIP: a read-only file is still writable here (running as root)");
            return;
        }
        let mut launch = StarPromptState::load_from(&path);
        assert!(!launch.increment_and_check(), "launch 9 asked");
        assert_eq!(launch.launch_count, 9);
        assert_eq!(
            StarPromptState::load_from(&path).launch_count,
            8,
            "the save went through"
        );
    }

    /// PRD #2174 (Qodo, PR #2321): two clients reaching one state file by
    /// different paths — one through a symlink — take the same lock, the one
    /// beside the file a save replaces.
    #[cfg(unix)]
    #[test]
    fn star_prompt_lock_sits_beside_the_symlink_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real").join("star.json");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "{}").unwrap();
        let link = dir.path().join("link.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let held = lock_star_prompt(&target)
            .unwrap()
            .expect("the lock is taken");
        assert!(dir.path().join("real").join(".star.json.lock").exists());
        assert!(!dir.path().join(".link.json.lock").exists());
        // Held through the real path, the link's lock is the same one: taking
        // it from another handle waits the whole bound and is refused.
        assert!(lock_star_prompt(&link).is_err());
        drop(held);
        assert!(lock_star_prompt(&link).unwrap().is_some());
    }

    /// PRD #2174 (Qodo, PR #2321): a symlinked state file — kept with
    /// dotfiles, say — stays a symlink, and its target holds the state.
    #[cfg(unix)]
    #[test]
    fn star_prompt_save_keeps_a_symlinked_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("dotfiles-star.json");
        let link = dir.path().join("star.json");
        std::fs::write(&target, "{}").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let mut state = StarPromptState::load_from(&link);
        state.increment_and_check();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(StarPromptState::load_from(&target).launch_count, 1);
    }

    /// PRD #2174: "Later" in one client pushes the next reminder in both
    /// another ten launches away.
    #[test]
    fn star_prompt_later_delays_both_clients_ten_launches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("star.json");
        let mut seed = StarPromptState::load_from(&path);
        for _ in 0..9 {
            seed.increment_and_check();
        }
        let mut asked = StarPromptState::load_from(&path);
        assert!(asked.increment_and_check());
        asked.snooze().unwrap();
        for launch in 11..=19 {
            let mut client = StarPromptState::load_from(&path);
            assert!(!client.increment_and_check(), "launch {launch} asks");
        }
        let mut twentieth = StarPromptState::load_from(&path);
        assert!(twentieth.increment_and_check(), "launch 20 asks again");
    }

    /// Issue #519: a `config.toml` written before the idle-art removal still
    /// carries an `[idle_art]` section. `DashboardConfig` sets no
    /// `#[serde(deny_unknown_fields)]`, so the stale table must be dropped
    /// silently — a hard parse failure here would print "Invalid config" and
    /// reset every OTHER key to its default on startup.
    #[test]
    fn dashboard_config_ignores_removed_idle_art_section() {
        let toml_str = r#"
default_command = "claude"
auto_config_prompt = false

[idle_art]
enabled = true
provider = "openai"
model = "gpt-4o-mini"
timeout_secs = 600
"#;
        let dc: DashboardConfig =
            toml::from_str(toml_str).expect("a stale [idle_art] section must not fail the load");
        assert_eq!(dc.default_command, "claude");
        assert!(!dc.auto_config_prompt);
    }

    /// Issue #519: the four `idle_art.*` keys are gone from [`CONFIG_KEYS`], so
    /// `config get` / `config set` now report them as unknown rather than
    /// accepting a setting nothing reads.
    #[test]
    fn idle_art_config_keys_are_unknown() {
        let mut dc = DashboardConfig::default();
        for key in [
            "idle_art.enabled",
            "idle_art.provider",
            "idle_art.model",
            "idle_art.timeout_secs",
        ] {
            assert!(dc.get_field(key).is_err(), "get_field({key}) should fail");
            assert!(
                dc.set_field(key, "true").is_err(),
                "set_field({key}) should fail"
            );
        }
        assert!(
            !CONFIG_KEYS.iter().any(|(k, _)| k.starts_with("idle_art.")),
            "no idle_art.* key should remain in the `config set --help` listing"
        );
    }

    /// Issue #2085: `config get` answers each of these keys with its field's
    /// value, moved off the default first; a key without its arm is refused.
    #[test]
    fn get_field_reads_each_bell_doorbell_and_command_key() {
        let mut dc = DashboardConfig {
            default_command: "my-agent --flag".into(),
            ..Default::default()
        };
        dc.bell.enabled = !dc.bell.enabled;
        dc.bell.on_waiting_for_input = !dc.bell.on_waiting_for_input;
        dc.bell.on_idle = !dc.bell.on_idle;
        dc.bell.on_error = !dc.bell.on_error;
        dc.doorbell.claude = !dc.doorbell.claude;
        dc.doorbell.codex = !dc.doorbell.codex;
        dc.doorbell.antigravity = !dc.doorbell.antigravity;
        for (key, expected) in [
            ("default_command", dc.default_command.clone()),
            ("bell.enabled", dc.bell.enabled.to_string()),
            (
                "bell.on_waiting_for_input",
                dc.bell.on_waiting_for_input.to_string(),
            ),
            ("bell.on_idle", dc.bell.on_idle.to_string()),
            ("bell.on_error", dc.bell.on_error.to_string()),
            ("doorbell.claude", dc.doorbell.claude.to_string()),
            ("doorbell.codex", dc.doorbell.codex.to_string()),
            ("doorbell.antigravity", dc.doorbell.antigravity.to_string()),
        ] {
            assert_eq!(dc.get_field(key), Ok(expected), "{key}");
        }
    }

    /// PRD #1223: `config set default_dir` takes an absolute path, refuses a
    /// relative one, unsets on empty, and an unset key is not written back.
    #[test]
    fn default_dir_is_settable_absolute_only_and_omitted_when_empty() {
        let mut dc = DashboardConfig::default();
        assert_eq!(dc.get_field("default_dir").unwrap(), "");
        dc.set_field("default_dir", "/srv/reports").unwrap();
        assert_eq!(dc.get_field("default_dir").unwrap(), "/srv/reports");
        assert!(dc.set_field("default_dir", "reports").is_err());
        assert_eq!(dc.default_dir, "/srv/reports");
        assert!(
            toml::to_string_pretty(&dc)
                .unwrap()
                .contains("default_dir = \"/srv/reports\"")
        );
        dc.set_field("default_dir", "").unwrap();
        assert!(!toml::to_string_pretty(&dc).unwrap().contains("default_dir"));
        let parsed: DashboardConfig = toml::from_str("default_dir = \"/srv/x\"").unwrap();
        assert_eq!(parsed.default_dir, "/srv/x");
        assert!(CONFIG_KEYS.iter().any(|(key, _)| *key == "default_dir"));
    }

    /// Issue #2075: `notifications.question_after_minutes` defaults to 10, takes
    /// a whole number of minutes, turns the notification off at 0, and is not
    /// written while it is at its default.
    #[test]
    fn question_notification_delay_is_settable_and_zero_turns_it_off() {
        let mut dc = DashboardConfig::default();
        let key = "notifications.question_after_minutes";
        assert_eq!(dc.get_field(key).unwrap(), "10");
        assert_eq!(
            dc.notifications.question_after(),
            Some(chrono::Duration::minutes(10))
        );
        assert!(
            !toml::to_string_pretty(&dc)
                .unwrap()
                .contains("notifications")
        );
        dc.set_field(key, "25").unwrap();
        assert_eq!(dc.get_field(key).unwrap(), "25");
        assert!(
            toml::to_string_pretty(&dc)
                .unwrap()
                .contains("question_after_minutes = 25")
        );
        assert!(dc.set_field(key, "ten").is_err());
        assert!(dc.set_field(key, "-1").is_err());
        // PR #2208 review: a number past what a duration holds is refused,
        // and one written into the file by hand reads as off, not a panic.
        assert!(dc.set_field(key, &u64::MAX.to_string()).is_err());
        assert!(dc.set_field(key, &i64::MAX.to_string()).is_err());
        let huge: DashboardConfig =
            toml::from_str("[notifications]\nquestion_after_minutes = 9223372036854775807")
                .unwrap();
        assert_eq!(huge.notifications.question_after(), None);
        assert_eq!(dc.notifications.question_after_minutes, 25);
        dc.set_field(key, "0").unwrap();
        assert_eq!(dc.notifications.question_after(), None);
        let parsed: DashboardConfig =
            toml::from_str("[notifications]\nquestion_after_minutes = 3").unwrap();
        assert_eq!(
            parsed.notifications.question_after(),
            Some(chrono::Duration::minutes(3))
        );
        assert!(CONFIG_KEYS.iter().any(|(k, _)| *k == key));
    }

    /// PRD #1258: the `[host_metrics]` thresholds are kept as written, so a
    /// bad value never fails the rest of `config.toml` and a `config set` of
    /// another key writes them back unchanged; `config set` refuses a value
    /// out of range or one that would make its pair's critical limit less
    /// strict than its warning, `config get` reports the threshold in effect,
    /// and a `host_metrics` that is not a table is dropped without failing the
    /// file.
    #[test]
    fn host_metrics_thresholds_round_trip_and_are_validated_by_config_set() {
        let dc: DashboardConfig = toml::from_str(
            "default_command = \"claude\"\n[host_metrics]\nload_per_core_warn = \"x\"\n",
        )
        .unwrap();
        assert_eq!(
            dc.default_command, "claude",
            "the rest of the file survives"
        );
        assert_eq!(
            dc.get_field("host_metrics.load_per_core_warn").unwrap(),
            "0.8"
        );
        assert!(
            toml::to_string_pretty(&dc)
                .unwrap()
                .contains("load_per_core_warn = \"x\""),
            "kept as written"
        );

        let mut dc = DashboardConfig::default();
        assert!(
            !toml::to_string_pretty(&dc)
                .unwrap()
                .contains("host_metrics"),
            "an unset table is not written"
        );
        dc.set_field("host_metrics.disk_free_warn_percent", "20")
            .unwrap();
        dc.set_field("host_metrics.load_per_core_critical", "2.5")
            .unwrap();
        assert_eq!(
            dc.get_field("host_metrics.disk_free_warn_percent").unwrap(),
            "20"
        );
        assert_eq!(
            dc.get_field("host_metrics.load_per_core_critical").unwrap(),
            "2.5"
        );
        let written = toml::to_string_pretty(&dc).unwrap();
        assert!(
            written.contains("disk_free_warn_percent = 20\n"),
            "{written}"
        );
        assert!(
            written.contains("load_per_core_critical = 2.5\n"),
            "{written}"
        );
        for (key, value) in [
            ("host_metrics.disk_free_warn_percent", "101"),
            ("host_metrics.load_per_core_warn", "-1"),
            ("host_metrics.load_per_core_warn", "inf"),
            ("host_metrics.load_per_core_warn", "busy"),
        ] {
            assert!(dc.set_field(key, value).is_err(), "{key} = {value}");
        }
        assert!(dc.set_field("host_metrics.typo", "1").is_err());
        assert!(dc.get_field("host_metrics.typo").is_err());

        // PR #1672 review: a value that would invert its pair is refused,
        // naming the other limit, rather than written and then ignored.
        let mut dc = DashboardConfig::default();
        let err = dc
            .set_field("host_metrics.disk_free_warn_percent", "3")
            .unwrap_err();
        assert!(
            err.contains("host_metrics.disk_free_critical_percent = 5") && err.contains("first"),
            "{err}"
        );
        assert!(dc.host_metrics.is_empty(), "nothing was written");
        dc.set_field("host_metrics.disk_free_critical_percent", "2")
            .unwrap();
        dc.set_field("host_metrics.disk_free_warn_percent", "3")
            .unwrap();
        let err = dc
            .set_field("host_metrics.load_per_core_critical", "0.5")
            .unwrap_err();
        assert!(
            err.contains("host_metrics.load_per_core_warn = 0.8"),
            "{err}"
        );
        dc.set_field("host_metrics.load_per_core_warn", "1.5")
            .unwrap();
        assert_eq!(
            dc.get_field("host_metrics.load_per_core_warn").unwrap(),
            "1.5",
            "equal limits are allowed"
        );
        for key in crate::host_metrics::HEALTH_THRESHOLD_KEYS {
            let full = format!("host_metrics.{key}");
            assert!(
                CONFIG_KEYS.iter().any(|(k, _)| *k == full),
                "{full} is listed"
            );
        }

        let dc: DashboardConfig =
            toml::from_str("default_command = \"claude\"\nhost_metrics = 5\n").unwrap();
        assert_eq!(dc.default_command, "claude");
        assert!(dc.host_metrics.is_empty());
    }

    // PRD #42 M2: asserts the Unix temp-dir + per-uid fallback specifically
    // (`current_uid()` is Unix-only and the path shape is POSIX), so it is
    // gated to Unix. The Windows endpoint is a per-user named pipe with no
    // temp-dir/uid analogue — covered separately under PRD #163/#164.
    //
    // Issue #1121 moved where the per-uid component sits — from a suffix on
    // each socket's own name to the directory holding both — so this reads it
    // off the directory now. The property being pinned is the one it always
    // was and the reason is unchanged.
    #[cfg(unix)]
    #[test]
    fn attach_socket_fallback_is_per_user() {
        // PRD #93 round-2 reviewer REV-2: when XDG_RUNTIME_DIR is unset
        // *and* DOT_AGENT_DECK_ATTACH_SOCKET is unset, the temp-dir fallback
        // must include the uid so two users on the same host don't
        // collide. The old `/tmp/dot-agent-deck-attach.sock` would
        // sandwich two daemons onto one path and let the first binder
        // arbitrarily lock the rest of the host out.
        let _g = STATE_DIR_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev_attach = std::env::var("DOT_AGENT_DECK_ATTACH_SOCKET").ok();
        let prev_sock = std::env::var("DOT_AGENT_DECK_SOCKET").ok();
        let prev_xdg = std::env::var("XDG_RUNTIME_DIR").ok();
        // SAFETY: a stated residual, not a proof (issue #1516). The state-dir
        // lock excludes the sibling tests that take it, not threads. This test
        // starts no runtime and no thread, so under nextest the threads that
        // exist are its own and libtest's runner thread, which waits for it.
        // Restored on the way out.
        unsafe {
            std::env::remove_var("DOT_AGENT_DECK_ATTACH_SOCKET");
            std::env::remove_var("DOT_AGENT_DECK_SOCKET");
            std::env::remove_var("XDG_RUNTIME_DIR");
        }

        let uid = crate::platform::paths::current_uid();
        let dir = crate::platform::paths::fallback_endpoint_dir();
        let attach = attach_socket_path();
        let hook = socket_path();
        assert_eq!(
            dir.file_name().and_then(|name| name.to_str()),
            Some(format!("dot-agent-deck-{uid}").as_str()),
            "the fallback endpoint directory must embed the uid: got {}",
            dir.display()
        );
        assert_eq!(
            attach.parent(),
            Some(dir.as_path()),
            "attach fallback must live in the per-uid directory: got {}",
            attach.display()
        );
        assert_eq!(
            hook.parent(),
            Some(dir.as_path()),
            "hook fallback must live in the per-uid directory: got {}",
            hook.display()
        );
        assert_ne!(
            attach, hook,
            "the two protocols must not share one endpoint"
        );
        assert_eq!(
            dir.parent(),
            Some(std::env::temp_dir().as_path()),
            "the fallback directory sits directly under the temp dir: got {}",
            dir.display()
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

    /// `state_dir()` takes `DOT_AGENT_DECK_STATE_DIR` first, then (on Unix)
    /// `$XDG_STATE_HOME/dot-agent-deck`, then `$HOME/.local/state/dot-agent-deck`.
    /// Windows has no XDG concept and ignores `$HOME`, so its last two rows
    /// expect `%LOCALAPPDATA%\dot-agent-deck`, derived from the same `dirs`
    /// crate the resolver uses so no machine-specific username is hardcoded.
    #[test]
    fn state_dir_prefers_the_override_then_xdg_state_home_then_home() {
        let _guard = STATE_DIR_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let keys = ["DOT_AGENT_DECK_STATE_DIR", "XDG_STATE_HOME", "HOME"];
        let prev: Vec<Option<String>> = keys.iter().map(|k| std::env::var(k).ok()).collect();
        let fallback = |unix_path: &str| -> PathBuf {
            #[cfg(unix)]
            {
                PathBuf::from(unix_path)
            }
            #[cfg(windows)]
            {
                let _ = unix_path;
                dirs::data_local_dir()
                    .map(|p| p.join("dot-agent-deck"))
                    .unwrap_or_else(|| {
                        crate::platform::paths::home_dir().join("AppData/Local/dot-agent-deck")
                    })
            }
        };
        let cases = [
            (
                [
                    Some("/tmp/explicit-state"),
                    Some("/should/be/ignored"),
                    prev[2].as_deref(),
                ],
                PathBuf::from("/tmp/explicit-state"),
            ),
            (
                [None, Some("/var/lib/state"), prev[2].as_deref()],
                fallback("/var/lib/state/dot-agent-deck"),
            ),
            (
                [None, None, Some("/home/test-user")],
                fallback("/home/test-user/.local/state/dot-agent-deck"),
            ),
        ];
        for (values, expected) in cases {
            // SAFETY: env-var lock held; restored after the loop.
            unsafe {
                for (key, value) in keys.iter().zip(values) {
                    match value {
                        Some(v) => std::env::set_var(key, v),
                        None => std::env::remove_var(key),
                    }
                }
            }
            assert_eq!(state_dir(), expected, "{values:?}");
        }

        // SAFETY: same lock held; restoring previous values.
        unsafe {
            for (key, value) in keys.iter().zip(prev) {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
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
        // `~/...` is expanded at load time. On Unix the home is the `HOME` we
        // set above; on Windows `~` resolves via the path resolver (e.g.
        // `%USERPROFILE%`), so assert the expansion *logic* (anchored at the
        // resolver's home, tilde gone, expected suffix) rather than a hardcoded
        // path — the username varies per machine.
        #[cfg(unix)]
        assert_eq!(minimal.working_dir, "/home/tester/scheduled/morning");
        #[cfg(windows)]
        {
            let home = crate::platform::paths::home_dir();
            assert!(minimal.working_dir.starts_with(&*home.to_string_lossy()));
            // Issue #1136: the remainder is now spelled with the host's own
            // separator. The `scheduled/morning` this used to assert IS the
            // mixed-separator text the issue is about — `C:\Users\me/scheduled/morning`.
            assert!(minimal.working_dir.ends_with(r"scheduled\morning"));
            assert!(!minimal.working_dir.contains('~'));
        }

        // Relative result (from $VAR) resolves against home.
        let with_var = &loaded.tasks[1];
        #[cfg(unix)]
        assert_eq!(with_var.working_dir, "/home/tester/projects/digest");
        #[cfg(windows)]
        {
            let home = crate::platform::paths::home_dir();
            assert!(with_var.working_dir.starts_with(&*home.to_string_lossy()));
            assert!(with_var.working_dir.ends_with("projects/digest"));
        }

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

    // PRD #120 (U1) — a fully-specified `issue_dispatch` task round-trips every
    // field from `schedules.toml`: the `[scheduled_tasks.issue_dispatch]` table
    // is the task-type discriminator, and a command-less entry is accepted
    // (unlike a #127 single-spawn task) because the per-issue command comes from
    // the cloned repo's config.
    #[test]
    fn issue_dispatch_round_trips_all_fields() {
        let toml_str = r#"
[[scheduled_tasks]]
name = "Issues vfarcic/dot-ai"
cron = "0 9 * * MON-FRI"
working_dir = "/work/space"
prompt = "Work on issue {{issue_number}}"
enabled = true

[scheduled_tasks.issue_dispatch]
repo = "vfarcic/dot-ai"
max_per_run = 3
label = "agent-eligible"
query = "is:open sort:created-asc"
"#;
        let loaded = LoadedSchedules::parse(toml_str);
        assert!(loaded.errors.is_empty(), "errors: {:?}", loaded.errors);
        assert_eq!(loaded.tasks.len(), 1);
        let t = &loaded.tasks[0];
        // Shared scheduler fields still carry the task's identity / schedule.
        assert_eq!(t.name, "Issues vfarcic/dot-ai");
        assert_eq!(t.cron, "0 9 * * MON-FRI");
        assert_eq!(t.working_dir, "/work/space");
        assert_eq!(t.prompt, "Work on issue {{issue_number}}");
        assert!(
            t.command.is_none(),
            "issue-dispatch needs no top-level command"
        );
        // The discriminator table carries every GitHub-specific field.
        let disp = t
            .issue_dispatch
            .as_ref()
            .expect("issue_dispatch table present → issue-dispatch task");
        assert_eq!(disp.repo, "vfarcic/dot-ai");
        assert_eq!(disp.max_per_run, 3);
        assert_eq!(disp.label.as_deref(), Some("agent-eligible"));
        assert_eq!(disp.query.as_deref(), Some("is:open sort:created-asc"));
    }

    // PRD #120 (U1) — the optional filters (`label`, `query`) default to `None`
    // when omitted, and a plain `[[scheduled_tasks]]` entry with no
    // `issue_dispatch` table is still a (command-required) single-spawn task.
    #[test]
    fn issue_dispatch_optional_filters_default_to_none() {
        let toml_str = r#"
[[scheduled_tasks]]
name = "Issues vfarcic/dot-ai"
cron = "0 9 * * *"
working_dir = "/work/space"
prompt = "Work on issue {{issue_number}}"

[scheduled_tasks.issue_dispatch]
repo = "vfarcic/dot-ai"
max_per_run = 5
"#;
        let loaded = LoadedSchedules::parse(toml_str);
        assert!(loaded.errors.is_empty(), "errors: {:?}", loaded.errors);
        let disp = loaded.tasks[0]
            .issue_dispatch
            .as_ref()
            .expect("issue-dispatch task");
        assert_eq!(disp.repo, "vfarcic/dot-ai");
        assert_eq!(disp.max_per_run, 5);
        assert!(disp.label.is_none(), "omitted label defaults to None");
        assert!(disp.query.is_none(), "omitted query defaults to None");
        // `enabled` still defaults true via the shared machinery.
        assert!(loaded.tasks[0].enabled);

        // A non-issue-dispatch entry has no table and is the original task type.
        let plain = r#"
[[scheduled_tasks]]
name = "plain"
cron = "0 9 * * *"
working_dir = "/tmp"
command = "claude"
prompt = "hi"
"#;
        let loaded = LoadedSchedules::parse(plain);
        assert!(loaded.errors.is_empty());
        assert!(loaded.tasks[0].issue_dispatch.is_none());
    }

    // PRD #120 — `max_per_run` is optional: a hand-written issue-dispatch table
    // that omits it deserializes (and validates/loads) with the changelog's
    // documented default of 3, rather than being rejected for a missing field.
    #[test]
    fn issue_dispatch_max_per_run_defaults_to_three_when_omitted() {
        let toml_str = r#"
[[scheduled_tasks]]
name = "Issues vfarcic/dot-ai"
cron = "0 9 * * *"
working_dir = "/work/space"
prompt = "Work on issue {{issue_number}}"

[scheduled_tasks.issue_dispatch]
repo = "vfarcic/dot-ai"
"#;
        let loaded = LoadedSchedules::parse(toml_str);
        // The entry must validate and load (not land in `errors`).
        assert!(loaded.errors.is_empty(), "errors: {:?}", loaded.errors);
        assert_eq!(loaded.tasks.len(), 1);
        let disp = loaded.tasks[0]
            .issue_dispatch
            .as_ref()
            .expect("issue-dispatch task");
        assert_eq!(disp.repo, "vfarcic/dot-ai");
        assert_eq!(
            disp.max_per_run, 3,
            "omitted max_per_run must default to 3 (matches changelog)"
        );
    }

    /// One issue-dispatch task on `cron` with `claim_keys` appended to its
    /// `[scheduled_tasks.issue_dispatch]` table.
    fn issue_dispatch_toml(cron: &str, claim_keys: &str) -> String {
        format!(
            r#"
[[scheduled_tasks]]
name = "Issues vfarcic/dot-ai"
cron = "{cron}"
working_dir = "/work/space"
prompt = "Work on issue {{{{issue_number}}}}"

[scheduled_tasks.issue_dispatch]
repo = "vfarcic/dot-ai"
{claim_keys}
"#
        )
    }

    fn loaded_claim(toml_str: &str) -> ClaimGatingConfig {
        let loaded = LoadedSchedules::parse(toml_str);
        assert!(loaded.errors.is_empty(), "errors: {:?}", loaded.errors);
        loaded.tasks[0]
            .issue_dispatch
            .as_ref()
            .expect("issue-dispatch task")
            .claim
            .clone()
    }

    // PRD #421 M3.4 — the claim keys are optional: gating off, a week's TTL.
    #[test]
    fn issue_dispatch_claim_keys_default_to_gating_off_and_a_week() {
        let claim = loaded_claim(&issue_dispatch_toml("0 9 * * MON-FRI", ""));
        assert_eq!(claim, ClaimGatingConfig::default());
        assert!(!claim.gating && claim.ttl_hours == 168 && claim.gating_off.is_none());
        assert!(!claim.gating_in_effect());
        assert_eq!(claim.ttl(), chrono::Duration::hours(168));
    }

    #[test]
    fn issue_dispatch_claim_keys_are_read_from_the_issue_dispatch_table() {
        let claim = loaded_claim(&issue_dispatch_toml(
            "0 9 * * MON-FRI",
            "claim_gating = true\nclaim_ttl_hours = 150",
        ));
        assert!(claim.gating && claim.ttl_hours == 150 && claim.gating_off.is_none());
        assert!(claim.gating_in_effect());
        let off = loaded_claim(&issue_dispatch_toml(
            "0 9 * * *",
            "claim_gating = false\nclaim_ttl_hours = 48",
        ));
        assert!(!off.gating_in_effect() && off.ttl_hours == 48);
    }

    // PRD #421 M3.4 — `claim_ttl_hours` is validated like the other keys: a
    // value below 1 or above 2160 (90 days), or one that is not a non-negative
    // integer, rejects the entry while its siblings still load.
    #[test]
    fn issue_dispatch_rejects_a_claim_ttl_outside_one_hour_to_ninety_days() {
        assert!(
            loaded_claim(&issue_dispatch_toml("0 9 * * *", "claim_ttl_hours = 2160")).ttl_hours
                == 2160
        );
        for keys in [
            "claim_ttl_hours = 0",
            "claim_ttl_hours = 2161",
            "claim_ttl_hours = -5",
            "claim_ttl_hours = \"a week\"",
            "claim_gating = \"yes\"",
        ] {
            let toml_str = format!(
                "{}\n{}",
                issue_dispatch_toml("0 9 * * *", keys),
                r#"
[[scheduled_tasks]]
name = "sibling"
cron = "0 9 * * *"
working_dir = "/work/space"
command = "claude"
prompt = "go"
"#
            );
            let loaded = LoadedSchedules::parse(&toml_str);
            assert_eq!(loaded.errors.len(), 1, "{keys}: {:?}", loaded.errors);
            assert_eq!(loaded.errors[0].entry, Some(0), "{keys}");
            assert_eq!(
                loaded
                    .tasks
                    .iter()
                    .map(|t| t.name.as_str())
                    .collect::<Vec<_>>(),
                vec!["sibling"],
                "{keys}"
            );
        }
    }

    // PRD #421 M3.1 — a TTL the task's cron cannot keep current (the weekday
    // cron's 72h weekend gap needs 145h) loads the task with gating turned
    // OFF rather than rejecting it, and says why.
    #[test]
    fn issue_dispatch_turns_gating_off_when_the_ttl_cannot_cover_the_cron() {
        let claim = loaded_claim(&issue_dispatch_toml(
            "0 9 * * MON-FRI",
            "claim_gating = true\nclaim_ttl_hours = 100",
        ));
        assert!(claim.gating, "the file's setting is kept");
        assert!(!claim.gating_in_effect(), "but the gate is off");
        let reason = claim.gating_off.expect("the reason is recorded");
        assert!(
            reason.contains("claim_ttl_hours = 100") && reason.contains("0 9 * * MON-FRI"),
            "{reason}"
        );
        // Gating switched off needs no check, so records no reason.
        let off = loaded_claim(&issue_dispatch_toml(
            "0 9 * * MON-FRI",
            "claim_ttl_hours = 100",
        ));
        assert_eq!(off.gating_off, None);
    }

    // PRD #421 M3.1 — the load-time "gating off" is never written back: a CLI
    // edit that rewrites the file keeps the user's `claim_gating = true`, and
    // keys left at their defaults stay out of the file.
    #[test]
    fn issue_dispatch_claim_keys_round_trip_without_the_load_time_verdict() {
        let tasks = LoadedSchedules::parse(&issue_dispatch_toml(
            "0 9 * * MON-FRI",
            "claim_gating = true\nclaim_ttl_hours = 100",
        ))
        .tasks;
        assert!(
            tasks[0]
                .issue_dispatch
                .as_ref()
                .unwrap()
                .claim
                .gating_off
                .is_some()
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("schedules.toml");
        crate::schedule_cli::write_atomic(&path, &tasks).expect("write schedules");
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            written.contains("claim_gating = true") && written.contains("claim_ttl_hours = 100"),
            "{written}"
        );
        let reloaded = LoadedSchedules::load_from(&path);
        assert!(reloaded.errors.is_empty(), "errors: {:?}", reloaded.errors);
        assert_eq!(reloaded.tasks, tasks);

        let defaults = LoadedSchedules::parse(&issue_dispatch_toml("0 9 * * *", "")).tasks;
        crate::schedule_cli::write_atomic(&path, &defaults).expect("write schedules");
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains("claim_"), "{written}");
    }

    // Issue #835 — a `shape` scalar round-trips through the file and is carried
    // on the loaded task. The field sits BEFORE `issue_dispatch` in the struct
    // precisely so it serializes ahead of any sub-table; this asserts the value
    // survives a load, and `shape_round_trips_alongside_an_issue_dispatch_sibling`
    // below covers the ordering half.
    #[test]
    fn shape_round_trips_from_the_file() {
        for (raw, label) in [
            ("single", "single"),
            ("orchestration", "bare orchestration"),
            ("orchestration:review", "named orchestration"),
        ] {
            let toml_str = format!(
                r#"
[[scheduled_tasks]]
name = "sweep"
cron = "0 9 * * *"
working_dir = "/work"
command = "claude"
prompt = "go"
shape = "{raw}"
"#
            );
            let loaded = LoadedSchedules::parse(&toml_str);
            assert!(
                loaded.errors.is_empty(),
                "{label}: errors: {:?}",
                loaded.errors
            );
            assert_eq!(
                loaded.tasks[0].shape.as_deref(),
                Some(raw),
                "{label}: shape must survive the load verbatim"
            );
        }
    }

    // Issue #835 — omitting `shape` leaves it `None`, which is what keeps every
    // pre-#835 schedule byte-identical in behaviour (the daemon skips the whole
    // override path when it is absent).
    #[test]
    fn shape_absent_means_config_derived() {
        let toml_str = r#"
[[scheduled_tasks]]
name = "sweep"
cron = "0 9 * * *"
working_dir = "/work"
command = "claude"
prompt = "go"
"#;
        let loaded = LoadedSchedules::parse(toml_str);
        assert!(loaded.errors.is_empty(), "errors: {:?}", loaded.errors);
        assert!(
            loaded.tasks[0].shape.is_none(),
            "an omitted `shape` must stay None, not default to a value"
        );
    }

    // Issue #835 — a typo'd `shape` is a LOAD error naming the task, not a
    // surprise on the next cron tick. The entry is skipped (never fired with some
    // other shape) and the error names both the bad value and the vocabulary.
    #[test]
    fn shape_rejects_an_unknown_value_at_load() {
        for bad in ["team", "Single", "orchestration:", "orchestrations:x", ""] {
            let toml_str = format!(
                r#"
[[scheduled_tasks]]
name = "typo"
cron = "0 9 * * *"
working_dir = "/work"
command = "claude"
prompt = "go"
shape = "{bad}"
"#
            );
            let loaded = LoadedSchedules::parse(&toml_str);
            assert!(
                loaded.tasks.is_empty(),
                "shape {bad:?} must not produce a loadable task"
            );
            let joined = loaded
                .errors
                .iter()
                .map(|e| e.message.clone())
                .collect::<Vec<_>>()
                .join(" | ");
            assert!(
                joined.contains("typo") && joined.to_lowercase().contains("shape"),
                "shape {bad:?}: the error must name the task and the field, got {joined:?}"
            );
        }
    }

    // Issue #835 — `shape` alongside `issue_dispatch` is REJECTED rather than
    // ignored. That task type resolves a shape per cloned repo, so a task-level
    // one has nothing to apply to, and silently dropping a field the author wrote
    // is the exact defect class #835 exists to remove.
    #[test]
    fn shape_is_rejected_alongside_issue_dispatch() {
        let toml_str = r#"
[[scheduled_tasks]]
name = "issues"
cron = "0 9 * * *"
working_dir = "/work"
prompt = "issue {{issue_number}}"
shape = "single"

[scheduled_tasks.issue_dispatch]
repo = "vfarcic/dot-ai"
"#;
        let loaded = LoadedSchedules::parse(toml_str);
        assert!(
            loaded.tasks.is_empty(),
            "the shape + issue_dispatch combination must not load"
        );
        let joined = loaded
            .errors
            .iter()
            .map(|e| e.message.clone())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(
            joined.contains("shape") && joined.contains("issue_dispatch"),
            "the error must name both fields so the author knows which to drop, got {joined:?}"
        );
    }

    // Issue #835 — the ordering half of the round-trip: `shape` is a SCALAR and
    // `issue_dispatch` serializes as a `[scheduled_tasks.issue_dispatch]`
    // sub-table, so a scalar emitted after that table would land inside it and
    // fail to re-load. Serialize a list holding both task kinds and read it back.
    #[test]
    fn shape_round_trips_alongside_an_issue_dispatch_sibling() {
        let tasks = vec![
            ScheduledTask {
                name: "sweep".to_string(),
                cron: "0 9 * * *".to_string(),
                working_dir: "/work".to_string(),
                command: Some("claude".to_string()),
                prompt: "go".to_string(),
                new_tab_per_fire: false,
                enabled: true,
                shape: Some("single".to_string()),
                issue_dispatch: None,
            },
            ScheduledTask {
                name: "issues".to_string(),
                cron: "0 10 * * *".to_string(),
                working_dir: "/work".to_string(),
                command: None,
                prompt: "issue {{issue_number}}".to_string(),
                new_tab_per_fire: false,
                enabled: true,
                shape: None,
                issue_dispatch: Some(IssueDispatchConfig {
                    repo: "vfarcic/dot-ai".to_string(),
                    max_per_run: 3,
                    label: None,
                    query: None,
                    claim: Default::default(),
                }),
            },
        ];
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("schedules.toml");
        crate::schedule_cli::write_atomic(&path, &tasks).expect("write schedules");
        let loaded = LoadedSchedules::load_from(&path);
        assert!(loaded.errors.is_empty(), "errors: {:?}", loaded.errors);
        assert_eq!(
            loaded.tasks.len(),
            2,
            "both tasks must survive the round-trip"
        );
        assert_eq!(loaded.tasks[0].shape.as_deref(), Some("single"));
        assert!(loaded.tasks[1].shape.is_none());
        assert_eq!(
            loaded.tasks[1]
                .issue_dispatch
                .as_ref()
                .expect("sibling keeps its issue_dispatch table")
                .repo,
            "vfarcic/dot-ai"
        );
    }

    /// A repo that is not an `owner/name` slug and a label starting with `-`
    /// are both rejected at load time (the values flow into `gh`/`git` argv):
    /// the entry is dropped with one per-entry error.
    #[test]
    fn issue_dispatch_rejects_a_repo_or_label_unsafe_for_argv() {
        let cases = [
            ("bad repo", "repo = \"ext::sh -c id\"", Some("owner/name")),
            (
                "bad label",
                "repo = \"acme/widgets\"\nlabel = \"-rf\"",
                None,
            ),
        ];
        for (name, fields, message) in cases {
            let toml_str = format!(
                r#"
[[scheduled_tasks]]
name = "{name}"
cron = "0 9 * * *"
working_dir = "/work/space"
prompt = "Work on issue {{{{issue_number}}}}"

[scheduled_tasks.issue_dispatch]
max_per_run = 3
{fields}
"#
            );
            let loaded = LoadedSchedules::parse(&toml_str);
            assert!(loaded.tasks.is_empty(), "{name} must be rejected");
            assert_eq!(loaded.errors.len(), 1, "{name}");
            if let Some(message) = message {
                assert!(
                    loaded.errors[0].message.contains(message),
                    "{name}: the error should explain the slug requirement: {:?}",
                    loaded.errors[0].message
                );
            }
        }
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
        // `~` expands to the home directory. On Unix that is the `HOME` we set
        // above; on Windows it resolves via the path resolver (`%USERPROFILE%`),
        // so derive the expected value from the resolver instead of hardcoding a
        // machine-specific path.
        #[cfg(unix)]
        assert_eq!(expand_path("~"), "/home/tester");
        #[cfg(windows)]
        assert_eq!(
            expand_path("~"),
            crate::platform::paths::home_dir()
                .to_string_lossy()
                .into_owned()
        );
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

    /// Issue #1136: `expand_path` must hand back a path spelled with the host's
    /// own separator, and must recognise the host's own tilde forms.
    ///
    /// On Windows the home directory is native (`C:\Users\me`), so the old
    /// `format!("{home}/{rest}")` glue produced `C:\Users\me/proj` — it
    /// resolves, but it is persisted to `schedules.toml`, displayed, and sent
    /// over the protocol as a fire's `working_dir`. And `~\proj`, the form a
    /// Windows user actually types, missed the `~/` test entirely and came back
    /// as `C:\Users\me\~\proj`.
    ///
    /// The Unix half is a preservation assertion: `\` is a legal character in a
    /// Unix file name, so `~\proj` there is a directory name and nothing about
    /// this platform's output moves.
    #[test]
    fn expand_path_uses_the_host_separator_and_tilde_forms() {
        let _guard = STATE_DIR_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev_home = std::env::var("HOME").ok();
        // SAFETY: env-var lock held; restored below.
        unsafe {
            std::env::set_var("HOME", "/home/tester");
        }

        #[cfg(unix)]
        {
            assert_eq!(expand_path("~/proj"), "/home/tester/proj");
            assert_eq!(expand_path("~/proj/sub"), "/home/tester/proj/sub");
            // NOT a tilde path here, and deliberately unchanged by #1136.
            assert_eq!(expand_path(r"~\proj"), r"/home/tester/~\proj");
            // An absolute path is returned as it was typed, a relative one is
            // anchored at $HOME, and a backslash inside a name survives both.
            assert_eq!(expand_path("/abs/path"), "/abs/path");
            assert_eq!(expand_path("rel/path"), "/home/tester/rel/path");
            assert_eq!(expand_path(r"odd\name"), r"/home/tester/odd\name");
        }

        #[cfg(windows)]
        {
            let home = crate::platform::paths::home_dir();
            let expected = home.join("proj");

            // Exact text, derived from the resolver rather than hardcoded: the
            // username varies per machine, and asserting the *text* is the
            // whole point — the path equality below would hold for the
            // mixed-separator spelling too, since both parse to the same
            // components.
            let expected_text = format!("{}\\proj", home.to_string_lossy());
            for input in ["~/proj", r"~\proj"] {
                let got = expand_path(input);
                assert_eq!(
                    got, expected_text,
                    "{input} must expand to the `proj` directory under the home \
                     directory, spelled with the host's own separator"
                );
                assert_eq!(Path::new(&got), expected.as_path());
            }

            // Untouched by #1136, and asserted so the narrowing stays pinned:
            // a non-tilde path is passed through exactly as typed. A relative
            // one still anchors at the home directory; a drive-absolute one and
            // a Unix-style rooted one both come back verbatim, forward slashes
            // and all.
            assert_eq!(
                Path::new(&expand_path("rel/path")),
                home.join("rel").join("path").as_path()
            );
            assert_eq!(expand_path(r"D:\work"), r"D:\work");
            assert_eq!(expand_path("/work/space"), "/work/space");
        }

        // SAFETY: same lock held; restore the previous value.
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
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
