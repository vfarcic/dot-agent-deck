# PRD #190: OpenCode plugin / hooks auto-install should run on the daemon lifecycle, not only TUI startup

**Status**: Not started
**Priority**: Medium
**Created**: 2026-06-22

## Problem

The OpenCode plugin (and the claude hooks) are auto-installed/refreshed **only on TUI startup** — `opencode_manage::auto_install()` and `hooks_manage::auto_install()` are called from `run_tui_session()` (`src/main.rs:844`). In the post-#76/#93 daemon-centric architecture this is too late for some paths, because **agents are spawned by the daemon, not the TUI**.

Confirmed lifecycle facts (read-only investigation, 2026-06-22):

- The daemon spawns agent PTYs: the TUI sends a `StartAgent` request over the attach socket; `daemon_protocol.rs:942` → `agent_pty.rs:720` does the actual spawn. Agents are daemon-owned resources.
- The daemon can run and host agents with **no TUI attached** (`dot-agent-deck daemon serve`).
- The daemon can spawn an OpenCode agent **before any TUI has run `auto_install`** via either a **scheduled task** firing (PRD #127, `src/daemon.rs:461-466`) or **agent hydration** when a TUI reconnects after an ssh drop (`src/embedded_pane.rs:5945`). In those paths the `opencode` process starts and loads its `index.js` from disk while the plugin may still be stale.
- The daemon **deliberately skips** auto-install on startup (`src/main.rs:1092`), with a rationale originally written for the claude hooks ("only needs reinstall on a binary version change, not every daemon start") that was applied to the OpenCode plugin too.

This is the trigger-location companion to PRD #79 (the plugin-layout *fan-out* fix, which fixed *where* the plugin is written, not *when* it is refreshed). PRD #79 composes cleanly with this work — the same `auto_install()` function would simply also be invoked from the daemon lifecycle, with no rework of the fan-out logic.

## Solution

Guarantee the OpenCode plugin (and, if the same gap applies, the claude hooks) is fresh before any daemon-spawned agent process can load it — not just when a TUI happens to start first. The implementation shape (where exactly to call it) is one of the open design questions below; this PRD's job is to close the freshness gap for daemon-spawned agents.

## Open design questions (resolve at `/prd-start`)

- **Where to call it:** daemon startup (`run_daemon_with` / `run_daemon_serve_cli`) vs. just-in-time **before each agent spawn** (in `spawn_agent`). Per-spawn is the most robust (the plugin is always fresh for that specific agent) but adds per-spawn cost; daemon-startup is cheaper but leaves a smaller residual gap (an in-place binary upgrade while the daemon keeps running).
- **Scope of agent types:** does `hooks_manage::auto_install()` (claude) have the same daemon-spawn-before-TUI gap, or is it already covered (the `remote add` path runs `hooks install`)? Decide whether this PRD covers both agent types or OpenCode only.
- **Overturn the documented skip:** `src/main.rs:1092` explicitly skips auto-install on daemon start. This PRD must consciously revise that rationale and document why the OpenCode plugin (at least) now warrants installation on the daemon lifecycle.
- **Idempotency / cost:** confirm writing on the chosen trigger is cheap and idempotent (it is — a tiny file, unconditional overwrite) and won't add meaningful startup latency or log noise.

## Acceptance Criteria

- [ ] An OpenCode agent spawned by the daemon **with no TUI attached** (e.g. a scheduled task, or hydration after reconnect) loads a plugin whose `BINARY_PATH` matches the running daemon binary — not a stale path.
- [ ] The chosen trigger refreshes **every existing layout** (reuses PRD #79's fan-out; no regression to the layout behavior).
- [ ] The previously-documented "skip auto-install on daemon start" decision (`src/main.rs:1092`) is explicitly revisited and the new behavior documented in code comments and the changelog.
- [ ] Decision recorded (in this PRD) on whether claude hooks need the same treatment, with the gap either closed or explicitly deferred with rationale.
- [ ] Tests cover the daemon-spawn-before-TUI path (synthetic where possible; a chain-smoke variant if warranted).
- [ ] No measurable startup-latency regression on the daemon path; install remains silent + best-effort (warn-on-failure, never aborts spawn/startup).

## Out of Scope

- The plugin-layout fan-out itself (delivered in PRD #79).
- Parsing `opencode.jsonc` to detect the active root (deferred in PRD #79).

## References

- `src/main.rs:844` — current TUI-startup auto-install call site
- `src/main.rs:1092` — daemon intentionally skips auto-install (rationale to revise)
- `src/daemon_protocol.rs:942`, `src/agent_pty.rs:720` — daemon-side agent spawn
- `src/daemon.rs:461-466` — scheduled-task agent spawn (PRD #127)
- `src/embedded_pane.rs:5945` — agent hydration on TUI reconnect
- PRD #79 (`prds/done/` after archival) — OpenCode plugin auto-install fan-out across every existing layout (this PRD's predecessor)
