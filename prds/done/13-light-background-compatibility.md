# PRD #13: Light Terminal Background Compatibility

**Status**: Complete (Phase 1 + Phase 2)
**Priority**: Medium
**Created**: 2026-03-29
**GitHub Issue**: [#13](https://github.com/vfarcic/dot-agent-deck/issues/13)

> **Phasing.** Phase 1 (overlay/prompt neutral-text migration to the palette) is complete and lives in PR [#133](https://github.com/vfarcic/dot-agent-deck/pull/133). During Phase 1 review the user found the dashboard still renders a **black background on a light terminal**, which exposed a deeper architectural flaw than Phase 1 addressed. Phase 2 (below) re-scopes the PRD to fix the root cause by committing fully to a **terminal-relative** color model. The Phase 1 work remains valid but becomes a stepping stone — some of it is simplified or superseded by Phase 2.

## Problem Statement

The dashboard UI has only been tested and designed for dark/black terminal backgrounds. Users running terminals with light backgrounds (e.g., Solarized Light, macOS default Terminal with white background) likely experience poor contrast, unreadable text, or invisible UI elements. Key concerns:

- **White text on white background**: Card titles use `Color::White` with BOLD, which disappears on light backgrounds
- **Gray/DarkGray labels**: Directory, prompt, and secondary text use `Color::Gray` and `Color::DarkGray`, which have poor contrast on light backgrounds
- **Cyan accents**: Active selection borders and highlights may wash out on light themes
- **Custom RGB(140, 140, 140)**: Recent tool names use a medium gray that may be hard to read on light backgrounds
- **Green status (Idle)**: Light green on white is notoriously low-contrast

## Solution Overview

**Decided direction (Phase 2): a single, terminal-relative color model.** Every color the dashboard emits is expressed in the terminal's own frame of reference — `Color::Reset` for the canvas and body text, named ANSI colors (Cyan/Yellow/Red/…) for semantic accents — and **no absolute `Rgb(...)` value is painted on any contrast-critical surface**. The terminal's theme already defines a readable foreground/background pair; the dashboard inherits it instead of overriding it. This makes light/dark legibility correct *by construction* and removes the need for runtime background detection to keep the UI readable.

This supersedes the three options the PRD originally floated (detect-and-switch / adaptive named colors / config flag). The shipped Phase 1 code took a *hybrid* of those — adaptive ANSI foregrounds **plus** absolute-RGB backgrounds selected by OSC-11 detection — and that hybrid is exactly what produced the black-background bug (see Decision Log: "Reference-frame discipline"). Phase 2 collapses the hybrid into the single terminal-relative frame.

## Scope

### In Scope
- **(Phase 1, done)** Audit hardcoded colors in `src/ui.rs`; migrate overlay/prompt neutral text to the palette.
- **(Phase 2)** Remove the absolute-RGB background fill (`palette.terminal_bg`) from the dashboard frame and all overlays — render the canvas with `Color::Reset` so the terminal background shows through.
- **(Phase 2)** Convert remaining neutral foregrounds to `Color::Reset` (terminal default) rather than named/RGB grays where they exist only to be "default text."
- **(Phase 2)** Re-express the selection highlight and tab-bar distinction *without* absolute-RGB tints — use reverse-video (`Modifier::REVERSED`) or named ANSI brights so the highlight has a guaranteed contrast pair in one frame.
- **(Phase 2)** Audit beyond `src/ui.rs` — at minimum `src/terminal_widget.rs` and other render surfaces — for absolute-RGB-vs-relative contrast pairs.
- **(Phase 2)** Keep semantic status/accent colors as named ANSI (they already inherit the terminal theme).
- **(Phase 2)** Add a test/lint that *structurally* forbids the regression: no absolute background fill on the canvas, and no contrast-critical pair that mixes an absolute color with a terminal-relative one.

### Out of Scope
- Full theme/color customization system (user-defined color palettes).
- Support for 256-color or truecolor-only terminals (keep basic 16-color compatibility).
- **Pixel-perfect/branded RGB visuals.** Phase 2 deliberately trades custom RGB tints (e.g. the blue selected-card background) for terminal-native robustness. Reintroducing rich RGB would require the absolute-everything-plus-theming model rejected in the Decision Log.
- The OSC-11 auto-detection and `light`/`dark` `ColorPalette` split (`src/theme.rs`) are **no longer load-bearing for legibility** after Phase 2. Removing vs. retaining them (for optional cosmetic tuning only) is a follow-up cleanup decision, not required by this PRD.

## Current Color Inventory

| Component | Current Color | Risk on Light BG |
|-----------|--------------|-------------------|
| Card titles | White (BOLD) | High - invisible |
| Directory/prompt labels | Gray | High - poor contrast |
| Recent tool names | RGB(140,140,140) | Medium - low contrast |
| Dashboard title | Cyan (BOLD) | Medium - may wash out |
| Selected border | Cyan (BOLD) | Medium |
| Session count | Gray | High - poor contrast |
| Idle status | Green | Medium-High |
| Working status | Yellow | Medium |
| Error/NeedsInput | Red (BOLD) | Low - usually fine |
| Thinking status | Cyan | Medium |
| Compacting status | Blue | Low-Medium |

## Success Criteria

- All text and UI elements are clearly readable on both dark and light terminal backgrounds
- Status indicators remain visually distinct from each other on both themes
- No regression in dark background appearance
- Solution works without requiring user configuration **and without runtime background detection** — legibility is correct by construction
- **The dashboard never paints an absolute background; the terminal's own background shows through** (no black slab on a light terminal)
- **No contrast-critical pair mixes an absolute color with a terminal-relative one** — enforced by a structural test/lint, not convention

## Milestones

### Phase 1 — overlay/prompt neutral-text migration (complete, PR #133)

- [x] Audit complete: document every color usage in ui.rs *(code-level audit; verified zero remaining hardcoded `White`/`Gray`/`DarkGray`/RGB-gray neutral colors in `ui.rs`. Screenshots superseded by color-aware L1 snapshots — see Decision Log)*
- [x] Determine approach (with rationale) *(Phase 1 chose the hybrid adaptive-palette + OSC-11 detect model; **revised in Phase 2** — see Decision Log)*
- [x] Implement color adaptation for all high-risk elements (White, Gray, DarkGray text) *(palette threaded through `ui.rs`; final 15 hardcoded colors in 6 overlay/prompt fns migrated)*
- [x] Fix status indicator colors to be distinguishable on both backgrounds *(status accents kept as theme-remapped ANSI colors; verified distinct in dark+light snapshots)*
- [x] Fix overlay/popup readability (help screen, filter input, rename prompt) *(quit/stop/star/config-gen/stats migrated; help/filter/rename already palette-clean; pinned by 12 snapshots, no White-on-white on light bg)*
- [x] Verify no regression on dark backgrounds *(dark snapshots + full fast tier 592 passed; dark appearance preserved)*
- [x] Update any relevant documentation *(no user-facing docs reference dashboard colors; none needed)*

### Phase 2 — terminal-relative refactor (open)

- [x] Remove the absolute `palette.terminal_bg` fill from the dashboard frame and every overlay/prompt block — canvas now renders with `Color::Reset`; terminal background shows through.
- [x] Convert neutral foregrounds to terminal-relative styling via `text_primary()` (`Reset`) / `text_dim()` (`Reset`+`DIM`) helpers. *(Refined post-manual-check — see Decision Log "Secondary-text contrast".)*
- [x] Re-express the selected-card highlight without absolute RGB. *(Landed as Option A — see Decision Log "Selection cue"; active-tab uses `Modifier::REVERSED`.)*
- [x] Audit other render surfaces beyond `ui.rs` (`terminal_widget.rs`, embedded panes, tab layout) — only self-contained ANSI pairs + vt100 passthrough remain; no mixed-frame pairs.
- [x] Confirm semantic status/accent ANSI colors still read correctly on a `Reset` canvas on both themes *(reviewer + manual check)*.
- [x] Add a structural regression guard: `theme/guard/001` (render-time: no cell has an absolute `Color::Rgb` background; selection cue present) + `theme/guard/002` (source lint forbidding `bg(Color::Rgb…)`/`bg(palette.*_bg)`); `theme/contrast/001` repurposed; `theme/contrast/002` removed.
- [x] Decide the fate of OSC-11 detection + `light`/`dark` `ColorPalette` — **removed** (`src/theme.rs` deleted, `terminal-colorsaurus` dep dropped, `--theme` flag + `theme` config key removed). No longer load-bearing under the terminal-relative model.
- [x] Manually verify on a real light terminal that the canvas is no longer a black slab — **confirmed by the user** (canvas inherits terminal background; Option-A selection legible; secondary text readable in both modes after the contrast refinement).

### Integration note
- Merged `origin/main`'s **PRD-80 (mouse parity, M1–M10)** into this branch; all PRD-80 button/tab/modal surfaces were reconciled to the terminal-relative model (no absolute `Rgb` backgrounds) with no loss of mouse functionality. The test catalog relocation (`prds/done/77-…` → `tests/CATALOG.md`) introduced by PRD-80 was adopted; our `theme/*` specs live there.
- Enabled Greptile `triggerOnUpdates` (`greptile.json`) so PRs re-review on every push.

## Decision Log

- **Reference-frame discipline → commit fully to a terminal-relative model (Phase 2). [Primary decision]** Colors in a TUI live in one of two frames: *terminal-relative* (`Color::Reset` + named ANSI, resolved by the user's terminal theme) or *absolute* (`Rgb(...)`, fixed pixels). The Phase 1 / pre-existing code mixed the two **within a single contrast pair** — an absolute-black `terminal_bg` painted under terminal-relative `White` text — which is the root cause of the black-background-on-light-terminal bug: once a foreground and its background come from different frames, there is no contrast guarantee.

  Three paths were weighed:
  1. **Absolute everything, no theming** — rejected: bakes the bug in permanently for whichever background isn't hardcoded; only coherent as "dark-only," which defeats this PRD.
  2. **Absolute everything, *with* theming** — viable but heavy: needs reliable detection or a config flag, ignores the terminal theme the user already tuned, and looks foreign (forcing `#FFFFFF` onto a cream background).
  3. **Terminal-relative everything** — **chosen.** Readable on any terminal with zero detection and zero config; every contrast pair is automatically in-frame; matches the standard TUI idiom (lazygit/gitui/helix). The accepted cost is giving up custom RGB tints (selection becomes reverse-video / ANSI brights rather than a subtle blue).

  A *disciplined* mix (every contrast pair self-contained in one frame) is technically safe but rejected as the default because the discipline lived only in reviewers' heads — nothing structural prevented the regression, and indeed no test caught it. Phase 2 therefore also adds a structural guard. **Consequence:** the OSC-11 detection and the `light`/`dark` palette split stop being load-bearing for legibility (their removal/retention is a follow-up).

- **Why Phase 1's tests did not catch the bug.** The two L1 contrast tests (`theme/contrast/001–002`) (a) rendered only the six overlay/prompt surfaces, never the top-level dashboard frame where `terminal_bg` is filled, and (b) *injected* an explicit palette (`resolve_palette(Theme::Dark/Light)`), bypassing the `Theme::Auto → detect_palette()` path that actually selects the palette at runtime. So neither the offending surface nor the offending code path was under test. Phase 2's structural guard targets exactly this gap.

- **Selection cue = Option A (post-manual-check refinement).** The first terminal-relative attempt highlighted the selected card by applying `Modifier::REVERSED` to the *whole card*, which fully inverted it to a solid opposite-of-theme block — too heavy, and redundant since selection is already signalled by the `▸ ` title prefix and a `Cyan`+`BOLD` border. Resolved by **dropping the whole-card `REVERSED`** and keeping the prefix + cyan-bold border (still fully terminal-relative). The active-*tab* `REVERSED` highlight is retained (a single line, where a strong invert reads well). `guard_001` was re-expressed to assert the cyan-bold border cue instead of `REVERSED`.

- **Secondary-text contrast (post-manual-check refinement).** In pure terminal-relative 16-color space the only "dimmer" knob is `Modifier::DIM`, but on the user's terminal `DIM` secondary text (`text_dim()`) was hard to read in dark mode and worse in light (a long-standing legibility issue, not new to this PRD). Resolved by reserving `DIM` for purely-decorative, non-read elements only (the `│` separators, unfocused-input / disabled-button / placeholder borders) and rendering all actual *text* (labels, hints, footers, values, "No agent", messages) at full contrast via `text_primary()`. Visual hierarchy now comes from wording/layout + the few remaining decorative dims, not from faint text. (Trade-off accepted: there is no readable terminal-relative "mid-gray"; full contrast wins over a faint hierarchy.)

- **Validation approach: L1 structural assertions instead of screenshots + manual emulator matrix.** The PRD predates the PRD-77 TUI testing harness. Rather than ad-hoc screenshots and manual Terminal.app/iTerm2/Alacritty runs, the contract is pinned by deterministic, CI-runnable (`cargo test-fast`) L1 tests. *(Phase 1 originally used a dark/light snapshot **pair** `theme/contrast/001`+`002`; Phase 2 replaced that with the terminal-relative model below, since light/dark no longer produce different buffers.)* The current guards are: `theme/guard/001` (render-time: no rendered cell carries an absolute `Color::Rgb(..)` background; selection uses `Modifier::REVERSED`), `theme/guard/002` (source lint: no `bg(Color::Rgb…)` / `bg(palette.*_bg)` in `ui.rs`), and `theme/contrast/001` (overlays emit a `Reset` background + `Reset`/ANSI foregrounds). Residual gap vs. a true terminal: L1 inspects the ratatui buffer, not a real emulator's ANSI rendering — closed by the milestone-8 manual check on a real light terminal.
- **Cyan/accent colors left hardcoded by design.** ANSI accent colors (Cyan title/borders, status colors) are remapped by the terminal's own theme, so they adapt without intervention; only neutral text (White/Gray/DarkGray) needed palette routing.

## Technical Notes

- All colors are currently hardcoded inline in `src/ui.rs` (~1400+ lines)
- ratatui's `Color::Reset` uses the terminal's default foreground/background, which adapts to theme
- The `termbg` crate can detect terminal background color at runtime
- crossterm (already a dependency) has some terminal query capabilities
- A simple approach: replace `Color::White` with `Color::Reset` for text that should use the terminal's default foreground

## Risks

- **Terminal detection unreliable**: Not all terminals support background color queries; need a fallback
- **Color semantics shift**: What looks like a warning (yellow) on dark may look different on light
- **Testing matrix**: Many terminal emulators with different color interpretations
