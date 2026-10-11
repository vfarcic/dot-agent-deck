# Troubleshooting internals

The published [`docs/troubleshooting.md`](../troubleshooting.md) says what a user sees and what to do about it. This page keeps the mechanisms and reasoning behind those entries, which PRD #1419's rewrite took off the user page (Decision 3: internals only where diagnosis needs them). Read it before changing the behaviour an entry describes, so the entry and the code move together.

Material already recorded elsewhere in `docs/develop/` is linked rather than repeated.

## Hook installers

**Write safety.** Every installer does a read-modify-write of a file the user also edits. The Claude Code (`src/hooks_manage.rs`) and Devin (`src/devin_hooks_manage.rs`) installers serialise that under an in-process mutex and publish atomically (temp file in the same directory, then `rename`), so a crash mid-write cannot leave a truncated file. The published file keeps its existing permissions; a file the deck creates is owner-only. Only the deck's own commands are touched: a user handler sharing a rule object with a deck command stays, with its matcher, through both install and uninstall. A user hook that merely mentions `dot-agent-deck` is not deck-owned; ownership is an exact match on `<executable> hook --agent <agent>`, with or without the `DOT_AGENT_DECK_BIN` wrapper in front of it (PRD #1497, `platform::paths::HOOK_BIN_OVERRIDE_PREFIX`), or the legacy `<executable> hook` shape.

**Unparseable and symlinked configs are refused, not clobbered.** `load_settings_or_refuse` backs the bytes up with `agent_hook_config::backup_malformed` (appending `.bak` to the file name, owner-only, never following a symlink planted at that name) and errors in both directions; issue #522 removed the lenient reader that let `uninstall` truncate a bad file to `{}`. `refuse_symlinked_destination` refuses a symlinked destination because a `rename` publish would replace the link with a regular file (orphaning the dotfiles copy), and writing through it would write outside the directory the deck was pointed at. Devin documents its config as JSON with comments, which `serde_json` cannot round-trip, so such a config is refused the same way.

**Codex trust.** Codex runs only hooks it trusts. The deck records trust scoped to exactly its own hook entries, pinned to each hook's content hash, in the Codex home's `config.toml`, and edits that file surgically so comments, model choice and user trust records survive byte for byte. It never trusts a hook it did not author, and it deliberately does not use `--dangerously-bypass-hook-trust`, which is invocation-global. Trust pinned to content fails closed: a changed definition is refused by Codex until the install re-records it. `src/codex_hooks_manage.rs`'s module comment has the full reasoning. `codex_home()` resolves `$CODEX_HOME`, else `$HOME/.codex`, and returns `None` when neither is set rather than guessing a third-party tool's home; it has no Windows branch, so on Windows, where `$HOME` is normally unset, Codex hooks install only when one of the two is set.

**Where hooks point (PRD #381, issues #1140, #1372).** `platform::paths::durable_binary_path` decides the executable a hook names, in this order: the running binary if it is itself an installed location; an installed name (`~/.local/bin` or a `$PATH` entry) that resolves to the running binary; `~/.local/bin/dot-agent-deck`; the first durable match on `$PATH`; and finally, as a last resort, the running binary pinned with a warning. A cargo build artifact is refused at every step, and when nothing else exists the install fails rather than write it. The last-resort arm exists for a machine whose only deck is the running binary, such as a packaged desktop app starting its bundled sidecar with no CLI installed.

**Repair only what is unusable.** Startup rewrites a deck-owned hook only when its binary is confirmed missing. A hook whose binary still works is left alone even when it differs from what the deck would write, because a user's wrapper or a deliberately used second checkout is not something a startup should repoint. The consequence is that two genuine installs each keep a rule, and the agent fires two deck hooks per event. The OpenCode plugin carries its pin as `const BINARY_PATH = …`; the auto-install regenerates the file (so template changes land) and carries a usable pin over.

**Why both processes install.** Issue #1157: the packaged desktop app starts `daemon serve` from its sidecar and never runs a TUI, so hooks installed only by the TUI left desktop-only users without hook-driven status. `daemon serve` now runs every registry entry's `startup_auto_install` after applying the login-shell `PATH` (so presence is detected against the daemon's real `PATH`), and the TUI still runs the same loop. Every installer is idempotent.

## Login-shell PATH capture

PRD #170. The daemon spawns pane commands by resolving a bare command against its own `PATH`. At `daemon serve` startup it runs `$SHELL -ilc` (interactive, so installers' `PATH` lines after the non-interactive guard in `~/.bashrc` are seen) with a 10 s `CAPTURE_TIMEOUT`, and sets the result into its own environment in the single-threaded window before the tokio runtime exists (the `set_var` soundness condition). On failure it keeps the inherited `PATH`. The capture runs before the daemon binds its endpoint, which is why `DAEMON_START_POLL_TIMEOUT` is derived from `CAPTURE_TIMEOUT` plus 5 s: a slow but healthy shell must not make the lazy-spawn launcher give up on a daemon that is about to bind.

## Logging

`DOT_AGENT_DECK_LOG`'s default is resolved per platform (issue #1135): on Windows the Unix literal `/tmp/dot-agent-deck.log` is rooted but driveless and resolved to `\tmp\…` on the current drive, which usually does not exist, and because the opener creates the file but not its parent, nothing was logged. The writer must stay a synchronous `std::fs::File` (never a non-blocking appender thread), because on `daemon serve` logging is initialised immediately before the login-shell `set_var`.

`RUST_LOG` precedence (issue #605): `logging::env_filter` builds `error,dot_agent_deck=info,<RUST_LOG>`. The obvious spelling, `from_default_env().add_directive("dot_agent_deck=info")`, had it backwards: `add_directive` replaces an equally specific directive, so a user's `dot_agent_deck=debug` was discarded.

## Version handshake

On every launch the TUI performs a build-version handshake with the daemon (PRD #103, #161; `src/build_version_handshake.rs`). With no agents running the older daemon is restarted silently, because there is nothing to lose. With agents running and a terminal, the prompt names the agents and takes a single key: `s`/`S` without Ctrl restarts, any other key keeps the daemon (never strand the agents). Without a terminal it prints the recovery hint and exits non-zero. Under an attach-protocol skew (issue #405) the decline key is relabelled because declining exits instead of attaching. Daemon-supplied strings (build id, agent names) go through `sanitize_for_prompt` before rendering. [`versioning.md`](versioning.md) covers what counts as a contract break.

## Hook capability tokens

[`hook-provenance.md`](hook-provenance.md) has the design, the threat it closes (another same-uid process signalling as a pane after reading its id from `daemon status`) and the history of the acknowledgement: `work-done` and `dispatch` read no reply until issue #1129, so a refused report exited 0 and was visible only in the daemon log. The acknowledgement is written at the gate, before the handler runs, so it says the message was admitted, not that the work succeeded. The `DOT_AGENT_DECK_HOOK_PROVENANCE` policy treats anything other than an exact, case-insensitive `warn` as enforce, so a typo fails safe.

## Orphaned orchestration roles

The role maps live in daemon memory only; [`daemon-teardown-paths.md`](daemon-teardown-paths.md) covers the teardown paths and their disclosure. Restoring the maps from disk on restart was considered and rejected: a restart kills the PTYs the daemon owned, so most panes in such a file would be dead, and a restored entry pointing at a dead pane would turn an honest refusal into a delegate that routes into nothing. The failure is expensive because it surfaces only when someone next delegates, which for an orchestrator can be hours into a run; that is why the card is marked `orphaned` (TUI only today) and why `daemon stop` refuses while roles are live (issue #770).

## Pane reconnect and give-up

`src/embedded_pane.rs`: when an agent goes away, the TUI looks it up again and re-attaches, which makes an ordinary respawn invisible. It gives up after `REATTACH_MAX_EMPTY_SESSIONS` (3) consecutive attaches that produce no output (`PaneLostReason::AgentKeptCrashing`, log reason `empty-sessions`), or when no live agent claims the pane within `REATTACH_LOOKUP_TOTAL_BUDGET` (10 s, derived from the respawn handover worst case). The latter logs one of `daemon-unreachable`, `attach-failing` or `no-live-agent`, chosen by the same branch order as the reported error so the two never disagree. The user-facing pane text is `AgentGone` for all three; a distinct daemon-unreachable message would change rendered strings and was left for its own change.

## Pane size and replay

A PTY has one window size, so two clients cannot see one live agent at different sizes. The daemon's focus-then-smallest policy is described in [`rendering-contract.md`](rendering-contract.md) (PRD #882, #1105) and, from the desktop's side, in [`desktop-gui.md`](desktop-gui.md), including `FOCUS_REAPPLY_INTERVAL` (250 ms). On a size change the daemon drops the replay ring, because those bytes were drawn for the old grid and replaying them into a differently sized screen produces overlapping text and stray strips at the right edge; a missing history was preferred to a scrambled one.

## Card grid overflow

Issue #588: the dashboard chooses the number of card columns and the card size together, widening to more columns when that is what it takes to show every card, so the `(↑a ↓b)` indicator appears only on a window too small for the cards at any layout. The indicator counts cards, not rows, and is styled apart from the title because the issue was a report of the overflow being invisible.

## Keyboard enhancement

PRD #227: the TUI pushes `KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES` only, and only when crossterm's `supports_keyboard_enhancement()` answers yes. It deliberately does not request `REPORT_ALL_KEYS_AS_ESCAPE_CODES`, which would re-encode ordinary text keys as CSI u and change how every dashboard binding arrives. The pop on exit (normal and panic paths) runs only if this process pushed, since an unmatched pop inside a multiplexer can discard another program's flags.
