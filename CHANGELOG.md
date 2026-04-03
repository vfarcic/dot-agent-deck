# Changelog

## [0.13.0] - 2026-04-03

### Added

- **Permission Prompt Control from Dashboard**
  Respond to agent permission prompts directly from the dashboard without switching panes. Previously, when Claude Code or OpenCode needed permission to run a tool (e.g., execute a bash command), users had to switch to that specific agent's pane to approve or deny — breaking the dashboard workflow and making multi-agent oversight tedious.
  Session cards now display a permission banner showing the tool name and details when an agent requests approval. Cards with pending permissions are highlighted with a distinct border color. Press `y` to allow or `n` to deny directly from the dashboard — the decision is sent back to the agent, which continues or receives denial feedback immediately. Multiple agents can have pending permissions simultaneously, and each is handled independently.
  The feature works through the `PermissionRequest` hook mechanism: the hook process stays connected to the daemon via a Unix socket while a oneshot channel mediates the response from the TUI. A 10-minute timeout prevents stale permissions from blocking agents indefinitely.



## [0.12.1] - 2026-04-02

### Fixed

- **OpenCode Prompts Render Again**
  The bundled OpenCode plugin now emits `session.prompt` events as soon as `message.created` fires, so OpenCode decks once again show the `Prmt:` label after opencode.ai’s recent API change. Reinstall the plugin (`dot-agent-deck hooks install --agent opencode`) to pick up the fix.



## [0.12.0] - 2026-04-02

### Added

- **Directory Picker Filtering**
  Finding a project directory is now instant. The new `/` shortcut puts the New Pane directory picker into filter mode so you can type part of a folder name (case insensitive) and see just the matches while the `..` parent entry stays available. Navigation wraps from the start/end of the list, and Esc clears the filter so a second Esc or `q` still closes the popup.
  Press `/` to start filtering, type to narrow the list, use `↑`/`↓` (or `j/k`) to move through the results, and hit `Enter` to accept the filter and keep navigating. Backspace edits the query, Esc clears it, and directories without subfolders now immediately confirm the selection instead of forcing you to go up.



## [v0.11.6] - 2026-04-02

### Fixed

- **OpenCode Decks Survive Session Clears**
  Clearing an OpenCode chat inside OpenCode now reuses the existing deck in dot-agent-deck instead of leaving the stale card behind and spawning a second one. The dashboard now remaps all incoming events that reference the same `pane_id` to the original session so pane layouts remain stable across `/clear` and new-chat resets.


## [v0.11.5] - 2026-04-02

### Fixed

- **Reliable OpenCode Decks**
  OpenCode sessions now show up immediately and stay inside a single deck even when you clear prompts or start a fresh chat inside the same TUI window. Previously every restart created a brand-new card (and sometimes no card at all) because the OpenCode plugin lost track of its session IDs, so the dashboard could not correlate the lifecycle events.
  The plugin now emits `session.prompt` events as soon as a user message arrives, synthesizes `session.created` and `session.deleted` transitions when OpenCode misses them, keeps a canonical session ID per working directory, and flushes the deck as soon as you exit with `Ctrl+C`. Reinstall the hook with `dot-agent-deck hooks install --agent opencode` (or rerun the installer via `cargo run`) to pick up the fix.



## [0.11.4] - 2026-04-02

### Fixed

- **Dashboard Shortcut Fix**
  `Opt+d` from agent panes in the second column now jumps directly back to the dashboard even when every pane is visible. Previously the shortcut only moved focus left one column, so multi-column layouts forced two keypresses to reach the dashboard while stacked mode kept working as expected.



## [0.11.3] - 2026-04-02

### Fixed

- **Balanced Pane Layout Toggle**
  Pressing `t` now fans agent panes out on an even grid, so each column and row gets equal space instead of inheriting inconsistent sizes from the `children` placeholder.

### Changed

- **Devbox Agent Script Defaults to OpenCode**
  Running `devbox run agent` now launches the `opencode` CLI so OpenCode sessions can be spun up without passing extra flags. The previous default pointed at `claude`, which no longer reflects the recommended workflow for the dashboard’s bundled OpenCode plugin.


## [0.11.2] - 2026-04-01

### Fixed

- **OpenCode Sessions Render Correctly**
  OpenCode panes now appear in the dashboard alongside Claude Code again. The bundled OpenCode plugin was rewritten to use OpenCode's new `DotAgentDeckPlugin` export so session, tool, and permission events are forwarded in the format the daemon expects. Previously, OpenCode quietly stopped emitting compatible events after their plugin API change, leaving the third card empty in dot-agent-deck.
  Reinstall the plugin with `dot-agent-deck hooks install --agent opencode` to pick up the fix—future OpenCode upgrades will continue to stream into the dashboard without manual tweaks.


## [0.11.1] - 2026-04-01

### Fixed

- **Version Update Notification**
  The upgrade notification in the dashboard status bar now reliably detects newer releases. Previously, a 24-hour version check cache could retain stale data, causing the app to incorrectly conclude no update was available. The cache has been removed — each launch now fetches the latest release directly from GitHub (in the background, with a 10-second timeout).



## [0.11.0] - 2026-04-01

### Added

- **OpenCode Agent Support**
  Monitor OpenCode (opencode.ai) sessions alongside Claude Code in the same unified dashboard. Previously, only Claude Code sessions were visible, forcing developers to context-switch between terminals to track what each agent is doing.
  OpenCode sessions now appear in the dashboard with an "OpenCode" label, with full event mapping for session lifecycle, tool execution, and permission prompts. The `hook` subcommand accepts an `--agent opencode` flag to receive events from OpenCode's native plugin system, and the `hooks install --agent opencode` command sets up a thin JS plugin in `~/.opencode/plugin/dot-agent-deck/` that automatically forwards events to the dashboard. Uninstalling is equally simple with `hooks uninstall --agent opencode`. All existing Claude Code functionality remains unchanged — Claude Code is still the default when no `--agent` flag is specified.



## [0.10.0] - 2026-04-01

### Added

- ## Toggle Stacked/Tiled Pane Layout
- 
- Switch between stacked and tiled layouts to see all agent panes at once. Previously, multiple agent panes used a stacked layout where only the active pane was expanded — making it impossible to monitor all agents simultaneously.
- 
- Press `t` from the dashboard (Normal mode) or `Alt+t` from any pane to cycle between layouts. In stacked mode, only the focused agent pane is expanded while others collapse to title bars. In tiled mode, all agent panes share the right column equally with responsive breakpoints: a single column for 1–3 agents, two columns for 4–6 agents, and three columns for 7 or more agents. The dashboard pane stays fixed at 33% width in both layouts.



## [0.9.1] - 2026-04-01

### Fixed

- Use true black (RGB 0,0,0) background instead of ANSI black, fixing purple background on terminals with custom themes. Modals now also have an explicit black background.
- Update notification no longer replaces keyboard shortcuts in the bottom bar; it now appears alongside them.
- Derive binary version from git tags instead of hardcoded Cargo.toml value, fixing incorrect "current v0.1.0" in update notifications.



## [0.8.0] - 2026-04-01

### Added

- Add `--version` / `-V` flag to display the current version.



## [0.7.1] - 2026-04-01

### Fixed

- Force black background on dashboard pane so colors remain readable on light terminal themes.



## [0.7.0] - 2026-04-01

### Added

- Add version update notification that checks GitHub Releases on startup and displays a non-intrusive TUI notification when a newer version is available. Results are cached for 24 hours to minimize API calls.



## [0.6.1] - 2026-04-01

### Fixed

- Fix WaitingForInput status not showing during permission prompts (e.g., Bash approval). The v0.4.1 guard incorrectly suppressed Notification events when a tool was active.



## [0.6.0] - 2026-04-01

### Fixed

- ## Fix Stats Bar Visibility
- 
- The idle count and tools count in the bottom stats bar were nearly invisible on dark terminal backgrounds. Changed their color from DarkGray to Gray for readable contrast while remaining visually subdued.



## [0.5.0] - 2026-03-31

### Added

- ## Aggregate Stats Bar
- 
- A persistent status bar at the bottom of the dashboard shows real-time aggregate metrics across all sessions. Instead of visually scanning every card to tally how many agents are active, waiting, or erroring, the stats bar provides an instant overview.
- 
- The bar displays total active sessions, per-status counts (working, thinking, compacting, waiting, error, idle), and a cumulative tool call count. Each status category is color-coded — green for working, yellow for waiting, red for errors — and zero-count categories are hidden to save space. Counts update automatically as agent events arrive with no user interaction required.

### Fixed

- ## WaitingForInput Status During AskUserQuestion
- 
- The dashboard now correctly shows "Waiting for Input" when Claude Code uses the AskUserQuestion tool. A previous fix to prevent spurious waiting status during non-interactive tools (like Bash) inadvertently blocked the status transition for interactive tools that genuinely wait for user input.



## [0.4.2] - 2026-03-31

### Fixed

- ## Cleaner Multi-Prompt Display
- 
- The "Prmt:" label now appears only on the first prompt line in session cards. Additional prompts are indented with spaces instead of repeating the label, reducing visual clutter when cards have room to show multiple prompts.



## [0.4.1] - 2026-03-31

### Fixed

- Fixed "Needs Input" status getting stuck in sidebar when a Notification event arrived while a tool was actively running.



## [0.4.0] - 2026-03-31

### Added

- ## Adaptive Card Density
- 
- Dashboard cards now automatically adjust their content density based on available screen height. When all cards fit on screen, each card shows up to three recent prompts and three tool commands for richer context. When cards would overflow, the layout switches to a compact mode showing one prompt and one tool per card, fitting more sessions on screen without scrolling.
- 
- The density recalculates on every frame, so resizing the terminal instantly adapts the layout. Three modes are available: Spacious (10 rows, 3 prompts, 3 tools), Normal (8 rows, 1 prompt, 3 tools), and Compact (6 rows, 1 prompt, 1 tool). The dashboard always selects the most spacious mode that avoids scrolling.



## [0.3.1] - 2026-03-31

### Fixed

- Preserve card position when a Claude Code session is cleared — restarted sessions on the same pane now keep their original index instead of jumping to the end.
- Fix changelog assembly to recognize semantic fragment types (feature, bugfix, breaking) so release notes are generated and fragments cleaned up correctly.



## [0.1.0] - 2026-03-27

### Added

- ## GitHub Actions CI/CD Workflows
- 
- Automated CI/CD pipeline for the project. Pull requests now run cargo fmt, clippy, build, and test checks automatically, with cargo audit for dependency vulnerability scanning.
- 
- Pushing a `v*` tag triggers multi-platform release builds for Linux (amd64/arm64), macOS (Intel/Apple Silicon), and Windows (amd64), with SHA256 checksums for all binaries. Releases are published to GitHub with auto-generated changelog notes from `changelog.d/` fragments. Homebrew formulas are published to `vfarcic/homebrew-tap` and Scoop manifests to `vfarcic/scoop-bucket` for easy installation.
- 
- Supporting workflows auto-label PRs based on changed files and manage stale issues/PRs. A `Taskfile.yml` provides distribution tasks for checksum generation, Homebrew formula creation, and Scoop manifest creation.
