---
sidebar_position: 6
title: Keyboard Shortcuts
---

# Keyboard Shortcuts

## Mouse

Every keyboard action below is also reachable with the mouse — the dashboard is fully clickable, not keyboard-only. Each clickable affordance carries its keyboard shortcut inline, so the on-screen controls double as a legend, and clicking one performs exactly the same action as its shortcut.

- **Persistent button bar.** The bottom row exposes the global commands — `[Back to Pane Ctrl+D]`, `[New Pane Ctrl+N]`, `[Close Ctrl+W]`, `[Toggle Layout Ctrl+T]`, `[Help ?]`, and `[Quit Ctrl+C]`. On terminals too narrow for the full labels the bar wraps to a second row rather than dropping any of them. This replaces the old status-bar legend. The bar follows the mode you are in: the first button reads `[Back to Pane Ctrl+D]` in command mode and `[Command Mode Ctrl+D]` while you are typing in a pane, and `[Close Ctrl+W]` is dimmed and inert outside command mode, matching the key. `[Close]` opens the same close confirmation the `Ctrl+W` key does.
- **Tab strip.** Click a tab header to switch to it; Mode and Orchestration tabs carry a clickable `[×]` close affordance (the Dashboard tab has none). The `[×]` opens the same close confirmation as `Ctrl+W` and the `[Close]` button — every route into a pane teardown asks first.
- **Dashboard cards.** Single-click a card to select it, double-click to focus its pane. The bar adds clickable `[Filter /]`, `[Rename r]`, and `[Generate g]` buttons.
- **Dialogs, picker, and forms.** Each carries explicit clickable buttons alongside its keyboard controls: quit/config-gen/star/help dialog buttons; the directory picker's clickable rows, `..` parent, and `[Confirm]`/`[Cancel]`/`[Filter]`; the inline filter/rename `[Apply]`/`[Save]`/`[Cancel]`; the `[Command Mode Ctrl+D]` affordance while in a pane; and the new-pane form's clickable mode chips with `[Submit]`/`[Cancel]`.

All the keyboard shortcuts below continue to work unchanged.

## Global Shortcuts

| Key | Action | Works from |
|---|---|---|
| `Ctrl+D` | Toggle between command mode and the pane — press it in a pane to reach the dashboard, press it again to go back to the pane you came from | Any mode |
| `Ctrl+N` | New pane (directory picker, then name + command form) | Any mode |
| `Ctrl+T` | Toggle stacked / tiled layout | Any mode |
| `Ctrl+W` | Close the selected pane on the dashboard, or tear down the entire mode tab (agent + side panes) when used on a mode tab — after a confirmation dialog. The dashboard tab itself cannot be closed. | **Command mode only** |

### `Ctrl+W` closes only from command mode

`Ctrl+W` is delete-previous-word in shells, readline, vim, and essentially every program you run inside a pane. So while you are typing in a pane, `Ctrl+W` is sent straight through to that program as `^W` (byte `0x17`) and deletes a word — it does not close anything. Press `Ctrl+D` first to reach command mode, and `Ctrl+W` there asks you to confirm before closing.

The confirmation defaults to **Cancel**, so an accidental `Ctrl+W` followed by a reflexive `Enter` leaves your pane exactly where it was. Choosing **Close** stops the agent and removes the card.

### `Ctrl+C`

In PaneInput mode, `Ctrl+C` is delivered to the terminal as SIGINT (0x03). From the dashboard (command mode), pressing `Ctrl+C` opens a quit confirmation dialog; press it again to quit immediately, or use the dialog keys (see [Dialogs](#dialogs)) to choose Yes / No.

## Tab Navigation

The tab bar appears when more than one tab is open.

| Key | Action |
|---|---|
| `Ctrl+PageDown` | Next tab (works from any mode, including in a focused pane) |
| `Ctrl+PageUp` | Previous tab (works from any mode, including in a focused pane) |
| `Tab` / `Right` / `l` | Next tab — **only in command mode** (press `Ctrl+D` first; otherwise the keystroke is sent to the agent pane) |
| `Shift+Tab` / `Left` / `h` | Previous tab — **only in command mode** (press `Ctrl+D` first; otherwise the keystroke is sent to the agent pane) |

## Mode Tab

These shortcuts work in Normal mode when a mode tab is active.

| Key | Action |
|---|---|
| `j` / `Down` | Focus next pane (cycles: agent → side panes → agent) |
| `k` / `Up` | Focus previous pane (cycles: agent → last side pane → … → agent) |
| `Enter` | Enter PaneInput mode on selected pane (agent pane if none selected) |
| `Esc` | Deselect side pane (return focus indicator to agent) |
| Mouse click | Click a side pane to select it; click agent pane to deselect |

In PaneInput mode, use `Ctrl+D` to return to Normal mode — and `Ctrl+D` again to go back into the pane.

## Dashboard

These shortcuts work in **command mode**. If you're typing in an agent pane, press `Ctrl+D` first to leave the pane — otherwise the keystroke is sent to the agent.

| Key | Action |
|---|---|
| `j` / `Down` | Select next card (wraps at end) |
| `k` / `Up` | Select previous card (wraps at start) |
| `1`–`9` | Jump to card N and focus its pane |
| `/` | Filter sessions (opens filter input — see [Dialogs](#dialogs)) |
| `r` | Rename selected session (opens rename input — see [Dialogs](#dialogs)) |
| `g` | Generate `.dot-agent-deck.toml` (opens config-generation prompt — see [Dialogs](#dialogs)) |
| `s` | Open the **Scheduled Tasks** manager (`S` also works) (see [Scheduled Tasks](./scheduled-tasks.md)) |
| `?` | Toggle help overlay |
| `y` / `n` | Approve / deny a pending permission request (only when an agent is waiting) |
| `Esc` | Clear active filter |

## Directory Picker

| Key | Action |
|---|---|
| `j` / `Down` | Select next directory |
| `k` / `Up` | Select previous directory |
| `l` / `Right` / `Enter` | Enter directory (or confirm if no subdirs) |
| `h` / `Left` / `Backspace` | Go up one level |
| `Space` | Confirm current directory |
| `/` | Enter filter mode; type to narrow directories (case-insensitive) |
| `Esc` | Clear filter (press twice to close) |
| `q` | Cancel |

Directory lists loop end-to-end, so pressing `Up` on the first entry jumps to the last (and vice versa). The `..` parent entry always remains visible even when a filter is active.

## New Pane / Mode Form

| Key | Action |
|---|---|
| `Tab` / `Shift+Tab` | Switch between fields |
| `Left` / `Right` / `h` / `l` | Cycle mode selector (when modes available) |
| `Enter` | Confirm field / submit form |
| `Esc` | Cancel |

## Dialogs

Several dashboard shortcuts open transient input fields or selection dialogs. The keys for each:

| Dialog | Trigger | Keys |
|---|---|---|
| **Filter** | `/` | Type to narrow visible cards · `Backspace` to delete · `Enter` to accept and stay filtered · `Esc` to clear and close |
| **Rename** | `r` | Type the new name · `Enter` to confirm · `Esc` to cancel |
| **Generate config** | `g` | `Up`/`Down` (or `k`/`j`) to choose **Yes** / **No** / **Never** · `Enter` to confirm · `Esc` to cancel. **Yes** sends a prompt to the agent to write `.dot-agent-deck.toml`; **Never** suppresses the hint permanently for that directory. |
| **Quit confirmation** | `Ctrl+C` from command mode | `Up`/`Down` (or `k`/`j`) to choose **Yes** / **No** · `Enter` to confirm · `Esc` to dismiss · `Ctrl+C` again to quit immediately |
| **Close confirmation** | `Ctrl+W` from command mode, the `[Close]` button, or a tab's `[×]` | `Up`/`Down` (or `k`/`j`) to choose **Cancel** (default) / **Close** · `Enter` to confirm · `Esc` to dismiss. Only **Close** tears the pane (or tab) down, and it closes exactly what was selected when the dialog opened — the dialog holds the keyboard while it is up, and any keystroke you typed before it appeared is discarded rather than answering it. |
| **Help overlay** | `?` | `?`, `Esc`, or `q` to dismiss |

## Customizing Keybindings

Every shortcut above can be remapped. dot-agent-deck reads an optional config file at:

```
~/.config/dot-agent-deck/keybindings.toml
```

(Override the path with the `DOT_AGENT_DECK_KEYBINDINGS` environment variable.) Keybindings are resolved **client-side**, on the machine running the TUI — so when two clients attach to one remote daemon, each can have its own bindings.

The file has two sections, `[global]` and `[dashboard]`. You only need to list the actions you want to change; everything else keeps its default. The help overlay (`?`) and the hints bar are generated from the active config, so they always show your real keys.

### Key notation

- **Modifiers:** `Ctrl+`, `Alt+`, `Shift+` — combine in any order, e.g. `Alt+Shift+t`.
- **Named keys:** `Enter`, `Esc`, `Tab`, `Space`, `Up`, `Down`, `Left`, `Right`, `Backspace`, `Delete`, `Home`, `End`, `PageUp`, `PageDown`, `Insert`, and `F1`–`F12`.
- **Printable characters:** `a`–`z`, `0`–`9`, `/`, `?`, etc.
- **Unbound:** an empty string (`new_pane = ""`) disables the action entirely.

Notation is case-insensitive for modifier and named keys (`ctrl+enter` == `Ctrl+Enter`).

### Example

```toml
# ~/.config/dot-agent-deck/keybindings.toml
# Only override what you need — defaults apply for everything else.

[global]
toggle_layout = "Alt+Shift+l"   # move it off Ctrl+t
new_pane = ""                    # disable the new-pane shortcut

[dashboard]
help = "F1"                      # open help with F1 instead of ?
```

### Actions and defaults

`[global]`:

| Action | Default | Description |
|---|---|---|
| `dashboard` | `Ctrl+d` | Toggle between command mode and the pane — works from any mode |
| `new_pane` | `Ctrl+n` | New pane (directory picker → name + command) — works from any mode |
| `close_pane` | `Ctrl+w` | Close selected pane / tear down mode tab, with confirmation — **command mode only**; in a pane the chord is ordinary input for whatever is running there |
| `toggle_layout` | `Ctrl+t` | Toggle stacked / tiled layout — works from any mode |
| `jump_1` … `jump_9` | `1` … `9` | Jump to card N and focus its pane |

`close_pane` stays in `[global]` — the section names the TOML table your binding is read from, not the modes it applies in, so an existing `[global] close_pane = "…"` line keeps working. Whatever chord you bind it to is command-mode only and reaches the pane as ordinary input everywhere else.

`[dashboard]` (command mode):

| Action | Default | Description |
|---|---|---|
| `move_down` | `j` | Select next card |
| `move_up` | `k` | Select previous card |
| `move_left` | `h` | Previous tab |
| `move_right` | `l` | Next tab |
| `filter` | `/` | Filter sessions |
| `rename` | `r` | Rename selected session |
| `help` | `?` | Toggle help overlay |
| `focus_pane` | `Enter` | Focus selected pane |
| `clear_filter` | `Esc` | Clear active filter |
| `approve_permission` | `y` | Approve a pending permission request |
| `deny_permission` | `n` | Deny a pending permission request |
| `generate_config` | `g` | Generate `.dot-agent-deck.toml` (config-generation prompt) |

The `Down`/`Up`/`Tab`/`Shift+Tab`/`Left`/`Right` aliases and `Ctrl+PageUp` / `Ctrl+PageDown` tab navigation are not remappable and always work alongside your bindings.

**Quit is not a remappable action.** No key directly quits — `Ctrl+C` (hardcoded, non-overridable) opens the quit/detach modal (Detach / Stop / Cancel). There is no `quit` config key; a `quit = "…"` line is treated as an unknown action and ignored with a warning.

### Edge cases

- **No config file** → all defaults (current behavior, nothing changes).
- **Malformed file** → dot-agent-deck warns on stderr and falls back to all defaults; it never crashes.
- **Conflicting bindings** (two actions on the same key) → a warning is printed and the first-defined action wins; the later one is left unbound.
- **Unknown action name** → ignored with a warning.
- **Empty binding** (`action = ""`) → that action is unbound and its default key does nothing.
- **`Ctrl+c` always quits.** It is a non-overridable safety net: quit is not a configurable action, and even if you bind another action to `Ctrl+c`, pressing `Ctrl+c` from command mode always opens the quit/detach modal — it is never routed through your config (so it can't be turned into "new pane", "switch tab", etc.).
