# PRD #50: Auto-Install Hooks on CLI Startup

**Status**: In Progress
**Priority**: High
**Created**: 2026-04-06

## Problem

Users must manually run `dot-agent-deck hooks install --agent <agent>` after installation to connect their agents to the dashboard. This step is easy to forget, especially for first-time users, and means they don't automatically receive new hook types when upgrading. The manual step adds friction to the getting-started experience and creates support issues when users launch the dashboard but see no agent sessions.

## Solution

On CLI startup, automatically detect which agents are installed by checking for their configuration directories (`~/.claude/` for Claude Code, `~/.opencode/` for OpenCode) and install/update hooks for each detected agent. The existing `hooks install` and `hooks uninstall` commands remain available as optional tools for debugging and manual removal.

The install logic is already idempotent and purely local file I/O (~100ms combined for both agents), so there is no meaningful startup time impact.

## User Experience

### Before (Current)
```bash
brew install dot-agent-deck
dot-agent-deck hooks install                    # easy to forget
dot-agent-deck hooks install --agent opencode   # if using OpenCode
dot-agent-deck
```

### After
```bash
brew install dot-agent-deck
dot-agent-deck                                  # hooks auto-installed
```

### Edge Cases
- **No agents installed**: Skip hook installation silently, launch normally
- **Only one agent installed**: Install hooks only for the detected agent
- **Hooks already current**: Idempotent — no changes made, no extra I/O beyond the directory check
- **User ran `hooks uninstall`**: Next startup will re-install. Users who truly want hooks removed should uninstall the CLI or we could add a config flag in a future iteration if demand arises

## Technical Approach

1. During startup (before entering the TUI), check if `~/.claude/` and `~/.opencode/` directories exist
2. For each detected agent, call the existing `install_impl()` / OpenCode equivalent
3. Errors during auto-install should be logged but not block startup
4. The `hooks install` and `hooks uninstall` CLI commands remain unchanged

## Milestones

- [x] Auto-detect installed agents and install hooks on startup
- [x] Errors during auto-install are logged without blocking startup
- [x] Existing `hooks install`/`uninstall` commands still work as before
- [ ] Documentation updated (getting-started.md quickstart and hook setup sections)
- [ ] Changelog fragment created

## Success Criteria

- New users can run `dot-agent-deck` after install with zero manual hook setup
- Upgrading users automatically get new hook types on next launch
- Startup time increase is imperceptible (< 150ms)
- No regressions for users who prefer manual hook management
