# PRD #164: Windows release artifacts, e2e validation & docs

**Status**: Not Started
**Priority**: Medium
**Created**: 2026-06-14

> **Phase 3 of native Windows support.** Depends on the foundation in **[#42](done/42-native-windows-support.md)** and the platform backends in **[#163](done/163-windows-platform-backends.md)**. Read #42 for the canonical architecture and decisions. This PRD ships the binaries, runs the full interactive validation, and documents Windows support — it is the "make it real for users" phase.

## Problem

Once #42 and #163 land, dot-agent-deck compiles and runs on Windows and the `windows-latest` CI job proves the Windows code paths build and pass unit/integration tests. But three things remain before Windows is a supported, usable platform:

1. **No shipped binaries** — `release.yml` produces no `.exe`; Scoop has no Windows URLs; users cannot install.
2. **No interactive end-to-end proof** — CI cannot assert the things that only a real Windows desktop shows: clipboard via `CONOUT$` in Windows Terminal, ConPTY resize repaint, "daemon survives shell exit," and the full install→hook→dashboard→pane→agent-monitoring workflow. It also cannot assert the runtime confirmations #163 explicitly deferred to this PRD — foreign-user pipe denial (which needs a second local account), Job-Object descendant teardown, and concurrent-spawn serialization — because #163's Windows evidence is compile/clippy gates plus unit tests, never a running desktop.
3. **No documentation** — README has no Windows install instructions or platform-support statement.

## Solution

### Release artifacts (#42 M9)

`release.yml` already parameterizes `binary_ext`/`artifact_suffix`/`use_cross` and publishes a Scoop manifest. Add matrix rows for `x86_64-pc-windows-msvc` and `aarch64-pc-windows-msvc` (cross-compile) on `windows-latest`, producing `.exe` artifacts; portable-shell the bash `Build`/`Package` steps for Windows; add Windows binary URLs to the Scoop manifest. Direct `.exe` download from GitHub Releases.

### Interactive e2e validation (#42 M10)

On a cloud Windows VM with **Windows Terminal** (not legacy conhost):

- Install from a release `.exe`; verify hook registration.
- Launch the dashboard; create panes; spawn and monitor agents.
- Clipboard copy via OSC 52 → `CONOUT$` actually populates the Windows clipboard.
- Outer-terminal resize and PTY resize repaint correctly under ConPTY (the one resize item #42 defers to runtime verification).
- Daemon survives the launching shell's exit; `daemon stop` works (graceful + force).
- aarch64 is covered by CI build; interactive validation focuses on x86_64 (aarch64 cloud Windows VMs are scarce — note any gaps).

#163 deferred three runtime confirmations to this pass, and left two questions that only a real host can settle. These are the substance of this milestone, not extras: #163 shipped the code plus its compile/clippy and unit/`windows-latest` gates for each, and recorded that observing the behavior on a real Windows host is #164's job.

- **Foreign-user pipe denial** — the runtime half of #163's release-gating **[BLOCKER]**, and the most important gap here. Requires a **second local account** on the VM: from that account, attempt to open the daemon's named pipe and confirm `ACCESS_DENIED`; and confirm our client's owner-SID verification rejects a pipe squatted by another user. #163 unit-tests the SID decision logic as pure string data on every platform — the part that is decidable without a second account — so the enforcement itself has never been observed. If provisioning a second account on the VM proves impractical, state that explicitly as a **known unverified release gate**; it must not be silently dropped.
- **Job-Object agent teardown** (deferred from #163's peer-PID + lifecycle milestone) — confirm that closing an agent, and stopping the daemon, reaps the agent's whole descendant tree rather than only the direct child. Additionally **measure** the known post-spawn adoption window: spawn an agent that immediately spawns a child of its own, tear down, and look for orphans. Closing that window requires replacing `portable-pty`'s ConPTY spawn (0.8.1 exposes neither `CREATE_SUSPENDED` nor `PROC_THREAD_ATTRIBUTE_JOB_LIST`) and is out of scope for #164 — the goal here is to observe the real exposure on a real host instead of reasoning about it.
- **Concurrent-spawn serialization** (deferred from #163's detach + lock milestone) — two simultaneous lazy-spawns must serialize on the named mutex and yield exactly one daemon (the mutex doubles as the singleton-daemon guard), including the `WAIT_ABANDONED` acquired-after-crash path.
- **Config-secret file ACLs on real user state** — spot-check that `remotes.toml`, `schedules.toml` and `session.toml` carry the protected owner-only DACL under a real `%LOCALAPPDATA%`, and again with a `DOT_AGENT_DECK_STATE_DIR` override pointing into a permissive directory. The `windows-latest` job already reads the stored DACL back and asserts it is protected with exactly one ACE, so this is a spot-check on real user state rather than a tempdir — worth doing because this is the item that shipped broken (see the evidence rule under **Milestones**).
- **Third-party agent support on Windows** — decide and record whether Codex and Pi are supported agents on Windows. Both resolve their home through deliberate Windows no-ops (`codex_hooks_manage::codex_home`, `orchestrator_ext::agent_dir_strict`) and work only when `$CODEX_HOME` / `$PI_CODING_AGENT_DIR` is set explicitly; `login_shell::capture_login_shell_path` has no Windows analogue at all (`cmd.exe`/`%COMSPEC%` has no login-profile concept). The "verify hook registration" item above does not distinguish between agents. Either verify each via its env escape hatch and document that requirement, or state plainly in the support matrix which agents are unsupported on Windows.

### Pre-release robustness (#163 review follow-ups)

Two auditor LOW findings from #163, deliberately not fixed there. Both are availability-only, neither is a security issue, and each needs a decision before a Windows release — fix it, or consciously accept and document it.

- `IpcListener::accept()` takes the pending pipe instance and, if creating the replacement instance fails, leaves `pending = None`, wedging the serve loop.
- `paths::endpoint_user_suffix` panics on an unreadable SID, so the failure surfaces as a panic inside `socket_path()` / `attach_socket_path()` rather than a graceful error.

### Documentation (#42 M11)

README platform-support section + Windows installation instructions (Scoop + direct `.exe`). Follow CLAUDE.md docs conventions (user-facing docs render in both Docusaurus and plain GitHub markdown; no hard-wrapped prose).

Document the v1 behavior differences carried from #42/#163:

- OSC 52 clipboard needs Windows Terminal — legacy conhost ignores it.
- Graceful agent shutdown is best-effort (`CTRL_BREAK_EVENT`, then hard `TerminateJobObject`).
- Isolation is per-user, not per-session.
- **Same-host foreign-user squat DoS.** The pipe (`\\.\pipe\dot-agent-deck-…`) and `Global\` mutex names are per-user but predictable, so another local user can squat a name and deny service. Confidentiality and integrity hold (we fail closed); availability does not. Inherent to the naming scheme — documented rather than fixed.
- **The post-spawn Job Object descendant window** from the teardown item above: a process an agent spawns in the window between `CreateProcessW` returning and the agent joining its job never joins the job and survives `TerminateJobObject`.
- **Which agents are supported on Windows**, per the third-party agent item above.

## Milestones

**Evidence rule — cross-compile gates never execute code.** `cargo check` / `cargo clippy --target x86_64-pc-windows-msvc` type-check and lint the `cfg(windows)` branches but never link, never produce a binary and never run a test, so they cannot validate a runtime property. #163 marked its release-gating config-secret ACL item closed on exactly that evidence, and it turned out `SetSecurityInfo` could never have succeeded on any Windows host: setting `DACL_SECURITY_INFORMATION` needs `WRITE_DAC`, `FILE_GENERIC_WRITE` does not include it, and a Win32 access check runs once at open and freezes the granted mask on the handle. It was caught only when the `windows-latest` job executed the tests. So: no runtime property in this PRD may be marked complete without either the `windows-latest` `cargo nextest run` or the interactive VM pass behind it.

- [ ] **Release binaries** (#42 M9) — `x86_64`/`aarch64-pc-windows-msvc` rows in `release.yml`; `.exe` published; Scoop updated.
- [ ] **Resize/ConPTY + full e2e on Windows VM** (#42 M10) — full workflow verified interactively in Windows Terminal, plus the runtime confirmations #163 deferred here: foreign-user pipe denial (the **[BLOCKER]**'s runtime half, needs a second local account), Job-Object descendant teardown and its post-spawn adoption window, concurrent-spawn serialization, config-secret ACLs on real user state, and the Codex/Pi support decision.
- [ ] **Pre-release robustness** — fix or consciously accept the two #163 auditor LOW findings (`IpcListener::accept` wedging on replacement-instance failure; `endpoint_user_suffix` panicking on an unreadable SID).
- [ ] **Docs** (#42 M11) — README platform-support + Windows install; documented behavior differences, including the foreign-user squat DoS, the Job Object descendant window, and the Windows agent-support matrix.

## Edge Cases

- Antivirus — Windows Defender may flag unsigned `.exe`; code signing is future work (out of scope).
- aarch64 interactive validation may be limited by VM availability — CI build coverage is the floor.
- Legacy cmd.exe / conhost silently ignores OSC 52 — documented, out of scope for support.
- Provisioning a second local account on a cloud Windows VM may be awkward or blocked by policy — the foreign-user pipe-denial check depends on it.
- **SDDL normalizes well-known trustees to abbreviations.** A Windows CI runner's built-in Administrator (SID ending `-500`) renders as `LA`, so a test that string-matches a raw `S-1-5-…` literal fails against a perfectly correct ACL. Compare both sides through the same renderer. This cost a CI round-trip during #163.

## Out of Scope (v1)

- Winget package submission.
- Code signing for Windows binaries.
- Windows-specific installer (MSI/NSIS).
- PowerShell module packaging.
