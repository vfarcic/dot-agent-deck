# PRD #58: Multi-Role Agent Orchestration with Skill-Based Handoff

**Status**: In Progress
**Priority**: High
**Created**: 2026-04-16
**GitHub Issue**: [#58](https://github.com/vfarcic/dot-agent-deck/issues/58)

## Problem Statement

When multiple AI agents (different models/CLIs) need to collaborate on a task — e.g., a TDD cycle where one agent writes tests and another implements code, or a code-then-review workflow — users must manually coordinate turn-taking, copy handoff context between sessions, and switch between panes. There is no structured way to define roles, handoff artifacts, or termination conditions. The dashboard can monitor agents but cannot orchestrate their collaboration.

The primitives exist (agents can read/write files, git diffs are natural handoff artifacts) but there is no tool that ties it together while keeping agents interactive and the human in the loop.

## Solution Overview

Add an **orchestration system** to dot-agent-deck that coordinates multi-role agent workflows through a **skill-based handoff** mechanism. Orchestrations are defined in `.dot-agent-deck.toml` alongside existing modes. A new **Orchestration tab** launches all role agents simultaneously, and coordinates turn-taking — either automatically or with manual user approval — while every agent remains fully interactive.

Instead of requiring agents to write structured handoff files with specific formats, a generic `/work-done` skill (deployed as a project-scoped command) signals completion. When an agent invokes `/work-done`, it writes a free-form summary of what it did. The **orchestrator** handles all routing: it identifies which role finished (via pane-to-role mapping), reads the summary, constructs the next role's prompt with the relevant context, and injects it. Agents never need to know about other roles, handoff formats, or the orchestration topology.

### Config Format

```toml
[[orchestrations]]
name = "tdd-cycle"
max_rounds = 3
auto = false  # false = user presses key to advance, true = auto-inject on handoff detection

[[orchestrations.roles]]
name = "tester"
command = "claude"
start = true
prompt_template = "Write failing tests for the feature."

[[orchestrations.roles]]
name = "coder"
command = "claude --model sonnet"
prompt_template = "Make the tests pass."
```

### Work-Done Skill

A single, generic `/work-done` skill is deployed as a project-scoped command file (e.g., `.claude/commands/work-done.md` for Claude Code, `.opencode/commands/work-done.md` for OpenCode). Both CLIs support custom slash commands via markdown files.

The skill instructs the agent to:
1. Write a summary file (e.g., `.ai/work-done.md`) describing what it accomplished
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
3. User selects an orchestration (e.g., "tdd-cycle")
4. New Orchestration tab opens — all role panes are created, each role's `command` launches
5. The role with `start = true` gets focus and its `prompt_template` is injected
6. Agent works interactively (user approves tool calls, answers questions)
7. Agent calls `/work-done` → writes summary file, runs `dot-agent-deck work-done`
8. Orchestrator detects completion via pane-to-role mapping:
   - **`auto = true`**: Reads summary, builds next role's prompt, injects it, shifts focus
   - **`auto = false`**: Shows notification in UI; user presses keybinding (`o`) to advance
9. Next role receives prompt (task instructions + previous role's summary), works interactively
10. Cycle repeats until max_rounds reached or user stops the orchestration
11. User pushes to git when satisfied

### Orchestration Tab Layout

**Left sidebar** — role cards stacked vertically:
- Role name + command (e.g., "tester — claude")
- Current status (Working, Waiting, Done)
- Round indicator (Round 2/3)
- Last handoff status (NEEDS_CHANGES)
- Active role highlighted

**Right area** — two view modes toggled by keybinding:
- **Focused**: Full-width pane for the active role's agent, follows turn automatically
- **Split**: All agent panes visible side by side

**Bottom status bar** — orchestration-level info:
- `"tdd-cycle: Round 2/3 — Coder's turn (auto)"`
- Or in manual mode: `"Press 'o' to send prompt to Coder"`

### Config Generation Extension

The existing config generation flow (`config_gen.rs`) that guides agents to create `.dot-agent-deck.toml` is extended to also suggest orchestrations:
- Ask if the user wants agent orchestrations (e.g., TDD cycle, code + review)
- If yes: ask about roles, commands, auto vs manual, which role starts
- Generate the `[[orchestrations]]` section alongside `[[modes]]`

## Scope

### In Scope
- `OrchestrationConfig` and `OrchestrationRoleConfig` structs in `project_config.rs`
- Config validation: exactly one `start = true` role, unique role names
- Generic `/work-done` skill: project-scoped command files for Claude Code and OpenCode
- `dot-agent-deck work-done` CLI subcommand: notifies the orchestrator that an agent finished
- Orchestration state machine: track current role, round count, pane-to-role mapping
- Orchestration tab type in the tab manager with role cards and pane layout
- Focused and split view modes for the orchestration tab
- Prompt construction: combine `prompt_template` with previous role's summary for context
- Prompt injection via PTY stdin (reusing embedded pane write mechanism)
- Manual mode: keybinding (`o`) to advance to next role
- Auto mode: automatic prompt injection on work-done detection
- Termination: stop on max_rounds or user-initiated stop, show completion notification
- New dir dialog: show orchestrations when toml defines them
- Config generation: extend prompt to suggest orchestrations

### Out of Scope
- Multi-directory orchestrations (all roles work in same directory)
- Remote agent orchestration (all agents run locally)
- Conditional role chains (role A → B or C based on status) — future enhancement
- Parallel role execution (roles run sequentially, one at a time) — future enhancement
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
    #[serde(default = "default_max_rounds")]
    pub max_rounds: usize,
    #[serde(default)]
    pub auto: bool,
    pub roles: Vec<OrchestrationRoleConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OrchestrationRoleConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub start: bool,
    pub prompt_template: String,
}
```

Note: `handoff_dir`, `writes`, and `reads` fields have been removed. The orchestrator manages all handoff content internally — agents just call `/work-done` and the orchestrator handles routing.

### Work-Done Skill and CLI Subcommand

**Skill files** (project-scoped, deployed when orchestration starts):
- `.claude/commands/work-done.md` — for Claude Code agents
- `.opencode/commands/work-done.md` — for OpenCode agents

Each skill instructs the agent to:
1. Write a free-form summary to `.ai/work-done.md` describing what it accomplished
2. Run `dot-agent-deck work-done` to notify the orchestrator

**`dot-agent-deck work-done` CLI subcommand**:
- Sends a notification to the running daemon (via the existing daemon communication channel)
- Includes the pane ID of the calling agent (derived from environment or PTY context)
- The orchestrator receives the notification, maps pane ID → role, and handles the transition

### Orchestration State Machine (new `src/orchestration.rs`)

```rust
pub struct OrchestrationState {
    pub config: OrchestrationConfig,
    pub current_role_index: usize,
    pub round: usize,
    pub status: OrchestrationStatus,
    pub role_pane_ids: Vec<String>,     // role index → pane ID
    pub last_summary: Option<String>,   // previous role's work-done summary
}

pub enum OrchestrationStatus {
    Running,
    WaitingForUser,    // manual mode, work-done detected
    Completed(String), // reason: max rounds, user stopped
}
```

### Prompt Construction and Injection

When a role completes and the next role should start:
1. Read the summary file written by the completed role
2. Build the next role's prompt: `prompt_template` + "\n\n## Context from previous role\n" + summary
3. Write the constructed prompt to the next role's pane via PTY stdin
4. Shift focus to that pane

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

- User can define orchestrations in `.dot-agent-deck.toml` and select them from the new dir dialog
- All role agents launch simultaneously in their own panes
- The `start = true` role receives its prompt and gets focus automatically
- Agent calling `/work-done` triggers the orchestrator to route to the next role
- In manual mode, user sees notification and presses key to advance
- In auto mode, next role's prompt is injected automatically with previous role's summary
- Orchestration terminates on max_rounds or user-initiated stop
- Role cards show current status, round, and summary info
- Focused/split view toggle works
- Config generation suggests orchestrations for applicable projects
- Existing modes and dashboard functionality are unaffected

## Milestones

Milestones are ordered to reach a usable two-agent workflow as fast as possible ("Phase 1: Dogfood"), so we can use the orchestration system on this very PRD while building the remaining features.

### Phase 1: Dogfood — minimal working two-agent orchestration

Goal: two agents launch, first gets a prompt, user manually advances after each turn. Rough edges are fine — the point is to start using it.

- [x] **M1: Config parsing** — `OrchestrationConfig` and `OrchestrationRoleConfig` structs with validation (exactly one `start = true`, unique role names) in `src/project_config.rs`
- [ ] **M2: Orchestration state machine** — `OrchestrationState` with pane-to-role mapping, round tracking, current role index in new `src/orchestration.rs`
- [x] **M3a: Basic orchestration tab (pane launch)** — new `Tab::Orchestration` variant that launches all role panes side by side (split view only, no role cards yet), no prompt injection — user types manually
- [ ] **M3b: Prompt injection for start role** — inject `prompt_template` into the `start = true` role's pane on orchestration launch (depends on M2 for state tracking). Note: role commands are already launched and start role gets focus in M3a; this milestone adds prompt_template injection after state machine exists.
- [ ] **M4: Work-done skill + CLI subcommand** — generic `/work-done` command files for Claude Code and OpenCode; `dot-agent-deck work-done` subcommand that notifies the orchestrator; orchestrator reads summary and updates state
- [ ] **M5: Manual advance (`o` key)** — on keypress, orchestrator reads previous role's summary, constructs next role's prompt (`prompt_template` + summary context), injects it via PTY stdin, shifts focus

**At this point**: you can define a two-role orchestration in TOML, launch it, and manually cycle between agents. Good enough to dogfood on PRD #58 tasks.

### Phase 2: Polish — UI, automation, and integration

- [ ] **M6: New dir dialog integration** — show orchestrations alongside modes when TOML defines them, launch Orchestration tab on selection, deploy `/work-done` skill files automatically
- [ ] **M7: Role cards sidebar** — left sidebar with role name, command, status (Working/Waiting/Done), round indicator, active role highlight
- [ ] **M8: Auto mode** — when `auto = true`, automatically inject next role's prompt on work-done detection without waiting for `o` keypress
- [ ] **M9: Focused/split view toggle** — keybinding to switch between full-width active role pane and side-by-side split
- [ ] **M10: Termination and status bar** — stop on max_rounds or user-initiated stop, show orchestration-level info in bottom status bar
- [ ] **M11: Config generation extension** — update `src/config_gen.rs` prompt to suggest orchestrations alongside modes

### Phase 3: Quality

- [ ] **M12: Tests** — config parsing, state machine, work-done handling, orchestration lifecycle
- [ ] **M13: No regressions** — all existing tests passing

## Key Files

- `src/project_config.rs` — Orchestration config structs and TOML parsing
- `src/orchestration.rs` — New module: state machine, work-done handling, prompt construction, turn coordination
- `src/tab.rs` — Orchestration tab type, role card rendering, pane layout
- `src/ui.rs` — Orchestration tab UI, focused/split toggle, status bar, `o` keybinding
- `src/config_gen.rs` — Extended prompt template with orchestration guidance
- `src/lib.rs` — Export new orchestration module
- `src/main.rs` — Wire orchestration into event loop, handle `work-done` subcommand
- `skills/work-done.md` — Generic work-done skill template (deployed per-project)

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

## Risks

- **PTY prompt injection timing**: If the agent is mid-output when a prompt is injected, text may interleave. Mitigation: wait for agent idle/waiting status before injecting.
- **Work-done skill discovery**: Agents must discover and invoke the `/work-done` skill when they finish. Mitigation: the initial `prompt_template` can mention "call /work-done when you're finished" as a gentle hint.
- **Max rounds edge case**: If agents keep cycling without converging, the orchestration should terminate cleanly at max_rounds with a clear message rather than leaving agents in limbo.
- **Embedded pane write**: The prompt injection mechanism depends on writing to pane stdin. Need to verify this works reliably for all supported agent CLIs (Claude Code, OpenCode, etc.).
- **Config backward compatibility**: Adding optional `orchestrations` field to `ProjectConfig` must not break existing configs that only define modes. Using `#[serde(default)]` ensures this.
- **Skill file deployment**: The orchestrator must deploy `/work-done` skill files to the correct CLI-specific directories when an orchestration starts. Different CLIs use different paths (`.claude/commands/` vs `.opencode/commands/`).
