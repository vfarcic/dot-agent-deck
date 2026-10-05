# Agent-config containment in tests

The deck writes into configuration that belongs to other programs: Codex's `hooks.json` and `config.toml`, Claude Code's `settings.json`, Devin's `config.json`, the OpenCode plugin file and the Pi extension. In production those live in the user's real home, which is the point. In a test they must not, and on 2026-10-03 they did: fast-tier fixtures in `tests/delegate_prompt_injection.rs` spawned Codex stand-ins through the registry without isolating `HOME`, the wrapper's automatic install ran against the operator's real `~/.codex`, and — because the binary under test sat in a custom `CARGO_TARGET_DIR` that was also on `PATH` — it pinned that build's path beside the installed deck's. Codex then held every start on its hook-review screen and wedged a worker for hours (PRD #1487; the diagnosis is in the PR).

Three fixes landed together. This page is the one for contributors; the other two are product behaviour.

## The contract

`src/config_write_guard.rs::ensure_config_write_allowed` is called by every agent-config writer before its first side effect — `create_dir_all`, a temp file, a `.bak` backup, the rename, or a removal:

| writer | guarded at |
| --- | --- |
| Codex `hooks.json` install / uninstall | `codex_hooks_manage::install_to_reporting`, and the shared publish |
| Codex `config.toml` trust | `codex_hooks_manage::edit_trust_state` |
| Claude Code `settings.json` | `hooks_manage::write_settings` |
| Devin `config.json` | `devin_hooks_manage::install_to_reporting`, and the shared publish |
| OpenCode plugin (install, stale-layout removal, uninstall) | `opencode_manage::write_plugin_reporting`, `uninstall_impl` |
| Pi extension | `orchestrator_ext::materialize` |
| any `agent_hook_config::write_atomic` / `backup_malformed` | `agent_hook_config::publish` |

Two environment variables carry it:

- `DOT_AGENT_DECK_TEST_CONFIG_WRITE` — any non-empty value arms containment.
- `DOT_AGENT_DECK_TEST_CONFIG_ROOT` — the owned root(s), separated like `PATH`. With the marker set explicitly, an unset or empty root refuses every write.

When armed, the destination is made absolute and its longest existing ancestor is canonicalized (so symlinks and `..` resolve the way the writer's own open will); the rest is appended. It must land under a canonicalized root, or the writer returns `PermissionDenied` without having created anything. A missing component that is `..`, or an entry that exists but does not resolve (a dangling symlink), is refused rather than guessed at. Without the marker and outside a test process the guard does nothing, so users see no difference.

## Armed by default in every test process

A test does not opt in. The guard also arms itself when:

- the code is the lib target's own unit-test binary (`cfg(test)`), or
- the process has `NEXTEST` in its environment — set by `cargo nextest` for every test it runs (every alias in `.cargo/config.toml` is nextest) and inherited by every child that does not clear its environment.

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

- **One deck entry per event.** An install replaces the deck's own entries in place — any command in the deck's hook shape naming an executable with the installing binary's basename — and consolidates duplicates, while user handlers keep their indices (Codex trust is positional, issue #1034). This reverses issue #730's preserve-a-valid-sibling policy on the events an install writes; the retired-event sweep still leaves a live sibling's rule alone. Shared by Codex, Claude and Devin as `agent_hook_config::consolidate_deck_handlers_in_place`.
- **No-op is not a write.** Every writer compares the merged result with what is on disk and publishes nothing when they are equal, so an unchanged file keeps its bytes, inode and mtime. When an automatic install does change a file it logs the destination, the pinned binary, the trigger and the pid.

Custom cargo output is recognised by cargo's own `.fingerprint/` and `deps/` directories beside the binary (`platform::paths::is_cargo_output_dir`), whatever the target directory is called, so it never outranks an installed deck in `durable_binary_path`.
