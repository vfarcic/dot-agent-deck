# Work identity: inventory, scenarios and the journal decision

This page is the evidence for PRD #628's discovery: the identities and records the deck keeps for manual panes, orchestrations, dispatches, schedules and worktree cleanup; the recovery, attribution and handoff questions those records cannot answer; a comparison of a minimal work-item model against reusing Git, GitHub or an event log; what a scratch persistence prototype showed; and the **Revise** decision, which narrows the PRD's work graph to a durable execution journal.

**This page describes `origin/main` at `e754ea08` (v0.48.1, 2026-10-25), and much of it is about to move.** PRD #1666 part 3 is rewriting dispatch returns, report recovery and a retained report store, task ids for `work-done`, the dispatched-units registry and what happens when the deck restarts or a client reconnects. The findings most likely to change when it merges are marked *(part 3)* below. The scheduler, the Git and GitHub records, telemetry and the TUI's `session.toml` are outside its areas.

## How the evidence was gathered

- **Inventory**: the source at `e754ea08`, read by area (daemon state and restart, dispatch and messaging, scheduler and external records), with each claim checked against the line it cites. Nothing in the tree was changed to gather it.
- **Live records**: the maintainer's running deck on 2026-10-25 (`daemon status`, `daemon status --json`, the state directory, `session.toml`, the inbox spool and spills), its worktrees, and this repository's GitHub history (300 merged `agent/*` pull requests, issue comments).
- **One sandboxed run**: no scheduled issue dispatch has ever run against this repository (no claim comment and no `agent-deck/claimed` label exist on it), so one was run on the same v0.48.1 binary in an isolated sandbox (`/var/tmp`, its own sockets, state directory, log, `HOME` and schedules file), against the stub `gh` from `tests/e2e_issue_dispatch.rs` and a local fixture remote, and its daemon was stopped mid-run with `SIGTERM` to its own pid.
- **Who reconstructed**: the reconstructions below were done by an agent from the records alone, and the times are its wall-clock times. The PRD asks a maintainer to do this; that pass has not happened, and the decision record says so.

## Inventory

Survival is given for a TUI detach and reattach (**D**), a daemon stop, restart or crash (**R**), and a machine reboot (**B**). A daemon stop or restart stops every managed agent ([`daemon-teardown-paths.md`](daemon-teardown-paths.md)), so after **R** the processes that held the in-memory identities are usually gone too; the exception is an agent that had detached from its PTY (issue #770).

### Records that survive a restart

| Record | Where | Identifies | Written by | D / R / B |
| --- | --- | --- | --- | --- |
| Deck id | `<state_dir>/deck-id`, 8 base32 characters (`src/deck_identity.rs`) | this per-user deck; prefix of every message id | daemon, once | yes / yes / yes |
| Branch | git refs: `agent/dispatch-<slug>` for a dispatch (`src/dispatch.rs`), `agent/issue-<n>` for a scheduled issue dispatch (`src/issue_dispatch.rs`) | the line of work, by naming convention | dispatch, scheduler | yes / yes / yes |
| Worktree | `<parent>/<repo>-dispatch-<slug>`, or `<clone>/.worktrees/issue-<n>` | the workspace | dispatch, scheduler | yes / yes / yes |
| Owner marker | `<gitdir>/worktrees/<name>/dot-agent-deck-owner`, JSON with `creator {kind, subject}`, `branch`, `created_at` and the creating daemon's `pid` (`src/worktree_owner.rs`) | which deck path created the worktree and for what (`dispatch` + name, or `issue-dispatch` + `<task>#<n>`) | dispatch, scheduler | yes / yes / yes |
| Abandoned-spawn marker | `dot-agent-deck-abandoned-spawn` beside the owner marker | a scheduled spawn refused during a restart, so a later fire can reclaim the worktree | scheduler | yes / yes / yes |
| Claim | GitHub label `agent-deck/claimed` plus one `<!-- dot-agent-deck:claim -->` comment per issue naming the task, a hashed machine id, the claim time and a lifetime (default 168 hours) (`src/issue_dispatch.rs`) | that a scheduled dispatch took an issue | scheduler; never removed by the deck | yes / yes / yes |
| Assignee and branch ledger | GitHub assignees, `agent/dispatch-(issue\|prd)-<n>` branches and `../*-dispatch-*` worktrees, read by `/issue-queue` and `/prd-queue` | that a skill-driven dispatch took an issue | the skills; a second ledger, separate from the claim above | yes / yes / yes |
| Pull request | GitHub, found from the branch by `gh pr list --head <branch>` (`src/pull_request.rs`, `src/worktree_reclaim.rs`) | the change and its outcome | GitHub; the deck stores no PR number | yes / yes / yes |
| Dispatch report file | `.dot-agent-deck/dispatch-report-<slug>-<pid><seq>.md` in the unit's worktree (`src/dispatch.rs`); the unit deletes it after `work-done` | one unit's report, before delivery | the unit | until deleted |
| Orchestrator context and worker task files | `.dot-agent-deck/orchestrator-context-<32hex>.md`, and `worker-task-<role>.md` written by an orchestrator | an orchestration's task text | daemon, orchestrator | yes / yes / yes; the context file is deleted when the orchestration ends, and coordination files go in a 14-day sweep |
| Inbox body spills | `.dot-agent-deck/inbox-message-<deck>-<unique>.md`, inventory in `<state_dir>/inbox-spills/` (`src/inbox_spills.rs`) | a long message body | daemon; kept 24 h after the message settles | yes / yes / yes |
| Schedule | `schedules.toml`, keyed by the user's `name`, which cannot be renamed | the schedule; there is no run identity | `schedule` CLI | yes / yes / yes |
| Saved session | `~/.config/dot-agent-deck/session.toml`: per pane `dir`, `name`, `command` and an optional orchestration snapshot (`src/config.rs`) | what the TUI re-creates on its daemon-empty restore | TUI | yes / yes / yes; it stores no pane, agent, session or orchestration id |
| Daemon output | `<state_dir>/daemon.log` for a lazily spawned daemon, and the deck's log file; the scheduler's per-fire outcome is a `[scheduler] …` line on stderr | history, as free text | daemon | yes / yes / yes; no rotation of `daemon.log` was found |

### Records that live only in the daemon's memory

These are held by the daemon process, so a TUI detach and reattach leaves them in place, and none of them survives a daemon restart.

| Record | Identifies | Notes |
| --- | --- | --- |
| Agent id | one agent generation | decimal counter from 1 per daemon, so ids repeat after a restart; clients key on the daemon's instance id to tell them apart |
| Pane id | a pane | TUI panes count from 1; daemon-spawned panes are `sched-<task>-<n>` or `sched-<task>-<n>-r<role>` from a counter that starts at 0 per daemon (`src/spawn.rs`), so `sched-dispatch-<name>-0-r0` recurs. The live `session.toml` still names `sched-dispatch-prd-633-0-r0` from a previous daemon |
| Daemon instance id | one daemon process | 16 random bytes per process, sent in `Hello` |
| Pane capability token | the right to post hooks and `work-done` for a pane | minted per spawn; after a restart an older token is refused (`refused (not-attested)` was observed in the sandbox) |
| Orchestration instance id | one orchestration tab | `orch-<16hex>-<seq>`, and the role maps `pane_role_map`, `pane_orchestration_map`, `orchestrator_pane_ids`, `pane_cwd_map` (`src/state.rs`), which have no persistence path ([`daemon-teardown-paths.md`](daemon-teardown-paths.md)) |
| Dispatched unit | one dispatch | `u-<epoch>-<n>` with its dispatcher, worktree, branch and up to 64 tombstones; memory-only by design (`src/dispatched_units.rs`) *(part 3)* |
| Dispatch return | where a unit's report goes | `DispatchReturns`, keyed on the unit's terminal pane, holding the caller's (pane, agent) pair; a caller that was respawned or closed loses the report, with no queue or retry (`src/dispatch_return.rs`, [`dispatcher-mode.md`](dispatcher-mode.md)) *(part 3)* |
| Message and delivery ids | one inbox message and its hand-outs | `m-<deck>-<16hex>` and `delivery <A>.<e>`; the inbox and its finished history are in memory ([`agent-messaging.md`](agent-messaging.md)) *(part 3)* |
| Delegation watches | an in-flight delegation | worker, role, orchestrator and due time *(part 3)* |
| Task id for `work-done` | — | none exists today: the deck infers which task a report answers *(part 3)* |
| Scheduler state | the registered tasks, one `running` flag per schedule, a revision counter | no run id; history is the stderr line |
| Reuse and worktree registries | which tab a schedule reuses; which worktree a tab close removes | both wiped on restart, so a scheduled worktree outlives its tab and its issue stays skipped until it is removed by hand ([`docs/scheduled-tasks.md`](../scheduled-tasks.md)) |
| PR cache | PR state per branch | refreshed from GitHub |

### Telemetry

The run trace carries `deck.run.id` (minted per spawn and passed into the agent's `OTEL_RESOURCE_ATTRIBUTES`), `deck.pane.id`, `deck.agent`, `deck.role`, `deck.orchestration.name`, `deck.orchestration.id` and `deck.pr.number`. `deck.issue.number` is defined and never filled, and no attribute carries a schedule name, a dispatch name or a unit id as its own value; the first two appear only inside a daemon-spawned pane id ([`telemetry.md`](telemetry.md)). A nested `codex exec` shares its pane's run id ([`execution-accounting.md`](execution-accounting.md)).

## Scenarios

Ten runs, reconstructed from the records above. For each: the request (what was asked), the attempts (which executions tried it), the owner now (who is responsible for its next step), the workspace, the result, and the next action. ✓ means a record answers it, ✗ means none does, and "prose" means only maintainer- or agent-written text does.

| # | Run | Launch path | Request | Attempts | Owner now | Workspace | Result | Next action | Time |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | PRD #633, `dispatch-prd-633` | interactive dispatch of an orchestration | ✓ issue; ✗ the dispatcher's task text (the inbox body is in memory and its spill went with the worktree) | ✓ one PR (#1870); ✗ how many role runs, except from the agents' own stores | ✗ which dispatcher launched it | ✓ branch; worktree removed | ✓ merged, issue closed, follow-ups #1858–#1860 | ✓ none | 2 min |
| 2 | PRD #421 | interactive dispatch, three times | ✓ issue (open, phased) | PRs #464 (`agent/dispatch-issue-421`), #1813 and #1867 (both `agent/dispatch-prd-421`); ✗ which run made which, since one branch name served two attempts | ✗ | ✓ branch | ✓ per PR | prose (the issue's phase list) | 4 min |
| 3 | Issue #1821 | two dispatches for one issue (`issue-1821`, `issue-1821-delete-prds`) | ✓ issue | PRs #1824 and #1823, both `Refs`, so GitHub's closing references are empty for both; the link is prose | ✗ | ✓ branches | ✓ merged | prose ("step 4 remains, after PRD #1666") | 3 min |
| 4 | Issues #1754 and #1629 | one dispatch for two issues (`issue-1754-1629`) | ✓ issues | #1765 (`Refs`, earlier) and #1848 (`Closes`); the link between them is prose | ✗ | ✓ branch | ✓ both closed by #1848 | ✓ none | 3 min |
| 5 | PRD #221, `dispatch-prd-221` | interactive dispatch | ✓ issue | ✓ PR #1855 | ✓ maintainer (comment) | ✓ branch | ✓ closed unmerged, PRD closed not planned | ✓ none | 1 min |
| 6 | `agent/fix-handshake-013-flake` | a hand-named branch; the PR body mentions a coder role | ✗ no issue, only "found by chasing the intermittent red" | ✓ PR #1838 | ✗ | ✓ branch | ✓ merged | ✓ none | 2 min |
| 7 | PRD #1859, live six-role orchestration | interactive dispatch of an orchestration, daemon running | ✓ issue, orchestrator context file, `worker-task-coder.md` | ✓ roles and labels from `daemon status`; the coder's inbox shows one queued message | ✗ which dispatcher launched it: `daemon status --json` carries no unit, dispatcher or orchestration id | ✓ cwd | open | ✗ (the orchestrator's conversation) | 2 min |
| 8 | 2026-09-15, issue #428 occurrence #5 | three dispatched units (one six-role orchestration, two single agents), daemon killed by an agent's `pkill -f "daemon serve"` | ✓ issues | ✗ in the deck; rebuilt from `deck.log`, worktrees and PRs by hand | ✗ the units' callers were never recorded outside memory | ✓ worktrees, all clean | two of three had pushed PRs | ✗ for the third, from the records | not recorded |
| 9 | Sandboxed scheduled issue dispatch, issues 7 and 8, `SIGTERM` mid-run | schedule → two orchestrations (four panes) | ✓ issue plus the schedule's prompt | ✗ no run id; the only history is two `dispatched issue #…` lines on the dead daemon's stderr | ✗ the owner marker names the dead daemon's pid; the claim comment says an agent was dispatched and stays live for 168 hours | ✓ worktree and owner marker | ✓ a commit on `agent/issue-7`; ✗ whether that work was finished | ✗ — the next fire printed `skipping already-claimed issue #7 … its worktree exists`, and the deck skips both issues until someone removes the worktrees | 3 min |
| 10 | The live dispatcher pane in the main checkout | a manual pane that dispatched today's units | ✗ (its conversation) | ✗ which units it launched: the dispatched-units registry is in memory, and `daemon status` does not show it | ✓ the pane | ✓ cwd | open | ✗ | 1 min |

The launch paths covered: manual panes (6, 10), orchestration (1, 7, 8, 9), interactive dispatch (1–5, 7, 8) and scheduled issue dispatch (9, sandboxed), with two interruptions (8, 9).

**What the sample says.** Finished work is answered well by GitHub: of the 300 most recent merged `agent/*` pull requests, 252 (84%) carry a closing reference to an issue and 293 (98%) name an issue number in their branch, their title or the start of their body. The questions the records could not answer fall into four groups, and most concern work that was in flight or interrupted:

- **Who asked, and where the result goes** (runs 1, 7, 8, 10): the dispatcher and the return address are held only in memory, keyed on a (pane, agent) generation that a respawn or restart retires.
- **Which attempt is which** (runs 2, 3, 4, 9): a branch name is not an attempt identity (two attempts shared `agent/dispatch-prd-421`); a scheduled fire has no run id; pane and agent ids repeat across daemon lifetimes.
- **Interrupted, finished or abandoned** (runs 8, 9): after a restart the deck keeps no record that marks a run as stopped mid-flight (the abandoned-spawn marker covers a spawn refused during a restart, not a run the restart interrupted). The scheduled case is the sharpest: the claim says the issue is taken, the worktree says it is in progress, no agent exists, and the deck skips the issue indefinitely.
- **Attribution** (runs 1, 6, and PRD #633's sample): PRD #633 found that 191 of 244 worktrees (78%) map to a work item by branch name, and that most of the rest worked on another branch's pull request ([`execution-accounting.md`](execution-accounting.md)). PRD #1860 depends on this.

No sampled run needed a **dependency** edge between pieces of work (one item blocking another). Continuations (runs 2 and 3) are adequately linked by sharing an issue.

## Comparison

| Need | Git only (branch, worktree, owner marker, notes) | GitHub only (issues, PRs, claims) | Append-only event log over today's ids | Minimal work record: a deck-minted work id, keyed into an append-only journal |
| --- | --- | --- | --- | --- |
| Who asked / return address across a restart | ✗ | ✗ | partly: records the (pane, agent) pair, which is gone after the restart | ✓ the parent's work id |
| Attempt identity across daemons | ✗ one branch, several attempts | ✗ no PR until late, none for review/verify runs | ✓ if keyed by (daemon instance, pane) | ✓ |
| Interrupted vs finished | ✗ | ✗ a claim does not end when its run is interrupted | ✓ | ✓ |
| Attribution to a work item | 78% by branch | ✓ for runs that opened a PR | ✗ without a key above attempts | ✓ with external refs as links |
| Manual panes, non-git directories | ✗ | ✗ | ✓ | ✓ |
| Replaces Git or forge authority | — | — | no | no: it links, it does not copy |
| New cost | none | rate limits, offline gaps | a file and a schema | a file, a schema, one id passed to agents |

The event log and the work record converge: an event log needs a key above attempts to link retries and attribute work, and once it mints one it is the minimal work record. What neither needs is a graph object with its own mutable lifecycle or dependency edges, since nothing in the sample used them.

## Prototype

A scratch Rust program (not in the repository and not in any pull request; it was kept on the development machine at `/var/tmp/dad628/proto`) implemented the journal: one JSON line per lifecycle event (`opened`, `attempt_started`, `attempt_ended`, `linked`, `reported`, `closed`), each carrying a version, a sequence number, the daemon instance id and a work id. A work has at most one parent, external refs (issue, PR, branch) and attempts; an attempt is keyed by (daemon instance, attempt number) and records its pane and agent. It replayed the sampled scenarios:

- **Restart recovery**: on open, the new daemon closes every attempt another instance left open as `interrupted`. After a simulated kill it listed the dispatcher's two children (a dispatch and an orchestration with one delegation) as interrupted, with their refs and workspaces, and a report written after the restart went to the caller's *work* and was retained there, although no attempt of the caller was live.
- **Scheduled issue 7**: the journal said `interrupted` rather than "worktree exists", which is enough for a later fire to start a second attempt on the same work and worktree instead of skipping. Both attempts had the same pane id, `sched-issues-widgets-0-r0`, which is why the attempt key includes the daemon instance.
- **Retry versus continuation**: two dispatches on `agent/dispatch-prd-421` became two work ids sharing `issue 421`, which a roll-up by ref groups. The deck cannot tell a retry from a continuation by itself; a launcher has to say "continues work X".
- **Migration**: a v1 file (an `issue` field instead of `refs`) is upcast on read with no rewrite; a v2 writer appends after it. A v1-only reader given the mixed file skipped 18 newer events and saw two works. So the rule that a reader skips unknown versions and kinds has to ship in the first release, and a downgrade sees only what happened before the upgrade.
- **Crash safety**: a torn last line is detected and truncated on open; a complete but unparsable line is skipped.
- **Redaction**: a field guard refused a value with a line break, which keeps prompt text out by shape. That is a shape check, not a privacy allow-list: a path still carries the user's home directory.
- **Cost** (release build, this machine's disk): `fdatasync` per event took 3.7 ms, which suits lifecycle events (tens per run) and rules out hook or tool events; 60,000 events for 10,000 works made an 11 MiB file that replayed in 186 ms; compaction that kept the last 10% of closed works, and the parents of whatever it kept, took 135 ms and left no dangling parent.

**What it did not do**: wire anything into the deck. Minting a work id at each launch, passing it to agents, writing attempts and recovering on start would change `src/spawn.rs`, `src/dispatch.rs`, `src/dispatch_return.rs`, `src/daemon.rs` and the orchestration registration in `src/state.rs`, all of which PRD #1666 part 3 owns until it merges, so it was not attempted. The scheduler (`src/issue_dispatch_run.rs`) and telemetry (`deck.work.id`) are outside those areas.

## Decision: Revise

Recorded in issue #628 on 2026-10-25. The PRD's own criteria:

| Criterion | Result |
| --- | --- |
| At least eight sampled runs across manual panes, orchestration, interactive dispatch and scheduled issue dispatch, with an interruption | met: ten runs, two interruptions; the scheduled run is sandboxed against a stub GitHub because none ever ran here |
| A maintainer reconstructs each run from current records, with gaps and times | partly met: an agent did it; a maintainer's pass is outstanding |
| `Go`: at least three launch paths share unresolved identity needs, and one minimal model answers them without replacing Git or forge authority, with a credible persistence and migration approach and a rule for how Git and the forge relate | the shared needs are met (manual panes, orchestration and interactive dispatch share all four needs above, and scheduled dispatch shares three: its requester is the schedule itself), and so is the persistence approach (the prototype); but the model that answers them is an execution journal with a parent link, not a work graph |
| `Revise`: history solves the observed failures without a graph | met: no sampled run needed dependency edges or a work object with its own lifecycle |
| `Stop`: Git, issue and session records answer every sampled workflow | not met: they answer finished work, not in-flight or interrupted work |

**The narrower model.** A durable execution journal: the deck mints a work id (prefixed with the deck id, so it is unique across machines) for each launch — a manual pane's first prompt, an orchestration, a delegation, a dispatch, a scheduled fire — and appends lifecycle events keyed by it: opened (launch path, parent work id, external refs, workspace), attempt started and ended (daemon instance, pane, agent, unit, how it ended), linked (a PR found later), reported (to which work, at which path), closed. Relationships: parent and child, and attempts within a work; continuation is a shared external ref, or an explicit "continues" given by the launcher. No dependency edges.

**The authority boundary.**

- The forge is authoritative for what was asked (issue text), what happened to the change (PR state, disposition) and claims between machines and people. The journal links to them by number and copies no prose.
- Git is authoritative for code, branches and whether a worktree exists.
- The journal is authoritative only for execution facts the deck observed: which attempts ran, where, under which parent, how they ended and where their reports went. It is observational about whether the work is done.
- An automated action that reads the journal (a scheduled fire resuming an interrupted issue) re-checks Git and the forge first, as today's skip checks do, so that a wrong journal entry does not route an action past the checks that already guard it.

**Storage.** An append-only JSON-lines journal in the deck's state directory, beside `deck-id`, written only by the daemon, lifecycle events only, with segment compaction. Not repository files (they would dirty worktrees and travel with clones), not Git notes (they cover only Git directories and one repository at a time), and not a database for now (the volume is small and replay is fast; a database earns its place only when a query needs an index). Moving a deck to another host is out of scope; the forge refs are what crosses hosts.

**Retention and redaction.** Refs, ids and paths only; no prompt, report or terminal text. Closed works are compacted after a retention window, and the ancestors of a kept work are kept. The per-field shape check is not a privacy allow-list, so the follow-up decides which fields may carry a path.

**Compatibility.** The journal is a new file with one writer, so introducing it needs no TUI↔daemon contract change and no `PROTOCOL_VERSION` bump. Showing a work id to clients later is an additive optional field (CLAUDE.md rule 18), and passing it to agents is an environment variable. Readers skip unknown versions and event kinds from the first release.

**What part 3 is likely to move.** Report delivery to a respawned or closed caller (part 3 adds report recovery and a retained report store), a task id for `work-done`, the dispatched-units registry, what the deck restores after a Stop or a restart, and possibly the pane id format. The follow-ups below are sequenced after part 3 merges and re-check these findings first. The scheduler's skip-forever behaviour, the two claim ledgers and the telemetry gaps are outside part 3's areas.

**Follow-ups.** Filed as new PRDs: #1889, a durable execution journal (work ids, attempts, parents, restart recovery, retention, and the work id on the run trace), which PRD #1860's attribution waits on; and #1890, interrupted work after a restart, which shows the user what was interrupted and lets a scheduled issue dispatch resume an interrupted issue instead of skipping it. Both are sequenced after PRD #1666 part 3.
