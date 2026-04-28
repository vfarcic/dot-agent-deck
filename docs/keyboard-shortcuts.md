---
sidebar_position: 6
title: Keyboard Shortcuts
---

# Keyboard Shortcuts

## Global Shortcuts (work from any mode)

| Key | Action |
|---|---|
| `Ctrl+d` | Enter command / navigation mode |
| `Ctrl+n` | New pane (directory picker, then name + command form) |
| `Ctrl+w` | Close selected pane on the dashboard, or tear down the entire mode tab (agent + side panes) when used on a mode tab. The dashboard tab itself cannot be closed. |
| `Ctrl+t` | Toggle stacked / tiled layout |

In PaneInput mode, `Ctrl+c` is delivered to the terminal as SIGINT (0x03). From the dashboard (command mode), pressing `Ctrl+c` opens a quit confirmation dialog; press it again to quit immediately, or use the dialog keys (see [Dialogs](#dialogs)) to choose Yes / No.

## Tab Navigation

The tab bar appears when more than one tab is open.

| Key | Action |
|---|---|
| `Ctrl+PageDown` | Next tab (works from any mode, including in a focused pane) |
| `Ctrl+PageUp` | Previous tab (works from any mode, including in a focused pane) |
| `Tab` / `Right` / `l` | Next tab — **only in command mode** (press `Ctrl+d` first; otherwise the keystroke is sent to the agent pane) |
| `Shift+Tab` / `Left` / `h` | Previous tab — **only in command mode** (press `Ctrl+d` first; otherwise the keystroke is sent to the agent pane) |

## Mode Tab

These shortcuts work in Normal mode when a mode tab is active.

| Key | Action |
|---|---|
| `j` / `Down` | Focus next pane (cycles: agent → side panes → agent) |
| `k` / `Up` | Focus previous pane (cycles: agent → last side pane → … → agent) |
| `Enter` | Enter PaneInput mode on selected pane (agent pane if none selected) |
| `Esc` | Deselect side pane (return focus indicator to agent) |
| Mouse click | Click a side pane to select it; click agent pane to deselect |

In PaneInput mode, use `Ctrl+d` to return to Normal mode.

## Dashboard

These shortcuts work in **command mode**. If you're typing in an agent pane, press `Ctrl+d` first to leave the pane — otherwise the keystroke is sent to the agent.

| Key | Action |
|---|---|
| `1`–`9` | Jump to card N and focus its pane |
| `/` | Filter sessions (opens filter input — see [Dialogs](#dialogs)) |
| `r` | Rename selected session (opens rename input — see [Dialogs](#dialogs)) |
| `g` | Generate `.dot-agent-deck.toml` (opens config-generation prompt — see [Dialogs](#dialogs)) |
| `?` | Toggle help overlay |
| `y` / `n` | Approve / deny a pending permission request (only when an agent is waiting) |
| `Esc` | Clear active filter |

> **Note:** `j`/`k` and `Up`/`Down` for cycling selection through cards are documented in the in-app help but are currently not working — see [#68](https://github.com/vfarcic/dot-agent-deck/issues/68). Use `1`–`9` to jump directly to a card.

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
| **Quit confirmation** | `Ctrl+c` from command mode | `Up`/`Down` (or `k`/`j`) to choose **Yes** / **No** · `Enter` to confirm · `Esc` to dismiss · `Ctrl+c` again to quit immediately |
| **Help overlay** | `?` | `?`, `Esc`, or `q` to dismiss |
