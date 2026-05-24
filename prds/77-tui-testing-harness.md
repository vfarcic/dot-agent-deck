# PRD #77: TUI Testing Harness

**Status**: Planning
**Priority**: High
**Created**: 2026-05-09
**GitHub Issue**: [#77](https://github.com/vfarcic/dot-agent-deck/issues/77)

## Problem Statement

The dot-agent-deck TUI is validated end-to-end by hand. Pane creation, status transitions (`running` / `waiting_for_input` / `idle` / `error`), focus, prompt regions, mode/tab navigation, layout, and the full hook → daemon → UI flow have no automated coverage at the rendered-screen level. Three concrete consequences:

1. **Regressions ship easily.** A change to layout, status logic, or hook handling can pass `cargo test` cleanly while breaking what the user sees. PRD-76 in particular is wiring a daemon ↔ TUI attach protocol whose UI-side correctness today depends on manual smoke-testing.
2. **Refactors are expensive.** Touching `state.rs`, `tab.rs`, `pane.rs`, `mode_manager.rs`, or `terminal_widget.rs` requires the author to manually reproduce a long checklist of flows. Confidence is a function of how patient the author is feeling.
3. **The PR feedback loop is human-bottlenecked.** Reviewers cannot verify TUI behavior without checking out the branch and running it locally. CI runs are silent on the question "does this still render the dashboard correctly?"

The two test files in `tests/` (`integration_test.rs`, `mode_integration_test.rs`, `local_attach.rs`, etc.) cover protocol- and state-layer logic. They do not exercise a spawned binary, do not parse rendered screens, and do not run real agent CLIs.

## Solution Overview

Introduce a **cross-platform end-to-end TUI test harness** that:

1. **Spawns the dot-agent-deck binary in an isolated PTY**, with `DOT_AGENT_DECK_SOCKET` and `DOT_AGENT_DECK_ATTACH_SOCKET` pointed at per-test paths and `HOME` redirected to a tempdir so global hook installation is sandboxed.
2. **Runs real Claude Code and OpenCode CLIs inside the deck** as agent processes, exactly as a user would.
3. **Captures the deck's rendered output through a vt100 parser**, exposing a structured screen grid (cells, attributes, cursor) rather than a string blob.
4. **Asserts only on the deck's observable state** — panes, statuses, focus, prompts, hook event delivery, attach stream presence. Never on agent text content.
5. **Runs identically on macOS, Linux, and Windows**. Cross-platform from day one in design; rolled out OS-by-OS.

### Toolchain choice

After surveying the landscape (see [Design Decisions](#design-decisions) below), the harness is built on:

- **`portable-pty 0.8`** for the outer PTY — already a production dep, and the only mature Rust PTY abstraction that covers Unix and Windows (ConPTY).
- **`vt100 0.16`** for ANSI parsing — already a production dep, gives a structured screen model with cells/attributes/cursor.
- **`insta`** (new dev-dep) for snapshot tests of the parsed grid where appropriate.
- **ratatui `TestBackend`** as a complementary in-process layer for pure widget/layout tests that don't need a spawned binary.
- **`vhs`** (Charm) is explicitly **not** used as the test foundation, but is kept in mind as a candidate post-M1 layer for double-duty docs/demo recordings.

This is the same approach **zellij**'s `src/tests/e2e` uses — the only mature Rust TUI E2E precedent.

### Test isolation strategy

Each test gets its own:
- Temporary directory (`tempfile::tempdir`)
- `DOT_AGENT_DECK_SOCKET` path inside that tempdir
- `DOT_AGENT_DECK_ATTACH_SOCKET` path inside that tempdir
- `HOME` pointing at the tempdir (so `~/.claude/settings.json` writes don't touch the developer's real config when hook-installation paths are exercised)
- Fixed PTY size (e.g. 120×40)
- Pinned `TERM=xterm-256color`, `LC_ALL=C.UTF-8`, `NO_COLOR` unset, `COLORTERM=truecolor`

This guarantees a developer running `cargo test` does not collide with their real running deck (the hook-socket-clash problem identified during scoping), and tests do not pollute each other.

### Assertion strategy

The hard rule: **assertions read the deck's rendered grid or the protocol surface, never the agent subprocess's stdout.**

Concrete examples:
- "Start a Claude Code agent" → assert a pane appears in the expected layout region with status `running`. Don't assert on what Claude prints.
- "Inject a `Notification: permission_prompt` hook event" → assert the pane status flips to `waiting_for_input` and the prompt indicator renders. Don't assert on which tool prompted.
- "Press `Tab` twice" → assert which pane region holds the focus marker. Don't assert on pane contents.
- "Open an embedded pane via attach" → assert bytes flow over `attach_socket_path()` and the embedded region is non-empty. Don't assert what's in it.

This eliminates LLM non-determinism as a flake source and makes the same test meaningful regardless of which model, account, or rate-limit state Claude/OpenCode happens to be in.

### Cross-platform rollout

Same test code, three-stage rollout:

1. **macOS local first** — developer laptop, primary feedback loop.
2. **Linux in GitHub Actions** — CI gate. Same tests, headless ubuntu-latest runner.
3. **Windows** — last. Specific runner choice (GHA `windows-latest`, self-hosted, or other) decided when M1 produces the test catalog and we know the actual ConPTY pain points.

If a test cannot run on a platform for a real reason (e.g. agent CLI not installed), it skips with an explicit reason, not a silent pass.

## Design Decisions

### Decision 1: portable-pty + vt100 over alternatives

**Surveyed:** rexpect, expectrl, tmux + bash, vhs, teatest, pexpect, asciinema, node-pty + xterm/headless, wezterm cli, ratatui-testlib, testty, ratatui `TestBackend` (alone).

**Eliminated for hard reasons:**
- `rexpect` — Unix only, no Windows ([open since 2020](https://github.com/philippkeller/rexpect/issues/11)).
- `tmux` send-keys / capture-pane — no Windows.
- `wezterm cli` — requires GUI session, awkward in CI.
- `node-pty` / `pexpect` — adds JS or Python toolchain to a pure-Rust CI for no proportional gain.
- `testty` / `ratatui-testlib` — single-author alphas, low stars, recently created, no production track record.
- `expectrl` — has Windows support but only string-scrape assertions (no grid model), and Windows path is the least-battle-tested.
- `vhs` — `.tape` DSL fragile under timing variation, Windows is the soft spot.

**Chosen:** `portable-pty` + `vt100` because:
- **Already production dependencies** in this project — zero new dep cost, zero new toolchain cost.
- Cross-platform including Windows (ConPTY).
- Structured grid assertions, not string scraping.
- Clean handling of nested children (real Claude/OpenCode CLIs spawn their own PTYs inside the deck).
- Production precedent: zellij's `src/tests/e2e` uses this same pattern.

**Trade:** the harness must be written in-house. That work is what M2 produces.

### Decision 2: ratatui `TestBackend` + `insta` as a complementary layer

For pure widget/layout regressions that do not need a spawned binary, ratatui's in-process `TestBackend` paired with `insta::assert_snapshot!` of the buffer is cheaper and faster than a full PTY spin-up. The harness has two layers:

- **L1 — in-process:** `TestBackend` + `insta` for "does this widget render the right cells given this state". No subprocess.
- **L2 — end-to-end:** spawned binary in PTY + vt100 + assertions for "does the whole system behave correctly when the daemon, hooks, attach protocol, and a real agent are all in the loop".

Test cases are placed at the layer that's strictly necessary. Don't burn a PTY spin-up for what's actually a widget render question.

### Decision 3: Real agents in the loop, observable-state assertions only

Real Claude Code and OpenCode CLIs run inside the harnessed deck. The deck does not get a mock-agent fixture mode for this. Reasoning:

- A mock agent would require maintaining a parallel set of "what hook events does Claude actually emit, in what order" — exactly the assumption that breaks silently when an agent CLI changes.
- Assertions on the deck's grid + protocol surface are already deterministic regardless of agent stdout, so the LLM non-determinism concern doesn't actually leak in.
- Cost in CI is bounded: API tokens per run are small if test count is reasonable and tests use cheap models.

Open question for M1: do we run *every* test against a real agent, or do some tests synthesize hook events directly (bypassing the agent CLI) for speed? Synthetic-event tests are valid where the test specifically targets deck behavior given a known event sequence; real-agent tests are required for "does the hook integration actually work end-to-end."

### Decision 4: Cross-platform from day one in design, OS-by-OS in rollout

The harness's design must accommodate Windows ConPTY quirks (alternate-screen repaints, line-ending oddities, `isatty` differences, environment-variable propagation) from the first commit, even though Windows is the third platform brought online. Anything macOS-specific or Linux-specific in the harness is a bug.

### Decision 5: M1 produces the scope; later milestones are TBD

The test catalog is itself the first milestone. Until that catalog exists, naming downstream milestones is guessing. M2+ are written after M1 lands.

### Decision 6: L2 tests live in `tests/` alongside existing tests, separated by filename convention + cargo feature

L2 (PTY-spawning, real-agent) tests are physically separated from the fast tier so that `cargo test` remains fast by default and doesn't burn LLM API tokens on every developer run.

**File layout:** all tests stay in the existing top-level `tests/` directory (no new test crate, no `tests/e2e/` subdirectory — Cargo would treat the latter as helper modules, not integration-test binaries). L2 files are named `e2e_*.rs` so a glance at `ls tests/` shows which tier a file belongs to.

**Execution gating:** each `e2e_*.rs` file opens with `#![cfg(feature = "e2e")]`. The fast tier (today's tests + new L1 `TestBackend`/`insta` tests) has no feature gate.

- `cargo test` → fast tier only (current behavior preserved; safe muscle memory)
- `cargo test --features e2e` → fast tier + L2

**Cargo aliases** (`.cargo/config.toml`):

```toml
[alias]
test-fast = "test"
test-e2e  = "test --features e2e"
```

These are the two "scripts" referenced by the orchestration and CLAUDE.md instructions below.

**Why naming + feature flag, not naming alone:** naming gives discoverability but doesn't change what `cargo test` runs by default. Without a gating mechanism, a developer's reflex `cargo test` would trigger PTY spawns and LLM API calls — a footgun. `#[ignore]` was considered but is per-function (easy to forget on new tests) and mixes meanings with other legitimate uses of `--ignored`. Feature-flag gating is per-file, hard to bypass, and clear in source.

**Why L1 files stay in the fast tier:** L1 tests use ratatui `TestBackend` + `insta` in-process — no subprocess, no API calls, milliseconds per test. They belong with the per-task fast tier despite being "functional" in nature. The split axis is *PTY-spawning / real-agent*, not *unit vs functional*.

**Orchestration integration:** the milestone that ships the first usable harness also updates `.dot-agent-deck.toml`:

- **Coder role** (currently runs `cargo test`): change to `cargo test-fast`.
- **Orchestrator workflow:** add a pre-release step that requires `cargo test-e2e` to pass before delegating to the release role.

This keeps the per-task feedback loop fast and makes the slow e2e suite a single gate before the PR, which matches `feedback_validate_pre_pr.md`.

### Decision 7: The test catalog is the spec, and tests are linked to it by stable IDs

The harness is built to serve double duty: validate behavior *and* describe it. The "describe it" half has known failure modes (BDD-style specs that drift from code, generated docs that nobody reads), so the structure here is deliberate.

**Two artifacts, one source of truth:**

1. **Human-readable spec** — the `## Test Case Catalog` section produced by M1 (extracted to `docs/tui-spec.md` once it grows past a couple hundred entries). Browsable by someone who doesn't read Rust.
2. **Executable spec** — the tests themselves. Fail loudly when behavior changes.

**Catalog entries have stable IDs:**

```
pane/lifecycle/001 — A pane appears in the next free layout region when an agent is started.
focus/nav/001     — Tab moves focus to the next pane in z-order; Shift-Tab reverses.
hooks/delivery/001 — A Notification:permission_prompt event flips the target pane to waiting_for_input.
```

IDs are stable across renames; the prose is the user-facing description. M1 defines the ID format (proposed: `<area>/<sub-area>/<NNN>`).

**Tests reference catalog IDs via a `#[spec(...)]` helper:**

```rust
#[spec("pane/lifecycle/001")]
#[test]
fn pane_appears_when_agent_starts() {
    let deck = TuiDeck::launch();
    deck.start_agent(claude_code());
    deck.assert_pane(0).is_visible().has_status(Running);
}
```

**CI-enforced linkage** (small Rust binary under `xtask/` or a shell script, runs in CI):

- Every catalog ID has at least one test referencing it.
- Every `#[spec("...")]` annotation references a real catalog ID.

This makes drift impossible to ship: deleting a catalog entry without its test (or vice versa) fails CI.

**File layout mirrors catalog sections** so a reader can move from spec to test mechanically:

```
tests/
  e2e_pane_lifecycle.rs       ← pane/lifecycle/*
  e2e_focus_navigation.rs     ← focus/nav/*
  e2e_hook_delivery.rs        ← hooks/delivery/*
  render_dashboard.rs         ← L1: layout/dashboard/*
  render_status_glyphs.rs     ← L1: layout/status/*
```

**Fluent harness API**: the L2 harness from M2+ is designed for test-body *readability*, not just correctness. Test bodies should read close to the catalog prose (`deck.start_agent(...)`, `deck.pane(0).wait_until_status(Running)`), not raw PTY plumbing. The harness exists either way; designing its surface to read like behavior costs nothing extra up front and a lot if retrofitted later.

**What this decision deliberately excludes:**

- **No Gherkin/Cucumber, no custom DSL.** Already a non-goal (line 148); BDD frameworks consistently produce specs that drift from the code they claim to describe. Stable IDs + linkage CI achieves the same intent with less surface area.
- **No catalog generated from tests.** Tempting, but the generator becomes the spec, and now there are two things to maintain. Catalog-first with enforced linkage is simpler and lands sooner.
- **Insta snapshots are not the spec.** They're ground truth for "did the rendered cells change," not a human-readable description of behavior. The catalog stays prose.

**Added to M1 scope** (in addition to the existing test catalog deliverable): (a) define the catalog ID format, (b) commit to the file-layout-mirrors-catalog convention, (c) write the linkage-check tool. These are small additions; the heavy lift remains producing the catalog itself.

### Decision 8: Synthetic events by default, real-agent tests reserved for chain smoke; Haiku in CI on every PR

This resolves the open question previously flagged at line 115 ("do we run every test against a real agent, or do some tests synthesize hook events directly?") and the CI economics question.

**The deck's only contract with the agent is the hook event stream.** A test that wants to verify "pane status flips to `waiting_for_input` when a permission prompt arrives" does not need a real Claude Code session producing that event; it can write the hook JSON directly to the deck's hook socket and assert the deck reacts correctly. That's a *synthetic-event test* — milliseconds, free, deterministic, no LLM in the loop.

**Real-agent tests** (spawning Claude Code or OpenCode inside the deck) are reserved for explicitly verifying that the *whole chain* works: the agent CLI produces hook events in the format the deck expects, the events arrive over the real socket, the deck handles them. These are "chain smoke" tests — small in number, not exhaustive.

**Default for new tests:** synthetic. A test is real-agent only when it explicitly answers "does the agent → hook → deck chain still work end-to-end?" — typically one or two tests per supported agent CLI, not one per behavior.

**CI configuration:**
- Real-agent tests run with **Haiku** (`claude-haiku-4-5-20251001`) — the cheapest current model. The deck asserts on its own grid/protocol, not on agent text content, so model quality is irrelevant; cost is the only axis.
- Real-agent tests run **on every PR**, not nightly-only. Conditional on the synthetic-default policy above, the per-PR token cost is small enough that gating PRs on the full chain is worth it.
- API keys live in GHA secrets; PR-from-fork handling is M2+ scope (fork PRs may have to skip real-agent tests until then).

This pair of policies — synthetic-by-default plus Haiku-on-every-PR — is the load-bearing assumption that makes the harness affordable. If a future change pushes real-agent tests above ~10% of the suite, the cost math needs revisiting.

### Decision 9: No auto-retry; flake = bug, fix it

The harness must never use `--retries=N`, `cargo nextest --retries`, or any "rerun until green" wrapper in CI. A flaky test is a bug — either in the test (timing assumption, missing quiescence wait, leaked state across tests) or in the deck itself (genuine race condition the user could also hit). Both deserve a fix, not a retry mask.

This pins three existing PRD positions together as policy:
- The hard rule against `sleep` (line 132): waits are quiescence-based or signal-based, never time-based.
- The per-test isolation strategy (tempdirs, scoped sockets, redirected `HOME`): no cross-test state leakage.
- `feedback_always_fix_failures.md`: existing project norm against dismissing failures.

**Operational consequence:** if a test flakes in CI, the merge is blocked and the test is either fixed or quarantined with an open issue referencing the catalog ID, *not* retried. Quarantine is for "we know what's wrong, fix is in progress" — it is not a graveyard.

## Sequencing note: PRD #77 vs PRD #84 (rendering rework)

PRD #84 is a structural rework of the rendering layer (single contract for layout → PTY size → drawn cells). Many of the visual bugs it targets are exactly the class this harness would prevent regressions on.

**Order:** the harness lands first. #84 then refactors against a green safety net, and accepts the resulting wave of L1 snapshot updates as part of that PRD's scope (most snapshots will need regeneration because the rendered cells will legitimately change). The alternative — wait for #84 — keeps the manual-validation marathon that motivated this PRD in the first place, and gives #84 no harness to refactor against.

L2 tests (which assert on observable state, not exact rendered cells) should largely survive #84 untouched. L1 snapshots are the churn surface.

## Key Design Constraints (carried over from survey)

These are tool-agnostic gotchas that any implementation must address. They are inputs to M1 and M2, not optional polish:

- **Pin terminal size explicitly** (e.g. 120×40). Never inherit host dimensions.
- **Pin `TERM`** (`xterm-256color`) and locale (`LC_ALL=C.UTF-8`).
- **Pin color env vars** (`NO_COLOR`, `COLORTERM`, `CLICOLOR_FORCE`).
- **Don't `sleep` for synchronization.** Wait for a render-stable signal — a specific string in the buffer, or N ms of byte-stream quiescence.
- **ratatui uses the alternate screen.** Capture mid-run, never post-quit.
- **Resize is a real test surface.** SIGWINCH propagation differs across PTY abstractions; cover at least one resize.
- **ConPTY rewrites the byte stream on Windows** in ways Linux/macOS don't. Assert on the parsed grid, never on the raw byte sequence.
- **Nested-PTY signal forwarding.** Ctrl-C must reach the right child. Cover with a test that backgrounds an inner agent and quits the deck.
- **Color goldens rot across terminal profiles.** Either strip colors before diffing or pin via env.
- **macOS GHA runners cap concurrent PTYs lower than Linux.** Parallel test count must be tunable.
- **Snapshot-review workflow** (insta) must be documented in CONTRIBUTING.md before any goldens land. Otherwise reviewers blind-accept diffs.

## Non-Goals (v1)

- Windows-first. macOS and Linux ship before Windows.
- Replacing existing protocol/state-layer tests in `tests/`. Those stay.
- Visual diff (image-level GIF comparison). Grid-level assertions only.
- Recording test runs as user-facing demo GIFs. (Possible post-M1 extension via `vhs`, but explicitly out of scope here.)
- Mocking agent CLIs. Real agents only.
- A test-DSL or YAML-driven test format. Tests are Rust code in `tests/`.
- Cross-shell coverage. Tests run under the shell the harness picks; users do not configure shells per test.

## Milestones

- [ ] **M1 — Test case catalog and assertion strategy.** Produce a written catalog (in this PRD) of the test cases the harness must cover, organized by feature area (dashboard panes, statuses, prompts, focus/navigation, modes/tabs, embedded pane attach, hook delivery, lifecycle, resize, error paths). For each test case, decide: which layer (L1 in-process vs L2 end-to-end), which agent if any, what is asserted, what is explicitly not asserted, expected platform coverage. Per Decision 7, also: (a) define the stable catalog ID format (proposed `<area>/<sub-area>/<NNN>`), (b) commit to the file-layout-mirrors-catalog convention listed in Decision 7, (c) specify the linkage-check tool (catalog ↔ `#[spec(...)]` annotations) to be implemented when the first tests land. Output: an updated `## Test Case Catalog` section in this PRD plus a `## Refined Milestones` section that fills in M2+ now that scope is known.
- [ ] M2+ — TBD, defined when M1 lands.

## Test Case Catalog

*Populated by M1.*

## Refined Milestones

*Populated by M1.*

**Pre-committed item (regardless of how M1 reshapes the rest):** the same milestone that ships the first usable end-to-end harness must also update `CLAUDE.md` with the convention that functional UI changes require corresponding harness tests. The convention arrives the moment it can be followed, not before. See [Appendix A](#appendix-a-proposed-claudemd-addition) for proposed wording.

## Appendix A: Proposed CLAUDE.md Additions

To be added as permanent instructions in `CLAUDE.md` in the same milestone that ships the first usable harness — not earlier:

> **Add or Update TUI Tests for Functional UI Changes**: When a change adds or modifies user-visible TUI behavior (panes, statuses, prompts, focus, layout, modes, embedded panes, hook delivery), add or update tests in the TUI harness. Use L1 (in-process `TestBackend` + `insta`) for pure widget/layout changes; use L2 (PTY + vt100, files named `e2e_*.rs`, gated by `#[cfg(feature = "e2e")]`) when the change touches the spawned binary, daemon, hooks, attach protocol, or real agent integration. Pure refactors with no observable behavior change do not require new tests.

> **Fast Tests Per Task, E2E Before PR**: `cargo test-fast` (alias for `cargo test`) runs the fast tier — protocol/state tests plus L1 widget/render tests — and is the per-task gate. `cargo test-e2e` (alias for `cargo test --features e2e`) additionally runs the L2 PTY/real-agent suite and is required to pass before the release flow. Do not run `cargo test-e2e` per task; it spawns binaries, hits LLM APIs, and is intentionally bounded to the pre-PR step.

The L1/L2 split is deliberate: it gives reviewers a precise question to answer ("does this change the rendered grid the user sees?") rather than a feeling to debate.

## Risks & Mitigations

| Risk | Mitigation |
|---|---|
| Test flake from timing assumptions | Quiescence-based waits + render-stable string signals; never raw `sleep`. |
| Windows ConPTY surprises (Decision 4) | Design for parsed grid only, not raw bytes. Build macOS+Linux first; Windows is a verification step, not a redesign. |
| Real-agent API costs on every CI run | Use cheap models in CI; keep test count bounded; consider a "smoke" subset on PRs and a "full" suite nightly. |
| Hook-socket clash with developer's real running deck | Per-test `DOT_AGENT_DECK_SOCKET` + `DOT_AGENT_DECK_ATTACH_SOCKET` + redirected `HOME`. |
| Snapshot rot from color/terminal profile drift | Strip colors before diffing or pin color env vars per test. Document in CONTRIBUTING.md. |
| Insta goldens accepted blindly during review | Documented review workflow + small snapshots that humans can read in a diff. |

## Dependencies

- `portable-pty 0.8` (already present)
- `vt100 0.16` (already present)
- `insta` (new dev-dep, latest)
- Real Claude Code and OpenCode CLIs installed in the test environment. CI must install both; local developers are assumed to already have them.

## Validation Strategy

End-to-end validation lives in the harness itself: once M1's catalog and M2+'s implementation milestones land, the harness's own test count + green CI on macOS + Linux is the validation. Windows validation is its own milestone in M2+.

The user (PRD owner) does final pre-PR sign-off per `feedback_validate_pre_pr.md`: not stopping per-milestone for end-to-end testing, single validation pass before the PR.
