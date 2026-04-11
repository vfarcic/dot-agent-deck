## Mode Tab Fixes and Enhancements

A batch of fixes addressing mode tab usability issues discovered during real-world usage.

### Text Wrapping in Agent Pane

The agent pane (Claude Code) in mode tabs now wraps text correctly at the pane boundary. Previously, text could extend beyond the visible area because the PTY was sized before the process started, or because switching tabs didn't update PTY dimensions. Three root causes were fixed:

- **Agent pane command now starts after PTY resize** — the agent pane is created as an empty shell, resized to the correct 50% width, and only then receives the command. The mode's `init_command` (e.g., `devbox shell`) is also sent to the agent pane.
- **Ctrl+t layout toggle uses correct width** — was hardcoded to 67% (dashboard width) for all panes; now uses 50% for mode tabs.
- **Tab switching resizes PTYs** — switching between dashboard (67%) and mode tabs (50%) now triggers a PTY resize so processes see the correct terminal width immediately.

### Mode Tab Session Restore (`--continue`)

Mode tabs are now fully restored when starting with `--continue`. Each saved pane records its mode name, and on restore the app looks up the mode config from the project's `.dot-agent-deck.toml` to recreate the full mode tab with agent and side panes. Falls back to a plain dashboard pane if the mode config is missing. The app always starts on the dashboard after restore for a better overview.

### Pane Navigation

- **Up/Down arrows cycle through all panes** including the agent pane, not just side panes. Down now wraps from the last side pane back to the agent.
- **Focus highlight syncs correctly** — navigating with j/k/Up/Down now updates the embedded controller's focus, fixing a bug where a previously-focused side pane kept its cyan border even when the agent pane was selected.

### Reactive Pane Prompt Suppression

Reactive (rule-triggered) panes now hide the shell prompt (`PS1`/`PS2`/`PROMPT`) so automated command output appears cleanly without prompt clutter. When entering a reactive pane manually (via `Enter`), a minimal `$ ` prompt is restored. Leaving with `Ctrl+d` re-suppresses it. The screen is cleared after prompt changes to keep output clean.

### Terminal Widget Rendering

Fixed a rendering bug where panes (especially the Clippy watch pane) would show only the last line of output. The viewport anchor now uses the cursor position instead of scanning for the last row with content, which was fooled by stray characters from shell initialization.

### Config Generation Hint

The persistent "g: generate .dot-agent-deck.toml" hint was removed from dashboard cards. Instead, a yellow italic tip appears contextually in the new-pane form when no modes are configured: "Tip: press g on dashboard to create modes".
