# PRD #163: Windows process, path & filesystem platform backends

**Status**: Not Started
**Priority**: Medium
**Created**: 2026-06-14

> **Phase 2 of native Windows support.** This PRD depends on the platform-abstraction foundation in **[#42](done/42-native-windows-support.md)** (the `src/platform/` seam, `cfg`-gated `libc`, the IPC abstraction, and the `windows-latest` CI job). Read #42 first — it holds the canonical architecture, the per-category Unix→Windows mapping table, the behavior-equivalence risk analysis, and the locked v1 decisions. This PRD implements the remaining platform backends on top of that seam.

## Problem

After #42 lands, the `src/platform/` seam exists and IPC works cross-platform, but five Unix-only mechanism categories still have no Windows backend, so the daemon cannot fully spawn, stop, signal, secure, or share-clipboard on native Windows:

1. **Paths & shell** (`platform::paths`, `platform::shell`) — `config::dirs_home()` resolves `$HOME` only; agent spawn wraps commands in `/bin/sh -c` / `$SHELL`.
2. **Daemon detach & spawn lock** (`platform::detach`, `platform::lock`) — `setsid`+`pre_exec` session-leader detach and `flock(LOCK_EX)` spawn serialization.
3. **Peer-PID & lifecycle signals** (`platform::peercred`, `platform::proc`) — `getsockopt(SO_PEERCRED/LOCAL_PEERPID)` peer-PID and `kill`/`killpg` SIGTERM→SIGKILL teardown.
4. **Filesystem security model** (`platform::fsperm`) — `umask`/mode-bits/`O_NOFOLLOW`/uid-verification.
5. **Clipboard** — `/dev/tty` OSC 52 write.

(Terminal resize needs no code change — see #42; it is validated in #164's e2e pass.)

## Solution

Implement the Windows backend (`windows.rs`) for each `platform/` submodule introduced in #42, preserving the Unix backend byte-for-byte. Per the decisions locked in #42:

| Backend | Windows implementation |
|---|---|
| `paths` | `dirs` crate → `%LOCALAPPDATA%`/`%APPDATA%`/`%USERPROFILE%`; per-user pipe-name suffix; `DOT_AGENT_DECK_*` env overrides stay authoritative. |
| `shell` | `%COMSPEC%`/`cmd /C`. |
| `detach` | `creation_flags(DETACHED_PROCESS \| CREATE_NEW_PROCESS_GROUP)` + `NUL` stdin; handle `CREATE_BREAKAWAY_FROM_JOB` if launched inside a kill-on-job-close Job Object. |
| `lock` | named mutex `Global\dot-agent-deck-spawn-{user}` (RAII shape preserved); treat `WAIT_ABANDONED` as acquired-after-crash. Doubles as the singleton-daemon guard. |
| `peercred` | `GetNamedPipeServerProcessId` / `GetNamedPipeClientProcessId` (`windows-sys`). |
| `proc` | `daemon stop` routes through the existing `KIND_SHUTDOWN`/ACK protocol then escalates to `TerminateProcess`; agent teardown assigns each agent to a Job Object and `TerminateJobObject` reaps descendants; best-effort `CTRL_BREAK_EVENT` grace window. |
| `fsperm` | current-user-only pipe security descriptor (replaces socket `0o600` + `verify_socket_trusted`); per-user `%LOCALAPPDATA%` ACL for dirs (optional explicit `SetNamedSecurityInfo`); audit each `cfg(unix)` permission site individually. |
| clipboard | `CONOUT$` OSC 52 (inline `cfg` in `copy_to_clipboard_osc52`). |

### v1 behavior-equivalence (locked in #42)

- Graceful agent shutdown is best-effort on Windows (`CTRL_BREAK_EVENT` then hard `TerminateJobObject`) — a faithful SIGTERM-trap grace window is not reproducible for console apps. Documented difference.
- The security model splits from uniform Unix mode-bits into "pipe security descriptor + dir ACL + `%LOCALAPPDATA%` defaults"; each site is audited so no security property is silently lost.

## Security & robustness requirements carried from #42's review (release-gating)

The #42 Foundation review (auditor + reviewer) confirmed no Unix regression but flagged concrete Windows-backend gaps that #42 deliberately left as compiling stubs / no-ops. In the Foundation, the Windows daemon is **hard-failed (`Unsupported`) at the `IpcListener::bind`/serve seam** so none of these gaps are reachable at runtime. **#163 unblocks the Windows daemon, so #163 MUST close all of these — the first item is a hard gate on any Windows release (#164):**

- **[BLOCKER] Named-pipe security descriptor.** `platform/ipc/windows.rs` currently creates the pipe with the *default* security descriptor (Everyone-read DACL) and `verify_endpoint_trusted` is a Windows no-op. Combined with the predictable per-user pipe name, a foreign local user could pipe-squat — read agent terminal output / hook payloads and impersonate the daemon to capture forwarded keystrokes. **Required:** create the pipe with an explicit current-user-SID DACL (`ServerOptions::security_attributes`, `Win32_Security` feature already enabled) **and** add client-side server-SID verification, replicating the Unix owner/mode trust check. The client-side verification must cover **both** client entry points — the ui/attach client (`IpcStream::connect`/`IpcClient::connect`) **and** the hook client (`hook::send_to_socket`/`request_from_socket`); the latter is *not* daemon-gated, so on Windows it can already connect to the predictable per-user pipe name and would leak hook payloads / accept spoofed replies from a foreign-squatted pipe unless it verifies the server SID before writing. Removing the #42 Windows daemon hard-fail is contingent on this landing.
- **Config-secret file ACLs.** `set_create_mode_owner_only`/`set_file_owner_only` are Windows no-ops, so `remotes.toml`/`schedules.toml` (may carry secrets) rely solely on the `%LOCALAPPDATA%` default ACL; a `DOT_AGENT_DECK_STATE_DIR` override to a permissive dir loses the Unix `0o600` guarantee. **Required:** explicit owner-only ACL on these writes (or refuse a world-writable target dir).
- **Sync IPC client timeouts (TUI hang-risk).** `IpcClient::set_timeouts` is a Windows no-op, so the synchronous TUI request path (`ui::send_daemon_request_blocking`) has no read/write deadline vs Unix's 5s — a wedged daemon hangs the TUI key path. **Required:** implement named-pipe read/write timeouts on Windows.
- **Pipe-name user suffix collision.** `paths::endpoint_user_suffix` falls back to the literal `"user"` when `USERNAME` is unset, colliding across users (Unix uses uid, which never collides). **Required:** a non-colliding fallback (or hard error) for the Windows pipe-name suffix.
- **Stale-endpoint dance on Windows.** `run_daemon_with` / `bind_attach_listener` run the Unix `path.exists()` + `remove_file()` stale-inode dance, which is semantically wrong on a `\\.\pipe\` name (named pipes have no inode; the singleton guard is `first_pipe_instance(true)`). **Required:** short-circuit the probe/remove dance on Windows when the daemon path is unblocked.
- **`poll_daemon_gone` path-existence check (Greptile P2, PR #167).** `src/build_version_handshake.rs` (~L378–390) `poll_daemon_gone` uses `attach_path.exists()`, which returns false for a `\\.\pipe\…` name even when the pipe server is active (`GetFileAttributesW` → `ERROR_BAD_PATHNAME`) — so the first loop iteration would immediately (and wrongly) declare the daemon gone. **Required:** use a pipe-aware liveness check on Windows (e.g. attempt a client connect) rather than `path.exists()` once the daemon path is unblocked.

## Milestones

These map to #42's M3–M7. Unix halves are behavior-preserving moves (Linux-testable, prove no regression); Windows behavior requires the `windows-latest` CI job (from #42) plus the e2e VM pass (in #164).

- [ ] **paths + shell** (#42 M3) — platform-dispatch home/runtime/state/lock resolution and the shell-wrap; Unix unchanged; add Windows branches. Linux-testable + `cargo check --target`.
- [ ] **detach + lock** (#42 M4) — Windows `DETACHED_PROCESS`/Job-breakaway + named mutex; confirm on a runner the daemon survives parent exit and concurrent spawns serialize.
- [ ] **peer-PID + lifecycle** (#42 M5) — `GetNamedPipeServerProcessId`; `daemon stop` graceful→force; Job-Object agent teardown.
- [ ] **filesystem security** (#42 M6) — pipe security descriptors + dir ACLs; verify foreign-user denial on a runner.
- [ ] **clipboard `CONOUT$`** (#42 M7) — `cfg` clipboard write; visual confirm in Windows Terminal (done in #164's e2e).

## Edge Cases

- `WAIT_ABANDONED` on the spawn mutex — treat as acquired-after-crash.
- Detached daemon inside a kill-on-job-close Job Object — may need `CREATE_BREAKAWAY_FROM_JOB`.
- `CTRL_BREAK_EVENT` honored inconsistently by console apps — `TerminateJobObject` is the unconditional backstop.
- Each `cfg(unix)` permission site must get a Windows counterpart or a justified no-op — easy to under-implement and silently drop a security property.

## Out of Scope (v1)

- Faithful SIGTERM-trap graceful-shutdown parity.
- Per-session / multi-desktop pipe isolation (per-user only).
- Release artifacts, e2e validation, and docs — those are **#164**.
