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

**CI-enforced linkage** — Rust binary at `xtask/linkage-check/`, invoked as `cargo xtask linkage-check`. Added to CI alongside fmt/clippy/test-fast, configured as a required status check on `main`. Seven checks, all must pass:

1. Every catalog ID has at least one test referencing it.
2. Every `#[spec("...")]` references a real catalog ID.
3. Catalog IDs match the format regex `^[a-z][a-z0-9-]*\/[a-z][a-z0-9-]*\/\d{3}$`.
4. Function name carries the annotation's `<sub>_<NNN>` prefix (Decision 17).
5. No raw `std::thread::sleep`, `tokio::time::sleep`, or `for _ in 0..N` polling in `tests/e2e_*.rs` (Decision 21).
6. No `#[ignore]` on `#[spec(...)]`-annotated tests (Decision 26).
7. Every `#[spec(...)]` test carries a `/// Scenario:` doc comment with a body AND `cargo xtask docs --tests` exits 0 against the current source + catalog (Decision 30; M4.3 simplification — the on-disk `.md` is not diffed because `.dot-agent-deck/` is gitignored).

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

**Cross-reference with Decision 31:** under the tester-coder TDD chain (Decision 31), tester writes the failing test that documents the fix; coder implements; tester re-runs to confirm GREEN.

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

### Decision 21: String-signal waits primary today; quiescence primary once the render loop is event-driven; raw sleeps forbidden

**Amended 2026-06-06** (was: "Quiescence-based waits primary; string-signal opt-in"). M2 execution surfaced that the deck's render loop redraws unconditionally every ~16 ms (the `crossterm::event::poll(16ms)` timeout *is* the tick) and async state changes do not wake the loop — they only become visible because of that constant repaint. So `wait_until_quiescent` never sees silence, and a naive env-gated tick-suppression breaks async-driven updates (the hook arrives but never paints until a keystroke). Quiescence is therefore deferred until the render loop becomes event-driven (wakeup channel bumped by the event subscriber + PTY tasks; loop idles until woken). That render change is a prerequisite, tracked for M4+/#84 — not part of #77's harness scope.

**Primary today:** `deck.wait_for_string("permission prompt")`. Catalog entries that assert on specific expected content (the dominant shape through M3) use this. The "use sparingly" caveat from the original framing is lifted — it's the de-facto primary until quiescence is wired.

**Primary once the render loop is event-driven:** `deck.wait_until_quiescent()` blocks until PTY output is silent for **50 ms** (default; tunable as a harness constant). At that point catalog entries that assert *nothing happened* (`dashboard/pane/003`, `prompt/permission/003`, `hooks/delivery/004`, …) become writable — until then they're blocked on the render-loop change.

**Forbidden in test bodies:**

- Raw `std::thread::sleep` / `tokio::time::sleep`.
- Polling loops with fixed retry counts (`for _ in 0..10 { ... }` as a disguised wait).

Linkage-check tool (Decision 7) grep-enforces these.

The 50ms quiescence default is a starting point. Tune once the render loop is event-driven and real test runtimes can be measured.

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

When an L2 test fails (panic or assertion failure), the harness dumps `.dot-agent-deck/recordings/<test-name>/`:

- **`final-grid.txt`** — vt100 grid at failure time as plain text.
- **`final-grid.svg`** — the same grid as styled SVG, colors preserved.
- **`full-stream.cast`** — asciinema-format recording of the entire PTY output. Replayable via `asciinema play`; convertible to GIF/MP4 via `agg <cast> <gif>` post-hoc.
- **`fixture.toml`** — copy of the `.dot-agent-deck.toml` the test used.
- **`test.md`** — the same paired `.md` Decision 30 produces, regenerated on every dump so the cast and its narrative travel together.

**Locality:** local-only artifact. L2 tests run only via `cargo test-e2e`, which is local-only per Decision 8. `cargo test-fast` in CI produces no recordings. No CI artifact upload, no GitHub Actions integration around recordings.

**Storage and cleanup:**

- Artifacts live under `.dot-agent-deck/recordings/<test-name>/`. The whole `.dot-agent-deck/` tree is gitignored dev-time state (M4.3) — no setup-script wipe is needed because re-running a test replaces its per-test artifacts in place via atomic write (tempfile + rename).

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

**Event-driven render — prerequisite for quiescence (added 2026-06-06).** M2 surfaced that the deck's render loop redraws unconditionally every ~16 ms with no async→render wakeup, which makes `wait_until_quiescent` non-functional (see amended Decision 21). The fix is a wakeup channel bumped by the event subscriber + per-pane PTY tasks; the render loop idles until woken or a long-timeout fires. This change does not fit cleanly inside the per-frame layout/PTY-size contract that #84 targets, but it shares #84's theme of "single owner of the render lifecycle" and the two changes can land together or sequentially. Either way it ships before the "assert nothing happened" catalog entries (`dashboard/pane/003`, `prompt/permission/003`, `hooks/delivery/004`, …) become writable.

### Decision 30: Paired test docs — every `#[spec]` test ships with an auto-generated `.md`

**Added 2026-06-06 after the M3 stop.** Reading the test source is the most accurate way to know what a test does, but it isn't the *easiest*. To make the catalog navigable for non-Rust readers (and to give AI workers a "what does this test do" answer that doesn't require reading the test source), every `#[spec]`-annotated test ships with a paired Markdown file.

**The `.md` contains:**
- Catalog ID + the catalog body (Layer, Agent, Asserts, Does not assert, Platform).
- **Scenario** — a 1–3 sentence plain-English narrative authored by the test writer via a `/// Scenario:` doc comment on the test function.
- **Steps** — auto-extracted from the test function body via `syn`; harness method calls map to plain-English steps (`launch_with_fixture(F)` → "Launch the deck with fixture F", `wait_for_string(S)` → "Wait for S to appear on screen", etc.). Unknown calls fall back to a raw call line.
- Source location (`file:function`).
- Cast filename (if any) + replay command.
- Rerun command (per Decision 18).

**Pair location:** the cast and `.md` live next to each other under `.dot-agent-deck/recordings/<test-fn-name>/` — a developer-machine path, never committed. The Scenario doc comment in test source is the authoritative external-reader surface (visible on GitHub); the `.md` is a local rendered view of (catalog entry + Scenario + auto-extracted Steps), regenerated by `cargo xtask docs --tests`. L1 tests have no cast; their `.md` lives at the same path with the Replay section omitted.

**Regeneration:**
- On-demand: `cargo xtask docs --tests` regenerates every `.md`.
- Automatic at record time: when `DOT_AGENT_DECK_RECORD=1` produces a cast, the harness also writes the paired `.md`.

**Enforcement (linkage-check rule 7).** Every `#[spec(...)]` test must:
1. Carry a `/// Scenario:` doc comment of at least one sentence.
2. Survive a `cargo xtask docs --tests` regeneration without error (catches missing Scenarios + malformed test sources).

Both checks run alongside the original six in `cargo xtask linkage-check` and are required status checks on `main`. The hard CI enforcement is the load-bearing layer; CLAUDE.md / CONTRIBUTING.md describe the convention but cannot force compliance on their own.

**Why the change (M4.3, 2026-06-06):** `.dot-agent-deck/` was moved to dev-time gitignored state — the asymmetric "this dir is data" exception was confusing, the milestone-prefixed `m2-recordings/` / `m3-recordings/` subdirs didn't scale to per-PRD additions, and a byte-identity diff against on-disk `.md` is meaningless when a fresh clone has no on-disk `.md` to begin with. Rule 7 simplifies to "Scenario comment present + generator succeeds."

**Why syntactic step extraction over per-statement comments:** the syntactic approach works on existing tests with zero author retrofit. New harness methods need a one-line entry in the method-name → step-template map, with a raw-call fallback so absence isn't a failure. Per-statement comments can be a future enhancement if quality demands it.

### Decision 31: Plan-first orchestrator workflow + tester role + synthetic-test inventory

**Added 2026-06-06, refined at the M4.4 stop.**

The orchestrator multi-agent setup gains a `tester` role specialized for TDD-style synthetic-test authoring + verification, and a plan-first workflow that decides upfront which work uses the TDD chain vs. coder-direct.

**Plan phase.** When starting work on a PRD, the orchestrator analyzes the PRD scope and produces a *test plan* listing catalog entries affected, test tier per entry, action (extend / modify / create / skip), and a one-sentence Scenario summary. The plan is surfaced to the user *before* any test or implementation work begins; the user approves or refines.

**Execution.** Per the approved plan:
- L2 synthetic + L1 widget items → TDD chain: tester (RED) → coder → tester (GREEN).
- Pure-data / chain-smoke / no-test items → coder direct.

These two paths can interleave within a single PRD; the orchestrator picks per item.

**Pre-merge inventory.** Before delegating release, the orchestrator runs `cargo xtask list-tests` and surfaces the synthetic-test inventory (catalog IDs created/modified, allowlist deltas) to the user as the final pre-merge confirmation.

**Tester binding instruction** (full text in `.dot-agent-deck.toml`): write *failing* synthetic tests per the approved plan; bias is extend > modify > new; every test ships with a `/// Scenario:` doc comment per Decision 30 + CLAUDE.md rule 7. Tester ignores chain-smoke (real-agent integration is not TDD-suited) and pure-data fixes (those belong with coder).

**Why now:** at M4's close PRD #77 hands the harness off to per-PRD test maintenance. Without a dedicated tester role, "write the test alongside the fix" rides on the coder's discretion under time pressure — exactly when it's most likely to be skipped. Splitting the test-author seat out makes the TDD step a delegation boundary the orchestrator can't accidentally walk past. The plan-first framing keeps the orchestrator from defaulting to either "everything is TDD" or "TDD only when convenient" — the plan, agreed with the user up front, is what decides per item. Pairing the workflow with `cargo xtask list-tests` makes the synthetic-test delta a structural artifact of every PR rather than something the reviewer has to derive from the diff.

**Scope guardrails for tester:**
- Sweet spot: L2 synthetic flows (hooks, status transitions, prompts, focus, lifecycle, resize, error paths) and L1 widget redesigns where rendering changes are observable in `TestBackend`.
- Out of scope: pure refactors, pure-data fixes, chain-smoke (real-agent work — stays with coder; cost + flakiness profile is not the tester's tier).
- Bias order: extend > modify > new. Adding a brand-new `#[spec]` test is the last resort, reached only when no catalog ID covers the surface.

**`cargo xtask list-tests` report shape:** four Markdown sections — Created, Modified, Catalog prose changes, Linkage-allowlist deltas. Empty sections render as `_(none)_` so the structure is stable for the orchestrator's pre-merge surface and for PR-template inclusion.

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

- [x] **M1 — Test case catalog and assertion strategy + STOP for validation** (per Decision 29). Two deliverables, in order:
  - **(1) Existing-test audit** (per Decision 10). Identify pure-data unit tests that stay live in `src/*/mod tests`; move everything else (all of `tests/*.rs`, all non-pure-data `src/*/mod tests`) into `./tmp/legacy-tests/` via `git mv` (`tmp/` is already gitignored). Audit lands *before* any new catalog entry is written — the moved tests serve as catalog-inspiration material during step (2).
  - **(2) Test case catalog.** Produce a written catalog (in this PRD) of the test cases the harness must cover, organized by feature area (dashboard panes, statuses, prompts, focus/navigation, modes/tabs, embedded pane attach, hook delivery, lifecycle, resize, error paths). For each test case, decide: which layer (L1 vs L2), which agent if any, what is asserted, what is explicitly not asserted, expected platform coverage. Per Decision 7, also commit to the file-layout-mirrors-catalog convention. **Per Decision 27, the catalog construction includes a docs cross-reference pass.**
  - **STOP** — user reviews both deliverables before M2 begins.
  - **Status (2026-05-26):** Validated. Audit + catalog landed across `ac9240d`, `14f15a0`, `101d1e2`. B1 (Decision 10 tmp/legacy-tests/ tracking tension) resolved via `git rm --cached -r tmp/legacy-tests/` in `ed93d4e` — option (a), no Decision 10 amendment needed.
- [x] **M2 — Minimum viable harness + 2 tests + STOP for validation** (per Decision 29). Build the minimum harness slice required to support exactly two specific catalog entries chosen from M1's catalog: one L1 test (in-process `TestBackend` + insta) and one L2 synthetic-event test (PTY + harness builder + recording on failure). Ship as part of this milestone: the linkage-check xtask binary (Decision 7), the failure-recording infrastructure (Decision 28), the CONTRIBUTING.md sections (Decision 19), the CLAUDE.md additions from Appendix A, and the `bacon.toml` at repo root (Decision 14). **M2 validation handoff additionally delivers `full-stream.cast` recordings of both seed tests** — capture them by running each test with `DOT_AGENT_DECK_RECORD=1` (Decision 28's existing opt-in) so the user can replay with `asciinema play` regardless of pass/fail. No amendment to Decision 28 is needed; this is a one-time delivery scope on M2's validation, not a new harness default. After this milestone lands, **stop and wait for explicit validation** before continuing to M3.
  - **Status (2026-06-06):** Validated. Deliverables landed across `6669e80`, `5eabe0a`, `d3f7bc2`, `9ee95fc`, `226a646`, `5b62538`, `f7c9042`, `58d72ba`. Reviewer + auditor fixes in `e40bd12`. Seed tests = `dashboard/pane/004` (L1) + `hooks/delivery/001` (L2 synthetic). Decision 16 re-check verdict: **keep in-house** (no ratatui-testlib at M2 kickoff meeting all four criteria). Q1 quiescence reconsideration resolved via **Option C**: Decision 21 amended to acknowledge `wait_for_string` as the primary practical wait today and quiescence as primary once the render loop becomes event-driven. Rationale: deck busy-redraws every ~16 ms and has no async→render wakeup, so a simple env-gated tick suppression would break async updates; a fingerprint-based change-detection alternative (option A) would be flake-prone for a test framework (violates Decision 9); and a production render-loop refactor (option B) was premature with only 2 harness tests as a safety net. Event-driven render is the prerequisite for quiescence and is tracked for M4+/#84 — not part of #77's scope.
- [x] **M3 — First chain-smoke tests + STOP for validation** (per Decision 29). One real Claude Code chain-smoke test (using `claude-haiku-4-5-20251001`), one real OpenCode chain-smoke test (using `openrouter/google/gemini-2.5-flash-lite`). Both picked from M1's catalog. After this milestone lands, **stop and wait for explicit validation** before continuing to M4+.
  - **Status (2026-06-06):** Validated — closed as **1 of 2 chain-smoke tests + di-001 escalated to PRD #79** (option (b) from the M3 stop). `chain-smoke/claude/001` shipped green (commits `ee58882`..`1788d54`); reviewer + auditor + 13-item fix batch (`1788d54`) all clean. `chain-smoke/opencode/001` deferred until PRD #79's scope expansion ships the deck install-path fix (`src/opencode_manage.rs` plugin registration vs OpenCode 1.x's loader). Harness side for OpenCode is complete (`with_imported_opencode_credentials`, fixture marker file) — the test drops in trivially once #79 lands. Catalog entry stays allowlisted in `xtask/linkage-check/m2.allowlist` with an inline comment naming the blocker.
- [x] **M4 — Test documentation generation** (per Decision 30). Build the auto-doc system that pairs a `.md` with every `#[spec]` test. Deliverables:
  - `cargo xtask docs --tests` generator (parses catalog + test source via `syn`; emits `.md` paired with each test under `.dot-agent-deck/<milestone>-recordings/`).
  - `/// Scenario:` doc-comment convention.
  - Linkage-check rule 7 (Scenario required + generated `.md` in sync; `cargo xtask linkage-check` exit 1 if either fails).
  - Generator hook in `tests/common/mod.rs` so `DOT_AGENT_DECK_RECORD=1` runs also write/refresh the paired `.md`.
  - Append CLAUDE.md rule 7 (Appendix A new entry below).
  - Update CONTRIBUTING.md "how to add a new test" with the Scenario step + the regenerate command.
  - Retrofit `Scenario:` doc comments + generated `.md` files for the existing three `#[spec]` tests (`dashboard/pane/004`, `hooks/delivery/001`, `chain-smoke/claude/001`).
  - After this milestone lands, **stop and wait for explicit validation** before opening the foundation PR.
  - **Status (2026-06-06):** Deliverables landed across `567069a`..`f7285cd`. Reviewer + auditor + 9-item fix batch in `21fa3ec` (symlink hardening on read paths, atomic write via tempfile+rename, Scenario markdown-injection sanitization, backtick fence escaping, Steps-section noise reduction, multi-paragraph Scenario preservation). Two polish follow-ups: `2d963b2` (M4.2 — pin `cargo-nextest@0.9.137` in `devbox.json`, add Prerequisites to CONTRIBUTING.md) and `6f891b3` (M4.3 — treat the whole `.dot-agent-deck/` as developer-machine state, blanket-gitignore; colocate cast + `.md` under `.dot-agent-deck/recordings/<test-fn-name>/`; simplify linkage-check rule 7 to "Scenario doc comment + generator succeeds" since on-disk byte-identity is meaningless when the file isn't committed). All 7 linkage-check rules green; `git ls-files .dot-agent-deck/` is empty. Awaiting Decision 29 user validation; on validation the foundation PR opens with all M1→M4 work bundled.
- [ ] **M5+ — Catalog buildout absorbed into per-PRD test maintenance** (locked in 2026-06-06 at the M3 stop). No dedicated catalog buildout phase within PRD #77. Per CLAUDE.md rule 4, every future PRD that touches a cataloged area ships the corresponding `#[spec]` test (and its `.md` per Decision 30 + CLAUDE.md rule 7) as part of the PRD's scope. The catalog IDs still allowlisted in `xtask/linkage-check/m2.allowlist` are the coverage roadmap; entries get removed as each future PRD lands its tests. The catalog-vs-deck "fix the deck not the test" policy (Decision 11) and the Discovered Issues collection (Decision 25) carry forward into those future PRDs. **PRD #77 closes after M4.** The [M4+ Buildout Strategy (Proposal)](#m4-buildout-strategy-proposal) section is preserved below for historical context — superseded by this entry.

## M4+ Buildout Strategy (Proposal)

**Status (2026-06-06): SUPERSEDED** by the M5+ milestone entry above. At the M3 user-validation stop, this proposal was set aside in favor of the simpler model: harness is permanent infrastructure; each future PRD adds the catalog tests for the area it touches (CLAUDE.md rule 4). No dedicated by-area buildout phase inside PRD #77. The text below is preserved as historical context.

**Status (original):** Proposal only. The PRD's M4+ milestone description defers the firm decision until M3 lands — what we learn from M2 (harness API ergonomics) and M3 (real-agent plumbing) will inform whether to revise this. Lock in formally at the M3 user-validation stop.

### Recommendation: one area per delegation

After M3, deliver the remaining ~91 catalog tests **one area at a time**, not one-test-at-a-time and not all-at-once.

| Strategy | Verdict |
|---|---|
| One test per delegation | Too much orchestrator overhead; ~91 round-trips for negligible per-test isolation gain. |
| **One area per delegation (~5–18 tests each) — recommended** | Sweet spot. Coder builds area context once; tests in the same `tests/e2e_*.rs` file share fixtures and harness helpers; reviewer + auditor each get a coherent chunk. |
| One sub-area at a time | Reasonable as a sub-strategy for the two largest areas (dashboard panes, prompts); split those by sub-area inside one area's batch. |
| All ~91 at once | Bad. Decision 25 mandates a stop after every Discovered Issue surfaces, so a single giant batch can't actually run to completion — it would halt mid-flight anyway. |

### Why area-grouping fits the existing decisions

- **Decision 7's file layout** (`tests/e2e_pane_lifecycle.rs`, `tests/e2e_focus_navigation.rs`, etc.) already mirrors catalog areas — one delegation per file is a natural unit.
- **Decision 11 + Decision 25** require stopping after Discovered Issues surface. Per-area batches let the orchestrator collect a clean Discovered Issues list per area before surfacing to the user; smaller stops are easier to act on than per-test stops.
- **Fixtures (Decision 12)** tend to be area-scoped — one prompt fixture, one orchestration config — and area-grouping amortizes fixture work.

### Proposed sequencing (~12 areas after M2 + M3)

1. **Validate harness API with simple snapshots first.** `status/badge` (L1 snapshots), `status/transition` (L2 synthetic events). Mechanically simple; catches harness ergonomics issues early.
2. **Exercise distinct harness surfaces.** `prompt/permission`, `prompt/quit`, `tabs/navigation`, `lifecycle/start`, `lifecycle/stop`, `lifecycle/restart`.
3. **High-blast-radius areas.** `embed/attach`, `hooks/delivery` + `hooks/install`, `session/restore`. Most likely to surface Decision 25 Discovered Issues — front-load them so the deck-bug tail is found early.
4. **Resize + error paths.** `resize/sigwinch`, `resize/layout`, `error/socket`, `error/config`, `error/agent-spawn`. Historically high signal-to-noise for deck bugs.
5. **Orchestration last.** `orchestration/delegate`. Most novel surface; benefits from the harness being mature.

After each area delegation: reviewer + auditor in parallel → resolve agreed findings → `/prd-update-progress` → Decision 25 stop if anything surfaced. PR cadence (one PR per area vs. rolled-up batches) is the next decision after this one, picked at M4+ kickoff with cost data from M2 + M3 in hand.

## M1: Existing-Test Audit

Per Decision 10 — pure-data unit tests stay live in `src/*/mod tests`; everything else moved to `./tmp/legacy-tests/` (gitignored; git history preserves originals via the rename detection on the staged delete).

**Carve-out result:** 296 pure-data unit tests kept across 22 `src/*` files; 25 `tests/*.rs` files plus `tests/common/` moved wholesale; 20 `src/*/mod tests` blocks moved (9 whole-block moves where every test in the file was non-pure-data, 11 partial splits where pure-data tests stay live and the remaining tests move).

**Note (2026-06-06):** the `cfg(test)` helpers in `src/config.rs` (`STATE_DIR_ENV_LOCK`, `CONFIG_GEN_STATE_ENV_LOCK`, `ConfigGenStateEnvGuard`) were re-introduced via `c479cb4` (merge from `main`) because main's resurrected `src/ui.rs` tests reference them. The audit's "deleted-helpers" framing in this section is partially stale as a result. See di-003.

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

## M2: Implementation Notes

Recorded at the M2 user-validation handoff so the user has the harness shape and the open questions in one place.

### Decision 16 re-check verdict — keep the in-house path

Default per Decision 16 is in-house on `portable-pty + vt100`. A single re-check happens at M2 kickoff; *all four* listed criteria must hold for ratatui-testlib to displace it.

| Criterion | Verdict | Notes |
|---|---|---|
| 1. Cross-platform support landed and tested | Not met | No widely-published library by the name `ratatui-testlib` exists in the ecosystem at M2 kickoff. The closest match is ratatui's built-in `TestBackend` — already used by the L1 slice — but it is an in-process buffer, not a PTY-driven L2 substrate. |
| 2. ≥200 stars + multiple maintainers | Not met | No discoverable repository. |
| 3. Released past 1.0 | Not met | No discoverable release. |
| 4. Covers our use cases | Not met | No published L2 library covers synthetic-event injection through the daemon's hook socket plus catalog-ID linkage as a single substrate. |

Conclusion: keep the in-house harness on `portable-pty 0.8` + `vt100 0.16` (both already production deps per Decision 1). The annotation surface for the linkage check is satisfied by the new `xtask/spec` proc-macro, not by a third-party library. Per Decision 16, no further re-check happens unless a future v1.0 + cross-platform release explicitly motivates a reopen.

### Seed catalog entries shipped

- **L1** — `dashboard/pane/004` (`tests/render_dashboard.rs::pane_004_card_title_row`). Renders a single Working session via `dot_agent_deck::ui::render_card_to_buffer` and snapshots the resulting `ratatui::buffer::Buffer` with `insta` (file snapshot at `tests/snapshots/render_dashboard__pane_004_card_title_row.snap`).
- **L2 synthetic** — `hooks/delivery/001` (`tests/e2e_hook_delivery.rs::delivery_001_session_start_creates_card`). Launches the production binary in a `portable-pty` PTY against the `minimal` fixture, writes a Claude Code `SessionStart` payload to the per-test hook socket, and waits for the rendered grid to contain the session token.

### New conventions added by M2

- `tests/common/mod.rs` is the shared L2 harness — `TuiDeck::launch_with_fixture("minimal")`, `TuiDeck::builder()` (with `.with_env(k, v)`, `.with_pty_size(cols, rows)`), `deck.resize(cols, rows)`, `deck.wait_until_quiescent()`, `deck.wait_for_string(s)`, `deck.hook_socket_path()` / `.attach_socket_path()`, plus a `write_hook_line` helper. Internals: portable-pty PTY pair, a reader thread feeding `vt100::Parser`, per-test tempdir + redirected `HOME`, pinned env per Decision 20, in-memory CastEvent ring buffer (asciinema v2 inline encoder). On `Drop` the harness dumps `target/test-recordings/<test-name>/{final-grid.txt, final-grid.svg, full-stream.cast, fixture.toml}` if the test panicked OR `DOT_AGENT_DECK_RECORD=1` is set.
- `xtask/` is now a Cargo workspace with two members: `xtask/linkage-check` (binary, `cargo xtask linkage-check`) and `xtask/spec` (proc-macro for the `#[spec(...)]` attribute). The root `Cargo.toml` gains a `[workspace]` block; `spec` is added as a `dot-agent-deck` dev-dep.
- `xtask/linkage-check/m2.allowlist` seeds 111 catalog IDs that are exempt from "must have a test" at M2 time. M4+ ticks entries off as it lands tests.
- `.cargo/config.toml` aliases `test-fast`, `test-e2e`, and `xtask`.
- `.config/nextest.toml` defines `profile.default` (retries=0, fail-fast=false, slow-timeout 60s/3, junit path) and `profile.e2e` (slow-timeout 120s/2); a nextest setup script wipes `target/test-recordings/` at the start of every `cargo test-e2e` per Decision 28.
- `bacon.toml` lives at repo root with the four jobs (test-fast / test-e2e / clippy / fmt) from Decision 14.
- `tests/fixtures/minimal/.dot-agent-deck.toml` is the seed fixture per Decision 12.
- `insta` is pinned via `=1.47.2` (Decision 22). The `e2e` feature is added to `Cargo.toml` and every `tests/e2e_*.rs` file opens with `#![cfg(feature = "e2e")]` (Decision 6).

### Second-opinion flags for the M2 stop

1. **`wait_until_quiescent` does not settle on the empty dashboard.** The production TUI emits periodic refresh ticks at idle, so a 50ms quiet window is never observed — `wait_until_quiescent()` panics with the 10s ceiling. The L2 seed test sidesteps this by using `wait_for_string` (which the catalog explicitly endorses for "loose substring matches"), but the PRD calls quiescence the *primary* wait. Two options to surface at the stop: (a) raise `QUIESCENT_IDLE_MS` to a value that beats the refresh cadence (the PRD's note says the 50ms default is tunable), or (b) suppress the refresh tick in test mode via a deck-side env-var or capped tick rate. I lean toward (a) but defer to user judgment — Decision 21 explicitly leaves the constant as tunable.
2. **`#[spec(...)]` requires a proc-macro crate.** Decision 7 specifies the annotation form `#[spec("...")]`. Rust rejects unknown attribute macros at compile time, so a proc-macro crate (`xtask/spec`) was added to define it as a no-op. That's three workspace members (main + linkage-check + spec) where the PRD only described one xtask. Surface as informational — the alternative was less consistent (free-standing const markers + linkage-check scanning a different shape).
3. **L1 cast format.** The worker-task asked for `full-stream.cast` recordings of *both* M2 tests. The L1 test renders in-process via `TestBackend` and has no PTY stream, so a cast doesn't fit; I committed the `insta` snapshot file instead and documented the choice in `.dot-agent-deck/m2-recordings/README.md`. Flagging in case the user expected a synthetic cast.
4. **M4+ Buildout Strategy** lives at the top of the milestone block (added in commit `8cac6e5` before this task). M2's work doesn't touch it; surfacing here for traceability.

## M3: Implementation Notes

Recorded at the M3 user-validation handoff so the milestone state and the open question for the OpenCode side are in one place.

### Catalog entries shipped

- **`chain-smoke/claude/001`** — `tests/e2e_chain_smoke_claude.rs::claude_001_thinking_working_idle`. Spawns the real `claude` CLI under a per-test redirected HOME (host credentials imported by the new `with_imported_claude_credentials()` harness builder), runs `claude -p "<prompt>" --model claude-haiku-4-5-20251001 --allowedTools Bash`, and asserts the deck's dashboard card status traverses Thinking → Working → Idle with the `Bash` tool name visible during Working. Test runs in ≈5 s; observed cost is one Haiku-4.5 invocation at <500 input + <200 output tokens — well under the <$0.05 Decision-23 bound.
- **`chain-smoke/opencode/001`** — *not shipping in M3.* See the deck-bug note below.

### Fixtures

- `tests/fixtures/chain-smoke-claude/.dot-agent-deck.toml` — empty marker (no modes / orchestrations). The agent invocation is driven by a generated `session.toml` written into the per-test tempdir.
- `tests/fixtures/chain-smoke-opencode/.dot-agent-deck.toml` — same shape, reserved for the OpenCode test once the deck plugin path is fixed.

### Harness extension

`tests/common/mod.rs` gains three test-facing pieces:

1. `TuiDeckBuilder::with_continue_session(name, command)` stages a `session.toml` in the per-test tempdir, sets `DOT_AGENT_DECK_SESSION` to that path, and adds `--continue` to the deck argv so a single chain-smoke pane auto-opens on launch. The pane's `dir` is the tempdir itself so the agent has a real cwd (the deck's restore path skips panes whose `dir` doesn't exist).
2. `TuiDeckBuilder::with_imported_claude_credentials()` copies `~/.claude/.credentials.json` (mode 0o600 preserved), `~/.claude/settings.json` (with `hooks` stripped — the deck installs its own pointing at the per-test socket), and `~/.claude/plugins/` if present. All `fs::copy` per the M2.1 auditor rule.
3. `TuiDeckBuilder::with_imported_opencode_credentials()` copies `~/.local/share/opencode/auth.json` + `~/.config/opencode/opencode.jsonc`. The host's `~/.config/opencode/plugin/dot-agent-deck/` is deliberately NOT copied — the deck reinstalls its own at launch.
4. `common::check_claude_available()` / `check_opencode_available()` + the new `skip_unless!(...)` macro implement Decision 26's runtime-skip exception (missing CLI or credentials → clean SKIP with a stable user-facing message). No silent model fallback per Decision 8.

### Open issue surfaced — OpenCode plugin discovery

The OpenCode test was written, run, and **failed because the deck's plugin install path is incompatible with current OpenCode**. Captured here so the M3 stop has the context:

- `src/opencode_manage.rs::auto_install` writes the deck's JS hook plugin to `<config>/plugin/dot-agent-deck/index.js` (`~/.config/opencode/plugin/dot-agent-deck/` on this host).
- OpenCode 1.15.10's plugin loader, observed via `opencode run … --print-logs --log-level DEBUG`, loads *only* internal plugins (`service=plugin name=EG loading internal plugin` and friends) and never attempts to load any external `<config>/plugin/<name>/index.js`. The deck's plugin file is on disk and never invoked.
- Consequence: the deck receives no `session.created` / `tool.execute.before` events from OpenCode, the card stays in the "No agent" placeholder forever, and the catalog's Thinking → Working → Idle traversal can't be observed. Confirmed by running `opencode run` directly against a manually-staged HOME mirror that included the deck's plugin and a netcat listener on `DOT_AGENT_DECK_SOCKET` — zero bytes hit the socket.
- Per Decision 11 (failing test = deck bug, not a test bug), I did not commit a failing OpenCode test or `#[ignore]` it (Decision 26 forbids the latter). The catalog entry `chain-smoke/opencode/001` is parked back on `xtask/linkage-check/m2.allowlist` with an inline comment naming the blocker. The harness builder methods + skip-check helper are ready for the OpenCode test to drop in once the deck plugin install is updated to match OpenCode 1.x's discovery (likely an `opencode plugin`-style npm-package install or a `package.json` declaration in `~/.config/opencode/`).

### Second-opinion flags for the M3 stop

1. **OpenCode deck-side fix.** Recommend a follow-up PRD or in-PRD-77 issue to align `src/opencode_manage.rs::auto_install` with OpenCode 1.x's plugin loader. Once that lands, the M3 chain-smoke test will need to be re-introduced — the harness side is already done.
2. **Snapshot fixture's `last_activity` was time-pinned to a fixed past instant.** This worked for the M2 commit but the snapshot drifts as the date rolls (the rendered `Last: Xs ago` is `Utc::now() - last_activity`). I switched `tests/render_dashboard.rs::working_session_fixture` to use `Utc::now()` so elapsed always reads `0s ago`. Not a catalog-level change; documenting so future contributors don't reintroduce the pin.

## M4: Implementation Notes

Recorded at the M4 user-validation handoff so the validation reviewer has the generator shape, the method-template map, and the open notes in one place.

### Deliverables shipped

- **`xtask/docs/`** — new workspace member (library + binary). Library is the single source of truth for both `cargo xtask docs --tests` and CI's rule 7 byte-identity check; binary is the developer-facing entrypoint. Decision 30's full machinery sits here.
- **`cargo xtask docs --tests`** wired through the existing `xtask-linkage-check` binary, which is now a subcommand multiplexer (first argv slot selects `docs` vs `linkage-check`, default to the latter for back-compat). Keeps the existing `cargo xtask` alias untouched.
- **Linkage-check rule 7** lives in `xtask/linkage-check/src/main.rs` alongside the six existing rules; calls `xtask_docs::check_in_sync` and surfaces missing `/// Scenario:` doc comments + drifted `.md` files. Success line bumped to "… 7 rules".
- **Harness regen-on-record hook** in `tests/common/mod.rs` — when the L2 harness's Drop dumps recordings (panic OR `DOT_AGENT_DECK_RECORD=1`), it also regenerates the paired `.md` for the running test via `xtask_docs::generate_all`. Best-effort: errors log to stderr but don't poison the test result; CI's rule 7 is the load-bearing enforcement.
- **CLAUDE.md rule 7** + **CONTRIBUTING.md "how to add a new test"** updated to call out the Scenario doc comment + `cargo xtask docs --tests` regen step.
- **Retrofit:** the three existing `#[spec]` tests (`dashboard/pane/004`, `hooks/delivery/001`, `chain-smoke/claude/001`) gain Scenario doc comments + paired `.md` files under `.dot-agent-deck/m2-recordings/` (L1 + L2 synthetic) and `.dot-agent-deck/m3-recordings/` (chain-smoke).

### Method-name → step-template map

The maintenance surface Decision 30 calls out. Initial entries (all in `xtask/docs/src/lib.rs::step_for_method` + `step_for_macro` + `step_for_free_call`):

| Source | Method / macro | Step template |
|---|---|---|
| method | `launch_with_fixture(F)` | `Launch the deck with fixture F` |
| method | `try_launch_with_fixture(F)` | `Launch the deck with fixture F (fallible variant)` |
| method | `wait_for_string(S)` | `Wait for S to appear on screen` |
| method | `wait_until_quiescent()` | `Wait until the deck stops emitting output` |
| method | `with_imported_claude_credentials()` | `Import Claude credentials into the test HOME` |
| method | `with_imported_opencode_credentials()` | `Import OpenCode credentials into the test HOME` |
| method | `with_continue_session(N, C)` | `Stage a saved session N running C` |
| method | `with_env(K, V)` | `Override env K=V` |
| method | `with_pty_size(C, R)` | `Set the PTY to C×R` |
| method | `resize(C, R)` | `Resize the PTY to C×R` |
| method | `builder` / `hook_socket_path` / `attach_socket_path` / `snapshot_grid` | _(skipped — plumbing)_ |
| free | `write_hook_line(_, P)` | `Write P to the hook socket` |
| free | `render_card_to_buffer(...)` | `Render the session card into a ratatui::TestBackend buffer` |
| free | `Some` / `None` / `Ok` / `Err` / `String` / `Vec` / `PathBuf` / `Default` / `format` / `builder` | _(skipped — language / plumbing)_ |
| macro | `skip_unless!(check_claude_available())` | `Skip unless Claude Code CLI is available` |
| macro | `skip_unless!(check_opencode_available())` | `Skip unless OpenCode CLI is available` |
| macro | `assert_snapshot!(buf)` | `Snapshot the rendered buffer (insta)` |
| method (unknown) | _any other_ | `Call: name(...)` fallback |
| free (unknown) | _any other not in skip list_ | `Call: name(...)` fallback |

Adding a new harness method later is a one-line edit in the corresponding match arm.

### Cross-xtask code-sharing decisions

- The docs generator's catalog parser is *richer* than linkage-check's existing ID-only one (extracts the full `Layer/Agent/Asserts/Does not assert/Platform coverage/Cost note` body), so I kept them separate rather than extracting a shared `xtask-catalog` crate. The linkage-check parser stays simple; the docs parser owns the richer model. If a third xtask needs the rich shape, that's the trigger to extract — not now.
- The `xtask-linkage-check` binary still owns the `cargo xtask` alias because that's the simplest path. Renaming to `xtask-cli` (umbrella) is a nice-to-have for after PRD #77 closes.

### Recording-dir mapping

The current pin-by-area heuristic in `xtask_docs::resolve_output_path`:
- `dashboard/* + hooks/*` → `.dot-agent-deck/m2-recordings/`
- `chain-smoke/*` → `.dot-agent-deck/m3-recordings/`
- anything else → `.dot-agent-deck/unmapped-recordings/`

Future PRDs that land catalog tests under new areas pin their own milestone dir by extending this match arm in a one-line PR (or by simply adding the `unmapped-recordings/` rename to whatever the PRD scopes).

> **Superseded by M4.3 (2026-06-06).** The milestone-subdir mapping above was removed. Every test now writes to `.dot-agent-deck/recordings/<test-fn-name>/test.md` regardless of catalog area; future PRDs adding tests don't need to think about milestone naming.

### M4.3 status note — `.dot-agent-deck/` is dev-time state

Locked in 2026-06-06 after the M4 review. The earlier M2/M3 convention committed paired recordings under `.dot-agent-deck/m2-recordings/` and `.dot-agent-deck/m3-recordings/`; M4.3 moves the entire `.dot-agent-deck/` tree to gitignored developer-machine state (like `cargo doc` output) and flattens the layout.

- **Layout:** `.dot-agent-deck/recordings/<test-fn-name>/{full-stream.cast,test.md,final-grid.txt,final-grid.svg,fixture.toml}`. No milestone-prefixed subdirs.
- **Generator output:** `cargo xtask docs --tests` writes `test.md` directly under the per-test dir. The catalog-area-to-milestone routing in `xtask_docs::resolve_output_path` is gone — one path for all tests.
- **Harness cast destination:** `tests/common/mod.rs::dump_recordings` writes to the same per-test dir, via tempfile + rename (atomic on Unix). The M2.1 per-run subdir (`<run-id>/`) was dropped — concurrent `cargo test-e2e` invocations on one checkout aren't a real-world workflow, and per-test atomic write means a re-run replaces prior artifacts in place. Last-writer-wins is fine for local debugging.
- **`.gitignore`:** blanket-ignores `.dot-agent-deck/`. The earlier `!.dot-agent-deck/m2-recordings/` negations were removed and the previously-committed `m2-recordings/` + `m3-recordings/` directories were `git rm`-ed in the same commit. They remain reachable in git history; no history scrub.
- **Linkage-check rule 7:** simplified. The byte-identity diff against on-disk `.md` is gone — with `.md` gitignored, the on-disk file doesn't exist on a fresh clone, so a diff is structurally meaningless. Rule 7 now asserts (a) every `#[spec(...)]` test carries a `/// Scenario:` doc comment with a body and (b) `cargo xtask docs --tests` exits 0 against the current source + catalog. The convention is enforced by Scenario-presence + the generator succeeding.
- **Why:** the asymmetric "this one dir under a gitignored prefix is committed" exception was confusing; milestone-prefixed subdirs didn't scale to per-PRD additions; reading the Scenario doc comment in the test source on GitHub is the actual external-reader surface, and the `.md` is a local convenience.

### M4.4 status note — `tester` role, `cargo xtask list-tests`, and the post-main-merge legacy shim

Locked in 2026-06-06 alongside Decision 31. Three threads land in one batch:

1. **`tester` role.** New entry in `.dot-agent-deck.toml`'s orchestration roles with `start = false` and a binding `prompt_template` that pins the TDD chain (RED → coder → GREEN), the bias order (extend > modify > new), and the Scenario-doc-comment requirement. `devbox.json` gains `"agent-tester": ["claude --model opus"]`. The orchestrator's own `prompt_template` is updated to describe the TDD chain for behavior-changing work AND to require running `cargo xtask list-tests` before delegating release. The dynamically-built `.dot-agent-deck/orchestrator-context.md` (regenerated at TUI launch from `build_orchestrator_context` in `src/ui.rs`) picks up both halves automatically.

2. **`cargo xtask list-tests`.** New subcommand added to the existing `xtask-linkage-check` multiplexer (cleaner than a separate workspace member — the multiplexer already routes `linkage-check` + `docs --tests`, this is the fourth arm). Implementation lives in `xtask/linkage-check/src/list_tests.rs`. Shells to `git merge-base HEAD origin/main` + `git show <ref>:<path>` + `git ls-tree -r --name-only <ref> tests` for the source diff; reuses `xtask_docs::parse_catalog` (via a tempfile staged from each ref's PRD body) for the catalog prose diff; reads `xtask/linkage-check/m2.allowlist` at both refs for the allowlist diff. Emits four Markdown sections (Created / Modified / Catalog prose / Allowlist), each rendering `_(none)_` when empty so the report's structure is stable. 8 unit tests cover the pure-data delta helpers + the markdown renderer + the syn-based source extractor; the git-shelling path is exercised end-to-end via the manual `cargo xtask list-tests` invocation rather than a unit test (synthetic git fixtures are flaky and the surface is thin).

3. **Legacy compatibility shim in `tests/common/mod.rs`.** The post-main merge (commit `c479cb4`) brought four test files back from main that weren't on this branch since the M1 audit. Those tests (`tests/snapshot_replay_dims.rs`, `tests/rehydration.rs`, `tests/daemon_protocol.rs`, `tests/spawn_time_role_prompt_submit_after_session_start.rs`) import `common::init_test_env`, `common::lock_dir_path`, `common::race_safe_tempdir`, and a `LOCK_DIR_GUARD` static — helpers that lived in the original `tests/common/mod.rs` before M1 moved it to `tmp/legacy-tests/`. M2 replaced that file wholesale with the L2 harness. The shim restores the legacy helpers (plus their `OnceLock<TempDir>` backing) as a separate section in our `tests/common/mod.rs`, behind a one-paragraph comment explaining the carry-forward. The two surfaces are orthogonal (legacy = process-global lock dir; harness = per-test tempdir + PTY) and naming-disjoint, so they coexist cleanly. Tracked as di-002 for the eventual refactor pass that brings the four legacy test files onto the L2 harness.

### Second-opinion flags for the M4 stop

1. **L1 doc steps include `Call: …` lines for author helpers.** Tests like `pane_004_card_title_row` call `working_session_fixture()` and `resolve_palette(Dark)` — both author-local helpers, not harness API. The generator surfaces them via the `Call:` fallback for visibility. That's per spec, but the resulting L1 `.md` "Steps" section reads a little noisy. The test author can either (a) inline those helper calls, (b) extend the skip list, or (c) live with the noise. M4 ships as-is; future polish in M5+.
2. **The `xtask-linkage-check` binary is the subcommand multiplexer.** The package name is a slight misnomer now that it also runs `cargo xtask docs --tests`. Rename to `xtask-cli` (or similar umbrella) is suggested for a post-PRD-77 polish PR — but the current shape works and keeps the alias stable, so deferring.
3. **`generate_for_spec` re-parses all tests for one regen.** The harness regen-on-record hook calls `generate_all` and filters by fn name. With ~3 tests today this is microseconds; if the test count grows to hundreds, a per-file `parse_one(path, spec_id)` shortcut would be worth adding. Flag for M5+ when the catalog actually fills out.

## Test Case Catalog

This is the authoritative list of test cases the harness must cover. IDs are stable per Decision 7; tests reference them via `#[spec("…")]` annotations once the harness exists in M2. Coverage is enumerated from the code as it ships today (Decision 27 — "code is authoritative"); documented behaviors with no catalog entry are listed as deliberate skips at the end of this section.

Platform coverage column shorthand: **mac+linux** = macOS and Linux (Windows once the harness's Windows path is ready per Decision 4); **mac+linux+windows** = portable from day one.

### Dashboard panes

#### dashboard/pane

##### dashboard/pane/001 — A pane appears in the next free layout region when an agent is started.
- **Layer:** L2 (PTY end-to-end).
- **Agent:** none (synthetic — `StartAgent` over the daemon protocol with a `sleep infinity` stub).
- **Asserts:** rendered card grid shows one new card; the corresponding pane region is visible on the right column.
- **Does not assert:** card text content beyond the display name, color of the status badge, exact pixel coordinates.
- **Platform coverage:** mac+linux.

##### dashboard/pane/002 — Closing a pane via `Ctrl+w` removes its card from the dashboard.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** card count decreases by one; the focused card index stays within bounds.
- **Does not assert:** which card receives focus next (`dashboard/selection/*` covers selection-after-close).
- **Platform coverage:** mac+linux.

##### dashboard/pane/003 — The dashboard pane (tab 0) is never closable.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** `Ctrl+w` from the dashboard tab with no card selected is a no-op (no panic, dashboard still rendered, tab count unchanged).
- **Does not assert:** any status-line text.
- **Platform coverage:** mac+linux.

##### dashboard/pane/004 — Card title row carries card number, display name, and a status badge.
- **Layer:** L1 (ratatui `TestBackend` + `insta`).
- **Agent:** none.
- **Asserts:** rendered card buffer matches the committed snapshot for a single Working session in the Normal density.
- **Does not assert:** pane content; this is a card layout snapshot only.
- **Platform coverage:** mac+linux+windows.

##### dashboard/pane/005 — Dashboard card highlight follows the stable `selected_session_id`, not card 0 (PRD #83 M3).
- **Layer:** L1 (ratatui `TestBackend` + `insta`).
- **Agent:** none.
- **Asserts:** with three session cards and a `Tab::Dashboard` whose `selected_session_id` points at the second card (`sess-beta`), `ui::sync_and_derive_selection` derives index 1 (not 0); the rendered snapshot shows the `▸` selection marker and highlighted border on the second card while the first and third stay unselected.
- **Does not assert:** keyboard-driven selection movement (`dashboard/selection/*`); absolute-time clocks (`Last:` is rendered against a fixed test clock).
- **Platform coverage:** mac+linux+windows.

##### dashboard/pane/006 — Card row shows `Dir:` (working directory basename), `Last:` (elapsed since last activity), `Tools:` (tool count), `Prmt:` (latest user prompts).
- **Layer:** L1.
- **Agent:** none.
- **Asserts:** rendered card snapshot has all four labels in order with the supplied fixture data.
- **Does not assert:** absolute-time clocks (`Last:` is rendered against a fixed test clock).
- **Platform coverage:** mac+linux+windows.

#### dashboard/density

##### dashboard/density/001 — Spacious density shows up to 3 prompts and 3 tool calls per card.
- **Layer:** L1.
- **Agent:** none.
- **Asserts:** snapshot rendered with one card in a wide viewport carries the 3+3 capacity.
- **Does not assert:** behavior on Compact / Normal (covered by separate entries).
- **Platform coverage:** mac+linux+windows.

##### dashboard/density/002 — Normal density shows 1 prompt and up to 3 tool calls per card.
- **Layer:** L1.
- **Agent:** none.
- **Asserts:** snapshot rendered with a card count that lands in the Normal-density tier.
- **Does not assert:** the exact boundary card count between tiers — picked by the layout helper.
- **Platform coverage:** mac+linux+windows.

##### dashboard/density/003 — Compact density shows 1 prompt and 1 tool call per card.
- **Layer:** L1.
- **Agent:** none.
- **Asserts:** snapshot rendered with a card count that lands in Compact density.
- **Does not assert:** card visual style beyond the rendered character buffer.
- **Platform coverage:** mac+linux+windows.

#### dashboard/selection

##### dashboard/selection/001 — `j` / `Down` selects next card; wraps at end.
- **Layer:** L2.
- **Agent:** none (3 synthetic panes).
- **Asserts:** selection indicator moves through cards in order and wraps to the first card after the last.
- **Does not assert:** how the selection indicator is drawn beyond "present at card N".
- **Platform coverage:** mac+linux.

##### dashboard/selection/002 — `k` / `Up` selects previous card; wraps at start.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** selection moves backwards and wraps from card 0 to the last card.
- **Does not assert:** rendering of inactive cards.
- **Platform coverage:** mac+linux.

##### dashboard/selection/003 — `1`–`9` jumps to card N and focuses its pane.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** keystroke `3` (with 3+ cards) selects card index 2 and the corresponding agent pane gains the focus border.
- **Does not assert:** what `0` or digits past the card count do (kept open until catalogued).
- **Platform coverage:** mac+linux.

##### dashboard/selection/004 — `Esc` clears an active filter.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** with the filter dialog populated, pressing `Esc` returns the visible cards to the unfiltered set.
- **Does not assert:** filter dialog dismissal animation.
- **Platform coverage:** mac+linux.

#### dashboard/filter

##### dashboard/filter/001 — `/` opens the filter input; typing narrows visible cards by display-name substring.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** after typing two characters that match one of three cards, only that card is rendered.
- **Does not assert:** case-sensitivity flag (covered separately when committed).
- **Platform coverage:** mac+linux.

##### dashboard/filter/002 — `Enter` accepts the filter and leaves the dashboard in the filtered view.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** filter dialog closes; the filtered card list remains; `Esc` then clears it.
- **Does not assert:** subsequent re-open behavior of the filter dialog with prior input restored — not yet specified.
- **Platform coverage:** mac+linux.

#### dashboard/rename

##### dashboard/rename/001 — `r` on the selected card opens a rename input pre-filled with the current name.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** rename input appears with the current display name shown; pressing `Esc` cancels without persisting.
- **Does not assert:** which keystrokes are valid in the input box (covered by `pane/rename/*` validators in the lib pure-data tier).
- **Platform coverage:** mac+linux.

##### dashboard/rename/002 — Confirming a valid new name updates the card title and is mirrored via the daemon `SetAgentLabel` request.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** the card title row shows the new name; a subsequent `list_agents` from a parallel daemon client returns the same `display_name`.
- **Does not assert:** persistence across daemon restart (covered by `session/restore/*`).
- **Platform coverage:** mac+linux.

#### dashboard/help

##### dashboard/help/001 — `?` toggles the help overlay; pressing `?`, `Esc`, or `q` dismisses it.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** the overlay region is rendered on `?` and removed on dismissal.
- **Does not assert:** the exact list of keys shown in the overlay (compared against a snapshot under `dashboard/help/002`).
- **Platform coverage:** mac+linux.

##### dashboard/help/002 — Help overlay content matches the committed snapshot.
- **Layer:** L1.
- **Agent:** none.
- **Asserts:** `insta` file snapshot of the overlay buffer.
- **Does not assert:** dynamic content (none today).
- **Platform coverage:** mac+linux+windows.

#### dashboard/config-gen

##### dashboard/config-gen/001 — `g` on a card opens the Generate Config dialog with options Yes / No / Never.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** dialog region appears; arrow keys move between Yes / No / Never; `Enter` on No dismisses without side effects.
- **Does not assert:** what Yes injects into the agent (covered by `orchestration/delegate/*` for delegate-driven prompt injection, and elsewhere if a non-orchestration path emerges).
- **Platform coverage:** mac+linux.

##### dashboard/config-gen/002 — Picking Never adds the cwd to the suppression list and the prompt does not re-open for that directory.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** after Never, re-opening the new-pane flow for the same cwd does not surface the auto-prompt.
- **Does not assert:** filesystem path of the suppression list (an implementation detail).
- **Platform coverage:** mac+linux.

### Statuses

#### status/transition

##### status/transition/001 — Session status transitions to Thinking on `UserPromptSubmit`.
- **Layer:** L2.
- **Agent:** none (synthetic hook event written to the per-test hook socket).
- **Asserts:** card status badge reads Thinking after the hook delivery.
- **Does not assert:** the previous status (covered by predecessor tests).
- **Platform coverage:** mac+linux.

##### status/transition/002 — Session status transitions to Working on `PreToolUse`, carrying the tool name.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** badge reads Working; the card's tool row shows the tool's name (e.g. `Read`).
- **Does not assert:** tool-detail formatting beyond presence of the tool name.
- **Platform coverage:** mac+linux.

##### status/transition/003 — Session status transitions to Idle on `Stop`.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** card status reads Idle.
- **Does not assert:** flashing-dot animation cadence.
- **Platform coverage:** mac+linux.

##### status/transition/004 — Session status transitions to Error on a hook-reported error.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** badge reads Error.
- **Does not assert:** error text content (the hook payload is opaque).
- **Platform coverage:** mac+linux.

##### status/transition/005 — Session status transitions to WaitingForInput on `PermissionRequest`.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** badge reads WaitingForInput; the card surfaces a `y`/`n` affordance.
- **Does not assert:** tool-detail of the permission (covered under `prompt/permission/*`).
- **Platform coverage:** mac+linux.

##### status/transition/006 — Session status transitions to Compacting on `PreCompact`.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** badge reads Compacting.
- **Does not assert:** status reverts on `PostCompact` — covered by a follow-up entry.
- **Platform coverage:** mac+linux.

##### status/transition/007 — A `PreToolUse` arriving while WaitingForInput does not override the WaitingForInput badge.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** WaitingForInput sticks until the matching `PostToolUse` or permission resolution.
- **Does not assert:** other badges' precedence rules — covered separately as each is added.
- **Platform coverage:** mac+linux.

#### status/badge

##### status/badge/001 — Status badge color and label render per palette for each session status.
- **Layer:** L1.
- **Agent:** none.
- **Asserts:** snapshot per status enum value renders the expected label and palette entry.
- **Does not assert:** the dot animation frame.
- **Platform coverage:** mac+linux+windows.

### Prompts

#### prompt/permission

##### prompt/permission/001 — `y` approves the pending permission request and clears the WaitingForInput status.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** badge transitions away from WaitingForInput; the daemon receives the approval over its protocol channel.
- **Does not assert:** how the daemon routes the approval to the agent process (out-of-scope at the TUI layer).
- **Platform coverage:** mac+linux.

##### prompt/permission/002 — `n` denies the pending permission request.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** badge transitions away from WaitingForInput; daemon receives a denial.
- **Does not assert:** retry behavior.
- **Platform coverage:** mac+linux.

##### prompt/permission/003 — `y`/`n` are no-ops when no session is waiting for input.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** keystroke produces no protocol traffic and leaves card status unchanged.
- **Does not assert:** any beep or visual ack.
- **Platform coverage:** mac+linux.

#### prompt/pane-input

##### prompt/pane-input/001 — `Enter` on a focused side pane enters PaneInput mode.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** the mode line / focus indicator updates to indicate PaneInput mode; a subsequent letter keystroke is forwarded to the side pane's PTY.
- **Does not assert:** the side pane's command output (depends on the fixture shell).
- **Platform coverage:** mac+linux.

##### prompt/pane-input/002 — `Ctrl+d` from PaneInput returns to Normal mode without writing the keystroke to the PTY.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** mode flips back to Normal; the PTY's parsed grid does not gain a stray `^D`.
- **Does not assert:** any toast / status-line message.
- **Platform coverage:** mac+linux.

##### prompt/pane-input/003 — `Ctrl+c` in PaneInput delivers SIGINT (0x03) to the pane's process.
- **Layer:** L2.
- **Agent:** none (fixture: `sh -c 'trap "echo INT" INT; sleep 5'`).
- **Asserts:** the pane PTY shows `INT` after the keystroke, confirming the signal was delivered.
- **Does not assert:** signal handling in the dashboard tab itself (covered by `dashboard/quit/*`).
- **Platform coverage:** mac+linux.

#### prompt/quit

##### prompt/quit/001 — `Ctrl+c` from command mode opens the quit confirmation dialog with three options: **Detach** (default), **Stop**, **Cancel**.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** dialog appears; option list reads `Detach / Stop / Cancel` in that order; the selection cursor starts on Detach (index 0).
- **Does not assert:** local-vs-remote rendering — the dialog is identical (`Detach` is the daemon-attach-aware option in both cases since every pane is daemon-backed).
- **Platform coverage:** mac+linux.

##### prompt/quit/002 — `Ctrl+c` again while the quit dialog is open exits the TUI without sending an explicit `KIND_DETACH` frame.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** the harness's spawned binary exits; daemon and managed agents stay alive; no detach frame was observed on the daemon socket.
- **Does not assert:** daemon's eventual idle exit (covered by `lifecycle/daemon-idle/*`).
- **Platform coverage:** mac+linux.

##### prompt/quit/003 — Selecting **Detach** from the quit dialog sends an explicit `KIND_DETACH` frame to the daemon, then exits.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** dialog yields a `KIND_DETACH` frame on the daemon's attach socket before the TUI exits; managed agents stay alive afterwards.
- **Does not assert:** any difference between local and remote daemons — the frame and exit behavior are identical; the observable difference (daemon-side log line) is daemon-side, not deck-side.
- **Platform coverage:** mac+linux.

##### prompt/quit/004 — Selecting **Stop** with managed agents alive opens a secondary confirm dialog (`No` / `Yes`, `No` default) naming the agent count.
- **Layer:** L2.
- **Agent:** none (synthetic — one running stub agent).
- **Asserts:** the secondary dialog appears with header containing `1 managed agent will be terminated`; options read `No / Yes` in that order with `No` selected; pressing `No` returns to the primary `Detach / Stop / Cancel` dialog; pressing `Yes` performs StopAndQuit (daemon and agents terminate).
- **Does not assert:** the singular/plural agent-count wording (loose substring match on the count).
- **Platform coverage:** mac+linux.

##### prompt/quit/005 — Selecting **Stop** with zero managed agents skips the secondary confirm and terminates the daemon directly.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** no secondary dialog appears; the TUI exits and the daemon socket disappears within the grace window.
- **Does not assert:** SIGTERM vs SIGKILL escalation (covered by `lifecycle/stop/003`).
- **Platform coverage:** mac+linux.

#### prompt/dir-picker

##### prompt/dir-picker/001 — `Ctrl+n` opens the new-pane flow; the directory picker is the first step and lists the start directory's entries.
- **Layer:** L2.
- **Agent:** none (fixture with a small directory tree at the harness's redirected `HOME`).
- **Asserts:** the picker appears with the fixture's root entries rendered; the selection cursor starts on the first entry (`..` parent is visible but not selected).
- **Does not assert:** sort order beyond "directories before files" (covered if needed).
- **Platform coverage:** mac+linux.

##### prompt/dir-picker/002 — `j` / `Down` / `k` / `Up` cycle the selected directory; selection wraps end-to-end.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** selection cursor advances through entries; pressing `Up` on the first entry jumps to the last (and vice versa).
- **Does not assert:** rendering of inactive entries beyond presence.
- **Platform coverage:** mac+linux.

##### prompt/dir-picker/003 — `l` / `Right` / `Enter` descend into the selected directory; `h` / `Left` / `Backspace` ascend.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** after descending, the picker shows the child directory's contents; after ascending, it shows the parent's contents again.
- **Does not assert:** any breadcrumb / path rendering beyond directory contents.
- **Platform coverage:** mac+linux.

##### prompt/dir-picker/004 — `Space` confirms the current directory and advances to the new-pane form.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** the directory picker closes; the new-pane form appears with the chosen directory pre-filled.
- **Does not assert:** the form's default field values (covered by `prompt/new-pane/*`).
- **Platform coverage:** mac+linux.

##### prompt/dir-picker/005 — `/` opens filter mode; typing narrows directories case-insensitively; the `..` parent stays visible.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** filter accepts a substring; only matching directories remain; `..` is rendered regardless of filter.
- **Does not assert:** filter regex syntax (it is plain substring matching).
- **Platform coverage:** mac+linux.

##### prompt/dir-picker/006 — `Esc` clears the active filter; pressing `Esc` again closes the picker.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** first `Esc` empties the filter and restores the full directory list; second `Esc` returns control to the dashboard.
- **Does not assert:** filter input box visibility between key presses.
- **Platform coverage:** mac+linux.

##### prompt/dir-picker/007 — `q` cancels the picker and returns to the dashboard without spawning a pane.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** the picker closes; no new pane appears; daemon `list_agents` is unchanged.
- **Does not assert:** rendering of any toast / status-line message.
- **Platform coverage:** mac+linux.

#### prompt/new-pane

##### prompt/new-pane/001 — The new-pane form opens after the directory picker with three fields visible (Name, Command, Mode) and the initial focus on Name.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** the form renders all three field labels; the focus indicator is on the Name field; Mode is set to the default.
- **Does not assert:** the default command string (a configurable `default_command`).
- **Platform coverage:** mac+linux.

##### prompt/new-pane/002 — `Tab` and `Shift+Tab` cycle focus forward and backward between fields.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** `Tab` from Name moves focus to Command; another `Tab` moves to Mode; `Shift+Tab` from Mode moves back to Command; cycling wraps at both ends.
- **Does not assert:** which field accepts which input (text vs cycle).
- **Platform coverage:** mac+linux.

##### prompt/new-pane/003 — On the Mode field, `Left` / `Right` / `h` / `l` cycle through the available modes including the default and any project-defined modes / orchestrations.
- **Layer:** L2.
- **Agent:** none (fixture `.dot-agent-deck.toml` defines one mode and one orchestration).
- **Asserts:** cycling from the default shows the mode name, then the orchestration name, then wraps back; the rendered Mode field text follows the cycle.
- **Does not assert:** what happens to other fields while the Mode cycles (Command may be hidden when an orchestration is selected — covered by `prompt/new-pane/004`).
- **Platform coverage:** mac+linux.

##### prompt/new-pane/004 — Selecting an orchestration hides the Command field (each role's command is supplied by the config).
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** with the Mode cycled to an orchestration, the Command label is not rendered; cycling back to a non-orchestration Mode re-renders Command.
- **Does not assert:** what content `Command` had before being hidden (no data loss expected, but not pinned here).
- **Platform coverage:** mac+linux.

##### prompt/new-pane/005 — `Enter` submits the form; the resulting pane (or mode / orchestration tab) is created.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** after submit, a card / tab appears that matches the form inputs.
- **Does not assert:** post-submit focus location (covered by `lifecycle/start/*`).
- **Platform coverage:** mac+linux.

##### prompt/new-pane/006 — `Esc` cancels the form and returns to the dashboard.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** form closes; no new pane appears; daemon `list_agents` is unchanged.
- **Does not assert:** the dashboard's selection cursor location on return.
- **Platform coverage:** mac+linux.

### Focus / navigation

#### focus/dashboard

##### focus/dashboard/001 — From command mode, `j` / `k` cycle the selected card; `Enter` is a no-op on the dashboard tab (selection is the source of truth).
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** selection moves; pressing `Enter` does not switch tabs or open any dialog from a selected card.
- **Does not assert:** the broken `Enter`-to-jump behavior tracked in [#68](https://github.com/vfarcic/dot-agent-deck/issues/68); see deliberate skips.
- **Platform coverage:** mac+linux.

#### focus/mode-tab

##### focus/mode-tab/001 — `j` / `k` cycle focus through agent → side panes → agent on a mode tab.
- **Layer:** L2.
- **Agent:** none (two persistent side panes from a fixture mode).
- **Asserts:** the cyan focus border moves through panes in order and wraps.
- **Does not assert:** focus during PaneInput mode (PaneInput pins focus on the active pane).
- **Platform coverage:** mac+linux.

##### focus/mode-tab/002 — `Esc` from a focused side pane returns focus to the agent pane.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** focus indicator jumps to the agent pane region.
- **Does not assert:** focus persistence across tab switches.
- **Platform coverage:** mac+linux.

#### focus/orchestration

##### focus/orchestration/001 — `1`–`9` on an orchestration tab jumps to role pane N and focuses it.
- **Layer:** L2.
- **Agent:** none (orchestration fixture with stub role commands).
- **Asserts:** focused pane index matches the keystroke; the sidebar role-card highlight follows.
- **Does not assert:** what happens beyond the available role count.
- **Platform coverage:** mac+linux.

##### focus/orchestration/002 — Sidebar role cards reflect each role's live status (Thinking / Working / WaitingForInput / Idle / Error).
- **Layer:** L2.
- **Agent:** none (synthetic events targeting two roles).
- **Asserts:** distinct sidebar entries show distinct statuses after distinct hook deliveries.
- **Does not assert:** sidebar layout pixel dimensions.
- **Platform coverage:** mac+linux.

### Modes / tabs

#### tabs/navigation

##### tabs/navigation/001 — `Ctrl+PageDown` / `Ctrl+PageUp` switch tabs from any mode (including from inside a focused pane).
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** active tab index advances / retreats; the keystroke is not delivered to the focused pane's PTY.
- **Does not assert:** the tab bar's exact label widths under truncation (covered by `tab_layout` pure-data tests in the lib tier).
- **Platform coverage:** mac+linux.

##### tabs/navigation/002 — `Tab` / `Shift+Tab` switch tabs only in command mode; in PaneInput mode the keystroke reaches the agent PTY.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** with PaneInput active, `Tab` is delivered to the pane (parsed grid grows); with command mode active, the tab index advances.
- **Does not assert:** `Left` / `Right` / `h` / `l` aliases — covered by `tabs/navigation/003`.
- **Platform coverage:** mac+linux.

##### tabs/navigation/003 — `Left` / `Right` / `h` / `l` alias `Shift+Tab` / `Tab` in command mode.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** each alias keystroke moves the active tab one step in the documented direction.
- **Does not assert:** any aliases under PaneInput mode (those go to the pane).
- **Platform coverage:** mac+linux.

#### tabs/mode

##### tabs/mode/001 — Selecting a mode on the new-pane form opens a mode tab with the agent pane on the left and persistent side panes stacked on the right.
- **Layer:** L2.
- **Agent:** none (fixture `.dot-agent-deck.toml` with one persistent pane).
- **Asserts:** new tab appears in the tab bar; agent pane is in the left half; side pane region renders on the right.
- **Does not assert:** the side pane's command output content beyond non-empty PTY bytes.
- **Platform coverage:** mac+linux.

##### tabs/mode/002 — `Ctrl+w` on a mode tab tears down the entire workspace (agent + all side panes).
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** tab disappears; the daemon's `list_agents` no longer returns the agent that lived in the tab.
- **Does not assert:** side panes' shells receive SIGTERM vs SIGKILL (an implementation detail).
- **Platform coverage:** mac+linux.

##### tabs/mode/003 — Reactive rule routes a matching agent bash command to a reactive side pane.
- **Layer:** L2.
- **Agent:** none (synthetic `PostToolUse` event for a `Bash` tool whose command matches a rule's pattern).
- **Asserts:** the reactive side pane is populated; its title reflects the matched command.
- **Does not assert:** the rule's regex internals (covered by `config_validation` pure-data tests).
- **Platform coverage:** mac+linux.

##### tabs/mode/004 — Once all reactive slots are full, the next match reuses the oldest slot (circular pool).
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** three distinct matches against a 2-slot pool leave the second and third matches visible; the first is gone.
- **Does not assert:** slot reuse ordering beyond "oldest first".
- **Platform coverage:** mac+linux.

#### tabs/orchestration

##### tabs/orchestration/001 — Selecting an orchestration on the new-pane form opens one pane per role with the orchestrator's pane in focus.
- **Layer:** L2.
- **Agent:** none (orchestration fixture with three stub-command roles, one with `start = true`).
- **Asserts:** the new tab contains three panes; the focused pane is the `start = true` role.
- **Does not assert:** what command is rendered in each pane (the stub fixture is opaque to the harness).
- **Platform coverage:** mac+linux.

##### tabs/orchestration/002 — `Ctrl+w` on an orchestration tab closes the tab and stops every role pane.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** tab disappears; the daemon no longer carries the role agents.
- **Does not assert:** the order in which roles are closed.
- **Platform coverage:** mac+linux.

#### tabs/selection

##### tabs/selection/001 — Each tab remembers its own selection by stable id across switch-away/switch-back (PRD #83 M1).
- **Layer:** L1 (in-process unit test; `src/tab.rs`).
- **Agent:** none (mock `PaneController`).
- **Asserts:** stamping a distinct stable id on the Dashboard (`selected_session_id`), a Mode tab (`focused_pane_id`), and an Orchestration tab (`focused_role_pane_id`), then switching through every tab and back, leaves each tab holding its own id unchanged — selection is per-tab, not a single global value.
- **Does not assert:** rendering of the selection; focus restore (covered by `tabs/selection/002`).
- **Platform coverage:** mac+linux+windows.

##### tabs/selection/002 — `switch_to` focus restore + capture round-trips a Mode tab's focused pane (PRD #83 M2).
- **Layer:** L1 (in-process unit test; `src/tab.rs`).
- **Agent:** none (mock `PaneController` records `focus_pane` calls).
- **Asserts:** focusing side pane #2 then switching out captures that pane id into the Mode tab; switching back calls `focus_pane` with the stored id; with the field cleared to `None`, switch-in instead focuses the agent pane.
- **Does not assert:** Dashboard focus restore (keyed by session id, handled in the UI loop, not `TabManager`).
- **Platform coverage:** mac+linux+windows.

##### tabs/selection/003 — Dashboard `selected_index` is derived from `selected_session_id`; the sync is gated to the active tab (PRD #83 M3).
- **Layer:** L1 (in-process unit test; `src/tab.rs`).
- **Agent:** none.
- **Asserts:** `ui::sync_and_derive_selection` resolves a Dashboard `selected_session_id` to its card index, and adopts a focused pane that maps to a visible card; running the same sync against a Mode tab returns `None` and never rewrites the Dashboard's stored id (no cross-tab leak).
- **Does not assert:** the per-frame call site in `run_tui` (exercised by the L1 render test `dashboard/pane/005`).
- **Platform coverage:** mac+linux+windows.

##### tabs/selection/004 — Stale-id fallback clears the field and defaults; reactive-pane recreation remaps focus (PRD #83 M4).
- **Layer:** L1 (in-process unit test; `src/tab.rs`).
- **Agent:** none (mock `PaneController`).
- **Asserts:** a remembered session/role id no longer in the filtered list is cleared and the selection falls back to index 0; `remap_focus_after_reactive_change` follows a `(closed_id, new_id)` pair to the successor pane on BOTH the active tab (returning its new id for re-focus) and a background (non-active) Mode/Orchestration tab, and clears the field on either when a focused pane vanished with no successor.
- **Does not assert:** the controller-level resize that follows a reactive swap.
- **Platform coverage:** mac+linux+windows.

##### tabs/selection/005 — Multi-tab walkthrough: each switch-in restores that tab's own deck/pane (PRD #83 M2/M6).
- **Layer:** L1 (in-process integration test; `src/tab.rs`).
- **Agent:** none (mock `PaneController` records `focus_pane` calls).
- **Asserts:** across a Dashboard, two Mode tabs, and one Orchestration tab, focusing a side pane on each Mode tab and switching between tabs restores each destination tab's own remembered pane (or its default agent / start-role pane) via a `focus_pane` call.
- **Does not assert:** rendering; this drives the `TabManager` capture/restore path directly.
- **Platform coverage:** mac+linux+windows.

### Embedded pane attach

#### embed/attach

##### embed/attach/001 — Starting an agent attaches a live PTY stream to the embedded pane region; its output renders into the parsed grid.
- **Layer:** L2.
- **Agent:** none (fixture stub command writes a fixed banner).
- **Asserts:** the banner string appears in the parsed grid for the agent pane region within a `wait_until_quiescent` window.
- **Does not assert:** byte-level timing of the stream.
- **Platform coverage:** mac+linux.

##### embed/attach/002 — Reattach replays the daemon's per-agent scrollback snapshot.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** after detaching and reattaching, a banner that was emitted before the detach is still in the parsed grid.
- **Does not assert:** the full scrollback length (the snapshot is bounded).
- **Platform coverage:** mac+linux.

##### embed/attach/003 — Mouse scroll forwards to the focused embedded pane when the pane reports mouse-mode support.
- **Layer:** L2.
- **Agent:** none (fixture: a pane that enables mouse tracking and echoes wheel events).
- **Asserts:** the parsed grid shows the wheel-event echo after a simulated scroll.
- **Does not assert:** scroll velocity / acceleration.
- **Platform coverage:** mac+linux.

##### embed/attach/004 — Scrollback navigation (Page Up / Down) does not corrupt the live region.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** after scrolling back and returning to the bottom, the parsed grid still tracks new bytes.
- **Does not assert:** the exact scroll keymap on every platform.
- **Platform coverage:** mac+linux.

##### embed/attach/005 — `AgentRecord.tab_membership` returned by the daemon's `list_agents` is sanitized on hydration; hostile fields (ANSI escapes, NUL bytes, control chars, oversized cwd/role_name) do not corrupt the rebuilt tab bar.
- **Layer:** L2.
- **Agent:** none (fixture forces a daemon to advertise an `AgentRecord` whose `tab_membership` carries `\x1b[31m`, an embedded NUL, and an over-cap role name; harness exposes a helper to override the daemon's outgoing record).
- **Asserts:** after reattach, the rebuilt tab bar contains no raw ANSI / control bytes in any rendered cell; the offending agent either appears under a sanitized label or is bucketed back to the dashboard (per `validate_tab_membership`'s policy).
- **Does not assert:** the exact sanitization output beyond "no raw control bytes survive into the rendered grid" (the pure-data `validate_tab_membership_*` tests pin the per-field policy).
- **Platform coverage:** mac+linux.

### Hook delivery

#### hooks/delivery

##### hooks/delivery/001 — A Claude Code `SessionStart` hook arriving at the daemon's hook socket creates a session entry on the dashboard.
- **Layer:** L2.
- **Agent:** none (write JSON directly to the per-test hook socket).
- **Asserts:** a card appears for the new `session_id`; status is the post-`SessionStart` resting state per the `state` module.
- **Does not assert:** card position in the grid (covered by `dashboard/pane/001`).
- **Platform coverage:** mac+linux.

##### hooks/delivery/002 — A `PreToolUse` hook updates the right session's card by `pane_id`/`session_id` correlation.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** with two synthetic sessions present, only the targeted card transitions to Working.
- **Does not assert:** how `pane_id` is propagated through the env var (a hooks-install concern covered by `hooks/install/*`).
- **Platform coverage:** mac+linux.

##### hooks/delivery/003 — An OpenCode `tool.execute.before` hook updates the right session's card.
- **Layer:** L2.
- **Agent:** none (synthetic OpenCode-format payload).
- **Asserts:** correct OpenCode session transitions to Working with the right tool name.
- **Does not assert:** Claude-vs-OpenCode card visual differentiation.
- **Platform coverage:** mac+linux.

##### hooks/delivery/004 — A malformed hook payload is dropped without disrupting the deck.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** sending invalid JSON to the hook socket leaves all cards and statuses unchanged; the deck does not exit.
- **Does not assert:** error logging content (best-effort logging path).
- **Platform coverage:** mac+linux.

##### hooks/delivery/005 — Hook events survive a TUI detach/reattach cycle (daemon buffers).
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** an event sent while the TUI is detached is reflected in the card status on reattach.
- **Does not assert:** how the daemon buffers (snapshot vs queue).
- **Platform coverage:** mac+linux.

##### hooks/delivery/006 — `DOT_AGENT_DECK_PANE_ID` is scrubbed and re-set per-agent so hooks from agent A never carry agent B's `pane_id`.
- **Layer:** L2.
- **Agent:** none (two synthetic agents started under the same daemon; each invokes the bundled `hook` subcommand and the daemon's env-scrub is what isolates them).
- **Asserts:** with two cards alive, a hook emitted from agent A updates only A's card; a subsequent hook from agent B updates only B's card; neither hook's payload arrives carrying the other agent's `pane_id`.
- **Does not assert:** the absolute env-scrub call sites (covered by `agent_pty` pure-data tests `spawn_scrubs_via_daemon_env_from_child`, `spawn_scrubs_pane_id_env_from_child`, `spawn_opts_env_overrides_pane_id_scrub` — moved to `tmp/legacy-tests/`; this catalog entry replaces that lost end-to-end signal).
- **Platform coverage:** mac+linux.

#### hooks/install

##### hooks/install/001 — Launching the deck with `~/.claude/` present writes hook entries into `~/.claude/settings.json` idempotently.
- **Layer:** L2.
- **Agent:** none (fixture redirects `HOME`).
- **Asserts:** after first launch, `settings.json` contains the expected hook list; a second launch leaves it byte-identical.
- **Does not assert:** other unrelated keys in `settings.json` (must be preserved verbatim).
- **Platform coverage:** mac+linux.

##### hooks/install/002 — Launching the deck with `~/.opencode/` present writes the JS plugin to `~/.opencode/plugin/dot-agent-deck/index.js`.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** plugin file exists; its content equals the bundled template with `BINARY_PATH` interpolated.
- **Does not assert:** the plugin runs (verified end-to-end by `hooks/delivery/003`).
- **Platform coverage:** mac+linux.

##### hooks/install/003 — Missing agent directories result in a silent skip — no error path.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** launching with neither `~/.claude/` nor `~/.opencode/` does not write any settings file and the TUI starts normally.
- **Does not assert:** the (absence of a) tracing log line.
- **Platform coverage:** mac+linux.

### Pane / agent lifecycle

#### lifecycle/start

##### lifecycle/start/001 — Starting an agent via the new-pane form creates one card and one PTY in the daemon registry.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** the daemon's `list_agents` returns one entry whose `pane_id_env` matches what the TUI assigned.
- **Does not assert:** PTY size at spawn (covered by `resize/sigwinch/*`).
- **Platform coverage:** mac+linux.

##### lifecycle/start/002 — An invalid command field shows an inline form error and does not spawn an agent.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** the form gains an error message; no new agent appears in `list_agents`.
- **Does not assert:** the error message wording (loose substring match).
- **Platform coverage:** mac+linux.

#### lifecycle/stop

##### lifecycle/stop/001 — `Ctrl+w` on a focused dashboard card stops the agent and removes the card.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** daemon-side `list_agents` shrinks; the card disappears.
- **Does not assert:** filesystem cleanup of the agent's scratch dir.
- **Platform coverage:** mac+linux.

##### lifecycle/stop/002 — `dot-agent-deck daemon stop` with managed agents alive exits non-zero without killing them (data-loss guard).
- **Layer:** L2.
- **Agent:** none (the harness runs the `daemon stop` subcommand).
- **Asserts:** subprocess exits non-zero; the daemon and managed agents are still alive afterwards.
- **Does not assert:** stderr content beyond mentioning `--force`.
- **Platform coverage:** mac+linux.

##### lifecycle/stop/003 — `daemon stop --force` kills the daemon and any managed agents, then exits zero.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** the daemon socket disappears within the grace window; managed agents are reaped.
- **Does not assert:** SIGTERM-vs-SIGKILL escalation timing (covered indirectly by the lib's terminate tests now living in `tmp/legacy-tests/`).
- **Platform coverage:** mac+linux.

##### lifecycle/stop/004 — `daemon stop` with no daemon running is idempotent (exit 0).
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** subprocess exits 0; no daemon spawned by the call.
- **Does not assert:** stdout content (loose contains-check).
- **Platform coverage:** mac+linux.

#### lifecycle/restart

##### lifecycle/restart/001 — `daemon restart` reuses the next-launch lazy-spawn — a subsequent `dot-agent-deck` launch comes up against a fresh daemon process.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** the daemon PID before and after a restart cycle differ; the deck still attaches.
- **Does not assert:** any timing characteristics of the restart.
- **Platform coverage:** mac+linux.

#### lifecycle/daemon-idle

##### lifecycle/daemon-idle/001 — The daemon exits after the idle window elapses with no TUI and no managed agents.
- **Layer:** L2.
- **Agent:** none (tunable idle window via `DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS`).
- **Asserts:** the daemon socket disappears within the window plus a small jitter budget.
- **Does not assert:** behavior with the env var set to `0` (covered by `lifecycle/daemon-idle/002`).
- **Platform coverage:** mac+linux.

##### lifecycle/daemon-idle/002 — Setting `DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS=0` disables the idle shutdown.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** after a window comfortably longer than the default, the daemon still answers.
- **Does not assert:** indefinite lifetime (capped by the test timeout).
- **Platform coverage:** mac+linux.

#### lifecycle/handshake

##### lifecycle/handshake/001 — Build-version match on attach proceeds silently into the dashboard.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** no mismatch prompt is rendered; the dashboard appears.
- **Does not assert:** any tracing log line.
- **Platform coverage:** mac+linux.

##### lifecycle/handshake/002 — Build-version mismatch in a TTY context renders the interactive prompt; pressing `S` terminates the old daemon and lazy-spawns a fresh one.
- **Layer:** L2.
- **Agent:** none (uses `DOT_AGENT_DECK_BUILD_ID_OVERRIDE` to simulate skew).
- **Asserts:** the rendered prompt contains both build IDs; after pressing `S` the dashboard appears against a daemon at the laptop's build.
- **Does not assert:** exact prompt-text character matching (already pinned in lib pure-data tests).
- **Platform coverage:** mac+linux.

##### lifecycle/handshake/003 — Build-version mismatch with live agents requires two consecutive `S` presses.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** one `S` does not terminate; two consecutive `S` presses do.
- **Does not assert:** the warning string wording (loose substring match).
- **Platform coverage:** mac+linux.

##### lifecycle/handshake/004 — Build-version mismatch on a non-TTY exits non-zero with a stderr recovery hint and no prompt.
- **Layer:** L2.
- **Agent:** none (run with stdout redirected to a pipe).
- **Asserts:** exit code is non-zero; stderr mentions `dot-agent-deck daemon stop`.
- **Does not assert:** exact stderr wording (pinned in lib pure-data tests).
- **Platform coverage:** mac+linux.

##### lifecycle/handshake/005 — Build-version mismatch prompt: pressing `Q` / `Ctrl+C` / `Ctrl+D` / `Esc` aborts startup with a non-zero exit and leaves the stale daemon running.
- **Layer:** L2.
- **Agent:** none (uses `DOT_AGENT_DECK_BUILD_ID_OVERRIDE` to simulate skew).
- **Asserts:** for each abort keystroke (`Q`, `q`, `Ctrl+C`, `Ctrl+D`, `Esc`): the TUI exits non-zero; the daemon socket is still answering after the exit; no fresh daemon was spawned.
- **Does not assert:** any rendered error message after abort (the prompt itself is the user-visible artifact).
- **Platform coverage:** mac+linux.

### Resize

#### resize/sigwinch

##### resize/sigwinch/001 — Resizing the outer terminal mid-run propagates a SIGWINCH and the dashboard re-renders to the new dimensions.
- **Layer:** L2.
- **Agent:** none (Decision 20 requires at least one catalog test here).
- **Asserts:** after `deck.resize(80, 24)`, the rendered grid is 80 columns wide; cards reflow accordingly.
- **Does not assert:** font-related metrics.
- **Platform coverage:** mac+linux.

##### resize/sigwinch/002 — Resize of the outer terminal also resizes every managed agent PTY.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** the daemon reports each agent's PTY at the new size; agent processes that print `tput cols` see the new column count.
- **Does not assert:** any visual reflow inside the agent (subprocess-dependent).
- **Platform coverage:** mac+linux.

##### resize/sigwinch/003 — Resize coalescing — a rapid sequence of resize events results in one final reflow, not N.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** observed reflow count under a burst of resize events is bounded; final size matches the last input.
- **Does not assert:** the exact debounce window (a harness constant).
- **Platform coverage:** mac+linux.

#### resize/layout

##### resize/layout/001 — `Ctrl+t` toggles stacked / tiled dashboard layout without dropping any agents.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** after toggling, all cards are still present; the layout differs across snapshots.
- **Does not assert:** which layout is the "default" (already a settled product call).
- **Platform coverage:** mac+linux.

### Error paths

#### error/socket

##### error/socket/001 — The deck refuses to attach to a Unix socket owned by another uid.
- **Layer:** L2.
- **Agent:** none (fixture builds a socket whose mode/owner mimic a foreign daemon).
- **Asserts:** the deck exits non-zero with a stderr message; the foreign socket is left intact.
- **Does not assert:** the message wording beyond mentioning the trust failure.
- **Platform coverage:** mac+linux.

##### error/socket/002 — Stale socket file (inode without a listener) is recovered transparently — the next launch unlinks it and lazy-spawns a fresh daemon.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** the dashboard appears on second launch; the socket is now a live daemon's.
- **Does not assert:** the time spent in the recovery path.
- **Platform coverage:** mac+linux.

#### error/config

##### error/config/001 — `.dot-agent-deck.toml` with an invalid regex makes the new-pane form refuse the mode and surface a status-line message.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** the mode is missing from the **Mode** cycle; a status-line message names the invalid pattern.
- **Does not assert:** message wording exact match.
- **Platform coverage:** mac+linux.

##### error/config/002 — Missing `.dot-agent-deck.toml` results in the **Mode** field showing only the default; the new-pane form still launches a plain pane.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** the form opens with the default mode selectable; submitting creates a dashboard pane (not a mode tab).
- **Does not assert:** the absence-of-config tip rendering (covered by `dashboard/config-gen/001`).
- **Platform coverage:** mac+linux.

#### error/agent-spawn

##### error/agent-spawn/001 — Submitting the new-pane form with a non-existent command produces a card whose status is Error and whose card body names the missing binary.
- **Layer:** L2.
- **Agent:** none (fixture command: `nonexistent-binary-78f3c`).
- **Asserts:** card appears; badge reads Error; card text contains the binary name.
- **Does not assert:** how long the failure takes to surface.
- **Platform coverage:** mac+linux.

### Orchestration delegation

#### orchestration/delegate

##### orchestration/delegate/001 — `dot-agent-deck delegate --to coder --task <text>` from the orchestrator pane writes the task into the target role's pane.
- **Layer:** L2.
- **Agent:** none (synthetic — invoke the delegate subcommand from inside the orchestrator pane via a scripted keystroke).
- **Asserts:** the target role's parsed grid contains the task text; the orchestrator's pane stays clean.
- **Does not assert:** the target agent's response (no real agent in the loop).
- **Platform coverage:** mac+linux.

##### orchestration/delegate/002 — Delegating to a role missing from the config produces a clear error on the orchestrator pane and no other side effects.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** the orchestrator pane's parsed grid carries an error mentioning the unknown role; no card statuses change.
- **Does not assert:** the error message text exactly.
- **Platform coverage:** mac+linux.

##### orchestration/delegate/003 — `dot-agent-deck work-done --task <summary>` from a worker pane writes the summary to the orchestrator and to `.dot-agent-deck/work-done-<role>.md`.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** orchestrator pane shows the summary; the file exists with the expected contents.
- **Does not assert:** the orchestrator's reply (no real LLM in this synthetic test).
- **Platform coverage:** mac+linux.

##### orchestration/delegate/004 — A worker calling `delegate` is rejected (only the `start = true` role may delegate).
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** worker's pane gains an error line; no task is delivered to any role.
- **Does not assert:** the daemon-side log entry.
- **Platform coverage:** mac+linux.

### Session restore

#### session/restore

##### session/restore/001 — `dot-agent-deck --continue` rehydrates dashboard panes from the saved session.
- **Layer:** L2.
- **Agent:** none (a saved `session.toml` with three panes; fixture redirects `DOT_AGENT_DECK_SESSION`).
- **Asserts:** three cards appear; their display names match the saved session.
- **Does not assert:** the agents' inner state (not preserved per docs).
- **Platform coverage:** mac+linux.

##### session/restore/002 — A saved mode tab is restored as a full mode tab when the project's `.dot-agent-deck.toml` still has the mode.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** after `--continue`, a tab with the mode's name appears and contains the persistent side panes.
- **Does not assert:** any reactive pane content.
- **Platform coverage:** mac+linux.

##### session/restore/003 — A saved mode whose `.dot-agent-deck.toml` no longer carries the mode falls back to a plain dashboard pane with a stderr warning.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** the saved pane becomes a dashboard card (not a mode tab); the harness's stderr capture contains a warning that names the missing mode.
- **Does not assert:** any rendering of the warning inside the TUI.
- **Platform coverage:** mac+linux.

##### session/restore/004 — A saved pane whose `dir` no longer exists is skipped with a stderr warning; other saved panes still restore.
- **Layer:** L2.
- **Agent:** none.
- **Asserts:** N-1 cards restore; stderr names the missing directory.
- **Does not assert:** which other panes survive (deterministic from the file order).
- **Platform coverage:** mac+linux.

### Chain-smoke (real-agent) coverage

#### chain-smoke/claude

##### chain-smoke/claude/001 — A real Claude Code agent run end-to-end emits hook events that drive the card through Thinking → Working → Idle.
- **Layer:** L2.
- **Agent:** Claude Code (`claude-haiku-4-5-20251001` per Decision 8).
- **Asserts:** card status traverses Thinking → Working → Idle within the test budget; tool name appears on the card during Working.
- **Does not assert:** any specific text the agent prints.
- **Platform coverage:** mac+linux (chain-smoke is local-only per Decision 8).
- **Cost note:** one Haiku invocation, ≲500 input + 200 output tokens — well under Decision 23's bound.

#### chain-smoke/opencode

##### chain-smoke/opencode/001 — A real OpenCode agent run end-to-end emits the OpenCode plugin's events and drives the card through Thinking → Working → Idle.
- **Layer:** L2.
- **Agent:** OpenCode (`openrouter/google/gemini-2.5-flash-lite` per Decision 8).
- **Asserts:** card status traverses Thinking → Working → Idle; OpenCode-format tool name appears on the card.
- **Does not assert:** any agent-generated text.
- **Platform coverage:** mac+linux.
- **Cost note:** one Gemini-Flash-Lite invocation via OpenRouter, ≲500 input + 200 output tokens.

### Docs cross-reference skips

Per Decision 27, documented user-facing behaviors that are deliberately not catalogued at M1:

| Doc behavior | Why skipped |
|---|---|
| Idle ASCII art rendering on cards ([docs/configuration.md#idle-ascii-art](../docs/configuration.md), [docs/configuration.md#standalone-cli](../docs/configuration.md)) | LLM-driven side feature; lives outside the deck/daemon/PTY surface the harness covers. Reconsider in M4+ if the feature warrants its own catalog section. |
| `dot-agent-deck connect <remote>` end-to-end SSH flow ([docs/remote-environments.md](../docs/remote-environments.md), [docs/remote-recipes.md](../docs/remote-recipes.md)) | Requires a remote-harness shape that does not exist yet. Catalogued at M4+ when remote testing lands. Local quit-dialog coverage (`prompt/quit/001`–`005`) already pins the Detach / Stop / Cancel behavior; remote attach adds only the daemon-side log distinction. |
| `dot-agent-deck remote add / list / upgrade / remove` ([docs/remote-environments.md](../docs/remote-environments.md)) | Same — remote-harness territory; the lib already covers the pure-data slices (URL parsing, command construction, error classification) in the kept tests. **Security properties deferred to M4+ end-to-end coverage:** shell-metacharacter quoting on remote-CLI argv assembly (unit-covered by `system_ssh_executor_quotes_arguments_safely`), `remotes.toml` written at mode 0o600 (covered by the now-moved `remotes_toml_written_at_0o600` test — restore at M4+), `DOT_AGENT_DECK_VIA_DAEMON=1` propagation on the remote shell (unit-covered by `build_connect_command_has_t_flag_and_via_daemon_env`). |
| `dot-agent-deck ascii` CLI subcommand ([docs/configuration.md#standalone-cli](../docs/configuration.md)) | Non-TUI subcommand; tested as a CLI smoke in M4+ if it warrants coverage. |
| `dot-agent-deck validate` CLI subcommand ([docs/workspace-modes.md#config-validation](../docs/workspace-modes.md)) | Non-TUI; the underlying validator is exhaustively covered by the pure-data `config_validation` tests. |
| `dot-agent-deck watch` CLI subcommand ([docs/workspace-modes.md#dot-agent-deck-watch](../docs/workspace-modes.md)) | Non-TUI subcommand; an L2 test would only exercise its output formatting against a real shell — low value compared to the deck-rendering surface. |
| `dot-agent-deck config get` / `config set` ([docs/configuration.md](../docs/configuration.md)) | Non-TUI; the underlying config field reflection is covered by pure-data tests (`*_get_set_field`, `*_get_set_fields`). |
| `dot-agent-deck hooks install` / `uninstall` CLI commands ([docs/troubleshooting.md#hooks](../docs/troubleshooting.md)) | Auto-install path is catalogued as `hooks/install/001`–`003`; the explicit subcommand variants share the same install/uninstall code. A targeted L2 test will be added only if a divergence appears. |
| Ghostty-specific Shift+Enter terminal config ([docs/troubleshooting.md#shift-enter-not-working-in-ghostty-terminal](../docs/troubleshooting.md)) | Outer-terminal config; no deck-side surface to test. |
| Mode-tab card jump via `Enter` (broken per docs note → [#68](https://github.com/vfarcic/dot-agent-deck/issues/68)) | Documented as broken. The catalog will gain an entry once the bug is closed; until then leaving it uncovered avoids pinning the broken behavior. |
| `--continue` "dashboard-first landing" detail ([docs/session-management.md#resuming-sessions](../docs/session-management.md)) | Implicit consequence of `session/restore/001`; not separately worth a catalog ID. Reconsider if the landing-tab logic ever has its own surface. |
## Refined Milestones

No refinement needed at M1 — the original [Milestones](#milestones) list stands as written. The Pre-committed items below remained load-bearing and shipped with M2.

**Pre-committed items** (regardless of how M1 reshapes the rest): the milestone that ships the first usable end-to-end harness must also:

- **(a) Update `CLAUDE.md`** with the conventions in [Appendix A](#appendix-a-proposed-claudemd-additions) (functional UI changes require harness tests; fast-tests-per-task / e2e-before-PR; single-test rerun pattern for failures).
- **(b) Ship the CONTRIBUTING.md sections** specified in Decision 19 (snapshot review workflow, TDD loop, how to add a new test).

Both arrive the moment they can be followed, not before.

## Discovered Issues

*Populated by M2+ as tests are written and run. See Decision 11 for the discovery policy and Decision 25 for the entry template + scoping. Under agent autonomy, the agent stops after this list is populated and surfaces it to the user before fixing, sorted by severity per Decision 25.*

Entry format (per Decision 25):

| ID | Catalog ref | Description | Severity | Status |
|---|---|---|---|---|
| di-001 | chain-smoke/opencode/001 | OpenCode 1.15.10's plugin loader does not auto-discover the deck's installed plugin at `<config>/plugin/dot-agent-deck/index.js`. Hooks never fire end-to-end, so the chain-smoke status traversal cannot be observed. Deck install path in `src/opencode_manage.rs` needs to align with OpenCode 1.x's loader (npm-package register OR package.json import). | major | escalated to PRD #79 |
| di-002 | (multiple) | Four test files added on `main` post-M1-audit (`tests/snapshot_replay_dims.rs`, `tests/rehydration.rs`, `tests/daemon_protocol.rs`, `tests/spawn_time_role_prompt_submit_after_session_start.rs`) coexist with the harness convention but carry no `#[spec]` annotation. They pass linkage-check (rule scope) but should eventually be either refactored onto the L2 harness with catalog IDs OR explicitly noted as permanent integration-test outliers. | minor | won't-fix in #77: future PRDs refactor each on its own when touching those areas |
| di-003 | (none) | `src/config.rs` re-gained the `cfg(test)` helpers (`STATE_DIR_ENV_LOCK`, `CONFIG_GEN_STATE_ENV_LOCK`, `ConfigGenStateEnvGuard`) via the post-M1 merge from `main` (commit `c479cb4`); main's resurrected `src/ui.rs` tests reference them. The M1 audit's deleted-helpers note is now partially stale. | minor | accepted: helpers are still gated `#[cfg(test)]`; production behavior unchanged |

`Severity` is one of: `blocker`, `major`, `minor`.
`Status` is one of: `fixed in <milestone>`, `filed as #NNN`, `won't-fix: <rationale>`, `escalated to PRD #NNN`.

## Appendix A: Proposed CLAUDE.md Additions

To be added as permanent instructions in `CLAUDE.md` in the same milestone that ships the first usable harness — not earlier:

> **Add or Update TUI Tests for Functional UI Changes**: When a change adds or modifies user-visible TUI behavior (panes, statuses, prompts, focus, layout, modes, embedded panes, hook delivery), add or update tests in the TUI harness. Use L1 (in-process `TestBackend` + `insta`) for pure widget/layout changes; use L2 (PTY + vt100, files named `e2e_*.rs`, gated by `#[cfg(feature = "e2e")]`) when the change touches the spawned binary, daemon, hooks, attach protocol, or real agent integration. Pure refactors with no observable behavior change do not require new tests.

> **Fast Tests Per Task, E2E Before PR**: `cargo test-fast` (alias for `cargo nextest run`) runs the fast tier — protocol/state tests plus L1 widget/render tests — and is the per-task gate. `cargo test-e2e` (alias for `cargo nextest run --features e2e`) additionally runs the L2 PTY/real-agent suite and is required to pass before the release flow. Do not run `cargo test-e2e` per task; it spawns binaries, hits LLM APIs, and is intentionally bounded to the pre-PR step.

> **Iterate on a Failing Test by Rerunning Only That Test**: When a single test fails, after fixing the code, rerun *only that test* first (`cargo test-fast lifecycle_001` or `cargo test-e2e lifecycle_001`) to verify the fix in isolation. Decision 17's function-name prefix (`<sub-area>_<NNN>_…`) makes the filter pick exactly one test. Only after that test passes, rerun the full tier (`cargo test-fast`, plus `cargo test-e2e` pre-PR) before committing.

> **Every `#[spec]` Test Has a Scenario Comment**: When adding or modifying a `#[spec(...)]`-annotated test in `tests/`, the test function MUST carry a `/// Scenario:` doc comment of 1–3 sentences describing in plain English what the test does — start the app, what gets pressed/sent/written, what should happen visibly. The `cargo xtask docs --tests` command regenerates a local `.md` under `.dot-agent-deck/recordings/<test>/test.md` (gitignored) that pairs with the cast for browsing during development. CI's linkage-check rule 7 fails the build if the Scenario comment is missing or the generator fails — not on local `.md` drift, since it isn't committed.

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
- `asciinema` (devbox-shipped at `asciinema@3.2.0`; required to replay the `full-stream.cast` recordings produced by Decision 28 / the M2 validation handoff)
- Real Claude Code and OpenCode CLIs installed on the developer's local machine (where e2e runs per Decision 8). CI runs the fast tier only and does not need the agent CLIs.
- An OpenRouter API key configured locally for OpenCode chain-smoke tests. Like the Anthropic credential, this is a developer-environment requirement — never a CI secret per Decision 8.

## Validation Strategy

- **Fast tier (CI):** `cargo test-fast` green on macOS and Linux in GitHub Actions is the per-PR signal. Windows joins when the harness's Windows path is ready.
- **E2e tier (local):** `cargo test-e2e` green on the developer's machine before opening the PR is the chain-level signal. Enforced by the orchestrator's pre-release gate (Decision 6) and the iterative validation cadence (Decision 29). Windows e2e validation is its own milestone in M2+.

The user (PRD owner) does explicit validation at the checkpoints in Decision 29 (end of M1, end of M2, end of M3), then standard pre-PR sign-off on each subsequent milestone.
