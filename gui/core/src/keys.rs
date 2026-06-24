//! GUI navigation bindings, resolved from the shared `keybindings` crate
//! (PRD #176).
//!
//! The webview navigates with the SAME shortcuts as the TUI — a maintainer
//! requirement (2026-06-24): switching front-ends must not re-train muscle
//! memory. So instead of hardcoding keys in JS, the GUI core loads the user's
//! [`KeybindingConfig`] (the exact source the TUI uses — their
//! `~/.config/dot-agent-deck/keybindings.toml` plus identical compiled-in
//! defaults) and projects the chrome-navigation actions into a small
//! DOM-shaped table the webview matches `KeyboardEvent`s against. Remap a key
//! in `keybindings.toml` and BOTH front-ends move together, by construction.
//!
//! Only the *navigation* subset is projected here (the leader, deck/pane
//! movement, focus, and the nine jump-to-pane digits). The webview also honors
//! the TUI's non-configurable arrow aliases (`←`/`→`/`↑`/`↓`) on its own side.

use crossterm::event::{KeyCode, KeyModifiers};
use keybindings::{Action, KeybindingConfig};
use serde::Serialize;

/// One navigation binding projected for the webview: the stable action name
/// plus the resolved chord expressed in DOM `KeyboardEvent` terms — the
/// `event.key` string and the modifier booleans — so the frontend matches it
/// without re-parsing notation or depending on crossterm.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KeyBinding {
    /// The action's stable config name (e.g. `"move_left"`, `"jump_1"`).
    pub action: String,
    /// DOM `KeyboardEvent.key` value (e.g. `"h"`, `"1"`, `"Enter"`).
    pub key: String,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

/// The actions the GUI chrome navigates with — the leader (`dashboard`),
/// deck/pane movement, focus, and the nine jump-to-pane digits. Other TUI
/// actions (new/close pane, rename, …) are separate slices.
const NAV_ACTIONS: &[Action] = &[
    Action::Dashboard,
    Action::MoveLeft,
    Action::MoveRight,
    Action::MoveUp,
    Action::MoveDown,
    Action::FocusPane,
    Action::Jump1,
    Action::Jump2,
    Action::Jump3,
    Action::Jump4,
    Action::Jump5,
    Action::Jump6,
    Action::Jump7,
    Action::Jump8,
    Action::Jump9,
];

/// Load the user's keybindings and project the navigation actions for the
/// webview. Reads the same config the TUI does (`KeybindingConfig::load`).
pub fn nav_keybindings() -> Vec<KeyBinding> {
    nav_keybindings_from(&KeybindingConfig::load())
}

/// Pure projection behind [`nav_keybindings`], split out so it can be tested
/// against a deterministic [`KeybindingConfig`] without depending on whatever
/// `keybindings.toml` happens to exist on the host. Bindings the GUI can't
/// express as a DOM key (an exotic remap) are skipped — the webview falls back
/// to its hardcoded arrow/Enter aliases for movement.
fn nav_keybindings_from(config: &KeybindingConfig) -> Vec<KeyBinding> {
    NAV_ACTIONS
        .iter()
        .filter_map(|&action| {
            let (code, mods) = config.binding(action).chord()?;
            let key = dom_key(code)?;
            Some(KeyBinding {
                action: action.config_name().to_string(),
                key,
                ctrl: mods.contains(KeyModifiers::CONTROL),
                shift: mods.contains(KeyModifiers::SHIFT),
                alt: mods.contains(KeyModifiers::ALT),
            })
        })
        .collect()
}

/// Map a crossterm `KeyCode` to the matching DOM `KeyboardEvent.key` string.
/// Covers the keys the navigation actions can resolve to; returns `None` for
/// anything not representable (the caller skips that binding).
fn dom_key(code: KeyCode) -> Option<String> {
    Some(match code {
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "Enter".to_string(),
        KeyCode::Esc => "Escape".to_string(),
        KeyCode::Tab => "Tab".to_string(),
        KeyCode::Left => "ArrowLeft".to_string(),
        KeyCode::Right => "ArrowRight".to_string(),
        KeyCode::Up => "ArrowUp".to_string(),
        KeyCode::Down => "ArrowDown".to_string(),
        KeyCode::Backspace => "Backspace".to_string(),
        KeyCode::Delete => "Delete".to_string(),
        KeyCode::Home => "Home".to_string(),
        KeyCode::End => "End".to_string(),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default bindings project to the chords the webview expects — the
    /// contract `main.js` switches on (Ctrl+d leader, h/l decks, j/k panes,
    /// digit→pane, Enter focus). Guards against silent drift in the action set
    /// or the crossterm→DOM mapping. Uses `KeybindingConfig::default()` so the
    /// assertions don't depend on any `keybindings.toml` on the host.
    #[test]
    fn default_nav_bindings_project_to_expected_dom_chords() {
        let table = nav_keybindings_from(&KeybindingConfig::default());
        let get = |name: &str| {
            table
                .iter()
                .find(|b| b.action == name)
                .cloned()
                .unwrap_or_else(|| panic!("{name} should be bound"))
        };

        let dash = get("dashboard");
        assert_eq!(
            (dash.key.as_str(), dash.ctrl, dash.shift, dash.alt),
            ("d", true, false, false)
        );
        assert_eq!(get("move_left").key, "h");
        assert_eq!(get("move_right").key, "l");
        assert_eq!(get("move_up").key, "k");
        assert_eq!(get("move_down").key, "j");
        assert_eq!(get("focus_pane").key, "Enter");
        assert_eq!(get("jump_1").key, "1");
        assert_eq!(get("jump_9").key, "9");
        // Movement keys carry no modifiers by default.
        assert!(!get("move_left").ctrl && !get("move_left").shift && !get("move_left").alt);
    }
}
