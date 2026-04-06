# PRD #51: Enhance Documentation with Screenshots

**Status**: Draft
**Priority**: Medium
**Created**: 2026-04-06

## Problem

The documentation site is entirely text-based. Users evaluating dot-agent-deck have no way to see what the UI looks like without installing and running it. This makes it harder to understand the dashboard layout, session cards, pane management, and keyboard-driven workflow — all of which are highly visual concepts that text alone struggles to convey.

## Solution

Add screenshots to key documentation pages showing the actual dot-agent-deck UI in action. Screenshots should cover the core user journey: the dashboard overview, session cards with various statuses, pane layouts (stacked and tiled), new-pane dialog, and keyboard-driven navigation. Images should be stored in the docs repository and embedded inline where they add the most context.

## User Experience

### Before (Current)
- Users read text descriptions like "a two-column layout with native embedded terminal panes" and must imagine what this looks like
- Session status table lists statuses but users can't see the actual card appearance
- Getting Started page describes the workflow in steps but provides no visual reference

### After
- Each major docs page includes 1-3 screenshots showing the described feature in action
- Users can evaluate the product visually before installing
- The Getting Started page walks users through the workflow with annotated screenshots matching each step

## Screenshots Needed

The following screenshots should be captured and added to the corresponding docs pages:

### Introduction (`intro.md`)
1. **Hero screenshot** — full dashboard view with a few active sessions, showing the two-column layout (dashboard cards on the left, agent pane on the right)

### Getting Started (`getting-started.md`)
2. **Dashboard on first launch** — empty or minimal dashboard after initial startup
3. **New pane dialog** — the Ctrl+n dialog for creating a new agent pane
4. **Active session** — dashboard with at least one session running, showing real-time status updates

### Session Management (`session-management.md`)
5. **Session cards with statuses** — cards showing different statuses (Thinking, Working, WaitingForInput, Idle)
6. **Session detail view** — a focused view of a single session card showing all metadata (agent type, directory, tool count, last prompt)

### Configuration (`configuration.md`)
7. **Configuration in action** — any visual that helps illustrate config changes (e.g., custom layout, theme)

### Keyboard Shortcuts (`keyboard-shortcuts.md`)
8. **Help overlay** — the `?` shortcut overlay showing available keybindings

## Technical Approach

1. **Image format**: PNG for screenshots, optimized for web (compressed, reasonable dimensions ~1200px wide max)
2. **Storage**: `docs/static/img/` directory (standard Docusaurus convention)
3. **Naming convention**: `{page}-{description}.png` (e.g., `getting-started-new-pane-dialog.png`)
4. **Embedding**: Standard Markdown image syntax in the `.md` files
5. **Capture method**: Manual screenshots from a real running instance with representative sample data
6. **Alt text**: Every image must have descriptive alt text for accessibility

## Milestones

- [ ] Create `docs/static/img/` directory and establish naming convention
- [ ] Capture and add hero screenshot to introduction page
- [ ] Capture and add Getting Started screenshots (first launch, new pane dialog, active session)
- [ ] Capture and add Session Management screenshots (status cards, detail view)
- [ ] Capture and add remaining screenshots (config, help overlay)
- [ ] Verify all images render correctly on the docs site
- [ ] Optimize image file sizes for fast page loads

## Success Criteria

- Every major documentation page has at least one screenshot
- Screenshots accurately reflect the current UI
- Images are optimized (each under 500KB)
- All images have descriptive alt text
- Docs site loads quickly with the added images

## Risks & Mitigations

| Risk | Mitigation |
|------|------------|
| Screenshots go stale as UI evolves | Add a note in CONTRIBUTING or PRD process to update screenshots when UI changes |
| Large images slow down docs site | Compress PNGs, enforce max dimensions, consider lazy loading |
| Terminal content in screenshots may contain sensitive paths | Use generic/demo paths and data when capturing |
