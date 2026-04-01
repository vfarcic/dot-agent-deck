# Changelog

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
