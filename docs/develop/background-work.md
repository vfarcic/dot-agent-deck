# Background work after an agent's turn

Issue #1678. When an agent that reports its own turn boundaries ends its turn, its card stays **Idle**, and work it left running — background commands and background subagents — is shown beside the status as `Background: 2 commands, 1 subagent · 3h 12m` instead of flipping the card back to **Working**. The user-facing description is [Session Management → Work left running after a turn](../session-management.md#work-left-running-after-a-turn); this page is the mechanism. The binding design notes for the unit live in the issue's comments.

Before this change the shell-activity monitor ([`shell-activity-signal.md`](shell-activity-signal.md)) promoted a pane whose agent had a qualifying command running in its own POSIX session to `Working`, which meant a command Claude Code backgrounded at its 120 s cap kept the card `Working` for as long as the command lived, and a subagent Claude Code 2.1.296 runs in the background left the card `Idle` with no sign anything was still going.

## The two latches

Both live in a private per-card `BackgroundTrack` in `AppState::background_tracks` (`src/state.rs`), keyed by card id and present only where the daemon's ownership oracle is installed. `SessionState` carries no field for them, because fixtures build it by exhaustive literal.

- **Reporting latch (`reports_turn_boundaries`).** Set by the first genuine status report from this spawn: any admitted event except what the daemon authors (`ShellBusy`, `ShellIdle`, the card-surfacing and wrapper-fork `SessionStart`s, delivery notices, quota lifts), the `Unknown` catch-all, and the Codex wrapper's output-classified frames, which read Codex's screen rather than a turn boundary (`is_genuine_turn_report`). Pi's extension reports count. It resets when a different agent id takes the card (process replacement). On a pane with the latch set, `ShellBusy`/`ShellIdle` never move the status; a pane without it keeps the PRD #386 behaviour, `shell_synthetic_working` and its takeover protections included.
- **Turn latch (`turn_ended`).** Set by a main-thread `Idle` (Claude's `Stop`, OpenCode's session idle, Pi's finished) or `QuotaBlocked`, and by a terminal main-thread `Error` without a tool name from Claude Code or OpenCode (Claude's `StopFailure`, OpenCode's `session.error`); a failed tool call does not end the turn. Cleared by `Thinking` or a main-thread `ToolStart`. Subagent events (anything carrying the subagent-id metadata key) and shell events never change it, so Needs Input, Blocked or Error after an ended turn leave it set.

Background counts are published only while both latches are set. During a live turn a process in its own session is the foreground tool call, not background work, so both counts are zero.

## The subagent tracker

`BackgroundTrack::subagents` maps a subagent id to the daemon's receipt time of its `SubagentStart`, in epoch milliseconds on the daemon's clock; payload timestamps are not used. The id is `AgentEvent.metadata[SUBAGENT_ID_METADATA_KEY]`, not `AgentEvent.agent_id`.

- A start inserts through `entry`, so a duplicate start keeps the original time.
- A stop removes the id. **A stop for an id the map does not hold is a no-op for the tracker**: it decrements nothing, clears nothing and raises nothing. Claude Code 2.1.296 sends three such unpaired internal stops per run. The tracker is updated beside `apply_event`'s existing logic, not by an early return in it, so the `SubagentWait` resolver still sees every stop (a stop can end a permission wait the tracker never saw start).
- **Bounds.** At most `MAX_SUBAGENT_IDS` (128) ids. An id must be non-empty and at most `MAX_TRACKED_SUBAGENT_ID_BYTES` (256) UTF-8 bytes; a longer one is not tracked at all rather than truncated, since two ids sharing a prefix would collapse into one. Its event keeps the subagent-id metadata key, so a `ToolStart` from it is still attributed to the subagent and does not read as a main-thread call.
- **The undercount is accepted.** A start that arrives while 128 ids are tracked is not recorded, and an oversized id is not recorded either, so the published `subagents` count understates in those cases. There is deliberately no anonymous overflow counter: `SubagentWait.overflow` is not reused, because an unknown stop decrements it, and a stop with nothing to pair against cannot be told apart from Claude's unpaired internal stops.
- The map is cleared on a genuine `SessionStart` that names a new hook generation for the pane (Claude Code's `/clear` sends one). The whole record goes when its card leaves `AppState::sessions` (a pane close, or a `SessionEnd` that removes the card), and its counts are emptied when the monitor retires the spawn (below).

## The counted scan

`qualifying_command_sessions` (`src/platform/proc/scan.rs`) returns the distinct POSIX session ids below the agent that qualify as commands: the same structural session-boundary test the boolean path uses, with the per-agent argv cross-check applied to **every** candidate when the agent has a shape (no early return on the first match), deduplicated by session id, and keeping `walk_with_reference_session`'s re-rooting at a nested terminal session so a Codex wrapper's inner agent is not counted. The boolean `descendant_shell_activity` is derived from it. It reads the same `ps` columns as before (issue #862's cost measurement still holds) and keeps the deadline, the retained sample, table-age rejection and backoff. An unknown root, session id or sample is `None`, not zero; on `None` the last known set is kept, and a valid sample replaces it.

On Windows the process-table scan returns `None`, so no command is ever counted there; subagent counts are unaffected.

## Durations: first seen, not process start

`BackgroundTrack::commands` maps each qualifying session id to the time the daemon **first sampled** it, including while it was still the foreground tool call, so a command that becomes background when the turn ends keeps its age. `observe_commands` keeps the first-seen time of every session still present and drops sessions that left a valid sample; an unknown sample restarts nothing. `since_ms` is the minimum over the commands and subagents currently tracked, recomputed when items leave, so it follows the oldest item still running rather than a sticky minimum. The process table has no start-time column, and adding one would put back the per-poll cost issue #862 removed; reading the OS start time is a possible later refinement.

## The aggregate is the daemon's, with a revision

`BackgroundWork { commands, subagents, since_ms }` (`src/event.rs`) is computed after the status transition, under the ingest write lock, by `BackgroundTrack::republish`, which advances a per-spawn `revision` (checked `u64`, reset with the track on process replacement) whenever the aggregate changes. It reaches clients two ways, both additive optional fields:

- `SessionSnapshot.background` / `background_revision`: `background` is `None` when there is nothing to show; the revision is present once the card has had any change.
- `AgentEvent.background` / `background_revision` on the event that changed the aggregate, and on every `ShellBusy`/`ShellIdle` of a reporting pane. `Some` with both counts zero is an explicit clear; an absent field means the event carries no aggregate, not that the work ended.

`daemon::ingest_event` applies **before** it broadcasts, still under the one write-lock hold, so the broadcast copy can carry the aggregate the apply computed while `SubscribeEventsWithSnapshot` ordering stays as it was.

**Producer copies are stripped.** `admit_producer_event` drops `background` and `background_revision` from every raw hook/producer frame, `ingest_event` clears them again before the apply, and `apply_event` clears them on the daemon's own path. A count a producer sends therefore never reaches a card through the hook path.

**Clients copy, never reconstruct.** The TUI's `AppState` and the desktop's `AgentView` run the same fold in client mode (no ownership oracle, so no `BackgroundTrack`). `fold_background_copy` keeps an event's aggregate when its revision is at least the one held (equal is idempotent) and otherwise forward-stamps the held one; `seed_hydrated_session` re-seeds from a snapshot unless the card already holds a newer revision. That is what closes the desktop's `AgentView::install` double-apply race: an event drained after a reply that already reflects a later revision cannot resurrect finished work. The client half of the reporting gate is the same signal: a `ShellBusy`/`ShellIdle` that carries an aggregate comes from a daemon that tracks turn boundaries and does not promote status; one without it comes from an older daemon and promotes as before.

## The monitor's emit rule

The shell-activity monitor (`src/daemon.rs`) keys its readings by spawn identity — pane, registry agent id, shell pid — revalidates under the state lock that the sampled spawn is still the pane's current one before it applies or broadcasts, and emits with the sampled spawn's agent id, never with the id of whatever card holds the pane now.

- **A reporting pane emits only when something it publishes changes**: the sampled session set (and with it the command count or ages), a busy edge, and once when its turn ends while busy. That last emit is the `ShellBusy` an older event-folding client has always received at that moment.
- **The PRD #386 M6b level re-emit (`status_regressed`) stays for non-reporting panes only.** Its predicate — busy and the card is `Idle` — is exactly the steady state of a reporting pane after `Stop`, so applying it there would emit every 500 ms for as long as the command runs.
- **A spawn that leaves the candidate set** (closed, respawned, exited) has its reading dropped and its background work retired through `retire_background_spawn`, without waiting for a `SessionEnd` it may never send.

## Known limits

- **An exited spawn's retirement is not broadcast.** `retire_background_spawn` republishes the aggregate in the daemon, where `AppState::live_session_for` reads it back, but no event carries it, so an attached TUI keeps the aggregate it last copied on that card until the pane closes or a later event replaces it. An exited agent's record also drops out of `ListAgents`, so the desktop's agent list loses that agent, line and all, on its next reconcile.
- **An old client on a new daemon** folds the daemon's `ShellBusy` into `Working` as it always has (it ignores the aggregate fields), so its card can read `Working` while its snapshots say `Idle`, and it shows no indicator. This is a display limitation of the old client, not a contract break: no field changed meaning and `PROTOCOL_VERSION` did not move.
- **A new client on an old daemon** receives no aggregate fields, so it promotes `ShellBusy` as before and shows no indicator.
- **Per-agent coverage** (CLAUDE.md rule 20): Claude Code, Codex and OpenCode report commands and subagents; Pi's extension and Devin's hooks carry no subagent lifecycle, so they report commands only.

## OpenCode subagents are child sessions

OpenCode runs a subagent as a child session: `session.created` names it with a `parentID`, and it then reports its own `session.status` (busy / retry / idle), `session.idle`, `session.error` and `session.deleted`, and its own tool calls, all under the child's id. Measured against `opencode 1.18.35` on 2026-10-11 with one `task` call on Haiku: the child's `session.created` (carrying `parentID`) arrived after the root's `task` tool call and before the child's first `busy`, the child then sent `busy` six times, its `bash` call's `tool.execute.before` carried the child's `sessionID`, and it ended with `session.status` idle followed by `session.idle`, before the root went idle. The deck's plugin (`plugin_template` in `src/opencode_manage.rs`) normalizes that into the paired lifecycle the tracker already understands, so the daemon needs no OpenCode-specific subagent logic:

- A child going busy or retry sends `subagent.start` with `subagent_id` set to the child's raw id; going idle, failing or being deleted sends `subagent.stop` for it. Both are filed under the session the child descends from (a grandchild's under the root), and the hook CLI (`build_opencode_event`) maps them to `SubagentStart` / `SubagentStop` with `SUBAGENT_ID_METADATA_KEY`.
- **Only a transition is sent.** The plugin keeps the set of children it reported active: a second busy sends nothing, an idle for a child that is not active sends nothing (a child goes idle through both `session.status` and `session.idle`, and a failed run sends `session.error` then idle), and a child that goes busy again after finishing starts again. Creation alone starts nothing and deletion only ends a child already active, because a child session outlives the call that ran it. At most 128 children are reported active at once, the tracker's own cap; a child past it sends neither its start nor its stop.
- **A child's other reports stay off the card's status (#1354).** Before this, the plugin filed a child's session events, tool calls and prompt under the root through its directory alias, since a child shares the root's directory: the child's `session.created` reached the daemon as the root's `SessionStart`, which repaints a card Idle, its going idle as the root's turn end, its tool calls as main-thread `ToolStart`s and the prompt the subagent was given as the user's prompt. Now its `session.created`/`updated` and other session events are not sent, its user message is not reported, and its tool calls carry `subagent_id`, which the decoder keeps only on `subagent.*` and `tool.execute.*`.
- **Ancestry must be known when the child reports.** It comes from `session.created` / `session.updated`, or from the background client lookup the reply path already makes. A session whose ancestry is not yet known is treated as the root's, as before; that is the case when the plugin loads after a child was created.
- **Not covered:** a child's permission request is still filed under the root unattributed (it needs the user's answer either way), so it does not take part in the `SubagentWait` resolution Claude Code's and Codex's subagent prompts get.
