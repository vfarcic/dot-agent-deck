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
- **Codex** — when the deck launches a `codex` pane, it writes a `hooks.json` into your Codex home (`$CODEX_HOME`, or `~/.codex`) whose hooks forward prompt, tool, and turn events to the dashboard. Your existing Codex `config.toml` and any hooks you authored yourself are preserved (the deck merges, it never overwrites).

Auto-install is idempotent and best-effort — if an agent directory is missing the step is silently skipped, and errors are logged without blocking startup.

### Codex events not showing (launching Codex through a script)

Codex only runs hooks it *trusts*. When the deck launches `codex` **directly** (its default command, or any pane whose command starts with `codex`), it adds `--dangerously-bypass-hook-trust` automatically so the deck's own hooks run without an interactive trust prompt — you don't have to do anything.

But if you launch Codex through a **wrapper script or launcher** — `devbox run …`, a `run_codex_agent.sh`, an alias, or a custom path whose name isn't `codex` — the deck can't reach inside your launcher to add that flag. The deck still installs the hooks into your Codex home, so the one thing you must do is add the flag to the `codex` line **inside your script**:

```sh
#!/bin/sh
# a launcher the deck wraps; add the flag yourself so the deck's hooks run
exec codex --dangerously-bypass-hook-trust "$@"
```

(Alternatively, run Codex once and approve the deck's hooks via its interactive `/hooks` review — Codex then remembers the trust and you can drop the flag.) Without one of these, Codex refuses to run the deck's hooks and the card falls back to coarse status only — no live tool or prompt detail.

### Codex as a role or worker: allow sandbox network access

Codex is usable as an orchestrator **role** or a delegated **worker**. In those flows the Codex agent has to reach the dashboard daemon — it runs `dot-agent-deck delegate …` to hand work to another pane and `dot-agent-deck work-done …` to report completion, both of which connect to the daemon over its local socket. Codex's `workspace-write` sandbox blocks that connection by default, so those commands silently fail and the orchestration pipeline never moves.

Launch Codex with `workspace-write`, non-interactive approvals, **and** sandbox network access so the deck's CLI can reach the daemon:

```bash
codex --sandbox workspace-write --ask-for-approval never \
  -c "sandbox_workspace_write.network_access=true"
```

The `-c "sandbox_workspace_write.network_access=true"` override is the important part — without it, `delegate` / `work-done` can't reach the daemon even though the pane itself looks healthy. Point a role at Codex by setting that full command as the role's `command` in `.dot-agent-deck.toml`.

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

## A bare command like `claude`, `opencode`, or `pi` fails to spawn

If a pane comes up with an error such as *"Unable to spawn `claude` because it doesn't exist on the filesystem and was not found in PATH"*, the daemon couldn't resolve that bare command against its `PATH`.

### Why This Happens

The daemon resolves a bare command against its own process `PATH`. At startup it captures your **login-shell PATH** — the PATH you get in an interactive login shell, the same as when you SSH in — so commands installed under, for example, `~/.local/bin` or a directory added by `~/.bashrc` (such as `~/.opencode/bin`) normally resolve. You can still hit this if the command isn't on your login shell's PATH at all, or if it was added — or the agent was installed — **after** the daemon last started, because the PATH is captured only once per daemon start.

### Fix

1. Confirm the command resolves in a fresh login shell of your own:
   ```bash
   $SHELL -ilc 'command -v claude'
   ```
   If that prints nothing, fix your shell startup files (for example, add the install directory to `PATH` in `~/.profile` or `~/.bashrc`) until it does.

2. Restart the daemon so it re-captures the login-shell PATH:
   ```bash
   dot-agent-deck daemon restart
   ```

If `command -v` finds the command in your login shell but a pane still can't spawn it after a daemon restart, capture debug logs with `DOT_AGENT_DECK_LOG=1` and file an issue — the daemon logs the PATH it captured at startup.

## Delegate prompts silently no-op after staying on an older daemon

After upgrading the `dot-agent-deck` binary, the new TUI can keep talking to a daemon that was spawned by the *previous* version. The wire format stays compatible, but newer features (delegate role maps, orchestration tab fields, and similar internal refactors) silently no-op because the older daemon doesn't know about the newer shape.

This only happens when you are **deliberately** still on the older daemon. The common cause: you upgraded while agents were running, the launch prompt warned that restarting would stop them, and you **declined the restart to keep your agents** — which leaves the new TUI attached to the older daemon on purpose. (It can also happen with a very old, pre-handshake binary that attached without any version check.) With no agents running, the handshake restarts the daemon silently, so a fresh daemon at the new version is the normal outcome.

### Symptom

You upgrade `dot-agent-deck`, keep your running agents on the existing daemon, and delegate prompts arrive in the TUI as if they were queued — but the orchestration pipeline never moves. Other recently-added features may also fail to take effect without an obvious error.

### Fix

When you are ready to move to the new version, let the daemon restart. The simplest path is to finish or detach your running agents and relaunch — with no agents left, the handshake restarts the daemon silently:

```bash
dot-agent-deck
```

If agents are still running and you want to upgrade now, relaunch and press **S** at the prompt (it names the live agents first) to restart the daemon onto the new version — this stops those agents. The TUI then lazy-spawns a fresh daemon at the new binary's version on its way into the dashboard.

If the relaunch is happening from a script, CI job, or piped context (no TTY) while agents are running, the TUI cannot prompt. Run `daemon stop` explicitly first:

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

On every launch, the TUI performs a build-version handshake with the daemon. When the binary versions differ, the resolution depends only on whether managed agents are running. With **no agents running**, the older daemon is restarted **silently** — there is nothing to lose. With **agents running** and an interactive terminal, the TUI prompts you: the prompt **names the live agents** and warns that restarting stops them, then offers a single-keystroke choice — press **S** to restart onto the new version, or any other key to **keep the current daemon** and stay attached to it with your agents intact. Keeping the current daemon is what leaves you on the older shape. When the TUI is not attached to a terminal (CI, pipes) and agents are running, it prints the recovery hint to stderr and exits non-zero instead of prompting.

## Enabling Debug Logs

When something goes wrong and the dashboard's status messages aren't enough to diagnose it, set the `DOT_AGENT_DECK_LOG` environment variable to capture tracing output to a file:

```bash
# Default — writes to /tmp/dot-agent-deck.log
DOT_AGENT_DECK_LOG=1 dot-agent-deck

# Custom path
DOT_AGENT_DECK_LOG=/tmp/my-debug.log dot-agent-deck
```

The log file captures session events, hook activity, mode-tab restoration, and any errors logged by the daemon. Attach the relevant excerpt when filing an issue. See [Configuration › Environment Variables](configuration.md#environment-variables) for the full list of variables.
