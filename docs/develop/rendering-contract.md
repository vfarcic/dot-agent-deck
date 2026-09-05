# Rendering Contract

This is the internal design contract for the dot-agent-deck render path. It is the spec that PRD #84 ("Rendering layer rework") implements against. It states the four invariants the render layer must hold and, for each, names the call site(s) responsible for enforcing it.

The goal is **fewer code paths that decide pixel-level outcomes** — not a different look-and-feel. Colors, borders, focus styling, the cursor block, and hints text are all unchanged by this contract.

> **Why this exists**
>
> Visual glitches (scrambled text near the bottom of a pane, an empty band on the right after a resize, short-lived scrambling on tab/mode switches) all trace back to one root cause: there is no single owner of "what gets drawn this frame." Layout rects, PTY sizes, and the vt100 buffer were each derived independently and resynced reactively through scattered code paths. When they fall out of lockstep for a frame, the user sees a glitch. The defensive `min(area, screen)` clamp in `TerminalWidget` is the smoking gun: the render layer was defending against an upstream invariant that nobody enforced. This contract makes that invariant explicit and assigns each part of it an owner.

> **A note on line numbers**
>
> `src/ui.rs` is large and changes often, so the line numbers below are approximate and given only as a starting hint. The **function names** are the durable reference — find the function, not the line.

## The four invariants

### 1. One layout pass per frame

A single function computes every rect that anything will draw into, named, once per frame:

```text
compute_frame_layout(frame_area, &TabView, &TabBarInfo, pane_ids) -> FrameLayout
```

`FrameLayout` carries the tab-bar rect, the hints rect, and the per-tab-variant pane rects keyed by pane id. Render functions **read** rects from this struct; they do not split layout themselves.

**Enforced by:**

- `compute_frame_layout(...)` (new; in `src/ui.rs` or a new `src/layout.rs`) — the sole producer of `FrameLayout`.
- `render_frame` (`src/ui.rs`, ~`fn render_frame`) — consumes `FrameLayout` instead of splitting the frame into tab bar + main + hints and computing per-variant dashboard/pane sub-splits inline.
- `render_mode_tab` (`src/ui.rs`, ~`fn render_mode_tab`) — consumes `FrameLayout` instead of computing its own layout.
- `ui.side_pane_rects` and `ui.agent_pane_rect` (used for mouse hit-testing) are populated **from** `FrameLayout` after computation, not assembled inline during render. This keeps hit-testing reading the same rects the widgets drew into.

### 2. PTY size is REQUESTED from the layout rect, not pushed by event handlers

After the layout pass, a single resize step compares each pane's current PTY size against its target inner rect (area minus borders) and commits only the deltas:

```text
resize_panes_to_layout(layout: &FrameLayout, embedded: &EmbeddedPaneController)
```

No code path resizes a PTY based on its own private dimension calculation. Tab state mutations just update tab state; the next layout pass picks up the new shape.

**PRD #882 changed who decides, and this is the single most important thing to know about invariants 2 and 3.** A PTY has exactly one window size, so every client attached to an agent sees the same grid — per-client sizing of a live TUI is unavailable, not unimplemented, because the agent lays out for one `TIOCGWINSZ` answer and emits absolute cursor positioning for that grid. The daemon therefore owns the geometry: it sizes each agent to the **smallest viewport among its attached viewers**, and larger clients pad the remainder. The layout rect is now what a client **asks for**, not what it gets.

Two consequences follow, and both are load-bearing:

- **The delta check compares against the last REQUEST, not the parser.** `resize_panes_to_layout` used to compare its target against the pane's local vt100 parser, which worked while `resize_pane_pty` set that parser synchronously — "kept in lockstep with the PTY" was true for exactly as long as one client existed (issue #883). The parser now holds the daemon's ANSWER, which legitimately differs from the target whenever another client's pane is smaller, so a parser-based comparison would find a delta on every frame and re-send forever. `EmbeddedPaneController::requested_dims` is the comparison.
- **The parser is reshaped by the answer, never by the request.** `resize_pane_pty` no longer touches it. The pane learns its geometry from the resize response's `applied_rows`/`applied_cols`, from the attach response, or from a `KIND_GEOMETRY` push — the last of which is what keeps a client that is only *watching* correct, since it never asks for anything and so would otherwise never learn that somebody else moved the size.

**Enforced by:**

- `resize_panes_to_layout(...)` (new) — the **only** caller of `resize_pane_pty` in the steady-state render loop. Replaces the per-tab-variant helpers `resize_dashboard_panes` / `resize_mode_tab_panes` / `resize_mode_tab_panes_for` (`src/ui.rs`, ~1320–1430), which go away.
- `resize_pane_pty` (`src/embedded_pane.rs`, ~`fn resize_pane_pty`) — remains the one resize primitive; it is now driven from one place.

**Removed** — every ad hoc `embedded.resize_pane_pty(...)` call that computed its own dimensions from a local view of the layout:

- Tab open / close paths (`src/ui.rs`, around the `resize_pane_pty` calls near ~1348, ~1354, ~1423, ~1510).
- Reactive pane recreation (`src/ui.rs`, ~6196 and nearby).
- Mode switch.
- Orchestration role transitions.

The next frame's layout-driven resize handles all of these.

### 3. `TerminalWidget` renders 1:1 against its area

The widget draws the PTY screen 1:1 into the area — screen cell (r, c) → inner cell (r, c), row-major, from row 0 / col 0 — and leaves anything past the screen blank. **No cursor-anchored row window.**

**PRD #882 removed the equality assertion, and the removal is a design change rather than a relaxation.** PRD #84 could assert `screen == inner area` because the client owned the geometry: `resize_pane_pty` set the parser synchronously, so the two were the same number by construction and any difference was a bug. The daemon owns it now (invariant 2), and the two legitimately differ whenever another client's view of the agent is smaller — which is the entire point of the policy.

Nor could the assertion be kept for one direction only. Every request is a round trip now, so during a shrink the parser is briefly *larger* than the area and during a grow briefly smaller. Both are transient, and both are indistinguishable from a real defect at a call site that cannot know whether an answer is in flight. An assertion there would either panic on ordinary operation or say nothing worth hearing.

**The invariant did not disappear; it moved to the side that can state it.** The daemon computes the applied geometry from its registered viewers, and "the PTY is never larger than any viewer's pane" is enforced and tested there. What remains in the widget is the rendering rule that follows from it — and `min(area, screen)` is now the **designed path** rather than a defense against an upstream failure. A pane narrower or shorter than its box is what a correctly applied policy looks like from inside a client that is not the smallest one.

`TerminalWidget::contract_guaranteed` survives as a parameter so existing callers and tests still compile, but no longer arms anything.

**The `PTY_RESIZE_DIM_MAX` cap is part of invariant 3's expectation, not an exception to it** (issue #747). A child PTY cannot be made larger than that cap — the daemon refuses on principle, since a same-uid peer on the attach socket could otherwise drive an agent to an absurd geometry — so on a terminal wider than the cap `resize_panes_to_layout` deliberately sizes the pane to the cap and the drawn area is legitimately larger. The pane then renders the child's full 4096 columns through the `min(area, screen)` fallback and leaves the rest blank, which is the honest outcome: the child has no more columns to show. `crate::agent_pty::clamp_pty_dims` is the single place that spells the cap, and invariant 2's `pane_target_dims` applies it so the layout target, the local vt100 parser, the `AttachRequest::Resize` on the wire and the child PTY all carry the same number. Clamping only at the far end — as the code did before #747 — left the parser rendering at one width while the child wrapped at another, silently.

**Enforced by:**

- `TerminalWidget::render` (`src/terminal_widget.rs`, ~`fn render`) — the 1:1 draw from the top-left, with `min(area, screen)` bounding the loop.
- `AgentPtyRegistry::effective_dims` / `apply_dims_locked` (`src/agent_pty.rs`) — where the geometry is actually decided, and the place to test that it never exceeds a viewer's pane.

### 4. Fixed, explicit resize sequencing

Within a single frame, the order is always:

1. **Compute layout** — `compute_frame_layout(...)`.
2. **Commit PTY resizes to match** — `resize_panes_to_layout(...)`, before `terminal.draw`.
3. **Render** — `render_frame` / `render_mode_tab` read from `FrameLayout`.

There is no path that renders before resizing, or resizes after rendering.

**Enforced by:**

- The call structure of the main loop in `src/ui.rs` — the (compute → resize → draw) order is hard-wired into the loop, not left to individual event handlers.
- `Event::Resize` (`src/ui.rs`, ~6503) is reduced to a **re-render trigger**: it breaks out to run another loop iteration, and the layout pass at the top of that iteration does the rest. It no longer pushes PTY dimensions itself.

## Convergence

Every trigger that changes the visible shape — terminal resize, tab open/close, mode switch, reactive pane recreation, orchestration role transition — converges to the same three steps:

```text
recompute layout  ->  resize PTYs to match  ->  render
```

That single convergence point is the whole contract. The earlier "everyone resizes their own panes" pattern is what this replaces.

## Out of scope / known caveats

- ~~**Stream-backed (daemon) panes have no PTY resize op.**~~ **Stale since PRD #76 M2.10 and corrected here.** `resize_pane_pty` handles stream-backed panes itself: it writes to a per-pane single-slot coalescing channel whose worker forwards an `AttachRequest::Resize` to the daemon. `resize_panes_to_layout` does not skip them — it drives every pane through the one primitive.
- **No new layout features.** No splittable panes, resizable splitters, or zoom mode. The layouts produced are exactly the ones the app already has.
- **No vt100/ratatui replacement.** The contract is about how *we* drive them, not about swapping them.

## Validation

The contract is measured against the M1 failure-mode catalog under `tests/` (one deterministic reproducer per known visual bug). The rule, from M5 onward: **if a reproducer still fails, fix the upstream code path — do not re-add the clamp.** A reproducer that can't be made to pass within the contract is signal that the contract has a hole, not that the widget needs another defensive heuristic.

## References

- PRD #84 — `prds/done/84-rendering-layer-rework.md` (Problem, Solution, Milestones).
- `src/ui.rs` — `render_frame`, `render_mode_tab`, the resize helpers, and the `Event::Resize` handler.
- `src/terminal_widget.rs` — `TerminalWidget::render` (the clamp + row window to be removed).
- `src/embedded_pane.rs` — `resize_pane_pty` (the one resize primitive).
