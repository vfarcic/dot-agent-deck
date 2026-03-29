# PRD #13: Light Terminal Background Compatibility

**Status**: Draft
**Priority**: Medium
**Created**: 2026-03-29
**GitHub Issue**: [#13](https://github.com/vfarcic/dot-agent-deck/issues/13)

## Problem Statement

The dashboard UI has only been tested and designed for dark/black terminal backgrounds. Users running terminals with light backgrounds (e.g., Solarized Light, macOS default Terminal with white background) likely experience poor contrast, unreadable text, or invisible UI elements. Key concerns:

- **White text on white background**: Card titles use `Color::White` with BOLD, which disappears on light backgrounds
- **Gray/DarkGray labels**: Directory, prompt, and secondary text use `Color::Gray` and `Color::DarkGray`, which have poor contrast on light backgrounds
- **Cyan accents**: Active selection borders and highlights may wash out on light themes
- **Custom RGB(140, 140, 140)**: Recent tool names use a medium gray that may be hard to read on light backgrounds
- **Green status (Idle)**: Light green on white is notoriously low-contrast

## Solution Overview

Audit all color usage in the dashboard, test against light terminal backgrounds, and fix any readability issues. The approach should either:

1. **Detect terminal background** and switch color palettes accordingly, or
2. **Use adaptive colors** (e.g., ratatui's named colors that respect terminal theme), or
3. **Add a config option** for light/dark theme selection

## Scope

### In Scope
- Audit all hardcoded colors in `src/ui.rs`
- Test rendering on at least 2 light terminal themes
- Fix contrast issues for all UI elements (titles, labels, borders, status indicators, overlays)
- Ensure status colors remain distinguishable on both light and dark backgrounds

### Out of Scope
- Full theme/color customization system (user-defined color palettes)
- Support for 256-color or truecolor-only terminals (keep basic 16-color compatibility)

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
- Solution works without requiring user configuration (auto-detection preferred)

## Milestones

- [ ] Audit complete: document every color usage in ui.rs with screenshots on light background
- [ ] Determine approach: auto-detection vs adaptive colors vs config option (with rationale)
- [ ] Implement color adaptation for all high-risk elements (White, Gray, DarkGray text)
- [ ] Fix status indicator colors to be distinguishable on both backgrounds
- [ ] Fix overlay/popup readability (help screen, filter input, rename prompt)
- [ ] Test on at least 3 terminal emulators with light themes (Terminal.app, iTerm2, Alacritty)
- [ ] Verify no regression on dark backgrounds
- [ ] Update any relevant documentation

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
