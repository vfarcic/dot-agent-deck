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

## Upgrading

After upgrading the `dot-agent-deck` binary, just relaunch it:

```bash
dot-agent-deck
```

On every launch, the TUI performs a build-version handshake with the running daemon. If a daemon spawned by the previous version is still alive, the TUI detects the mismatch and prompts you in your terminal (the prompt itself names both build IDs and, when managed agents are running, lists them before you confirm). Press **S** to stop the stale daemon and continue — the TUI lazy-spawns a fresh one at the new version and continues into the dashboard. No separate command needed.

If the TUI is not attached to a terminal (CI, scripts, piped stdout), it cannot prompt, so it prints a recovery hint to stderr and exits non-zero. In that case, run `dot-agent-deck daemon stop` explicitly before relaunching — see [Recycling the local daemon](#recycling-the-local-daemon) below.

See [Troubleshooting › Delegate prompts silently no-op after an upgrade](troubleshooting.md#delegate-prompts-silently-no-op-after-an-upgrade) for the symptom you'll see if you ever connect to a stale daemon without seeing the prompt first.

## Recycling the local daemon

`dot-agent-deck daemon stop` shuts down the running daemon gracefully. Use it after a binary upgrade or any time you want to start a fresh daemon process.

```bash
dot-agent-deck daemon stop
```

- **Idempotent.** If no daemon is running, the command prints `no daemon running` and exits 0.
- **Data-loss guard.** If managed agents are still alive, the command refuses with a list of agent IDs and exits non-zero — terminating the daemon would kill their PTYs. Detach the agents first (close their panes, or quit the TUI to detach the deck while keeping the agents running), or pass `--force`.
- **Grace window.** Sends `SIGTERM` and polls for the socket to disappear for up to 5 seconds. With `--force`, escalates to `SIGKILL` after that window. A `SIGTERM` timeout without `--force` exits non-zero so you can re-run with `--force` consciously.

```bash
# Force shutdown even when managed agents are running. This kills the agents.
dot-agent-deck daemon stop --force
```

`dot-agent-deck daemon restart` is a thin wrapper: it runs `daemon stop`, then returns. The next `dot-agent-deck` invocation lazy-spawns a fresh daemon (see [How it runs](#how-it-runs) above). `--force` works the same way.

```bash
dot-agent-deck daemon restart
```

> Stopping a *remote* daemon works differently — each remote attach has its own per-host daemon, governed by the lifecycle in [Remote Environments](remote-environments.md). The local `daemon stop` only touches the daemon on this machine.
