# Idle Workers & Notifications

When a worker in an [orchestration](orchestration.md) gets stuck (it stops responding, sits at a prompt, runs out of provider credits, or exits without finishing its task), the deck sends a report to the orchestrator. What the orchestrator does next, such as chasing the worker, handing the task to another role, or telling you, depends on the instructions you give it.

Reports are sent only inside an orchestration: an orchestration tab in the TUI, an **ORCHESTRATION** group on the desktop app's Dashboard, or an orchestration started by a dispatcher or a schedule. A standalone agent and a single-agent schedule get none. The deck does not message you itself; to be notified on your phone, see [Get notified when a run needs you](#get-notified-when-a-run-needs-you).

## The reports

Each report is typed into the orchestrator's pane and submitted as a new message, in both clients. It says it is a `dot-agent-deck daemon report` so the orchestrator does not mistake it for you, and it asks the orchestrator to decide what to do.

| Report starts with | Sent when | Setting |
|---|---|---|
| `A delegated worker has not responded with work-done` | The worker has not run `work-done` within `worker_response_timeout_minutes` (default 120) of receiving its task. | [`worker_response_timeout_minutes`](#change-how-long-a-worker-may-take) |
| `⚠ delegated worker went quiet` | The worker showed no sign of starting its task: no event from its agent and no `ack`. Sent 30 seconds after delivery (or after `worker_response_timeout_minutes`, if shorter), or, when the deck is [re-sending the task](orchestration.md#a-lost-task-is-re-sent-into-the-same-worker), after the re-sends run out: about 3 minutes 40 seconds with the default schedule. | `DOT_AGENT_DECK_DELEGATE_NO_EVENT_WINDOW_MS` |
| `A delegated worker is waiting for input` | A worker that has not finished its task has been waiting for input for 30 seconds. | `DOT_AGENT_DECK_WAITING_NOTICE_DEBOUNCE_MS` |
| `⚠ delegated worker exited without work-done` | The worker's agent exited before it reported. | none |
| `⚠ delegated worker never came up` | A `clear = true` worker restarted for a new task exited before it took the task, so the task was not delivered. | none |
| `⚠ delegated worker respawn failed` | A `clear = true` worker could not be restarted at all, usually because the role's `command` fails, so the task was not delivered. | none |
| `⚠ delegated worker blocked by a provider usage limit` | A worker that has not finished its task shows **Blocked**: its provider's usage limit or credits ran out. | none |

What each report means for the orchestrator's next step:

- **Went quiet** and **waiting for input** include the last lines the worker's pane shows (the agent idle at its input, a permission prompt, a login screen), so the orchestrator can tell "stuck at a prompt" from "never got the task". A worker that ran `ack`, or shows **Blocked**, is not reported as quiet.
- **Waiting for input** depends on the agent. For Claude Code it means a permission prompt; a Claude Code worker that asks a question in plain text simply ends its turn, so only the timeout report covers it. [Session Management](session-management.md) lists what each status means per agent.
- **Exited:** the worker still counts as busy with its task. Run `dot-agent-deck pane restart <role>`, or delegate with `--supersede`, before giving that role new work ([One task per worker at a time](orchestration.md#one-task-per-worker-at-a-time)). A `work-done` that arrives just after this report is to be trusted over it.
- **Never came up** and **respawn failed:** re-delegating to the same role runs the same `command` and fails the same way until the role's configuration is fixed, so the orchestrator should tell you or give the task to another role. See [A delegated worker never came up](orchestration.md#a-delegated-worker-never-came-up).
- **Blocked:** a usage limit can clear on its own. The report asks the orchestrator to check the worker's card first: if it still shows Blocked, give the task to another role or tell you; if the worker is working again, keep waiting.

The exited, never-came-up, respawn-failed and blocked reports name the worker by its pane id, not by its role. The pane id usually contains a form of the orchestration's name. The [daemon log](troubleshooting.md#enabling-debug-logs) line written beside each report names the role and, where there is one, the error.

If you are part-way through typing in the orchestrator's pane, a report waits until you send or clear your text, for at most 60 seconds; see [A deck prompt waits while you have an unsent draft](orchestration.md#a-deck-prompt-waits-while-you-have-an-unsent-draft).

## Change how long a worker may take

The idle-worker report (`A delegated worker has not responded with work-done`) is controlled by `worker_response_timeout_minutes` in the `.dot-agent-deck.toml` that defines the orchestration.

| Value | Effect |
|---|---|
| not set | `120` minutes |
| `0` | The idle-worker report is off. So is the went-quiet report, unless `DOT_AGENT_DECK_DELEGATE_NO_EVENT_WINDOW_MS` is set to a non-zero value. `work-done` still reaches the orchestrator. |
| `1`–`10080` | That many minutes (up to seven days). |
| anything larger | Ignored: `120` minutes is used and the daemon log records a warning. |

The deck reads the value from the orchestration's directory. A worker running in another directory (a dispatched unit's worktree, for example) uses the orchestration's file too; the worker's own `.dot-agent-deck.toml` is read only when the orchestration's is missing or cannot be parsed. A change applies to the next task the orchestrator delegates; nothing needs restarting.

### Put the key above the first table header

`worker_response_timeout_minutes` is a top-level key, so it must come **before** the first `[...]` or `[[...]]` header in the file. Written further down, TOML makes it part of the table above it, where the deck ignores it: the file still loads, `dot-agent-deck validate` still prints `Config is valid.`, and the timeout stays at 120 minutes.

Wrong, at the end of the file, where it belongs to the last role:

```toml
[[orchestrations]]
name = "my-project"

[[orchestrations.roles]]
name = "orchestrator"
command = "claude"
start = true

worker_response_timeout_minutes = 45
```

Right, above every table header:

```toml
worker_response_timeout_minutes = 45

[[orchestrations]]
name = "my-project"

[[orchestrations.roles]]
name = "orchestrator"
command = "claude"
start = true
```

Comments and blank lines before it are fine. **Check:** the first non-comment, non-blank line of the file is the `worker_response_timeout_minutes = …` line (or another top-level key), not a `[`-header.

## Tune or turn off the other reports

Two environment variables control the went-quiet and waiting reports. They are read by the daemon, so set them on the command that starts the daemon and restart a daemon that is already running; see [Setting the delivery variables](orchestration.md#setting-the-delivery-variables).

| Variable | Default | Values |
|---|---|---|
| `DOT_AGENT_DECK_DELEGATE_NO_EVENT_WINDOW_MS` | 30 seconds, or `worker_response_timeout_minutes` if shorter; none when that is `0` | Milliseconds, at most `30000` (larger values are capped). `0` turns the went-quiet report off. A non-zero value turns it on even when `worker_response_timeout_minutes = 0`. |
| `DOT_AGENT_DECK_WAITING_NOTICE_DEBOUNCE_MS` | `30000` | Milliseconds a worker must stay waiting before it is reported, at most `600000` (larger values are capped). `0` turns the waiting report off. |

The waiting report is sent at most once per wait, and at most once per worker every four debounce windows (two minutes by default); a worker still waiting when that interval ends is reported then. A prompt you answer yourself within the debounce window produces no report.

```bash
DOT_AGENT_DECK_DELEGATE_NO_EVENT_WINDOW_MS=0 DOT_AGENT_DECK_WAITING_NOTICE_DEBOUNCE_MS=0 dot-agent-deck
```

## What to expect

- **One idle-worker report per task.** A worker stuck for a day produces one report, not a series.
- **A `work-done` cancels the pending reports for that task**, even one that arrives a second before the deadline.
- **Closing the worker's pane, or `dot-agent-deck pane restart <role>`, cancels them too.** A task that was still being handed to the worker when you restarted it goes to the replacement and is watched as usual.
- **Reports go only to the orchestrator that delegated the task.** If that orchestrator is gone when a report is due (its pane closed, or a different agent now runs in it), the report is dropped.
- **Reports do not act on the worker.** Sending a report does not stop, restart or interrupt the worker; the orchestrator decides what happens next. (Re-sending a lost task, which the deck does before the went-quiet report, does type into the worker's pane; see [A lost task is re-sent into the same worker](orchestration.md#a-lost-task-is-re-sent-into-the-same-worker).)
- **Restarting the daemon forgets the tasks in progress.** Tasks delegated before the restart are not reported; tasks delegated after it are.
- **The timeout counts time, not activity.** A worker busy on a long task is still reported when the timeout passes; the orchestrator can ignore it. That is why the default is two hours.
- **Two overlapping tasks for one worker** (possible only with `delegate --supersede`) can occasionally produce one report too many, or leave one task unreported.
- **Waiting reports can outlive the prompt.** A wait raised by a Claude Code or Codex subagent ends when that subagent stops or fails: the worker's TUI card leaves **Needs Input** (its desktop row leaves **needs input**), and a waiting report not yet sent is cancelled. One already sent stays sent, so the orchestrator can receive a report about a prompt that is gone.
- **The deck sends no report about the orchestrator itself.** If the orchestrator's agent crashes, or the orchestration fails before any agent starts, nobody receives a report.

## Get notified when a run needs you

To be told on your phone when a run needs you, give the orchestrator a way to send a message (an MCP server for your chat app, or a script that posts to one) and say in its `prompt_template` when to use it: when one of these reports arrives, and wherever your workflow stops to wait for you. For example:

```toml
[[orchestrations.roles]]
name = "orchestrator"
command = "claude"
start = true
prompt_template = """
…your workflow…

Notifications: when you receive a dot-agent-deck daemon report, or when you stop to wait for the user,
send one message with the notify tool. Start it with the repository name and the task. Do not wait for,
check, or retry the delivery.
"""
```

What works well:

- **Let only the orchestrator send messages.** Have workers put their questions in their `work-done` report instead, so only one agent needs the messaging tool.
- **Notify only where you may have walked away**, and start each message with the repository and task.
- **Send and carry on.** A failed send should cost one notification, not the run.
- **Keep the channel's credentials and chat ids in the agent's own configuration**, out of the repository. The deck does not read or store them.

An instruction in a prompt can be lost when a long session is compacted, so a notification you asked for may not be sent. The deck's own reports do not depend on the prompt and arrive when they are due.

**Check:** ask the orchestrator, in its pane, to send a test notification with the tool you gave it, and confirm the message arrives.

## See also

- [Orchestration](orchestration.md): roles, delegation and `work-done`
- [Configuration](configuration.md): the rest of `.dot-agent-deck.toml` and the environment variables
- [Schedules](scheduled-tasks.md): runs that start on a timer and finish while you are away
- [Troubleshooting](troubleshooting.md): the daemon log and other problems
