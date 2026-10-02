# New Agent

**New agent** starts something on a daemon: a single agent, an orchestration from a project's `.dot-agent-deck.toml`, a dispatcher, or an agent that helps you write a schedule. It is the desktop counterpart of the TUI's `Ctrl+n` form and offers the same kinds of start except workspace modes.

![The New agent dialog over the Dashboard: the Local daemon chosen under Daemon, a directory chosen in the browser with Use this directory beside it, and the form below with Dir, the Mode chips (No mode selected), Name pre-filled from the directory, an empty Command, and Discard and Create agent](/img/new-agent-desktop.png)

## Start an agent

1. On the [Dashboard](dashboard.md), press **New agent** at the top, or `Ctrl+N` / `⌘N` (not while an agent pane is open or a text field has focus). A daemon section's own **New agent** button does the same with that daemon already chosen.
2. Under **Daemon**, choose the daemon the agent should run on (below).
3. Under **Directory**, browse to the directory the agent should work in and press **Use this directory**, or `Space` (below).
4. In the form, leave **Mode** on **No mode**, check **Name**, and type the agent's **Command**, for example `claude`.
5. Press **Create agent**.

**Check it worked:** the dialog waits for the daemon to list the new agent, then closes and opens that agent's [pane](dashboard.md#the-agent-pane), where you see its terminal. The agent also appears as a row in its daemon's section. If the daemon has not listed it within about 12 seconds, the dialog closes and the Dashboard says the agent was started but is not listed yet; press **Refresh**.

To start an orchestration instead, choose a directory tagged **project** in step 3, pick its `Orch: <name>` chip in step 4, and press **Activate orchestration**. Every role the orchestration defines starts, and each appears as a row in one **ORCHESTRATION** group. See [Orchestration](../orchestration.md) for defining one.

## Daemon

The daemons that can take a new agent are listed, each with a tag saying whether it is **LOCAL** or **REMOTE**. A daemon that cannot take one right now, for example because it is not connected or is a different version from this app, is not listed here; the dashboard shows it with the reason and what you can do about it. When exactly one daemon can take an agent, it is chosen for you. If none can, or no daemon is configured, the dialog says so in one line instead of showing a list. Keys: `j` / `k` or the arrow keys move, `Enter` chooses. If you ask for a daemon by voice that cannot take a new agent, the app says which one and why, for example "“build box” can't take a new agent: it is older than this app."

## Directory

The browser lists directories **on the chosen daemon's machine**, not on the computer the app runs on, so for a remote daemon you are browsing the remote host. It opens at that daemon's [`default_dir`](../configuration.md#set-the-directory-new-agents-start-browsing-in) when one is set, and at the daemon user's home directory otherwise.

- A directory holding a `.dot-agent-deck.toml` is tagged **project**. Once you choose it, its orchestrations become Mode chips.
- A symbolic link to a directory is tagged **link**. Opening it lists the directory it leads to, and an agent started there runs in that real directory.
- Type in the filter (or press `/`) to narrow the list by name. **Show hidden** (or `.`) also lists directories whose names start with `.`.
- When a directory has more subdirectories than the daemon lists at once, the browser says so; typing a filter then asks the daemon to search all of them.

| Key in the list | Does |
| --- | --- |
| `j` / `k`, `↓` / `↑` | Move |
| `l`, `Enter`, `→` | Open the highlighted directory |
| `h`, `Backspace`, `←` | Go up to the parent |
| `Space` | Use the directory being shown |
| `/` | Focus the filter |
| `.` | Show or hide hidden directories |
| `Escape` | Clear the filter; with no filter, close the dialog |
| `q` | Close the dialog |

**Show hidden**, **link** rows and the daemon-side search need a daemon from v0.43.0 or later; an older daemon is browsed without them. A daemon that cannot list directories at all says so, and you have to choose another daemon.

The TUI's directory picker is different: it reads the filesystem of the machine the TUI runs on and leaves out hidden and symlinked directories.

## The form

The form stays disabled until a directory is chosen; **Dir** then shows it.

**Mode** chooses what to start. `←` / `→` move between the chips. The chips offered depend on the directory and the daemon:

| Chip | Starts |
| --- | --- |
| **No mode** | A single agent running **Command**. |
| `Orch: <name>` | The orchestration of that name from the chosen project's `.dot-agent-deck.toml`: every role in it. The **Command** field is hidden (each role has its own command), and the button reads **Activate orchestration**. See [Orchestration](../orchestration.md). |
| **schedule** | An agent that helps you write a schedule. See [Schedules](../scheduled-tasks.md). |
| **schedule: issues** | An agent that helps you write an issue-dispatch schedule. Offered only when the chosen daemon has the `experimental` flag on. |
| **dispatcher** | A dispatcher agent. See [Dispatcher Mode](../dispatcher-mode.md). |

If a project defines two orchestrations with the same name, both chips are shown disabled with a sentence asking you to rename one. A daemon too old to say which schedule and dispatcher agents it can start gets a sentence saying why those chips are missing.

**Name** names the agent, or the orchestration run. It is pre-filled with the directory's name. For an orchestration, a name a live orchestration already uses on that daemon is refused, and if the directory already runs an orchestration on that daemon you are warned that the two share its role files and working tree.

**Command** is what the agent runs, typically `claude`, `opencode`, `pi`, `codex` or `devin`. It is pre-filled from the chosen daemon's [`default_command`](../configuration.md#set-the-command-new-agents-start-with) when one is set, otherwise from the command this app last started a plain agent with on that daemon since the app was opened. Left empty:

- with **No mode**, it starts the daemon's default shell;
- with **schedule**, **schedule: issues** or **dispatcher**, it starts the daemon's `default_command`, or `claude` when none is set. The field's placeholder names the command.

With [voice](voice.md#setting-the-command) on, you can also say "set the command to" followed by the command. That only fills the field; the agent starts when you press **Create agent** or say "start it".

## Closing and discarding

Closing the dialog (`Escape`, `q`, the close button, or a click outside it) keeps what you entered as a draft, and the next **New agent** brings it back until you quit the app. **Discard** closes the dialog and forgets the draft; a successful start forgets it too. While a start is waiting for the daemon to answer, the dialog cannot be closed.

## When it does not work

| What you see | What to do |
| --- | --- |
| The daemon you want is not listed, or the dialog says "No daemon can take a new agent now." | That daemon cannot take a new agent right now. The dashboard shows it with the reason and what to do. A remote daemon that is not answering is covered by [Daemons → Test connection](daemons.md#test-connection). |
| "This daemon cannot list directories, so no directory can be chosen on it here." | That daemon does not answer the directory-listing request, which is what a daemon from an older release does. Upgrade the deck on that machine, or choose another daemon. |
| The directory you want is not listed | It may be hidden (press `.`), or past the daemon's listing limit (type part of its name in the filter). |
| No `Orch:` chip for a project | The chosen directory is not the one holding `.dot-agent-deck.toml`, or the file has an error; the form shows the daemon's reason when it has one. [Orchestration](../orchestration.md) covers the file. |
| An error under the form after **Create agent** | The daemon refused the start; the message says why, and **Detail** shows the full text. |
| The agent starts but its status never leaves **RUNNING** | Its hooks are not reporting; see [Troubleshooting → Hooks](../troubleshooting.md#hooks). |
