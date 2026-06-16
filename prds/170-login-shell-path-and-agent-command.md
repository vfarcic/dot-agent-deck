# PRD #170: Spawned panes inherit login-shell PATH + configurable agent command

**Status**: Not Started
**Priority**: High
**Created**: 2026-06-16
**GitHub Issue**: [#170](https://github.com/vfarcic/dot-agent-deck/issues/170)
**Related**: PRD #93 (always-external daemon — this builds on the daemon being the single spawn authority), PRD #127 (scheduler — the authoring helper and scheduled-fire spawn live here), PRD #20 (multi-agent support — the configurable command is a step toward it)

## Problem Statement

The daemon spawns every pane command — normal dashboard panes, scheduled-task fires, and the schedule-authoring helper — through `agent_pty`'s `CommandBuilder`, which makes `portable-pty` resolve a *bare* command against the **daemon's own process PATH** (`src/agent_pty.rs:768-822`; `portable-pty` `search_path`, `cmdbuilder.rs:415-434`). When the daemon is launched without the user's login profile — e.g. over SSH non-interactively, or any non-login context — its PATH lacks `~/.local/bin`, which is the default install location for both Claude Code and opencode. The result is that a bare command such as `claude` or `opencode` fails to spawn with *"Unable to spawn `<cmd>` because it doesn't exist on the filesystem and was not found in PATH."*

This was first hit via the schedule **Edit/Add** button, which spawns a hardcoded authoring agent (`SCHEDULE_AUTHORING_AGENT = "claude"`, `src/ui.rs:393`). But the same failure affects **normal dashboard panes** created with `claude` as the command and **any scheduled task** whose `command` is a bare `claude`. Multi-word commands like `devbox run agent-new` escape the failure only incidentally: they contain whitespace, so they are wrapped as `$SHELL -c "<cmd>"` (`src/agent_pty.rs:768-777`, `src/spawn.rs:245-250`), and `devbox` happens to live in `/usr/local/bin`, which *is* on the daemon's PATH. Verified on the affected machine: against the daemon's exact PATH, `devbox` resolves and `claude` does not.

The single thing the system cannot work around on its own is the **initial spawn of the agent binary**: that is done by the daemon *before any agent process exists*, so there is nothing yet that could re-establish a richer environment. (Once an agent like Claude Code is running, it sources the user's profile for its own shell-outs, which is why agents "just work" after they start — see Design Decisions.)

## Solution Overview

Two independent changes:

1. **Login-shell PATH parity (the bug fix, on by default).** At daemon startup — before the async runtime and worker threads start — capture the user's login-shell PATH once (`$SHELL -lc 'printf %s "$PATH"'`, with a timeout) and set it into the **daemon's own** process environment. Every pane the daemon subsequently spawns inherits that PATH automatically, with no change to the hot spawn path. On capture failure (no `$SHELL`, timeout, empty result) the daemon keeps its inherited PATH, so behavior never regresses. This makes "a command that resolves for the logged-in user resolves in a dot-agent-deck pane" true for normal panes, scheduled fires, and the authoring helper alike.

2. **Configurable agent command (the feature, visible by default).** Replace the hardcoded `SCHEDULE_AUTHORING_AGENT = "claude"` with a user-chosen command, defaulting to the already-existing new-pane default (`default_command`, `src/config.rs:10`). Surface an agent-command field/picker in the schedule authoring flow (Edit/Add) and the dashboard new-pane flow so users can pick `claude` / `opencode` / a path / a custom command. This removes the last hardcoded agent name and is a step toward broader multi-agent support (PRD #20).

## Scope

### In Scope

- A small helper that captures the login-shell PATH once (`$SHELL -lc 'printf %s "$PATH"'`), bounded by a timeout, returning `None` on any failure/empty output.
- A single daemon-startup block (before the runtime/threads start) that applies the captured PATH to the daemon's own environment, logging what was captured, and leaving the inherited PATH untouched on failure.
- Replacing the hardcoded authoring agent with a configurable command that defaults to `default_command`.
- An agent-command field/picker at schedule authoring (Edit/Add) and the dashboard new-pane flow, visible by default (no experimental flag).
- L2 e2e proving a bare `claude`-style command resolves in a spawned pane when the daemon is launched with a PATH stripped of `~/.local/bin`; L1 widget coverage for the picker UI.
- User docs for both: command resolution now uses the login-shell PATH (and a profile change needs a daemon restart to take effect), and the agent-command picker.

### Out of Scope / Non-Goals

- **Full login-shell *environment* capture** (non-PATH vars: `KUBECONFIG`, `GH_TOKEN`, cloud creds, `LANG`, etc.). Deliberately deferred — see Design Decisions. The same daemon-startup mechanism can be extended to capture more variables if a concrete env-dependency case ever surfaces.
- **Per-project / per-directory environments** (direnv, devbox-per-dir auto-activation). These vary by pane `cwd` and are a separate, larger effort; project tools remain reachable via `devbox run …` exactly as today.
- **Windows support.** Unix-only, consistent with PRD #93.
- **The `experimental` feature flag** (PRD #139). The user chose to ship the command picker visible by default: no `features.rs` wrapper, no `graduate-` follow-up issue.

## Design Decisions

1. **PATH-only, not the full login environment.** The only failure the system cannot self-heal is the initial spawn of the agent binary (no agent is running yet to fix its own environment). Once running, agents like Claude Code source the user's profile for their shell tool — observed directly: a Claude Code session running inside a dot-agent-deck pane has `~/.local/bin` and the devbox project paths on its Bash PATH even though the daemon that launched it has neither, which also re-exports any profile-set vars. So capturing the full environment would only add robustness against a *hypothetical* agent that neither sources the profile nor finds what it needs on PATH — not an observed problem with `claude` or `opencode`. PATH-only keeps the change small and the auditor surface minimal (one variable, trivial precedence). Full-env is a documented future extension via the same mechanism.

2. **Capture once at daemon startup into the daemon's own env, not per-spawn.** This is a single touch point — no branching in the hot spawn path, no "don't clobber `opts.env`" logic — and children inherit the PATH naturally. The `set_var` is performed before the async runtime and worker threads start, so it is sound and free of concurrent-`getenv` hazards. Granularity tradeoff: a profile change is not picked up until the daemon restarts; this is acceptable and documented.

3. **Command picker visible by default (no experimental flag).** It fills a gap (the authoring agent is hardcoded today) rather than introducing an experimental surface, so it ships on.

## Success Criteria

- A daemon launched with a PATH stripped of `~/.local/bin` still spawns a bare `claude` pane successfully via all three paths: dashboard new-pane, a scheduled-task fire, and the schedule-authoring helper.
- The PATH fix adds **no** per-spawn branching to `agent_pty` / `spawn.rs`; it is a single daemon-startup block. On capture failure the daemon falls back to its inherited PATH with no behavior change.
- `grep -rn "SCHEDULE_AUTHORING_AGENT" src/` finds no hardcoded agent name; the authoring command resolves from config (`default_command`) and is user-overridable.
- The agent command is selectable at schedule authoring (Edit/Add) and the dashboard new-pane flow.
- `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test-fast` pass; `cargo test-e2e` passes before the PR (CLAUDE.md rule 5).
- User docs describe login-shell PATH resolution (incl. the daemon-restart caveat) and the agent-command picker.

## Milestones

### Phase 1 — Login-shell PATH parity (bug fix)

- [ ] **M1.1** — Add a login-shell PATH capture helper: run `$SHELL -lc 'printf %s "$PATH"'` with a timeout, returning `None` on missing `$SHELL`, non-zero exit, timeout, or empty output. Unit-test the parse/fallback logic.
- [ ] **M1.2** — At daemon startup, before the async runtime/threads start, apply the captured PATH to the daemon's own environment (log the result); on `None`, leave the inherited PATH untouched.
- [ ] **M1.3** — L2 e2e: with the daemon launched under a PATH stripped of `~/.local/bin`, a bare `claude`-style command resolves and spawns in a dashboard pane, a scheduled fire, and the authoring helper.

### Phase 2 — Configurable agent command (feature)

- [ ] **M2.1** — Remove the hardcoded `SCHEDULE_AUTHORING_AGENT`; the authoring command resolves from config, defaulting to `default_command`.
- [ ] **M2.2** — Surface an agent-command field/picker in the schedule authoring (Edit/Add) flow and the dashboard new-pane flow, visible by default; tests (L1 widget for the picker, behavior tests for default resolution).

### Phase 3 — Docs & release gate

- [ ] **M3.1** — User docs: command resolution uses the login-shell PATH (with the daemon-restart caveat) and the agent-command picker; changelog fragment via `dot-ai-changelog-fragment`.
- [ ] **M3.2** — Pre-PR gate: `cargo test-e2e` green; review (Greptile) settled per CLAUDE.md rule 8.

## Risks & Mitigations

- **Login-shell capture hangs or is slow.** Bound it with a timeout; on timeout fall back to the inherited PATH (no regression).
- **`set_var` soundness in a long-running multi-threaded daemon.** Perform the capture-and-set once at process start, before the runtime and any worker threads exist.
- **A profile that adds `~/.local/bin` only in the interactive (`.bashrc`) path, not the login (`.profile`) path.** Start with `$SHELL -lc`; if real profiles need it, escalate to an interactive-login capture (`-lic`) handling the usual `-i` quirks. On the reference machine `~/.local/bin` is added by `~/.profile`, so `-lc` suffices.
