# PRD #241: Safe pane closing — scope Ctrl+W to command mode, confirm destructive close, unwedge NotFound

**Status**: Implementation complete — PR pending (M1–M5 done; M6 partial)
**Priority**: High
**Created**: 2026-07-28
**GitHub Issue**: [#241](https://github.com/vfarcic/dot-agent-deck/issues/241)
**Consolidates**: [#88](https://github.com/vfarcic/dot-agent-deck/issues/88), [#192](https://github.com/vfarcic/dot-agent-deck/issues/192), [#218](https://github.com/vfarcic/dot-agent-deck/issues/218)
**Feature flag**: **No** `experimental` gate (CLAUDE.md rule 9, asked and answered). This is a safety fix for an already-destructive action, and gating it would leave the footgun armed by default for every user who does not opt in — inverting the flag's purpose. The confirmation dialog in M3 is technically a new user-visible surface, but it exists to *guard* the destructive path, so it ships with it.

## Problem Statement

`Ctrl+W` — delete-previous-word in shells, readline, vim, and essentially every other TUI — instantly and irreversibly tears down a pane in dot-agent-deck, from any mode, with no confirmation. Three users have reported it independently over two months. It is two defects plus a discoverability gap.

### Gap 1 — `close_pane` resolves with no mode guard

`global_action` (`src/ui.rs:5333`) is called at `src/ui.rs:9646` **unconditionally** with respect to mode:

```rust
// src/ui.rs:9646 — no ui.mode check
if action.is_none() && !is_ctrl_c {
    action = global_action(&kb, &key);
}
```

Inside it, `ClosePane` maps straight to `Action::CloseSelected` (`src/ui.rs:5347`). Because this runs *before* the per-mode dispatch, `handle_pane_input_key` (`src/ui.rs:3823`) — which would otherwise forward the keystroke to the PTY via `keyevent_to_bytes` — never sees the key. Typing `Ctrl+W` in an embedded shell therefore kills the pane instead of deleting a word, and there is no way to obtain the word-delete behavior at all.

The surrounding code makes the omission stark. The two adjacent dispatch blocks are both mode-gated:

- `src/ui.rs:9625` — jump-to-card: `if !is_ctrl_c && ui.mode == UiMode::Normal`
- `src/ui.rs:9651` — tab cycling: `if action.is_none() && !is_ctrl_c && ui.mode == UiMode::Normal`

And `Ctrl+C` gets an explicit carve-out at `src/ui.rs:9620` (`is_ctrl_c`, threaded through every block) so it reaches the quit modal from the dashboard while being delivered as SIGINT `0x03` in `PaneInput`. That is exactly the precedent #218 cites — already implemented, just as a hardcoded special case for one key rather than a general rule. `close_pane` is *more* destructive than quit (quit is modal-guarded; close is immediate) yet it is the one that fires while typing.

`global_action`'s own doc comment describes the behavior as intentional — "the four configurable global commands resolve from the active keybinding config (any chord, **any mode**)" — so this is a design decision to revisit, not an oversight to patch silently.

### Gap 2 — a `NotFound` stop-agent response wedges the close forever

Reported by buffdeveloper in [#218's follow-up](https://github.com/vfarcic/dot-agent-deck/issues/218) and confirmed in the code. `close_pane` (`src/embedded_pane.rs:1799`) removes the pane from the registry, then requires `stop_agent` to succeed before teardown completes. There *is* already a retry for `"not found"` (`src/embedded_pane.rs:1864-1877`, from PRD #92 F12), but it retries with the **same** shared agent id:

```rust
Ok(Err(ClientError::Server(ref msg))) if msg.to_lowercase().contains("not found") => {
    let retry_id = shared_agent_id.lock().unwrap().clone();   // same dead id for a ghost card
    ...
}
```

That retry exists for a narrow race — `Ctrl+W` landing inside the ~300 ms reattach window, where the io_task may have swapped in a *new* agent id. For a stale card whose daemon-side agent is simply gone, `retry_id == initial_agent_id`, both calls return `"not found"`, and control falls to `Ok(Err(e))` at `src/embedded_pane.rs:1885`, which **re-inserts the pane** and returns an error. The user sees:

```
Failed to close pane 20: Command failed: stop-agent failed for pane 20: daemon returned error: Agent 20 not found — press Ctrl+W to retry
```

Retry can never succeed, because the "failure" — the agent is already gone — is precisely the state the stop was trying to reach. Only a full detach and relaunch clears the ghost card.

The re-insert itself is correct and deliberate: the comment at `src/embedded_pane.rs:1885` explains that silently degrading to detach would leave the agent alive on the remote with no signal to the user. The bug is narrower than the restore path — it is that `NotFound` is classified as a failure at all.

### Gap 3 — command mode has no visible exit

petolofsson, commenting on #88: *"Just sat down again... I honestly don't know how to get out of command mode."* They were reaching for `Ctrl+W` and finding `Ctrl+T`.

The UI actively causes this. `dashboard_hints_string` (`src/ui.rs:11917`) renders, while the user is **already in** the dashboard:

```
Ctrl+n: new  Ctrl+w: close  Ctrl+t: layout  Ctrl+d: dashboard (1-9 ? /)  Ctrl+c: quit
```

`Ctrl+d: dashboard` is dead text in that context — it names the destination the user is standing in, never revealing that the same chord is the way back to the pane. The help overlay repeats the one-directional framing: `help_key_line(&n(KbAction::Dashboard), "Command mode (dashboard)")` (`src/ui.rs:12005`). Meanwhile `PaneInput` *does* tell you (`src/ui.rs:4168`: `"PaneInput mode — type to interact, {} for dashboard"`), so the guidance exists in one direction only.

### Why this took three reports to surface

The maintainer was not receiving notifications for issues opened by others (the repo was unwatched, so GitHub never sent them), so #88 sat for two months and #192 and #218 arrived as independent rediscoveries of the same wound. Recorded here because it explains the duplication, not as a technical constraint.

## Solution Overview

Make closing a pane something the user can only do **on purpose**, and make it work when they do.

1. **Mode-gate `close_pane` (fixes Gap 1).** Resolve `ClosePane` only in command mode, so `Ctrl+W` falls through to `handle_pane_input_key` and reaches the PTY in `PaneInput`. Generalizes the `is_ctrl_c` precedent instead of adding a second special case.
2. **Confirm the deliberate close (what #88 and #192 asked for).** A modal on the close path, modeled on the existing `QuitConfirm` machinery.
3. **Treat `NotFound` as already-stopped (fixes Gap 2).** Close completes instead of wedging.
4. **Make command mode's exit visible (fixes Gap 3).** Fix the hint that currently names the mode you are already in.

### Architecture

The Gap 1 fix sits at one seam — the `global_action` call site at `src/ui.rs:9646` — and the design question is *where* the mode knowledge lives:

**Recommended: gate dispatch, keep the config key in `[global]`.** Add the `ui.mode` condition at the call site (or split `ClosePane` out of `global_action` into the Normal-mode blocks), leaving `ActionSpec { section: Section::Global, name: "close_pane", default: "Ctrl+w" }` (`src/keybindings.rs:124-129`) untouched.

**Rejected: move `ClosePane` to `Section::Dashboard`.** `Section` (`src/keybindings.rs:28-31`) determines the **TOML table name** a binding is read from (`Section::as_str`), so reclassifying the action silently relocates its config key from `[global]` to `[dashboard]`. Every existing `keybindings.toml` setting `[global] close_pane` would stop applying — and buffdeveloper's follow-up shows at least one user has exactly that line. `from_toml_str` returns warnings rather than hard errors (`src/keybindings.rs:597`), so the failure mode is a warning the user may never see plus a silent revert to the `Ctrl+W` default. Not worth it for an organizational nicety.

Note that `Section` is config-facing only: dispatch is hand-written per call site, so a section change would require the dispatch edit *anyway*. Gating dispatch alone is strictly less disruptive.

**Coupled surface:** `dashboard_hints_string` (`src/ui.rs:11917`) advertises `Ctrl+w: close` unconditionally. Once the action is mode-gated, the hints bar must not advertise it outside command mode, or it promises a key that no longer closes. The hints bar is the single source of truth for both the live bar and the `render_hints_bar_to_buffer` snapshot entrypoint, so this is one edit with L1 coverage already pointed at it.

## Scope

### In Scope

- Gate `ClosePane` resolution on command mode; `Ctrl+W` forwards to the PTY in `PaneInput` via the existing `keyevent_to_bytes` path.
- Confirmation dialog before a pane close, following the `UiMode::QuitConfirm` / `handle_quit_confirm_key` pattern (`src/ui.rs:181`, `src/ui.rs:3831`).
- Classify a `"not found"` stop-agent response (after the existing PRD #92 F12 retry) as already-stopped: complete the teardown rather than re-inserting the pane.
- Fix the command-mode exit hint in `dashboard_hints_string` and the help overlay's `Ctrl+D` description.
- Make the hints bar reflect mode-gated availability of `close`.
- L1 coverage for dispatch gating, the confirmation dialog, and the hints-bar strings; PTY-attached L2 coverage per CLAUDE.md rule 4.
- Docs for the changed keybinding semantics, changelog fragment, and the rule 12 cross-version answer.

### Out of Scope

- **General per-mode scoping in `keybindings.toml`.** #218 offers it as an alternative; deferred until a second action needs it. It is a config-format expansion (new axis, parsing, validation, migration, docs) and the reported pain is fully addressed without it.
- **Reclassifying `ClosePane` into `Section::Dashboard`** — rejected above on config-compatibility grounds.
- **Rebinding `Ctrl+C`** or revisiting the quit flow. The `is_ctrl_c` carve-out is the precedent being generalized, not the target.
- **The broader keybinding discoverability question** beyond the command-mode exit hint. No general audit of every hint string.
- **Changing what close *does*** once confirmed — `stop-agent` remains the explicit kill path per the `src/embedded_pane.rs:1817` contract.

## Technical Approach

### M1 — mode-gate `close_pane`

Prefer splitting `ClosePane` out of `global_action` over threading a mode parameter into it: `global_action` is also called by `tests/mouse_dispatch.rs:37` and is exported (`pub fn`), so keeping its signature stable avoids churn in the mouse-dispatch mapper, which legitimately wants the mode-independent mapping. Whichever shape is chosen, `Dashboard` / `NewPane` / `ToggleLayout` must keep working from any mode — only `ClosePane` becomes mode-scoped.

Confirm the fall-through actually reaches the PTY: with the action no longer claiming the key, `handle_pane_input_key` should encode `Ctrl+W` as `0x17` (`^W`) and `write_raw_bytes` should deliver it, giving readline its native word-delete.

### M2 — unwedge `NotFound`

In the `Ok(Err(e))` arm at `src/embedded_pane.rs:1885`, branch on the `"not found"` classification already computed upstream and return `Ok(())` for it, letting `s` drop so the io_task aborts. Keep the existing re-insert for every other error and for the timeout arm at `src/embedded_pane.rs:1915` — those are the cases where the agent may genuinely still be alive.

Match the existing string-sniffing convention (`msg.to_lowercase().contains("not found")`) or, better, introduce a typed daemon error for it; note that string-matching a daemon message is the fragile part of the current design and a typed variant would be a small, contained improvement.

### M3 — confirmation dialog

`QuitConfirm` is the working model: a `UiMode` variant (`src/ui.rs:181`), a selection index in `UiState` (`src/ui.rs:1606-1609`), and a key handler (`src/ui.rs:3831`) returning an `Action`. Two decisions for the implementer:

- **Default option.** `QuitConfirm` deliberately defaults to the non-destructive choice ("Detach stays the default so the existing muscle memory does not become destructive" — `src/ui.rs:3835`). Apply the same reasoning: default to Cancel.
- **Where it fires.** `Action::CloseSelected` is dispatched from the keybinding (`src/ui.rs:5824`) *and* from the `[Close]` button (`src/ui.rs:11156`). Both should confirm, so the modal belongs on the action, not the key.

### M4 — command-mode exit discoverability

Make the `Ctrl+D` hint bidirectional. In command mode it should read as the way back to the pane rather than `dashboard`; the help overlay's `"Command mode (dashboard)"` should name it as a toggle. Cheapest honest fix is mode-aware hint text, which the existing `dashboard_hints_string` seam supports.

### M5 — tests (CLAUDE.md rule 4)

- **L1**: dispatch gating (`Ctrl+W` in `PaneInput` yields `ForwardToPane`, in `Normal` yields the confirm modal); confirmation dialog render + key handling; hints-bar snapshots for both modes.
- **L2, PTY-attached**: the genuine user-visible behavior is *"I typed Ctrl+W in a shell and it deleted a word instead of destroying my work."* A real shell with readline is the correct target for that assertion — this is not a stand-in standing in for an agent, it is the actual thing whose semantics are under test. Assert both halves: the word was deleted **and** the pane still exists. The negative half is what pins the bug.
- **L2, real agent**: additionally drive a real agent on a cheap model (Haiku) and confirm `Ctrl+W` while typing does not tear down the agent pane — this is the reported pain in its reported context. Follow `scheduler/dispatch/013` for interactive-headless setup (`prepare_claude_home`, per-folder trust, `--allowedTools`).
- Bug fix, so **no ` [reel]` marker** on the CATALOG entries.

### Cross-version contract (CLAUDE.md rule 12)

Touches the daemon RPC path, so the question is owed explicitly. Expected answer: **no** `PROTOCOL_VERSION` bump — M2 changes only how the *TUI interprets* an existing `stop-agent` error response; no wire shape moves and the daemon is unchanged. It is arguably a semantic change on the client side only, which does not break older/newer interop in either direction: an older daemon still returns the same error string, and a newer TUI simply stops treating it as fatal. Run the manual older-daemon check anyway (branch TUI against previous-release daemon; confirm delegate routing and hook delivery still work), and re-classify if the diff drifts further into the daemon.

## Success Criteria

- `Ctrl+W` while typing in an embedded shell deletes the previous word and **does not** close the pane.
- `Ctrl+W` while typing in an embedded agent pane does not tear down the agent.
- From command mode, `Ctrl+W` prompts for confirmation; cancelling leaves the pane untouched; confirming closes it.
- The `[Close]` button confirms on the same path as the keybinding.
- Closing a pane whose daemon-side agent is already gone **succeeds** and removes the card, instead of erroring and re-inserting it.
- A genuine `stop-agent` failure (daemon reachable, agent alive, stop refused) still retains the pane and surfaces the error — no silent degradation to detach.
- The hints bar does not advertise `close` in a mode where it does not close.
- A user in command mode can tell from the screen how to get back to their pane.
- `Dashboard`, `NewPane`, and `ToggleLayout` still work from every mode.
- Existing `keybindings.toml` files setting `[global] close_pane` keep applying.

## Milestones

- [x] **M1**: `ClosePane` mode-gated — `Ctrl+W` reaches the PTY in `PaneInput`, still closes from command mode; other global actions unregressed
- [x] **M2**: `NotFound` treated as already-stopped — ghost cards close cleanly; genuine stop failures still retain the pane
- [x] **M3**: Confirmation dialog on the close action (keybinding and `[Close]` button), defaulting to Cancel
- [x] **M4**: Command-mode exit discoverable — mode-aware hints bar and help overlay; `close` not advertised where unavailable
- [x] **M5**: L1 coverage plus PTY-attached L2 (real shell word-delete, real-agent no-teardown) green in the pre-PR e2e tier
- [ ] **M6**: Docs updated, changelog fragment added, rule 12 cross-version answer recorded; #88, #192, #218 closed with the fix referenced
  - [x] Docs updated (`docs/keyboard-shortcuts.md` plus four cross-references)
  - [x] Rule 12 cross-version answer recorded and **verified** — see Implementation Notes
  - [ ] Changelog fragment — written by the release step during `/prd-done`'s first push
  - [ ] #88, #192, #218 closed with the fix referenced — happens at merge

## Implementation Notes

Recorded because the implementation diverged from the plan in ways a future reader would otherwise have to re-derive. Commits: `ea98c68` (M1–M4 + docs), `6912998` (two pre-existing close flows made confirmation-aware), `991f270` (review/audit findings), `1a8eb7f` (tests driven through real dispatch + blocker guards), `b56ed08` (real-agent test reaches its assertion).

**`dashboard_hints_string` was never on the live path — not even before this PRD.** The Solution Overview calls it "the single source of truth for both the live bar and the `render_hints_bar_to_buffer` snapshot entrypoint". That is wrong: the live hints row *is the button bar*, so M4's first cut changed only a string the running app never rendered, and its test passed over a dead seam. Both now read one shared `ModeGlobals::for_mode`, and `Ctrl+D` was made a genuine two-way toggle (`resume_pane_input_target`) so the new "back to pane" hint is actually true. Anything else assuming `dashboard_hints_string` is live should be treated with the same suspicion.

**The confirmation had to be bound to a stable target.** Storing a selection index was not enough — `Ctrl+PgUp`/`Ctrl+PgDn` still resolved while the modal was open, so a confirmed close could destroy a different tab than the one armed. A `CloseTarget` (tab id or session id) is captured at arm time and re-resolved at confirm time; if the target is gone, nothing closes. Root cause of the leak: the existing `blocking_overlay` / `modal_active` lists are **mouse-only**, so they never guarded keyboard navigation.

**M2's predicate is narrower than "contains `not found`".** The original match would classify any error containing the phrase (`pane not found`, `session not found`, a wrapped `file not found`) as already-stopped and silently discard a **live** pane. It is now the exact id-scoped `Agent {id} not found`. A respawn race also needed closing: the daemon is asked who owns the pane slot, so a *replacement* agent is stopped rather than orphaned.

**Open Question 1 — skippable confirmation:** shipped the plain modal, no opt-out, per the PRD's own recommendation. A `y`/`n` one-keypress shortcut was added and then **removed**: a `y` that arrived before the modal rendered could confirm a close the user never saw. Confirming is `Down`+`Enter`; the event loop now discards input queued before the modal actually rendered (the mouse burst path was the live hole).

**Open Question 2 — tab vs pane copy:** the modal now **does** distinguish, after a fact-check found the single string inaccurate on every multi-pane target. A plain dashboard pane keeps the original wording (`Close selected pane?` / "stop the agent and remove it"); any close that takes a whole Mode/Orchestration tab reads `Close this tab and all its panes?` / "stop all agents and remove the tab". **No pane count is rendered, deliberately** — the number would have to be recomputed against a moving world (reactive pools grow and shrink, roles die into dead slots) and a confidently wrong "3 panes" is worse than no number.

The decision is **not** made from the `CloseTarget` variant, which was the trap: `CloseTarget::Session` does not mean "one card". The confirmed close resolves the armed session, finds its pane belongs to a Mode/Orchestration tab, and closes that entire tab; only a plain dashboard pane reaches the one-pane branch. So both the dialog's wording and the teardown branch on one function, `resolve_close_plan` → `ClosePlan::{Tab, Pane}` (`src/ui.rs`), and the scope is frozen onto `CloseConfirmState` at arm time exactly like the target itself.

**Teardown ordering — the tab is removed LAST.** `TabManager::close_tab` used to `tabs.remove(index)` *before* running the per-pane closes, so a genuine `stop-agent` failure produced an incoherent result: the tab was gone while the failed pane's card was deliberately retained "so the user can retry", with nothing left to press `Ctrl+W` on. Now every pane closes first and the tab is removed only when `outcome.is_clean()`. Closing stays deliberately **non-transactional** — panes that already closed stay closed — so on partial failure the tab is kept holding exactly what could not be stopped, and `forget_closed_panes` strikes the rest off it. That last part is load-bearing: without it the retry would re-`close_pane` panes that are already out of the registry, collect "Pane N not found", and the tab could never be closed again — issue #218's wedge, one level up. `StopOutcome::Done` **and** `DoneUnverified` both count as success for removal (G2's warning still reaches the status line via the `close_warnings` drain), as does an exact-`NotFound` agent.

**Per-pane closes run concurrently.** `close_panes_concurrently` (`src/pane.rs`) gives each pane its own scoped thread and collects results in input order. `close_pane`'s documented worst case is ~22.65 s (two `CTRL_W_STOP_TIMEOUT` rounds plus the 12.65 s bounded slot-owner chase); run one after another, a six-role orchestration tab's worst case was ~2 min 16 s of blocked render thread — and that was already true *before* the reorder, because the old `tabs.remove` was a `Vec` mutation with no frame drawn between it and the closes. Concurrency collapses the tab's worst case back to a single pane's ~22.65 s regardless of role count. No overall wall-clock budget was added on top: cancelling an in-flight `close_pane` from outside would abandon a pane mid-teardown with its re-insert-on-failure path still pending, which trades a bounded wait for an ambiguous registry.

**Open Question 3 — typed daemon error:** declined, with reason. Older daemons only ever send the string, so a typed variant would not remove the string match — it would add wire surface and force a `PROTOCOL_VERSION` bump for no safety gain. The match is confined to one `is_agent_not_found` helper.

**Open Question 4 — is the modal still worth it:** yes, shipped. #88 and #192 asked for it by name and both describe *deliberate* misclicks that mode-scoping does not catch.

**Close entry points:** four exist and all confirm — the chord, the `[Close]` button, the tab-strip `×`, and the modal's own `[Close]`. The tab-strip click originally bypassed the modal entirely. Six further paths were audited and excluded with reasons (two spawn-failure rollbacks, `TabManager::close_tab` internals, `ModeManager` reactive-pool churn, the quit dialog's Stop, and TUI-exit detach).

**Rule 12 cross-version result (verified, not just asserted):** no `PROTOCOL_VERSION` bump — it remains 6. This branch's TUI was run against a **v0.35.0** daemon: attach, delegate routing, status/`work-done` hook delivery, the close confirmation, and the stale-pane `NotFound` close all worked. No contract break behind a stable wire, so no `.breaking.md` fragment is owed.

**E2E gate:** 2153 passed, 0 skipped. Two failures were investigated rather than retried — `prompt/pane-input/022` was a real test defect (the new-pane Name field seeds from the directory basename, so the label landed behind a nondeterministic tempdir prefix and the readiness wait never matched, meaning the test had never reached its `Ctrl+W` press); and an OpenCode failure in `tests/e2e_delegate_work_done_chain.rs` (`NotFound(worker-pane)`) was confirmed **pre-existing** by reproducing it on `origin/main` — OpenCode cannot open its sandbox-blocked log path and exits. That test carries no `tests/CATALOG.md` entry, which is why it stayed invisible until it failed.

## Risks and Mitigations

| Risk | Mitigation |
|---|---|
| Mode-gating breaks a user who relies on `Ctrl+W` closing from inside a pane | That behavior is the reported bug, and the confirmation dialog means even the command-mode path is now guarded. Call it out in the changelog as an intentional behavior change. |
| Treating `NotFound` as success masks a real orphaned agent | `NotFound` is the daemon asserting the agent does not exist — the exact state stop wants. Every other error and the timeout keep the existing retain-and-surface path. |
| String-matching `"not found"` is brittle across daemon versions | Already the status quo at `src/embedded_pane.rs:1865`. Prefer a typed error variant; if not, keep the match in one helper rather than duplicating it. |
| A confirmation prompt adds friction to a common deliberate action | Default to Cancel but make confirm a single keypress. If friction complaints appear, an opt-out config is a cheap follow-up — do not pre-build it. |
| `global_action` signature churn breaks the mouse-dispatch mapper | `tests/mouse_dispatch.rs:37` calls it directly and wants the mode-independent mapping. Prefer splitting `ClosePane` out over adding a mode parameter. |

## Open Questions

1. **Should confirmation be skippable?** A `Ctrl+W Ctrl+W` double-tap or a config opt-out would serve power users, but adds surface. Recommendation: ship the plain modal, wait for feedback.
2. **Should confirmation distinguish "close pane" from "close tab"?** `Action::CloseSelected` closes a whole mode/orchestration tab when one is active (`src/ui.rs:18808`), which destroys more than a single pane. A tab-close arguably warrants different copy stating how many panes die.
3. **Typed daemon error for `NotFound`?** Contained improvement over string-sniffing, but touches `daemon_client`/`daemon_protocol` and may pull rule 12 into play more seriously. Sizing call for the implementer.
4. **Does the confirmation belong on `close_pane` at all once mode-gating lands?** #218 argues it becomes optional polish. Included here because #88 and #192 asked for it by name and both reporters described *deliberate* misclicks (`Shift+W`) that mode-scoping does not catch. Revisit only if M3 proves disproportionate.

## Verification Notes (from triage)

Recorded so the implementer does not re-derive them:

- `global_action` (`src/ui.rs:5333`) is called at `src/ui.rs:9646` with no `ui.mode` condition; the jump-to-card block at `:9625` and the tab-cycling block at `:9651` both carry `ui.mode == UiMode::Normal`.
- `is_ctrl_c` (`src/ui.rs:9620`) is the existing single-key precedent for mode-dependent handling of a global chord.
- `handle_pane_input_key` (`src/ui.rs:3823`) is a pure `keyevent_to_bytes` forward — nothing else has to change for `Ctrl+W` to reach the PTY once the action stops claiming it.
- The `"not found"` retry at `src/embedded_pane.rs:1864` re-reads the *shared* agent id, which is unchanged for a stale card, so both attempts fail identically and control reaches the re-insert at `:1885`.
- `Section` (`src/keybindings.rs:28-31`) maps to TOML table names via `as_str`, which is why reclassifying `ClosePane` is a config-compatibility break rather than a refactor.
- `dashboard_hints_string` (`src/ui.rs:11917`) hardcodes `Ctrl+c: quit` and derives everything else from config; it feeds both the live bar and `render_hints_bar_to_buffer`, so hint changes land in existing L1 snapshots.
- All three issues reproduce on `main` as of this PRD.
