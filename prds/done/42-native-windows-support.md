# PRD #42: Native Windows Support

**Status**: Complete
**Priority**: Medium
**Created**: 2026-04-05
**Completed**: 2026-06-14

## Scope expansion (2026-06-14)

This PRD was re-validated and its scope materially expanded on 2026-06-14. The original draft listed only three Unix-specific categories (Unix domain sockets, `/dev/tty`, SIGWINCH). A full code investigation against the post-#76/#93 daemon refactor found **nine** categories that need platform work, spanning ~8 source files for the IPC layer alone, plus three build/plumbing items the original draft never mentioned. The sections below reflect the expanded scope. The original validation-refresh note that prompted this expansion is preserved at the bottom of the document for history. A detailed design investigation (file:function inventory, per-category Win32 mappings, behavior-equivalence risks) lives in the branch scratch report and is summarized here.

### Decisions locked for v1

- **IPC transport**: Windows **named pipes** (not AF_UNIX). Named pipes have no on-disk inode, which removes the entire stale-socket class of bug; AF_UNIX on Windows would still need a non-tokio code path and still lacks peer-PID-by-socket.
- **Win32 binding crate**: `windows-sys` (raw, light, fast to compile) for security descriptors, Job Objects, `GetNamedPipeServerProcessId`, and locks.
- **Path resolution**: the `dirs` crate for `%LOCALAPPDATA%`/`%APPDATA%`/`%USERPROFILE%`; existing `DOT_AGENT_DECK_*` env overrides remain authoritative on all platforms.
- **v1 behavior-equivalence**: best-effort agent graceful-shutdown on Windows (`CTRL_BREAK_EVENT` grace window then hard `TerminateJobObject`); per-user `%LOCALAPPDATA%` ACL + a current-user-only pipe security descriptor in place of replicating every `0o600`/`O_NOFOLLOW`/uid-check site. The differences from Unix are documented rather than papered over.
- **Targets**: `x86_64-pc-windows-msvc` and `aarch64-pc-windows-msvc` (cross-compile) on `windows-latest`.
- **Shipping**: a normal platform target, visible by default on Windows — *not* behind the `experimental` feature flag (CLAUDE.md #9 does not apply; Windows support is a platform port, not a new in-app user-visible surface).
- **Pipe-name namespacing**: per-user (the Win32 analogue of the per-uid `/tmp` suffix). Per-session / multi-desktop isolation is out of scope for v1.
- **Validation strategy**: a `windows-latest` GitHub Actions job lands early as the continuous build/clippy/test gate; a cloud Windows VM (Windows Terminal) covers the final interactive e2e pass (clipboard, ConPTY resize repaint, daemon-survives-exit, full workflow). aarch64 is covered by CI build; interactive validation focuses on x86_64.

## Problem

dot-agent-deck only runs on macOS and Linux. Windows users must use WSL, which adds friction (extra setup, filesystem bridging, terminal quirks) and reduces discoverability — users who find the project may not realize they need WSL or may not have it configured.

The codebase has accumulated nine categories of Unix-specific code (the post-daemon-refactor reality, not the three the original draft listed), plus three build/plumbing assumptions:

1. **Unix domain sockets** — `tokio::net::UnixListener`/`UnixStream` and `std::os::unix::net::UnixStream` across ~8 files in a **dual-socket** layout: a hook-ingestion socket (`config::socket_path`) and a binary TUI-attach socket (`config::attach_socket_path`). Consumers: `daemon.rs` (bind/probe), `daemon_protocol.rs` (attach bind/serve), `daemon_client.rs` (async client + split halves), `daemon_stop.rs`, `build_version_handshake.rs`, `hook.rs` (sync client), `ui.rs` (sync client). These types are `cfg(unix)`-only in tokio/std and do not exist on native Windows.
2. **Daemon process detach** — `libc::setsid()` via `CommandExt::pre_exec` in `daemon_attach.rs` makes the lazy-spawned `daemon serve` a session leader so it survives the parent shell's exit/SIGHUP. `setsid`/fork/`pre_exec` have no Windows equivalent.
3. **Spawn serialization / file locking** — `flock(LOCK_EX)` on `spawn.lock` (`daemon_attach.rs`, reused by `daemon.rs` lock-root) serializes concurrent first-attaches so only one process races the bind. `flock(2)` is POSIX.
4. **Peer-credential / peer-PID resolution** — `getsockopt(SO_PEERCRED)` (Linux) / `LOCAL_PEERPID` (macOS) in `daemon_attach.rs::peer_pid()` reads the connected peer's PID with zero protocol bytes (works against any daemon version); consumed by `daemon_stop.rs` and `build_version_handshake.rs` for SIGTERM targeting. Both are OS-specific socket options absent on Windows.
5. **Process lifecycle signals** — `libc::killpg`/`kill` (`agent_pty.rs`, `build_version_handshake.rs`): SIGTERM→poll→SIGKILL escalation for agent teardown (whole process group) and daemon stop. Signals, process groups, and `kill(2)` do not exist on Windows.
6. **Filesystem security model** — `umask`/mode-bits/`O_NOFOLLOW`/uid-verification: the `umask(0o177)` bind dance (`daemon.rs`), `verify_socket_trusted` owner/mode/type check (`daemon_attach.rs`), `O_NOFOLLOW` daemon-log open, `0o700`/`0o600` dirs and atomic config writes (`daemon_attach.rs`, `daemon.rs`, `daemon_protocol.rs`, `remote.rs`, `schedule_cli.rs`). All Unix mode-bit primitives.
7. **Clipboard** — `copy_to_clipboard_osc52` (`ui.rs`) writes the OSC 52 escape directly to `/dev/tty`, bypassing ratatui's captured stdout. `/dev/tty` does not exist on Windows; the console equivalent is `CONOUT$`.
8. **`libc` dependency** — `Cargo.toml` declares `libc` unconditionally (and as a dev-dep). `libc` compiles on Windows but the symbols above (`setsid`/`flock`/`getsockopt`/`SO_PEERCRED`/`killpg`/`umask`/`getuid`/`getppid`) are undefined there, so every use must be `cfg(unix)`-isolated.
9. **Home-dir + shell assumptions** — `config::dirs_home()` resolves `$HOME` only; `agent_pty.rs` wraps multi-word commands in `/bin/sh -c` and falls back to `$SHELL`/`/bin/sh`. Windows needs `%USERPROFILE%`/`%LOCALAPPDATA%`/`%APPDATA%` and `%COMSPEC%`/`cmd /C` (or PowerShell).

**Not a problem (verified):** terminal resize. The codebase contains no SIGWINCH handling — resize is entirely `crossterm::event::Event::Resize` (Console API on Windows) plus `portable_pty::MasterPty::resize()` (ConPTY on Windows). Both are already cross-platform; no code change is required, only Windows-runner verification that ConPTY resize propagates.

## Solution

Make dot-agent-deck compile and run natively on Windows without WSL, while preserving **identical** behavior on macOS and Linux. The mechanism is a single `src/platform/` module tree that hides every Unix-specific mechanism behind a `cfg`-dispatched API: the Unix backend is a behavior-for-behavior move of today's code, and a Windows backend is added alongside it. Each platform compiles exactly one backend per category.

### What stays the same

- All TUI rendering (ratatui + crossterm are cross-platform).
- PTY management (portable-pty supports Windows via ConPTY).
- Hook-system and attach-protocol *architecture* and wire framing — only the transport changes.
- Dashboard UI, keybindings, configuration.
- All existing macOS/Linux behavior (the Unix backends are lifts of current code; the existing test suites must stay green with zero behavior change).

## Technical Design

### Module structure

Replace the originally-proposed sockets-only `src/ipc/` with a full `src/platform/` tree. Each submodule follows the same shape: `mod.rs` (cfg-dispatched public API), `unix.rs` (`#![cfg(unix)]`, lifts current code), `windows.rs` (`#![cfg(windows)]`).

```
src/platform/
  mod.rs        — re-exports; the single `use crate::platform::*` seam
  ipc/          — IpcListener / IpcStream (AsyncRead+AsyncWrite+into_split) + sync IpcClient + endpoint-name resolution
  detach/       — spawn_detached(cmd, log)
  lock/         — SpawnLock (RAII) + acquire_spawn_lock(path) (signatures preserved)
  peercred/     — peer_pid(&IpcStream) (server + client variants)
  proc/         — terminate_pid_graceful, terminate_agent_tree (killpg analogue), current_ppid (cfg(unix), test-gated)
  fsperm/       — restrict_dir_owner_only, create_owner_only_file, verify_endpoint_trusted
  paths.rs      — home/runtime/state/config-root resolution (replaces config::dirs_home, current_uid)
  shell.rs      — default-shell + command-wrap policy (replaces inline agent_pty logic)
```

**Most load-bearing abstraction — `IpcStream`.** Must be `AsyncRead + AsyncWrite + Unpin + Send` and expose `into_split()` returning associated read/write half types, because `daemon_client.rs`'s protocol helpers are written against `tokio::net::unix::{OwnedReadHalf,OwnedWriteHalf}`. Using `tokio::io::split` over a boxed stream is the portable way to get halves that work for both `UnixStream` and named pipes. Use named pipes in **byte mode** (`PipeMode::Byte`) so the existing length-prefixed framing is unchanged.

### Per-category Windows mapping

| Category | Unix (preserved) | Windows |
|---|---|---|
| IPC | UnixListener/UnixStream, dual socket files | `tokio::net::windows::named_pipe` server/client (per-instance accept loop, re-create next instance to keep one pending), `ERROR_PIPE_BUSY` retry, byte mode; sync hook/ui client via file-open on `\\.\pipe\…`. Pipe names `\\.\pipe\dot-agent-deck-{user}-hook` / `-attach`. No stale-inode handling needed. |
| Detach | `setsid` + `pre_exec` + `/dev/null` stdin | `creation_flags(DETACHED_PROCESS \| CREATE_NEW_PROCESS_GROUP)` + `NUL` stdin; handle `CREATE_BREAKAWAY_FROM_JOB` if launched inside a kill-on-close Job Object. |
| Lock | `flock(LOCK_EX/UN)` on `spawn.lock` | named mutex `Global\dot-agent-deck-spawn-{user}` (idiomatic cross-process mutex; doubles as singleton-daemon guard), `WAIT_ABANDONED` treated as "acquired, prior owner crashed". RAII shape preserved. |
| Peer-PID | `getsockopt(SO_PEERCRED/LOCAL_PEERPID)` | `GetNamedPipeServerProcessId` (client learns daemon PID) / `GetNamedPipeClientProcessId`. Same zero-protocol-bytes, any-version property. |
| Signals | `kill`/`killpg` SIGTERM→SIGKILL | `daemon stop` routes through the existing `KIND_SHUTDOWN`/ACK protocol then escalates to `TerminateProcess`; agent teardown assigns each agent to a Job Object and `TerminateJobObject` reaps descendants; best-effort `CTRL_BREAK_EVENT` for the grace window. |
| FS security | `umask`/mode-bits/`O_NOFOLLOW`/uid-check | current-user-only pipe security descriptor (replaces socket `0o600` + `verify_socket_trusted` in one stroke); rely on per-user `%LOCALAPPDATA%` ACL for dirs (optional explicit `SetNamedSecurityInfo`). |
| Clipboard | `/dev/tty` OSC 52 | `CONOUT$` OSC 52 (small inline `cfg`, no module needed). |
| Resize | crossterm `Event::Resize` + portable-pty | **no code change** — already cross-platform; verify ConPTY propagation on a runner. |
| Paths | `$HOME`/`$XDG_RUNTIME_DIR`, `getuid` suffix | `dirs` crate → `%LOCALAPPDATA%`/`%APPDATA%`/`%USERPROFILE%`; `{user}` pipe suffix. |
| Shell | `$SHELL`/`/bin/sh -c` | `%COMSPEC%`/`cmd /C`. |
| `libc` dep | `[dependencies] libc` | move to `[target.'cfg(unix)'.dependencies]`; add `windows-sys`/`dirs` under `[target.'cfg(windows)'.dependencies]`. |

### Behavior-equivalence risks (explicit)

- **Graceful agent shutdown is weaker on Windows.** A SIGTERM trap (an agent flushing state / reaping sub-shells) cannot be faithfully reproduced; `CTRL_BREAK_EVENT` is honored inconsistently by console apps. v1 ships best-effort + hard `TerminateJobObject`, documented.
- **Detached-daemon survival** depends on *not* inheriting a kill-on-job-close Job Object — needs Windows-runner confirmation and possibly `CREATE_BREAKAWAY_FROM_JOB`.
- **Security model splits** from uniform Unix mode-bits into "pipe SD + dir ACL + `%LOCALAPPDATA%` defaults." Each `cfg(unix)` permission site must be audited individually, not blanket-`cfg`'d away, or a security property is silently lost.
- **Named-mutex vs flock**: mutex is abandoned (not auto-released) on owner death; callers must handle `WAIT_ABANDONED`.
- **ConPTY resize** has historically had repaint quirks — verify on a runner.

### Testability split (this worktree is Linux-only)

- **Locally testable here** (compile + `clippy -D warnings` + `cargo test-fast`, and pre-PR `cargo test-e2e`): the entire `platform/` refactor and all Unix backends — proving **zero Linux regression**. `cargo check --target x86_64-pc-windows-msvc` (target installed, no link) type-checks the Windows branches and catches missing-symbol / cfg-coverage mistakes without a runner.
- **Requires Windows CI / VM** (cannot be validated here): named-pipe accept/serve + framing; detached-daemon-survives-exit; lock serialization + `WAIT_ABANDONED`; `GetNamedPipeServerProcessId`; graceful + force daemon stop; Job-Object descendant reaping + `CTRL_BREAK_EVENT`; pipe SD / dir ACL denial; `CONOUT$` clipboard in Windows Terminal; ConPTY resize; the full e2e workflow; that the `.exe` runs.

### CI & releases

- `ci.yml` is currently a single `ubuntu-latest` job — add a `windows-latest` job (build + clippy + test) and land it **early** (right after the IPC abstraction) so the Windows branches are continuously compiled/run rather than discovered broken at the end. Bash-idiom steps stay on Linux or use a portable shell.
- `release.yml` already parameterizes `binary_ext`/`artifact_suffix`/`use_cross` and publishes a Scoop manifest — add the two `*-pc-windows-msvc` matrix rows producing `.exe`; portable-shell the bash `Build`/`Package` steps for Windows.

### Installation

- **Scoop**: existing manifest pipeline — add Windows binary URLs.
- **Direct download**: `.exe` from GitHub Releases.
- **Winget**: future enhancement, not required for v1.

## Phasing — this PRD vs follow-ups

The full effort is split across three PRDs so each is an independently shippable, reviewable unit. **This PRD (#42) holds the canonical architecture/design above and delivers the foundation; the follow-ups implement on top of it and reference back here for design detail.**

- **#42 (this PRD) — Foundation**: M1 + M2 + M8. The `platform/` seam, `libc` gating, the IPC abstraction (the single biggest category, 8 files), and the `windows-latest` CI job that proves the Windows branches compile and pass tests.
- **[#163](163-windows-platform-backends.md) — Process, path & filesystem backends**: M3–M7 (paths/shell, detach/lock, peer-PID/lifecycle, fsperm, clipboard).
- **[#164](164-windows-release-e2e-docs.md) — Release, e2e & docs**: M9–M11 (`.exe`/Scoop, interactive e2e on a Windows VM, README/platform docs).

## Milestones (this PRD — Foundation)

Tagged **[Linux-testable]** (a behavior-preserving refactor validatable on this Linux-only worktree) or **[Windows-runner]** (Windows behavior written/`cargo check`'d on Linux but *validated* on `windows-latest`). The Unix half of every Windows-runner item is a behavior-preserving move this worktree can prove regression-free.

- [x] **M1 — `platform/` seam + `libc` gating** [Linux-testable]: create `src/platform/`; move every `libc::`/`std::os::unix` call behind `platform::*::unix`; gate `libc` to `cfg(unix)`. Zero Linux behavior change; existing suite green.
- [x] **M2 — IPC abstraction** [Linux-testable for Unix backend]: extract `IpcListener`/`IpcStream`/`IpcClient`; port the 8 socket files to it (Unix backend = current code). Windows named-pipe backend written here, validated on a runner.
- [x] **M8 — Windows CI job** [Windows-runner]: add `x86_64-pc-windows-msvc` build+clippy+test on `windows-latest`. Land it with M2 so the Windows IPC backend is continuously compiled and run.

### Deferred to follow-up PRDs

- **#163**: M3 paths+shell, M4 detach+lock, M5 peer-PID+lifecycle, M6 filesystem security, M7 clipboard `CONOUT$`.
- **#164**: M9 release binaries, M10 resize/ConPTY validation + e2e, M11 docs.

## Edge Cases

- Named pipe permissions — current-user-only security descriptor.
- Multiple instances / singleton daemon — named mutex replaces the Unix stale-inode bind race; no `remove_file` needed.
- `WAIT_ABANDONED` on the spawn mutex — treat as acquired-after-crash.
- Detached daemon inside a kill-on-job-close Job Object — may need `CREATE_BREAKAWAY_FROM_JOB`.
- Antivirus — Windows Defender may flag unsigned `.exe`; code signing is future work.
- Windows Terminal vs legacy cmd.exe — OSC 52 only works in Windows Terminal / modern terminals; legacy conhost silently ignores it.
- Path separators — socket/config path logic already uses `std::path`.

## Out of Scope (v1)

- Winget package submission.
- Code signing for Windows binaries.
- Windows-specific installer (MSI/NSIS).
- Legacy cmd.exe / conhost support for OSC 52.
- PowerShell module packaging.
- Per-session / multi-desktop pipe isolation (per-user only in v1).
- Faithful SIGTERM-trap graceful-shutdown parity (best-effort `CTRL_BREAK_EVENT` + `TerminateJobObject` in v1).

---

## Validation refresh (2026-04-05 → 2026-06-14) — original note, preserved for history

Re-validated against current code — verdict: **status accurate (not started), but scope materially understated**. The PRD's three Unix-specific categories predate the daemon refactor (#76/#93), which roughly **5–6×'d the Unix-only surface**. Beyond the listed sockets / `/dev/tty` / SIGWINCH, native Windows now also requires handling: `setsid(2)` + `CommandExt::pre_exec` daemon detach (`src/daemon_attach.rs`); `flock(2)`/`LOCK_EX` spawn serialization (`src/daemon_attach.rs`, `src/daemon.rs`); `SO_PEERCRED`/`LOCAL_PEERPID` peer-PID resolution used for SIGTERM targeting (`peer_pid()` in `src/daemon_attach.rs`, consumed by `src/daemon_stop.rs` and `src/build_version_handshake.rs`); `libc::kill`/`killpg` lifecycle signals (`src/agent_pty.rs`, `src/daemon_stop.rs`); and the `umask`/`O_NOFOLLOW`/mode-bit security model. `UnixListener`/`UnixStream` now spans ~8 files with a dual-socket layout (`socket_path()` for hooks, `attach_socket_path()` for TUI attach). The proposed `src/ipc/` abstraction must cover all of that. Recommend expanding the scope sections (and adding a Windows process-detach / file-lock / peer-credential plan) before any work starts. *(Addressed by the 2026-06-14 scope expansion above.)*
