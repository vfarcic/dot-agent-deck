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
