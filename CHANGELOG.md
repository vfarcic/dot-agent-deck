# Changelog

## [0.24.2] - 2026-04-28

### Documentation

  Restructure the docs site with annotated screenshots, accuracy fixes, and a richer home page. The landing page gains a "Why Agent Deck" narrative, design principles, and tabbed installation instructions for macOS / Linux / Windows (WSL). Getting Started, Session Management, and Workspace Modes pages now include screenshots illustrating the dashboard layout, session card details, and mode tabs in action. Several long-standing inaccuracies were corrected: `Ctrl+c` quit-dialog behavior, `Ctrl+w` semantics on dashboard vs. mode tabs, command-mode requirements for `Tab` / `Shift+Tab` / `j` / `k`, and the "Mode tabs are restored on `--continue`" claim (now tracked as a bug — see issues #68 and #69).



## [0.24.1] - 2026-04-27

### Fixed

  Release binaries now report the correct version. Previously the release workflow's build job used a shallow checkout without tag refs, causing `git describe` in `build.rs` to fall back to `CARGO_PKG_VERSION` (`0.1.0`) — so installed binaries always reported `v0.1.0` and the update banner appeared even on the latest release.



## [0.24.0] - 2026-04-27

### Added

- **Customizable Config-Gen Prompt and Orchestration Role Library**
  The Ctrl+G config-generation prompt and orchestration role definitions now live in editable asset files instead of being hardcoded in the binary. The prompt template is at `assets/config_gen_prompt.md` and the role library (coder, reviewer, auditor, tester, documenter, release, researcher) is at `assets/roles.toml`. Both are bundled at compile time, so behavior is unchanged for users who don't customize, but contributors can iterate on the prompt without touching Rust source.
  The default prompt has also been improved: it now teaches the AI to discover project-defined agent launchers (devbox/npm/task scripts, `.claude/`/`opencode.json` configs, etc.), match them to roles by semantic intent, record the full invocation form (e.g. `devbox run agent-big`, never the bare script name), and propose a dedicated `release` role by default whenever the project has release-flow signals — with explicit context-handoff guidance for the orchestrator so workers cold-starting with no shared scratchpad still receive the file paths and prior findings they need.
  The bundled `.dot-agent-deck.toml` reflects these defaults: a `release` role with `clear = false` so it can resume after CI flakes, and a context-handoff section in the orchestrator's `prompt_template`.

### Fixed

- **Reliable Prompt Submission to Agent Panes**
  Prompts written to agent panes now self-submit reliably instead of sitting in the agent's input buffer waiting for a manual Enter. Multi-line prompts are wrapped in bracketed paste so embedded newlines stay as input rather than triggering a premature submit, and a brief delay between the payload and the trailing carriage return makes agent CLIs (Claude Code, opencode) honor it as Enter rather than absorbing it as a newline-in-input.
  This affects two flows: pressing Ctrl+G to generate a `.dot-agent-deck.toml` config, and orchestration startup where the orchestrator's bootstrap prompt is injected into its agent pane. Both previously left the prompt un-submitted in some cases — the orchestration path additionally fused the role launch command into the prompt buffer because the role command was being written twice (once when the pane was spawned, once again after resize). The duplicate write has been removed.
  Status-bar messages now stay visible for 15 seconds instead of 3, so wrapped error messages such as "Orchestration failed: …" remain readable.
- **Docs Pod Readiness Probe Failure on Startup**
  The docs deployment's readiness and liveness probes now include a 5-second initial delay, preventing transient "connection refused" failures during pod startup. Previously, probes fired immediately before nginx had finished initializing and bound to port 8080, causing unhealthy pod events on every new pod creation.



## [0.23.0] - 2026-04-23

### Added

  Clickable hyperlinks in embedded terminal panes. Tools like Claude Code emit OSC 8 hyperlink sequences for URLs — these are now parsed and tracked so that Ctrl+click on a link row opens the URL in your default browser.



## [0.22.1] - 2026-04-22

### Fixed

  ### Fixed
  - **Mode tab agent pane leak on close**
    Closing a mode tab (Ctrl+W) now properly closes the agent pane's embedded PTY. Previously, `close_tab()` only closed persistent and reactive panes via `deactivate_mode()`, leaving the agent pane orphaned in the embedded pane controller. These orphaned panes accumulated on the dashboard's right-side terminal pane list each time a mode tab was closed and reopened.



## [0.22.0] - 2026-04-21

### Added

  Add multi-role agent orchestration system that enables coordinated multi-agent workflows with a dedicated orchestrator agent driving delegation decisions. Supports parallel fan-out, worker context isolation, and interactive panes.



## [0.21.0] - 2026-04-15

### Added

- **Improved Help Overlay**
  The `?` help overlay now shows every keybinding regardless of context, laid out in two columns for easier scanning. Previously, the Tab Navigation section was hidden whenever the dashboard tab was active — making tab shortcuts undiscoverable for users who hadn't yet opened a mode tab.
  The overlay now includes Mode Tab (in-tab pane navigation) and Directory Picker sections that were documented elsewhere but missing from the in-app help. All section headers use a single accent color for visual consistency.
  See the [Keyboard Shortcuts](https://agent-deck.devopstoolkit.ai/keyboard-shortcuts) reference for the full list.



## [0.20.3] - 2026-04-13

### Documentation

- **Troubleshooting Documentation**
  Added troubleshooting guide for Ghostty terminal users experiencing Shift+Enter not creating newlines when using Claude Code or other AI coding agents inside dot-agent-deck.
  Documents the root cause (Ghostty intercepts Shift+Enter when mouse capture is enabled) and provides the configuration solution using CSI u format keybind: `keybind = shift+enter=csi:13;2u`
  See the [Troubleshooting Guide](https://agent-deck.devopstoolkit.ai/troubleshooting) for complete instructions.



## [0.17.1] - 2026-04-13

### Documentation

- **Troubleshooting Documentation**
  Added troubleshooting guide for Ghostty terminal users experiencing Shift+Enter not creating newlines when using Claude Code or other AI coding agents inside dot-agent-deck.
  Documents the root cause (Ghostty intercepts Shift+Enter when mouse capture is enabled) and provides the configuration solution using CSI u format keybind: `keybind = shift+enter=csi:13;2u`
  See the [Troubleshooting Guide](https://agent-deck.devopstoolkit.ai/troubleshooting) for complete instructions.



## [0.20.0] - 2026-04-11

### Fixed

- **Mode Tab Fixes and Enhancements**
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



## [0.19.0] - 2026-04-11

### Added

- **AI-Generated ASCII Art for Idle Dashboard Cards**
  Idle dashboard cards now display funny, context-aware ASCII art instead of a static status display. When an agent session goes idle for more than 5 minutes (configurable), dot-agent-deck calls a lightweight LLM to generate humorous ASCII art based on the session's prompts and final response, then animates it directly in the dashboard card.
  The `dot-agent-deck ascii` CLI subcommand provides standalone art generation — pipe in prompts and get ASCII art back, useful for scripting or quick demos. The dashboard integration captures both first and last prompts to give the LLM full narrative context, producing art that reflects what the agent actually worked on. Multi-frame animations cycle on the dashboard tick loop, and a generate-validate-retry mechanism (up to 3 attempts) ensures broken art never reaches the screen — falling back to the flashing-dot indicator if needed. Art only renders in Spacious card density mode to avoid truncation artifacts.
  Enable the feature with `dot-agent-deck config set idle_art.enabled true` and set `ANTHROPIC_API_KEY` (or `OPENAI_API_KEY` for OpenAI, or use Ollama for zero-cost local generation). Configure provider and model in `[idle_art]` section of `.dot-agent-deck.toml`.
  See the [Configuration Guide](https://devopstoolkit.ai/docs/dot-agent-deck/configuration) for setup details.



## [0.18.0] - 2026-04-10

### Added

- **Extensible Modes System**
  Workspace modes transform dot-agent-deck from a multi-agent dashboard into a focused development environment. When AI agents execute commands, relevant output now appears in dedicated side panes alongside the agent instead of being buried in the conversation.
  Each mode is defined in a per-project `.dot-agent-deck.toml` config file. Modes create tab-based workspaces with an agent pane on the left and configurable side panes on the right in a 50/50 layout. Side panes come in two types: persistent panes run predefined commands immediately (e.g., `cargo watch -x test`, `kubectl get pods -w`), while reactive panes populate automatically when the agent executes commands matching user-defined regex rules (e.g., `kubectl describe`, `terraform plan`). Watch rules periodically re-execute commands via the built-in `dot-agent-deck watch` subcommand for live-updating output.
  Mode activation is integrated into the new-agent flow: press `Ctrl+n`, select a directory, and if a `.dot-agent-deck.toml` exists, cycle through available modes with arrow keys in the unified form. Tab navigation uses `Tab`/`Shift+Tab`, arrow keys, or `h`/`l` for vim users. Side panes support keyboard focus (`j`/`k`), click-to-focus, and full shell interaction via `Enter` on a focused pane. The `dot-agent-deck init` command scaffolds new config files, and `dot-agent-deck validate` checks config correctness. An agent-driven config generation flow (`g` on dashboard cards) analyzes the project and proposes a config interactively.
  See the [Workspace Modes Guide](https://devopstoolkit.ai/docs/ui/workspace-modes) for configuration examples and usage details.

### Fixed

- **Dashboard Card for Every Pane**
  Panes created with `Ctrl+n` now immediately display a dashboard card, even before an agent starts. Previously, new panes had no card until an agent emitted its first event, leaving the pane orphaned with no way to switch back to it or close it.
  A placeholder card appears instantly with a "No agent" label and a muted gray border, distinguishing it from active sessions. When an agent starts in the pane, the placeholder transitions seamlessly into a real session card. If a session ends (e.g., via `/clear`), the placeholder is restored so the pane remains visible and reusable. Placeholder sessions are excluded from active session statistics to keep dashboard counts accurate.



## [0.17.0] - 2026-04-06

### Added

- **Auto-Install Hooks on Startup**
  Agent hooks are now automatically installed when the dashboard launches. The CLI detects which agents are present (`~/.claude/` for Claude Code, `~/.opencode/` for OpenCode) and installs hooks for each one. Manual `hooks install` commands are no longer required for normal use.



## [0.16.0] - 2026-04-06

### Added

- **Project Documentation Site**
  A dedicated documentation site built with Docusaurus v3 replaces the monolithic README as the primary resource for users. Previously, all guides, configuration details, and feature overviews lived in the README, making it increasingly difficult to navigate as the project grew.
  The site covers the core user journey: installation, getting started, configuration, keyboard shortcuts, session management, and licensing. A custom homepage provides a polished entry point with feature highlights and quick-start links. The docs directory in the repository serves as the single source of truth — update a Markdown file and the site rebuilds automatically.
  Deployment uses a multi-stage Docker build (Node.js builder + nginx) published to ghcr.io, with a Helm chart for Kubernetes hosting monitored by Argo CD. The CI/CD pipeline builds and publishes the docs image as part of the release workflow on version tags.
  Visit the documentation at [agent-deck.devopstoolkit.ai](https://agent-deck.devopstoolkit.ai).
- **Star Repo Reminder Dialog**
  A non-intrusive dialog now appears every 10 launches encouraging you to star the GitHub repository. The dialog offers three options: press `s` to open the repo in your browser and dismiss permanently, `l` or `Esc` to snooze (reminder returns after 10 more launches), or `d` to permanently hide the dialog.
  State is persisted in `~/.config/dot-agent-deck/star-prompt-state.json` so your preference survives across sessions.
- **Arrow Key Navigation Focuses Panes**
  Arrow keys and vim-style navigation (`j`/`k`/`h`/`l`) on the dashboard now focus the selected session's pane, matching the behavior of the 1-9 number key shortcuts. Previously, arrow keys only moved the card highlight without switching the pane view.

### Fixed

- **"Needs Input" Status Clears After Tool Completion**
  Dashboard session cards no longer remain stuck on "Needs Input" after a permission-gated tool finishes executing. Previously, approving a tool (e.g., a long-running `gcloud` command) left the status as "Needs Input" indefinitely because the `PostToolUse` event did not update the session status. The status now transitions to "Thinking" once the tool completes, accurately reflecting that the agent is processing the result.



## [0.15.0] - 2026-04-05

### Added

- **Light Theme Option for Dashboard**
  The dashboard now adapts to your terminal's color scheme instead of forcing a black background. Previously, the hardcoded black background created a visual mismatch for users running light terminal themes — the dashboard pane appeared as a dark rectangle next to light-themed agent panes.
  On startup, the dashboard auto-detects whether your terminal uses a light or dark background (via OSC 11 query) and selects the appropriate foreground color palette. Accent colors (Cyan, Green, Yellow, Red, Blue, Magenta) remain unchanged since terminals already remap these per-theme. Only neutral text colors (titles, labels, secondary text) switch between themes to maintain readability and visual hierarchy on both light and dark backgrounds.
  Use `--theme auto|light|dark` (default: `auto`) to override auto-detection when needed — useful for tmux or SSH sessions where detection may not work reliably. The theme can also be set in the config file, for example `theme = "auto"`, `theme = "light"`, or `theme = "dark"`. The `dashboard` subcommand has been removed since `dot-agent-deck` defaults to dashboard mode and top-level args now work directly.
- **Session Restore**
  Pick up where you left off with automatic session persistence. Previously, launching `dot-agent-deck` always started from a blank slate, requiring users to re-open every agent pane, reselect directories, re-enter names, and retype commands each time.
  The dashboard now automatically tracks every pane's launch metadata (directory, name, and command) while running and persists the full pane set on exit — no explicit save step required. On the next launch, pass `--continue` to restore all saved panes in their original directories with their original commands and names. Panes that reference directories that no longer exist are skipped with a warning, so partial restores work gracefully without aborting.
  Session state is stored in `~/.config/dot-agent-deck/session.toml` (configurable via the `DOT_AGENT_DECK_SESSION` environment variable). The file uses a simple TOML format that can be edited manually or synced with dotfiles. Start with `dot-agent-deck --continue` or `dot-agent-deck dashboard --continue` to restore your last session.

### Fixed

- **Fix stuck "Needs Input" status**
  Removed the permission approval queue and blocking `PermissionRequest` hook that caused sessions to display "Needs Input" indefinitely. The deck previously registered both a `Notification` and a `PermissionRequest` hook for the same permission event — the blocking hook delayed every permission prompt and left stale entries in the queue when users approved in the terminal instead of the deck. The deck's permission UI (y/n approval) was already disabled, making the blocking hook purely harmful.
  The "Needs Input" status indicator still works correctly via the fire-and-forget `Notification` hook and clears automatically when the agent resumes work.



## [0.14.4] - 2026-04-05

### Fixed

  **Stale "Needs Input" status clears promptly after permission approval**
  The dashboard no longer shows "Needs Input" after the user approves a permission prompt. ToolStart events now match the front of the pending permission queue by tool name, so the permission is dequeued and the status transitions to "Working" immediately. Previously, synthetic permission IDs could never match real tool_use_ids, causing the status to stay stuck until an Idle event arrived.



## [0.14.3] - 2026-04-05

### Fixed

  **Stale "Needs Input" status clears promptly after permission approval**
  The dashboard no longer shows "Needs Input" after the user approves a permission prompt. Previously, approved permissions were not dequeued from the pending list, causing the status to stay stuck until an Idle event arrived. ToolStart events now resolve the matching permission by tool_use_id so the status transitions to "Working" immediately.



## [0.14.2] - 2026-04-05

### Fixed

- **Status indicator blinks at a comfortable rate and Idle status now blinks**
  The status dot for "Needs Input" now pulses at ~1 blink per second instead of flickering rapidly at ~30Hz. The "Idle" status also blinks now, since it represents a state where the user needs to provide the next prompt.



## [0.14.1] - 2026-04-05

### Fixed

- **Permission prompt status no longer overridden by concurrent tool events**
  The dashboard now correctly maintains "Needs Input" status when a permission prompt is active and subagent tools complete concurrently. Previously, `ToolStart` and `ToolEnd` events from subagent tools (e.g., an Explore agent running Bash commands) would override the `WaitingForInput` status back to `Working`, making the permission prompt invisible in the dashboard card. The status now stays as "Needs Input" until all pending permissions are resolved.



## [0.14.0] - 2026-04-04

### Changed

- **Native Terminal Panes (Zellij Removed)**
  Zellij is no longer required. dot-agent-deck now embeds terminal panes directly using Ratatui-native widgets with `portable-pty` for PTY management and `vt100` for terminal emulation. The application is now a single binary with no external dependencies.
  All keybindings have switched from Alt-based to Ctrl-based: `Ctrl+1`-`Ctrl+9` to select cards, `Ctrl+t` to toggle layout, `Ctrl+d` to return to dashboard from a pane. Terminal panes support mouse text selection (double-click word, triple-click paragraph), clipboard copy via OSC 52, mouse scrollback, bracketed paste, and `Alt+Backspace`/`Alt+arrows` for word-level editing. Layout modes (stacked/tiled) are now managed internally with the dashboard at 33% left and panes at 67% right.
  Users who previously installed Zellij solely for dot-agent-deck can uninstall it. No configuration migration is needed — press `Ctrl+n` to open the directory picker and create a new embedded pane.



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
