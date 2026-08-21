# PRD #610: Pilot Dosu for repository knowledge and documentation drift

**Status**: Proposed

**Issue**: [#610](https://github.com/vfarcic/dot-agent-deck/issues/610)

**Priority**: Medium

**Created**: 2026-08-19
**Related**: [#421](https://github.com/vfarcic/dot-agent-deck/issues/421) (issue labels and dispatch claims), [#603](https://github.com/vfarcic/dot-agent-deck/issues/603) (priority and size tags)

## Problem Statement

dot-agent-deck's engineering knowledge is distributed across source code, 100+ PRD files, 170+ open issues, contributor and maintainer documentation, test catalogs, changelog fragments, and dozens of agent skills. `CLAUDE.md` alone is roughly 43 KB and records detailed empirical decisions that are expensive to load into every agent session, while issue and PR history carries additional context that is not available from the checkout alone.

The repository already has strong point solutions:

- `AGENTS.md` / `CLAUDE.md` provide mandatory local instructions.
- `.claude/skills/` and `.agents/skills/` encode repeatable maintainer workflows.
- `dot-ai-manageKnowledge` exposes a semantic knowledge route.
- `issue-queue` and `pr-review-queue` handle work selection and isolation.
- Greptile reviews code changes.
- GitHub Actions labels PRs deterministically and handles stale issues.
- PRDs and documentation-first development preserve decisions in the repository.

What is not yet measured is whether a cross-source knowledge layer can retrieve the right fragment from code, docs, PRDs, issues, and PRs more reliably and cheaply than the existing routes, or catch real documentation drift without adding bot noise. Adopting a broad GitHub bot configuration before answering that question would duplicate existing automation and make rollback harder.

## Solution Overview

Run a bounded, reversible 30-day Dosu pilot on the public `vfarcic/dot-agent-deck` repository. The pilot evaluates only the three areas where prior analysis found a plausible gap:

1. **Agent knowledge retrieval** through Dosu's MCP server.
2. **Duplicate and related-issue discovery** against repository history.
3. **Reviewed documentation-drift findings** on selected user-facing documentation.

The pilot deliberately does not hand Dosu existing workflow authority. GitHub answers remain mention-only; documentation changes require human review; repository write-back, automatic stale handling, automatic closure, LGTM labels, PR size labels, and lifecycle/workflow labels remain disabled.

At the end of the trial, maintainers compare measured usefulness, error rate, review effort, and cost against the existing repository and `dot-ai` knowledge workflows. Dosu is retained only if it provides a distinct net benefit.

## Open-source eligibility and cost boundary

Dosu's public pricing and OSS pages stated on 2026-08-19 that:

- Dosu is free to use on public open-source repositories.
- the ordinary Free plan includes public repositories, one team member, and 200 monthly credits;
- a separate **Maintainers Free** offering adds automatic issue labeling, issue/discussion deduplication, and public Spaces, but is shown as a contact-the-team upgrade;
- knowledge reviews consume more usage than MCP tool calls.

dot-agent-deck is a public MIT-licensed repository and therefore fits the published public-OSS condition. This PRD does **not** interpret that statement as unlimited usage or guaranteed multi-maintainer access. Before enabling the pilot, a maintainer must confirm with Dosu:

1. that `vfarcic/dot-agent-deck` is accepted for Maintainers Free;
2. the included monthly credits and any rate limits;
3. whether both current maintainers can review drafts without a paid seat;
4. whether the proposed MCP and Knowledge Review usage is included; and
5. that no overage can be incurred without explicit opt-in.

If the maintainer offer is unavailable, the pilot may run on the ordinary Free plan only if it remains useful within one seat and 200 credits. No paid upgrade is authorized by this PRD.

Sources:

- [Dosu pricing](https://dosu.dev/pricing)
- [Dosu for open source](https://dosu.dev/oss)
- [Dosu FAQ: plans, access, and data handling](https://app.dosu.dev/9affd04a-e6a9-452c-b927-c639e979994c/documents/64ea8338-f406-4a23-a219-ea1029389290)

## Maintainer-facing workflow

### Knowledge retrieval

A maintainer may ask a connected coding agent to research a task through Dosu. Dosu searches the repository Library and returns cited source material. The agent treats those results as leads, not authority: repository contents and current GitHub state remain the source of truth, and mutable claims are rechecked at their authoritative source before action.

The pilot compares Dosu with the existing checkout search and `dot-ai-manageKnowledge` route on the same benchmark questions. It does not add a mandatory Dosu call to every agent task or to `AGENTS.md`; an unavailable third-party service must not block normal development.

### GitHub questions

Dosu responds publicly only after an explicit `@dosu` mention. Automatic replies remain disabled. Maintainers may use explicit mentions on selected existing issues to evaluate answer quality, but must not summon Dosu on security-sensitive reports or threads containing non-public material.

### Documentation review

Knowledge Review starts with Auto-Accept off and no repository write-back. It may inspect selected code changes and suggest gaps in imported user-facing documentation, but the original PR author decides whether to apply a finding in the original PR. The pilot must not create follow-up documentation PRs or push generated changes to protected `main`.

Dosu is a documentation/knowledge reviewer in this workflow, not a replacement for Greptile. Greptile's check, summary, review object, and inline comments remain the code-review surfaces required by `CLAUDE.md` rule 8.

### Issue triage

Duplicate or related-issue suggestions are advisory. Automatic issue labeling starts disabled. If benchmark results justify a limited labeling phase, Dosu may choose only from semantic labels inferable from issue content:

- `bug`
- `enhancement`
- `documentation`
- `tests`
- `config`
- `source`
- `ci-cd`

The following remain excluded because they carry workflow authority, require project judgment, or are calculated incorrectly for this repository:

- `PRD`
- `in-progress` and any dispatch claim
- `priority:*`
- `size:*`
- `stale`
- `security`
- `pinned`
- `wont-fix`, release, ownership, or approval labels

This does not replace #421. In particular, Dosu cannot express the orchestration instance, host, and timestamp required by an authoritative dispatch claim. It also does not replace #603 until maintainers decide a priority and size taxonomy; Dosu's built-in PR sizing excludes tests and configuration, both of which are load-bearing here.

## Configuration boundaries

### GitHub App

- Install the Dosu GitHub App for `vfarcic/dot-agent-deck` only, not all account repositories.
- Record the granted GitHub permissions in the pilot runbook before accepting them.
- Keep issue responses in Mention mode.
- Disable stale handling, automatic closure, issue voting, LGTM labels, PR size labels, and automatic PR replies.
- Do not make Dosu a required check or a merge gate during the pilot.

### Library and sources

Create one public Library dedicated to dot-agent-deck. It may index public repository code, issues, PRs, and the following repository content:

- `README.md`, `CONTRIBUTING.md`, and `CLAUDE.md`;
- `src/**`, `assets/**`, and relevant root manifests;
- `docs/**`;
- `prds/**`;
- `.claude/skills/**`;
- `tests/CATALOG.md` and test sources when needed to answer behavior questions.

Exclude generated, local, ephemeral, or duplicated paths, including `target/**`, `tmp/**`, `.dad-sandbox/**`, `.dot-agent-deck/**`, recordings, and the `.agents/skills/**` symlink mirror. Indexing patterns and exclusions must be copied into the runbook so the configuration is reviewable even though Dosu stores it externally.

### MCP credentials

- Configure the MCP server locally for one maintainer first.
- Keep deployment IDs, API keys, and generated client configuration out of the repository.
- Prefer OAuth where supported; otherwise use a scoped Dosu API key stored in the user's normal secret/config mechanism.
- Revoke the pilot key during rollback or when the pilot owner changes.
- Do not install proactive hooks during the first phase; on-demand MCP calls make usage and attribution easier to measure.

### Documentation monitor

- Start with published user documentation under `docs/`, excluding `docs/develop/**` from automatic update proposals.
- Monitor only source paths plausibly affecting user behavior, initially `src/**`, `assets/**`, `Cargo.toml`, and user-facing configuration examples.
- Keep Auto-Accept off.
- Do not enable sync-back or generated repository PRs.
- Treat changelog fragments as release inputs, not Dosu-maintained Documents.

## Scope

### In Scope

- Confirming and recording the applicable free OSS plan, credit allowance, seats, and no-overage behavior.
- A source-controlled maintainer runbook at `docs/develop/dosu-pilot.md`, linked from `CONTRIBUTING.md`.
- A one-repository Dosu GitHub App installation and a public Library with explicit include/exclude patterns.
- One maintainer's on-demand MCP connection, with no committed credentials.
- A fixed benchmark of architectural/history questions evaluated through checkout search, `dot-ai-manageKnowledge`, and Dosu.
- Advisory duplicate/related-issue evaluation against known historical issue pairs.
- Human-reviewed documentation-drift evaluation on selected PRs.
- A written 30-day go/no-go decision with metrics and rollback evidence.
- Updating repository process wording if Dosu becomes an active PR-commenting integration, so Greptile remains accurately described as the only automated **code** reviewer.

### Out of Scope / Non-goals

- Adding Dosu features to the dot-agent-deck binary or TUI.
- Making Dosu required for contributors, development, CI, issue dispatch, PR review, release, or merge.
- Replacing Greptile, GitHub Actions labeler/stale workflows, repository PRD skills, or dispatch claims.
- Auto-replying to every new issue or discussion.
- Automatically closing, marking stale, prioritizing, sizing, approving, or merging work.
- Automatically editing repository documentation or opening follow-up documentation PRs.
- Sending private repositories, private conversations, credentials, local agent transcripts, `.dot-agent-deck/` coordination files, or test sandboxes to Dosu.
- Buying a paid plan or enabling billable overages.
- Treating vendor performance claims as accepted results.

## Design Decisions

1. **Pilot before adoption.** The repository already has overlapping automation and knowledge tooling. A time-boxed comparison establishes incremental value before Dosu becomes part of normal process.

2. **Knowledge layer, not workflow authority.** Dosu may retrieve, cite, and suggest. Existing deterministic systems continue to own CI, review, state transitions, claims, and merges.

3. **Mention-only public behavior.** The historical issue stream is overwhelmingly maintainer-authored engineering work rather than repetitive support. Unsolicited replies would create more noise than value at current community volume.

4. **On-demand MCP before proactive hooks.** Explicit calls are measurable and reversible. Automatic context injection is deferred until Dosu demonstrates that its retrieval is both relevant and cheaper than existing context.

5. **Repository remains the source of truth.** Dosu Documents and saved Topics cannot become the only record of a decision. Durable decisions still land in PRDs, docs, issues, code comments, or agent skills under review.

6. **No repository write-back during evaluation.** Follow-up documentation PRs would trigger protected-main review and CI, and would conflict with the repository's documentation-first expectation that user-facing documentation travels with the implementation PR.

7. **No built-in PR sizing.** Dosu excludes tests and YAML/JSON configuration from its size calculation. That model understates change risk in this repository, where test and configuration changes are explicitly load-bearing.

8. **Compare with the existing knowledge path.** Dosu is retained only if it adds meaningful capability beyond checkout search and `dot-ai-manageKnowledge`; duplicating a knowledge service is not itself a benefit.

9. **No experimental feature flag.** This PRD adds no pane, field, command, tab, footer, keybinding, or other user-visible TUI surface. The repository's `experimental` presentation flag is therefore not applicable.

## Evaluation Method

Before connecting Dosu, commit a benchmark appendix to the pilot runbook containing:

- 20 representative architecture, behavior, governance, and historical-decision questions with expected source artifacts;
- at least 10 known related or duplicate issue pairs, including both obvious textual matches and conceptually related issues with different wording;
- a representative sample of at least 10 merged or open PRs that changed user-visible behavior, including examples where docs were correctly updated and where a known documentation gap remained;
- baseline completion time and source quality using normal repository search;
- the same measurements using `dot-ai-manageKnowledge` where it is available.

For each Dosu result, record:

- whether it found the authoritative source;
- whether every material claim was supported by a valid citation;
- whether the answer was correct, incomplete, misleading, or unsupported;
- elapsed time and, where the client exposes it, token/credit consumption;
- maintainer review or correction time;
- whether it found distinct information the other routes missed.

Do not create synthetic public issues merely to exercise Dosu. Use existing artifacts and private/web-app benchmark queries; explicit GitHub mentions are limited to cases where a public answer is genuinely useful to the thread.

## Success Criteria

The pilot is successful only if all safety criteria and the relevant value criteria pass.

### Safety

- No public response is posted without an explicit `@dosu` mention.
- No issue or PR is closed, approved, merged, marked stale, assigned a workflow label, or given a PR size label by Dosu.
- No generated change is pushed to the repository and no follow-up documentation PR is opened.
- No secret, private source, local transcript, test sandbox, or `.dot-agent-deck/` coordination artifact is indexed or committed.
- Disabling the GitHub Agent, removing the Library source, revoking the MCP key, and uninstalling the App are documented and tested as rollback steps.

### Knowledge retrieval

- At least 16 of the 20 benchmark questions identify the expected authoritative source and produce a materially correct answer.
- Zero benchmark answers invent a repository rule, command, test guarantee, or current GitHub state without a supporting source.
- Dosu provides distinct useful context on at least 4 questions, or demonstrates a meaningful measured time/context saving over both existing routes.

### Issue discovery

- At least 8 of 10 known related/duplicate pairs are surfaced without more than 2 unrelated high-confidence suggestions.
- Maintainers confirm that suggestions are advisory and do not interfere with #421's dispatch-claim design or the taxonomy decision in #603.

### Documentation review

- At least 10 representative PRs are evaluated.
- At least 70% of actionable documentation findings are judged correct and relevant.
- The review adds no more than five minutes median maintainer handling time per evaluated PR.
- At least one real documentation gap missed by the original PR workflow is found, unless the sample contains no such gap; absence of a gap is recorded rather than manufacturing one.

### Cost and operability

- The pilot stays entirely within the confirmed free OSS allowance, with overages disabled.
- Credit consumption is low enough to support the intended monthly workload or the retained scope is narrowed accordingly.
- A Dosu outage or exhausted allowance leaves all existing repository workflows operational.

## Milestones

- [ ] **M1 — Eligibility, permissions, and ownership confirmed.** Dosu confirms the applicable Maintainers Free terms, credit allowance, seats, included features, and no-overage posture for `vfarcic/dot-agent-deck`; one maintainer owns the external configuration and rollback.
- [ ] **M2 — Reproducible pilot specification.** Add `docs/develop/dosu-pilot.md` and link it from `CONTRIBUTING.md`; record permissions, include/exclude patterns, disabled features, credential handling, benchmark artifacts, measurements, and rollback steps before connecting the App.
- [ ] **M3 — Minimal GitHub and Library setup.** Install the App for this repository only; create the public Library; verify excluded paths are absent; configure Mention-only replies and every prohibited automation as disabled; capture redacted evidence in the runbook.
- [ ] **M4 — On-demand agent knowledge evaluation.** Connect one maintainer through MCP without proactive hooks or committed credentials; run the fixed 20-question comparison against repository search and `dot-ai-manageKnowledge`; record accuracy, citations, time, credits, and distinct value.
- [ ] **M5 — Issue discovery evaluation.** Test related/duplicate discovery against the fixed historical set; keep labeling disabled until results are reviewed, then optionally trial only the approved semantic label allowlist without workflow or priority/size labels.
- [ ] **M6 — Documentation-drift evaluation.** Import only the selected user-facing Documents; enable narrow-path Knowledge Review with Auto-Accept and sync-back disabled; evaluate at least 10 representative PRs and apply accepted findings manually to the original PR where appropriate.
- [ ] **M7 — Go/no-go and cleanup.** After 30 days, publish the scored comparison and choose retain, narrow, or remove; if retained, document its exact non-authoritative role and update stale process wording; if rejected, uninstall/revoke/delete the pilot configuration and record that rollback completed.

## Validation and repository gates

This integration changes maintainer process and external GitHub state, not TUI behavior. It does not require L1 snapshots or PTY-attached L2 tests unless implementation later adds a user-visible dot-agent-deck surface, which is outside this PRD.

Source-controlled changes must still satisfy normal repository gates:

- validate Markdown links and the `CONTRIBUTING.md` developer-doc index;
- run `cargo fmt --check` and `cargo clippy --workspace --all-targets --features e2e -- -D warnings` before every commit, even if the expected Rust diff is empty;
- run `cargo test-fast` for any task that changes executable or checked repository behavior;
- do not run `cargo test-e2e` unless implementation unexpectedly changes the binary or reaches the pre-PR release gate.

External-state validation is recorded with timestamps and redacted screenshots or text in the pilot runbook. Never capture API keys, installation tokens, or private account identifiers.

## Risks and Mitigations

- **Published OSS terms do not equal unlimited free use.** The ordinary Free tier has one seat and 200 credits, while Maintainers Free requires contact. *Mitigation*: M1 blocks activation until exact limits and no-overage behavior are confirmed; no paid upgrade is in scope.
- **Bot noise on a maintainer-authored issue ledger.** Most issues are internal engineering findings, not support questions. *Mitigation*: Mention-only public behavior and no auto-reply.
- **Conflicting automation.** Dosu can overlap with Greptile, stale Actions, path labeling, and #421. *Mitigation*: every overlapping action is disabled; Dosu remains advisory and is never required.
- **Misleading PR size labels.** Dosu excludes test and configuration changes that matter here. *Mitigation*: built-in PR sizing stays disabled.
- **Documentation becomes split between Dosu and the repository.** Saved Topics or Documents could become an unreviewed second source of truth. *Mitigation*: durable decisions must be written back manually through the existing repository workflow; no Dosu-only policy is authoritative.
- **Generated documentation causes CI and approval churn.** Sync PRs would invoke protected-main governance and may arrive after the implementation PR. *Mitigation*: no sync-back or generated PRs; use findings on the original PR.
- **Third-party context is stale or hallucinates authority.** Indexed data and generated synthesis can lag or be wrong. *Mitigation*: require citations, recheck mutable claims, score unsupported assertions as failures, and never let Dosu own a gate.
- **Data or credential exposure.** The App has read access to contents and write access to GitHub collaboration surfaces; MCP requests and answers are service data. *Mitigation*: public repo only, strict source exclusions, local scoped credentials, no local transcripts or coordination files, and tested revocation.
- **Vendor lock-in or service outage.** Agent workflows could learn to depend on Dosu. *Mitigation*: no mandatory hook or instruction, repository remains source of truth, and all existing workflows must function with Dosu absent.
- **Evaluation bias.** Ad hoc questions can make any retrieval system look good. *Mitigation*: fix the benchmark and expected sources before activation and run the same set against every route.

## Rollback

Rollback must be possible without a code release:

1. Pause or delete the Dosu GitHub Agent.
2. Disable the Source Monitor and remove the repository from the Library.
3. Revoke the Dosu MCP/API key and remove the local MCP client entry.
4. Uninstall or restrict the Dosu GitHub App installation.
5. Confirm no Dosu check is required by the repository ruleset and no webhook or bot setting remains active.
6. Retain the benchmark and decision record in `docs/develop/dosu-pilot.md`; remove only credentials and external configuration.

No product migration or data conversion is required because the repository, GitHub issues, PRs, and existing documentation remain authoritative throughout the pilot.

## Open Questions

- What credit allowance and team-member limit does Dosu grant this repository under Maintainers Free?
- Can two maintainers review drafts under that offering without a paid seat?
- Does related-issue detection run without enabling automatic replies or labels?
- Can Knowledge Review be enabled without any repository sync-back permission or generated PR behavior?
- Which MCP client exposes enough usage metadata to compare token/time savings consistently?
- Is `dot-ai-manageKnowledge` available and populated consistently enough to serve as the full pilot baseline, or should checkout search be the only mandatory comparator?
