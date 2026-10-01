# Settings

**Settings**, in the rail on the left, opens the app's settings over the current screen. It has four sections: **Appearance**, **Daemons**, **Voice** and **Zoom**. Changes are saved to the [settings file](#the-settings-file) as you make them. Press `Escape` to close it.

**Daemons** is described on its own page, [Daemons](daemons.md), and **Voice** on [Voice Control](voice.md).

## Appearance

**System**, **Light** or **Dark**; **System** is the default and follows your operating system's setting. The agent terminals stay dark in every appearance.

The TUI has no appearance setting; it uses your terminal's colours.

## Zoom

Scales the whole window. The levels are 75%, 90%, 100%, 110%, 125%, 150%, 175%, 200%, 250% and 300%; 100% is the default. Choose one in **Settings → Zoom**, or use the keys:

| Keys | Does |
| --- | --- |
| `Ctrl` or `⌘` with `=` or `+` | Zoom in one level |
| `Ctrl` or `⌘` with `-` | Zoom out one level |
| `Ctrl` or `⌘` with `0` | Back to 100% |

Either modifier works on every platform, and the keys work while an agent's terminal has focus too: they are not passed to the agent. The level is saved in the settings file.

This is not the TUI's `Ctrl+z`, which makes the focused pane fill the TUI's frame and changes no text size.

## The settings file

The footer of **Settings** names the file your settings are stored in: `~/.config/dot-agent-deck/desktop.toml`, beside the TUI's `config.toml`. Set `DOT_AGENT_DECK_DESKTOP_CONFIG` to a file path in the app's environment to use another file. The file is written by the app; you can also edit it by hand while the app is closed.

It holds:

| Table and key | Setting | Values | Default |
| --- | --- | --- | --- |
| `[appearance]` `mode` | Appearance | `"system"`, `"light"`, `"dark"` | `"system"` |
| `[zoom]` `level` | Zoom, as a scale factor | `0.75`, `0.9`, `1.0`, `1.1`, `1.25`, `1.5`, `1.75`, `2.0`, `2.5`, `3.0`; another number is rounded to the nearest of these | `1.0` |
| `[endpoints]` `selection` | What the Dashboard's **Daemon** selector shows | `"local"` (This machine), `"all"` (All daemons), or a remote daemon's id | `"local"` |
| `[voice]` | The **Voice** settings | See [Voice Control → Settings → Voice](voice.md#settings--voice) | Written once you change a voice setting |

The remote daemons themselves are not in this file: they are in `remotes.toml`, which the app shares with the CLI (see [Daemons](daemons.md)). API keys for voice are not in it either; they are in your operating system's keychain. [Configuration](../configuration.md#desktoptoml-reference) has the full reference.

The desktop app and the TUI share no settings file: the TUI's `config.toml` and `keybindings.toml` do not affect the app, and `desktop.toml` does not affect the TUI. What both clients share is the daemon's own configuration, such as [`default_dir`](../configuration.md#set-the-directory-new-agents-start-browsing-in) and [`default_command`](../configuration.md#set-the-command-new-agents-start-with), because it belongs to the daemon.

**If the file cannot be read** (it is not valid TOML, a value has the wrong type or is refused, the path is a directory or a symlink, or the file is larger than 256 KiB), the footer says so instead of showing the path, the app runs on default settings for this session, and it does not save over the file. Fix or remove the file, then restart the app. An unknown key is ignored, and an unknown `mode` reads as `"system"`.

## Keyboard shortcuts

The desktop app has a few keyboard shortcuts of its own. None of the TUI's [keyboard shortcuts](../keyboard-shortcuts.md) apply to it, and it has no keybinding customisation.

| Keys | Where | Does |
| --- | --- | --- |
| `Ctrl+N` / `⌘N` | Dashboard, with no agent pane open and no text field focused | Opens [New agent](new-agent.md) |
| `Escape` | Agent pane, when you are not typing in its terminal | Closes the pane and returns to the Dashboard |
| `Ctrl+Shift+C` / `⌘C` | An agent's terminal, with text selected | Copies the selected text. Plain `Ctrl+C` still goes to the agent as an interrupt |
| `Escape` | Settings | Closes Settings |
| `Escape` | New agent | Clears the directory filter; otherwise closes the dialog, keeping a draft |
| `Ctrl` / `⌘` with `=`, `+`, `-`, `0` | Everywhere | Zoom (above) |
| `j` `k` `l` `h` `Space` `/` `.` `q` | New agent's directory list | Move, open, go up, use, filter, show hidden, close (see [New agent → Directory](new-agent.md#directory)) |
| `←` `→` | New agent's Mode chips | Previous or next chip |

### Keys typed into an agent's terminal

While you are typing in an agent's terminal, your keys go to the agent the way they do in the TUI, so the agent's own shortcuts work: `Escape`, `Tab`, `Shift+Tab`, the arrow keys and `Ctrl` with a letter all reach it. `Ctrl+C` interrupts the agent, and `Escape` goes to the agent rather than closing the pane, so use **Back to dashboard** to leave. The zoom keys stay with the app.

To start a new line without sending the message, press `Shift+Enter` or `Ctrl+J`; both work in every supported agent. `Ctrl+Enter` reaches the agent as `Ctrl+Enter`, and what it does is up to the agent, as it is in the TUI. See [Shift+Enter or Ctrl+Enter sends the message](../troubleshooting.md#shiftenter-or-ctrlenter-sends-the-message) for what each agent does with it.

#### Editing and pasting in an agent's prompt

The agent's prompt answers to your platform's usual text-editing shortcuts, in every supported agent:

| To | macOS | Windows | Linux |
| --- | --- | --- | --- |
| Go to the start / end of the line | `⌘←` / `⌘→` | `Home` / `End` | `Home` / `End` |
| Move one word left / right | `⌥←` / `⌥→` | `Ctrl+←` / `Ctrl+→` | `Ctrl+←` / `Ctrl+→` |
| Delete to the start of the line | `⌘⌫` | | |
| Delete the previous word | `⌥⌫` | `Ctrl+Backspace` | `Ctrl+Backspace` |
| Delete the next word | `⌥⌦` (`fn+⌥⌫` on a laptop keyboard) | `Ctrl+Delete` | `Ctrl+Delete` |
| Paste | `⌘V` | `Ctrl+V` or `Ctrl+Shift+V` | `Ctrl+Shift+V` |

The app follows the computer you are typing on, not the machine the agent runs on: on a Mac connected to a Linux daemon, `⌘←` still goes to the start of the line. "Line" means the line the cursor is on, so in a message of several lines these keys stay on that line.

Agents differ in small ways. Moving a word right in OpenCode lands on the start of the next word, where the other agents stop at the end of the current one, and deleting the next word in OpenCode and Devin also removes the space after it.

`Ctrl+V` pastes only on Windows. On macOS and Linux it goes to the agent, which may have its own use for it — Claude Code, for one, pastes an image with it.
