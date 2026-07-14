# PRD #201: Pi as a first-class agent — deterministic orchestrator + third agent type

**Status**: Implemented — pre-PR (review resolved, e2e gate green; M5.3 manual cross-version test + M5.4 Greptile settle during `/prd-done`)
**Priority**: Medium
**Created**: 2026-07-10
**GitHub Issue**: [#201](https://github.com/vfarcic/dot-agent-deck/issues/201)
**Related**: PRD #58 (multi-role agent orchestration — the delegate/work-done model this makes deterministic for one agent), PRD #82 (orchestrator-role reinforcement — the prompt-and-pray fragility this replaces with native tools), PRD #93 (always-external daemon — the daemon is the single source of truth the extension reports into), PRD #50 (auto-install hooks — the mechanism that becomes unnecessary for a Pi pane), PRD #176 (desktop GUI — its agents-communication graph consumes the same structured orchestration events this PRD's producer emits), PRD #139 (the `experimental` feature flag — Pi was gated behind it during development, then un-gated before merge; see Design Decision #8), and a **companion PRD (to be created): cross-agent orchestration test matrix + backfill**, which inherits this PRD's synthetic-agent harness and generalizes it across `{claude, opencode, pi}`.

## Problem Statement

dot-agent-deck does not run an agent loop — it is a control plane over **external** agent processes. It spawns `claude`/`opencode` as PTY children (`src/agent_pty.rs`), observes their status by scraping Claude Code hooks it installs into `~/.claude/settings.json` (`src/hook.rs`, `src/hooks_manage.rs`), and coordinates them by having an orchestrator agent *choose to type* `dot-agent-deck delegate --to <role>`, routed through the daemon into worker PTYs (`src/state.rs`, `docs/orchestration.md`). The agent is a black box; every integration mechanism is a workaround for not being able to see inside it.

That model is fine for workers, but it is weakest exactly where determinism matters most: **the orchestrator.** Delegation works only if the orchestrator agent remembers to shell out the right CLI string, formats it correctly, and the 10s `SessionStart` wait (`src/state.rs:33`) lands. Status arrives only through global `settings.json` mutation and a hook-event mapping that can drift (PRD #91 exists because hook freshness is a real problem). PRD #82 ("orchestrator-role reinforcement") is a whole PRD spent nudging a black-box model to behave — because we cannot make it behave, only prompt it.

**Pi** (`earendil-works/pi`, MIT, TypeScript) is a different kind of thing: a minimal agent harness whose entire thesis is *primitives, not features* — a small toolset plus a first-class **TypeScript extension API** with access to tools, commands, and an **event bus**. It is not a black box; it is a box we can open. That means for a Pi agent we can replace the workarounds with owned mechanisms: `delegate`/`work-done` as native, schema-validated tools instead of prompt-and-pray CLI strings, and status reported directly from the agent's event stream instead of scraped from installed hooks.

The workarounds do not get *ported* to Pi — for a Pi pane they *dissolve*, because they only ever existed to compensate for not owning the agent.

## Solution Overview

Add **Pi as a third, first-class, status-tracked agent type** alongside `claude`/`opencode`, and use its extension API to make the **orchestrator role deterministic**. Pi is not bundled as a runtime and does not replace anything; it is detected on PATH like the other agents, and is opt-in by installing `pi` and pointing a role at `command = "pi"` (it was gated behind the `experimental` flag during development; that gate was removed before merge once the flagship was proven — see Design Decision #8).

Six ideas carry the design:

1. **Pi is a detected runtime; only our extension is bundled.** Pi needs a Node/Bun runtime, so shipping "Pi itself" inside the Rust binary would mean shipping Node — rejected, because the single-static-binary distribution story is a real asset. Instead Pi is detected on PATH exactly like `claude`/`opencode`, and the one thing that gives us control — **our orchestrator extension** — is a small TypeScript asset compiled into the `dot-agent-deck` binary (`include_str!`) and materialized on demand. The glue is bundled; the engine is detected. This is the honest reading of "bundle it inside the project."

2. **`AgentType::Pi` makes Pi first-class everywhere, not just in orchestration.** Today `AgentType::from_command` (`crates/protocol/src/event.rs`) maps only `claude`/`opencode`; anything else runs but gets no live status. Adding `Pi` and wiring the extension's status reporting means a plain `pi` pane in a dashboard tab, and a scheduled `pi` job, are **status-tracked like any other agent** — the orchestrator win is the flagship, but general third-agent support falls out of the same work (see Scope).

3. **The extension is a cleaner *producer* for the existing protocol, not a second path.** The extension reports status and hand-offs into the **same** `EventType`/`AgentEvent` stream the daemon already consumes (`crates/protocol/src/event.rs`, routed in `src/daemon.rs`). It does not invent a parallel status or orchestration channel. The daemon, the TUI, the GUI graph, and scheduled runs all see Pi through the identical contract they already use — Pi is just a higher-fidelity source feeding it.

4. **For a Pi pane, no hooks and no `settings.json` mutation.** The Claude-Code hook install (`src/hooks_manage.rs`) simply does not run for a Pi agent. The extension subscribes to Pi's event bus and reports lifecycle/status directly. This is the workaround-dissolution: it applies to the **Pi pane only** — workers that are still `claude`/`opencode` keep their hooks and their `work-done` CLI, because they are still black boxes. The clean path extends to a worker only when that worker is itself Pi.

5. **The extension talks back over the existing CLI, not a new socket client (v1).** Pi's extension implements its tools and event handlers by shelling the CLI that already exists — `dot-agent-deck delegate`, `dot-agent-deck work-done`, plus a small new `dot-agent-deck agent-event --type <state>` — routed over the daemon socket via the pane env vars the daemon already injects (`DOT_AGENT_DECK_PANE_ID` / `_AGENT_ID` / `_VIA_DAEMON`). This turns "the model remembers to type a command" into "the model calls a validated tool whose body runs the command deterministically," with **zero new wire surface beyond one additive subcommand**. A native JS protocol client that speaks the socket directly is an explicit later option, not v1. **Update (shipped):** prompt/seed *delivery* was made native — on `session_start` the extension pulls the pane's seed via a read-only `dot-agent-deck get-seed` and hands it to pi's `sendUserMessage`, so a Pi pane receives its prompt through pi's own API, not PTY keystroke injection (kept only as a bounded exactly-once fallback). The outbound tools/status still shell the CLI.

6. **Zero-step setup — the extension auto-materializes.** The deck AUTO-materializes the bundled extension into Pi's extension dir at pi-spawn time (the same pattern as claude's hooks / opencode's plugin auto-install), so setup is just: install `pi` + set `command = "pi"` in `.dot-agent-deck.toml`. `dot-agent-deck orchestrator setup` remains as an explicit path (materialize on demand + print the install hint if `pi` is absent). No manual step, no hunting for an extension to install by hand.

## User-facing behavior & documentation (documentation-first)

### Setup (one time)

```
1. Install dot-agent-deck                 # as today — single binary
2. Install pi                             # once, via pi's installer; like installing claude/opencode
3. In .dot-agent-deck.toml, set the orchestrator role:  command = "pi"
   # That's it — the deck AUTO-materializes the bundled extension at pi-spawn time.
   # (Optional/explicit: `dot-agent-deck orchestrator setup` materializes on demand
   #  and prints the pi install hint if pi is missing.)
```

### What happens at runtime

Opening an orchestration behaves as today, except the orchestrator pane is Pi:

- The daemon spawns `pi` for the orchestrator role and `claude`/`opencode` for workers, injecting the env vars it already sets.
- The extension exposes `delegate(role, task)` and `work-done(summary)` as **native typed tools**; calling `delegate` shells `dot-agent-deck delegate` and the daemon routes to the worker PTY exactly as today (`handle_delegate`).
- The extension subscribes to Pi's event bus and reports status via `dot-agent-deck agent-event`, so the Pi pane shows running / waiting-for-input / finished in the TUI and GUI **without any hook installed**.
- A plain `pi` pane opened from the dashboard (`Ctrl+n`, `command = "pi"`) and a scheduled `pi` job are status-tracked the same way — status reporting is pane-agnostic; the `delegate` tool is simply unused outside an orchestration (and is already daemon-rejected from non-orchestrator panes).

### What it deliberately does NOT do

It does not bundle or vendor the Pi/Node runtime, does not replace `claude`/`opencode`, does not remove hooks for non-Pi agents, and does not adopt Pi's own multi-agent orchestration (TEAM/CHAIN/PIPELINE) — dot-agent-deck's daemon remains the orchestrator-of-record; Pi is a better-behaved node inside it.

### Docs

- User doc: enabling Pi (install, `orchestrator setup`, `command = "pi"`), added under `docs/` and `site/sidebars.js`.
- Developer doc under `docs/develop/`: the extension's tool/event contract, how it maps Pi events to `agent-event` types, the embedded-asset materialization, and the JS toolchain (linked from `CONTRIBUTING.md`).
- `docs/develop/experimental-flag.md` — the Pi flag entry was added during development and removed when Pi was un-gated before merge; Pi is no longer listed there.
- Changelog fragment via `dot-ai-changelog-fragment`.

## Scope

### In Scope

- **`AgentType::Pi`** in `crates/protocol` (`from_command` mapping, status plumbing), making Pi a first-class status-tracked agent in **dashboard panes and scheduled jobs**, not orchestrator-only.
- **The bundled orchestrator extension** (TypeScript): native `delegate`/`work-done` tools and event-bus → status reporting, compiled into the binary as an asset.
- **`dot-agent-deck orchestrator setup`**: pi detection + install hint, extension materialization + enablement.
- **`dot-agent-deck agent-event --type <state>`** (proposed, additive): the small CLI seam the extension uses to report status into the existing `EventType`/`AgentEvent` stream.
- **A thin, agent-agnostic synthetic-agent test harness** — scripted stand-in that calls `delegate`/`work-done`/`agent-event` deterministically — built for Pi's own contract coverage but parameterized by agent identity from line one, so the companion PRD generalizes rather than rewrites it.
- **Tests** across three layers (see Design Decision #7): synthetic-harness contract tests (fast tier), TS extension unit tests, and real-`pi` e2e (`e2e_*.rs`, `#[cfg(feature="e2e")]`), including **headless/unattended status reporting with no client attached**.
- **`experimental`-flag gating** at the render seam (Pi status affordances) via one `features::show_pi_agent()` wrapper per rule #9 — **implemented then removed before merge; Pi ships un-gated** (see Design Decision #8).
- The **rule-12 cross-version contract check** for the `agent-event` addition and any orchestration-event shape, with the `PROTOCOL_VERSION` / `.breaking.md` decision recorded.
- **Existing-PRD cross-reference sweep**: review every PRD under `prds/` (and `prds/done/`) and, wherever a PRD *specifically enumerates or discusses the supported agent types* (today `claude`/`opencode`), add `pi` so the corpus reflects Pi as a first-class agent. Generic references to "the agent" are left alone; only explicit agent-type enumerations are updated. Done last, after all functional work has landed.
- Docs and changelog as above.

### Out of Scope / Non-Goals

- **Bundling / vendoring the Pi or Node/Bun runtime.** Pi is detected on PATH; only the extension asset ships in-binary. A batteries-included build that ships Node is a possible *later, opt-in* artifact, not this PRD.
- **Adopting Pi's own orchestration (TEAM/CHAIN/PIPELINE, `pi-subagents`, `pi-crew`).** That overlaps and competes with dot-agent-deck's control plane; explicitly rejected.
- **A full native JS daemon-socket client for the extension's OUTBOUND side.** The `delegate`/`work-done`/`agent-event` tools + status still shell the CLI. (INBOUND prompt/seed delivery WAS made native via `sendUserMessage` + a read-only `get-seed` pull — see Design Decision #5.) `clear=false` mid-session re-delegation also keeps PTY injection (no fresh `session_start` to pull on) — a further enhancement.
- **Removing hooks for `claude`/`opencode`.** The workaround-dissolution is Pi-only by construction.
- **The cross-agent test matrix + backfill of uncovered orchestration features across all three agents.** That is the **companion PRD** — this PRD builds only the reusable harness seam and Pi's own coverage.

## Design Decisions

1. **Bundle the extension, detect the runtime.** Shipping Node to bundle Pi would forfeit the single-binary distribution advantage. The extension is small text; `include_str!` + materialize keeps it versioned atomically with the release that ships it (no separate npm cadence to chase) while Pi stays a detected dependency like `claude`/`opencode`.

2. **Pi feeds the existing protocol; it is a producer, not a fork.** The one rule that prevents this from becoming a second status/orchestration path: the extension emits into the same `EventType`/`AgentEvent` contract every existing client already consumes. Higher fidelity, identical wire.

3. **The value is the orchestrator; the third-agent-type support is the free consequence.** The reason to do this is deterministic orchestration (native tools, event-driven status, no prompt-and-pray). Dashboard + scheduled support for plain Pi panes costs almost nothing extra once `AgentType::Pi` exists, so it is in scope — but the flagship, and the thing tests must prove, is the orchestrator hand-off chain.

4. **Workaround-dissolution is Pi-only, stated plainly.** Hooks and `settings.json` mutation vanish for a Pi pane, not repo-wide. Nobody should expect the hook machinery to disappear while workers remain `claude`/`opencode`.

5. **Shell the existing CLI in v1.** Reusing `delegate`/`work-done` + one additive `agent-event` subcommand, routed via already-injected pane env vars, is the smallest possible new surface and reuses tested daemon routing. A direct socket client for the outbound side is deferred. **Update (shipped):** the concrete reason arrived for the INBOUND side — to leverage pi's strengths rather than mimic the PTY-injection workaround, prompt/seed delivery is now native via pi's `sendUserMessage` (the extension pulls the seed with a read-only `get-seed` on `session_start`), with PTY injection kept only as a bounded exactly-once fallback. This also removed the pi-worker 10s `SessionStart` timeout. Outbound tools/status still shell the CLI.

6. **Do not adopt Pi's orchestration.** dot-agent-deck's differentiator is being the cross-agent, observable, daemon-backed control plane. Letting Pi orchestrate would hollow that out and make the hand-offs invisible to the dashboard/graph. Pi is a node in *our* orchestration.

7. **LLM use in tests is a quality decision, not a cost decision.** The synthetic harness exists because the plumbing under test — routing, protocol frames, status wiring — is genuinely more reliable asserted *deterministically*; it is **not** a token-saving dodge. Where the behavior under test is real agent behavior (does an orchestrator decide to delegate, call the tool correctly, react to `work-done`), a real agent is the higher-quality test and its coverage is **bounded by flakiness and wall-clock, not by token cost.** We will not shrink real-agent e2e to save money when the real agent gives better confidence. _This principle likely warrants revisiting the testing guidance in our skills/conventions (CLAUDE.md rules 4–6 lean toward minimizing the real-agent tier); flagged for follow-up, not resolved here._

8. **Gate behind `experimental` (PRD #139).** Pi as a selectable agent, its status affordances, and the setup command are a new user-visible surface. Per rule #9, gate only the render/input seam via a single `features::show_pi_agent()` wrapper (not business logic, not the daemon protocol, not the extension), note the flag in this PRD + changelog + `docs/develop/experimental-flag.md`, and file a `graduate-pi-agent` follow-up at ship time. **Reversed before merge — Pi ships un-gated.** Once the flagship was proven end to end (real-`pi` orchestration + event-driven status), the gate was removed: it was cosmetic-only (it merely suppressed the Pi identity label on a card — it never guarded business logic, the protocol, or the deferred parity paths, none of which are flag-gated), it had no effect on existing users (`claude`/`opencode` panes are unaffected, and Pi is opt-in by install/config), and leaving it on showed adopters a confusing "No agent" label after they'd completed setup. So `show_pi_agent()` and the render-seam gate were deleted, the flag notes were stripped from the docs/changelog, and `graduate-pi-agent` is moot.

## Success Criteria

- With `pi` installed and `orchestrator setup` run, an orchestration whose orchestrator role is `command = "pi"` completes a real hand-off end to end: the Pi orchestrator calls the native `delegate` tool, the daemon routes the task to a `claude`/`opencode` worker PTY, and `work-done` returns — verified with a **real `pi` agent** driving a real model.
- The Pi pane's status (running / waiting-for-input / finished) is shown in the TUI **with no hook installed and no `~/.claude/settings.json` mutation**, driven entirely by the extension reporting into the existing `AgentEvent` stream.
- A plain `pi` pane opened from the dashboard **and** a **scheduled** `pi` job are status-tracked identically, including **unattended with no client attached**.
- The extension auto-materializes at pi-spawn time (no manual step); `orchestrator setup` remains as an explicit path that materializes on demand and prints a clear one-line install hint when `pi` is absent.
- Prompt/seed delivery to a Pi pane is native (extension `sendUserMessage` via a read-only `get-seed` pull), not PTY keystroke injection; injection remains only as a bounded exactly-once fallback.
- The synthetic-agent harness deterministically exercises delegate/work-done/status routing in the fast tier, and is written agent-agnostically (parameterized by agent identity) so the companion PRD can run it across `{claude, opencode, pi}`.
- Real-`pi` e2e coverage is sized to confidence, not to token budget (Design Decision #7); it passes in `cargo test-e2e` before the PR.
- The `agent-event` addition is classified per rule #12 (protocol bump or `.breaking.md` recorded), and the cross-version manual test passes: a previous-release daemon with a Pi orchestrator under the branch TUI still routes delegates and receives status.
- The surface ships **un-gated** (visible by default): the `experimental` gate added during development was removed before merge (see Design Decision #8), so no `graduate-pi-agent` follow-up is needed.
- `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test-fast` pass for the Rust crates; the extension's TS tests pass; `cargo test-e2e` passes before the PR (CLAUDE.md rules 2, 5, 8).

## Milestones

### Phase 1 — Agent type & the synthetic harness

- [x] **M1.1** — `AgentType::Pi` in `crates/protocol` (`from_command`, status plumbing) with unit tests; a plain `pi` pane runs and is recognized (status wiring lands in M2).
- [x] **M1.2** — `dot-agent-deck agent-event --type <state>` subcommand: routes over the daemon socket via pane env vars into the existing `EventType`/`AgentEvent` stream. Rule-12 classification recorded.
- [x] **M1.3** — Agent-agnostic **synthetic-agent harness**: a scripted stand-in that calls `delegate`/`work-done`/`agent-event` on cue; deterministic contract tests for daemon routing, the pane-role guard, and status transitions (fast tier).

### Phase 2 — The extension & clean status

- [x] **M2.1** — Orchestrator extension (TypeScript): native `delegate`/`work-done` tools shelling the CLI; TS unit tests for invocation-building and error paths. JS toolchain contained in the extension subdirectory.
- [x] **M2.2** — Extension event-bus → `agent-event` status mapping; a Pi pane reports running/waiting/finished with **no hook installed**. TS mapping tests + a synthetic-harness assertion, including **headless/unattended** (no client attached).

### Phase 3 — Delivery & setup

- [x] **M3.1** — Bundle the extension as an in-binary asset (`include_str!`) and materialize it; test that materialization writes the expected files to a temp dir.
- [x] **M3.2** — `dot-agent-deck orchestrator setup`: pi detection + install hint + extension enablement; fast tests for present/absent pi. **Superseded as the default by auto-materialize** (Phase 7 M7.1) — the extension now auto-materializes at pi-spawn time; `orchestrator setup` remains the explicit path.

### Phase 4 — Real-agent proof & scheduled/dashboard parity

- [x] **M4.1** — Real-`pi` e2e (`e2e_pi_orchestrator.rs`, `#[cfg(feature="e2e")]`): real orchestrator delegates to a real worker and receives `work-done`. Sized to confidence per Design Decision #7.
- [x] **M4.2** — Dashboard `pi` pane and **scheduled** `pi` job status-tracked end to end (the scheduler uses the same spawn primitive; assert unattended status).

### Phase 5 — Flag, docs, contract & release gate

- [x] **M5.1** — `experimental` gating implemented (`features::show_pi_agent()` at the render seam) **then reversed before merge**: the gate was cosmetic-only, so Pi ships visible by default; the wrapper + flag notes were removed and `graduate-pi-agent` is moot (see Design Decision #8).
- [x] **M5.2** — Docs: user enablement doc under `docs/` (+ `site/sidebars.js`); developer extension-contract doc under `docs/develop/` (+ `CONTRIBUTING.md`); changelog fragment.
- [x] **M5.3** — Rule-12 cross-version manual test (previous-release daemon + branch TUI + Pi orchestrator: delegate routes, status arrives); `PROTOCOL_VERSION`/`.breaking.md` finalized. Verified during `/prd-done`: same-version interop confirmed live with two real-agent e2e runs on branch HEAD (`delegate_work_done_chain_claude` — real Haiku worker delegate+work-done round trip; `opencode_auto_submits_daemon_injected_prompt` — real OpenCode worker), both green; cross-version refusal confirmed by building the real v0.32.0 (`PROTOCOL_VERSION` 4) binary from tag, capturing its genuine `daemon hello` output, and feeding it through the branch's actual `probe_remote_protocol` — result: clean typed `ProtocolMismatch { remote: Some(4), local: 5, .. }`, no corruption (verification harness was a throwaway `examples/` file, run and deleted, never committed).
- [ ] **M5.4** — Pre-PR gate: `cargo test-e2e` green; review (Greptile) settled per rule #8.

### Phase 6 — Existing-PRD cross-reference sweep

- [x] **M6.1** — Go through all existing PRDs under `prds/` (and `prds/done/`) and include `pi` wherever a PRD *specifically* mentions or enumerates the supported agent types (`claude`/`opencode`), so Pi's first-class status is reflected consistently across the PRD corpus. Skip generic "the agent" references; touch only explicit agent-type enumerations. This is the **final task**, run after all functional work in Phases 1–5 has landed, so the sweep reflects the shipped behavior.

### Phase 7 — Native pi (post-review re-scope)

Added after the pre-merge review raised the bar: "full parity, and leverage pi's strengths rather than mimic the claude/opencode workarounds — otherwise we cannot use it." A spike confirmed pi's `ExtensionAPI` exposes native inbound delivery (`sendUserMessage`), so the last mimicked workaround (PTY prompt injection) was dissolved.

- [x] **M7.1** — **Auto-materialize** the bundled extension at pi-spawn time (guarded on the spawn command being `pi`, idempotent, HOME-unset-safe — skips, no `/tmp` write), so Pi needs no manual setup; `orchestrator setup` stays as the explicit path. Fast-tier tests (clean HOME / absent-pi / HOME-unset / idempotent); the real-pi e2e now rely on it. Reverses Design Decision #6.
- [x] **M7.2** — **Native prompt/seed delivery** via pi's `sendUserMessage`: a read-only `dot-agent-deck get-seed` verb (on the unversioned hook socket) + an additive `StartAgent.seed` field (spawn-time seed, since pi's `session_start` fires before render-loop injection would). The extension pulls the seed on `session_start` and delivers it natively; PTY injection is kept only as a **bounded exactly-once fallback**. Removes the pi-worker 10s `SessionStart` timeout. Covers the orchestrator seed + `clear=true` worker respawns; `clear=false` mid-session keeps injection (further enhancement). Rule-12: **no `PROTOCOL_VERSION` bump** (rides the unversioned hook socket; `StartAgent.seed` is additive; degrades gracefully). Real-pi e2e assert the native path (`seed_delivered_native`); RPC mode explicitly rejected (headless — would break the live pane).
- [x] **M7.3** — pi-as-worker proven (`chain-smoke/pi/002`): a real pi worker receives the delegated task and signals `work-done`, so both pi roles (orchestrator + worker) are proven with a real pi.

## Risks & Mitigations

- **Coupling to a young, single-author project's extension API.** Pi moves fast and is one person's project. Mitigation: keep Pi optional (detected on PATH, opt-in by config), keep our surface to a small extension shelling stable CLI, and pin the tested Pi version in docs; never make Pi a required core dependency.
- **A second toolchain (Node/TS) in a Rust-centric repo and its gates.** Mitigation: contain all JS build tooling in the extension subdirectory; keep `cargo fmt`/`clippy`/`nextest` authoritative for the Rust crates; run TS tests off the Rust critical path.
- **Two status paths (hook-based vs extension-based) drift apart.** Mitigation: both feed the *same* `EventType`/`AgentEvent` contract (Design Decision #2); the synthetic harness asserts the Pi path produces the identical event shapes the hook path does.
- **Temptation to over-minimize real-agent tests for cost.** Directly counter to Design Decision #7. Mitigation: the principle is written into the PRD and success criteria; real-agent coverage is bounded by flakiness/time, and the maintainer-flagged skills revisit is tracked as follow-up.
- **Scope creep into the cross-agent test backfill.** Mitigation: that is the named companion PRD; this PRD builds only the reusable harness seam and Pi's own coverage.
- **"Bundle it inside the project" misread as vendoring Node.** Mitigation: Design Decision #1 states the extension is bundled and the runtime is detected; a Node-bundling build is explicitly out of scope / a later opt-in artifact.
- **Pi's YOLO (no-permission) security model.** Same posture as existing agents (Claude Code with full fs/bash); does not change dot-agent-deck's sandbox story, but noted so container-execution guidance in docs covers Pi too.
