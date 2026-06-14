# PRD #164: Windows release artifacts, e2e validation & docs

**Status**: Not Started
**Priority**: Medium
**Created**: 2026-06-14

> **Phase 3 of native Windows support.** Depends on the foundation in **[#42](42-native-windows-support.md)** and the platform backends in **[#163](163-windows-platform-backends.md)**. Read #42 for the canonical architecture and decisions. This PRD ships the binaries, runs the full interactive validation, and documents Windows support — it is the "make it real for users" phase.

## Problem

Once #42 and #163 land, dot-agent-deck compiles and runs on Windows and the `windows-latest` CI job proves the Windows code paths build and pass unit/integration tests. But three things remain before Windows is a supported, usable platform:

1. **No shipped binaries** — `release.yml` produces no `.exe`; Scoop has no Windows URLs; users cannot install.
2. **No interactive end-to-end proof** — CI cannot assert the things that only a real Windows desktop shows: clipboard via `CONOUT$` in Windows Terminal, ConPTY resize repaint, "daemon survives shell exit," and the full install→hook→dashboard→pane→agent-monitoring workflow.
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

### Documentation (#42 M11)

README platform-support section + Windows installation instructions (Scoop + direct `.exe`). Note the documented v1 behavior differences from #42/#163: OSC 52 needs Windows Terminal (legacy conhost ignores it), best-effort graceful agent shutdown, per-user (not per-session) isolation. Follow CLAUDE.md docs conventions (user-facing docs render in both Docusaurus and plain GitHub markdown; no hard-wrapped prose).

## Milestones

- [ ] **Release binaries** (#42 M9) — `x86_64`/`aarch64-pc-windows-msvc` rows in `release.yml`; `.exe` published; Scoop updated.
- [ ] **Resize/ConPTY + full e2e on Windows VM** (#42 M10) — full workflow verified interactively in Windows Terminal.
- [ ] **Docs** (#42 M11) — README platform-support + Windows install; documented behavior differences.

## Edge Cases

- Antivirus — Windows Defender may flag unsigned `.exe`; code signing is future work (out of scope).
- aarch64 interactive validation may be limited by VM availability — CI build coverage is the floor.
- Legacy cmd.exe / conhost silently ignores OSC 52 — documented, out of scope for support.

## Out of Scope (v1)

- Winget package submission.
- Code signing for Windows binaries.
- Windows-specific installer (MSI/NSIS).
- PowerShell module packaging.
