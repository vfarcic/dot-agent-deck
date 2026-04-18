# PRD #58: Multi-Role Agent Orchestration with Skill-Based Handoff

**Status**: In Progress
**Priority**: High
**Created**: 2026-04-16
**GitHub Issue**: [#58](https://github.com/vfarcic/dot-agent-deck/issues/58)

## Problem Statement

When multiple AI agents (different models/CLIs) need to collaborate on a task — e.g., a TDD cycle where one agent writes tests and another implements code, or a code-then-review workflow — users must manually coordinate turn-taking, copy handoff context between sessions, and switch between panes. There is no structured way to define roles, handoff artifacts, or termination conditions. The dashboard can monitor agents but cannot orchestrate their collaboration.

The primitives exist (agents can read/write files, git diffs are natural handoff artifacts) but there is no tool that ties it together while keeping agents interactive and the human in the loop.

## Solution Overview

Add an **orchestration system** to dot-agent-deck that coordinates multi-role agent workflows through a **dedicated orchestrator agent**. Orchestrations are defined in `.dot-agent-deck.toml` alongside existing modes. A new **Orchestration tab** launches all role agents simultaneously, while a designated orchestrator agent (the `start = true` role) drives all delegation decisions dynamically. Every agent remains fully interactive — the user can talk to any pane at any time.

The orchestrator agent is an LLM that never does work itself — it only delegates to worker agents and coordinates their collaboration. dot-agent-deck acts as the **message bus**: it intercepts the orchestrator's delegation commands, injects prompts into worker panes, monitors `/work-done` signals from workers, and reports results back to the orchestrator. The orchestrator decides routing (who to call next), parallel fan-out (multiple agents at once), and termination (when the orchestration is complete). Worker agents signal completion via a generic `/work-done` skill that writes a per-role summary file (`work-done-{role-name}.md`).

### Config Format

```toml
[[orchestrations]]
name = "code-review"

[[orchestrations.roles]]
name = "orchestrator"
command = "claude"
start = true  # the orchestrator — delegates work, never does it
prompt_template = """
You are an orchestration manager. You NEVER do work yourself.
You only delegate to available agents and coordinate their work.
"""

[[orchestrations.roles]]
name = "coder"
command = "claude --model sonnet"
description = "Implements code changes, fixes bugs, writes features"
prompt_template = "Always run cargo test before finishing."  # optional standing instructions
# clear = true  (default — restart agent session between delegations)

[[orchestrations.roles]]
name = "reviewer"
command = "claude"
description = "Reviews code for correctness, style, and edge cases"

[[orchestrations.roles]]
name = "security-auditor"
command = "claude"
description = "Audits for security vulnerabilities"

[[orchestrations.roles]]
name = "release"
command = "claude"
description = "Runs release workflow: changelog, tag, PR"
```

The `start = true` role is the orchestrator — it receives the user's initial request and delegates to worker roles. dot-agent-deck auto-appends the available agents list (built from worker `name` + `description`) and the delegation protocol to the orchestrator's `prompt_template`.

### Work-Done Skill

A single, generic `/work-done` skill is deployed as a project-scoped skill (e.g., `.claude/skills/agent-deck-work-done/SKILL.md` for Claude Code, `.opencode/commands/work-done.md` for OpenCode). Both CLIs support custom slash commands via markdown files.

The skill instructs the agent to:
1. Write a summary file (e.g., `.dot-agent-deck/work-done.md`) describing what it accomplished
2. Run `dot-agent-deck work-done` to notify the orchestrator

The orchestrator then:
1. Identifies which pane triggered the notification → maps to role name
2. Reads the agent's summary file
3. Determines the next role from the orchestration config
4. Constructs the next role's prompt by combining its `prompt_template` with the previous role's summary
5. Injects the prompt into the next role's pane (or waits for user approval in manual mode)

This design means:
- **`prompt_template` is purely about the task** — no handoff mechanics
- **Agents don't need to know about roles or handoff formats** — they just call `/work-done`
- **One skill works for all agents** regardless of CLI (Claude Code, OpenCode, etc.)
- **The orchestrator owns all routing logic** — role ordering, context passing, termination

### User Flow

1. User opens new dir dialog, selects a directory
2. If `.dot-agent-deck.toml` has `[[orchestrations]]`, they appear as options alongside modes
3. User selects an orchestration (e.g., "code-review")
4. New Orchestration tab opens — all role panes are created, each role's `command` launches
5. The orchestrator (`start = true`) gets focus; its prompt is injected (base `prompt_template` + auto-appended agents list + delegation protocol)
6. User types a request to the orchestrator (e.g., "implement task 1 of this PRD")
7. Orchestrator calls `/work-done` with a delegation payload (target agents + task prompt)
8. dot-agent-deck parses the delegation, optionally restarts worker sessions (`clear = true`), prepends worker's `prompt_template` if present, injects combined prompt into target pane(s)
9. Worker agent(s) work interactively — user can interact with any pane at any time
10. Worker calls `/work-done` → writes `work-done-{role-name}.md`; if parallel delegation, system waits for all workers
11. dot-agent-deck combines worker summaries and injects them into the orchestrator pane
12. Orchestrator decides: delegate again (back to step 7) or signal `DONE`
13. On `DONE`, orchestration is marked complete

### Orchestration Tab Layout

**Left sidebar** — role cards stacked vertically:
- Role name + command (e.g., "coder — claude")
- Current status (Working, Waiting, Done)
- Active role(s) highlighted

**Right area** — two view modes toggled by keybinding:
- **Focused**: Full-width pane for the active role's agent
- **Split**: All agent panes visible side by side

**Bottom status bar** — orchestration-level info:
- `"code-review: coder working, reviewer waiting"`
- `"code-review: reviewer + security-auditor working (parallel)"`
- `"code-review: complete"`

### Config Generation Extension

The existing config generation flow (`config_gen.rs`) that guides agents to create `.dot-agent-deck.toml` is extended to also suggest orchestrations:
- Ask if the user wants agent orchestrations (e.g., TDD cycle, code + review)
- If yes: ask about roles, commands, auto vs manual, which role starts
- Generate the `[[orchestrations]]` section alongside `[[modes]]`

## Scope

### In Scope
- `OrchestrationConfig` and `OrchestrationRoleConfig` structs in `project_config.rs` with `description`, `clear`, optional `prompt_template`
- Config validation: exactly one `start = true` role, unique role names, worker description warnings
- Orchestrator prompt construction: auto-append available agents list and delegation protocol to `start = true` role's `prompt_template`
- Generic `/work-done` skill: project-scoped command files for Claude Code and OpenCode, per-role files (`work-done-{role-name}.md`)
- `dot-agent-deck work-done` CLI subcommand: notifies the daemon that an agent finished
- Message bus: intercept orchestrator delegation commands, dispatch prompts to worker panes, report work-done summaries back to orchestrator
- Parallel fan-out: orchestrator delegates to multiple agents, system waits for all `/work-done` signals before reporting back
- Worker context isolation: restart agent session between delegations when `clear = true` (default)
- Orchestration tab type in the tab manager with role cards and pane layout
- Focused and split view modes for the orchestration tab
- Prompt injection via PTY stdin (reusing embedded pane write mechanism)
- Orchestration completion when orchestrator signals `DONE`
- New dir dialog: show orchestrations when toml defines them
- Config generation: extend prompt to suggest orchestrations

### Out of Scope
- Multi-directory orchestrations (all roles work in same directory)
- Remote agent orchestration (all agents run locally)
- TDD orchestration with unit tests (unit tests too tightly coupled to implementation; functional/integration testing possible but infrastructure doesn't exist yet)
- Integration with external CI/CD or git push automation

## Technical Approach

### Config Parsing (`src/project_config.rs`)

Extend `ProjectConfig` to optionally include orchestrations:

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct ProjectConfig {
    #[serde(default)]
    pub modes: Vec<ModeConfig>,
    #[serde(default)]
    pub orchestrations: Vec<OrchestrationConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OrchestrationConfig {
    pub name: String,
    pub roles: Vec<OrchestrationRoleConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OrchestrationRoleConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub start: bool,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub prompt_template: Option<String>,
    #[serde(default = "default_clear")]
    pub clear: bool,  // default true — restart agent session between delegations
}
```

The orchestrator agent (`start = true`) drives all routing. `description` on worker roles is used to auto-build the orchestrator's available agents list. `prompt_template` on workers is optional standing instructions prepended to each task prompt. `clear` controls whether the agent session is restarted between delegations for context isolation.

### Work-Done Skill and CLI Subcommand

**Skill files** (project-scoped, deployed when orchestration starts):
- `.claude/skills/agent-deck-work-done/SKILL.md` — for Claude Code agents
- `.opencode/commands/work-done.md` — for OpenCode agents

Each skill instructs the agent to:
1. Write a free-form summary to `.dot-agent-deck/work-done-{role-name}.md` describing what it accomplished
2. Run `dot-agent-deck work-done` to notify the daemon

Per-role files (`work-done-{role-name}.md`) prevent parallel agents from overwriting each other's summaries.

**`dot-agent-deck work-done` CLI subcommand**:
- Sends a notification to the running daemon (via the existing daemon communication channel)
- Includes the pane ID of the calling agent (derived from environment or PTY context)
- The daemon maps pane ID → role name and processes the work-done signal

### Message Bus (new `src/orchestration.rs`)

dot-agent-deck acts as a message bus between the orchestrator agent and worker agents:

```rust
pub struct OrchestrationState {
    pub config: OrchestrationConfig,
    pub role_pane_ids: HashMap<String, String>,  // role name → pane ID
    pub pending_workers: HashSet<String>,          // role names we're waiting on
    pub status: OrchestrationStatus,
}

pub enum OrchestrationStatus {
    WaitingForOrchestrator,  // orchestrator is thinking/working
    Delegated,               // workers are executing, waiting for work-done signals
    Completed,               // orchestrator signaled DONE
}
```

**Message bus responsibilities:**
1. Parse orchestrator's `/work-done` output to identify delegation (targets + prompt) or completion (`DONE`)
2. Dispatch prompts to target worker panes (with optional session restart and `prompt_template` prepend)
3. Track pending workers in parallel fan-out
4. Combine worker summaries when all pending workers complete
5. Inject combined results into orchestrator pane

### Prompt Construction and Injection

**Orchestrator prompt** (constructed once at orchestration launch):
1. Start with the orchestrator's `prompt_template` from config
2. Auto-append available agents list built from worker roles' `name` + `description`
3. Auto-append delegation protocol instructions (hardcoded by dot-agent-deck)

**Worker prompt** (constructed on each delegation):
1. If worker has `prompt_template`: prepend as standing instructions
2. Append the orchestrator's task prompt from the delegation
3. Append `/work-done` instruction
4. Inject via PTY stdin into the worker's pane

### New Dir Dialog Integration

When a directory is selected and its `.dot-agent-deck.toml` has `[[orchestrations]]`:
- Show orchestrations as selectable options alongside modes
- On selection, create an Orchestration tab instead of a Mode tab
- Deploy `/work-done` skill files for the agent CLIs used in the orchestration

### Config Generation Extension (`src/config_gen.rs`)

Add orchestration guidance to the prompt template:
- Include the `[[orchestrations]]` format documentation
- Add guidelines for suggesting orchestrations based on project type
- Include example orchestrations (TDD, code-review)

## Success Criteria

- User can define orchestrations in `.dot-agent-deck.toml` with an orchestrator role and worker roles
- All role agents launch simultaneously in their own panes
- The orchestrator (`start = true`) receives its prompt with auto-appended agents list and delegation protocol
- Orchestrator delegates to worker(s) via `/work-done` with delegation payload; dot-agent-deck injects prompts into target panes
- Parallel fan-out works: orchestrator delegates to multiple agents, system waits for all `/work-done` signals before reporting back
- Worker agent sessions are restarted between delegations when `clear = true` (default)
- Worker `prompt_template` (standing instructions) is prepended to task prompt when present
- Orchestration terminates when orchestrator signals `DONE`
- User can interact with any pane at any time during the orchestration
- Role cards show current status (Working/Waiting/Done)
- Focused/split view toggle works
- Config generation suggests orchestrations for applicable projects
- Existing modes and dashboard functionality are unaffected

## Milestones

Milestones are ordered to reach a usable orchestrator-driven workflow as fast as possible ("Phase 1: Dogfood"), so we can use the orchestration system on this very PRD while building the remaining features.

### Phase 1a: Manual validation — prove the workflow before automating

Goal: validate the full orchestration chain manually. User coordinates handoffs by invoking `/work-done`, generating context files via Claude, and pasting prompts. Zero automation needed.

- [x] **M1: Config parsing** — `OrchestrationConfig` and `OrchestrationRoleConfig` structs with validation (exactly one `start = true`, unique role names) in `src/project_config.rs`
- [x] **M3a: Basic orchestration tab (pane launch)** — new `Tab::Orchestration` variant that launches all role panes side by side (split view only, no role cards yet), no prompt injection — user types manually
- [x] **M4a: Work-done skill file** — generic `/work-done` skill for Claude Code (`.claude/skills/agent-deck-work-done/SKILL.md`) that instructs the agent to write a summary to `.dot-agent-deck/work-done.md`
- [x] **M4b: Handoff file format design** — handoff file format documented in Design Decisions section (note: superseded by orchestrator pattern — see design decision "2026-04-17: Orchestrator agent pattern")

### Phase 1b: Config update and orchestrator prompt construction

Goal: update config structs to match the orchestrator pattern, build the orchestrator's prompt with auto-appended agents list.

- [x] **M1c: Config struct update** — remove `max_rounds` and `auto` from `OrchestrationConfig`; add `description: Option<String>`, `clear: bool` (default true), make `prompt_template: Option<String>` on `OrchestrationRoleConfig`; update validation and all tests
- [x] **M3b: Orchestrator prompt construction** — on orchestration launch, construct the orchestrator's full prompt: base `prompt_template` + auto-generated "Available agents" list (from worker `name` + `description`) + delegation protocol instructions; write to `.dot-agent-deck/orchestrator-context.md` and inject one-liner into the `start = true` pane once agent starts

### Phase 1c: Message bus — work-done handling and delegation dispatch

Goal: dot-agent-deck acts as message bus between orchestrator and workers. Orchestrator delegates, workers execute, results flow back.

- [x] **M4c: CLI subcommands** — two separate subcommands: `dot-agent-deck delegate --to <role> --task "..."` (orchestrator sends work to workers) and `dot-agent-deck work-done --task "..." [--done]` (workers report results back); sends `DaemonMessage::Delegate` or `DaemonMessage::WorkDone` via Unix socket; daemon maps pane ID → role name via `pane_role_map`; writes summary to `work-done-{role-name}.md`
- [x] **M4d: Separate skill files** — `/work-done` skill (`.claude/skills/agent-deck-work-done/`) for workers only; `/delegate` skill (`.claude/skills/agent-deck-delegate/`) for orchestrator only; clean separation prevents workers from accidentally delegating
- [x] **M5: Delegation dispatch** — `dispatch_delegate_events()` in `src/ui.rs` drains `DelegateSignal` events, resolves target roles from config, restarts panes if `clear = true` (close + create + update all mappings), prepends worker's `prompt_template` to task, injects prompt via PTY stdin; `PendingDispatch` queue handles deferred injection for restarted panes; `OrchestrationConfig` stored in `Tab::Orchestration` for dispatch access
- [x] **M5b: Orchestrator feedback loop** — `feedback_worker_results()` in `src/ui.rs` drains `WorkDoneSignal` events from workers, immediately injects result summary into orchestrator pane (no batching — each worker result forwarded as it arrives); orchestrator decides when it has enough info to proceed
- [x] **M5c: Orchestration completion** — when orchestrator signals `--done` via `work-done`, dot-agent-deck marks the orchestration as complete and shows a completion notification

**Design questions resolved (M4c/M4d/M5/M5b):**
- **Delegation**: Separate `delegate` command — `--to <role>` (repeatable for fan-out), `--task <description>`
- **Work completion**: Separate `work-done` command — `--task <summary>`, `--done` (orchestrator completion)
- **Completion signal**: `--done` flag on the `work-done` command (orchestrator only)
- **Role name resolution**: Daemon resolves pane ID → role name via `pane_role_map` in `AppState` (populated when orchestration tab opens). No env var or skill parameter needed.
- **Feedback model**: Immediate per-worker feedback — no batching or waiting for parallel workers. Orchestrator receives each result as it arrives and decides when to proceed.

**At this point**: orchestrator-driven workflow is functional. User talks to orchestrator, orchestrator delegates to workers (including parallel), results flow back, orchestrator decides next steps or completes.

### Phase 2: Polish — UI and integration

- [x] **M6: New dir dialog integration** — show orchestrations alongside modes when TOML defines them, launch Orchestration tab on selection, deploy `/work-done` skill files automatically
- [x] **M7: Role cards sidebar** — left sidebar with role name, status (Working/Waiting/Done), active role highlight
- [x] **M9: Focused/split view toggle** — keybinding to switch between full-width active role pane and side-by-side split
- [ ] **M10: Status bar** — show orchestration-level info in bottom status bar (e.g., "code-review: coder working, reviewer waiting")
- [ ] **M11: Config generation extension** — update `src/config_gen.rs` prompt to suggest orchestrations alongside modes
- [ ] **M14: Documentation** — update README and/or user-facing docs with orchestration usage: `[[orchestrations]]` TOML format, role configuration (`start`, `description`, `prompt_template`, `clear`), delegation workflow, and example orchestrations (code-review, TDD)

### Phase 3: Quality

- [ ] **M12: Tests** — config parsing ✓, message bus ✓, work-done handling ✓, delegation dispatch ✓; still needed: parallel fan-out integration test, feedback loop integration test
- [x] **M13: No regressions** — all 384 existing tests passing after M5/M5b implementation

## Key Files

- `src/project_config.rs` — Orchestration config structs and TOML parsing
- `src/orchestration.rs` — New module: message bus, delegation dispatch, work-done handling, prompt construction
- `src/tab.rs` — Orchestration tab type, role card rendering, pane layout
- `src/ui.rs` — Orchestration tab UI, focused/split toggle, status bar
- `src/config_gen.rs` — Extended prompt template with orchestration guidance
- `src/lib.rs` — Export new orchestration module
- `src/main.rs` — Wire orchestration into event loop, handle `work-done` subcommand
- `.claude/skills/agent-deck-work-done/SKILL.md` — Work-done skill template (deployed per-project)

## Design Decisions

### 2026-04-16: Skill-based handoff replaces file-based handoff
- **Decision**: Instead of requiring agents to write structured handoff files with STATUS lines and specific formats, a generic `/work-done` skill signals completion. The orchestrator handles all routing.
- **Rationale**: Agents are trained to use tools/skills — it's more natural and reliable than "write a markdown file with STATUS on line 1". A single generic skill works across all agent CLIs (Claude Code, OpenCode) without per-role customization. The orchestrator owns the routing logic, so agents don't need to know about other roles or handoff formats.
- **Impact**: Removed `writes`, `reads`, and `handoff_dir` from config. Removed `notify` crate dependency (no file watcher needed). `prompt_template` is now purely task-focused. Added new scope: `/work-done` skill files and `dot-agent-deck work-done` CLI subcommand.
- **Eliminated risk**: "Handoff file format compliance" risk is gone — agents call a skill instead of writing a specific format.

### 2026-04-16: Split M3 into M3a (pane launch) and M3b (prompt injection)
- **Decision**: M3a launches panes from config with no prompt injection — user types manually into each role's pane. M3b (after M2) injects `prompt_template` into the start role's pane automatically.
- **Rationale**: M3a validates M1 (config parsing) end-to-end and gives a usable two-agent setup immediately without needing the state machine. The user can manually coordinate agents, which is the `auto = false` workflow anyway. Prompt injection requires knowing which role to prompt and when — that's state machine territory (M2).
- **Impact**: M3 split into M3a and M3b in Phase 1 milestones. M3a can be implemented immediately after M1. M3b depends on M2.

### 2026-04-16: Dogfood-first milestone ordering
- **Decision**: Reorder milestones to reach a minimal working two-agent orchestration (Phase 1) in ~5 tasks, then polish UI/automation in Phase 2. Manual operations (launching via code, pressing `o` to advance) are acceptable in Phase 1.
- **Rationale**: We can use the orchestration system on this very PRD while building the remaining features. Dogfooding surfaces design issues early and provides immediate value. The new dir dialog integration, role cards, auto mode, and config generation are nice-to-haves that don't block a working workflow.
- **Impact**: Phase 1 (M1–M5) delivers a usable but rough two-agent cycle. Phase 2 (M6–M11) adds polish. Phase 3 (M12–M13) ensures quality.

### 2026-04-17: Manual-first validation before automation
- **Decision**: Instead of building the full state machine (M2) next, focus on creating the `/work-done` skill and designing the handoff file format. Validate the entire orchestration workflow manually before automating any step.
- **Rationale**: The user can already launch orchestration tabs with multiple agents (M3a). By creating the `/work-done` skill and defining the handoff file logic, the full workflow can be tested manually: (1) give coder agent its task, (2) invoke `/work-done` in the agent's pane, (3) ask Claude to generate the reviewer's context file from the summary, (4) paste the reviewer's prompt into its pane. This validates the chain end-to-end with zero automation risk and surfaces design issues before committing to implementation.
- **Impact**: M4 (work-done skill) is partially prioritized ahead of M2 (state machine). The skill file and summary file format are implemented first. The CLI subcommand (`dot-agent-deck work-done`) and daemon notification are deferred until automation is needed. M2, M3b, and M5 remain unchanged but are deferred until the manual workflow is validated. A new interim step exists: manually generating handoff context files based on the designed format/logic.
- **Manual test loop**: Launch orchestration tab → manually prompt coder → `/work-done` in coder pane → manually generate reviewer context → paste reviewer prompt. Once validated, proceed to automate (M2 → M3b → M5).

### 2026-04-17: Orchestrator agent pattern replaces config-based routing

- **Decision**: Replace config-encoded routing (sequential role lists, `max_rounds`, `auto` flag, manual `o` key advance) with a dedicated **orchestrator agent** — the `start = true` role — that drives all delegation dynamically. dot-agent-deck becomes a message bus, not a router.

- **Rationale**: Routing decisions (who to call next, whether to fan out in parallel, when to terminate) are judgment calls that an LLM handles better than a config file. A reviewer sending work back to a coder because of critical issues vs. approving and moving to release is contextual — encoding every possible path in TOML is brittle and complex. The orchestrator agent understands context and decides.

- **Key principle — context isolation**: Separate agents are valuable when a fresh perspective produces better results (reviewer evaluating code without the coder's reasoning, security auditor with no "I know this is safe" bias). Not valuable when the next agent needs the reasoning (architect→coder). The orchestrator pattern handles both since the orchestrator controls how much context to pass in each delegation.

- **Config changes**:
  - **Removed**: `max_rounds` (orchestrator decides when to stop via `DONE`), `auto` (all orchestrations are orchestrator-driven)
  - **Added**: `description` on worker roles (optional; used to build the orchestrator's available agents list automatically)
  - **Added**: `clear` on worker roles (optional, default `true`; whether to restart the agent session between delegations for context isolation)
  - **Changed**: `prompt_template` on workers becomes optional standing instructions (behavior/constraints like "always run tests before finishing"), not task instructions. The orchestrator provides task instructions each time via delegation.
  - **Changed**: `prompt_template` on the orchestrator (start=true) is base instructions. dot-agent-deck auto-appends the available agents list (built from worker `name` + `description`) and the delegation protocol.

- **Delegation protocol** (hardcoded by dot-agent-deck, not user-configurable):
  - Orchestrator delegates via `/work-done` with a delegation payload (targets + prompt)
  - Worker agents signal completion via `/work-done` with a summary payload
  - Orchestrator signals orchestration complete via `DONE` in its `/work-done` output
  - All communication goes through work-done files — no PTY output parsing

- **Parallel fan-out**: When the orchestrator delegates to multiple agents, dot-agent-deck injects prompts into all target panes, then waits for ALL to call `/work-done` before combining summaries and reporting back to the orchestrator. No mid-conversation injection (would corrupt agent state).

- **Per-role work-done files**: `work-done-{role-name}.md` instead of a single `work-done.md`, so parallel agents don't overwrite each other.

- **The flow**:
  1. User talks to orchestrator pane
  2. Orchestrator calls `/work-done` with delegation: targets + prompt
  3. dot-agent-deck parses delegation, injects prompt into target pane(s) (prepending worker's `prompt_template` if present)
  4. If `clear = true` (default), agent session is restarted before injection
  5. Agents work; user interacts with any pane freely
  6. Agent calls `/work-done` → writes `work-done-{role}.md`
  7. If parallel delegation: wait for all. Then combine summaries, inject into orchestrator pane
  8. Orchestrator decides next step (delegate again, or `DONE`)

- **What dot-agent-deck does NOT do**: routing decisions, round counting, approval/rejection parsing. The orchestrator LLM owns all of that.

### 2026-04-17: Split `work-done` into `delegate` and `work-done` commands

- **Decision**: Replace the single overloaded `work-done` command (with `--delegate` flag) with two separate CLI subcommands: `delegate --to <role> --task "..."` (orchestrator → workers) and `work-done --task "..."` (workers → orchestrator). Separate `DaemonMessage` variants (`Delegate` and `WorkDone`) and separate skill files.

- **Rationale**: The original `work-done --delegate <role>` conflated two different intents — "here's work for you" and "I'm done with my work". This made it ambiguous at the CLI level and required the daemon to infer intent from flag combinations + sender identity. Separate commands make intent explicit, prevent workers from accidentally delegating, and produce cleaner skill files (workers only see `work-done`, orchestrator only sees `delegate`).

- **Impact**: `DaemonMessage` enum gains `Delegate(DelegateSignal)` alongside `WorkDone(WorkDoneSignal)`. `WorkDoneSignal` loses its `delegate: Vec<String>` field. New `DelegateSignal` struct with `to: Vec<String>`. Separate skill directories: `agent-deck-work-done/` (workers) and `agent-deck-delegate/` (orchestrator). Orchestrator context file updated to reference `delegate` command.

### 2026-04-17: Immediate worker feedback (no batching)

- **Decision**: When a worker calls `work-done`, its result is forwarded to the orchestrator pane immediately. No waiting for other parallel workers to finish.

- **Rationale**: Batching assumes the orchestrator needs all results before deciding, but in practice: (1) sequential delegation (one worker at a time) would be blocked waiting for nothing, (2) the orchestrator is the smart agent — it can track what it's waiting for and decide when to proceed, (3) simpler implementation with no pending-worker tracking state. Workers always report to the orchestrator; workers never delegate to other workers.

- **Impact**: No `pending_workers` tracking in state. `feedback_worker_results()` is a simple drain-and-forward loop. The "parallel fan-out: wait for all" behavior described in the orchestrator pattern design decision is superseded — the orchestrator handles coordination itself.

- **Impact**: Removed `max_rounds`, `auto` from config and validation. Removed manual advance (`o` key). Parallel execution now in-scope. Milestones restructured: M1b (routing design) resolved by this decision, M2 becomes message bus instead of state machine, M5 (manual advance) removed.

- **Not supported via orchestration**: TDD with separate agents — unit tests are too tightly coupled to function signatures/struct fields. A tester agent writing tests from a spec will guess at the implementation shape, then the coder contorts code to match or rewrites the tests. Functional/integration testing could work but the infrastructure doesn't exist yet.

### 2026-04-17: Handoff prompt format for inter-agent context passing
- **Decision**: When the orchestrator advances to the next role, it constructs a handoff file at `.dot-agent-deck/handoff-to-{role-name}.md` and injects a reference-based prompt into the next agent's pane. The orchestrator also auto-appends a `/work-done` instruction so agents know how to signal completion.
- **Injected prompt format**:
  ```
  {next role's prompt_template from TOML}

  The context from the previous role ({previous_role_name}) is in `.dot-agent-deck/handoff-to-{role-name}.md`. Read it first.

  When you are finished, run /work-done to summarize your work.
  ```
- **Handoff file format** (`.dot-agent-deck/handoff-to-{role-name}.md`):
  ```markdown
  {verbatim contents of .dot-agent-deck/work-done.md}
  ```
- **Rationale**: Reference-based prompt avoids blowing up PTY stdin with large pastes — the agent reads the file at its own pace. The `/work-done` instruction is auto-appended by the orchestrator so users don't need to include it in every `prompt_template`. Agents may also discover `/work-done` from its skill description, but the explicit instruction guarantees it.
- **Impact**: Defines the contract between M4a (work-done skill writes `.dot-agent-deck/work-done.md`) and M5 (manual advance constructs handoff and injects prompt). During manual validation, the user constructs this handoff themselves; M5 automates it.

## Risks

- **PTY prompt injection timing**: If the agent is mid-output when a prompt is injected, text may interleave. Mitigation: wait for agent idle/waiting status before injecting.
- **Work-done skill discovery**: Agents must discover and invoke the `/work-done` skill when they finish. Mitigation: the orchestrator's delegation prompt includes a `/work-done` instruction; the skill is also discoverable via agent CLI's skill listing.
- **Orchestrator delegation parsing**: dot-agent-deck must reliably parse DELEGATE and DONE signals from the orchestrator's `/work-done` output. LLM output may vary in format. Mitigation: define a clear structured format in the delegation protocol; validate agent names against config; reject malformed delegations with error feedback to orchestrator.
- **Parallel fan-out deadlock**: If one agent in a parallel delegation never calls `/work-done`, the orchestrator blocks indefinitely waiting for all results. Mitigation: user can interact with the stuck agent directly; future enhancement could add a timeout.
- **Embedded pane write**: The prompt injection mechanism depends on writing to pane stdin. Need to verify this works reliably for all supported agent CLIs (Claude Code, OpenCode, etc.).
- **Config backward compatibility**: Adding optional `orchestrations` field to `ProjectConfig` must not break existing configs that only define modes. Using `#[serde(default)]` ensures this.
- **Skill file deployment**: dot-agent-deck must deploy `/work-done` skill files to the correct CLI-specific directories when an orchestration starts. Different CLIs use different paths (`.claude/skills/` vs `.opencode/commands/`).
- **Worker session restart**: When `clear = true`, restarting the agent session (killing and relaunching the command) must be clean — no orphaned processes, no lost PTY state. Need to verify the PTY teardown/recreation is reliable.
