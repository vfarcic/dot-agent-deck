# Execution budgets and stop conditions: catalogue, simulation and recommendation

This page is the evidence for PRD #635's discovery. It covers:

- what the deck limits, times out, retries and stops today, across manual panes, orchestrations, dispatches and schedules, and what it preserves when something stops;
- which budget scopes, aggregation rules and measurements are available, and which limits could be enforced rather than only advised;
- what pausing and stopping an agent actually does, tried on a real agent;
- a replay of a month of this repository's agent runs against candidate limits, plus the hard cases (nested, restarted, missing data, dirty worktree, override);
- the agent's **recommendation: Revise**. The `Go`/`Revise`/`Stop` decision itself is the maintainer's, recorded on issue #635.

**This page describes `origin/main` at `1fe600e6` (v0.49.3, 2026-10-30). Some of it is about to move.** PRD #1666 part 3 is rewriting spawn, the PTY layer, process control, inbox delivery, `work-done` routing and what is restored after a restart. Findings likely to change when it merges are marked *(part 3)*. This discovery changed nothing in those files. The scheduler, the project configuration and the agents' own behaviour are outside part 3's areas.

## How the evidence was gathered

- **Catalogue:** the source at `1fe600e6`, read by flow. Each claim names the file it comes from, and the constants were checked at their definitions. Nothing in the tree was changed.
- **Run sample:** a metadata-only replay of this machine's local agent stores for the worktrees of this repository. These are Claude Code transcripts, Codex rollouts, OpenCode's database and Pi's sessions. The replay reads timestamps, counts, and Claude Code's own `cost-state` totals (estimated cost and lines changed). It never reads prompts, replies or file content, the same rule PRD #633 followed.
  - **Coverage:** 1,862 sessions in 242 working directories, 2026-09-24 to 2026-10-30. That is 692 Claude Code, 348 Codex, 538 OpenCode and 284 Pi.
  - **Outcomes:** each worktree was joined to the outcome of the pull request on its branch (`agent/<slug>`), using 1,075 pull requests from GitHub.
  - **Activity:** "active" time counts only gaps of five minutes or less between a session's events, so a pane left open overnight is idle, not active.
  - **Scripts:** the two replay scripts (`sessions.py`, `analyse.py`) and their output are kept outside the repository, in the main checkout's ignored `.dot-agent-deck/prd-635-evidence/`.
- **Incidents on record:** issues and maintainer pages that describe waste or a stop that went wrong. These are #714, #590, #1678, #1757, #428 occurrence #5, `orphan-reaper.md`, and the runs PRD #628 reconstructed in [`work-identity.md`](work-identity.md).
- **Experiments:** three runs of a real agent (Claude Code 2.1.314, Haiku) in a scratch git repository at `/var/tmp/prd635-pause`, outside the deck, each started in its own session the way the deck starts an agent. The scripts are with the replay scripts. They used a cheap model and cost a few cents.
- **Who did this:** an agent, from the records above. No maintainer has reviewed the classification of waste yet.

## 1. Catalogue

### Limits on how much work runs

| Flow | Limit | Value | What happens at the limit |
| --- | --- | --- | --- |
| Manual panes, orchestration, dispatch | number of agents or panes | none | — |
| Dispatch | units still *starting* whose report has nowhere else to go (`src/retained_store.rs`) | 16 (`DOT_AGENT_DECK_DISPATCH_FALLBACK_SLOTS`), plus 256 MiB and 4096 files of retained reports | the new dispatch is refused (`dispatch_slots_full`). This bounds a report store, not running units *(part 3)* |
| Dispatch | nesting depth | none: the daemon checks that the caller is a live agent, not its role, so a worker can dispatch | — |
| Orchestration | delegations per orchestration | none | — |
| Orchestration | tasks per worker | one at a time: a worker that still owes a `work-done` is refused as busy unless `--supersede` | the delegate is refused |
| Orchestration | who may delegate | the orchestrator only (`refuse_unless_orchestrator_caller`, `src/state.rs`) | a worker's delegate is refused |
| Schedule | overlap of one schedule with itself | one `running` flag per schedule (`src/scheduler.rs`) | the fire is skipped (`SkippedStillRunning`). The flag covers the fire's own work, not the agents it started, which nothing waits on |
| Schedule | issues per issue-dispatch fire | `max_per_run`, default 3, minimum 1, no maximum (`src/config.rs`) | caps issues *considered*, so claimed issues use up the cap (#194, still open). The field's own doc comment says "dispatched", which the code does not do |
| Schedule | concurrency across schedules | none: every due schedule fires | — |
| Any | elapsed time, tokens, money, CPU, memory, changed lines, output volume | none in production | — |

### Timeouts

**None of these stops an agent.** Production code has no deadline on an agent, a delegation, a dispatched unit, an orchestration or a scheduled run. The timeouts that exist bound the deck's own plumbing or raise a notice:

- **Idle worker** (`worker_response_timeout_minutes`, default 120, `0` turns it off, range 1 minute to 7 days; `src/project_config.rs`, `src/state.rs`):
  - It starts when the worker acknowledges the task, not when the task is sent.
  - It posts one notice per delegation to the orchestrator that delegated, then nothing more.
  - It stops nothing.
  - The value is re-read from the orchestration's directory, then the worker's, on every delegate. See [override](#override-who-can-raise-a-limit).
- **Waiting for input:** the orchestrator is told after 30 s, at most once per four such windows per worker.
- **Blocked by a provider limit** (#714):
  - **Detection:** Claude Code, Codex and OpenCode report a usage limit or depleted credits through structured signals. The card then reads Blocked with the reset time, and the orchestrator gets one notice per outstanding delegation.
  - **What it does not do:** nothing is paused, retried or stopped. A delegate to a Blocked worker is sent with a warning, because sending the task is how the agent retries.
  - **Gaps:** Pi and Devin have no quota classifier. No notice is sent when the orchestrator itself is Blocked. That notice depends on the idle-worker record, so with the idle-worker detector off it is not sent either. This was read from the code and not tested.
- **Delegation commission** (`DELEGATION_COMMISSION_TTL`, 7 days; #590): bookkeeping. A worker's debt of one `work-done` expires, and nothing is stopped *(part 3)*.
- **Plumbing:**
  - 600 s per `gh`/`git` call in dispatch and the scheduler, so a hung `gh` cannot hold a schedule's `running` flag forever.
  - 30 s per claim call and 120 s per run for GitHub claims.
  - Inbox redelivery: a 60 s acknowledgement wait, at most 5 hand-overs, and a notice to the orchestrator after 5 minutes undelivered *(part 3)*.
  - Hook holds for questions, up to about an hour.
- **Daemon:** the 30 s idle shutdown fires only with no clients, no agents and no pending schedules. The test-only lifetime and orphan backstops are switched on by environment variables that production never sets.

### Retries

- **A crashed agent is not restarted when it crashes.** It is marked crashed, and the orchestrator is told (`compose_worker_exited_notice`).
  - Recovery is a person's or the orchestrator's `pane restart`.
  - A `clear = true` role whose agent exited is started again on its next delegate. A failed respawn is tried once and then reported.
- **Scheduled fires are not retried.** A failed issue is reconsidered on the next fire. Fires missed while the daemon was down are not made up (catch-up covers at most 60 s). The one retry is a fire refused because the daemon was restarting, which waits and fires again if the restart is called off.
- **Retries of work are people's or skills' decisions, not the deck's.** A second dispatch for the same issue is a new unit with no link to the first ([`work-identity.md`](work-identity.md)).

### Stop paths

| Path | Who | Guard | Signals |
| --- | --- | --- | --- |
| `StopAgent`: the close dialog in both clients | a person | none at the daemon; the dialog is the confirmation | SIGTERM to the agent's process group, 3 s grace (`AGENT_TERMINATE_GRACE`), then SIGKILL to the group *(part 3)* |
| `dot-agent-deck close` (`src/close_agents.rs`) | a person or an agent, for units its generation dispatched | refuses `not-reported`, `busy` and `strands-orchestration` unless `--force` | the same sequence, orchestrator first |
| `pane restart` / respawn | the orchestrator or a person | refused unless the pane crashed, or `--force` | closes the PTY first (most agents exit on EOF), then the same sequence |
| Daemon stop, signal, `KIND_SHUTDOWN`, restart request | a person, a service manager, a client | `daemon stop` refuses while orchestration roles are live (#770). The restart request asks first (#1487). The signal and the frame do not refuse ([`daemon-teardown-paths.md`](daemon-teardown-paths.md)) | every agent: SIGTERM, 3 s, SIGKILL, reap bounded at 1 s |
| The agent exits on its own | — | — | none. The record stays until it is closed |

**There is no pause.** No code sends SIGSTOP or SIGCONT to an agent. The closest mechanisms refuse *new* work and leave running agents alone:

- the restart's admission freeze;
- the `close` verb's per-instance and per-generation Closing state;
- `hold_dir_for_removal`, which refuses spawns into a directory being removed.

### Child lifecycles

- **Each agent is a session and process-group leader.** A stop signals the group. For a wrapped agent (Codex), the wrapper starts the agent in a session of its own, forwards the signal to it, and escalates after 1.5 s *(part 3)*.
- **A descendant that starts its own session is outside the group.** No production code puts an agent in a cgroup, makes the daemon a subreaper, or kills a process tree. The source says so where it hopes a well-behaved agent cleans up its own sub-shells (`close_agent`'s doc in `src/agent_pty.rs`; `src/wrap.rs`). On record:
  - an orphaned MCP server at 104% of one core for 1 day 12 hours, and a compiler stuck 3 days 20 hours in a deleted dispatch worktree (`orphan-reaper.md`);
  - test escapees alive for days (`src/lifetime_tag.rs`).
- **The net is outside the deck.** `scripts/reap-orphans.sh` runs on a user timer and by default reaps only orphaned MCP servers.
- **One experiment ended cleanly.** In experiment 3 below, the deck's own stop sequence ended both the agent and its long-running tool command. That is one trial with one agent version, which narrows the gap without closing it: the recorded escapees came from other process shapes.

### What a stop preserves

| What | Preserved? |
| --- | --- |
| Files the agent wrote | yes, on disk. Stopping a process does not touch them; removing the worktree does |
| Dispatch worktree on close | kept when dirty, or when its status cannot be checked (`RemovalPolicy::KeepIfDirty`, #236). The branch is always kept. The `close` verb reports which (`kept-dirty`, `removed`, …) |
| **Scheduled issue-dispatch worktree on tab close** | **no: `RemovalPolicy::Force` removes it, dirty or not** (`src/issue_dispatch_run.rs`). This is deliberate: a worktree left behind makes later fires skip the issue forever. But it means closing a scheduled issue-dispatch tab discards uncommitted work. A worktree abandoned during a restart is force-removed the same way |
| Scrollback | no: a 1 MiB ring in memory, gone on close or respawn *(part 3)* |
| A worker's report | yes, in the retained-report store under the state directory *(part 3)* |
| The agent's conversation | in the agent's own store. The deck never resumes it: a restored pane re-runs its command fresh, and resuming is the user's `claude --resume` |
| Role registrations, delegations, dispatched units, the scheduler's reuse and worktree registries | no: in the daemon's memory only, gone on a daemon stop ([`work-identity.md`](work-identity.md)) |

### What can be measured at decision time

"At decision time" means the daemon can read the value while the run is still going, which is the only time a limit can act on it.

| Candidate budget | Available to the daemon while the run is going? | Provenance |
| --- | --- | --- |
| Elapsed time | yes: spawn time per agent (`spawned_at_ms` on ListAgents), `started_at` and `last_activity` per session (`src/state.rs`, `src/agent_pty.rs`). `daemon status --json` carries none of them | measured |
| Active time and idle since | derivable from status transitions. Today only the telemetry tracker keeps them, and only with telemetry on | measured |
| Attempts | no counter of respawns, restarts or dispatches per piece of work. Retries are separate units with no link (#1889) | — |
| Concurrency | yes: live agents, per orchestration instance and per dispatched unit, from in-memory registries. Lost on restart | measured |
| Tokens | no: the telemetry tracker polls usage per run, but only exports it when the run ends, and only with telemetry and trace export on. No client or protocol surface carries it (#1858, open) | provider-reported, at run end |
| Money | no; and when it exists, it is an estimate (Claude Code, Pi), a subscription zero (OpenCode) or absent (Codex) | estimated or unavailable ([`execution-accounting.md`](execution-accounting.md)) |
| Allowance used | Codex only, inside the usage reader, exported at run end. Claude Code has no channel until a limit is hit | provider-reported |
| Provider limit reached | yes: Blocked status for Claude Code, Codex and OpenCode | agent-reported |
| CPU and memory per run | no: the per-machine collector (#1859) exports over OTLP, and nothing in the daemon reads it. The daemon has host-wide figures (`src/host_metrics.rs`) | measured, host-wide only |
| Changed lines, output volume | no: no diff or PTY byte counter | — |

## 2. Scopes, aggregation, provenance

**Candidate scopes, using today's identities.**

| Scope | Identity today | Survives a daemon restart? | Fits which limit |
| --- | --- | --- | --- |
| Agent (one generation in a pane) | agent id, pane id | no | elapsed, idle, provider-blocked |
| Delegation | delegation watch, commission *(part 3)* | no | elapsed-since-acknowledgement (exists as a notice) |
| Orchestration instance | `orch-<16hex>-<seq>` | no | concurrency of its roles, total elapsed |
| Dispatched unit | `u-<epoch>-<n>` *(part 3)* | no | total elapsed, concurrency of its descendants |
| Dispatcher (a pane that dispatches) | its pane and generation | no | concurrency of its units |
| Schedule | the user's `name` in `schedules.toml` | yes | concurrency of its fires, a cap per fire (exists) |
| Machine | the daemon | — | concurrency of every agent, host health |
| Work item (issue, PRD) | branch naming only, 78% attributable (#633) | yes (git, GitHub) | attempts, total cost; needs #1889's journal |

**Aggregation rules, if a budget aggregates at all.**

- **Charge time to the scope that started the run.** A delegation's time belongs to its delegation, orchestration and unit. A unit's descendants (units it dispatched) roll up to it through the parent link #1889 would record, which does not exist today: `close` is deliberately not transitive, and reports descendants rather than closing them.
- **Count elapsed time as wall time and concurrency as live agents, not as sums.**
  - Summing the active time of a team of six over-states how long anything took. Wall time per scope is what a person waits on.
  - A concurrency limit counts live agents at the moment of admission.
  - The replay below shows why the difference matters: Claude Code sessions sit idle for long stretches. 99 of 692 were idle for more than 8 hours of their span.
- **Never double-count tokens.** Claude Code's own total already includes its subagents (#633). Usage is taken from one source per agent and rolled up per run id.
- **A retry is a new attempt of the same work.** Charging it to that work's budget needs an attempt identity that does not exist yet. Until #1889, an attempt budget can only count per scope (a unit, a schedule), not per work item.
- **What a restart does to a budget.** A budget held in daemon memory resets with the daemon, which would let an interrupted run start a fresh budget. Either a budget is journal-backed (#1889), or it is advisory, so that a reset cannot do harm.

**Advisory or enforceable.** A limit can be *enforced* only when four things hold:

1. the deck measures it at decision time;
2. its provenance is measured or provider-reported, not estimated;
3. its scope survives what the deck itself does, such as a restart;
4. its action destroys nothing.

By the measurement table above, today:

| Budget | Enforceable now? | Why |
| --- | --- | --- |
| Elapsed time (agent, delegation) | as a **notice** yes. As a stop, no | measured, but the scope is lost on restart, and the replay says a stop would mostly cut productive work |
| Concurrency at admission (refuse or defer a *new* start) | yes | measured at the moment it matters, and refusing to start destroys nothing |
| Attempts | no | nothing counts attempts or links them |
| Tokens | no | known only at run end, and only with telemetry on |
| Money | no, and not planned | an estimate never triggers a destructive action (the PRD's own rule). #633 dropped cost as spend |
| CPU and memory per run | no | the daemon cannot read the collector |
| Changed lines, output volume | no | not measured, and no sampled waste involved them |

## 3. Pause and graceful stop

The PRD asks for a non-destructive pause and a graceful stop for elapsed time, attempts and concurrency before anything cost-based. **The process-level half of both lives in files PRD #1666 part 3 owns:** signalling an agent, freezing it, and asking it to save its work before a stop (`src/agent_pty.rs`, `src/spawn.rs`, `src/platform/proc/`, `src/wrap.rs`, inbox delivery).

The admission half is different. Holding a scheduled fire or a dispatch sits in the scheduler and dispatch code, outside those files. But a prototype there would change how launches behave, which this discovery must not do.

So this milestone is a design, backed by experiments on a real agent outside the deck. No code was written.

### Experiments

Each run started `claude -p` with Haiku in its own session (`setsid`), as the deck starts an agent, in a scratch repository.

| # | What was done | Result |
| --- | --- | --- |
| 1 | The agent ran `sleep 40 && date > out1.txt` through its Bash tool. 15 s in, SIGSTOP went to the agent's process group; 90 s later, SIGCONT | **The tool command kept running while the agent was stopped**: `out1.txt` was written 31 s into the 90 s pause. After SIGCONT the agent finished normally and replied DONE. A process-group stop freezes the agent, but not a command it runs in a session of its own |
| 2 | The agent wrote a 2,000-word file. 8 s in, before the file existed, SIGSTOP went to the group; 150 s later, SIGCONT | The run survived: it finished one second after SIGCONT, wrote 2,661 words and exited 0. Whether the reply was still streaming or already in the socket buffer at the stop was not determined. Longer stops, and Codex and OpenCode, were not tried, so how long a provider stream tolerates a frozen client is untested |
| 3 | The agent ran a 127 s command (its harness moved it to the background). 15 s in came the deck's stop sequence: SIGTERM to the group, 3 s, SIGKILL to the group | The agent and its command were both gone after the stop, with no survivor. One trial, one agent version |

**What this means for "pause":**

- **SIGSTOP is not a pause of the work.** A command the agent started in its own session keeps changing files and spending resources, while the card can only show the agent as stopped.
- **It may also not be safe for the provider stream, beyond the 150 s tried.**
- **A real pause would have to freeze the whole tree,** which needs a cgroup freezer per run (the same machinery as #1887's `contained` posture). That is Linux-only, and part 3's area.
- **So the design below does not pause processes.** It pauses *admission* and *delegation*, and asks before stopping.

### Design: what each action does

| Action | What happens | Destroys anything? | Exists today as |
| --- | --- | --- | --- |
| **Notify** | one notice to whoever started the work (the orchestrator, the dispatcher, the user's notification route), naming the scope, the measured value, its provenance and the limit | no | the idle-worker, waiting and Blocked notices |
| **Hold new work** | the scope refuses new delegations, dispatches and scheduled fires until a person (or a client action) releases it. Running agents keep running | no | the restart's admission freeze; the `close` verb's Closing state; the busy refusal on delegate |
| **Refuse to start** | a new launch over a concurrency limit is refused or deferred, with the reason. A deferred scheduled fire is attempted again on its next due time | no | the scheduler's restart deferral |
| **Graceful stop** | first ask the agent to commit or save what it has and report (an inbox message, as delegation does now), then close it through `close`'s guarded path with `KeepIfDirty`, and report what was kept | no, if the worktree policy keeps dirty trees | `close` (#1589) without the ask-first step |
| **Force stop** | today's stop sequence | the process, its scrollback and its in-memory registrations; the worktree only under `Force` | `StopAgent`, `close --force` |

**The rules this design needs, from the PRD's hypotheses:**

1. A limit's **default action** is Notify. Hold new work and Refuse to start are opt-in per scope. Graceful stop is a person's choice offered by the notice, never automatic. Force stop is never the response to a budget.
2. A value that is **estimated, missing or stale** can at most Notify, and the notice says so. That is the PRD's rule for cost, and it applies equally to a token count that has not arrived.
3. **Who can change a limit.** Only a source the agent cannot write: the user's configuration outside the worktree, a client action, or a CLI call not made from an agent's pane. See the override scenario below for why the orchestration's one time limit fails this.
4. **After a daemon restart,** an in-memory budget is unknown, not zero. It notifies that it was reset, and never enforces from a fresh start, until #1889 makes it durable.

## 4. Simulations

### Replaying the sample against candidate limits

Each work item below is one worktree. The figures are from the replay.

- **Outcomes:** 197 of the 241 non-main worktrees reached a merged pull request on their branch. 3 had only closed, unmerged ones. 41 had no pull request of their own: most were reviews, rebases and fixes of someone else's pull request, which #633 also found.
- **Per work item:** median 1 session and 0.7 h active (90th percentile: 13 sessions, 2.4 h active, a peak of 3 sessions at once). Claude Code's estimated cost has a median of $7 and a 90th percentile of $26.
- **Per session:** median 0.12 h active and 0.14 h of wall time. At the 99th percentile, 2.2 h active against 32 h of wall time, because panes stay open while idle.

| Candidate limit | Work items it would have hit | Of those, merged | Of those, not merged |
| --- | --- | --- | --- |
| active time per item > 2 h | 31 | 29 | 1 closed (PRD #1542), 1 review with no PR of its own |
| active time per item > 8 h | 14 | 13 | 1 closed (PRD #1542) |
| wall-time span per item > 24 h | 27 | 25 | 2 with no PR of their own |
| sessions per item > 20 | 20 | 19 | 1 closed (PRD #1542) |
| more than 4 sessions at once per item | 15 | 14 | 1 closed (PRD #1542) |
| more than 6 sessions at once per item | 1 | 1 | 0 |
| Claude Code estimated cost per item > $50 | 15 | 14 | 1 closed (PRD #1542) |
| one session active > 2 h | 23 sessions | 16 in merged items | 6 in the maintainer's own checkout, 1 in a review |

**Every candidate limit cuts mostly productive work.** The one high-effort item that did not merge, PRD #1542 (55 sessions, 16.9 h active, about $152 estimated), was not a runaway: its pull request (#1561) was closed because it conflicted with a parallel PRD in about 30 files, and replaced by a fresh branch. That is waste #629 (dependency-aware execution) is about, and a budget would only have stopped it earlier, without telling anyone why. The other two closed pull requests (PRDs #1692 and #221) took under 0.5 h each, and closing them was a decision about the work, not about cost.

**Attempts:**

- 8 of 194 issue and PRD numbers had more than one worktree. All 8 were deliberate: a review, follow-ups, a desktop half, or a prototype's scratch copy.
- 9 of 530 `agent/*` branches carried more than one pull request. The largest, PRD #421, had three, a phased PRD continued by hand (#628's run 2).
- No sampled retry was a loop the deck could have stopped.

**Concurrency across the machine:**

- Up to 25 sessions overlapped. More than 8 overlapped for 17% of the sampled time, and more than 16 for 2.5%.
- Machine load does cause waste on record: a test reddened by a loaded machine is common enough that every harness panic now ends with a "load context" block (issue #701).
- That is the one waste pattern that admission, rather than stopping, could reduce. The sample does not say how many runs it cost.

### Waste cases on record

| Case | What was wasted | Would a budget have caught it? | Today |
| --- | --- | --- | --- |
| #714: two workers hit provider limits and kept their processes for about two hours, one reading Thinking | hours of a team's progress, no tokens | an elapsed or no-progress notice would have, at its threshold | caught directly from the agents' own signals (Blocked), which is better than any timer |
| Orphaned processes alive for days (`orphan-reaper.md`): one at 104% of a core for 1 day 12 hours | CPU and memory long after the work ended | not by a budget on the agent, which had ended. A lifetime bound on an agent's *descendants* would have | an external reaper for MCP servers only |
| #428 occurrence #5: a stray `pkill` stopped nine panes across three dispatched units | the runs, and the ability to tell what was interrupted | no: that was an unwanted stop, not a missing one | disclosure (#1109); interrupted-work tracking is #1890 |
| PRD #628's run 9: after a restart, a scheduled issue was skipped indefinitely because its worktree existed | the issue, not compute | no | #1890 |
| PRD #1542: a superseded orchestration | about 17 active hours | only by stopping earlier, without the reason | #629's area |
| Long-idle panes: 99 Claude Code sessions idle for more than 8 hours of their span; 10 sessions in the maintainer's own checkout ran for more than 24 hours | memory and a slot, no tokens while idle | an idle-time notice would surface them; stopping them would lose the conversation's place | nothing |

### Hard cases

| Scenario | What happens today | Is useful work recoverable? | What a budget must do |
| --- | --- | --- | --- |
| **Nested:** an orchestrator's worker dispatches a unit, which dispatches another | allowed (no role check on `dispatch`, no depth limit). `close` on the outer unit reports the inner one as an open descendant and leaves it running | yes: every unit has a worktree and branch; the inner unit's report may have no caller left *(part 3)* | charge the inner unit to the outer through a parent link (#1889). Until then, scope concurrency to the dispatcher and the machine, not to a tree it cannot see |
| **Daemon restarted mid-run** | agents stopped (3 s grace). Units, delegations, roles and registries are lost. Dispatch worktrees and branches remain. A scheduled issue's worktree makes later fires skip it | the files, yes; the run's place, partly (the agent's own conversation store) | treat every in-memory budget as unknown after a restart, notify, and never enforce from a fresh zero |
| **Missing data:** telemetry off, or an agent with no usage channel (Devin, a plain command), or a usage total that arrives only at run end | a token or cost budget sees nothing until the run has ended | n/a | Notify only, saying the value is missing; never stop on it |
| **Dirty worktree at a stop** | dispatch: kept and reported. **Scheduled issue dispatch: force-removed on tab close, uncommitted work lost.** A worktree abandoned during a restart is force-removed too | dispatch yes; scheduled issue dispatch **no** | a graceful stop must ask the agent to commit first and must use `KeepIfDirty`. The scheduled path's `Force` is a preservation gap independent of budgets |
| **Override: who can raise a limit** | the orchestration's one time limit, `worker_response_timeout_minutes`, is read on every delegate from the `.dot-agent-deck.toml` in the orchestration's directory, then the worker's. In a dispatch, that is a file in the worktree, which the agents can edit, so an agent can lengthen it or set it to `0` (off) | n/a | read limits only from sources agents cannot write, which is #1885's project-config trust. Raising a limit is a person's action (#634's escalation rule) |
| **Concurrency spike from a schedule:** several schedules fire at once, each starting a team | every due schedule fires, and each issue-dispatch fire starts up to `max_per_run` teams | yes; nothing is stopped | refuse or defer the *new* fire with its reason, and leave a deferred issue looking unclaimed (the restart deferral already removes the fresh worktree it made, for the same reason) |

## 5. Against the PRD's criteria

The PRD's bar for a budget type to be a `Go` candidate:

- it would have prevented at least two sampled waste cases;
- it takes no destructive action on estimated or missing data;
- it preserves all dirty work in stop simulations;
- it has an enforceable measurement, a clear scope and aggregation rule, a user-visible explanation and override semantics;
- the sample shows it prevents more waste than productive work.

| Budget type | Prevents ≥ 2 sampled waste cases | Prevents more waste than productive work | Enforceable measurement | Verdict |
| --- | --- | --- | --- | --- |
| Elapsed time, as a hard stop | no: the one hit that did not merge was a conflict | no: 13 merged against 1 at 8 h | yes | **Stop** as enforcement |
| Elapsed time, as a notice (total and idle) | yes, as notices: #714's two workers before Blocked existed, the long-idle panes | a notice stops nothing, so there is no productive work to lose | yes | **Revise**: advisory |
| Attempts | no sampled loop | — | no counter, no attempt identity | **Stop** until #1889, then re-evaluate |
| Concurrency at admission (refuse or defer new starts) | not shown in the sample: load-induced reds are on record but uncounted | refusing a start loses no work | yes | **Revise**: opt-in admission for unattended launches, plus visibility |
| Tokens | not tested: no runtime value | — | no (run end only) | **Stop** for enforcement; revisit after #1858 |
| Money | — | — | no, estimated only | **Stop**, per #633 |
| CPU and memory per run | the orphans: 3 on record | — | no (the collector is outside the daemon) | **Revise** toward a descendant-lifetime bound under #1887's containment, not a budget |
| Changed lines, output volume | none observed | — | not measured | **Stop** |

## 6. Recommendation: Revise

**This is the agent's recommendation, for the maintainer to decide on issue #635.**

**Why not Go.** No budget type meets the bar. Every hard limit in the replay cut far more merged work than waste. The one sizeable unmerged item was lost to a conflict, not to a runaway. The two waste cases a timer would have caught (#714) are now caught better from the agents' own signals. Tokens and money cannot be read while a run is going. Attempts are not counted.

**Why not Stop.** The catalogue found real gaps that are cheap to close without stopping anything:

- elapsed and idle time are measured but invisible (not in `daemon status --json`, and not on the cards as a duration);
- unattended launches have no admission limit at all;
- the orchestration's one time limit can be changed by the agents it applies to;
- a stop of a scheduled issue dispatch loses uncommitted work.

**What Revise would mean.** Advisory first, non-destructive enforcement only where the measurement is exact:

1. **Visibility:** each agent's elapsed time and idle-since in both clients and in `daemon status --json`, from values the daemon already holds (CLAUDE.md rule 22).
2. **Notices, never stops:** an optional total-elapsed notice per delegation, unit and scheduled fire, beside today's idle-worker notice, to whoever started the work, with the measured value.
3. **Admission for unattended launches:** an opt-in limit on live agents (per machine, per schedule, per dispatcher) that refuses or defers a *new* scheduled fire or dispatch, with the reason. Running work is never touched.
4. **Limits from trusted sources only:** depends on #1885.
5. **Preservation, separate from budgets:** a graceful stop that asks the agent to save its work before `close`, and a look at scheduled issue dispatch's `Force` removal, which discards dirty work on a tab close.
6. **Descendant lifetime:** under #1887's per-run containment, not as a budget.

**Stop applies to:**

- token and money enforcement until #1858 gives a runtime value with provenance, and money as anything but a labelled estimate;
- attempt budgets until #1889 gives an attempt identity;
- changed-lines and output-volume budgets;
- SIGSTOP as a pause;
- any limit whose default action stops a process.

**Sequencing.**

- **Wait for PRD #1666 part 3 to merge:** items 1 and 2, and item 5's graceful stop. They touch files part 3 owns: the status output, the PTY layer, inbox delivery and the clients' agent views.
- **Outside part 3's files:** item 3 and item 5's removal policy, which sit in the scheduler and dispatch code.
- **Wait on other PRDs:** item 4 waits for #1885, and item 6 for #1887.

Follow-up PRDs are for the maintainer to file once the decision is recorded.

## Not covered

- **The waste classification is the agent's.** A merged pull request was taken as productive and a closed one as waste. Neither says whether a run inside a merged item looped. That would need conversation content, which the replay did not read.
- **Session gaps.** The sample covers this machine and this repository only, from 2026-09-24. Devin has no sessions in it, and a session is not a run in the deck's sense: an orchestration restarts a role's agent for each task.
- **Pause experiments.** They used one agent and one model, with stops of at most 150 s. Codex and OpenCode, and longer stops, were not tried.
- **No scheduled issue dispatch was run.** None has run against this repository. Its behaviour is from the code and PRD #628's sandboxed run.
