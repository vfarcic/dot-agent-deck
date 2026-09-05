---
sidebar_position: 6
title: Keyboard Shortcuts
---

# Keyboard Shortcuts

## Mouse

Every keyboard action below is also reachable with the mouse. Every clickable control shows its keyboard shortcut inline, so the on-screen controls double as a legend. On a dashboard card, a single click selects it and a double click focuses its pane.

The wheel scrolls the focused pane — or a mode tab's side pane when the pointer is over one. In command mode the wheel always drives Agent Deck's own scrollback and is never forwarded to the agent, so a full-screen TUI cannot move under you while you read. While you are typing in a pane, the wheel goes to the agent if the agent has mouse reporting enabled.

**Whether anything actually moves depends on the agent** — see [Scrolling back through a pane](#scrolling-back-through-a-pane).

## Global Shortcuts

| Key | Action | Works from |
|---|---|---|
| `Ctrl+D` | Toggle between command mode and the pane — press it in a pane to reach the dashboard, press it again to go back to the pane you came from | Any mode |
| `Ctrl+N` | New pane (directory picker, then name + command form) | Any mode |
| `Ctrl+T` | Toggle stacked / tiled layout — stacked shows only the focused pane at full height, tiled shows every pane at once | Any mode |
| `Ctrl+L` | Toggle the orchestration sidebar/pane-column split ratio between 34/66 and 25/75 (applies to every orchestration tab) | **Orchestration tabs, command mode only** |
| `Ctrl+Z` | Zoom the focused agent pane — it takes the whole frame. Press again to restore. See [`Ctrl+Z` zooms the focused agent pane](#ctrlz-zooms-the-focused-agent-pane). | **Dashboard and orchestration tabs, command mode only** |
| `Ctrl+W` | Close the selected pane on the dashboard, or tear down an entire mode tab — after a confirmation. The dashboard tab itself cannot be closed. | **Command mode only** |
| `Ctrl+E` | **Experimental — off by default.** Toggle the command-entry lock on an orchestration tab. See [`Ctrl+E` locks command entry to the orchestrator pane](#ctrle-locks-command-entry-to-the-orchestrator-pane). | **Command mode only, on an orchestration tab**, with the `experimental` flag on |
| `Ctrl+C` | In a pane, sent to the agent as SIGINT. In command mode, opens the quit dialog — see [Dialogs](#dialogs). | Any mode |

### Which mode you're in

A chip at the far left of the bottom bar reads ` COMMAND ` when your keystrokes drive the deck and ` TYPING ` when they go into the focused pane. The first button in the bar — `[Back to Pane Ctrl+D]` or `[Command Mode Ctrl+D]` — says where `Ctrl+D` would take you. (While a **Filter** or **Rename** field is open, that row *is* the input field and its own prompt tells you where your keystrokes go.)

Three other cues follow the mode:

- **The cursor.** The focused pane shows a cursor only while you are typing into it. A cursor means what you type lands in that pane.
- **Dimming and a banner.** Command mode dims the focused pane and overlays `COMMAND MODE — Ctrl+D to type`. The banner clears after a moment, or when you press a command-mode key. A key that isn't bound to anything keeps it up — that is the moment you most likely thought you were talking to the agent.
- **The selected card.** It keeps its `▸ ` marker in both modes, but its highlight is de-emphasised while you are typing in a pane.

### `Ctrl+W` closes only from command mode

`Ctrl+W` is delete-previous-word in shells, readline, and vim. So while you are typing in a pane it is sent straight through and deletes a word — it does not close anything. Press `Ctrl+D` first, and `Ctrl+W` there asks you to confirm before closing.

The confirmation defaults to **Cancel**, so an accidental `Ctrl+W` followed by a reflexive `Enter` changes nothing. Choosing **Close** stops the agent and removes the card.

### `Ctrl+E` locks command entry to the orchestrator pane

> **Experimental — off unless you turn it on.** Set `experimental = true` under `[features]` in your `.dot-agent-deck.toml`, or launch with `DOT_AGENT_DECK_EXPERIMENTAL=1` (the environment variable wins). With the flag off, `Ctrl+E` is not claimed anywhere and keystrokes reach a focused worker pane as usual.

With the flag on, typing into a **worker** pane on an orchestration tab is locked by default. Keystrokes still reach the orchestrator's pane; aimed at a worker they are dropped, and the bottom bar says `Pane locked — Ctrl+d then Ctrl+e to unlock`. Press `Ctrl+D`, then `Ctrl+E`, and the deck reports `Pane entry: unlocked`. `Ctrl+E` leaves you in command mode, so press `Ctrl+D` again to type.

This is not a read-only mode. Dashboard and mode tabs are untouched, and every pane still shows live output and scrolls normally. Why the pause is worth it is covered in [Typing into a worker is locked by default](orchestration.md#typing-into-a-worker-is-locked-by-default-experimental).

- **`Ctrl+E` is command-mode only**, because it is readline's `end-of-line` inside a pane.
- **The lock is one setting for the whole deck**, adopted by newly opened orchestration tabs, and not saved across restarts — every deck starts locked.
- **A worker waiting on you is not locked.** While a role pane reports `WaitingForInput` every key reaches it, and the lock returns when that status clears. An agent that never reports `WaitingForInput` gets no exemption, and a temporarily typeable pane looks no different from a locked one.

While locked, focus also follows a worker that starts waiting on you and returns to the orchestrator afterwards — see [Focus follows the lock](orchestration.md#focus-follows-the-lock).

### `Ctrl+Z` zooms the focused agent pane

On the Dashboard or an orchestration tab, `Ctrl+Z` in command mode gives the focused pane the whole frame, hiding the card sidebar and the other panes. The border title gains a `[Z]` so you can tell. Press `Ctrl+Z` again to restore the previous view exactly, including a `Ctrl+L` split. The full gesture is `Ctrl+D` then `Ctrl+Z`.

Nothing is stopped while zoomed, only hidden. What that costs on an orchestration tab is covered in [Zooming the focused pane](orchestration.md#zooming-the-focused-pane).

- **Command mode only**, so `Ctrl+Z` inside a pane still suspends whatever is running there.
- **Zoom follows focus.** Jump to another role with `1`–`9` while zoomed and you stay zoomed on that agent.
- **Per-tab, and never saved.** Each tab remembers its own zoom, a tab you open later starts unzoomed, and reattaching returns the full view.
- **A Mode tab has no sidebar to reclaim**, so `Ctrl+Z` reaches the pane there as ordinary input.

The agent reflows to the new width both ways, so nothing is lost or garbled.

## Tab Navigation

The tab bar appears when more than one tab is open.

| Key | Action |
|---|---|
| `Ctrl+PageDown` | Next tab (works from any mode, including in a focused pane) |
| `Ctrl+PageUp` | Previous tab (works from any mode, including in a focused pane) |
| `Tab` / `Right` / `l` | Next tab — **only in command mode** |
| `Shift+Tab` / `Left` / `h` | Previous tab — **only in command mode** |

The command-mode-only keys reach the agent instead while you are typing in a pane, so press `Ctrl+D` first.

## Mode Tab

Command mode, when a mode tab is active.

| Key | Action |
|---|---|
| `j` / `Down` | Focus next pane (cycles: agent → side panes → agent) |
| `k` / `Up` | Focus previous pane (cycles: agent → last side pane → … → agent) |
| `Enter` | Start typing into the selected pane (agent pane if none selected) |
| `Esc` | Deselect side pane (return focus indicator to agent) |
| Mouse click | Click a side pane to select it; click the agent pane to deselect |

## Dashboard

Command mode. If you're typing in a pane, press `Ctrl+D` first — otherwise the keystroke goes to the agent.

| Key | Action |
|---|---|
| `j` / `Down` | Select next card (wraps at end) |
| `k` / `Up` | Select previous card (wraps at start) |
| `1`–`9` | Jump to card N and focus its pane |
| `Enter` | Focus the selected card's pane |
| `PageUp` | Scroll the focused pane back (see [Scrolling back through a pane](#scrolling-back-through-a-pane)) |
| `PageDown` | Scroll the focused pane forward |
| `/` | Filter sessions (see [Dialogs](#dialogs)) |
| `r` | Rename selected session (see [Dialogs](#dialogs)) |
| `g` | Generate `.dot-agent-deck.toml` (see [Dialogs](#dialogs)) |
| `s` | Open the **Scheduled Tasks** manager (`S` also works) (see [Scheduled Tasks](./scheduled-tasks.md)) |
| `?` | Toggle help overlay |
| `y` / `n` | Approve / deny a pending permission request (only when an agent is waiting) |
| `Esc` | Clear active filter |

### Scrolling back through a pane

`PageUp` / `PageDown` scroll the **focused** pane back and forward — the keyboard equivalent of the wheel. They work in **command mode only**; while you are typing in a pane they go to the agent as `ESC[5~` / `ESC[6~`, so a pager, an editor, or the agent's own scrollback keeps them. `Ctrl+PageUp` / `Ctrl+PageDown` are separate chords and stay on tab navigation.

#### How far back you can scroll depends on the agent

Agent Deck routes the wheel and the scroll keys the same way for every pane, but **what there is to scroll is decided by the agent**.

- **Agents that keep their own transcript** — `claude`, for example — request mouse tracking and redraw their conversation as you scroll. While you are typing in the pane, the wheel and scroll keys go to the agent and the agent scrolls. The history you reach is its own.
- **Agents that expect the terminal to hold the history, while contributing none of it** — `codex` is the current example. It repaints its whole transcript in place instead of emitting new lines, so nothing ever scrolls off the top and the terminal is handed nothing to keep.

For an agent in the second group, Agent Deck has nothing of its own to scroll in command mode, by wheel or by key. That is not a setting you can change. Rather than doing nothing silently, a scroll that cannot land briefly overlays the pane with `Nothing to scroll — this pane has no scrollback to move through` (or `Nothing to scroll — no scrollback` in a narrow pane). It clears after a moment or on your next keystroke, and that keystroke is not swallowed — it reaches the agent or runs its shortcut as usual. You only ever see it on a pane you actually tried to scroll.

**A pane showing a full-screen interface is a third case.** A picker, a permission dialog or an editor switches to a second screen that keeps no scrollback of its own, so the deck cannot reach that pane's history while it is there. Nothing is lost — leaving that screen brings every line back — so a scroll there simply does nothing, with no notice.

It can look like "scrolling works fine outside Agent Deck": scrolling up during a `codex` session in an ordinary terminal reaches what was on screen *before* codex started, never an earlier part of the conversation. A pane Agent Deck spawns starts empty, so there is nothing above to reach.

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

Directory lists loop end-to-end, and the `..` parent entry stays visible even when a filter is active.

## New Pane / Mode Form

| Key | Action |
|---|---|
| `Tab` / `Shift+Tab` | Switch between fields |
| `Left` / `Right` / `h` / `l` | Cycle mode selector (when modes available) |
| `Enter` | Confirm field / submit form |
| `Esc` | Cancel |

## Dialogs

| Dialog | Trigger | Keys |
|---|---|---|
| **Filter** | `/` | Type to narrow visible cards · `Backspace` to delete · `Enter` to accept and stay filtered · `Esc` to clear and close |
| **Rename** | `r` | Type the new name · `Enter` to confirm · `Esc` to cancel |
| **Generate config** | `g` | `Up`/`Down` (or `k`/`j`) to choose **Yes** / **No** / **Never** · `Enter` to confirm · `Esc` to cancel. **Yes** asks the agent to write `.dot-agent-deck.toml`; **Never** suppresses the hint permanently for that directory. |
| **Quit** | `Ctrl+C` from command mode | `Up`/`Down` (or `k`/`j`) to choose **Detach** (default) / **Stop** / **Cancel** · `Enter` to confirm · `Esc` to dismiss · `Ctrl+C` again to leave immediately. Detach keeps your agents running in the daemon; Stop terminates them and asks once more first. |
| **Close confirmation** | `Ctrl+W`, the `[Close]` button, or a tab's `[×]` | `Up`/`Down` (or `k`/`j`) to choose **Cancel** (default) / **Close** · `Enter` to confirm · `Esc` to dismiss. The dialog names its target and closes exactly what was selected when it opened; a keystroke typed before it appeared is discarded rather than answering it. If a pane refuses to stop, the tab is kept so you can retry. |
| **Help overlay** | `?` | `?`, `Esc`, or `q` to dismiss |

## Customizing Keybindings

Every shortcut above can be remapped. dot-agent-deck reads an optional config file at:

```
~/.config/dot-agent-deck/keybindings.toml
```

Override the path with `DOT_AGENT_DECK_KEYBINDINGS`. Keybindings are resolved **client-side**, so when two clients attach to one remote daemon, each can have its own.

The file has two sections, `[global]` and `[dashboard]`. List only what you want to change. The help overlay (`?`) and the button bar are generated from the active config, so they always show your real keys.

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
toggle_orchestration_split = "Alt+Shift+s"   # move it off Ctrl+l
toggle_zoom = "Ctrl+Alt+z"       # move zoom off Ctrl+Z
new_pane = ""                    # disable the new-pane shortcut

[dashboard]
help = "F1"                      # open help with F1 instead of ?
```

### Actions and defaults

`[global]`:

| Action | Default | Description |
|---|---|---|
| `dashboard` | `Ctrl+d` | Toggle between command mode and the pane — any mode |
| `new_pane` | `Ctrl+n` | New pane (directory picker → name + command) — any mode |
| `close_pane` | `Ctrl+w` | Close selected pane / tear down mode tab, with confirmation — **command mode only** |
| `toggle_layout` | `Ctrl+t` | Toggle stacked / tiled layout — any mode |
| `toggle_orchestration_lock` | `Ctrl+e` | **Experimental — requires the `experimental` flag.** Toggle the orchestration command-entry lock — **command mode only, on an orchestration tab** |
| `toggle_orchestration_split` | `Ctrl+l` | Toggle the orchestration split between 34/66 and 25/75, for every orchestration tab — **orchestration tabs, command mode only** |
| `toggle_zoom` | `Ctrl+Z` | Zoom the focused pane to the whole frame; press again to restore. Per-tab, never saved — **Dashboard and orchestration tabs, command mode only** |
| `jump_1` … `jump_9` | `1` … `9` | Jump to card N and focus its pane |

The section name is the TOML table a binding is read from, not the modes it applies in — which is why the command-mode-only actions live in `[global]`. Anywhere a chord is not claimed, it reaches the pane as ordinary input for whatever is running there.

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
| `generate_config` | `g` | Generate `.dot-agent-deck.toml` |
| `scroll_pane_up` | `PageUp` | Scroll the focused pane back — **command mode only** |
| `scroll_pane_down` | `PageDown` | Scroll the focused pane forward — **command mode only** |

The `Down`/`Up`/`Tab`/`Shift+Tab`/`Left`/`Right` aliases and `Ctrl+PageUp` / `Ctrl+PageDown` tab navigation are not remappable and always work alongside your bindings. Remapping the scroll actions does not affect tab navigation, because those are separate chords.

Rebinding an action both enables the new chord and retires the default, so `scroll_pane_up = "Ctrl+u"` leaves plain `PageUp` doing nothing in command mode. Setting either scroll action to `""` leaves the wheel as the only way to scroll that pane.

**Quit is not a remappable action.** `Ctrl+C` is hardcoded: from command mode it always opens the quit dialog. A `quit = "…"` line is ignored with a warning.

### Edge cases

- **No config file** → all defaults.
- **Malformed file** → a warning on stderr and a fallback to all defaults; it never crashes.
- **Conflicting bindings** (two actions on one key) → a warning, and the first-defined action wins; the later one is left unbound.
- **Unknown action name** → ignored with a warning.
- **Empty binding** (`action = ""`) → that action is unbound and its default key does nothing.
- **`Ctrl+c` is never routed through your config.** Even if you bind another action to it, `Ctrl+c` from command mode always opens the quit dialog.
