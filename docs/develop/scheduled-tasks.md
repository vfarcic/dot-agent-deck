# Scheduled tasks: design notes

Maintainer notes on schedules (PRD #127) and issue-dispatch schedules (PRD #120). The user-facing behaviour and the full reference are in [`docs/scheduled-tasks.md`](../scheduled-tasks.md); this page keeps the reasoning that was removed from it when the published docs were rewritten for the user's agent (PRD #1419).

## Why tab reuse is the default

The expected access pattern is a notification first and a look at the deck later, so one tab per schedule that each run overwrites ("one weather tab, ever") suits most schedules. `new_tab_per_fire = true` exists for the case where a history of runs matters. The reuse registry (`spawn::ReuseRegistry`) is daemon memory keyed by task name, which is why a restart makes the next run open a new tab and why renaming a schedule is forbidden (it would orphan the reused tab).

Reuse is gated on the delivery pane's live agent being one the task spawned (issue #617), so an agent restarted in place in that pane is not reused. The next run opens a fresh tab instead: an extra tab is the safe direction compared with submitting a scheduled prompt into a process that never ran this task. A consequence the user page states: while a reused tab is alive, a changed `working_dir` or `command` does not take effect, because `decide_reuse` compares only the target's kind (single vs. named orchestration), and only when a `shape` resolved one.

## Why `command` is required on a plain schedule

There is no `$SHELL` fallback for a scheduled task (a user decision in PRD #127's follow-up): a bare shell cannot act on the prompt. The loader rejects a missing or blank `command`, and `schedule add` refuses to write one. Issue-dispatch tasks are exempt because each clone's own config, or the global `default_command`, provides the command. When neither exists, the issue-dispatch spawn does fall back to the daemon's `$SHELL`; the user page states that as a failure mode.

## Why `shape` exists (issue #835)

Before it, the shape was derivable only from `working_dir`'s config, so a schedule pointed at a repository defining `[[orchestrations]]` fired the whole team and silently ignored its own `command`, with no way to say "one agent, here". The value is resolved at fire time, not at registration, because the target directory's config can change between authoring and a run months later. An unresolvable shape abandons the run rather than falling back to the config-derived target, since a silent fallback would reproduce the defect the field removes. `shape` with `issue_dispatch` is rejected at load rather than ignored for the same reason.

## Local time, missed runs, catch-up

Cron is evaluated in `chrono::Local`; a per-schedule time zone was deferred until demand appears (PRD #127 Open Q2), and the DST skip/double-run is the accepted trade-off. The firing loop evaluates every whole second since its last wake-up, capped at 60 seconds (`MAX_CATCHUP_SECONDS`), so a brief stall does not drop a run but resuming from a long suspend does not replay hours of runs. Runs that fell due while the daemon was down are deliberately not made up.

## Where failures are reported

Every scheduler failure goes through the `scheduler::Notifier` seam, whose only production implementation is `StderrNotifier`: `[scheduler] …` lines on the daemon's stderr, which a lazily spawned daemon appends to `<state_dir>/daemon.log`. PRD #126 was meant to supply a deck-visible notification channel behind the same trait; until then no schedule error reaches the TUI or the desktop app, and the user page says so.

## The desktop app and schedules

No schedule data reaches the desktop app: its types and snapshot carry no schedule field. The only schedule-derived value on the wire is `Scheduler::revision` (issue #887), one integer that lets a client notice that the daemon's project list may have changed (a schedule's `working_dir` seeds project enumeration) without leaking schedule contents. This is why the Schedules manager is TUI-only and the desktop app only offers the authoring chips.

## Issue dispatch

- The clone directory is named after the schedule (sanitised), not the repository, so two schedules on one repository get two clones.
- `gh issue list --limit max_per_run` is also capped in code, because the list may ignore `--limit`. Claimed issues count toward the cap.
- Worktrees are removed with `--force` on tab close (`RemovalPolicy::Force`): the clone is daemon-owned, and a worktree left behind would skip its issue on every later run. The worktree registry is daemon memory, so after a restart closing a tab leaves the worktree in place.
- The GitHub-only scope is deliberate for now; other forges are tracked by demand.
