# PRD #4: Comprehensive Documentation

**Status**: Not Started
**Priority**: Medium
**GitHub Issue**: [#4](https://github.com/vfarcic/dot-agent-deck/issues/4)
**Depends on**: PRD #1 (Agent Status Dashboard), PRD #2 (Pane Control)

## Problem

The project has only a one-line README. New users have no way to understand how to install, configure, or use dot-agent-deck without reading the source code. This limits adoption and makes it difficult for contributors to get started.

## Solution

Create comprehensive user-facing documentation that covers the full user journey: from installation through daily usage, plus contributor guidance. Documentation lives in the repository as markdown files, with the README as the primary entry point linking to detailed docs.

### Documentation Structure

```
README.md                      # Project overview, quick start, badges, links
docs/
  installation.md              # Prerequisites, build from source, binary install
  getting-started.md           # First run walkthrough, hook setup, launching
  configuration.md             # Environment variables, socket path, hook config
  architecture.md              # System design, event flow, component overview
  keyboard-shortcuts.md        # Complete keybinding reference (dashboard + Zellij)
CONTRIBUTING.md                # Development setup, testing, PR guidelines
```

### README.md Rewrite

The README should serve as the landing page:
- Project description with a terminal screenshot/recording (GIF or asciinema)
- Feature highlights (real-time monitoring, pane control, keyboard-driven)
- Quick start (3-5 steps to get running)
- Links to detailed docs
- Badge section (CI status, license, version)

### docs/installation.md

- Prerequisites: Rust toolchain, Zellij (optional but recommended)
- Build from source: `cargo build --release`
- Binary location and PATH setup
- Verify installation: `dot-agent-deck --help`

### docs/getting-started.md

- First launch walkthrough
- How Claude Code hooks get installed automatically
- Understanding the two-column layout
- Basic workflow: start dashboard, open agents, monitor status
- What the different session statuses mean

### docs/configuration.md

- Socket path configuration
- Claude Code hook integration details
- Zellij keybinding customization
- Environment variables reference

### docs/architecture.md

- High-level system diagram (event flow from Claude Code hooks through daemon to UI)
- Component overview: daemon, hook parser, state manager, UI renderer, pane controller
- Event schema and types
- Multiplexer abstraction (PaneController trait)
- IPC via Unix domain socket

### docs/keyboard-shortcuts.md

- Dashboard navigation keys
- Pane control keys
- Zellij-level shortcuts (Alt+key)
- Filter and search usage
- Help overlay

### CONTRIBUTING.md

- Development environment setup (devbox, Rust toolchain)
- Running tests: `cargo test`
- Project structure overview
- PR guidelines and commit message conventions
- Code style (rustfmt, clippy)

## Non-Goals (v1)

- Auto-generated API/rustdoc documentation (can be added later)
- Hosted documentation site (GitHub markdown is sufficient for now)
- Video tutorials
- Internationalization / translations
- Man pages

## Milestones

- [ ] README rewrite with project overview, quick start, feature highlights, and doc links
- [ ] Installation and getting-started guides covering full first-run experience
- [ ] Configuration and architecture documentation
- [ ] Keyboard shortcuts reference (dashboard + Zellij)
- [ ] CONTRIBUTING.md with development setup, testing, and PR guidelines
- [ ] Terminal screenshot or asciinema recording embedded in README

## Success Criteria

- A new user can go from zero to a running dashboard by following the docs alone
- All keyboard shortcuts are documented in one place
- Architecture docs give contributors enough context to understand the codebase
- README clearly communicates what the project does and how to get started
