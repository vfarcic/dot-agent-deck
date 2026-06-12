# PRD #143: Verified supervised / always-on daemon for unattended scheduling

**Status**: Planning
**Priority**: Medium
**Created**: 2026-06-11
**GitHub Issue**: [#143](https://github.com/vfarcic/dot-agent-deck/issues/143)
**Depends on**: [#127](https://github.com/vfarcic/dot-agent-deck/issues/127) (cron-scheduled prompt dispatch — provides the scheduler whose unattended use this enables)
**Related**: `src/daemon.rs` (lazy-spawn, idle shutdown, M1.4 enabled-schedule keep-alive), `docs/scheduled-tasks.md`, `docs/getting-started.md` ("How it runs")

## Problem Statement

Scheduled tasks (PRD #127) only fire while the daemon is running. The daemon is lazy-spawned when the user opens the deck, and an enabled schedule keeps it alive between fires — but it is **not** started at boot and **not** respawned after it exits. After a reboot, logout, or crash the daemon stays down until the next `dot-agent-deck` launch, and any fire due in that window is silently missed (there is no catch-up and no persisted last-fire timestamp). There is no built-in always-on mode.

The result: the natural production use of a scheduler — "fire at 09:00 even though the machine rebooted overnight and I haven't opened the deck" — is not reliably supported.

## Why this is its own PRD (not just docs)

`docs/scheduled-tasks.md` previously carried systemd-unit and launchd-plist recipes for running the daemon always-on. They were removed (commit `dac0ad0`) because:

- **Never verified.** The launchd `ProgramArguments` hardcoded the Intel-only `/usr/local/bin/dot-agent-deck`, which fails on Apple Silicon — evidence the recipe was written from assumption, not from running it.
- **Wrong content for docs.** It splits into (a) generic OS knowledge (how to author a user unit / LaunchAgent — the OS documents this better than we can) and (b) unverified project behavior. Neither belongs in docs until (b) is tested.
- **Wrong home.** Supervising the daemon is a daemon-general concern, not scheduling-specific.

Documenting before verifying means shipping either low-value generic config or unconfirmed claims. This PRD does the verification (and possibly the tooling) first, then documents.

## Proposed Scope (to refine)

1. **Verify** `dot-agent-deck daemon serve` runs correctly under a supervisor and survives reboot/login on both platforms:
   - Linux: systemd user unit (+ `loginctl enable-linger`).
   - macOS: launchd LaunchAgent (`RunAtLoad` + `KeepAlive`), with the correct Homebrew path per architecture (`/opt/homebrew/bin` on Apple Silicon, `/usr/local/bin` on Intel).
   - Confirm `DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS=0` keeps it alive and that a fire actually happens unattended.
2. **Consider productizing** instead of shipping copy-paste config: a `dot-agent-deck daemon install` / `uninstall` (or similar) that generates and loads the correct unit/plist for the current platform, resolving the arch-specific binary path automatically (e.g. from the running executable's own path). This turns "untested OS config in the docs" into a project-owned, testable capability.
3. **Then document** the verified path — likely a short daemon-lifecycle section (not inside `scheduled-tasks.md`), linked from scheduling.

## Open Questions

- Ship `daemon install` tooling, or just a verified + tested doc recipe?
- Where does the always-on / daemon-lifecycle doc live? (`getting-started.md` "How it runs" already covers lazy-spawn + idle shutdown.)
- Windows (WSL) story?

## Out of Scope

- Persistent fire catch-up / replay of fires missed while the daemon was down (separate concern; the current "no catch-up" contract stands).
- Any change to the scheduler primitives themselves (owned by #127).
