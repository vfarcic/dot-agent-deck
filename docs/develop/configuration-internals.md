# Configuration internals

Maintainer notes on how the deck's configuration files and environment variables behave, moved out of the published `docs/configuration.md` when that page was rewritten for the user's agent (PRD #1419). The user page says what to set and how to check it; this page records the mechanisms and the gaps behind it.

## Endpoint directory and relocation

When `XDG_RUNTIME_DIR` is unset, both endpoints (`hook.sock`, `attach.sock`) go in `<temp dir>/dot-agent-deck-{uid}/`, created at mode `0700` (issue #1121). The root is `std::env::temp_dir()`, not a literal `/tmp`, so on macOS it is under `$TMPDIR` (`/var/folders/…`). The directory exists so that another local uid cannot create an entry at the endpoint path: under `/tmp`'s sticky bit the deck could not unlink a squatted entry, and the daemon's `bind(2)` would fail with `EADDRINUSE` until root or the squatter cleared it.

The directory's own name is predictable, so another uid can create it first. In that case `daemon serve` binds in a relocated sibling, `dot-agent-deck-{uid}.` plus 16 random hex digits, created at `0700`, and the connect side finds it by listing (issue #1173, `crate::endpoint_resolve::prepare_bind_endpoint`). The relocation is logged with `tracing::warn!` ("this deck's endpoints are in … instead of …"), which reaches a file only when `DOT_AGENT_DECK_LOG` is set for the daemon. `platform::paths::fallback_endpoint_dir` does not know about relocation and must not: it stays the pure answer for a host where the name is free.

Since issue #1211 the daemon also binds the pre-#1121 spellings, `/tmp/dot-agent-deck-{uid}.sock` and `/tmp/dot-agent-deck-attach-{uid}.sock` (a literal `/tmp`, because that is what older builds wrote), as best-effort aliases so an older client finds a newer daemon. An alias bind is allowed to fail; the daemon unlinks only its own stale socket there, and at exit only the inode it bound. `platform::paths::legacy_socket_path` carries the full reasoning.

## `config.toml` load and save

`DashboardConfig::load` (TUI and `config get`/`set`) falls back to defaults on a parse error and prints `Invalid config at …` to stderr. `config set` then calls `save`, which serialises the struct with `toml::to_string_pretty` and writes the whole file. Two consequences the user page states: comments and unknown keys do not survive a `config set`, and a `config set` against an unparseable file replaces it with defaults plus the one key set. `default_dir` is skipped when empty so an unrelated `config set` does not write `default_dir = ""`.

The daemon reads the same file with `DashboardConfig::load_bounded` (PRD #1223 audit A4) per `NewAgentOptions` request: `O_NONBLOCK` open, regular files only, at most 1 MiB (`MAX_DASHBOARD_CONFIG_BYTES`, the same cap as `MAX_PROJECT_CONFIG_BYTES`). Anything else yields defaults and a `warn!` that omits the TOML error's snippet. `issue_dispatch` scheduled tasks and the daemon's single-agent `dispatch` read `default_command` through `DashboardConfig::load` at fire time.

## `auto_config_prompt` and `config-gen-state.json` are written but not read

`auto_config_prompt` is accepted by `config get`/`set` and serialised, and the TUI's Generate dialog prints `Disable: dot-agent-deck config set auto_config_prompt false`, but no code path reads the field (`grep -rn auto_config_prompt src` finds only `config.rs` and that dialog line). The dialog's **Never** option records the directory in `config-gen-state.json` (`ConfigGenState::suppress_dir`), but `is_suppressed` has no production caller, and the card hint the **No** option refers to ("hint stays on card") no longer exists. The dialog opens only when the user presses `g` (the remappable `generate_config` action) or clicks the dashboard's **Generate** button, so there is nothing for either setting to suppress today. The New Agent form's `Tip: press g on dashboard to create a config` line (`src/ui.rs`, in the form renderer) is likewise unreachable: it renders only when the form has no Mode row, and since PRD #127 M3.2 `NewPaneFormState::new` always sets `has_mode_field = true` (the `schedule` authoring option is always offered), while the schedule-locked form skips the tip branch. The user page describes this as it is; fixing it (reading the flag, or removing the key and the **Never** option) is a separate change.

## Which process reads which variable

Environment variables are read by the process they are set in, and a daemon's environment is fixed when it starts. A lazily spawned daemon inherits the environment and working directory of the client that spawned it (the TUI, the desktop app, or a CLI verb), so a variable the daemon reads takes effect only after `dot-agent-deck daemon stop` and a relaunch from an environment that carries it. `DOT_AGENT_DECK_DRAFT_DEFER_CAP_MS` is read once when the daemon's `AgentPtyRegistry` is constructed; the delegate tuning variables (`…_READINESS_BUFFER_MS`, `…_RETRY_SCHEDULE_MS`, `…_NO_EVENT_WINDOW_MS`, `…_WAITING_NOTICE_DEBOUNCE_MS`) are read at use time, but still from the daemon's own environment.

The `[features] experimental` flag is read independently by the TUI and the daemon, each from the `.dot-agent-deck.toml` found by walking up from its own launch directory to the first regular file owned by the current uid (`config::resolve_project_dir`, issue #577), and re-read every ~2 s with a 200 ms settle. The desktop app does no walk: it reads `DOT_AGENT_DECK_EXPERIMENTAL`, else the file `DOT_AGENT_DECK_FEATURES_CONFIG` names, once at startup (`features::init_from_process_env`).

## Environment variables left out of the user docs

These exist in the source and are deliberately not documented for users, because they are test seams, developer switches or values the deck sets itself:

- Test and e2e seams: every `DOT_AGENT_DECK_TEST_*` variable, `DOT_AGENT_DECK_WORKER_RESPONSE_TIMEOUT_MS` (milliseconds override of `worker_response_timeout_minutes`), `DOT_AGENT_DECK_SESSION_START_WAIT_MS`, `DOT_AGENT_DECK_SEED_FALLBACK_SECS`, `DOT_AGENT_DECK_EXIT_WHEN_ORPHANED`, `DOT_AGENT_DECK_EXIT_AFTER_HANDSHAKE`, `DOT_AGENT_DECK_BUILD_ID_OVERRIDE` (compiled out of release builds), `DOT_AGENT_DECK_WRAP_BIN`, `DOT_AGENT_DECK_AMBIENT_GIT_SANDBOX` / `DOT_AGENT_DECK_AMBIENT_GIT_CHILD`, `DOT_AGENT_DECK_KEYCHAIN_TEST`, `DOT_AGENT_DECK_DESKTOP_SEED_BUFFER_MS`, and the `DAD_*` build and test variables.
- Developer switches: `DOT_AGENT_DECK_LOCK_DIR` (daemon lock-file root), `DOT_AGENT_DECK_DESKTOP_ALLOW_BUILD_MISMATCH` (the in-app **Connect anyway** is the user-facing equivalent).
- Set by the deck for its own children rather than by a user: `DOT_AGENT_DECK_PANE_ID`, `DOT_AGENT_DECK_AGENT_ID`, `DOT_AGENT_DECK_PANE_CAPABILITY`, `DOT_AGENT_DECK_VIA_DAEMON`. The user page names the first and third because their absence explains a refused `work-done`/`delegate`.
