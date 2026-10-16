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

**Check:** run `dot-agent-deck daemon status`. It prints one row per agent with the columns `PANE AGENT ROLE STATUS TOOL INBOX WAKE LABEL CWD`. The three roles appear with ROLE `orchestrator (orchestrator)`, `coder` and `reviewer`, and CWD is the project directory. `(orchestrator)` marks the role that may delegate.

### 4. Give the orchestrator a request

Type your request into the orchestrator's pane, for example *"Add input validation to the signup form and have it reviewed."* The orchestrator already knows the roles and how to delegate; the deck gives it that context when the orchestration starts.

**Check each hand-off:**

- When the orchestrator delegates, the `coder` card (TUI) or tile (desktop) shows `✉1 queued` until the coder takes the task, then `✉ delivered`, and its status changes to a working one. The task reaches the coder's pane as a deck message whose `Sent by:` line names the orchestrator; see [Delivery in the TUI and desktop app](#delivery-in-the-tui-and-desktop-app) for how each agent shows it. In the desktop app, the handoff rail shows `delegated · queued`, then `delivered · working`. A copy of the task is also saved to `.dot-agent-deck/worker-task-coder.md` in the project directory.
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
| `subscribe` | array of strings | no | `[]` | Announcement topics received by roles in this orchestration. See [Announcements and pooled jobs](#announcements-and-pooled-jobs). |
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
| `clear` | boolean | no | `true` | `true` restarts the worker's agent before each task, so every task starts with a fresh context. `false` keeps the agent running and queues each task in its inbox. See [What `clear` does to delivery](#what-clear-does-to-delivery). |
| `subscribe` | array of strings | no | `[]` | Additional announcement topics received by this role. |
| `take_offers` | array of strings | no | `[]` | Job pools this role can take work from. |

Which role is the orchestrator, for a file `validate` rejects: the first role with `start = true`; if none has it, the role named `orchestrator`; otherwise the first role.

### Declaring the agent behind a launcher command

The deck identifies the agent from the start of `command`: `claude --model opus`, `/usr/local/bin/codex`, `env FOO=1 codex` and `sh -c 'codex …'` are recognised. It cannot see what a launcher starts: `devbox run -- codex`, `mise exec -- codex`, `nix develop -c codex`, `make codex`, `./run-codex.sh`.

A role whose agent the deck cannot identify:

- shows **No agent** and no status on its TUI card, and a blank **CLI** column in the desktop app. A Codex role behind a launcher stays blank until its first task starts. A Claude Code role behind a launcher identifies itself when it starts, so it looks normal;
- can lack the agent-specific doorbell guards and status detection until its integration identifies itself. Native inbox wake depends on that integration, so a launcher does not by itself prevent native delivery.

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
2. The deck writes the worker's task file, `.dot-agent-deck/worker-task-<role>.md` in the worker's directory: the role's `prompt_template`, then `## Task`, then the task. It queues an inbox message pointing the worker to that file.
3. The worker's integration receives and acknowledges the message. The worker does the work and runs `dot-agent-deck work-done --task-file <file>`.
4. The deck saves the report to `.dot-agent-deck/work-done-<role>.md` and puts a message in the orchestrator's inbox.

The deck teaches the orchestrator and the workers these commands when they start, so a `prompt_template` only needs to describe your workflow.

The orchestrator can delegate one task to several roles at once (`--to reviewer --to auditor`). Each receives its own inbox message and reports back separately.

### Commands

These commands work only from a pane the deck started, which sets `DOT_AGENT_DECK_PANE_ID` and the other variables they need. Run anywhere else, `delegate`, `work-done`, `pane restart` and `pane spawn` print `Error: DOT_AGENT_DECK_PANE_ID environment variable not set.` and exit non-zero; `ack` prints a message and exits 0. `delegate`, `pane restart` and `pane spawn` are further limited to the orchestrator's pane.

| Command | Who runs it | What it does |
|---|---|---|
| `dot-agent-deck delegate --to <role> [--to <role> …] (--task <text> \| --task-file <path>) [--supersede]` | orchestrator | Sends a task to one or more workers. `--task-file -` reads the task from stdin. `--task-file` is the safe choice for text with quotes, backticks, `$` or newlines; the file must be a regular file of at most 1 MiB. `--supersede` sends even to a worker that still owes a `work-done` ([One task per worker at a time](#one-task-per-worker-at-a-time)). |
| `dot-agent-deck work-done (--task <text> \| --task-file <path>) [--done]` | worker, or orchestrator with `--done` | Reports a finished task to the orchestrator. The orchestrator uses `--done` to mark the whole orchestration complete; in a dispatched unit, that reports back to the dispatcher ([Dispatcher Mode](dispatcher-mode.md)). |
| `dot-agent-deck ack <message-id>` | worker or agent integration | Acknowledges a message. The installed integration normally does this itself. Always exits 0. |
| `dot-agent-deck inbox list [--all] [--json]` | agent | Lists its inbox messages and their delivery states. An orchestrator can use `--all` to inspect its orchestration's inboxes. |
| `dot-agent-deck inbox fetch [--json]` | agent | Reads and acknowledges its waiting messages. Run inside the recipient's deck pane; fetching changes delivery state. |
| `dot-agent-deck pane restart <role> [--force]` | orchestrator | Replaces the worker's agent with a fresh one and drops the task it owed. Without `--force`, only a worker whose agent has exited (crashed or finished) is restarted. |
| `dot-agent-deck pane spawn <role>` | orchestrator | Starts a role that is in `.dot-agent-deck.toml` but not running in this orchestration, for example one added to the file after the orchestration started, or one whose pane was closed. Refused for a role that is already running and for the orchestrator role. |

`delegate` exit status and output:

| Outcome | Exit status | Printed on stderr |
|---|---|---|
| Accepted for every named role | 0 | nothing; check the inbox badge for delivery. |
| Accepted for some roles, not others | 0 | `Warning: …` naming the roles that were missed and why. Re-send only to those roles; repeating the whole command gives the accepted roles the task twice. |
| Accepted for no role | non-zero | `Error: delegate from pane … reached no worker for role(s): …`, or `… was NOT sent: every worker it reached still owes a work-done …` |
| Refused: the caller is not the orchestrator | non-zero | `Error: delegate from pane … failed: pane … is the \`<role>\` role, not this orchestration's orchestrator, so it may not delegate.` |
| Daemon not reachable | non-zero | `Error: could not reach the dot-agent-deck daemon socket, …` |

A role "reaches no worker" when it is not in the file, when it is the orchestrator itself, when its pane was closed, or when it was renamed or added after the orchestration started.

`work-done` exits 0 when the daemon accepted the report (or gave no reply it could read), and non-zero with `Error: the daemon did not accept this …` when it refused it, or `Failed to send work-done signal to daemon socket.` when no daemon answered.

### One task per worker at a time

A worker that has been given a task is busy until it sends `work-done`, and until then `delegate` refuses to give it another. What counts is whether the worker has reported, not its status: a worker's card (TUI) or row (desktop) can read idle while the worker still owes a `work-done`. A worker whose agent exited without reporting is still busy, until a different agent takes over its pane: a task the exited agent had received does not make the new agent busy.

When the earlier task is not coming back:

- **`delegate --supersede`** queues the new task and retires an earlier task that has not been acknowledged. On a `clear = false` worker, work it already received can finish and a late `work-done` for that earlier task still counts. On a `clear = true` worker the agent is restarted, so its earlier work is lost.
- **`pane restart <role>`** replaces the worker's agent and drops the task it owed, together with its pending [idle-worker reports](idle-workers-and-notifications.md).
- **Wait.** Seven days after a task was sent, the deck stops counting it.

If the orchestrator's own agent was replaced since it delegated, the new orchestrator is not blocked by the old one's tasks; `delegate` says which earlier task it superseded.

### What `clear` does to delivery

With `clear = true` (the default), the deck replaces the worker's agent in the same pane and queues the task for the replacement. Its card in the TUI or row in the desktop app keeps the same name, and its previous conversation is gone. With `clear = false`, the task goes to the running agent's inbox and the worker keeps its conversation. A `clear = true` worker whose agent has exited is started again on its next task; restart a `clear = false` worker before delegating to it again.

If the replacement cannot start, or exits before taking the task, the orchestrator is told. See [A delegated worker never came up](#a-delegated-worker-never-came-up).

### Delivery in the TUI and desktop app

Tasks, worker reports, dispatch results, scheduled prompts and deck notices wait in the recipient's inbox. The installed hooks, plugin or extension hand them to the agent and acknowledge receipt. A successful `delegate` means the task was accepted; delivery and finishing the work are separate steps.

Both clients show `✉N queued` while messages await acknowledgement and `✉ delivered` after receipt when nothing remains pending. A narrow TUI card shortens the badge to `✉N`. If messages wait longer than a wake-up normally takes while the agent is not working, both clients show `✉N waiting to wake: send it a prompt`. That happens when nothing the deck can do wakes the agent, for example when a doorbell is held by a draft you left in the input box or by a screen the deck does not recognise. Send the agent any prompt, or clear your draft: the agent takes its messages at the start or end of its next turn, and the badge goes back to `✉N queued` or `✉ delivered`. If a message cannot be delivered, the recipient shows **Error** (desktop **error**) and a `not delivered: …` notice. Delivered means the agent received the message; read its `work-done` report to judge the result.

Long turns affect delivery the same way in the TUI and the desktop app:

- When OpenCode or Pi keeps a message queued behind a busy turn, the message waits for that turn to end. The deck does not report it as not delivered during this wait, and OpenCode does not receive another copy. Claude Code has no way to confirm this wait: a message waiting behind a turn longer than about five minutes can be reported not delivered before it arrives; the orchestrator is then told it arrived.
- If the deck reported a task as not delivered and the agent takes it after all, the orchestrator is told that the task arrived, and the agent's TUI card or desktop tile leaves its delivery error state once no other delivery error remains.
- If the orchestrator was told a task has not reached its worker and the worker then picks it up, the orchestrator is told the task arrived after all.
- After a direct message has been held for an hour, the usual delivery limit and not-delivered notices apply again. An offered job waits for up to its claim time or one hour, whichever is shorter; see [Announcements and pooled jobs](#announcements-and-pooled-jobs).

**TUI:**

![Planner and builder working in demo-loop, each with one queued inbox message beside its role card](/img/agent-messaging-tui.png)

**Desktop:**

The Daemons view below has experimental features enabled. The inbox badge also appears in an agent's open pane.

![The same planner and builder working in demo-loop, each with one queued inbox message on its tile](/img/agent-messaging-desktop.png)

The desktop app's handoff rail shows `delegated · queued`, `delivered · working`, `work-done` or `not delivered` for worker handoffs. The rail is a desktop surface; TUI users follow the cards and the orchestrator's pane.

In the recipient's pane, deck messages show a `Sent by:` line. Reports from other agents are marked UNTRUSTED: read them as information. After refreshing Claude Code's hooks, native messages show `✉ dot-agent-deck message` in its pane. Codex's turn-end hand-over can appear as **Blocked by hook** with an explanation of the deck message; this is how the next task reaches Codex.

A long message can arrive as an excerpt naming a file with its full content, usually in the project's `.dot-agent-deck/` directory. Read that file to get the rest of the task or report. If the deck cannot save the full content, it refuses the message and reports the problem to the sender.

The deck removes these files itself, on the same schedule whether you watch the agents in the TUI or the desktop app:

- While the message waits to be received, its file stays.
- A delegated task's file stays until the worker reports `work-done`, a newer task replaces it, or the worker exits.
- After that, and for any other message once it has been received or dropped, the file is kept for about a day.
- When many long messages pile up, the deck removes the oldest of these files sooner, but not within an hour of the message being received or the task ending.
- Restarting the deck keeps counting from when the message was received. A file the deck was still keeping for a waiting message or an unfinished task when it stopped is kept for about a day from the restart.

Copy anything you want to keep out of these files into your own project files.

### Which agents wake for messages

The delivery behavior is the same whether you watch the pane in the TUI or the desktop app. Run `dot-agent-deck hooks install` after upgrading the deck or an agent, then restart the agent to load the refreshed integration.

| Agent | What happens when a message arrives | Fallback or limitation |
|---|---|---|
| Claude Code | Native wake is enabled from version 2.1.296 after installing the integration. | A message waiting behind a long turn can be reported not delivered before it arrives; the orchestrator is then told it arrived. Claude Code cannot confirm that it is keeping a queued message during the turn, so a job not claimed within the deck's wait can go to the next worker while the Claude worker is still busy. Older or unrecognised versions get the doorbell line without a native-wake delay. Upgrade Claude Code and re-run `hooks install`, then restart Claude Code to enable native wake. |
| Codex | The doorbell wakes an idle agent; hooks hand messages over at turn end when it is busy. | With untrusted hooks, the model runs `inbox fetch` itself and needs `network_access = true`. If a future Codex UI is not recognised, the idle doorbell waits and the card shows `waiting to wake`; send the first prompt yourself so turn-end hand-over can take over. |
| OpenCode | The full TUI receives messages through its TUI plugin. `--mini` receives messages natively and gets a redraw so they become visible; a partial frame can remain briefly. | If the deck cannot update `tui.json`, it uses the doorbell; add the plugin entry by hand as described below. A `/models` choice before the first prompt is not readable by the integration; launch with `--model` to choose that first message's model. `attach`, `acp`, `web`, `pr` and `github` have not been measured for this delivery path; expect the doorbell. |
| Pi | Native delivery works while idle and queues follow-ups while busy. | The extension stops trying to connect when Pi runs outside a deck pane. Restart Pi inside the deck to receive deck messages. |
| Devin | Uses the doorbell. | No native inbox wake integration is provided. |

When OpenCode's installation reports that it left `tui.json` unchanged, use the exact path it prints in that file's `plugin` list, for example:

```json
"plugin": ["/home/you/.config/opencode/dot-agent-deck-tui.js"]
```

Preserve your other plugin entries, then restart OpenCode. The installer prints the path for the configuration directory it found; yours may differ.

#### A lost task is re-sent into the same worker

A message that was taken but never acknowledged becomes available again. Its id stays the same so an agent can recognise a repeat. The deck stops retrying after the delivery limit and shows `not delivered`; a delegated task that remains unacknowledged also produces a notice for its orchestrator. Restarting an agent does not move an already bound message to the replacement: delegate the task again when needed.

In both clients, replacing an earlier task with a new delegation or restarting the worker stops delivery notices for the superseded task. A late `work-done` for a task the worker acknowledged leaves a newer task it has not taken queued, and the orchestrator is still told if that newer task does not reach the worker. A worker that reports `work-done` twice for one task, though, can make the deck drop a newer task it has not taken yet, without telling the orchestrator; delegate that task again if the worker never picked it up. If the worker takes a task directly from its file, it should acknowledge the message id too: otherwise its completion can stop delivery notices for other tasks waiting in its inbox, or, if it acknowledged an earlier task it never finished, the deck can report the task it took from the file as not delivered and hand it to the worker again.

### A deck prompt waits while you have an unsent draft

Native inbox delivery does not type a task into your input box. When native wake is unavailable, the deck sends a fixed doorbell line only when the agent is idle and your input is clear:

```text
dot-agent-deck: message m-… waiting, run: dot-agent-deck inbox fetch
```

The message stays queued while you have an unsent draft or the agent needs a permission or dialog answer. Send or clear your draft, or answer the prompt, to let the doorbell proceed. In either client, no timeout forces the doorbell over your draft. The installed integration can replace this line with the message; otherwise the agent runs the fetch command.

### Inspect delivery

Run `dot-agent-deck daemon status` in your terminal to see **INBOX** and **WAKE**. `2q 1f` means two queued messages and one taken but not acknowledged; a dash means none pending. **WAKE** reads `native`, `turn-end`, `doorbell` or `none`; `none` means the deck has neither an armed integration nor a reported idle moment for the doorbell.

Inside the recipient's deck pane, `dot-agent-deck inbox list` shows its messages and states; recent delivered and undeliverable messages are included while retained. An orchestrator can add `--all` to inspect its orchestration's inboxes, or any agent can use `--json` for structured output. `dot-agent-deck inbox fetch` reads and acknowledges the waiting messages, so use `list` when you only want to inspect them. These commands work for agents in either client.

Recent delivery history is temporary: older entries and message bodies expire, and closing a pane removes its delivery history. Keep task results in your project files or saved reports when you need them later. Long subjects are shortened in the inbox listing and card labels.

### Announcements and pooled jobs

Agents can announce progress to subscribed roles instead of sending a separate message to each one:

```bash
dot-agent-deck inbox publish review-ready --text 'The signup change is ready for review.'
dot-agent-deck inbox publish progress --file .dot-agent-deck/progress.md
```

An announcement reaches subscribers in the sender's orchestration, or its dispatch tree when dispatched. The sender does not receive its own announcement. In either client, the recipient's pane labels the text as another agent's UNTRUSTED announcement: it is information to assess, rather than an instruction from the user. The command prints how many subscribers it reached and identifies recipients whose inboxes were full.

An orchestrator, or an agent that dispatched a unit, can offer a job to a pool. Configure workers with `take_offers` and topics with `subscribe`; see [Configuration](configuration.md#subscribe-to-announcements-and-job-pools).

```bash
dot-agent-deck inbox offer reviews --file .dot-agent-deck/review-task.md --claim-ttl 30m
```

One eligible worker receives the job at a time. Receiving and acknowledging it claims the work; the worker's pane shows the orchestrator's instructions and the commands to finish or hand it back:

```bash
dot-agent-deck inbox complete <id>
dot-agent-deck inbox release <id>
```

The default claim lasts 30 minutes; `--claim-ttl` accepts durations from one minute to 24 hours, such as `90s` or `1h30m`. If the worker releases the job, its claim expires, or it exits, the job returns to the pool for another worker. Once a claim has expired, the worker can no longer complete or release that job: `inbox complete` and `inbox release` are refused, because the job has gone back to its pool and may already be with another worker. A worker that never picks up its assigned job also loses that assignment after five minutes by default. The offerer receives notices when a job is claimed, completed, its claim expires or delivery attempts are exhausted without completion.

In both the TUI and the desktop app, if OpenCode or Pi is busy with a long turn when a job arrives, the job waits for that agent and is not offered to another worker during that wait, preventing duplicate work. It can wait for up to the job's claim time or one hour, whichever is shorter. For Claude Code, a job not claimed within the deck's wait still goes to the next worker.

In the TUI and desktop app, a claimed job stays counted as a taken message until completed or handed back. The desktop evidence rail records **Job handed back to its pool** when an assignment returns. The CLI commands work in agent panes in either client. Both `publish` and `offer` accept exactly one of `--text` or `--file`; `--file -` reads standard input, with a 1 MiB body limit.

If a command is refused, check its explanation: offers require an orchestrator or dispatching pane, topic and pool names must follow the [configuration rules](configuration.md#subscribe-to-announcements-and-job-pools), and publish and offer share a limit of 60 requests per minute per agent. For a full inbox, let the recipient consume its queued work before trying again.

### Setting the delivery variables

Delivery normally needs no timing settings. The waiting-for-input notice is configurable in [Idle Workers & Notifications](idle-workers-and-notifications.md#tune-or-turn-off-the-other-reports). Daemon settings must be present when it starts; a variable on a later client command does not change an already running daemon. See [Configuration](configuration.md#daemon-behaviour).


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
| `orchestrator-context-<id>.md` | when an orchestration starts, and again each time the orchestrator's conversation is compacted or cleared while the orchestration is open in the TUI (the desktop app does not rewrite it) | What the orchestrator is told: its `prompt_template`, the available workers, how to delegate, and its task if it was given one. |
| `orchestrator-context.md` | when an orchestration starts | A copy of a recent orchestrator context, kept for compatibility. |
| `worker-task-<role>.md` | each delegation | The task for that role, overwritten by the next one. |
| `work-done-<role>.md` | each `work-done` for a delegated task | The worker's last report. |
| other `*.md` files | when an agent writes them | Longer context an orchestrator passes by file. |

When it writes an orchestrator context, the deck deletes `.md` files in this directory, other than `orchestrator-context.md` and the saved copies of long messages (`inbox-message-*.md`), that were last modified more than 14 days ago. The saved copies follow their own schedule, described in [Delivery in the TUI and desktop app](#delivery-in-the-tui-and-desktop-app). `DOT_AGENT_DECK_COORDINATION_RETENTION_DAYS` changes the number of days, and `0` turns the clean-up off. Do not keep your own files there.

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

Check the inbox badge and run `dot-agent-deck daemon status`. If the message stays queued, see [An agent shows the doorbell instead of waking](troubleshooting.md#an-agent-shows-the-doorbell-instead-of-waking). If the card says `not delivered`, see [A message says not delivered](troubleshooting.md#a-message-says-not-delivered). A delivered task whose agent does no work needs its pane inspected; receipt does not mean completion.

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

Reports wait in the orchestrator's inbox and appear in its pane when taken. If that agent exits, its pending messages are not transferred to a replacement; a delegated report is also saved to `.dot-agent-deck/work-done-<role>.md`. If the daemon log shows `failed to write work-done summary`, that file is from an earlier task; the orchestrator then receives the report inline, on one line, without its Markdown formatting.

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
