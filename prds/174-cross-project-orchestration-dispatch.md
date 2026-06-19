# PRD #174: Cross-project orchestration dispatch

**Status**: Not Started
**Priority**: Medium
**Created**: 2026-06-19
**GitHub Issue**: [#174](https://github.com/vfarcic/dot-agent-deck/issues/174)
**Related**: PRD #93 (always-external daemon — the daemon is the single spawn authority and signal router this builds on), PRD #107 / #111 (orchestration identity as a `(name, cwd)` tuple — the boundary this feature must route *across*), PRD #58 (multi-role orchestration — the delegate/work-done loop being extended), Issue #140 (concurrent orchestrations / per-user daemon scoping — this feature makes running two orchestrations at once load-bearing by design)

## Problem Statement

Orchestration today is a strict tree confined to a single project. An orchestrator delegates to its own workers (`Delegate`) and workers report back (`WorkDone`), with all routing scoped by the `(orchestration_name, orchestration_cwd)` tuple (`src/state.rs` `pane_orchestration_map`, `handle_delegate` ~613, `handle_work_done` ~719). There is **no path for an orchestrator to obtain work or information from a *different* project**.

Real multi-project workflows need exactly that. A frontend orchestration needs a backend endpoint built before it can wire up the UI; or it merely needs to know the shape of another team's API. A platform orchestration needs a dependency bumped in a sibling service. Without cross-project dispatch, the human has to manually break the chain: stop, go drive the other project's orchestration, come back, resume — defeating the "hand it off and come back when done" premise of orchestration.

The naive fix — letting orchestrators message each other as peers — was considered and **rejected**: a peer mesh introduces cycles (A asks B, B asks A) and deadlock (each waiting on the other's result) with no arbiter to break them. We keep the dependency graph a **DAG** instead: the originator does not talk to a peer, it *becomes the initiator* of a fresh unit of work in the target project, exactly the parent→child shape the delegation model already has.

## Solution Overview

An orchestrator **A** in project **X** issues a *dispatch* into another project **Y**, declaring intent — `info` or `work`. The dispatch resolves Y, spawns the appropriate unit inside it, registers a callback, and **A goes idle** (suspend), exactly like a worker waiting to be kicked off. When Y finishes, its completion is injected as a prompt back into A's pane — the same `write_to_pane_and_submit` mechanism workers already use to wake the orchestrator — and A resumes.

Four ideas carry the whole design:

1. **"Wait" is suspend-and-resume, never a blocking call.** A's dispatch command returns immediately; A finishes its turn and goes idle; Y's completion re-awakens it. A blocking CLI call would park an LLM agent mid-tool-call for the entire duration of Y's work, hit tool timeouts, lose the result if A detaches, and fail to nest (A blocking on Y blocking on Z collapses on any one timeout). Suspend/resume is exactly how the local delegate→work-done loop already works and is detach/reattach safe.

2. **The spawn shape is derived from observable facts, not free agent choice.** Intent (`info`/`work`) plus whether Y defines an orchestration decide single-agent-vs-orchestration and main-vs-worktree (table below). A may override explicitly, but the default is deterministic and inspectable.

3. **Targets resolve from the *originator's* config, and that list is also the authorization allowlist.** X's `.dot-agent-deck.toml` declares a peer map (logical name → local path). "Projects X declared where to find" is exactly "projects X is allowed to dispatch into" — one structure, two jobs. A search fallback locates undeclared targets but never auto-writes into them.

4. **Two failure channels, both designed in from the start.** Operational failures (Y ran but failed/timed out) call back to A. Setup failures (Y can't be resolved) escalate to the *human* and halt — but **also** release A's wait with a `blocked` outcome so A never sleeps forever on something only a human can fix.

### Spawn decision table

| Request | Target defines an orchestration (with roles) in its `.dot-agent-deck.toml`? | Spawn | Base |
|---|---|---|---|
| `info` | either | single agent | **main** (read-only enforced) |
| `work` | yes | orchestration | **worktree** |
| `work` | no | single agent | **worktree** |

- **`info` → single agent on the target's main checkout, read-only by construction.** Read-only is enforced by the spawn posture/sandbox, not merely instructed, so running against Y's live (possibly dirty) checkout is safe. Info wants Y's *current* state, not a clean worktree.
- **`work` → always a worktree**, regardless of single-agent vs orchestration, so A's request never stomps on whatever a human or another orchestration is doing in Y's main.
- **No orchestration in Y's config = Y opted out of orchestration** (not a degradation): a single agent is the *correct* answer, not a compromise. The check is "config defines at least one orchestration *with roles*", not merely "the file exists"; if Y defines several, A selects one (a designated default or a named one). Config presence governs *how* A works in Y — it does **not** govern *whether* A may touch Y; that is the peer-map allowlist's job.

### Target resolution & authorization (in X's `.dot-agent-deck.toml`)

Resolution is a single escalation at three confidence levels:

- **Declared + resolves** → proceed. The peer map gives a logical name → local path; the path is validated to be a real repo (declared paths can go stale).
- **Undeclared but found by search** → the target was located by searching sibling directories by name, but it is *outside the allowlist*, so it must **not** silently auto-dispatch. Confirm with the human before writing (an `info`/read-only request can be looser; a `work` request must confirm).
- **Not found / stale path** → halt and escalate to the human. This is also the guardrail against A *hallucinating* a nonexistent dependency.

**v1 scope is local paths (sibling directories) only.** Clone-from-git-URL (network, auth, larger blast radius) is deferred to a follow-up.

### The one genuinely new wire: cross-boundary addressing

Local `work-done` routing finds the orchestrator via the shared `(name, cwd)` tuple — but A lives in a *different* orchestration and a *different* cwd, so that lookup will never find A. So at dispatch, A registers a callback `dispatch-id → A's pane`; the `dispatch-id` rides into the spawned unit; and on the target's `work-done --done` (or a single agent's `work-done`) the daemon resolves `dispatch-id → A's pane` and injects, instead of doing the tuple lookup. Everything else is reuse of the existing delegate/work-done machinery.

The return edge fires on **four terminal states**: `done | failed | timeout | blocked`. `blocked` is the not-found/escalated-to-human outcome — it both pages the human and releases A.

## User-facing behavior & documentation (documentation-first)

### Declaring peers (`.dot-agent-deck.toml` in the originating project)

```toml
# Projects this project may dispatch into. The name on the left is the
# logical handle an orchestrator uses; the path is a local repo directory.
# This list is also the authorization allowlist: an orchestrator can only
# dispatch into projects named here (anything else requires confirmation).
[dispatch.peers]
backend  = "../backend-service"
payments = "../payments"
```

### Dispatching from an orchestrator

```bash
# Request information (read-only single agent on the target's main checkout).
dot-agent-deck dispatch --to backend --mode info \
  --task "What is the request/response shape of POST /orders? Return the JSON schema."

# Request work (worktree; orchestration if the target defines one, else a single agent).
dot-agent-deck dispatch --to backend --mode work \
  --task "Add a POST /orders/{id}/refund endpoint. Include tests. Open a PR."
```

After dispatching, the orchestrator stops working and goes idle. When the target finishes, a prompt is injected back into the orchestrator's pane summarizing the outcome (and, for `work`, the worktree/branch and PR link), and the orchestrator resumes automatically.

### What the orchestrator sees on return

- **Done** — "Dispatch to `backend` completed: \<summary\>. Result at \<path / PR link\>."
- **Failed / timed out** — "Dispatch to `backend` failed: \<reason\>." The orchestrator decides whether to retry, adjust, or proceed without it.
- **Blocked (target not found)** — the orchestrator is told it was escalated to a human, and **you** are notified (the dispatch pane goes "needs attention" via the existing bell + waiting-for-input surface) with an actionable message, e.g. "Dispatch to `payments` halted: no repo found at `../payments`. Fix the path or peer entry and re-dispatch."

### Decision table (user-facing)

Reproduce the spawn decision table above in `docs/orchestration.md` so users can predict what a dispatch will do.

## Scope

### In Scope

- A `dispatch` command/flag surface on `dot-agent-deck` carrying `--to <peer>`, `--mode info|work`, `--task <text>`, and an explicit override for the spawn shape.
- Peer-map config (`[dispatch.peers]`) in the project config, parsed by `project_config.rs`, doubling as the authorization allowlist.
- Target resolution: declared-path-first, sibling-search fallback, declared-path validation, human-confirm for undeclared-but-found, halt+notify for not-found.
- The spawn decision table: `info`→read-only single agent on main; `work`+orchestration→orchestration on worktree; `work`+no-orchestration→single agent on worktree; with explicit-override support.
- Read-only-by-construction enforcement for `info` dispatches.
- Worktree creation/teardown for `work` dispatches, with cleanup ownership so dispatched worktrees are not orphaned when A finishes.
- Cross-boundary callback: `dispatch-id → originating pane` registration in daemon state; routing the target's terminal signal back to A via `write_to_pane_and_submit`.
- Four terminal outcomes (`done | failed | timeout | blocked`) delivered to A; human escalation for `blocked`/setup failures reusing the bell + waiting-for-input surface.
- Tests: L2 e2e for the dispatch→spawn→return loop across two project directories (both `info` and `work`, including a not-found escalation); L1/behavior tests for resolution, the decision table, and config parsing.
- User docs: a cross-project dispatch section in `docs/orchestration.md` (command, `info|work`, peer-map config, decision table, escalation behavior), plus correcting the now-stale concurrent-orchestration warning (see below).

### Out of Scope / Non-Goals

- **Peer-to-peer orchestrator messaging / a mesh.** Explicitly rejected (cycles/deadlock). The model is strictly initiator→target (DAG).
- **Clone-from-git-URL targets.** v1 is local sibling paths only; remote/clone-on-demand is a deferred follow-up.
- **Blocking/synchronous dispatch.** "Wait" is always suspend-and-resume.
- **Nested-dispatch policy depth limits / cycle detection across >2 hops.** The DAG shape prevents cycles by construction for the initiator→target relationship; deeper policy (e.g. A→Y→X back-edge prevention) is noted as a risk, not built in v1 beyond the obvious self-dispatch guard.
- **The `experimental` feature flag** (PRD #139). The user chose to ship this **visible by default**: no `features.rs` wrapper, no `graduate-` follow-up issue.

## Design Decisions

1. **DAG, not mesh.** The originator initiates a fresh unit in the target rather than messaging a peer. This is the single decision that removes cycles/deadlock and lets the entire feature reuse the existing parent→child delegate/work-done vocabulary.

2. **Suspend-and-resume, not a blocking call.** Blocking parks an LLM mid-tool-call, fights the daemon's detach-safe design (the PTY scrollback is the durable journal), loses results on detach, and does not nest. Idle-then-re-awaken is already how local delegation waits.

3. **Deterministic spawn shape from observable facts.** Deriving single-vs-orchestration and main-vs-worktree from `info|work` + target config presence (rather than asking A to freestyle) reduces what the agent can get wrong and makes a dispatch's behavior predictable and inspectable. An explicit override remains for the rare case A knows better.

4. **`info` is read-only by construction.** The safety of running on Y's live `main` rests on enforcement (sandbox/posture), not on instructing the agent to behave. Enforced read-only is what makes "info on main against a possibly-dirty checkout" correct rather than risky.

5. **No-config = opt-out, not degradation.** A project without a defined orchestration has chosen not to use one; a single agent is the right answer. The real predicate is "defines an orchestration *with roles*", because a `.dot-agent-deck.toml` can exist for unrelated settings.

6. **Resolution lives in the originator's config and is the allowlist.** Y's config cannot be where A looks up Y's location (chicken-and-egg: A must find Y first). Putting the peer map in X's config solves resolution *and* the cross-project authorization gate with one structure. The search fallback never crosses the allowlist silently.

7. **Two failure channels, and `blocked` releases the waiter.** Operational failures loop back to A; setup failures page a human. The non-obvious requirement is that a setup failure must *also* deliver a terminal `blocked` outcome to A — otherwise A waits forever on a condition only a human can clear.

8. **Concurrent orchestrations become load-bearing.** This feature runs ≥2 orchestrations at once by design, so the daemon's per-orchestration scoping (already implemented for delegate/work-done via the `(name, cwd)` tuple per Issue #140's cwd-partition fix) is a hard dependency. The stale warning in `docs/orchestration.md` (lines ~10-12 and ~370) should be corrected as part of this PRD's docs work, since same-basename directories are already disambiguated by full path.

## Success Criteria

- An orchestrator in project X can run `dot-agent-deck dispatch --to <peer> --mode info --task ...` and, against a sibling project Y, receive an injected answer that wakes it up — without X blocking inside a tool call (verified: X's pane is idle while Y runs, then resumes).
- `dot-agent-deck dispatch --to <peer> --mode work --task ...` spawns an **orchestration on a worktree** when Y defines one, and a **single agent on a worktree** when Y does not; `--mode info` spawns a **read-only single agent on Y's main** in both cases — matching the decision table.
- An `info` dispatch cannot modify Y's working tree (enforced, not instructed) — verified by attempting a write from the spawned agent and observing it blocked.
- A dispatch to an **undeclared** peer found by search prompts for human confirmation before any write; a dispatch to a **non-existent** target halts, pages the human via the bell/waiting-for-input surface, **and** delivers a `blocked` outcome that releases the originator (verified: X resumes with a blocked message rather than hanging).
- The originator's wait terminates on all four outcomes (`done | failed | timeout | blocked`); none leaves X idle indefinitely.
- The peer map in `.dot-agent-deck.toml` is the authorization boundary: a dispatch target not in the map (and not human-confirmed) is never written to.
- `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test-fast` pass; `cargo test-e2e` passes before the PR (CLAUDE.md rules 2 & 5).
- User docs describe the dispatch command, `info|work` intent, peer-map config, the decision table, and escalation behavior; the stale concurrent-orchestration warning is corrected.

## Milestones

### Phase 1 — Resolution & authorization

- [ ] **M1.1** — Peer-map config (`[dispatch.peers]`) parsed by `project_config.rs`; declared-path-first resolution with real-repo validation. Behavior tests for parse + resolve + stale-path → not-found.
- [ ] **M1.2** — Sibling-directory search fallback with the three-level escalation (declared→proceed, undeclared-found→human-confirm, not-found→halt+notify) and a self-dispatch guard. Tests for each branch.

### Phase 2 — Dispatch & spawn

- [ ] **M2.1** — `dot-agent-deck dispatch` command (`--to`, `--mode info|work`, `--task`, explicit override) that resolves the target and emits the dispatch signal to the daemon.
- [ ] **M2.2** — Spawn decision table wired to the daemon spawn path: `info`→read-only single agent on main (read-only enforced by construction); `work`+orchestration→orchestration on worktree; `work`+no-orchestration→single agent on worktree. Worktree creation + cleanup ownership.

### Phase 3 — Cross-boundary callback & failure channels

- [ ] **M3.1** — Callback registration (`dispatch-id → originating pane`) in daemon state; the `dispatch-id` carried into the spawned unit; the target's terminal signal routed back to A via `write_to_pane_and_submit` (originator goes idle, then resumes).
- [ ] **M3.2** — Four terminal outcomes delivered to A (`done | failed | timeout | blocked`); operational failures/timeouts loop back to A; setup/not-found escalates to the human via the bell + waiting-for-input surface **and** releases A with `blocked`.

### Phase 4 — Tests, docs & release gate

- [ ] **M4.1** — L2 e2e across two project directories: an `info` dispatch returns an answer and wakes the originator; a `work` dispatch produces a worktree result and wakes the originator; a not-found dispatch escalates to the human and releases the originator. L1/behavior coverage for resolution and the decision table.
- [ ] **M4.2** — User docs: cross-project dispatch section in `docs/orchestration.md` (command, `info|work`, peer-map config, decision table, escalation); correct the stale concurrent-orchestration warning; changelog fragment via `dot-ai-changelog-fragment`.
- [ ] **M4.3** — Pre-PR gate: `cargo test-e2e` green; review (Greptile) settled per CLAUDE.md rule 8.

## Risks & Mitigations

- **A dispatches into a project mid-edit and corrupts state.** `work` always uses a fresh worktree (never main); `info` is read-only by construction. The dangerous cell (writing to a live checkout) does not exist in the table.
- **A waits forever.** Every terminal outcome — including setup failures via `blocked` and a `timeout` — releases A; no path leaves the originator idle indefinitely.
- **A hallucinates a dependency / dispatches into the wrong repo.** Resolution validates the target is a real repo; undeclared targets require human confirmation; not-found halts and pages a human.
- **Orphaned worktrees / runaway dispatched orchestrations.** Dispatched `work` units carry cleanup ownership so the worktree is reclaimed when the originator's wait resolves; a dispatched unit whose originator died is surfaced rather than left running silently.
- **Back-edge / nesting cycles (A→Y→X).** v1 keeps the initiator→target DAG and guards self-dispatch; deeper cycle policy across multiple hops is noted as a future concern, not a v1 blocker.
- **Concurrent-orchestration scoping regressions.** This feature leans hard on the daemon's `(name, cwd)` per-orchestration scoping; e2e exercises two concurrent orchestrations to keep that path honest, and the docs warning is corrected to match reality.
- **Authorization bypass via the search fallback.** The fallback never auto-writes into an undeclared target; `work` into undeclared-but-found always requires human confirmation.
