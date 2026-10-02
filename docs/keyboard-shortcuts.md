# Keyboard Shortcuts

Everything on this page is the TUI's, except [Editing an agent's prompt](#editing-an-agents-prompt), which works the same in both clients. The desktop app has its own shortcuts, listed on [Desktop app → Settings](desktop/settings.md#keyboard-shortcuts); `keybindings.toml` does not affect it.

Press `?` in command mode to see the shortcuts in the TUI. The overlay and the bottom button bar are generated from your active [keybindings](#customizing-keybindings), so they show your real keys.

![The TUI's help overlay, opened with ?, listing the shortcuts by section: Global, Tab Navigation, Dashboard (command mode), Mode Tab, New Agent Form, Directory Picker and Session](/img/help-tui.png)

## Modes

The TUI is in one of two modes, and most keys depend on which:

- **Command mode**: keys drive the deck. The chip at the far left of the bottom bar reads ` COMMAND `, the focused pane is dimmed, and a `COMMAND MODE — Ctrl+D to type` banner appears over it. The banner clears after a moment or when you press a command-mode key, and stays up if you press a key that is not bound to anything.
- **Typing in a pane**: keys go to the program in the focused pane. The chip reads ` TYPING ` and the pane shows a cursor.

`Ctrl+D` switches between them. The first button in the bar, `[Back to Pane Ctrl+D]` or `[Command Mode Ctrl+D]`, says where it takes you. The selected card keeps its `▸ ` marker in both modes.

## Mouse

The main commands have clickable buttons along the bottom, each labelled with its keyboard shortcut. On a dashboard card, a click selects it and a double click focuses its pane. Cards, tab headers, dialogs, the directory picker and forms respond to clicks.

The wheel scrolls the focused pane while the pointer is inside it. Anywhere else (the card list, the bars, a pane that is not focused, a pane's border) the wheel does nothing. The Schedules manager scrolls its own list; other dialogs ignore the wheel.

- In command mode the wheel scrolls the deck's own copy of the pane's history and is not sent to the agent.
- While typing in a pane, the wheel goes to the agent if the agent has turned on mouse reporting.

Whether there is anything to scroll depends on the agent; see [Scrolling back through a pane](#scrolling-back-through-a-pane).

## Global Shortcuts

| Key | Action | Works in |
|---|---|---|
| `Ctrl+D` | Switch between command mode and the pane you came from | Both modes |
| `Ctrl+N` | New agent: directory picker, then the New Agent form | Both modes |
| `Ctrl+T` | Toggle stacked layout (only the focused pane, full height; the default) and tiled layout (every pane) | Both modes |
| `Ctrl+PageDown` / `Ctrl+PageUp` | Next / previous tab | Both modes |
| `Ctrl+W` | Close the selected pane on the dashboard, or a whole orchestration tab, after a confirmation. The dashboard tab cannot be closed. | Command mode |
| `1`–`9` | Jump to card N and focus its pane | Command mode |
| `Ctrl+L` | Toggle the orchestration sidebar / pane split between 34/66 and 25/75, for every orchestration tab | Command mode, on an orchestration tab |
| `Ctrl+Z` | Zoom the focused pane to the whole frame; again to restore. See [below](#ctrlz-zooms-the-focused-agent-pane). | Command mode, on the dashboard or an orchestration tab |
| `Ctrl+E` | **Experimental, off by default.** Toggle the command-entry lock. See [below](#ctrle-locks-command-entry-to-the-orchestrator-pane). | Command mode, on an orchestration tab, with the `experimental` flag on |
| `Ctrl+C` | In a pane: sent to the program (interrupt). In command mode, the directory picker or the New Agent form: opens the [quit dialog](#dialogs). | Both modes |

Where a key is not claimed (for example `Ctrl+W`, `Ctrl+L`, `Ctrl+Z` or `Ctrl+E` while typing in a pane), it is passed to the program in the pane. While the close confirmation is open, only its keys and `Ctrl+C` work.

### `Ctrl+W` closes only from command mode

While you type in a pane, `Ctrl+W` is passed through (it deletes the previous word in shells and editors). In command mode it opens a confirmation that defaults to **Cancel**; choose **Close** to stop the agent and remove its card.

### `Ctrl+Z` zooms the focused agent pane

On the dashboard or an orchestration tab, `Ctrl+Z` in command mode gives the focused pane the whole frame and hides the card sidebar and other panes. The pane's border title gains `[Z]`. Press `Ctrl+Z` again to restore the previous layout, including a `Ctrl+L` split. Hidden panes keep running; see [Zooming the focused pane](orchestration.md#zooming-the-focused-pane) for what that means on an orchestration tab.

- In a pane, `Ctrl+Z` is passed to the program (it suspends a job in a shell).
- `1`–`9` while zoomed switches the zoom to that card's pane.
- Zoom is per tab and is not saved: a new tab starts unzoomed, and reattaching shows the full view.

### `Ctrl+E` locks command entry to the orchestrator pane

> **Experimental, off unless you enable it.** Set `experimental = true` under `[features]` in `.dot-agent-deck.toml`, or run with `DOT_AGENT_DECK_EXPERIMENTAL=1` (the environment variable wins). With the flag off, `Ctrl+E` is passed to the pane like any other key.

With the flag on, typing into a **worker** pane on an orchestration tab is locked. Keys still reach the orchestrator's pane; keys aimed at a worker are dropped and the bottom bar says `Pane locked — Ctrl+d then Ctrl+e to unlock`. Press `Ctrl+D`, then `Ctrl+E`: the deck reports `Pane entry: unlocked`, and you are still in command mode, so press `Ctrl+D` to type. Output and scrolling are not affected.

- The lock is one setting for the whole deck, applied to every orchestration tab, and every deck starts locked.
- A worker whose status is **Needs Input** accepts keys until that status clears. An agent that never reports Needs Input gets no such exception.
- While locked, focus moves to a worker that starts waiting on you and back to the orchestrator afterwards; see [Focus follows the lock](orchestration.md#focus-follows-the-lock) and [Typing into a worker is locked by default](orchestration.md#typing-into-a-worker-is-locked-by-default-experimental).

## Tab Navigation

The tab bar appears when more than one tab is open.

| Key | Action | Works in |
|---|---|---|
| `Ctrl+PageDown` | Next tab | Both modes |
| `Ctrl+PageUp` | Previous tab | Both modes |
| `Tab` / `Right` / `l` | Next tab | Command mode |
| `Shift+Tab` / `Left` / `h` | Previous tab | Command mode |

## Dashboard

Command mode. If you are typing in a pane, press `Ctrl+D` first.

| Key | Action |
|---|---|
| `j` / `Down` | Select the next card (wraps) |
| `k` / `Up` | Select the previous card (wraps) |
| `1`–`9` | Jump to card N and focus its pane |
| `Enter` | Focus the selected card's pane (start typing into it) |
| `PageUp` / `PageDown` | Scroll the focused pane back / forward |
| `/` | Filter cards |
| `Esc` | Clear the filter |
| `r` | Rename the selected agent |
| `g` | Ask the selected agent to generate `.dot-agent-deck.toml` for its directory |
| `s` or `S` | Open the **Schedules** manager (see [Schedules](scheduled-tasks.md)) |
| `y` / `n` | Approve / deny a pending permission request; only when the selected card shows **Needs Input** |
| `?` | Toggle the help overlay |

### Scrolling back through a pane

`PageUp` / `PageDown` (and the wheel) scroll the focused pane in **command mode**. While you type in a pane they are passed to the program, so a pager, an editor or the agent's own scrolling receives them.

#### How far back you can scroll depends on the agent

What there is to scroll depends on what the program in the pane writes:

- **Agents that keep their own transcript and handle the mouse**, such as `claude`: while you are typing in the pane, the wheel and scroll keys go to the agent and it scrolls its own history.
- **Agents that redraw their whole screen in place**, such as `codex`: nothing scrolls off the top, so the deck has no history to show. A scroll in command mode briefly shows `Nothing to scroll — this pane has no scrollback to move through` (`Nothing to scroll — no scrollback` in a narrow pane). It clears after a moment or on your next key, and that key still does what it normally does.
- **A full-screen program or dialog in the pane** (a picker, a permission dialog, an editor): scrolling does nothing, with no message, until it closes; the history is still there afterwards.

A pane the deck starts is empty at first, so there is nothing above the agent's first output to scroll back to.

## Editing an agent's prompt

While you type in an agent's prompt, in the TUI or in the [desktop app](desktop/settings.md#editing-and-pasting-in-an-agents-prompt), these text-editing shortcuts work in every supported agent, and each does the same in both clients on every platform:

| To | Press |
| --- | --- |
| Go to the start / end of the line | `Home` / `End`, or `⌘←` / `⌘→` |
| Move one word left / right | `Ctrl+←` / `Ctrl+→`, or `Alt+←` / `Alt+→` (`⌥←` / `⌥→` on a Mac) |
| Delete to the start of the line | `⌘⌫` |
| Delete the previous word | `Ctrl+Backspace`, or `Alt+Backspace` (`⌥⌫`) |
| Delete the next word | `Ctrl+Delete`, or `Alt+Delete` (`⌥⌦`, `fn+⌥⌫` on a laptop keyboard) |

`⌘` is the Windows key on Windows and the Super key on Linux. Your system keeps some of these chords for itself before either client sees them: macOS switches Spaces on `Ctrl+←` / `Ctrl+→`, and Windows and most Linux desktops arrange windows on the Windows key or Super with an arrow. On Linux the desktop app cannot see the Super key at all, so there `Super+←` arrives as a plain `←`; use `Home` and `End`.

"Line" means the line the cursor is on, so in a message of several lines these keys stay on that line. Agents differ in small ways: moving a word right in OpenCode lands on the start of the next word, where the other agents stop at the end of the current one, and deleting the next word in OpenCode and Devin also removes the space after it.

Keys that are not in the table reach the agent as you pressed them, the same from both clients, and do whatever that agent does with them. `Home` and `End` with `Ctrl`, `Shift` or `Alt` held are an example: `Shift+Home` selects to the start of the line in OpenCode and moves there in Claude Code, `Ctrl+Home` moves there in Pi, and Codex and Devin ignore them. To move along the line in every agent, use the keys in the table. A program that asks the terminal for the alternate form of the arrow keys, `Home` and `End`, as some editors and pagers do, gets that form from both clients.

Paste is not in the table, because each client pastes the way its platform does: in the TUI with your terminal's paste key, and in the desktop app with `⌘V` on macOS, `Ctrl+V` or `Ctrl+Shift+V` on Windows and `Ctrl+Shift+V` on Linux. `Ctrl+V` goes to the agent wherever it is not the paste key, and Claude Code, for one, pastes an image with it.

### In the TUI: what your terminal passes on

None of these is a deck shortcut, so the deck never takes them while you type. The TUI acts on a key only if your terminal passes it on, and some terminals keep these keys for themselves or send them as a plainer key. Where one does nothing, or deletes or moves by a single character, set your terminal up as below.

- **`⌘←`, `⌘→` and `⌘⌫` on macOS.** Ghostty sends them as start of line, end of line and delete to the start of the line with no setup. iTerm2 uses `⌘←` and `⌘→` to switch tabs: choose **Settings → Profiles → Keys → Key Mappings → Presets → Natural Text Editing**, which also sets up `⌥←`, `⌥→`, `⌥⌫` and `⌥⌦`. In any other terminal, bind `⌘←` to send `Ctrl+A` (hex `0x01`), `⌘→` to send `Ctrl+E` (`0x05`) and `⌘⌫` to send `Ctrl+U` (`0x15`).
- **`⌥←`, `⌥→` and `⌥⌫` on macOS.** These need the terminal to treat Option as Alt (Meta). If `⌥←` types a character instead, turn that on: in iTerm2, set **Left Option key** to **Esc+** or use the Natural Text Editing preset above; in Terminal.app, **Use Option as Meta key** in the profile's Keyboard settings.
- **`Ctrl+Backspace`.** Terminals send it as a one-character backspace unless the enhanced ("kitty") keyboard protocol is on. The TUI turns that protocol on in a terminal that supports it, such as kitty, Ghostty, foot, Alacritty 0.13 or later, iTerm2, Windows Terminal 1.25 or later, and WezTerm with `enable_kitty_keyboard = true`. GNOME Terminal and Konsole do not support it, and inside tmux the TUI does not turn it on. There, bind `Ctrl+Backspace` to send `Ctrl+W` (`0x17`), or press `Ctrl+W` or `Alt+Backspace` instead:
  - Windows Terminal (`settings.json`, `actions`): `{ "command": { "action": "sendInput", "input": "\u0017" }, "keys": "ctrl+backspace" }`
  - Alacritty: `[keyboard]` `bindings = [{ key = "Backspace", mods = "Control", chars = "\u0017" }]`
  - foot: under `[text-bindings]`, `\x17 = Control+BackSpace`
  - Konsole: in a copy of the profile's keyboard layout, `key Backspace +Control : "\x17"`
- **`⌘` chords on Linux and Windows.** The TUI reads the Super or Windows key only from a terminal that passes it on under the enhanced keyboard protocol, such as kitty. Most terminals do not, so use `Home`, `End` and the `Ctrl` chords there.

**Check it worked:** type a few words into the agent's prompt, without sending them, and press `Ctrl+Backspace` or `Alt+Backspace` (`⌥⌫`). The last word disappears.

## Directory Picker

Opened by `Ctrl+N` and by schedule authoring.

| Key | Action |
|---|---|
| `j` / `Down` | Select the next directory |
| `k` / `Up` | Select the previous directory |
| `l` / `Right` / `Enter` | Open the selected directory; in a directory with no subdirectories, choose it |
| `h` / `Left` / `Backspace` | Go up one level |
| `Space` | Choose the directory you are in |
| `/` | Filter: type to narrow the list (case-insensitive) |
| `Esc` | Clear the filter; with no filter, cancel |
| `q` | Cancel |
| `Ctrl+C` | Open the quit dialog |

While filtering: `Enter` stops editing and keeps the filter, `Backspace` deletes a character, `Up` / `Down` move, and `Esc` clears the filter. A `q` typed into the filter cancels the picker rather than adding the letter. The list wraps at both ends, and the `..` entry stays visible while a filter is active.

## New Agent Form

| Key | Action |
|---|---|
| `Tab` / `Shift+Tab` | Next / previous field |
| `Left` / `Right` or `h` / `l` | On **Mode**: cycle `No mode`, the orchestrations in the directory's `.dot-agent-deck.toml`, `schedule` and `dispatcher` |
| `Enter` | Move to the next field; on the last field, submit |
| `Esc` | Cancel |
| `Ctrl+C` | Open the quit dialog |

## Dialogs

| Dialog | Opened by | Keys |
|---|---|---|
| **Filter** | `/` | Type to narrow the cards · `Backspace` deletes · `Enter` keeps the filter and closes · `Esc` clears and closes |
| **Rename** | `r` | Type the new name · `Enter` confirms · `Esc` cancels |
| **Generate config** | `g` | `Up` / `Down` (or `k` / `j`) to choose **Yes**, **No** or **Never** · `Enter` confirms · `Esc` cancels. **Yes** asks the selected agent to write `.dot-agent-deck.toml`; **Never** stops offering it for that directory. |
| **Quit** | `Ctrl+C` in command mode | `Up` / `Down` (or `k` / `j`) to choose **Detach** (default), **Stop** or **Cancel** · `Enter` confirms · `Esc` dismisses · `Ctrl+C` again leaves immediately. **Detach** keeps the agents running; **Stop** stops them and the daemon, asking once more first while agents are running. See [Resuming Sessions](session-management.md#resuming-sessions). |
| **Close confirmation** | `Ctrl+W`, the `[Close]` button, or a tab's `[×]` | `Up` / `Down` (or `k` / `j`) to choose **Cancel** (default) or **Close** · `Enter` confirms · `Esc` dismisses. It names what it will close and closes exactly that. A key typed just before it appeared is discarded. If a pane refuses to stop, the tab is kept so you can try again. |
| **Star prompt** | Shown at startup about once every 10 launches, until dismissed | `s` opens the repository to star it · `l` or `Esc` asks again later · `d` never asks again |
| **Help overlay** | `?` | `?`, `Esc` or `q` closes it |

## Customizing Keybindings

Remap shortcuts in an optional file:

```
~/.config/dot-agent-deck/keybindings.toml
```

`DOT_AGENT_DECK_KEYBINDINGS` overrides the path. The file is read by the TUI when it starts, so restart the TUI after editing it. Each TUI reads its own file, so two TUIs attached to one daemon can use different keys.

The file has two tables, `[global]` and `[dashboard]`, each mapping an action name to a key. List only what you change; everything else keeps its default. The table name is where the action is read from, not the mode it works in.

### Example

```toml
# ~/.config/dot-agent-deck/keybindings.toml
[global]
toggle_layout = "Alt+Shift+l"                # move it off Ctrl+t
toggle_orchestration_split = "Alt+Shift+s"   # move it off Ctrl+l
toggle_zoom = "Ctrl+Alt+z"                   # move zoom off Ctrl+z
new_pane = ""                                # unbind New Agent

[dashboard]
help = "F1"                                  # help on F1 instead of ?
```

**Check:** start `dot-agent-deck`. Any problem in the file is printed to the terminal before the dashboard opens, prefixed `keybindings (<path>):` or `Invalid keybindings at <path>:`. In the TUI, `?` and the bottom bar show the new keys.

### Key notation

- **Modifiers:** `Ctrl+` (or `Control+`), `Alt+`, `Shift+`, in any order and combination, for example `Alt+Shift+t`.
- **Named keys:** `Enter`, `Esc` (or `Escape`), `Tab`, `Space`, `Up`, `Down`, `Left`, `Right`, `Backspace`, `Delete` (or `Del`), `Home`, `End`, `PageUp`, `PageDown`, `Insert`, `F1`–`F12`.
- **Any single character:** `a`, `Z`, `1`, `/`, `?` and so on.
- **Unbound:** `""` disables the action.

Modifier and named-key names are case-insensitive (`ctrl+enter` equals `Ctrl+Enter`). With `Ctrl` or `Alt`, a letter's case is ignored (`Ctrl+T` equals `Ctrl+t`); use `Shift+` for a shifted chord. Without them, an uppercase letter is the shifted key: `D` equals `Shift+d`.

### Actions and defaults

`[global]`:

| Action | Default | Action performed | Works in |
|---|---|---|---|
| `dashboard` | `Ctrl+d` | Switch between command mode and the pane | Both modes |
| `new_pane` | `Ctrl+n` | New agent | Both modes |
| `close_pane` | `Ctrl+w` | Close the selected agent or orchestration tab, with confirmation | Command mode |
| `toggle_layout` | `Ctrl+t` | Toggle stacked / tiled layout | Both modes |
| `toggle_orchestration_lock` | `Ctrl+e` | Toggle the command-entry lock (**experimental flag required**) | Command mode, orchestration tab |
| `toggle_orchestration_split` | `Ctrl+l` | Toggle the orchestration split between 34/66 and 25/75 | Command mode, orchestration tab |
| `toggle_zoom` | `Ctrl+z` | Zoom the focused pane; again to restore | Command mode, dashboard or orchestration tab |
| `jump_1` … `jump_9` | `1` … `9` | Jump to card N and focus its pane | Command mode |

`[dashboard]` (command mode):

| Action | Default | Action performed |
|---|---|---|
| `move_down` | `j` | Select the next card |
| `move_up` | `k` | Select the previous card |
| `move_left` | `h` | Previous tab |
| `move_right` | `l` | Next tab |
| `filter` | `/` | Filter cards |
| `rename` | `r` | Rename the selected agent |
| `help` | `?` | Toggle the help overlay |
| `focus_pane` | `Enter` | Focus the selected card's pane |
| `clear_filter` | `Esc` | Clear the filter |
| `approve_permission` | `y` | Approve a pending permission request |
| `deny_permission` | `n` | Deny a pending permission request |
| `generate_config` | `g` | Generate `.dot-agent-deck.toml` |
| `open_scheduled_tasks` | `s` | Open the Schedules manager |
| `scroll_pane_up` | `PageUp` | Scroll the focused pane back |
| `scroll_pane_down` | `PageDown` | Scroll the focused pane forward |

Not remappable, and they work alongside your bindings: `Down` / `Up` for card selection, `Tab` / `Shift+Tab` / `Left` / `Right` for tabs, `Ctrl+PageUp` / `Ctrl+PageDown`, `S` for the Schedules manager, and `Ctrl+C`, which always opens the quit dialog from command mode. There is no `quit` action.

Rebinding an action also retires its default key: after `scroll_pane_up = "Ctrl+u"`, `PageUp` does nothing in command mode. With both scroll actions set to `""`, the wheel is the only way to scroll.

### Problems in the file

| Problem | Result |
|---|---|
| No file | All defaults. |
| The file is not valid TOML | `Invalid keybindings at <path>: …` on stderr, and all defaults. The TUI still starts. |
| Unknown table (for example `[globale]`) | Warning; the table is ignored and the rest applies. |
| Unknown action name | Warning; that line is ignored. |
| A key the notation does not recognise | Warning; that action keeps its default. |
| An action bound to `Ctrl+C` | Warning; that action is left unbound. |
| Two actions on one key (including a new binding that lands on another action's default) | Warning; the action listed earlier in the tables above keeps the key, and the later one is unbound. |
| `action = ""` | That action is unbound. |
