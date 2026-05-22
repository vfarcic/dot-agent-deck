# PRD #92: Pre-daemon parity audit + remediation

**Status**: Planning
**Priority**: Medium
**Created**: 2026-05-17
**Last updated**: 2026-05-22
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

### Out of Scope

- **Hypothetical bugs in current code that have no baseline equivalent.** The v1 audit attempt drifted into this and surfaced findings (notably one about remote-network-attach assumptions) against an architecture this codebase does not have — TUI and daemon are always co-located, see `docs/remote-environments.md:8` and `:52–67`. Parity only.
- **Performance, security, or any other axis.** Behavioral parity only.
- **Pre-PRD-#76 bugs that the daemon transition incidentally fixed.** Those are improvements, not regressions.
- **Features that genuinely did not exist at baseline** (the `remote add/list/remove/upgrade` family, daemon idle-shutdown, daemon log destination, lazy-spawn semantics, attach protocol Hello handshake, KIND_EVENT plumbing, etc.). These are post-baseline additions, not parity concerns. List them in an appendix to the audit doc so a future re-audit knows what was deliberately added.
- **Fixes for any finding *beyond* F1 / F2 / F3.** The audit produced only those three actionable rows; any future regressions surfaced by a re-audit are out of scope for this PRD and would be filed as a successor PRD.

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
- **No CLI command this round.** `dot-agent-deck stop` and `dot-agent-deck remote stop <name>` are explicitly deferred to a successor PRD (filed as F4 in the audit doc).

**F2 fix — `y` / `n` permission key:**

- Pressing `y` on a card whose session is in `WaitingForInput` approves the pending permission request.
- Pressing `n` on the same card denies it.
- The help overlay text (`src/ui.rs:5536` in current code) accurately describes the now-working behavior — nothing to change in the help text itself.
- Unit tests cover both the approve and deny key arms and the status gating.

**F3 fix — stale socket-path doc:**

- `docs/configuration.md:22` (plus any other stale references found in `docs/installation.md` and `docs/remote-requirements.md`) reflects the actual `/tmp/dot-agent-deck-{uid}.sock` path and includes a one-sentence note explaining the per-user disambiguation. The `$XDG_RUNTIME_DIR/dot-agent-deck.sock` default and the env-var override behavior are unchanged in the doc.

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

### Phase 6: Design + implement F1 — Stop option in Ctrl+C dialog

- [x] **M6.1** — Lock the F1 design. *Shipped: see the locked "F1 design" subsection in Design Decisions below. Stop is a third option in the existing Ctrl+C dialog (Detach default / Stop / Cancel). Secondary y/n confirmation only when agents are alive (defaults to No). `KIND_SHUTDOWN` is the wire signal; the daemon iterates the registry, SIGTERMs each agent with a short grace before SIGKILL, then exits. No CLI command this round — `dot-agent-deck stop` deferred to F4.*
- [ ] **M6.2** — Implement Stop per the locked design. Touches `src/ui.rs` (dialog rendering + key handler for the primary and secondary dialogs), `src/daemon_protocol.rs` (`KIND_SHUTDOWN` frame definition), `src/daemon_client.rs` (TUI-side send-and-await-ack helper), `src/daemon.rs` (handler that triggers the existing shutdown path immediately), `src/agent_pty.rs` (terminate-all path with SIGTERM→SIGKILL escalation if not already present).
- [ ] **M6.3** — Tests covering: primary dialog shows Detach / Stop / Cancel with Detach default; Stop with `agents_count == 0` skips the secondary dialog and triggers shutdown; Stop with `agents_count > 0` shows the secondary dialog with the agent count rendered in the text; secondary dialog defaults to No; Yes confirms (triggers shutdown), No returns to the primary dialog without shutting down; daemon receives `KIND_SHUTDOWN`, terminates managed agents, and exits within a bounded window; idempotency — two `KIND_SHUTDOWN` frames in quick succession do not crash the daemon; session save runs on the Stop path.

### Phase 7: Pre-release

- [ ] **M7.1** — Manual test pass covering F1, F2, F3 (orchestrator drives with the user). Confirm the quit dialog still behaves as the M4.2-collapsed Detach/Cancel; confirm idle shutdown still works for the no-agents case; confirm the new F1 command behaves per the locked design; confirm `y` / `n` approve/deny works on a real `WaitingForInput` session; confirm the doc updates read cleanly on the rendered docs site.
- [ ] **M7.2** — Changelog fragment via `dot-ai-changelog-fragment`. The user-visible headlines are the new F1 command, the `y` / `n` keybindings going live, and the doc fix; the audit deliverable itself is internal and does not need a changelog entry.
- [ ] **M7.3** — PR description includes (a) the audit findings summary (counts per bucket plus a pointer to `audit/pre-daemon-parity-audit.md`), (b) the F1 / F2 / F3 fix summary, (c) the manual-test-pass results from M7.1, and (d) links to any successor PRDs or follow-up issues if the audit surfaces additional work during implementation.

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

Fix targets (Phases 4–6):

- **F3** (doc fix): `docs/configuration.md` (line 22 plus surrounding env-var table), `docs/installation.md`, `docs/remote-requirements.md`.
- **F2** (`y` / `n` permission key): `src/ui.rs` (`handle_normal_key`, `WaitingForInput` gating, plus any approve/deny wiring); `src/state.rs` if approve / deny needs to mutate session state. Unit tests inline in `src/ui.rs` next to the existing `test_mode_transitions` cluster.
- **F1** (Stop option in Ctrl+C dialog): `src/ui.rs` (primary and secondary dialog rendering + key handlers + `KeyResult::Stop` variant), `src/daemon_protocol.rs` (`KIND_SHUTDOWN` frame), `src/daemon_client.rs` (TUI-side `send_shutdown` helper), `src/daemon.rs` (handler that triggers the existing shutdown path immediately on `KIND_SHUTDOWN`), `src/agent_pty.rs` (terminate-all path with SIGTERM→SIGKILL escalation). Tests under `tests/` — a new `tests/stop_dialog.rs` for the dialog flow plus `KIND_SHUTDOWN` round-trip tests added to `tests/daemon_protocol.rs` and `tests/daemon_lifecycle.rs`. No CLI command this round, so `src/main.rs` is untouched.

## Design Decisions

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
5. ~~Local vs remote scope~~ → local only this round; remote-stop deferred to F4.
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

*Partially superseded by the 2026-05-22 scope-expansion decision above. The "audit excludes fixes" guidance no longer applies to F1 / F2 / F3 specifically — those land on this PRD's branch. The broader principle (that future audits should not interleave fix work with their findings discovery) is unchanged.*
