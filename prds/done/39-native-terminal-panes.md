# PRD #39: Native Terminal Panes (Replace Zellij)

**Status**: Complete (2026-04-05)
**Priority**: High
**Created**: 2026-04-04

## Problem

dot-agent-deck depends on Zellij as an external terminal multiplexer. This causes five categories of friction discovered during PRD 34 (Extensible Modes System):

1. **External dependency** — users must install Zellij separately (`brew install zellij` or `cargo install zellij`). dot-agent-deck cannot be distributed as a single binary.
2. **Tab-bar visibility** — Zellij's tab-bar is a layout plugin that only appears in tabs created with a layout that includes it. The dashboard tab (created by the session) and mode tabs (created dynamically) have inconsistent tab-bar visibility.
3. **Keybinding conflicts** — `Alt+N` is used by our dashboard for card selection, by Zellij for tab switching, and by terminal emulators (Ghostty, iTerm) for their own features. No clean non-conflicting scheme exists across all three layers.
4. **Layout control limitations** — Zellij's `new-pane` splits the focused pane, not where we want. Achieving a 50/50 agent + side panes layout required the `--direction` flag workaround with careful ordering.
5. **Pane collapse on command exit** — when a command exits (e.g., `kubectl` with no cluster), Zellij collapses the pane to a thin status bar. Required workaround: create shell panes and send commands via `write_to_pane`.

## Solution

Replace Zellij entirely with **Ratatui-native embedded terminal panes**. Each agent pane becomes an embedded terminal widget rendered directly in the Ratatui TUI. The architecture:

1. **`portable-pty`** — spawns shell processes with pseudo-terminals (PTYs). Cross-platform (macOS, Linux).
2. **VT parser** (`alacritty_terminal` or `vt100` crate) — parses ANSI escape sequences from PTY output into a grid of styled cells (characters, colors, cursor position, scrollback).
3. **Ratatui terminal widget** — renders the parsed terminal grid as a Ratatui widget within the existing TUI layout. Each pane is a `Rect` area showing live terminal output.
4. **Input routing** — keyboard input is forwarded to the focused pane's PTY. The dashboard captures its own keybindings before forwarding.

### Benefits

- **Single binary** — no external dependencies to install
- **Complete layout control** — we own every pixel; 50/50 splits, stacked panes, tabs are all internal state
- **No keybinding conflicts** — we handle all input before any shell sees it
- **No pane collapse** — we control rendering; a finished command just shows its output
- **Consistent tabs** — tabs are our own UI element, always visible, styled how we want

## Technical Design

### Current Zellij Surface (to replace)

**`PaneController` trait** (`src/pane.rs`) — 9 methods:
- `focus_pane`, `create_pane`, `close_pane`, `list_panes`, `resize_pane`, `rename_pane`, `toggle_layout`, `write_to_pane`, `name`/`is_available`

**Zellij launcher** (`src/main.rs:maybe_exec_zellij`) — 180 lines:
- Writes layout.kdl (swap layouts for stacked/tiled), config.kdl (keybindings, plugins), shell wrapper
- Launches `zellij --session dot-agent-deck --new-session-with-layout`
- Session reattachment logic

**12 Zellij CLI commands** used across `src/pane.rs`:
- `new-pane`, `close-pane`, `list-panes`, `rename-pane`, `resize`, `next-swap-layout`, `write`, `focus-next-pane`, `new-tab`, `go-to-tab-name`, `close-tab`, `query-tab-names`

**Environment variables**: `ZELLIJ` (detection), `ZELLIJ_PANE_ID` (pane identity in hooks)

### New Architecture

```
┌─────────────────────────────────────────────────┐
│  Ratatui TUI (src/ui.rs)                        │
│  ┌──────────┐  ┌──────────────────────────────┐ │
│  │Dashboard │  │ Embedded Terminal Pane(s)     │ │
│  │(existing)│  │ ┌──────────────────────────┐  │ │
│  │          │  │ │ VT grid → Ratatui widget │  │ │
│  │          │  │ │ ← PTY stdout             │  │ │
│  │          │  │ │ → PTY stdin (keystrokes) │  │ │
│  │          │  │ └──────────────────────────┘  │ │
│  └──────────┘  └──────────────────────────────┘ │
└─────────────────────────────────────────────────┘
```

### Key Components

**`EmbeddedPaneController`** — new `PaneController` implementation replacing `ZellijController`:
- `create_pane()` → spawn PTY via `portable-pty`, start VT parser, return internal pane ID
- `focus_pane()` → update internal focus state (which pane receives keyboard input)
- `close_pane()` → kill PTY process, drop VT parser state
- `list_panes()` → return in-memory pane registry
- `write_to_pane()` → write bytes to PTY stdin
- `resize_pane()` → notify PTY of new dimensions (SIGWINCH)
- `toggle_layout()` → switch between stacked/tiled `Rect` arrangements

**`TerminalWidget`** — new Ratatui widget:
- Takes a VT parser's cell grid as input
- Renders cells with colors, bold, underline, etc. as Ratatui `Span`s
- Handles scrollback display
- Shows cursor position when pane is focused

**Layout engine** — replaces Zellij's swap layouts:
- Dashboard gets left 33%
- Agent panes share right 67%
- Stacked mode: only focused pane expanded (others show title bar)
- Tiled mode: equal-height split
- Mode tabs: 50/50 agent + stacked side panes

**Input router**:
- Dashboard mode: keybindings handled by existing `handle_normal_key` etc.
- Pane focused: most keys forwarded to PTY stdin
- Escape sequence (e.g., `Alt+d`) returns to dashboard

### What stays the same

- `PaneController` trait interface — callers in `src/ui.rs` and `src/mode_manager.rs` don't change
- `AgentEvent.pane_id` — still a string ID, just internally generated instead of from `ZELLIJ_PANE_ID`
- `SessionState.pane_id` — same tracking, same rendering
- Hook system — still captures events, just pane_id source changes
- Dashboard rendering — untouched, same card layout
- Config system — `DashboardConfig` unchanged

### What gets removed

- `ZellijController` impl in `src/pane.rs`
- `maybe_exec_zellij()` in `src/main.rs` (~180 lines)
- Layout.kdl and config.kdl generation
- Shell wrapper script
- `NoopController` (embedded panes always available)
- `ZELLIJ` and `ZELLIJ_PANE_ID` env var handling in `src/hook.rs`
- Zellij tab methods (`create_tab`, `go_to_tab`, `close_tab`, `list_tabs`, `send_keys`, `create_pane_directed`)

### Crate evaluation

| Crate | Purpose | Notes |
|-------|---------|-------|
| `portable-pty` | PTY spawning | Cross-platform, well-maintained, used by WezTerm |
| `alacritty_terminal` | VT parser | Full-featured, extracted from Alacritty. Heavier but complete. |
| `vt100` | VT parser (alternative) | Simpler, lighter. May lack some escape sequences. |
| `termwiz` | VT parser (alternative) | From WezTerm project. Also includes PTY support. |

**Recommendation**: Start with `portable-pty` + `vt100` for simplicity. Upgrade to `alacritty_terminal` only if `vt100` proves insufficient for real-world agent output.

## Edge Cases

- Agent outputs raw binary / non-UTF8 → VT parser handles this (terminal emulation is byte-level)
- Terminal resize mid-session → send SIGWINCH to PTY, re-query VT grid
- Multiple panes updating simultaneously → each pane has its own PTY reader thread; UI polls/redraws on tick
- Shell exits → pane stays rendered with last output + "[exited]" indicator
- Permission prompts (OpenCode y/n) → `write_to_pane` writes to PTY stdin, same as today
- Large scrollback → cap at configurable limit (e.g., 10,000 lines)
- Mouse support → future enhancement, not required for v1

## Milestones

- [x] Crate evaluation and proof of concept — spawn a PTY with `portable-pty`, parse output with `vt100`, render a single terminal in Ratatui. Validate that Claude Code / shell output renders correctly.
- [x] `EmbeddedPaneController` core — implement `create_pane`, `close_pane`, `list_panes`, `focus_pane`, `write_to_pane` against `PaneController` trait using PTY + VT parser
- [x] Terminal widget rendering — Ratatui widget that renders VT grid cells with colors, cursor, and scrollback. Handles resize notifications (SIGWINCH). MasterPty stored for resize, Event::Resize updates PTY dimensions.
- [x] Layout engine — stacked and tiled modes replacing Zellij swap layouts. Dashboard 33% left, panes 67% right. Toggle with `Ctrl+t`. Stats bar in dashboard area, hints bar full-width. Auto-focus and card sync on new pane creation.
- [x] Input routing — `UiMode::PaneInput` forwards keystrokes to PTY stdin via `write_raw_bytes()`. `keyevent_to_bytes()` converts crossterm events to terminal byte sequences (control codes, ANSI escapes, F-keys, Alt prefix). Auto-enters PaneInput on pane focus. Ctrl+d returns to dashboard. Ctrl+C forwarded as 0x03. Quit confirmation dialog (Ctrl+C twice to exit). Poll reduced to 16ms for responsive typing. `KeyEventKind::Press` filter added.
- [x] Remove Zellij — deleted `ZellijController`, `NoopController`, `maybe_exec_zellij()`, layout/config KDL generation, shell wrapper, `ZELLIJ_PANE_ID` fallback. ~600 lines removed. App no longer depends on or launches Zellij.
- [x] Tests and validation — 187 unit tests + 10 integration tests covering PTY lifecycle, VT rendering, layout calculations, keyevent_to_bytes, color mapping, cursor, and selection. Manual validation with Claude Code. vt100 upgraded to 0.16, ratatui to 0.30.
- [x] Documentation — README updated to remove all Zellij references, keybindings updated to Ctrl-based global shortcuts, Zellij installation section removed, Launching section rewritten for native panes.
- [x] Terminal polish — typing latency fix (event drain loop), mouse scrollback (vt100 built-in), mouse text selection with double-click word / triple-click paragraph, clipboard copy via OSC 52, bracketed paste, Alt+Backspace/arrows, real blinking cursor.

**Note**: Tab support for modes is tracked in PRD 34 (Extensible Modes System), not this PRD.

## Out of Scope (v1)

- Sixel / image protocol rendering
- Split panes within a single embedded terminal
- SSH / remote PTY connections
- OSC 52 clipboard read (write is implemented)
- Focus event forwarding (`\x1b[?1004h`)
- Terminal title display from OSC 0/2 sequences
