# PRD #78: Tab-Level Status Indicators

**Status**: Not started
**Priority**: Medium
**Created**: 2026-05-09

## Problem

Users running multiple tabs (Dashboard, Modes, Orchestrations) cannot tell at a glance which tab needs their attention. To find out whether any agent is waiting on them, has errored, or has finished its task, they must switch into each tab in turn. The only existing tab-level decoration is the orchestration suffix `[done]` / `[active]` (`src/ui.rs:2297-2300`), which is limited to orchestration tabs and conveys orchestration-flow state rather than "should I switch here?"

The signal users actually want at the tab level is: *is there something actionable for me in that tab?*

## Solution

Aggregate the per-session statuses within each tab into a single tab-level badge, with strict priority:

1. **Needs Input** — at least one session in the tab is `SessionStatus::WaitingForInput`
2. **Error** — at least one session is `SessionStatus::Error` (and no session needs input)
3. **Idle** — every session is `SessionStatus::Idle` *or* a placeholder (`AgentType::None`)
4. **(no badge)** — otherwise; tab name renders alone

Working / Thinking / Compacting do **not** produce a badge. The badge is for "action needed," not "activity in progress."

The same rule applies uniformly to every tab type — Dashboard, Mode, Orchestration. There are no per-tab-type special cases.

The badge replaces the existing orchestration suffix `[done]` / `[active]`. Mapping is mostly lossless:

- `OrchestrationStatus::Completed` → all sessions idle → "Idle"
- `OrchestrationStatus::WaitingForOrchestrator` → orchestrator session is `WaitingForInput` → "Needs Input"
- `OrchestrationStatus::Delegated` → role agents working → no badge

### Design decisions (from discussion)

- **Placeholders count as idle-equivalent.** OpenCode panes show as `AgentType::None` until the first response, but a placeholder slot is functionally "ready for a task" — the same signal Idle conveys. So a tab full of placeholders + any number of idle sessions reports "Idle."
- **Show only when actionable.** Working / Thinking / Compacting tabs render the bare tab name. This keeps the tab strip uncluttered and avoids displacing tab names on narrower terminals.
- **Strict priority, no stacking.** A tab with both a `WaitingForInput` and an `Error` session shows only "Needs Input." Once the user resolves it, the next render surfaces "Error" automatically.
- **Text labels, not icons, in v1.** "Needs Input" (~11 chars) is roughly equivalent in width to the existing `[active]` suffix once that is removed. Iconography (colored dot) is a future polish if width turns out to matter.
- **No debounce in v1.** Sessions can transition Idle ↔ Working in short bursts, which will cause the Idle badge to flicker. Ship without debounce; only add one if it actually proves distracting in real use.

## Acceptance Criteria

### Aggregation rules
- [ ] A tab with at least one `WaitingForInput` session shows "Needs Input" — even if other sessions in the same tab are erroring, idle, or working.
- [ ] A tab with at least one `Error` session and no `WaitingForInput` session shows "Error".
- [ ] A tab where every non-placeholder session is `Idle`, and at least one session exists in any state, shows "Idle". Placeholder sessions (`AgentType::None`) are treated as idle-equivalent.
- [ ] A tab with any session in `Working`, `Thinking`, or `Compacting` (and no `WaitingForInput` or `Error`) shows no badge — bare tab name only.

### Tab-type coverage
- [ ] Dashboard tab participates in the same rule using its associated sessions.
- [ ] Mode tabs participate using their `agent_pane_id` session.
- [ ] Orchestration tabs participate using the union of orchestrator + role pane sessions.
- [ ] The orchestration suffix `[done]` / `[active]` is removed from `src/ui.rs:2297-2300`. Orchestration tabs render the same `name + badge` format as every other tab type.

### Rendering
- [ ] Badge appears as part of the tab label in `tab_bar_labels` (`src/ui.rs:2283-2303`), rendered consistently across the three tab types.
- [ ] Tab-name truncation logic still applies; long tab names degrade gracefully when a badge is present.

## Out of Scope

- Debouncing rapid Idle ↔ Working transitions. Revisit only if flicker is actually annoying in practice.
- Coloring the tab label or the badge by status (e.g., red for Error). Text-only in v1.
- Showing per-session counts inside the badge ("Idle 3/5", "Errors 2"). Aggregate single-state badge only.
- Reusing this aggregation for the OS-level bell or other notification surfaces. Bell logic stays as-is.
- New `SessionStatus` variants or changes to existing variants.
- Visual changes inside the tab body (cards, sidebar, etc.).

## Risks & Mitigations

| Risk | Mitigation |
|------|------------|
| Removing `[done]` / `[active]` is a behavioral change for users who relied on them | The new badge conveys equivalent or better information (Completed → Idle, WaitingForOrchestrator → Needs Input). Mention the change in release notes / changelog. |
| Mid-orchestration "Idle" false positive — orchestrator briefly idle between role rounds | In practice the orchestrator is in a `Working` state while a role is running and while it processes the role's return value, so the all-idle state should only surface at true completion or when waiting on the user (which is `Needs Input`, not `Idle`). Accept the residual flicker risk for v1; debounce if it manifests. |
| Tab strip width pressure on narrow terminals | "Needs Input" is the longest badge (~11 chars). Removing the orchestration suffix simultaneously frees comparable space. Reuse existing tab-name truncation; do not introduce a new layout system. |
| Placeholder semantics drift if a future agent type also reports `AgentType::None` for non-idle reasons | Aggregation logic is centralized in one helper — revisit if a new agent integration introduces a different meaning for `None`. |

## Implementation Notes

- All changes localize to `src/ui.rs`. The label-building loop at `src/ui.rs:2283-2303` is the single render site for tab labels.
- Add a helper, e.g. `fn tab_status_badge(tab: &Tab, sessions: &[SessionState]) -> Option<&'static str>`, that:
  1. Resolves the set of sessions belonging to the tab (Dashboard: all top-level sessions; Mode: by `agent_pane_id`; Orchestration: orchestrator + role pane IDs).
  2. Walks them in priority order, returning `Some("Needs Input")` / `Some("Error")` / `Some("Idle")` / `None`.
  3. Treats `AgentType::None` as idle-equivalent, mirroring the placeholder-card logic at `src/ui.rs:4919`.
- The orchestration `match` at `src/ui.rs:2297-2300` collapses into `Tab::Orchestration { name, .. } => name.clone()`. The badge is then appended uniformly for all three tab arms.
- No state plumbing changes required — `SessionStatus` is already available in `snapshot.sessions` at render time.

## References

- `src/state.rs:15` — `SessionStatus` enum (Thinking, Working, Compacting, WaitingForInput, Idle, Error)
- `src/ui.rs:5160-5172` — `status_style` function (per-session label + color)
- `src/ui.rs:4919-4924` — placeholder (`AgentType::None`) handling on cards
- `src/ui.rs:2283-2303` — tab label construction (`tab_bar_labels`)
- `src/tab.rs:38` — `OrchestrationStatus` enum
- `src/tab.rs:55-85` — `Tab` enum (Dashboard / Mode / Orchestration)
