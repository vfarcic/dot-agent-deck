# PRD #116 — M3.2 Validation (post-edit regeneration)

This is the Phase-3 validation pass. After the four catalogued patterns (P1–P4) were landed in `assets/config_gen_prompt.md` and `assets/roles.toml` (M3.1), every project's baseline was regenerated a **second time** with the *updated* prompt and diffed against the user-improved config again. This document records the residual gap per project and the per-region delta versus the pre-edit (Phase-2) baseline.

**Headline (honest verdict): the PRD's "≥3 projects × ≥2 structured regions" target is NOT met under a consistent taxonomy — it is met for 2 projects (`dot-agent-deck`, `dot-ai`), one short of 3.** All four edits landed and fire reliably (see "Per-pattern effectiveness"), but because every edit concentrates in the single `[[orchestrations.roles]]` region and two of the candidate projects (`dot-ai-cli`, `youtube-automation`) already had the matching role *roster* before the edits, only two projects improve in two independent region-axes once `reactive_panes` nondeterminism is excluded uniformly. The full accounting is in "Counting taxonomy" and "Headline verdict" below; this is reported as-is, not massaged to hit the target.

> **Accepted outcome.** The strict PRD criterion — **≥3 projects improving in ≥2 structured regions** — was **MET for 2 projects** (`dot-agent-deck`, `dot-ai`) and not the required 3. The other three candidates fell short for documented *structural* reasons, not because the edits failed: `dot-ai-cli` and `youtube-automation` already matched the user role roster in v1, so only the template-content axis was left to improve (one region, not two); `dot-ai-infra` is excluded as date-gate-confounded (its user config predates the orchestration-proposing prompt, so it carries no orchestration/role signal to compare against). **This 2-project result was reviewed and accepted by the author as the proven slice for this pass** — consistent with the PRD's own "ship after one full pass through Phase 3" risk guidance (subsequent passes are separate, smaller exercises). All four catalogued patterns (P1–P4) landed and reproduce reliably, so the PRD's qualitative goal ("less in need of hand-editing") is advanced. Follow-up issue [#183](https://github.com/vfarcic/dot-agent-deck/issues/183) tracks re-running this analysis against the strict ≥3-project metric as more hand-improved projects accumulate.

## Engine (identical to Phases 1–2 — required for a valid comparison)

The v2 baselines were regenerated with the **same engine** as the pilot and Phase 2, with **only the prompt asset changed**: the authenticated **`claude` CLI single-shot** (`claude -p`), all filesystem/shell tools disabled (`--disallowedTools Bash Read Edit Write Glob Grep WebFetch WebSearch Task TodoWrite NotebookEdit`), the project laid out inline (the unchanged `project-snapshot.md`), model pinned to **`claude-haiku-4-5`** (the deck default). No Anthropic-API path and no Sonnet baseline, exactly as before. The only difference between the v1 and v2 runs is the rendered prompt: `rendered-prompt-v2.md` carries the P1–P4 edits and `rendered-prompt.md` does not. Per-project v2 artifacts live under `analysis/prd-116/<project>/`: `rendered-prompt-v2.md`, `baseline-input-v2.md` (= `rendered-prompt-v2.md` + `capture-appendix.md` + `project-snapshot.md`, identical assembly to v1), `baseline-v2-raw-output.md`, `baseline-v2.toml` (the extracted config, all five parse as a valid `ProjectConfig`), and `diff-v2.md`.

> **Engine choice — deliberate, and explicitly signed off by the user.** Using the **`claude` CLI single-shot** is a *deliberate, user-confirmed* deviation from PRD decision #1's original wording ("send the rendered prompt to the **Anthropic API** … pin model + temperature, use temperature 0 for determinism"). The user explicitly approved the CLI engine before the pilot because it is **more faithful to how the deck actually generates configs**: the deck feeds the rendered prompt to a live `claude`/`opencode` **agent pane** (`send_config_gen_prompt`) — it does **not** call the `src/llm.rs` Anthropic API client for config-gen. The trade-off this choice accepts is that the `claude` CLI **exposes no temperature flag, so temperature 0 is not pinnable**. Determinism was therefore not *forced* but **checked empirically over repeated runs** with the model pinned to `claude-haiku-4-5`, and backed by the PRD's cross-project-recurrence rule (a delta seen in 2+ independent projects is stronger evidence than N re-runs of one). This is a recorded decision, not a fallback or an oversight.

## Counting taxonomy (defined up front, applied uniformly)

The PRD success criterion is: after the edits, regenerating the baseline for **at least 3 of the analyzed projects** yields a config with a **smaller diff in at least 2 of the structured regions**. To judge that honestly, the counting rules are fixed *before* looking at results, and applied identically to every project.

1. **The structured regions** are decision #2's 8-region taxonomy: `init_command`, `reactive_panes`, `seed_prompt`, `[[modes]]`, `[[modes.panes]]`, `[[modes.rules]]`, `[[orchestrations]]`, `[[orchestrations.roles]]`.

2. **A region "improves"** for a project iff its v1→v2 diff against the user config got *strictly smaller* — a disagreement (`✗`) became agreement (`✓`), or a U-only / B-only item was resolved. **A region that already agreed in v1 cannot "improve"**: there is no diff left to shrink. This matters below for `dot-ai-cli` and `youtube-automation`, whose role rosters already matched the user before the edits.

3. **`reactive_panes` is EXCLUDED from the improved-region count for every project, no exception.** It is a single integer (the reactive-slot count) that the no-temperature `claude` engine emits with run-to-run nondeterminism, and **none of P1–P4 touches `reactive_panes` guidance** — the prompt's slot-count coupling (catalogue C3) was deliberately left unchanged because the catalogue confirmed the generator already gets it right. In this very batch the count moved *toward* the user for `dot-ai-cli` (2→3) and *away* for `dot-ai` (2→3 vs user 2) — opposite directions, same prompt, same run — the signature of noise. Counting it as an improvement where it happened to flip the right way (the earlier draft of this doc did, for `dot-ai-cli`) while dismissing it as noise where it didn't (for `dot-ai`) is exactly the inconsistency this taxonomy exists to prevent. So it is counted **neither as an improvement nor as a regression**, anywhere.

4. **`[[orchestrations.roles]]` is read at role granularity along two distinct axes — this split is load-bearing, so it is stated and justified here, not assumed.** All four catalogued edits (P1–P4) deliberately target this one region. At the **coarse 8-region taxonomy** that means the edits move *only one region* for every project, and **no project could ever reach "2 regions improved"** — the coarse reading is too blunt to measure what the edits did. The region is split because it genuinely bundles two independent kinds of change:
   - **Roster axis** — *which* roles the orchestration contains (P2 adds `tester`, P3 adds `auditor`). Measured by the count of U-only roles (roles the user keeps that the baseline lacks).
   - **Template-content axis** — *what the roles say* (P1's two-phase release + mandatory pre-release gate, P4's exact test command). Measured by whether the role `prompt_template`s gained the user-aligned structure they previously lacked.

   These are independent: a baseline can match the user's roster while every template is generic (`dot-ai-cli` v1), or carry a thin roster with rich templates. **A project counts as "2 regions improved" only if BOTH axes improved** — its roster got closer *and* its role templates got closer. We do **not** sub-divide further (e.g. treating each role's template as its own region): that would let a single coarse region masquerade as several, which is precisely the "implied coarse-region success it lacks" the reviewer warned against. The headline verdict therefore does **not** rely on per-role over-counting.

5. **`prompt_template` equality is judged by structure, not byte-equality.** A project-tuned template never byte-matches the user's, so the diff tool always flags it `✗`; the signal is whether the v2 template gained the specific user-aligned feature it lacked (two-phase release, mandatory gate, RED/GREEN chain, exact test command), read from the prompts (quoted below), not from the flag.

6. **Cosmetic-name normalization for pairing.** The diff tool matches modes/orchestrations **by name**, and the generator picks those names nondeterministically (the prompt intentionally does not dictate them). Where the v2 name differed from the user's, `diff-v2.md` was generated against a copy with only the cosmetic mode/orchestration `name` aligned to the user's (a note at the top of each such file records the substitution); the authentic output is preserved in `baseline-v2.toml` / `baseline-v2-raw-output.md`. `dot-ai-cli` paired natively.

7. **Date gate (C4) carried forward.** `dot-ai-infra` (pilot) predates the orchestration-proposing prompt, so its orchestration/role region carries no user signal and is **excluded** from the comparison — reported for completeness only.

## Headline verdict — target NOT met (2 of the required 3 projects)

Under the taxonomy above (`reactive_panes` excluded everywhere; `[[orchestrations.roles]]` read as roster + template-content; a region must have had a v1 diff to "improve"), the per-project tally is:

| Project | Roster axis (U-only roles, v1→v2) | Template-content axis | Regions improved (excl. `reactive_panes`) | ≥2 regions? |
|---|---|---|---|---|
| `dot-agent-deck` | **2 → 0** ✅ improved | release + gate + coder cmd ✅ improved | **2** | **✅** |
| `dot-ai` | **3 → 1** ✅ improved | tester TDD chain + release + coder cmd ✅ improved | **2** | **✅** |
| `dot-ai-cli` | 0 → 0 (already matched in v1 — no diff to shrink) | release + gate + coder cmd ✅ improved | **1** | **❌** |
| `youtube-automation` | 0 → 0 (already matched in v1 — no diff to shrink) | release ✅ improved | **1** | **❌** |
| `dot-ai-infra` (pilot) | confounded (date gate) — excluded | confounded — excluded | — | — |

**So the criterion is satisfied for `dot-agent-deck` and `dot-ai` only — 2 projects, one short of the required 3.**

**Why `dot-ai-cli` is no longer the third project.** An earlier draft counted `dot-ai-cli` as meeting ≥2 by treating its `reactive_panes` flip (✗→✓, 2→3) as the second region. Under rule 3 that flip is excluded as nondeterminism — the identical flip went the *wrong* way for `dot-ai` in the same batch. With `reactive_panes` excluded, `dot-ai-cli` improves only on the template-content axis; its roster already matched the user in v1 (all of `orchestrator/coder/reviewer/auditor/release` present), so there was no roster diff to shrink. `youtube-automation` is the same shape: roster already matched, only the `release` template improved.

**The only ways to reach a third project both fail the taxonomy:** (a) counting nondeterministic `reactive_panes` flips (rejected by rule 3, and arbitrary — the same axis regressed `dot-ai`), or (b) sub-dividing `[[orchestrations.roles]]` per-role so `dot-ai-cli`'s three improved templates count as "≥2 regions" (rejected by rule 4 as coarse-region masquerade). Under the strict coarse-8 reading the target is met for **0** projects; under the principled roster-vs-template reading it is met for **2**. It is met for 3 only under the most generous per-role split. The honest verdict is therefore **2 projects, criterion not met.**

**What the edits DID achieve (real, just not the 3×2 bar).** All four patterns landed and reproduce reliably (see "Per-pattern effectiveness"): every catalogued project's release flow gained the two-phase / stop-before-merge shape and a mandatory pre-release gate; `tester` and `auditor` now appear exactly where the user keeps them; the exact project test command is named in `coder`/`tester`. The generator is meaningfully closer to the user-improved configs — the qualitative "less in need of hand-editing" goal of the PRD is advanced. The strict region-count metric simply isn't reachable for a 3rd project by edits that all concentrate in one coarse region when two candidates already had the matching roster.

## Cross-project region delta (v1 baseline → v2 baseline, vs user)

"U-only roles" = roles the user has that the baseline lacks (lower is closer). ✓/✗ = agrees/disagrees with the user config.

| Region / signal | dot-agent-deck | dot-ai | dot-ai-cli | youtube | dot-ai-infra |
|---|---|---|---|---|---|
| `init_command` | ✓ → ✓ | ✓ → ✓ | ✓ → ✓ | (no mode) | ✓ → ✓ |
| `reactive_panes` **(EXCLUDED — noise, not counted either way)** | ✓ → ✓ (2/2) | ✓ → ✗ (2→3) | ✗ → ✓ (2→3) | n/a | ✗ → ✗ (3 vs 2) |
| persistent panes | 1/1 → 1/1 | 1/1 → 1/1 | 1/1 → 1/1 | 1 → 1 (user 0) | 2 → 2 (user 1) |
| **roster axis — U-only roles** | **2 → 0** ✅ | **3 → 1** ✅ | 0 → 0 (already matched) | 0 → 0 (already matched) | (confounded) |
| `auditor` present (P3) | ✗ → **✓** | ✗ → **✓** | ✓ → ✓ | ✓ → ✓ | ✗ → ✓\* |
| `tester` + TDD chain (P2) | ✗ → **✓** | ✗ → **✓** | ✗ → ✗ (user ✗) | ✗ → ✗ (user ✗) | ✗ → ✗ (user ✗) |
| `release` two-phase / stop-before-merge (P1) | ✗ → **✓** | ✗ → **✓** | ✗ → **✓** | partial → **✓** | n/a (confounded) |
| orchestrator mandatory pre-release gate (P1) | ✗ → **✓** | ✓ → ✓ | ✗ → **✓** | ✓ → ✓ | n/a |
| coder/tester names **exact** test cmd (P4) | ✗ → **✓** | ✗ → **✓** | partial → **✓** | n/a (no test toolchain) | n/a |
| **template-content axis improved?** | ✅ | ✅ | ✅ | ✅ | (confounded) |
| **counts as ≥2 regions?** | ✅ (roster + template) | ✅ (roster + template) | ❌ (template only) | ❌ (template only) | — |

\* `dot-ai-infra` gained an `auditor` (P3 fired), but the user config has **no orchestration at all** (time-drift-confounded), so this does not reduce a catalogued diff — reported, not counted.

## Per-project residual gap

### `dot-agent-deck` — **2 regions improved ✅** (roster + template; strongest result)

- **Roster axis** went from `{orchestrator, coder, reviewer, release}` (2 U-only roles: `auditor`, `tester`) to **exactly the user's set** `{orchestrator, tester, coder, reviewer, auditor, release}` — 0 U-only roles. P2 and P3 both fired. **(roster axis improved)**
- **`release` (P1):** v1 said *"open a PR, pass CI, merge, tag, close the issue"* (auto-merge). v2 is a two-phase worker that opens the PR, **waits for CI + Greptile review (polling ~5 min for the `greptile-apps` comment), reports a categorized findings summary, and STOPS — Phase 2 merges only after the orchestrator re-delegates with explicit go-ahead** — mirroring the user's hand-written release role. **(template axis improved)**
- **`orchestrator` (P1 + P2):** v1 said *"5. Delegate release to release (PR, merge, tag, close issue)"* — no gate. v2 wires a `tester → coder → tester` RED/GREEN chain, runs reviewer + auditor in parallel, and adds the mandatory pre-release STOP-for-confirmation gate.
- **`coder`/`tester` (P4):** v2 names `cargo test-fast <test-name>` for the RED/GREEN step rather than a generic "run the tests".
- **Residual gap:** the orchestrator `prompt_template` is still shorter than the user's (16 vs 46 lines) and omits some project-specific lore (the `#[spec]`/scenario-comment discipline, the e2e gating). Cosmetic mode/orchestration names differ. Expected — the prompt produces a strong scaffold, not a verbatim copy.

### `dot-ai` — **2 regions improved ✅** (roster + template)

- **Roster axis** went from 3 U-only roles (`auditor`, `tester`, `documenter`) to 1 (`documenter` only). P2 and P3 fired; `documenter` correctly stays unproposed (an n=1, uncatalogued role). **(roster axis improved)**
- **`tester` + TDD chain (P2):** v2 adds a `tester` role and an integration-test RED/GREEN chain in the orchestrator, matching the user's integration-test discipline. **(template axis improved)**
- **`release` (P1):** v2 reproduces the wait-for-CI-and-reviews-then-STOP two-phase shape.
- **`coder`/`tester` (P4):** v2 names `npm run test:integration` and the `./tmp/test-output.log` redirect convention.
- **`reactive_panes` (EXCLUDED):** moved 2 → 3 (user is 2), so the raw flag flips ✓ → ✗. This is **not** counted as a regression (nor would the opposite flip be counted as an improvement) — see the taxonomy (rule 3) and "Regressions". None of P1–P4 touches `reactive_panes`.
- **Residual gap:** `documenter` still unproposed (intended); `reactive_panes` overshoot (excluded noise).

### `dot-ai-cli` — **1 region improved ❌** (template only; does NOT meet ≥2)

- **`release` (P1):** generic auto-merge → two-phase / stop-before-merge. **(template axis improved)**
- **orchestrator pre-release gate (P1):** absent → present (mandatory STOP-for-confirmation).
- **`coder` (P4):** v2 names the `task test` command and the `tmp/test-output.txt` redirect convention rather than a generic instruction.
- **Roster axis:** already matched the user in v1 (all of `orchestrator/coder/reviewer/auditor/release` present, 0 U-only roles), so there was **no roster diff to shrink** — the edits correctly did not over-add a `tester` the user doesn't keep, but that means the roster axis cannot count as "improved".
- **`reactive_panes` (EXCLUDED):** ✗ → ✓ (2 → 3, matching the user's 3). A real flip toward the user this run, but it is run-to-run noise (the same axis regressed `dot-ai` in the same batch) and **is not counted** under rule 3. This is the change that demotes `dot-ai-cli` from the earlier "2 regions" claim to **1 region**.
- **Verdict:** improves on exactly **one** structured region (template content). Does not meet the ≥2-region bar.

### `youtube-automation` — **1 region improved ❌** (template only; secondary)

- **`release` (P1):** v2 picks up the two-phase / wait-then-stop shape; the orchestrator already carried a pre-release gate in v1 (and still does). **(template axis improved)**
- **Roster axis:** already matched (`auditor` present both runs; the user keeps no `tester` and v2 correctly proposes none) — no roster diff to shrink.
- The user's config is **modes-0** (orchestration only); the baseline keeps a mode (an n=1 structural choice), so the `[[modes]]` region stays divergent as before. No region regressed from the edits.
- **Verdict:** improves on **one** structured region (template content). Does not meet the ≥2-region bar.

### `dot-ai-infra` (pilot) — confounded, reported only

- The user config has **no orchestration** and predates the orchestration-proposing prompt (date gate C4), so its orchestration/role region carries no user signal and is excluded. P3 did fire (v2 baseline now includes an `auditor`), confirming the edit is active, but it does not reduce a catalogued diff here. `reactive_panes` (3 vs user 2) and the second persistent pane are unchanged from v1 — both pre-existing, non-edit-related, n=1 pilot idiosyncrasies.

## Per-pattern effectiveness

- **P1 (release human-merge gate + two-phase release) — landed and effective.** v1: 0/5 baselines had any stop-before-merge wording (all reproduced the library's "open a PR, merge, tag"). v2: all four catalogued projects produce a two-phase release that waits for CI + automated review and STOPS before merge, and the orchestrator carries a mandatory pre-release human gate. This is the primary, universal pattern and it now reproduces reliably.
- **P2 (tester role + RED/GREEN chain) — landed and precisely targeted.** `tester` appeared in 0/5 v1 baselines. In v2 it appears in **exactly** the two test-mandatory projects (`dot-agent-deck`, `dot-ai`) and is correctly **not** proposed for the three projects whose users keep no tester. The concrete `CLAUDE.md`/test-mandatory trigger replaced the vague "TDD signals" phrasing the haiku generator never recognized.
- **P3 (auditor default-on) — landed and effective.** `auditor` went from 2/5 v1 baselines to **5/5** v2 baselines. As flagged in the catalogue, roster composition is the least-deterministic axis, so this is a directional signal backed by the 4/4 user-retention finding — and the v2 result is unanimous, consistent with that rationale.
- **P4 (exact test/lint command + output convention) — landed and effective.** The three projects with a real test toolchain (`dot-agent-deck`, `dot-ai`, `dot-ai-cli`) now name their exact command (`cargo test-fast`, `npm run test:integration`, `task test`) and output-redirect convention in the `coder`/`tester` templates rather than a generic "run the tests".

**Note on the gap between "patterns effective" and "3×2 target met".** P1–P4 demonstrably work — the qualitative improvement is real and visible above. The 3×2 *region-count* target is a stricter, structural metric, and it is not reached for a 3rd project because (a) all four edits land in the single `[[orchestrations.roles]]` region, so per-project improvement is concentrated in one coarse region (two axes at most), and (b) two of the four candidate projects already had the matching roster, leaving them only one axis to improve. This is a limitation of the metric-vs-edit shape, not evidence the edits are wrong.

## Regressions (flagged per PRD risk policy)

**No edit-induced regression.** The one region whose raw flag moved the "wrong" way — `dot-ai` `reactive_panes` (2 → 3, user is 2) — is **excluded under the counting taxonomy (rule 3)** and is therefore neither a regression nor counted: none of P1–P4 touches `reactive_panes` or rule-count guidance (the C3 coupling was deliberately left unchanged because the catalogue confirmed the generator already gets it right), and the *same* axis flipped the **other** way for `dot-ai-cli` (2 → 3, *toward* the user) in the same batch — the signature of run-to-run nondeterminism on the no-temperature haiku engine, not of a wrong edit. The PRD's suspect-catalogue-entry rule does not apply: no catalogue entry drove this delta. No edit should be rolled back.

## Reproduction

See `docs/develop/config-gen-regeneration.md` for the full reproducible procedure (tooling locations, the exact `claude -p` invocation, authoring a new project's capture from scratch, where outputs go, and the date-gate methodology).
