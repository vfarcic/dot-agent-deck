# PRD #82: Orchestrator role reinforcement against delegation drift

**Status**: M1 complete — solution decided; implementation (M2+) pending
**Priority**: Medium
**Created**: 2026-05-10
**GitHub Issue**: [#82](https://github.com/vfarcic/dot-agent-deck/issues/82)

## Validation refresh (2026-06-14)

Re-validated against current code — verdict: **current** as an investigation doc; the problem framing, open questions, and milestone structure still hold, and no reinforcement code has landed (`rg reinforc` over `src/` is empty). Two precision fixes applied below: the worker handoff is a **PTY-injected one-liner** written into the orchestrator's pane by the daemon's `handle_work_done` (`src/state.rs`), **not** a Claude Code system-reminder / `additionalContext`; and OpenCode's event map is now `map_opencode_event_type` (line drift only). The `PreCompact` mapping in `src/hook.rs` and the auto-install-list claims (`PreCompact` installed) remain accurate. (Corrected by M1, 2026-06-22: the `PostCompact` arm is **dead** — Claude Code does not emit a `PostCompact` event at all; the real post-compaction signal is `SessionStart{source:"compact"}`. See `## Solution`.)

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
- **Worker handoff goes through async `dot-agent-deck delegate` + `work-done-<role>.md`.** `Commands::Delegate` / `Commands::WorkDone` (`src/main.rs`); on completion the daemon's `handle_work_done` (`src/state.rs`) writes a one-liner **directly into the orchestrator's pane** via `write_to_pane_and_submit` ("Worker {role} has completed their task. Read .dot-agent-deck/work-done-{role}.md…"). NOTE: this is a **PTY-injected message, not** a Claude Code system-reminder / `additionalContext` (an earlier draft described it as a "system reminder"). That PTY-write site is a candidate injection point.
- **Claude Code emits a `PreCompact` hook; `PostCompact` is *not* an event Claude Code actually emits.** `src/hook.rs:95-96` maps both `"PreCompact"` → `Compacting` and `"PostCompact"` → `Thinking`, but **M1 found the `"PostCompact"` arm is dead** — Claude Code never sends that event, and the real post-compaction signal is `SessionStart` with `source: "compact"` (see `## Solution`). `PreCompact` is in the auto-installed hook list (`src/hooks_manage.rs:5-16`) and fires *before* compaction, so today we observe the compaction moment via `PreCompact` for status only — not via any post-compaction hook that carries context back to the agent. (This corrects the bullet's original wording, which assumed `PostCompact` was a real hook that merely wasn't auto-installed.)
- **OpenCode's compaction hook surface is unknown.** `map_opencode_event_type` (`src/hook.rs:177-199`) covers `session.*`, `tool.execute.*`, and `permission.*`. No compaction event is mapped. Whether OpenCode even *has* a compaction event, and whether its hooks support emitting context back to the agent (the way Claude Code's post-compact `additionalContext` is meant to), is unverified.
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

## Solution

**Decided 2026-06-22 (M1 outcome).** Drift's root cause is **placement**, not model stickiness: the orchestrator role is delivered as a transcript `Read` result, never as a system prompt, so compaction summarizes it away — and it does so **symmetrically on both Claude Code and OpenCode** (both auto-compact by replacing detailed history with an LLM summary). The chosen fix is to **detect the post-compaction moment on both harnesses and re-assert the orchestrator role there, through deck-owned channels that touch no developer-owned file.**

### Root cause (delivery path confirmed in code)

`build_orchestrator_context` (`src/ui.rs:1625`) composes the role text and `prepare_orchestrator_prompt` (`src/ui.rs:1701`) writes it to `.dot-agent-deck/orchestrator-context.md`, then PTY-injects a one-liner telling the agent to `Read` that file. The role therefore enters the conversation as a `Read` tool result inside the transcript — exactly the content compaction summarizes/discards first. `spawn` (`src/agent_pty.rs:768`) carries no `--append-system-prompt`, so nothing pins the role into the system prompt. **Code-confirmed:** this delivery path is identical for both harnesses — OpenCode reuses the same deck-side `Read`-result placement, so the role enters the transcript the same compaction-mortal way on both. **Doc-sourced (not code-confirmed):** that OpenCode auto-compacts the way Claude Code does (summary replaces detailed history, last user message replayed) comes from OpenCode's documentation (investigation OpenCode Q1), not from anything in the deck's code. Taken together the failure mode is symmetric across harnesses, not Claude-Code-only. This is the mechanical origin of trigger #1; trigger #2 (drift without compaction) remains unconfirmed.

### Rejected alternatives

- **Durable system-prompt placement** — rejected. The flag path (`--append-system-prompt[-file]`) is blocked because the deck spawns an opaque launcher (`command = "devbox run agent-*"`) it can't pass flags through. The managed-instructions-file path (`CLAUDE.md` / `AGENTS.md` / OpenCode's `instructions` array) is ruled out by a product boundary: **the deck must not write or manage developer-owned instruction files.** OpenCode's `experimental.chat.system.transform` is unusable — mutations to `output.system` are silently discarded before reaching the LLM (sst/opencode #17100, closed not-planned). So durable placement, the cleanest root-cause fix in principle, has no deck-owned, both-harness path available today.
- **Pure reword of the `prompt_template`** — complement only, not the deliverable. It is compaction-mortal (it sits in the same transcript position the summarizer erases) and only affects newly generated configs. It raises the baseline floor but cannot by itself cure the structural loss.
- **Per-handoff work-done one-liner re-assert** — deferred (see Refined Milestones). It carries the highest over-correction risk (a terse "always delegate" drilled on every handoff is exactly what makes the orchestrator refuse its legitimate carve-outs), and the trigger it targets (#2, drift without compaction) is still unconfirmed. Building it before #2 is measured risks shipping the riskiest mechanism for a problem that may not exist independently.

### Chosen approach — detect + re-assert on both harnesses

- **Detection — Claude Code:** `SessionStart` with `source: "compact"`, which fires *after* compaction. The hook is already auto-installed and the event already reaches the binary, but the `source` field is currently dropped into `_extra` in `src/hook.rs` — parse it. `PreCompact` is the wrong injection point because injecting before compaction lets the summarizer erase the reminder. Cleanup: `src/hook.rs:96` maps a dead `"PostCompact"` string Claude Code never emits — remove or repurpose it.
- **Detection — OpenCode:** the observable `session.compacted` event. It already passes the deck plugin's `startsWith("session.")` forwarder unchanged and reaches the binary; it is dropped only because `map_opencode_event_type` (`src/hook.rs`) has no matching arm. Minimal fix is a single arm — e.g. mapping `session.compacted` to the existing `EventType::Compacting`. No plugin or protocol change.
- **These detection facts are doc-sourced, not yet verified against a live binary:** that `SessionStart{source:"compact"}` fires after compaction, that `session.compacted` is observable, and that #15174 breaks Claude Code's `additionalContext` path all come from documentation and issue trackers (the investigation flagged this). The M2 gating spike verifies exactly these signals on each harness before any injection is built on them.
- **Injection — Claude Code:** the deck's own **PTY lever** — on detecting post-compaction for an orchestrator pane, the daemon PTY-writes the re-assert into that pane (the same mechanism the work-done one-liner uses, `src/state.rs:778`). This deliberately avoids Claude Code's post-compact `additionalContext` path, which is reportedly broken upstream (bug #15174).
- **Injection — OpenCode:** either the same PTY lever, or the native `experimental.session.compacting` hook (`output.context.push(...)`) added to the deck-owned `plugin_template` (`src/opencode_manage.rs`), which folds the role text into the summary itself and is arguably more robust. The per-harness injection choice is settled in M2 after the live spike — not over-committed here.
- **Re-assert content:** it re-uses the **same single-source-of-truth role pointer** as startup — "re-read `.dot-agent-deck/orchestrator-context.md`" (the file persists on disk through compaction) — so the full role and its carve-outs are restored from one source, never a hardcoded "always delegate." It differs from the startup prompt only in its trailing instruction: startup ends with "Acknowledge your role and wait for instructions"; the post-compaction re-assert instead says "resume coordinating the current work — delegate the next step; do not implement, review, or audit yourself." It must **not** tell the orchestrator to wait for instructions mid-session — that would stall it. (On the OpenCode native-hook path the content is the role text pushed into the summary rather than a re-read pointer.)
- **Token cost is negligible.** The re-assert fires **one-shot per compaction** (not per turn, not per handoff) and carries only a short **re-read pointer**, not the full role text — so the per-handoff token concern in the Risks table does not apply to this design. That concern applies only to the **deferred** per-handoff one-liner re-assert, which is gated on confirming trigger #2.
- **Free complement:** tighten the generated orchestrator `prompt_template` wording in `assets/config_gen_prompt.md` so new configs start stricter and foreground the carve-outs.

### Over-correction guard (the PRD's named risk)

Because the re-assert points at the role's own file (single source of truth), the carve-outs (MAY run `/prd-next` / `/prd-update-progress` directly, read the PRD file, the two user gates) are preserved verbatim rather than compressed into "always delegate." Validation must include a scenario where, after the re-assert fires, the orchestrator still legitimately runs a carve-out command and honors the user gates.

### Harness story

Both harnesses are covered, so the asymmetry the PRD feared is eliminated. If anything it reverses: OpenCode's compaction-injection hook (`experimental.session.compacting`) is documented and purpose-built, while Claude Code's post-compact `additionalContext` is reportedly broken upstream (bug #15174) — which is why Claude Code uses the PTY path.

## Milestones

- [x] **M1 — Investigation and decision.** Reproduce both failure modes (post-compaction and long-handoff drift) on Claude Code and OpenCode. Audit the actual injection mechanism and survival of `prompt_template` across compaction in each harness. Catalog candidate injection points (`PostCompact` hook, work-done system reminder, OpenCode equivalents, `prompt_template` placement changes, prompt rewording). For each candidate, list: blast radius, plumbing cost, harness coverage (Claude Code only vs both), token cost per fire, risk of over-correction (orchestrator refusing legitimate carve-outs). Decide the chosen approach, or decide explicitly that no fix is worth shipping at this time. Output: this PRD updated with a new `## Solution` section and a `## Refined Milestones` section that fills in M2+ with concrete implementation milestones based on the chosen path. **Done** — see `## Solution` (root cause: compaction-mortal placement, symmetric across both harnesses; chosen fix: detect + re-assert on both) and `## Refined Milestones` below. On the "reproduce both failure modes" deliverable: the *mechanism* (compaction-mortal `Read`-result placement + compaction erasure) is established deterministically, but the *behavioral* drift itself is stochastic and not deterministically reproducible; live signal verification (that the post-compaction signal fires and a re-assert lands in context) is deferred to the M2 gating spike. So M1 settled the mechanism and the decision — not a finished behavioral reproduction.
- **M2+** — defined by M1; see `## Refined Milestones`.

## Refined Milestones

Concrete implementation milestones for the chosen detect-and-re-assert approach. They may be renumbered or organized as the work demands; the spike gates the rest.

- [ ] **M2 — Live-verification spike (first, gating).** Instrument the raw hook payloads, force a compaction on each harness (Claude Code via `/compact`; OpenCode via overflow or the proactive path), and confirm: the signal fires (`SessionStart{source:"compact"}` / `session.compacted`), it is parseable, and a re-assert injection lands in the post-compaction context. Independent go/no-go per harness — one failing does not sink the other. This doubles as the deterministic *mechanism* repro M1 mandated: the behavioral drift itself is stochastic and not deterministically reproducible, but the mechanism is.
- [ ] **M3 — Claude Code re-assert.** Parse `source` in `src/hook.rs`; on the `compact` case for an orchestrator pane, have the daemon PTY-write the resume-flavored re-assert. Remove the dead `"PostCompact"` arm — Claude Code never emits that event, so the correct post-compaction signal is `SessionStart{source:"compact"}` and removing the arm closes no real capability.
- [ ] **M4 — OpenCode re-assert.** Add the `session.compacted` map arm; implement the re-assert via the PTY lever and/or the native `experimental.session.compacting` hook (`output.context.push(...)`) in the deck-owned `plugin_template`.
- [ ] **M5 — Wording complement.** Tighten the generated orchestrator `prompt_template` in `assets/config_gen_prompt.md` (foreground the carve-outs, tighten the boundary) so new configs start stricter.
- [ ] **Validation.** The over-correction regression check (the orchestrator still runs its carve-outs and honors the user gates after the re-assert fires) plus a bounded manual drift observation. Per `feedback_validate_pre_pr.md`, the PRD owner does a single pre-PR sign-off, not per-milestone stops.
- [ ] **Deferred follow-up (gated on confirming trigger #2 is real).** The per-handoff work-done one-liner re-assert. Split into a separate task; do not build it until drift without compaction is measured as a genuine, distinct failure mode.

**Note for implementation milestones (CLAUDE.md rule 12).** M3/M4 touch hooks, the daemon, and orchestration, so the cross-version contract check applies: before the PR, answer "did this change the TUI↔daemon contract?" and run the cross-version manual test (older daemon + branch TUI; confirm a delegate still routes and hooks still arrive). Expectation: adding an event-type mapping arm plus a daemon-side PTY write is same-wire, so likely no `PROTOCOL_VERSION` bump and no `.breaking.md` — but the check must be made, not assumed.

## Validation Strategy

Settled by M1: the chosen path is **detect + re-assert on both harnesses**, so validation follows the reinforcement-text shape — demonstrate the mechanism, apply the re-assert, and show the orchestrator stays in role. The concrete checks live in `## Refined Milestones`:
- The **M2 gating spike** confirms the post-compaction signal fires (`SessionStart{source:"compact"}` / `session.compacted`), is parseable, and that a re-assert injection lands in the post-compaction context — independent go/no-go per harness. This doubles as the deterministic *mechanism* repro (the behavioral drift itself being stochastic).
- The **over-correction regression check** confirms the orchestrator still runs its legitimate carve-outs (e.g. `/prd-next` directly) and honors the two user gates after the re-assert fires.

The user (PRD owner) does final pre-PR sign-off per `feedback_validate_pre_pr.md`: not stopping per-milestone for end-to-end testing, single validation pass before the PR.

## Risks & Mitigations

| Risk | Mitigation |
|---|---|
| Drift may be unfixable cheaply — it could be a model-stickiness property no reminder can fully cure | M1 must include a "no fix worth shipping" exit. Not every observed friction has an engineering answer; documenting the workaround is a valid M1 outcome. |
| Reinforcement that's too forceful causes the orchestrator to refuse legitimate carve-outs (e.g. running `/prd-next` directly, reading PRD files) | Any chosen reminder must be tested against the existing `prompt_template` carve-outs, not just the "always delegate" rule. M1's decision must specify how this regression is prevented. |
| Claude Code and OpenCode have asymmetric hook surfaces — chosen mechanism may only work on one harness | Resolved by M1: the investigation confirmed coverage on **both** harnesses (Claude Code via the PTY lever on `SessionStart{source:"compact"}`; OpenCode via `session.compacted` plus the PTY lever or the native `experimental.session.compacting` hook). There is no silent gap — if anything the asymmetry reverses in OpenCode's favor. See the Solution's **Harness story**. |
| Investigation produces no clear winner and stalls | M1 has a hard deliverable: an updated PRD. "Picked option X with these tradeoffs" is the goal, not "found the perfect option." Pick the least-bad and ship. |
| Token-cost reinforcement on every handoff adds up on long PRDs | Resolved by M1 for the chosen design: it fires **one-shot per compaction** (not per turn or per handoff) and re-injects only a short **re-read pointer** to `orchestrator-context.md`, not the full role text — so per-handoff token cost is negligible and this concern does not apply. It would apply only to the **deferred** per-handoff one-liner re-assert, which is gated on confirming trigger #2. |

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
