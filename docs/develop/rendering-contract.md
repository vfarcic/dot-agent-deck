# Rendering Contract

This is the internal design contract for the dot-agent-deck render path. It is the
spec that PRD #84 ("Rendering layer rework") implements against. It states the four
invariants the render layer must hold and, for each, names the call site(s)
responsible for enforcing it.

The goal is **fewer code paths that decide pixel-level outcomes** — not a different
look-and-feel. Colors, borders, focus styling, the cursor block, and hints text are
all unchanged by this contract.

> **Why this exists**
>
> Visual glitches (scrambled text near the bottom of a pane, an empty band on the
> right after a resize, short-lived scrambling on tab/mode switches) all trace back
> to one root cause: there is no single owner of "what gets drawn this frame."
> Layout rects, PTY sizes, and the vt100 buffer were each derived independently and
> resynced reactively through scattered code paths. When they fall out of lockstep
> for a frame, the user sees a glitch. The defensive `min(area, screen)` clamp in
> `TerminalWidget` is the smoking gun: the render layer was defending against an
> upstream invariant that nobody enforced. This contract makes that invariant
> explicit and assigns each part of it an owner.

> **A note on line numbers**
>
> `src/ui.rs` is large and changes often, so the line numbers below are approximate
> and given only as a starting hint. The **function names** are the durable
> reference — find the function, not the line.

## The four invariants

### 1. One layout pass per frame

A single function computes every rect that anything will draw into, named, once per
frame:

```text
compute_frame_layout(frame_area, &TabView, &TabBarInfo, pane_ids) -> FrameLayout
```

`FrameLayout` carries the tab-bar rect, the hints rect, and the per-tab-variant pane
rects keyed by pane id. Render functions **read** rects from this struct; they do not
split layout themselves.

**Enforced by:**

- `compute_frame_layout(...)` (new; in `src/ui.rs` or a new `src/layout.rs`) — the
  sole producer of `FrameLayout`.
- `render_frame` (`src/ui.rs`, ~`fn render_frame`) — consumes `FrameLayout` instead of
  splitting the frame into tab bar + main + hints and computing per-variant
  dashboard/pane sub-splits inline.
- `render_mode_tab` (`src/ui.rs`, ~`fn render_mode_tab`) — consumes `FrameLayout`
  instead of computing its own layout.
- `ui.side_pane_rects` and `ui.agent_pane_rect` (used for mouse hit-testing) are
  populated **from** `FrameLayout` after computation, not assembled inline during
  render. This keeps hit-testing reading the same rects the widgets drew into.

### 2. PTY size is derived from the layout rect, not pushed by event handlers

After the layout pass, a single resize step compares each pane's current PTY size
against its target inner rect (area minus borders) and commits only the deltas:

```text
resize_panes_to_layout(layout: &FrameLayout, embedded: &EmbeddedPaneController)
```

No code path resizes a PTY based on its own private dimension calculation. Tab
state mutations just update tab state; the next layout pass picks up the new shape.

**Enforced by:**

- `resize_panes_to_layout(...)` (new) — the **only** caller of `resize_pane_pty` in
  the steady-state render loop. Replaces the per-tab-variant helpers
  `resize_dashboard_panes` / `resize_mode_tab_panes` / `resize_mode_tab_panes_for`
  (`src/ui.rs`, ~1320–1430), which go away.
- `resize_pane_pty` (`src/embedded_pane.rs`, ~`fn resize_pane_pty`) — remains the one
  resize primitive; it is now driven from one place.

**Removed** — every ad hoc `embedded.resize_pane_pty(...)` call that computed its own
dimensions from a local view of the layout:

- Tab open / close paths (`src/ui.rs`, around the `resize_pane_pty` calls near
  ~1348, ~1354, ~1423, ~1510).
- Reactive pane recreation (`src/ui.rs`, ~6196 and nearby).
- Mode switch.
- Orchestration role transitions.

The next frame's layout-driven resize handles all of these.

### 3. `TerminalWidget` renders 1:1 against its area

The widget assumes the upstream contract holds — the PTY screen is already the size
of the inner area in cells — and draws every cell of the PTY screen into the
corresponding cell of the area, in row-major order. **No `min(area, screen)` column
clamp. No cursor-anchored row window.**

If the contract is violated, the failure is **loud**, not silently wrong:

- **Debug builds:** `debug_assert!` that PTY `(rows, cols)` equals inner-area
  `(height, width)`.
- **Release builds:** log once on mismatch and fall back to `min` rather than
  panicking — behavior at least as good as today, never a crash.

**Enforced by:**

- `TerminalWidget::render` (`src/terminal_widget.rs`, ~`fn render`). The current
  `cols = inner.width.min(screen_size.1)` clamp (~line 94) and the cursor-anchored
  row-window logic (~lines 98–117) are removed and replaced by the 1:1 draw plus the
  debug-assert / release-log-and-fallback above.

### 4. Fixed, explicit resize sequencing

Within a single frame, the order is always:

1. **Compute layout** — `compute_frame_layout(...)`.
2. **Commit PTY resizes to match** — `resize_panes_to_layout(...)`, before
   `terminal.draw`.
3. **Render** — `render_frame` / `render_mode_tab` read from `FrameLayout`.

There is no path that renders before resizing, or resizes after rendering.

**Enforced by:**

- The call structure of the main loop in `src/ui.rs` — the (compute → resize →
  draw) order is hard-wired into the loop, not left to individual event handlers.
- `Event::Resize` (`src/ui.rs`, ~6503) is reduced to a **re-render trigger**: it
  breaks out to run another loop iteration, and the layout pass at the top of that
  iteration does the rest. It no longer pushes PTY dimensions itself.

## Convergence

Every trigger that changes the visible shape — terminal resize, tab open/close, mode
switch, reactive pane recreation, orchestration role transition — converges to the
same three steps:

```text
recompute layout  ->  resize PTYs to match  ->  render
```

That single convergence point is the whole contract. The earlier "everyone resizes
their own panes" pattern is what this replaces.

## Out of scope / known caveats

- **Stream-backed (daemon) panes have no PTY resize op.** `resize_panes_to_layout`
  silently skips them; they keep current best-effort behavior. The daemon-side
  resize op is owned by PRD #81 (Remote Kubernetes Transport), not this contract.
  See the note in `src/embedded_pane.rs` (~272–295).
- **No new layout features.** No splittable panes, resizable splitters, or zoom mode.
  The layouts produced are exactly the ones the app already has.
- **No vt100/ratatui replacement.** The contract is about how *we* drive them, not
  about swapping them.

## Validation

The contract is measured against the M1 failure-mode catalog under `tests/` (one
deterministic reproducer per known visual bug). The rule, from M5 onward: **if a
reproducer still fails, fix the upstream code path — do not re-add the clamp.** A
reproducer that can't be made to pass within the contract is signal that the
contract has a hole, not that the widget needs another defensive heuristic.

## References

- PRD #84 — `prds/84-rendering-layer-rework.md` (Problem, Solution, Milestones).
- `src/ui.rs` — `render_frame`, `render_mode_tab`, the resize helpers, and the
  `Event::Resize` handler.
- `src/terminal_widget.rs` — `TerminalWidget::render` (the clamp + row window to be
  removed).
- `src/embedded_pane.rs` — `resize_pane_pty` (the one resize primitive).
