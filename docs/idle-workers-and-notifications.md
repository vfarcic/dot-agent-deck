---
title: Idle Workers & Notifications
---

# Idle Workers & Notifications

If a worker in an [orchestration](orchestration.md) gets stuck — it stops responding, sits at a prompt, or exits without finishing its task — the deck tells the orchestrator, so the orchestrator can chase the worker, hand the task to another role, or let you know. What it does with that news is up to the instructions you give it.

This only happens inside an orchestration — an orchestration tab in the TUI, an **ORCHESTRATION** group on the desktop app's Dashboard. A plain agent pane and a single-agent schedule never get these reports.

The deck does not message you itself. To hear about a stuck run on your phone, have the orchestrator send the message — see [Getting these moments to you](#getting-these-moments-to-you).

## The reports

Each report appears in the orchestrator's pane as a new message — the same in the TUI and the desktop app — marked `dot-agent-deck daemon report` so the orchestrator knows it comes from the deck and not from you, and asks the orchestrator to decide what to do next.

| Report starts with | When you get it | How to tune it |
|---|---|---|
| `A delegated worker has not responded with work-done` | A worker has not sent `work-done` within `worker_response_timeout_minutes` (default 120) of receiving its task | [Configuring the timeout](#configuring-the-timeout) |
| `⚠ delegated worker went quiet` | A worker showed no sign of starting its task within 30 seconds of receiving it | `DOT_AGENT_DECK_DELEGATE_NO_EVENT_WINDOW_MS` |
| `A delegated worker is waiting for input` | A worker that has not finished its task has been waiting for input for 30 seconds | `DOT_AGENT_DECK_WAITING_NOTICE_DEBOUNCE_MS` |
| `⚠ delegated worker exited without work-done` | A worker's agent exited before it reported | — |
| `⚠ delegated worker never came up` | A `clear = true` worker that was restarted for a new task died before it could take the task, so the task was not delivered | — |
| `⚠ delegated worker respawn failed` | A `clear = true` worker could not be restarted at all — usually because the role's `command` cannot be started — so the task was not delivered | — |
| `⚠ delegated worker blocked by a provider usage limit` | A worker that has not finished its task shows **Blocked** because its provider's usage limit or credits ran out | — |

Good to know:

- **The went-quiet and waiting reports include what the worker's pane is showing** — the agent idle at its input, a permission prompt, a login screen — so the orchestrator can tell "stuck on a prompt" from "never got the task".
- **"Waiting for input" depends on the agent.** For Claude Code it means a permission prompt. A Claude Code worker that asks you a question in plain text simply finishes its turn, so only the first report (the timeout) covers it. [Session management](session-management.md) lists which agents show Blocked.
- **A wait raised by a Claude Code or Codex subagent ends when that subagent stops or fails**, since its prompt goes with it: the worker's card in the TUI leaves **Needs Input** (in the desktop app its row leaves **WAITING**, unless the agent is now idle), and a waiting report not yet sent is cancelled. One already sent stays sent, so the orchestrator can receive a waiting report about a prompt that is no longer there.
- **A worker that sent `work-done` while still at a prompt and is then given a new task** can be reported as waiting again, for the new task, no sooner than two minutes after its previous report.
- **After an "exited" report, the worker still counts as busy with its task**, so run `dot-agent-deck pane restart <role>` (or use `delegate --supersede`) before giving that role new work; see [One task per worker at a time](orchestration.md#one-task-per-worker-at-a-time). A `work-done` that arrives just after this report is to be trusted over it.
- **The "blocked" report asks the orchestrator to look at the worker's card first**, because a usage limit can clear on its own: reassign or tell you if the card still shows Blocked, keep waiting if the worker is working again.
- **After a "respawn failed" report, re-delegating to that role fails the same way** until the role's configuration is fixed, so the orchestrator should tell you or reassign the task.
- **The exited, never-came-up, respawn-failed and blocked reports name the worker by its pane, never by its role.** The pane id can include the orchestration's name from your project configuration, in a sanitised form ([#1380](https://github.com/vfarcic/dot-agent-deck/issues/1380) tracks that). The [daemon log](troubleshooting.md#enabling-debug-logs) line next to each names the role and, where there is one, the underlying error. What to do when you see one yourself is under [A delegated worker never came up](orchestration.md#a-delegated-worker-never-came-up).
- **If you are part-way through typing in the orchestrator's pane**, in the TUI or the desktop app, a report waits until you send or clear what you typed — see [A deck prompt waits while you have an unsent draft](orchestration.md#a-deck-prompt-waits-while-you-have-an-unsent-draft).

## Configuring the timeout

`worker_response_timeout_minutes` is a **top-level key** in the `.dot-agent-deck.toml` that defines the orchestration. (Workers that run in a separate clone or worktree take it from that file too, not from their own; the deck reads the worker directory's `.dot-agent-deck.toml` only when the orchestration's is missing or cannot be read.)

| | |
|---|---|
| **Default** | `120` minutes |
| **Accepted range** | `1`–`10080` (one minute to seven days) |
| **`0`** | **Turns the idle-worker report off** — and the went-quiet report too, unless you set its window explicitly (see [Tuning the other reports](#tuning-the-other-reports)) |
| **Out of range** | Uses the **default**, not the nearest bound — `20000` gives you 120 minutes, and a warning in the [daemon log](troubleshooting.md#enabling-debug-logs) |

`0` means **off**, not "report immediately". It only turns reports off: a `work-done` still reaches the orchestrator as usual.

A change applies to the next task the orchestrator delegates. You do not need to restart anything.

### Where the key goes — read this before you file a bug

> **A misplaced `worker_response_timeout_minutes` is silently ignored.** It must appear **above the first table header** — above the first `[[orchestrations]]` (or any other table header) in the file. Added at the end of the file, it becomes part of whatever table came last, where it does nothing. The file still loads, `dot-agent-deck validate` still says `Config is valid.`, and the timeout stays at 120 minutes.

This is the most likely reason for "I set the timeout and nothing changed":

```toml
# WRONG — at the end of the file, this belongs to the last [[orchestrations.roles]]
# table and is ignored.
[[orchestrations]]
name = "my-project"

[[orchestrations.roles]]
name = "orchestrator"
command = "claude"
start = true

worker_response_timeout_minutes = 45
```

```toml
# RIGHT — a top-level key, above every table header in the file.
worker_response_timeout_minutes = 45

[[orchestrations]]
name = "my-project"

[[orchestrations.roles]]
name = "orchestrator"
command = "claude"
start = true
```

Comments and blank lines before the first table are fine. If your file starts with `[[orchestrations]]` on line one, the key goes on line one and `[[orchestrations]]` moves down.

## Tuning the other reports

Both of these are environment variables, set on the command that starts the deck.

**The went-quiet report** waits 30 seconds, or `worker_response_timeout_minutes` if that is shorter. Set `DOT_AGENT_DECK_DELEGATE_NO_EVENT_WINDOW_MS` to shorten the wait, or to `0` to turn this report off; values above 30 seconds are capped. Turning it off leaves the idle-worker report on, and setting a window turns this report on even when `worker_response_timeout_minutes = 0`.

```bash
DOT_AGENT_DECK_DELEGATE_NO_EVENT_WINDOW_MS=0 dot-agent-deck
```

**The waiting-for-input report** waits 30 seconds, so a prompt you answer yourself within that time produces nothing. You get at most one report per wait, and at most one per worker every two minutes. Set `DOT_AGENT_DECK_WAITING_NOTICE_DEBOUNCE_MS` to change the 30 seconds (the two-minute spacing is always four times the value), or to `0` to turn this report off; values above ten minutes are capped. It does not depend on either setting above.

```bash
DOT_AGENT_DECK_WAITING_NOTICE_DEBOUNCE_MS=0 dot-agent-deck
```

## What to expect

- **One idle-worker report per task.** A run stuck for a day produces one report, not a stream of reminders.
- **A `work-done` cancels the pending reports for that task**, even one that arrives a second before the deadline.
- **Closing the worker's pane, or `dot-agent-deck pane restart <role>`, cancels them too.** The exception is a task that was still being handed to the worker when you restarted it: the replacement gets that task, and it is watched as usual.
- **Reports go only to the orchestrator that delegated the task.** If that orchestrator is gone when a report is due — its pane closed, or a different agent now runs in it — the report is dropped.
- **The deck never touches the worker.** No kill, no restart, no interrupt: the worker's pane stays exactly as it was, and what happens next is the orchestrator's decision.
- **Restarting the daemon forgets every task in progress.** Tasks delegated before the restart are never reported; tasks delegated afterwards are.
- **The timeout counts time, not activity.** A worker busy on a long task still gets a report when the timeout passes, which the orchestrator can ignore — that is why the default is two hours.
- **Two overlapping tasks for the same worker** (only possible with `delegate --supersede`) can occasionally produce one report too many, or leave one task unreported.
- **Nothing reports on the orchestrator itself.** If the orchestrator crashes, or the orchestration fails before any agent starts, nobody receives a report.

## Getting these moments to you

To be told on your phone when a run needs you, give the orchestrator a way to send a message — an MCP server for your chat app, or a script that posts to one — and say in its `prompt_template` when to use it: when one of these reports arrives, and wherever your workflow stops and waits for you. What works well:

- **Let only the orchestrator send messages.** Have workers return their questions through `work-done` instead of messaging you, so only one agent is ever waiting on your answer and only one needs the messaging tool.
- **Notify only where you may have walked away**, and start each message with the repository and task, so you know where to go.
- **Send and carry on.** The orchestrator should not wait for, check or retry a delivery; a failed send should cost you one notification, not the run.
- **Keep the channel's credentials and chat IDs in the agent's own configuration**, out of the repository. The deck never reads or stores them.

An instruction in a prompt can be forgotten when a long session is compacted, so a notification you asked for may not be sent. The deck's own reports are not affected: each arrives when it is due.

## See also

- [Orchestration](orchestration.md) — how delegation, `work-done` and roles work
- [Configuration](configuration.md) — the rest of `.dot-agent-deck.toml` and the global settings
- [Schedules](scheduled-tasks.md) — runs that start on a timer and finish while you are away
