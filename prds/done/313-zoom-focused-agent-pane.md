# PRD #313: Zoom the focused agent pane

**Status**: Implementation complete (M1–M7) — PR pending
**Priority**: Medium
**Created**: 2026-08-01

## Problem Statement

Even after [#311](https://github.com/vfarcic/dot-agent-deck/issues/311) gives the pane column entirely to the focused agent, a third of the width is still sidebar (`ORCHESTRATION_PANES_PERCENT = 66`, `src/ui.rs:1952`), plus the pane border, the button bar and — with more than one tab open — the tab strip.

That split is right when you are supervising: the sidebar is how you see which of seven agents is working, and it is the reason the orchestration view is worth looking at. It is wrong when you have stopped supervising and started *working in one agent* — reading a long diff, following a plan, going back and forth with the orchestrator on a laptop screen. In that mode the other agents are noise, and there is currently no way to say so.

[#307](https://github.com/vfarcic/dot-agent-deck/issues/307) asked for "bigger screen and easy to work with laptop screen", and followed up with "even a shortcut to get rid of that part on demand will help eyes". #311 answers the literal request; this covers the on-demand part.

## Solution Overview

A reversible zoom: press the key and the focused agent takes effectively the whole terminal; press it again and the normal multi-agent view returns. Two states, one key, no configuration.

This is the `tmux prefix+z` model, and deliberately so — it is the dominant precedent for terminal users (also zellij fullscreen, i3/sway fullscreen, vim `Ctrl+W _`), which means the behaviour needs no explaining and the word "zoom" already means this to the audience.

Zoom is a *view* state, not a mode with its own rules: everything you can do zoomed, you can do unzoomed. It changes what is drawn, never what is running.

## Scope

### In Scope

- A zoom toggle affecting the focused pane in any tab type that has more than one thing on screen.
- Hiding the sidebar and reclaiming the full width for the focused pane.
- A visible indicator that zoom is active.
- PTY resize on zoom and unzoom, so the agent reflows to the larger and smaller area.

### Out of Scope

- Terminal font size. "Zoom" here means "this pane fills the screen", not text scaling.
- Any change to which agents run — zoomed or not, every pane stays alive.
- Per-pane zoom persistence in the saved session (see Open Questions).
- The experimental feature flag. Explicitly decided against for this feature (CLAUDE.md rule 9 asks the question; the answer here is no — it ships visible).

## Technical Approach

Zoom is a third arrangement rather than a flag layered on top of the existing ones: given #311 and #312, the renderer already resolves "what arrangement does this tab use" in one place, and zoom is one more answer to that question. Expressing it in the same type keeps the compiler responsible for finding every geometry site, rather than sprinkling `if zoomed` through the render path.

The state itself is per-tab (or global — see Open Questions), and lives in UI state only; nothing about it reaches the daemon.

### Keybinding

`Ctrl+Z` is free — the `teleport = "Ctrl+z"` line in `src/keybindings.rs:1387` is a fixture inside `unknown_action_ignored_with_warning`, not a real action. But it is not free of consequence: `Ctrl+Z` in a pane is currently encoded to `0x1a` and forwarded to the agent (pinned by `keyevent_ctrl_c_and_ctrl_a`, `src/ui.rs:21419`), so a global binding takes away job control inside every pane.

The alternative is a command-mode letter (`z`), matching the existing command-mode single-key actions (`g`, `r`, `/`) and mapping almost exactly onto `tmux prefix+z` muscle memory — `Ctrl+D` is already this app's prefix. That costs one extra keystroke and preserves `Ctrl+Z` passthrough. Decide before implementing; either way the action is remappable.

### Cross-version safety

None — TUI-only view state. Patch bump.

## Success Criteria

- One key zooms the focused agent to effectively the whole terminal; the same key restores the previous view exactly.
- While zoomed, it is obvious at a glance that you are zoomed — nobody concludes their other agents have disappeared.
- Zooming and unzooming resizes the agent's PTY and the agent reflows correctly both ways, with no lost scrollback.
- Every non-focused agent keeps running while zoomed; delegation and hooks are unaffected.
- Repeated toggling does not degrade the agent's rendering.

## Milestones

- [x] **M1 — Zoom state and geometry.** The focused pane resolves to a full-terminal rect; sidebar and other panes are not drawn.
- [x] **M2 — Toggle wired to a binding**, per the decision in Open Question 1, remappable like every other action.
- [x] **M3 — Zoom indicator.** A visible marker while zoomed (see Open Question 2).
- [x] **M4 — Focus-change behaviour settled and implemented** (Open Question 3).
- [x] **M5 — L1 snapshot coverage** for zoomed and unzoomed geometry, per CLAUDE.md rule 4.
- [x] **M6 — L2 PTY coverage.** A vt100 test zooms a live agent, asserts the sidebar is gone and the agent still paints, unzooms and asserts the view is restored. Per rule 4 this is a user-facing feature, so it needs at least one PTY-attached test — and a real agent if it is to be reel-eligible.
- [x] **M7 — Docs and changelog.** `docs/keyboard-shortcuts.md` and `docs/orchestration.md` updated; changelog fragment added.

## Risks

- **Forgetting you are zoomed.** The failure mode is a user who thinks their agents vanished, or who watches one agent while another sits blocked. tmux mitigates this with a `Z` marker in the status line; M3 is not optional.
- **Hiding the sidebar hides the only live status of the other agents.** That is the point of the feature, but it means a zoomed user is genuinely less informed. Worth confirming the notification paths (idle-worker detection, work-done lines into the orchestrator pane) still reach them — they should, since those write into the orchestrator's own pane.
- **Resize churn.** Every toggle resizes the PTY; an agent that reflows badly will look worse zoomed than not. This is the thing to verify with a real agent rather than a stand-in.
- **Scope creep into a "focus mode".** Zoom should not acquire its own keybindings, rules or behaviours. If it starts to, it has become a mode and needs a different PRD.

## Open Questions

**All five are decided as of 2026-08-29 and implemented — see the Work Log for each decision and its reasoning.** They are kept here as written so the trade-off each one names stays legible next to the answer.

1. **`Ctrl+Z` globally, or `z` in command mode?** The trade-off is one keystroke against job-control passthrough in every pane. Leaning command-mode `z` for the tmux parallel and because losing `Ctrl+Z` inside an agent's shell is a real cost. — **This framing is a false dichotomy and the leaning was wrong; see the 2026-08-30 Work Log entry.** Scoping belongs to the action rather than the key, so a third option exists — `Ctrl+Z` claimed *only in command mode* — which keeps job-control passthrough in full and matches the `Ctrl+` convention every other global action uses. That is what shipped.
2. **What exactly does zoom hide?** Sidebar certainly. The pane border is the interesting one: it carries the title, focus, status colour (PRD #155 M3) and command-mode state (`9345a74` — a deliberate fix). Dropping it silently undoes that fix unless the button bar's `[Command Mode Ctrl+D]` is judged sufficient. Leaning: keep the border, and let it carry the zoom indicator for M3.
3. **Does zoom follow focus?** If you are zoomed on the orchestrator and jump to a role with `1`–`9`, do you stay zoomed on the new pane or drop back? tmux unzooms on pane switch; here the role-jump keys are a deliberate "go work with that agent" action, so following focus seems more useful. Needs a decision, not a default.
4. **Is zoom per-tab or global?** Per-tab means switching tabs shows each in its own state; global means zoom is a posture you are in. Per-tab is more predictable; global is simpler to explain.
5. **Does zoom survive detach/reattach and session restore?** tmux persists it. Here it is ephemeral UI state and simplest not to persist — but a user who zooms, detaches and returns to an unzoomed deck may find that surprising.

## Work Log

### 2026-08-30 — Open Question 1 REVISED to `Ctrl+Z`, and zoom extended to the Dashboard

Both changes come from review questions on PR #753, and both correct real mistakes in what was shipped the day before rather than adding scope.

**Open Question 1 is revised: the binding is now `Ctrl+Z`, still command-mode only.** The PRD framed the question as "`Ctrl+Z` **globally**, or `z` in command mode?" and stated the trade-off as one keystroke against job-control passthrough in every pane. That is a **false dichotomy**, and adopting it was the error: scoping is a property of the *action*, not of the key. `scope_zoom` already un-resolves the toggle whenever the mode is not command mode, so `Ctrl+Z` bound there costs **no** job control at all — in a pane the chord falls straight through to `ForwardToPane([0x1a])` exactly as before, which is precisely how `Ctrl+l` stays readline's clear-screen and `Ctrl+w` stays word-delete while you type. The cost the PRD priced was the cost of a *global* binding, which was never the only alternative.

With that cost gone the consistency argument decides it. Every other entry in the `[global]` section is a `Ctrl+` chord — `Ctrl+d`, `Ctrl+n`, `Ctrl+w`, `Ctrl+t`, `Ctrl+e`, `Ctrl+l` — and the digits `1`–`9`. A plain `z` was the only non-digit bare key in the section, which reads as tmux in one place and as this deck's own convention everywhere else. The bare letters elsewhere in the deck (`j`/`k`/`h`/`l`, `r`, `/`, `?`, `g`, `s`) are all `Section::Dashboard` and dashboard-scoped; on an orchestration tab nothing but the digits was a bare key before this PRD. `orchestration/layout/007` now pins both halves — `Ctrl+Z` resolves in command mode, a plain `z` never resolves at all, and `Ctrl+Z` in `PaneInput` still forwards `0x1a`.

**Zoom now works on the Dashboard as well, which is the scope this PRD always claimed.** The In Scope section reads "a zoom toggle affecting the focused pane in **any tab type that has more than one thing on screen**", and shipping orchestration-only was a silent narrowing of it — one that nobody caught, because the reviewer, the auditor and the orchestrator were all reasoning from the Problem Statement, which is written entirely about the orchestration tab. The justification recorded in `scope_zoom`'s doc comment ("on a Dashboard or Mode tab there is no orchestration sidebar to reclaim") was simply **wrong for the Dashboard**: it is a card sidebar beside a stack of agent panes at 33/67, structurally the same as an orchestration tab's 34/66, and the two are so alike they already share `right_column_pane_dims` — whose own doc comment says the dashboard and orchestration helpers "share the body and only differ on the width percentage". A third of the width is worth the same on either.

The extension is the mirror of what already existed: `Tab::Dashboard::zoomed` (a separate field from the orchestration one, so the two tabs never zoom each other), `dashboard_layout_percents(zoomed)` returning `(0, 100)` or the 33/67 default, the same effective-`Stacked` resolution so M1's "other panes are not drawn" half holds under `Ctrl+t`, and the same `[Z]` indicator. The scoping predicate widened from `is_orchestration_tab` to `tab_has_card_sidebar`. `orchestration/layout/012` pins the geometry, the reversibility and the Tiled behaviour, and asserts the **33/67** default specifically so the test fails if zoom is ever wired to the orchestration resolver by mistake.

**Mode tabs are deliberately still excluded, and filed rather than guessed at.** A Mode tab is not sidebar-plus-panes — it is two pane regions, an agent pane on the left and side panes on the right at 50/50 — so "hide the sidebar and take the frame" has no meaning there, and the honest design question ("what does zoom mean when a *side* pane is focused?") has no obvious answer. It needs a decision and its own tests rather than a third geometry branch bolted onto the end of this PR. Tracked as [#758](https://github.com/vfarcic/dot-agent-deck/issues/758), which records the two candidate semantics and what each would cost.

### 2026-08-29 — Verification complete: e2e gate green with recording, rule 12 cross-version run clean; M6 ticked

**M6 is done and all seven milestones are complete.** `tabs/orchestration/011` (stand-in `cat` roles, deterministic) and `tabs/orchestration/012` (a real interactive Claude agent on Haiku, ` [reel]`-marked) both pass, with casts recorded under `.dot-agent-deck/recordings/`.

**The e2e tier ran once with `DOT_AGENT_DECK_RECORD=1`**, per CLAUDE.md rule 5 and PRD #180: 9181 tests, 9177 passed, 170 per-test recording directories written. Three failures were unrelated flaky-tolerant real-agent tests, each green on rerun — `card_stats_005` (agent still `Processing…` at the deadline, though the grid showed the card had narrowed correctly), `chain_smoke_pi_002` (`PermissionDenied` staging the pi extension into the worker HOME), and `shell_activity_006` (the model never issued the Bash tool call; self-diagnosed in the failure output as prompt adherence, not a badge regression). None touches `src/ui.rs` or `src/terminal_widget.rs`.

**`tabs/orchestration/012` had a genuine defect, and it was in the test rather than in zoom.** It first timed out at 180.004s with no grid attached, because its declared waits budget over 500s against nextest's default `60s x 3` — the default could only ever kill it mid-wait, hiding the real cause. A scoped `[[profile.default.overrides]]` carve-out (`terminate-after = 9`, the same shape `route_001` and `shell_activity_005/006` already carry) turned the timeout into a proper failure at 182.3s with its grid attached, and that grid showed zoom working perfectly: `┌worker [Z]` at column 0, the real agent's UI reflowed to 120 columns, nothing blanked. The directive was sitting in Claude Code's composer unsubmitted. Replaying the cast through a vt100 emulator pinned it — the whole focus → zoom → type sequence completed by t=5.0s and then nothing happened for the remaining 175s, because an Enter pressed in the first seconds of a Claude Code boot is dropped. `orchestration/lock/012` types a directive the same way and passes only because its own 20s locked-directive wait sits in front of it as an accidental readiness gate. Fixed entirely in the test (`6c53691`) by waiting for the pane stream to settle before typing, plus a bounded re-press recovery that only the sentinel token can end. Green twice afterwards, at 15.4s and 12.2s.

**Rule 12 cross-version run: no contract change, confirmed at runtime rather than inferred.** Run against the previous release **v0.38.0**, with both builds reporting `server_version: 7` — so this exercised semantics behind a stable wire, which is what rule 12 actually targets, rather than a handshake rejection. Delegate routing, status hooks and work-done delivery all survived five zoom/unzoom cycles, **including while zoomed**: a work-done notice painted into the zoomed orchestrator pane at full 120-column width, visibly un-wrapped beside the earlier 78-column ones, and a status transition that occurred while the sidebar was hidden was still correct on unzoom. The worker pane reflowed 78 → 118 → 78 columns with its entire scrollback intact, which confirms in practice the reading of vt100 0.16.2's `Screen::set_size` recorded in the previous entry. This closes the audit's third blocker. No `PROTOCOL_VERSION` bump, no `.breaking.md`, patch bump.

**The cross-version scenario is not reachable the obvious way, and that is now written into CLAUDE.md rule 12.** Following the rule literally — start the v0.38.0 daemon, launch the branch TUI — the branch TUI silently terminated that daemon and lazy-spawned its own in under a second, because PRD #103/#161's build-version handshake restarts a mismatched daemon with no prompt when **no agents are running**. The result looks like a successful cross-version run and is a same-version run. Rule 12's existing phrase "with an agent under it" turns out to be load-bearing; the reachable sequence is v0.38.0 daemon → v0.38.0 TUI to bring the roles up → close that TUI → branch TUI → decline the mismatch prompt. Verified by exactly one `Attach protocol listening` line for the whole run and the same daemon pid (`0.38.0-g5a56361`) serving it end to end.

**Three follow-up issues were spun off rather than fixed here**, all deliberately out of this PRD's scope: [#747](https://github.com/vfarcic/dot-agent-deck/issues/747) (above ~4096 columns the client parser is not bounded by `PTY_RESIZE_DIM_MAX`, so the pane renders at a different width than the child uses — pre-existing in the shared resize path, which zoom only reaches at a lower threshold), [#748](https://github.com/vfarcic/dot-agent-deck/issues/748) (24 exported `*_to_buffer` render seams allocate a caller-sized `TestBackend` with no upper bound; PRD #313 bounded only its own new seam), and [#749](https://github.com/vfarcic/dot-agent-deck/issues/749) (`render_frame`'s `pane_layout` parameter is now vestigial and should be removed so the effective layout has a single compiler-enforced source).

**The `[Z]` assertions were hardened against a spoofed display name.** The audit's spoof is real and agent-reachable — display names arrive through the hook socket and are sanitized by `sanitize_display_name`, but `strip_control_and_bidi` strips control characters and bidi overrides and not brackets — and it was measured: an unzoomed role literally named `orchestrator [Z]` satisfied a `grid.contains("[Z]")` assertion. Every positive assertion is now anchored to the border title of the box the geometry actually expanded, via a new shared `common::role_pane_border_title` helper, and `render/layout/006` additionally asserts the marker's cells carry `Modifier::REVERSED` — the one channel plain title text cannot occupy, and the discriminator that separates the real marker from a spoofed one. Negative assertions were deliberately left as whole-grid `!contains` checks: "the marker is nowhere" is strictly stronger than "not on this one border", and a spoof there can only cause a false failure, never a false pass.

### 2026-08-29 — Tiled closed, review and audit findings resolved; M5 ticked

M1's second half ("sidebar and other panes are not drawn") was only true under `Stacked`. Under `Tiled` the sidebar resolved away correctly and then every role pane kept an equal full-width slice of the column, so zoom read as "make all the panes wider" instead of "get everything else out of the way" — pinned by the tester as `orchestration/layout/011` with three roles, so that "only the focused one is drawn" cannot be satisfied by an implementation that merely drops one slot.

**The fix is an EFFECTIVE layout resolved at the geometry seam: `if zoomed { Stacked } else { pane_layout }`, in the Orchestration arm of `compute_frame_layout`.** That is the one place zoom already owns, and resolving there reuses PRD #311's existing "the focused pane fills the area, every other pane reserves zero rows" machinery, so zoom needs no drawing rule of its own and there is no second `if zoomed` anywhere. Effective, not stored: `ui.pane_layout` is the user's `Ctrl+t` choice and stays exactly as they set it, which is what makes "the same key restores the previous view exactly" true for a tiled deck — zoom overrides the frame, not the preference.

**Sub-decision A — `FrameContent::Cards { pane_layout }` carries the EFFECTIVE value, not the raw one.** So `pane_target_dims` sizes the undrawn panes "as if focused" (PRD #311 M2), consistent with what `orchestration/layout/010` pins for `Stacked`. Carrying the raw `Tiled` instead would give them `(0, cols)`, `resize_panes_to_layout` would skip them on its `rows == 0` guard, and they would sit on their stale pre-zoom Tiled slice for the whole zoom and reflow only on the way back — which is the double reflow #311 M2 exists to prevent.

**Sub-decision B — resolve ONCE and thread the same value to both `compute_frame_layout` and `render_frame`.** The tester confirmed that resolving only the geometry is *safe* (zero-height rects render nothing and `contract_guaranteed` is exempt at zero dimensions), but it leaves two places disagreeing about what layout the frame is in, which is exactly what PRD #84 invariant 1 exists to stop. Rather than pass a second argument down, `render_frame` now reads the effective value back out of `FrameContent::Cards` — so the render pass uses the value the geometry actually used, by construction, and its `pane_layout` argument stays the deck's stored toggle.

**The `[Z]` marker now keys off the pane the geometry expanded, not focus equality** (reviewer finding 1). The marker was appended when `zoomed && focused_id == Some(id)`, but the pane that actually fills a zoomed frame is chosen by `stacked_expanded_index`, which falls back to pane 0 when focus is `None`. The reviewer could not reproduce a divergence and believes it unreachable — you must be in command mode on the tab to have zoomed it — but a full-frame pane rendering with no marker is precisely the "you forgot you were zoomed" failure M3 exists to prevent, and it would be arrived at by two answers disagreeing rather than by either being wrong. Asking the same helper the split asked makes "the visible pane carries `[Z]`" structural instead of contingent, and it stays correct under the effective-`Stacked` path above.

**The `[Z]` marker is rendered in its own style, and display names are deliberately NOT validated** (auditor suggestion 5). The concern is real and agent-reachable: display names arrive over the hook socket and `sanitize_display_name` strips control characters and bidi overrides but not brackets, so an agent can call itself `worker [Z]` and make an *unzoomed* pane look zoomed. The fix is to give the real indicator a channel plain text cannot occupy — `TerminalWidget::with_zoom_marker` draws it as a separate span in `zoom_marker_style()` (reversed + bold on the focus accent) — rather than to reject or rewrite names containing `[Z]`. Reserved-token validation was considered and declined: a user who names a role `worker [Z]` and confuses themselves is not a security boundary, and `sanitize_display_name` is shared code well outside this PRD. Styling also serves M3's actual goal, since an indicator that stands out is better at its job. The marker moved out of `render_terminal_panes`' `pane_name` closure and into the widget for this, which also keeps deck chrome out of the pane name the PRD #84 contract diagnostic reports.

**The L1 render seam is now total over its arguments and bounded in allocation** (auditor blockers 1 and 2). `render_orchestration_frame_to_buffer` is `#[doc(hidden)] pub`, which hides it from the docs but is not a compile barrier: empty `role_names` hit an `assert!`, a zero dimension panicked inside ratatui via `render_tab_strip`, and `TestBackend::new(u16::MAX, u16::MAX)` would have asked for 4.29 billion cells. It now clamps to `RENDER_SEAM_DIM_MAX` (1024) and `RENDER_SEAM_ROLES_MAX` (64) *before* allocating and returns a blank buffer of the clamped size for empty roles or a zero dimension. This is hardening of a test seam this PRD introduced, not the closing of an exploitable hole — it is here because the fix is a few lines and a panicking `pub fn` is a bad thing to leave in a library. The sibling seams were checked and deliberately left alone as out of scope; see the follow-up note below.

**The zero-width sidebar is no longer laid out while zoomed** (reviewer finding 2). Zoomed, `split_cards_area` still yielded a dashboard rect of width 0 and `render_card_grid` ran on it every frame, writing `ui.columns`. The reviewer verified it was harmless — `max_columns_for_width(0).max(1)` and `grid_columns(0)` both floor at 1 — and recommended no change. Guarded anyway, because the objection is not the wasted work: it is a hidden surface writing shared UI state, which is a trap for whoever touches `ui.columns` next. It came out as one `draw_sidebar = dashboard_area.width > 0` and three wraps in `render_frame`, so it stayed inside the "only if it is a small guard" bound.

**Declined: normalizing the >4096-column PTY resize mismatch (auditor suggestion 4) — filed as [#747](https://github.com/vfarcic/dot-agent-deck/issues/747).** At terminal width 4099 zoom produces 4097 inner columns; `resize_pane_pty` applies `vt100::Screen::set_size(.., 4097)` locally and sends 4097 on the wire while `AgentPtyRegistry::resize` silently clamps the child PTY to `PTY_RESIZE_DIM_MAX` = 4096, so the local parser renders at a width the child is not using. It is declined here because it is **pre-existing**: the unzoomed 34/66 split has the same mismatch at roughly 6.2k columns, and zoom only lowers the threshold. Fixing it means changing the clamp for every layout in the app, through the shared resize path — which is the scope creep this PRD's own risk list names as its top risk. The issue records the two thresholds and the four places that must agree (the rendered rect, the local parser, the wire request, the daemon PTY).

**Not addressed here: the rule 12 cross-version run (auditor blocker 3).** Static audit confirms no wire shape, daemon handler, hook handler, role-map meaning or `PROTOCOL_VERSION` changed, and nothing in this batch moves that conclusion — every change is in `src/ui.rs` and `src/terminal_widget.rs`. The manual run against the previous-release daemon remains outstanding for the pre-PR step.

**M5 ticked** — `render/layout/006` landed and passes, and both its snapshots are unchanged by the styling above (they capture cell text, not style). M6 stays unticked: `tabs/orchestration/011` and `/012` exist but the e2e tier has not been run against the implementation yet.

### 2026-08-29 — Open Questions decided; M1–M4 and M7 implemented

All five Open Questions are settled, and the implementation lands the decisions rather than deferring any of them. Each decision, and why:

**Q1 — `z` in command mode, not `Ctrl+Z`.** The PRD's own leaning, confirmed by what implementing it showed. `Ctrl+Z` in a pane is currently encoded to `0x1a` and forwarded to the agent (pinned by `keyevent_ctrl_c_and_ctrl_a`), so a global binding would take job control away from every shell and TUI running inside a pane — a permanent cost paid by everyone, to save one keystroke for the person zooming. `Ctrl+D` is already this app's prefix, so `Ctrl+D` `z` maps onto `tmux prefix+z` almost exactly. The bill for the choice is that the binding is an ordinary letter, which makes the scoping load-bearing rather than tidy: `global_action_for_mode` resolves ahead of every per-mode handler, so an unscoped `z` would be swallowed not only in `PaneInput` but in the filter row, a rename and the new-pane form. `scope_zoom` therefore gates on BOTH terms — orchestration tab AND `UiMode::Normal` — and that was verified against the live funnel, not assumed: a `z` typed into the filter, into a rename and into the new-pane form's Name field all land as the literal character with the tab left unzoomed, while the same key in command mode on the same tab zooms. Remappable as `toggle_zoom`.

**Q2 — hide the sidebar and the non-focused panes; KEEP the focused pane's border.** The PRD's leaning, and the border turns out to be doing more work than the question implied: it carries the title, the focus weight, PRD #155 M3's status colour and commit `9345a74`'s command-mode fix. Dropping it would silently undo that fix for the one view where the user is most zoomed-in on a single agent. Keeping it also gives M3 somewhere to live that costs no rows. The indicator is the literal `[Z]` appended AFTER the role name (`orchestrator [Z]`) — bracketed so it cannot be confused with a role name or agent output, and positioned after the name because the pane-box scan every orchestration test anchors on looks for `<corner><role>` with no separator.

**Q3 — zoom follows focus.** The role-jump keys are a deliberate "go work with that agent", so dropping the zoom on a jump would fight the intent; tmux unzooms on pane switch, but tmux's pane switch is navigation rather than a role selection. It also costs nothing to implement: `focus_deck` does not write the tab's focused role at all (that is synced once per frame from the pane controller), so as long as `zoomed` lives on the TAB and the render zooms whichever pane is focused, following focus falls out for free. Anything that reset `zoomed` on a focus change would have had to be added deliberately.

**Q4 — per-tab.** tmux zooms a *window*, not a session, and the two states here answer different questions: PRD #336's split is a standing reading preference (hence global), while zoom says "I have stopped supervising and am working in *this* agent". A tab the user never zoomed must not silently lose its sidebar, which a global would do to every tab opened afterwards. It is also strictly simpler: `Tab::Orchestration::zoomed` is itself the source of truth with no `TabManager` mirror, so none of the cross-tab broadcast loop `TabManager::toggle_orchestration_split` needs exists here.

**Q5 — ephemeral.** Not persisted across launches and not written to the saved session. Reattaching should always return the full supervisory view — the deck's answer to "what is everyone doing" is the thing you come back for, and a session that restores zoomed hides it at exactly the wrong moment. It also keeps the change strictly presentation-only, with zero persistence surface to version.

**What landed.** `KbAction::ToggleZoom` (`toggle_zoom`, default `z`) and `ui::Action::ToggleZoom`; `scope_zoom` applied at the one dispatch site with tab context and, mode-term only, inside `key_action_for_mode`; a `dispatch_action` arm that flips the ACTIVE tab's flag and nothing else; `Tab::Orchestration::zoomed` and its mirror on `ActiveTabView::Orchestration`; `orchestration_layout_percents(narrow, zoomed)` wrapping PRD #336's `orchestration_split_percents` and answering `(0, 100)` when zoomed, consumed by `compute_frame_layout`; the `[Z]` marker in `render_terminal_panes`' `pane_name` closure; a help-overlay row; and `render_orchestration_frame_to_buffer`, a `#[doc(hidden)] pub` full-frame L1 render seam (the first of its kind — every other `*_to_buffer` export renders one bar, card, grid or modal) that M5's `render/layout/006` drives.

**Scope discipline held.** Zoom touches geometry in exactly one place and the border title in exactly one place. Nothing else branches on it: the PTY resize needed no plumbing of its own, because `resize_panes_to_layout` reads `FrameLayout::pane_target_dims()`, which derives from the rects `compute_frame_layout` already produced — so making the layout pass zoom-aware is sufficient for the agent to reflow. The spawn path was left alone: a newly opened tab always starts unzoomed, so no role pane can ever be spawned into a zoomed tab.

**Rule 12 — no contract change.** The diff is confined to `src/keybindings.rs`, `src/tab.rs` and `src/ui.rs`, and touches no daemon, protocol, orchestration-runtime or hook code; `AttachRequest::Resize { id, rows, cols }` already exists and is already driven by the per-frame sweep, so zoom adds no wire message. No `PROTOCOL_VERSION` bump, no `.breaking.md`, no cross-version run — patch bump. Related and checked: `agent_pty::resize()` drops the daemon-side scrollback RING on a real dimension change, but that only affects the snapshot a *fresh* subscriber would replay; the attached TUI's own vt100 screen is resized through `vt100::Screen::set_size`, which resizes the grid in place and touches neither its contents nor its scrollback. A toggle therefore does not blank what the user is looking at.

**Rule 9 — no experimental flag**, per this PRD's explicit decision. It ships visible.

### 2026-08-01 — Created

Split out of the [#307](https://github.com/vfarcic/dot-agent-deck/issues/307) discussion. Sequenced last: [#311](https://github.com/vfarcic/dot-agent-deck/issues/311) and [#312](https://github.com/vfarcic/dot-agent-deck/issues/312) both reshape the layout seam this builds on. Note that #311 alone may satisfy the original request — this is the additional, on-demand step, and is worth reassessing once #311 has shipped and been used.
