# PRD #99: Orchestrator Completion Notifications (Pluggable Channels)

**Status**: Planning
**Priority**: Medium
**Created**: 2026-05-21
**GitHub Issue**: [#99](https://github.com/vfarcic/dot-agent-deck/issues/99)
**Related**: PRD #8 (terminal bell — local, per-session), PRD #58 / #82 (multi-role orchestration), PRD #78 (tab-level status indicators)

## Problem Statement

Orchestrator runs in `dot-agent-deck` can be long — a single orchestration may delegate work to several roles, each running for minutes to hours, before the orchestrator finalizes. During that time the user is typically not staring at the TUI. The deck has a terminal bell (PRD #8) that fires on per-session state transitions, but it has two limitations that make it the wrong tool for "orchestrator done":

1. **It is local to the terminal.** The bell only reaches a user whose terminal window is focused (or whose terminal is configured to bounce the dock / flash the taskbar). A user on a different machine, in another tab, or away from the keyboard gets nothing.
2. **It fires per-session, not per-orchestration.** When an orchestrator delegates to three roles, the user gets bell events for each role's idle/waiting transitions. There is no single "the whole orchestration is finished" signal — the user has to infer it.

The result: users start a long orchestrator run, walk away, and either come back too early (and waste a context switch) or too late (and stall the next iteration). There is no out-of-band signal that closes the loop.

## Solution Overview

Introduce a **notification layer** with three pieces:

1. **A completion event source** in the orchestrator. The orchestrator already knows when a run starts and when it ends (PRD #58 establishes the lifecycle). This PRD adds an explicit "orchestrator run completed" event with metadata: which orchestration, success/failure, duration, summary text.
2. **A pluggable `Notifier` trait** that takes a completion event and dispatches it. The trait surface is small: `fn notify(&self, event: &CompletionEvent) -> Result<()>`. Each channel (desktop, email, Slack, webhook, ...) is one implementation.
3. **Per-user config** in `config.toml` that selects which channels are enabled and which events trigger them. Opt-in by default — no channel fires until the user configures it.

The PRD ships **one channel first** to validate the layer end-to-end (recommended: desktop notification via the existing OS notification command, since it has no external dependencies and works for the common single-user-at-a-laptop case). Additional channels (webhook, Slack, email) are subsequent milestones — each is a self-contained `Notifier` implementation plus a config block, and can land independently.

## Scope

### In Scope

- **`CompletionEvent` type** with fields: orchestration ID, started-at, ended-at, outcome (`Success | Failure { reason }`), role count, optional summary text.
- **Emission point** in the orchestrator: a single hook that fires exactly once per orchestrator run, on terminal state (success, failure, user-cancelled).
- **`Notifier` trait** and a dispatcher that fans the event out to all enabled notifiers. Errors from one notifier do not block the others; they are logged.
- **Desktop notification channel** as the first concrete implementation. Uses `notify-rust` or a shelled-out OS command (`osascript` / `notify-send`) — final choice during M1.
- **`config.toml` block** under `[notifications]` with `enabled = false` default and per-channel subtables. Reuses the existing config loading path.
- **Tests**: unit tests for the dispatcher and the event-emission predicate; an integration test with a mock notifier verifying it fires exactly once per run.
- **Docs**: a `docs/notifications.md` (or extension to an existing config doc) covering how to enable each channel and what the config looks like.

### Out of Scope (this PRD)

- **Per-role completion notifications.** This PRD is about the orchestrator as a whole. Per-role bells are already covered by PRD #8 in-terminal; per-role external notifications can be a follow-up if there is demand.
- **Notifications for non-orchestrator events** (single-agent runs, hook events, status transitions). The trait is reusable, but wiring other event sources is future work.
- **Bidirectional channels** (e.g. a Slack bot that can be replied to). One-way notification only.
- **Per-orchestration channel routing** (e.g. "this run should notify Slack but not email"). Config is global; per-run overrides can come later.
- **Cryptographic signing of webhooks** or other production-grade webhook hardening. We will document the basic shape; hardening is follow-up.

## Success Criteria

- A user with `[notifications.desktop] enabled = true` in `config.toml` receives an OS desktop notification within ~1 second of an orchestrator run reaching a terminal state.
- The notification fires **exactly once** per orchestrator run, regardless of how many roles the orchestrator delegated to or how many state transitions occurred internally.
- Disabling notifications (`[notifications] enabled = false`, or the channel-specific toggle) suppresses the notification with zero side effects on the orchestrator path.
- A failing notifier (e.g. Slack webhook returns 500) does not crash the deck, does not block the orchestrator, and surfaces the error in the daemon log.
- Adding a new channel requires adding one file (the `Notifier` impl) and one config sub-block — no changes to the orchestrator or dispatcher.
- `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` pass. `cargo test` passes including the new tests.

## Open Questions (resolve during M1)

1. **Which crate / mechanism for desktop notifications?** `notify-rust` is cross-platform but adds a dep; shelling out to `osascript` / `notify-send` is dep-free but platform-specific. Default recommendation: try `notify-rust` first; fall back to shell-out if it pulls in heavy transitive deps.
2. **Where does the emission point live?** Probably in the orchestrator coordinator that already tracks role lifecycles (PRD #58). M1.1 includes locating the exact site.
3. **Is "summary text" something the orchestrator can produce, or does it come from the user's prompt?** For v1, a generic "Orchestration <id> completed (<duration>)" is sufficient. Richer summaries can use the existing role-done content but are not required.
4. **Order of channel rollout after desktop**: webhook (most flexible, easiest to test) → Slack (high user value but needs webhook URL story) → email (most setup overhead).

## Milestones

### Phase 1: Core layer + desktop channel

- [ ] **M1.1** — Define `CompletionEvent` and locate the emission point in the orchestrator (PRD #58 / #82 coordinator). Confirm the event fires exactly once per terminal state.
- [ ] **M1.2** — Implement the `Notifier` trait and a dispatcher that loads enabled notifiers from config and fans events out.
- [ ] **M1.3** — Implement the desktop-notification `Notifier`. Decide between `notify-rust` and shell-out based on dep weight. Wire it to the dispatcher.
- [ ] **M1.4** — Add the `[notifications]` block to `config.toml` schema with `enabled = false` default and a `[notifications.desktop]` subtable.

### Phase 2: Additional channels

- [ ] **M2.1** — Webhook channel: POSTs the `CompletionEvent` as JSON to a user-configured URL. Config: URL, optional bearer token header.
- [ ] **M2.2** — Slack channel: sends a formatted message via incoming webhook URL. Config: webhook URL, optional channel override.
- [ ] **M2.3** — Email channel: sends via SMTP. Config: SMTP host, port, credentials, from/to addresses. (Lowest priority — may defer if demand is low.)

### Phase 3: Tests and validation

- [ ] **M3.1** — Unit tests for the dispatcher: empty-config no-op, single-channel fan-out, multi-channel fan-out, failing-channel does-not-block-others.
- [ ] **M3.2** — Integration test with a mock `Notifier` that asserts exactly-once delivery per orchestrator run.
- [ ] **M3.3** — Manual validation: run a real orchestration with `[notifications.desktop] enabled = true` and confirm the desktop notification appears at the expected moment.

### Phase 4: Docs and release

- [ ] **M4.1** — Write `docs/notifications.md` (or extend an existing config doc) with per-channel setup instructions and config examples.
- [ ] **M4.2** — Note in `docs/getting-started.mdx` that orchestrator completion notifications exist and link to the new doc.
- [ ] **M4.3** — Changelog fragment via `dot-ai-changelog-fragment`. Frame as "you can now get notified when long orchestrator runs finish".
- [ ] **M4.4** — PR, review, audit, merge, release.

## Key Files

- `src/orchestrator/` (per PRD #58 / #82) — emission point for the completion event.
- `src/notifications/mod.rs` — new module: `CompletionEvent`, `Notifier` trait, dispatcher.
- `src/notifications/desktop.rs` (M1.3), `src/notifications/webhook.rs` (M2.1), `src/notifications/slack.rs` (M2.2), `src/notifications/email.rs` (M2.3) — one file per channel.
- `src/config.rs` — extend with `[notifications]` block.
- `tests/notifications.rs` — dispatcher and exactly-once tests.
- `docs/notifications.md` — new user-facing doc.

## Risks and Mitigations

- **Risk**: Cross-platform desktop notifications are fiddly (macOS Notification Center permissions, Linux daemon variations).
  - *Mitigation*: Start with `notify-rust` (handles platform branching); document fallback if a user's OS blocks notifications.
- **Risk**: A misconfigured webhook URL or unreachable SMTP server stalls the orchestrator shutdown.
  - *Mitigation*: Dispatcher runs notifiers on a separate task with a short timeout; errors are logged but never propagate.
- **Risk**: Scope creep — users will ask for per-role notifications, per-event channel routing, two-way Slack, etc.
  - *Mitigation*: This PRD is explicit about the v1 surface (single completion event, global channel selection). Follow-ups get their own PRDs.
- **Risk**: Notification spam if "completion" is defined too loosely (e.g. firing on every role's "done").
  - *Mitigation*: M1.1 nails down the predicate: exactly one event per orchestrator run, on terminal state only.
