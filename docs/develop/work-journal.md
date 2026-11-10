# Work journal

The work journal (PRD #1889) is the daemon's append-only record of what the deck ran: one **work** per launch (a pane, an orchestration, a delegation, a dispatch, a scheduled fire, a scheduled issue dispatch), the **attempts** (agent generations) that served it, where their reports went, and how each attempt and work ended. It survives daemon restarts, so a later daemon can tell which of a previous run's attempts never finished. It is the narrower model PRD #628 chose ([work-identity.md](work-identity.md#decision-revise)); this page is how it is built and how to read it.

The code is in two modules: `src/work_journal.rs` is the file format and the store (ids, the line codec, the redaction guard, the per-run writer thread, the reader, recovery and compaction), and `src/work_tracking.rs` is the daemon's side (`WorkTracker`, owned by `AgentPtyRegistry` and reached through `registry.work()`), which decides which work each launch, attempt, report and end belongs to.

## Where it lives

Everything is in `<state_dir>/work-journal/`, an owner-only directory beside `deck-id` (`state_dir` is `crate::config::state_dir()`, in absolute form). Several daemons can share one state directory, so each file has one writer:

| File | What it is |
| --- | --- |
| `run-<run>.jsonl`, then `run-<run>.<n>.jsonl` | The segments of one daemon run, `<run>` being the daemon instance id the daemon sends in `Hello` (it never holds a `.`, so the first `.` starts the number). `run-<run>.jsonl` is created when the journal opens, even before the run has anything to write. When the next line would take the current segment past 8 MiB (`SEGMENT_MAX_BYTES`), the run starts `run-<run>.1.jsonl`, then `.2`, and so on, all under the same run lock. One rule governs rotation: **the run's highest-numbered segment on disk is always the one it is writing**, because every reader and every compaction, this run's or another daemon's, treats the segments below it as sealed. The new segment's directory entry is synced before the run writes to it. When that sync fails, the new file is removed and the current segment keeps taking lines; when the removal fails too, the file is already the run's highest segment to everyone else, so the run switches to it anyway and logs a `warn!` (its lines are synced, but its entry may not survive a crash). A failed rotation is tried again only once the segment has grown further: a sixteenth of 8 MiB more after the first failure, doubling per failure up to 8 MiB (`Writer::rotate`), so a lasting fault costs one attempt and one warning per stretch of lines, not one per line. The writer remembers the next segment number across rotations (`Writer::next_segment`) and it only grows: a name that is already taken (only the run's own writer creates its segments, so such a file came from outside it) moves the number past it for good, with a rate-limited `warn!`, so taken names cannot stall rotation however many there are. Only the highest-numbered segment is still written; the lower ones are **sealed**. Only that run writes them. |
| `run-<run>.lock` | Held by run `<run>` for its whole life, the way `retained_store` holds its run lock (`.pending-<run>.lock` is created, locked, then renamed), and released only after the writer thread has written everything it was handed. A run whose lock is absent or can be taken is **dead**, and so are all its segments. A lock that cannot be judged counts as live, so its files are left alone. |
| `compacted-<run>.jsonl`, then `compacted-<run>.<n>.jsonl` | **Chunks** run `<run>`'s compaction wrote: the lines it kept from the files it folded, cut at about 8 MiB each (`COMPACTED_CHUNK_BYTES`). Each chunk is written to a temp file, `sync_all`, renamed to the next chunk name no file has, and the directory synced, all under `maintenance.lock`; a chunk is never appended to, so any run's compaction may fold any chunk again, whether the run that wrote it is live or dead. |
| `maintenance.lock` | Recovery and compaction hold it **exclusively**, and a read of the directory (`read_journal`, `WorkJournal::read`) holds it **shared** (the platform's whole-file lock, `std::fs::File::lock_shared`). So a read sees the directory before or after a compaction, never between its publish and its unlinks, and two daemons sharing the directory never maintain it at once. Recovery and compaction take it without waiting; a reader waits for it. |

A daemon whose journal cannot open (a directory that fails the owner-only check, an I/O error) runs with it disabled: it records nothing, and that is never fatal. A disabled journal still mints work ids, so agents still get `DOT_AGENT_DECK_WORK_ID` and telemetry still gets `deck.work.id` (see [The work id outside the journal](#the-work-id-outside-the-journal)).

**Read the directory, not a file.** Compaction folds dead and sealed segments into `compacted-*.jsonl` chunks and deletes them, and a long-lived run writes several segments, so a reader (and a test) scans every `run-*.jsonl` (numbered or not) and `compacted-*.jsonl` in the directory and merges them, never one segment path.

## Writing

`WorkJournal::record` never blocks and never touches a file: it stamps the event's `ts_ms` and hands it to the run's **writer thread** (`work-journal`) through a bounded queue of 4096 events (`QUEUE_CAPACITY`). The writer assigns `seq` as it writes, appends the line and follows it with `File::sync_data`, so the file is in the order events were handed over and `seq` has no gaps. When the queue is full the event is **dropped**, counted (`WorkJournal::dropped_events`) and logged with a `warn!` at most once a minute; a slow or stalled disk therefore costs journal lines, never a spawn, a report's acknowledgement or its wake effects. Because `record` only queues, the tracker calls it under its bookkeeping mutex (which is what orders an attempt's end after its start), and no bookkeeping lock or state write guard is ever held across file I/O. The filesystem reads a `workspace` needs are made before that mutex is taken.

`WorkJournal::try_record` is the blocking form (it waits for the write and answers its `seq`), for recovery and tests only. A graceful daemon shutdown (`shutdown_all_graceful`) waits at most 2 seconds (`SHUTDOWN_DRAIN`) for the queue to drain, and dropping the last handle waits as long for the writer to finish and release the run lock.

## Line schema, version 1

Every line is one ASCII-escaped JSON object (non-ASCII written as `\u` escapes, as `retained_store` writes) ending in `\n`, at most 4096 bytes before the newline:

```text
{"v":1,"seq":<u64>,"ts_ms":<u64>,"run":"<run>","kind":"<kind>","work":"w-<deck id>-<16 hex>", …kind fields}
```

- `v` is the schema version, `1` in this release.
- `seq` counts from 1 within one run's writing; `ts_ms` is Unix milliseconds; `run` is the writing daemon's instance id.
- `work` is a **work id**, `w-<deck id>-<16 lowercase hex>`: the deck id from `<state_dir>/deck-id` (so ids are unique across machines) and 8 bytes of OS randomness.
- An **attempt id** is `<run>/<agent id>`: the registry's agent id of one generation, under the run that spawned it.

| `kind` | Fields (`?` = optional) |
| --- | --- |
| `opened` | `launch`: `pane`, `orchestration`, `delegation`, `dispatch`, `schedule_fire` or `issue_dispatch`; `parent`?: a work id; `refs`: an object with any of `issue` (number), `repo` (`owner/name`), `branch`, `pr` (number), `task_id`, `unit_id`, `orchestration_id`, `orchestration`, `role`, `schedule`, `pane`; `workspace`?: a path |
| `attempt_started` | `attempt`; `pane`; `agent` (the agent id, a number); `role`? |
| `attempt_ended` | `attempt`; `how`: `exited`, `closed`, `replaced`, `replacement_failed`, `interrupted` or `unknown`; `code`?: the exit code, with `exited`; `cause`?: `daemon_shutdown` or `daemon_gone`, with `interrupted` |
| `linked` | `refs`, the same shape as `opened.refs` (defined, not emitted yet: see below) |
| `reported` | `report_id`; `task_id`?; `unit_id`?; `destination`: `inbox` or `recovery`; `to`?: the recipient's work id, when the deck knows it |
| `closed` | `how`: `pane_closed`, `unit_ended`, `task_done`, `fire_done` or `orchestration_ended` |

`work` on every kind is the work the event belongs to.

### Reader rules

These ship in the first release, so a later schema can add to the journal without breaking this build's reader:

- A line whose `v` is greater than the reader knows is skipped.
- A line whose `kind` the reader does not know is skipped.
- Unknown fields are ignored.
- A complete line that cannot be parsed is skipped, wherever it is in the file.
- A final line with no `\n` is a torn write and is dropped.
- A line over 4096 bytes is skipped without being buffered.
- Lines are merged across files in `(ts_ms, run, seq)` order, and a second line with the same `run` and `seq` is a duplicate and dropped (a compacted chunk and a file it was folded from whose unlink failed can both be present).

`read_journal` and `WorkJournal::read` apply all of these and return what they skipped in `ReadStats`. Recovery rewrites no file. Compaction carries a line it cannot read into its output byte for byte, up to 10,000 such lines per round (`carried_unreadable`); past that, and for a line over 4096 bytes or a torn tail, the line is discarded and counted in `CompactReport::discarded` (see [Budgets](#budgets)).

## What each launch mints

| Launch | Work | Parent | Opened |
| --- | --- | --- | --- |
| Pane (`StartAgent`, `StartPreparedAgent`, saved-session restore) | one per pane, minted at spawn, with `refs.pane` and `workspace` = the pane's cwd | none | when its first attempt is published, so a refused spawn writes nothing |
| Orchestration | one per instance id, minted the first time the daemon sees the instance, with `refs.orchestration_id`, `refs.orchestration` and `workspace` | the dispatch's or the scheduled fire's work that started it, else none | at its first published role pane |
| Delegation | one per delegated task, with `refs.task_id` and `refs.role` (no `workspace`) | the orchestration's work | once the delegate is posted (see [Delegations](#delegations)) |
| Dispatch (`dot-agent-deck dispatch`, either shape) | one per unit, with `refs.branch`, `refs.unit_id` and `workspace` = the worktree | the dispatcher pane's work, resolved daemon-side from the attested caller | after the unit is registered and its id known, before its task is posted |
| Scheduled fire | one per fire, `launch=schedule_fire`, with `refs.schedule` and `workspace` = the schedule's working directory | none | when the fire runs |
| Scheduled issue dispatch | one per issue, `launch=issue_dispatch`, with `refs.issue`, `refs.repo`, `refs.branch` and `workspace` = its worktree | the fire's work | at its spawn's first published pane, so a spawn that starts nothing writes nothing |

**Which work an attempt serves.** A pane that belongs to an orchestration instance is an attempt of the orchestration's work, with `role` on `attempt_started`, and gets no pane work of its own. A single-agent dispatched unit's generations are attempts of the dispatch's work, and a plain fire's spawned pane is an attempt of the fire's work. A scheduled issue dispatch's panes are attempts of the issue's work when it is single-agent; when it is orchestration-shaped, they are attempts of an orchestration work that is a **child of the issue's work**, by the orchestration rule above, so the issue's refs are one `parent` link up from the attempts. A respawn of the same pane (a `clear = true` delegate, `pane restart`, a re-create) keeps the work its previous generation served and is a new attempt.

### Delegations

The task id is the message id `dispatch_one_owned` mints before the post, so the delegation's work is minted and registered under that task id before `post_reserved_delegate` runs, with no hub lock held (`WorkTracker::delegate_pending`). Its `opened` and the `attempt_started` naming the bound worker generation are held in memory, and:

- written as soon as the post goes in (`delegate_posted`), whether the task is still live or was retired between the inbox accepting the post and that callback (a later post that superseded it called back first, its worker generation ended, its orchestration ended): the inbox accepted it, so it ran. A retired task whose events are still held is not evicted from the 64-entry ring of retired tasks; the ring evicts its oldest settled entry instead, so held entries are bounded by the posts in flight;
- written first, under the tracker's lock, if the worker's `work-done --task-id` is committed before that (a report that races the post);
- dropped if the post is refused (`delegate_refused`), live or retired, so a refused delegate leaves nothing in the journal, as a refused spawn does.

The worker's report is then `reported` on the delegation's work with `task_id`, `report_id`, `destination` and `to` = the orchestration's work, followed by `closed how=task_done`.

### Reports

`reported` means a report the worker or unit itself sent with `work-done`, written after the report is committed (`task_completion.rs`, `journal_report`), from both the live path and the supervisor's recovery path:

- a task's report goes on the delegation's work, `to` = the orchestration's work;
- a unit's terminal report (`work-done --done`) goes on the dispatch's work, with `unit_id`, `to` = the dispatcher's work;
- a note goes on the producer's work, `to` = the recipient's work when known.

`destination` is `inbox` when the report was posted to the recipient's inbox and `recovery` when it was set aside as a recovery row. The deck's own "dispatch ended" notice for a unit that ended without reporting is **not** a `reported`; that unit's work gets only `closed how=unit_ended`.

## Attempts and how ends map

`attempt_started` is written as `spawn_agent_with_dir` publishes the generation, under the registry lock that publishes it, so a stop (which can only find the generation once it is published) closes the pane's work after its start is booked. `attempt_ended` is written in `pump_reader` after `capture_termination`, mapped from the generation's `Termination`:

| `Termination` | `how` |
| --- | --- |
| `PaneCommandExited` | `exited`, with `code` |
| `Closed` | `closed` |
| `Closed` while the registry is shutting down | `interrupted`, `cause=daemon_shutdown` (best effort: the reader threads are detached and may not run before the process exits) |
| `Replaced` | `replaced` |
| `ReplacementFailed` | `replacement_failed` |
| `ReplacementPending`, `ConnectionEnded`, `Unknown` | `unknown` (none of them says how the attempt ended) |

An end that arrives before its start is held (at most 64) and written right after the start.

Works close as follows: a pane's own work `pane_closed` when its pane is stopped or closed; a dispatch's work `unit_ended` when its last pane goes (a dispatch with no caller registers no unit, so its work closes as a scheduled issue dispatch's does, below: `WorkTracker::launched_without_unit`); an orchestration's work `orchestration_ended` when its instance has no records left; a delegation's work `task_done` after its report; an issue-dispatch fire's work `fire_done` after the run finishes; a scheduled issue dispatch's work `pane_closed` with its pane when it is single-agent, and `orchestration_ended` when the orchestration it started ends when it is orchestration-shaped (right away when that pane or orchestration is already gone by the time its spawn returns). A plain fire's work closes `fire_done` with the pane it spawned when that pane is still serving it once the fire has run (`WorkTracker::fire_settled`), and right away otherwise: a fire that re-posted into a pane an earlier fire opened (`new_tab_per_fire = false`, so it has no attempts), one that opened an orchestration (whose own work carries on as its child), one whose spawn failed, and one whose pane already closed.

## The daemon's bookkeeping is bounded

`WorkTracker`'s in-memory book holds entries only for what is live, and every entry is retired on a terminal path of what it tracks, so a long-lived daemon does not accumulate it:

- **A spawn** gets a `SpawnWork` guard from `resolve_spawn` and holds it to the end of `spawn_agent_with_dir`. Dropped before the spawn's first attempt is published (a refused duplicate pane, an error after the fork, an unwind), it forgets the pane work or orchestration work the spawn minted. Spawns of one orchestration share its unpublished work through a count, so a failed role spawn never removes a work another role is still starting or has already published.
- **A delegated task** is retired on its report, on its refusal, when a newer task's post supersedes it, when the worker generation it was bound to ends, and when its orchestration ends. Supersession is by id: the post retires exactly the tasks the inbox's task ledger settled as superseded by it (`PostedDelegate::superseded_tasks`), so a newer task to the same pane that is still on its way to being posted is never retired by an older one's post, whichever post completes first. Tasks retired without a report sit in a ring of the 64 most recent (`MAX_RETIRED_TASKS`), so a report already on its way still finds its work; a settled task evicted from that ring is remembered as task id → its work and its orchestration's work only, up to 4096 of them (`MAX_SETTLED_TASKS`, oldest first out), so a late report of a task superseded long before is still written on its work.
- **A pane** is forgotten when it is closed: by a stop (`stop_agent_steps`), and by a spawn's rollback (`close_started_agents`, when a later orchestration role fails or a pane's first prompt is refused), each once its close is confirmed. So a plain fire whose pane was rolled back is closed `fire_done` at once rather than waiting on a pane that is gone.
- **A dispatch's or a scheduled issue dispatch's work** is held (`WorkTracker::hold_launch`) until its spawn settles. A spawn that fails having written nothing of it forgets it. One that fails after something of it was written (an attempt of its own, or the orchestration it started, whose `opened` names it as parent, before a later role failed) writes its `opened`, so that child's parent can be found, and closes it: `unit_ended` for a dispatch, and as what it started ended for an issue dispatch. A unit's entry goes when the unit ends.
- **An end that arrives before its start** is held, at most 64.

## Restart recovery

At startup, in `run_daemon_with`'s state-dir block after the retained store opens and before the inbox is installed and the scheduler starts, the daemon:

1. opens the journal for its run;
2. starts recovery (`WorkJournal::start_recovery_in_background`) on its own thread, so the async startup path does not wait for it (only when that thread cannot be started does the first attempt run in place): it reads every file, streaming each one line by line, and for every `attempt_started` of a dead run with no `attempt_ended` anywhere in the journal, appends `attempt_ended {how: interrupted, cause: daemon_gone}` to **its own** segment, with `work` = that attempt's work;
3. starts compaction off the startup path.

Recovery never rewrites a dead segment, and **never runs without `maintenance.lock`**, held exclusively. It runs on a background thread (`work-journal-recovery`): at once, and when another daemon's recovery or compaction (or a reader) holds the lock, again every 500 ms (`RECOVERY_RETRY`) until it gets it. This run's compaction is skipped (`CompactReport::skipped_recovery_pending`) until that first attempt has run. The skipped round is not left to the next hourly tick: the compaction task (`spawn_compaction`) waits for that first attempt on the blocking pool, in one-second slices (`WorkJournal::wait_recovery_attempted`), and runs the round as soon as the attempt has run. Recovery may therefore run while this run's own agents are live, which is safe: it only ends attempts of dead runs, whose segments this run never writes.

An attempt that gets the lock ends one of three ways (`RecoveryReport`): **completed** (`completed()`), **incomplete** (over its budget: it wrote nothing), or **failed** (an I/O error, kept in `error`, or an interruption line the writer could not write). Only a completed recovery ends the phase. An incomplete or failed one (`RecoveryStart::Retrying`) is tried again on the same background thread, after a wait that doubles from 500 ms up to 5 minutes (`RECOVERY_BACKOFF_MAX`), and right after every compaction round that folded files, since folding is what brings an over-budget journal back within recovery's budget. Compaction does not wait for an incomplete recovery (it would otherwise never get the chance to shrink the journal). The interruptions are therefore written later in the same run, not only at the next start. A deferred recovery runs while this run's own agents are live, which is safe: it only ends attempts of dead runs. After a SIGTERM an attempt's end may read `daemon_shutdown` (written by the dying daemon) or `daemon_gone` (written by the next one's recovery); both are correct.

No report is recorded against a task posted before the restart. The new daemon's tracker has no record of the old run's task ids, so `WorkTracker::reported` writes nothing for one; and a report from an agent of the old daemon, one that detached from its PTY and survived the restart, carries a hook capability token the new daemon has no record of, which it refuses (`Refusal::UnknownToken`, under every `DOT_AGENT_DECK_HOOK_PROVENANCE` policy). Such a task's attempts still open at the restart end `interrupted` instead, written by the dying daemon (`daemon_shutdown`) or by the new one's recovery (`daemon_gone`).

## Retention and compaction

Compaction runs once right after startup, in a spawned task on the blocking pool, and then hourly. Each round takes `maintenance.lock` exclusively without waiting (it skips the round when anyone holds it, a reader included). It may fold every dead run's files, every **sealed** segment (a live run's segment below its highest-numbered one, this run's own included) and every compacted chunk, into new chunks of its own run, and deletes what it folded. A live run's highest-numbered (writable) segment is read, so a work with a line there is kept, but never touched. So a daemon that stays up has its own finished history compacted and aged out without a restart. Lines are carried byte for byte; a torn tail may be dropped.

A work is **finished** when none of its lines is in a live run's writable segment and every attempt it started has an `attempt_ended`, whether or not it was `closed`: a pane work whose daemon went away is never closed, but recovery ends its attempts, and it must not be kept forever. There is one exception: a work that a **live** run started an attempt of and has not closed is not finished, unless it is a delegation (which gets no new attempt). That run may still serve it: a pane whose agent exited keeps its work until it is closed, and restarting it writes a new attempt of the same work, which must still find its `opened`, workspace and parent. It becomes finished once the run closes it or is gone. Compaction drops a finished work whose last event is older than 30 days (`RETENTION_AGE`) and keeps at most the 10,000 most recent finished works (`RETENTION_FINISHED_WORKS`). In both cases a work a kept work needs is kept too: its ancestors, and a work whose line ends an attempt the kept work started (a delegation's attempt is its worker generation, whose end is written on the orchestration's work). A work that is not finished is kept.

A round makes two streaming passes:

1. **Index.** It reads every file and keeps only a per-work index: when the work was last seen, its parent, the attempts it started and which work's line ended each, whether a line of it is in a writable segment, and which files hold its lines. The index is bounded by the compaction budget's entries and is not byte-budgeted: it reads every file, full chunks included, until it is full. When the index is over that budget, or the directory listing was truncated, the round **drops no work and removes no lock** (`CompactReport::retention_skipped`): a line it did not read could belong to, be a child of, or pin a work it would drop. It still folds files, which brings the journal's file count and bytes back within the rewrite's budget over rounds.
2. **Rewrite.** It picks files within the round's file and byte budget: first the files of the oldest works retention drops, each together with the files of every work that needs it, then segments and chunks under half the chunk size. A work is dropped only in a round that rewrites every file holding a line of it, so it is never half removed. Each picked file is streamed into new chunks; what a round does not pick waits for a later one (`CompactReport::over_budget`). A full chunk with nothing to drop is not rewritten, so no round copies the same full chunk again (the index pass still reads it every round). While the index fits its budget, a round with something to drop rewrites the files of the oldest works it drops first, so the journal comes down to the retention limits over rounds, in files and bytes, however far it is over one round's rewrite budget. A work whose files alone exceed the budget is folded on its own, over budget, when nothing else could be dropped.

**Index saturation is an accepted limitation.** The index needs an entry for every distinct work, attempt and file reference in the journal. When there are more than the budget's entries, which at the defaults (500,000) is roughly 125,000 finished works with one attempt each (a work, its attempt, the reference to that attempt and one file reference: four entries apiece), the index is full every round: retention is skipped (fail closed, as above), no dead run's lock is removed, and `CompactReport::index_saturated` is set, with a `warn!` at most once a day naming the condition. Folding alone does not cure it, because packing the lines into fewer files leaves the same number of distinct works and attempts; finished works stop aging out until the budget (`Budgets::compaction.entries`) is raised. Curing it in place would need a different indexing strategy (for example partitioned passes over the works), which this PRD does not build.

## Budgets

Every file is streamed and a line over 4096 bytes is never buffered. A read keeps the events it returns; recovery keeps an index of attempts; compaction keeps its per-work index (bounded by entries, not bytes) and, while it rewrites, a de-duplication set of at most the budget's entries `(run, seq)` keys and the unreadable lines it carries (`CompactReport::held_peak`). A listing keeps at most 65,536 names. Reading, recovery and compaction each work within a `Budget` of files, bytes and entries (`Budgets`, in `JournalOptions`). The budgets limit the input and memory a call takes on, so a huge or hostile directory, many short junk lines, many files or one enormous line cannot exhaust the daemon's memory; they do not bound wall-clock time, and the startup recovery still reads and writes the disk (each interruption it writes waits for its `sync_data`), on its own thread.

| Who | Default budget | Over budget |
| --- | --- | --- |
| A read (`read_journal`, `WorkJournal::read`) | 4096 files, 256 MiB, 500,000 lines | answers what it read, with `ReadStats::truncated` set and a `warn!` |
| Recovery | 4096 files, 512 MiB, 500,000 attempts | writes **nothing** that attempt (`RecoveryReport::incomplete`), since an end it did not read could make an interruption it wrote false, and is retried (see [Restart recovery](#restart-recovery)) |
| Compaction | 1024 files and 128 MiB rewritten per round; 500,000 index entries; 500,000 de-duplication keys per round | rewrites what fits and leaves the rest for a later round (`over_budget`); over its index budget it drops no work and removes no lock (`retention_skipped`, `index_saturated`), which lasts until the budget is raised (see above) |

Compaction also carries at most 10,000 unreadable lines (`carried_unreadable`) per round; the rest are discarded as they are read, never collected, and counted in `CompactReport::discarded`, so a directory full of junk shrinks instead of being carried forever.

## Redaction

- Refs, ids and paths only. No prompt, report or terminal text is written: `reported` carries a report id, and a dispatch's free-text name is not recorded (its slug is, in the branch).
- Only `workspace` may hold a filesystem path. A path under the home directory is written with the home prefix as `~` (`~/code/x`), and the home directory itself as `~`; any other path is written in its lexical form (below).
- The path is judged **as resolved**, not as spelled: `.` and `..` are resolved lexically in both the path and the home, and the path's canonical (symlink-resolved) form is compared against the home's lexical and canonical forms where they resolve. So `/home/../home/u/x`, `/home/u/./x` and a symlinked home reached by its target's spelling are all written `~/x`. The home's canonical form is computed once, when the journal opens. When the path's canonical form cannot be read for a reason other than its absence (a permission error, say), no privacy-safe spelling can be established and `workspace` is left out of the event.
- Every other string field is refused when it looks like a path (absolute, `~`-relative, a Windows drive path, or one containing either spelling of the home directory). Role, schedule and orchestration names get no stricter grammar: they are not paths into the home directory.
- Every string field is refused when it contains a control character (including `\n`, `\r`, `\t`) or is longer than 1024 bytes.
- A refused event, or a line over 4096 bytes, is dropped with a `warn!`, never truncated. A ref the rule refuses is left out of the event the tracker builds rather than dropping the whole event.

## The work id outside the journal

- **`DOT_AGENT_DECK_WORK_ID`.** With the journal enabled, every agent the deck spawns gets the id of the work its attempt serves, set beside `DOT_AGENT_DECK_AGENT_ID` after stripping any inherited or replayed value, whether or not telemetry is configured. An orchestration role's generation, including one respawned by a `clear = true` delegate, gets the orchestration's work id, never the delegation's: the delegation's link to that generation is its own `attempt_started`. The id is pinned on the generation's `AttestFacts.work`, which is how a respawn keeps it and how `dispatch` resolves the caller's work.
- **`deck.work.id`.** With telemetry configured, the same id is on the run's `agent.run` span and in the agent's `OTEL_RESOURCE_ATTRIBUTES` (`telemetry::propagation::agent_env`), so the agent's own telemetry carries it. It is not a resource attribute of the deck's own exporter, and `agent.handoff` does not carry it.
- **When there is no id.** A disabled journal (its directory unusable) still mints work ids, so both the variable and the attribute are set as usual; the work is just not recorded. The daemon always installs a journal, disabled or not, so the id is absent only when minting fails, which needs the operating system's random source to fail, or on a registry the daemon did not set up (a unit test's).

## The authority boundary

Git and the forge stay authoritative: Git for code, branches and whether a worktree exists; GitHub for what was asked (issue text), what happened to a change (PR state) and claims between people and machines. The journal is authoritative only for what the deck itself ran: which attempts ran, where, under which parent, how they ended and where their reports went. It is observational about whether the work is done. Anything that acts on it is to re-check Git and the forge first, as the scheduler's skip checks do today ([work-identity.md](work-identity.md#decision-revise)).

## Orderings a reader must not assume

- **`opened` before a work's attempts.** A single-agent dispatched unit's `attempt_started` is written before the dispatch's `opened`, because the unit id `opened` carries is minted after the unit's panes spawn.
- **A predecessor's end before its successor's start.** On a respawn the successor's `attempt_started` can precede the predecessor's `attempt_ended`, because the old reader thread ends after the successor is published.
- **A delegation's start before its worker's end.** When the worker generation ends between the inbox accepting a delegate and the tracker's callback for that post, the delegation's `attempt_started` is written after that generation's `attempt_ended` (which is on the orchestration's work). Key ends by attempt id, not by line order.
- **One work per attempt id.** A delegation's `attempt_started` names the worker generation, which is also an attempt of its role's (the orchestration's) work. That attempt's single `attempt_ended` names one of the two works. Recovery and compaction key ends by attempt id across the whole journal, and a reader should too.
- **File order.** Files are merged by `(ts_ms, run, seq)`; the order of files in the directory means nothing.

## What is deliberately not recorded

- **`linked` is defined but not emitted.** Linking a work to a pull request after the fact is left to a later PRD; until then a work's `pr` ref is absent and the branch is the join key.
- **No issue on interactive dispatch.** No issue number is parsed out of a branch or a dispatch name; only a scheduled issue dispatch, which knows its issue, records `refs.issue`.
- **No text.** No prompt, report body, terminal output or dispatch name, and nothing on the hook-ingest path or per tool call: the journal holds lifecycle events only.
- **A fire that reuses an existing pane** (`new_tab_per_fire = false`) has a fire work with no attempts; it is `closed how=fire_done` right after the re-post.

## For the PRDs that build on it

Every consumer reads through the shared reader, and **checks `ReadStats::truncated` before inferring that something is absent or finished**: a truncated read stopped at its budget, so a missing `attempt_ended`, `closed` or work there means "not read", not "not written".

- **#1890, interrupted work after a restart.** Read the whole directory; a work is interrupted when an attempt of it ended `interrupted` (either cause) and the work has no `closed`. To find the issue to resume, start from the interrupted attempt's work and **walk up its `parent` chain to the nearest work whose `opened` carries `refs.issue`**: for a single-agent scheduled issue dispatch that is the work itself, and for an orchestration-shaped one it is the orchestration work's parent. That work's `refs.issue`, `refs.repo` and `refs.branch` say which issue to resume, and the authority boundary above says to re-check the branch and issue before acting.
- **#1860, per-work totals.** Join telemetry on `deck.work.id` (on `agent.run`, and on the agents' own data through `OTEL_RESOURCE_ATTRIBUTES`) and walk `parent` links for roll-ups: a delegation's parent is its orchestration, an orchestration's is the dispatch or fire that started it, a dispatch's is the dispatcher's work.
- **#1790.** Use the same reader (`read_journal` or `WorkJournal::read`, which take the shared maintenance lock and the read budget) rather than a second parser, so the skip rules above stay in one place, and key on work ids and attempt ids rather than on panes, which are reused. A truncated read cannot show that a work is done.
