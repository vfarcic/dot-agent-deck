---
sidebar_position: 8
title: Troubleshooting
---

# Troubleshooting

## Shift+Enter Not Working in Ghostty Terminal

When running Claude Code or other AI coding agents inside dot-agent-deck with the **Ghostty terminal emulator**, Shift+Enter may not create newlines in chat inputs as expected.

### Why This Happens

Ghostty intercepts Shift+Enter for its own terminal features when applications enable mouse capture mode. This prevents the keystroke from reaching the embedded application.

### Solution

Add the following line to your Ghostty configuration file:

**Location:** `~/Library/Application Support/com.mitchellh.ghostty/config`

```
keybind = shift+enter=csi:13;2u
```

This uses the CSI u format (modern keyboard protocol) to send Shift+Enter with the SHIFT modifier preserved.

### How to Apply

1. Edit the Ghostty config file:
   ```bash
   nano ~/Library/Application\ Support/com.mitchellh.ghostty/config
   ```

2. Add the keybind line (you can add it anywhere in the file)

3. Restart Ghostty or reload its configuration

4. Test in dot-agent-deck: Shift+Enter should now create newlines in chat applications

### Verification

After applying the fix:
- Regular **Enter** should submit messages
- **Shift+Enter** should create newlines without submitting

### Note

This configuration only affects Ghostty. Other terminal emulators (iTerm2, Alacritty, Warp, etc.) typically work without additional configuration.

## Hooks

Hooks are **auto-installed on every startup** — most users never need to think about them. The CLI detects which agents are present and installs hooks accordingly:

- **Claude Code** (`~/.claude/` detected) — writes entries into `~/.claude/settings.json` for hook types: SessionStart, SessionEnd, UserPromptSubmit, PreToolUse, PostToolUse, Notification, Stop, PreCompact, SubagentStart, SubagentStop.
- **OpenCode** (`~/.opencode/` detected) — creates a JS plugin at `~/.opencode/plugin/dot-agent-deck/index.js` that forwards session, tool, and permission events.

Auto-install is idempotent and best-effort — if an agent directory is missing the step is silently skipped, and errors are logged without blocking startup.

### Manual Management

The `hooks install` and `hooks uninstall` commands are available when you need to debug or temporarily remove hooks:

```bash
# Install manually
dot-agent-deck hooks install                    # Claude Code
dot-agent-deck hooks install --agent opencode   # OpenCode

# Remove hooks
dot-agent-deck hooks uninstall                    # Claude Code
dot-agent-deck hooks uninstall --agent opencode   # OpenCode
```

> **Note:** If you uninstall hooks manually, the next dashboard launch will re-install them automatically.

## Enabling Debug Logs

When something goes wrong and the dashboard's status messages aren't enough to diagnose it, set the `DOT_AGENT_DECK_LOG` environment variable to capture tracing output to a file:

```bash
# Default — writes to /tmp/dot-agent-deck.log
DOT_AGENT_DECK_LOG=1 dot-agent-deck

# Custom path
DOT_AGENT_DECK_LOG=/tmp/my-debug.log dot-agent-deck
```

The log file captures session events, hook activity, mode-tab restoration, and any errors logged by the daemon. Attach the relevant excerpt when filing an issue. See [Configuration › Environment Variables](configuration.md#environment-variables) for the full list of variables.
