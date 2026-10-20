# PRD #143: Verified supervised / always-on daemon for unattended scheduling

**Status**: Complete (2026-10-20) — launchd half UNVERIFIED, see the verification record
**Priority**: Medium
**Created**: 2026-06-11
**GitHub Issue**: [#143](https://github.com/vfarcic/dot-agent-deck/issues/143)
**Depends on**: [#127](https://github.com/vfarcic/dot-agent-deck/issues/127) (cron-scheduled prompt dispatch — provides the scheduler whose unattended use this enables)
**Related**: `src/daemon.rs` (lazy-spawn, idle shutdown, M1.4 enabled-schedule keep-alive), `docs/scheduled-tasks.md`, `docs/getting-started.md` ("How it runs")

## Validation refresh (2026-06-14)

Re-validated against current code — verdict: **mostly current**. The premise holds: there is still no boot-time supervision — `DaemonCmd` in `src/main.rs` exposes `Serve`/`Hello`/`Stop`/`Restart` but no `Install`, and the daemon is only lazy-spawned on attach. The **#127 dependency has shipped** (archived in `prds/done/127-cron-scheduled-prompt-dispatch.md`): it provides the scheduler primitives, `src/daemon.rs`/`src/scheduler.rs`, and the `DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS` keep-alive this PRD assumes, so treat "Depends on #127" as satisfied. One concrete loose end to fold into scope: `docs/getting-started.md` already links readers to "supervisor recipes" in `docs/scheduled-tasks.md`, but commit `dac0ad0` removed those recipes — a live broken promise in published docs that this PRD should repair.

## Validation refresh (2026-10-20)

Since the 2026-06-14 refresh: the broken "supervisor recipes" link in `docs/getting-started.md` is already gone; `docs/remote-requirements.md` carries a systemd user unit and (issue #1172, #1160) a launchd LaunchAgent, both labelled "a starting point, not validated"; and the daemon already cooperates with a service manager — `daemon_restart::detect_supervisor` recognises systemd and launchd, and an accepted upgrade restart exits `75` so the manager starts the installed build. `platform::paths::durable_binary_path()` (PRD #381, #1140) resolves the absolute path this build should write into another program's persistent configuration. What is still missing is exactly this PRD's premise: nothing the deck ships sets the service up, and the hand-written recipes need a hand-edited binary path.

## Decisions

- **2026-10-20 — Ship `dot-agent-deck daemon install` / `daemon uninstall`, not only a doc recipe** (open question 1). The failure that removed the original recipes was a hand-written binary path; `durable_binary_path()` already answers "which path goes into another program's config" for agent hooks, so the command writes the unit/plist from it instead of the user editing one. It also makes the behaviour project-owned and testable: the generated files are unit-tested, the command is tested end to end against stand-in `systemctl` / `launchctl`, and the systemd half is verified against a real systemd user manager. The three host-specific choices #1172 named stay with the user: the binary path is resolved, credentials go in a drop-in (`systemctl --user edit`) or the plist's `EnvironmentVariables`, and the macOS auto-login/FileVault trade-off is documented, not decided.
- **2026-10-20 — Neither command stops a running daemon.** `install` starts the service only when no daemon answers at the endpoint (otherwise it enables it and says how to switch), and reinstalling over an active service never restarts it; `uninstall` disables and removes the unit/plist and leaves the running daemon alone. Stopping a daemon stops its agents, which is `daemon stop`'s decision to make (#770), not a side effect of installing.
- **2026-10-20 — Restart on failure, not on every exit.** systemd `Restart=on-failure`; launchd `KeepAlive` = `{SuccessfulExit: false}` rather than the old recipe's `KeepAlive true`. A crash, a kill and the upgrade restart (exit 75) bring the daemon back; `daemon stop` and the TUI's **Stop** (exit 0) stay stopped, on both platforms. `DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS=0` is written into both.
- **2026-10-20 — The endpoint and directory overrides are carried into the service.** `DOT_AGENT_DECK_SOCKET`, `DOT_AGENT_DECK_ATTACH_SOCKET`, `DOT_AGENT_DECK_STATE_DIR`, `XDG_CONFIG_HOME` and `XDG_STATE_HOME` (and, for launchd, `XDG_RUNTIME_DIR` and `TMPDIR`), when set in the shell that runs `install`, are written into the unit/plist, so the service listens where that shell's clients look and reads the same configuration (PR #1762 review). Nothing else from the environment is copied; credentials are never written by the deck.
- **2026-10-20 — Lingering is reported, not changed.** `install` on Linux reads `loginctl show-user <user> -p Linger`; when it is off it prints the `sudo loginctl enable-linger` line. The deck never runs `sudo`.
- **2026-10-20 — Doc home** (open question 2): a published page of its own, `docs/daemon-service.md`, linked from getting-started's "How it runs", from Schedules and from the remote requirements page, which keeps only what is remote-specific. Supervising the daemon is daemon-general, as this PRD argued.
- **2026-10-20 — Windows** (open question 3): `daemon install` refuses on Windows. WSL with systemd enabled is Linux to the deck and takes the systemd path; WSL without systemd gets the refusal `systemctl` produces, reported as such.
- **2026-10-20 — Decisions read the service manager's own state, and readiness needs a reply** (PR #1762 review). launchd's running/stopped split comes from the `pid =` line of `launchctl print`, not from a probe at the shell's endpoint, which a shell with other socket settings would find empty; a loaded job whose daemon stopped is started with `kickstart`. systemd's `FragmentPath` must name the file `install` wrote, or it refuses before enabling anything. `install` reports success only on a completed `Hello`; anything else that holds the endpoint still counts as occupied when deciding whether to start. After `uninstall` leaves a running daemon, the service manager still restarts it after a crash until it stops; the output and docs say so, and `daemon stop` ends it.
- **2026-10-20 — launchd is UNVERIFIED.** The work was done on Linux; the plist and the `launchctl` calls are unit- and stand-in-tested only, and the docs say the macOS path has not been run on a Mac. Issue #1172 stays open until it has been.
- **2026-10-20 — No experimental flag** (CLAUDE.md rule 9). Asked, and the maintainer answered through the dispatcher that started this unit: `daemon install` / `uninstall` ship visible by default, with no `features::show_*` wrapper. It is an opt-in command that changes nothing for anyone who does not run it.
- **2026-10-20 — The service starts in the home directory** (`WorkingDirectory=~`; the plist's `WorkingDirectory`). Project configuration is found by walking up from the daemon's directory, so a service started in `/` would miss `~/.dot-agent-deck.toml`, which a daemon lazy-spawned from a shell in the home directory reads. On macOS `TMPDIR` is carried too, because the socket fallback sits under it and a LaunchAgent is not guaranteed to inherit it.

## Milestones

- [x] **M1** — `daemon install` / `daemon uninstall` for systemd (Linux) and launchd (macOS): generated files from `durable_binary_path()`, live-daemon and active-service handling, linger report, refusal on Windows. Unit tests for rendering and the decisions; an e2e test of the binary against stand-in `systemctl` / `launchctl`.
- [x] **M2** — Verify the systemd path on a real systemd user manager: the installed unit runs `daemon serve`, survives a `SIGKILL` (restarted), honours `daemon stop` (stays stopped), restarts onto a new binary after the upgrade restart, and a schedule fires with no client attached. Evidence recorded here. launchd reported UNVERIFIED.
- [x] **M3** — User docs: `docs/daemon-service.md` (listed in `docs/published.toml`), links from getting-started, Schedules, remote requirements and the desktop daemons page; the unverified hand-written recipes replaced by the command. Changelog fragment.

## Verification record (2026-10-20, Linux, systemd 259)

Done on the dev box against the real systemd user manager, in a sandbox (own `HOME`, sockets, state dir, log and working directory) so that the production daemon and real schedules were never involved.

- **The unit is generated, not hand-written.** `daemon install` against a stand-in `systemctl` wrote the unit; `systemd-analyze --user verify` on it exited 0.
- **Installing the unit for real was not permitted on this box.** Writing a persistent unit into the operator's `~/.config/systemd/user` was refused by the session's permission policy (unauthorized persistence), so the generated directives were run as a transient unit instead: `systemd-run --user --unit=dad-prd143-verify -p Restart=on-failure -p RestartSec=2 -p WorkingDirectory=<sandbox> -E …` the unit's `Environment` lines, running the branch build's `daemon serve`. `enable`, `WantedBy=default.target` and start at boot or login are therefore **not** exercised here; `daemon install`'s own `systemctl` calls are covered by `daemon/service/001`–`004` against stand-ins.
- **Runs and answers.** One `Attach protocol listening` line per start; `daemon probe` → `running: 0.47.0`. Pre-bind took 7.5 s at a load average of 33–50.
- **Restarted after `SIGKILL`.** Journal: `Main process exited, code=killed, status=9/KILL` → `Scheduled restart job, restart counter is at 1` two seconds later → `Started`; `NRestarts=1`, new `MainPID`, `daemon probe` answering.
- **A schedule fired unattended.** `schedule add --cron "* * * * *"` on the supervised daemon, no client attached; the fire's command wrote `Tue Oct 20 04:23:00 UTC 2026` and `daemon status` listed `sched-prd143-fire-0`.
- **The upgrade restart lands on the new binary.** Atomic `mv` of a fresh copy over the installed path, then `daemon restart-installed --expect-version 0.47.0 --confirm-hex …` → `accepted`; deck log `restart accepted under a service manager; exiting for it to start the installed build supervisor=Systemd exit_status=75`; journal `status=75/TEMPFAIL` → `Scheduled restart job, restart counter is at 2` → `Started`; the new `MainPID`'s `/proc/<pid>/exe` is the new inode (the old one read `(deleted)`).
- **`daemon stop` stays stopped.** `daemon stop` → `daemon stopped`; eight seconds later the unit is `inactive`, `Result=success`, and `daemon probe` says `not running`.
- **Not verified here:** a real reboot (lingering is on on this box, but the box was not rebooted), and everything on macOS — the LaunchAgent, `launchctl bootstrap`/`bootout`, `KeepAlive {SuccessfulExit: false}`, `TMPDIR` carry-over. These are unit- and stand-in-tested only and stay UNVERIFIED; issue #1172 stays open for them.
- **Found and filed, not fixed:** a local restart onto a new build (the TUI's or desktop's Replace, `ClientSpawns`) makes a supervised daemon exit 0, so the service stays stopped and the client runs an unsupervised daemon — issue #1760. The fix needs an additive field in `src/daemon_protocol.rs`, reserved for another unit while this PRD ran; `docs/daemon-service.md` tells the user what happens and what to run.

## CLAUDE.md rule 12

The change adds two CLI subcommands that write a unit or plist and call the service manager. It changes no TUI↔daemon message, field, handler or meaning: no `PROTOCOL_VERSION` bump and no `.breaking.md`. The only daemon-adjacent edit is making `daemon_attach::daemon_start_poll_timeout` public so `install` waits as long as lazy-spawn does.

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

All three are answered in [Decisions](#decisions) (2026-10-20).

- Ship `daemon install` tooling, or just a verified + tested doc recipe?
- Where does the always-on / daemon-lifecycle doc live? (`getting-started.md` "How it runs" already covers lazy-spawn + idle shutdown.)
- Windows (WSL) story?

## Out of Scope

- Persistent fire catch-up / replay of fires missed while the daemon was down (separate concern; the current "no catch-up" contract stands).
- Any change to the scheduler primitives themselves (owned by #127).
