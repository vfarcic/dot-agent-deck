# PRD #155: Centralized color palette for consistent deck and pane theming

**Status**: Implementation complete — pending M6 visual verification (at PR review)
**Priority**: Medium
**Created**: 2026-06-14
**GitHub Issue**: [#155](https://github.com/vfarcic/dot-agent-deck/issues/155)

## Problem Statement

Colors in the TUI are scattered as inline `Color::X` literals across `src/ui.rs` (dozens of usages), with no single source of truth. Worse, the two deck-bearing surfaces use *inconsistent* schemes:

- **Dashboard deck cards**: the border color encodes the **agent's status** — green = working, blue = thinking, yellow = waiting, red = error, idle = dimmed — and the *selected* card additionally gets a Cyan (bold) border plus a `▸` title marker.
- **Embedded panes / agents**: the border color encodes **focus** — cyan when focused, dimmed otherwise.

So the same underlying state can look different depending on whether it is rendered as a deck card or as an embedded pane, and adding/auditing colors means hunting through scattered literals. This surfaced during PRD #113 manual testing, where deck-vs-pane rendering divergence repeatedly caused confusion.

## Solution Overview

Introduce a small, centralized set of **named semantic color roles** (global constants / a palette), make them the single source of truth, and apply **one consistent scheme** across both the dashboard decks and the embedded panes/agents so a given state looks the same in both places.

This requires explicitly **defining the role set and what each context's border conveys**, resolving the status-vs-focus-vs-selection overlap. Key constraint discovered in discussion: **green already means "working"**, so it cannot also mean "selected" — selection/focus need roles distinct from the status palette.

### Key design decision (to finalize in the plan phase)

The card border currently encodes **status**; the pane border encodes **focus**. A unified scheme must decide what each context's border shows. Candidate approaches:

- **A (recommended default)**: Border = **status** in *both* contexts (decks and panes), keeping the rich at-a-glance status signal everywhere. **Selection** and **focus** are conveyed by **non-status cues** that don't collide with the status palette — e.g. selection = the `▸` marker + a distinct accent role (not green), focus = a distinct accent (cyan retained as the `focused` role). Status colors are never reused for selection/focus.
- **B**: Border = **focus/selection** in both contexts; status shown only via the per-card status badge (not the border). Simpler border semantics, but drops status-on-border.
- **C**: Layered — border = status (both contexts), with separate, clearly-distinct `selected` and `focused` accent roles defined so the three axes never visually collide.

The PRD plan phase will lock one of these (A/C lean) before implementation. Whatever is chosen, the per-card **status badge** remains the authoritative status indicator, so any border-policy change does not lose status information.

## Scope

### In Scope
- A centralized palette: named semantic color roles as global vars / a small module (one source of truth).
- A defined role set: e.g. `status_working`, `status_thinking`, `status_waiting`, `status_error`, `idle`, `selected`, `focused` (final names/set decided in plan phase).
- Replacing the scattered inline `Color::X` usages in the deck and pane render paths with the named roles.
- Applying the *same* scheme consistently to dashboard decks **and** embedded panes/agents.
- Resolving the status/focus/selection overlap so no role collides (notably: selection/focus must not reuse the status green).
- Keeping the existing theme guards green (`theme/guard/001`, `theme/guard/002` — no absolute backgrounds; `theme/contrast/001`), extending them if useful (e.g. a guard that source uses palette roles, not raw `Color::X`, in the render paths).

### Out of Scope
- Any behavioral / selection-state logic (highlight activation, Enter-restore, tab-switch clearing) — owned by PRD #113.
- Absolute background fills — still forbidden by the theme guards; this PRD is foreground/border colors only.
- A full user-configurable theming system / multiple themes (could be a later PRD) — this is about one consistent built-in palette.

## Key Files

| File | Change |
|------|--------|
| `src/ui.rs` | Replace inline `Color::X` in the deck-card and pane render paths with named palette roles |
| `src/<palette module>` (new, or a section) | Define the semantic color roles / global vars |
| `tests/render_dashboard.rs` | Theme guard/contrast coverage; render snapshots for deck + pane coloring per role |

## Milestones

- [x] **M1 — Role set + border policy decided**: **Option A** locked — border encodes status in both decks and panes; selection = Magenta + `▸` + BOLD, focus = Cyan, status = Green/Blue/Yellow/Red/DarkGray. Selection/focus roles are distinct from the status palette (no green collision). Documented in `.dot-agent-deck/prd-155-plan.md`.
- [x] **M2 — Central palette in place**: New `src/palette.rs` defines the named roles (`STATUS_WORKING/THINKING/WAITING/ERROR/IDLE`, `FOCUSED`, `SELECTED`) + `status_color()` as the single source of truth; the deck-card render path uses them instead of inline `Color::X` (commit `932cc59`).
- [x] **M3 — Panes use the same scheme**: `TerminalWidget` gained a status-aware border (`with_status`), and `render_frame` builds `build_pane_status` and threads it through `render_terminal_panes`/`render_mode_tab`, so a given state looks identical in decks and panes (commits `932cc59`, `e0fb794`).
- [x] **M4 — Guards hold (and tighten)**: `theme/guard/001-002` + `theme/contrast/001` remain green; added `theme/guard/003` source-lint asserting the deck/pane render paths (incl. `render_stats_bar`) use palette roles, not raw status `Color::X` (commits `5feb78b`, `3ec8cac`).
- [x] **M5 — Tests**: L1 render coverage (`theme/palette/001-004`) asserts deck and pane coloring per role and that selection/focus/status are visually distinct; `build_pane_status` unit test guards the M3 join; `test_status_style` updated to the locked mapping. Fast tier **758/758**, E2E **1119/1119** green; no snapshot churn.
- [ ] **M6 — Visual verification**: Run the app (`run-dot-agent-deck`) and confirm decks and panes are visually consistent across states; update any user-facing docs if applicable. *(Deferred to PR review — the production `render_frame` pane path has no daemon-free L1 seam, so this manual check is load-bearing.)*

## Success Criteria

1. There is one source of truth for the TUI's semantic colors (named roles), with no inline `Color::X` in the deck/pane render paths.
2. A given state (e.g. a working agent, a selected deck, a focused pane) renders with the *same* color in both the dashboard deck and the embedded-pane contexts.
3. Selection, focus, and status are each visually distinguishable — no role reuses another's color (in particular, "selected"/"focused" do not reuse the working-status green).
4. The theme guards (`theme/guard/001-002`, `theme/contrast/001`) remain green.
5. The per-card status badge continues to convey status regardless of the chosen border policy.

## Notes

- Originated from a live design discussion during PRD #113 (deck-selection highlight + orchestration-identity work, PR #151). Separate from that PR by design — this is a theming/consistency refactor, not selection-behavior.
- Related follow-ups discussed but tracked separately: deeper deck **state-model unification** (`selected_session_id` vs `focused_role_pane_id`), orchestration **unique-instance ID** robustness, and orchestration display-title persistence across daemon reconnect.
- The hardest part is the design decision (what each border conveys), not the mechanical extraction — M1 gates the rest.
