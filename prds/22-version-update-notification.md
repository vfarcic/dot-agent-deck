# PRD #22: Version Update Notification

**Status**: Draft
**Priority**: Medium
**Created**: 2026-04-01
**GitHub Issue**: [#22](https://github.com/vfarcic/dot-agent-deck/issues/22)

## Problem Statement

Users have no way to know when a newer version of dot-agent-deck is available. They may run outdated versions indefinitely, missing important bug fixes, security patches, and new features. Since the app is distributed via GitHub Releases, Homebrew, and Scoop, there is no built-in update channel that pushes notifications to users.

## Solution Overview

On startup, spawn a non-blocking async task that checks the GitHub Releases API for the latest published version. Compare it with the version compiled into the binary. If a newer version is available, display a brief, non-intrusive notification in the TUI. Cache the result locally so the API is not hit on every launch.

The check is fully asynchronous — the TUI renders immediately and is never blocked by the version check. If the network is slow or unavailable, the user sees no delay or error.

## Scope

### In Scope
- Compile the crate version into the binary via `env!("CARGO_PKG_VERSION")`
- Non-blocking async HTTP request to the GitHub Releases API on startup
- Semantic version comparison (current vs. latest)
- TUI notification when a newer version is available
- Local file-based cache to throttle API checks (at most once per 24 hours)
- Graceful degradation: if the check fails (no network, rate-limited), silently skip

### Out of Scope
- Automatic downloading or installing of updates
- In-app update mechanism (users update via their package manager)
- Pre-release or nightly version tracking
- Opt-in/opt-out configuration (can be added later if needed)
- Version checking for the `hook` subcommand (only the dashboard)

## Technical Approach

### Version Constant
- Use `env!("CARGO_PKG_VERSION")` to embed the crate version at compile time
- This already reflects the value in `Cargo.toml` — no extra build step needed

### HTTP Client
- Add `reqwest` dependency with minimal features (`rustls-tls`, `json`) to keep binary size small
- Single GET request to `https://api.github.com/repos/vfarcic/dot-agent-deck/releases/latest`
- Set `User-Agent` header (required by GitHub API)
- Parse the `tag_name` field from the JSON response

### Version Comparison
- Add `semver` crate for robust semantic version parsing and comparison
- Strip leading `v` prefix from the GitHub tag (e.g., `v0.5.0` -> `0.5.0`)
- Compare: if `latest > current`, flag an update as available

### Cache (`~/.config/dot-agent-deck/version-check.json`)
- Store: `{ "latest_version": "0.5.0", "checked_at": "2026-04-01T12:00:00Z" }`
- On startup, read the cache; if `checked_at` is within the last 24 hours, use the cached value
- If cache is missing, expired, or unreadable, perform a fresh API check
- Write updated cache after each successful API response

### Integration with Dashboard Startup (`src/main.rs`)
- Spawn the version check as a `tokio::spawn` background task before or alongside the TUI task
- Send the result (if an update is available) to the TUI via a `oneshot` channel or the existing event channel
- The TUI displays the notification without blocking startup — the dashboard is fully usable immediately

### TUI Notification (`src/ui.rs`)
- When an update is available, render a short message in the status area or as a temporary overlay
- Example: `Update available: v0.5.0 (current: v0.4.0) — upgrade via your package manager`
- The notification should be dismissible (disappears after a keypress or timeout) or persist subtly in a corner
- Use a distinct but non-alarming color (e.g., cyan or yellow)

### New Module (`src/version.rs`)
- `pub async fn check_for_update() -> Option<String>` — returns the latest version string if newer, `None` otherwise
- `fn read_cache()` / `fn write_cache()` — handle the local cache file
- `fn current_version() -> semver::Version` — parses `env!("CARGO_PKG_VERSION")`
- Keep all version-check logic isolated in this module

## Dependencies Added

| Crate    | Purpose                        | Features              |
|----------|--------------------------------|-----------------------|
| `reqwest`| HTTP client for GitHub API     | `rustls-tls`, `json`  |
| `semver` | Semantic version parsing/compare | default              |

## Success Criteria

- On first launch with network, the dashboard shows a notification if a newer GitHub release exists
- The notification includes both the current and latest version numbers
- If no update is available, nothing is shown — zero noise
- Startup time is not perceptibly affected (check runs as a background async task)
- Subsequent launches within 24 hours use the cached result (no API call)
- If the network is unavailable or the API fails, no error is shown to the user

## Milestones

- [ ] Version module (`src/version.rs`) with GitHub API check, semver comparison, and file-based cache
- [ ] Integration into dashboard startup as a non-blocking background task
- [ ] TUI notification rendering when an update is available
- [ ] Tests for version comparison logic and cache behavior
- [ ] End-to-end validation: build with an older version, confirm notification appears against a newer release

## Risks & Mitigations

| Risk | Impact | Mitigation |
|------|--------|------------|
| GitHub API rate limit (60 req/hr unauthenticated) | Check fails silently | 24-hour cache ensures at most 1 request per day per user |
| `reqwest` increases binary size | Larger download | Use minimal features (`rustls-tls` avoids system OpenSSL dependency) |
| Network delay on startup | Perceived slowness | Fully async, non-blocking — TUI renders immediately |
| Version tag format changes | Comparison breaks | Graceful fallback: if parsing fails, skip the check |
