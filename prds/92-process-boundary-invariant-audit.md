# PRD #92: Pre-daemon parity audit + remediation

**Status**: Planning
**Priority**: Medium
**Created**: 2026-05-17
**Last updated**: 2026-05-23
**GitHub Issue**: [#92](https://github.com/vfarcic/dot-agent-deck/issues/92)
**Depends on**: PRD #76 (shipped, `prds/done/76-remote-agent-environments.md`) and PRD #93 Phases 1–3 (shipped — commits `48b9180`, `3d2b2db`). Baseline for the audit is commit `2fc39c3` — the last commit before PRD #76 merged.

## Problem Statement

The deck went through two back-to-back architectural pivots:

- **PRD #76** introduced the daemon as a separate process and remote environments as a first-class concept. The in-process arm stayed for local use, with the daemon-attach arm added alongside it for remote.
- **PRD #93** deleted the in-process arm. The daemon is now always external, lazy-spawned per user; every `dot-agent-deck` invocation attaches to it.

Both pivots were tested by re-implementing the architecture — confirming the new path works, fixing bugs that surfaced under it, then moving on. Neither pivot was tested by enumerating the pre-pivot user-visible features and confirming each one survived intact. Bugs that did surface (PRD #76 M2.11–M2.20; PRD #93 round 5+ in its implementation notes) were caught reactively — a user hit the regression on a real session and reported it.

There is no reason to believe every regression has been caught. The features that were used during the pivots got covered; features the maintainers happen not to use day-to-day could be silently broken or silently changed. The first known example, carried over from earlier audit attempts, is the **force-shutdown gap**: at the baseline you quit the deck and the agents died with it; in current main the daemon persists across deck exits and there is no in-product command to stop it (`pkill` is the only option). PRD #93 line 39 explicitly anticipated needing one but never shipped the command. Neither `DaemonCmd::Stop` nor `RemoteCmd::Stop` exists in current code; `remote remove` only deregisters a local entry.

The audit's job is to enumerate the rest of these. Baseline-versus-current. Each user-visible feature that existed at `2fc39c3` — is it still there, is it still doing the same thing, or did it quietly change shape? Anything missing, different, or silently regressed gets flagged. Anything that changed deliberately gets documented so future re-audits do not re-litigate it.

## Solution Overview

A parity audit between the pre-daemon baseline (`2fc39c3`) and current main. Read baseline code, docs, and tests at that commit. Enumerate user-visible features and behaviors. For each, locate the current implementation and compare against the baseline. Triage into one of three buckets:

- **Preserved** — feature works identically in current code. Evidence required: current code path plus at least one test that exercises it.
- **Regressed** — feature is missing, incomplete, or behaves differently than baseline. Drafted as a follow-up milestone in the audit document; not filed as a GitHub issue until the user reviews and authorizes.
- **Intentional change** — feature changed, but the change was a deliberate design decision. Cite the PRD or commit that justifies it so a future re-audit does not re-flag it.

Output goes to `audit/pre-daemon-parity-audit.md` (new file).

The audit is *baseline-versus-current parity*, not a forward-looking review of current code. Current-code-only issues — bugs that have no baseline equivalent — are out of scope.

**Scope expansion (2026-05-22).** The audit has now landed (`audit/pre-daemon-parity-audit.md`) and surfaced exactly three actionable findings: F1 (force-shutdown gap), F2 (`y` / `n` permission key never implemented), F3 (stale `/tmp` socket-path doc). All three are small, discrete, and clearly scoped. Splitting each into its own PRD would cost more in process overhead — issue files, milestone numbering, separate review threads — than it gains in scope clarity. This PRD therefore expands to include the remediation: F1, F2, and F3 are implemented on the same branch the audit shipped on, before the PRD closes. Trade-off acknowledged: this PRD now mixes audit and fix work, which the prior 2026-05-17 "Audit, not refactor" decision tried to avoid. The trade is accepted because the audit deliverable is already committed and reviewed, so reviewer/auditor coverage of the audit itself is no longer at risk from interleaving with fix work. See the 2026-05-22 scope-expansion design decision below for the full rationale.

## Scope

### In Scope

- **Every user-visible feature or behavior that shipped at `2fc39c3`**. Read baseline `src/`, `tests/`, `docs/`, and any closed PRDs in `prds/done/` whose work landed before `2fc39c3`. Build the feature list from baseline, not from current main.
- **For each feature, locate the current implementation** and compare against baseline:
  - Same UX (commands, flags, output shapes, dialog prompts)?
  - Same lifecycle (when it starts, when it stops, what survives a restart)?
  - Same edge-case handling (failure modes, error messages, validation)?
- **Triage** every row into Preserved / Regressed / Intentional change.
- **Worked example — force-shutdown gap**: pre-daemon, quitting the deck killed every agent. Post-daemon the daemon persists across exits and there is no in-product stop command. PRD #93 line 39 anticipated one (`"dot-agent-deck remote stop (or equivalent local command) to force shutdown"`); neither form shipped. The audit must include this as a Regressed row anchored to PRD #93 line 39.
- **Historical anchors**: M2.11 / M2.12 / M2.13 / M2.17 / M2.19 / M2.20 from PRD #76, plus the round 5+ rounds in PRD #93's implementation notes. Each was a regression caught reactively; confirm the corresponding feature is now Preserved in current main, and use the anchor as a spot-check on the methodology (if the audit's pre-existing list does not catch one of these, the methodology is too narrow).
- **Follow-up milestones for every Regressed row**, drafted in the audit document under a "Follow-up milestones to file" section. The user reviews drafts before any GitHub issue is filed.
- **Implement F1 — force-shutdown command for the daemon.** Pre-daemon the user could quit the deck and every agent died with it; post-daemon the daemon persists and no in-product gesture stops it. Implement an in-product command that restores an equivalent user gesture. **Design pending — see Design Decisions for current open questions.**
- **Implement F2 — `y` / `n` permission key.** The TUI help overlay (in both baseline and current code) documents `y` / `n` as "Approve / deny permission" but no handler exists. Implement the handler so the documented contract holds; ties back to PRD #18 (permission prompt control).
- **Implement F3 — fix stale socket-path doc.** `docs/configuration.md:22` still documents `/tmp/dot-agent-deck.sock` while current code uses `/tmp/dot-agent-deck-{uid}.sock`. Update the literal and add a one-sentence per-user-disambiguation note; cross-check `docs/installation.md` and `docs/remote-requirements.md` for the same staleness.
- **Implement F4 — Ctrl+W respects `close_pane` errors.** Auditor-found regression introduced when PRD #76 turned agent kills into RPCs that can fail. The TUI currently does `let _ = pane.close_pane(...)` and unconditionally removes the dashboard card / session — so a failed `StopAgent` RPC leaves the agent alive in the daemon registry while the card vanishes from the dashboard. Fix the single-pane path (`src/ui.rs`) to inspect the `Result` and preserve the card on `Err`; fix the group-close paths (`TabManager::close_tab` in `src/tab.rs`, `ModeManager::deactivate_mode` in `src/mode_manager.rs`) to return per-pane results so partial failure does not silently destroy the whole tab.
- **Implement F5 — process-group kill semantics.** Auditor-found defect (possibly pre-existing — baseline's `pane.child.kill()` had the same single-PID limitation, but the user-visible symptom is now sharper because the daemon outlives the TUI). Commands launched via `$SHELL -c <cmd>` (the spawn path in `src/agent_pty.rs`) register the shell's PID, not the actual agent's; on shutdown, only the shell dies and the agent + its descendants are orphaned to init. Fix the spawn path to `setpgid(0, 0)` (or `setsid`) the child into its own process group, and fix the kill paths (`force_kill_child_and_wait` + `shutdown_all_graceful`) to `killpg` instead of `kill`.
- **Implement F9 — restore worker context cleanup per `.dot-agent-deck.toml` `clear` setting.** Pre-daemon, the orchestrator cleared a worker's pane before each delegation when the role's `clear` field was true (the default; the `release` role explicitly opts out with `clear = false`). Post-daemon (current main), this is lost — workers retain previous pane content across delegations, which is visually messy and can be confusing for the orchestrator reading worker output. Likely lost in PRD #93 round 5 (commit `d39930f`) when delegate / work-done dispatch moved into the daemon: either the `clear` field was dropped from `OrchestrationConfig.roles[*]` parsing, or its honoring code in the delegate dispatch path was removed. Fix the parse, fix the honoring, restore pre-baseline behavior.
- **F10 — dedupe double work-done notifications (DEFERRED 2026-05-24).** Pre-daemon, a worker's `work-done` signal produced exactly one orchestrator notification. Post-daemon, the orchestrator was observed receiving the same "Worker X completed" notification twice for a single commit (visible across F2 / F5 / F8 work-done messages while reviewer/auditor workers ran on `opencode --model openrouter/openai/gpt-5.5`). After the 2026-05-23 reviewer/auditor migration to `claude --model opus`, duplicates have not recurred. Deferred pending reproduction. See the 2026-05-24 Design Decision below.

### Out of Scope

- **Hypothetical bugs in current code that have no baseline equivalent.** The v1 audit attempt drifted into this and surfaced findings (notably one about remote-network-attach assumptions) against an architecture this codebase does not have — TUI and daemon are always co-located, see `docs/remote-environments.md:8` and `:52–67`. Parity only.
- **Performance, security, or any other axis.** Behavioral parity only.
- **Pre-PRD-#76 bugs that the daemon transition incidentally fixed.** Those are improvements, not regressions.
- **Features that genuinely did not exist at baseline** (the `remote add/list/remove/upgrade` family, daemon idle-shutdown, daemon log destination, lazy-spawn semantics, attach protocol Hello handshake, KIND_EVENT plumbing, etc.). These are post-baseline additions, not parity concerns. List them in an appendix to the audit doc so a future re-audit knows what was deliberately added.
- **Fixes for any finding *beyond* F1 / F2 / F3 / F4 / F5 / F8 / F9 / F10.** The original audit produced three actionable rows; F4 and F5 were surfaced by the audit-of-the-audit (see Design Decisions, 2026-05-22 "Fold F4 + F5 into scope"); F8 was surfaced by F5's manual-testing pass against Claude Code (see Design Decisions, 2026-05-23 "Fold F8 into scope"); F9 and F10 were surfaced during F8 manual testing as two more daemon-rewrite regressions (see Design Decisions, 2026-05-23 "Fold F9 + F10 into scope"). Any *further* regressions surfaced by a re-audit are out of scope for this PRD and would be filed as a successor PRD.

## Success Criteria

**Audit (Phases 1–3 — already shipped):**

- `audit/pre-daemon-parity-audit.md` exists.
- Every user-visible feature present at `2fc39c3` has a row in the document with a triage column (Preserved / Regressed / Intentional change), a one-sentence rationale, and an evidence pointer (file:line in current code plus a baseline reference where useful).
- The force-shutdown gap appears as a Regressed row anchored to PRD #93 line 39.
- Every Regressed row has a corresponding 2–3 sentence follow-up milestone draft in the deliverable's "Follow-up milestones to file" section.
- The audit document opens with a coverage statement: which baseline feature categories were checked, which were deferred and why. A future re-audit can extend the statement rather than redo the work.
- No numeric floor on findings. Count is not the goal; honest coverage is.

**F1 fix — Stop option in the Ctrl+C dialog:**

- The Ctrl+d → Ctrl+C confirmation dialog (currently *Detach / Cancel*) gains a third option **Stop**, in the order *Detach / Stop / Cancel* (Detach remains the default).
- Selecting Stop with `managed_agents_count == 0`: proceeds to shutdown immediately, no secondary prompt.
- Selecting Stop with `managed_agents_count > 0`: shows a secondary `y / n` confirmation dialog. The dialog text names the count ("{N} managed agent(s) will be terminated and the daemon will shut down. Continue?"). Defaults to **No**. Pressing `y` or Enter on Yes confirms; pressing `n`, Esc, or Enter on No returns to the primary dialog.
- On confirmed Stop, the TUI sends a `KIND_SHUTDOWN` attach-protocol message; the daemon stops accepting new clients, terminates every managed agent (SIGTERM, with a short grace before SIGKILL), and exits. The TUI's session state is saved per the normal exit path so `--continue` from the same cwd is not poisoned.
- Existing daemon-lifecycle behaviors are unchanged: the Detach option still detaches without killing agents; idle shutdown still fires only when `clients == 0 AND agents == 0`; persist-when-agents-alive still holds for the implicit-quit path. Stop is purely additive.
- **No CLI command this round.** `dot-agent-deck stop` and `dot-agent-deck remote stop <name>` are explicitly deferred to a successor PRD (filed as **F6** in the audit doc — renumbered from F4 when F4 / F5 took the next slots).

**F2 fix — `y` / `n` permission key:**

- Pressing `y` on a card whose session is in `WaitingForInput` approves the pending permission request.
- Pressing `n` on the same card denies it.
- The help overlay text (`src/ui.rs:5536` in current code) accurately describes the now-working behavior — nothing to change in the help text itself.
- Unit tests cover both the approve and deny key arms and the status gating.

**F3 fix — stale socket-path doc:**

- `docs/configuration.md:22` (plus any other stale references found in `docs/installation.md` and `docs/remote-requirements.md`) reflects the actual `/tmp/dot-agent-deck-{uid}.sock` path and includes a one-sentence note explaining the per-user disambiguation. The `$XDG_RUNTIME_DIR/dot-agent-deck.sock` default and the env-var override behavior are unchanged in the doc.

**F4 fix — Ctrl+W respects `close_pane` errors:**

- Pressing Ctrl+W on a single pane inspects the `Result` returned by `PaneController::close_pane`. On `Ok(())`, the card and session are removed exactly as before. On `Err`, the card / session / metadata are preserved (the controller has already restored the local pane state); the error is surfaced in `ui.status_message` so the user can retry.
- Group-close paths (a mode-tab agent's Ctrl+W tears down the agent pane plus its side panes; an orchestration tab tears down every role) return per-pane results. Successfully-closed panes are removed; failed ones keep their cards and surface their errors via `ui.status_message`. The TUI does not silently drop cards while their underlying agents are still alive in the daemon registry.
- Unit tests cover the single-pane Ok / Err paths and the group-close partial-failure case for both `TabManager::close_tab` and `ModeManager::deactivate_mode`.

**F5 fix — process-group kill semantics:**

- Each spawned agent runs in its own POSIX process group (set via `setpgid(0, 0)` in a `pre_exec` hook so the child becomes the group leader). The daemon records the process-group id alongside the child PID.
- Every shutdown path that previously called `kill(pid, SIGKILL)` now calls `killpg(pgid, SIGKILL)` (or the SIGTERM-then-SIGKILL escalation for the graceful path) so shell-wrapped commands and the descendants they spawn are reaped together.
- A new test launches a shell-wrapped agent (`sh -c 'sleep 30 & wait'` or equivalent), captures the descendant PID, calls `StopAgent`, and asserts both the shell PID and the descendant PID are dead within ~2 seconds.

**F8 fix — graceful single-pane close (SIGTERM-then-SIGKILL on Ctrl+W):**

- Single-pane Ctrl+W now sends `SIGTERM` to the agent's process group first, waits up to 3 seconds for the child to exit, and only then escalates to `SIGKILL`. Pre-F8 it went straight to `SIGKILL`, leaving even well-behaved agents no opportunity to run their own cleanup hooks.
- Daemon-shutdown (Ctrl+C → Stop) already had the graceful pattern via `shutdown_all_graceful` (PRD #92 F1); F8 brings the single-pane path to parity.
- A well-behaved agent that traps `SIGTERM` and writes a sentinel before exiting sees its sentinel land on disk after Ctrl+W. An uncooperative agent (`sh -c 'trap "" TERM; sleep 60'`) is still reaped, just after the 3-second grace window.

**F9 fix — restore worker context cleanup (respawn semantics):**

- The `clear` field on each `[[orchestrations.roles]]` entry in `.dot-agent-deck.toml` is parsed correctly into the in-memory orchestration-role representation. Default is `true` (matches baseline); the `release` role's explicit `clear = false` continues to opt out for its scrollback walkthrough.
- Before writing the new task prompt to a worker's pane on delegate, the orchestrator **respawns the role's agent** — terminating the existing child (`SIGTERM` with the F8 3-second grace, then `SIGKILL` if needed) and spawning a fresh one running the role's `command` — if and only if the role's effective `clear == true`. This matches the pre-baseline contract: the new task lands on a worker with empty in-memory CONTEXT, not just a visually-cleared screen. The pane_id_env is preserved across the respawn so the TUI's pane card stays put and `write_to_pane` routing still works against the same identity.
- Integration tests cover: `clear` field parse (default + explicit true + explicit false); a `clear = true` delegate replaces the child PID (with the old PID dead per `kill(pid, 0)` returning ESRCH) and the new agent receives the prompt; a `clear = false` delegate keeps the same PID across delegations; and a sanity check that pre-respawn scrollback does not leak into the new agent.

**F10 fix — dedupe double work-done notifications (DEFERRED 2026-05-24):**

- *Originally specified*: each `work-done` signal produces exactly one orchestrator notification; broadcast semantics preserved.
- *Status*: deferred pending reproduction. The duplication that motivated F10 has not recurred since the 2026-05-23 reviewer/auditor migration off `opencode --model openrouter/openai/gpt-5.5`. If duplicates recur, F10 reopens.

## Milestones

### Phase 1: Baseline enumeration — shipped

- [x] **M1.1** — Read baseline state at `2fc39c3`. Use `git show 2fc39c3:<path>` for individual files or check out a temporary worktree at the baseline. Cover baseline `src/`, baseline `tests/`, baseline `docs/`, and any closed PRDs in `prds/done/` that shipped before `2fc39c3`. Build a feature/behavior list. The list comes from baseline, not from current code.
- [x] **M1.2** — Map each historical anchor (M2.11, M2.12, M2.13, M2.17, M2.19, M2.20, plus PRD #93 implementation-notes rounds) onto one or more rows in the list. Confirm the methodology would have caught each anchor if it had not already been fixed.

### Phase 2: Current-state verification — shipped

- [x] **M2.1** — For each baseline feature, locate the current implementation in main and decide the triage bucket. Use `Explore` agents for breadth where the surface is wide (event delivery, daemon lifecycle, attach protocol, orchestration dispatch).
- [x] **M2.2** — For each Preserved candidate, require at least one current test that exercises the daemon path. If no test, demote to Regressed — untested parity is unverified parity. *(Refinement during the audit: the bar was relaxed to label-level — rows where no test exists are marked Preserved-but-untested rather than demoted to Regressed, since Regressed is reserved for actual behavioral mismatch with baseline. See the audit doc's coverage statement.)*
- [x] **M2.3** — For each Intentional change, record the PRD or commit that justifies the change (so future re-audits do not re-flag).

### Phase 3: Writeup and follow-up — shipped

- [x] **M3.1** — Finalize `audit/pre-daemon-parity-audit.md` with: coverage statement, findings table, worked example (force-shutdown gap), historical-anchor appendix, post-baseline-additions appendix.
- [x] **M3.2** — Draft a 2–3 sentence follow-up milestone for each Regressed finding. Do **not** file GitHub issues — the user reviews drafts before any filing.
- [x] **M3.3** — Brief writeup in the PR description summarizing counts per triage bucket.

### Phase 4: Implement F3 — doc fix

- [ ] **M4.1** — Update `docs/configuration.md:22` (`/tmp/dot-agent-deck.sock` → `/tmp/dot-agent-deck-{uid}.sock`) and add a one-sentence per-user-disambiguation note. Cross-check `docs/installation.md` and `docs/remote-requirements.md` for the same staleness; mirror the fix wherever the old path appears.
- [ ] **M4.2** — Spot-check the rendered docs pages (if there is a docs build pipeline in the repo) so the change reads cleanly and the surrounding env-var table still scans correctly.

### Phase 5: Implement F2 — y / n permission key — shipped

- [x] **M5.1** — Add `KeyCode::Char('y')` and `KeyCode::Char('n')` arms in `handle_normal_key` (`src/ui.rs`), gated on the selected card's status being `WaitingForInput`. Both keys must no-op for any other status so the existing Ctrl+n new-pane handler and ordinary text typing are unaffected. *Shipped: `handle_normal_key` now takes a `selected_status: Option<SessionStatus>` and returns `KeyResult::SendPermissionResponse(bool)` only when the gate matches. The `Ctrl+n` arm in the outer dispatch loop is untouched (different modifier level, no conflict).*
- [x] **M5.2** — Wire the approve / deny path to whatever PRD #18's permission-prompt infrastructure expects. If PRD #18's machinery does not currently expose a clean approve/deny entry point, decide whether to extend it or to defer the key arms behind a guard that no-ops until the infrastructure lands. *Shipped: PRD #18's richer mechanism (`PermissionResponders` map, blocking-hook response channel, `pending_permission` on `SessionState`) is not present in current code — only the `EventType::PermissionRequest` mapping and the resulting `WaitingForInput` status survived through the PRD #76 / #93 pivots. Chose the simpler PTY-forward model per the F2 context: the dispatcher writes the literal `y` / `n` character to the selected pane's PTY via `pane.write_to_pane`, which already handles encode + `SUBMIT_DELAY` + CR. The agent's prompt is waiting on the PTY for the same input the user would type if they switched to the pane.*
- [x] **M5.3** — Add unit tests for both handlers: the approve / deny key arms, the `WaitingForInput` gating (no-op on other statuses), and (if the wiring in M5.2 is in place) the end-to-end approve/deny outcome on `SessionState`. *Shipped: 4 new unit tests in `src/ui.rs` — `permission_y_on_waiting_for_input_returns_approve`, `permission_n_on_waiting_for_input_returns_deny`, `permission_y_n_on_non_waiting_status_is_no_op` (covers Working / Idle / Thinking / Error / Compacting), `permission_y_n_with_no_card_selected_is_no_op` (covers `total == 0` and `selected_status == None` while a card exists). The PTY-write side effect itself is exercised by the existing `write_to_pane` test cluster.*

### Phase 6: Design + implement F1 — Stop option in Ctrl+C dialog — shipped

- [x] **M6.1** — Lock the F1 design. *Shipped: see the locked "F1 design" subsection in Design Decisions below. Stop is a third option in the existing Ctrl+C dialog (Detach default / Stop / Cancel). Secondary y/n confirmation only when agents are alive (defaults to No). `KIND_SHUTDOWN` is the wire signal; the daemon iterates the registry, SIGTERMs each agent with a short grace before SIGKILL, then exits. No CLI command this round — `dot-agent-deck stop` deferred to F6 (renumbered from F4 when the audit-of-the-audit surfaced F4 / F5 as in-scope for this PRD).*
- [x] **M6.2** — Implement Stop per the locked design. *Shipped: 3-option primary dialog (`src/ui.rs::handle_quit_confirm_key` + `render_quit_confirm`) with Detach default / Stop (1) / Cancel (2). Secondary y/n confirmation (`src/ui.rs::handle_stop_confirm_key` + `render_stop_confirm`) with `UiMode::StopConfirm`, agent count cached at transition. `KeyResult::StopAndQuit` calls `EmbeddedPaneController::shutdown_daemon` (`src/embedded_pane.rs`), which sends `KIND_SHUTDOWN` (`0x15`) via `DaemonClient::send_shutdown` (`src/daemon_client.rs`). Daemon-side `handle_connection` (`src/daemon_protocol.rs`) short-circuits `KIND_SHUTDOWN` before the usual `KIND_REQ` decoding, calls `AgentPtyRegistry::shutdown_all_graceful(Duration::from_secs(3))` (`src/agent_pty.rs`), then signals the daemon's shutdown `Notify` so `run_hook_loop` exits. The registry's Drop calls `shutdown_all` (SIGKILL) as the backstop for survivors. Idempotency: `AgentPtyRegistry::shutting_down: AtomicBool` latches on first entry.*
- [x] **M6.3** — Tests covering: primary dialog shows Detach / Stop / Cancel with Detach default; Stop with `agents_count == 0` skips the secondary dialog and triggers shutdown; Stop with `agents_count > 0` shows the secondary dialog with the agent count rendered in the text; secondary dialog defaults to No; Yes confirms (triggers shutdown), No returns to the primary dialog without shutting down; daemon receives `KIND_SHUTDOWN`, terminates managed agents, and exits within a bounded window; idempotency — two `KIND_SHUTDOWN` frames in quick succession do not crash the daemon; session save runs on the Stop path. *Shipped: 9 new unit tests in `src/ui.rs` (`quit_confirm_stop_with_no_agents_returns_stop_and_quit`, `::quit_confirm_stop_with_agents_prompts_secondary_dialog`, `::quit_confirm_down_clamps_to_three_options`, `::quit_confirm_cancel_returns_to_normal_mode` updated for new index, `::stop_confirm_defaults_to_no`, `::stop_confirm_yes_returns_stop_and_quit`, `::stop_confirm_y_shortcut_returns_stop_and_quit`, `::stop_confirm_no_returns_to_primary_dialog`, `::stop_confirm_down_clamps_to_two_options`) and 3 new integration tests in `tests/stop_dialog.rs` (`send_shutdown_returns_cleanly_with_no_agents`, `::send_shutdown_drains_registry_and_signals_notify`, `::double_shutdown_is_idempotent`). Session-save runs via the same `'outer` break the existing DetachAndQuit path uses; no new test required because the dispatcher branch shares the exit path verified by existing session-restore coverage.*
  *Followup (2026-05-23): reviewer flagged the original "socket close == ack" wire as a blocker — a daemon predating `PROTOCOL_VERSION = 2` would also close the connection on an unknown `KIND_SHUTDOWN` frame, so the TUI would interpret upgrade-mismatch silence as a successful shutdown. The followup adds:*
   - `KIND_SHUTDOWN_ACK = 0x16` (`src/daemon_protocol.rs`): header-only server → client frame. Daemon writes it **before** beginning teardown.
   - `PROTOCOL_VERSION` bumped from 1 to 2.
   - `DaemonClient::send_shutdown` waits up to 1s for `KIND_SHUTDOWN_ACK`; timeout / EOF / unexpected-frame all return `Err`. The TUI's `KeyResult::StopAndQuit` arm now stays in Normal mode on Err, surfaces the error via `ui.status_message`, and lets the user retry, Detach, or `pkill`.
   - Daemon rejects `KIND_SHUTDOWN` frames with a non-empty payload (auditor #1).
   - Daemon refuses `AttachRequest::StartAgent` when the registry's `shutting_down` latch is set (auditor #2). New public `AgentPtyRegistry::is_shutting_down()`.
   - `pid_to_pgid` helper in `src/agent_pty.rs` guards both `killpg` call sites against `pid == 0` and `pid > i32::MAX` (auditor #3). 4 unit tests for the boundary semantics.
   - `tests/stop_dialog.rs` gains 5 new tests: `send_shutdown_times_out_without_ack`, `send_shutdown_errors_on_eof_without_ack`, `send_shutdown_errors_on_unexpected_frame`, `kind_shutdown_with_payload_is_rejected_by_daemon`, `start_agent_during_shutdown_is_refused`.
   - `tests/process_group_kill.rs` gains `close_pane_does_not_signal_daemon_process_group` (auditor #5) that probes the daemon's own pgid before / after a close and asserts it survives — proving `killpg` targets the child's group, not the daemon's.

### Phase 8: Implement F4 — Ctrl+W respects `close_pane` errors — shipped

- [x] **M8.1** — Single-pane Ctrl+W in `src/ui.rs` inspects the `Result` returned by `pane.close_pane(pane_id)`. On `Ok(())`, current behavior: remove session, card, pane metadata. On `Err(e)`, keep the session / card / metadata, surface `e` via `ui.status_message`. The user can retry — the controller's `close_pane` has already restored the local pane state on the error path (verified by `tests/daemon_attach_cleanup.rs::ctrl_w_stop_agent_timeout_restores_pane_and_returns_error`). *Shipped: the `KeyCode::Char('w')` arm now matches on `pane.close_pane(pane_id)` and only removes the session on `Ok`. On `Err` it logs via `tracing::warn!` and sets `ui.status_message` to a retry-hinting message.*
- [x] **M8.2** — Group-close paths: `TabManager::close_tab` (`src/tab.rs`) and `ModeManager::deactivate_mode` (`src/mode_manager.rs`) change shape from "Vec of pane IDs to remove" to a per-pane result list. Successfully-closed panes get removed by the caller; failed panes stay in the registry (controller-side) and on the dashboard (TUI-side), with a status-message line listing the failures. Tab-close partial-failure does not destroy the tab entirely if at least one pane closed — but the now-empty side of the tab is best-effort cleaned up so the surviving failed pane retains its card. *Shipped: new `CloseTabOutcome` type in `src/pane.rs` with `closed: Vec<String>` and `failed: Vec<(String, String)>`; `TabManager::close_tab` returns `Result<CloseTabOutcome, TabError>` and `ModeManager::deactivate_mode` returns `Result<CloseTabOutcome, ModeManagerError>`. The Ctrl+W dispatcher builds a `HashSet<&str>` from `outcome.closed` and uses it to filter session removal — failed panes retain their sessions. Existing tests at the `TabManager` / `ModeManager` layer (`src/tab.rs`, `tests/mode_integration_test.rs`) updated to the new shape; tab-internal call sites updated.*
- [x] **M8.3** — Tests: UI-handler test (mock `close_pane` → `Err`) preserves the card / session and surfaces the error; UI-handler test (mock `close_pane` → `Ok`) removes them; orchestration group-close partial-failure preserves the failed pane and removes the rest; mode-deactivate partial-failure does the same. *Shipped: 4 new integration tests in `tests/close_pane_errors.rs` (`ok_close_pane_records_in_closed_with_no_failures`, `err_close_pane_records_in_failed_keeps_clean_panes_in_closed`, `err_on_mode_agent_pane_keeps_failing_id_in_failed`, `mode_deactivate_with_one_failing_pane_splits_outcome`). The Ctrl+W dispatcher itself is verified by code-read of the changed arm in `src/ui.rs` since the dispatcher is embedded in the main event loop and is not exposed as a unit-testable function; the dispatcher logic is a pure transformation of `outcome.closed` / `outcome.failed` into session removals / status messages, both of which are fully exercised by the integration tests at the `TabManager` / `ModeManager` layer.*

### Phase 9: Implement F5 — process-group kill semantics — shipped

- [x] **M9.1** — Spawn each agent in its own POSIX process group. *Shipped: no code change needed — `portable-pty` already calls `setsid()` in its `pre_exec` on Unix (see `portable-pty-0.8.1/src/unix.rs:220`). Every PTY-spawned child is therefore a session leader by construction, meaning the child's PID equals its session ID and process-group ID. The F5 design originally anticipated adding `setpgid(0, 0)` in a `pre_exec` hook on `src/agent_pty.rs`'s spawn path; that step is redundant given portable-pty's existing behavior. Documented inline in the F5 fix-target comment in `src/agent_pty.rs`.*
- [x] **M9.2** — Replace `kill(pid, SIGKILL)` with `killpg(pgid, SIGKILL)` in `force_kill_child_and_wait` (`src/agent_pty.rs`), and the SIGTERM-then-SIGKILL escalation in `shutdown_all_graceful` (`src/agent_pty.rs`) likewise targets the group. *Shipped: both `force_kill_child_and_wait` (SIGKILL final) and the SIGTERM phase inside `shutdown_all_graceful` now call `libc::killpg(pid as i32, sig)` instead of `libc::kill(pid as i32, sig)`. Used `libc::killpg` directly rather than `nix` because `libc` is already a direct dependency (`Cargo.toml:21`) and the call site is two lines; pulling `nix` in just for one wrapper would add a dependency cost the project doesn't otherwise need. The Drop path (`shutdown_all`) reuses `force_kill_child_and_wait`, so it benefits automatically.*
- [x] **M9.3** — Tests: launch an agent via a shell wrapper that backgrounds a child (`sh -c 'sleep 30 & wait'`), capture the descendant `sleep` PID, call `close_pane`, and assert both the shell PID and the descendant `sleep` PID are dead within ~2s. *Shipped: `tests/process_group_kill.rs::close_pane_reaps_shell_descendants`. The shell writes its background `sleep`'s PID to a tempdir relay file via `echo $! > FILE` so the test can capture it without relying on `/proc/<pid>/task/children` or `pgrep -P` (both Linux-only and brittle under CI). The test asserts both PIDs are alive before the close (so a descendant that died on its own can't make the test pass for the wrong reason) and both are dead within 3s after `close_pane`. Liveness probed via `libc::kill(pid, 0)` returning ESRCH. The 3s timeout is well under the descendant's 30s `sleep`, so a pre-F5 `kill(pid)` regression would leave the descendant alive past the bound and fail the assertion.*

### Phase 10: Implement F8 — graceful single-pane close — shipped

- [x] **M10.1** — Refactor `src/agent_pty.rs::force_kill_child_and_wait` (the single-pane Ctrl+W kill path) to do `killpg(pgid, SIGTERM)` first, wait up to 3 seconds for the child to exit (polling `try_wait`), and only then escalate to `killpg(pgid, SIGKILL)`. Mirror the pattern `shutdown_all_graceful` already uses for the all-agents-at-once path. Extract a shared helper so the two paths converge on the same grace-window logic rather than duplicating it. *Shipped: new `terminate_child_with_grace_and_wait` for single-pane Ctrl+W (used by `AgentPtyRegistry::close_agent`); `force_kill_child_and_wait` retained as SIGKILL-only for the contexts where a grace window is wrong or unnecessary (`PtyGuard` / `AgentPty` Drop, `shutdown_all` from registry Drop, `shutdown_all_graceful`'s phase-3 SIGKILL backstop). Both paths share `signal_child_pgroup_or_fallback` as the low-level killpg-or-`child.kill`-fallback helper, so the `pid_to_pgid` boundary check and `tracing::warn!` shape can't drift. New `AGENT_TERMINATE_GRACE: Duration` constant set to 3 s. Controller-side `CREATE_PANE_STOP_TIMEOUT` (2 s) split: Ctrl+W now uses a dedicated `CTRL_W_STOP_TIMEOUT` of 5 s (3 s grace + 2 s buffer) so the controller-layer RPC timeout doesn't trip before the daemon's graceful path completes; the original 2 s constant stays in place for the attach-failure cleanup it was originally written for.*
- [x] **M10.2** — Tests: (a) a well-behaved agent that traps `SIGTERM` and writes a sentinel before exiting — close the pane, assert the sentinel exists on disk so we know `SIGTERM` was delivered and the trap ran; (b) an uncooperative agent (`sh -c 'trap "" TERM; sleep 60'`) — close the pane, assert the shell process is dead within ~3.5 seconds (3-second SIGTERM grace + SIGKILL). *Shipped: `tests/process_group_kill.rs::close_pane_well_behaved_agent_runs_sigterm_trap` and `::close_pane_uncooperative_agent_killed_after_grace`. The well-behaved test traps SIGTERM, writes a sentinel, and exits 0; the test polls for the sentinel to appear within 2 s after the close. The uncooperative test installs an empty SIGTERM trap (`trap '' TERM`) so the shell ignores the signal, then asserts the close completes in under 3.5 s (3 s grace + SIGKILL delivery) and the shell PID is dead within another 500 ms. Existing `daemon_attach_cleanup::ctrl_w_stop_agent_timeout_restores_pane_and_returns_error` adjusted: outer deadline 4 s → 7 s to match the new `CTRL_W_STOP_TIMEOUT`.*
- [ ] **M10.3** — Manual verification (orchestrator drives with the user): close a real Claude Code pane via Ctrl+W and confirm the agent's own cleanup runs (any cleanup hooks the user has wired up); verify no regression in the existing Ctrl+W coverage (`tests/local_attach.rs::close_pane_stops_agent_in_daemon`, `tests/daemon_lifecycle.rs::close_pane_removes_agent_from_registry`). *Pending manual test pass per M7.1.*

### Phase 11: Implement F9 — restore worker context cleanup per `.dot-agent-deck.toml` clear setting

- [x] **M11.1** — Verify the `clear` field on `[[orchestrations.roles]]` is parsed into the in-memory orchestration-role representation (whatever shape the post-PRD-#93 daemon now uses — likely an `OrchestrationConfig.roles[*]` or equivalent). If parsing was silently dropped during the round-5 refactor at commit `d39930f`, restore it. Default to `true` so existing configs without a `clear` field continue to match baseline behavior. *Shipped: parsing was already intact — `OrchestrationRoleConfig.clear: bool` in `src/project_config.rs` with `#[serde(default = "default_clear")]` and `default_clear() -> true`. The regression was dispatch-side only (M11.2). The default + true + false round-trip is pinned by the existing `project_config::tests::orchestration_clear_defaults_to_true` together with `parse_full_orchestration_config` (which exercises both the explicit `false` value and the unspecified-default `true` value).*
- [x] **M11.2** — Wire the pane-clear into the daemon-side delegate dispatch path (the `handle_delegate` flow in `src/state.rs`, plus any equivalent in `src/agent_pty.rs`'s `write_to_pane` if cleaner there). Before writing the new task prompt to the worker's pane, clear the pane if the role's `clear` field is true. Skip the clear when `clear = false` (release role keeps its scrollback). *Shipped: implemented as faithful **respawn semantics** matching baseline `2fc39c3:src/ui.rs::dispatch_delegate_events`, not a lighter ANSI-clear shim. A new `AgentPtyRegistry::respawn_agent_for_pane(pane_id_env, command)` (`src/agent_pty.rs`) atomically lifts the existing agent out of the registry, terminates its child via the F8 helper `terminate_child_with_grace_and_wait` (SIGTERM with `AGENT_TERMINATE_GRACE` (3 s) grace, then SIGKILL backstop) on a `spawn_blocking` pool task, and spawns a fresh agent reusing the same `pane_id_env` + cwd + `display_name` + `tab_membership` + `agent_type` — so the TUI's pane card stays put and `write_to_pane` routing keeps working. `AppState::handle_delegate` (`src/state.rs`) calls this method when the resolved role config has `clear = true`, sleeps `RESPAWN_READY_DELAY` (250 ms) for the new agent's TUI initialization to settle, then writes the prompt as before. `clear = false` skips the respawn entirely.*
- [x] **M11.3** — Tests: parse-level test for default (no field → true) and explicit `clear = true` / `clear = false` values; dispatch-level test that the orchestrator emits a clear-screen sequence (or equivalent) to the worker's pane when `clear == true` and does not when `clear == false`. *Shipped: parse covered by the pre-existing `project_config::tests::parse_full_orchestration_config` (default + explicit `false`) and `orchestration_clear_defaults_to_true` (missing-field default). Dispatch covered by three new integration tests in `tests/orchestration_delegate.rs`: `delegate_respawns_worker_agent_when_role_clear_is_true` (two delegations to a `clear = true` role; asserts the old child PID is dead via `kill(pid, 0)` ESRCH, a new PID is spawned, the pane_id_env is stable across both rotations, and the new agent receives the prompt); `delegate_does_not_respawn_worker_when_role_clear_is_false` (two delegations to a `clear = false` role; asserts the registry agent id and child PID stay constant); and `delegate_respawn_clears_agent_scrollback_when_role_clear_is_true` (sanity: seeds the pre-respawn agent's scrollback with a marker, fires a `clear = true` delegate, asserts the new agent's scrollback does not contain the marker — proves context lifetime tracks the agent process, not the pane id).*

### Phase 12: Implement F10 — dedupe double work-done notifications — DEFERRED (2026-05-24)

Phase 12 milestones are deferred pending reproduction. See the 2026-05-24 Design Decision below. The original milestone spec is preserved verbatim so that if the bug recurs the work can resume without re-deriving the plan.

- [ ] **M12.1** — Identify the duplicate source. Likely candidates: the daemon's `handle_work_done` path (`src/state.rs`) firing the notification once directly while a broadcast subscriber elsewhere also fires it; or a hook-event subscriber firing on every `KIND_EVENT` carrying a `work-done` payload alongside the direct dispatch. Use `Explore` agents to map the work-done delivery graph if the surface is wide.
- [ ] **M12.2** — Fix the duplicate. Likely a single-emit point or a guard flag preventing double notification.
- [ ] **M12.3** — Tests: subscribe a fake orchestrator-side listener, fire one `work-done` signal, assert exactly one notification arrives. Add a regression test that the broadcast machinery still delivers `work-done` to all attached TUIs (so the dedup did not accidentally break broadcast).

### Phase 7: Pre-release

- [ ] **M7.1** — Manual test pass covering F1, F2, F3, F4, F5, F8, F9, F10 (orchestrator drives with the user). Confirm the quit dialog behaves as the M6.2 three-option Detach/Stop/Cancel; confirm idle shutdown still works for the no-agents case; confirm Stop terminates managed agents and exits the daemon; confirm `y` / `n` approve/deny works on a real `WaitingForInput` session; confirm the doc updates read cleanly on the rendered docs site; confirm Ctrl+W on an unhealthy agent surfaces an error instead of silently removing the card; confirm a shell-wrapped agent's descendants die when the pane is closed; confirm Ctrl+W on a well-behaved agent delivers SIGTERM (visible via the agent's own cleanup-hook output) before the 3-second grace closes with SIGKILL; confirm a worker's pane is cleared before each delegation when its role's `clear == true` (default) and is not cleared when `clear == false` (release role); confirm a single `work-done` signal produces exactly one orchestrator notification.
- [ ] **M7.2** — Changelog fragment via `dot-ai-changelog-fragment`. The user-visible headlines are the new F1 Stop dialog option, the `y` / `n` keybindings going live, the F4 close-pane error surfacing, the F5 descendant-process cleanup, the F8 SIGTERM grace on single-pane close, the F9 worker-context cleanup restored, the F10 work-done deduplication, and the doc fix; the audit deliverable itself is internal and does not need a changelog entry.
- [ ] **M7.3** — PR description includes (a) the audit findings summary (counts per bucket plus a pointer to `audit/pre-daemon-parity-audit.md`), (b) the F1 / F2 / F3 / F4 / F5 / F8 / F9 / F10 fix summary, (c) the manual-test-pass results from M7.1, and (d) links to any successor PRDs or follow-up issues if the audit surfaces additional work during implementation.

## Key Files

Baseline reading targets (at `2fc39c3` — use `git show 2fc39c3:<path>` or a temporary worktree):

- `src/main.rs` — baseline CLI surface, dashboard entry point.
- `src/embedded_pane.rs` — baseline pane I/O and lifecycle.
- `src/state.rs`, `src/ui.rs`, `src/tab.rs` — baseline `AppState` shape and TUI behaviors.
- `src/hook.rs` — baseline hook ingestion.
- `tests/` — baseline integration test coverage (every test that exists at baseline is a baseline-feature assertion worth checking against current main).
- `docs/` — baseline user-facing documentation, especially `getting-started.mdx` and `installation.md`.

Current-code read targets (for the parity check):

- `src/state.rs` — `AppState`, target-pane resolution, session lifecycle.
- `src/daemon.rs` — daemon startup and idle-shutdown.
- `src/daemon_protocol.rs` — attach protocol wire format.
- `src/daemon_client.rs` — TUI-side attach protocol client.
- `src/agent_pty.rs` — `AgentPtyRegistry`, daemon-side PTY ownership, `write_to_pane`.
- `src/pane_input.rs` — `encode_pane_payload`, `SUBMIT_DELAY`, bracketed-paste handling.
- `src/embedded_pane.rs` — `EmbeddedPaneController`, pane read/write paths.
- `src/main.rs` — auto-spawn, lock contention, CLI surface.
- `src/ui.rs`, `src/hook.rs` — TUI-side event consumers.
- `tests/rehydration.rs`, `tests/event_forwarding.rs`, `tests/daemon_integration.rs`, `tests/orchestration_delegate.rs`, `tests/local_attach.rs` — real-daemon integration coverage.

Audit deliverable:

- `audit/pre-daemon-parity-audit.md` — new file.

Fix targets (Phases 4–10):

- **F3** (doc fix): `docs/configuration.md` (line 22 plus surrounding env-var table), `docs/installation.md`, `docs/remote-requirements.md`.
- **F2** (`y` / `n` permission key): `src/ui.rs` (`handle_normal_key`, `WaitingForInput` gating, plus any approve/deny wiring); `src/state.rs` if approve / deny needs to mutate session state. Unit tests inline in `src/ui.rs` next to the existing `test_mode_transitions` cluster.
- **F1** (Stop option in Ctrl+C dialog): `src/ui.rs` (primary and secondary dialog rendering + key handlers + `KeyResult::Stop` variant), `src/daemon_protocol.rs` (`KIND_SHUTDOWN` frame), `src/daemon_client.rs` (TUI-side `send_shutdown` helper), `src/daemon.rs` (handler that triggers the existing shutdown path immediately on `KIND_SHUTDOWN`), `src/agent_pty.rs` (terminate-all path with SIGTERM→SIGKILL escalation). Tests under `tests/` — a new `tests/stop_dialog.rs` for the dialog flow plus `KIND_SHUTDOWN` round-trip tests added to `tests/daemon_protocol.rs` and `tests/daemon_lifecycle.rs`. No CLI command this round, so `src/main.rs` is untouched.
- **F4** (Ctrl+W error handling): `src/ui.rs` (Ctrl+W handler around line 3736 and the group-close fan-out), `src/tab.rs` (`TabManager::close_tab` signature widens to return per-pane results), `src/mode_manager.rs` (`ModeManager::deactivate_mode` similarly). Tests in a new `tests/close_pane_errors.rs` (or extending `tests/orchestration_delegate.rs`) using a mock `PaneController` that returns `Err` from `close_pane`.
- **F5** (process-group kill semantics): `src/agent_pty.rs` is the single edit site — spawn path uses `pre_exec` for `setpgid(0, 0)`; `force_kill_child_and_wait` and `shutdown_all_graceful` switch from `kill(pid, ...)` to `killpg(pgid, ...)`. Tests in a new `tests/process_group_kill.rs` or as an extension to `tests/local_attach.rs` — launch a shell-wrapped agent, capture the descendant PID, close the pane, assert both PIDs are dead.
- **F8** (graceful single-pane close): `src/agent_pty.rs` is again the single production edit site — `force_kill_child_and_wait` switches from "SIGKILL only" to "SIGTERM, poll `try_wait` up to 3 s, then SIGKILL," sharing the escalation helper with `shutdown_all_graceful`. Tests extend `tests/process_group_kill.rs` (well-behaved-trap-runs + uncooperative-SIGKILL-after-grace).
- **F9** (worker context cleanup): `.dot-agent-deck.toml` parsing — wherever the orchestration-role config is deserialized (likely `src/state.rs` or a dedicated config module in current code); plus the daemon-side delegate dispatch path (`src/state.rs::handle_delegate` and/or `src/agent_pty.rs::write_to_pane`). Tests for both the parse and the dispatch.
- **F10** (dedupe work-done notifications): the daemon's work-done delivery graph — `src/state.rs::handle_work_done`, the broadcast subscribers in `src/daemon.rs` / `src/daemon_protocol.rs`, and any hook-event handler that forwards `work-done` payloads. Tests assert exactly-once notification delivery and broadcast preservation.

## Design Decisions

### 2026-05-24: Defer F10 pending reproduction

F10 was folded into scope on 2026-05-23 on the basis of observed duplicate work-done notifications in this PRD's implementation conversation — specifically while reviewer / auditor workers were running on `opencode --model openrouter/openai/gpt-5.5`. On 2026-05-23 those workers were migrated to `claude --model opus` for unrelated reasons (per-call cost / GPT credit exhaustion). After the migration the duplication has not recurred across three observed completion cycles (F8-followup auditor re-run, F9-respawn coder, F9 reviewer + auditor passes — every completion produced exactly one notification).

Two reasonable hypotheses for the apparent fix:
- The duplication was specific to how the `opencode` binary signals `work-done` on exit (a binary-specific double-emit), and the migration to `claude` incidentally removed the trigger without touching the daemon-side notification path.
- The duplication was network-blip-induced retries from the OpenRouter-hosted GPT model, manifesting only in a model that's network-latency-sensitive.

Either hypothesis means the daemon's notification dedup logic is not necessarily broken — the duplication may have been agent-binary-specific. Implementing F10's dedup defensively without a reproducible bug risks (a) churn on a non-problem and (b) masking the underlying agent-binary issue if one exists.

Decision: defer F10's implementation. The orchestrator continues to count notifications per worker completion and surfaces any recurrence to the user; if duplicates reappear, F10 reopens with the original milestone spec intact. The PRD #92 scope shrinks from 8 fixes to 7 (F1, F1-followup, F2, F3, F4, F5, F8, F9 — F10 deferred).

User decision recorded as: *"Let's remove duplications from the list of tasks, but keep an eye on them and let me know if it happens again."* The task-list cleanup (deleting the F10 implementation task) and the PRD's deferral marker are paired — both reflect the same decision.

### 2026-05-23: Fold F9 + F10 into scope (rewrite regressions found during F8 manual testing)

During the F8 manual-test pass, the user surfaced two more daemon-rewrite regressions that the parity audit's row-by-row enumeration did not catch (the audit focused on user-visible features expressed through user-facing commands and UI; F9 and F10 are user-visible but expressed through orchestrator behavior, which the v2 audit pass did not extensively enumerate):

- **F9 — worker context cleanup.** Each `[[orchestrations.roles]]` entry in `.dot-agent-deck.toml` supports a `clear` field (default `true`; the `release` role explicitly sets `clear = false` to keep its scrollback for the release-flow walkthrough). Pre-daemon (baseline `2fc39c3`), the orchestrator honored this setting and cleared a worker's pane before each delegation when `clear == true`. Post-daemon (current main), this honoring is lost — workers retain previous pane content across delegations. Likely lost in the PRD #93 round-5 refactor (commit `d39930f`) when delegate / work-done dispatch moved into the daemon: either the field was silently dropped from the in-memory config representation, or its honoring code was removed during the dispatch rewrite.
- **F10 — double work-done notifications.** Pre-daemon, a worker's `work-done` signal produced exactly one orchestrator notification. Post-daemon, the orchestrator frequently receives the same "Worker X completed" notification twice for a single commit. This is reproducible — it manifested in this very PRD #92 implementation conversation across F2 / F5 / F8 work-done messages. Likely lost in the same PRD #93 round-5 refactor (`d39930f`) that moved work-done dispatch into the daemon. Either dedup logic was removed, or two parallel code paths now both notify.

Both are exactly the parity-class regressions PRD #92 was created to surface. User confirmed fold-in:

> *"Change @devbox.json..."* — testing reaction surfacing the issues
> *"We should add those two to the list of fixes and tackle them after we finish working on the one we're working on now."*

Same precedent as F4 / F5 (audit-of-the-audit findings) and F8 (manual-test finding). The "audit excludes fixes" line from 2026-05-17 is partially superseded again for F9 / F10 specifically. Bundling is cheaper than spinning up two more follow-up PRDs (same calculus as before — issue files, branch overhead, separate review threads).

Ordering: F9 and F10 land *after* F1 / F2 / F3 / F4 / F5 / F8 are all proven (i.e., after the M7.1 manual test pass validates the prior fixes). Strictly sequential within F9 / F10 — F9 first because the parse / dispatch surface is narrower; F10 second because the work-done delivery graph requires Explore-agent breadth.

F9 implementation note: when fixing the field's honoring, also confirm the default (no `clear` field present in TOML) resolves to `true`. The release role's `clear = false` must remain functional — verify by spot-checking the resulting `release` worker pane retains scrollback across delegations after F9 ships.

F10 implementation note: be careful not to break the broadcast semantics. `work-done` is delivered both to the orchestrator (as a notification) and broadcast to attached TUIs (for status display). Exactly-once is the orchestrator-notification contract; the broadcast can still fan out to all subscribers, but each subscriber receives the signal once.

### 2026-05-23: Fold F8 into scope (graceful single-pane close)

Manual testing of F5 against a real Claude Code agent surfaced a limitation in the F5 fix: `killpg` reaches every direct descendant of the agent's own session, but Claude Code internally `setsid`s its sub-shells, placing those sub-shells (and *their* descendants) in fresh process groups that escape the outer `killpg`. The orphaned sub-shells then survive Ctrl+W as init-parented zombies.

User's framing — which we accept — is: *"If we ensure that agents are killed, the agent is responsible for the processes it spawns."* That is the right boundary; the daemon shouldn't try to chase arbitrary descendant trees the agent itself created. But the pre-F8 Ctrl+W path used raw `SIGKILL` (uncatchable), so even a well-behaved agent that *wanted* to clean up its descendants couldn't — it received no signal it could trap.

F8 closes the gap by giving the single-pane Ctrl+W path the same SIGTERM-with-grace shape F1's `shutdown_all_graceful` already uses for daemon-wide Stop. Well-behaved agents now have a 3-second window during which a `SIGTERM` handler can run; misbehaving agents are still SIGKILL'd after the grace, with whatever descendants the agent left behind continuing to leak (and that's an agent bug, not a deck bug).

Choice points and trade-offs:

- **Shared helper vs. duplicated logic.** Shared. `force_kill_child_and_wait` (single-pane) and `shutdown_all_graceful` (daemon-wide) now both delegate to a small `terminate_with_grace` helper that takes the child + a grace duration and runs the SIGTERM-poll-SIGKILL sequence. Reduces drift risk.
- **3-second grace constant.** Hardcoded for now (`AGENT_TERMINATE_GRACE: Duration`). Matches the F1 graceful-shutdown grace, which is the natural sibling. Can be lifted to a `DashboardConfig` field if a future user genuinely needs to tune it — the constant is one symbol to find, and the production code reads it from a single named site.
- **Scope guarantee.** F8 covers Ctrl+W only. Daemon-shutdown Stop (Ctrl+C → Stop) was already graceful via the F1 pathway; F8 is the parity fix, not new behavior.

Acknowledged limitation that F8 does *not* fix: agents that internally `setsid` their sub-shells (Claude Code does this) still leak the sub-session if the agent itself doesn't reap them during the SIGTERM grace. That's the agent's responsibility now that it gets a catchable signal. If the agent ignores SIGTERM or doesn't clean up, the descendants survive as orphans — visible via `pgrep` after Ctrl+W. The deck's job is to deliver SIGTERM and wait; the agent's job is to use it.

### 2026-05-22: Fold F4 + F5 into scope (audit-of-the-audit findings)

While the F1 / F2 / F3 fixes were landing, the audit picked up its own audit pass and surfaced two additional issues that fit the same pattern of "daemon pivot left behind something the parity audit missed":

- **F4 — Ctrl+W respects `close_pane` errors.** Post-PRD-#76 regression: when agent kills became RPCs that can fail, the TUI's Ctrl+W handler did `let _ = pane.close_pane(...)` and unconditionally removed the dashboard card. A failed `StopAgent` RPC leaves the agent alive in the daemon registry while the card vanishes from the dashboard. Did not exist at baseline (where the kill was an in-process call that couldn't fail in the same way). Auditor classified as blocker.
- **F5 — process-group kill semantics.** Possibly pre-existing defect: commands launched via `$SHELL -c <cmd>` register the shell's PID; on shutdown only the shell is signalled and its descendants (the actual agent, language servers, file watchers) are orphaned to init. Baseline had the same shape (`pane.child.kill()` was a single-PID call), but the user-visible symptom is now sharper because the daemon outlives the TUI so orphaned descendants persist across deck restarts. Auditor classified as suggestion; user confirmed fold-in to fix the visible symptom ("agents still running after Ctrl+W").

User confirmed both fold-ins: "Yes" (F4) and "Confirm F5 fold-in" (F5). Ordering F1 → F4 → F5, strictly sequential.

The "audit excludes fixes" decision (2026-05-17) is now superseded for these two findings as well; same trade-off as F1 / F2 / F3 — bundling is cheaper than spinning up two more follow-up PRDs. The original "Audit, not refactor" principle still applies to *future* audits run after this PRD closes.

This also forces a renumbering: the original **F4 — Scripted shutdown via `dot-agent-deck stop`** (drafted in the audit's Follow-up section when F1 shipped) is renumbered to **F6** so the in-scope fixes occupy contiguous slots F1–F5. Cross-references in F1's locked design and in row 14 of the findings table are updated accordingly.

**F4 design choice — group-close partial-failure handling.** When Ctrl+W tears down a mode tab or orchestration tab, multiple `close_pane` calls fan out. The handler must choose between three behaviors when *some* of those calls fail: (a) abort the entire tab-close and surface the first error, (b) close all the successful ones and leave the failed ones present as orphan cards, or (c) close all the successful ones and tear down the now-empty side of the tab anyway. The orchestrator chose (b) — close successful panes, list failed ones in `ui.status_message`, keep their cards present for retry. User confirmed. (b) is preferred because (a) is too coarse — the user has to retry every healthy pane just to retry the unhealthy one — and (c) loses the failed cards' visibility, defeating the whole point of the fix.

**F5 design choice — `setpgid` vs `setsid`.** Both work; `setpgid(0, 0)` creates a new process group within the existing session (cleaner — does not detach from the controlling tty), and the resulting `pgid` equals the child's PID so the daemon doesn't need to record a separate field. `setsid` creates a new session with its own controlling tty, which is more isolation than F5 needs and would interact awkwardly with the PTY master/slave the agent already has. Going with `setpgid` per the F4/F5 context recommendation.

### 2026-05-22: Expand scope to include F1 / F2 / F3 fixes

The audit shipped (`audit/pre-daemon-parity-audit.md`) and produced exactly three actionable findings: F1 (force-shutdown gap), F2 (`y` / `n` permission key never implemented), F3 (stale `/tmp` socket-path doc). All three are small, discrete, and clearly scoped — F3 is a doc edit; F2 is two key handlers plus tests; F1 is a single new command. Splitting each into its own PRD costs more in process overhead — separate issue files, separate milestone numbering, separate review threads, separate changelog fragments — than it gains in scope clarity. Bundling them in this PRD's branch keeps the work momentum tight and the audit-to-fix trace direct.

This decision **partially supersedes** the 2026-05-17 "Audit, not refactor" entry. The "audit excludes fixes" guidance from that entry is replaced for F1 / F2 / F3 by this one. The rest of "Audit, not refactor" — that mixing audit and fix work obscures the audit's scope — still applies to any *future* findings: a re-audit run after these fixes ship would still draft its own follow-up milestones in its own deliverable rather than expand mid-stream.

Acknowledged trade-off: this PRD now mixes audit and fix work, which the prior decision warned against. The trade is accepted because the audit deliverable is already committed and reviewed — reviewer/auditor coverage of the audit itself is no longer at risk from interleaving with fix work. The cost is a thicker PRD; the win is one PR instead of four.

#### F1 design (locked 2026-05-22)

The seven open questions from earlier are now resolved. F1 ships as a **dialog option**, not a CLI command — the user gesture being restored is "one keystroke that takes everything down," and the existing Ctrl+d → Ctrl+C dialog is the right place to host it. Scripted shutdown (a CLI `stop` command and its `remote stop` cousin) is deferred to a successor PRD; see the F4 follow-up draft in the audit doc.

**User-facing UX.**

The primary dialog (the existing Ctrl+d → Ctrl+C confirmation) now has three options instead of two:

- Index 0: **Detach** (default — unchanged behavior).
- Index 1: **Stop** (new).
- Index 2: **Cancel** (unchanged).

Detach stays the default so the muscle memory built up around the existing dialog does not become destructive. Stop sits between Detach and Cancel so the destructive option requires a deliberate selection: a user who hammers Enter still gets Detach.

When Stop is selected, the behavior depends on the managed-agent count:

- **`managed_agents_count == 0`** — proceed to shutdown immediately. The "no agents to terminate" case is the no-stakes case; a secondary prompt would be friction without value.
- **`managed_agents_count > 0`** — show a secondary `y / n` confirmation dialog whose text names the count: *"{N} managed agent(s) will be terminated and the daemon will shut down. Continue?"*. The dialog defaults to **No** (safer default for a destructive action). Pressing `y` or Enter on Yes confirms. Pressing `n`, Esc, or Enter on No returns to the primary dialog (Stop selected, so the user can re-confirm or pick Detach/Cancel without re-opening the dialog).

The "return to primary" behavior on No is chosen over "dismiss" because it is more discoverable — a user who reads the count and decides not to proceed can immediately move to Detach without restarting the Ctrl+C sequence.

**Effect when Stop is confirmed** (whether via the primary dialog with 0 agents or via the secondary confirmation with >0):

1. TUI saves session state per the normal exit path. `--continue` from the same cwd must still work after a Stop.
2. TUI sends a `KIND_SHUTDOWN` attach-protocol frame to the daemon.
3. Daemon stops accepting new clients, broadcasts a "daemon stopping" `BroadcastMsg::Event` to all attached clients (so any second TUI disconnects cleanly), iterates the agent registry and terminates each PTY (SIGTERM, then SIGKILL after a short grace), then exits.
4. TUI waits a brief moment for the daemon's socket close to confirm shutdown — falls back to closing on a ~1s timeout — then exits cleanly with `ExitCode::SUCCESS`. SIGTERM to the daemon's PID via the per-user lock file is the last-resort fallback if the protocol frame cannot be delivered.

**Implementation choices.**

- **Wire signal**: new attach-protocol message kind `KIND_SHUTDOWN`, header-only frame (no payload). Mirrors the existing `KIND_DETACH` shape so the daemon's protocol dispatcher recognizes it via the same machinery. Ack is daemon-side socket close (the daemon exits as soon as the shutdown path finishes); the TUI does not require a positive ack frame.
- **Daemon behavior on `KIND_SHUTDOWN`**: trigger the existing idle-shutdown teardown immediately, bypassing the `clients == 0 && agents == 0` gate. The idle-shutdown path already knows how to cleanly stop the daemon; Stop is "skip the wait, do it now."
- **Agent termination**: SIGTERM first; SIGKILL after a 3-second grace if the child has not exited. The grace gives long-running tool invocations a chance to flush state. Coder discretion to tune the 3s if it surfaces problems during implementation.
- **Idempotency**: if the daemon is already in the shutdown path (e.g., a second `KIND_SHUTDOWN` arrives mid-tear-down, or idle-shutdown already started), the new signal is a no-op. The shutdown path checks a `shutting_down: AtomicBool` (or equivalent generation counter — coder's call) before re-entering.
- **No-daemon case**: not reachable from the dialog (the dialog only exists in an attached TUI), so no explicit handling needed. The TUI's existing detach-on-exit flow handles a missing daemon already.

**Scope boundaries.**

- **No CLI command** in this PRD. `dot-agent-deck stop` (and `dot-agent-deck remote stop <name>` for remote daemons) is anticipated by PRD #93 line 39 but deferred from F1's gesture-restoration scope. The audit's follow-up draft for scripted shutdown is filed as **F4** to be picked up once F1 is proven.
- **Remote-daemon Stop**: same deferral. The dialog Stop only affects the daemon the TUI is attached to. Stopping a remote daemon you have not attached to is the deferred CLI-command's job.
- **No `--force` semantics**: there is no "always force without prompting" surface this round. The secondary y/n dialog is the only friction, and it suffices because the dialog only appears when there is actually something destructive to confirm.

**Open questions explicitly resolved.**

1. ~~Command name and surface~~ → no CLI command this round; dialog option only.
2. ~~Force-shutdown semantics with managed agents alive~~ → user confirms via secondary dialog; on confirm, agents are SIGTERM'd (then SIGKILL after a short grace) and the daemon exits.
3. ~~Confirmation prompt~~ → primary dialog is the confirmation when no agents; secondary y/n dialog when agents alive. No `--force` flag.
4. ~~Multi-agent handling under force~~ → daemon iterates the registry and terminates each PTY in order.
5. ~~Local vs remote scope~~ → local only this round; remote-stop deferred to F6 (renumbered from F4 when the audit-of-the-audit surfaced F4 / F5 as in-scope for this PRD).
6. ~~Wire-level signal~~ → new `KIND_SHUTDOWN` protocol frame; SIGTERM to the daemon PID as last-resort fallback if the frame cannot be delivered.
7. ~~Idempotency / missing-daemon cases~~ → second signal is a no-op (guarded by a `shutting_down` flag in the daemon); no-daemon case is unreachable from the dialog.

### 2026-05-22: Parity audit framing

The audit's axis is baseline-versus-current behavioral parity, not a forward-looking review of current code. The deck shipped two architectural pivots back-to-back (PRD #76 daemon-as-separate-process, PRD #93 daemon-as-only-process) and each was tested by re-implementing the architecture, not by enumerating pre-pivot features and confirming they survived. The "what changed silently" question is the right one to ask of the resulting code, and the answer requires looking at what was there before — hence the parity framing.

The baseline is `2fc39c3`, the last commit before PRD #76 merged. That is "the deck as it was before the two pivots." Newer baselines drift into the architectures being audited.

A v1 attempt at this PRD ran in a different direction — a forward-looking behavior audit of the current codebase. That attempt confirmed the force-shutdown gap is real (carried forward as the worked example in this PRD). The v1 attempt's other findings do not carry forward: they either target a hypothetical remote-network-attach architecture (laptop-TUI ↔ remote-daemon over a network) that this codebase does not implement, or they concern post-baseline behavior (detach windows, daemon-side event application during disconnect) that has no parity analog because the baseline had no daemon. Those questions belong to a separate post-baseline behavior audit, not this one.

### 2026-05-22: Three-bucket taxonomy

Preserved / Regressed / Intentional change. The v1 attempt had four buckets including "Local-attach assumption" — scoped against a network-attach architecture that the deck does not implement. `docs/remote-environments.md:52–67` is explicit: `connect` runs the TUI on the remote alongside the daemon, the laptop is just a terminal. Same host, same filesystem, same user, same process tree. The Local-attach bucket has no referent in this codebase and is dropped.

### 2026-05-17: Audit, not refactor

Retained from the original PRD. The audit explicitly does not fix anything. Mixing audit and fix work obscures the audit's scope — readers cannot tell whether a clean area was checked or simply not visited. Each Regressed row is drafted as a follow-up milestone in the audit document; not filed as a GitHub issue until the user reviews and authorizes, and fixes are scoped separately.

*Partially superseded by the four 2026-05-22 / 2026-05-23 scope-expansion decisions above. The "audit excludes fixes" guidance no longer applies to F1 / F2 / F3 / F4 / F5 / F8 / F9 / F10 specifically — all eight land on this PRD's branch. The broader principle (that future audits should not interleave fix work with their findings discovery) is unchanged.*
