---
sidebar_position: 8
title: Troubleshooting
---

# Troubleshooting

## Shift+Enter Submits Instead of Inserting a Newline

Inside an embedded agent pane, **Shift+Enter** inserts a newline into the agent's draft and plain **Enter** submits it — the same behavior you get running the agent directly. This works with **no terminal configuration** on any terminal that implements the enhanced ("kitty") keyboard protocol, which the deck negotiates for you at startup.

### What Used to Cause This

The break was never in your terminal emulator. It was two dot-agent-deck defects that compounded, both now fixed:

- The deck never asked the terminal for the enhanced keyboard protocol, so a terminal that *could* encode Shift+Enter distinctly stayed in legacy mode and delivered a bare carriage return — the SHIFT modifier was gone before any deck code ran.
- The deck's pane-input encoder dropped the SHIFT modifier even when it did arrive, mapping Enter to `\r` unconditionally. Shift+Enter and plain Enter were literally the same byte on the wire, so every agent read both as "submit".

Earlier versions of this page blamed Ghostty for intercepting the keystroke and prescribed adding `keybind = shift+enter=csi:13;2u` to the Ghostty config. That attribution was wrong, and the keybind could not have been the fix on its own — it made the modifier arrive, and the deck then discarded it. If you already have that line in `~/Library/Application Support/com.mitchellh.ghostty/config`, you can leave it: it still works and does no harm, it is simply no longer necessary.

### If It Still Submits

- **You are running the deck inside tmux.** tmux reports no keyboard-enhancement support, so the deck skips the negotiation there and Shift+Enter falls back to its previous behavior. Either run the deck outside tmux, or have tmux pass extended keys through with `set -s extended-keys always` and `set -s extended-keys-format csi-u`.
- **Your terminal does not implement the enhanced keyboard protocol.** Bind the keystroke to the CSI u encoding yourself if your terminal supports custom keybinds — in Ghostty that is the `keybind = shift+enter=csi:13;2u` line above. The deck forwards the modifier faithfully either way.
- **You are on an older dot-agent-deck.** Upgrade; no configuration change is needed after that.

## Hooks

Hooks are **auto-installed on every startup** — most users never need to think about them. The CLI detects which agents are present and installs hooks accordingly:

- **Claude Code** (`~/.claude/` detected) — writes entries into `~/.claude/settings.json` for hook types: SessionStart, SessionEnd, UserPromptSubmit, PreToolUse, PostToolUse, Notification, Stop, PreCompact, SubagentStart, SubagentStop. Only the deck's own hook commands are touched; your `model`, `env`, `permissions` and every other setting survive byte-for-byte, and if you put one of your own hooks in the same rule object as the deck's, `hooks uninstall` removes only the deck's command and leaves yours (with its matcher) in place. The read-modify-write is serialized by an in-process mutex and published atomically (temp file + `rename`), so a crash mid-write never leaves a truncated file, and the file keeps its existing permissions (a settings file the deck creates itself is owner-only). A `settings.json` the deck cannot parse — one trailing comma is enough — is backed up to `settings.json.bak` and the install *or uninstall* errors out rather than clobbering it. A `settings.json` that is a **symlink** (a dotfiles arrangement) is refused for the same reason: the deck will neither replace your link with a regular file nor write through it to a path outside `~/.claude/`. Point it at a real file, or edit the linked file's hooks yourself.
- **OpenCode** (`~/.opencode/` detected) — creates a JS plugin at `~/.opencode/plugin/dot-agent-deck/index.js` that forwards session, tool, and permission events.
- **Codex** (`codex` found on `PATH`) — writes a `hooks.json` into your Codex home (`$CODEX_HOME`, or `~/.codex`) whose hooks forward prompt, tool, and turn events to the dashboard, and records trust for **exactly those hooks** in that home's `config.toml` (Codex only runs hooks it trusts). Both happen at startup and again whenever the deck launches a Codex pane, so they work however you launch Codex. Your own hooks are preserved (the deck merges, it never overwrites), and `config.toml` is edited surgically — comments, your model choice, and any trust records you made yourself are left byte-for-byte intact. The deck never trusts a hook it didn't author: a third-party hook sitting in the same file stays untrusted.
- **Devin** (`devin` found on `PATH`) — merges a `"hooks"` object into Devin's user config, whose commands shell `dot-agent-deck hook --agent devin`. The config is located the way Devin locates it: `$XDG_CONFIG_HOME/devin/config.json` when that variable is set, and `~/.config/devin/config.json` otherwise. Devin ships a Claude-Code-compatible hooks engine, so its native command hooks post the same stdin JSON shape Claude's do and ride the existing hook socket — no wrapper, no trust ceremony. Only the `"hooks"` key is touched; your `agent` (model), `permissions`, `mcpServers`, `theme_mode`, and every other setting survive byte-for-byte. The read-modify-write is serialized by an in-process mutex and published atomically (temp file + `rename`), so a crash mid-write never leaves a truncated config, and the file keeps its existing permissions (a config the deck creates itself is owner-only). Devin documents its config as JSON *with comment support*, which the deck's parser cannot edit in place: a config it cannot parse is backed up to `config.json.bak` and the install errors rather than clobbering it.

Auto-install is idempotent and best-effort — if an agent directory is missing the step is silently skipped, and errors are logged without blocking startup.

### A hook fails with `not found` and names a path you never typed

If an agent reports something like `Stop hook error: /bin/sh: 1: /home/you/code/dot-agent-deck-pr-356/target/release/dot-agent-deck: not found`, the hook command in your agent's config points at a **build directory** rather than at your installed deck. Build directories are not durable: `cargo clean` removes them and they disappear entirely when a git worktree is deleted. Older versions of the deck wrote whichever binary was installing the hooks into config, so running the deck once from a checkout was enough to leave every hook pointing at a path that would stop existing later. The install also runs silently on every dashboard launch, which is why the failure tends to surface days after the run that caused it.

Recent versions do not write such a path. A deck running from a checkout resolves your installed deck instead (`~/.local/bin/dot-agent-deck`, or `dot-agent-deck` on your `PATH`), and if it cannot find one, `hooks install` fails with a message telling you what to install and writes nothing rather than leaving a broken command behind. They also repair themselves: on the next launch, a deck-owned hook whose binary is confirmed missing is rewritten to the durable path, for Claude Code's `settings.json`, Codex's `hooks.json` and the OpenCode plugin alike. For **Claude Code and OpenCode**, a hook whose path still works is deliberately left alone even if it differs from what the deck would write — your own wrapper, or a second checkout you are deliberately using, is not something a startup should quietly repoint. **Codex and Devin do not yet keep that promise:** they re-pin their own rules to the resolved durable path on every launch, so a deck-owned Codex or Devin hook pointing at a second, still-valid install is repointed rather than preserved ([#730](https://github.com/vfarcic/dot-agent-deck/issues/730) tracks closing the gap). That is not a harmless one-time correction if you launch the deck from more than one install: two checkouts, or an installed deck and a checkout, resolve *different* durable paths, so the pin never settles — it **flaps** between them, following whichever install you launched last. A hook of your own that merely mentions `dot-agent-deck` is never touched, on any of the four.

To fix it now:

1. **Upgrade the deck**, then start the dashboard once. That is usually the whole fix — the broken entries are repaired on startup.
2. **Or repair explicitly**, per agent: `dot-agent-deck hooks install`, adding `--agent codex`, `--agent opencode` or `--agent devin` for the others.
3. **If the install refuses**, it is telling you there is no durable deck on the machine to point at. Install one — `cargo install --path .` from a checkout, or put the release binary on your `PATH` — and run the install again.

You can confirm the result by looking for the path in the config: `grep -o '[^"]*dot-agent-deck' ~/.claude/settings.json | sort -u`. It should name your installed binary, never a `target/debug` or `target/release` directory.

### Codex events not showing

Codex only runs hooks it *trusts*, and the deck handles that for you: it records trust for its own hook entries — and only those — in your Codex home's `config.toml`. This is independent of how you start Codex, so a launcher (`devbox run codex-big`, a `run_codex_agent.sh`, an alias, a path whose name isn't `codex`) needs **nothing** added to it. Launch Codex however you already do.

If a Codex card still shows only coarse status with no tool or prompt detail, check these in order:

1. **Is `codex` on the deck's `PATH`?** The setup step self-skips when it isn't. Run `codex --version` from the same shell you start the deck from, then restart the deck.
2. **Does your launcher re-export `CODEX_HOME`?** The deck pins the home it prepared onto the process it starts, but a script can override that before running `codex` — and the deck's hooks and trust records live in the *original* home. Drop the re-export, or point it at the same home the deck uses (`$CODEX_HOME`, else `~/.codex`).
3. **Re-run the install manually** to see any error the silent startup step swallowed: `dot-agent-deck hooks install --agent codex`.
4. **Approve them by hand as a fallback:** run Codex once and approve the deck's hooks in its interactive `/hooks` review. Codex remembers that trust for subsequent runs.

Trust is pinned to each hook's exact content, so it deliberately fails *closed*: if a definition changes underneath a trust record, Codex refuses to run it and the card falls back to coarse status rather than running something unreviewed. Re-running the install re-records trust for the new content.

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
dot-agent-deck hooks install --agent codex      # Codex
dot-agent-deck hooks install --agent devin      # Devin

# Remove hooks
dot-agent-deck hooks uninstall                    # Claude Code
dot-agent-deck hooks uninstall --agent opencode   # OpenCode
dot-agent-deck hooks uninstall --agent codex      # Codex
dot-agent-deck hooks uninstall --agent devin      # Devin
```

> **Note:** If you uninstall hooks manually, the next dashboard launch will re-install them automatically.

## A bare command like `claude`, `opencode`, `pi`, `codex`, or `devin` fails to spawn

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

## A pane says "disconnected" and ignores what you type

A pane whose title ends in `— disconnected` is no longer connected to an agent. Its last output stays on screen so you can read what happened, but the pane cannot accept input again — typing into it reports that it is disconnected rather than sending anything. Close the pane and start a new one; there is nothing to recover in place.

The deck reaches this state only after it has already tried to reconnect and failed. When an agent goes away — a crash, an external `kill`, or a restart that never comes back — the deck looks the agent up again and re-attaches, which is what makes a normal respawn invisible to you. It gives up in two cases, and the status message tells you which:

- **"Agent exited on every restart"** — the agent was found and re-attached to repeatedly, but produced no output each time. Usually the agent itself fails immediately on startup: check its command and working directory, and try running that command directly in a shell.
- **"Agent is no longer running"** — no agent claimed the pane within the retry window, so the daemon no longer has one. Expected if you stopped it deliberately or the daemon restarted underneath the pane.

If a pane disconnects and neither cause fits — the agent looks healthy, or it keeps happening — that is worth reporting. Re-run with logging on and attach the excerpt:

```bash
DOT_AGENT_DECK_LOG=1 dot-agent-deck
```

Search the log for `giving up on this pane`. The `reason` field on that line (`empty-sessions` or `no-live-agent`) identifies which path was taken, and the surrounding lines show the reconnect attempts that preceded it — that is the detail needed to tell a genuine bug from an agent that simply died.

## An agent on a remote says an image or file "does not exist"

You are connected to a [remote environment](remote-environments.md), you drag a screenshot onto your terminal window (or paste one with `Ctrl+V` / `Cmd+V`), and the agent replies that the file is not there.

Nothing is broken. The agent runs on the **remote**, but your terminal — and the file — are on your **laptop**. Dragging inserts a laptop path, which is meaningless from the remote's point of view; pasting reads the remote's clipboard, which has no screenshot on it. Plain `ssh remote` followed by `claude` fails the same way, and the same drag into a deck running locally works fine.

Copy the file to the remote first, then reference its remote path:

```bash
scp ~/Desktop/screenshot.png my-vm:/tmp/
```

See [Remote Environments › Getting files to the remote](remote-environments.md#getting-files-to-the-remote) for the full explanation and the ssh-config note.

## A remote will not connect, or an ssh tunnel to it is not working

Ask the deck instead of guessing:

```bash
dot-agent-deck remote doctor my-vm
```

It runs a fixed ordered list of read-only checks — ssh reachability and auth, whether the deck is installed on the remote, the forwards `ssh -G` actually resolved, the remote's own `AllowTcpForwarding` and `ClientAliveInterval` from `sshd -T`, and whether a configured forward is really bound — and prints each as PASS / WARN / FAIL / UNKNOWN with the directive and file to change. Three exit codes: `0` clear, `1` a check failed, `2` a check could not be determined — so an incomplete diagnosis never reads as all-clear, and is still distinguishable from a broken one. It writes nothing: not your ssh config, not the remote's `sshd_config`, not the registry, not the remote — and the ssh sessions it opens clear all forwardings, so it does not even create the tunnel it is inspecting.

It earns its keep on one case in particular. `AllowTcpForwarding no` on the remote and a port collision produce **byte-identical** errors from ssh, so no client-side message can separate them; the doctor reads the remote's own sshd policy and tells you which one you have. See [Reverse tunnels › Troubleshooting with `remote doctor`](remote-recipes.md#troubleshooting-with-remote-doctor) for annotated output of every case.

## The deck is missing cards — a role or session I know is running has no card

Check the deck's title row first. If it reads something like `dot-agent-deck — 7 session(s)  (↓2)`, nothing is wrong with the agents: the count is right, and the `(↓2)` says two cards are below the bottom of the window. `(↑2)` means two are above it, and both appear together when you are scrolled into the middle. Move the selection with `j` / `k` (or the arrow keys) to bring them into view, or give the terminal a few more rows and they all fit again.

The deck fits as many cards as it can before it resorts to this. It picks the number of card columns and the card size together, widening the grid to a second or third column when that is what it takes to show every card, so the marker only appears on a window genuinely too small for the cards at any layout — a very short terminal, or a very large number of sessions.

If there is **no** marker and a card you expect is still absent, the count in the title is the thing to read next: it is the number of sessions the deck actually knows about, and `dot-agent-deck daemon status` lists what the daemon has. A role present in your `.dot-agent-deck.toml` but missing from both is one that never started — check its `command` (see [a bare command fails to spawn](#a-bare-command-like-claude-opencode-pi-codex-or-devin-fails-to-spawn)).

## Enabling Debug Logs

When something goes wrong and the dashboard's status messages aren't enough to diagnose it, set the `DOT_AGENT_DECK_LOG` environment variable to capture tracing output to a file:

```bash
# Default — writes to /tmp/dot-agent-deck.log
DOT_AGENT_DECK_LOG=1 dot-agent-deck

# Custom path
DOT_AGENT_DECK_LOG=/tmp/my-debug.log dot-agent-deck
```

The log file captures session events, hook activity, mode-tab restoration, and any errors logged by the daemon. Attach the relevant excerpt when filing an issue. See [Configuration › Environment Variables](configuration.md#environment-variables) for the full list of variables.

### Turning the verbosity up

The log is written at `info` for the deck itself and `error` for its dependencies. `RUST_LOG` overrides that, using the standard [`tracing` filter syntax](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html):

```bash
# Everything the deck logs, at debug
RUST_LOG=dot_agent_deck=debug DOT_AGENT_DECK_LOG=1 dot-agent-deck

# Just one subsystem, to keep the file readable
RUST_LOG=dot_agent_deck::daemon=debug DOT_AGENT_DECK_LOG=1 dot-agent-deck
```

`RUST_LOG` on its own does nothing — it selects *what* is logged, while `DOT_AGENT_DECK_LOG` decides *whether* there is a log file at all, so the two go together. A directive naming the `dot_agent_deck` target replaces the built-in default; anything else (a bare level such as `RUST_LOG=debug`, or a different crate) is layered alongside it, so the deck stays at `info` unless you name it.
