---
sidebar_position: 3
title: Installation
---

# Installation

## Platform Support

| Platform | Status |
|---|---|
| macOS (Intel & Apple Silicon) | Supported |
| Linux (amd64 & arm64) | Supported |
| Windows (via WSL) | Supported (runs as Linux) |
| Windows (native) | Coming soon ([#42](https://github.com/vfarcic/dot-agent-deck/issues/42)) — comment on the issue if you need this! |

## Homebrew (macOS / Linux)

```bash
brew tap vfarcic/tap
brew install dot-agent-deck
```

## Download Binary

Download the latest binary for your platform from the [Releases](https://github.com/vfarcic/dot-agent-deck/releases/latest) page. Binaries are available for Linux (amd64, arm64) and macOS (amd64, arm64).

## Build from Source

```bash
git clone https://github.com/vfarcic/dot-agent-deck.git
cd dot-agent-deck
cargo build --release
```

The binary will be at `target/release/dot-agent-deck`.

## Verify

```bash
dot-agent-deck --help
```

## Installing on a Remote Host

If you plan to run Agent Deck on a per-project remote Linux VM (so agents survive laptop sleep and network drops), the install on the remote is handled automatically by `dot-agent-deck remote add` from the laptop — you don't install on the remote by hand. See [Remote Environments](remote-environments.md), [Requirements](remote-requirements.md), and [Recipes](remote-recipes.md) for the full path.
