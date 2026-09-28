---
title: Idle Workers & Notifications
---

# Idle Workers & Notifications

An orchestrator that has delegated work and is waiting for a `work-done` cannot notice that the worker is stuck — it gets no turn until the worker answers. So the deck watches every delegation for it, and when a worker stops making progress it sends the orchestrator a short **report** saying what happened. What happens next — notify you, chase the worker, re-delegate, or keep waiting — is up to the orchestrator's own instructions.

**The deck reports; it never notifies you.** There is no notification channel built into the deck and it holds no credentials. If you want these moments to reach your phone, instruct your orchestrator to send a message when it receives one (see [Getting these moments to you](#getting-these-moments-to-you)).

**This requires an [orchestration](orchestration.md).** Reports are about *delegations*, and a delegation only exists inside an orchestration tab — so a plain agent pane, a workspace mode, and a single-agent schedule never produce one, however long they run.

## The reports

Each report arrives in the orchestrator's pane as a turn of its own, names itself as a `dot-agent-deck daemon report` so the orchestrator does not mistake it for a message from you, and asks the orchestrator to decide how to proceed.

| Report starts with | When it is sent | How to tune it |
|---|---|---|
| `A delegated worker has not responded with work-done` | A worker has not sent `work-done` within `worker_response_timeout_minutes` (default 120) of being delegated to | [Configuring the timeout](#configuring-the-timeout) |
| `⚠ delegated worker went quiet` | A worker showed no sign of starting work within 30 seconds of receiving its task — for example, the task never reached an agent that was still starting up | `DOT_AGENT_DECK_DELEGATE_NO_EVENT_WINDOW_MS` |
| `A delegated worker is waiting for input` | A worker that still owes a `work-done` has been waiting for input for 30 seconds | `DOT_AGENT_DECK_WAITING_NOTICE_DEBOUNCE_MS` |
| `⚠ delegated worker exited without work-done` | A worker's process ended before it reported | — |
| `⚠ delegated worker never came up` | A `clear = true` replacement worker never started | — |
| `⚠ delegated worker blocked by a provider usage limit` | A worker that still owes a `work-done` turned **Blocked** because its provider's usage limit or credit pool ran out | — |

A few details worth knowing:

- **The went-quiet and waiting reports quote what the worker's pane is showing** — an agent idle at its own input, a permission prompt, an authentication screen — so the orchestrator can tell "blocked on a prompt" from "never got the task" without guessing.
- **"Waiting for input" depends on the agent.** For Claude Code it means a permission prompt. A Claude Code worker that asks its question in prose simply ends its turn and reports idle, which only the first report covers. See [Session management](session-management.md) for which agents report Blocked.
- **After an exited-worker report, the worker still owes its task**, so re-delegating to that role needs `dot-agent-deck pane restart <role>` (or `delegate --supersede`) first; see [One task per worker at a time](orchestration.md#one-task-per-worker-at-a-time).
- **The blocked report asks the orchestrator to check the worker's card first**, because a usage limit can clear: reassign or tell you if it still shows Blocked, keep waiting if it is working again.
- **One notice is not submitted as a turn:** `⚠ respawn failed for role …`, for a `clear = true` respawn that could not start the replacement at all, is written into the orchestrator's pane without an Enter, so an unattended orchestrator does not act on it by itself.
- If you are part-way through typing into the orchestrator's pane when a report arrives, it waits until you send or clear your draft, or until the draft-deferral cap passes — see [A deck prompt waits while you have an unsent draft](orchestration.md#a-deck-prompt-waits-while-you-have-an-unsent-draft).

## What the deck does not do

- **It does not notify anybody** — no email, no chat, no webhook, no push.
- **It holds no credentials.** If a message reaches your phone, it is because *your agent* sent it with *its own* configuration.
- **It does not decide.** Notify you, chase the worker, re-delegate, abandon the run, or keep waiting because you know the task is long — which one happens depends on your orchestrator's instructions.
- **It does not touch the worker.** No kill, no restart, no interrupt. The worker's pane is exactly as it was, and the orchestrator can look at it.

## Configuring the timeout

`worker_response_timeout_minutes` is a **top-level key** in your project's `.dot-agent-deck.toml`.

| | |
|---|---|
| **Default** | `120` minutes |
| **Accepted range** | `1`–`10080` (one minute to seven days) |
| **`0`** | **Disables the idle-worker report** — and the went-quiet report too, unless you set its window explicitly (see [Tuning the other reports](#tuning-the-other-reports)) |
| **Out of range** | Falls back to the **default**, not clamped to the nearest bound |

`0` means **off**, not "report immediately".

An out-of-range value is **replaced by the default**, so `worker_response_timeout_minutes = 20000` gives you 120 minutes and a warning in the daemon log — not seven days.

The value is read **per delegation**, from the `.dot-agent-deck.toml` in the orchestration's directory (falling back to the worker's, which can differ when workers run in clones or worktrees). Editing it takes effect on the next delegation — you do not need to restart the daemon or respawn the panes.

Setting it to `0` does not affect completion reporting: a `work-done` still reaches the orchestrator normally, and one that answers no delegation is still [labelled as unsolicited](orchestration.md#orchestrator-is-told-a-completion-was-unsolicited).

### Where the key goes — read this before you file a bug

> **A misplaced `worker_response_timeout_minutes` is silently ignored, and nothing will tell you.** It is a top-level scalar, so in TOML it must appear **above the first table header** — above the first `[[modes]]` or `[[orchestrations]]` in the file. Appended to the end of a config, it becomes a key of whatever table came last, where it means nothing. The config still parses, `dot-agent-deck validate` still says `Config is valid.` (unknown keys inside tables are accepted for forward compatibility), and your detector quietly keeps using the 120-minute default.

This is the single most likely reason for "I set the timeout and nothing changed", so it is worth seeing both shapes side by side.

```toml
# WRONG — appended at the end of the file. TOML reads this as
# orchestrations.roles.worker_response_timeout_minutes, which nothing looks at.
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

Comments and blank lines before the first table are fine; the rule is only about table headers. If your file starts with `[[modes]]` on line one, the key goes on line one and `[[modes]]` moves down.

## Tuning the other reports

**The went-quiet report** waits `worker_response_timeout_minutes` or 30 seconds, whichever is shorter. Set `DOT_AGENT_DECK_DELEGATE_NO_EVENT_WINDOW_MS` on the process that starts the deck to shorten it, or to `0` to turn this report off; values above 30 seconds are capped. Turning it off leaves the idle-worker report working, and setting a window explicitly turns it on even in a project with `worker_response_timeout_minutes = 0`.

```bash
DOT_AGENT_DECK_DELEGATE_NO_EVENT_WINDOW_MS=0 dot-agent-deck
```

**The waiting-for-input report** waits 30 seconds, so a prompt you clear yourself within that time produces nothing, and it is sent at most once per wait and at most once per worker every two minutes. Set `DOT_AGENT_DECK_WAITING_NOTICE_DEBOUNCE_MS` on the process that starts the deck to change the 30 seconds (the two-minute spacing scales with it, at four times the value), or to `0` to turn this report off. Values above ten minutes are capped. It is independent of both settings above.

```bash
DOT_AGENT_DECK_WAITING_NOTICE_DEBOUNCE_MS=0 dot-agent-deck
```

## What you can rely on

- **One idle-worker report per delegation.** A run stuck for a day produces one report, not a stream of nags.
- **A `work-done` cancels the idle-worker, went-quiet and waiting reports for that task.** You do not get a "worker is silent" report for a worker that answered, even one second before the deadline.
- **Closing the worker's pane cancels them too**, and so does `dot-agent-deck pane restart <role>` — except for a delegation that was still on its way to the pane when you restarted it, which is delivered to the replacement and still watched.
- **Those three reports only go to the orchestrator that delegated.** If that orchestrator is gone by the time a report is due — its pane closed, or a different agent now runs in it — the report is dropped rather than delivered to whoever is there.
- **A report changes nothing by itself.** It grants, cancels and reroutes nothing; the orchestrator decides.

## Limitations worth knowing

- **Restarting the daemon forgets every outstanding delegation.** Anything in flight at the restart will never be reported; delegations made afterwards are tracked normally.
- **The timeout measures elapsed time, not activity.** A legitimately long task produces one report you can read and discard, which is why the default is long.
- **Two overlapping delegations to the same worker** (which takes `delegate --supersede`) can occasionally produce one spurious report, or leave one delegation unreported, because a `work-done` is credited to the oldest one.
- **There is no report when the orchestrator itself is gone.** If the orchestrator crashed, or the orchestration failed before any agent started, there is nobody to report to. This watches *workers* for a live orchestrator; it is not a watchdog for the run as a whole.

## Getting these moments to you

To be told on your phone when a run needs you, give the orchestrator a way to send a message — an MCP server for your chat app, or a script that posts to one — and say in its `prompt_template` when to use it: when one of these reports arrives, and at the other points where your workflow stops and waits for you. A few things make that work well:

- **Let only the orchestrator send messages.** Workers should return questions through `work-done` rather than messaging you and waiting, so exactly one agent — the one that can act on your answer — is ever waiting on you, and only that one agent's tool needs setting up.
- **Notify only where you may have walked away**, and make each message start with the repo and task so you know where to go.
- **Send and continue.** The orchestrator should never wait for, check or retry a delivery; a failed send should cost you a notification, not the run.
- **Keep the channel's credentials and chat identifiers in the agent's own configuration.** The deck never reads or stores them.

Instructions in a prompt can be lost when a long session is compacted, so a notification you asked for may silently not be sent; the deck's own reports above do not have that problem, because each one arrives fresh when it is due.

## See also

- [Orchestration](orchestration.md) — how delegation, `work-done`, and role configuration work
- [Configuration](configuration.md) — the rest of `.dot-agent-deck.toml` and the global settings
- [Schedules](scheduled-tasks.md) — the other long-running, daemon-owned surface where a run finishes while you are not watching
