# Pre-daemon parity audit

**PRD**: [#92 — Pre-daemon parity audit](../prds/92-process-boundary-invariant-audit.md)
**Branch**: `prd-92-process-boundary-invariant-audit`
**Audited**: 2026-05-22
**Baseline**: `2fc39c3` — *"chore: update docs chart to v0.24.7 [skip ci]"*, the last commit before PRD #76 (`4b81c06`) merged. Pre-daemon state.
**Current**: tip of branch (= `main` at `4087df2`, plus the v2 PRD rewrite).
**Methodology**: feature/behavior list built FROM baseline (`git worktree add /tmp/dot-agent-deck-baseline 2fc39c3`); each item verified against current code via code-read and tests. `Explore` agents fanned out across TUI keybindings/dialogs, dashboard/session/modes, CLI commands/env vars, and the quit/lifecycle surface. Triage buckets per PRD #92 v2: **Preserved** / **Regressed** / **Intentional change** / **Unclear**.

## Coverage statement

Baseline sources read:

- `docs/getting-started.mdx`, `docs/installation.md`, `docs/keyboard-shortcuts.md`, `docs/configuration.md`, `docs/session-management.md`, `docs/troubleshooting.md`, `docs/workspace-modes.md` at `2fc39c3`.
- `src/main.rs` at `2fc39c3` for the CLI surface.
- `src/lib.rs` at `2fc39c3` for the module map.
- The 22 shipped PRDs in `prds/done/` at `2fc39c3` (read as titles; spot-checked when a behavior was unclear).
- `tests/integration_test.rs`, `tests/mode_integration_test.rs`, `tests/session_restore_test.rs` at `2fc39c3` (the entire baseline integration suite — 1265 LOC total).

Feature categories covered:

- CLI commands and global flags.
- TUI keybindings (global, tab nav, mode tab, dashboard, dialogs, directory picker, new-pane form).
- Dashboard card display (statuses, density, fields).
- Session lifecycle (save on exit, `--continue` restore, mode-tab restoration).
- Workspace modes (config schema, persistent panes, reactive panes, circular pool).
- Daemon socket / hook ingestion (the in-process daemon's external surface — env vars, file paths).
- Environment variables (`DOT_AGENT_DECK_SOCKET`, `_CONFIG`, `_SESSION`, `_LOG`).
- Auto-install of hooks on TUI startup.
- Idle ASCII art (config keys, CLI standalone).
- Quit / shutdown lifecycle (the F1 worked-example surface).

Post-baseline additions (out of scope per PRD #92; listed in the *Intentional changes appendix* below):

- `daemon serve`, `daemon hello` subcommands.
- `remote add|list|remove|upgrade` and `connect` subcommands.
- New env var `DOT_AGENT_DECK_ATTACH_SOCKET`.
- Auto-spawn of the daemon, idle-shutdown logic, attach protocol versioning.
- `DOT_AGENT_DECK_PANE_ID` const moved into `agent_pty.rs` (refactor only).

A future re-audit can extend this coverage by reading any PRD-shipped feature in `prds/done/` that landed before `2fc39c3` and re-checking parity against current main.

## Worked example: force-shutdown gap (F1)

At baseline, quitting the deck killed every agent — period. `src/main.rs:413–418` in the baseline:

```rust
// TUI exited — clean up
daemon_handle.abort();

if path.exists() {
    let _ = std::fs::remove_file(&path);
}
```

The daemon was an in-process tokio task (`tokio::spawn` at baseline `src/main.rs:378`); the agent PTYs were `tokio` children of the deck process (`pane::detect_multiplexer` returning an in-process `PaneController`). Exiting the TUI aborted the daemon task, removed the hook socket file, and let the deck process exit — taking every agent PTY with it. The baseline quit dialog (`docs/keyboard-shortcuts.md:93`) was a literal **Yes / No** confirmation that quitting would happen.

In current code (post PRD #76 + PRD #93):

- The daemon is fork-execed detached via `setsid(2)` into its own session (`src/daemon_attach.rs:343`); the comment at `src/main.rs:626–630` makes this explicit — *"the daemon was fork-execed detached by `ensure_external_daemon_or_die` (setsid'd into its own session) so it is intentionally outside this process tree: we do not abort the daemon and do not unlink its sockets. Agents must survive TUI exit (PRD #76 line 199)."*
- Agent PTYs are owned by the daemon's `AgentPtyRegistry` (`src/agent_pty.rs:805+`), spawned via `daemon.rs:709–710`, well outside the TUI's process tree.
- The quit dialog was deliberately collapsed to **Detach / Cancel** per PRD #93 M4.2 — *"every pane is daemon-backed so quitting the TUI is always a detach, never a kill"* (`src/ui.rs:1666–1700, 535–536, 599, 5328–5339`).
- The daemon idle-shuts-down only when *both* `clients == 0 AND agents == 0` (`src/daemon.rs:480–551`), so as long as any agent is alive, the daemon stays up — by design (PRD #93 line 32).
- **Neither `DaemonCmd::Stop` nor `RemoteCmd::Stop` exists.** `DaemonCmd` has only `Serve` and `Hello` (`src/main.rs:135–150`); `RemoteCmd` has only `Add`, `List`, `Remove`, `Upgrade` (`src/main.rs:160–209`); `Remove` deregisters a local registry entry, it does not touch the remote daemon.

PRD #93 line 39 anticipated this exact gap: *"User can `dot-agent-deck remote stop` (or equivalent local command) to force shutdown."* The promise was never met. Today's only way to stop the daemon while agents are managed is `pkill dot-agent-deck`. This is the parity regression at the heart of the audit — the same user gesture (quit the deck) used to kill the agents, and now no in-product gesture does.

This is the only Regressed row in the audit. Everything else is Preserved or Intentional change with citation.

## Findings

| # | Baseline feature | Triage | Baseline evidence (`2fc39c3`) | Current evidence | Rationale | Follow-up |
|---|---|---|---|---|---|---|
| 1 | **`dot-agent-deck` runs the TUI dashboard** (two-column: 1/3 dashboard, 2/3 panes) | Preserved | `src/main.rs:155–158`, `docs/getting-started.mdx:60–73` | `src/main.rs:257`, `src/ui.rs:700` (`dashboard_pane_dims` → `[33,67]`) | Same entrypoint, same layout proportions. | — |
| 2 | **`--continue` restores last session** (panes from `~/.config/dot-agent-deck/session.toml`; mode tabs restored with their full structure; warn-to-stderr if directory missing or mode renamed) | Preserved | `src/main.rs:19–20`, `docs/session-management.md:43–57` | `src/main.rs:25–26, 574–638`; `src/ui.rs:2247, 2600–2700`; `src/config.rs:319` (env override); `tests/session_restore_test.rs` | Same flag, same on-disk format, same warn-and-skip semantics; the hydration path additionally consults the daemon registry first (PRD #76 M2.11/12), but for `--continue` parity the saved-session file remains authoritative for dir/name/command, deduped against live daemon panes. | — |
| 3 | **`--theme` flag** (auto/light/dark) | Preserved | `src/main.rs:25–27` | `src/main.rs:31–33` | Identical clap definition. | — |
| 4 | **`hook --agent <claude-code\|opencode>`** subcommand reads stdin, sends to daemon socket | Preserved | `src/main.rs:40–44`; `src/hook.rs::send_to_socket` | `src/main.rs:46–49`; `src/hook.rs:243–250` | Same default (claude-code), same wire path. | — |
| 5 | **`hooks install` / `hooks uninstall` (with `--agent`)** | Preserved | `src/main.rs:46–48, 110–124` | `src/main.rs:52–55, 211–225` | Same shape, same defaults. | — |
| 6 | **Auto-install hooks on TUI startup** (silent / idempotent / best-effort; missing agent dir silently skipped; Claude Code: SessionStart/SessionEnd/UserPromptSubmit/PreToolUse/PostToolUse/Notification/Stop/PreCompact/SubagentStart/SubagentStop; OpenCode: JS plugin at `~/.opencode/plugin/dot-agent-deck/index.js`) | Preserved | `src/main.rs:393–395`; `docs/troubleshooting.md:51–58` | `src/main.rs:604–605`; `src/hooks_manage.rs:174–525` (with tests) | Same call site, same idempotent semantics. | — |
| 7 | **`config get` / `config set`** (with `config_keys_help`) | Preserved | `src/main.rs:50–53, 212–238` | `src/main.rs:57–60, 310–336` | Same shape. | — |
| 8 | **`ascii --input --output [--provider --model]`** standalone CLI ASCII art | Preserved | `src/main.rs:55–69, 428–442` | `src/main.rs:62–75, 816–831` | Same flags, same default behavior. | — |
| 9 | **`init [--path]`** generate `.dot-agent-deck.toml` template (does not overwrite existing) | Preserved | `src/main.rs:70–75` | `src/main.rs:77–80, 337` | Same default `.`, same non-destructive semantics. | — |
| 10 | **`validate [--path]`** validate config; prints `Config is valid.` on success; lists issues with appropriate exit code | Preserved | `src/main.rs:76–81, 307–337` | `src/main.rs:83–86, 491–521` | Identical output text and exit-code mapping. | — |
| 11 | **`watch --interval <secs> <command>`** periodic re-exec | Preserved | `src/main.rs:82–89` | `src/main.rs:89–94, 338–340` | Same shape. | — |
| 12 | **`delegate --task --to <role>...`** orchestrator delegate; requires `DOT_AGENT_DECK_PANE_ID`; fails with specific error if unset; fails if `--to` empty | Preserved | `src/main.rs:90–98, 243–276` | `src/main.rs:97–103, 341–373` | Same shape and error messages; the env var is now imported as a const from `agent_pty.rs` rather than a literal — functionally equivalent. | — |
| 13 | **`work-done --task [--done]`** worker work-done signal; same env-var requirement | Preserved | `src/main.rs:99–107, 277–306` | `src/main.rs:106–112, 375–403` | Same shape. | — |
| 14 | **Quit kills agents** (closing the deck process aborts the in-process daemon and removes the hook socket; agent PTYs die with the process) | **Regressed** | `src/main.rs:413–418` (the `daemon_handle.abort()` + `remove_file` block) | Daemon now setsid'd into its own session (`src/daemon_attach.rs:343`); TUI exit no longer aborts daemon or unlinks sockets (`src/main.rs:626–630`); no `DaemonCmd::Stop` or `RemoteCmd::Stop` exists (`src/main.rs:135–150, 160–209`) | The same user action — "quit the deck" — no longer reaches the agents. The daemon persists; agents persist; there is no in-product gesture to stop them. PRD #93 line 39 anticipated needing a force-shutdown command but it never shipped. | **F1** below |
| 15 | **Quit confirmation dialog** (Ctrl+C in command mode → Yes/No to quit immediately) | Intentional change | `docs/keyboard-shortcuts.md:93`; `src/ui.rs:1332–1358` | `src/ui.rs:1666–1700, 535–536, 599, 5328–5339` | Dialog collapsed to **Detach / Cancel** per PRD #93 M4.2 (`Index 0 is Detach, index 1 is Cancel`). Visible symptom of the architectural pivot; the "Yes kills everything" path is no longer reachable. See **Intentional changes appendix § A**. | — |
| 16 | **Ctrl+d (enter command/navigation mode)** | Preserved | `src/ui.rs:2853` (handler) | `src/ui.rs:3645` (sets `UiMode::Normal`) | Identical. | — |
| 17 | **Ctrl+n (new pane → directory picker → name+command form)** | Preserved | `src/ui.rs:2862–2868` | `src/ui.rs:3694–3699` | Identical entry into `DirPicker`. | — |
| 18 | **Ctrl+w (close selected pane / tear down mode tab; dashboard tab cannot be closed)** | Preserved | `src/ui.rs:2871+` | `src/ui.rs:3702–3759` | Tab-vs-pane branching present in current code. | — |
| 19 | **Ctrl+t (toggle stacked / tiled layout)** | Preserved | `src/ui.rs:2807–2815` | `src/ui.rs:3665–3691` | Same toggle. | — |
| 20 | **Ctrl+C in PaneInput delivers SIGINT (0x03)** | Preserved | `src/ui.rs::keyevent_to_bytes` | `src/ui.rs:1470–1480` (`Ctrl+c` → `vec![0x03]`) | Identical encoding. | — |
| 21 | **Tab navigation: Ctrl+PageDown/Up (any mode); Tab/Right/l, Shift+Tab/Left/h (command mode only)** | Preserved | `src/ui.rs` (multiple handlers) | `src/ui.rs:3762–3781, 3796–3823` | Same key sets, same gating on Normal-mode-only for the letter/arrow variants. | — |
| 22 | **Mode-tab pane focus: j/k or Down/Up cycle agent↔side panes; Enter enters PaneInput; Esc returns focus to agent** | Preserved | `src/ui.rs:3086–3089` (Esc) | `src/ui.rs:3844–3922` | Identical cycling logic. | — |
| 23 | **Dashboard interactive keys: 1–9 jump-to-card, `/` filter, `r` rename, `g` generate-config, `?` help, Esc clears filter** | Preserved | `src/ui.rs:1875–1895, 1730+` | `src/ui.rs:3600–3636, 1875–1895, 1730` | All six keys behave the same; `g` still uses the Yes/No/Never three-option dialog and respects `auto_config_prompt`. | — |
| 24 | **y / n approve / deny permission request** (documented in baseline help overlay) | Preserved | `src/ui.rs:4531` (help text) but **no handler** for plain `y` / `n` at command mode | `src/ui.rs:5536` (help text) but **no handler** for plain `y` / `n` at command mode | Baseline help text documents `y` / `n` approve/deny but no handler existed at baseline either (only `KeyCode::Char('n')` triggers Ctrl+n new-pane); current code matches. Pre-existing defect rooted in PRD #18 (permission prompt control), not a parity regression introduced by the pivots. See **F2** follow-up draft below for the underlying bug. | **F2** |
| 25 | **Directory picker keys** (j/k navigate, l/Enter enter, h/Backspace up, Space confirm, `/` filter, Esc clear, `q` cancel; loops; `..` always visible) | Preserved | `src/ui.rs` (DirPicker handler) | `src/ui.rs:2090–2125` | Same eight keys present and tested. | — |
| 26 | **New-pane form keys** (Tab/Shift+Tab cycle fields; Left/Right/h/l cycle mode selector; Enter confirm; Esc cancel) | Preserved | `src/ui.rs` (form handler) | `src/ui.rs:2159–2228` | Identical. | — |
| 27 | **Filter dialog** (type to narrow, Backspace delete, Enter accept-and-stay-filtered, Esc clear+close) | Preserved | baseline tests cover this | `src/ui.rs:1901–1924`; `ui.rs::test_filter_typing, ::test_filter_esc_clears` | Same semantics, tested. | — |
| 28 | **Rename dialog** (type, Enter confirm, Esc cancel) | Preserved | baseline tests | `src/ui.rs:1994–2019`; `ui.rs::test_rename_handler_*` | Same, tested. | — |
| 29 | **Help overlay dismiss with `?` / Esc / `q`** | Preserved | baseline | `src/ui.rs:1926–1934` | All three dismiss keys present. | — |
| 30 | **Six session statuses** (Thinking / Working / Compacting / WaitingForInput / Idle / Error) | Preserved | `src/state.rs:15–22` | `src/state.rs:17–24` | Identical enum. | — |
| 31 | **Card display fields** (Title row with card number + display name + animated dot + status label; `Dir:` truncated basename; `Last:` elapsed time + `Tools:` count; `Prmt:`; recent tool calls) | Preserved | `src/ui.rs:4776–5046` | `src/ui.rs:5781–6051` | All fields present in the same shape. | — |
| 32 | **Card density auto-selection** (Spacious: 3+3; Normal: 1+3; Compact: 1+1) | Preserved | `src/ui.rs:50–100` | `src/ui.rs:50–100` | Same thresholds. | — |
| 33 | **Session save on exit** (panes' dir/name/command saved automatically) | Preserved | `SavedSession::snapshot`; `tests/session_restore_test.rs` | Same path; same tests still pass after the daemon pivots. | The on-disk format is unchanged; the deduplication against the live daemon registry on `--continue` is an additive layer that does not alter the saved-side schema. | — |
| 34 | **Mode tab restoration on `--continue`** (each agent pane records its mode; reopens full mode tab — agent + side panes — by looking up `.dot-agent-deck.toml`; agent conversation NOT restored; warn-to-stderr if config missing or mode renamed) | Preserved | baseline `src/ui.rs:1909–1927` (fallback warn) | `src/ui.rs:2324` (`TabMembership::Mode { name }` lookup); test `session_restore_test.rs` | Same fallback, same restored shape. Daemon-side tab_membership (M2.12) is additive — it lets the daemon's own registry rebuild tabs on attach; the saved-session file remains the authority for restart cases. | — |
| 35 | **Session/config file paths** (`~/.config/dot-agent-deck/session.toml`, overridable via `DOT_AGENT_DECK_SESSION`; `~/.config/dot-agent-deck/config.toml` overridable via `DOT_AGENT_DECK_CONFIG`) | Preserved | `src/config.rs:255–265` | `src/config.rs:312–322` | Same env-var overrides, same default paths. | — |
| 36 | **`DOT_AGENT_DECK_SOCKET` env var** (hook ingestion socket path; baseline default `$XDG_RUNTIME_DIR/dot-agent-deck.sock` or `/tmp/dot-agent-deck.sock`) | Intentional change | `src/config.rs:52–62` at baseline | `src/config.rs:52–68` in current | The env var is still honored as an override and the XDG_RUNTIME_DIR default is unchanged. The `/tmp` fallback path was changed to include a uid suffix (`/tmp/dot-agent-deck-{uid}.sock`) for multi-user host safety. Introduced in commit `48b9180` (the PRD #93 squash-merge); rationale lives in the inline code comment at `src/config.rs:61–67` (and the mirror at `:85–91` for `attach_socket_path`). The doc-default in `docs/configuration.md:22` is now slightly out-of-date; not a behavior regression. See **Intentional changes appendix § B**. | — |
| 37 | **`DOT_AGENT_DECK_LOG` env var** (empty/`1` → `/tmp/dot-agent-deck.log`; any other value used verbatim) | Preserved | `src/main.rs:346–351` | `src/main.rs:535–540` (`init_logging_from_env` helper) | Identical default + override behavior. | — |
| 38 | **Workspace modes** (`[[modes]]` with `name`, optional `init_command`, `panes`, `rules`, `reactive_panes` default 2) | Preserved | `src/project_config.rs:38–42` | `src/project_config.rs:38–42` | Identical schema. | — |
| 39 | **Persistent panes** (`[[modes.panes]]` with `command`, optional `name`, `watch` default true; watch re-execs every 10s via built-in `watch`) | Preserved | `src/project_config.rs:51–55` | `src/project_config.rs:51–55` | Same. | — |
| 40 | **Reactive panes / rules** (`[[modes.rules]]` with `pattern` regex, `watch`, `interval`; start empty; populate on regex match) | Preserved | `src/project_config.rs:62` | `src/project_config.rs:62` | Same. | — |
| 41 | **Circular pane pool** (persistent first; reactive cycle; oldest reused when full) | Preserved | `src/mode_manager.rs:163, 335–345` | `src/mode_manager.rs:194–196, 396–412` | Same algorithm; covered by `tests/mode_integration_test.rs::reactive_pool_cycling_with_real_config`. | — |
| 42 | **PaneInput in side pane** (Enter → type into the pane's shell; Ctrl+C SIGINT; Ctrl+d exits) | Preserved | `src/ui.rs:1324–1330` | `src/ui.rs:3890–3915`; submit-key parity via `src/pane_input.rs` | The submit-key dance was lifted out of `embedded_pane.rs` into `pane_input.rs` (PRD #93 round 5) but is functionally identical at the user surface; tested by `tests/pane_input.rs`. | — |
| 43 | **Idle ASCII art** (opt-in `idle_art.enabled`; provider/model/timeout_secs config; only in Spacious density; CLI standalone via `ascii`) | Preserved | `src/config.rs:112, 181, 183`; `src/ui.rs::update_idle_art` | `src/config.rs:169, 181, 238–240`; `src/ui.rs:6246–6248` | Same config keys, same density gate, same standalone CLI. | — |
| 44 | **Version update notification** (PRD #22 — background `version::check_for_update()` polls the latest release and surfaces `update_available` on the dashboard) | Preserved | `prds/done/22-version-update-notification.md`; baseline `src/main.rs:384–389` spawns the check; baseline `src/ui.rs:482, 551, 2169–2170` carries `update_available` on `UiState` and mirrors it from the `AppState` snapshot | `src/main.rs:594–599` (same spawn-and-store pattern); `src/ui.rs:510, 586, 3019–3020` (mirror), `src/ui.rs:5297` (rendering in the dashboard footer) | Same background-poll-and-render shape. Preserved-but-untested (no dedicated unit test in `tests/`; exercised by the dashboard render path). | — |
| 45 | **Aggregate stats bar** (PRD #17 — single status bar across the bottom of the dashboard summarizing session counts by status) | Preserved | `prds/done/17-aggregate-stats-bar.md`; baseline `src/ui.rs:3764, 3852` (call sites) and `render_stats_bar` defined earlier | `src/ui.rs:4765, 4853` (call sites); `src/ui.rs:5169–5214` (`fn render_stats_bar`) | Same single-bar layout, called from the same two render contexts. Preserved-but-untested (rendering exercised via the dashboard render path). | — |
| 46 | **Star repo reminder** (PRD #37 — periodic dismissible dialog asking the user to star the GitHub repo; supports snooze and permanent dismiss) | Preserved | `prds/done/37-star-repo-reminder.md`; baseline `src/ui.rs:116` (`UiMode::StarPrompt`), `:498, :559` (`star_prompt_state`), `:1360–1379` (`handle_star_prompt_key` with `dismiss_permanently` + `snooze`), `:1882–1886` (`StarPromptState::load` + activation), `:3107` (mode dispatch) | `src/ui.rs:116` (mode), `:526, :594` (`star_prompt_state`), `:1703` (`handle_star_prompt_key`), `:2272–2276` (load + activation), `:3981` (mode dispatch), `:4901` (rendering) | Same mode, same handler, same snooze / dismiss-permanently semantics. Preserved-but-untested (no unit test in `tests/`). | — |
| 47 | **Terminal bell notifications** (PRD #8 — emit a bell when an agent transitions to `WaitingForInput`; per-status toggles via `BellConfig`; no repeat for unchanged status; cleanup on session removal) | Preserved | `prds/done/8-terminal-bell-notifications.md`; baseline `src/ui.rs:1084` (`compute_bell_needed`), `:2327` (call site) | `src/ui.rs:1410` (`compute_bell_needed`), `:3177` (call site); 8 unit tests at `src/ui.rs:8252–8351` (`bell_on_transition_to_waiting`, `bell_no_repeat_same_status`, `bell_respects_config_toggle_off`, `bell_respects_config_toggle_on`, `bell_disabled_globally`, `bell_multiple_transitions_single_bool`, `bell_cleanup_removed_sessions`, `bell_new_session_triggers`) | Same `compute_bell_needed` signature and behavior, called from the same render-loop site, with strong unit-test coverage. | — |
| 48 | **OpenCode sessions on the dashboard** (PRD #30 — OpenCode sessions render with the `OpenCode` label and OpenCode hook events flow through the same `AgentEvent` pipeline as Claude Code) | Preserved | `prds/done/30-opencode-agent-support.md`; baseline `AgentType::OpenCode` variant in `src/event.rs`, label rendering in `src/ui.rs` | `src/event.rs:26` (variant), `:38–48` (`AgentType::from_command` matches `"opencode"`); `src/ui.rs:33` (`AgentType::OpenCode => write!(f, "OpenCode")`); tests `src/event.rs::agent_type_from_command_recognizes_opencode` (line 230), `::parse_open_code_event` (line 256, validates the `"open_code"` serde tag end-to-end) | Same `AgentType::OpenCode` enum variant; same `"open_code"` wire tag; same dashboard label rendering. The JS plugin install path is covered by row 6 (auto-install) and the CLI dispatch by rows 4–5 (`hook --agent`, `hooks install --agent`); this row covers the user-visible OpenCode-shows-up-in-the-dashboard behavior specifically. | — |
| 49 | **Always-create-pane-card placeholder** (PRD #54 — every newly-spawned pane immediately gets a dashboard card via a placeholder `SessionState` with the `"No agent"` label, so a user who returns to the dashboard before launching an agent can still see / select / close the pane; the placeholder transitions seamlessly to a real session when the first `SessionStart` event arrives) | Preserved | `prds/done/54-always-create-pane-cards.md`; baseline `src/state.rs::insert_placeholder_session` + `register_pane`, baseline `src/ui.rs` `KeyResult::NewPane` handler around `:1783` plus `Ctrl+w` close path | `src/state.rs:175` (`register_pane`), `:179–219` (`insert_placeholder_session` with doc comment "*until a real `SessionStart` hook fires*"), `:485–528` (session-reuse / placeholder transition in `apply_event`), `:220` (`unregister_pane`); tests `src/state.rs::placeholder_session_created` (line 1079), `::placeholder_session_carries_supplied_agent_type` (line 1099, PRD #76 M2.13 extension), `::placeholder_session_defaults_to_none_when_agent_type_unknown` (line 1121) | Same placeholder-on-pane-create, same `"No agent"` label, same `SessionStart`-triggered transition. The M2.13 hydration extension is additive — placeholder now optionally carries `agent_type` from the daemon registry so a reconnected pane shows `ClaudeCode` / `OpenCode` instead of `"No agent"` (`src/ui.rs:2304` comment), but the baseline behavior is preserved when no daemon-side metadata exists. | — |
| 50 | **Light / dark theme palette switching** (PRD #27 — auto-detect terminal background via OSC 11 query, swap a small "neutral text colors" palette (White↔Black titles, Gray↔DarkGray labels); accent ANSI colors (Cyan/Green/Yellow/Red/Blue/Magenta) unchanged; fallback to dark on detection failure; `--theme auto\|light\|dark` and `theme:` config-file option override the detection) | Preserved | `prds/done/27-light-theme-option.md`; baseline `src/theme.rs` with `ColorPalette::dark`/`::light`, `resolve_palette(Theme)`, and `detect_palette()` using `terminal_colorsaurus`; baseline `src/main.rs:25–27` (CLI flag) and `src/config.rs::theme` field | `src/theme.rs:44–94` (`ColorPalette` struct, `dark`/`light` constructors, `resolve_palette`), `:99–109` (`detect_palette` calling `terminal_colorsaurus::color_palette`, falling back to dark on failure); `src/main.rs:609` (`resolve_palette(effective_theme)`); tests `src/theme.rs::dark_palette_uses_expected_colors` (line 152), `::light_palette_uses_expected_colors` (line 163), `::resolve_palette_dispatches_to_palettes` (lines 174–187) | Same palette structure, same constructor names, same `terminal_colorsaurus`-based detection, same dark-on-failure fallback. The accent-ANSI-colors-unchanged contract is preserved by simply not having those colors in `ColorPalette` (they're emitted directly from rendering code). | — |
| 51 | **Dashboard UI improvements: Dir-ellipsis truncation and plain-digit deck selection** (PRD #12 — `Dir:` field truncates with `…` when it would overlap `Last`/`Tools` on the same row; plain `1`–`9` (no Alt modifier) selects and focuses deck N in Normal mode, alongside the historical Alt+N keybinding) | Preserved | `prds/done/12-dashboard-ui-improvements.md`; baseline `src/ui.rs` `render_session_card` with `truncate_with_ellipsis` helper, baseline `handle_normal_key` plain-digit arm using shared `focus_deck` helper | `src/ui.rs:1780` (`truncate_with_ellipsis` helper), `:6014` (call site for `Dir:`), `:1795` (`focus_deck` shared helper), `:3620–3637` (plain-digit arm: `if ui.mode == UiMode::Normal && let KeyCode::Char(c @ '1'..='9') = key.code && key.modifiers == KeyModifiers::NONE`, dispatching to `focus_deck`); Alt+N path also calls `focus_deck` so the shared-helper invariant from the PRD holds | Same two behaviors, same shared `focus_deck` helper, same ellipsis truncation. Plain digits are gated on `Normal` mode only (Filter/Rename input still consume them), matching the PRD's non-goal note. Preserved-but-untested at the unit-test level for the keybinding itself; dashboard rendering exercises the truncation path. | — |

### Counts

- Preserved: 48 (rows 1–13, 16–35, 37–51)
- Regressed: 1 (row 14 / **F1**)
- Intentional change: 2 (rows 15, 36)
- Unclear: 0

Total: 51 rows. Row 24 is plain Preserved — the documented-but-unimplemented `y` / `n` keybinding survives unchanged from baseline; see **F2** for the underlying defect.

## Follow-up milestones to file

The user reviews and authorizes filing separately. Drafts (2–3 sentences each).

### F1 — Add `dot-agent-deck daemon stop` (and `remote stop`) force-shutdown command

**Problem**: At baseline, quitting the deck killed every agent. After the PRD #76 + PRD #93 pivots, agents are owned by a setsid'd daemon process that intentionally outlives the TUI, idle-shutdown is gated on `agents == 0`, and the quit dialog was deliberately collapsed to Detach/Cancel — leaving no in-product gesture that stops a running deck installation. PRD #93 line 39 anticipated needing a force-shutdown command but neither `DaemonCmd::Stop` nor `RemoteCmd::Stop` shipped.

**Suggested approach**: Add `DaemonCmd::Stop` that sends a shutdown sentinel to the daemon over the attach protocol (or SIGTERM via the per-user lock file). Add `RemoteCmd::Stop` that ssh-execs `dot-agent-deck daemon stop` on the remote. Decide whether `daemon stop` should kill the managed agents or refuse and require `--force`; default to refuse-and-prompt to preserve the persist-when-agents-alive philosophy and keep the destructive case opt-in.

**Likely PRD home**: PRD #93 Phase 4 (still in flight) or a small successor PRD.

### F2 — Implement the `y` / `n` permission key documented in the help overlay (PRD #18 follow-up)

**Problem**: The TUI help overlay has listed `y` / `n` as "Approve / deny permission" since baseline (`src/ui.rs:5536` in current; `:4531` in baseline). No code path implements it — `KeyCode::Char('y')` has no handler at command mode, and the only `KeyCode::Char('n')` handler is for Ctrl+n new-pane. A user who reads the help and tries to approve a `WaitingForInput` session with `y` gets nothing. The defect predates both PRD #76 and PRD #93 and ties back to PRD #18 (permission prompt control) — the help text was added but the keybinding never wired up.

**Suggested approach**: In `handle_normal_key` (around `src/ui.rs:1875–1895`), add `KeyCode::Char('y')` and `KeyCode::Char('n')` arms gated on `selected card status == WaitingForInput`. Dispatch through the same approve/deny path the permission-prompt dialog should already use (or define one if it does not — confirm against PRD #18's scope). Alternative: remove the help line if the design has moved on from in-TUI permission approval.

**Likely PRD home**: a small successor PRD pointing at PRD #18, or a one-off bug fix.

## Intentional changes appendix

Behaviors that changed between baseline and current, where the change is a deliberate design decision with citation. Recording so a future re-audit does not re-flag.

### § A. Quit confirmation dialog: Yes/No → Detach/Cancel

**Where**: `src/ui.rs:1666–1700, 5328–5339`.

**Justification**: PRD #93 M4.2 explicitly collapses the dialog. Every pane is now daemon-backed, so the TUI cannot kill agents from inside the dialog regardless of which option the user picks; offering a "Yes (quit)" that no longer kills anything would be a misleading UX. The fact that the architecturally-correct dialog now reads "Detach / Cancel" is the visible symptom of F1 — the user can no longer reach the kill path that "Yes" used to perform.

### § B. `DOT_AGENT_DECK_SOCKET` `/tmp` fallback gains uid suffix

**Where**: `src/config.rs:52–68` (`socket_path`) and mirror at `:76–91` (`attach_socket_path`).

**Justification**: Introduced in commit `48b9180` (the PRD #93 squash-merge that brought in always-external-daemon work). The inline code comment at `src/config.rs:61–67` records the rationale: *"the `/tmp` fallback must include the uid so two users on the same host can't collide on the same socket path (the daemon is per-user; the 0o600 mode is on the socket inode, but the *path* still has to be unique, otherwise the loser's `bind(2)` sees `EADDRINUSE` against the winner's inode). Same rationale as `attach_socket_path` below."* The PRD document itself does not contain a numbered REV-2 reference; the citation lives in the squash-merge commit message and the code comment. On a shared-host where `$XDG_RUNTIME_DIR` is unset and multiple users fall back to `/tmp`, the original `/tmp/dot-agent-deck.sock` would be a collision target. The new fallback is `/tmp/dot-agent-deck-{uid}.sock`. The env-var override behavior is unchanged; only the `/tmp` default differs. The doc default at `docs/configuration.md:22` is mildly out-of-date but does not change the user's ability to override.

### § C. In-process daemon → external daemon

**Where**: `src/main.rs:574–638`, `src/daemon_attach.rs:99–294`, `src/daemon.rs:317–551`, `src/agent_pty.rs:805+`.

**Justification**: PRD #76 + PRD #93 Phases 1–3. The daemon is now a separate process; agents are owned by `AgentPtyRegistry` in the daemon; the TUI talks to the daemon over a per-user Unix socket via the attach protocol; auto-spawn is lazy at TUI startup and protected by flock. This is the architectural change that produced F1; everything else in this audit is a consequence of it.

### § D. Post-baseline additions (out of scope for parity, listed so future re-audits do not enumerate them as baseline features)

- `daemon serve` and `daemon hello` subcommands (`src/main.rs:135–150`).
- `remote add|list|remove|upgrade` and `connect` subcommands (`src/main.rs:160–209, 409–489`).
- `DOT_AGENT_DECK_ATTACH_SOCKET` env var (`src/config.rs:76–82`).
- `attach_socket_path()` and the streaming-attach socket (`src/config.rs:76+`, `src/daemon_protocol.rs`, `src/daemon_client.rs`).
- Auto-spawn / idle-shutdown / startup-race protection / stale-socket recovery (`src/daemon_attach.rs`, `src/daemon.rs`).
- Attach protocol Hello / `PROTOCOL_VERSION` handshake (`src/daemon_protocol.rs:97–121`).
- Hook event `KIND_EVENT` fanout and TUI `spawn_event_subscriber` (`src/main.rs:640–700`).
- Daemon-owned orchestration dispatch (`src/state.rs:251–479`, `src/agent_pty.rs::write_to_pane`).
- `RunningAgent` / `AgentRecord` metadata for reconnect (`src/agent_pty.rs:699–800`) — M2.11/12/13.
- `pane_input.rs` (submit-key parity lifted out of `embedded_pane.rs`).

---

*End of audit.*
