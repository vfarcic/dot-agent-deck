# PRD #116 — M1.4 Pilot Analysis: `dot-ai-infra`

Pilot write-up that pressure-tests the M1.3 region taxonomy before fanning out (Phase 2).
It records, **per region**, what the AI generates vs. what the user wrote, the
prompt/role-library change each delta *suggests*, and — crucially — whether the delta is
**stable across re-runs** and **free of confounds**.

> **Catalogue gate.** A pattern only enters the Phase-2 catalogue if it shows up in **2+
> independent projects** or is an obvious universal improvement (execution-context
> cross-cutting rule). With a single project, **nothing here is catalogue-ready** — every
> item below is a *candidate hypothesis* tagged with what to verify in Phase 2.

## How the baseline was produced (M1.2 recap)

- **Engine:** the authenticated **`claude` CLI in print mode** (`claude -p`), run
  single-shot with **all filesystem/shell tools disabled** and the project "laid out" in
  the prompt (see `project-snapshot.md`). This is the reproducible stand-in for the deck's
  real config-gen flow, in which the rendered prompt is fed to a `claude`/`opencode` agent
  pane (`send_config_gen_prompt` → `config_gen::config_gen_prompt`, `src/ui.rs:4296`) — the
  deck does **not** use the `src/llm.rs` API client for config-gen (that client only powers
  ascii/idle-art). Disabling tools guarantees the run can never read or overwrite the
  user's real repo.
- **Why the CLI and not the raw API:** decision #1's literal path is "render the prompt and
  pipe it to the Anthropic API." `ANTHROPIC_API_KEY` is **not available** in this
  environment, so the raw-API path is blocked; the `claude` CLI is installed
  (v2.1.185) and authenticated, and is in fact the *more faithful* engine (it is what the
  deck drives). See "Blockers & caveats" below.
- **Model (pinned):** `claude-haiku-4-5` — the deck's documented default LLM model
  (`src/config.rs:170`, the model `src/llm.rs`'s Anthropic client uses by default).
- **Temperature:** the `claude` CLI exposes **no temperature flag**, so temperature 0
  could not be pinned. Determinism was instead checked empirically with **two runs** (see
  "Determinism" below).
- **Artifacts:** exact prompt sent → `baseline-input.md` (= the rendered
  `rendered-prompt.md` + the non-interactive `capture-appendix.md` + `project-snapshot.md`); raw
  outputs → `baseline-raw-output.md` (run 1), `baseline-run2-raw-output.md` (run 2);
  extracted configs → `baseline.toml`, `baseline-run2.toml` (both pass `dot-agent-deck
  validate`); structured diff (run 1 vs user) → `diff.md`.

### Exact reproduction

```bash
# 1. Render the exact prompt the deck would send (no {dir}/{roles} placeholders left):
cargo run --quiet --example render_config_gen_prompt -- /home/vfarcic/code/dot-ai-infra \
  > analysis/prd-116/dot-ai-infra/rendered-prompt.md

# 2. (project-snapshot.md is the laid-out project; baseline-input.md concatenates
#    rendered-prompt.md + capture-appendix.md + project-snapshot.md)

# 3. Generate the baseline single-shot, tools disabled, model pinned:
claude -p --model claude-haiku-4-5 \
  --disallowedTools Bash Read Edit Write Glob Grep WebFetch WebSearch Task TodoWrite NotebookEdit \
  < analysis/prd-116/dot-ai-infra/baseline-input.md

# 4. Diff baseline vs the user-improved config by region:
cargo run --quiet --example diff_config -- \
  analysis/prd-116/dot-ai-infra/baseline.toml \
  /home/vfarcic/code/dot-ai-infra/.dot-agent-deck.toml
```

## Per-region deltas

### `init_command` — ✅ agreement (validates the prompt)

Both baseline runs and the user config set `init_command = "devbox shell"`. The prompt's
"if the project ships a reproducible-environment manifest (`devbox.json`, …) default
`init_command` to its activation command" guidance fires correctly. **No change. Do not
touch** — this is a positive confirmation.

### `reactive_panes` — baseline `3` (stable) vs user `2`

The user has **6 rules but only `reactive_panes = 2`**; both baselines produced **3 rules
and `reactive_panes = 3`** (the prompt tells the agent to match the slot count to the rule
count). So the generator *follows* the prompt, and the divergence is the user keeping
**fewer pane slots than rules** — they want many rules captured but only ~2 reactive panes
on screen at once (the deck recycles slots, most-recent-wins).

- **Candidate:** soften the prompt's "increase `reactive_panes` to match rule count"
  coupling — `reactive_panes` is *how many reactive panes you want visible at once*, not
  *how many rules exist*. A user can have many rules and few slots.
- **Confidence:** low. A first glance at the other configs suggests `dot-ai` (2 rules /
  `reactive_panes=2`) and `dot-ai-cli` (3 rules / `reactive_panes=3`) **do** match slots to
  rules, so this may be a `dot-ai-infra` idiosyncrasy. **Verify in Phase 2.**

### `seed_prompt` — both `(none)`

Neither side sets it (PRD #127 field). No signal here, but the region is now wired into
the diff taxonomy and will catch divergence in projects that do use it. **Keep tracking.**

### `[[modes]]` — 1 mode each; cosmetic name difference

Baseline `GitOps` / `GitOps Infrastructure` vs user `gitops`. Casing/wording only; the
prompt doesn't (and shouldn't) dictate the mode name. **No signal.**

### `[[modes.panes]]` — git-status agreement; baseline over-adds a 2nd persistent pane

- **Agreement:** both baselines and the user keep a **git-status persistent pane**
  (`git status -s` ≈ user's `git status --short`). The prompt's "a `git status -s` /
  `git diff --stat HEAD` is often the right default for AI-paired work" guidance fires
  correctly. ✅
- **Stable delta:** both baselines add a **second** persistent kubectl pane (run 1:
  `kubectl get applications -A`; run 2: `kubectl top nodes`) that the user **does not**
  keep — the user runs a single persistent pane and treats cluster state as *reactive*
  (on-demand) instead. The *specific* second pane is **not** stable across runs, but
  *"adds a second persistent pane"* is.
- **Candidate:** for AI-paired work, bias toward a **single** persistent pane (git status)
  and push volatile cluster/state inspection into reactive rules rather than a constantly
  re-running persistent pane. (Also: run 1's `kubectl get applications -A` is the kind of
  wide, cross-namespace output the prompt's "compact output" rule warns against.)
- **Confidence:** low–medium, 1 project. **Verify in Phase 2** (does any other user keep
  ≤1 persistent pane?).

### `[[modes.rules]]` — consolidated (AI) vs narrow (user); a missed verb

The diff matches rules by exact pattern, so the sets look disjoint; semantically:

| | Baseline (both runs) | User |
|---|---|---|
| kubectl | one alternation `kubectl (get\|describe\|logs\|[top\|]tree)` | **five** narrow rules: `kubectl get applications`, `kubectl get`, `kubectl describe`, `kubectl logs`, `kubectl rollout status` |
| helm | `helm (list\|status\|diff\|values)` | `helm list` |
| git | `git (log\|diff\|show[\|status])` | _(none — git is the persistent pane)_ |

Two stable observations:

1. **Style inversion.** The prompt says *"Prefer consolidated alternations … over many
   narrow rules"*, and the generator obeys (3 tidy alternations). The **user does the
   opposite** — many single-verb rules, grown incrementally (git history shows
   `kubectl rollout status` appended in its own later commit). This is the kind of
   one-off-vs-generalizable divergence the PRD's risk section flags; a quick glance shows
   `dot-ai`/`dot-ai-cli` *do* use consolidated alternations, so the narrow style looks
   **idiosyncratic to `dot-ai-infra`** rather than a prompt gap. **Likely not a pattern —
   confirm in Phase 2.**
2. **Missed read-only verb: `rollout status`.** The user mirrors `kubectl rollout status`;
   **neither baseline** includes `rollout` in its kubectl alternation. `kubectl rollout
   status` is a common, safe, read-only inspection in a GitOps workflow.
   - **Candidate:** in the prompt's read-only whitelist / kubectl guidance, name
     `rollout status` (and arguably `get`, `describe`, `logs`, `events`, `top`) as the
     canonical safe kubectl read verbs to mirror for Kubernetes projects.
   - **Confidence:** medium, 1 project. **Verify in Phase 2** against `dot-ai` (also k8s).

### `[[orchestrations]]` — baseline proposes one; user has none — ⚠️ **CONFOUNDED**

Both baselines emit a competent orchestration (`devbox run agent` launchers, a
context-handoff rule, a `release` role with `clear = false`, PRD coordination skills wired
in); the user config has **zero** orchestrations.

**This delta carries no user-preference signal, because of a time-drift confound:**

- The config-gen prompt **first began proposing orchestrations on 2026-04-27**
  (commit `3b83478`, "feat(config-gen): … release role"; the orchestration *engine*,
  PRD #58, landed `99a4a03` 2026-04-21).
- The `dot-ai-infra` config was **last edited 2026-04-22** — **5 days before** the prompt
  ever proposed an orchestration.

So the absence of an orchestration almost certainly means *"this config was generated by a
pre-orchestration prompt and never regenerated"*, **not** *"the user evaluated an
orchestration and removed it"*. **Do not draw a prompt conclusion from this delta.** (It is
plausible a GitOps/manifests repo genuinely wants modes-only — `dot-ai-infra` has no
test/build/lint toolchain — but that hypothesis must be tested against a config edited
*after* 2026-04-27, e.g. `dot-ai`/`dot-ai-cli`/`youtube-automation`, in Phase 2.)

## Determinism / re-run stability

Two runs, same pinned model, no temperature control. **Structurally stable, lexically
variable:**

| Property | Run 1 | Run 2 | Stable? |
|---|---|---|---|
| `init_command` | `devbox shell` | `devbox shell` | ✅ |
| `reactive_panes` | 3 | 3 | ✅ |
| mode count / persistent panes | 1 / 2 | 1 / 2 | ✅ |
| reactive-rule shape | 3 consolidated alternations | 3 consolidated alternations | ✅ |
| orchestration present, `devbox run agent`, context-handoff, `release clear=false` | yes | yes | ✅ |
| 2nd persistent pane (exact command) | `kubectl get applications -A` | `kubectl top nodes` | ❌ |
| worker roster | orchestrator/coder/reviewer/release | orchestrator/operator/release | ❌ |
| names & prompt_template wording | — | reworded | ❌ |

**Takeaway:** reason only about the **stable** deltas (the ✅ rows and the *"adds a 2nd
persistent pane"* / *"misses `rollout status`"* findings, which held across both runs).
Treat exact role rosters, pane choices, and wording as noise until temperature can be
pinned. To get true determinism, Phase 2 should obtain an `ANTHROPIC_API_KEY` and use the
raw-API path (temperature 0) — see blockers.

## Format pressure-test (did the M1.3 taxonomy hold up?)

- ✅ The taxonomy surfaced **every** material delta cleanly: the orchestration gap, the
  persistent-pane choice, the rule-style inversion, `reactive_panes`, `init_command`
  agreement. The region breakdown reads well and the tool output (`diff.md`) is reusable
  as-is for Phase 2 (M2.2).
- 🔧 **Limitation:** rules (and panes) are matched by **exact string**, so a consolidated
  alternation vs. several narrow rules shows up as two disjoint lists; the *semantic*
  overlap has to be read in prose (as done above). Acceptable for now; a future enhancement
  could add semantic verb-set overlap detection. Not required for Phase 2.
- 🔧 **Taxonomy gap to fix before fanning out:** the diff must be read alongside the
  **user config's last-edit date vs. the date each prompt feature shipped**. The
  orchestration confound above would have been mis-catalogued as a strong user signal
  without it. **Phase 2 action:** record, per project, the config's last-edit date and gate
  each region's delta on "was this prompt capability present when the user last touched the
  config?" Stale configs should be flagged (or regenerated by the user) rather than mined
  for orchestration/`seed_prompt` deltas.

## Positive confirmations — leave these alone

The generator already gets these right; **do not** "fix" them in Phase 3:

- `init_command = devbox shell` from `devbox.json` discovery.
- A git-status persistent pane as the AI-paired default.
- `devbox run agent` launcher discovery (the project's only agent script) for every role.
- `kubectl tree` picked up from the `kubectl-tree` devbox package.
- Context-handoff rule present in the orchestrator `prompt_template`; `release` role with
  `clear = false`; PRD coordination skills (`/dot-ai-prd-*`, `/dot-ai-tag-release`,
  `/dot-ai-changelog-fragment`) wired into orchestrator/release.

## Candidate hypotheses to carry into Phase 2

| # | Hypothesis (prompt/role change it suggests) | Evidence | Confidence | Phase-2 check |
|---|---|---|---|---|
| C1 | Name `kubectl rollout status` (+ `events`, `top`) in the read-only kubectl verbs to mirror for k8s projects | user mirrors it; baseline misses it (both runs) | medium | corroborate on `dot-ai` (k8s) |
| C2 | Bias toward a **single** persistent pane (git status); push volatile cluster state to reactive rules | baseline adds a 2nd persistent pane both runs; user keeps 1 | low–med | does any other user keep ≤1 persistent pane? |
| C3 | Decouple `reactive_panes` from rule count (slots = how many visible at once) | user: 6 rules / 2 slots | low | likely idiosyncratic — `dot-ai`/`dot-ai-cli` match slots≈rules |
| C4 | (Methodology, not prompt) gate every delta on config-age vs. prompt-feature dates | orchestration confound | high | apply to all Phase-2 projects |

## Blockers & caveats (reported to the orchestrator)

1. **`ANTHROPIC_API_KEY` is not available**, so decision #1's literal raw-API path
   (which supports `temperature 0`) is blocked. Worked around with the authenticated
   `claude` CLI, which is arguably more faithful but cannot pin temperature. For truly
   deterministic Phase-2 baselines, provide an API key and switch to a raw-API renderer.
2. **Model faithfulness:** the baseline used `claude-haiku-4-5` (the deck's *configured*
   default model). The deck's **interactive** config-gen actually runs through whatever
   model the user's pane agent launches — often Sonnet/Opus — so a haiku baseline could in
   principle understate baseline quality. In practice the haiku output here was faithful
   and valid, so this is a low-risk caveat, but Phase 2 may want to also capture a
   Sonnet-class baseline to confirm deltas aren't model-strength artifacts.
3. **Time-drift confound** (orchestration region) — see above; the pilot's most important
   methodological finding.
