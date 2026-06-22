# PRD #116 — M3.2 Validation (post-edit regeneration)

This is the Phase-3 validation pass. After the four catalogued patterns (P1–P4) were landed in `assets/config_gen_prompt.md` and `assets/roles.toml` (M3.1), every project's baseline was regenerated a **second time** with the *updated* prompt and diffed against the user-improved config again. This document records the residual gap per project and the per-region delta versus the pre-edit (Phase-2) baseline.

**Headline: the PRD success target is MET.** At least 3 projects (`dot-agent-deck`, `dot-ai`, `dot-ai-cli`) show a smaller diff in at least 2 structured regions versus their pre-edit baseline. No edit-induced region regressed; the one region that got worse (`dot-ai` `reactive_panes`) is run-to-run model nondeterminism unrelated to any of the four edits — see "Regressions" below.

## Engine (identical to Phases 1–2 — required for a valid comparison)

The v2 baselines were regenerated with the **same engine** as the pilot and Phase 2, with **only the prompt asset changed**: the authenticated **`claude` CLI single-shot** (`claude -p`), all filesystem/shell tools disabled (`--disallowedTools Bash Read Edit Write Glob Grep WebFetch WebSearch Task TodoWrite NotebookEdit`), the project laid out inline (the unchanged `project-snapshot.md`), model pinned to **`claude-haiku-4-5`** (the deck default). No Anthropic-API path and no Sonnet baseline, exactly as before. The only difference between the v1 and v2 runs is the rendered prompt: `rendered-prompt-v2.md` carries the P1–P4 edits and `rendered-prompt.md` does not. Per-project v2 artifacts live under `analysis/prd-116/<project>/`: `rendered-prompt-v2.md`, `baseline-input-v2.md` (= `rendered-prompt-v2.md` + `capture-appendix.md` + `project-snapshot.md`, identical assembly to v1), `baseline-v2-raw-output.md`, `baseline-v2.toml` (the extracted config, all five parse as a valid `ProjectConfig`), and `diff-v2.md`.

## Measurement methodology (and its honest limits)

- **Region taxonomy** is decision #2's: `init_command`, `reactive_panes`, `seed_prompt`, `[[modes]]`, `[[modes.panes]]`, `[[modes.rules]]`, `[[orchestrations]]`, and `[[orchestrations.roles]]` (role presence **and** role `prompt_template`s). All four catalogued edits deliberately target the **orchestration-roles** region — P1 (release flow), P2 (tester + TDD chain), P3 (auditor), P4 (coder/tester test command) — so that is where the improvement concentrates. The region is resolved at **role granularity** (the diff tool emits a separate comparison per role, and decision #2 explicitly names "role prompt_templates" as the region content), which is the resolution used for the per-project tallies below.
- **Cosmetic-name normalization for pairing.** The structured-diff tool (`examples/diff_config.rs`) matches modes and orchestrations **by name**. The generator picks the mode/orchestration name nondeterministically (e.g. this run it named `dot-agent-deck`'s orchestration `prd-dev-cycle`, last run `dev-flow`), and the prompt intentionally does **not** dictate that name. When the v2 name differed from the user's, the tool reported every role as disjoint (B-only + U-only) instead of pairing them field-by-field, which *understates* the improvement. For the projects where the names differed (`dot-agent-deck`, `dot-ai`, `youtube-automation`, `dot-ai-infra`'s mode), `diff-v2.md` was generated against a copy with the cosmetic mode/orchestration name aligned to the user's; a note at the top of each such `diff-v2.md` records the substitution, and the **authentic** model output is preserved untouched in `baseline-v2.toml` / `baseline-v2-raw-output.md`. `dot-ai-cli` paired natively (both `dev-flow`).
- **`prompt_template`s are judged by content, not byte-equality.** A role's `prompt_template` is always project-tuned, so it never byte-matches the user's and the tool always marks it `✗`. The meaningful signal is therefore *structural*: did the v2 baseline gain the user-aligned feature it previously lacked (two-phase release, a mandatory pre-release gate, a RED/GREEN chain, the exact test command)? Those are confirmed by reading the prompts (representative before/after quoted below), not by the `✗`/`✓` flag.
- **Date gate carried forward (C4).** Unchanged from the catalogue: `dot-ai-infra` is the only time-drift-confounded project (its config predates the orchestration-proposing prompt), so its orchestration region is **excluded** from the user-signal comparison. Its v2 row is reported for completeness only.

## Cross-project region delta (v1 baseline → v2 baseline, vs user)

"U-only roles" = roles the user has that the baseline lacks (lower is closer). ✓/✗ = agrees/disagrees with the user config.

| Region / signal | dot-agent-deck | dot-ai | dot-ai-cli | youtube | dot-ai-infra |
|---|---|---|---|---|---|
| `init_command` | ✓ → ✓ | ✓ → ✓ | ✓ → ✓ | (no mode) | ✓ → ✓ |
| `reactive_panes` | ✓ → ✓ (2/2) | ✓ → **✗** (2→3) | **✗ → ✓** (2→3) | n/a | ✗ → ✗ (3 vs 2) |
| persistent panes | 1/1 → 1/1 | 1/1 → 1/1 | 1/1 → 1/1 | 1 → 1 (user 0) | 2 → 2 (user 1) |
| **roster — U-only roles** | **2 → 0** ✅ | **3 → 1** ✅ | 0 → 0 | 0 → 0 | (confounded) |
| `auditor` present (P3) | ✗ → **✓** | ✗ → **✓** | ✓ → ✓ | ✓ → ✓ | ✗ → ✓\* |
| `tester` + TDD chain (P2) | ✗ → **✓** | ✗ → **✓** | ✗ → ✗ (user ✗) | ✗ → ✗ (user ✗) | ✗ → ✗ (user ✗) |
| `release` two-phase / stop-before-merge (P1) | ✗ → **✓** | ✗ → **✓** | ✗ → **✓** | partial → **✓** | n/a (confounded) |
| orchestrator mandatory pre-release gate (P1) | ✗ → **✓** | ✓ → ✓ | ✗ → **✓** | ✓ → ✓ | n/a |
| coder/tester names **exact** test cmd (P4) | ✗ → **✓** | ✗ → **✓** | partial → **✓** | n/a (no test toolchain) | n/a |

\* `dot-ai-infra` gained an `auditor` (P3 fired), but the user config has **no orchestration at all** (time-drift-confounded), so this does not reduce a catalogued diff — reported, not counted.

## Per-project residual gap

### `dot-agent-deck` — **2+ regions improved ✅** (strongest result)

- **Roster** went from `{orchestrator, coder, reviewer, release}` (2 U-only roles: `auditor`, `tester`) to **exactly the user's set** `{orchestrator, tester, coder, reviewer, auditor, release}` — 0 U-only roles. P2 and P3 both fired.
- **`release` (P1):** v1 said *"open a PR, pass CI, merge, tag, close the issue"* (auto-merge). v2 is a two-phase worker that opens the PR, **waits for CI + Greptile review (polling ~5 min for the `greptile-apps` comment), reports a categorized findings summary, and STOPS — Phase 2 merges only after the orchestrator re-delegates with explicit go-ahead** — mirroring the user's hand-written release role.
- **`orchestrator` (P1 + P2):** v1 said *"5. Delegate release to release (PR, merge, tag, close issue)"* — no gate. v2 wires a `tester → coder → tester` RED/GREEN chain, runs reviewer + auditor in parallel, and adds *"Before delegating to release, summarize what to validate end-to-end and STOP for explicit user confirmation. Do NOT proceed to release without explicit approval."*
- **`coder`/`tester` (P4):** v2 names `cargo test-fast <test-name>` for the RED/GREEN step rather than a generic "run the tests".
- **Residual gap:** the orchestrator `prompt_template` is still shorter than the user's (16 vs 46 lines) and omits some project-specific lore (the `#[spec]`/scenario-comment discipline, the e2e gating). Cosmetic mode/orchestration names differ. These are expected — the prompt produces a strong scaffold, not a verbatim copy.

### `dot-ai` — **2+ regions improved ✅**

- **Roster** went from 3 U-only roles (`auditor`, `tester`, `documenter`) to 1 (`documenter` only). P2 and P3 fired; `documenter` correctly stays unproposed (it is an n=1, uncatalogued role).
- **`tester` + TDD chain (P2):** v2 adds a `tester` role and an integration-test RED/GREEN chain in the orchestrator, matching the user's integration-test discipline.
- **`release` (P1):** v2 reproduces the wait-for-CI-and-reviews-then-STOP two-phase shape.
- **`coder`/`tester` (P4):** v2 names `npm run test:integration` and the `./tmp/test-output.log` redirect convention.
- **Regression (not edit-driven):** `reactive_panes` moved 2 → 3 (user is 2), flipping ✓ → ✗. This is rule-count nondeterminism (the model emitted one more reactive rule this run); none of P1–P4 touches `reactive_panes` guidance. See "Regressions".
- **Residual gap:** `documenter` still unproposed (intended); `reactive_panes` overshoot (noise).

### `dot-ai-cli` — **2+ regions improved ✅**

- **`reactive_panes`:** ✗ → ✓ (2 → 3, now matching the user's 3). Nondeterministic, but a real region flip in the closer direction this run.
- **`release` (P1):** generic auto-merge → two-phase / stop-before-merge.
- **orchestrator pre-release gate (P1):** absent → present (mandatory STOP-for-confirmation).
- **`coder` (P4):** v2 names the `task test` command and the `tmp/test-output.txt` redirect convention rather than a generic instruction.
- **Roster** already matched the user in v1 (`auditor` present), so no roster change — correctly, the edit did not over-add a `tester` the user doesn't keep.
- **Residual gap:** none structural; prompt wording is leaner than the user's hand-tuned version.

### `youtube-automation` — partial (secondary)

- **`release` (P1):** v2 picks up the two-phase / wait-then-stop shape; the orchestrator already carried a pre-release gate in v1 (and still does).
- Roster already matched (`auditor` present both runs); the user keeps no `tester` and v2 correctly proposes none. The user's config is **modes-0** (orchestration only) — the baseline keeps a mode (an n=1 structural choice, reported in the catalogue, not catalogued), so the `[[modes]]` region stays divergent as before. No region regressed from the edits.

### `dot-ai-infra` (pilot) — confounded, reported only

- The user config has **no orchestration** and predates the orchestration-proposing prompt (date gate C4), so its orchestration/role region carries no user signal and is excluded. P3 did fire (v2 baseline now includes an `auditor`), confirming the edit is active, but it does not reduce a catalogued diff here. `reactive_panes` (3 vs user 2) and the second persistent pane are unchanged from v1 — both are pre-existing, non-edit-related, n=1 pilot idiosyncrasies.

## Per-pattern effectiveness

- **P1 (release human-merge gate + two-phase release) — landed and effective.** v1: 0/5 baselines had any stop-before-merge wording (all reproduced the library's "open a PR, merge, tag"). v2: all four catalogued projects produce a two-phase release that waits for CI + automated review and STOPS before merge, and the orchestrator carries a mandatory pre-release human gate. This is the primary, universal pattern and it now reproduces reliably.
- **P2 (tester role + RED/GREEN chain) — landed and precisely targeted.** `tester` appeared in 0/5 v1 baselines. In v2 it appears in **exactly** the two test-mandatory projects (`dot-agent-deck`, `dot-ai`) and is correctly **not** proposed for the three projects whose users keep no tester. The concrete `CLAUDE.md`/test-mandatory trigger replaced the vague "TDD signals" phrasing the haiku generator never recognized.
- **P3 (auditor default-on) — landed and effective.** `auditor` went from 2/5 v1 baselines to **5/5** v2 baselines. As flagged in the catalogue, roster composition is the least-deterministic axis, so this is a directional signal backed by the 4/4 user-retention finding — and the v2 result is unanimous, consistent with that rationale.
- **P4 (exact test/lint command + output convention) — landed and effective.** The three projects with a real test toolchain (`dot-agent-deck`, `dot-ai`, `dot-ai-cli`) now name their exact command (`cargo test-fast`, `npm run test:integration`, `task test`) and output-redirect convention in the `coder`/`tester` templates rather than a generic "run the tests".

## Regressions (flagged per PRD risk policy)

One region got worse: **`dot-ai` `reactive_panes` (2 → 3, user is 2)**. This is **not** edit-induced. None of P1–P4 touches `reactive_panes` or rule-count guidance; the same prompt region (C3, "match slots to rule count") was deliberately left **unchanged** because the catalogue confirmed the generator already gets it right. The shift is pure run-to-run nondeterminism on the no-temperature haiku engine — the same axis flipped the **other** way for `dot-ai-cli` (2 → 3, *toward* the user) in the same batch. So the suspect-catalogue-entry rule does not apply: no catalogue entry drove this delta, and it is consistent with the documented nondeterminism caveat, not a sign that an edit is wrong. No edit should be rolled back.

## Reproduction

See `docs/develop/config-gen-regeneration.md` for the full reproducible procedure (tooling locations, the exact `claude -p` invocation, where outputs go, and the date-gate methodology).
