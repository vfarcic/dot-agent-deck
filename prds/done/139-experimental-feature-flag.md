# PRD #139: Experimental feature flag (single-toggle gating for in-flight features)

**Status**: Complete
**Completed**: 2026-06-12
**Priority**: Medium
**Created**: 2026-06-08
**GitHub Issue**: [#139](https://github.com/vfarcic/dot-agent-deck/issues/139)
**Related**: project-wide convention (touches `CLAUDE.md`, config, every future user-facing PRD)

## Problem Statement

New features developed against `main` ship to anyone running `dot-agent-deck` — including the maintainer's day-to-day usage. There is no way to land work-in-progress code on `main` while keeping the user-facing surface hidden from real use during testing.

Concretely, the current options for an in-flight feature are all bad:

1. **Keep the feature on a long-lived branch.** Defers integration pain, drifts from `main`, blocks parallel work.
2. **Merge with the surface fully exposed.** The maintainer is forced to interact with half-finished UI in normal sessions; mis-clicks reach unstable code; bug reports from one's own usage are indistinguishable from "real" bugs.
3. **Merge and rely on no one finding it.** Implicit and fragile — a stray keystroke or layout change can surface the unfinished feature.

The maintainer wants a way to merge work-in-progress code that **adds new visible surfaces** (panes, fields, commands, tabs) without those surfaces appearing in normal use until they are explicitly opted into for testing.

## Solution Overview

A single boolean feature flag — `experimental` — that gates **only user-visible surfaces** introduced by in-flight work. Off by default. Opt-in via config or environment variable. Toggling is live (no restart). Both the TUI and the daemon honour the flag.

Key design choices and the rationale for each:

1. **One flag, not many.** All in-flight experimental surfaces ride behind a single `experimental` toggle. Either all experimental surfaces are visible, or none are. This is a deliberate simplification: the maintainer is the only test population, so granular per-feature toggles add maintenance overhead without buying anything. If two unrelated experimental features are active at the same time, they are toggled together — accepted trade-off.

2. **Minimal in-repo implementation, no SDK.** A small `Features` module read from the existing `.dot-agent-deck.toml` (`[features]` table) plus a `DOT_AGENT_DECK_EXPERIMENTAL` env-var override. A file watcher in both TUI and daemon re-evaluates on change. No new dependency. OpenFeature was considered and rejected — the Rust SDK is community-maintained and thin, a file provider would have to be custom-written anyway, and the standard's portability value props (provider swapping, contextual evaluation, observability hooks) are unused for a single-machine local CLI with one boolean flag. Adopting the standard here would be ceremonial cost without payoff.

3. **Gate only at the user-visible seam.** The flag is a presentation switch, not a behaviour switch. Underlying code paths run regardless. The flag controls whether the user can *see* the new surface. This keeps the flag count of touches per feature low — typically one or two `if features::show_<name>() { ... }` checks at the render or input-binding layer.

4. **Per-feature wrapper functions for graduation traceability.** Even with one shared flag, each gated surface declares a small wrapper:

   ```rust
   // src/features.rs
   pub fn show_redesigned_dashboard() -> bool { experimental_enabled() }
   pub fn show_new_status_field()      -> bool { experimental_enabled() }
   ```

   All call sites read `if features::show_redesigned_dashboard() { ... }`. When a feature graduates, `grep show_redesigned_dashboard` finds every site, the wrapper is deleted, and the `true` branches are inlined. Cost: ~2 lines of overhead per gated feature. Benefit: fully greppable per-feature removal, no fragile comment markers, mechanical refactor.

5. **Process discipline lives in `CLAUDE.md`.** Project-specific instructions added to this repo's `CLAUDE.md` require that, when starting work on a PRD, the user is asked whether the feature should be behind the `experimental` flag. If yes, the gating rule (wrapper function, user-visible-only) applies, the changelog fragment notes the flag, the docs note the flag, and a `graduate-<feature>` follow-up issue is filed when the PRD ships. Upstream skills in `vfarcic/dot-ai` and `vfarcic/prompts` are **not** modified — other dot-ai consumers may use different flag tools or none at all, so the policy stays scoped to this project.

6. **Per-feature graduation issue, not a recurring sweep.** When a PRD ships a flag-gated feature, the same PRD files a follow-up `graduate <feature>` issue. That issue is the durable signal that the feature still needs to graduate; closing it requires removing the wrapper, the flag note in docs, and the changelog note. No periodic review issue is needed — each feature carries its own graduation reminder.

## Scope

### In Scope

- **`Features` struct** (`src/features.rs`, new): single boolean field `experimental`, plus the `experimental_enabled()` accessor and per-feature wrapper functions added as features land.
- **Config wiring**: `[features]` table in `.dot-agent-deck.toml` with `experimental = false` (default). Loaded at startup; re-read on file change.
- **Env override**: `DOT_AGENT_DECK_EXPERIMENTAL=1` (or `true`, case-insensitive) overrides the TOML value. Env takes precedence over file. Documented.
- **Live update via file watcher**: both TUI and daemon watch `.dot-agent-deck.toml` for changes; on change, re-read the `[features]` table and update the in-memory `Features` value. Existing surfaces re-evaluate `features::show_<name>()` on next render / input cycle.
- **Daemon ↔ TUI consistency**: both processes evaluate the flag independently from the same source of truth (the TOML file). No cross-process synchronization protocol; the file is the contract.
- **`CLAUDE.md` permanent instruction** (#8 or next available number): when starting a PRD that introduces a user-visible surface, ask whether to gate it behind the `experimental` flag. If yes, follow the wrapper convention, note the flag in the PRD itself, ensure the changelog fragment and docs note it, and file a `graduate-<feature>` follow-up issue when the PRD ships.
- **Tests** for the flag plumbing:
  - Unit: TOML parsing, env override precedence, file-watch reload.
  - L1 widget snapshot per gated surface: one snapshot with flag forced ON (surface visible), one with flag forced OFF (surface hidden / unchanged from pre-feature baseline).
- **Documentation** under `site/`: short page explaining the `experimental` flag, how to enable it (TOML + env), what it does, and that user-visible features marked "experimental" in their PRDs are gated by it.

### Out of Scope (this PRD)

- **No additional flags.** This PRD ships exactly one boolean. If a second flag is ever required, that is a separate PRD; the current scope is single-flag-forever.
- **No OpenFeature or any external SDK.** Decided after evaluating the trade-off: the Rust SDK ecosystem is thin, the portability benefit is unused for a single-machine local tool, and a file provider would be custom-built either way.
- **No remote / network-served flag source.** File is the source of truth.
- **No per-user / per-context targeting.** Flag is global to the running deck instance.
- **No flag observability / audit logging.** The flag is a presentation switch; its state is greppable in config and printed at startup. No counters, no eval logs.
- **No TUI screen to toggle the flag interactively.** Toggling is done by editing the config file or setting the env var. A TUI screen is a follow-up if it ever becomes painful.
- **No changes to upstream skills in `vfarcic/dot-ai` or `vfarcic/prompts`.** The flag-aware PRD process lives in this project's `CLAUDE.md`. Other dot-ai consumers handle flags however they like.
- **No automated graduation reminders or scheduled reviews.** Each flag-gated PRD files its own follow-up issue at ship time; that issue is the reminder.
- **No CI matrix run under flag-ON.** CI runs at the default (OFF). ON-state coverage comes from the targeted L1 snapshots above, not from a duplicate suite pass.
- **No behavioural gating.** The flag does not branch business logic, daemon protocols, or hook handling. It strictly controls whether a surface is rendered / a command is bound.

## Success Criteria

- A user with `experimental = false` (the default) running the deck sees zero in-flight experimental surfaces — same as if the experimental feature did not exist.
- A user with `experimental = true` in `.dot-agent-deck.toml` (or `DOT_AGENT_DECK_EXPERIMENTAL=1` set) sees every currently-flagged experimental surface.
- Toggling the flag (TOML edit or env change followed by deck restart for env; file edit during a running session for TOML) is reflected in the UI without restart for the file path, within a render cycle or two of the file save.
- A `grep features::show_<feature_name>` from the repo root finds every call site for that feature, enabling mechanical removal at graduation time.
- `CLAUDE.md` contains the permanent instruction that PRDs introducing user-visible surfaces must answer the flag question explicitly.
- At least one new test in `tests/` exercises both flag-ON and flag-OFF paths for the wrapper / file-watcher plumbing.
- Documentation under `site/` explains the flag in 1–2 short sections.
- `cargo fmt --check` and `cargo clippy -- -D warnings` pass. `cargo test-fast` passes; `cargo test-e2e` passes pre-PR.
- No new third-party crate dependencies are added.

## Open Questions (resolve during M1)

1. **File-watch crate vs. periodic re-read.** Working assumption: use whichever file-watch mechanism the deck already uses for config reload, if any; otherwise add `notify` crate. M1.1 confirms. If `notify` and its transitive deps balloon, fall back to a periodic re-read (e.g. every 2 seconds) — the flag changes rarely, polling is cheap.
2. **Where does the `Features` struct live in process state?** Working assumption: a single shared `Arc<RwLock<Features>>` held by TUI and (separately) daemon, refreshed by the watcher task. M1.2 confirms the exact integration point with existing config plumbing.
3. **Env-override semantics on hot reload.** When the env var is set, file edits should be ignored for that field (env wins). M1.1 documents this and includes a unit test.
4. **Startup log line.** Should the deck print `experimental flag: ON` / `OFF` at startup? Working assumption: yes, a single info-level line — cheap, prevents "why is the new pane showing?" confusion. M1.3 confirms.
5. **Initial gated surface for end-to-end validation.** This PRD ships the plumbing only — no real feature is gated by it yet. The first real flag-gated feature will be the validation case in its own PRD. Working assumption: the first gated surface arriving after this PRD ships becomes the de-facto integration test. If no such PRD is in flight when this lands, add a trivial throwaway gated surface (e.g. a footer label "experimental: on" rendered only when the flag is on) and remove it once a real gated feature lands.

## Milestones

### Phase 1: Core flag plumbing

- [x] **M1.1** — `src/features.rs` (new): `Features { experimental: bool }`, `experimental_enabled()` accessor, env-override precedence, unit tests for TOML parse + env override.
- [x] **M1.2** — Wire `Features` into existing config loading. Both TUI and daemon read `[features]` from `.dot-agent-deck.toml` at startup. Single shared `Arc<RwLock<Features>>` per process.
- [x] **M1.3** — Startup log line indicating flag state. Documented in code, covered by a startup-log unit test if one exists for related output.

### Phase 2: Live reload

- [x] **M2.1** — File watcher for `.dot-agent-deck.toml`. On change, re-parse the `[features]` table and update the shared `Features` value. Env override still wins.
- [x] **M2.2** — Test that surfaces re-evaluate the wrapper on the next render after a file change. Use the in-process `TestBackend` + a synthetic file event.

### Phase 3: Project policy

- [x] **M3.1** — Add a permanent instruction to this repo's `CLAUDE.md` describing the flag-gating policy: ask the question at PRD start, follow the wrapper convention, note the flag in PRD / changelog / docs, file a `graduate-<feature>` follow-up issue when shipping.
- [x] **M3.2** — Document the wrapper convention in `CLAUDE.md` or a referenced docs page: gate at the user-visible seam, one wrapper function per feature, no flag-checks scattered through implementation code.

### Phase 4: Tests + initial gated surface

- [x] **M4.1** — L1 snapshot tests for an initial gated surface (either the first real flag-gated feature in another PRD, or a trivial throwaway footer label). One snapshot with flag forced ON, one with flag forced OFF.
- [x] **M4.2** — Test helper: `Features::test_with(experimental: true/false)` constructor for per-test forcing. PTY/E2E tests can inject via env (`DOT_AGENT_DECK_EXPERIMENTAL=1`).

### Phase 5: Docs, ship

- [x] **M5.1** — Documentation under `site/`: a short section on the `experimental` flag (what it does, how to enable, why features are gated).
- [x] **M5.2** — Changelog fragment via `dot-ai-changelog-fragment`. Frame as "new project-wide convention: in-flight features can be gated behind an `experimental` flag; off by default."
- [x] **M5.3** — `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test-fast`, `cargo test-e2e` all green. PR, review, audit, merge.

## Key Files

- `src/features.rs` (new) — `Features` struct, `experimental_enabled()`, per-feature wrapper functions (initially empty; populated as flag-gated PRDs land).
- `src/config.rs` (extend) — parse `[features]` table from `.dot-agent-deck.toml`; merge env override.
- Wherever config is currently loaded for TUI and daemon (located in M1.2) — initialize the shared `Features` value, start the file-watcher task.
- `tests/features.rs` (new) — unit tests for parsing, env override, file-watch reload behaviour.
- `CLAUDE.md` — new permanent instruction codifying the flag policy.
- `site/content/docs/experimental-flag.md` (new) — user-facing documentation page.

## Risks and Mitigations

- **Risk**: Wrapper convention is not followed; future PRDs gate surfaces by inlining `experimental_enabled()` checks all over the codebase.
  - *Mitigation*: `CLAUDE.md` permanent instruction makes the wrapper convention explicit. Reviewers / auditors check for direct `experimental_enabled()` calls outside `src/features.rs` and request a wrapper. Optional follow-up: a lint or grep-based CI check that fails the build on direct calls outside the wrapper module.

- **Risk**: Single flag means two unrelated experimental features can't be tested independently. If experimental feature A is broken in a way that blocks testing experimental feature B, the maintainer must disable both.
  - *Mitigation*: Accepted trade-off (explicit in the design). If this becomes a real pain, a follow-up PRD can either (a) introduce per-feature flags, or (b) introduce a small set of named experimental groups. Single-flag-forever is the current scope.

- **Risk**: File-watch reload misfires on partial writes (editor writes the file in chunks; watcher fires before the file is complete; TOML parse fails).
  - *Mitigation*: Standard fix: debounce the watcher (e.g. 200ms) and tolerate parse failures by keeping the previous value. M2.1 includes a unit test for "invalid TOML during reload → keep current value, log a warning."

- **Risk**: Flag is honoured at render but not at input-binding — user can still trigger a keybinding for a hidden experimental feature.
  - *Mitigation*: The wrapper convention covers input-binding as well as rendering. Documentation and reviewer checklist call this out: if a feature is gated, both the visible surface AND the input affordances must be gated by the same wrapper.

- **Risk**: Forgotten graduation — a feature graduates in practice (no longer experimental) but its flag wrapper lingers indefinitely.
  - *Mitigation*: Each flag-gated PRD files its own `graduate-<feature>` follow-up issue at ship time. Closing that issue requires removing the wrapper, the flag note in docs, and the changelog note. The issue is the durable reminder.

- **Risk**: Daemon and TUI drift if one re-reads the file and the other does not (e.g. on a partial write, one parses successfully, the other does not).
  - *Mitigation*: Both processes use the same parsing and the same "keep previous on parse error" rule. Documented behaviour; covered by tests.

- **Risk**: Initial gated surface for validation doesn't exist when this PRD ships; the plumbing is unverified end-to-end.
  - *Mitigation*: Either pair this PRD with the first real flag-gated feature PRD (preferred), or ship a trivial throwaway gated surface (footer label) and remove it once a real one arrives. M4.1 covers this explicitly.

## Dependencies

- Existing config loading (TOML parsing already in use for `.dot-agent-deck.toml`).
- A file-watcher mechanism — either an existing one in the deck, or the `notify` crate (or polling fallback). M1.1 / M2.1 confirm.
- Existing test harnesses (L1 `TestBackend` + `insta`, L2 PTY) — used as-is.

## Validation Strategy

- **Unit**: `Features` parses correctly from TOML; env override wins; partial / invalid TOML during reload keeps previous value; file-watcher fires on save and updates the shared value.
- **L1 widget**: at least one gated surface tested with flag forced ON (surface visible) and forced OFF (surface hidden / baseline unchanged). Per the PRD's testing rule: feature-itself tests force the flag ON; hiding tests force it OFF; everything else runs at the default (OFF).
- **L2 / E2E**: existing tests run at default (OFF) and must not change behaviour. One E2E test pre-PR confirms that setting `DOT_AGENT_DECK_EXPERIMENTAL=1` in the spawned binary's env produces the expected visible surface.
- **Manual** (per `feedback_validate_pre_pr`):
  - Run the deck with the default config; confirm no experimental surfaces visible.
  - Set `experimental = true` in `.dot-agent-deck.toml` while the deck is running; confirm the gated surface appears within a few render cycles.
  - Set `DOT_AGENT_DECK_EXPERIMENTAL=1` and start the deck; confirm the gated surface is visible regardless of TOML setting.
  - Set the env var but `experimental = false` in TOML; confirm env wins (gated surface visible).
- **Regression**: every existing pane, status indicator, command, and hook unchanged at default (OFF). The flag is additive; no existing test should require modification.

## CLAUDE.md Compliance

- `cargo fmt --check` and `cargo clippy -- -D warnings` before every commit (project rule #2).
- No `m*_*` or `prd*_*` prefixes in source/test filenames (project rule #3). Use semantic names: `src/features.rs`, `tests/features.rs`.
- Ask before creating branches or worktrees (project rule #1). `/prd-start` will prompt the user accordingly.
- L1 widget tests for the gated surface(s); L2 covered by the existing E2E suite plus the one targeted env-injection test pre-PR (project rules #4, #5).
- This PRD itself **adds** a new permanent instruction to `CLAUDE.md` codifying the flag-gating policy.
