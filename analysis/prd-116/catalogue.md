# PRD #116 — M2.3 Pattern Catalogue

Aggregates the per-project baseline-vs-user diffs (M2.2) across the pilot (`dot-ai-infra`) and the four Phase-2 projects (`dot-agent-deck`, `dot-ai`, `dot-ai-cli`, `youtube-automation`) into the recurring deltas worth pushing back into `assets/config_gen_prompt.md` and `assets/roles.toml`. Each catalogued pattern names the gap, lists the projects exhibiting it, shows representative AI-baseline-vs-user snippets, and proposes one targeted edit. **Phase 3 lands the edits — this document only proposes them.**

## Engine (identical to the pilot)

All five baselines were regenerated with the **same engine** so the cross-project comparison is valid: the authenticated **`claude` CLI single-shot** (`claude -p`), all filesystem/shell tools disabled, the project laid out inline (`project-snapshot.md`), model pinned to **`claude-haiku-4-5`** (the deck default). No Anthropic-API path and no Sonnet baseline this pass (per the orchestrator's engine decision). Per-project inputs (`rendered-prompt.md`, `capture-appendix.md`, `project-snapshot.md`, `baseline-input.md`), raw outputs (`baseline-raw-output.md`), extracted configs (`baseline.toml`, all five pass `dot-agent-deck validate`), and region diffs (`diff.md`) live under `analysis/prd-116/<project>/`.

## The catalogue gate (PRD risk mitigation)

A delta enters the catalogue **only** if it (a) clears the **date gate** below — it is not a prompt-evolution time-drift confound — **and** (b) appears in **2+ independent projects** OR is an obvious universal improvement. Single-project structural choices and model-nondeterminism artifacts are reported but not catalogued.

## Date gate (C4 — the pilot's key methodology finding, applied to all projects)

The pilot's most important finding (C4) was that a delta is only a user signal if the relevant prompt/role capability **existed when the user last edited the config**. The config-gen prompt asset (`assets/config_gen_prompt.md`) was created on **2026-04-27** (commit `3b83478`) and has **not changed since**, so the *entire current prompt* — orchestration-proposing, the role library, the `reactive_panes` guidance, the consolidated-rules guidance, the release-role guidance — has been in effect since that date. (The orchestration *engine*, PRD #58, landed `99a4a03` on 2026-04-21; the prompt began *proposing* orchestrations on 2026-04-27.)

| Project | Config last-edited | Orchestration introduced in config | After 2026-04-27 prompt? | Time-drift confounded? |
|---|---|---|---|---|
| `dot-ai-infra` *(pilot)* | 2026-04-22 | (none) | **No — predates** | **Yes** — orchestration absence is a confound, not a signal |
| `dot-agent-deck` | 2026-06-21 | 2026-04-21 (`99a4a03`), maintained through 06-21 | Yes | No |
| `dot-ai` | 2026-06-13 | 2026-05-26 (`5bd3360`) | Yes | No |
| `dot-ai-cli` | 2026-05-26 | 2026-05-26 (`3742ebe`, single commit) | Yes | No |
| `youtube-automation` | 2026-05-09 | 2026-04-24 (`52481a3`), maintained through 05-09 | Yes | No |

> **Headline date-gate result.** Unlike the pilot, **none of the four Phase-2 configs is time-drift-confounded.** Every one was last edited *after* the current prompt (and current role library) shipped, and each carries an orchestration that was introduced or actively maintained post-feature. So their orchestration and role deltas **are valid user signals** — the pilot's orchestration confound (a pre-2026-04-27 config that simply never saw the feature) does not recur here. `dot-agent-deck` and `youtube-automation` first added an orchestration a few days *before* 2026-04-27, but both configs were maintained for weeks afterward with full knowledge of what the generator produces, so the presence (and shape) of their orchestrations is intentional, not stale.

## Cross-project matrix (region by region)

"B" = regenerated baseline, "U" = user-improved. ✓ = B and U agree.

| Region / aspect | dot-agent-deck | dot-ai | dot-ai-cli | youtube | pilot (dot-ai-infra) |
|---|---|---|---|---|---|
| `init_command` | `devbox shell` ✓ | `devbox shell` ✓ | `devbox shell` ✓ | (no mode) | `devbox shell` ✓ |
| modes count B/U | 1 / 1 | 1 / 1 | 1 / 1 | 1 / **0** | 1 / 1 |
| persistent panes B/U | **1 / 1** (git) | **1 / 1** (git) | **1 / 1** (git) | 1 / 0 | 2 / 1 (B over-adds) |
| rule style | consolidated ✓ | consolidated ✓ | consolidated ✓ | consolidated | B consolidated / U narrow |
| `reactive_panes` B/U | 2 / 2 ✓ | 2 / 2 ✓ | 2 / 3 | 3 / — | 3 / 2 |
| orchestration present B/U | yes / yes | yes / yes | yes / yes | yes / yes | yes / **no** (confounded) |
| per-role launchers discovered | B✓ U✓ | B (generic*) | **B✓ U✓** | **B✓ U✓** | B✓ U✓ |
| `release` `clear=false` | B✓ U✓ | B✓ U✓ | B✓ U✓ | B✓ U✓ | B✓ |
| context-handoff in orchestrator | B✓ U✓ | B✓ U✓ | B✓ U✓ | B✓ U(omitted) | B✓ |
| **`auditor` role** | B✗ **U✓** | B✗ **U✓** | B✓ U✓ | B✓ U✓ | — |
| **`tester` role / TDD chain** | B✗ **U✓** | B✗ **U✓** | B✗ U✗ | B✗ U✗ | B✗ U✗ |
| `documenter` role | B✗ U✓ | B✗ U✓ | B✗ U✗ | B✗ U✗ | — |
| **pre-release human gate** | B✗ **U✓** | B✓ U✓ | B✗ **U✓** | B✓ U✓ | — |
| **release stops before merge / waits for CI+review** | B✗ **U✓** | B✗ **U✓** | B✗ U(orch-gated) | B✗ U(orch-gated) | — |

\* `dot-ai`'s `devbox.json` defines only generic launchers (`agent-new`, `agent-tester`, `agent-medium`), no per-role `agent-coder`/`agent-reviewer`/… scripts, so neither B nor U could use per-role launchers — both correctly fall back to the generic scripts. Not a gap.

---

## Catalogued patterns

### P1 — The release flow auto-merges; users *always* insert a human gate before merge (and the test-heavy projects make `release` itself wait for CI + automated review and STOP) — **PRIMARY, universal**

**Gap.** The `release` role template in `assets/roles.toml` literally says *"open a PR, merge, tag"* and the prompt's release guidance only tells the orchestrator to gate *once* ("Before delegating to release, summarize what to test end-to-end and STOP until the user confirms"). The generator faithfully reproduces the auto-merge wording in every baseline, and applies even the single pre-release gate only ~half the time. **Every** user, by contrast, ensures the merge cannot happen without an explicit human go-ahead — and the two most release-disciplined projects rebuild `release` into a *two-phase* worker: open the PR, **wait for CI and any automated PR review to settle, report a findings summary, and STOP without merging**; merge/close only on a later explicit re-delegation.

**Projects:** all 4 (universal). `dot-ai` and `dot-agent-deck` put the stop-before-merge in the `release` role itself; `dot-ai-cli` and `youtube-automation` put a "STOP until the user confirms" gate in the orchestrator *before* `release` is delegated. Date gate: ✓ (all post-2026-04-27; the `dot-ai` release rewrite is in its 06-13 edits, `dot-agent-deck`'s in its 06-21 edits).

**AI baseline (library template, reproduced by all baselines):**
```
Your job is to run the project's release flow (open a PR, merge, tag). Do NOT modify source code.
If any step fails, report the exact error and stop ...
```
Baseline orchestrators also frequently skip the pre-release gate, e.g. `dot-ai-cli` baseline: *"4. Coordinate release via the release agent when ready."* and `dot-agent-deck` baseline: *"5. Delegate release to release (PR, merge, tag, close issue)."* — no human gate.

**User-written (dot-ai `release`):**
```
... After opening the PR, WAIT for all PR processes to finish — CI / GitHub Actions and automated
reviews (e.g. CodeRabbit ...) — then report back: the PR URL, per-check CI conclusions ...
Once the PR is open, CI is green, and reviews have settled, STOP — do NOT merge. Report back via
work-done with the PR URL ... Only merge the PR and close the issue when the orchestrator
re-delegates with an explicit instruction to continue.
```
**User-written (dot-ai-cli / youtube orchestrator gate):** *"Before delegating to release, summarize what to verify end-to-end and STOP until the user confirms. Then delegate to release."* / *"Do NOT delegate to release until the user explicitly tells you to proceed."*

**Proposed edit (one each):**
- `assets/roles.toml` — rewrite the `release` `prompt_template` to the two-phase shape: *open the PR via the project's release flow, then **wait for CI and any automated PR review to settle**, report a categorised findings summary (PR URL, per-check CI conclusions, review findings), and **STOP — do NOT merge**. Merge and close the issue only when re-delegated with an explicit go-ahead. On any failure, report the exact error and stop.* (Keep `clear = false`.)
- `assets/config_gen_prompt.md` — in the release guidance (step 4 / the `release`-role note), make the orchestrator's pre-release human gate **mandatory and unconditional**: the orchestrator must summarize what to validate end-to-end and STOP for explicit user confirmation before delegating `release`, and `release` never merges on its own initiative.

---

### P2 — A `tester` role and a RED/GREEN TDD chain are never proposed, even when the project's CLAUDE.md makes tests mandatory — **2 projects**

**Gap.** The prompt gates the tester on a vague trigger — *"Add `tester` if the project uses TDD signals"* — and the haiku generator **never** fires it: `tester` appears in **0 of 5** baselines (pilot included). Yet the two projects whose `CLAUDE.md` makes tests mandatory both hand-built a substantial `tester` role *and* wired a tester→coder→tester chain into the orchestrator. The "TDD signals" phrase is too abstract for a single-shot model to recognise "mandatory integration tests" or "`cargo test-fast` is the per-task gate" as a trigger.

**Projects:** `dot-ai` (integration-test TDD: `tester` exclusively owns `tests/integration/`, runs scoped `npm run test:integration <pattern>` for RED/GREEN, coder forbidden from touching integration tests) and `dot-agent-deck` (L2 synthetic TDD: `tester` writes a failing `#[spec]` test, confirms RED, coder makes it pass, tester confirms GREEN). Date gate: ✓ (`dot-ai`'s commit "add tester worker with integration-test TDD chain" is in its post-feature history; `dot-agent-deck`'s tester is in its 06-xx edits).

**AI baseline:** roster is `orchestrator, coder, reviewer[, auditor], release` — no `tester`, no TDD chain in the orchestrator workflow (baselines go straight `coder → reviewer`).

**User-written (dot-ai orchestrator, abridged):**
```
For behavior-changing implementation: run a TDD chain on INTEGRATION tests only — delegate to tester
(writes/extends a failing integration test, runs ONLY the related group ... to confirm RED ...), then
to coder (implements production code only; never writes or modifies integration tests) ... delegate
back to tester to re-run that same scoped pattern and confirm GREEN.
```

**Proposed edit (config_gen_prompt.md, one targeted change):** replace the vague tester trigger with a concrete one — *"If the project's `CLAUDE.md`/test setup makes tests mandatory or describes a test-first/TDD flow, include a `tester` role AND wire a tester→coder→tester RED/GREEN chain into the orchestrator's `prompt_template`: tester writes/extends a failing test and confirms RED; coder makes it pass with production code only (never editing the tester's tests); tester re-runs the scoped test to confirm GREEN."* Optionally enrich the `roles.toml` `tester` template to state it owns the test suite and follows the RED/GREEN protocol (today it is a generic "writes and runs tests").

---

### P3 — `auditor` is under-proposed even though the prompt lists it as a "common pick" — **4 projects (universal in users); secondary, nondeterminism-caveated**

**Gap.** All 4 users keep an `auditor`, but the generator includes it in only **2 of 4** baselines — and only when a dedicated `agent-auditor` launcher nudges it (`dot-ai-cli`, `youtube`). Even that nudge is unreliable: `dot-agent-deck`'s `devbox.json` *also* defines `agent-auditor`, yet its baseline dropped the role; `dot-ai` (no `agent-auditor` script) dropped it too. The prompt already says *"Common picks: coder + reviewer + auditor"* — so this is a **reliability** gap, not a missing instruction.

> **Caveat (honest determinism note).** Role-roster composition is **nondeterministic** under the pinned-haiku / no-temperature engine — the pilot saw the roster vary run-to-run (run 1 `orchestrator/coder/reviewer/release`, run 2 `orchestrator/operator/release`). So P3 is a **directionally consistent** signal across four independent baselines (all undershoot the roster relative to users, and users are unanimous), not a hard per-delta finding. The proposed edit is a low-risk nudge; Phase 3 regeneration is the real test.

**Projects:** 4/4 users keep `auditor`. Date gate: ✓ (the role library has shipped since 2026-04-27; all four kept auditor in post-feature edits).

**Proposed edit (config_gen_prompt.md):** promote `auditor` from "common pick" to **default-on** — *"include `auditor` by default for any project with a code surface; drop it only when there is genuinely nothing to audit (a pure-docs or infra-manifest repo)."*

---

### P4 — Worker `prompt_template`s stay generic where users bake in the project's exact test command and mandated output-handling convention — **3 projects; secondary**

**Gap.** The prompt says to *"substitute the actual test command"*, but the single-shot baseline routinely leaves the `coder`/`tester` template generic ("run the project's test command", "run cargo test-fast"), while users hard-code the exact command **and** the project's mandated output-redirect/log convention from `CLAUDE.md`.

**Projects:** `dot-ai-cli` (`mkdir -p tmp && task test > tmp/test-output.txt 2>&1`, then `tail -30`, plus the `//go:build integration` + mock-server convention), `dot-ai` (`npm run test:integration > ./tmp/test-output.log 2>&1; tail -30`, teardown on success / keep cluster on failure), `dot-agent-deck` (`cargo fmt` + `cargo clippy -- -D warnings` + `cargo test`, commit-clean gate). Date gate: ✓.

**AI baseline (dot-ai-cli coder):** *"... Redirect test output to ./tmp/test-output.txt and check the last 30 lines. ..."* — it picked up the redirect path (good) but not the `task test` command or the integration-test convention.
**User-written (dot-ai-cli coder):** names `task test`, the `mkdir -p tmp && task test > tmp/test-output.txt 2>&1` form, `tail -30`, and "prefer extending integration tests over inline `httptest` unit tests."

**Proposed edit (config_gen_prompt.md):** in the role-tuning guidance, make it explicit — *"In the `coder`/`tester` `prompt_template`, name the project's **exact** test/lint command(s) and any output-redirect or log-handling convention the project's `CLAUDE.md` mandates, rather than a generic 'run the tests'."* This overlaps with the general "tune to the project" instruction; the edit just makes the test-command specificity non-optional.

---

## Carried-forward pilot candidates (C1–C4) — verdicts against the full project set

| # | Pilot candidate | Verdict | Why (date-gated) |
|---|---|---|---|
| **C1** | Name `kubectl rollout status` (+ `events`/`top`) in the read-only kubectl verbs | **REJECT** | Only `dot-ai-infra` mirrors `rollout status`. The other k8s-touching project, `dot-ai`, uses `kubectl (get\|describe\|logs\|top)` — **no** `rollout`. 1 project → fails the 2+ gate. (Not a time-drift issue, just unsupported.) |
| **C2** | Bias toward a **single** persistent pane (git status); push volatile state to reactive rules | **REJECT as a general pattern** | All four code-project baselines **already** emit exactly one `git status -s` persistent pane. The "adds a 2nd persistent pane" behavior occurred **only** in the infra pilot (a k8s repo, n=1). The generator already does the right thing for code projects; nothing to fix. |
| **C3** | Decouple `reactive_panes` from rule count (slots = how many visible at once) | **REJECT** | `dot-ai` (2 rules / `reactive_panes=2`), `dot-ai-cli` (3 / 3), `dot-agent-deck` (2 / 2) all **match slots to rule count**, exactly as the prompt couples them. Only the infra pilot diverged (6 rules / 2 slots, n=1, idiosyncratic). User behavior **confirms** the prompt's coupling. |
| **C4** | (Methodology) gate every delta on config-age vs prompt-feature dates | **CONFIRM + carried forward** | Applied to all four (table above). Decisive: it cleared the four Phase-2 configs of the pilot's orchestration confound, so their orchestration/role deltas count as signals. Without it, P1/P2/P3 would have been mis-weighted by the pilot's stale `dot-ai-infra` data. |

**One delta rejected specifically as a time-drift confound:** the pilot's `[[orchestrations]]` "user removed the orchestration" delta in `dot-ai-infra` — the config (last edited 2026-04-22) predates the prompt that began proposing orchestrations (2026-04-27), so the absence is "generated by a pre-orchestration prompt, never regenerated," not a user choice. It is excluded and does **not** feed any catalogue pattern.

## Positive confirmations — the generator already gets these right (do NOT "fix" in Phase 3)

- **`init_command = "devbox shell"`** from `devbox.json` discovery — agrees in 4/4 (5/5 with the pilot).
- **A single `git status -s` / `git diff --stat HEAD` persistent pane** as the AI-paired default — baselines already default to one pane for code projects.
- **Consolidated rule alternations** (`cargo (test|clippy|check)`, `git (log|status|show|diff)`) — baselines and 3/4 users agree (rejecting the pilot's narrow-rule worry as `dot-ai-infra`-specific).
- **`reactive_panes` ≈ rule count** — both sides match (rejecting C3).
- **Per-role semantic launchers** (`devbox run agent-coder`/`agent-reviewer`/…) discovered correctly wherever the project defines them (`dot-ai-cli`, `youtube`, `dot-agent-deck` baselines all matched the user's mapping exactly).
- **`release` role with `clear = false`** — 4/4 baselines.
- **Mandatory context-handoff block** in the orchestrator `prompt_template` — 4/4 baselines (one user, `youtube`, even omitted it while the baseline kept it).
- **Orchestrator runs lightweight coordination skills itself** (`/prd-next`, `/prd-update-progress`) — most baselines reproduce it.
- **Proposing an orchestration by default** — 4/4 baselines, and validly so (these configs post-date the feature).

## Project-specific structural choices — reported, NOT catalogued (each n=1)

- **`youtube-automation` drops all `[[modes]]`** (orchestration-only config). The baseline keeps a mode. n=1 — a single project's preference, not a prompt gap.
- **`dot-ai-infra` drops the orchestration entirely** (modes-only). n=1 **and** time-drift-confounded (see C4) — doubly excluded.
- **`dot-ai` keeps a `documenter` role.** Only project to do so (n=1); the prompt's "add `documenter` if the project has substantial docs" is plausible but unsupported at 2+.

## Methodology notes & limitations

- **Determinism in Phase 2 = cross-project recurrence.** The `claude` CLI exposes no temperature flag, so (as in the pilot) exact role rosters, pane choices, and wording are lexically variable. Phase 2 substitutes the PRD's **"2+ independent projects"** rule for per-run re-sampling: a delta seen across multiple independent project baselines is stronger evidence than N re-runs of one project. Accordingly the catalogue reasons only about **structural** deltas (role presence, pane count, launcher mapping, release-flow shape, template specificity), and P3 is explicitly flagged as directional because roster composition is the least stable axis.
- **Diff-tool limitation (carried from M1.3).** `diff_config` matches modes/orchestrations by **name**. Where the baseline names the orchestration `dev-flow` but the user named it `dot-agent-deck` / `youtube-automation`, the tool shows them as disjoint add/remove instead of pairing roles field-by-field (see those two `diff.md`s). Role-level comparison for those projects was done in prose against the raw configs; `dot-ai` and `dot-ai-cli` (both `dev-flow`) pair cleanly and their `diff.md`s carry the full field-level role diffs.
- **No `assets/` edits in Phase 2.** All proposed edits above are for Phase 3 (M3.1); `config_gen_prompt.md` and `roles.toml` are untouched here.
