# Troubleshooting

Each entry below starts from what you see, says what causes it, and gives the commands that fix it and a way to check that the fix worked. Most entries apply to both clients, the TUI and the [desktop app](desktop/index.md), because the problem lives in the daemon or in the agents they share; an entry that applies to one client says so under its heading.

If you are not sure where to start, turn on logging ([Enabling Debug Logs](#enabling-debug-logs)) and list what the daemon is managing:

```bash
dot-agent-deck daemon status
```

`daemon status` prints each managed agent's pane id, label, working directory, orchestration role, status and active tool, and its inbox: INBOX counts the messages waiting for the agent and those it has taken but not yet acknowledged (`2q 1f`, or `-` when there are none), and WAKE says how a new message reaches it — `native` when the agent's own hooks, plugin or extension take it, `turn-end` when the agent takes it at the end of its turn, `doorbell` when the deck types a one-line notice into the pane once the agent is idle, and `none` when there is no native wake or reported idle moment for a doorbell. It only reads: it never starts a daemon, and it reports a missing or unreachable daemon instead of starting one. Add `--json` for machine-readable output.

## Logs and diagnostics

### Enabling Debug Logs

Set `DOT_AGENT_DECK_LOG` to write the deck's log to a file:

```bash
# Default location
DOT_AGENT_DECK_LOG=1 dot-agent-deck

# A path of your choosing
DOT_AGENT_DECK_LOG=/tmp/my-debug.log dot-agent-deck
```

| Value of `DOT_AGENT_DECK_LOG` | Where the log goes |
| --- | --- |
| unset | no log file |
| `1` or empty | `/tmp/dot-agent-deck.log` on macOS and Linux; `dot-agent-deck.log` in the system temp directory (the one `%TMP%`/`%TEMP%` names) on Windows |
| anything else | that path, used as given |

The file is appended to, never truncated. If it cannot be opened, the command prints `Warning: failed to open log file <path>: <error>` on stderr and runs without logging; the deck creates the file but not its parent directory, so point the variable into a directory that exists.

**The variable applies to the process it is set on, and the daemon is a separate process.** When `dot-agent-deck` has to start the daemon, the daemon inherits the variable and logs to the same file. When a daemon is already running, it keeps the logging it was started with, so `DOT_AGENT_DECK_LOG=1 dot-agent-deck` then logs only the TUI. To capture the daemon's side (hook events, spawning agents, delegation, orchestration roles), restart the daemon with the variable set:

```bash
dot-agent-deck daemon stop
DOT_AGENT_DECK_LOG=1 dot-agent-deck
```

`daemon stop` refuses while managed agents or orchestration roles are live; see [Recycling the daemon](#recycling-the-daemon) before you add `--force`.

Check that it worked: after the deck starts, `grep 'login-shell PATH' /tmp/dot-agent-deck.log` finds the line the daemon writes at startup (`applied login-shell PATH to the daemon environment`, or `no login-shell PATH captured`). If that line is missing, the daemon was started without the variable.

A daemon the deck starts in the background also writes its standard output and error to `daemon.log` in the deck's state directory: `$XDG_STATE_HOME/dot-agent-deck/daemon.log` when `XDG_STATE_HOME` is set, otherwise `~/.local/state/dot-agent-deck/daemon.log`, and `%LOCALAPPDATA%\dot-agent-deck\daemon.log` on Windows (`DOT_AGENT_DECK_STATE_DIR` replaces the directory). It is not the debug log: it holds what the daemon prints rather than logs, such as a crash message or the `[scheduler]` notices [schedules](scheduled-tasks.md) print as they run (an issue dispatched or skipped, a run that failed, a configuration error). The log described above is the more useful of the two; without schedules, `daemon.log` is often empty. Attach the relevant excerpts of both when you file an issue. [Configuration](configuration.md) lists the other environment variables.

#### With the desktop app

*Applies to the desktop app.*

The desktop app writes no log file of its own. The log to collect is the **daemon's**, and the daemon has to be started with `DOT_AGENT_DECK_LOG` set. Restarting only the app does not turn the log on, because the app does not restart the daemon. The desktop app is built for macOS (Apple Silicon) and Linux (amd64); there is no Windows build.

Start the daemon yourself with logging on, rather than with the app's **Start daemon** (see [How the desktop app gets a daemon](installation.md#how-the-desktop-app-gets-a-daemon)):

1. If a daemon is already running, stop it with `dot-agent-deck daemon stop`. It refuses while agents are running, so finish or close them first; see [Recycling the daemon](#recycling-the-daemon).
2. Start a daemon with the variable set, in a terminal:

   ```bash
   DOT_AGENT_DECK_LOG=1 dot-agent-deck daemon serve
   ```

   On macOS with no CLI installed, use the app's own copy:

   ```bash
   DOT_AGENT_DECK_LOG=1 "/Applications/Agent Deck.app/Contents/MacOS/dot-agent-deck" daemon serve
   ```

   Starting the TUI instead works too: `DOT_AGENT_DECK_LOG=1 dot-agent-deck`.
3. Press **Reconnect** in the app. A `daemon serve` that nothing connects to exits after about 30 seconds, so reconnect promptly.

The log lands at `/tmp/dot-agent-deck.log` on both macOS and Linux, or at the path you gave the variable, and the `login-shell PATH` line described above confirms that the daemon logs. `RUST_LOG` ([below](#turning-the-verbosity-up)) goes on the same command, for example `RUST_LOG=dot_agent_deck=debug DOT_AGENT_DECK_LOG=1 dot-agent-deck daemon serve`. For a [remote daemon](desktop/daemons.md), start it on its host the same way; the log is then on that host.

With the `experimental` flag on, the app can start a daemon itself, with **Start daemon** or **Replace daemon**. That daemon gets the variables only when the app itself was launched with them. On Linux, that means starting the app from a terminal, for example `DOT_AGENT_DECK_LOG=1 dot-agent-deck-desktop`. An app opened from the macOS Dock or Finder, or from a Linux application menu, does not get variables exported in your shell profile, so a daemon it starts has no log. In that case, start the daemon yourself as above.

#### Turning the verbosity up

The log records the deck itself at `info` and its dependencies at `error`. `RUST_LOG` changes that, using the [`tracing` filter syntax](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html):

```bash
# Everything the deck logs, at debug
RUST_LOG=dot_agent_deck=debug DOT_AGENT_DECK_LOG=1 dot-agent-deck

# One subsystem, to keep the file readable
RUST_LOG=dot_agent_deck::daemon=debug DOT_AGENT_DECK_LOG=1 dot-agent-deck
```

`RUST_LOG` does nothing on its own: it selects what is logged, and `DOT_AGENT_DECK_LOG` decides whether there is a log file, so set both. Your `RUST_LOG` directives are applied after the defaults, so `dot_agent_deck=debug` replaces the `info` default; a directive for anything else is added beside the defaults, which means a bare `RUST_LOG=debug` raises the dependencies but leaves the deck at `info` unless you name `dot_agent_deck`. As with `DOT_AGENT_DECK_LOG`, the daemon has to be started with `RUST_LOG` set for its side to change.

### "daemon failed to start within …ms"

`dot-agent-deck` exits with:

```text
daemon failed to start within 15000ms: endpoint <path> never became available. For daemon stderr see <state dir>/daemon.log — but note it stays empty unless the daemon was started with DOT_AGENT_DECK_LOG set, so an empty log is not evidence the daemon never ran.
```

The deck started a daemon in the background and waited for it to open its socket. The most common cause is slow shell startup files: before it opens its socket, the daemon runs your login shell (`$SHELL -ilc`) once to learn your `PATH`, and waits up to 10 seconds for it. The daemon may finish starting after the deck has given up.

1. Run `dot-agent-deck` again. If a daemon came up late, the second run attaches to it.
2. Time your shell: `time $SHELL -ilc true`. If it takes several seconds, trim what your startup files run in non-interactive contexts.
3. If it still fails, run the daemon in the foreground to see its errors directly: `DOT_AGENT_DECK_LOG=1 dot-agent-deck daemon serve`. Stop it with `Ctrl+C` when you are done.

### "refusing to connect to daemon attach socket"

The deck found a file at its socket path that it does not trust (wrong owner, type or permissions), and refuses to connect to it or remove it. The message names the path. Move the deck to a socket path only you can write by setting `DOT_AGENT_DECK_ATTACH_SOCKET`, for example under `$XDG_RUNTIME_DIR` or your home directory, then run `dot-agent-deck` again. Set the same value for every process that should reach that daemon.

### "refusing the daemon connection at …: the process listening there runs as uid …"

The deck connected to its socket and found that the process listening there belongs to another user, so it closed the connection without talking to it. Another user on the machine may have bound that path first. Set `DOT_AGENT_DECK_SOCKET` and `DOT_AGENT_DECK_ATTACH_SOCKET` to paths inside a directory only you can write (for example `mkdir -m 700 ~/.dad-sock` and put both sockets there), export them in every shell that starts the TUI, the desktop app or the daemon, and run `dot-agent-deck` again. Check with `dot-agent-deck daemon status`, which should report your daemon.

## Hooks

Hooks are how an agent tells the deck what it is doing: prompts, tool use, waiting for you, finishing. Without them a card still appears, but its status is coarse and it shows no tool or prompt detail. The deck installs them for you: when the TUI starts and when the daemon starts, it installs hooks for every supported agent it detects. The daemon half is what covers the desktop app, which never runs the TUI.

| Agent | Detected when | What the deck writes |
| --- | --- | --- |
| Claude Code | `~/.claude/` exists | hook entries in `~/.claude/settings.json` for `SessionStart`, `SessionEnd`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Notification`, `Stop`, `PreCompact`, `SubagentStart` and `SubagentStop`, plus `StopFailure` when the installed Claude Code is 2.1.78 or newer |
| OpenCode | `$XDG_CONFIG_HOME/opencode/` (default `~/.config/opencode/`) or `~/.opencode/` exists | a plugin file, `plugin/dot-agent-deck.js`, in each of those directories that exists |
| Codex | `codex` is on the `PATH` | `hooks.json` in the Codex home (`$CODEX_HOME`, else `~/.codex`), and trust records for exactly those hooks in that home's `config.toml` |
| Devin | `devin` is on the `PATH` | a `"hooks"` object in `$XDG_CONFIG_HOME/devin/config.json` (when `XDG_CONFIG_HOME` is an absolute path), else `~/.config/devin/config.json` |
| Pi | `pi` is on the `PATH` | no hooks; the daemon writes the deck's Pi extension to `$PI_CODING_AGENT_DIR/extensions/dot-agent-deck` (default `~/.pi/agent/extensions/dot-agent-deck`) when it starts, and again, into the directory that agent's own `HOME` and `PI_CODING_AGENT_DIR` name, each time it starts an agent it identifies as Pi. `dot-agent-deck orchestrator setup` does the same on demand |

Each hook runs `<path to dot-agent-deck> hook --agent <agent>`. On macOS and Linux, an agent started with `DOT_AGENT_DECK_BIN` set to an absolute path reports through the binary that variable names, and every other agent runs the installed binary (see [Environment variables](configuration.md#paths-and-endpoints)); a value that is not an absolute path is ignored. The OpenCode plugin and the Pi extension honour the same variable. On Windows, Claude Code and Codex hooks always run the installed binary. Hooks an earlier release wrote are refreshed automatically the next time the deck installs hooks.

On macOS and Linux, a `dot-agent-deck` installed at a path that contains a backslash (`\`) gets no Claude Code, Codex or Devin hooks: the install fails with an error naming the path and leaves the agent's configuration as it was. Install `dot-agent-deck` at a path without one, then reinstall the hooks ([Manual Management](#manual-management)). The OpenCode plugin and the Pi extension are installed as usual at such a path. If another `dot-agent-deck` copy left a hook entry that is not safe to run, the next install fixes it: an entry for a path with a backslash is replaced with a safe entry, or removed from a Codex or Devin hook event the deck no longer installs, and an entry whose path contains characters such as `;` or `$(` is rewritten so they are not run as commands.

The deck changes only entries it recognises as its own; your other settings and your own hooks are kept, including a hook of yours that shares a rule with a deck entry. For Claude Code, an install also removes the deck's entries from hook types it no longer installs, such as `StopFailure` after Claude Code is downgraded below 2.1.78. The startup install is silent: a problem is written to the log (see [Enabling Debug Logs](#enabling-debug-logs)) and does not stop the deck. Run the install by hand ([Manual Management](#manual-management)) to see errors on your terminal.

On Windows, `$HOME` is usually unset, so Codex hooks are installed only when `CODEX_HOME` is set.

The install runs when the daemon **starts**. After you install an agent, or upgrade the desktop app from a version that did not install hooks, a daemon already running has not installed them; restart it ([Recycling the daemon](#recycling-the-daemon)) or run [the manual install](#manual-management).

### Checking that hooks are installed

Each command prints the binary every deck hook names, followed by its `hook --agent` arguments (the `DOT_AGENT_DECK_BIN` wrapper in front of it is left out). Every line should name an installed `dot-agent-deck` binary (for example `~/.local/bin/dot-agent-deck` or the one `command -v dot-agent-deck` prints), not a `target/debug` or `target/release` directory and not a file that no longer exists:

```bash
# Claude Code
grep -oE "('[^']*'|[^\"' ]+) hook --agent claude-code" ~/.claude/settings.json | sort -u

# Codex
grep -oE "('[^']*'|[^\"' ]+) hook --agent codex" "${CODEX_HOME:-$HOME/.codex}/hooks.json" | sort -u

# Devin
grep -oE "('[^']*'|[^\"' ]+) hook --agent devin" "${XDG_CONFIG_HOME:-$HOME/.config}/devin/config.json" | sort -u

# OpenCode (prints the pinned binary from each plugin file that exists)
grep -h 'const BINARY_PATH' "${XDG_CONFIG_HOME:-$HOME/.config}/opencode/plugin/dot-agent-deck.js" ~/.opencode/plugin/dot-agent-deck.js 2>/dev/null
```

No output means no deck hooks are installed for that agent. Then start an agent in the deck and check that its card moves past a coarse status and shows its prompt and tools.

### A newly installed agent never changes status

The agent works, but its card stays on a coarse status with no prompt or tool detail. The automatic install at startup writes hooks only for an agent it detects (see the table above): Claude Code and OpenCode by their config directory, Codex and Devin by their command on the daemon's `PATH`. An agent installed after the daemon started, or whose config directory does not exist yet, has no hooks.

1. Install them explicitly: `dot-agent-deck hooks install --agent <claude-code|opencode|codex|devin>`.
2. Restart the agent (close its pane and start it again), because an agent reads its hook configuration when it starts.
3. Check with [Checking that hooks are installed](#checking-that-hooks-are-installed).

### A hook fails with `not found` and names a path you never typed

An agent reports something like:

```text
Stop hook error: /bin/sh: 1: /home/you/code/dot-agent-deck-pr-356/target/release/dot-agent-deck: not found
```

The hook command in that agent's config names a deck binary that is no longer there. A build directory is the usual cause: `cargo clean` removes it, and deleting a git worktree removes it with the worktree.

The deck writes an **installed** binary into hooks when it can find one: `~/.local/bin/dot-agent-deck`, or a `dot-agent-deck` in a directory on your `PATH`. With no installed binary, it refuses to write a build-directory path at all (`hooks install` fails and says what to install), and pins any other path as a last resort, with a warning in the log. The exception is the desktop app on macOS opened from its disk image, or from the temporary location macOS runs an app from before it has been moved: that path stops working once the image is ejected or the app is reopened, so no hooks are installed and both clients say `Move Agent Deck to /Applications and reopen it to turn agent hooks on.`

To fix it:

1. **Start the deck once** (`dot-agent-deck`, or restart the daemon). On startup the deck rewrites a deck hook whose binary is missing so that it names the binary it resolves.
2. **Or reinstall explicitly**: `dot-agent-deck hooks install`, adding `--agent codex`, `--agent opencode` or `--agent devin` for the others.
3. **If the install refuses**, there is no installed deck to point at. Install one (see [Installation](installation.md)) and run the install again.

Check with the commands in [Checking that hooks are installed](#checking-that-hooks-are-installed).

A hook command whose binary still **exists** keeps calling it, even when it names a different copy from the one the deck would write, until an installed copy of a newer release starts and switches the hooks to itself. There is still one deck entry per event either way. To switch now, run `dot-agent-deck hooks install` from the copy you want, or delete the copy you no longer want and start the deck once (its entry is then repaired, because its binary is missing).

### The deck says an agent's hooks run an older dot-agent-deck

The TUI shows a line at the bottom of the dashboard, or the desktop app a strip under a daemon's header, such as:

```text
⚠ Claude Code, Codex hooks run dot-agent-deck 0.45.1 (/opt/homebrew/bin/dot-agent-deck); this deck is 0.46.0 — Run: brew upgrade dot-agent-deck
```

Those agents' hooks call an older copy of the deck than the one you are running, so whatever that copy does not know how to send is missing: a status, a reply or a signal. With a copy old enough, those agents' cards stop updating altogether. It usually happens when two copies are installed and only one was upgraded, for example a Homebrew CLI next to the desktop app or next to a copy in `~/.local/bin`.

1. **Do what the notice says.** `Run: brew upgrade dot-agent-deck` upgrades the Homebrew copy the hooks call. `Run: <path> hooks install --agent <agent>` points the hooks at the deck you are running (on Windows it is a PowerShell command, `& '<path>' hooks install --agent <agent>`). `Upgrade the dot-agent-deck the hooks run, …` means the deck you are running cannot take the hooks over and has no command to offer: upgrade or replace the copy the notice names in parentheses, or run `hooks install` from the copy you want. In the desktop app, **Copy** copies the command shown, and appears only when the fix is a command.
2. **Restart the agents** whose hooks changed, because an agent reads its hook configuration when it starts.
3. **Check** with [Checking that hooks are installed](#checking-that-hooks-are-installed): every line should name the copy you meant.

Once the hooks no longer call the old copy, the notice goes away within about a minute without restarting anything: the deck reads the agents' hook settings again every minute, and it also clears a notice as soon as the copy it names sends an event as a current release. A notice about a copy the hooks are still set up to call stays, even while that copy sends nothing. A copy the agent's own hook settings do not name, for example one a project's settings call, is known only from its events, so its notice goes away with one of the agent's events once that copy has sent nothing for at least five minutes. If an agent's hook settings call the deck by name alone, such as `dot-agent-deck` with no path, the deck cannot tell which copy that is until the agent sends an event: a copy too old to report its version then raises a notice naming the command as written, and it goes away once you run `hooks install` from the copy you want, which writes the full path. Restarting the daemon does not clear a notice on its own: the daemon checks the copies the hooks are set up to call again, and the notice stays while the copy it names is still older. In the TUI, a fix is never cut off: a command too wide for the terminal is replaced by the upgrade-or-reinstall advice, and a terminal too narrow for either shows the notice without a fix; widen the terminal, or use the desktop app, to see it. "Predates version reporting" means the copy is too old to say its version, and "did not report its version" that it did not answer in time when the deck asked; treat both as an older copy. Pi is never named, because its extension always runs the copy that started its pane.

### An agent's config file cannot be edited

`dot-agent-deck hooks install` or `hooks uninstall` fails with one of:

- `<path> is not valid JSON (left unchanged, original preserved at <path>.bak): …` — the config (for example `~/.claude/settings.json`) does not parse; one trailing comma is enough. The deck leaves the file as it is and copies it to `<name>.bak` beside it. If a `<name>.bak` already exists, for example a copy you made before editing, the deck leaves it alone and the message says `original not copied: <path>.bak already exists and was left as it was` instead. Fix the syntax and run the install again; the command exits with a non-zero status until it succeeds. Devin documents its config as JSON with comments; the deck cannot edit a Devin config that contains comments, so remove them or add the hooks by hand.
- `<path> is a symlink (left unchanged): …` — the config is a symbolic link, as in a dotfiles setup. The deck neither replaces the link nor writes through it. Point it at a regular file, or add the deck's hooks to the linked file yourself.

At startup the same problems are logged instead of printed, and the hooks are not installed.

### A message says not delivered

The TUI card shows **Error** and the desktop tile shows **error**, with a `not delivered: …` line naming the problem. Open the pane first: the agent may have exited, or its integration may have repeatedly taken messages without acknowledging them. Run `dot-agent-deck daemon status` to inspect **INBOX** and **WAKE**. Inside the recipient's deck pane, ask the agent to run `dot-agent-deck inbox list` to inspect recent delivery states; `inbox fetch` reads and acknowledges waiting messages. `not delivered` means the deck stopped trying, not that the agent never got the message: one the deck had already handed over, for example before it ran out of attempts, can still have reached the agent. Check the pane before posting it again; if the agent takes it after all, the card or tile may leave its error state and an orchestrator may be told the task arrived, so go by the pane rather than by whether either happens.

Refresh the integration with `dot-agent-deck hooks install` on the machine running the agent, then restart the agent. Check that its command starts successfully. If a delegated task belonged to the exited agent, ask the orchestrator to delegate it again to the replacement. The desktop overview counts these agents under **FAILED**, while each tile uses **error**.

### A message says delivery uncertain

The deck handed a message to the agent, then lost track of whether the agent got it or started on it, and either two marked repeats did not settle it or for a few minutes it could neither check nor hand the message to the agent again (for example because the agent's deck integration stopped). It applies to Claude Code, OpenCode and Pi agents. Open the pane and look: if the agent is working on it, Cancel the delivery; if it never saw it, Retry. In the TUI press `i` on the card and then `r` or `c`; in the desktop app use the tile's **Retry** or **Cancel**. If a message is out of attempts, only Cancel is offered: post it again if it is still needed. If `r` answers `make the window taller to see what Retry does` or `make the window wider to see what Retry does`, the TUI's window is too small to show what Retry would do: make it larger and press `r` again.

### A card says a message may not have reached the agent

`✉ may not have reached the agent` appears only for Pi. The deck handed Pi a message, and Pi has been idle for a few minutes since without the deck seeing it start on it. Pi usually took the message and could not start a turn on it: look in its pane for an error, such as a model provider it has no credentials for, and fix that first. Then, in the TUI press `i` on the card, or in the desktop app use the tile's buttons:

- **Retry** (`r`) if the message is not in Pi's pane, or Pi stopped before working on it. The deck sends it again, marked as a possible repeat, so Pi may see it twice.
- **Cancel** (`c`) if you no longer want it delivered. It cannot take back a copy Pi already has.

The notice goes away by itself as soon as Pi starts working, for example after you send it a prompt; if the message is still not picked up after that turn ends, it can come back a few minutes later. The deck does not send the message again until you choose Retry. [Delivery in the TUI and desktop app](orchestration.md#delivery-in-the-tui-and-desktop-app) has the details.

### A card says the deck integration is out of date, too old, or not connected

The notice under the card's directory says which:

- `this agent's deck integration is out of date; restart the agent to update it` — the agent loaded an older copy of the deck's OpenCode plugin or Pi extension. Restart the agent; the deck rewrites its integration from the running deck before the agent starts. For OpenCode, when the plugin names another installed copy of `dot-agent-deck`, the deck usually keeps that copy and updates the plugin around it; a newer installed copy of the deck can replace it instead. For Pi, the deck updates the extension only when it can tell the agent is Pi: if the notice comes back after a restart, the role's command probably starts Pi through a launcher, so declare `agent = "pi"` on the role ([Declaring the agent behind a launcher command](orchestration.md#declaring-the-agent-behind-a-launcher-command)), or run `dot-agent-deck orchestrator setup` with the same `HOME` and `PI_CODING_AGENT_DIR` as the agent, then restart it.
- `the deck could not update this agent's deck integration: run dot-agent-deck hooks install --agent opencode to see why, then restart the agent` — the deck could not write the agent's plugin files when it started, so restarting again will not help by itself. The install command prints the file it cannot write; fix its permissions, then restart the agent.
- `the deck could not update this agent's Pi extension: run dot-agent-deck orchestrator setup with the same HOME and PI_CODING_AGENT_DIR as the agent to see why, then restart the agent` — the same for Pi: the deck could not write the extension into the directory that Pi loads it from. Run `dot-agent-deck orchestrator setup` in a shell where `HOME` and `PI_CODING_AGENT_DIR` (if the agent sets it) have the values the agent's role gives them, so it writes the same directory and prints what fails, usually a directory you cannot write to or an extension file that is a symbolic link, which the deck does not write through. Fix that, then restart the agent. The deck's log also names the directory.
- `the dot-agent-deck command this agent runs does not support this deck's integration: upgrade dot-agent-deck, then restart the agent` — the agent's integration runs an older `dot-agent-deck` than the deck. Upgrade it (or point `DOT_AGENT_DECK_BIN` at the current one), then restart the agent.
- `this agent's deck hooks run a dot-agent-deck too old for its messages: run <command>, then restart the agent` — a Claude Code agent's hooks call a `dot-agent-deck` older than the deck, which cannot take deck messages the way this deck hands them over. The command is usually `dot-agent-deck hooks install --agent claude-code`, with the deck's own path. Run it, then restart the agent: the command rewrites all of the agent's deck hooks together, while restarting the agent alone does not change them. Until then the agent's messages wait and no doorbell is typed. When the deck cannot name a command, the notice says `upgrade the dot-agent-deck the hooks run, or run hooks install from the copy you want them to use, then restart the agent` instead. The deck shows this notice once it knows the agent is Claude Code; a Claude Code agent started from a plain command whose hooks have not yet reported it as Claude Code gets its messages the older way, with no notice. Hooks from a release before 0.47 never ask for the agent's messages at all. When their events carry the deck's hook capability token, the card shows the same notice as soon as one of them reaches the deck, usually when the agent starts; with those hooks the doorbell is still typed, and the agent gets its messages by running the `dot-agent-deck inbox fetch` it names. Hooks older still, which carry no token, get no notice, because the deck cannot confirm which agent their events are from ([`work-done`, `dispatch` or `delegate` fails with "refused: … hook capability token"](#work-done-dispatch-or-delegate-fails-with-refused--hook-capability-token) says what they show instead); run `dot-agent-deck hooks refresh`, then restart the agent. After you upgrade the CLI from the TUI, `dot-agent-deck upgrade` or the desktop app, the upgrade's result says whether it moved the hooks to the new copy ([Installation → Your agents' hooks after an upgrade](installation.md#your-agents-hooks-after-an-upgrade)). A Codex agent whose hooks come from before 0.47 gets the same notice, under the same condition, with `--agent codex` in the command. Claude Code's and Codex's are the only cards that name hooks from before 0.47: for Devin, the same upgrade or `dot-agent-deck hooks refresh` updates its hooks, and the deck rewrites OpenCode's plugin and Pi's extension each time such an agent starts; in every case, restart an agent that was already running so it loads them.
- `this agent's deck integration has not connected yet; its messages wait until it does` — the agent is still starting, or its integration did not load. It clears by itself once the integration connects, and the waiting messages are then delivered; if it stays, restart the agent, or run `dot-agent-deck hooks install --agent opencode` for OpenCode or `dot-agent-deck orchestrator setup` for Pi.

While a notice stands, the agent's messages wait, except under the notice for hooks from before 0.47 above: there the doorbell is still typed and the agent fetches its messages itself. When the integration connects without a restart, as it does for an agent that was still starting, they are then delivered. Restarting the agent is different: messages already waiting for it, uncertain ones included, are not handed to the restarted agent. They show as not delivered, an orchestrator is told about each of its tasks so it can delegate them again, and a job offered to the agent goes back to its pool. Post any other message again if it is still needed. [Delivery in the TUI and desktop app](orchestration.md#delivery-in-the-tui-and-desktop-app) has the details.

### A card says messages may repeat or deck messages are paused

`messages may repeat: the deck cannot record message receipts for this agent (…)` means the deck could not write its receipts, usually because its state directory is full or not writable; the reason follows. Messages are still delivered, but one may be sent twice. Free the space or fix the permissions, then restart the agent. When the reason says `the receipt spool could not start`, the problem is the deck's, not this agent's: fix the state directory, then restart the deck itself, since restarting the agent does not start the spool.

`deck messages paused: N delivered messages still wait for proof that older copies cannot run; restart the agent to clear them` means the deck stopped handing this agent new messages until it can account for ones it already delivered. Restarting the agent clears it, but the messages waiting for it are not handed to the restarted agent: they show as not delivered, and an orchestrator is told about each of its tasks so it can delegate them again. Post any other message again if it is still needed. A long-running OpenCode session can also build up this pause after cancelled messages; restart the agent, then post or delegate any waiting work again.

### Claude Code shows an error from the deck's hook

The deck's hooks in Claude Code take deck messages for it and confirm them to the deck. When one of them cannot, Claude Code shows its error, which starts with `dot-agent-deck:`, and the hook takes no new message. Waiting messages stay with the deck. The error says what to do:

- `the deck daemon did not answer in time whether it serves message receipts, so this hook took no message and left the messages waiting for this agent queued` — the deck is busy, or it runs an older `dot-agent-deck` than the hooks, which does not answer at all. If it happens once, nothing is needed: the deck hands the messages over at a later wake-up. If it keeps happening, make the deck and the hooks the same release: restart the deck daemon on the hooks' release, or run `dot-agent-deck hooks install --agent claude-code` with the release the daemon runs, then restart the agent.
- `the deck daemon did not answer in time …, so this hook did not confirm to it that the message in this prompt was received, and took no further message` — the same, for a message Claude Code already has: it is in the prompt, but the deck was not told. The deck can send it again later, marked as a possible repeat. The remedy is the same.
- `the deck daemon did not confirm that it serves message receipts, so no message was taken` — the deck daemon answered, and is most likely older than the hooks. Restart the deck daemon on the newer release, or run `dot-agent-deck hooks install --agent claude-code` with the release the daemon runs, then restart the agent.
- `the deck daemon did not confirm that it serves message receipts, so this hook did not confirm to it that the message in this prompt was received` — the same, for a message Claude Code already has: it is in the prompt, but the deck was not told. The remedy is the same.
- `the connection to the deck daemon was lost before it answered whether it serves message receipts` — the deck daemon may have stopped or restarted. If the error goes on to name the message in this prompt, Claude Code has that message, but the deck was not told. Check that the deck daemon is running with `dot-agent-deck daemon status`. If this keeps happening while it runs, restart the agent.
- `the deck socket refused this hook access` — another user's deck listens on the socket, or the permissions of the socket or a directory above it keep you out. If you set `DOT_AGENT_DECK_SOCKET` or `DOT_AGENT_DECK_ATTACH_SOCKET`, point them at your own deck's sockets, or unset them; otherwise fix the permissions of the socket and the directories above it. Then restart the agent.

`hooks install` rewrites all of Claude Code's deck hooks together; restarting the agent alone does not change them. While the hooks and the deck disagree, `dot-agent-deck inbox fetch`, run in the agent's pane, can still read a waiting message that the deck is handing over the older way; some messages need the updated hooks instead.

### An agent finishes an interrupted request before a delegated task

If you interrupt an agent and a delegated task arrives soon after, the agent may finish the request you interrupted first. When the agent's card or tile shows `✉ delivered`, or you can see the task in the agent's pane, the deck has delivered it and the order is the agent's choice; if the agent ends its turn without doing the task, ask it to, or delegate the task again. If the badge still shows `✉N queued` or `✉N waiting to wake: send it a prompt`, the task has not reached the agent yet: see [Delivery in the TUI and desktop app](orchestration.md#delivery-in-the-tui-and-desktop-app) and [An agent shows the doorbell instead of waking](#an-agent-shows-the-doorbell-instead-of-waking).

### An agent shows the doorbell instead of waking

A line such as `dot-agent-deck: message m-… waiting, run: dot-agent-deck inbox fetch` means a message is waiting and native wake is unavailable. It is expected for Codex while idle, Claude Code below 2.1.296, and Devin. Both clients use the same delivery paths. [Which agents wake for messages](orchestration.md#which-agents-wake-for-messages) lists the supported paths and the OpenCode configuration remedy.

Run `dot-agent-deck hooks install` after upgrading, then restart the agent; for OpenCode you can use `dot-agent-deck hooks install --agent opencode`. Run those commands on the remote host when the agent runs there. If installation says it cannot edit OpenCode's `tui.json`, add the exact path it prints to that file's `plugin` list, preserve the other entries and restart OpenCode.

The doorbell waits while the agent is busy, a permission or dialog answer is needed, or you have an unsent draft, and after an agent starts until it shows it is ready: Claude Code once its input box is on screen after it starts, or once it ends a turn; Codex once its empty input box is on screen; other agents once they report that they have started. Send or clear the draft and answer the prompt. When messages have waited a couple of minutes while the agent is not working, the TUI card and the desktop tile both say `waiting to wake: send it a prompt`: send the agent any prompt and it takes its messages at that turn. Codex also waits for a recognised empty input box; if a future Codex UI is not recognised, send the first prompt yourself so messages can arrive at turn end. Claude Code is the same at start-up: if the deck does not recognise its input box, for example after a Claude Code update changes how the box looks, the deck sends no first doorbell and the message waits, even though you can see the box. Type the agent's first prompt yourself, or press Enter in its empty input box, in the TUI or the desktop app; the agent takes its messages at that turn, and the doorbell works from then on. If Claude Code's own wake-up is working, its messages arrive without the doorbell anyway. With untrusted Codex hooks, the model must run `inbox fetch` and its sandbox needs `network_access = true`, as described below.

### An inbox request is refused

An agent's hooks, plugin, extension and tool shell take its deck messages by running `dot-agent-deck` with the deck environment of the agent's pane. On Linux and macOS the deck accepts such a request from any process that carries that pane's current deck environment and runs as the same user as the deck, including a process the agent started outside its pane, such as Claude Code's background session.

When the deck refuses one of the agent's own requests while messages wait for the agent, the TUI card and the desktop tile both say `Messages can't reach this agent`, the cause, and `Restart it from the deck; messages addressed to it are not handed to the restarted agent, so have them sent again.` In the TUI, press `i` on the card to read the whole notice if the card cuts it off; if the window is too short for all of it, the notice says to make the window taller, unless the window is too small even for that line. The command that was refused prints the cause and what to do, with a code:

- `inbox_other_user`: the request came from a process outside the agent, running as another user. Run it as the user that runs the deck.
- `inbox_peer_unidentified`: the deck could not identify the process that made the request, for example because it exited before the deck answered, or could not tell where the process that would receive the agent's messages sits relative to the agent, for example because a process between them exited while the deck looked, or the agent runs nested under a very long chain of launchers. Run the command again from the agent's pane and let it finish. If it is refused every time, start the agent more directly, with fewer launchers between the deck's pane and the agent.
- `inbox_component_not_caller`: the agent's integration named a process other than its own as the one that receives messages. Run the inbox command from the agent's own integration.
- `inbox_receiver_above_agent`: the request named, directly or through a shell, a process the agent itself runs under, such as its terminal or the deck, as the one that receives its messages. This happens when a command carrying the agent's deck environment runs from a shell outside the agent, such as another tab of the same terminal. Run the inbox command from the agent's own integration.
- `inbox_nested_agent`: an agent of the same kind running inside the agent asked for its messages. Run the inbox command from the pane's own agent, not from an agent it started.
- `inbox_not_agent_process`: the request came from a process outside the agent, on a system where the deck cannot check which user runs it. Run it from the agent's own pane.

If the notice stays after the cause is fixed, restart the agent from the deck, in the TUI or the desktop app. Messages already addressed to it are not handed to the restarted agent: an orchestrator is told about each of its tasks among them, and others have to be sent again.

Some refusals do not mark any card, because the deck cannot tell which agent the request is from: a request carrying another pane's deck environment (`token_names_another_pane`), none at all (`missing_token`), or that of an agent the deck has since restarted or a daemon that has since been replaced (`unknown_token`). The command prints the refusal; run it from the agent's current pane, or restart an agent that outlived the daemon that started it.

### Codex events not showing

Codex only runs hooks it trusts. The deck records trust for its own hook entries, and only those, in the Codex home's `config.toml`, at startup and whenever it starts a Codex pane. This does not depend on how you launch Codex, so a launcher (`devbox run codex-big`, a script, an alias) needs nothing added.

If a Codex card never shows a tool, a prompt or **Needs Input**, its hooks are not running. You see one of two things. Usually the card shows **Thinking** while Codex's screen is changing and **Idle** once it has been still for a few seconds (in the desktop app, **running** and then **idle**; [Which agents report which status](session-management.md#which-agents-report-which-status)). If instead it stays **Idle** even while Codex works, start with step 2. Check in this order:

1. **Is `codex` on the daemon's `PATH`?** The install is skipped when it is not. `$SHELL -ilc 'command -v codex'` should print a path; if you installed Codex after the daemon started, restart the daemon ([Recycling the daemon](#recycling-the-daemon)).
2. **Does your launcher change `CODEX_HOME`?** The deck sets `CODEX_HOME` on the Codex process it starts, to the home it installed into. A script that re-exports `CODEX_HOME` before running `codex` points Codex at a home without the deck's hooks. The deck cannot tell that this happened, so the card stays **Idle** even while Codex works. Remove the re-export, or make it the same home (`$CODEX_HOME`, else `~/.codex`).
3. **Run the install by hand** to see errors the startup install only logs: `dot-agent-deck hooks install --agent codex`. Then check with [the Codex listing command](#checking-that-hooks-are-installed).
4. **Approve the hooks in Codex** as a fallback: run Codex once and approve the deck's hooks in its `/hooks` review. Codex remembers that trust.

Trust is tied to each hook's exact content. If a hook definition changes after trust was recorded, Codex refuses to run it and the card falls back to coarse status; running the install again records trust for the new content.

Codex keeps those trust records under `[hooks.state]` in its `config.toml`, one per hook position in `hooks.json`. When the deck's hook moves to a different position, the deck removes the record it left at the old one the next time it records trust. It never removes a record for one of your own hooks, for another Codex home's hooks, or for a position Codex still lists, and it removes nothing while Codex reports a warning or an error about your hook files.

**If you turn off one of the deck's hooks** in Codex's `/hooks` list, the deck leaves it off: it keeps recording trust for it, but Codex reports nothing through it, so the agent's card (TUI) or row (desktop) is missing that hook's detail. This is the same in the TUI and the desktop app. `dot-agent-deck hooks install --agent codex` names every deck hook that is turned off, for example:

```text
Trusted hooks: 10
Note: the deck's Codex hook for PreToolUse is turned off in Codex's /hooks list, so Codex reports nothing through it. The deck leaves it off; turn it back on in Codex's /hooks list to restore that detail.
```

The deck's log also records a warning naming those hooks each time the deck records trust. To get the detail back, turn the hook on again in Codex's `/hooks` list.

While Codex's hooks are not trusted, or if you turn off the deck's `UserPromptSubmit` hook in Codex's `/hooks` list, a doorbell asks Codex to run `dot-agent-deck inbox fetch` itself. That command acknowledges receipt and needs sandbox network access. Trust the hooks to restore automatic message hand-over, including at turn end.

### Codex as a role or worker: allow sandbox network access

A Codex agent used as an orchestration role or a worker runs `dot-agent-deck delegate …` and `dot-agent-deck work-done …`, which connect to the daemon's local socket. Codex's `workspace-write` sandbox blocks that connection unless network access is allowed, and the pipeline then stops moving while the pane looks healthy.

Give the role a `command` in `.dot-agent-deck.toml` that allows it:

```bash
codex --sandbox workspace-write --ask-for-approval never -c "sandbox_workspace_write.network_access=true"
```

The `-c "sandbox_workspace_write.network_access=true"` override is the part that matters. See [Orchestration](orchestration.md) for role definitions.

### Manual Management

`hooks install` and `hooks uninstall` take `--agent` with one of `claude-code` (the default), `opencode`, `codex` or `devin`:

```bash
# Install
dot-agent-deck hooks install                    # Claude Code
dot-agent-deck hooks install --agent opencode   # OpenCode
dot-agent-deck hooks install --agent codex      # Codex (also records trust)
dot-agent-deck hooks install --agent devin      # Devin

# Remove
dot-agent-deck hooks uninstall                    # Claude Code
dot-agent-deck hooks uninstall --agent opencode   # OpenCode
dot-agent-deck hooks uninstall --agent codex      # Codex
dot-agent-deck hooks uninstall --agent devin      # Devin
```

For OpenCode, an explicit install always writes the binary it resolves now, replacing the path in an existing plugin file, and it writes the plugin into each OpenCode directory that exists, or creates `$XDG_CONFIG_HOME/opencode/plugin/dot-agent-deck.js` (default `~/.config/opencode/…`) when neither exists.

Hooks you uninstall come back the next time the TUI or the daemon starts, while the agent is still detected.

## Spawning agents

### A bare command like `claude`, `opencode`, `pi`, `codex`, or `devin` fails to spawn

A pane comes up with an error such as:

```text
Unable to spawn claude because it doesn't exist on the filesystem and was not found in PATH
```

The daemon looks up a bare command on its own `PATH`. When it starts, it runs your login shell (`$SHELL -ilc`) once and adopts the `PATH` that shell reports, so commands installed under `~/.local/bin` or added by `~/.bashrc` normally resolve. The lookup fails when the command is not on your login shell's `PATH`, when you installed the agent or changed your `PATH` after the daemon started, or when the shell took longer than 10 seconds and the daemon kept the `PATH` it inherited.

1. Check that the command resolves in a fresh login shell:

   ```bash
   $SHELL -ilc 'command -v claude'
   ```

   If that prints nothing, add the install directory to `PATH` in your shell startup files (for example `~/.profile` or `~/.bashrc`) until it does.
2. Restart the daemon so it reads the `PATH` again ([Recycling the daemon](#recycling-the-daemon)), then start the agent again.

Alternatively, give the full path to the agent as the command. If `command -v` finds it, the daemon was restarted, and the pane still cannot spawn it, look for the `login-shell PATH` line in the daemon's log ([Enabling Debug Logs](#enabling-debug-logs)): it records the `PATH` the daemon adopted, or that it captured none.

## Upgrades and version mismatches

### Recycling the daemon

Several fixes on this page end with a daemon restart. The daemon keeps running when you quit the TUI, so a new binary or a changed `PATH` takes effect only when a new daemon starts.

```bash
dot-agent-deck daemon stop     # or: dot-agent-deck daemon restart
dot-agent-deck                 # starts a fresh daemon on the way in
```

`daemon restart` is the same as `daemon stop`; the next `dot-agent-deck` starts the new daemon. Both refuse while the daemon manages running agents (`daemon has N managed agent(s) running; pass --force to terminate them`) or holds live orchestration roles (see [below](#an-orchestration-stops-being-able-to-delegate-the-daemon-holds-no-orchestration-role-for-pane-)). `--force` stops the daemon anyway: it terminates those agents, and escalates to `SIGKILL` if the daemon does not exit in time. Finish or stop the agents you care about first. In the desktop app, reconnect after the new daemon is up.

### Delegate prompts silently no-op after staying on an older daemon

*Applies to the TUI.*

After you upgrade `dot-agent-deck`, a new TUI can stay attached to a daemon started by the previous version. Features the older daemon does not know about may then not take effect, without an error: for example, delegated prompts appear to be sent but the orchestration does not move.

This happens when you chose it. When the TUI starts and finds a daemon from a different build:

- With **no agents running**, it restarts the daemon onto the new version without asking.
- With **agents running** and an interactive terminal, it shows `⚠  Daemon version mismatch  (N agent(s) running)`, names the agents a restart would stop, and offers `[S] restart daemon and continue   [any other key] keep current daemon`. Any key other than `S` keeps the older daemon and your agents.
- With **agents running** and no terminal (a script, a CI job, a pipe), it prints `error: local daemon is build <old> but this TUI is build <new>` and `recover with: dot-agent-deck daemon stop`, and exits non-zero.

To move to the new version, let the daemon restart: finish or stop your agents and run `dot-agent-deck` (it restarts the daemon without asking), or run `dot-agent-deck` and press `S` at the prompt, which stops the running agents. From a script, use `dot-agent-deck daemon stop` (with `--force` if agents must be terminated) and then `dot-agent-deck`. Check with `dot-agent-deck --version`, and relaunch: with no mismatch, no prompt appears.

### "daemon speaks attach protocol vN, but this binary speaks vM"

*Applies to the TUI and the desktop app.*

When the upgrade changed the attach protocol, the new TUI cannot attach to the older daemon at all. The mismatch prompt says `This binary cannot attach to it`, and its second option becomes `exit, leaving the daemon running`; declining (or running without a terminal) prints `error: daemon speaks attach protocol vN, but this binary speaks vM` and exits, leaving the daemon and its agents running. Either attach with the build that started that daemon to keep the agents, or stop it (`dot-agent-deck daemon stop`, which stops the agents too) and relaunch. See [Installation](installation.md) for upgrading.

If the daemon is replaced by one on another attach protocol while the TUI is open, the TUI does not attach to it, list its agents, start agents on it, or stop, rename or message its agents: a footer row says `⚠ Not attaching: daemon speaks attach protocol vN, but this binary speaks vM` with the same two ways out, after a short retry its panes show as disconnected, and a new agent pane you open is not started there. `Ctrl+w` on one of those panes or cards still closes it, in the TUI only, and says `Closed this pane here only: the daemon now speaks attach protocol vN, not this deck's vM, so the agent it showed could not be checked or stopped and may still be running`: that agent ran under the daemon that was replaced, and the new daemon is sent nothing about it. The footer goes away once a daemon on the TUI's own protocol answers again. In the desktop app, a daemon on another attach protocol shows as **Incompatible daemon** ([Dashboard](desktop/dashboard.md)). That includes one that replaced the daemon while the app was open: the app does not list its agents, follow their activity, start agents on it, or stop, rename, message or answer its agents, and an agent's terminal opened against it shows an error ending `Not attaching: the daemon speaks attach protocol vN, but this app speaks vM.` An agent pane you had open still closes with its **×**, which closes the view only. A replacement daemon that the app would refuse when it connects for another reason, a version that could make the app misread what it reports, is refused the same way, with the same message the Dashboard shows, unless you chose **Connect anyway**. Stopping one agent is refused there, but **Stop daemon** and **Replace daemon** still reach such a daemon, so that you can recover from it. The same prompt appears on `dot-agent-deck connect <remote>` when an upgrade that changed the protocol installed the new release but kept the remote's daemon running: press `S` to restart the remote's daemon (which stops its agents), or return to the version those agents run with `dot-agent-deck remote upgrade <remote> --version <old version>`.

### An upgrade installed the new release but the daemon still runs the old one

*Applies to the TUI, the CLI and the desktop app.*

`dot-agent-deck remote upgrade <remote>`, `connect`'s upgrade offer and the desktop app's **Upgrade** install the new release and then restart the remote's daemon onto it. When the result says the new release is installed but the daemon keeps running, the reason is in the same message:

- **You kept it, or nobody could answer.** Agents or orchestration roles were running, and you chose **Keep current daemon**, or the command ran without a terminal. Upgrade again when they have finished, and choose **Restart now** if you are ready to stop them.
- **`… is too old to restart itself`.** The daemon was started by a release that cannot be asked to restart. Run `dot-agent-deck connect <remote>`: the TUI on the host restarts the older daemon, asking first when agents are running.
- **`… did not answer within 20s`.** The old daemon stopped and the new one did not come up in time. Run `dot-agent-deck connect <remote>`, which starts one. If a systemd user service runs the daemon on the host, systemd normally starts the new one itself; check that the unit keeps `Restart=on-failure` and run `systemctl --user restart dot-agent-deck.service` there. When the message adds `it is still the daemon that was asked to restart`, the old daemon agreed to restart but never stopped: run the upgrade again, or run `dot-agent-deck daemon restart` on the host.
- **`… was replaced, but the new binary did not pass its version check`.** The upgrade failed while installing, after the new binary was already in place, so the host no longer has the old one; the message names what it has now, a version or `an unverified build`. The daemon keeps running. Run the upgrade again, and if the check keeps failing, run the binary the message names with `--version` on the host to see what it reports.
- **`… is too old to restart it from here`.** You installed an older release with `--version`, one from before the deck could restart a daemon during an upgrade. The daemon keeps running. Run `dot-agent-deck connect <remote>`: the TUI on the host restarts the daemon onto the older release, asking first when agents are running.

[Remote Environments → When the upgrade cannot restart the daemon](remote-environments.md#when-the-upgrade-cannot-restart-the-daemon) lists every result with its fix, and [Daemons → Upgrade a remote daemon](desktop/daemons.md#upgrade-a-remote-daemon) shows how the desktop app words them.

## Orchestration and delegation

### "DOT_AGENT_DECK_PANE_ID environment variable not set"

`delegate`, `work-done`, `dispatch`, `pane spawn` or `pane restart` exits 1 with:

```text
Error: DOT_AGENT_DECK_PANE_ID environment variable not set.
This command should be run from within a dot-agent-deck managed pane.
```

These commands act for the pane they run in, and the deck sets that variable only in panes it starts. Run the command from the orchestrator, worker or dispatcher pane it belongs to, not from a separate terminal. If an agent's own shell reports it, its launcher (a script, `env -i`, a container) dropped the deck's environment: pass the `DOT_AGENT_DECK_*` variables through. See [Orchestration](orchestration.md) for the commands.

### `work-done`, `dispatch` or `delegate` fails with "refused: … hook capability token"

A command an agent runs to talk to the daemon fails instead of returning quietly, for example:

```text
Error: the daemon did not accept this work-done report: refused: this pane was issued a hook capability token and the message presented none. The usual cause is that the `dot-agent-deck` binary invoked in this pane is older than the daemon that spawned it; set DOT_AGENT_DECK_HOOK_PROVENANCE=warn on the daemon to accept it anyway. [missing_token]
```

The daemon gives each agent it starts a token in that pane's environment, and the commands an agent uses to act for its pane present it: `delegate`, `work-done`, `dispatch` (including `--list-targets`), `pane spawn`, `pane restart`, `inbox list`, `inbox fetch` and `ack` (`ack` always exits 0, so it never shows this error). The daemon refuses one that names a token-bearing pane without that pane's token. The bracketed code says which case you have:

| Code | Message | Cause | Fix |
| --- | --- | --- | --- |
| `missing_token` | `…the message presented none…` | The `dot-agent-deck` the pane runs is older than the daemon, usually because the daemon was started from a different build than the one on the pane's `PATH`. | Make the two the same build: [recycle the daemon](#recycling-the-daemon) from the binary on your `PATH`. |
| `unknown_token` or `malformed_token` | `…is not one this daemon issued…` | The agent outlived the daemon that started it (a `daemon stop`, a version restart, a crash). | Restart that agent. It has also lost its orchestration role; see the [next entry](#an-orchestration-stops-being-able-to-delegate-the-daemon-holds-no-orchestration-role-for-pane-). |
| `token_names_another_pane` | `…was issued for a different pane…` | The message named a different pane from the one whose token it carried, for example because `DOT_AGENT_DECK_PANE_ID` was changed in that shell. | Run the command in the agent's own pane, with the environment the deck gave it. |

To keep a mixed install working for now, start the **daemon** with `DOT_AGENT_DECK_HOOK_PROVENANCE=warn`. That accepts a message with no token (and logs a warning naming the pane each time); it does not accept a token issued for another pane or by another daemon. Any other value, including a typo, leaves the check on.

```bash
dot-agent-deck daemon stop
DOT_AGENT_DECK_HOOK_PROVENANCE=warn dot-agent-deck
```

An accepted `work-done` or `dispatch` means the daemon admitted the message, not that the work behind it succeeded.

### An agent's card stops updating, and the daemon log says `refused a status event`

An agent the deck started keeps working in its pane, but its card stays on an old status, and the daemon's log ([Enabling Debug Logs](#enabling-debug-logs) says how to turn it on) has a line like:

```text
hook socket: refused a status event whose hook capability token does not attest the pane it names … reason="missing_token"
```

The status updates that drive a card carry the same token as the commands in the entry above, and the daemon refuses an update that names a pane it started without that pane's token. The reasons and fixes are the ones in that table: `missing_token` almost always means the `dot-agent-deck` on the pane's `PATH` is older than the daemon, so [recycle the daemon](#recycling-the-daemon) from the binary on your `PATH`, or start the daemon with `DOT_AGENT_DECK_HOOK_PROVENANCE=warn` to accept the updates with a warning. `token_names_another_agent` means the update carried this pane's token but named a different agent than the one the deck started there; run the agent with the environment the deck gave it. `token_generation_replaced` means the update named no agent and carried the token of an agent the deck has since replaced in that pane, usually a leftover process of the previous agent that is still running; the card keeps following the current agent, and stopping the leftover process ends the log lines.

If an agent was restarted in its pane and its card keeps showing the previous agent, with no such line in the log, the new agent's updates are not saying which agent sent them: make sure `DOT_AGENT_DECK_AGENT_ID` is still set in the agent's environment (a wrapper script that resets the environment drops it), or detach and reattach the deck (in the desktop app, reconnect to the daemon) to refresh its cards.

Agents you start yourself, outside the deck, need no token: their updates are accepted and, in the TUI, they get a card of their own. The desktop app lists only the agents the deck started, so such an agent has no card there (see [Agents the deck did not start](session-management.md#agents-the-deck-did-not-start)).

### An orchestration stops being able to delegate: "the daemon holds no orchestration role for pane …"

An orchestrator that has been delegating cannot any more. Its `dot-agent-deck delegate` fails with:

```text
Error: delegate from pane sched-issue-work-17-r0 failed: the daemon holds no orchestration role for pane sched-issue-work-17-r0, so this action was routed nowhere. Only a pane spawned as part of an orchestration can delegate.
```

The pane is still running and its card keeps updating. The daemon holds orchestration roles in memory only, so a daemon restart (`daemon stop --force`, a version restart, a crash) loses them. An agent that survives the restart keeps working and reporting status to the new daemon, but can no longer delegate or be delegated to.

In the TUI, an affected card shows `orphaned` in its title and an `Orphaned — delegation unavailable` row, once the new daemon hears from that agent. The desktop app does not show this marker; use `dot-agent-deck daemon status`, where the pane has no orchestration role.

There is no in-place recovery: close the orphaned panes and start the orchestration again (see [Orchestration](orchestration.md)). To avoid it, let an orchestration finish before restarting the daemon. `daemon stop` refuses while roles are live and lists them:

```text
daemon holds 2 live orchestration role(s):
  sched-issue-work-17-r0 orchestrator (orchestrator) [issue-work]
  sched-issue-work-17-r1 coder [issue-work]
stopping the daemon deletes these registrations for good — they are held in memory only, so any agent that survives the restart keeps running but can never delegate again
pass --force to stop anyway
```

Treat `--force` at that point as abandoning those runs.

## Panes and the dashboard

### Shift+Enter or Ctrl+Enter Sends the Message

In an agent's pane, in the TUI or in the [desktop app](desktop/index.md), **Shift+Enter** and **Ctrl+J** insert a new line in the agent's draft and **Enter** sends it, the same as when you run the agent directly. Both newline keys work in every supported agent.

**Ctrl+Enter** reaches the agent as Ctrl+Enter, and agents do different things with it:

| Agent | Ctrl+Enter |
| --- | --- |
| Claude Code | Sends the message (its own "send now" shortcut) |
| OpenCode | Inserts a new line |
| Codex, Pi, Devin | Nothing |

That is each agent's own choice, and an agent's update can change it. If you want a new line, use Shift+Enter or Ctrl+J.

The desktop app needs no configuration for any of this. The TUI asks the terminal for the enhanced ("kitty") keyboard protocol at startup, which is what lets it tell the keys apart; a terminal that supports that protocol needs no configuration. If you already have `keybind = shift+enter=csi:13;2u` in your Ghostty config, it does no harm.

If Shift+Enter still sends the message in the TUI:

- **You are running the deck inside tmux.** The deck does not enable the enhanced protocol when the terminal does not report support for it, which is the usual case inside tmux, and Shift+Enter then arrives as plain Enter. Run the deck outside tmux, or try having tmux pass extended keys through (`set -s extended-keys always` and `set -s extended-keys-format csi-u` in `~/.tmux.conf`).
- **Your terminal does not support the enhanced protocol.** If it supports custom key bindings, bind Shift+Enter to the CSI u sequence yourself (in Ghostty, the `keybind` line above). The deck forwards the modifier either way.
- **Your deck is older than this behaviour.** Upgrade.

### An editing shortcut does nothing in an agent's prompt

In an agent's prompt, in the TUI or in the desktop app, the same editing shortcuts work on every platform: `Home`, `End`, `⌘←`, `⌘→`, `Ctrl+←`, `Ctrl+→`, `⌥←`, `⌥→`, `⌘⌫`, `Ctrl+Backspace`, `⌥⌫`, `Ctrl+Delete` and `⌥⌦` (`⌥` is `Alt` and `⌘` the Windows or Super key outside a Mac), plus the paste key. [Editing an agent's prompt](keyboard-shortcuts.md#editing-an-agents-prompt) has the table. If one of them does nothing, or deletes or moves by a single character instead of a word or a line:

- **Your deck or desktop app is older than this behaviour.** Earlier desktop apps sent nothing for `⌘←` and `⌘→`, deleted one character for `⌘⌫` and `Ctrl+Backspace`, typed `Ctrl+V` into the agent on Windows instead of pasting. On a Mac they deleted one character, or nothing, for `Ctrl+Backspace` and `Ctrl+Delete`; on Windows and Linux, `Alt+Delete` made Claude Code delete everything after the cursor. Earlier TUIs deleted one character for `Ctrl+Backspace`, `Ctrl+Delete` and `⌘⌫`, and in some agents `⌥⌦` deleted one character and `Alt+←` / `Alt+→` moved one, while others typed `[3~`, `[D` or `[C` into the prompt. Upgrade.
- **Your system keeps the key.** macOS switches Spaces on `Ctrl+←` / `Ctrl+→`, and Windows and most Linux desktops arrange windows on the Windows key or Super with an arrow, so neither client receives those chords. Use `Home`, `End` or the `⌥` chords instead.
- **In the TUI: your terminal keeps the key or sends a plainer one.** The TUI acts only on what your terminal passes on. iTerm2 switches tabs on `⌘←` / `⌘→`, and a terminal without the enhanced keyboard protocol (GNOME Terminal, Konsole, or any terminal inside tmux) sends `Ctrl+Backspace` as a one-character backspace. [Editing an agent's prompt](keyboard-shortcuts.md#in-the-tui-what-your-terminal-passes-on) says what to set in each.
- **In the desktop app on Linux: a Super chord.** The app cannot see the Super key, so `Super+←` arrives as a plain `←`. Use `Home` and `End`.

**Check it worked:** type a few words into the agent's prompt, without sending them, and press `Ctrl+Backspace` or `⌥⌫`. The last word disappears.

### A pane says "disconnected" and ignores what you type

*Applies to the TUI.*

A pane whose title ends in `— disconnected` is no longer connected to an agent. Its last output stays on screen, and typing into it shows the reason instead of sending anything. Close the pane and start a new one; there is nothing to recover in place.

The TUI gets here only after trying to reconnect. The status message says which case you have:

- **`Agent exited on every restart — pane is disconnected. Close it to start over.`** The agent kept exiting without output (three times in a row). It usually fails at startup: check the command and working directory, and run that command yourself in a shell.
- **`Agent is no longer running — pane is disconnected. Close it to start over.`** No running agent claimed the pane within the retry window (about 10 seconds). Expected if you stopped the agent or the daemon restarted.

If neither fits, capture a log ([Enabling Debug Logs](#enabling-debug-logs); the TUI's side is enough here) and search it for `giving up on this pane`. The line's `reason` field is one of `empty-sessions` (the agent kept exiting), `no-live-agent` (the daemon answered and had no agent for the pane), `daemon-unreachable` (the daemon stopped answering) or `attach-failing` (the daemon had the agent but attaching to it kept failing). Include that line and the reconnect attempts before it when you report the problem.

### The deck restarted and its agents disappeared

*Applies to the TUI and the desktop app.*

When the deck restarts, the agents it was running can no longer be reached through it. When the TUI or the desktop app reconnects to the restarted deck, it removes those agents' cards and panes, and in the TUI an orchestration tab left with none of its agents, and shows one notice naming them: `The deck restarted; these agents were stopped: coder (worker in team), api-fix.` Each agent is named by its card's label, with its role and orchestration when it had them. The notice stays until you dismiss it, and agents a later restart stops are added to it rather than replacing the names you have not seen yet. In the TUI it has a row of its own above the footer's buttons, so a filter or rename you are typing does not hide it, and Help or a dialog opens above it when the dialog fits there. A dialog that would lose part of itself in that space covers the notice while it is open instead; the notice is back when you close the dialog. In a terminal too small for a Quit, Close, Stop or configuration confirmation, `Down`, `Enter` and Stop's `y` do nothing when they would move to or confirm a choice it does not show in full, including one its buttons cover: press `Esc` to cancel, or make the terminal larger. The Schedules manager's delete confirmation, the star prompt and the directory picker do not have this protection yet ([issue #1799](https://github.com/vfarcic/dot-agent-deck/issues/1799)). A terminal only a few rows tall can leave the notice no room at all, so make the window taller if you cannot see it. With more than three agents it names the first three and says how many more there are, as far as the terminal is wide. On the dashboard or an orchestration tab, in command mode, press `a` to list every agent, wrapping long names (in a terminal too short for the list, it asks you to make the terminal taller), and `c` to dismiss the notice. While you are typing in a pane or a field those keys type as usual: press `Ctrl+D` to leave the pane, or `Esc` to leave the field, first, or click `[Agents]` or `[Dismiss]`. In the desktop app it is a notice in the corner that lists every agent, scrolling when the list is long; close it with its `×`. If the restarted deck runs an agent in the same pane or under the same name, that agent gets a card of its own and does not inherit the removed card's name, and so does an agent outside an orchestration started on the restarted deck while the TUI was still reconnecting to it; the selection moves as it does when you close a card. A pane that only lost its connection for a moment, while the deck kept running, reconnects to its own agent as before. An orchestration started while the TUI was reconnecting is the exception ([issue #1800](https://github.com/vfarcic/dot-agent-deck/issues/1800)): the TUI builds no tab or role cards for it, so a role whose agent reports no status can stay without a card. Quit the TUI with **Detach**, which keeps the agents running, and start it again: it then builds the orchestration's tab from what the deck runs. The desktop app shows such an orchestration's agents on its Dashboard.

**TUI:**

![The TUI after the deck restarted: the dashboard says no agents are active, and the row above the footer's buttons says The deck restarted; these agents were stopped: api-fix, docs-review., with [Agents a] and [Dismiss c] at its right](/img/restart-notice-tui.png)

**Desktop:**

![The desktop app's Agent dashboard after the deck restarted: the Local daemon section runs no agents, and a notice in the bottom-right corner says The deck restarted; these agents were stopped: api-fix, docs-review., with a close button](/img/restart-notice-desktop.png)

Removing them stops nothing and deletes nothing: the agents simply stop being shown. A dispatched unit's worktree and branch stay on disk, so its work is still there. To pick up a stopped agent's conversation, start a new agent in the same directory with the agent's own resume option. For Claude Code, `claude --resume` lists that directory's conversations to choose from, and `claude --resume <session id>` resumes one directly.

#### An action says "that agent is gone (the deck restarted)"

You acted on an agent from before the restart in the moment before its card or pane was removed, or through something that still held it, such as a confirmation you had already opened or, in the desktop app, a voice command: you opened or focused it, typed into it, closed, renamed or messaged it, sent it the configuration request, retried or cancelled one of its messages, or, in the desktop app, answered its question or had its replies read aloud by voice. The action was not sent to the restarted deck, which may be running a different agent under the same name or in the same pane. Anything you typed into a TUI pane while it was reconnecting to the restarted deck is discarded rather than sent to the new agent. A rename refused this way is taken back, so the card keeps its old name. Act on the agents the restarted deck lists instead.

An orchestration whose cleanup meets a restart names its roles in the cleanup warning with this message.

If an action says `the deck did not identify itself, so the action on that agent was not sent`, the app could not tell whether the deck had restarted since it listed that agent, so it did not send the action. Restart the deck and try again.

### The deck is missing cards — a role or agent I know is running has no card

*Applies to the TUI.*

Read the deck's title row first:

- **`dot-agent-deck — 7 agent(s)  (↓2)`**: all agents are there, and two cards are below the bottom of the window. `(↑2)` means two are above; both show when you are scrolled into the middle. Move the selection (`j`/`k` by default) or give the terminal more rows.
- **`dot-agent-deck — 3/7 agent(s)`**: a filter is hiding four cards. Clear it with `Esc` (the default `clear_filter` key); `/` starts a new one. See [Keyboard Shortcuts](keyboard-shortcuts.md).

If the title shows neither and a card is still missing, the count is the number of agents the TUI knows about; compare it with `dot-agent-deck daemon status`. A role defined in `.dot-agent-deck.toml` that is in neither never started: check its `command` ([a bare command fails to spawn](#a-bare-command-like-claude-opencode-pi-codex-or-devin-fails-to-spawn)). A role that `daemon status` lists but the TUI has no card for can belong to an orchestration started while the TUI was reconnecting to the deck, which the TUI builds no tab for ([issue #1800](https://github.com/vfarcic/dot-agent-deck/issues/1800)): quit the TUI with **Detach** and start it again, as [The deck restarted and its agents disappeared](#the-deck-restarted-and-its-agents-disappeared) describes.

### Keys set in `keybindings.toml` have no effect

*Applies to the TUI.*

The TUI reads `keybindings.toml` once, at startup, and prints each problem it finds as a `keybindings (<path>): …` warning on stderr before it takes over the screen, so the warnings are easy to miss. A file that is not valid TOML is ignored as a whole, so every action keeps its default. In a valid file, an entry with a problem keeps its default binding (a binding to `Ctrl+C` instead leaves that action unbound).

1. Restart the TUI after editing the file.
2. To read the warnings, start it with stderr sent to a file, then quit: `dot-agent-deck 2>/tmp/dad-keys.txt`, then `grep keybindings /tmp/dad-keys.txt`.
3. Fix each entry the warnings name, using the key names and actions in [Keyboard Shortcuts](keyboard-shortcuts.md).

### A pane does not fill its box, or is cut off, while another app is open on the same agent

This is expected when two clients show the same agent: an agent has one screen size at a time, and the daemon chooses it.

- **The client you used last decides.** Focusing the desktop app's window, or clicking, scrolling or typing in a TUI, makes that client decide the size of every agent it shows. The other client shows the agent at that size: where its own pane is smaller, the agent is cut off at the right or bottom; where it is larger, the rest of the box stays blank. Using the other client switches the size back. Switching to an unrelated app, such as a browser, changes nothing.
- **Until a client has claimed focus, or when the one that did is not showing this agent, the smallest pane wins** on each axis, and larger panes leave the remainder blank. The agent grows back when the smaller view goes away: close the other client, or close the agent's pane in the desktop app.
- **A terminal without focus reporting** (tmux without `set -g focus-events on`, for example) cannot tell the TUI it gained focus, so the TUI takes over on your first key press or click rather than when you switch to it.
- **A TUI from an older release never claims focus.** While a current desktop app has focus on an agent they both show, the older TUI shows that agent at the desktop's size. Upgrade the TUI.

If a pane stays smaller than its box with no other client open, check for a `dot-agent-deck` TUI still running in another terminal or tmux window: it counts as a client for as long as it is open.

### Scrolling back in a pane shows nothing after another client resized the agent

When an agent's size changes, the daemon discards the output history it keeps for clients that attach later, because that output was drawn for the old size and would replay garbled. Only a client that attaches or re-attaches after the change is affected: it gets the correct live screen with no history behind it. A client that was already attached keeps its own scrollback. In practice you see this when you open an agent's pane in the desktop app after the agent was resized, or when a pane reconnects. Switching between two clients whose panes differ in size resizes the agent, so each switch discards the history again (switches within about a quarter of a second count as one). The history fills back in as the agent keeps working.

### An agent has a pull request but no badge

The pull request badge, on a TUI card and on the desktop app's rows and panes, comes from GitHub's `gh` command, run by the deck on the machine where the agent runs. For a [remote environment](remote-environments.md) that is the remote host, not your laptop. Check, on that machine and in the agent's working directory:

1. **`gh` is installed and signed in:** `gh auth status` says you are logged in to github.com. Install it from [cli.github.com](https://cli.github.com) and run `gh auth login` if not. On a remote host, run both there, for example `ssh <host> gh auth status`.
2. **The repository is on GitHub:** `git remote get-url origin` is a github.com address. A repository whose `origin` is elsewhere, or that has no `origin`, gets no badge.
3. **The agent is on the pull request's branch:** `git branch --show-current` names the branch the pull request was opened from. The repository's default branch (usually `main`), and a checkout of a commit rather than a branch, get no badge.
4. **GitHub has a pull request for that branch:** `gh pr list --head "$(git branch --show-current)" --state all` lists it. A pull request opened from a fork's branch of the same name is someone else's and gets no badge.

When all four hold, the badge appears without restarting anything: the deck looks again when the agent finishes its next turn, and otherwise within about a minute. If a lookup fails, the daemon's log says `pull request lookup failed` with the reason (see [Enabling Debug Logs](#enabling-debug-logs) for where the log is), and the deck tries again a few minutes later.

### Card borders or right edges look misaligned in the TUI

If card and pane borders are broken or shifted, a card's bottom-right corner is painted over, or text in card titles runs into the border, check whether your terminal is set to draw "ambiguous-width" characters two columns wide. The lines the deck draws borders with, and the `·`, `…` and `—` it uses in card titles and messages, are such characters, and the TUI expects them to be one column wide. That setting is not supported, so turn it off:

- **iTerm2:** Settings → Profiles → Text, clear **Ambiguous characters are double-width**.
- **GNOME Terminal:** Preferences → your profile → Compatibility, set **Ambiguous-width characters** to **Narrow**.
- **Other terminals:** look for a setting with "ambiguous" or "East Asian width" in its name.

The borders should line up again. If they are still off, quit the TUI, choosing **Detach** so your agents keep running, and start it again. The desktop app is not affected by this setting.

## Configuration and schedules

### A setting or environment variable has no effect

The TUI reads `config.toml` only when it starts, and the daemon keeps the environment it was started with. So a changed `config.toml` needs a TUI restart, and an exported `DOT_AGENT_DECK_*` variable that the daemon reads needs a daemon restart ([Recycling the daemon](#recycling-the-daemon)) from a shell that has the variable. [Configuration › When a setting does not take effect](configuration.md#when-a-setting-does-not-take-effect) lists the other cases, including a desktop app that offers no orchestrations for a directory the TUI handles, and `config set` rewriting the file (it drops comments and unknown keys, and replaces a file that does not parse with defaults plus the key you set, so repair such a file by hand first).

### A schedule does not run, or opens the wrong thing

Nothing opens at the scheduled time, or a run opens the whole team instead of one agent (or the reverse). Schedule problems are not shown in either client; the daemon writes them as `[scheduler]` lines to its output:

```bash
grep '\[scheduler\]' ~/.local/state/dot-agent-deck/daemon.log | tail -20
```

Also check the day-of-week field: numeric days count from `1` = Sunday, so `1-5` means Sunday to Thursday; write `MON-FRI` instead. [Schedules › When a schedule does not run](scheduled-tasks.md#when-a-schedule-does-not-run) maps each symptom and log line to its fix.

### Closing a dispatched issue tab lost my changes

Closing the tab of an issue-dispatch run removes the worktree the run created, with `git worktree remove --force`, so uncommitted changes in it are discarded. Commit (and push) the work you want to keep before closing the tab. See [Schedules › Clean up](scheduled-tasks.md#clean-up).

## Remotes

### An agent on a remote says an image or file "does not exist"

You are connected to a [remote environment](remote-environments.md), you drag a screenshot onto your terminal (or paste one with `Ctrl+V` / `Cmd+V`), and the agent says the file is not there. The agent runs on the remote; the file is on your laptop. Dragging inserts a laptop path, which does not exist on the remote, and pasting reads the remote's clipboard. Copy the file to the remote first, then give the agent the remote path:

```bash
scp ~/Desktop/screenshot.png my-vm:/tmp/
```

See [Remote Environments › Getting files to the remote](remote-environments.md#getting-files-to-the-remote).

### A remote will not connect, or an ssh tunnel to it is not working

First check plain ssh, the way the deck calls it (non-interactively, so it cannot answer a host-key or passphrase prompt):

```bash
ssh -o BatchMode=yes <user>@<host> true
```

If that fails, fix ssh before anything else: run a plain `ssh <user>@<host>` once to accept a new host key (`remote add` fails with `ssh failed: host key not yet trusted for <target>` until you do), and add a passphrase-protected key to your agent with `ssh-add <key>`. [Remote Environments › Failure modes](remote-environments.md#failure-modes) lists each message `connect`, `remote add` and `remote upgrade` print, with its fix.

When plain ssh works, run the diagnosis for that remote:

```bash
dot-agent-deck remote doctor my-vm
```

It runs read-only checks — ssh reachability and authentication, whether the deck is installed on the remote, the forwards `ssh -G` resolved, the remote sshd's `AllowTcpForwarding` and `ClientAliveInterval` (from `sshd -T`), and whether a configured forward is bound — and prints each as `PASS`, `WARN`, `FAIL` or `UNKNOWN`, naming the setting and file to change. It changes nothing: not your ssh config, not the remote's sshd config, not the deck's list of remotes, and nothing on the remote.

| Exit code | Meaning |
| --- | --- |
| `0` | every check is clear |
| `1` | a check failed |
| `2` | a check could not be determined |

On a remote with no reverse tunnel configured, `remote doctor` exits `1` because its `RemoteForward` check fails with `ssh resolved no reverse tunnel`; that is expected. For such a remote, only `HostReachable`, `RemoteBinary` and `ProtocolCompatible` say whether it works.

It tells apart two causes whose ssh error messages are identical: `AllowTcpForwarding no` on the remote and a local port collision. See [Remote Recipes › Troubleshooting with `remote doctor`](remote-recipes.md#troubleshooting-with-remote-doctor) for example output of each case.

## Desktop app

### "Daemon disconnected" or "Incompatible daemon"

*Applies to the desktop app.*

- **Daemon disconnected**: the app is not connected to that daemon, and the sentence under the title says why. When it says no daemon is running there, press **Start daemon**; otherwise press **Reconnect**. [Daemons → Start a daemon from the app](desktop/daemons.md#start-a-daemon-from-the-app) lists what a failed start says and what to do. A daemon started on its own with `dot-agent-deck daemon serve` exits after 30 seconds with no clients, agents or pending schedules; to keep it up while you start the app, run `DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS=0 dot-agent-deck daemon serve`.
- **Incompatible daemon** (for example `This daemon is older than this app, and the two cannot work together.`): the daemon and the app are different versions, usually after upgrading one of the two, and the message says which one is older. Update that one so both run the same version and press **Reconnect**: for a remote daemon older than the app, press **Upgrade** in the message, which installs the app's version there and restarts the daemon, asking before it stops any running agent ([Daemons → Upgrade a remote daemon](desktop/daemons.md#upgrade-a-remote-daemon)); for a local daemon, [recycle it](#recycling-the-daemon) from the matching binary. When the message says the app has not connected because it could misread what the daemon reports, the app also offers **Connect anyway**, which uses the daemon as it is until you quit the app. The choice covers the whole app session: until you quit, the app also uses any other daemon it would refuse for that same reason, on any deck, including one that replaces this daemon later. **Technical details** under the message shows the exact versions. If it still cannot connect, for example because the daemon stopped answering in the meantime, the daemon's card says why and what to do next.

See [Daemons](desktop/daemons.md) for adding and testing daemons in the app.
