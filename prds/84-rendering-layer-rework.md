# PRD #84: Rendering layer rework — contract-driven render instead of symptom patches

**Status**: Defined; ready to implement
**Priority**: Medium
**Created**: 2026-05-10
**GitHub Issue**: [#84](https://github.com/vfarcic/dot-agent-deck/issues/84)

## Problem

Visual rendering keeps breaking in the same recurring ways, and targeted fixes have not stuck:

- **Text scrambles in panes, especially near the bottom.** Cells in the lower rows of an embedded pane sometimes show stale or wrong content — characters from earlier output, fragments of a different layout, or off-by-one row shifts. It happens most after layout changes (tab switch, opening/closing a side pane, resizing) and disproportionately near the bottom of the pane.
- **Empty space appears on the right after resize.** Growing the terminal leaves a column band of unfilled space on the right edge of one or more panes until something forces another redraw cycle.
- **Other scrambling under layout changes.** Reactive pane recreation, mode switches, and tab-bar toggling have each at various points caused short-lived render glitches that needed their own targeted patches.

We have made several attempts at fixing each symptom individually. Most of them helped, but none of them solved the whole class — and each fix has tended to add another defensive clamp or heuristic to the render path rather than remove the cause.

### Why this is happening (current architecture)

There is no single owner of "what gets drawn this frame." Three responsibilities are derived independently and resynced reactively, through scattered code paths:

- **Layout rects** are computed inline inside the render functions. `render_frame` (`src/ui.rs:3563`) splits the frame into tab bar + main + hints, then per active tab variant computes its own dashboard/pane sub-splits. `render_mode_tab` (`src/ui.rs:4108`) computes another layout. Each tab-open / pane-close / mode-switch path also computes layout-derived dimensions ad hoc to drive its own resize call.
- **PTY sizes** are pushed reactively from many entry points. `resize_dashboard_panes` and `resize_mode_tab_panes` (`src/ui.rs:586-664`) are called on `Event::Resize` (`src/ui.rs:2402-2407`), but `embedded.resize_pane_pty(...)` is *also* called from at least: tab open/close (`src/ui.rs:1510`, `src/ui.rs:2011-2017`), reactive pane recreation (`src/ui.rs:2147-2158`), mode-switch (`src/ui.rs:2828-2865`), and orchestration role transitions (`src/ui.rs:3259-3422`). Each of these computes its own dimensions from its own local view of the layout. There is no single function that says "given the current frame area and active tab, here is the size each pane's PTY must be."
- **Render-time decisions in `TerminalWidget`** mask sync errors with heuristics. `src/terminal_widget.rs:96-117` clamps `cols = inner.width.min(screen_size.1)` "to avoid reading stale cells from a wider/taller buffer before a resize event fires" and chooses a row window anchored on the cursor position. Both are defenses against the layout/PTY mismatch the upstream code paths can't guarantee. They mostly hide the bug; when they don't, the symptom is "scrambled text" or "empty space on the right."

So when the user resizes, opens a tab, or changes mode:

1. The frame area changes (or the layout sub-area does).
2. Some — but not necessarily all — code paths fire `resize_pane_pty`. Whether they do, and to what dimensions, depends on which path triggered the change.
3. The next frame renders. `TerminalWidget` reads the vt100 screen at whatever size it currently is, clamps to `min(area, screen)`, and chooses a row window heuristically from the cursor.
4. If layout area, PTY size, and vt100 buffer size are not in lockstep this frame, the user sees a glitch: blank columns on the right (area > pty cols), stale rows at the bottom (area > pty rows, or cursor moved unexpectedly), or scrambled fragments (different code paths resized to different dimensions).

The vt100 clamp comment (`src/terminal_widget.rs:94-95`) is the smoking gun: the render layer is defending against an upstream invariant that nobody enforces.

### Why this matters

- **Symptom-level fixes don't converge.** Every patch so far has been local — clamp here, re-resize there, defensive heuristic in the widget. Each one moves the failure to a different corner of the layout space. We will keep paying for this until the contract is explicit.
- **Defensive clamps mask real bugs.** When the widget silently truncates to `min(area, pty.cols)`, a layout bug that hands a wider area than the PTY size shows up as "empty columns" instead of a panic — easy to miss in tests, easy to confuse with a styling issue, hard to root-cause.
- **The blast radius is growing.** Layout complexity has gone up (tab bar, multiple tab variants, orchestration role panes, reactive pane recreation, mouse hit-testing on rects). Each new layout site is another place that has to remember to resize PTYs correctly. The current "everyone resizes their own panes" pattern does not scale.
- **It blocks honest testing.** PRD #77 (TUI testing harness) gives us a way to assert on rendered output, but until the render path has a contract, there is nothing meaningful to assert beyond "doesn't panic."

## Solution

Define an explicit rendering contract, validate it against a catalog of known failure modes, and reimplement the render path against the contract. The goal is **fewer code paths that decide pixel-level outcomes**, not a different look-and-feel.

The contract we are aiming for:

1. **One layout pass per frame.** A single function takes the frame area, active tab view, and pane set, and produces a structured layout: every rect that anything will draw into, named. Render functions read from this structure; they do not split layout themselves.
2. **PTY size is derived from the layout rect, not pushed by event handlers.** After the layout pass, a single resize step compares each pane's current PTY size against its target rect (inner area, minus borders) and commits any deltas via `resize_pane_pty`. Tab open/close, mode switch, reactive pane recreation, and `Event::Resize` all converge to "recompute layout → resize PTYs to match → render." No code path resizes a PTY based on its own private dimension calculation.
3. **`TerminalWidget` renders 1:1 against its area.** No `min(area, screen)` clamp. No cursor-anchored row windowing. The widget assumes the upstream contract holds (PTY size matches inner area in cells); it draws every cell of the PTY screen into the corresponding cell of the area. If the contract is violated, the failure is loud — an assertion in debug builds, a single explicit log in release — not a silently-wrong frame.
4. **Resize sequencing is fixed and explicit.** Within a frame: (a) compute layout, (b) commit PTY resizes to match layout, (c) render. The order is enforced by the call structure of the main loop — there is no path that renders before resizing or resizes after rendering.

### Shape of the change

- **`src/ui.rs:586-664`** (`resize_dashboard_panes`, `resize_mode_tab_panes`, and similar helpers): collapsed into a single `resize_panes_to_layout(layout: &FrameLayout, embedded: &EmbeddedPaneController)` that runs after layout and before render. The per-tab-variant helpers go away.
- **All ad hoc `embedded.resize_pane_pty(...)` calls** in tab open/close, reactive pane recreation, mode switch, and orchestration role transitions (sites listed under *Why this is happening*): removed. The next frame's layout-driven resize handles them. Tab-state mutations stop trying to push PTY dimensions; they just update tab state, and the layout pass picks up the new shape.
- **A new `compute_frame_layout(...)` function** (in `src/ui.rs` or a new `src/layout.rs`) takes `(frame_area, &TabView, &TabBarInfo, pane_ids)` and returns a `FrameLayout` struct: tab-bar rect, hints rect, and per-tab-variant pane rects keyed by pane id. Both the layout-driven resize and the render path consume this struct. `render_frame` and `render_mode_tab` stop computing rects inline.
- **`src/terminal_widget.rs:94-117`** (the col clamp and the cursor-anchor row window): removed. The widget renders the full PTY screen against the area it was given, in row-major order, no clamping, no windowing. A debug-mode `debug_assert!` checks that PTY (rows, cols) matches inner-area (height, width); release-mode logs once and falls back to `min` rather than panicking.
- **`Event::Resize` handling at `src/ui.rs:2402-2407`** becomes trivial: it just `break`s to trigger a re-render. The new layout pass at the top of each render iteration handles the rest.
- **A failure-mode catalog** is added under `tests/` (or extending the harness from PRD #77) with one deterministic reproducer per known visual bug: "scramble in bottom rows after tab switch with X panes," "right-edge empty band after enlarging from W to W+10," "scramble after reactive pane replace," etc. These are run as part of CI and are the gate for M5 — the new contract has to make every entry pass.

### Out of scope

- **No look-and-feel changes.** Colors, borders, focus styling, cursor block, hints bar text — all unchanged. This PRD is about *correctness*, not appearance.
- **No new layout features.** No splittable panes, no resizable splitters, no zoom mode. The layouts produced by the new contract are the ones the app already has.
- **No vt100/terminal-emulator replacement.** We continue to use vt100 + ratatui. The contract is about how *we* drive them, not about swapping them.
- **No PTY protocol changes for the daemon.** Stream-backed (daemon) panes still have no resize op; the M2.x note in `src/embedded_pane.rs:272-279` stands. Local panes get the contract; remote panes keep their current best-effort behavior until the daemon resize op lands.
- **No persistence of layout state across restarts.** Layout is recomputed every frame, as today.
- **No mouse / interaction changes.** Hit-testing already reads from `ui.side_pane_rects` / `ui.agent_pane_rect`; those will be populated from `FrameLayout` instead of inline, which is a refactor, not a behavior change.

## Milestones

- [ ] **M1 — Failure-mode catalog and reproducers.** Build a list of every known visual bug from prior fixes, in-tree commit history, and current observed behavior (scramble near bottom, empty band on resize, scramble on tab switch with N panes, scramble on reactive pane replace, mode-switch artefacts). For each: a deterministic reproducer, ideally as a test against the PRD #77 TUI testing harness. If the harness can't yet drive the needed scenario (resize event, tab switch, pane open/close), extend it minimally as part of this milestone. **Exit criterion**: every catalogued bug has a failing or flagging test. This is the gate that makes M2-M5 measurable.

- [ ] **M2 — Write the rendering contract.** Land a short design doc (in `prds/84-rendering-layer-rework.md` as an addendum, or `docs/rendering-contract.md`) that states the four invariants — single layout pass, layout-driven PTY size, 1:1 widget render, fixed resize sequencing — and names the call sites that enforce each. No code changes in this milestone. The contract is the spec the next milestones implement against.

- [ ] **M3 — Single layout pass.** Add `compute_frame_layout(...)` returning `FrameLayout`. Migrate `render_frame` and `render_mode_tab` to read rects from `FrameLayout` instead of computing them inline. `ui.side_pane_rects` and `ui.agent_pane_rect` are populated from `FrameLayout` after computation. No PTY-resize changes yet — this milestone is purely a layout-extraction refactor. Existing tests should still pass; behavior should be unchanged.

- [ ] **M4 — Layout-driven PTY resize.** Add `resize_panes_to_layout(...)` that runs once per frame, before `terminal.draw`. Remove ad hoc `resize_pane_pty` calls from tab open/close, reactive pane recreation, mode switch, and orchestration role transitions. `Event::Resize` handler is reduced to a re-render trigger. M1 reproducers for "empty band after resize" and "scramble after layout change" should now pass without the widget-level clamps still in place — this is the test that the contract is doing real work, not just being masked by `terminal_widget`.

- [ ] **M5 — Simplify `TerminalWidget`.** Remove the `min(area, screen)` col clamp and the cursor-anchored row windowing in `src/terminal_widget.rs:94-117`. Add `debug_assert!` for the PTY-size-equals-area invariant; in release, log once on mismatch and fall back to `min` so we never panic in production. Re-run the M1 catalog: every reproducer must pass. If any still fail, the contract has a hole — fix the upstream code path, do *not* re-add the clamp.

- [ ] **M6 — Tests and pre-PR validation.** Promote the M1 reproducers from "scenarios we ran by hand" to permanent CI tests covering the contract's invariants. Then a single end-to-end pass per `feedback_validate_pre_pr.md`: drive the failure scenarios from the Problem section interactively (resize, tab switch with N panes, mode switch, reactive pane replace) and confirm no scrambling, no empty bands, no glitches. Then PR.

## Validation Strategy

The bug class is interactive and partly non-deterministic — race-y resize timing, layout-state-dependent. Validation has two layers:

1. **The M1 catalog.** Every known failure mode has a deterministic reproducer in the test harness. The catalog is the *truth* — if a reproducer passes after M5, that bug is fixed; if it fails, it's not. This replaces "did we patch enough symptoms" with "does the contract hold."
2. **Pre-PR interactive pass.** Per `feedback_validate_pre_pr.md`, a single user-driven end-to-end pass before PR, exercising the failure scenarios listed in the Problem section. The catalog gives confidence; the interactive pass catches anything the catalog missed.

Per-milestone interactive validation is explicitly *not* required (per `feedback_validate_pre_pr.md`). M3 and M4 are mechanical refactors validated by the existing test suite; M5 is validated by the M1 catalog; M6 is the interactive pass.

## Risks & Mitigations

| Risk | Mitigation |
|---|---|
| The M1 catalog underestimates the failure-mode space — there are scrambling bugs we haven't seen yet that the new contract doesn't address. | The catalog is a starting point, not a closed set. M5 explicitly says "if a reproducer still fails, fix the upstream path, don't re-add the clamp" — this rule applies to bugs found *after* M1 too. The contract's invariants (layout owns size, widget renders 1:1) are strong enough that most resize/layout glitches should fall within them. New failure modes that don't fit are signal that the contract itself is wrong, not that we should patch the widget. |
| Removing the `min(area, screen)` clamp in `TerminalWidget` (M5) will surface latent bugs that the clamp was hiding — wrong-sized PTY, off-by-one in layout. | The `debug_assert!` in M5 surfaces these in CI / dev runs immediately. Release-mode fall-through-to-`min` keeps behavior at least as good as today. The M1 catalog's purpose is exactly to make these latent bugs visible before M5 ships, not after. |
| Removing scattered `resize_pane_pty` calls (M4) breaks features that rely on the *exact frame* of resize — e.g. an open-pane animation, or a code path that reads the PTY screen synchronously after resize. | Audit every removed `resize_pane_pty` call site for "is something reading the PTY screen between this resize and the next render?" — grep `screen()` and `parser.lock()` after each. None of the current call sites should depend on synchronous resize, but the audit is mandatory in M4. |
| The daemon-backed (stream) panes have no PTY resize op (`src/embedded_pane.rs:272-279`); the layout-driven resize in M4 will silently skip them, leaving stream panes with the same drift the contract is meant to eliminate for local panes. | Acceptable for this PRD. Stream panes inherit current best-effort behavior, documented. PRD #81 (Remote Kubernetes Transport) and the daemon resize op are the places where stream-pane resize gets fixed; this PRD does not block on them. The contract holds for local panes — the majority of pane usage — and degrades gracefully for stream. |
| The PRD #77 testing harness may not yet support all the events M1 needs (resize, tab switch, mode switch, reactive pane replace). | M1 is allowed to extend the harness as part of its scope. If the harness needs significant new capability, that becomes a sub-milestone before M1 can complete. The harness extensions are in service of this PRD's measurability, not a separate effort. |
| `render_frame` and `render_mode_tab` are large (8309-line `ui.rs`); the M3 layout extraction touches a lot of code and risks regression in the current visual appearance. | M3 is a *refactor only* — no behavior change. Existing test suite must pass unchanged before merge. If any rendered output differs from `main` for the same input, the refactor is wrong. |
| The "scramble near the bottom" symptom may not be a layout/PTY-sync bug at all — it could be a vt100-parser bug, a bell sequence handling issue, or a scrollback interaction. | M1's deterministic reproducer for this specific symptom is the test. If after M4 + M5 the reproducer still fails, the cause is below this PRD's scope (in vt100 or below) and we open a follow-up. The contract still gives us a clean foundation to investigate from. |

## References

- `src/ui.rs:586-664` — `resize_dashboard_panes`, `resize_mode_tab_panes` (per-tab-variant resize helpers, to be unified in M4)
- `src/ui.rs:1510`, `src/ui.rs:2011-2017`, `src/ui.rs:2147-2158`, `src/ui.rs:2828-2865`, `src/ui.rs:3259-3422` — ad hoc `resize_pane_pty` call sites in tab open/close, mode switch, reactive pane recreation, orchestration role transitions (to be removed in M4)
- `src/ui.rs:2402-2407` — `Event::Resize` handler (reduced to a re-render trigger in M4)
- `src/ui.rs:3563-3656` — `render_frame` (computes layout inline; migrates to `FrameLayout` in M3)
- `src/ui.rs:4108` — `render_mode_tab` (computes layout inline; migrates to `FrameLayout` in M3)
- `src/terminal_widget.rs:94-117` — col clamp + cursor-anchored row windowing (removed in M5)
- `src/embedded_pane.rs:272-295` — `resize_pane_pty` (the one resize primitive; stream-backed panes have no remote resize op)
- `prds/77-tui-testing-harness.md` — testing harness used by M1 (may need extension)
- `prds/81-remote-kubernetes-transport.md` — separate effort that owns daemon-side PTY resize
- `feedback_validate_pre_pr.md` — single pre-PR validation pass policy
