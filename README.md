# dot-agent-deck

A terminal dashboard for monitoring and controlling multiple AI coding agent sessions.

[![CI](https://github.com/vfarcic/dot-agent-deck/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/vfarcic/dot-agent-deck/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/vfarcic/dot-agent-deck)](https://github.com/vfarcic/dot-agent-deck/releases/latest)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

## Features

- **Real-time session monitoring** — see status, active tool, working directory, and last prompt for every agent session
- **Pane control** — create, focus, close, and rename agent panes without leaving the dashboard
- **Keyboard-driven interface** — vim-style navigation with single-key actions
- **Auto-installed hooks** — one command registers all required hooks

Currently supports **Claude Code** and **OpenCode**. Want support for your favorite TUI agent? [Open an issue](https://github.com/vfarcic/dot-agent-deck/issues/new) and let us know!

## Quick Start

```bash
# 1. Install dot-agent-deck
brew tap vfarcic/tap && brew install dot-agent-deck

# 2. Register agent hooks
dot-agent-deck hooks install                    # Claude Code
dot-agent-deck hooks install --agent opencode   # OpenCode

# 3. Launch the dashboard
dot-agent-deck
```

Once the dashboard is running, press `?` inside the app to see all shortcuts.

## Documentation

Full documentation is available in the [`docs/`](docs/) directory, covering:

- [Getting Started](docs/getting-started.md) — hook setup, launching, basic workflow
- [Installation](docs/installation.md) — platform support, Homebrew, binary download, build from source
- [Session Management](docs/session-management.md) — session statuses, resuming sessions
- [Keyboard Shortcuts](docs/keyboard-shortcuts.md) — all shortcuts for dashboard, directory picker, panes
- [Configuration](docs/configuration.md) — default command, environment variables

## License

[MIT](LICENSE)
