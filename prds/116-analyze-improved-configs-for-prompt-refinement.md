# PRD #116: Analyze user-improved `.dot-agent-deck.toml` configs to refine the config-gen prompt

**Status**: Planning
**Priority**: Medium
**Created**: 2026-05-25
**GitHub Issue**: [#116](https://github.com/vfarcic/dot-agent-deck/issues/116)
**Related**: `assets/config_gen_prompt.md`, `assets/roles.toml`, `src/config_gen.rs`

## Validation refresh (2026-06-14)

Re-validated against current code — verdict: **current, with one minor schema gap**. The referenced assets and code all exist and work as described: `assets/config_gen_prompt.md` (embedded via `include_str!` in `src/config_gen.rs`), `assets/roles.toml`, and the Anthropic-by-default `src/llm.rs`. The structured-diff taxonomy should gain two regions the per-mode schema grew since this PRD: `seed_prompt` (optional per-mode seeding prompt, added by PRD #127) and `reactive_panes` (pane slot count) — both are real fields user-improved configs may set differently from the AI baseline.

## Problem Statement

dot-agent-deck ships an AI config generator: an agent reads a project, follows the prompt in `assets/config_gen_prompt.md`, and writes an initial `.dot-agent-deck.toml`. That initial config is rarely the final one. In every project where the author actually uses the deck (`dot-ai/`, `dot-ai-infra/`, `youtube-automation/`, `dot-agent-deck/` itself, and the worktrees forked from it) the config has been edited by hand — sometimes lightly, sometimes substantially — to make modes, panes, rules, and orchestrations actually fit the work.

Each of those edits is a small piece of evidence about what the prompt and role library get wrong. Today that evidence is:

- **Locked inside individual repos.** Each project has its own improved file. Nobody is reading them as a set.
- **Lost as soon as the prompt is updated.** When `assets/config_gen_prompt.md` or `assets/roles.toml` change, the "baseline" that those user edits diverged from also changes, and the diff loses its meaning unless we captured it.
- **Not informing the prompt.** Improvements to the generator come from intuition and one-off pain points, not from systematic review of what users actually do post-generation.

The result is a generator that keeps making the same kinds of mistakes — missing init commands, weak orchestration shapes, generic pane choices, role prompts that don't reflect how the role is actually used — even though there are working counter-examples sitting in sibling repos.

## Solution Overview

Treat the set of hand-improved configs as a **training set for prompt engineering**. For each project where a `.dot-agent-deck.toml` has been edited beyond the AI-generated baseline:

1. **Reconstruct the baseline** the AI would generate *today*, given the current prompt and role library, against that project.
2. **Diff the baseline against the user-improved config** along structured axes (init commands, persistent panes, reactive rules, orchestrations, role prompt templates).
3. **Aggregate patterns across projects.** Surface recurring deltas — things the user adds, removes, restructures, or rewords every time.
4. **Translate patterns into prompt and role-library edits.** Push the durable lessons back into `assets/config_gen_prompt.md` and `assets/roles.toml`.
5. **Validate** by regenerating the baseline for each project after the prompt edits and confirming that the new baseline is meaningfully closer to the user-improved version.

The deliverable is **not a new feature in the deck binary**. It is a repeatable analysis workflow, plus a concrete round of prompt/role-library improvements that come out of it. The analysis itself can live as a small script (or just a documented, reproducible procedure) checked into the repo so it can be re-run as more projects accumulate.

## Scope

### In Scope

- **Inventory** of projects on the author's machine that have a `.dot-agent-deck.toml`, plus a quick triage of which have been meaningfully hand-edited vs. left as the AI generated them.
- **Baseline regeneration**: an executable procedure (script or documented `dot-agent-deck`/`claude` invocation) that runs the current `assets/config_gen_prompt.md` against a target project and emits the config the AI would produce today. Reproducible from any machine with the right credentials.
- **Structured diff** between baseline and improved config, broken down by config region (`init_command`, `[[modes]]`, `[[modes.panes]]`, `[[modes.rules]]`, `[[orchestrations]]`, `[[orchestrations.roles]]`).
- **Pattern catalogue**: a written summary of recurring deltas across projects, with examples and a hypothesis for the prompt change that would close each gap.
- **Prompt and role-library edits**: a targeted update to `assets/config_gen_prompt.md` and/or `assets/roles.toml` driven by the catalogue.
- **Validation regeneration**: re-run the baseline for each project after the edits, diff against the improved config again, and document the residual gap. Goal is "smaller and qualitatively different," not "zero."
- **Documented re-run procedure** so this can be repeated as more projects are added or as the prompt evolves.

### Out of Scope (this PRD)

- **Automated continuous evaluation** (e.g. CI that regenerates configs on every prompt change). Possible follow-up; here we just need it runnable on demand.
- **Cross-user data collection.** Only the author's projects. No telemetry, no sharing of others' configs.
- **Changes to the deck binary** (config generator UX, new CLI flags, etc.) unless a discovered pattern *requires* a binary change to be expressible in the prompt. If that happens, file a separate PRD.
- **Rewriting `roles.toml` from scratch.** Edits are targeted at observed gaps; we are not redesigning the role taxonomy here.
- **Synthetic projects / benchmarks.** Real configs only — the value is that they reflect real preferences.

## Success Criteria

- A reviewer can open the pattern catalogue and see, per pattern, (a) which projects exhibit it, (b) what the AI generates, (c) what the user wrote instead, and (d) a proposed prompt or role-library change.
- After the prompt/role-library edits land, regenerating the baseline for **at least 3 of the analyzed projects** produces a config that is qualitatively closer to the improved version than the pre-edit baseline (smaller diff in at least 2 of the structured regions). Documented in the validation section of the analysis output.
- The re-run procedure is reproducible: a fresh checkout + the documented steps regenerates the baselines and diffs without manual fixup.
- No regression in the existing `src/config_gen.rs` tests; if the prompt changes require updating assertion strings, those updates are intentional and called out in the commit message.
- `cargo fmt --check` and `cargo clippy -- -D warnings` pass.

## Open Questions (resolve during M1)

1. **How do we regenerate the baseline reliably?** The simplest path is to invoke the same prompt the deck would: `config_gen_prompt(project_dir)` rendered, then sent to an LLM with the project laid out. M1 picks a concrete invocation (Claude via API? `claude` CLI? `dot-agent-deck`'s own generator command if one exists?) and pins model + temperature so the baseline is reproducible.
2. **What does "structured diff" actually look like?** Probably parse both TOML files, normalize ordering, and compare by region. Alternative: a side-by-side rendering. M1 prototypes one project and we decide based on what reads best.
3. **Where does the analysis output live?** Candidates: a Markdown document under `docs/internal/` (not user-facing), a directory of per-project artifacts under `analysis/`, or attached to this PRD as appendices. Default: a directory under `analysis/` or similar, ignored from the published docs site.
4. **Single-project pilot first, or all projects at once?** Likely pilot on one project (probably `dot-ai-infra/` since it's the established testing reference per memory) to shake out tooling, then fan out.

## Milestones

### Phase 1: Tooling and pilot

- [ ] **M1.1** — Inventory hand-improved configs. Walk the author's `~/code/` directory, list every `.dot-agent-deck.toml`, classify each as "AI-generated, untouched" / "lightly edited" / "substantially edited". Drop worktree duplicates of the same logical project. Output: a small table in the analysis dir.
- [ ] **M1.2** — Define and implement the baseline regeneration procedure. Pin LLM provider, model, and any sampling parameters. Reproduce a baseline for one pilot project end-to-end. Capture the exact prompt sent (post-render) alongside the output for auditability.
- [ ] **M1.3** — Define the structured diff. Choose region taxonomy (`init_command`, persistent panes, reactive rules, orchestration roles, role `prompt_template`s, etc.) and produce a readable diff for the pilot project.
- [ ] **M1.4** — Pilot analysis: write up the pilot project's deltas with the proposed prompt changes they suggest. Use it to pressure-test the format before scaling.

### Phase 2: Fan out and catalogue

- [ ] **M2.1** — Regenerate baselines for the remaining substantially-edited projects identified in M1.1.
- [ ] **M2.2** — Produce per-project diff artifacts using the M1.3 format.
- [ ] **M2.3** — Aggregate into the pattern catalogue: each pattern names the gap, lists the projects that exhibit it, shows representative before/after snippets, and proposes a prompt or role-library edit.

### Phase 3: Apply prompt edits and validate

- [ ] **M3.1** — Land the prompt and role-library edits derived from the catalogue. Keep them targeted — one edit per identified pattern, with the catalogue entry referenced in the commit body. Update `src/config_gen.rs` tests only if the assertions need to track the change.
- [ ] **M3.2** — Regenerate baselines a second time using the updated prompt. Diff against the user-improved configs again. Document the residual gap per project.
- [ ] **M3.3** — Document the re-run procedure (where the scripts live, how to invoke them, where outputs go) so this analysis can be repeated as more projects are added.

### Phase 4: Wrap-up

- [ ] **M4.1** — Cross-check: confirm `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test` all pass.
- [ ] **M4.2** — Open follow-up issues for any pattern that surfaces but cannot be solved by a prompt change alone (e.g. requires a deck-binary feature). Do not bundle those into this PRD.

## Risks and Mitigations

- **Risk**: The user-improved configs diverge for reasons that aren't generalizable (one-off project quirks, in-flight experiments). **Mitigation**: a pattern only enters the catalogue if it appears in **two or more** independent projects, or is an obvious universal improvement (e.g. respecting `devbox.json`) regardless of count.
- **Risk**: Regenerating the baseline is non-deterministic because the underlying model output varies. **Mitigation**: pin model and (where supported) temperature; regenerate 2–3 times per project and only flag *stable* deltas. Note in the analysis dir whether a delta was stable across re-runs.
- **Risk**: The pattern catalogue grows but the prompt becomes a sprawling, contradictory document. **Mitigation**: each prompt edit is targeted; prefer adding a short imperative ("default `init_command` to ...") over adding examples. Re-read the rendered prompt as a whole after edits.
- **Risk**: Time sink. The temptation is to keep adding projects and patterns indefinitely. **Mitigation**: ship after one full pass through Phase 3. Subsequent passes are separate, smaller exercises.

## Dependencies

- The current set of hand-improved `.dot-agent-deck.toml` files in the author's local projects.
- Working access to the LLM used by the deck's config generator (Anthropic API by default, per `src/llm.rs`).
- No changes required from other projects or teams.

## Validation Strategy

Per-project, the validation step is the regeneration diff in M3.2 — concretely: the structured-region diff after the prompt edit should be smaller in at least two regions for at least three projects. If a region gets *worse* after the edit, the catalogue entry that drove that edit is suspect and gets rolled back or revised.

End-to-end, the analysis is a success if the next time the author scaffolds a brand-new project with the deck's generator, the output is noticeably less in need of hand-editing — which is a softer signal but the one this PRD ultimately exists for.
