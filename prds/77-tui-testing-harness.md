# PRD #77: TUI Testing Harness

**Status**: Planning
**Priority**: High
**Created**: 2026-05-09
**GitHub Issue**: [#77](https://github.com/vfarcic/dot-agent-deck/issues/77)

## Stop Conditions for Autonomous Execution

When this PRD is executed by an autonomous agent (e.g. via `/dot-ai-prd-full` or any similar runner), the agent **must stop and surface the situation to the user — never push through silently** — in any of these cases:

1. **Pre-defined validation checkpoints.** End of M1, M2, and M3 (per Decision 29). The agent presents the milestone's deliverables and waits for explicit go-ahead before proceeding.
2. **Discovered Issues collected during test runs.** Per Decisions 11 and 25, the agent stops after the Discovered Issues list is populated, sorts by severity, and waits for user direction before fixing anything (except the three Decision 11 exceptions, which may be resolved in-flight with a PR note).
3. **Decision reconsideration.** Any decision in this PRD is open to reconsideration if execution reveals it was wrong, infeasible, in conflict with new information, or produces poor results. The mechanism is stop + surface: describe what was hit, what alternative seems better, why the original doesn't fit. The user amends the decision or confirms the original.
4. **Anything unclear or "feels wrong."** If the agent finds itself thinking *"the decision says X but in this case Y feels right"*, or *"this works but the result is poor"*, or *"I'm not sure which rule applies"*, that is a stop point. Default to stop, not to push through.

**Silent deviation is not allowed.** Better to over-stop than to apply a rule outside its intended scope. The user is responsible for unblocking; the agent's job is to surface, not to decide.

## Problem Statement

The dot-agent-deck TUI is validated by hand. Pane creation, status transitions, focus, layout, prompt regions, and the hook → daemon → UI flow have no automated coverage at the rendered-screen level. Regressions ship easily; refactors are expensive; PR review is human-bottlenecked. Existing tests in `tests/` cover protocol/state but do not exercise the spawned binary or rendered screens.

## Solution Overview

Build a **cross-platform end-to-end TUI test harness** that:

1. Spawns the dot-agent-deck binary in an isolated PTY, with per-test sockets and a redirected `HOME`.
2. Runs real Claude Code and OpenCode CLIs inside the deck for chain-smoke tests.
3. Captures the deck's rendered output through a vt100 parser (structured grid, not a string blob).
4. Asserts only on the deck's observable state (panes, statuses, focus, prompts, hook event delivery, attach stream presence) — never on agent text content.
5. Runs identically on macOS, Linux, and Windows. Cross-platform from day one in design; rolled out macOS → Linux → Windows.

## Design Decisions

### Decision 1: portable-pty + vt100 as the foundation

- Outer PTY: **`portable-pty 0.8`** (cross-platform including Windows ConPTY).
- ANSI parsing: **`vt100 0.16`** (structured grid model — cells, attributes, cursor).
- Both already production deps; no new toolchain cost.
- Production precedent: zellij's `src/tests/e2e`.
- Trade: harness is written in-house. That work is what M2 produces.

### Decision 2: Two-layer harness

- **L1 — in-process:** ratatui `TestBackend` + `insta` for pure widget/layout regressions. No subprocess.
- **L2 — end-to-end:** spawned binary in PTY + vt100 + assertions for full system behavior.

Test cases land at the layer strictly necessary.

### Decision 3: Real agents in chain-smoke tests; observable-state assertions only

Real Claude Code and OpenCode CLIs run inside the harnessed deck. No mock-agent fixture mode. All assertions read the deck's rendered grid or protocol surface, never the agent subprocess's stdout.

### Decision 4: Cross-platform from day one in design

The harness's design must accommodate Windows ConPTY quirks (alternate-screen repaints, line-ending oddities, `isatty` differences, env propagation) from the first commit. Rollout is macOS → Linux → Windows. Anything macOS-specific or Linux-specific in the harness is a bug.

### Decision 5: M1 scope; M2/M3 shape in Decision 29; M4+ TBD

The test catalog is M1's deliverable. *Specific* catalog entries covered by M2 and M3 are picked from M1's output at M2/M3 kickoff. M2 and M3 *shape* (small slice + user-validation stop) is locked in Decision 29. M4+ shape and scope are written after M3 lands.

### Decision 6: L2 tests in `tests/e2e_*.rs`, gated by `#[cfg(feature = "e2e")]`

**File layout:** L2 files live in the existing top-level `tests/` directory with the `e2e_` filename prefix. L1 files live in `tests/` without that prefix.

**Execution gating:** each `e2e_*.rs` file opens with `#![cfg(feature = "e2e")]`. Fast tier has no feature gate.

**Cargo aliases** (`.cargo/config.toml`):

```toml
[alias]
test-fast = "nextest run"
test-e2e  = "nextest run --features e2e"
```

Both route through `cargo nextest run` (Decision 13).

**Orchestration integration** (in the milestone that ships the first usable harness):

- Coder role: change from `cargo test` to `cargo test-fast`.
- Orchestrator workflow: add a pre-release step requiring `cargo test-e2e` to pass.

### Decision 7: The catalog is the spec; tests link to it by stable IDs

**Catalog entries** live in `## Test Case Catalog` (in this PRD; extract to `docs/tui-spec.md` once past a couple hundred entries):

```
pane/lifecycle/001 — A pane appears in the next free layout region when an agent is started.
```

**ID format:** `<area>/<sub-area>/<NNN>`

- Area and sub-area in kebab-case.
- Numeric tail zero-padded to 3 digits (`001`, not `1`).
- Sparse numbering allowed (insert as `006` between `004` and `005`).
- Globally unique across the catalog.
- No deeper hierarchy.

**Tests reference the ID via a `#[spec(...)]` annotation:**

```rust
#[spec("pane/lifecycle/001")]
#[test]
fn lifecycle_001_pane_appears_on_agent_start() {
    let deck = TuiDeck::launch();
    deck.start_agent(claude_code());
    deck.assert_pane(0).is_visible().has_status(Running);
}
```

**File layout mirrors catalog sections:**

```
tests/
  e2e_pane_lifecycle.rs       ← pane/lifecycle/*
  e2e_focus_navigation.rs     ← focus/nav/*
  e2e_hook_delivery.rs        ← hooks/delivery/*
  render_dashboard.rs         ← L1: layout/dashboard/*
```

**CI-enforced linkage** — Rust binary at `xtask/linkage-check/`, invoked as `cargo xtask linkage-check`. Added to CI alongside fmt/clippy/test-fast, configured as a required status check on `main`. Six checks, all must pass:

1. Every catalog ID has at least one test referencing it.
2. Every `#[spec("...")]` references a real catalog ID.
3. Catalog IDs match the format regex `^[a-z][a-z0-9-]*\/[a-z][a-z0-9-]*\/\d{3}$`.
4. Function name carries the annotation's `<sub>_<NNN>` prefix (Decision 17).
5. No raw `std::thread::sleep`, `tokio::time::sleep`, or `for _ in 0..N` polling in `tests/e2e_*.rs` (Decision 21).
6. No `#[ignore]` on `#[spec(...)]`-annotated tests (Decision 26).

**Fluent harness API:** test bodies read close to catalog prose (`deck.start_agent(...)`, `deck.pane(0).wait_until_status(Running)`), not raw PTY plumbing.

**Excluded:** no Gherkin/Cucumber/DSL; no catalog generated from tests; insta snapshots are not the spec.

### Decision 8: Synthetic events by default; real-agent for chain-smoke only; e2e is local-only

**Default for new tests:** synthetic — write hook JSON directly to the deck's hook socket; no LLM in the loop.

**Real-agent (chain-smoke) tests** are reserved for verifying the *whole chain* end-to-end. Typically one or two tests per supported agent CLI.

**Execution model:**

- `cargo test-fast` runs in CI on every PR. No API keys, no agent CLIs needed.
- `cargo test-e2e` runs locally only — on the developer's machine, before opening the PR. Never in GHA. Enforced by the orchestrator's pre-release gate (Decision 6) and Decision 29.
- No nightly e2e drift run.

**Pinned models for chain-smoke** (via env, so a test can't accidentally pick a more expensive one):

- Claude Code: `claude-haiku-4-5-20251001`
- OpenCode: `openrouter/google/gemini-2.5-flash-lite` (routed via OpenRouter using the developer's existing credential)

If a pinned model is unavailable (deprecation, outage), the test skips with explicit reason — no silent fallback to a different model.

### Decision 9: No auto-retry; flake = bug

The harness never uses `--retries=N`, `cargo nextest --retries`, or any "rerun until green" wrapper. A flaky test is a bug.

**Operational consequence:** a flaking test in CI blocks the merge; the test is either fixed (deck or test per Decision 11) or the catalog entry is descoped and the test deleted (Decision 26). No quarantine, no `#[ignore]` workaround.

### Decision 10: Existing tests move to `./tmp/legacy-tests/` at M1 start; only pure-data unit tests stay live

At the start of M1, all existing tests in `tests/*.rs` and most `src/*/mod tests` move (via `git mv`) into `./tmp/legacy-tests/`, preserving directory structure. `tmp/` is already gitignored. After M1, the directory can be deleted locally; git history preserves the originals.

**Pure-data carve-out** — these stay live in `src/*/mod tests`. A test is pure-data if the function it verifies:

- Takes data in, returns data out.
- Touches no I/O, no global state, no UI dependencies.
- Has no L1/L2 counterpart possible.

**Examples that stay:** TOML/JSON parsers, ID generators, validation predicates, format encoders.
**Examples that move:** anything touching `AppState`, `Daemon`, `Pane*`, `Mode*`, `Tab*`, hooks, sockets, PTY. The full `tests/*.rs` inventory.

**M1 audit deliverable:** a short list of pure-data tests that stay (one row per test function, one-line justification), plus the `git mv` commit. That is the entire audit.

### Decision 11: Failing tests fix the deck, not the test

**Default:** when a test fails, fix the deck. The test is the spec.

**Three exceptions** (each requires a written justification in the PR description):

1. **Catalog entry was wrong.** Correct the catalog, update the test.
2. **Test overspecified.** Loosen the test to match the catalog.
3. **Intentional behavior change driven by another PRD.** Update the catalog as part of that PRD's scope; the ID stays stable, the prose updates.

Anything outside these three → the deck has a bug → fix the deck.

**Discovered Issues** are populated by M2+ when a test fails for a non-exception reason. Entry format in Decision 25.

**Autonomous-execution stop point.** Under agent autonomy, the agent stops after the Discovered Issues list is populated and surfaces it to the user before fixing anything (except the three exceptions above).

### Decision 12: Test fixtures live in `tests/fixtures/<scenario>/`, copied into per-test tempdirs

- Fixtures are committed files under `tests/fixtures/<scenario>/`, **copied into the per-test tempdir at launch** by the harness. Never referenced in place.
- The harness `git init`s the tempdir after the copy.
- Synthetic-event fixtures use stub role commands (e.g. `sh -c 'sleep infinity'`); chain-smoke fixtures point at real agent CLIs and pin the cheap models from Decision 8.
- Only the `minimal/` fixture ships with the harness skeleton. Others are added in the milestone that first needs them.
- The repo-root `.dot-agent-deck.toml` is **not** reusable as a test fixture — its role commands invoke real agents in the developer's environment.

### Decision 13: cargo-nextest as the test runner for both tiers

Both `test-fast` and `test-e2e` route through `cargo nextest run`. Process-per-test isolation, exact-match filtering via `-E 'test(name)'`, profile config at `.config/nextest.toml`.

**Profile config:**

```toml
[profile.default]
retries = 0
slow-timeout = { period = "60s", terminate-after = 3 }
fail-fast = false

[profile.default.junit]
path = "target/nextest/default/junit.xml"

[profile.e2e]
slow-timeout = { period = "120s", terminate-after = 2 }
```

**Doctests caveat:** nextest doesn't run doctests. If the project ships doctests that must be validated, CI adds a separate `cargo test --doc` step. Confirm at implementation time.

**Installation:** `cargo install cargo-nextest` (or via devbox). CI installs it as part of workflow setup.

### Decision 14: bacon as the recommended TDD watch-loop tool

A `bacon.toml` is committed at repo root. Bacon is recommended for local TDD, not required.

**Committed `bacon.toml`:**

```toml
default_job = "test-fast"

[jobs.test-fast]
command = ["cargo", "nextest", "run", "--no-fail-fast"]
need_stdout = true

[jobs.test-e2e]
command = ["cargo", "nextest", "run", "--features", "e2e", "--no-fail-fast"]
need_stdout = true

[jobs.clippy]
command = ["cargo", "clippy", "--", "-D", "warnings"]
need_stdout = false

[jobs.fmt]
command = ["cargo", "fmt", "--check"]
need_stdout = false
```

Not mandated: CI does not install bacon. CLAUDE.md does not require it.

### Decision 15: rstest is not adopted as a project-wide convention

The harness itself is the fixture mechanism (Decision 12). Default for new tests is plain `#[test]` functions calling into the harness. If a specific test someday needs parameterization, rstest can be introduced then as a dev-dep — not pre-blessed.

### Decision 16: ratatui-testlib re-check at M2 kickoff, otherwise in-house

**Default:** in-house harness on `portable-pty + vt100`.

**Single re-check at M2 kickoff.** Developer evaluates ratatui-testlib's then-current state against four criteria — *all four must be true* to adopt:

1. Cross-platform support landed and tested.
2. ≥200 stars and multiple maintainers.
3. Released past 1.0.
4. Covers our use cases (synthetic-event injection, chain-smoke runs, catalog-ID linkage).

If adopted, the strategy decisions (D7, D8, D11, D12, …) still apply — the library becomes a substrate, not a replacement for the strategy.

After M2 kickoff, no further re-checks unless a major release or v1.0 + cross-platform explicitly motivates a reopen.

### Decision 17: Test function names follow `<sub-area>_<NNN>_<short_descriptive_suffix>`

In snake_case. `<sub-area>` and `<NNN>` come from the test's catalog ID. The `<area>` prefix is omitted (the file name encodes it).

**Example:**

- Catalog ID: `pane/lifecycle/001`
- File: `tests/e2e_pane_lifecycle.rs`
- Function: `lifecycle_001_pane_appears_on_agent_start`

The descriptive suffix is required for readability. The `#[spec(...)]` annotation is authoritative for linkage; the function name is a convenience for filtering and review readability.

### Decision 18: Single-test invocation pattern

**One-shot (any developer or AI worker):**

```sh
cargo test-fast lifecycle_001    # fast tier
cargo test-e2e  lifecycle_001    # e2e tier (local only)
```

The positional is nextest's substring filter against the function name. Per D17's naming, the `<sub-area>_<NNN>_` prefix is unique by construction.

**Watch-mode wrapper (developer, optional):**

```sh
bacon test-e2e -- lifecycle_001
```

While in bacon: `f` restricts execution to currently-failing tests, `esc` returns to the configured filter.

**CONTRIBUTING.md** gets a short TDD-loop section (3–4 lines, not a tutorial).

**CLAUDE.md addition (Appendix A):** AI workers in coder roles must rerun only the failing test after fixing the code, then rerun the full tier before committing.

### Decision 19: CONTRIBUTING.md sections ship with the harness milestone

All CONTRIBUTING.md content documenting the harness ships in the same milestone that lands the first usable harness — never deferred.

**Sections that ship:**

1. **Snapshot review workflow** — how to evaluate `insta` diffs in PR review.
2. **TDD loop** — the canonical one-shot and bacon-watch commands from Decision 18, plus a pointer to Decision 17.
3. **How to add a new test** — pick or create a catalog ID, write the entry, write the test with `#[spec(...)]` annotation, run `cargo xtask linkage-check` locally.

**Not in CONTRIBUTING.md:** catalog ID format rules (live in this PRD), decision rationale.

### Decision 20: Pinned terminal environment with explicit-override policy

The harness pins these values for every L2 test launch, overriding host inheritance:

| Variable | Value |
|---|---|
| PTY size | `120 cols × 40 rows` |
| `TERM` | `xterm-256color` |
| `LC_ALL` | `C.UTF-8` |
| `COLORTERM` | `truecolor` |
| `NO_COLOR` | unset (removed if inherited) |
| `CLICOLOR_FORCE` | unset |

**Rule:** "no silent host inheritance." Tests override these via the harness builder API when their behavior under test demands a different value:

```rust
// Default
let deck = TuiDeck::launch_with_fixture("minimal");

// Resize mid-run
deck.resize(80, 24);

// NO_COLOR-specific test
let deck = TuiDeck::builder()
    .with_env("NO_COLOR", "1")
    .launch_with_fixture("minimal");
```

Overrides are always explicit in the test body. At least one catalog test covers a PTY resize mid-run (SIGWINCH path).

### Decision 21: Quiescence-based waits primary; string-signal opt-in; raw sleeps forbidden

**Primary:** `deck.wait_until_quiescent()` blocks until PTY output is silent for **50 ms** (default; tunable as a harness constant).

**Opt-in for faster waits when content is stable:** `deck.wait_for_string("permission prompt")`. Use sparingly.

**Forbidden in test bodies:**

- Raw `std::thread::sleep` / `tokio::time::sleep`.
- Polling loops with fixed retry counts (`for _ in 0..10 { ... }` as a disguised wait).

Linkage-check tool (Decision 7) grep-enforces these.

The 50ms default is a starting point. Tune once real test runtimes can be measured.

### Decision 22: Insta file snapshots only; no inline snapshots

L1 widget/render tests use file-based insta snapshots exclusively. Inline (`assert_snapshot!(buf, @"...")`) is not used.

**Conventions:**

- L1 tests call `assert_snapshot!(buf)` (no `@` heredoc form).
- Snapshots land under `tests/snapshots/`.
- `insta` is added to `Cargo.toml` as a pinned dev-dep (specific version, not `latest`).

### Decision 23: No explicit e2e cost ceiling; control is upstream

No per-run spend cap, no `MAX_E2E_COST_USD`, no CI cost-tracking integration.

Cost is bounded by upstream choices already made: synthetic-event default (free), chain-smoke count deliberately scarce (≤8 LLM-using tests total), pinned cheap models (Decision 8). Estimated upper bound: <$0.05 per full `cargo test-e2e` run.

**Catalog entries for non-trivial chain-smoke tests** include a one-sentence cost note (e.g. *"~500 input + 200 output tokens against Haiku"*).

### Decision 25: Discovered Issues — entry template and bounded scope

The Discovered Issues framework (per Decision 11) applies only during this PRD's active work. Once PRD #77 closes, the table freezes; new test failures are handled as ordinary bugs.

**Entry template:**

| ID | Catalog ref | Description | Severity | Status |

- **ID** — `di-NNN`, sequentially numbered.
- **Catalog ref** — the catalog ID that surfaced the issue.
- **Description** — one line. Longer detail in a filed GitHub issue.
- **Severity** — `blocker`, `major`, or `minor`. `blocker`/`major` require explicit user direction before fixing under autonomy; `minor` allows file-and-continue.
- **Status** — `fixed in <milestone>`, `filed as #NNN`, `won't-fix: <rationale>`, or `escalated to PRD #NNN`.

**Autonomous-execution presentation:** sort by severity (blockers first); header includes severity counts.

### Decision 26: No quarantine — failing tests are fixed or deleted

A test that fails — for any reason — is either fixed (deck or test per Decision 11) or deleted (and its catalog entry descoped). No `#[ignore = "fix later"]` workaround.

**Runtime-skip exception** (stays): a test that cannot *run* because of an environmental condition (agent CLI not installed locally, API key missing, OS-specific feature unavailable) skips at runtime with an explicit reason — conditional on environment, not on test brokenness.

**Linkage-check enforcement:** `#[ignore = "..."]` on a `#[spec(...)]`-annotated test fails CI.

### Decision 27: Catalog construction cross-references docs as a coverage floor

While building the catalog in M1, the developer cross-references user-facing docs (`docs/`, top-level `README.md`, any onboarding material).

**Rules:**

1. **Code is authoritative for what exists.** The catalog never describes a behavior the code doesn't have.
2. **Docs are a coverage floor.** Each documented user-facing behavior either appears in the catalog, is filed as a Discovered Issue (Decision 25), or is noted as a deliberate skip with one-line rationale in the relevant catalog section header.

### Decision 28: Failed L2 tests produce a replayable recording (local only)

When an L2 test fails (panic or assertion failure), the harness dumps `target/test-recordings/<test-name>/`:

- **`final-grid.txt`** — vt100 grid at failure time as plain text.
- **`final-grid.svg`** — the same grid as styled SVG, colors preserved.
- **`full-stream.cast`** — asciinema-format recording of the entire PTY output. Replayable via `asciinema play`; convertible to GIF/MP4 via `agg <cast> <gif>` post-hoc.
- **`fixture.toml`** — copy of the `.dot-agent-deck.toml` the test used.

**Locality:** local-only artifact. L2 tests run only via `cargo test-e2e`, which is local-only per Decision 8. `cargo test-fast` in CI produces no recordings. No CI artifact upload, no GitHub Actions integration around recordings.

**Storage and cleanup:**

- Artifacts live under `target/test-recordings/<test-name>/`. `target/` is gitignored.
- `target/test-recordings/` is wiped at the start of every `cargo test-e2e` invocation via a nextest setup script in `.config/nextest.toml`.

**Cost model:** in-memory ring buffer during each test; persisted only on failure. Passing tests produce zero artifacts.

**Opt-in always-record for development:**

```sh
DOT_AGENT_DECK_RECORD=1 cargo test-e2e lifecycle_001
```

**Dep impact:** the asciinema cast format is trivial (one JSON line per event: `[time_seconds, "o", bytes]`). ~30 lines of inline Rust to encode. No new dep.

### Decision 29: Iterative validation cadence — three explicit user checkpoints

The harness ships in small validated slices. Three stop-for-user-validation checkpoints sit at the end of M1, M2, and M3.

**Milestone shape:**

- **M1** ships the existing-test audit (move-to-tmp + pure-data list per Decision 10) and the catalog (with docs cross-reference per Decision 27). **STOP** — user reviews before M2 begins.
- **M2** ships a minimum harness supporting one L1 test + one L2 synthetic-event test, both picked from M1's catalog. **STOP** — user validates the in-house harness direction.
- **M3** ships one Claude Code chain-smoke + one OpenCode chain-smoke, both picked from M1's catalog. **STOP** — user validates real-agent plumbing.
- **M4+** is the catalog buildout. Shape TBD when M3 lands.

**Autonomous-execution interaction.** Under `/dot-ai-prd-full` or any similar autonomous runner, the agent stops at the end of M1, M2, and M3 — in addition to the Discovered Issues stop point (Decision 11). **Four forced pauses across this PRD's lifetime.** Stops are mandatory, not advisory.

## Sequencing note: PRD #77 vs PRD #84 (rendering rework)

The harness lands first; #84 then refactors against a green safety net and accepts the resulting wave of L1 snapshot regeneration as part of its scope. L2 tests (assertions on observable state) should largely survive #84 untouched.

## Key Design Constraints

- **Pin terminal environment** per Decision 20. No silent host inheritance.
- **No raw `sleep` or polling loops** per Decision 21. Linkage-check tool enforces.
- **ratatui uses the alternate screen.** Capture mid-run, never post-quit.
- **Resize is a real test surface.** At least one catalog test covers SIGWINCH.
- **ConPTY rewrites bytes on Windows.** Assert on the parsed grid, never raw bytes.
- **Nested-PTY signal forwarding.** Ctrl-C must reach the right child. At least one test covers this with a backgrounded inner agent.
- **Color goldens rot across terminal profiles.** Strip colors before diffing or pin via env (Decision 20).
- **macOS GHA runners cap concurrent PTYs lower than Linux.** Parallel test count must be tunable.
- **Snapshot-review workflow** (insta) must be documented in CONTRIBUTING.md before any goldens land (Decision 19).

## Non-Goals (v1)

- Windows-first. macOS and Linux ship before Windows.
- Visual diff (image-level GIF comparison). Grid-level assertions only.
- Recording test runs as user-facing demo GIFs.
- Mocking agent CLIs. Real agents only.
- A test-DSL or YAML-driven test format. Tests are Rust code in `tests/`.
- Cross-shell coverage.

## Milestones

- [ ] **M1 — Test case catalog and assertion strategy + STOP for validation** (per Decision 29). Two deliverables, in order:
  - **(1) Existing-test audit** (per Decision 10). Identify pure-data unit tests that stay live in `src/*/mod tests`; move everything else (all of `tests/*.rs`, all non-pure-data `src/*/mod tests`) into `./tmp/legacy-tests/` via `git mv` (`tmp/` is already gitignored). Audit lands *before* any new catalog entry is written — the moved tests serve as catalog-inspiration material during step (2).
  - **(2) Test case catalog.** Produce a written catalog (in this PRD) of the test cases the harness must cover, organized by feature area (dashboard panes, statuses, prompts, focus/navigation, modes/tabs, embedded pane attach, hook delivery, lifecycle, resize, error paths). For each test case, decide: which layer (L1 vs L2), which agent if any, what is asserted, what is explicitly not asserted, expected platform coverage. Per Decision 7, also commit to the file-layout-mirrors-catalog convention. **Per Decision 27, the catalog construction includes a docs cross-reference pass.**
  - **STOP** — user reviews both deliverables before M2 begins.
- [ ] **M2 — Minimum viable harness + 2 tests + STOP for validation** (per Decision 29). Build the minimum harness slice required to support exactly two specific catalog entries chosen from M1's catalog: one L1 test (in-process `TestBackend` + insta) and one L2 synthetic-event test (PTY + harness builder + recording on failure). Ship as part of this milestone: the linkage-check xtask binary (Decision 7), the failure-recording infrastructure (Decision 28), the CONTRIBUTING.md sections (Decision 19), the CLAUDE.md additions from Appendix A, and the `bacon.toml` at repo root (Decision 14). After this milestone lands, **stop and wait for explicit validation** before continuing to M3.
- [ ] **M3 — First chain-smoke tests + STOP for validation** (per Decision 29). One real Claude Code chain-smoke test (using `claude-haiku-4-5-20251001`), one real OpenCode chain-smoke test (using `openrouter/google/gemini-2.5-flash-lite`). Both picked from M1's catalog. After this milestone lands, **stop and wait for explicit validation** before continuing to M4+.
- [ ] **M4+ — Catalog buildout.** Scope shape TBD when M3 lands. Carries the catalog-vs-deck "fix the deck not the test" policy (Decision 11) and the Discovered Issues collection (Decision 25).

## M1: Existing-Test Audit

Per Decision 10 — pure-data unit tests stay live in `src/*/mod tests`; everything else moved to `./tmp/legacy-tests/` (gitignored; git history preserves originals via the rename detection on the staged delete).

**Carve-out result:** 296 pure-data unit tests kept across 17 `src/*` files; 25 `tests/*.rs` files plus `tests/common/` moved wholesale; 11 `src/*/mod tests` blocks partially or fully moved.

### Pure-data tests (stay live)

| File | Test function | One-line justification |
|---|---|---|
| src/agent_pty.rs | `pid_to_pgid_accepts_positive_normal_pid` | Pure u32→Option<i32> conversion; no I/O. |
| src/agent_pty.rs | `pid_to_pgid_rejects_zero_pid` | Validation predicate on pid. |
| src/agent_pty.rs | `pid_to_pgid_accepts_max_i32_pid` | Boundary predicate. |
| src/agent_pty.rs | `pid_to_pgid_rejects_overflowing_u32_pid` | Boundary predicate. |
| src/agent_pty.rs | `resolve_display_name_prefers_trimmed_form_name` | String-in / string-out resolver. |
| src/agent_pty.rs | `resolve_display_name_whitespace_only_form_falls_through_to_command` | String-in / string-out resolver. |
| src/agent_pty.rs | `resolve_display_name_no_inputs_falls_back_to_shell` | String-in / string-out resolver. |
| src/agent_pty.rs | `resolve_display_name_rejects_control_char_form_name` | String validator. |
| src/agent_pty.rs | `resolve_display_name_rejects_control_char_command_falls_to_shell` | String validator. |
| src/agent_pty.rs | `validate_tab_membership_rejects_orchestration_cwd_with_nul_byte` | Validation predicate on TabMembership. |
| src/agent_pty.rs | `validate_tab_membership_rejects_orchestration_cwd_with_control_char` | Validation predicate. |
| src/agent_pty.rs | `validate_tab_membership_rejects_relative_orchestration_cwd` | Validation predicate. |
| src/agent_pty.rs | `validate_tab_membership_rejects_oversized_orchestration_cwd` | Validation predicate. |
| src/agent_pty.rs | `validate_tab_membership_accepts_well_formed_orchestration_cwd` | Validation predicate. |
| src/agent_pty.rs | `validate_tab_membership_rejects_oversized_role_index` | Validation predicate. |
| src/agent_pty.rs | `validate_tab_membership_accepts_role_index_at_ceiling` | Validation predicate. |
| src/agent_pty.rs | `validate_tab_membership_rejects_role_name_with_ansi_escape` | Validation predicate. |
| src/agent_pty.rs | `validate_tab_membership_rejects_role_name_with_nul_byte` | Validation predicate. |
| src/agent_pty.rs | `validate_tab_membership_accepts_empty_role_name` | Validation predicate. |
| src/ascii_art.rs | `system_prompt_embedded` | Static-data sanity check. |
| src/ascii_art.rs | `parse_single_frame` | String parser. |
| src/ascii_art.rs | `parse_multi_frames` | String parser. |
| src/ascii_art.rs | `parse_trailing_delimiter` | String parser. |
| src/ascii_art.rs | `parse_empty_input` | String parser. |
| src/ascii_art.rs | `validate_valid_frame` | Validation predicate. |
| src/ascii_art.rs | `validate_too_many_lines` | Validation predicate. |
| src/ascii_art.rs | `validate_line_too_long` | Validation predicate. |
| src/ascii_art.rs | `validate_non_ascii_rejected` | Validation predicate. |
| src/ascii_art.rs | `validate_empty_frame` | Validation predicate. |
| src/ascii_art.rs | `validate_all_frames_empty_vec` | Validation predicate. |
| src/ascii_art.rs | `validate_all_frames_mixed` | Validation predicate. |
| src/ascii_art.rs | `build_user_message_format` | Format encoder. |
| src/build_version_handshake.rs | `non_tty_error_message_names_both_build_ids` | Format encoder. |
| src/build_version_handshake.rs | `non_tty_error_message_renders_unknown_daemon_build` | Format encoder. |
| src/build_version_handshake.rs | `mismatch_prompt_no_agents_matches_prd_form` | Format encoder. |
| src/build_version_handshake.rs | `mismatch_prompt_with_agents_lists_them_under_header_and_warns_about_data_loss` | Format encoder. |
| src/build_version_handshake.rs | `mismatch_prompt_pluralization_pinned_at_n_managed_agents` | Format encoder. |
| src/config.rs | `bell_config_defaults` | Struct-default predicate. |
| src/config.rs | `bell_config_deserialize_empty` | TOML parser round-trip. |
| src/config.rs | `bell_config_deserialize_partial` | TOML parser round-trip. |
| src/config.rs | `dashboard_config_without_bell_section` | TOML parser round-trip. |
| src/config.rs | `dashboard_config_with_bell_section` | TOML parser round-trip. |
| src/config.rs | `should_bell_respects_enabled` | Pure predicate on config. |
| src/config.rs | `theme_defaults_to_auto` | TOML parser round-trip. |
| src/config.rs | `theme_deserialize_light` | TOML parser round-trip. |
| src/config.rs | `theme_get_set_field` | Pure field reflection. |
| src/config.rs | `saved_session_round_trip` | TOML serde round-trip. |
| src/config.rs | `saved_session_empty_default` | Struct-default predicate. |
| src/config.rs | `saved_session_deserialize_empty` | TOML parser round-trip. |
| src/config.rs | `should_bell_per_status` | Pure predicate matrix. |
| src/config.rs | `star_prompt_default_values` | Struct-default predicate. |
| src/config.rs | `star_prompt_serde_round_trip` | JSON serde round-trip. |
| src/config.rs | `star_prompt_serde_missing_fields` | JSON parser. |
| src/config.rs | `star_prompt_increment_and_check_triggers_at_10` | Pure counter logic. |
| src/config.rs | `star_prompt_snooze_resets_window` | Pure counter logic. |
| src/config.rs | `star_prompt_dismiss_permanently` | Pure counter logic. |
| src/config.rs | `idle_art_config_defaults` | Struct-default predicate. |
| src/config.rs | `dashboard_config_without_idle_art` | TOML parser round-trip. |
| src/config.rs | `dashboard_config_with_idle_art` | TOML parser round-trip. |
| src/config.rs | `idle_art_get_set_fields` | Pure field reflection. |
| src/config.rs | `auto_config_prompt_defaults_to_true` | Struct-default predicate. |
| src/config.rs | `auto_config_prompt_deserialize_missing` | TOML parser. |
| src/config.rs | `auto_config_prompt_deserialize_false` | TOML parser. |
| src/config.rs | `auto_config_prompt_get_set_field` | Pure field reflection. |
| src/config.rs | `config_gen_state_default_empty` | Struct-default predicate. |
| src/config.rs | `config_gen_state_suppress_and_check` | Pure membership predicate. |
| src/config.rs | `config_gen_state_serde_round_trip` | JSON serde round-trip. |
| src/config_gen.rs | `prompt_interpolates_directory` | Template-string encoder. |
| src/config_gen.rs | `prompt_contains_key_sections` | Template-string encoder. |
| src/config_gen.rs | `prompt_contains_orchestration_sections` | Template-string encoder. |
| src/config_gen.rs | `prompt_contains_orchestration_guidelines` | Template-string encoder. |
| src/config_gen.rs | `prompt_contains_orchestration_example` | Template-string encoder. |
| src/config_gen.rs | `prompt_contains_role_library_section` | Template-string encoder. |
| src/config_gen.rs | `prompt_renders_every_library_role` | Template + static-data check. |
| src/config_gen.rs | `role_library_parses_and_has_expected_roles` | Static-data parser. |
| src/config_gen.rs | `prompt_contains_context_handoff_mandate` | Template-string encoder. |
| src/config_gen.rs | `every_worker_role_has_missing_context_backstop` | Static-data invariant. |
| src/config_gen.rs | `release_role_has_clear_false` | Static-data invariant. |
| src/config_validation.rs | `valid_config_has_no_issues` | Pure validation predicate. |
| src/config_validation.rs | `invalid_regex_produces_error` | Pure validation predicate. |
| src/config_validation.rs | `duplicate_mode_names_produce_warning` | Pure validation predicate. |
| src/config_validation.rs | `interval_without_watch_produces_warning` | Pure validation predicate. |
| src/config_validation.rs | `watch_with_interval_is_valid` | Pure validation predicate. |
| src/config_validation.rs | `multiple_issues_across_modes` | Pure validation predicate. |
| src/config_validation.rs | `display_format` | Format encoder. |
| src/config_validation.rs | `rules_with_zero_reactive_panes_produces_error` | Pure validation predicate. |
| src/config_validation.rs | `empty_config_is_valid` | Pure validation predicate. |
| src/config_validation.rs | `empty_mode_is_valid` | Pure validation predicate. |
| src/config_validation.rs | `valid_orchestration_has_no_issues` | Pure validation predicate. |
| src/config_validation.rs | `orchestration_fewer_than_two_roles_is_error` | Pure validation predicate. |
| src/config_validation.rs | `orchestration_no_start_role_is_error` | Pure validation predicate. |
| src/config_validation.rs | `orchestration_multiple_start_roles_is_error` | Pure validation predicate. |
| src/config_validation.rs | `orchestration_duplicate_role_names_is_error` | Pure validation predicate. |
| src/config_validation.rs | `orchestration_duplicate_names_produce_warning` | Pure validation predicate. |
| src/config_validation.rs | `orchestration_worker_without_description_warns` | Pure validation predicate. |
| src/config_validation.rs | `orchestration_role_name_with_slash_is_error` | Pure validation predicate. |
| src/config_validation.rs | `orchestration_role_name_with_backslash_is_error` | Pure validation predicate. |
| src/config_validation.rs | `orchestration_role_name_with_path_traversal_is_error` | Pure validation predicate. |
| src/config_validation.rs | `sanitize_role_name_removes_traversal` | Pure validation predicate. |
| src/config_validation.rs | `sanitize_role_name_slash_removal_cannot_create_dotdot` | Pure validation predicate. |
| src/connect.rs | `build_connect_command_has_t_flag_and_via_daemon_env` | Pure `Command` construction. |
| src/connect.rs | `build_connect_command_passes_port_and_key` | Pure `Command` construction. |
| src/connect.rs | `build_connect_command_omits_key_when_none` | Pure `Command` construction. |
| src/connect.rs | `parse_version_output_accepts_canonical_and_v_prefixed` | Pure string parser. |
| src/connect.rs | `parse_version_output_rejects_wrong_program_name` | Pure string parser. |
| src/connect.rs | `parse_version_output_rejects_garbage_and_empty` | Pure string parser. |
| src/connect.rs | `exit_code_from_status_passes_through_normal_exit` | Pure ExitStatus mapper. |
| src/connect.rs | `exit_code_from_status_maps_signal_to_128_plus_signal` | Pure ExitStatus mapper. |
| src/daemon_protocol.rs | `frame_round_trip` | Binary frame serde (in-memory Cursor). |
| src/daemon_protocol.rs | `frame_eof_returns_none` | Binary frame serde (in-memory). |
| src/daemon_protocol.rs | `frame_partial_header_returns_err` | Binary frame serde (in-memory). |
| src/daemon_protocol.rs | `frame_partial_body_returns_err` | Binary frame serde (in-memory). |
| src/daemon_protocol.rs | `frame_zero_length_payload` | Binary frame serde (in-memory). |
| src/daemon_protocol.rs | `frame_rejects_oversize` | Binary frame serde (in-memory). |
| src/daemon_protocol.rs | `request_serde_round_trip` | JSON serde round-trip. |
| src/daemon_protocol.rs | `start_agent_omits_display_name_when_none` | JSON serde wire-shape pin. |
| src/daemon_protocol.rs | `start_agent_with_mode_tab_membership_round_trip` | JSON serde round-trip. |
| src/daemon_protocol.rs | `start_agent_with_orchestration_tab_membership_round_trip` | JSON serde round-trip. |
| src/daemon_protocol.rs | `agent_record_with_tab_membership_round_trip` | JSON serde round-trip. |
| src/daemon_protocol.rs | `agent_record_omits_tab_membership_when_none` | JSON serde wire-shape pin. |
| src/daemon_protocol.rs | `agent_record_without_tab_membership_field_deserializes` | JSON parser. |
| src/daemon_protocol.rs | `start_agent_deserializes_old_client_shape_without_tab_membership` | JSON parser. |
| src/daemon_protocol.rs | `agent_record_deserializes_old_daemon_shape_without_tab_membership` | JSON parser. |
| src/daemon_protocol.rs | `set_agent_label_serde_round_trip` | JSON serde round-trip. |
| src/daemon_protocol.rs | `resize_request_serde_round_trip` | JSON serde round-trip. |
| src/daemon_protocol.rs | `subscribe_events_request_serde_round_trip` | JSON serde round-trip. |
| src/daemon_protocol.rs | `kind_event_frame_round_trip` | Binary frame + JSON serde (in-memory). |
| src/daemon_protocol.rs | `response_helpers` | Pure constructor checks. |
| src/daemon_protocol.rs | `hello_request_serde_round_trip` | JSON serde round-trip. |
| src/daemon_protocol.rs | `hello_request_omits_client_build_version_when_none` | JSON serde wire-shape pin. |
| src/daemon_protocol.rs | `hello_request_deserializes_legacy_shape_without_client_build_version` | JSON parser. |
| src/daemon_protocol.rs | `hello_response_serde_round_trip` | JSON serde round-trip. |
| src/daemon_protocol.rs | `response_omits_build_version_when_none` | JSON serde wire-shape pin. |
| src/daemon_protocol.rs | `response_deserializes_legacy_shape_without_build_version` | JSON parser. |
| src/daemon_protocol.rs | `response_omits_server_version_when_none` | JSON serde wire-shape pin. |
| src/daemon_protocol.rs | `response_deserializes_legacy_shape_without_server_version` | JSON parser. |
| src/daemon_stop.rs | `live_agents_error_message_mentions_force_flag` | Format encoder. |
| src/daemon_stop.rs | `timed_out_error_message_mentions_force_recovery` | Format encoder. |
| src/event.rs | `parse_full_event` | JSON parser. |
| src/event.rs | `parse_minimal_event` | JSON parser. |
| src/event.rs | `parse_event_with_user_prompt` | JSON parser. |
| src/event.rs | `parse_event_without_user_prompt` | JSON parser. |
| src/event.rs | `reject_invalid_event_type` | JSON parser. |
| src/event.rs | `agent_type_from_command_recognizes_claude` | Pure classifier. |
| src/event.rs | `agent_type_from_command_recognizes_opencode` | Pure classifier. |
| src/event.rs | `agent_type_from_command_returns_none_for_unknown_or_empty` | Pure classifier. |
| src/event.rs | `parse_open_code_event` | JSON parser. |
| src/event.rs | `serialize_deserialize_delegate_signal` | JSON serde round-trip. |
| src/event.rs | `serialize_deserialize_work_done_signal` | JSON serde round-trip. |
| src/event.rs | `work_done_signal_defaults` | JSON parser default. |
| src/event.rs | `agent_event_not_parseable_as_daemon_message` | JSON parser. |
| src/hook.rs | `map_session_start` | Pure event-type mapper. |
| src/hook.rs | `map_pre_tool_use` | Pure event-type mapper. |
| src/hook.rs | `map_post_tool_use` | Pure event-type mapper. |
| src/hook.rs | `map_notification` | Pure event-type mapper. |
| src/hook.rs | `map_permission_request` | Pure event-type mapper. |
| src/hook.rs | `map_stop` | Pure event-type mapper. |
| src/hook.rs | `map_session_end` | Pure event-type mapper. |
| src/hook.rs | `map_unknown_returns_none` | Pure event-type mapper. |
| src/hook.rs | `tool_detail_bash_command` | Pure JSON extractor. |
| src/hook.rs | `tool_detail_bash_truncates_long_command` | Pure JSON extractor. |
| src/hook.rs | `tool_detail_read_file_path` | Pure JSON extractor. |
| src/hook.rs | `tool_detail_edit_file_path` | Pure JSON extractor. |
| src/hook.rs | `tool_detail_grep_pattern` | Pure JSON extractor. |
| src/hook.rs | `tool_detail_glob_pattern` | Pure JSON extractor. |
| src/hook.rs | `tool_detail_agent_description` | Pure JSON extractor. |
| src/hook.rs | `tool_detail_unknown_tool_uses_first_string` | Pure JSON extractor. |
| src/hook.rs | `tool_detail_none_when_no_input` | Pure JSON extractor. |
| src/hook.rs | `tool_detail_none_when_no_tool_name` | Pure JSON extractor. |
| src/hook.rs | `build_event_session_start` | Pure builder. |
| src/hook.rs | `build_event_tool_start_with_detail` | Pure builder. |
| src/hook.rs | `build_event_unknown_hook_returns_none` | Pure builder. |
| src/hook.rs | `build_event_user_prompt_submit_extracts_prompt` | Pure builder. |
| src/hook.rs | `build_event_prompt_truncated_to_200` | Pure builder. |
| src/hook.rs | `build_event_bash_tool_start_stores_full_command` | Pure builder. |
| src/hook.rs | `build_event_bash_tool_end_no_bash_command` | Pure builder. |
| src/hook.rs | `build_event_non_bash_tool_start_no_bash_command` | Pure builder. |
| src/hook.rs | `deserialize_claude_code_hook_input` | JSON parser. |
| src/hook.rs | `deserialize_minimal_hook_input` | JSON parser. |
| src/hook.rs | `map_opencode_session_created` | Pure event-type mapper. |
| src/hook.rs | `map_opencode_session_deleted` | Pure event-type mapper. |
| src/hook.rs | `map_opencode_session_idle` | Pure event-type mapper. |
| src/hook.rs | `map_opencode_session_error` | Pure event-type mapper. |
| src/hook.rs | `map_opencode_session_status_default` | Pure event-type mapper. |
| src/hook.rs | `map_opencode_session_status_idle` | Pure event-type mapper. |
| src/hook.rs | `map_opencode_session_status_error` | Pure event-type mapper. |
| src/hook.rs | `map_opencode_tool_before` | Pure event-type mapper. |
| src/hook.rs | `map_opencode_tool_after` | Pure event-type mapper. |
| src/hook.rs | `map_opencode_permission_asked` | Pure event-type mapper. |
| src/hook.rs | `map_opencode_unknown_returns_none` | Pure event-type mapper. |
| src/hook.rs | `build_opencode_event_session_created` | Pure builder. |
| src/hook.rs | `build_opencode_event_tool_with_detail` | Pure builder. |
| src/hook.rs | `build_opencode_event_bash_tool_start_stores_full_command` | Pure builder. |
| src/hook.rs | `build_opencode_event_unknown_returns_none` | Pure builder. |
| src/hook.rs | `deserialize_opencode_hook_input` | JSON parser. |
| src/hook.rs | `deserialize_minimal_opencode_input` | JSON parser. |
| src/hook.rs | `pane_id_propagated_from_env_claude_code` | Pure builder + env (test-local). |
| src/hook.rs | `pane_id_propagated_from_env_opencode` | Pure builder + env (test-local). |
| src/hook.rs | `send_to_missing_socket_returns_none` | Pure failure predicate. |
| src/hyperlink.rs | `plain_text_passes_through` | Stream filter parser. |
| src/hyperlink.rs | `single_link_bel` | Stream filter parser. |
| src/hyperlink.rs | `single_link_st` | Stream filter parser. |
| src/hyperlink.rs | `link_with_id_param` | Stream filter parser. |
| src/hyperlink.rs | `link_with_surrounding_text` | Stream filter parser. |
| src/hyperlink.rs | `ansi_inside_link` | Stream filter parser. |
| src/hyperlink.rs | `non_osc8_passes_through` | Stream filter parser. |
| src/hyperlink.rs | `split_across_chunks` | Stream filter parser. |
| src/hyperlink.rs | `regular_csi_passes_through` | Stream filter parser. |
| src/hyperlink.rs | `row_set_get` | Pure map predicate. |
| src/hyperlink.rs | `row_shift_up` | Pure map predicate. |
| src/hyperlink.rs | `row_clear` | Pure map predicate. |
| src/hyperlink.rs | `full_pipeline` | Stream filter parser (in-memory vt100). |
| src/opencode_manage.rs | `plugin_template_uses_exec_file_sync` | Template encoder. |
| src/opencode_manage.rs | `plugin_subpath_appends_plugin_and_name` | Pure path constructor. |
| src/pane.rs | `rename_outcome_applied_returns_applied_for_valid_input` | Validation predicate. |
| src/pane.rs | `rename_outcome_applied_trims_surrounding_whitespace` | Validation predicate. |
| src/pane.rs | `rename_outcome_applied_rejects_control_bytes` | Validation predicate. |
| src/pane.rs | `rename_outcome_applied_treats_empty_as_cleared` | Validation predicate. |
| src/pane.rs | `rename_outcome_applied_treats_whitespace_only_as_cleared` | Validation predicate. |
| src/pane.rs | `rename_outcome_applied_rejects_oversized_label` | Validation predicate. |
| src/pane.rs | `rename_outcome_applied_accepts_unicode_label` | Validation predicate. |
| src/pane_input.rs | `encode_pane_payload_single_line` | Pure encoder. |
| src/pane_input.rs | `encode_pane_payload_strips_trailing_whitespace` | Pure encoder. |
| src/pane_input.rs | `encode_pane_payload_wraps_multiline` | Pure encoder. |
| src/pane_input.rs | `encode_pane_payload_multiline_with_trailing_newline` | Pure encoder. |
| src/pane_input.rs | `encode_pane_payload_empty` | Pure encoder. |
| src/pane_input.rs | `encode_pane_payload_rejects_embedded_paste_end_marker` | Pure encoder. |
| src/pane_input.rs | `encode_pane_payload_rejects_embedded_paste_start_marker` | Pure encoder. |
| src/pane_input.rs | `encode_pane_payload_single_line_with_marker_still_passes` | Pure encoder. |
| src/project_config.rs | `parse_valid_full_config` | TOML parser. |
| src/project_config.rs | `parse_minimal_config` | TOML parser. |
| src/project_config.rs | `watch_defaults_to_false` | TOML parser default. |
| src/project_config.rs | `pane_watch_defaults_to_true` | TOML parser default. |
| src/project_config.rs | `pane_watch_can_be_set_to_false` | TOML parser. |
| src/project_config.rs | `pane_name_defaults_to_none` | TOML parser default. |
| src/project_config.rs | `reactive_panes_defaults_to_two` | TOML parser default. |
| src/project_config.rs | `reactive_panes_configurable` | TOML parser. |
| src/project_config.rs | `parse_full_orchestration_config` | TOML parser. |
| src/project_config.rs | `parse_orchestration_alongside_modes` | TOML parser. |
| src/project_config.rs | `orchestration_clear_defaults_to_true` | TOML parser default. |
| src/project_config.rs | `orchestration_description_defaults_to_none` | TOML parser default. |
| src/project_config.rs | `orchestration_prompt_template_defaults_to_none` | TOML parser default. |
| src/project_config.rs | `synthesize_uses_provided_orchestration_name` | Pure data transform. |
| src/project_config.rs | `synthesize_role_count_matches_max_index_plus_one` | Pure data transform. |
| src/project_config.rs | `synthesize_marks_start_role_from_metadata` | Pure data transform. |
| src/project_config.rs | `synthesize_leaves_display_fields_at_defaults` | Pure data transform. |
| src/project_config.rs | `synthesize_handles_empty_role_name_via_placeholder` | Pure data transform. |
| src/project_config.rs | `synthesize_empty_slots_yields_empty_roles` | Pure data transform. |
| src/project_config.rs | `synthesize_first_wins_on_duplicate_role_index` | Pure data transform. |
| src/project_config.rs | `orchestration_role_start_defaults_to_false` | TOML parser default. |
| src/project_config.rs | `modes_only_config_still_works` | TOML parser. |
| src/project_config.rs | `orchestrations_only_config_works` | TOML parser. |
| src/project_config.rs | `missing_required_pattern_is_error` | TOML parser error path. |
| src/remote.rs | `ssh_target_parse_with_user` | URL parser. |
| src/remote.rs | `ssh_target_parse_without_user` | URL parser. |
| src/remote.rs | `system_ssh_executor_quotes_arguments_safely` | Pure Command construction. |
| src/remote.rs | `system_ssh_executor_omits_key_flag_when_none` | Pure Command construction. |
| src/remote.rs | `system_ssh_executor_emits_no_timeout_options_by_default` | Pure Command construction. |
| src/remote.rs | `system_ssh_executor_with_wallclock_timeout_sets_ssh_options` | Pure Command construction. |
| src/remote.rs | `detect_platform_known` | Pure classifier. |
| src/remote.rs | `detect_platform_unknown` | Pure classifier. |
| src/remote.rs | `parse_version_output_typical` | Pure string parser. |
| src/remote.rs | `validate_version_string_accepts_semver_shapes` | Validation predicate. |
| src/remote.rs | `validate_version_string_strips_optional_v_prefix` | Validation predicate. |
| src/remote.rs | `validate_version_string_rejects_malformed` | Validation predicate. |
| src/remote.rs | `build_install_command_rejects_invalid_version` | Pure command-string builder. |
| src/remote.rs | `build_install_command_url_unprefixed_version` | Pure command-string builder. |
| src/remote.rs | `build_install_command_is_atomic` | Pure command-string builder. |
| src/remote.rs | `build_install_command_url_normalizes_v_prefixed_version` | Pure command-string builder. |
| src/remote.rs | `classify_ssh_error_host_key_verification_failed` | Pure classifier. |
| src/remote.rs | `classify_ssh_error_host_key_changed_routes_to_same_variant` | Pure classifier. |
| src/remote.rs | `classify_ssh_error_connection_refused_still_works` | Pure classifier. |
| src/remote.rs | `classify_ssh_error_auth_failed_still_works` | Pure classifier. |
| src/state.rs | `compose_delegate_prompt_appends_work_done_footer` | Pure prompt encoder. |
| src/tab_layout.rs | `all_fit_returns_labels_unchanged` | Pure layout-fitting math. |
| src/tab_layout.rs | `single_overflow_truncates_only_long_tab` | Pure layout-fitting math. |
| src/tab_layout.rs | `all_overflow_caps_every_label` | Pure layout-fitting math. |
| src/tab_layout.rs | `unicode_label_width_uses_cells_not_bytes` | Pure layout-fitting math. |
| src/tab_layout.rs | `single_tab_truncates_or_passes_through` | Pure layout-fitting math. |
| src/tab_layout.rs | `zero_width_collapses_to_empty_when_cap_is_zero` | Pure layout-fitting math. |
| src/tab_layout.rs | `rendered_total_matches_padding_plus_dividers_formula` | Pure layout-fitting math. |
| src/tab_layout.rs | `truncated_total_stays_within_available_width` | Pure layout-fitting math. |
| src/theme.rs | `dark_palette_values` | Static-data predicate. |
| src/theme.rs | `light_palette_values` | Static-data predicate. |
| src/theme.rs | `resolve_explicit_dark` | Pure resolver. |
| src/theme.rs | `resolve_explicit_light` | Pure resolver. |
| src/theme.rs | `theme_display` | Format encoder. |
| src/theme.rs | `theme_from_str` | String parser. |
| src/ui.rs | `dead_slot_pane_id_is_deterministic_per_role` | ID generator. |
| src/ui.rs | `dead_slot_pane_id_disambiguates_hyphenated_inputs` | ID generator. |
| src/version.rs | `test_current_version_parses` | Static-data parser. |
| src/version.rs | `test_should_notify_newer` | Semver predicate. |
| src/version.rs | `test_should_notify_same` | Semver predicate. |
| src/version.rs | `test_should_notify_older` | Semver predicate. |
| src/version.rs | `test_v_prefix_stripped` | Semver predicate. |
| src/version.rs | `test_invalid_version_returns_none` | Semver predicate. |

### Moved to tmp/legacy-tests/

#### Whole-file moves under `tests/`

| Original path | Moved to | Reason |
|---|---|---|
| tests/agent_metadata.rs | tmp/legacy-tests/tests/agent_metadata.rs | Spawns daemon and exercises label-lifecycle protocol; needs L2 rewrite. |
| tests/build_id.rs | tmp/legacy-tests/tests/build_id.rs | Spawns process with build-id overrides; touches env + subprocess. |
| tests/build_version_handshake.rs | tmp/legacy-tests/tests/build_version_handshake.rs | Boots real daemon to exercise the build-version handshake; needs L2. |
| tests/close_pane_errors.rs | tmp/legacy-tests/tests/close_pane_errors.rs | Drives daemon close-pane error paths; needs L2. |
| tests/common/mod.rs | tmp/legacy-tests/tests/common/mod.rs | Shared scaffolding for the moved tests — moves with them. |
| tests/connect_lookup.rs | tmp/legacy-tests/tests/connect_lookup.rs | Touches filesystem registry and SSH probing; needs L2 once remote harness exists. |
| tests/daemon_attach_cleanup.rs | tmp/legacy-tests/tests/daemon_attach_cleanup.rs | Boots daemon, attaches, asserts socket-cleanup; needs L2. |
| tests/daemon_integration.rs | tmp/legacy-tests/tests/daemon_integration.rs | Boots daemon, exercises real protocol; needs L2. |
| tests/daemon_lifecycle.rs | tmp/legacy-tests/tests/daemon_lifecycle.rs | Daemon start/stop lifecycle; needs L2. |
| tests/daemon_protocol.rs | tmp/legacy-tests/tests/daemon_protocol.rs | Live daemon protocol roundtrip; needs L2 (in-memory frame tests remain in src/). |
| tests/daemon_stop.rs | tmp/legacy-tests/tests/daemon_stop.rs | Exercises `daemon stop` against a real daemon; needs L2. |
| tests/event_forwarding.rs | tmp/legacy-tests/tests/event_forwarding.rs | Hook → daemon → UI event flow against real sockets; needs L2. |
| tests/external_daemon.rs | tmp/legacy-tests/tests/external_daemon.rs | Spawns + ensures external daemon, trust checks; needs L2. |
| tests/integration_test.rs | tmp/legacy-tests/tests/integration_test.rs | Top-level integration smoke against the spawned binary; needs L2. |
| tests/local_attach.rs | tmp/legacy-tests/tests/local_attach.rs | Attaches a real PTY to a running daemon; needs L2. |
| tests/mode_integration_test.rs | tmp/legacy-tests/tests/mode_integration_test.rs | Mode activation + pane wiring through Tab/Mode/Daemon; needs L2. |
| tests/orchestration_delegate.rs | tmp/legacy-tests/tests/orchestration_delegate.rs | Drives orchestration delegate signal through daemon; needs L2. |
| tests/pane_auto_renew_on_respawn.rs | tmp/legacy-tests/tests/pane_auto_renew_on_respawn.rs | Spawns/renews PTY against a real daemon; needs L2. |
| tests/process_group_kill.rs | tmp/legacy-tests/tests/process_group_kill.rs | Exercises real process-group signalling; needs L2. |
| tests/rehydration.rs | tmp/legacy-tests/tests/rehydration.rs | Daemon hydration after reconnect; needs L2. |
| tests/remote_add.rs | tmp/legacy-tests/tests/remote_add.rs | Touches filesystem registry + SSH; needs L2 (remote harness later). |
| tests/remote_lifecycle.rs | tmp/legacy-tests/tests/remote_lifecycle.rs | End-to-end remote add/connect/remove flow; needs L2. |
| tests/resize_coalescing.rs | tmp/legacy-tests/tests/resize_coalescing.rs | SIGWINCH coalescing across daemon + PTY; needs L2. |
| tests/session_restore_test.rs | tmp/legacy-tests/tests/session_restore_test.rs | Restores saved session against daemon + TUI; needs L2. |
| tests/spawn_time_role_prompt_atomic.rs | tmp/legacy-tests/tests/spawn_time_role_prompt_atomic.rs | Atomic role-prompt write at agent spawn; touches filesystem + daemon. |
| tests/stop_dialog.rs | tmp/legacy-tests/tests/stop_dialog.rs | Exercises stop-dialog interactive flow; needs L2 (TUI input). |

#### Partial / full `mod tests` moves under `src/`

| Original path | Moved to | Reason |
|---|---|---|
| src/agent_pty.rs (`mod tests`: spawn_*, registry_*, write_to_pane_*, close_agent_*, agent_records_*, change_notify_*, live_count_*, child_guard_*, spawn_options_env_*, spawn_scrubs_*) | tmp/legacy-tests/src/agent_pty.rs | Touches PTY registry, child processes, async I/O. |
| src/build_version_handshake.rs (`mod tests`: terminate_*) | tmp/legacy-tests/src/build_version_handshake.rs | Async terminate path drives real Unix sockets and child processes. |
| src/config.rs (`mod tests`: saved_session_load_save_clear, star_prompt_load_save_cycle, attach_socket_fallback_is_per_user, state_dir_*, config_gen_state_suppress_dir_deduplicates, config_gen_state_load_save_cycle) + cfg(test) STATE_DIR_ENV_LOCK / CONFIG_GEN_STATE_ENV_LOCK / ConfigGenStateEnvGuard helpers | tmp/legacy-tests/src/config.rs | Read/write filesystem and process-global env vars. |
| src/connect.rs (`mod tests`: lookup_*, picker_*, probe_*, run_connect_*, touch_last_connected_*, probe_timeout_secs_*, probe_protocol_*) | tmp/legacy-tests/src/connect.rs | Exercises SSH mocks, filesystem registry, env vars — all subsystem-shaped, not pure-data. |
| src/daemon.rs (whole `mod tests`) | tmp/legacy-tests/src/daemon.rs | Boots daemons, sockets, idle monitor, lock dirs. |
| src/daemon_attach.rs (whole `mod tests`) | tmp/legacy-tests/src/daemon_attach.rs | Unix-socket trust checks, spawn lock, env vars. |
| src/daemon_client.rs (whole `mod tests`) | tmp/legacy-tests/src/daemon_client.rs | End-to-end daemon-protocol calls against a real server. |
| src/daemon_protocol.rs (`mod tests`: daemon_hello_dispatch_*) | tmp/legacy-tests/src/daemon_protocol.rs | Dispatches through `handle_connection` on a live UnixStream pair. |
| src/daemon_stop.rs (`mod tests`: stop_returns_*, restart_no_daemon_running_is_idempotent) | tmp/legacy-tests/src/daemon_stop.rs | Touches real attach sockets and async daemon termination. |
| src/hooks_manage.rs (whole `mod tests`) | tmp/legacy-tests/src/hooks_manage.rs | Writes Claude Code settings.json on the filesystem. |
| src/init.rs (whole `mod tests`) | tmp/legacy-tests/src/init.rs | Writes config files to disk. |
| src/llm.rs (whole `mod tests`) | tmp/legacy-tests/src/llm.rs | Hits LLM HTTP endpoints / asserts env-missing errors. |
| src/mode_manager.rs (whole `mod tests`) | tmp/legacy-tests/src/mode_manager.rs | Drives `ModeManager` through a mock `PaneController` — Mode* subsystem. |
| src/opencode_manage.rs (`mod tests`: install_*, uninstall_*, auto_install_*, detect_opencode_root_*) plus dead test-only `auto_install_to` helper | tmp/legacy-tests/src/opencode_manage.rs | Touches filesystem and env vars to install/inspect OpenCode plugin dirs. |
| src/project_config.rs (`mod tests`: load_missing_file_returns_none, load_malformed_toml_returns_error, load_valid_file, mode_lookup_by_name_returns_none_when_renamed) | tmp/legacy-tests/src/project_config.rs | Filesystem-backed config load. |
| src/remote.rs (`mod tests`: run_with_wallclock_kill_*, remotes_toml_written_at_0o600, remotes_toml_save_creates_parent_directory, remotes_file_round_trip_two_entries) | tmp/legacy-tests/src/remote.rs | Spawns real child processes and writes registry files. |
| src/state.rs (`mod tests`: session lifecycle, placeholder, dispatch, work_done file write, async wait_for_session_start, …) | tmp/legacy-tests/src/state.rs | Exercises `AppState` event handling, async, and filesystem. |
| src/tab.rs (whole `mod tests`) | tmp/legacy-tests/src/tab.rs | Drives `TabManager` through a mock controller — Tab* subsystem. |
| src/terminal_widget.rs (whole `mod tests`) | tmp/legacy-tests/src/terminal_widget.rs | UI render tests against a ratatui buffer — will be reborn as L1 widget tests in M2. |
| src/ui.rs (`mod tests`: layout dims, partition_*, dedupe_*, fill_dead_slots_*, dashboard_filter_*, synthesised_orchestration_tab_*, config-gen prompt handlers, …) | tmp/legacy-tests/src/ui.rs | UI/state subsystem — large block of TUI helpers driven against handcrafted state. |

## Test Case Catalog

*Populated by M1.*

## Refined Milestones

*Populated by M1.*

**Pre-committed items** (regardless of how M1 reshapes the rest): the milestone that ships the first usable end-to-end harness must also:

- **(a) Update `CLAUDE.md`** with the conventions in [Appendix A](#appendix-a-proposed-claudemd-additions) (functional UI changes require harness tests; fast-tests-per-task / e2e-before-PR; single-test rerun pattern for failures).
- **(b) Ship the CONTRIBUTING.md sections** specified in Decision 19 (snapshot review workflow, TDD loop, how to add a new test).

Both arrive the moment they can be followed, not before.

## Discovered Issues

*Populated by M2+ as tests are written and run. See Decision 11 for the discovery policy and Decision 25 for the entry template + scoping. Under agent autonomy, the agent stops after this list is populated and surfaces it to the user before fixing, sorted by severity per Decision 25.*

Entry format (per Decision 25):

| ID | Catalog ref | Description | Severity | Status |
|---|---|---|---|---|
| *(empty until M2+)* | | | | |

`Severity` is one of: `blocker`, `major`, `minor`.
`Status` is one of: `fixed in <milestone>`, `filed as #NNN`, `won't-fix: <rationale>`, `escalated to PRD #NNN`.

## Appendix A: Proposed CLAUDE.md Additions

To be added as permanent instructions in `CLAUDE.md` in the same milestone that ships the first usable harness — not earlier:

> **Add or Update TUI Tests for Functional UI Changes**: When a change adds or modifies user-visible TUI behavior (panes, statuses, prompts, focus, layout, modes, embedded panes, hook delivery), add or update tests in the TUI harness. Use L1 (in-process `TestBackend` + `insta`) for pure widget/layout changes; use L2 (PTY + vt100, files named `e2e_*.rs`, gated by `#[cfg(feature = "e2e")]`) when the change touches the spawned binary, daemon, hooks, attach protocol, or real agent integration. Pure refactors with no observable behavior change do not require new tests.

> **Fast Tests Per Task, E2E Before PR**: `cargo test-fast` (alias for `cargo nextest run`) runs the fast tier — protocol/state tests plus L1 widget/render tests — and is the per-task gate. `cargo test-e2e` (alias for `cargo nextest run --features e2e`) additionally runs the L2 PTY/real-agent suite and is required to pass before the release flow. Do not run `cargo test-e2e` per task; it spawns binaries, hits LLM APIs, and is intentionally bounded to the pre-PR step.

> **Iterate on a Failing Test by Rerunning Only That Test**: When a single test fails, after fixing the code, rerun *only that test* first (`cargo test-fast lifecycle_001` or `cargo test-e2e lifecycle_001`) to verify the fix in isolation. Decision 17's function-name prefix (`<sub-area>_<NNN>_…`) makes the filter pick exactly one test. Only after that test passes, rerun the full tier (`cargo test-fast`, plus `cargo test-e2e` pre-PR) before committing.

## Risks & Mitigations

| Risk | Mitigation |
|---|---|
| Test flake from timing assumptions | Quiescence-based waits (Decision 21); never raw `sleep`. |
| Windows ConPTY surprises (Decision 4) | Design for parsed grid only, not raw bytes. Build macOS + Linux first; Windows is a verification step, not a redesign. |
| Real-agent API costs | Eliminated from CI per Decision 8 (e2e is local only). Local runs use Haiku (Claude) and a cheap OpenRouter model (OpenCode); synthetic-event default keeps real-agent test count small. |
| Hook-socket clash with developer's real running deck | Per-test `DOT_AGENT_DECK_SOCKET` + `DOT_AGENT_DECK_ATTACH_SOCKET` + redirected `HOME`. |
| Snapshot rot from color/terminal profile drift | Pin color env vars per test (Decision 20); document review workflow in CONTRIBUTING.md (Decision 19). |
| Insta goldens accepted blindly during review | Documented review workflow + small snapshots that humans can read in a diff. |

## Dependencies

- `portable-pty 0.8` (already present)
- `vt100 0.16` (already present)
- `insta` (new dev-dep, pinned version per Decision 22)
- `cargo-nextest` (installed via `cargo install` or devbox; per Decision 13)
- Real Claude Code and OpenCode CLIs installed on the developer's local machine (where e2e runs per Decision 8). CI runs the fast tier only and does not need the agent CLIs.
- An OpenRouter API key configured locally for OpenCode chain-smoke tests. Like the Anthropic credential, this is a developer-environment requirement — never a CI secret per Decision 8.

## Validation Strategy

- **Fast tier (CI):** `cargo test-fast` green on macOS and Linux in GitHub Actions is the per-PR signal. Windows joins when the harness's Windows path is ready.
- **E2e tier (local):** `cargo test-e2e` green on the developer's machine before opening the PR is the chain-level signal. Enforced by the orchestrator's pre-release gate (Decision 6) and the iterative validation cadence (Decision 29). Windows e2e validation is its own milestone in M2+.

The user (PRD owner) does explicit validation at the checkpoints in Decision 29 (end of M1, end of M2, end of M3), then standard pre-PR sign-off on each subsequent milestone.
