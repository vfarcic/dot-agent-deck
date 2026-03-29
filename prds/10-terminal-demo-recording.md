# PRD #10: Terminal Demo Recording for README

**Status**: Open
**Priority**: Low
**GitHub Issue**: [#10](https://github.com/vfarcic/dot-agent-deck/issues/10)
**Depends on**: PRD #4 (Documentation)

## Problem

The README has no visual demo. New visitors must install and run the tool to understand what it looks like. A terminal recording showing the dashboard in action would immediately communicate the value proposition.

## Solution

Record a screen capture of dot-agent-deck running inside Zellij with live agent sessions, convert it to a GIF (or embedded video), and add it to the README in place of the existing `<!-- TODO -->` placeholder.

### Why Screen Recording (Not Asciinema)

Asciinema records a single shell's stdout. It cannot capture Zellij's rendered TUI with split panes, borders, and layout. A screen recording of the terminal window captures exactly what users will see.

### Recording Content

The demo should show:
- The two-column Zellij layout (dashboard left, agent panes right)
- At least 2-3 agent sessions with different statuses
- Basic navigation: moving between cards, focusing a pane, returning to dashboard
- A session status changing in real-time (e.g., Thinking → Working → Idle)

### Technical Approach

1. Screen-record the terminal window (macOS screen capture, OBS, or similar)
2. Convert to GIF using `ffmpeg` or `gifski` (keep under ~5 MB for GitHub)
3. Store the GIF in the repo (e.g., `assets/demo.gif`)
4. Replace the `<!-- TODO -->` comment in README.md with `![demo](assets/demo.gif)`

## Non-Goals (v1)

- Narrated video or voiceover
- Hosted video (YouTube, etc.) — GIF in repo is sufficient
- Multiple recordings for different features
- Asciinema or VHS-based recordings (incompatible with Zellij multi-pane UI)

## Milestones

- [ ] Record screen capture of dashboard with live agent sessions
- [ ] Convert to optimized GIF (< 5 MB)
- [ ] Add GIF to repo and embed in README
- [ ] Remove the `<!-- TODO -->` placeholder from README

## Success Criteria

- README displays an animated demo above the fold
- New visitors can see what the dashboard looks like without installing
- GIF file size is reasonable for GitHub rendering (< 5 MB)
