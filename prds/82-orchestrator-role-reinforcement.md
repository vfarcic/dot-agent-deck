# PRD #82: Orchestrator role reinforcement against delegation drift

**Status**: Problem-stated; solution TBD
**Priority**: Medium
**Created**: 2026-05-10
**GitHub Issue**: [#82](https://github.com/vfarcic/dot-agent-deck/issues/82)

## Problem

The orchestrator agent — the role with `start = true` in an `[[orchestrations]]` block (e.g. `.dot-agent-deck.toml:21-44`) — drifts from delegation discipline over the course of a session. Two distinct triggers have been observed or are suspected:

1. **Post-compaction (observed).** After Claude Code compacts the conversation, the orchestrator frequently stops delegating and starts performing implementation, review, or audit work itself, despite its `prompt_template` explicitly forbidding that. The user notices, manually re-prompts ("delegate, don't do it yourself"), and the orchestrator resumes correct behaviour — until the next drift.
2. **Long-handoff drift (suspected, not yet measured).** Even without compaction, on long PRDs with many worker handoffs, the orchestrator may begin doing small pieces of work itself rather than delegating. Whether this is a genuine separate failure mode or just the same compaction issue triggered earlier than the user noticed is currently unknown — the investigation milestone has to settle this.

### Why this matters

The orchestrator is the user's only interface to a multi-agent flow. When it drifts:

- Worker agents go unused — the user paid the configuration cost but loses the parallelism, role isolation, and context-isolation benefits that motivated multi-role orchestration in the first place.
- The orchestrator's context fills with implementation details that worker context isolation is *supposed* to keep out, accelerating compaction (the very event that triggers the most visible failure mode — a feedback loop).
- The user has to babysit the orchestrator, defeating the "describe what you want, walk away" workflow that PRD #58 (multi-role agent orchestration) was built for.
- It silently undermines confidence in the whole orchestration feature: a user who sees the orchestrator do work itself a few times will reasonably conclude the orchestration setup is broken or pointless.

### What we know about the current setup

Concrete plumbing as of this PRD's creation date — relevant inputs to the investigation, not a prescription:

- **Orchestrator role definition lives in project config.** `[[orchestrations.roles]]` with `start = true`, configured per-project (e.g. `.dot-agent-deck.toml:18-44` in this repo). The `prompt_template` is the standing instruction.
- **Worker handoff goes through async `dot-agent-deck delegate` + `work-done-<role>.md`.** Per `feedback_delegate_workflow.md`: orchestrator delegates, waits for a system reminder, reads the work-done file, continues. Whatever emits that reminder is a candidate injection point.
- **Claude Code emits `PreCompact` and `PostCompact` hooks.** Mapped in `src/hook.rs:95-96` (`PreCompact` → `Compacting`, `PostCompact` → `Thinking`). `PreCompact` is in the auto-installed hook list (`src/hooks_manage.rs:5-16`); **`PostCompact` is not auto-installed today** — we observe the post-compact moment via `PreCompact` for status only, not via a `PostCompact` hook that could carry context back to the agent.
- **OpenCode's compaction hook surface is unknown.** `map_opencode_event_type` (`src/hook.rs:177-199`) covers `session.*`, `tool.execute.*`, and `permission.*`. No compaction event is mapped. Whether OpenCode even *has* a compaction event, and whether its hooks support emitting context back to the agent (the way Claude Code's `PostCompact` `additionalContext` does), is unverified.
- **Hook auto-install plumbing already exists** for Claude Code (`src/hooks_manage.rs`) and for OpenCode (`src/opencode_manage.rs` plugin write). If a chosen solution wants to install a new hook per orchestrator role, the install path is not the hard part.

### What we don't know

These are the questions the investigation milestone must answer before any solution is committed to:

1. **Where does the drift mechanically come from?**
   - Does the orchestrator's `prompt_template` survive compaction, or is it summarized into the post-compact synthetic message?
   - In Claude Code specifically: how is the `prompt_template` actually injected — as system prompt, as first user message, as part of the system block? Different injection points have different compaction survival properties.
   - In OpenCode: same question. Probably a different answer.
   - Reproduce both reported failure modes deterministically before designing fixes for them. If we can't reproduce, we don't understand the problem.

2. **What injection points are actually available?**
   - Claude Code `PostCompact` hook — confirm its `additionalContext` / `hookSpecificOutput` mechanism actually reaches the next turn's agent context, and whether it's per-session or global.
   - The existing work-done system reminder — locate the exact code path that emits it; confirm we can extend the reminder text per-orchestrator-role.
   - OpenCode equivalents — does the plugin surface support either of the above? If not, what does it support?
   - Any harness-agnostic option we haven't considered (e.g. a `dot-agent-deck delegate` post-completion hook that types into the orchestrator pane).

3. **What's the right reinforcement *content*?**
   - Hardcoded "you are an orchestrator, delegate" — simple but loses every project-specific carve-out the role's `prompt_template` defines (e.g. "MAY run `/prd-next` directly", "STOP before delegating to release until user confirms").
   - Re-inject the role's own `prompt_template` verbatim — single source of truth, but may be long enough to be a token cost concern when fired on every handoff.
   - Configurable per-role "reminder" field in TOML — third option, more plumbing, more flexibility.
   - The right answer probably differs between the post-compact case (one-shot, can afford to be longer) and the per-handoff case (frequent, must be terse).

4. **Is reinforcement *the right shape* of fix at all?**
   - Drift might be addressable by changing how `prompt_template` is injected (e.g. ensuring it's actually in the system prompt, where compaction can't touch it) rather than re-injecting it after the fact. Investigation should not assume the answer is a reminder mechanism — that's a hypothesis to test, not a foregone conclusion.
   - Drift might also be a symptom of `prompt_template` wording rather than placement. If a stricter prompt eliminates drift without any new mechanism, that's the cheapest possible fix and we should not skip past it.

5. **Does this belong in the deck binary or in user/role config?**
   - A reminder mechanism could be entirely user-side: the orchestrator's own `prompt_template` could include "after every worker handoff, restate your role to yourself before continuing." No code change.
   - It could be deck-generated as part of orchestration auto-install, sitting alongside the existing hook-install path.
   - It could be a runtime injection point inside `dot-agent-deck delegate`. Different blast radius and different testability.

6. **Cost of *not* fixing this.**
   - How often does drift actually cost the user time? Is it a 1-in-20 PRD problem or a 1-in-3 problem? Worth a quick measurement before committing engineering time. If it's rare, the right answer might be "document the manual re-prompt pattern" rather than build new mechanism.

## Out of Scope (until M1 reshapes scope)

- Anything that changes the `[[orchestrations.roles]]` schema or the project config format. Role-config schema changes are deferred until the investigation actually concludes a schema change is the chosen path.
- Cross-role reinforcement. This PRD is specifically about the orchestrator (`start = true`) role. If worker roles also benefit from mid-session role reinforcement, that's a separate PRD.
- Visible UI for "the orchestrator was reminded" status. Reinforcement, if implemented, should be silent from the user's perspective — they should just see less drift.
- Changing model selection / provider behaviour for the orchestrator (e.g. "use Opus for orchestrator because it's stickier on roles"). Out of scope; orthogonal lever.

## Milestones

- [ ] **M1 — Investigation and decision.** Reproduce both failure modes (post-compaction and long-handoff drift) on Claude Code and OpenCode. Audit the actual injection mechanism and survival of `prompt_template` across compaction in each harness. Catalog candidate injection points (`PostCompact` hook, work-done system reminder, OpenCode equivalents, `prompt_template` placement changes, prompt rewording). For each candidate, list: blast radius, plumbing cost, harness coverage (Claude Code only vs both), token cost per fire, risk of over-correction (orchestrator refusing legitimate carve-outs). Decide the chosen approach, or decide explicitly that no fix is worth shipping at this time. Output: this PRD updated with a new `## Solution` section and a `## Refined Milestones` section that fills in M2+ with concrete implementation milestones based on the chosen path.
- [ ] **M2+** — TBD, defined by M1.

## Validation Strategy

Defined by M1. The validation shape depends on which solution path is chosen — for example:
- A reinforcement-text approach validates by reproducing the drift, applying the fix, and showing the orchestrator stays in role through the same scenario.
- A `prompt_template` placement fix validates by inspecting the post-compact agent context.
- A "no fix worth shipping" outcome validates by documenting the manual re-prompt pattern in `docs/` and closing the PRD with that documentation as the deliverable.

The user (PRD owner) does final pre-PR sign-off per `feedback_validate_pre_pr.md`: not stopping per-milestone for end-to-end testing, single validation pass before the PR.

## Risks & Mitigations

| Risk | Mitigation |
|---|---|
| Drift may be unfixable cheaply — it could be a model-stickiness property no reminder can fully cure | M1 must include a "no fix worth shipping" exit. Not every observed friction has an engineering answer; documenting the workaround is a valid M1 outcome. |
| Reinforcement that's too forceful causes the orchestrator to refuse legitimate carve-outs (e.g. running `/prd-next` directly, reading PRD files) | Any chosen reminder must be tested against the existing `prompt_template` carve-outs, not just the "always delegate" rule. M1's decision must specify how this regression is prevented. |
| Claude Code and OpenCode have asymmetric hook surfaces — chosen mechanism may only work on one harness | Acceptable as long as the asymmetry is named explicitly in the decision and the OpenCode story is "documented limitation" rather than "silent gap." |
| Investigation produces no clear winner and stalls | M1 has a hard deliverable: an updated PRD. "Picked option X with these tradeoffs" is the goal, not "found the perfect option." Pick the least-bad and ship. |
| Token-cost reinforcement on every handoff adds up on long PRDs | Quantify in M1 — measure tokens per reminder × typical handoff count per PRD. If the chosen content is short (1-2 lines), this is unlikely to be material; verify rather than assume. |

## References

- `.dot-agent-deck.toml:18-44` — this repo's orchestrator role definition (the failure mode's primary subject)
- `src/hook.rs:84-101` — Claude Code hook event type mapping (current `PreCompact` / `PostCompact` handling)
- `src/hook.rs:177-199` — OpenCode hook event type mapping (no compaction event mapped today)
- `src/hooks_manage.rs:5-16` — Claude Code hook auto-install list (`PostCompact` is **not** in this list today)
- `src/opencode_manage.rs` — OpenCode plugin auto-install path
- `assets/roles.toml` — embedded role library (worker roles only; orchestrator is project-defined)
- `assets/config_gen_prompt.md` — config-gen prompt that produces orchestrator `prompt_template`s
- `feedback_delegate_workflow.md` — describes the work-done system reminder that's a candidate injection point
- PRD #58 — multi-role agent orchestration (the feature whose value is undermined when the orchestrator drifts)
- PRD #59 — orchestration documentation (where any "manual re-prompt" workaround would land if M1 chooses no-fix)
