# PRD #58: Multi-Role Agent Orchestration with File-Based Handoff

**Status**: Draft
**Priority**: High
**Created**: 2026-04-16
**GitHub Issue**: [#58](https://github.com/vfarcic/dot-agent-deck/issues/58)

## Problem Statement

When multiple AI agents (different models/CLIs) need to collaborate on a task — e.g., a TDD cycle where one agent writes tests and another implements code, or a code-then-review workflow — users must manually coordinate turn-taking, copy handoff context between sessions, and switch between panes. There is no structured way to define roles, handoff artifacts, or termination conditions. The dashboard can monitor agents but cannot orchestrate their collaboration.

The primitives exist (agents can read/write files, git diffs are natural handoff artifacts) but there is no tool that ties it together while keeping agents interactive and the human in the loop.

## Solution Overview

Add an **orchestration system** to dot-agent-deck that coordinates multi-role agent workflows through file-based handoff. Orchestrations are defined in `.dot-agent-deck.toml` alongside existing modes. A new **Orchestration tab** launches all role agents simultaneously, watches for handoff file changes, and coordinates turn-taking — either automatically or with manual user approval — while every agent remains fully interactive.

### Config Format

```toml
[[orchestrations]]
name = "tdd-cycle"
max_rounds = 3
handoff_dir = ".ai"
auto = false  # false = user presses key to advance, true = auto-inject on handoff detection

[[orchestrations.roles]]
name = "tester"
command = "claude"
start = true
writes = "handoff-tester.md"
reads = "handoff-coder.md"
prompt_template = "Write failing tests for the feature. Write handoff to {writes} with STATUS: DONE."

[[orchestrations.roles]]
name = "coder"
command = "claude --model sonnet"
start = false
writes = "handoff-coder.md"
reads = "handoff-tester.md"
prompt_template = "Make the tests pass. See {reads} for context. Write handoff to {writes} with STATUS: DONE or NEEDS_CHANGES."
```

### Handoff File Format

Handoff files are markdown with a `STATUS:` keyword on the first line:

```markdown
STATUS: DONE

## Summary
Wrote 5 unit tests for the auth middleware...

## Files Changed
- tests/auth_test.rs (new)

## Notes for Next Role
Tests expect a `validate_token()` function in src/auth.rs...
```

Valid status values: `DONE`, `NEEDS_CHANGES`, `LGTM`, `BLOCKED`

### User Flow

1. User opens new dir dialog, selects a directory
2. If `.dot-agent-deck.toml` has `[[orchestrations]]`, they appear as options alongside modes
3. User selects an orchestration (e.g., "tdd-cycle")
4. New Orchestration tab opens — all role panes are created, each role's `command` launches
5. The role with `start = true` gets focus and its `prompt_template` is injected
6. Agent works interactively (user approves tool calls, answers questions)
7. Agent writes its handoff file with a STATUS line
8. File watcher detects the change, reads the status:
   - **`auto = true`**: Immediately injects next role's `prompt_template` into their pane, shifts focus
   - **`auto = false`**: Shows notification in UI; user presses keybinding (`o`) to advance
9. Next role receives prompt, works interactively
10. Cycle repeats until `LGTM` status or `max_rounds` reached
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
- If yes: ask about roles, commands, handoff flow, auto vs manual, which role starts
- Generate the `[[orchestrations]]` section alongside `[[modes]]`

## Scope

### In Scope
- `OrchestrationConfig` and `OrchestrationRoleConfig` structs in `project_config.rs`
- Config validation: exactly one `start = true` role, unique role names, unique handoff filenames
- File watcher using `notify` crate on the handoff directory
- Handoff file parser: extract STATUS line and content
- Orchestration state machine: track current role, round count, status history
- Orchestration tab type in the tab manager with role cards and pane layout
- Focused and split view modes for the orchestration tab
- Prompt injection via PTY stdin (reusing embedded pane write mechanism)
- Manual mode: keybinding (`o`) to advance to next role
- Auto mode: automatic prompt injection on handoff detection
- Termination: stop on LGTM or max_rounds, show completion notification
- New dir dialog: show orchestrations when toml defines them
- Config generation: extend prompt to suggest orchestrations
- `{reads}` and `{writes}` template variable substitution in prompt_template

### Out of Scope
- Multi-directory orchestrations (all roles work in same directory)
- Remote agent orchestration (all agents run locally)
- Conditional role chains (role A → B or C based on status) — future enhancement
- Parallel role execution (roles run sequentially, one at a time) — future enhancement
- Handoff file versioning or history beyond current round
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
    #[serde(default = "default_handoff_dir")]
    pub handoff_dir: String,
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
    pub writes: String,
    pub reads: String,
    pub prompt_template: String,
}
```

### Orchestration State Machine (new `src/orchestration.rs`)

```rust
pub struct OrchestrationState {
    pub config: OrchestrationConfig,
    pub current_role_index: usize,
    pub round: usize,
    pub status: OrchestrationStatus,
    pub role_pane_ids: Vec<String>,
    pub last_handoff_status: Option<String>,
}

pub enum OrchestrationStatus {
    Running,
    WaitingForUser,   // manual mode, handoff detected
    Completed(String), // final status (LGTM, max rounds)
}
```

### File Watcher (in `src/orchestration.rs`)

Use the `notify` crate to watch the handoff directory:
- On file write/modify events matching a role's `writes` filename
- Parse the STATUS line from the file
- Update orchestration state
- Either auto-inject next prompt or set WaitingForUser status

### Prompt Injection

Reuse the existing embedded pane PTY write mechanism:
- Substitute `{reads}` and `{writes}` in `prompt_template` with actual file paths
- Write the rendered prompt to the target role's pane stdin
- Shift focus to that pane

### New Dir Dialog Integration

When a directory is selected and its `.dot-agent-deck.toml` has `[[orchestrations]]`:
- Show orchestrations as selectable options alongside modes
- On selection, create an Orchestration tab instead of a Mode tab

### Config Generation Extension (`src/config_gen.rs`)

Add orchestration guidance to the prompt template:
- Include the `[[orchestrations]]` format documentation
- Add guidelines for suggesting orchestrations based on project type
- Include example orchestrations (TDD, code-review)

## Success Criteria

- User can define orchestrations in `.dot-agent-deck.toml` and select them from the new dir dialog
- All role agents launch simultaneously in their own panes
- The `start = true` role receives its prompt and gets focus automatically
- Handoff file creation/modification is detected and triggers the next role transition
- In manual mode, user sees notification and presses key to advance
- In auto mode, next role's prompt is injected automatically
- Orchestration terminates on LGTM status or max_rounds
- Role cards show current status, round, and handoff info
- Focused/split view toggle works
- Config generation suggests orchestrations for applicable projects
- Existing modes and dashboard functionality are unaffected

## Milestones

- [ ] Config parsing: `OrchestrationConfig` and `OrchestrationRoleConfig` structs with validation (exactly one `start = true`, unique names/files) in `src/project_config.rs`
- [ ] Orchestration state machine: `OrchestrationState`, round tracking, status transitions, handoff file parser in new `src/orchestration.rs`
- [ ] File watcher: `notify`-based watcher on handoff directory with STATUS line detection in `src/orchestration.rs`
- [ ] Orchestration tab: new tab type with role cards (left sidebar), focused/split pane views (right area), status bar in `src/tab.rs` and `src/ui.rs`
- [ ] Prompt injection and turn coordination: template variable substitution, PTY stdin write, focus management, manual (`o` key) and auto modes
- [ ] New dir dialog integration: show orchestrations when toml defines them, launch Orchestration tab on selection
- [ ] Config generation extension: update `src/config_gen.rs` prompt to suggest orchestrations alongside modes
- [ ] Tests passing for config parsing, state machine, handoff parsing, and orchestration lifecycle
- [ ] All existing tests passing — no regressions

## Key Files

- `src/project_config.rs` — Orchestration config structs and TOML parsing
- `src/orchestration.rs` — New module: state machine, file watcher, handoff parser, prompt injection
- `src/tab.rs` — Orchestration tab type, role card rendering, pane layout
- `src/ui.rs` — Orchestration tab UI, focused/split toggle, status bar, `o` keybinding
- `src/config_gen.rs` — Extended prompt template with orchestration guidance
- `src/lib.rs` — Export new orchestration module
- `src/main.rs` — Wire orchestration into event loop
- `Cargo.toml` — Add `notify` crate dependency

## Risks

- **File watcher reliability**: The `notify` crate may behave differently across macOS/Linux (FSEvents vs inotify). Needs cross-platform testing. Fallback: periodic polling of handoff files.
- **PTY prompt injection timing**: If the agent is mid-output when a prompt is injected, text may interleave. Mitigation: wait for agent idle/waiting status before injecting.
- **Handoff file format compliance**: Agents may not consistently write the STATUS line correctly. Mitigation: validate format and show clear error if STATUS line is missing or malformed; include format instructions in the prompt_template.
- **Max rounds edge case**: If both agents keep saying NEEDS_CHANGES, the orchestration should terminate cleanly at max_rounds with a clear message rather than leaving agents in limbo.
- **Embedded pane write**: The prompt injection mechanism depends on writing to pane stdin. Need to verify this works reliably for all supported agent CLIs (Claude Code, OpenCode, etc.).
- **Config backward compatibility**: Adding optional `orchestrations` field to `ProjectConfig` must not break existing configs that only define modes. Using `#[serde(default)]` ensures this.
