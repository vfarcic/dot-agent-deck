# dot-agent-deck

A terminal dashboard for monitoring and controlling multiple AI coding agent sessions — with optional AI-generated ASCII art for idle sessions.

[![CI](https://github.com/vfarcic/dot-agent-deck/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/vfarcic/dot-agent-deck/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/vfarcic/dot-agent-deck)](https://github.com/vfarcic/dot-agent-deck/releases/latest)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

## Quick Start

```bash
brew tap vfarcic/tap && brew install dot-agent-deck
dot-agent-deck hooks install
dot-agent-deck
```

## Desktop GUI (early developer preview)

An optional desktop control room for the same daemon lives in [`desktop/`](desktop/). It is an early preview and is not part of the release artifacts.

Try it in your browser in under a minute — no Rust toolchain, no daemon, no API cost. Fixture mode renders a fully simulated run (agents, terminals, metrics, evidence) so you can explore the whole UI safely:

```bash
cd desktop && pnpm install && pnpm dev
# then open http://localhost:1420/?fixture=1
```

To run it for real against a local daemon: `pnpm tauri dev` (requires Rust; see [docs/develop/desktop-gui.md](docs/develop/desktop-gui.md)).

## Documentation

For installation guides, configuration, keyboard shortcuts, and more, visit the documentation site:

**[Agent Deck](https://agent-deck.devopstoolkit.ai)**

## License

[MIT](LICENSE)
