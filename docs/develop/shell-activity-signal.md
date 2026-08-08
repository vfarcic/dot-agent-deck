# The shell-activity signal and its coupling to another product's process shape

This note exists because the "a pane reads `Working` while its agent's shell command runs" signal (PRD #370's goal, PRD #386's mechanism) is built on an **observed behaviour of Claude Code, not on a documented interface**. That coupling is fine — it is the only instrument available — but it has two failure modes that are invisible from inside this repo, and this page records what they are, what would detect them, and what is inference rather than measurement.

## What the signal actually tests

The daemon's `run_shell_activity_monitor` (`src/daemon.rs`) polls every live pane every 500 ms. For each pane it samples the whole process table once (`process_table()` — one `ps -A -w -w -o pid=,ppid=,tty=,args=` per poll cycle, parsed once and reused across panes) and hands it to `descendant_shell_activity()` (`src/platform/proc/scan.rs`), which walks the pane's PTY child's transitive descendants and asks one structural question per candidate:

> is this descendant in a **different POSIX session** than the agent — `getsid(descendant) != getsid(agent_pid)`?

That is the whole primary test. It reads no argv and no controlling-terminal flag, and it compares against the *agent's own* session id rather than any constant, which is what keeps it meaningful in a container where the agent itself has no controlling terminal (a bare "descendant has no tty" test matches everything there and pins every pane at `Working` — see PRD #386's "CI trap" section).

It answers correctly for exactly one reason: **Claude Code `setsid`-detaches its Bash-tool child into a session of its own**, while every other child of the agent — MCP servers, `caffeinate`, plugins — stays in the agent's session on the pane's tty. Measured on 2026-08-06 against Claude Code 2.1.220.

## Failure mode 1 — Claude Code stops `setsid`-ing: a total, silent false negative

If a future Claude Code release spawns its Bash-tool child inside the agent's own session, `getsid(descendant) == getsid(agent)` for every descendant, no candidate ever matches, and the signal simply never fires again. Nothing is logged, no error surfaces, and no test that uses a fixture can notice: a captured process table keeps the old shape forever. The pane goes back to reading `Idle` during long commands — the exact bug this mechanism exists to repair — and the only symptom is silence.

**What detects it:** `status/shell-activity/005`, the rot canary. A real interactive Haiku agent is prompted to run a real ~20-second `ping`, and the test asserts that the daemon synthesizes a `ShellBusy` event for that pane. It asserts on the *event*, never on the badge, because the badge already reads `Working` from `ToolStart` whether or not this mechanism is alive — asserting on the badge is precisely how #370 shipped green while dead.

## Failure mode 2 — something else starts detaching itself: a false positive, and worse than the bug

The inverse is the load-bearing assumption of the design. If an MCP server, plugin, or hook `setsid`s *itself*, it is a descendant in a session of its own for as long as it lives, so the pane pins at `Working` forever. That is **worse** than the stale `Idle` it replaces, because it is unfalsifiable to the user: a permanently-busy pane looks exactly like a busy pane.

The assumption that nothing else detaches was measured **once, on one machine, with one MCP configuration** (`context7`, `task-master`, `engram`, `pysemgrep`, `caffeinate` — all in the agent's session). Nothing guarantees the next MCP server behaves that way.

**What detects it:** `status/shell-activity/007`. A real agent is brought up and left at its idle prompt with its real MCP servers alive as children, and the pane's rendered badge must read `Idle`. It checks only *this* machine's configuration, which is the honest limit of the check.

## How rot is detected in practice — and when it is not

Both canaries are **real-agent e2e tests**. They need Claude credentials, and **CI has none**, so both self-skip there and report green. They only ever fire when someone runs the e2e tier locally (`cargo test-e2e shell_activity_005` / `…_007`, CLAUDE.md rule 5 exception (a)).

The practical consequence: **a green CI run says nothing at all about whether this signal still works.** Rot is caught at the pre-PR local e2e run, or not at all. If you are touching the scan, the monitor, or the pane primitive, run those two tests locally — that is the only place the coupling is checked against reality.

## What is measured and what is inference

Only **Claude Code's** shell-tool shape was ever measured. Codex, OpenCode, Pi and `dot-agent-deck wrap` are inference: the session-id test asks the right question for them (it compares each agent against its own session, needing no per-agent data), but whether their shell children detach at all is unknown. If they do not, the signal is simply never emitted for those panes.

**The failure mode to watch for across all of them is silence, not noise.** A pane that never reads `Working` during a long command looks identical to a pane that had nothing to run. There is no log line, no metric, and no test that fails.

Two smaller gaps belong in the same list. On **Windows** `process_table()` returns `None` unconditionally, so the signal never fires there at all — a documented gap, not a bug. And if Linux **sandboxing** (bwrap / PID namespaces) is ever enabled for the Bash tool, the payload runs in its own PID namespace and a host-side descendant walk may not enumerate it; neither predicate is known to survive that, and it needs its own measurement before anyone relies on it.

## The argv cross-check is a per-agent veto, not the signal

The Bash-tool **argv shape** (`shell-snapshots/snapshot-` plus `&& eval `, with `\builtin unalias -- 'unsetenv'` covering the no-snapshot variant) is kept as a *secondary* check, because the two predicates fail on disjoint sets: the structural test dies if Claude stops `setsid`-ing and false-positives on a self-detaching MCP server, neither of which touches the argv; the argv test dies on prologue rewording, `CLAUDE_CODE_SHELL_PREFIX`, sandbox mode and the missing-snapshot variant, none of which touches the session id.

It is applied as a **veto**: a structurally-busy candidate is discarded unless it also matches one of the shapes it was handed. Which shapes a pane gets is selected **per agent kind**, not globally:

- `crate::platform::proc::MEASURED_SHELL_TOOL_SHAPES` is the catalog of shapes actually measured — today exactly `[CLAUDE_BASH_TOOL_SHAPE]`. The daemon passes the whole catalog and decides nothing itself.
- `AgentPtyRegistry::shell_foreground_busy_snapshot` selects from it per pane via `shell_tool_shape_key` (`src/agent_pty.rs`), keyed on `RunningAgent::agent_type`: `AgentType::ClaudeCode` → the measured Claude shape; **everything else, including `None` → `&[]`**, i.e. the structural session-id test alone.
- `None` maps to `&[]` deliberately rather than to "assume Claude". A pane spawned through a launcher (`devbox run codex-big`) carries `agent_type == None` because `AgentType::from_command` cannot see through the launcher, so treating unknown as Claude would apply Claude's argv veto to exactly the panes least likely to carry it — turning an unmeasured agent into a silent false negative, which is failure mode 1 by construction.

If you add a per-agent shape, add it to the catalog **and** to `shell_tool_shape_key`, and only after measuring it against that agent — an invented shape is a veto that silently kills the signal for the agent it claims to support.

## Where the measurements live

The numbers behind all of the above were taken on 2026-08-06 and are recorded in PRD #386 (`prds/386-descendant-scan-shell-activity-signal.md`), which reproduces the `getsid` table, the confounder list, the rejected argv predicates, and the Linux/container checks. The raw working notes (`.dot-agent-deck/370-diagnosis-notes.md`, `.dot-agent-deck/hook-silence-notes.md`, `.dot-agent-deck/386-argv-notes.md`) are gitignored and machine-local; the PRD is the durable record.
