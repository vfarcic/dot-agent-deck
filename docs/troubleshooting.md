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

## A bare command like `claude` or `opencode` fails to spawn

If a pane comes up with an error such as *"Unable to spawn `claude` because it doesn't exist on the filesystem and was not found in PATH"*, the daemon couldn't resolve that bare command against its `PATH`.

### Why This Happens

The daemon resolves a bare command against its own process `PATH`. At startup it captures your **login-shell PATH** (see [Configuration › Command Resolution and the Login-Shell PATH](configuration.md#command-resolution-and-the-login-shell-path)) so commands installed under, for example, `~/.local/bin` normally resolve. You can still hit this if the command isn't on your login shell's PATH at all, or if it was added — or the agent was installed — **after** the daemon last started, because the PATH is captured only once per daemon start.

### Fix

1. Confirm the command resolves in a fresh login shell of your own:
   ```bash
   $SHELL -lc 'command -v claude'
   ```
   If that prints nothing, fix your shell profile (for example, add the install directory to `PATH` in `~/.profile`) until it does.

2. Restart the daemon so it re-captures the login-shell PATH:
   ```bash
   dot-agent-deck daemon restart
   ```

If `command -v` finds the command in your login shell but a pane still can't spawn it after a daemon restart, capture debug logs with `DOT_AGENT_DECK_LOG=1` and file an issue — the daemon logs the PATH it captured at startup.

## Delegate prompts silently no-op after an upgrade

After upgrading the `dot-agent-deck` binary, the new TUI may attach to a daemon that was spawned by the *previous* version. The wire format stays compatible, but newer features (delegate role maps, orchestration tab fields, and similar internal refactors) silently no-op because the stale daemon doesn't know about the newer shape.

### Symptom

You upgrade `dot-agent-deck`, relaunch it, and delegate prompts arrive in the TUI as if they were queued — but the orchestration pipeline never moves. Other recently-added features may also fail to take effect without an obvious error.

If you see this, you connected to the stale daemon without going through the version-mismatch prompt — either because an earlier `dot-agent-deck` version (pre-handshake) attached silently, or because the relaunch happened in a non-interactive context. Newer builds prompt you at launch (see *Why this happens* below).

### Fix

Relaunch `dot-agent-deck` from an interactive terminal:

```bash
dot-agent-deck
```

You'll see the mismatch prompt — press **S** to stop the stale daemon and continue. The TUI lazy-spawns a fresh daemon at the new binary's version on its way into the dashboard.

If the relaunch is happening from a script, CI job, or piped context (no TTY), the TUI cannot prompt. Run `daemon stop` explicitly first:

```bash
dot-agent-deck daemon stop
dot-agent-deck
```

If managed agents are still running and you cannot detach them first, pass `--force` to terminate them along with the daemon:

```bash
dot-agent-deck daemon stop --force
```

See [Installation › Recycling the local daemon](installation.md#recycling-the-local-daemon) for the full command reference, including the data-loss guard and exit codes.

### Why this happens

On every launch, the TUI performs a build-version handshake with the daemon. When it detects a mismatch *and* your terminal is interactive, it prompts you with both build IDs and a single-keystroke choice — press **S** to recycle the daemon and continue, or **Q** to abort. (If managed agents are running, the prompt itself lists them and warns before you confirm.) When the TUI is not attached to a terminal (CI, pipes), it prints the recovery hint to stderr and exits non-zero instead of prompting.

## Enabling Debug Logs

When something goes wrong and the dashboard's status messages aren't enough to diagnose it, set the `DOT_AGENT_DECK_LOG` environment variable to capture tracing output to a file:

```bash
# Default — writes to /tmp/dot-agent-deck.log
DOT_AGENT_DECK_LOG=1 dot-agent-deck

# Custom path
DOT_AGENT_DECK_LOG=/tmp/my-debug.log dot-agent-deck
```

The log file captures session events, hook activity, mode-tab restoration, and any errors logged by the daemon. Attach the relevant excerpt when filing an issue. See [Configuration › Environment Variables](configuration.md#environment-variables) for the full list of variables.
