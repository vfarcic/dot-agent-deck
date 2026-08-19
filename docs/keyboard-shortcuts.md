---
sidebar_position: 6
title: Keyboard Shortcuts
---

# Keyboard Shortcuts

## Mouse

Every keyboard action below is also reachable with the mouse — the dashboard is fully clickable, and every clickable control carries its keyboard shortcut inline, so the on-screen controls double as a legend and clicking one does exactly what its shortcut does. Two things the labels cannot tell you: a single click on a dashboard card selects it while a double click focuses its pane, and the button bar along the bottom wraps onto more rows on a narrow terminal rather than dropping any of its commands.

A mode tab's side panes scroll when the pointer is over them; anywhere else the wheel scrolls the focused pane. In command mode it is always routed to Agent Deck's own scrollback and is never forwarded to the agent, so a full-screen TUI running in a pane cannot move under you while you read. While you are typing in a pane, the wheel goes to the agent whenever the agent has mouse reporting enabled.

**Whether anything actually moves depends on the agent**, and for some agents the answer is nothing — see [Scrolling back through a pane](#scrolling-back-through-a-pane).

## Global Shortcuts

| Key | Action | Works from |
|---|---|---|
| `Ctrl+D` | Toggle between command mode and the pane — press it in a pane to reach the dashboard, press it again to go back to the pane you came from | Any mode |
| `Ctrl+N` | New pane (directory picker, then name + command form) | Any mode |
| `Ctrl+T` | Toggle stacked / tiled layout — stacked shows only the focused pane at full height, tiled shows every pane at once | Any mode |
| `Ctrl+L` | Toggle the orchestration sidebar/pane-column split ratio between 34/66 and 25/75 (applies to every orchestration tab) | **Orchestration tabs, command mode only** |
| `Ctrl+W` | Close the selected pane on the dashboard, or tear down the entire mode tab (agent + side panes) when used on a mode tab — after a confirmation dialog. The dashboard tab itself cannot be closed. | **Command mode only** |
| `Ctrl+E` | **Experimental — off by default.** Toggle the command-entry lock — whether you can type directly into a worker pane on an orchestration tab. See [`Ctrl+E` locks command entry to the orchestrator pane](#ctrle-locks-command-entry-to-the-orchestrator-pane). | **Command mode only, on an orchestration tab**, and only while the `experimental` flag is on |

### Which mode you're in

`Ctrl+D` toggles between two modes, and the deck names the one you are in. A chip at the far left of the bottom bar reads ` COMMAND ` when your keystrokes drive the deck and ` TYPING ` when they go into the focused pane — in the same place on every tab, except while an inline **Filter** or **Rename** field is open, where that row *is* the input field and its own prompt tells you where your keystrokes are going. Those two words are the vocabulary the rest of this page uses: "command mode" is ` COMMAND `, and "typing in a pane" (`PaneInput` internally) is ` TYPING `. The chip says where you are; the first button in the bar — `[Back to Pane Ctrl+D]` or `[Command Mode Ctrl+D]` — says where `Ctrl+D` would take you.

Three other things follow the mode:

- **The cursor.** The focused pane shows a cursor only while you are typing into it. A cursor means, without exception, that what you type lands in that pane.
- **The focused pane dims, with a banner.** Entering command mode dims the focused pane's content — still perfectly readable, just visibly inert — and overlays a `COMMAND MODE — Ctrl+D to type` banner. The banner clears itself after a moment, or immediately when you press a command-mode key or click a bottom-bar button; a key that isn't bound to anything keeps it up, because that is the moment you most likely thought you were talking to the agent. The dimming stays for as long as you are in command mode.
- **The selected dashboard card.** It keeps its `▸ ` marker in both modes so you never lose track of the selection, but its highlight is de-emphasised while you are typing in a pane — the deck looks inert exactly when the pane looks live.

### `Ctrl+W` closes only from command mode

`Ctrl+W` is delete-previous-word in shells, readline, vim, and essentially every program you run inside a pane. So while you are typing in a pane, `Ctrl+W` is sent straight through to that program as `^W` (byte `0x17`) and deletes a word — it does not close anything. Press `Ctrl+D` first, and `Ctrl+W` there asks you to confirm before closing.

The confirmation defaults to **Cancel**, so an accidental `Ctrl+W` followed by a reflexive `Enter` leaves your pane exactly where it was. Choosing **Close** stops the agent and removes the card.

### `Ctrl+E` locks command entry to the orchestrator pane

> **Experimental — the whole of this section is off unless you turn it on.**
>
> The command-entry lock is gated behind the `experimental` feature flag while it is evaluated in real use. With the flag off — the default — `Ctrl+E` is not claimed anywhere, keystrokes reach a focused worker pane exactly as they always have, and the deck never moves focus on its own. To try it, set `experimental = true` under a `[features]` table in your `.dot-agent-deck.toml`, or launch with `DOT_AGENT_DECK_EXPERIMENTAL=1` (the environment variable wins over the file). Note that the focus steering described at the end of this section is part of the same gated surface — it only ever runs while the lock is engaged.

With the flag on, on an **orchestration tab**, typing into a worker pane is locked by default. Your keystrokes reach the orchestrator's pane exactly as before; aim them at a worker role and they are dropped rather than delivered, and the bottom bar says `Pane locked — Ctrl+d then Ctrl+e to unlock`. Press `Ctrl+D` to reach command mode, then `Ctrl+E`, and the deck reports `Pane entry: unlocked`; the same chord locks it again. `Ctrl+E` leaves you in command mode, so press `Ctrl+D` once more to return to the pane and type.

**This is not a read-only mode, and it does not apply anywhere else.** Dashboard and mode tabs are untouched, nothing is disabled, and every pane still shows live output and scrolls normally. On an orchestration tab the lock costs one deliberate `Ctrl+D`, `Ctrl+E` before you can type at a worker — and that pause is the point. An orchestration is one workflow with a single coordinator, and an open pane invites the reflex of answering a worker's question on the spot. Doing that puts a second, uncoordinated actor inside the workflow: you change state the orchestrator believes it owns, with no way for it to find out. Most often this is not even deliberate — you inspect a worker pane, get distracted, and type your next instruction into the wrong pane. The lock turns that reflex into a decision, and the default has to be locked for it to mean anything.

Three details worth knowing:

- **`Ctrl+E` is command-mode only**, for the same reason `Ctrl+W` is. `Ctrl+E` is readline's `end-of-line` (byte `0x05`) in shells, agents, and anything else running inside a pane. While you are typing in a pane the deck does not claim it, so the byte reaches the program and moves your cursor to the end of the line as usual.
- **The lock is one setting for the whole deck.** Unlocking on one orchestration tab unlocks all of them, and a newly opened orchestration tab adopts whatever the current setting is. It describes how you are working right now, not which tab you happened to open. It is not saved across restarts — every deck starts locked.
- **A worker that has stopped and asked you something is not locked.** While a role pane reports `WaitingForInput`, every key reaches it with no unlock at all, and the lock re-engages the moment that status clears. Answering a question the agent itself asked is a response to a request, not an interruption of one. The flip side: an agent that never reports `WaitingForInput` gets no such exemption, and reaching it still needs a deliberate `Ctrl+D`, `Ctrl+E`.

Focus follows the same setting. While locked, the deck steers focus for you: onto a worker the moment it starts waiting on you, then back to the orchestrator once nothing is waiting any more. While unlocked, the deck moves focus nowhere at all — it stays exactly where you put it until you lock again.

### `Ctrl+C`

While you are typing in a pane, `Ctrl+C` is delivered to the terminal as SIGINT (0x03). In command mode it opens the quit dialog — **Detach** (default) / **Stop** / **Cancel**, see [Dialogs](#dialogs) — and pressing `Ctrl+C` again from there leaves immediately.

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

These shortcuts work in command mode when a mode tab is active.

| Key | Action |
|---|---|
| `j` / `Down` | Focus next pane (cycles: agent → side panes → agent) |
| `k` / `Up` | Focus previous pane (cycles: agent → last side pane → … → agent) |
| `Enter` | Start typing into the selected pane (agent pane if none selected) |
| `Esc` | Deselect side pane (return focus indicator to agent) |
| Mouse click | Click a side pane to select it; click agent pane to deselect |

`Ctrl+D` leaves the pane, and `Ctrl+D` again goes back into it.

## Dashboard

These shortcuts work in **command mode**. If you're typing in an agent pane, press `Ctrl+D` first to leave the pane — otherwise the keystroke is sent to the agent.

| Key | Action |
|---|---|
| `j` / `Down` | Select next card (wraps at end) |
| `k` / `Up` | Select previous card (wraps at start) |
| `1`–`9` | Jump to card N and focus its pane |
| `PageUp` | Scroll the focused pane back (see [Scrolling back through a pane](#scrolling-back-through-a-pane)) |
| `PageDown` | Scroll the focused pane forward |
| `/` | Filter sessions (opens filter input — see [Dialogs](#dialogs)) |
| `r` | Rename selected session (opens rename input — see [Dialogs](#dialogs)) |
| `g` | Generate `.dot-agent-deck.toml` (opens config-generation prompt — see [Dialogs](#dialogs)) |
| `s` | Open the **Scheduled Tasks** manager (`S` also works) (see [Scheduled Tasks](./scheduled-tasks.md)) |
| `?` | Toggle help overlay |
| `y` / `n` | Approve / deny a pending permission request (only when an agent is waiting) |
| `Esc` | Clear active filter |

### Scrolling back through a pane

`PageUp` / `PageDown` scroll the focused pane's output back and forward — the keyboard equivalent of the scroll wheel. They are the `scroll_pane_up` and `scroll_pane_down` actions and are remappable like any other binding (see [Actions and defaults](#actions-and-defaults)).

They work in **command mode only**. While you are typing in a pane they are sent straight through to whatever is running there as `ESC[5~` / `ESC[6~`, so a pager, an editor, or the agent's own scrollback keeps them; press `Ctrl+D` first and the same keys scroll the deck's view of the pane instead. `Ctrl+PageUp` / `Ctrl+PageDown` are separate chords and stay on tab navigation.

#### How far back you can scroll depends on the agent

Agent Deck routes the wheel and the scroll keys the same way for every pane, but **what there is to scroll is decided by the agent running in it**, and terminal applications split into two camps here.

**App-managed agents keep their own transcript and scroll it themselves.** They ask the terminal for mouse tracking so they can receive your wheel events directly — `claude` requests all four mouse modes — and they redraw their conversation at whatever position you scroll to. While you are typing in the pane, Agent Deck forwards the wheel straight to the agent, and the agent scrolls. This works, and the history you reach is the agent's own, as long as it chooses to keep it.

**Terminal-managed agents expect the terminal to hold the history, while contributing none of it.** `codex` is the current example: it requests no mouse events at all (only focus reporting, which is not the same thing), it does not switch to the alternate screen, and it sizes its drawing region to the exact height of the pane and repaints its whole transcript in place. Because it repaints rather than emitting new lines, **no line ever scrolls off the top**, so nothing is ever handed to the terminal to keep. Measured against a real session at the height it rendered for, the retained buffer is **zero lines**.

**For an agent like that, Agent Deck has nothing to scroll — by wheel or by key, in any mode.** This is not a setting you can change and there is no alternative chord that reaches further back: `PageUp` does nothing for the same reason the wheel does. Rather than doing nothing silently, the deck says so: a scroll that cannot land briefly overlays the pane with `Nothing to scroll — this pane has no scrollback to move through`, in the same reversed single-line style as the command-mode banner. It clears itself after a moment or on your next keystroke, and that keystroke is not swallowed — it reaches the agent, or runs its shortcut, exactly as it would have. A pane too narrow for the full sentence gets the short form, `Nothing to scroll — no scrollback`. You will only ever see it on a pane you actually tried to scroll; it never appears on its own. The message reports what the deck observed about that pane and stops there — *why* a particular pane has no scrollback is the per-agent distinction this section explains, and the deck cannot establish it from the pane's output alone.

It is worth knowing why this can look like "scrolling works fine outside Agent Deck". Scrolling up during a long `codex` session in an ordinary terminal reaches whatever was on your screen *before* codex started — your shell prompt, the command you typed — and never an earlier part of the conversation. Codex does not use the alternate screen, so that earlier content simply stays in your terminal's own scrollback above it. A pane Agent Deck spawns for the agent starts empty, so there is no such content and nothing moves at all.

Making a terminal-managed agent's transcript scrollable is something only that agent can do, by using the alternate screen, enabling mouse tracking, or binding a scroll key of its own.

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

Several dashboard shortcuts open transient input fields or selection dialogs. The keys for each:

| Dialog | Trigger | Keys |
|---|---|---|
| **Filter** | `/` | Type to narrow visible cards · `Backspace` to delete · `Enter` to accept and stay filtered · `Esc` to clear and close |
| **Rename** | `r` | Type the new name · `Enter` to confirm · `Esc` to cancel |
| **Generate config** | `g` | `Up`/`Down` (or `k`/`j`) to choose **Yes** / **No** / **Never** · `Enter` to confirm · `Esc` to cancel. **Yes** sends a prompt to the agent to write `.dot-agent-deck.toml`; **Never** suppresses the hint permanently for that directory. |
| **Quit** | `Ctrl+C` from command mode | `Up`/`Down` (or `k`/`j`) to choose **Detach** (default) / **Stop** / **Cancel** · `Enter` to confirm · `Esc` to dismiss · `Ctrl+C` again to leave immediately. Detach keeps your agents running in the daemon; Stop terminates them and asks once more first. |
| **Close confirmation** | `Ctrl+W` from command mode, the `[Close]` button, or a tab's `[×]` | `Up`/`Down` (or `k`/`j`) to choose **Cancel** (default) / **Close** · `Enter` to confirm · `Esc` to dismiss. The dialog names its target — a single dashboard pane, or a Mode/Orchestration tab and all its panes. It closes exactly what was selected when it opened, and any keystroke you typed before it appeared is discarded rather than answering it. If a pane refuses to stop, the tab is kept holding whatever could not be closed, so you can press `Ctrl+W` again to retry. |
| **Help overlay** | `?` | `?`, `Esc`, or `q` to dismiss |

## Customizing Keybindings

Every shortcut above can be remapped. dot-agent-deck reads an optional config file at:

```
~/.config/dot-agent-deck/keybindings.toml
```

(Override the path with the `DOT_AGENT_DECK_KEYBINDINGS` environment variable.) Keybindings are resolved **client-side**, on the machine running the TUI — so when two clients attach to one remote daemon, each can have its own bindings.

The file has two sections, `[global]` and `[dashboard]`. You only need to list the actions you want to change; everything else keeps its default. The help overlay (`?`) and the button bar are generated from the active config, so they always show your real keys.

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
| `toggle_orchestration_lock` | `Ctrl+e` | **Experimental — requires the `experimental` flag; without it the chord is never claimed.** Toggle the orchestration command-entry lock — **command mode only, on an orchestration tab**; everywhere else the chord is ordinary input for whatever is running in the pane |
| `toggle_orchestration_split` | `Ctrl+l` | Toggle the orchestration sidebar/pane-column split between 34/66 and 25/75 — one press applies to every orchestration tab, including ones you open afterwards. **Orchestration tabs, command mode only**; in a pane, and on every other tab, the chord is ordinary input for whatever is running there |
| `jump_1` … `jump_9` | `1` … `9` | Jump to card N and focus its pane |

`close_pane`, `toggle_orchestration_lock`, and `toggle_orchestration_split` live in `[global]` because the section names the TOML table your binding is read from, not the modes it applies in. Whatever chord you bind any of them to is command-mode only and reaches the pane as ordinary input everywhere else.

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
| `scroll_pane_up` | `PageUp` | Scroll the focused pane back — **command mode only**; in a pane the key is passed to the agent |
| `scroll_pane_down` | `PageDown` | Scroll the focused pane forward — **command mode only**; in a pane the key is passed to the agent |

The `Down`/`Up`/`Tab`/`Shift+Tab`/`Left`/`Right` aliases and `Ctrl+PageUp` / `Ctrl+PageDown` tab navigation are not remappable and always work alongside your bindings. Because `Ctrl+PageUp` / `Ctrl+PageDown` are separate chords from the unmodified `PageUp` / `PageDown`, remapping the scroll actions does not affect tab navigation.

Rebinding an action both enables the new chord and retires the default, so `scroll_pane_up = "Ctrl+u"` leaves plain `PageUp` doing nothing in command mode. Setting either scroll action to `""` leaves the wheel as the only way to scroll that pane.

**Quit is not a remappable action.** No key directly quits — `Ctrl+C` (hardcoded, non-overridable) opens the quit dialog (Detach / Stop / Cancel). There is no `quit` config key; a `quit = "…"` line is treated as an unknown action and ignored with a warning.

### Edge cases

- **No config file** → all defaults.
- **Malformed file** → dot-agent-deck warns on stderr and falls back to all defaults; it never crashes.
- **Conflicting bindings** (two actions on the same key) → a warning is printed and the first-defined action wins; the later one is left unbound.
- **Unknown action name** → ignored with a warning.
- **Empty binding** (`action = ""`) → that action is unbound and its default key does nothing.
- **`Ctrl+c` is never routed through your config.** Even if you bind another action to it, `Ctrl+c` from command mode always opens the quit dialog — it cannot be turned into "new pane", "switch tab", or anything else.
