# PRD #42: Native Windows Support

**Status**: Not Started
**Priority**: Medium
**Created**: 2026-04-05

## Problem

dot-agent-deck only runs on macOS and Linux. Windows users must use WSL, which adds friction (extra setup, filesystem bridging, terminal quirks) and reduces discoverability — users who find the project may not realize they need WSL or may not have it configured.

The codebase has three categories of Unix-specific code:

1. **Unix domain sockets** (`src/daemon.rs`, `src/hook.rs`) — `tokio::net::UnixListener`/`UnixStream` and `std::os::unix::net::UnixStream` for all daemon IPC (event ingestion, permission responses). These do not exist on native Windows.
2. **`/dev/tty`** (`src/ui.rs:copy_to_clipboard_osc52`) — direct TTY write for OSC 52 clipboard sequences. Windows uses `conout$` instead.
3. **PTY signal handling** — `portable-pty` already supports Windows, but SIGWINCH (terminal resize) is Unix-specific. Windows uses console event notifications instead.

## Solution

Make dot-agent-deck compile and run natively on Windows without WSL, while preserving identical behavior on macOS and Linux.

### Approach

Use compile-time `cfg` guards to abstract platform-specific code behind a common interface:

1. **IPC layer** — extract socket creation/connection into a platform module:
   - Unix: keep `UnixListener`/`UnixStream` (no change)
   - Windows: use Tokio named pipes (`tokio::net::windows::named_pipe`) for the daemon, synchronous named pipe client for hooks
   - Both expose the same async read/write stream interface

2. **Clipboard** — add `conout$` fallback in `copy_to_clipboard_osc52`:
   - Try `/dev/tty` (Unix) or `conout$` (Windows) via `cfg`
   - No external clipboard crate needed — OSC 52 works in Windows Terminal and most modern terminals

3. **Terminal resize** — `portable-pty` handles Windows PTY resize internally via `ConPTY`. No SIGWINCH needed. Verify `crossterm::event::Event::Resize` fires correctly on Windows (it does via Console API).

4. **CI & releases** — add Windows targets to build matrix, ship `.exe` binaries

### What stays the same

- All TUI rendering (ratatui + crossterm are cross-platform)
- PTY management (portable-pty supports Windows via ConPTY)
- Hook system architecture (just the transport changes)
- Dashboard UI, keybindings, configuration
- All existing macOS/Linux behavior

## Technical Design

### IPC Abstraction

```
src/
  ipc/
    mod.rs          — public traits: IpcListener, IpcStream
    unix.rs         — UnixListener/UnixStream impl (cfg(unix))
    windows.rs      — NamedPipeServer/NamedPipeClient impl (cfg(windows))
```

**Daemon side** (`src/daemon.rs`):
- Replace `UnixListener::bind(path)` with `IpcListener::bind(path)`
- On Unix: path is the socket file (unchanged)
- On Windows: path becomes `\\.\pipe\dot-agent-deck-{user}`

**Hook side** (`src/hook.rs`):
- Replace `UnixStream::connect(path)` with `IpcStream::connect(path)`
- Same path convention as daemon

**Socket path** (`src/config.rs`):
- Unix: `$XDG_RUNTIME_DIR/dot-agent-deck.sock` or `/tmp/dot-agent-deck.sock` (unchanged)
- Windows: `\\.\pipe\dot-agent-deck-{username}`

### Clipboard

```rust
fn open_tty_write() -> Option<std::fs::File> {
    #[cfg(unix)]
    { std::fs::OpenOptions::new().write(true).open("/dev/tty").ok() }
    #[cfg(windows)]
    { std::fs::OpenOptions::new().write(true).open("CONOUT$").ok() }
}
```

### CI Changes

Add to `.github/workflows/ci.yml` build matrix:
- `x86_64-pc-windows-msvc` on `windows-latest`
- `aarch64-pc-windows-msvc` on `windows-latest` (cross-compile)

Add to `.github/workflows/release.yml` build matrix:
- Same targets, producing `.exe` binaries

### Installation

- **Scoop**: already have a manifest pipeline — add Windows binary URLs
- **Winget**: future enhancement, not required for v1
- **Direct download**: `.exe` from GitHub Releases

## Edge Cases

- Named pipe permissions — use default security descriptor (current user only)
- Multiple instances — named pipe name includes a unique suffix (PID or session) if needed; or fail-fast like Unix socket `bind` error
- Antivirus — Windows Defender may flag unsigned `.exe`; consider code signing in future
- Windows Terminal vs legacy cmd.exe — OSC 52 only works in Windows Terminal / modern terminals; legacy conhost silently ignores it
- Path separators — ensure socket/config path logic uses `std::path` consistently (already the case)

## Milestones

- [ ] IPC abstraction layer — extract Unix socket code behind trait, add Windows named pipe implementation, both compile-gated with `cfg`. Daemon and hooks work on both platforms.
- [ ] Clipboard cross-platform — `conout$` fallback for OSC 52 on Windows. Works in Windows Terminal.
- [ ] CI builds for Windows — add `x86_64-pc-windows-msvc` to CI and release workflows. Tests pass on Windows runner.
- [ ] Release binaries — Windows `.exe` artifacts published in GitHub Releases. Scoop manifest updated.
- [ ] Documentation — README updated with Windows installation instructions. Platform support section added.
- [ ] End-to-end validation — full workflow tested on Windows: install, hook registration, dashboard launch, pane creation, agent monitoring, clipboard copy.

## Out of Scope (v1)

- Winget package submission
- Code signing for Windows binaries
- Windows-specific installer (MSI/NSIS)
- Legacy cmd.exe / conhost support for OSC 52
- PowerShell module packaging
