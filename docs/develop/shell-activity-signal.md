# The shell-activity signal and its coupling to another product's process shape

This note exists because the "a pane reads `Working` while its agent's shell command runs" signal (PRD #370's goal, PRD #386's mechanism) is built on an **observed behaviour of Claude Code, not on a documented interface**. That coupling is fine — it is the only instrument available — but it has two failure modes that are invisible from inside this repo, and this page records what they are, what would detect them, and what is inference rather than measurement.

## What the signal actually tests

The daemon's `run_shell_activity_monitor` (`src/daemon.rs`) polls every live pane every 500 ms. Each tick first resolves the candidate panes under the registry lock (`AgentPtyRegistry::shell_activity_candidates`), then — only if there is at least one — samples the whole process table once (one `ps -A -w -w -o pid=,ppid=,tty=,args=` per poll cycle, parsed once and reused across panes) and hands it to `descendant_shell_activity()` (`src/platform/proc/scan.rs`), which walks the pane's PTY child's transitive descendants and asks one structural question per candidate:

> is this descendant in a **different POSIX session** than the agent — `getsid(descendant) != getsid(agent_pid)`?

That is the whole primary test. It reads no argv and no controlling-terminal flag, and it compares against the *agent's own* session id rather than any constant, which is what keeps it meaningful in a container where the agent itself has no controlling terminal (a bare "descendant has no tty" test matches everything there and pins every pane at `Working` — see PRD #386's "CI trap" section).

It answers correctly for exactly one reason: **Claude Code `setsid`-detaches its Bash-tool child into a session of its own**, while every other child of the agent — MCP servers, `caffeinate`, plugins — stays in the agent's session on the pane's tty. Measured on 2026-08-06 against Claude Code 2.1.220.

## What the sample costs, and the two guards around it

The `ps` sample is the expensive part of the mechanism, and the number matters for PRD #386's M5 (the cadence, and the Route A `ps` vs. Route B native-enumeration question). Measured on an idle 16-core Linux box with ~620 processes, release build, 40 samples:

| | per sample | duty cycle at 2 Hz |
|---|---|---|
| Wall time | ~49 ms | ~10% of one core |
| Our own CPU (`getsid` loop + parse) | ~1.4 ms | ~0.3% of one core |

The gap between the two rows is the whole story: ~97% of the cost is the `ps` child's own work and the wait on it, not anything this process computes. Two consequences shaped the code (issues #493 and #429).

**The sample is conditional.** `process_table()` used to be the first statement of `shell_foreground_busy_snapshot`, so the cost above was paid unconditionally — a daemon with zero panes forked `ps -A` twice a second to classify nobody, and the daemon's idle shutdown does not bound that (it requires no clients **and** no agents, so a TUI attached with no panes polls forever). Resolving the candidates first makes the sample conditional on there being something to classify. Note the ordering constraint this had to respect: the original sample-first ordering existed so no fork/exec ever ran while the registry lock — which TUI-facing paths also take — was held. That property is now preserved by *dropping* the lock before sampling, which is why `ShellActivityCandidate` carries owned data (including the pane's `shell_pid`) rather than borrowing from the registry.

**The sample is awaited and bounded.** In the daemon it goes through `process_table_async()` (tokio `Command`, `kill_on_drop`) under a 2-second `tokio::time::timeout`. Synchronously blocking on it held a Tokio worker thread for the ~49 ms above every 500 ms, and indefinitely if `ps` wedged in D-state on a stuck filesystem — stalling hook ingestion, client requests and daemon shutdown behind a status signal. `spawn_blocking` is *not* the fix: it relocates the stall to the blocking pool, and because a `timeout` around a `spawn_blocking` handle does not cancel the thread, a permanently-wedged `ps` at 2 Hz would leak one pool thread per tick up to the 512-thread cap. Awaiting an async child frees the worker and makes the deadline real.

**The deadline bounds the wait, not the child.** This distinction is easy to get backwards and the natural-looking version is wrong. `timeout(d, sample())` followed by dropping the expired future would abandon the `ps` and start a fresh one next tick — but a process in uninterruptible sleep does not act on the `SIGKILL` that `kill_on_drop` sends until it leaves D-state, so the abandoned `ps` stays on the process table and the retry adds another one every 2.5 s (~24/minute), turning a stalled signal into a pid leak. The monitor instead **retains** the overrunning future and re-awaits it on the following tick, which holds the invariant that *at most one `ps` child exists at a time*. Retention is unconditional, including on a tick with no candidates (which simply does not poll it): dropping it there looks tidier but reopens the accumulation path through pane churn, and a paneless daemon starts no sample at all anyway. A sample that answers "failed" is finished, so it is dropped and the next tick starts fresh. The overrun is logged once per wedged sample, not once per tick.

**A retained sample is trusted only while its table is fresh, and only about the panes it could have seen.** Retention is what makes a *late* answer possible, so it needs two guards of its own, and the failure they prevent is the same one in both cases: a pid is only a name for a process *at a moment*, and a table taken at one moment being applied at another can attribute one process's descendants to a different pane. Under pid reuse that is exactly what happens, and it is worse than it sounds, because the monitor's `last_known` has no entry for a newly-appeared pane — so the wrong reading is a transition, and emits immediately rather than being swallowed as a non-change.

- **Freshness.** `MAX_TABLE_AGE` (3 s) is compared against the sample's start; an older answer is discarded rather than classified. Without it, a wedge that outlasts every pane and then recovers would classify today's pids against a table from before they existed. A healthy sample answers in ~49 ms and a heavily loaded one in a few hundred, so this never trips in normal operation.
- **Identity.** A bound is not an identity check: a pane can be replaced *inside* the 3 s window. So on the resumed path the monitor classifies only panes whose `(pane_id, shell_pid)` pair is unchanged since the sample began. A respawn in the same slot keeps the pane id but takes a new pid; a fresh pane brings a new pane id — either way the pair differs and the pane waits for the next sample, which is the honest answer, since this table predates it. A sample started on the current tick has an identical set by construction, so the common path pays nothing.

Discarding an *answered* sample is free and cannot accumulate — that child is already finished, unlike an abandoned un-answered one. That is why these guards do not conflict with the retention above: retention refuses to abandon an *un-answered* sample; these refuse to believe an *answered* one that has aged out or is being asked about the wrong pane.

**A timed-out sample means "no opinion", never "not busy".** This is the one decision in the monitor that is not recoverable from the code, so it is commented at the call site. A wedged `ps` says nothing about the panes; they are exactly as busy as they were a moment earlier. Reading a blown deadline as `Some(false)` would synthesize a `ShellIdle` for every live pane and flip them all to `Idle` — the stale-`Idle` bug this whole mechanism exists to fix, reintroduced with a new trigger. Both a failed and a timed-out sample therefore skip the tick entirely, leaving the monitor's `last_known` edge-detection map untouched (clearing it would make every pane look new on the next good sample and re-emit a spurious edge for each one). A timeout logs a warning; that log line is the only trace such a tick leaves.

## Failure mode 1 — Claude Code stops `setsid`-ing: a total, silent false negative

If a future Claude Code release spawns its Bash-tool child inside the agent's own session, `getsid(descendant) == getsid(agent)` for every descendant, no candidate ever matches, and the signal simply never fires again. Nothing is logged, no error surfaces, and no test that uses a fixture can notice: a captured process table keeps the old shape forever. The pane goes back to reading `Idle` during long commands — the exact bug this mechanism exists to repair — and the only symptom is silence.

**What detects it:** `status/shell-activity/005`, the rot canary. A real interactive Haiku agent is prompted to run a real ~20-second `ping`, and the test asserts that the daemon synthesizes a `ShellBusy` event for that pane. It asserts on the *event*, never on the badge, because the badge already reads `Working` from `ToolStart` whether or not this mechanism is alive — asserting on the badge is precisely how #370 shipped green while dead.

## Failure mode 2 — something else starts detaching itself: a false positive, and worse than the bug

The inverse is the load-bearing assumption of the design. If an MCP server, plugin, or hook `setsid`s *itself*, it is a descendant in a session of its own for as long as it lives, so the pane pins at `Working` forever. That is **worse** than the stale `Idle` it replaces, because it is unfalsifiable to the user: a permanently-busy pane looks exactly like a busy pane.

The assumption that nothing else detaches was measured **once, on one machine, with one MCP configuration** (`context7`, `task-master`, `engram`, `pysemgrep`, `caffeinate` — all in the agent's session). Nothing guarantees the next MCP server behaves that way.

**What detects it:** `status/shell-activity/007`. A real agent is brought up and left at its idle prompt with its real MCP servers alive as children, and the pane's rendered badge must read `Idle`. It checks only *this* machine's configuration, which is the honest limit of the check.

## How rot is detected in practice — and when it is not

Both canaries are **real-agent e2e tests**. They live in `tests/e2e_shell_activity_real_agent.rs`, which since issue #502 is gated `#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]` — so they are lane 2, and lane 1 (the job that runs on every PR) does not compile them at all.

**Lane 2 runs in no CI job at all** — no e2e test reaches a real agent there, and no test credential is registered on this repository (issue #502; [`e2e-lanes.md`](e2e-lanes.md) has the two reasons, and the scope note about the separately credentialed Codex issue-labeler). Both canaries open with `skip_unless!(common::check_claude_available())`, which accepts a non-empty `ANTHROPIC_API_KEY` as a third path beside `~/.claude/.credentials.json` and the macOS Keychain, so a key-only host can run them — but only when a person runs them.

So the practical consequence is worth stating precisely, and it is not uniform across the three `cargo nextest run --workspace` jobs. `status/shell-activity/001`–`004` are fast-tier tests in `tests/shell_activity.rs`, and the Linux `build` and `build-macos` jobs run all four — covering the process-table primitive, the session-id discriminator and the pane-level rising and falling edge. `build-windows` runs only the two that carry no `cfg` because they are pure fixture data — `002`'s `ppid`-cycle termination and `003`'s discriminator — plus a Windows-only `001` variant asserting `process_table()` returns `None`; `001`'s real-grandchild half and the whole of `004` are `#[cfg(unix)]`, so no CI job covers the pane-level edge on Windows, which is consistent with the signal never firing there at all (see the Windows gap below). What no CI job covers is whether the signal still fires for a **real agent**: **a green CI run says nothing about that.** That rot is caught by running these two canaries locally, or not at all. If you are touching the scan, the monitor, or the pane primitive, run `cargo test-e2e-live shell_activity_005` and `…_007` on a machine with real Claude credentials, under `DOT_AGENT_DECK_REQUIRE_REAL_E2E=1` so a missing credential fails instead of silently passing as nextest would otherwise count it.

## What is measured and what is inference

Only **Claude Code's** shell-tool shape was ever measured. Codex, OpenCode, Pi and `dot-agent-deck wrap` are inference: the session-id test asks the right question for them (it compares each agent against its own session, needing no per-agent data), but whether their shell children detach at all is unknown. If they do not, the signal is simply never emitted for those panes.

**The failure mode to watch for across all of them is silence, not noise.** A pane that never reads `Working` during a long command looks identical to a pane that had nothing to run. There is no log line, no metric, and no test that fails.

Two smaller gaps belong in the same list. On **Windows** both `process_table()` and its async twin return `None` unconditionally, so the signal never fires there at all — a documented gap, not a bug. And if Linux **sandboxing** (bwrap / PID namespaces) is ever enabled for the Bash tool, the payload runs in its own PID namespace and a host-side descendant walk may not enumerate it; neither predicate is known to survive that, and it needs its own measurement before anyone relies on it.

## The argv cross-check is a per-agent veto, not the signal

The Bash-tool **argv shape** (`shell-snapshots/snapshot-` plus `&& eval `, with `\builtin unalias -- 'unsetenv'` covering the no-snapshot variant) is kept as a *secondary* check, because the two predicates fail on disjoint sets: the structural test dies if Claude stops `setsid`-ing and false-positives on a self-detaching MCP server, neither of which touches the argv; the argv test dies on prologue rewording, `CLAUDE_CODE_SHELL_PREFIX`, sandbox mode and the missing-snapshot variant, none of which touches the session id.

It is applied as a **veto**: a structurally-busy candidate is discarded unless it also matches one of the shapes it was handed. Which shapes a pane gets is selected **per agent kind**, not globally:

- `crate::platform::proc::MEASURED_SHELL_TOOL_SHAPES` is the catalog of shapes actually measured — today exactly `[CLAUDE_BASH_TOOL_SHAPE]`. The daemon passes the whole catalog and decides nothing itself.
- `AgentPtyRegistry::shell_activity_candidates` selects from it per pane via `shell_tool_shape_key` (`src/agent_pty.rs`), keyed on `RunningAgent::agent_type`: `AgentType::ClaudeCode` → the measured Claude shape; **everything else, including `None` → `&[]`**, i.e. the structural session-id test alone. It happens there because the pane's agent kind is only visible under the registry lock, so that is the one place the selection can be made.
- `None` maps to `&[]` deliberately rather than to "assume Claude". A pane spawned through a launcher (`devbox run codex-big`) carries `agent_type == None` because `AgentType::from_command` cannot see through the launcher, so treating unknown as Claude would apply Claude's argv veto to exactly the panes least likely to carry it — turning an unmeasured agent into a silent false negative, which is failure mode 1 by construction.

If you add a per-agent shape, add it to the catalog **and** to `shell_tool_shape_key`, and only after measuring it against that agent — an invented shape is a veto that silently kills the signal for the agent it claims to support.

## Where the measurements live

The numbers behind all of the above were taken on 2026-08-06 and are recorded in PRD #386 (`prds/386-descendant-scan-shell-activity-signal.md`), which reproduces the `getsid` table, the confounder list, the rejected argv predicates, and the Linux/container checks. The raw working notes (`.dot-agent-deck/370-diagnosis-notes.md`, `.dot-agent-deck/hook-silence-notes.md`, `.dot-agent-deck/386-argv-notes.md`) are gitignored and machine-local; the PRD is the durable record.
