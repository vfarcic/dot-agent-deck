//! PRD #40 — Customizable keybindings: the pure-data config layer.
//!
//! This module is intentionally free of any TUI / event-loop wiring (that is
//! Round 3). It owns:
//!
//! - the [`Action`] set (every remappable command) and its sections,
//! - key-notation parsing (`"Alt+Shift+t"`, `"Ctrl+n"`, `"Enter"`, `""`, …),
//! - [`Binding`] + [`matches_binding`] for testing a crossterm `KeyEvent`,
//! - [`KeybindingConfig`] with a `Default` that reproduces *exactly* today's
//!   hardcoded bindings (mirrored from `src/ui.rs`), plus TOML merge,
//!   conflict detection, unknown-action handling, and a file-loading
//!   entrypoint following the existing `DashboardConfig` conventions.
//!
//! Locked design (see `.dot-agent-deck/keybindings-impl.md`): defaults are
//! byte-for-byte unchanged, the config resolves client-side, and `Ctrl+C`
//! stays a non-overridable quit safety net (enforced in Round 3, not here).

use std::collections::HashMap;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::Deserialize;

use crate::config::dirs_home;

/// The two config sections: `[global]` and `[dashboard]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Global,
    Dashboard,
}

impl Section {
    /// TOML table name for this section.
    pub fn as_str(self) -> &'static str {
        match self {
            Section::Global => "global",
            Section::Dashboard => "dashboard",
        }
    }
}

/// Every remappable command. The variant order here is the canonical
/// "definition order" used for conflict resolution (first-defined wins).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    // [global]
    Quit,
    Dashboard,
    NewPane,
    ClosePane,
    ToggleLayout,
    Jump1,
    Jump2,
    Jump3,
    Jump4,
    Jump5,
    Jump6,
    Jump7,
    Jump8,
    Jump9,
    // [dashboard]
    MoveDown,
    MoveUp,
    MoveLeft,
    MoveRight,
    Filter,
    Rename,
    Help,
    FocusPane,
    ClearFilter,
    ApprovePermission,
    DenyPermission,
}

/// Static description of one action: which section it lives in, its config
/// key name, its default key notation, and a human label (for the Round 3
/// help overlay / hints bar).
pub struct ActionSpec {
    pub action: Action,
    pub section: Section,
    pub name: &'static str,
    pub default: &'static str,
    pub description: &'static str,
}

/// The single source of truth for the action set. Order matters: it is the
/// canonical definition order for conflict resolution and for help/hints
/// generation in Round 3.
///
/// The `default` notations mirror the authoritative hardcoded checks in
/// `src/ui.rs` as of this branch:
/// - global Ctrl+ shortcuts: `Ctrl+d` (dashboard/command mode), `Ctrl+n`
///   (new pane), `Ctrl+w` (close pane), `Ctrl+t` (toggle layout); quit is
///   `Ctrl+c` (opens the quit-confirm flow); `1`..`9` jump to a card.
/// - dashboard Normal-mode keys: `j`/`k`/`h`/`l`, `/`, `r`, `?`, `Enter`,
///   `Esc`, `y`, `n`.
pub const ACTIONS: &[ActionSpec] = &[
    // [global]
    ActionSpec {
        action: Action::Quit,
        section: Section::Global,
        name: "quit",
        default: "Ctrl+c",
        description: "Quit",
    },
    ActionSpec {
        action: Action::Dashboard,
        section: Section::Global,
        name: "dashboard",
        default: "Ctrl+d",
        description: "Dashboard (command mode)",
    },
    ActionSpec {
        action: Action::NewPane,
        section: Section::Global,
        name: "new_pane",
        default: "Ctrl+n",
        description: "New pane",
    },
    ActionSpec {
        action: Action::ClosePane,
        section: Section::Global,
        name: "close_pane",
        default: "Ctrl+w",
        description: "Close pane",
    },
    ActionSpec {
        action: Action::ToggleLayout,
        section: Section::Global,
        name: "toggle_layout",
        default: "Ctrl+t",
        description: "Toggle layout",
    },
    ActionSpec {
        action: Action::Jump1,
        section: Section::Global,
        name: "jump_1",
        default: "1",
        description: "Jump to card 1",
    },
    ActionSpec {
        action: Action::Jump2,
        section: Section::Global,
        name: "jump_2",
        default: "2",
        description: "Jump to card 2",
    },
    ActionSpec {
        action: Action::Jump3,
        section: Section::Global,
        name: "jump_3",
        default: "3",
        description: "Jump to card 3",
    },
    ActionSpec {
        action: Action::Jump4,
        section: Section::Global,
        name: "jump_4",
        default: "4",
        description: "Jump to card 4",
    },
    ActionSpec {
        action: Action::Jump5,
        section: Section::Global,
        name: "jump_5",
        default: "5",
        description: "Jump to card 5",
    },
    ActionSpec {
        action: Action::Jump6,
        section: Section::Global,
        name: "jump_6",
        default: "6",
        description: "Jump to card 6",
    },
    ActionSpec {
        action: Action::Jump7,
        section: Section::Global,
        name: "jump_7",
        default: "7",
        description: "Jump to card 7",
    },
    ActionSpec {
        action: Action::Jump8,
        section: Section::Global,
        name: "jump_8",
        default: "8",
        description: "Jump to card 8",
    },
    ActionSpec {
        action: Action::Jump9,
        section: Section::Global,
        name: "jump_9",
        default: "9",
        description: "Jump to card 9",
    },
    // [dashboard]
    ActionSpec {
        action: Action::MoveDown,
        section: Section::Dashboard,
        name: "move_down",
        default: "j",
        description: "Move down",
    },
    ActionSpec {
        action: Action::MoveUp,
        section: Section::Dashboard,
        name: "move_up",
        default: "k",
        description: "Move up",
    },
    ActionSpec {
        action: Action::MoveLeft,
        section: Section::Dashboard,
        name: "move_left",
        default: "h",
        description: "Move left / previous tab",
    },
    ActionSpec {
        action: Action::MoveRight,
        section: Section::Dashboard,
        name: "move_right",
        default: "l",
        description: "Move right / next tab",
    },
    ActionSpec {
        action: Action::Filter,
        section: Section::Dashboard,
        name: "filter",
        default: "/",
        description: "Filter",
    },
    ActionSpec {
        action: Action::Rename,
        section: Section::Dashboard,
        name: "rename",
        default: "r",
        description: "Rename",
    },
    ActionSpec {
        action: Action::Help,
        section: Section::Dashboard,
        name: "help",
        default: "?",
        description: "Help",
    },
    ActionSpec {
        action: Action::FocusPane,
        section: Section::Dashboard,
        name: "focus_pane",
        default: "Enter",
        description: "Focus pane",
    },
    ActionSpec {
        action: Action::ClearFilter,
        section: Section::Dashboard,
        name: "clear_filter",
        default: "Esc",
        description: "Clear filter",
    },
    ActionSpec {
        action: Action::ApprovePermission,
        section: Section::Dashboard,
        name: "approve_permission",
        default: "y",
        description: "Approve permission",
    },
    ActionSpec {
        action: Action::DenyPermission,
        section: Section::Dashboard,
        name: "deny_permission",
        default: "n",
        description: "Deny permission",
    },
];

impl Action {
    /// Look up the static spec for this action.
    pub fn spec(self) -> &'static ActionSpec {
        ACTIONS
            .iter()
            .find(|s| s.action == self)
            .expect("every Action variant has an ACTIONS entry")
    }

    /// Section this action belongs to.
    pub fn section(self) -> Section {
        self.spec().section
    }

    /// Config key name (e.g. `"toggle_layout"`).
    pub fn config_name(self) -> &'static str {
        self.spec().name
    }

    /// Default key notation (e.g. `"Ctrl+t"`).
    pub fn default_notation(self) -> &'static str {
        self.spec().default
    }

    /// Human-readable label for help / hints (Round 3).
    pub fn description(self) -> &'static str {
        self.spec().description
    }

    /// Find an action by `(section, name)`. Returns `None` for unknown names.
    pub fn from_section_name(section: Section, name: &str) -> Option<Action> {
        ACTIONS
            .iter()
            .find(|s| s.section == section && s.name == name)
            .map(|s| s.action)
    }
}

/// A single parsed key binding.
///
/// `chord == None` means the action is **unbound** (config value `""`): it is
/// a distinct, matchable-as-never state — [`matches_binding`] returns `false`
/// for every key event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    chord: Option<(KeyCode, KeyModifiers)>,
}

impl Binding {
    /// A bound key chord.
    pub fn bound(code: KeyCode, mods: KeyModifiers) -> Self {
        Binding {
            chord: Some((code, mods)),
        }
    }

    /// The explicitly-unbound state (config value `""`).
    pub fn unbound() -> Self {
        Binding { chord: None }
    }

    /// `true` if this binding is unbound (matches no key).
    pub fn is_unbound(&self) -> bool {
        self.chord.is_none()
    }

    /// The parsed `(KeyCode, KeyModifiers)`, or `None` if unbound.
    pub fn chord(&self) -> Option<(KeyCode, KeyModifiers)> {
        self.chord
    }

    /// Render this binding back to its key-notation string (e.g. `"Ctrl+t"`,
    /// `"Alt+Shift+l"`, `"F1"`). An unbound binding renders to the empty
    /// string. Used to drive the help overlay / hints bar from the active
    /// config so they always show the user's real keys.
    pub fn notation(&self) -> String {
        match self.chord {
            None => String::new(),
            Some((code, mods)) => render_chord(code, mods),
        }
    }
}

/// Parse a key-notation string into a [`Binding`].
///
/// - `""` (or all-whitespace) → unbound.
/// - Modifiers `Alt+` / `Ctrl+` (or `Control+`) / `Shift+`, case-insensitive,
///   in any order, may combine: `"Alt+Shift+t"`.
/// - Special keys (case-insensitive): `Enter`, `Esc`/`Escape`, `Tab`,
///   `Space`, `Up`, `Down`, `Left`, `Right`, `Backspace`, `Delete`, `Home`,
///   `End`, `PageUp`, `PageDown`, `Insert`, and `F1`..`F12`.
/// - A single printable character: `j`, `/`, `?`, `1`, …
/// - Anything else (junk) → `Err`.
pub fn parse_binding(notation: &str) -> Result<Binding, String> {
    let trimmed = notation.trim();
    if trimmed.is_empty() {
        return Ok(Binding::unbound());
    }

    let parts: Vec<&str> = trimmed.split('+').collect();
    // The final segment is the key; everything before it is a modifier.
    let (key_part, modifier_parts) = parts
        .split_last()
        .expect("split never yields an empty vec on a non-empty string");

    let mut mods = KeyModifiers::NONE;
    for m in modifier_parts {
        match m.to_ascii_lowercase().as_str() {
            "alt" => mods |= KeyModifiers::ALT,
            "ctrl" | "control" => mods |= KeyModifiers::CONTROL,
            "shift" => mods |= KeyModifiers::SHIFT,
            other => return Err(format!("unknown modifier '{other}' in '{notation}'")),
        }
    }

    let code = parse_key_code(key_part)
        .ok_or_else(|| format!("unknown key '{key_part}' in '{notation}'"))?;

    Ok(Binding::bound(code, mods))
}

/// Parse the key portion (no modifiers) of a notation string.
fn parse_key_code(key: &str) -> Option<KeyCode> {
    // Named keys are case-insensitive.
    match key.to_ascii_lowercase().as_str() {
        "enter" => return Some(KeyCode::Enter),
        "esc" | "escape" => return Some(KeyCode::Esc),
        "tab" => return Some(KeyCode::Tab),
        "space" => return Some(KeyCode::Char(' ')),
        "up" => return Some(KeyCode::Up),
        "down" => return Some(KeyCode::Down),
        "left" => return Some(KeyCode::Left),
        "right" => return Some(KeyCode::Right),
        "backspace" => return Some(KeyCode::Backspace),
        "delete" | "del" => return Some(KeyCode::Delete),
        "home" => return Some(KeyCode::Home),
        "end" => return Some(KeyCode::End),
        "pageup" => return Some(KeyCode::PageUp),
        "pagedown" => return Some(KeyCode::PageDown),
        "insert" => return Some(KeyCode::Insert),
        _ => {}
    }

    // Function keys: F1..F12 (case-insensitive prefix).
    if let Some(rest) = key.strip_prefix('F').or_else(|| key.strip_prefix('f'))
        && let Ok(n) = rest.parse::<u8>()
        && (1..=12).contains(&n)
    {
        return Some(KeyCode::F(n));
    }

    // A single printable character (counted by Unicode scalar value).
    let mut chars = key.chars();
    if let Some(c) = chars.next()
        && chars.next().is_none()
    {
        return Some(KeyCode::Char(c));
    }

    None
}

/// `true` iff `key` matches `binding` and only that binding. An unbound
/// binding matches nothing.
///
/// Both sides are normalized first (see [`normalize_chord`]) so that a
/// shifted letter is compared case-folded: e.g. a binding parsed from
/// `"Alt+Shift+l"` (`Char('l')` + ALT|SHIFT) matches the crossterm event a
/// terminal actually delivers for that chord (`Char('L')` + ALT, with Shift
/// folded into the capital). Without this, legacy terminals — which encode
/// Shift into the character rather than the modifier bitset — would never
/// match a `Shift+<letter>` binding.
pub fn matches_binding(key: &KeyEvent, binding: &Binding) -> bool {
    match binding.chord {
        None => false,
        Some((code, mods)) => {
            normalize_chord(key.code, key.modifiers) == normalize_chord(code, mods)
        }
    }
}

/// Canonicalize a `(KeyCode, KeyModifiers)` for comparison: for an ASCII
/// alphabetic character, fold a `SHIFT` modifier into the uppercase form and
/// drop the `SHIFT` bit. This reconciles the two ways a shifted letter can
/// arrive — `(Char('l'), SHIFT)` (enhanced/kitty protocol or an explicit
/// notation) vs `(Char('L'), NONE)` (legacy terminals) — into one form.
fn normalize_chord(code: KeyCode, mods: KeyModifiers) -> (KeyCode, KeyModifiers) {
    if let KeyCode::Char(c) = code
        && c.is_ascii_alphabetic()
    {
        let folded = if mods.contains(KeyModifiers::SHIFT) {
            c.to_ascii_uppercase()
        } else {
            c
        };
        return (KeyCode::Char(folded), mods - KeyModifiers::SHIFT);
    }
    (code, mods)
}

/// Raw TOML shape: `[global]` and `[dashboard]` tables of `name = "notation"`.
#[derive(Debug, Default, Deserialize)]
struct RawKeybindings {
    #[serde(default)]
    global: HashMap<String, String>,
    #[serde(default)]
    dashboard: HashMap<String, String>,
}

/// The resolved keybinding configuration: one [`Binding`] per [`Action`].
///
/// `Default` reproduces today's hardcoded bindings exactly. User config is
/// layered on via [`KeybindingConfig::from_toml_str`] / [`KeybindingConfig::load`].
#[derive(Debug, Clone)]
pub struct KeybindingConfig {
    bindings: HashMap<Action, Binding>,
}

impl Default for KeybindingConfig {
    fn default() -> Self {
        let mut bindings = HashMap::new();
        for spec in ACTIONS {
            let binding = parse_binding(spec.default).unwrap_or_else(|e| {
                panic!(
                    "built-in default '{}' for {:?} is invalid: {e}",
                    spec.default, spec.action
                )
            });
            bindings.insert(spec.action, binding);
        }
        KeybindingConfig { bindings }
    }
}

impl KeybindingConfig {
    /// The binding for an action. Always present (defaults cover every action).
    pub fn binding(&self, action: Action) -> Binding {
        self.bindings
            .get(&action)
            .copied()
            .unwrap_or_else(Binding::unbound)
    }

    /// The key-notation string for an action's active binding (`""` if
    /// unbound). Convenience for help / hints generation.
    pub fn notation(&self, action: Action) -> String {
        self.binding(action).notation()
    }

    /// Override a single action's binding in-memory (used by tests and by the
    /// merge logic).
    pub fn set(&mut self, action: Action, binding: Binding) {
        self.bindings.insert(action, binding);
    }

    /// `true` iff `key` triggers `action` under this config.
    pub fn matches(&self, action: Action, key: &KeyEvent) -> bool {
        matches_binding(key, &self.binding(action))
    }

    /// Find the (first, in canonical order) action triggered by `key`, if any.
    /// Round 3 uses this; conflict resolution guarantees at most one match.
    pub fn action_for(&self, key: &KeyEvent) -> Option<Action> {
        ACTIONS
            .iter()
            .map(|s| s.action)
            .find(|&a| self.matches(a, key))
    }

    /// Build a config from TOML text: start from defaults, apply overrides.
    ///
    /// Returns `Err` only when the TOML itself is syntactically invalid
    /// (caller falls back to defaults). When the TOML parses, per-entry
    /// problems are *not* fatal — they are collected as warning strings and
    /// the rest of the config is still applied:
    /// - unknown action name → ignored + warning,
    /// - unparseable notation → keep default + warning,
    /// - two actions on the same key → first-defined (canonical order) wins,
    ///   the later one is unbound + warning.
    ///
    /// The returned warnings let tests assert behavior without scraping
    /// stderr; [`KeybindingConfig::load`] prints them.
    pub fn from_toml_str(contents: &str) -> Result<(KeybindingConfig, Vec<String>), String> {
        let raw: RawKeybindings =
            toml::from_str(contents).map_err(|e| format!("invalid keybindings TOML: {e}"))?;
        Ok(Self::from_raw(raw))
    }

    fn from_raw(raw: RawKeybindings) -> (KeybindingConfig, Vec<String>) {
        let mut config = KeybindingConfig::default();
        let mut warnings = Vec::new();

        for (section, table) in [
            (Section::Global, &raw.global),
            (Section::Dashboard, &raw.dashboard),
        ] {
            for (name, notation) in table {
                let Some(action) = Action::from_section_name(section, name) else {
                    warnings.push(format!(
                        "unknown keybinding action '[{}] {}' — ignored",
                        section.as_str(),
                        name
                    ));
                    continue;
                };
                match parse_binding(notation) {
                    Ok(binding) => config.set(action, binding),
                    Err(e) => warnings.push(format!(
                        "invalid binding for '[{}] {}' ({e}) — keeping default",
                        section.as_str(),
                        name
                    )),
                }
            }
        }

        Self::resolve_conflicts(&mut config, &mut warnings);
        (config, warnings)
    }

    /// First-defined (canonical [`ACTIONS`] order) wins: if two actions share
    /// the same chord, the earlier action keeps it and every later conflicting
    /// action is unbound with a warning.
    ///
    /// Dedup is keyed on the *normalized* chord (the same [`normalize_chord`]
    /// folding [`matches_binding`] applies), so two bindings that are distinct
    /// as written but collapse to the same key event — e.g. `"Shift+d"` and
    /// `"D"` — are detected as a conflict instead of both silently firing.
    fn resolve_conflicts(config: &mut KeybindingConfig, warnings: &mut Vec<String>) {
        let mut claimed: HashMap<(KeyCode, KeyModifiers), Action> = HashMap::new();
        for spec in ACTIONS {
            let action = spec.action;
            let Some((code, mods)) = config.binding(action).chord() else {
                continue;
            };
            let chord = normalize_chord(code, mods);
            if let Some(&winner) = claimed.get(&chord) {
                warnings.push(format!(
                    "keybinding conflict: '{}' and '{}' both bound to '{}' — '{}' wins, '{}' unbound",
                    winner.config_name(),
                    action.config_name(),
                    action.default_or_current(config),
                    winner.config_name(),
                    action.config_name(),
                ));
                config.set(action, Binding::unbound());
            } else {
                claimed.insert(chord, action);
            }
        }
    }

    /// File-loading entrypoint, mirroring `DashboardConfig::load`.
    ///
    /// Path resolution: `$DOT_AGENT_DECK_KEYBINDINGS` if set, else
    /// `~/.config/dot-agent-deck/keybindings.toml`. Missing file → all
    /// defaults. Malformed TOML → warn on stderr + all defaults. Valid TOML
    /// with per-entry problems → warnings on stderr + best-effort merge.
    pub fn load() -> Self {
        let path = keybindings_path();
        match std::fs::read_to_string(&path) {
            Ok(contents) => match Self::from_toml_str(&contents) {
                Ok((config, warnings)) => {
                    for w in &warnings {
                        // SECURITY: warnings embed attacker-controlled text
                        // (notation, action names, TOML-parser snippets) from a
                        // possibly-planted keybindings.toml, and they print to
                        // the live terminal *before* the alt-screen. Sanitize
                        // control/ESC bytes so a planted file can't inject
                        // terminal escape sequences.
                        eprintln!(
                            "keybindings ({}): {}",
                            path.display(),
                            sanitize_for_terminal(w)
                        );
                    }
                    config
                }
                Err(err) => {
                    eprintln!(
                        "Invalid keybindings at {}: {}",
                        path.display(),
                        sanitize_for_terminal(&err)
                    );
                    Self::default()
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(err) => {
                eprintln!(
                    "Failed to read keybindings at {}: {}",
                    path.display(),
                    sanitize_for_terminal(&err.to_string())
                );
                Self::default()
            }
        }
    }
}

/// Escape control characters (including ESC `0x1b`, CR, LF, etc.) in a string
/// destined for the live terminal, leaving printable and non-ASCII characters
/// intact. Used on every keybinding warning so a planted `keybindings.toml`
/// cannot smuggle terminal escape sequences through the pre-alt-screen stderr
/// output (the warning text echoes attacker-controlled notation / action names
/// / TOML-parser snippets verbatim).
fn sanitize_for_terminal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

impl Action {
    /// Best-effort notation string for an action's *current* binding, used
    /// only to make conflict warnings readable. Falls back to the default
    /// notation when the binding can't be rendered.
    fn default_or_current(self, config: &KeybindingConfig) -> String {
        match config.binding(self).chord() {
            Some((code, mods)) => render_chord(code, mods),
            None => self.default_notation().to_string(),
        }
    }
}

/// Render a `(KeyCode, KeyModifiers)` back to notation, for warning messages.
fn render_chord(code: KeyCode, mods: KeyModifiers) -> String {
    let mut s = String::new();
    if mods.contains(KeyModifiers::CONTROL) {
        s.push_str("Ctrl+");
    }
    if mods.contains(KeyModifiers::ALT) {
        s.push_str("Alt+");
    }
    if mods.contains(KeyModifiers::SHIFT) {
        s.push_str("Shift+");
    }
    let key = match code {
        KeyCode::Char(' ') => "Space".to_string(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "Enter".to_string(),
        KeyCode::Esc => "Esc".to_string(),
        KeyCode::Tab => "Tab".to_string(),
        KeyCode::Up => "Up".to_string(),
        KeyCode::Down => "Down".to_string(),
        KeyCode::Left => "Left".to_string(),
        KeyCode::Right => "Right".to_string(),
        KeyCode::Backspace => "Backspace".to_string(),
        KeyCode::Delete => "Delete".to_string(),
        KeyCode::Home => "Home".to_string(),
        KeyCode::End => "End".to_string(),
        KeyCode::PageUp => "PageUp".to_string(),
        KeyCode::PageDown => "PageDown".to_string(),
        KeyCode::Insert => "Insert".to_string(),
        KeyCode::F(n) => format!("F{n}"),
        other => format!("{other:?}"),
    };
    s.push_str(&key);
    s
}

/// Path of the keybindings config file. `$DOT_AGENT_DECK_KEYBINDINGS`
/// overrides (tests use this); otherwise
/// `~/.config/dot-agent-deck/keybindings.toml`, matching the `config.toml`
/// path convention in `DashboardConfig`.
fn keybindings_path() -> PathBuf {
    if let Ok(dir) = std::env::var("DOT_AGENT_DECK_KEYBINDINGS") {
        return PathBuf::from(dir);
    }
    dirs_home().join(".config/dot-agent-deck/keybindings.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    // ---- notation parsing -------------------------------------------------

    #[test]
    fn parse_modifier_combo() {
        let b = parse_binding("Alt+Shift+t").unwrap();
        assert_eq!(
            b.chord(),
            Some((KeyCode::Char('t'), KeyModifiers::ALT | KeyModifiers::SHIFT))
        );
    }

    #[test]
    fn parse_ctrl_letter() {
        let b = parse_binding("Ctrl+n").unwrap();
        assert_eq!(b.chord(), Some((KeyCode::Char('n'), KeyModifiers::CONTROL)));
    }

    #[test]
    fn parse_control_alias() {
        assert_eq!(
            parse_binding("Control+d").unwrap().chord(),
            Some((KeyCode::Char('d'), KeyModifiers::CONTROL))
        );
    }

    #[test]
    fn parse_named_keys() {
        assert_eq!(
            parse_binding("Enter").unwrap().chord(),
            Some((KeyCode::Enter, KeyModifiers::NONE))
        );
        assert_eq!(
            parse_binding("Esc").unwrap().chord(),
            Some((KeyCode::Esc, KeyModifiers::NONE))
        );
        assert_eq!(
            parse_binding("escape").unwrap().chord(),
            Some((KeyCode::Esc, KeyModifiers::NONE))
        );
        assert_eq!(
            parse_binding("Space").unwrap().chord(),
            Some((KeyCode::Char(' '), KeyModifiers::NONE))
        );
        assert_eq!(
            parse_binding("Up").unwrap().chord(),
            Some((KeyCode::Up, KeyModifiers::NONE))
        );
    }

    #[test]
    fn parse_function_key() {
        assert_eq!(
            parse_binding("F1").unwrap().chord(),
            Some((KeyCode::F(1), KeyModifiers::NONE))
        );
        assert_eq!(
            parse_binding("Alt+Shift+f12").unwrap().chord(),
            Some((KeyCode::F(12), KeyModifiers::ALT | KeyModifiers::SHIFT))
        );
    }

    #[test]
    fn parse_printable_punctuation() {
        assert_eq!(
            parse_binding("/").unwrap().chord(),
            Some((KeyCode::Char('/'), KeyModifiers::NONE))
        );
        assert_eq!(
            parse_binding("?").unwrap().chord(),
            Some((KeyCode::Char('?'), KeyModifiers::NONE))
        );
    }

    #[test]
    fn parse_empty_is_unbound() {
        let b = parse_binding("").unwrap();
        assert!(b.is_unbound());
        assert_eq!(b.chord(), None);
        // whitespace-only is also unbound
        assert!(parse_binding("   ").unwrap().is_unbound());
    }

    #[test]
    fn parse_junk_is_rejected() {
        assert!(parse_binding("Hyper+x").is_err()); // unknown modifier
        assert!(parse_binding("Banana").is_err()); // unknown multi-char key
        assert!(parse_binding("Ctrl+Nope").is_err()); // unknown key after modifier
        assert!(parse_binding("F13").is_err()); // out-of-range function key
    }

    // ---- matches_binding --------------------------------------------------

    #[test]
    fn matches_only_the_bound_chord() {
        let b = parse_binding("Ctrl+n").unwrap();
        assert!(matches_binding(
            &ev(KeyCode::Char('n'), KeyModifiers::CONTROL),
            &b
        ));
        // wrong modifier
        assert!(!matches_binding(
            &ev(KeyCode::Char('n'), KeyModifiers::NONE),
            &b
        ));
        // wrong key
        assert!(!matches_binding(
            &ev(KeyCode::Char('m'), KeyModifiers::CONTROL),
            &b
        ));
        // extra modifier
        assert!(!matches_binding(
            &ev(
                KeyCode::Char('n'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            ),
            &b
        ));
    }

    #[test]
    fn shift_letter_matches_legacy_capital_event() {
        // "Alt+Shift+l" parses to (Char('l'), ALT|SHIFT) but a legacy
        // terminal delivers Alt+Shift+l as (Char('L'), ALT) — Shift folded
        // into the capital. Normalization must make these match.
        let b = parse_binding("Alt+Shift+l").unwrap();
        assert!(matches_binding(
            &ev(KeyCode::Char('L'), KeyModifiers::ALT),
            &b
        ));
        // and the enhanced-protocol form (Char('l'), ALT|SHIFT) matches too
        assert!(matches_binding(
            &ev(KeyCode::Char('l'), KeyModifiers::ALT | KeyModifiers::SHIFT),
            &b
        ));
        // a different letter does not
        assert!(!matches_binding(
            &ev(KeyCode::Char('K'), KeyModifiers::ALT),
            &b
        ));
    }

    #[test]
    fn notation_round_trips() {
        assert_eq!(parse_binding("Ctrl+t").unwrap().notation(), "Ctrl+t");
        assert_eq!(
            parse_binding("Alt+Shift+l").unwrap().notation(),
            "Alt+Shift+l"
        );
        assert_eq!(parse_binding("F1").unwrap().notation(), "F1");
        assert_eq!(parse_binding("?").unwrap().notation(), "?");
        assert_eq!(parse_binding("").unwrap().notation(), "");
        let c = KeybindingConfig::default();
        assert_eq!(c.notation(Action::Quit), "Ctrl+c");
        assert_eq!(c.notation(Action::Help), "?");
    }

    #[test]
    fn unbound_matches_nothing() {
        let b = parse_binding("").unwrap();
        assert!(!matches_binding(
            &ev(KeyCode::Char('n'), KeyModifiers::CONTROL),
            &b
        ));
        assert!(!matches_binding(
            &ev(KeyCode::Enter, KeyModifiers::NONE),
            &b
        ));
    }

    // ---- defaults mirror today's hardcoded bindings -----------------------

    #[test]
    fn defaults_match_current_hardcoded_keys() {
        let c = KeybindingConfig::default();
        assert!(c.matches(Action::Quit, &ev(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(c.matches(
            Action::Dashboard,
            &ev(KeyCode::Char('d'), KeyModifiers::CONTROL)
        ));
        assert!(c.matches(
            Action::NewPane,
            &ev(KeyCode::Char('n'), KeyModifiers::CONTROL)
        ));
        assert!(c.matches(
            Action::ClosePane,
            &ev(KeyCode::Char('w'), KeyModifiers::CONTROL)
        ));
        assert!(c.matches(
            Action::ToggleLayout,
            &ev(KeyCode::Char('t'), KeyModifiers::CONTROL)
        ));
        assert!(c.matches(Action::Jump1, &ev(KeyCode::Char('1'), KeyModifiers::NONE)));
        assert!(c.matches(Action::Jump9, &ev(KeyCode::Char('9'), KeyModifiers::NONE)));
        assert!(c.matches(
            Action::MoveDown,
            &ev(KeyCode::Char('j'), KeyModifiers::NONE)
        ));
        assert!(c.matches(Action::MoveUp, &ev(KeyCode::Char('k'), KeyModifiers::NONE)));
        assert!(c.matches(
            Action::MoveLeft,
            &ev(KeyCode::Char('h'), KeyModifiers::NONE)
        ));
        assert!(c.matches(
            Action::MoveRight,
            &ev(KeyCode::Char('l'), KeyModifiers::NONE)
        ));
        assert!(c.matches(Action::Filter, &ev(KeyCode::Char('/'), KeyModifiers::NONE)));
        assert!(c.matches(Action::Rename, &ev(KeyCode::Char('r'), KeyModifiers::NONE)));
        assert!(c.matches(Action::Help, &ev(KeyCode::Char('?'), KeyModifiers::NONE)));
        assert!(c.matches(Action::FocusPane, &ev(KeyCode::Enter, KeyModifiers::NONE)));
        assert!(c.matches(Action::ClearFilter, &ev(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(c.matches(
            Action::ApprovePermission,
            &ev(KeyCode::Char('y'), KeyModifiers::NONE)
        ));
        assert!(c.matches(
            Action::DenyPermission,
            &ev(KeyCode::Char('n'), KeyModifiers::NONE)
        ));
    }

    // ---- merge precedence -------------------------------------------------

    #[test]
    fn merge_user_override_wins_unspecified_keep_default() {
        let toml = r#"
[global]
toggle_layout = "Alt+Shift+l"
"#;
        let (c, warnings) = KeybindingConfig::from_toml_str(toml).unwrap();
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        // overridden
        assert!(c.matches(
            Action::ToggleLayout,
            &ev(KeyCode::Char('l'), KeyModifiers::ALT | KeyModifiers::SHIFT)
        ));
        // old default no longer triggers it
        assert!(!c.matches(
            Action::ToggleLayout,
            &ev(KeyCode::Char('t'), KeyModifiers::CONTROL)
        ));
        // unspecified action keeps its default
        assert!(c.matches(
            Action::NewPane,
            &ev(KeyCode::Char('n'), KeyModifiers::CONTROL)
        ));
    }

    #[test]
    fn merge_dashboard_section() {
        let toml = r#"
[dashboard]
help = "F1"
"#;
        let (c, warnings) = KeybindingConfig::from_toml_str(toml).unwrap();
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        assert!(c.matches(Action::Help, &ev(KeyCode::F(1), KeyModifiers::NONE)));
        assert!(!c.matches(Action::Help, &ev(KeyCode::Char('?'), KeyModifiers::NONE)));
    }

    #[test]
    fn unbind_via_empty_string() {
        let toml = r#"
[global]
new_pane = ""
"#;
        let (c, warnings) = KeybindingConfig::from_toml_str(toml).unwrap();
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        assert!(c.binding(Action::NewPane).is_unbound());
        assert!(!c.matches(
            Action::NewPane,
            &ev(KeyCode::Char('n'), KeyModifiers::CONTROL)
        ));
    }

    // ---- conflict detection (first-defined wins) --------------------------

    #[test]
    fn conflict_first_defined_wins() {
        // Bind both new_pane and close_pane to the same chord. new_pane is
        // earlier in canonical ACTIONS order, so it wins; close_pane is
        // unbound and a warning is produced.
        let toml = r#"
[global]
new_pane = "Ctrl+x"
close_pane = "Ctrl+x"
"#;
        let (c, warnings) = KeybindingConfig::from_toml_str(toml).unwrap();
        assert!(c.matches(
            Action::NewPane,
            &ev(KeyCode::Char('x'), KeyModifiers::CONTROL)
        ));
        assert!(c.binding(Action::ClosePane).is_unbound());
        assert_eq!(warnings.len(), 1, "warnings: {warnings:?}");
        assert!(warnings[0].contains("conflict"));
    }

    #[test]
    fn conflict_with_default_binding() {
        // Rebind move_up onto move_down's default ("j"). move_down is earlier,
        // so it keeps "j" and the user's move_up override is dropped.
        let toml = r#"
[dashboard]
move_up = "j"
"#;
        let (c, warnings) = KeybindingConfig::from_toml_str(toml).unwrap();
        assert!(c.matches(
            Action::MoveDown,
            &ev(KeyCode::Char('j'), KeyModifiers::NONE)
        ));
        assert!(c.binding(Action::MoveUp).is_unbound());
        assert_eq!(warnings.len(), 1, "warnings: {warnings:?}");
        assert!(warnings[0].contains("conflict"));
    }

    #[test]
    fn no_false_conflict_between_unbound_actions() {
        // Two actions unbound to "" must NOT be reported as conflicting.
        let toml = r#"
[global]
new_pane = ""
close_pane = ""
"#;
        let (c, warnings) = KeybindingConfig::from_toml_str(toml).unwrap();
        assert!(c.binding(Action::NewPane).is_unbound());
        assert!(c.binding(Action::ClosePane).is_unbound());
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    }

    #[test]
    fn normalized_equivalent_bindings_conflict_and_dispatch_is_unique() {
        // "Shift+d" -> (Char('d'), SHIFT) and "D" -> (Char('D'), NONE) are
        // distinct as written but fold to the SAME normalized event the
        // terminal delivers. Conflict detection must catch this (it dedups on
        // the normalized chord, matching matches_binding) so they don't both
        // fire. dashboard is earlier in ACTIONS order, so it wins; new_pane is
        // unbound.
        let toml = r#"
[global]
dashboard = "Shift+d"
new_pane = "D"
"#;
        let (c, warnings) = KeybindingConfig::from_toml_str(toml).unwrap();
        assert!(c.matches(
            Action::Dashboard,
            &ev(KeyCode::Char('D'), KeyModifiers::NONE)
        ));
        assert!(
            c.binding(Action::NewPane).is_unbound(),
            "later normalized-equivalent binding must be unbound, not silently co-firing"
        );
        assert_eq!(
            warnings.iter().filter(|w| w.contains("conflict")).count(),
            1,
            "exactly one conflict warning expected: {warnings:?}"
        );

        // (b) dispatch yields AT MOST ONE action for the colliding event, in
        // both the capital-letter and Shift+letter encodings a terminal might
        // send.
        for event in [
            ev(KeyCode::Char('D'), KeyModifiers::NONE),
            ev(KeyCode::Char('d'), KeyModifiers::SHIFT),
        ] {
            let matched: Vec<Action> = ACTIONS
                .iter()
                .map(|s| s.action)
                .filter(|&a| c.matches(a, &event))
                .collect();
            assert_eq!(
                matched,
                vec![Action::Dashboard],
                "exactly one action may fire for {event:?}, got {matched:?}"
            );
            assert_eq!(c.action_for(&event), Some(Action::Dashboard));
        }
    }

    // ---- unknown action ignored -------------------------------------------

    #[test]
    fn unknown_action_ignored_with_warning() {
        let toml = r#"
[global]
teleport = "Ctrl+z"

[dashboard]
move_down = "k"
"#;
        let (c, warnings) = KeybindingConfig::from_toml_str(toml).unwrap();
        // the known override still applied
        assert!(c.matches(
            Action::MoveDown,
            &ev(KeyCode::Char('k'), KeyModifiers::NONE)
        ));
        // unknown produced exactly one warning
        let unknowns: Vec<_> = warnings.iter().filter(|w| w.contains("unknown")).collect();
        assert_eq!(unknowns.len(), 1, "warnings: {warnings:?}");
        assert!(unknowns[0].contains("teleport"));
    }

    #[test]
    fn invalid_notation_keeps_default_with_warning() {
        let toml = r#"
[global]
toggle_layout = "Banana"
"#;
        let (c, warnings) = KeybindingConfig::from_toml_str(toml).unwrap();
        // default retained
        assert!(c.matches(
            Action::ToggleLayout,
            &ev(KeyCode::Char('t'), KeyModifiers::CONTROL)
        ));
        assert_eq!(warnings.len(), 1, "warnings: {warnings:?}");
        assert!(warnings[0].contains("invalid"));
    }

    #[test]
    fn malformed_toml_is_err() {
        assert!(KeybindingConfig::from_toml_str("this is not = = toml [[[").is_err());
    }

    #[test]
    fn empty_toml_is_all_defaults() {
        let (c, warnings) = KeybindingConfig::from_toml_str("").unwrap();
        assert!(warnings.is_empty());
        assert!(c.matches(
            Action::ToggleLayout,
            &ev(KeyCode::Char('t'), KeyModifiers::CONTROL)
        ));
    }

    #[test]
    fn action_for_returns_first_match() {
        let c = KeybindingConfig::default();
        assert_eq!(
            c.action_for(&ev(KeyCode::Char('n'), KeyModifiers::CONTROL)),
            Some(Action::NewPane)
        );
        assert_eq!(
            c.action_for(&ev(KeyCode::Char('z'), KeyModifiers::CONTROL)),
            None
        );
    }
}
