# Changelog

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
