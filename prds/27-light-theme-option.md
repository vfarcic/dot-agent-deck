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

Remove forced backgrounds and auto-detect the terminal's theme to select the appropriate foreground color palette:

- **Remove all hardcoded `bg()` backgrounds** — let the terminal's native background show through, eliminating the visual mismatch with other panes
- **Auto-detect terminal theme** using `terminal-colorsaurus` (OSC 11 query) to determine if the terminal has a light or dark background
- **Keep accent ANSI colors unchanged** — semantic colors (Cyan, Green, Yellow, Red, Blue, Magenta) are already remapped by terminal themes and remain readable on both light and dark backgrounds
- **Switch only neutral text colors** — a small palette (~3-4 colors) that flips text/label colors between themes (White/Gray on dark, Black/DarkGray on light)
- **CLI/config override** — `--theme auto|light|dark` (default: `auto`) for cases where detection fails

## Scope

### In Scope
- Remove all forced `bg(Rgb(0,0,0))` background colors from rendering
- Auto-detection of terminal background color via `terminal-colorsaurus` crate
- CLI flag `--theme auto|light|dark` (default: `auto`)
- Config file option `theme: auto|light|dark`
- Small foreground-only color palette for neutral text colors (no background colors in palette)
- Accent/status ANSI colors kept as-is (terminal remaps them per-theme)
- Fallback to dark palette when auto-detection fails

### Out of Scope
- User-defined custom color palettes
- Per-element color configuration
- Runtime theme switching without restart

## Color Palette Design

### Colors That Stay the Same (both themes)
These ANSI accent colors are remapped by the terminal per-theme and need no switching:

| Element | Color |
|---------|-------|
| Dashboard title | Cyan (BOLD) |
| Selected border | Cyan (BOLD) |
| Status: working | Green |
| Status: thinking | Blue |
| Status: compacting | Magenta |
| Status: waiting | Yellow |
| Status: error | Red |
| Status: idle | Gray |

### Colors That Switch (palette)

| Element | Dark Terminal | Light Terminal |
|---------|-------------|---------------|
| Card titles | White (BOLD) | Black (BOLD) |
| Labels (Dir, Last, Tools) | Gray | DarkGray |
| Secondary text | DarkGray | Gray |
| Tool lines | DarkGray | DarkGray |

*Note: No background colors in either palette — the terminal's native background is used.*

*Note: Light palette colors are initial estimates — will need testing and iteration.*

## Success Criteria

- Dashboard has no forced background — blends naturally with terminal theme
- Dashboard is fully readable on both light and dark terminal themes
- Auto-detection correctly identifies theme on major terminals (Ghostty, kitty, iTerm2, Alacritty, VS Code)
- Theme can be overridden via CLI flag or config file when auto-detection fails
- Status indicators remain visually distinct on both themes
- No regression in dark theme appearance
- All existing tests passing with both themes

## Milestones

- [x] Remove all forced `bg()` background colors from `src/ui.rs`
- [x] Replace `Rgb(140,140,140)` with ANSI `DarkGray` color
- [x] Add `terminal-colorsaurus` dependency to `Cargo.toml`
- [x] Implement theme auto-detection on startup
- [x] Define small foreground-only palette struct with dark/light variants (`src/theme.rs`)
- [x] Add `--theme auto|light|dark` CLI flag and config file option
- [x] Thread palette through render functions for neutral text colors
- [ ] Test on 3+ terminal emulators with light themes
- [ ] Verify no regression on dark theme
- [x] All existing tests passing with both themes

## Key Files

- `src/ui.rs` — Color palette definitions and rendering (~10 `bg()` calls to remove, ~4 palette colors to thread)
- `src/main.rs` — CLI flag handling
- `src/config.rs` — Config file parsing
- `Cargo.toml` — Add `terminal-colorsaurus` dependency

## Technical Notes

- All colors are currently hardcoded inline in `src/ui.rs`
- Only ~3-4 neutral text colors need palette switching; accent colors stay as ANSI
- The palette struct is small — just the colors that need to flip between themes
- `terminal-colorsaurus` sends OSC 11 query, gets background RGB, calculates perceived lightness
- The `--theme` flag overrides auto-detection; `auto` is the default
- Use an enum (`Theme::Auto`, `Theme::Dark`, `Theme::Light`) with `Auto` resolving at startup

## Risks

- **tmux passthrough**: OSC 11 queries may not reliably pass through tmux — mitigated by defaulting to dark when detection fails and providing config override
- **Color tuning**: Light palette will require iterative testing across terminals
- **Maintenance**: Two palettes means color changes need updating in two places (but palette is small, ~4 colors)
- **Detection latency**: OSC 11 query requires a brief timeout (~20ms) on startup

## Decision Log

| Date | Decision | Rationale |
|------|----------|-----------|
| 2026-04-05 | Remove forced black backgrounds | Dashboard's forced `bg(Rgb(0,0,0))` creates visual mismatch with other panes in light terminals; terminal should control its own background |
| 2026-04-05 | No backgrounds in palettes | Neither dark nor light palette should set background colors — backgrounds are the terminal's responsibility |
| 2026-04-05 | Auto-detect via terminal-colorsaurus | Proven crate used by bat, delta, helix; OSC 11 query works on most modern terminals |
| 2026-04-05 | Keep accent ANSI colors as-is | Semantic colors (Cyan, Green, Yellow, Red, Blue, Magenta) are remapped by terminal themes and remain readable on both backgrounds |
| 2026-04-05 | Small foreground-only palette | Only text/label neutral colors (White, Gray, DarkGray) need to flip; accent colors adapt naturally |
| 2026-04-05 | --theme auto\|light\|dark (default: auto) | Auto-detection covers most users; override available for tmux/SSH edge cases |
| 2026-04-05 | Replace Rgb(140,140,140) with DarkGray | Eliminate last hardcoded RGB foreground color; use ANSI color that adapts to terminal theme |
| 2026-04-05 | Default to dark on detection failure | Safe fallback since most terminal users use dark themes; config override available |
