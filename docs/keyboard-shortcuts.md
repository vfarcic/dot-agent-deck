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
| `Ctrl+w` | Close selected agent pane |
| `Ctrl+t` | Toggle stacked / tiled layout |

In PaneInput mode, `Ctrl+c` is delivered to the terminal as SIGINT (0x03). From the dashboard, pressing `Ctrl+c` twice triggers the quit confirmation dialog.

## Tab Navigation

The tab bar appears when more than one tab is open.

| Key | Action |
|---|---|
| `Tab` / `Right` / `l` | Next tab (cycles) |
| `Shift+Tab` / `Left` / `h` | Previous tab (cycles) |
| `Ctrl+PageDown` | Next tab (secondary) |
| `Ctrl+PageUp` | Previous tab (secondary) |

## Dashboard

| Key | Action |
|---|---|
| `j` / `Down` | Select next card |
| `k` / `Up` | Select previous card |
| `1`–`9` | Jump to card N and focus its pane |
| `Enter` | Focus selected agent pane (switches to mode tab if applicable) |
| `/` | Filter sessions |
| `Esc` | Clear filter |
| `r` | Rename session |
| `g` | Generate `.dot-agent-deck.toml` (opens config generation dialog) |
| `?` | Toggle help overlay |
| `y` / `n` | Approve / deny pending permission request |

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
