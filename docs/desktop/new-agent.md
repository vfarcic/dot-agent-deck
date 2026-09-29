---
title: New Agent
---

# New Agent

**New agent** starts an agent on a daemon. Open it with the **New agent** button at the top of the [Dashboard](dashboard.md), with a daemon section's own **New agent** button (which picks that daemon for you), or with `Ctrl+N` / `⌘N` on the Dashboard (not while an agent pane is open). It is the desktop counterpart of the TUI's `Ctrl+n` form, and it offers the same kinds of start except workspace modes.

The dialog has three parts, top to bottom: **Daemon**, **Directory**, and the form.

![The New agent dialog over the Dashboard: the Local daemon chosen under Daemon, a directory chosen in the browser with Use this directory beside it, and the form below with Dir, the Mode chips (No mode selected), Name pre-filled from the directory, an empty Command, and Discard and Create agent](/img/new-agent-desktop.png)

## Daemon

Every daemon the app knows is listed, with a tag saying what kind it is. Pick the one the agent should run on. A daemon that cannot take a new agent right now is greyed out, with the reason beside it. If no daemon is configured, the list says so.

## Directory

The directory browser lists directories **on the chosen daemon's machine**, not on the computer the app runs on, so for a remote daemon you are browsing the remote host. It opens at that daemon's [`default_dir`](../configuration.md#default-directory) when one is set.

- A directory holding a `.dot-agent-deck.toml` is tagged **project**; its orchestrations become Mode chips once you choose it.
- A symbolic link to a directory is tagged **link**. Opening it lists the directory it leads to, and an agent started there runs in that real directory.
- Type in the filter (or press `/`) to narrow the list by name. **Show hidden** (or `.`) lists directories whose names start with `.`.
- When a directory has more subdirectories than the daemon lists at once, the browser says so; typing a filter then asks the daemon to search all of them.

Keys in the list: `j` / `k` move, `l` or `Enter` opens, `h` or `Backspace` goes up, `Space` uses the current directory, and `q` closes the dialog. Or click **Use this directory**.

**Show hidden**, **link** rows and the daemon-side search need a daemon from v0.43.0 or later; an older daemon is browsed without them. A daemon that cannot list directories at all says so, and you have to choose another daemon.

The TUI's picker reads the filesystem of the machine the TUI runs on and skips hidden and symlinked directories.

## The form

The form stays disabled until a directory is chosen; **Dir** then shows it.

**Mode** chooses what to start. The chips offered depend on the directory and the daemon:

| Chip | Starts |
| --- | --- |
| **No mode** | A single agent running **Command**. |
| `Orch: <name>` | The orchestration of that name, from the chosen project's `.dot-agent-deck.toml`: every role in it. There is no **Command** field, and the button reads **Activate orchestration**. See [Orchestration](../orchestration.md). |
| **schedule** | An agent that helps you write a schedule. See [Schedules](../scheduled-tasks.md). |
| **schedule: issues** | An agent that helps you write an issue-dispatch schedule. Shown only when the chosen daemon has the `experimental` flag on. |
| **dispatcher** | A dispatcher agent. See [Dispatcher Mode](../dispatcher-mode.md). |

The TUI also offers the project's workspace modes (`[[modes]]`) as Mode choices; the desktop app does not. A daemon too old to offer the schedule and dispatcher chips gets a sentence saying why they are missing instead.

**Name** names the agent, or the orchestration run. For an orchestration, a name a live orchestration already uses on that daemon is refused, and if the directory already runs an orchestration on that daemon you are warned that the two share its role files and working tree.

**Command** is what the agent runs, typically `claude`, `opencode`, `pi`, `codex` or `devin`. It is pre-filled from the chosen daemon's [`default_command`](../configuration.md#default-command), or the last command started there when none is set, as in the TUI. Left empty, it starts the daemon's default shell; for the schedule and dispatcher chips, the placeholder names the agent command an empty field starts instead.

Press **Create agent** (or **Activate orchestration**). The dialog waits for the daemon to list the new agent, then closes and opens that agent's [pane](dashboard.md#the-agent-pane). If the daemon has not listed it in time, the dialog closes and the Dashboard says the agent was started but is not listed yet.

## Closing and discarding

Closing the dialog keeps what you entered as a draft, and the next **New agent** brings it back. **Discard** closes the dialog and forgets the draft. While a start is waiting for the daemon to answer, the dialog cannot be closed.
