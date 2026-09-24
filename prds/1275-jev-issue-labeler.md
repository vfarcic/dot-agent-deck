# PRD #1275: Jev as the issue and PR labeler's classifier

**Status**: Draft — **blocked on Jev access.** No TypeSafe account is available yet, so no milestone can start. M1 is a go/no-go measurement, and nothing after it is built unless M1 passes.
**Priority**: Low
**Created**: 2026-09-24
**Related**: [PRD #1273](1273-jev-voice-command-backend.md) (Jev as a voice command model — same model, independent decision), [PRD #421](421-issue-triage-labels-and-dispatch-claims.md) (the other classification mechanism; the interlock below still holds)

## Problem Statement

The adaptive labeler ([`docs/develop/issue-labeling.md`](../docs/develop/issue-labeling.md)) classifies new issues and same-repository pull requests into a **closed taxonomy** of 21 labels across six axes (type, area, component, priority, size, triage). It does that by running a full Codex agent (`gpt-5-mini`) inside a GitHub Agentic Workflow, with read tools, a threat-detection pass and correction memory.

The work itself is not agent-shaped. The safe-output allowlist already restricts the model to choosing from the taxonomy, and a validator rejects anything else — the output *is* a set of choices. Paying for an agent to make them shows in the one trial on record (2026-08-20): **0.736 AI credits for classification plus 0.454 for threat detection, and 27.5 s of model time**, for a single synthetic issue. An earlier configuration spent 5.680 credits and abstained.

The labeler has **never been enabled**: there is no `ISSUE_LABELER_ENABLED` repository variable and no `memory/issue-labeler` branch (checked 2026-09-24). So there is no production behaviour to preserve yet, and no correction memory.

[Jev](https://docs.typesafe.ai/) (TypeSafe AI, 2026-09-15) is a decision model that returns only typed answers — choices, yes/no confidences, scores — with sub-second latency and pricing of $0.042 per million input tokens and free output. A closed-taxonomy classifier is exactly its intended workload.

## Solution Overview

Replace the Codex classification with **one Jev request per item**, whose questions are derived from the taxonomy table:

| axis | rule (from `issue-labeling.md`) | Jev question |
| --- | --- | --- |
| Type | at most one | choice: `bug` · `feature` · `enhancement` · `documentation` · `question` · `none` |
| Area | zero or more | one yes/no per label: `source`, `config`, `dependencies`, `tests`, `ci-cd`, `devbox` |
| Component | zero or more | one yes/no per label: `daemon`, `tui`, `desktop` |
| Priority | issues only; one when estimable | choice: `high` · `medium` · `low` · `unknown` |
| Size | one when estimable | choice: `low` · `medium` · `high` · `unknown` |
| Triage | issues only, when type or priority cannot be chosen confidently | see Open Question 1 |

State is the item's title, body and (for pull requests) changed file paths. Jev answers all questions in parallel; the workflow keeps the top answer of each choice (`none` and `unknown` mean no label on that axis) and every yes/no above 0.5, then applies the existing per-item cap of 6. A yes/no answer is a confidence by nature, so 0.5 is simply what "the top result" means for it, not a tuned threshold.

**What stays exactly as it is:**

- **The allowlist is still the enforcement boundary.** Jev proposes; a separate step applies only labels in `add-labels.allowed`, and never authority or lifecycle labels (`PRD`, `duplicate`, `wontfix`, …).
- **Activation stays with the maintainer**: automatic runs remain gated on `ISSUE_LABELER_ENABLED`, and the batch workflow's preview-by-default path stays.
- **Correction memory and its validator.** Predictions and human corrections keep the same files, fields and limits on the memory branch, and the validator — "the workflow's one safety property that does not rest on the model behaving" — keeps its tests in `xtask/linkage-check/src/issue_labeler_memory.rs`. The BM25-selected past corrections can be passed to Jev as part of the state (Open Question 3).
- **Fork pull requests stay skipped**, because the workflow holds a credential.
- **The PRD #421 interlock**: exactly one mechanism applies the classification taxonomy to this repository at a time. This PRD replaces the classifier inside the existing mechanism; it does not add a second one.

## Why the security model gets simpler, and what still has to hold

The current workflow wraps an **agent with tools**, which is why it needs gh-aw's sandbox, the read-only token, `gh-proxy`, safe outputs staged through separate jobs, and a fail-closed threat detector. Jev has no tools and cannot write text: whatever an issue body says, Jev can only answer the questions it was asked with the options it was given. A prompt injection in an issue can therefore at worst move which **allowlisted** labels are proposed.

That is a narrower claim than "injection is harmless", and the PRD keeps it narrow. Jev's own docs list adversarial content among its failure modes, so a hostile issue *can* get itself mislabelled. What still has to hold, and is verified rather than assumed:

- the apply step enforces the allowlist and the cap independently of Jev's answer;
- the memory validator bounds what can be stored, independently of Jev's answer;
- issue text never reaches a step that executes anything.

Whether the gh-aw framework is still worth keeping for a tool-less classifier is Open Question 2.

## Scope

### In Scope

- An offline evaluation harness and M1's measurement.
- A Jev classification step for the live and preview workflows, reusing the batch workflow's fan-out.
- Removing the Codex classification path and the labeler's use of `OPENAI_API_KEY` once Jev is live.
- Updating every place that describes the labeler's credential (see M5).

### Out of Scope

- **Enabling the labeler.** Setting `ISSUE_LABELER_ENABLED` remains a separate maintainer decision.
- **Backlog sweeps** (PRD #421's Phase 1 territory). Retro-labeling stays the batch workflow's manual 20-item runs.
- **Using Jev's probabilities** beyond the 0.5 yes/no cut — e.g. applying `needs-triage` from low confidence. Possible follow-up.
- **Changing the taxonomy.**

## Technical Approach

### Evaluation data: the labels already on this repository's issues

There is no correction memory to evaluate against, but the repository already carries taxonomy labels on a substantial share of its issues — counted 2026-09-24 over 677 issues: `bug` 144, `desktop` 62, `enhancement` 41, `daemon` 26, `documentation` 19, `priority:medium` 18, `size:low` 16, `priority:low` 13, `size:medium` 10, `tui` 9, and single digits for the rest (`feature` 3, `question` 2, `priority:high` 3, `size:high` 4, `needs-triage` 0). The counts are recorded as the reason this is workable, not as a target; the harness reads whatever is there when M1 runs.

Three limits of that data, stated so M1's result is read correctly:

- **Labels were not applied by a controlled process.** Some were applied by agents through a maintainer's `gh` credential, so "the existing label" is a reference, not verified ground truth.
- **An absent label is ambiguous.** An issue without a type label may have been judged typeless or simply never labeled. So evaluation scores **only axes the issue carries a label on** for type, priority and size, and treats area/component yes/no answers as agreement measures rather than accuracy.
- **Some classes are too sparse to measure** (`feature`, `question`, `priority:high`, `size:high`). M1 reports them without drawing conclusions.

### The comparison

M1 compares Jev with the current Codex classifier, not only with the labels. Codex runs through the batch workflow's preview path (20 items per dispatch, metered in AI credits, so the sample is chosen deliberately); Jev runs through the offline harness on the same items and on the full labeled set.

### Credential

The workflow would hold a TypeSafe key as a repository secret instead of `OPENAI_API_KEY`. That is the same exposure class the labeler already has (CLAUDE.md rule 5 carves the labeler out of the e2e tier's no-credential decision and gives it its own threat model), so replacing one secret with the other is neutral — provided the OpenAI secret is actually removed from the labeler once Jev is live, which is what M5 is for. Rule 5's e2e decision is unaffected: no e2e test gains a credential.

### Flags, contract, changelog

- **CLAUDE.md rule 19**: no changelog fragment. This is repository automation; no user of the deck can observe it.
- **Rules 9 and 12** do not apply: no app surface, no daemon or protocol change.

## Success Criteria

- **Go/no-go (M1):** on the items both classifiers see, Jev's agreement with the existing labels, per axis, is at least the Codex classifier's. Below that, the PRD stops at M1 with the measurement recorded.
- Per item, the Jev path completes in seconds and costs a small fraction of the current 0.736 + 0.454 credits; M1 records both.
- A preview run proposes labels in the run summary and writes nothing; an apply run adds only allowlisted labels, within the cap, and records the prediction.
- The memory validator's existing tests pass unchanged, and a new test proves a crafted Jev reply containing a non-taxonomy label is not applied.
- After M5, nothing in the repository describes the labeler as using Codex or `OPENAI_API_KEY`, and the labeler no longer references that secret.

## Milestones

- [ ] **M1 — Go/no-go measurement.** With a Jev account: an offline harness that pulls labeled issues, asks Jev the taxonomy questions, and scores per-axis agreement; a Codex preview run over a deliberately chosen sample of the same items. Record agreement per axis, latency and cost for both. **Stop here if the bar is not met.**
- [ ] **M2 — Jev in the preview path.** The preview workflow classifies with Jev and reports proposed labels in the run summary with no writes; the batch workflow's fan-out and input validation are unchanged.
- [ ] **M3 — Jev in the live path.** Apply step enforcing allowlist and cap independently of the model; predictions and feedback written through the existing memory files and validator; tests for the crafted-reply case.
- [ ] **M4 — Codex path removed.** The agent engine, threat-detection pass and gh-aw machinery that only an agent needed are removed or kept per Open Question 2's answer; the labeler stops referencing `OPENAI_API_KEY`.
- [ ] **M5 — Every description of the labeler's credential updated.** `docs/develop/issue-labeling.md`; CLAUDE.md rule 5's paragraph on the Codex issue-labeler (and a check that rule 17's historical example still reads as history); the comments in `.github/workflows/ci.yml` that describe the labeler passing `OPENAI_API_KEY` into `codex exec`; `THIRD_PARTY_NOTICES.md` if the `dosu-ai/auto-label` derivative no longer applies. Sweep with `grep -rn "OPENAI_API_KEY\|codex exec\|issue-labeler"` rather than trusting this list.

## Risks

- **Jev access or API may change.** A startup's v1.13, early access opened 2026-09-24. Mitigation: nothing is built before M1, which starts by checking the API against the docs of the day.
- **The reference labels are noisy.** Mitigation: the comparison is Jev against Codex on the same items, not Jev against a truth neither has.
- **Adversarial issues can mislabel themselves.** Accepted within the bound above: the allowlist, cap and validator hold regardless of Jev's answer.
- **Losing gh-aw's framework loses things nobody listed.** The AI-credit guardrail, the conclusion job, the generated diagnostics. Mitigation: Open Question 2 is answered by enumerating what gh-aw provides here before removing any of it.

## Open Questions

1. **How is `needs-triage` decided?** Derived deterministically (type is `none`, or priority is `unknown`, on an issue), or asked as its own yes/no question? Derived is predictable and testable. No existing issue carries `needs-triage`, so M1 has nothing to measure either option against, and this is a design decision rather than a measurement.
2. **Keep gh-aw or move to a plain Actions workflow?** gh-aw exists to contain an agent. A tool-less classifier may not need it, but its guardrails and memory plumbing are real. Answer by listing what the labeler uses from gh-aw before M4.
3. **Does Jev benefit from past corrections in its state?** The BM25 prefilter selects relevant corrections for the agent today. Irrelevant context is one of Jev's documented failure modes, so more examples may hurt. Measurable once memory exists; M1 cannot, since there is none.
4. **Pull-request path labels.** A deterministic path labeler already applies area labels to pull requests. Should Jev skip the area axis for pull requests, or ask it and let the union stand?

## Work Log

### 2026-09-24 — Created

Split out from the discussion that produced PRD #1273. Found while writing it: the labeler has never been enabled and has no memory branch, so the "feedback store as a ready test set" idea was withdrawn in favour of the labels already on the repository's issues.
