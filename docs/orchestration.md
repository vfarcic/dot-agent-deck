# Orchestration

An orchestration is a team of agents that you start together. One role, the **orchestrator**, receives your request and hands tasks to the other roles, the **workers**, with `dot-agent-deck delegate`. Each worker runs in its own pane, does its task, and reports back with `dot-agent-deck work-done`. You talk to the orchestrator; it runs the team.

The team is defined in a `.dot-agent-deck.toml` file in the project directory. Both clients start and show orchestrations: the TUI opens one as an orchestration tab, and the desktop app shows it as an **ORCHESTRATION** group on its Dashboard. The orchestration keeps running in the daemon when you detach the TUI or close the desktop app, and the workers' reports are in the orchestrator's pane when you come back.

A video walkthrough of a coder → reviewer + auditor → release pipeline is at <https://youtu.be/ZIWWDDu02Ik>.

## Set up a three-role orchestration

This task creates an orchestrator, a `coder` and a `reviewer` for one project, starts them, and hands them a first request.

Before you start:

- `dot-agent-deck` is installed and `dot-agent-deck --version` prints a version. See [Installation](installation.md).
- The agent CLI each role runs is installed and signed in. The example uses `claude` (Claude Code). The other agents the deck recognises are `opencode`, `pi`, `codex` and `devin`.
- The agent's hooks are installed, so role cards show live status. See [Getting Started](getting-started.md).

### 1. Write `.dot-agent-deck.toml`

Create `.dot-agent-deck.toml` in the project's root directory, the directory you will open the orchestration in:

```toml
[[orchestrations]]
name = "team"

[[orchestrations.roles]]
name = "orchestrator"
command = "claude"
start = true
prompt_template = """
You coordinate the team. You never write or review code yourself; you delegate.

Workflow:
1. Delegate implementation to coder.
2. When coder reports done, delegate a review of the change to reviewer.
3. If reviewer reports blocking issues, re-delegate to coder with the exact findings.
4. When reviewer is satisfied, summarise the result for the user and stop.

Workers start with no memory of this conversation. Put everything a worker needs in the task:
file paths, the spec, and the previous worker's findings.
"""

[[orchestrations.roles]]
name = "coder"
command = "claude"
description = "Implements features, fixes bugs, refactors code"
prompt_template = "Implement the requested change. Run the project's tests before reporting completion. If the project is a Git repository, commit your changes before you report."

[[orchestrations.roles]]
name = "reviewer"
command = "claude"
description = "Reviews code changes for correctness, style, and edge cases"
prompt_template = "Review the change. Report findings only; do not modify code."
```

The example works in any directory. In a Git repository (`git rev-parse --is-inside-work-tree` prints `true`), the coder also commits each change, which gives the reviewer a commit to read; outside one it leaves the changes in the working tree.

`dot-agent-deck init` writes a two-role starter file with the same shape if you prefer to start from it; it refuses to overwrite an existing `.dot-agent-deck.toml`. Every key is described in the [configuration reference](#configuration-reference).

**Check:** run `dot-agent-deck validate` in the same directory. It prints `Config is valid.` and exits 0. Anything it prints instead is described under [Validate your config](#validate-your-config).

### 2. Let the roles write files and run the deck's commands

The orchestrator and the workers hand tasks and reports to each other through files under `.dot-agent-deck/` and through the `dot-agent-deck delegate`, `work-done` and `ack` commands. A role whose agent must ask permission for each of those stops at an approval prompt, and an unattended pane waits there indefinitely. If you launch a role with a restricted tool allowlist, see [Context handoff and permissions](#context-handoff-and-permissions) before continuing. A plain `claude` command, as in the example, asks you to approve these steps in its pane the first time.

### 3. Start the orchestration

**TUI:**

1. Run `dot-agent-deck`.
2. Press `Ctrl+n` to open the New Agent form.
3. Use `Enter` to step into directories and `Space` to select the project directory.
4. On the **Mode** field, press `Left`/`Right` (or `h`/`l`) until it shows `Orch: team`. The Command field disappears: each role runs its own `command`.
5. Press `Enter`.

A new tab opens with one pane per role. The role cards are in the left sidebar, and the orchestrator's pane is focused.

![The demo-loop orchestration tab with both roles at work: the tab bar shows Dashboard and demo-loop, two role cards are stacked in the left sidebar, planner (Claude Code, reading src/checkout/flow.ts, Last: 2s) and builder (Codex, editing src/checkout/RetryPayment.tsx, Last: 3s), both Working, and the orchestrator role, planner, is selected with its pane active on the right](/img/orchestration-tui.png)

**Desktop:**

1. Open **New agent** from the Dashboard (or press `Ctrl+N` / `⌘N`) and choose the daemon.
2. Browse to the project directory (it is tagged **project**) and press **Use this directory**.
3. Under **Mode**, pick the `Orch: team` chip. There is no **Command** field: each role runs its own `command`.
4. Optionally type a **Name** for the run, then press **Activate orchestration**.

The Dashboard shows an **ORCHESTRATION** group with a numbered row per role and an **ORCHESTRATOR** badge on the orchestrator. Click a row to open that role's terminal. See [Desktop app → Dashboard](desktop/dashboard.md) and [New agent](desktop/new-agent.md).

![The desktop app's Dashboard with an activated orchestration: below the standalone agents, an ORCHESTRATION group named demo-loop with a Close button and a numbered row per role, 01 planner carrying the ORCHESTRATOR badge and 02 builder](/img/orchestration-desktop.png)

**Check:** run `dot-agent-deck daemon status`. It prints one row per agent with the columns `PANE AGENT ROLE STATUS TOOL LABEL CWD`. The three roles appear with ROLE `orchestrator (orchestrator)`, `coder` and `reviewer`, and CWD is the project directory. `(orchestrator)` marks the role that may delegate.

### 4. Give the orchestrator a request

Type your request into the orchestrator's pane, for example *"Add input validation to the signup form and have it reviewed."* The orchestrator already knows the roles and how to delegate; the deck gives it that context when the orchestration starts.

**Check each hand-off:**

- When the orchestrator delegates, the `coder` card (TUI) or row (desktop) changes to a working status, and the coder's pane shows a line of the form `Read .dot-agent-deck/worker-task-coder.md for your task. [delivery d-…]`. The task is in `.dot-agent-deck/worker-task-coder.md` in the project directory.
- When the coder finishes, it runs `dot-agent-deck work-done`. Its report appears in the orchestrator's pane and is saved to `.dot-agent-deck/work-done-coder.md`.
- The orchestrator then delegates to `reviewer`, and the same two signs appear for it.

If a step does not happen, see [When something goes wrong](#when-something-goes-wrong). If a worker stops responding, the deck reports it to the orchestrator; see [Idle Workers & Notifications](idle-workers-and-notifications.md).

## Quick setup

Instead of writing the file yourself, you can have an agent in the TUI propose one. Generating is a TUI feature; the file it writes is used by both clients.

![The Generate .dot-agent-deck.toml dialog with Yes / No / Never options](./img/orchestration-generate-dialog.png)

1. Run `dot-agent-deck` and open an agent pane on the project directory (`Ctrl+n`, Mode `No mode`).
2. Press `Ctrl+d` to enter command mode, select that agent's card, and press `g`.
3. Choose **Yes**. The deck sends the agent a prompt asking it to analyse the project, pick roles from the [role library](#role-library), find the commands that launch agents in this project (devbox scripts, Makefile targets, bare `claude`, `opencode`, `pi`, `codex` or `devin`), and propose a config. **No** closes the dialog; **Never** stops the deck offering it for that directory.
4. Review the proposal and tell the agent what to change. It writes `.dot-agent-deck.toml` to the project directory.
5. Run `dot-agent-deck validate`.

## Configuration reference

### Where the file is read from

The deck reads `.dot-agent-deck.toml` from the directory the orchestration is opened in, and only from that directory; it does not look in parent directories. A dispatched unit reads the copy in its own worktree ([Dispatcher Mode](dispatcher-mode.md)).

When edits take effect:

- A worker's `command`, `agent`, `prompt_template` and `clear` are re-read on every delegation, so an edit applies to that worker's next task without restarting anything.
- What the orchestrator knows (its own `prompt_template` and the workers' names and `description`s) is written into its context when the orchestration starts. To be sure the orchestrator sees an edit to those, start the orchestration again.
- Role **names** are fixed when the orchestration starts. A running role keeps the name it was started with. A role you **add** to the file can be started into the running orchestration with [`dot-agent-deck pane spawn <role>`](#commands); a role you **rename** is reachable under its new name only after you start the orchestration again.

Keys the deck does not recognise are ignored without a warning, and `dot-agent-deck validate` does not report them, so a misspelled key (`promt_template`) silently has no effect.

### Top-level keys

These go **above the first table header** in the file. TOML attaches a key written below a table header to that table, where the deck ignores it.

| Key | Type | Default | Description |
|---|---|---|---|
| `worker_response_timeout_minutes` | integer | `120` | Minutes a worker may take before the orchestrator is told it has not reported. `0` turns the report off; `1`–`10080` are used as written; any other value falls back to `120`. See [Idle Workers & Notifications](idle-workers-and-notifications.md#change-how-long-a-worker-may-take). |
| `[features]` | table | — | Feature flags such as `experimental`. See [Configuration](configuration.md). |
| `[[orchestrations]]` | array of tables | none | One entry per orchestration, described below. |

A `[[modes]]` block from older releases is ignored; `dot-agent-deck validate` warns that it can be deleted.

### `[[orchestrations]]`

| Key | Type | Required | Default | Description |
|---|---|---|---|---|
| `name` | string | no | the directory's name | The orchestration's name, shown in the tab bar, on the `Orch: <name>` Mode chip, and used by `dispatch --orchestration <name>` and a schedule's `shape = "orchestration:<name>"`. An empty or missing name uses the project directory's name. |
| `default` | boolean | no | `false` | Marks the orchestration a run opens when nothing names one. See [Which orchestration a schedule opens](#which-orchestration-a-schedule-opens). |
| `extends` | string | no | — | The `name` of another orchestration in the same file whose roles this one inherits. See [Sharing a workflow with `extends`](#sharing-a-workflow-with-extends). |
| `roles` | array of tables | yes | — | The roles, written as `[[orchestrations.roles]]` entries. `validate` requires at least two after inheritance. A block that `extends` another may list only the roles it changes. |

### `[[orchestrations.roles]]`

| Key | Type | Required | Default | Description |
|---|---|---|---|---|
| `name` | string | yes | — | The role's name: shown on its card, passed to `delegate --to`, `pane restart` and `pane spawn`, and used in the file names `.dot-agent-deck/worker-task-<name>.md` and `work-done-<name>.md`. Must be unique within the orchestration, must not be empty, and must not contain `/`, `\` or `..`. Case-sensitive. |
| `command` | string | yes | — | The shell command that starts the role's agent, run in the orchestration's directory, for example `claude`, `claude --model sonnet`, `opencode --model gpt-4o`, `codex`, `devbox run agent-coder`. May be omitted only in a block that `extends` another and inherits the role. |
| `agent` | string | no | derived from `command` | Which agent `command` starts, when `command` runs it through a launcher: one of `claude`, `opencode`, `pi`, `codex`, `devin`. See [Declaring the agent behind a launcher command](#declaring-the-agent-behind-a-launcher-command). |
| `start` | boolean | no | `false` | `true` marks the orchestrator. `validate` requires exactly one role with `start = true`. |
| `description` | string | no | — | What the role is for. The orchestrator reads it to choose which worker gets a task, and it is shown on the role's card. `validate` warns about a worker without one. |
| `prompt_template` | string | no | — | Standing instructions. For a worker, they are written at the top of every task file it receives, followed by a `## Task` heading and the orchestrator's task. For the orchestrator, they are included in the context it receives when the orchestration starts. |
| `clear` | boolean | no | `true` | `true` restarts the worker's agent before each task, so every task starts with a fresh context. `false` keeps the agent running and types each task into its existing session. See [What `clear` does to delivery](#what-clear-does-to-delivery). |

Which role is the orchestrator, for a file `validate` rejects: the first role with `start = true`; if none has it, the role named `orchestrator`; otherwise the first role.

### Declaring the agent behind a launcher command

The deck identifies the agent from the start of `command`: `claude --model opus`, `/usr/local/bin/codex`, `env FOO=1 codex` and `sh -c 'codex …'` are recognised. It cannot see what a launcher starts: `devbox run -- codex`, `mise exec -- codex`, `nix develop -c codex`, `make codex`, `./run-codex.sh`.

A role whose agent the deck cannot identify:

- shows **No agent** and no status on its TUI card, and a blank **CLI** column in the desktop app. A Codex role behind a launcher stays blank until its first task starts. A Claude Code role behind a launcher identifies itself when it starts, so it looks normal;
- if it is a Codex, Pi or OpenCode worker with `clear = true`, waits up to 30 seconds before every task is delivered;
- gets no automatic [re-send of a lost task](#a-lost-task-is-re-sent-into-the-same-worker).

`dot-agent-deck validate` warns about each such role. Fix it by declaring the agent:

```toml
[[orchestrations.roles]]
name = "reviewer"
command = "devbox run -- codex --sandbox workspace-write"
agent = "codex"
description = "Reviews code changes"
```

- The value is lower case: `claude`, `opencode`, `pi`, `codex` or `devin`.
- A name the deck does not know gives the role no agent at all; it does not fall back to the command. `validate` warns and lists the accepted names.
- `agent` wins over `command`. Keep the two consistent.
- An empty value is the same as leaving the key out.

### Validate your config

```bash
cd your-project
dot-agent-deck validate            # or: dot-agent-deck validate --path your-project
```

| Output | Exit status | Meaning |
|---|---|---|
| `Config is valid.` | 0 | No errors and no warnings. |
| lines starting `[warning]` on stderr | 0 | Usable, but something is probably not what you meant. |
| any line starting `[error]` on stderr | non-zero | The orchestration will not open correctly. Fix every error. |
| `No .dot-agent-deck.toml found in <dir>` | non-zero | No file in that directory. |
| a TOML parse error naming the file | non-zero | The file cannot be read at all, and no orchestration in it opens. |

Each issue is printed as `[error] '<orchestration>': <message>` or `[warning] '<orchestration>': <message>`.

Errors:

- `orchestration must have at least 2 roles`
- `orchestration must have exactly one role with start = true`
- `role name is empty or whitespace`, or `role name '…' contains unsafe path characters (../, /, or \)`
- `role '…' has an empty command`
- `duplicate role name '…'`
- `more than one orchestration declares default = true (…)`
- `declares default = true but defines no roles, …`

Warnings:

- `duplicate orchestration name`
- `N orchestrations are defined and none declares default = true, …`
- `worker role '…' has no description — orchestrator won't know its capabilities`
- `role '…': unknown agent '…' …` (an `agent` value the deck does not know)
- `role '…': the deck cannot tell which agent … launches and the role declares no agent …`
- `workspace modes were removed (#1199); this block is ignored and can be deleted` (a leftover `[[modes]]` block)

These are reported as parse errors instead, and stop the whole file from loading: an `extends` naming an orchestration that is not in the file, naming a name that more than one block uses, or forming a cycle; an empty `extends`; and a new role (one not inherited through `extends`) with no `command`.

## Role library

Roles are yours to define; the deck imposes no fixed set. When it [generates a config](#quick-setup), the agent starts from these suggestions:

| Role | Description | Suggested `clear` |
|---|---|---|
| `coder` | Implements features, fixes bugs, refactors code | `true` |
| `reviewer` | Reviews code changes for correctness, style, and edge cases | `true` |
| `auditor` | Audits code for security vulnerabilities and unsafe patterns | `true` |
| `tester` | Writes and runs tests; useful for TDD-style flows | `true` |
| `documenter` | Writes and updates documentation only — never modifies source code | `true` |
| `release` | Runs the project's release/PR/merge workflow; never modifies code | `false` |
| `researcher` | Investigates the codebase or external sources to gather context | `true` |

A `release` role uses `clear = false` because a release spans several tasks (open the PR, wait for CI, merge), and a restarted agent would forget the branch and PR it was working on.

## Example orchestrations

### Code review pipeline

Orchestrator → coder → reviewer and auditor in parallel → release.

```toml
[[orchestrations]]
name = "dev-flow"

[[orchestrations.roles]]
name = "orchestrator"
command = "claude --model opus"
start = true
prompt_template = """
You coordinate the team. You never implement, review, or audit work yourself.

Workflow:
1. Delegate implementation to coder. Include the relevant spec path.
2. After coder is done, delegate to reviewer and auditor in parallel. Include the files coder changed.
3. If reviewer or auditor flags a blocking issue, re-delegate to coder with the exact finding.
4. Repeat until reviewer and auditor are satisfied.
5. Before delegating to release, summarise what to validate end-to-end and stop until the user confirms.
6. Delegate the release flow to release.

Workers start with no memory of this conversation or of other workers' output. Include all context in
the task: file paths, spec paths, error messages, findings. If context is long, write it to
.dot-agent-deck/<slug>.md and pass that file rather than pasting it.
"""

[[orchestrations.roles]]
name = "coder"
command = "claude --model sonnet"
description = "Implements features, fixes bugs, refactors code"
prompt_template = """
Implement the requested change. Read the spec file first if one is referenced.
Run the project's test suite before reporting completion.
Commit your changes before calling dot-agent-deck work-done.
If critical context is missing from the task, say so in your work-done summary.
"""

[[orchestrations.roles]]
name = "reviewer"
command = "claude"
description = "Reviews code changes for correctness, style, and edge cases"
prompt_template = """
Review the change. Report findings only; do not modify code.
Focus on correctness, consistency with the codebase, edge cases, and missed requirements.
If a spec is referenced, verify the implementation matches it.
"""

[[orchestrations.roles]]
name = "auditor"
command = "opencode --model gpt-4o"
description = "Audits code for security vulnerabilities and unsafe patterns"
prompt_template = """
Audit the change for security vulnerabilities and OWASP top-10 class issues. Report findings only; do not modify code.
"""

[[orchestrations.roles]]
name = "release"
command = "claude --model haiku"
clear = false
description = "Runs the project's release flow; never modifies source code"
prompt_template = """
Run the release flow: create branch, push, open PR, wait for CI, merge.
Do not modify source code. If any step fails, report the exact error and stop.
"""
```

### TDD cycle

Orchestrator → tester (writes failing tests) → coder (makes them pass) → tester (confirms) → repeat.

```toml
[[orchestrations]]
name = "tdd"

[[orchestrations.roles]]
name = "orchestrator"
command = "claude --model opus"
start = true
prompt_template = """
You run a TDD cycle. You never write code or tests yourself.

Workflow:
1. Delegate to tester to write failing tests for the requested feature.
2. Delegate to coder to implement until the tests pass.
3. Delegate back to tester to confirm the tests pass and coverage is adequate.
4. If tester finds gaps, re-delegate to coder with the specific failing tests.

Include test file paths and the feature spec in every delegation. When chaining tester → coder, list which tests fail.
"""

[[orchestrations.roles]]
name = "tester"
command = "claude"
description = "Writes and runs tests; useful for TDD-style flows"
prompt_template = """
Write tests first, then run them to confirm they fail before any implementation.
Follow the project's test layout and naming conventions.
Report which tests you wrote and which pass or fail.
"""

[[orchestrations.roles]]
name = "coder"
command = "claude --model sonnet"
description = "Implements features, fixes bugs, refactors code"
prompt_template = """
Implement the minimum code to make the listed failing tests pass.
Do not modify the test files. Run the test suite before reporting completion.
"""
```

## Working in an orchestration

### Navigating the orchestration tab

*The TUI's orchestration tab. In the desktop app, open a role's terminal by clicking its row in the ORCHESTRATION group.*

These keys work in command mode; press `Ctrl+d` first if you are typing in a role pane:

| Key | Action |
|---|---|
| `Left` / `Right` (or `h` / `l`) | Previous / next tab |
| `1`–`9` | Focus role card N and its pane |
| `Ctrl+t` | Toggle between `Stacked` (only the focused role's pane is shown) and `Tiled` (every role's pane) |
| `Ctrl+l` | Toggle the sidebar width between 34% and 25% of the frame (one setting for every orchestration tab) |
| `Ctrl+z` | Zoom the focused pane to the whole frame; press again to return |
| `Ctrl+w` | Close the orchestration tab, which stops every role, after a confirmation |
| `Ctrl+e` | Toggle the command-entry lock; only with the experimental flag on (see below) |

`Ctrl+PageDown` / `Ctrl+PageUp` switch tabs from anywhere, including while typing in a role pane. The keys can be remapped; see [Keyboard Shortcuts](keyboard-shortcuts.md).

The sidebar shows each role's status live. A **background** orchestration tab's label takes the colour of its most urgent role: Error (red), then Needs Input (magenta), then Working (green), then Thinking (blue). A tab whose roles are all idle keeps the ordinary tab colour.

### Zooming the focused pane

`Ctrl+z` in command mode shows the focused role's pane over the whole frame, and pressing it again restores the previous view. Every agent keeps running while you are zoomed, and reports and statuses carry on, but the sidebar is hidden, so you do not see a worker that starts waiting for you while the border shows `[Z]`. See [Keyboard Shortcuts](keyboard-shortcuts.md#ctrlz-zooms-the-focused-agent-pane).

### Typing into a worker is locked by default (experimental)

> This lock exists only when the experimental flag is on: `experimental = true` under `[features]` in `.dot-agent-deck.toml`, or `DOT_AGENT_DECK_EXPERIMENTAL=1` in the environment. With the flag off, the default, you can type into any role pane and the deck does not move focus on its own.

With the flag on, keystrokes aimed at a worker pane in the TUI are dropped until you unlock with `Ctrl+d` then `Ctrl+e`, so an instruction meant for the orchestrator does not land in a worker by mistake. Unlock whenever you need to reach a worker directly: a stuck agent, a prompt you did not expect. See [Keyboard Shortcuts](keyboard-shortcuts.md#ctrle-locks-command-entry-to-the-orchestrator-pane).

#### Focus follows the lock

While the TUI is **locked**, it moves focus within the active orchestration tab: to a role pane as soon as that role starts waiting for input (the lowest-numbered one first when several are waiting), and back to the orchestrator once none is waiting. It does not switch tabs to follow a waiting pane elsewhere; that tab's label colour shows it instead. While **unlocked**, focus stays where you put it.

### Closing an orchestration

**TUI:** `Ctrl+w` on the orchestration tab, after a confirmation. **Desktop:** the group's **Close** button, after a confirmation that lists the roles (**Close all N roles**). Either stops every role's agent.

## How delegation works

1. The orchestrator runs `dot-agent-deck delegate --to <role> --task-file <file>` (or `--task "<text>"`).
2. The deck writes the worker's task file, `.dot-agent-deck/worker-task-<role>.md` in the worker's directory: the role's `prompt_template`, then `## Task`, then the task. It then types one line into the worker's pane: `Read .dot-agent-deck/worker-task-<role>.md for your task. [delivery d-XXXXXXXX]`.
3. The worker runs `dot-agent-deck ack d-XXXXXXXX` (the task file tells it to), does the work, and runs `dot-agent-deck work-done --task-file <file>`.
4. The deck saves the report to `.dot-agent-deck/work-done-<role>.md` and types it into the orchestrator's pane as a new message.

The deck teaches the orchestrator and the workers these commands when they start, so a `prompt_template` only needs to describe your workflow.

![Coder pane active and working after receiving a delegation from the orchestrator](./img/orchestration-coder.png)

The orchestrator can delegate one task to several roles at once (`--to reviewer --to auditor`). Each starts immediately and reports back separately.

![Orchestrator delegating to reviewer and auditor in parallel — both cards light up simultaneously](./img/orchestration-delegation-parallel.png)

### Commands

These commands work only from a pane the deck started, which sets `DOT_AGENT_DECK_PANE_ID` and the other variables they need. Run anywhere else, `delegate`, `work-done`, `pane restart` and `pane spawn` print `Error: DOT_AGENT_DECK_PANE_ID environment variable not set.` and exit non-zero; `ack` prints a message and exits 0. `delegate`, `pane restart` and `pane spawn` are further limited to the orchestrator's pane.

| Command | Who runs it | What it does |
|---|---|---|
| `dot-agent-deck delegate --to <role> [--to <role> …] (--task <text> \| --task-file <path>) [--supersede]` | orchestrator | Sends a task to one or more workers. `--task-file -` reads the task from stdin. `--task-file` is the safe choice for text with quotes, backticks, `$` or newlines; the file must be a regular file of at most 1 MiB. `--supersede` sends even to a worker that still owes a `work-done` ([One task per worker at a time](#one-task-per-worker-at-a-time)). |
| `dot-agent-deck work-done (--task <text> \| --task-file <path>) [--done]` | worker, or orchestrator with `--done` | Reports a finished task to the orchestrator. The orchestrator uses `--done` to mark the whole orchestration complete; in a dispatched unit, that reports back to the dispatcher ([Dispatcher Mode](dispatcher-mode.md)). |
| `dot-agent-deck ack <delivery-id>` | worker | Tells the deck the task arrived, which stops re-sends. Always exits 0. |
| `dot-agent-deck pane restart <role> [--force]` | orchestrator | Replaces the worker's agent with a fresh one and drops the task it owed. Without `--force`, only a worker whose agent has exited (crashed or finished) is restarted. |
| `dot-agent-deck pane spawn <role>` | orchestrator | Starts a role that is in `.dot-agent-deck.toml` but not running in this orchestration, for example one added to the file after the orchestration started, or one whose pane was closed. Refused for a role that is already running and for the orchestrator role. |

`delegate` exit status and output:

| Outcome | Exit status | Printed on stderr |
|---|---|---|
| Delivered to every named role | 0 | nothing |
| Delivered to some roles, not others | 0 | `Warning: …` naming the roles that were missed and why. Re-send only to those roles; repeating the whole command gives the delivered roles the task twice. |
| Delivered to no role | non-zero | `Error: delegate from pane … reached no worker for role(s): …`, or `… was NOT sent: every worker it reached still owes a work-done …` |
| Refused: the caller is not the orchestrator | non-zero | `Error: delegate from pane … failed: pane … is the \`<role>\` role, not this orchestration's orchestrator, so it may not delegate.` |
| Daemon not reachable | non-zero | `Error: could not reach the dot-agent-deck daemon socket, …` |

A role "reaches no worker" when it is not in the file, when it is the orchestrator itself, when its pane was closed, or when it was renamed or added after the orchestration started.

`work-done` exits 0 when the daemon accepted the report (or gave no reply it could read), and non-zero with `Error: the daemon did not accept this …` when it refused it, or `Failed to send work-done signal to daemon socket.` when no daemon answered.

### One task per worker at a time

A worker that has been given a task is busy until it sends `work-done`, and until then `delegate` refuses to give it another. What counts is whether the worker has reported, not its status: a worker's card (TUI) or row (desktop) can read idle while the worker still owes a `work-done`. A worker whose agent exited without reporting is still busy, until a different agent takes over its pane: a task the exited agent had received does not make the new agent busy.

When the earlier task is not coming back:

- **`delegate --supersede`** sends the task anyway. The earlier task is not cancelled: on a `clear = false` worker the new task goes into the same session, and a late `work-done` for the earlier task still counts. On a `clear = true` worker the agent is restarted, so the earlier task is lost.
- **`pane restart <role>`** replaces the worker's agent and drops the task it owed, together with its pending [idle-worker reports](idle-workers-and-notifications.md).
- **Wait.** Seven days after a task was sent, the deck stops counting it.

If the orchestrator's own agent was replaced since it delegated, the new orchestrator is not blocked by the old one's tasks; `delegate` says which earlier task it superseded.

### What `clear` does to delivery

With `clear = true` (the default), the deck stops the worker's agent, starts the role's `command` again in the same pane, waits for the new agent to be ready, and then types the task pointer. The role's card or row stays in place with the same name, but the previous conversation is gone. The wait is about one to eight seconds for `claude`, `codex`, `opencode` or `pi`, depending on the agent, and up to 30 seconds for a Codex, Pi or OpenCode role behind a launcher without an [`agent`](#declaring-the-agent-behind-a-launcher-command) line. A worker whose agent has exited is started again the same way on its next task.

With `clear = false`, the agent keeps running and the task pointer is typed into its current session immediately. The worker keeps what it learned from earlier tasks.

If the new agent cannot be started, or exits before it takes the task, the task is not delivered and the orchestrator is told; see [A delegated worker never came up](#a-delegated-worker-never-came-up).

If tasks are lost regularly on your machine because agents are not yet ready when the task is typed (the task line sits unsubmitted in the worker's input box, or the worker looks idle and never starts), raise the wait with `DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS`, in milliseconds. It replaces the deck's own wait for every agent, also applies to a schedule's first prompt, and is capped at `30000`. Like the other variables on this page, it is read by the daemon; see [Setting the delivery variables](#setting-the-delivery-variables).

#### A lost task is re-sent into the same worker

When a worker shows no sign of starting its task and has not run `ack`, the deck sends the task again into the same agent; it does not restart the worker to do it. If the task line is sitting unsent in the worker's input box, the deck presses Enter instead of typing it again. With the default schedule it tries three more times, about 20 seconds, 1 minute and 2 minutes 20 seconds after the first attempt. The task file tells the worker that a task line it sees twice is the same task.

If none of the re-sends gets a response, the orchestrator receives the [went-quiet report](idle-workers-and-notifications.md#the-reports), about 3 minutes 40 seconds after the task was first sent with the default schedule, or later if the worker was still starting or the task waited for an unsent draft.

Which workers are covered:

- Claude Code, OpenCode, Devin and Pi workers: re-sent as described.
- Codex workers: the deck presses Enter for a task left in the input box, but does not type the task a second time.
- A role whose agent the deck cannot identify (a launcher without an `agent` line): no re-send.
- A Pi role with `clear = true` fetches its task itself, so there is nothing to re-send.

A re-send is skipped whenever you have typed into that worker's pane since the task went in, so it does not submit text you started there.

`DOT_AGENT_DECK_DELEGATE_RETRY_SCHEDULE_MS` changes the schedule: a comma-separated list of waits in milliseconds, each measured from the previous attempt (default `20000,40000,80000`). `0` or an empty value turns re-sending off. Each wait is clamped to 100–300000 ms and at most 8 entries are read; a value that is not a list of numbers is ignored and the default is used.

### A deck prompt waits while you have an unsent draft

The deck types messages into panes for you: a delegated task, a worker's report, a scheduled prompt, the reports in [Idle Workers & Notifications](idle-workers-and-notifications.md). If you have typed text into that pane through the deck, in either client, and not sent it, the deck's message waits instead of being submitted together with your text. Press **Enter** to send your text, or **Ctrl+U** or **Ctrl+C** to clear it, and the waiting message follows. Other panes keep running meanwhile.

The wait lasts at most **60 seconds**. After that the message is sent anyway, possibly together with your text, and the pane's status shows **Error** in the TUI (**FAILED** in the desktop app). `DOT_AGENT_DECK_DRAFT_DEFER_CAP_MS` changes the limit (milliseconds, at most `600000`); `0` switches the wait off.

Text the agent itself put in its input box, such as a prompt recalled from history, does not make a message wait. Messages the deck sends at your request, such as a new orchestration's first prompt, do not wait either.

### Setting the delivery variables

`DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS`, `DOT_AGENT_DECK_DELEGATE_RETRY_SCHEDULE_MS`, `DOT_AGENT_DECK_DRAFT_DEFER_CAP_MS` and the variables in [Idle Workers & Notifications](idle-workers-and-notifications.md#tune-or-turn-off-the-other-reports) are read by the **daemon**. Set them in the environment of the command that starts the daemon, for example:

```bash
DOT_AGENT_DECK_DELEGATE_RETRY_SCHEDULE_MS=10000,20000 dot-agent-deck
```

A daemon that is already running keeps the environment it started with, so a variable set on a later `dot-agent-deck` command does not reach it. Restart the daemon to apply a change: `dot-agent-deck daemon restart` (it refuses while agents are running unless you pass `--force`, which stops them). See [Configuration](configuration.md) for the full list of environment variables.

## Context handoff and permissions

A worker starts each task with no memory of your conversation with the orchestrator and no access to other workers' output. The task text, plus the worker's `prompt_template`, is everything it knows. Use the orchestrator's `prompt_template` to say how to delegate well: which files to reference, how to summarise earlier findings when chaining workers, what to include when retrying.

A reliable pattern is to give the orchestrator a tracking file (a spec, a PRD, a checklist) and tell it to keep it updated. Workers can be pointed at it, and an orchestrator whose context was compacted or restarted can read it to resume.

The roles hand over tasks and reports by writing files and running the deck's commands. For roles launched with a restricted tool allowlist:

- **Allow writing files.** A role launched with `claude --allowedTools Bash Read`, for example, stops at an approval prompt each time it writes a task or report. Add the file-writing tool: `--allowedTools Bash Read Write`.
- **Match the full path.** The deck tells agents to run its commands by their full path, such as `/home/you/.local/bin/dot-agent-deck work-done …`. A rule written against the bare command text, such as the Claude Code allow rule `Bash(dot-agent-deck work-done:*)`, does not match. Write the rule against the path shown in the agent's pane.
- **Allow `ack` wherever you allow `work-done`.** An allowlist that names `Bash(/home/you/.local/bin/dot-agent-deck work-done:*)` also needs `Bash(/home/you/.local/bin/dot-agent-deck ack:*)`, or the worker stops at an approval prompt at the start of every task. The orchestrator needs `delegate` (and `pane` if it should restart workers) allowed the same way.

## Files the deck writes

The deck keeps its hand-off files in `.dot-agent-deck/` inside the orchestration's directory (for a worker in another directory, inside that worker's directory). In a git repository, `dot-agent-deck init` and writing an orchestrator context add `.dot-agent-deck/` to `.git/info/exclude`, if it is not already there, so git does not pick the files up; `.gitignore` is not edited. A file git already tracks stays tracked.

| File | Written when | Contents |
|---|---|---|
| `orchestrator-context-<id>.md` | when an orchestration starts | What the orchestrator is told: its `prompt_template`, the available workers, and how to delegate. |
| `orchestrator-context.md` | when an orchestration starts | A copy of a recent orchestrator context, kept for compatibility. |
| `worker-task-<role>.md` | each delegation | The task for that role, overwritten by the next one. |
| `work-done-<role>.md` | each `work-done` for a delegated task | The worker's last report. |
| other `*.md` files | when an agent writes them | Longer context an orchestrator passes by file. |

When it writes an orchestrator context, the deck deletes `.md` files in this directory, other than `orchestrator-context.md`, that were last modified more than 14 days ago. `DOT_AGENT_DECK_COORDINATION_RETENTION_DAYS` changes the number of days, and `0` turns the clean-up off. Do not keep your own files there.

## More than one orchestration

A project can define several `[[orchestrations]]` blocks, for different kinds of work or to run the same team on different providers.

### Sharing a workflow with `extends`

`extends` makes an orchestration inherit another's roles, so you write only what differs. Typical use: the same team on another provider, where only each role's `command` changes.

```toml
[[orchestrations]]
name = "mixed"
default = true

[[orchestrations.roles]]
name = "orchestrator"
command = "devbox run agent-orchestrator"
start = true
prompt_template = """
You coordinate the team. …
"""

[[orchestrations.roles]]
name = "coder"
command = "devbox run agent-coder"
description = "Implements features, fixes bugs"

[[orchestrations]]
name = "GPT"
extends = "mixed"

[[orchestrations.roles]]
name = "orchestrator"
command = "devbox run agent-orchestrator-oc"

[[orchestrations.roles]]
name = "coder"
command = "devbox run agent-coder-oc"
```

`GPT` gets both roles with `mixed`'s `start`, `description` and `prompt_template`; only the commands differ.

Rules:

- `extends` names the parent's literal `name`. The parent may be anywhere in the file. A block with no `name` cannot be a parent, and a name used by more than one block cannot be extended.
- Roles are matched by name and keep the parent's order, whatever order the overrides are written in.
- A key you omit keeps the parent's value. To turn off an inherited `clear = true`, write `clear = false`; an omitted key means "inherit", not "false". The same applies to `start`.
- A role name the parent does not have is added as a new role at the end and must have its own `command`.
- Chains work (`a` extends `b` extends `c`); a cycle is an error.
- `name`, `default` and `extends` itself are not inherited.

An `extends` error stops the whole file from loading, with a message naming the orchestrations involved.

### Which orchestration a schedule opens

When you start an orchestration yourself you choose it: the TUI's Mode field and the desktop app's **Mode** chips list the project's orchestrations, and a [dispatcher](dispatcher-mode.md) asks you. `default = true` matters when nobody is asked: a [schedule](scheduled-tasks.md) whose directory defines several orchestrations and whose `shape` names none, or `dispatch --orchestration=` with an empty value.

```toml
[[orchestrations]]
name = "prd"
default = true
# roles …

[[orchestrations]]
name = "issue"
# roles …
```

- At most one orchestration may set `default = true`, and it must have roles.
- If none sets it, the first orchestration in the file that has roles is used. With several orchestrations, set it anyway: otherwise reordering the blocks changes which team every scheduled run opens, and `validate` warns about exactly that.
- With a single orchestration the key has no effect.

A dispatcher agent sees the default marked in its target list:

```
Available dispatch targets:
  single            one agent (--single)
  orchestration     'prd' — 6 roles (--orchestration 'prd')  [default]
  orchestration     'issue' — 4 roles (--orchestration 'issue')

Ask the user which they want before dispatching, then pass the matching flag.
```

A schedule shows nothing, so for a schedule the "none declares default" warning reaches only the [daemon log](troubleshooting.md#enabling-debug-logs).

### Running several at the same time

Orchestrations in **different directories** run side by side. Tasks and reports stay within their own orchestration, even when two orchestrations have the same `name`.

For parallel work on the same project, give each orchestration its own git worktree:

```bash
git worktree add ../myproject-feature-x -b feature-x
```

Then start the orchestration in `../myproject-feature-x`. A [dispatcher](dispatcher-mode.md) does this for you.

Two orchestrations in the **same directory** still route tasks to the right workers, but they share the task and report files (`worker-task-<role>.md` and `work-done-<role>.md` are named by role only, so two `coder` roles overwrite each other's) and the working tree. The TUI's New Agent form warns before you start one:

```
  ! This directory already runs an orchestration.
    Both share .dot-agent-deck/*-{role}.md files
    and one working tree; /worktree-prd isolates.
```

`Enter` starts it anyway. The desktop app's **New agent** shows a similar warning, and **Activate orchestration** still starts the run.

## When something goes wrong

General problems (hooks, spawning agents, remotes) are in [Troubleshooting](troubleshooting.md). The daemon log, which several entries below point to, is described in [Enabling debug logs](troubleshooting.md#enabling-debug-logs).

### `DOT_AGENT_DECK_PANE_ID environment variable not set`

`delegate`, `work-done`, `dispatch` or `pane` was run outside a pane the deck started, for example in your own terminal. Run them from a role pane.

### `… is the <role> role, not this orchestration's orchestrator, so it may not delegate`

Only the orchestrator can run `delegate`, `pane restart` and `pane spawn`. Check that exactly one role has `start = true` (`dot-agent-deck validate`). A file without one makes the role named `orchestrator`, or else the first role, the orchestrator, which may not be the one you meant.

### `the daemon holds no orchestration role for pane …`

The pane is not part of a running orchestration in the daemon. If the orchestration was running earlier, see [An orchestration stops being able to delegate](troubleshooting.md#an-orchestration-stops-being-able-to-delegate-the-daemon-holds-no-orchestration-role-for-pane-).

### `delegate` says "reached no worker for role(s)"

- The `--to` value must match a role `name` exactly, including case.
- A role renamed in the file after the orchestration started keeps its old name until the orchestration is started again.
- A role added after the orchestration started, or whose pane was closed, is not running: have the orchestrator run `dot-agent-deck pane spawn <role>`.
- A worker that crashed or quit on its own is not running either, unless its role has `clear = true`, which starts a fresh worker for every task: have the orchestrator run `dot-agent-deck pane restart <role>`, then delegate again.
- The orchestrator cannot delegate to itself.
- Delegation does not cross orchestrations: the worker must be in the same orchestration as the orchestrator.

### `delegate` says "this delegate was NOT sent: every worker it reached still owes a work-done"

The worker has not reported its earlier task. See [One task per worker at a time](#one-task-per-worker-at-a-time). If you believe it did report, look for its `work-done` in its pane: a refused one (for example over a [capability token](troubleshooting.md#work-done-dispatch-or-delegate-fails-with-refused--hook-capability-token)) never reached the deck.

### The worker received the task line but never started

The deck [re-sends it](#a-lost-task-is-re-sent-into-the-same-worker), and the orchestrator gets the went-quiet report once the re-sends run out. The task line in the worker's pane ends with `[delivery d-…]`; search the daemon log for that id to see each re-send and why they stopped. A role behind a launcher gets no re-send; declare its [`agent`](#declaring-the-agent-behind-a-launcher-command). If this happens often, raise `DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS` ([What `clear` does to delivery](#what-clear-does-to-delivery)).

### A worker stops at an approval prompt at the start of every task

Its allowlist does not permit `ack`, writing files, or `work-done` by its full path. See [Context handoff and permissions](#context-handoff-and-permissions).

### A role card reads "No agent", or a Codex role stays blank until its first task

The role's `command` starts the agent through a launcher. Add an [`agent`](#declaring-the-agent-behind-a-launcher-command) line. If there already is one, check its spelling; `dot-agent-deck validate` names an unknown value.

### A delegated worker never came up

A `clear = true` worker was restarted for a new task and the new agent did not come up, so the task was not delivered and no `work-done` will come for it. The orchestrator's pane gets one of:

- `⚠ delegated worker respawn failed (dot-agent-deck daemon report)`: the new agent could not be started at all.
- `⚠ delegated worker never came up (dot-agent-deck daemon report)`: the new agent started, then exited before it took the task.

The report names the worker's pane; the daemon log names the role and, for a failed start, the error. The usual cause is the role's `command`: a launcher that fails in that directory, a binary that is not on the `PATH` the daemon was started with, or an agent that exits immediately. Look at the worker's pane for what the agent printed, and run the role's `command` yourself in the worker's directory. Re-delegating to the role runs the same command, so it fails the same way until the command is fixed.

### `pane restart` says "has not crashed; pass --force to restart a healthy pane"

Without `--force`, `pane restart` restarts only a worker whose agent has exited. An agent that is running but hung has not exited, so it is refused the same way. Look at the worker's pane; if it is stuck, run `dot-agent-deck pane restart <role> --force`. The orchestrator can use `--force` when it sees this message; if you want force-restarts to stay your decision, say so in its `prompt_template`.

### `pane restart` says the worker's working directory "is not a directory"

The directory the worker runs in has been deleted, or a file now has its path. Nothing was restarted and the running worker was left as it was. Put the directory back, or close the orchestration and start it again from a directory that exists.

### `pane restart` says the prepared working directory "was replaced"

The role was started from the desktop app's New agent dialog or Runs screen, and the directory at its path is no longer the one that launch checked: the project was moved away and another directory put in its place. Nothing was restarted and the running worker was left as it was. Move the original directory back, or start the orchestration again from the desktop app.

### `pane spawn` says the role "is already running in this orchestration"

`pane spawn` starts a role that has no pane; it does not start a second copy. To run two workers of the same kind, give the second its own role name in `.dot-agent-deck.toml` (for example `reviewer2`) and spawn that.

### The orchestrator receives no report

Reports are typed into the orchestrator's pane. If that pane is closed, the report is lost, but for a delegated task it is also saved to `.dot-agent-deck/work-done-<role>.md`. If the daemon log shows `failed to write work-done summary`, that file is from an earlier task; the orchestrator then receives the report inline, on one line, without its Markdown formatting.

An orchestrator that runs `work-done` without `--done` reports to nobody; it should delegate the work to a role instead.

### The orchestrator is told a report was "unsolicited"

A `work-done` the deck cannot match to a task the orchestrator delegated reaches it labelled as unsolicited, and `.dot-agent-deck/work-done-<role>.md` is not updated. Causes:

- you gave the worker a task directly by typing in its pane, and it reported again. Give tasks through the orchestrator instead;
- the task never reached the worker (the orchestrator saw `⚠ delegated worker respawn failed` or `⚠ delegated worker never came up`);
- the task was sent more than seven days ago;
- `pane restart` dropped the task the worker owed.
- the worker the task went to exited before reporting, and the report came from a different agent started in its pane since.

### The report went to a different file than `work-done-<role>.md`

A file the deck did not write was already at `.dot-agent-deck/work-done-<role>.md`, usually because the worker saved its own report there. The deck leaves that file as it is and saves the report to a new file in the same `.dot-agent-deck` directory. The orchestrator's pane, in the TUI and in the desktop app alike, is told where the report is and that the existing file was left alone, since it may hold more of the worker's report. To avoid this, have workers save their reports under another name; the reporting instructions the deck gives them already suggest one.

### The orchestrator does not know its workers, or a dispatched orchestration is refused

The orchestrator learns its roles and how to delegate from a context file the deck writes into `.dot-agent-deck/`. The deck will not write it into a `.dot-agent-deck` that is a symlink, or that grants write access to group or other and cannot be fixed with `chmod`. A dispatched orchestration is then not started, and the dispatcher is told why: the reason names the symlink, or the directory's mode and `chmod go-w`. An orchestration started from the TUI still opens, but its orchestrator is not given that context. Replace a symlinked `.dot-agent-deck` with a real directory in the project, or run `chmod go-w .dot-agent-deck`, then start the orchestration again.

### A `prompt_template` change has no effect

Changes apply to the next task. Check that the file is in the orchestration's directory, that the key is spelled `prompt_template` (unknown keys are ignored silently), and that the role's `name` matches the `--to` value exactly.

## See also

- [Idle Workers & Notifications](idle-workers-and-notifications.md): what the deck reports to the orchestrator about stuck workers
- [Dispatcher Mode](dispatcher-mode.md): start an agent or a whole orchestration in an isolated copy of the repository
- [Schedules](scheduled-tasks.md): start an orchestration on a timer
- [Configuration](configuration.md): the rest of `.dot-agent-deck.toml`, global settings and environment variables
- [Keyboard Shortcuts](keyboard-shortcuts.md): every TUI key
