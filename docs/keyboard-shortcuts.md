---
sidebar_position: 5
title: Keyboard Shortcuts
---

# Keyboard Shortcuts

## Global Shortcuts (work from any mode)

| Key | Action |
|---|---|
| `Ctrl+d` | Return to dashboard |
| `Ctrl+n` | New pane (directory picker, then name + command form) |
| `Ctrl+w` | Close selected agent pane |
| `Ctrl+t` | Toggle stacked / tiled layout |

In PaneInput mode, `Ctrl+c` is delivered to the terminal as SIGINT (0x03). From the dashboard, pressing `Ctrl+c` twice triggers the quit confirmation dialog.

## Dashboard

| Key | Action |
|---|---|
| `j` / `Down` | Move down |
| `k` / `Up` | Move up |
| `h` / `Left` | Move left |
| `l` / `Right` | Move right |
| `1`–`9` | Jump to card N and focus its pane |
| `Enter` | Focus selected agent pane |
| `/` | Filter sessions |
| `Esc` | Clear filter |
| `r` | Rename session |
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

## New Pane Form

| Key | Action |
|---|---|
| `Tab` / `Shift+Tab` | Switch between Name and Command fields |
| `Enter` | Confirm field / submit form |
| `Esc` | Cancel |
