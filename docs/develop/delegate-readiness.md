# Delegate readiness: delivering a task to a respawned worker

Maintainer notes for how a `clear = true` delegation waits for its replacement agent before writing the task (PRD #249 and follow-ups). The user-facing behaviour is in [Orchestration → What `clear` does to delivery](../orchestration.md#what-clear-does-to-delivery); this page keeps the timings, their history and the per-agent mechanics that page leaves out. The constants live in `src/state.rs` (`DELEGATE_READINESS_BUFFER`, `NO_SIGNAL_READINESS_BUFFER`, `WRAPPER_INTERFACE_READINESS_BUFFER`, `DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS`).

## The race

A freshly launched agent announces that its session has started well **before** its input box is ready to accept a line of text and treat Enter as "submit", so a task written the instant that signal arrives can land in a pane that is not listening yet. Where the write falls on the agent's startup decides the symptom: the task text sitting unsubmitted in the worker's input box until a human presses Enter, or nothing at all — no text, no activity, a worker that looks healthy and idle while the orchestrator waits for a `work-done` that never comes.

## The readiness buffer

A `clear = true` delegation first terminates the worker's agent (SIGTERM, escalating to SIGKILL if it does not exit) and relaunches the role's `command` in the same pane; the buffer below is what stands between that relaunch and the task write.

The deck therefore holds a `clear = true` task for a short readiness buffer after the replacement signals its session start (and after the fallback wait expires, for agents that never signal at all). The default is 1000 ms: the spawn-time path's 500 ms (`SPAWN_TIME_READINESS_BUFFER`), which was tuned for a warm pane, doubled because a respawn is a cold start. How long the deck actually holds a task depends on what it has been able to establish about the worker:

| what the deck can tell about the worker | how long it holds the task |
|---|---|
| it announced that its session is up | 1 second |
| the deck watched it take over its terminal | 5 seconds |
| it announces nothing before its first task | 8 seconds |
| the deck cannot tell which agent it is, and it announced nothing | the 30-second wait, then 1 second |

Which row a worker falls into depends on how its agent integrates with the deck, and on the deck being able to tell which agent the role runs. For a plain `claude`, `codex`, `opencode` or `pi` command it can. For a role launched through something else, such as `devbox run codex-big`, it cannot unless the role declares [`agent`](../orchestration.md#declaring-the-agent-behind-a-launcher-command), and a Codex, Pi or OpenCode worker then lands in the last row on every delegation.

## Per-agent readiness, and why launchers cost 30 seconds

The quick ways the deck has of knowing a Codex, Pi or OpenCode replacement is ready each depend on knowing it is that agent: it watches a Codex terminal, hands a Pi worker its task natively, and gives an OpenCode worker a fixed wait sized for OpenCode's start-up. An agent it cannot identify gets none of them, so the deck waits up to 30 seconds for the agent to announce itself before writing the task anyway. Claude announces itself as soon as it starts, which is why a Claude role behind a launcher delivers promptly while a Codex, Pi or OpenCode role behind one pays the full 30 seconds on every delegation. `dot-agent-deck validate` warns about each role in this state, and the daemon log records a warning each time a delegation pays that wait. The same identification is also what lets the deck monitor the pane, and for Codex that monitoring is the only thing that can identify the pane before it is given work — Codex does not announce itself until its first turn begins — which is why a Codex role behind a launcher stays blank from launch until its first task.

## The override

`DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS` (milliseconds, on the daemon's environment) replaces **every** row of the table above rather than being added to it, so raising it slows every case equally and setting it below one of the longer waits shortens that case to the given value. That is deliberate: the user knows something about their machine that watching one worker start does not refute. Values above `30000` are clamped, and `0` disables the wait entirely — the pre-fix behaviour, useful only for reproducing the problem. It covers a schedule's first prompt as well as a delegation. A report that a machine needs more than a second is the evidence needed to size the buffer per agent.

## What the buffer proves, and what it does not

A fixed delay makes the race much less likely; it cannot prove the replacement is listening. The regression test behind it drives a deterministic fixture deliberately built to ignore input for 650 ms, and confirms the task is lost with the buffer at `0` and delivered and submitted at `1000`. That pins the mechanism. It does not measure how long any real agent version takes to boot on a given machine.

## History

**The `clear = false` workaround.** Before the buffer existed, `clear = true` delegations could be lost outright, and users hit it consistently enough that two of them independently found the same workaround: set `clear = false` on the affected roles. It works because it removes the respawn, and with it the race — the agent is already running and listening, so there is no startup window to write into. It was confirmed across different agents and agent versions. The trade-off is that those workers carry context between delegations. On a release that includes the buffer the workaround should not be needed; the user page no longer mentions it.

**The "never came up" notice.** Before the `⚠ delegated worker never came up` report existed, a replacement that never started made the deck wait out its full 30-second readiness window, write into the empty pane, have the write refused, and drop the task with only a line in the daemon log — so the orchestrator was told nothing was wrong and waited for a completion that could never arrive.
