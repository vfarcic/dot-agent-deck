# PRD #5: GitHub Actions CI/CD Workflows

**Status**: Complete
**Priority**: High
**GitHub Issue**: [#5](https://github.com/vfarcic/dot-agent-deck/issues/5)
**Reference**: Follows similar patterns to [dot-ai CI/CD workflows](https://github.com/vfarcic/dot-ai)

## Problem

The project has no CI/CD automation. Tests don't run on PRs, there's no automated release process, and no security scanning. Contributors can merge broken code, and releasing requires manual steps.

## Solution

Create GitHub Actions workflows following the dot-ai project's approach, adapted for a Rust project:

1. **CI workflow** on PRs: lint, build, test
2. **Release workflow** on tags: build release binaries, generate changelog, create GitHub release
3. **Supporting workflows**: PR labeler, stale issue management

### CI Workflow (`.github/workflows/ci.yml`)

**Triggers**: Pull requests to `main` (opened, synchronize, reopened), manual dispatch

**Jobs**:

- **build**: Lint, build, and test
  - `cargo fmt --check` — enforce consistent formatting
  - `cargo clippy -- -D warnings` — catch common mistakes
  - `cargo build --release` — verify compilation
  - `cargo test` — run all unit and integration tests

- **security**: Code quality and dependency audit
  - `cargo audit` — check for known vulnerabilities in dependencies

### Release Workflow (`.github/workflows/release.yml`)

**Triggers**: Tags matching `v*`, manual dispatch with version input

**Jobs**:

- **prepare**: Extract version, build changelog from fragments
  - Extract version from tag (strip `v` prefix)
  - Run changelog generation from `changelog.d/` fragments (using a simple script or towncrier equivalent for Rust)
  - Commit and push changelog updates to main

- **build**: Build release binaries for multiple platforms
  - Build matrix: `linux-amd64`, `linux-arm64`, `darwin-amd64` (Intel), `darwin-arm64` (Apple Silicon), `windows-amd64`
  - Use `cross` for cross-arch Linux and Windows builds, native runners for macOS
  - Upload raw binaries as artifacts (e.g., `dot-agent-deck-linux-amd64`, `dot-agent-deck-windows-amd64.exe`)

- **finalize**: Create GitHub release and publish packages
  - Download all binary artifacts
  - Generate SHA256 checksums (`checksums.txt`)
  - Create GitHub release with changelog notes, binaries, and checksums
  - Generate and publish Homebrew formula to `vfarcic/homebrew-tap`
  - Generate and publish Scoop manifest to `vfarcic/scoop-bucket`

### PR Labeler (`.github/workflows/labeler.yml`)

**Triggers**: `pull_request_target` events

**Labels based on changed files**:
- `documentation`: `docs/**`, `*.md`, `README*`
- `source`: `src/**`
- `tests`: `tests/**`
- `ci-cd`: `.github/workflows/**`, `Dockerfile*`
- `dependencies`: `Cargo.toml`, `Cargo.lock`
- `config`: `*.toml`, `*.json`, `*.yaml`, `*.yml`

### Stale Issue/PR Management (`.github/workflows/stale.yml`)

**Triggers**: Daily cron schedule

**Configuration**:
- Issues: stale after 60 days, close after 7 more days
- PRs: stale after 30 days, close after 7 more days
- Exempt labels: `pinned`, `security`, `PRD`

## Changelog Process

Following dot-ai's approach:
- Changelog fragments stored in `changelog.d/` as individual markdown files
- Fragment naming: `{issue-or-pr-number}.{type}.md` where type is `added`, `changed`, `fixed`, `removed`
- Release workflow assembles fragments into `CHANGELOG.md` and clears `changelog.d/`
- Assembled changelog section used as GitHub release notes

## Non-Goals (v1)

- Docker image builds (no Dockerfile exists yet)
- Helm chart publishing (not a Kubernetes-deployed app)
- npm/crate publishing (evaluate later)
- Fork PR testing workflow (can add when external contributors appear)

## Milestones

- [x] CI workflow: cargo fmt, clippy, build, and test on PRs
- [x] Security workflow: cargo audit for dependency vulnerabilities
- [x] Changelog infrastructure: `changelog.d/` fragments and assembly script
- [x] Release workflow: multi-platform binary builds on tag push
- [x] Release workflow: GitHub release creation with changelog and binary attachments
- [x] Supporting workflows: PR labeler and stale issue/PR management
- [x] Release workflow: Windows binary build (`x86_64-pc-windows-gnu`)
- [x] Release workflow: SHA256 checksums for all binaries
- [x] Release workflow: Homebrew formula generation and publish to `vfarcic/homebrew-tap`
- [x] Release workflow: Scoop manifest generation and publish to `vfarcic/scoop-bucket`
- [x] Taskfile.yml with distribution tasks (checksums, homebrew, scoop)

## Success Criteria

- Every PR runs lint + build + test automatically; broken PRs are clearly flagged
- Pushing a `v*` tag produces a GitHub release with binaries for Linux, macOS, and Windows (amd64 + arm64 where applicable)
- Release includes SHA256 checksums for all binaries
- Release notes are auto-generated from changelog fragments
- Homebrew formula published to `vfarcic/homebrew-tap` on each release
- Scoop manifest published to `vfarcic/scoop-bucket` on each release
- Stale issues/PRs are automatically managed
- PRs are auto-labeled based on changed files
