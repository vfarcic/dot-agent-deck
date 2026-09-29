---
title: Settings
---

# Settings

**Settings**, in the rail on the left, opens the app's settings. It has four sections: **Appearance**, **Daemons**, **Voice** and **Zoom**. Press `Escape` to close it. Changes are saved as you make them.

**Daemons** is on its own page, [Daemons](daemons.md), and **Voice** on [Voice control](voice.md).

## Appearance

**System**, **Light** or **Dark**. **System** follows your operating system's setting. The agent terminals stay dark in every appearance.

The TUI has no appearance setting; it uses your terminal's colours.

## Zoom

Scales the whole window, from 75% to 300%. Choose a level in **Settings → Zoom**, or use the keys:

| Keys | Does |
| --- | --- |
| `Ctrl` or `⌘` with `=` or `+` | Zoom in one step |
| `Ctrl` or `⌘` with `-` | Zoom out one step |
| `Ctrl` or `⌘` with `0` | Back to 100% |

Either modifier works on every platform, and the keys work while an agent's terminal has focus too: they are not passed to the agent. The level is saved in the settings file.

This is not the TUI's `Ctrl+z`, which makes the focused pane fill the TUI's frame and changes no text size.

## The settings file

The footer of **Settings** names the file your settings are stored in. It is `desktop.toml` in the same directory as the TUI's config: `~/.config/dot-agent-deck/desktop.toml` on macOS and Linux. Set `DOT_AGENT_DECK_DESKTOP_CONFIG` in the app's environment to use another file.

The desktop app and the TUI share no settings file: the TUI's `config.toml` and `keybindings.toml` do not affect the app, and `desktop.toml` does not affect the TUI. What both clients share is the daemon's own configuration, such as [`default_dir`](../configuration.md), because it belongs to the daemon.

If the file cannot be read, **Settings** says so in the footer instead of the path.

## Keyboard shortcuts

The desktop app has a few keyboard shortcuts of its own. None of the TUI's [keyboard shortcuts](../keyboard-shortcuts.md) apply to it, and it has no keybinding customisation.

| Keys | Where | Does |
| --- | --- | --- |
| `Ctrl+N` / `⌘N` | Dashboard, with no agent pane open | Opens [New agent](new-agent.md) |
| `Escape` | Agent pane | Closes the pane and returns to the Dashboard |
| `Escape` | Settings | Closes Settings |
| `Ctrl` / `⌘` with `=`, `+`, `-`, `0` | Everywhere | Zoom (above) |
| `j` `k` `l` `h` `Space` `/` `.` `q` | New agent's directory list | Move, open, go up, use, filter, show hidden, close (see [New agent](new-agent.md#directory)) |
