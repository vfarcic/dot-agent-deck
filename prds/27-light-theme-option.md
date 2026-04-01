# PRD #27: Light Theme Option for Dashboard

**Status**: Draft
**Priority**: Low
**Created**: 2026-04-01
**GitHub Issue**: [#27](https://github.com/vfarcic/dot-agent-deck/issues/27)

## Problem Statement

The dashboard forces a black background to ensure all existing dark-optimized colors (White, Gray, DarkGray, Cyan, etc.) remain readable regardless of terminal theme. This works well but creates a visual mismatch for users running light terminal themes — the dashboard pane appears as a black rectangle next to light-themed agent panes.

## Background

PRD #13 identified that hardcoded colors (White, Gray, DarkGray) were unreadable on light terminal backgrounds. Rather than adopting adaptive colors (which made everything the same color and lost visual hierarchy) or runtime detection (unreliable across terminals), the decision was to force a black background and defer a proper light theme to a separate effort.

Key findings from PRD #13 investigation:
- `Color::DarkGray` is nearly invisible on dark backgrounds (commit 6f8a0db)
- `Color::White` and `Color::Gray` are invisible on light backgrounds
- `Color::Reset` makes all text the same color, losing visual hierarchy
- ANSI semantic colors (Red, Green, Yellow, Cyan) are remapped by terminals per-theme, but brightness-specific colors (White, Gray, DarkGray) are not

## Solution Overview

Add a theme option (`--theme light|dark`) that switches between two hardcoded color palettes:

- **Dark (default)**: Black background with current colors (White, Gray, DarkGray, Cyan, etc.)
- **Light**: White background with dark-optimized equivalents (Black text for titles, DarkGray for labels, darker status colors)

## Scope

### In Scope
- CLI flag `--theme light|dark` (default: `dark`)
- Config file option `theme: light|dark`
- Two hardcoded color palettes (dark and light)
- Light palette with white/light background and dark text colors
- All UI elements readable on both themes

### Out of Scope
- Auto-detection of terminal background color
- User-defined custom color palettes
- Per-element color configuration
- Runtime theme switching (requires restart)

## Color Palette Design

### Dark Theme (current, default)
| Element | Color |
|---------|-------|
| Background | Black |
| Card titles | White (BOLD) |
| Labels (Dir, Last, Tools) | Gray |
| Secondary text | DarkGray |
| Tool lines | RGB(140,140,140) |
| Dashboard title | Cyan (BOLD) |
| Selected border | Cyan (BOLD) |

### Light Theme (new)
| Element | Color |
|---------|-------|
| Background | White |
| Card titles | Black (BOLD) |
| Labels (Dir, Last, Tools) | DarkGray |
| Secondary text | Gray |
| Tool lines | DarkGray |
| Dashboard title | Blue (BOLD) |
| Selected border | Blue (BOLD) |

*Note: Light palette colors are initial estimates — will need testing and iteration.*

## Success Criteria

- Dashboard is fully readable on both light and dark themes
- Theme can be set via CLI flag or config file
- Default behavior (dark) is unchanged for existing users
- Status indicators remain visually distinct on both themes
- No regression in dark theme appearance

## Milestones

- [ ] Define theme data structure and two color palettes (`src/ui.rs`)
- [ ] Add `--theme` CLI flag and config file option
- [ ] Thread theme through render functions to use palette colors instead of hardcoded values
- [ ] Test light theme on 3+ terminal emulators with light themes
- [ ] Verify no regression on dark theme
- [ ] All existing tests passing with both themes

## Key Files

- `src/ui.rs` — Color palette definitions and rendering
- `src/main.rs` — CLI flag handling
- `src/config.rs` — Config file parsing

## Technical Notes

- All colors are currently hardcoded inline in `src/ui.rs`
- A theme struct with named color fields would replace inline `Color::*` references
- The `--theme` flag should override the config file value
- Consider using an enum (`Theme::Dark`, `Theme::Light`) passed into render functions

## Risks

- **Color tuning**: Light palette will require iterative testing across terminals
- **Maintenance**: Two palettes means color changes need updating in two places
- **Scope creep**: Resist adding auto-detection or custom themes in this PRD
