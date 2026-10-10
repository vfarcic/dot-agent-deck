# Agent-config containment in tests

The deck writes into configuration that belongs to other programs: Codex's `hooks.json` and `config.toml`, Claude Code's `settings.json`, Devin's `config.json`, the OpenCode plugin file and the Pi extension. In production those live in the user's real home, which is the point. In a test they must not, and on 2026-10-03 they did: fast-tier fixtures in `tests/delegate_prompt_injection.rs` spawned Codex stand-ins through the registry without isolating `HOME`, the wrapper's automatic install ran against the operator's real `~/.codex`, and — because the binary under test sat in a custom `CARGO_TARGET_DIR` that was also on `PATH` — it pinned that build's path beside the installed deck's. Codex then held every start on its hook-review screen and wedged a worker for hours (PRD #1487; the diagnosis is in the PR).

Three fixes landed together. This page is the one for contributors; the other two are product behaviour.

## The contract

`src/config_write_guard.rs::ensure_config_write_allowed` is called by every agent-config writer before its first side effect — `create_dir_all`, a temp file, a `.bak` backup, the rename, or a removal:

| writer | guarded at |
| --- | --- |
| Codex `hooks.json` install / uninstall | `codex_hooks_manage::install_to_reporting`, and the shared publish |
| Codex `config.toml` trust | `codex_hooks_manage::edit_trust_state` |
| Claude Code `settings.json` | `hooks_manage::write_settings`, and `lock_settings_for_install` before it creates the directory |
| Devin `config.json` | `devin_hooks_manage::install_to_reporting`, and the shared publish |
| OpenCode plugin (install, stale-layout removal, uninstall) | `opencode_manage::write_plugin_reporting`, `uninstall_impl` |
| Pi extension | `orchestrator_ext::materialize` |
| any `agent_hook_config::write_atomic` / `backup_malformed` | `agent_hook_config::publish` |
| the cross-process lock sidecar `.<name>.lock` and its stale temp-file reap | `agent_hook_config::lock_config`, which takes no lock where the guard would refuse the config's write — the writer's own guard then refuses it, and a no-op uninstall stays a no-op |

Two environment variables carry it:

- `DOT_AGENT_DECK_TEST_CONFIG_WRITE` — any non-empty value arms containment.
- `DOT_AGENT_DECK_TEST_CONFIG_ROOT` — the owned root(s), separated like `PATH`. With the marker set explicitly, an unset or empty root refuses every write.

When armed, the destination is made absolute and its longest existing ancestor is canonicalized (so symlinks and `..` resolve the way the writer's own open will); the rest is appended. It must land under a canonicalized root, and so must the destination's own directory entry — its parent resolved the same way, with the final name appended unresolved — or the writer returns `PermissionDenied` without having created anything. The two differ only when the final component is a symlink, and the second is the one the writer actually acts on: its temp file and backup are created beside that entry, the rename replaces it and a removal unlinks it, none of which follows the symlink. Judging only where the symlink points let a config symlink outside the owned root that pointed inside it pass, and the publish then renamed over the outside symlink (issue #1614; `hooks/containment/004`). A missing component that is `..`, or an entry that exists but does not resolve (a dangling symlink), is refused rather than guessed at. Every refusal is logged at `warn!` (`agent config write refused by test containment`) with the destination and the reason, so a write that did not happen is never silent. Without the marker and outside a test process the guard does nothing, so users see no difference.

**It is best-effort protection against accidental misconfiguration, not a sandbox** (PRD #1487 audit A4). The check judges the path when it is called; the writer then creates its temp file, renames and removes by pathname. A directory on the path swapped for a symlink *between* the check and the write — a concurrent ancestor swap — is outside what it guarantees. That is the right scope for what it is for: the failure it exists to stop is a test that forgot to isolate `HOME`, which is a static misconfiguration, not a race. Nothing in the deck swaps directories under a writer, and a fixture that did would be attacking its own scratch directory. Making it race-resistant would mean opening every ancestor with no-follow, beneath-root resolution and handle-relative create and rename for each writer — a rewrite this guard was deliberately not built to justify.

## Armed by default in every test process

A test does not opt in. The guard also arms itself when:

- the code is the lib target's own unit-test binary (`cfg(test)`), or
- the process has both `NEXTEST` and `NEXTEST_RUN_ID` non-empty in its environment — the pair `cargo nextest` sets for every test it runs (every alias in `.cargo/config.toml` is nextest), inherited by every child that does not clear its environment. `NEXTEST` alone does not arm it (review S4): it is a generic-looking name a user's shell can carry for unrelated reasons, and arming on it would refuse that user's real config writes. The run id is nextest's own; nothing in this repository sets it.

Armed that way with no `DOT_AGENT_DECK_TEST_CONFIG_ROOT`, the roots default to where tests make scratch directories (`config_write_guard::default_test_roots`): `/var/tmp/dad-e2e-<uid>`, `DAD_E2E_TMPDIR` when set, and the OS temp dir. The operator's home is none of them, so a test that forgets to isolate `HOME` gets a refused write (an auto-install logs a warning and carries on; an explicit install returns the error) instead of rewriting real config.

A child started with `env_clear` inherits neither signal. The e2e harness therefore pins both variables into every environment it builds from scratch — `TuiDeck`'s and `DaemonProc`'s — through `tests/common/mod.rs::config_containment_env`, with the process's harness root plus the default roots. A test that moves `HOME` somewhere else with `with_env` is judged at write time against those roots.

## Writing a test that installs hooks

- Put every agent home the code under test can reach — `HOME`, `CODEX_HOME`, `XDG_CONFIG_HOME`, `PI_CODING_AGENT_DIR` — inside a directory from `test_temp::tempdir()` or `harness_tempdir()`.
- Registry spawns (`spawn_agent` / `SpawnOptions`) inherit the test process's environment unless `env` overrides it. Pass the homes in `SpawnOptions::env` so respawns carry them too; `owned_wrapped_agent_env` in `tests/delegate_prompt_injection.rs` is the worked example, and it also sets the marker with the fixture's own root, which is the strictest form.
- A `Command` you start with `env_clear` needs the marker and a root of its own if you want it contained; `tests/e2e_agent_config_containment.rs` drives the real wrapper that way.
- A test that deliberately exercises the refusal sets `DOT_AGENT_DECK_TEST_CONFIG_WRITE=1` with an empty or foreign root in a child process — never in the test process itself, where nextest runs threads.

## What it does not cover

- Plain `cargo test` of an integration-test binary: it sets no `NEXTEST`, so only `cfg(test)` (the lib's unit tests) and the harness-pinned environments are armed. The repo's aliases all run nextest.
- A child started with `env_clear` and given neither variable — for example a raw `Command` in a fast-tier test that builds its own environment. Such a child must pin `HOME` itself, as before.
- The default roots include the OS temp dir, so a test that points `HOME` somewhere under it is allowed. It protects the operator's home; it does not prove a test used its *own* scratch directory.
- Windows: the guard is the same, but the default roots are the OS temp dir alone.

## The other two fixes, for reference

- **One deck entry per event.** An install consolidates the deck's own entries — any command in the deck's hook shape naming an executable with the installing binary's basename — down to one per event, refreshed where the first one sits, while user handlers keep their indices (Codex trust is positional, issue #1034). Shared by Codex, Claude and Devin as `agent_hook_config::consolidate_deck_handlers_in_place`. Which command that one entry carries depends on the install mode (`agent_hook_config::InstallMode`):
  - **Explicit** (`hooks install`): the installing binary's, replacing whatever install was there. This reverses issue #730's preserve-a-valid-sibling policy on the events an install writes.
  - **Automatic** (TUI and daemon startup, `wrap --agent codex`): when the event's first deck entry that names a live, durable install (`agent_hook_config::auto_install_keeps` → `platform::paths::is_live_durable_install`: absolute, positively reported to exist, an executable file, not cargo output by `is_build_artifact_path`) belongs to another install, that command is written back verbatim, so nothing changes and the file is not published; an event with no such entry gets the file's first kept command, so the file keeps naming one install. Only a dead or build-output entry is replaced by the installing binary — and so is one whose existence cannot be read (a permission error on the path): the keep rule fails safe toward replacing, so uncertainty never strands a stale entry beside the installing binary's. That is stricter than `pin_is_repairable`, which leaves such a pin alone because repairing it is a rewrite nobody asked for. Without this, two installs that each resolve to themselves (a Homebrew TUI and a `~/.local/bin` daemon, the desktop's bundled daemon and a CLI) rewrote the file on every start, and a Codex pane then needed fresh trust. Codex's trust write is about the kept install's command, so an unchanged file needs no new grant. The OpenCode plugin follows the same rule (`opencode_manage::auto_install_to`), including the fail-safe half: its automatic install used to fall back to `pin_is_repairable` for a pin it did not keep, which left an unstatable pin in place, and since the PRD #1487 re-check (R3) anything not positively live and durable is replaced by the installing binary there too (`auto_install_replaces_a_pin_whose_existence_cannot_be_read`). The Pi extension pins no binary, so there is nothing to keep.
  - The retired-event sweep still leaves a live sibling's rule alone for Codex and Devin. Claude's retired-type sweep is binary-agnostic, so two installs that disagree on which hook types to install still remove each other's extra types.
- **No-op is not a write.** Every writer compares the merged result with what is on disk and publishes nothing when they are equal, so an unchanged file keeps its bytes, inode and mtime. When an automatic install does change a file it logs the destination, the pinned binary, the trigger and the pid.

Custom cargo output is recognised by cargo's own `.fingerprint/` and `deps/` directories beside the binary (`platform::paths::is_cargo_output_dir`), whatever the target directory is called, so it never outranks an installed deck in `durable_binary_path`.
