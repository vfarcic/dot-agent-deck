# PRD #40: Customizable Keybindings

**Status**: Implemented — PR pending
**Priority**: Medium
**Created**: 2026-04-04
**Implemented**: 2026-06-07

### Implementation notes / divergences from this draft

- **Keybindings resolve client-side.** Bindings are interpreted in the TUI event loop (`run_tui` / `src/ui.rs`), never in the daemon. The config file lives on the machine running the TUI client; the daemon stays binding-agnostic, so multiple clients attached to one remote daemon can each have their own bindings.
- **Authoritative defaults are Ctrl-based, not Alt-based.** The `Alt+…` notation in the draft example below predates inspection of the real code. Actual defaults: `dashboard = Ctrl+d`, `new_pane = Ctrl+n`, `close_pane = Ctrl+w`, `toggle_layout = Ctrl+t`; jump keys are **bare** `1`–`9`; dashboard keys `j/k/h/l`, `/`, `r`, `?`, `Enter`, `Esc`, `y`, `n`. See `docs/keyboard-shortcuts.md` for the full defaults table.
- **Quit is NOT a remappable action.** No key directly quits: `Ctrl+C` (hardcoded, non-overridable) opens the quit/detach modal (Detach / Stop / Cancel) and you quit by selecting in it. Because no rebindable key triggers quit, `quit` was dropped from the action set entirely — there is no `quit` config key (a `quit = "…"` line is treated as an unknown action and ignored with a warning), and `Ctrl+C` can never be remapped, unbound, or hijacked by binding another action to it.

## Problem

dot-agent-deck uses hardcoded keyboard shortcuts (Alt+n, Alt+w, Alt+t, Alt+d, Alt+q, Alt+1-9). Users cannot change these, which creates friction when:

1. **Terminal emulator conflicts** — Ghostty, iTerm2, WezTerm, and other terminals reserve certain Alt+key combinations for their own features (e.g., Alt+n for new window, Alt+w for close tab). Users have no way to resolve these conflicts without changing their terminal emulator config.
2. **Personal preferences** — Users accustomed to different keybinding schemes (vim-style, emacs-style, or custom) cannot adapt the tool to their muscle memory.
3. **Accessibility** — Some key combinations are difficult for users with certain physical constraints. Remapping to more comfortable combinations is not possible.
4. **International keyboards** — Alt+key combinations behave differently across keyboard layouts. Some combinations produce special characters instead of triggering shortcuts.

## Solution

Add a keybinding configuration system that allows users to remap all keyboard shortcuts via a configuration file. The system should:

1. **Config file** — Read keybindings from a TOML/JSON config file (e.g., `~/.config/dot-agent-deck/keybindings.toml` or within the existing `config.toml`).
2. **Sensible defaults** — Ship with the current keybindings as defaults. Users only need to override what they want to change.
3. **Validation** — Detect and warn about conflicting mappings (two actions bound to the same key).
4. **Documentation** — List all available actions and their default bindings in help overlay and docs.

### Benefits

- **No more terminal conflicts** — Users remap to keys their terminal doesn't intercept
- **Personalized workflow** — Adapt to individual preferences
- **Accessibility** — Choose physically comfortable key combinations
- **International support** — Work around keyboard layout issues

## Technical Design

### Configuration Format

```toml
# ~/.config/dot-agent-deck/keybindings.toml
# Only override what you need — defaults apply for everything else.

[global]
dashboard = "Alt+d"
new_pane = "Alt+n"
close_pane = "Alt+w"
toggle_layout = "Alt+t"
jump_1 = "Alt+1"
jump_2 = "Alt+2"
# ... jump_3 through jump_9

[dashboard]
move_down = "j"
move_up = "k"
move_left = "h"
move_right = "l"
filter = "/"
rename = "r"
help = "?"
focus_pane = "Enter"
clear_filter = "Esc"
approve_permission = "y"
deny_permission = "n"
```

### Key Notation

Support a simple key notation:
- Modifiers: `Alt+`, `Ctrl+`, `Shift+`
- Special keys: `Enter`, `Esc`, `Tab`, `Space`, `Up`, `Down`, `Left`, `Right`
- Printable characters: `a`-`z`, `0`-`9`, `/`, `?`, etc.
- Combinations: `Alt+Shift+t`, `Ctrl+n`

### Architecture

**`KeybindingConfig`** — new struct in config system:
- Parsed from config file at startup
- Merged with defaults (user overrides take precedence)
- Validated for conflicts (warn on stderr, don't crash)
- Passed to `run_tui()` alongside `DashboardConfig`

**Key matching in event loop** — replace hardcoded key checks with config lookups:
- Current: `KeyCode::Char('t') && modifiers == ALT => toggle layout`
- New: `matches_binding(key, &config.global.toggle_layout) => toggle layout`

**Help overlay** — dynamically generated from active keybinding config, not hardcoded strings.

**Hints bar** — dynamically generated from active bindings.

### What stays the same

- All current default keybindings (users who don't configure anything see no change)
- The action set (what you can do doesn't change, only how you trigger it)
- Config file location follows existing `DashboardConfig` patterns

### What changes

- Key matching in `ui.rs` event loop uses config lookups instead of hardcoded checks
- Help overlay and hints bar are generated from the config
- `DashboardConfig` extended (or a new `KeybindingConfig` added alongside it)

## Edge Cases

- Config file doesn't exist → use all defaults (current behavior)
- Malformed config → warn on stderr, fall back to defaults for unparseable entries
- Conflicting bindings (two actions on same key) → warn, first-defined wins
- Unknown action names in config → warn and ignore
- Empty binding (e.g. `new_pane = ""`) → action is unbound (no key triggers it)
- Ctrl+C → always opens the quit/detach modal regardless of config (safety net, not overridable); quit is not a remappable action

## Milestones

- [x] Design and implement `KeybindingConfig` struct with defaults matching current shortcuts — `src/keybindings.rs` (`Action`/`Section` enums, `ACTIONS` table, `Binding`, `KeybindingConfig`).
- [x] Add config file parsing (extend existing config system or new file) — `KeybindingConfig::load()` reads `$DOT_AGENT_DECK_KEYBINDINGS` or `~/.config/dot-agent-deck/keybindings.toml`; `from_toml_str` for in-memory parsing.
- [x] Replace hardcoded key checks in `ui.rs` event loop with config-driven matching — dispatch via `cfg.matches(Action, &KeyEvent)` / `action_for`; single-match invariant enforced.
- [x] Generate help overlay dynamically from active keybinding config — `render_help_overlay_to_buffer`; live overlay refactored through the same config-driven path.
- [x] Generate hints bar dynamically from active bindings — `render_hints_bar_to_buffer`; live hints bar refactored through the same path.
- [x] Validation: detect and warn about conflicting bindings — `resolve_conflicts` (normalized-chord keyed, first-defined wins, later unbound + stderr warning).
- [x] Documentation: add keybinding customization section to README — added to `docs/keyboard-shortcuts.md` (README delegates all shortcut docs there).
- [x] Tests: unit tests for config parsing, key matching, conflict detection — ~26 pure-data unit tests in `src/keybindings.rs`, plus 2 L1 snapshot tests (`keybindings/help/001`, `keybindings/hints/001`) and 6 L2 synthetic tests (`keybindings/remap/00{1,2}`, `unbind/001`, `fallback/001`, `safety/00{1,2}`).

## Out of Scope (v1)

- Runtime keybinding changes (requires restart)
- GUI keybinding editor
- Keybinding profiles (e.g., "vim mode", "emacs mode")
- Mouse button remapping
- Macro recording / multi-key sequences
