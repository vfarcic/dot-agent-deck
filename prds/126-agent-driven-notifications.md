# PRD #126: Agent-driven notifications with minimal deck-side fallback

**Status**: Planning
**Priority**: Medium
**Created**: 2026-05-25
**GitHub Issue**: [#126](https://github.com/vfarcic/dot-agent-deck/issues/126)
**Closes**: [#99](https://github.com/vfarcic/dot-agent-deck/issues/99) (supersedes prior orchestrator-only design)
**Prerequisite for**: [#120](https://github.com/vfarcic/dot-agent-deck/issues/120) (scheduled issue dispatch). Note: the scheduler PRD #127 (cron-scheduled-prompt-dispatch) **shipped** and consumed this PRD's notification seam via a temporary `StderrNotifier` stub; #120 is the active downstream dependent.
**Related**: PRD #8 (terminal bell — per-session, in-terminal), PRD #20 (multi-agent support), PRD #58 / #82 (orchestration lifecycle)

## Validation refresh (2026-06-14)

Re-validated against current code — verdict: **current**. Nothing here has shipped yet (no `src/notifications.rs`, no `[notifications]` config block, no inactivity timer). The dependency relationships are real and live: PRD #99 is correctly closed/superseded by this PRD, and PRD #127 (cron scheduler) shipped consuming this PRD's notification seam via a temporary `StderrNotifier` stub. When implementing M2.2, fill the existing `Notifier`/`NotifyEvent`/`StderrNotifier` seam in `src/scheduler.rs` rather than inventing a new one.

## Problem Statement

Long-running agents and scheduled tasks in `dot-agent-deck` leave users with no reliable out-of-band signal that they need attention. The terminal bell (PRD #8) only reaches a focused terminal — useless once the user walks away. The original PRD #99 proposed solving this with a pluggable `Notifier` trait inside the deck plus four channel implementations (desktop, webhook, Slack, email), all wired into an orchestrator-completion lifecycle hook.

Three things are wrong with that design:

1. **The deck would re-implement what the ecosystem already provides.** Slack has an MCP server. So does Gmail. ntfy.sh, Pushover, `osascript`, `notify-send`, `gh`, `curl` — all exist and are well-maintained. A Rust trait + four channel implementations is duplication, and it makes the deck a credential holder for third-party services it has no business holding credentials for.

2. **The event source was too narrow.** PRD #99 fires on orchestrator completion only. Scheduled tasks running as single-agent cards, agent-side "blocked / needs input" states, scheduler-side failures before any agent spawns — none of these are covered. Expanding #99's event model to cover them while keeping the channel-pluggability adds complexity without buying anything.

3. **The agent is the only thing that knows what "done" means in context.** A lifecycle hook can fire "orchestration ended," but the agent itself knows whether it finished a task, hit a wall, or needs a decision. Inferring those from outside is heuristic; declaring them from inside is explicit.

The result of #99 as currently scoped would be: the deck ships ~500 lines of credential-handling channel code that duplicates existing MCPs, covers one event type, and still doesn't reach the scheduler PRD's use cases.

## Solution Overview

Replace the pluggable-channels model with **agent-driven delegation plus a minimal deck-side fallback**:

1. **Default path: agents notify via their own tools.** Users configure their agents (Claude Code, Codex, Gemini, Aider) with whatever notification tools they want — Slack MCP, ntfy CLI, `osascript`, Pushover, etc. The deck appends a configurable hint to the spawn prompt instructing the agent to notify the user on done / blocked / needs-input. The agent uses its own tools to deliver. The deck never touches Slack APIs, SMTP, or webhook URLs.

2. **Inactivity timer fires via prompt injection.** The deck observes "no PTY output for N minutes" per agent. On expiry, it injects a prompt — *"You have been inactive for N minutes; please notify the user via your notification tools"* — into the agent's session, reusing the existing prompt-injection plumbing. The agent then notifies via its own tools. No new sending code path is added for inactivity.

3. **One minimal deck-side desktop channel for events with no agent.** A single, non-pluggable local desktop notification (`notify-rust` or shell-out to `osascript` / `notify-send`). Used **only** when no agent exists to delegate to:
   - Scheduler-side pre-spawn failures (clone failed, `mkdir` failed before any agent spawned).
   - Agent crashed silently (process gone, can't inject a prompt anymore).

   Not exposed to agents. Not pluggable. Not a trait. Maybe 30 lines of code.

4. **Dedup rule.** When the deck fires its own desktop notification for a target, it marks that target as "user-notified" and suspends its inactivity timer. Without this, a crashed agent would generate a crash notification *and* an inactivity nudge 30 minutes later for the same dead target.

The deck's notification responsibilities collapse to: one minimal local channel, an inactivity-timer-with-injection, a prompt-hint config field, and docs telling users how to set up their preferred MCP/CLI. No `Notifier` trait, no dispatcher, no third-party credentials, no per-event routing.

## Scope

### In Scope

- **`agent_notification_hint` config field** in `.dot-agent-deck.toml`. User-authored string appended to the spawn-time prompt for every agent / orchestrator role. Optional. Ships with a reasonable default example users can copy and edit.
- **Inactivity timer per agent / task.** Tracks last PTY-output timestamp. On `notify_when_inactive_after` expiry, injects the inactivity prompt into that agent's session. Resets on any PTY output (including the agent calling its own notify tool — that's PTY activity).
- **Inactivity timer suspension rule.** When the deck fires its own desktop notification for a target, that target's inactivity timer is suspended (does not fire again until the next user-initiated action on the target).
- **Minimal deck-side desktop channel.** Single non-pluggable implementation. Decision between `notify-rust` crate vs. shell-out to `osascript` / `notify-send` deferred to M1.1 based on transitive dep weight. Used by the deck only — never callable by agents.
- **Configuration block** in `.dot-agent-deck.toml`:
  ```toml
  [notifications]
  desktop_enabled = false                    # default: off
  notify_when_inactive_after = "30m"         # default: unset (disabled)
  agent_notification_hint = """
  When you finish, get blocked, or need user input, send a notification
  via your available tools (Slack MCP, ntfy CLI, osascript, etc).
  """
  ```
- **Documentation under `site/`**: how to configure the hint, recommended MCP/CLI setups for common channels (Slack MCP first because the user already has it; ntfy and `osascript` as zero-setup alternatives), the inactivity model and its caveats, the deck-side desktop-channel role.
- **Closing #99** with a comment explaining the supersession.

### Out of Scope (this PRD)

- **No `Notifier` trait, dispatcher, or pluggable channels in the deck.** This is the architectural inversion of #99; it must stay out of scope.
- **No Slack / email / webhook / SMTP / Pushover implementations in deck code.** Those live in agent MCPs / CLIs.
- **No per-event-type filtering** ("send `done` to Slack but `blocked` to desktop"). All agent-fired notifications go through whatever channel the agent's tool was configured with. Filtering is a follow-up PRD if noise becomes a real complaint.
- **No two-way / bidirectional channels.** One-way only.
- **No deck-side credentials of any kind.** Third-party tokens stay with the agent's MCP/CLI config.
- **No agent-callable CLI** (`dot-agent-deck notify ...`). Agents use their own tools directly; we don't introduce a new shell entry-point for them to call.
- **No secondary timer for "stuck agent that didn't process the injection".** Documented limitation; future PRD if it becomes a real pain.
- **No orchestrator lifecycle hook.** The original #99 emission point is not implemented. The orchestrator role prompt should include the notification hint; lifecycle hooks are not needed.
- **No "exactly-once" guarantee.** Agents may notify zero, one, or many times per run. Documented; acceptable for v1.

## Success Criteria

- A user with `agent_notification_hint` configured can spawn an agent that has a notification MCP/CLI installed (e.g. Slack MCP), and the agent posts to that channel when it completes — without any deck-side Slack code, credentials, or webhook URL.
- A user with `notify_when_inactive_after = "30m"` and a long-running agent sees a notification (delivered via that agent's own tools) approximately 30 minutes after the agent goes quiet — driven by deck-injected prompt, not by deck-side sending.
- A scheduler-side failure (e.g. clone error before any agent spawns) produces a desktop notification via the minimal local channel.
- An agent crash (process gone unexpectedly) produces a desktop notification via the minimal local channel, and the inactivity timer for that agent is suspended — no redundant inactivity nudge 30 minutes later.
- The deck contains zero Slack / SMTP / webhook / Pushover code or credentials. `cargo tree` shows no third-party-service crates added.
- `cargo fmt --check` and `cargo clippy -- -D warnings` pass. `cargo test` passes including new tests.
- Documentation under `site/` covers the configuration, the inactivity model, recommended channel setups (Slack MCP / ntfy / osascript), and the documented limitations.
- #99 is closed with a link to this PRD as the supersession.

## Open Questions (resolve during M1)

1. **`notify-rust` vs. shell-out for the desktop channel.** `notify-rust` is cross-platform but pulls in transitive deps; shell-out to `osascript` / `notify-send` is dep-free but platform-branched. Working assumption: try `notify-rust` first; if transitive dep weight is bad, fall back to shell-out with explicit platform detection. M1.1 picks one.
2. **Inactivity detection signal.** "No PTY output for N minutes" is the proposed heuristic. Alternatives considered: "no tool calls for N minutes" (requires agent-specific introspection), "agent in waiting-for-input state" (requires reliable detection of that state). M1.2 confirms "no PTY output" is workable; if not, falls back to a simpler "session idle" measure already exposed by the deck.
3. **Prompt-injection behavior when the agent is at a "waiting for user input" prompt.** Different agents may treat a stdin write differently in that state — some append to the input buffer, some treat it as a system message. M1.2 verifies Claude Code's behavior at minimum; documents per-agent caveats for the others.
4. **What is "user-initiated action" for resetting suspension?** When the deck has suspended a timer after firing a desktop notification, what re-enables it? Options: any user keystroke into that agent, agent restart, manual config reload, tab close. Working assumption: agent restart and tab close are the meaningful events; suspension persists otherwise until the target is gone.
5. **Default for `agent_notification_hint`.** Should we ship a sane default, or require the user to author one? Working assumption: ship a default (the example in the config block above) so the feature works out-of-the-box for users with Slack MCP / ntfy already configured. Users can override.

## Milestones

### Phase 1: Inactivity timer + prompt-injection plumbing

- [ ] **M1.1** — Implement the inactivity timer: per-agent `last_pty_output` tracking, configurable threshold, fires on expiry. Resets on PTY output. Disabled when `notify_when_inactive_after` is unset.
- [ ] **M1.2** — Wire the inactivity expiry to existing prompt-injection plumbing: on fire, inject the inactivity prompt into the target agent's session. Verify Claude Code's behavior at "waiting for user input" state; document per-agent caveats.
- [ ] **M1.3** — Implement the `agent_notification_hint` config field. Append to spawn-time prompt for every agent / orchestrator role. Ship a sensible default.

### Phase 2: Deck-side desktop channel + dedup

- [ ] **M2.1** — Implement the minimal desktop channel. Decide `notify-rust` vs. shell-out based on transitive dep weight. Single file, no trait, not exposed to agents.
- [ ] **M2.2** — Wire scheduler-side pre-spawn failures and agent-crash detection to the desktop channel. (Scheduler failure wiring is a stub here; the actual scheduler PRD will hook into it.)
- [ ] **M2.3** — Implement the suspension/dedup rule: deck-fired desktop notification for a target → suspend that target's inactivity timer until a user-initiated reset event.

### Phase 3: Tests

- [ ] **M3.1** — Unit tests: inactivity timer reset/expiry/suspension behavior, prompt-injection content, dedup rule.
- [ ] **M3.2** — Integration test: spawn a fixture agent with the hint, simulate quiet PTY for N seconds, assert injection fires with expected content. Trigger a simulated agent crash, assert desktop notification fires and inactivity timer is suspended.

### Phase 4: Docs, close #99, ship

- [ ] **M4.1** — Documentation under `site/`: config reference, the inactivity model, recommended channel setups (Slack MCP, ntfy, `osascript`), known limitations.
- [ ] **M4.2** — Close #99 with a comment linking to this PRD. Audit any existing docs that reference #99 and redirect them.
- [ ] **M4.3** — Changelog fragment via `dot-ai-changelog-fragment`. Frame as "agents can now notify you out-of-band using their own tools; the deck nudges them and handles fallback for crashes."
- [ ] **M4.4** — `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test` all green. PR, review, audit, merge.

## Key Files

- `src/notifications.rs` (new) — inactivity timer, desktop channel, dedup state. Single module, ~150 LoC target.
- `src/config.rs` — extend with the `[notifications]` block.
- Existing prompt-injection module (TBD path; located in M1.2) — extended to accept inactivity-fired prompts.
- `tests/inactivity_timer.rs` (new) — unit + integration tests for the timer and suspension behavior.
- `site/content/docs/notifications.md` (new) — user-facing documentation.

## Risks and Mitigations

- **Risk**: Users without any configured notification MCP / CLI get the hint injected but no actual delivery happens — silent no-op. They may not realize they need to set up Slack MCP / ntfy.
  - *Mitigation*: Documentation calls this out prominently with a "Pick one" section recommending the easiest zero-setup option (`osascript` on macOS, `notify-send` on Linux, ntfy.sh elsewhere) before fancier options.

- **Risk**: Stuck/wedged agent doesn't process the inactivity injection — user never gets notified.
  - *Mitigation*: Documented limitation. Case 3 (agent crash detection) catches the case where the agent is fully dead. The narrow band of "alive but unresponsive" remains uncovered; follow-up PRD can add a secondary deck-side timer if it's a real pain.

- **Risk**: Per-agent behavior when injecting prompts at "waiting for input" state varies. Some agents may append to the input field rather than treat the injection as a system message.
  - *Mitigation*: M1.2 verifies Claude Code's behavior; per-agent caveats documented. PRD #20 (multi-agent support) is the natural place to remediate per-agent quirks if they emerge.

- **Risk**: Inactivity threshold misfires for agents that legitimately work silently for long stretches (e.g. a long compilation in a sub-shell).
  - *Mitigation*: Threshold is user-configurable and defaults to unset. Document that the heuristic is "no PTY output," so users who run silent long-haul work should set a higher threshold or disable.

- **Risk**: The minimal desktop channel adds a heavy cross-platform crate (`notify-rust` and its deps) for what is supposed to be ~30 lines of code.
  - *Mitigation*: M2.1 evaluates `cargo tree` for `notify-rust` before committing. If transitive deps balloon, fall back to shell-out with explicit `cfg(target_os)` branches.

- **Risk**: Scope creep — users will ask for per-event-type routing, Slack-specific formatting, retry policies, batched digests.
  - *Mitigation*: All of those are explicitly out of scope. Each becomes a follow-up PRD if usage proves the need. The "agent uses its own tools" model already pushes most of the policy questions to the user's MCP/CLI config, which is the right place for them.

- **Risk**: Closing #99 may surprise watchers who expected that design to land.
  - *Mitigation*: The close comment links to this PRD and explains the architectural inversion. The desktop-channel idea survives — just as the *only* channel, not the first of N.

## Dependencies

- Existing prompt-injection plumbing (already in the deck for orchestration role-prompt injection). M1.2 confirms it's reusable for inactivity-fired prompts.
- Existing PTY-output observation (already in the deck for status indicators and bell). M1.1 confirms it's reusable for the inactivity timer.
- `notify-rust` crate **or** shell-out to `osascript` / `notify-send`. M1.1 decides.
- No external services. No new third-party credentials.

## Validation Strategy

- **Unit**: inactivity timer (resets on PTY output, fires on expiry, suspension prevents re-fire), prompt-injection content (correct template substitution), dedup rule (deck-fired notification suspends timer for the right target).
- **Integration**: spawn a fixture agent with the hint, simulate quiet PTY for N seconds, assert injection fires with expected content; simulate agent crash, assert desktop notification fires and timer is suspended; simulate scheduler-side mkdir failure, assert desktop notification fires.
- **Manual** (per `feedback_validate_pre_pr`):
  - Configure Slack MCP in a real agent, run a real orchestration, verify the agent posts to Slack at completion.
  - Set `notify_when_inactive_after = "1m"` against a quiet agent, walk away, verify a Slack post arrives ~1 minute after silence.
  - Trigger a scheduler-side failure (e.g. typo'd working_dir), verify a desktop notification appears.
- **Regression**: existing terminal-bell behavior (PRD #8) unchanged; existing orchestration / dashboard / status-indicator behavior unchanged. The new notification module is additive — it should not change the shape of any existing test.

## CLAUDE.md Compliance

- `cargo fmt --check` and `cargo clippy -- -D warnings` before every commit (project rule #2).
- No `m*_*` or `prd*_*` prefixes in source/test filenames (project rule #3). Use semantic names: `src/notifications.rs`, `tests/inactivity_timer.rs`, `tests/desktop_notification.rs`.
- Ask before creating branches or worktrees (project rule #1). `/prd-start` will prompt the user accordingly.
