# PRD #80: Mouse Parity for Keyboard Actions

**Status**: Not started
**Priority**: Medium
**Created**: 2026-05-10
**GitHub Issue**: [#80](https://github.com/vfarcic/dot-agent-deck/issues/80)

## Problem

Most TUI actions in dot-agent-deck are reachable only through keyboard shortcuts. The mouse already works for a few interactions inherited from the underlying terminal — clicking a side pane to focus it, scrolling, text selection, Ctrl+click on a hyperlink — but every other action (creating a pane, closing a pane, switching tabs, toggling layout, generating config, renaming a session, navigating cards, dismissing a modal, picking a directory, filling out the new-pane form) requires the user to know the right keystroke.

The full keyboard surface is documented in the `?` help overlay (the canonical render site is the event loop in `src/ui.rs`), but discovery is gated behind already knowing about `?`. For new users, mouse-first users, or anyone reaching for the trackpad mid-flow, there is no on-screen affordance to fall back to.

The only on-screen hints today are scattered status-bar legends like `Ctrl+C: quit` — they work as cues but do nothing when clicked.

## Solution

Add a visible, clickable affordance for every keyboard-only action, distributed across the existing UI surfaces (status/button bar, tab strip, dashboard cards, modal dialogs, pickers, forms). Each affordance carries its keyboard shortcut inline — e.g., `[New Pane Ctrl+N]` — so it doubles as a built-in legend for users learning the keyboard set.

This unifies three things into one piece of UI:
- **Mouse parity** — every action reachable by click.
- **Discoverability** — actions visible without opening `?`.
- **Keyboard education** — the inline shortcut on each button trains the user toward keyboard productivity (the user's stated end state).

Existing mouse interactions (click-to-focus pane, scroll, text selection, Ctrl+click links, child-app mouse forwarding) are preserved as-is. The hit-testing infrastructure already in place (`focused_pane_rect`, `side_pane_rects`, `agent_pane_rect`, `last_click`) is the same pattern we extend for buttons.

### Design decisions (from discussion)

- **Buttons carry their shortcut inline.** `[New Pane Ctrl+N]`, not `[New Pane]`. The user values keyboard productivity and wants the UI itself to teach the shortcuts. The cleaner "button-only" alternative was rejected in favor of this teaching effect.
- **Existing legends collapse into buttons.** Today's status-bar text cues (`Ctrl+C: quit`, etc.) get replaced by clickable buttons that show the same shortcut. No duplication between legend text and button.
- **Text input stays keyboard.** Inline edit boxes (filter, rename, new-pane form fields) accept clicks for cursor placement / focusing the field, but the typing itself is keyboard. Submit / Cancel become explicit buttons.
- **No drag-to-reorder, no hover tooltips.** The shortcut is already inline; tooltips would duplicate. Drag is a different interaction model and is out of scope.
- **Right-click and middle-click stay unused** at the TUI layer. The terminal emulator's own selection / paste behavior is unchanged. Avoids fighting the user's terminal.
- **Modals get explicit Yes/No/Cancel buttons alongside the existing selection list.** Keystrokes continue to drive the highlighted selection and Enter still confirms — buttons are an additional path, not a replacement.
- **Click and key dispatch share one action layer.** Both funnel into the same per-action function; no parallel implementations. This makes the parity claim self-evident in code.
- **Per-region rollout.** The work decomposes into independent regions (global bar, tab strip, dashboard, mode tab, modals, pickers, form). Each lands and validates independently — no global UI rewrite.

## Milestones

Region-based; each is independently shippable and validated.

- [ ] **M1 — Action layer + button widget foundation.** Refactor every keyboard-only action into a single dispatch table (`fn dispatch_action(Action)`); the existing keystroke handler becomes a thin keystroke→Action mapper. Introduce a `Button` widget (label, shortcut, action, enabled) with a render+hit-test pair that follows the existing `*_rect` pattern. No behavioral change yet; the foundation tests prove key/click both invoke `dispatch_action`.
- [ ] **M2 — Global button bar.** Persistent bottom-row button bar in every UI mode exposing New Pane, Close, Toggle Layout, Help, Quit. Removes redundant status-bar legend text. Includes the narrow-terminal fallback (shortcut-only labels).
- [ ] **M3 — Tab strip clicks.** Click-to-switch on every tab header (Dashboard, Mode, Orchestration). Per-mode-tab `[×]` close affordance reusing Ctrl+W's existing semantics. Dashboard tab has no close.
- [ ] **M4 — Dashboard mouse parity.** Card click-to-select / double-click-to-focus. Clickable Filter / Rename / Generate-config buttons. Existing `?`/Tab/Shift+Tab/keystrokes continue to work.
- [ ] **M5 — Modal mouse parity.** Quit-confirm, config-gen, star-prompt, and help overlay each gain explicit clickable buttons (`[Quit][Detach][Cancel]`, `[Yes][No][Never]`, `[Star][Snooze][Dismiss]`, `[Close]`). Keystrokes preserved.
- [ ] **M6 — Inline edit (filter / rename) Apply/Cancel buttons.** Click in the field to focus; explicit `[Apply]`/`[Save]` and `[Cancel]` buttons next to the input. Enter/Esc preserved. Also adds the PaneInput-mode `[Detach Ctrl+D]` affordance.
- [ ] **M7 — Directory picker mouse parity.** Clickable rows (single = select, double = enter), clickable parent / breadcrumb, `[Confirm]`/`[Cancel]` buttons, clickable filter entry.
- [ ] **M8 — New-pane form mouse parity.** Click-to-focus fields, clickable mode chips, `[Submit]`/`[Cancel]` buttons. Tab/Shift+Tab preserved.
- [ ] **M9 — Tests.** Coverage for click→action and key→action paths through `dispatch_action`. Coverage that existing mouse behavior (pane focus, scroll, text selection, hyperlinks, child-app forwarding) is preserved. Coverage that buttons short-circuit the click before falling through to pane logic. (Depends on PRD #77 if its harness lands first; otherwise reuses current test infrastructure.)
- [ ] **M10 — Help overlay refresh + docs.** Update `?` overlay content to match the post-button-bar shortcut set (overlay remains the canonical reference). Update any user-facing docs / screenshots impacted by the new bar (coordinate with PRD #51).

## Acceptance Criteria

### Global controls (always visible)
- [ ] A persistent button bar exposes the global commands: New Pane, Close, Toggle Layout, Help, Quit.
- [ ] Each button label shows its keyboard shortcut inline (e.g., `[New Pane Ctrl+N]`).
- [ ] Clicking each button triggers the same action as the corresponding keystroke.
- [ ] All keystrokes (Ctrl+N/W/T/C, ?) continue to work unchanged.

### Tab strip
- [ ] Clicking a tab header switches to that tab — equivalent to Tab / Shift+Tab / Ctrl+PageDown / Ctrl+PageUp.
- [ ] Mode and Orchestration tabs carry a clickable close affordance (e.g., `[×]`) — equivalent to Ctrl+W on the tab. Dashboard tab has no close affordance.
- [ ] Tab / Shift+Tab / Ctrl+PageDown / Ctrl+PageUp continue to work unchanged.

### Dashboard / Normal mode
- [ ] Clicking a session card moves selection to that card — equivalent to j/k navigation.
- [ ] Double-clicking a session card focuses the underlying pane (enters PaneInput) — equivalent to Enter on a selected card.
- [ ] A clickable Filter button enters filter mode (equivalent to `/`).
- [ ] A clickable Rename button on a selected card enters rename mode (equivalent to `r`).
- [ ] A clickable Generate-config button triggers the config-gen prompt (equivalent to `g`).
- [ ] j/k/1-9/Enter/r/g/`/` keystrokes continue to work unchanged.

### Mode tab (in-tab navigation)
- [ ] Existing click-to-focus side pane / agent pane behavior is preserved unchanged.
- [ ] Ctrl+D (exit PaneInput) is exposed as a clickable affordance (e.g., a `[Detach Ctrl+D]` button visible while in PaneInput).
- [ ] Existing scroll forwarding to child apps with `mouse_mode_enabled` is preserved unchanged.

### Modal dialogs
- [ ] Quit-confirm modal exposes clickable `[Quit]`, `[Detach]`, `[Cancel]` buttons. j/k/Enter/Esc still work.
- [ ] Config-gen modal exposes clickable `[Yes]`, `[No]`, `[Never]` buttons. Keystrokes still work.
- [ ] Star-prompt exposes clickable `[Star]`, `[Snooze]`, `[Dismiss]` buttons. `s`/`l`/`d` still work.
- [ ] Help overlay (`?`) has a clickable close button. `?`/`q`/`Esc` still work.

### Inline edit (filter / rename)
- [ ] Filter row exposes clickable `[Apply]` and `[Cancel]` buttons. Enter/Esc still work.
- [ ] Rename row exposes clickable `[Save]` and `[Cancel]` buttons. Enter/Esc still work.
- [ ] Clicking inside the text field places the cursor / focuses the field (typing remains keyboard).

### Directory picker
- [ ] Each directory row is clickable: single-click selects, double-click enters.
- [ ] A breadcrumb / parent-directory affordance is clickable (equivalent to `h` / Backspace / Left).
- [ ] `[Confirm]` and `[Cancel]` buttons are clickable. Space / q / Esc still work.
- [ ] A clickable Filter button or input opens the picker filter (equivalent to `/`).

### New-pane form
- [ ] Each form field is clickable to receive focus. Tab / Shift+Tab still work.
- [ ] The mode chip selector exposes clickable chips (equivalent to Left / Right / h / l).
- [ ] `[Submit]` and `[Cancel]` buttons are clickable. Enter / Esc still work.

### Visual / layout
- [ ] Buttons render with consistent styling across all surfaces.
- [ ] On terminals narrow enough that the full button bar doesn't fit, button labels degrade gracefully (shortcut-only fallback) — no truncation that makes a button unidentifiable.
- [ ] Existing status-bar legend text is removed where a button now exposes the same action. No duplication.

### Preservation of existing mouse behavior
- [ ] Click-to-focus side pane / agent pane in mode tabs is preserved.
- [ ] Scroll (and SGR-mode forwarding to child apps) is preserved.
- [ ] Text selection (single-click drag, double-click word, triple-click paragraph) and OSC 52 copy is preserved.
- [ ] Ctrl+click hyperlink opening is preserved.
- [ ] Buttons are hit-tested before pane-region logic; a click that misses every button falls through to the existing pane / selection path.

### Keystroke preservation
- [ ] No keystroke is removed; keyboard-only operation remains complete.
- [ ] The `?` help overlay reflects the same shortcut set after the change.

## Out of Scope

- **Customizable button bar / button order.** The set is fixed by what the global keystrokes cover.
- **Customizable shortcuts.** That's PRD #40 (Customizable Keybindings). Buttons will reflect whatever shortcut is bound at render time once #40 lands.
- **Drag-to-reorder panes / cards.** Distinct interaction model, not in scope.
- **Hover tooltips.** Inline shortcut text already serves the hint purpose.
- **Right-click context menus.** Stays unused at the TUI layer to avoid colliding with terminal-emulator behavior.
- **Touch gestures.** Not a TUI concern.
- **New actions** that don't already have a keyboard shortcut. This PRD is parity-only; new actions go in their own PRD.
- **Reworking the help overlay layout.** Update content; keep the format.

## Risks & Mitigations

| Risk | Mitigation |
|------|------------|
| Visual clutter — a permanent button bar competes for vertical real estate already pressured by panes, cards, status text. | Single-row bar replacing the existing status legend (net zero or near-zero height delta). On narrow terminals, fall back to shortcut-only labels. |
| Narrow-terminal width pressure — full labels like `[New Pane Ctrl+N]` don't fit. | Two-tier label: full text when there's room, shortcut-only (`[Ctrl+N]`) when there isn't. Width pressure is the same problem the existing status legend already has; reuse the truncation approach. |
| Regression in existing mouse handling (click-to-focus, scroll, text selection, child-app forwarding). | Hit-test order: button rects checked first; misses fall through to existing logic. Tests cover both paths and the preservation cases explicitly. |
| Modal Yes/No buttons confuse keyboard users who expect the existing j/k Enter flow. | Buttons render alongside the existing selection list, not instead of it. Keystroke flow is unchanged. |
| Tab close `[×]` is easy to mis-click and triggers Ctrl+W's existing destructive close. | Reuse Ctrl+W's existing semantics as-is. If Ctrl+W gains a confirm step in the future, the click inherits it through the shared `dispatch_action`. |
| Removing legend text breaks muscle memory for users who relied on `Ctrl+C: quit` always being visible. | The button shows the same `Ctrl+C` inline, so the cue moves rather than disappears. |
| `?` help overlay drifts out of sync with the set of buttons. | Both the overlay and the button bar render off the same source-of-truth keymap / action table. Single table walked twice. |
| Forwarding scroll to child apps must keep working — buttons must not eat scroll events that should pass through. | Buttons hit-test only on Down/Up clicks. Scroll events bypass the button layer entirely; the existing forwarding branch is untouched. |
| Click and key paths drift apart over time, eroding parity. | Both go through `dispatch_action(Action)`. New actions add a single entry; the next added action either has both paths or neither. Lint / test guards the dispatch table. |

## Implementation Notes

- The hit-testing pattern is established in `src/ui.rs`: `focused_pane_rect`, `side_pane_rects`, `agent_pane_rect`, populated each render and consulted in the mouse-event branch. Extend with a `button_rects: Vec<(Action, Rect)>` (and per-modal equivalents) populated during render.
- Introduce a small `Button` widget — label, shortcut, `Action`, enabled flag. Render and hit-test live next to each other so they cannot drift.
- A central `dispatch_action(Action)` is the single funnel. The current keystroke branch in the event loop becomes a `KeyEvent → Option<Action>` mapper; mouse Down on a button rect produces the same `Action`. New actions add a single dispatch entry.
- The button bar is a single row at the bottom of the screen, replacing the existing status legend. Rendered in every UI mode. Modal-specific buttons render inside their respective modals.
- Modal Yes/No/Cancel buttons render alongside the existing highlighted selection-list — they do not replace it.
- Tab close `[×]` glyph: render at the right edge of each Mode and Orchestration tab in `tab_bar_labels`, with rects tracked in a `tab_close_rects: Vec<(TabId, Rect)>`. Dashboard tab is excluded.
- Inline-edit Apply/Save/Cancel buttons render at the right edge of the input row, in the same modal/overlay region as the input itself.
- Existing scroll, text selection, multi-click, hyperlink, and child-app mouse forwarding stays in its current branch. Button hit-tests run first and short-circuit only on a hit.
- Overall change is mostly additive in `src/ui.rs`; no protocol or state-shape changes. The action table is the highest-leverage shared structure.

## References

- `src/ui.rs` — current event-loop branch including all keyboard and mouse handling (`focused_pane_rect`, `side_pane_rects`, `agent_pane_rect`, `last_click`, `tab_bar_labels`)
- `src/main.rs` — terminal init: `EnableMouseCapture`, `EnableBracketedPaste`
- PRD #40 — Customizable Keybindings (relationship: button labels resolve through whatever keymap is active once #40 lands)
- PRD #51 — Enhance docs with screenshots (relationship: docs/screenshots refresh after this lands)
- PRD #68 — Fix card navigation keys (relationship: card click semantics should match the corrected nav semantics)
- PRD #77 — TUI testing harness (relationship: mouse/click event tests benefit from the harness once available)
