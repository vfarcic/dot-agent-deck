# Client behaviour notes

Design rationale and mechanism detail that used to sit in the published user pages (`docs/installation.md`, `docs/getting-started.md`, `docs/session-management.md`, `docs/keyboard-shortcuts.md`) and moved here when PRD #1419 rewrote those pages for the user's agent. The user pages now say what happens; this page says why.

## Installation and the Nix flake

- **Why the flake has no `x86_64-darwin`.** The nixpkgs the flake pins has dropped `x86_64-darwin` outright (evaluation throws, it does not merely fail to build), so Intel Macs are served by the release binaries and the Homebrew tap. The flake builds from source against the committed `Cargo.lock`, so there is no per-release hash to maintain, and it pins the released version so `--version` reports it rather than a source-build placeholder. `flake.nix` carries the full reasoning.
- **Why rustc 1.97.1 is stated as a minimum.** It is the toolchain the project is built and tested on, declared as `rust-version` in `Cargo.toml`, so cargo refuses an older one up front and names the version it wants instead of failing somewhere inside the build. An older rustc may work; nothing tests one, which is why it is not promised. The `follows` option trades one nixpkgs in the closure for building against a nixpkgs this project has not tested.
- **What the home-manager module leaves alone, and why.** `session.toml` is runtime state the deck writes itself, and home-manager links its files read-only out of the store, so managing it would stop the deck saving. `remotes.toml` is written imperatively by `dot-agent-deck remote add`, the same problem. `schedules.toml` is the one file whose location honours `$XDG_CONFIG_HOME`, and managing it correctly needs handling the other files must not get, so it was left for a follow-up. The module uses `home.file`, never `xdg.configFile`, because `config_dir()` ignores `$XDG_CONFIG_HOME` (see the comment in `flake.nix`).
- **Why the module does not run `hooks install`.** That command edits other tools' configuration (Claude Code, OpenCode, Codex, Devin), which is outside home-manager's ownership and does not roll back when you switch generations.
- **`nix develop`** is a consumer shell with just the Rust toolchain; devbox pins the toolchain and ships the recording and docs tooling the test suites need.

## `daemon status`

- **Why it never starts a daemon.** Spawning a daemon to answer "is a daemon running?" would make the question unanswerable, so an absent daemon is reported as a failure and the socket is left as found. Launching the TUI lazy-spawns one; `daemon status` does not, in either output form.
- **Why an unreachable daemon exits 1, unlike `daemon stop`.** A status query that got no answer learned nothing, so reporting success would be wrong; `daemon stop` against no daemon has achieved its goal and exits 0. Exit 2 (clap's usage error) lets a script tell an unreachable daemon apart from a binary too old to know the subcommand.
- **Why it does not retry.** A retry loop would add load to the exact daemon being diagnosed; the 3-second bound abandons the query instead.
- **Why it omits prompt text and tool arguments.** A status snapshot is routinely pasted into bug reports or run in a terminal someone else watches, so it carries diagnostics and nothing private. `schema_version` 2 removed `active_tool.detail`, which version 1 carried; a v1 script that read tool arguments gets nothing under v2 rather than something subtly different.
- **Why `daemon stop` has a separate orchestration refusal.** The daemon keeps role registrations (`pane_role_map` and friends) in memory only, so stopping it deletes them for good. An agent that survives keeps running and reporting status, and its card looks healthy, but its `delegate` is refused from then on (issue #770). See also [`daemon-teardown-paths.md`](daemon-teardown-paths.md).
- **Why a protocol skew exits rather than attaching.** A TUI on a different attach protocol would render a normal-looking dashboard while silently dropping every event it could not decode (issue #405).

## Session restore (TUI)

- **Card state on reattach is the agent's real, current state.** The reconnected dashboard does not reset cards to Idle (or "No agent") and wait for the next event; it reads the daemon's snapshot, so an agent that has been waiting shows Needs Input immediately.
- **Why only the tab is restored after a rebuild.** When agents are gone and panes are recreated from the snapshot, the panes are new, and matching a remembered focused pane against them could put the user in front of a different agent, which is worse than not restoring the position.
- **Why cards get more compact instead of scrolling.** Scrolling through cards would defeat the point of a single dashboard, so density is chosen from the card count and the space available (`CardDensity` in `src/ui.rs`).
- **Why `remote remove` does not clear the snapshot.** It is registry-only, so removing an unrelated remote never wipes the local workspace.
- **Why the Claude Code `StopFailure` hook is version-gated.** Claude Code releases older than 2.1.78 that were tested ignore every hook in the settings file when that key is present (`STOP_FAILURE_MIN_CLAUDE_VERSION` in `src/hooks_manage.rs`).

## Keyboard and mouse (TUI)

- **Why the wheel outside the focused pane is dropped.** While typing, the wheel is forwarded to the agent carrying the cell the pointer is on, so a wheel from outside the pane is dropped rather than delivered at the nearest edge. In command mode it is never forwarded to the agent's mouse protocol, so a full-screen TUI cannot scroll under you while you read. The card grid has no scroll of its own; it moves only with the selection.
- **Why command mode is the resting state.** It is the one mode in which a stray keystroke cannot reach an agent, which is why reading and scrolling work there.
- **Why `Ctrl+W`, `Ctrl+E`, `Ctrl+L` and `Ctrl+Z` are claimed only in command mode (and some only on orchestration tabs).** Each collides with a byte a pane's occupant wants: word-delete, readline end-of-line, clear-screen, job-control suspend. Claiming them only in command mode costs one extra `Ctrl+D` rather than the chord (PRD #241, issue #438, `scope_orchestration_chord` and `scope_zoom` in `src/ui.rs`).
- **Why `Ctrl+C` cannot be rebound.** It is the non-overridable quit trigger; a binding to it would be guaranteed dead, so it is warned about and left unbound rather than silently accepted.
- **Why a key typed before the close confirmation appeared is discarded.** A reflexive `Enter` must not answer a dialog the user has not seen; the dialog also defaults to Cancel for the same reason, and it is identity-bound to what was selected when it opened.
- **Scrollback and agents that repaint in place.** Codex repaints its whole transcript in place rather than emitting new lines, so nothing scrolls off the top and the terminal emulator is handed nothing to keep. In an ordinary terminal, scrolling up during a codex session reaches what was on screen before codex started, never an earlier part of the conversation; a deck pane starts empty, so there is nothing above. A full-screen program uses the alternate screen, which keeps no scrollback, so the deck cannot reach history while it is shown.
- **The command-entry lock** is not saved across restarts on purpose: every deck starts locked. A temporarily typeable pane (a worker in WaitingForInput) looks no different from a locked one.
