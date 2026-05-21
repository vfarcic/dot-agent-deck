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

## How it runs

The first time you run `dot-agent-deck`, the binary auto-spawns a small per-user background daemon and connects to it over a Unix socket (under `$XDG_RUNTIME_DIR` when available, otherwise a per-uid path in `/tmp`). The same daemon is used for both local and remote (`dot-agent-deck connect`) sessions; there is no separate "local mode".

The daemon outlives the TUI: detach the deck, your agents keep running, reattach later and they're still there. About 30 seconds after the TUI has detached *and* every managed agent is gone, the daemon exits on its own. Set `DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS` to override the window (`0` keeps it up indefinitely).
