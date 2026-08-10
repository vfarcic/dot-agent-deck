# PRD #421: Issue triage labels, and a dispatch claim the deck writes back to GitHub

**Status**: Not started — **Phases 1–2 overlap an open implementation, [PR #471](https://github.com/vfarcic/dot-agent-deck/pull/471), whose design differs on most axes. Reconcile before building either. See Open Question 4.**
**Priority**: Medium
**Created**: 2026-08-09
**GitHub Issue**: [#421](https://github.com/vfarcic/dot-agent-deck/issues/421)
**Feature flag**: None. Rule 9's trigger is a new *user-visible surface* (pane, field, command, tab, footer, keybinding) and this PRD introduces none — the only new UI-adjacent artifact is a repo skill. The `experimental` flag is also the wrong tool here by rule 9's own terms: it is a **presentation** switch (PRD #139 M3.2, "never branch business logic / daemon protocols / hook handling on the flag"), and every behaviour below is dispatch logic. Phases 2 and 3 are de-risked by being best-effort and by the Phase 3 kill switch, not by a flag.

## Problem Statement

Two separate gaps, both stemming from the same fact: **the deck reads GitHub and never writes to it.** Its entire `gh` surface today is three read-only invocations — `repo clone` (`src/issue_dispatch_run.rs:531`), `issue list` (`:647`), `pr list` (`:655`). There is no write of any kind, anywhere in `src/`.

**Gap 1 — nothing is triaged.** Issues carry no priority and no size. Of ~110 open issues, the large majority carry no labels at all; #421 itself was unlabelled when this PRD was written. The existing label vocabulary (`bug`, `enhancement`, `documentation`, `PRD`, …) describes *kind*, never *urgency* or *cost*, so there is no way to answer "what should be picked up next" without reading every issue. `manual-review` exists as a label but is referenced nowhere in the repo — a leftover.

**Gap 2 — dispatched work is invisible on GitHub until a PR exists.** When an `issue_dispatch` task puts an agent on an issue, it leaves no mark on the issue. `dispatch_decision` (`src/issue_dispatch.rs:277`) gates on two signals, both inferred side-effects:

1. the per-issue worktree exists on disk, or
2. an open PR's head branch is `agent/issue-<n>`.

Neither is visible to a human scanning the issue list, and neither is checkable by a *second* deck before it starts work. PRD #120 deliberately chose the head-branch check over parsing `Closes #n` (`src/issue_dispatch.rs:159-162`), so the deck never writes an issue↔PR link either. The result is a blind window between dispatch and the first PR — which, for a long-running or abandoned agent, is the entire lifetime of the work. Two decks pointed at the same repo will both dispatch the same issue; today that only surfaces later, when the second push collides on the shared `agent/issue-<n>` branch.

### Why the triage classifier runs locally

CI cannot do it: the repo's Actions secrets are exactly `HOMEBREW_TAP_TOKEN`, `RELEASE_TOKEN`, `SCOOP_BUCKET_TOKEN`, and workflow `secrets.*` references add only `GITHUB_TOKEN`. There is no LLM credential in CI and no intent to add one. The deck already spawns agents on a cron with `gh` on `PATH`, so the classifier belongs there.

## Solution Overview

Three phases, deliberately ordered so the zero-code win lands first and the riskiest change lands last behind a kill switch.

**Phase 1 — triage labels, no app code.** A repo skill carries the vocabulary and the uncertainty rule; an ordinary `[[scheduled_tasks]]` entry invokes it on a cron. This needs nothing from the application: a plain scheduled task already delivers a prompt to an agent that inherits the daemon's `PATH` and `gh` auth, and `docs/scheduled-tasks.md` already documents the prompt-invokes-a-skill pattern (`prompt = "/prd-full {{issue_number}}"`).

**Phase 2 — a visible claim, display-only.** On successful dispatch, assign the issue: `gh issue edit <n> --add-assignee @me`. Assignment is GitHub's first-class "someone is on this" — it renders in the issue list, is filterable, and needs no new vocabulary in any repo. The project's own docs already offer `query = "is:open no:assignee"` as the advanced-filter example; this closes that loop. **Whether the claim is ever cleared is an open decision (M2.2)** — an unconditional release on tab close deletes a human's own self-assignment and is ruled out. **Best-effort throughout: a failure never fails a dispatch.**

**Phase 3 — the claim becomes a third `dispatch_decision` signal**, so a second deck will not start work a first deck already began.

### Architecture: why Phase 3 derives rather than maintains

A claim the deck *maintains* rots. The only production site that reclaims a dispatched worktree is the `StopAgent` handler (`src/daemon_protocol.rs:1501-1510`), gated on `take_worktree` against a registry that is **wiped on daemon restart** (`src/issue_dispatch_run.rs:75`). So a maintained claim is orphaned whenever the daemon restarts before the tab closes, whenever `remove_worktree` fails (it is best-effort and only warns, `:225-229`), whenever the daemon is killed by a stray signal (a known recurring event here — #428, #272), and whenever the user follows the documented manual path: *"Run `git worktree remove` (or reopen and close the tab) to release a slot manually"* (`docs/scheduled-tasks.md`). An orphaned claim that also *gates* dispatch silently suppresses that issue on every machine, forever.

Phase 3 therefore follows #422's rule — **derive the verdict from live GitHub state at decision time; never trust a flag to have been cleared.** Concretely, an assignee alone is not a claim. A claim is *fresh assignment plus no evidence the work is dead*:

- **Claim age comes from GitHub, not from deck-local state.** `GET /repos/{owner}/{repo}/issues/{n}/events` returns each event's `created_at` (verified against this repo; assignment events additionally carry `assignee`). The age of the most recent `assigned` event is therefore computable with no local bookkeeping, which is exactly what makes it survive a daemon restart, a machine swap, or a `~/.config` wipe.
- **A claim older than a configurable TTL is stale and does not gate.** The issue is dispatched anyway and the claim's clock must move forward — **by a mechanism the next bullet records as unresolved.** The intent is self-healing by construction: the worst case is one duplicate dispatch after the TTL, never a permanently wedged issue.
- **How the claim gets re-stamped is UNRESOLVED, and is the open design problem of this phase.** Three Greptile P1s on PR #464 landed on it in succession, each one exposed by the fix for the previous:
  1. A bare `--add-assignee @me` refreshes nothing. GitHub's assignee API is idempotent: adding an already-present assignee is a no-op emitting **no new `assigned` event**. The derived age stays pinned, the claim reads stale forever, and gating silently collapses from the first TTL expiry — failing *open*, with no signal.
  2. Releasing the claim is destructive. `--remove-assignee @me` cannot tell the deck's claim from the user's own, because they are the same GitHub identity — so an unconditional release on tab close deletes a human's self-assignment. Recorded in full under Phase 2 below, since that is where the release call lives.
  3. So re-stamp with an explicit **unassign-then-reassign** pair, which does emit a fresh event. But every write on the claim path is best-effort by design, so a successful unassign followed by a failed reassign leaves the issue **fully unassigned** — reintroducing finding 2's destructive case through a different door. The pair is also non-atomic even when both calls succeed: between them the issue reads unclaimed, and a concurrent deck can dispatch into that window.

  **All three trace to one root cause, already named in Risks: assignment is a shared GitHub identity, not deck-owned vocabulary.** It cannot be re-stamped without a destructive step, and it cannot be attributed to the deck. That is structural, not a bug to patch — which is why this bullet records the problem rather than pretending a third patch settles it.

  Candidate resolutions, to be decided before Phase 3 is built:
  - **A deck-owned `in-progress` label instead of assignment** for the *gating* claim. `labeled` events carry timestamps just as `assigned` events do, so the TTL still derives from GitHub. Remove-then-add is still non-atomic, but it destroys nothing a human owns and is cleanly attributable. This reverses the choice made when the PRD was written; the accumulated evidence favours it, and it remains the leading candidate. Its one cost — the label must exist in every dispatched repo, since `--add-label` fails where it is absent — is **real, and is paid on the dispatch path**, not dissolved by Phase 1 as an earlier revision of this bullet claimed. See "Provisioning the gating label" below.
  - **Never refresh.** Anchor freshness to something that moves on its own — recent commits on `agent/issue-<n>`, or PR activity — so no re-stamp is ever needed. Truest to "derive, don't maintain", and it removes every destructive call. **Known gap: the dispatch → first-commit window.** A freshly dispatched issue has no branch activity yet, so the claim reads stale immediately and the mechanism provides nothing exactly when it is most needed. That window needs an explicit bound in the design (a grace period anchored to branch creation, say) rather than an assumption that agents commit quickly.
  - **Split the jobs.** Keep assignment as a write-only human-visible marker that is never removed (Phase 2, no destructive call at all) and gate on the two signals that already exist — worktree presence and an open PR on `agent/issue-<n>`. This is not a deferral: it is the minimal answer, and it is what the original analysis of #421 recommended before cross-machine gating was brought into scope. It de-risks Phase 2 immediately and costs Phase 3's cross-machine coordination, which has never been observed to be needed here.

  Note that **M3.3's interrupted-re-stamp test stands as a regression guard regardless of which candidate wins** — including the ones that avoid re-stamping — so that a later change cannot quietly reintroduce a destructive write.

  **M3.1 must also confirm the idempotent-assign behaviour empirically** whichever resolution wins — it was asserted rather than measured when this PRD was first written, which is how finding 1 got in.
- **The open-PR signal still wins.** An open PR on `agent/issue-<n>` means real work exists regardless of claim age.

This is what makes the phase safe, and it is why **M3.3 is the load-bearing milestone**: if a stale claim can wedge an issue, the phase is a regression however well the rest works.

#### Provisioning the gating label

**A fourth Greptile P1 on PR #464 landed on the label candidate itself: nothing in this PRD ever creates `in-progress` in a dispatch target.** The finding is valid, and it is the same failure shape as finding 1 — a gate that silently stops gating — arriving through the candidate that was supposed to fix it. Recorded here rather than patched into the bullet above, because the answer changes *where* the cost lands.

**Phase 1 cannot provision it, and the earlier claim that it "dissolves the cost" was wrong on two counts.** First, `in-progress` is not in Phase 1's vocabulary: In Scope and M1.1 list `priority:*`, `size:*` and `needs-triage`, and nothing else. Second — the part that survives even if the label is added to that list — **Phase 1 offers no ordering guarantee over Phase 3.** The triage schedule is a hand-written `[[scheduled_tasks]]` entry naming one repo in its prompt, configured independently of every `issue_dispatch` task's `repo`. A repo can therefore enter the dispatch queue having never been triaged, and nothing detects it. Both halves are measurable today: `vfarcic/dot-agent-deck` carries no `in-progress` label, and `vfarcic/dot-ai` — the target `docs/scheduled-tasks.md` uses for its own `issue_dispatch` example — carries neither `in-progress` nor any of the seven triage labels.

**So provisioning belongs on the dispatch path**: once per run, before any issue's claim is attempted, and **unconditional** — never gated on whether triage is enabled, because the claim never is. `gh label create` **without** `--force`, so a repo that already carries its own `in-progress` keeps its colour and description instead of having them rewritten on every fire (up to 96×/day on a `*/15` cron). Without `--force` that call *fails* once the label exists, which is the steady state from the second run onward, so the implementation must recognise `gh`'s already-exists rejection and treat it as success — noting that `gh` reports it as CLI prose (`label with name "<name>" already exists`) with no status code or JSON to branch on, so the match is textual and can drift on a `gh` upgrade. It should drift toward a spurious warning, never toward swallowing a genuine failure.

**When the claim write fails anyway, it must be reported, not logged.** Best-effort is right — a claim failure must never fail a dispatch, which genuinely succeeded — but "best-effort" was never meant to mean invisible. A `tracing::warn!` reaches a sink nobody watching the deck is looking at, so the exact case this PRD exists to prevent (a read-only token; a repo the user cannot label) presents as a run of clean successful dispatches while every claim silently no-ops and a second deck duplicates all of the work. **A failed claim needs its own notification, distinguishable from both a successful dispatch and a failed one.** That obligation holds for the assignee candidate too — it is the general rule that finding 1 should have produced and did not.

**PR #471 has already implemented exactly this**, and reached it the same way, through its own review rather than by design: `ensure_claim_label` (unconditional, once per run, create-without-`--force`, already-exists treated as success), `NotifyEvent::IssueClaimFailed` for the write failure, and `scheduler/dispatch/020`, an e2e test that dispatches against a repo seeded with *no* labels and asserts the claim genuinely succeeded rather than merely being attempted. Its `gh` stub was tightened to hard-error on an unknown label name, mirroring real `gh`'s client-side name-to-ID resolution — which is also the empirical confirmation that `--add-label` fails where the label is absent, a claim this PRD had until now only asserted.

## Scope

### In Scope

- A `triage-issues` repo skill carrying the label vocabulary and the uncertainty rule, invocable as `/triage-issues <owner/name>`.
- Creating the label vocabulary: `priority:high|medium|low`, `size:high|medium|low`, `needs-triage`. **The separator is unsettled** — the repo has no `namespace:value` label today and its only compound label is hyphenated (`ci-cd`), while PR #471 implements the hyphenated form. See Open Question 4.
- A documented `[[scheduled_tasks]]` recipe that invokes the skill on a cron.
- Assignee-based dispatch claim: written after a successful spawn, best-effort at every step. **Whether it is ever cleared is M2.2's decision** — an unconditional release on tab close is ruled out (see Phase 2).
- Claim-aware `dispatch_decision`: a third signal, TTL-bounded, derived from live GitHub state.
- **If the claim is a label**, provisioning it in the dispatch target: once per run, on the dispatch path, before any claim write, unconditional. Phase 1 cannot carry this — see "Provisioning the gating label".
- **A notification for a failed claim write**, distinct from a successful dispatch and from a failed one. Best-effort must not mean invisible; a claim that silently no-ops is a gate that silently stops gating.
- A kill switch so claim-gating can be turned off in config without a rebuild.
- Test coverage across both tiers, including the `gh` stub arms that make the writes observable.
- Docs: `docs/scheduled-tasks.md` (the claim, the TTL, the recipe), `CONTRIBUTING.md` (the label vocabulary).
- A `changelog.d/` fragment covering the cross-build behaviour skew Phase 3 introduces.

### Out of Scope

- **Sticky claim comments.** Assignment was chosen over a comment; a comment notifies every watcher, resets the `actions/stale` clock (`.github/workflows/stale.yml`), and needs comment-id bookkeeping to edit rather than append. Revisit only if assignment proves insufficient. Note that PR #471 posts exactly such a comment, to record the claimant — see Open Question 4.
- ~~**An `in-progress` label.** Superseded by assignment.~~ **Reopened** — this exclusion no longer holds. It rested on the label needing to exist in every dispatched repo (`--add-label` fails where it is absent), a cost Phase 1's per-repo triage run largely dissolves. Three P1 findings against the assignee mechanism now make the label the **leading candidate** for the gating claim (see Architecture), and PR #471 has already implemented it. The assignee-vs-label decision is live, not settled.
- **Recording *which orchestration* claimed an issue.** Assignment identifies a GitHub user, not an instance. See "The identity question" below for why the obvious mechanism does not work; #425's local ownership marker is the right home if this is wanted later.
- **Non-GitHub forges.** Dispatch stays `gh`-only.
- **Retro-triage of the existing backlog by hand.** The first scheduled run handles it.
- **Any restriction on what the triage agent may run.** Decided: the skill scopes behaviour by prompt only. See Risks.

## Technical Approach

### Phase 1 — triage skill and schedule

The skill lives at `.claude/skills/triage-issues/SKILL.md` with `user-invocable: true`. It states the vocabulary, and the rule that decides the whole phase's usefulness: **when not confident, apply `needs-triage` and leave priority unset.** A wrong priority is worse than an absent one, because it is indistinguishable from a considered one.

The schedule is a plain task, not an `issue_dispatch` one — `issue_dispatch` would clone the repo and build a worktree per issue, which is heavy machinery for a job that only reads issue text:

```toml
[[scheduled_tasks]]
name = "triage-issues"
cron = "0 8 * * MON-FRI"
working_dir = "~/code/dot-agent-deck"   # must be a checkout of THIS repo
command = "claude"                       # required for a plain task
prompt = "/triage-issues vfarcic/dot-agent-deck"
enabled = true
```

Two constraints that are easy to get wrong, and belong in the docs: `working_dir` must be a checkout of this repo because `.claude/skills/` is project-scoped — that is how the agent discovers the skill; it is *not* the repo being triaged, which is an argument in the prompt. And `--command` is mandatory for a plain task (`src/schedule_cli.rs:70`: *"a scheduled task needs an agent command … to act on its prompt"*); only `issue_dispatch` tasks may omit it.

**Durability gap worth stating.** The three artifacts live in three places and only one is versioned: the skill is in git; the schedule is in `~/.config/dot-agent-deck/schedules.toml` on whichever machine runs the daemon; the labels are GitHub repo settings. The schedule vanishes with the machine. Documenting the recipe in `docs/scheduled-tasks.md` is the mitigation — the recipe is reproducible even though the instance is not.

### Phase 2 — the assignee claim

A pure argv builder in `src/issue_dispatch.rs` (unit-testable, alongside `issue_list_argv` / `pr_list_for_issue_argv`, carrying the same `--` end-of-options guard), plus a `claim_issue` / `release_issue` pair in `src/issue_dispatch_run.rs` modelled on `ensure_worktrees_excluded`'s never-fatal discipline.

Write ordering is the one thing that must not be got wrong: **claim only after `spawn` returns `Ok`** (`src/issue_dispatch_run.rs:448`), so a failed dispatch never leaves a claim. Release from the existing close path (`src/daemon_protocol.rs:1501-1510`), beside `remove_worktree`.

**Release must not be unconditional** (Greptile P1, PR #464). `--add-assignee @me` is idempotent and harmless, but `--remove-assignee @me` is destructive and **cannot tell the deck's own claim from the user's**, because they are the same GitHub identity. So a human who assigns themselves to an issue, which the deck then dispatches and later closes, has their own signal silently deleted. The same applies to two decks sharing one GitHub account: the first tab to close unassigns while the second is still working, and the issue reads as free.

This is the sharp edge of choosing assignment over a deck-owned label, and it lands in Phase 2 — the phase otherwise characterised as safe because it is display-only. Two candidate mitigations, to be decided in M2.2:

- **Release only a claim the deck can prove it created.** Local provenance is exactly what [#425](https://github.com/vfarcic/dot-agent-deck/issues/425)'s worktree ownership marker provides, so this converges with work already proposed rather than inventing a second mechanism. Preferred if #425 lands first.
- **Do not release at all**; let the TTL expire the claim. Consistent with "derive, don't maintain", and it removes the destructive call entirely. Cost: the assignment lingers for up to one TTL after the tab closes, so the issue looks busier than it is.

Best-effort means a missing permission, a read-only repo, or a `gh` failure never fails the dispatch, which genuinely succeeded. It does **not** mean the failure is invisible: per M3.6 it raises its own notification rather than only a WARN. Phase 2's claim is decorative, so a swallowed failure costs little there — but the same code path carries Phase 3's gate, where a silent no-op is a gate that has stopped gating, and this is exactly why Phase 3 cannot simply trust the claim to exist.

### Phase 3 — claim-aware dispatch

`dispatch_decision` gains a third parameter. The pure function stays pure — the caller resolves claim state and passes a verdict in, so the truth table remains exhaustively unit-testable. Evaluation order matters for the same reason the existing short-circuit does (`src/issue_dispatch_run.rs:361-371`): the worktree signal is local and free, so it is checked first and a `gh` failure on a locally-claimed issue can never turn a clean skip into a spurious failure.

The kill switch is a config knob on `IssueDispatchConfig` (`src/config.rs:586`) — claim-gating off by default until the TTL behaviour has been observed in the wild, so the feature cannot wedge a backlog while it is being trusted.

**Cross-version behaviour (CLAUDE.md rule 12).** This touches the daemon, so the check is triggered. The TUI↔daemon wire is **unchanged** — no `PROTOCOL_VERSION` bump. But an old daemon dispatches without claiming while a new daemon reads claims and skips, so a mixed fleet disagrees about what is in flight. That skew is a compatibility note, not a wire break: it needs a `changelog.d/421.md` fragment and the rule 12 cross-version manual run (isolating `DOT_AGENT_DECK_LOG` along with the sockets and `HOME`, per rule 12's third bullet).

### The identity question (issue §2), and why it is out of scope

Issue #421 proposes recording *which* orchestrator claimed an issue, via `mint_orchestration_id()`. Verified against the code, that mechanism cannot do the job:

1. **It is not durable.** `src/agent_pty.rs:381-382`: *"The token deliberately does NOT need to be reproducible across restarts"* — a per-process nonce plus a counter. So the test it is meant to enable ("is this my own earlier claim, or someone else's?") fails after any daemon restart, which is the same restart that already wipes `WorktreeRegistry`.
2. **It is not always minted.** `mint_orchestration_id()` is called only in the `SpawnTarget::Orchestration` branch (`src/spawn.rs:499`). A repo with no `[[orchestrations]]` block dispatches as a single-agent card and has no instance identity at all.
3. **The stated fallback is not unique where it matters.** `ScheduledTask.name` is "unique per daemon" (`src/config.rs:609`), but the default seed is `Issues <owner>/<repo>` (`src/issue_dispatch.rs:45-47`) — so two machines on default config produce byte-identical claimants, which is precisely the collision the proposal exists to solve.

The TTL design sidesteps this entirely: it never needs to know *who* claimed an issue, only *how long ago*. If per-instance identity is wanted later, #425's worktree ownership marker is the right home — local, durable, and free of GitHub-write risk.

Issue #421's closing observation is correct and worth acting on separately: **#156**'s premise ("there is no unique per-instance ID") was superseded by PRD #140. That issue's problem statement should be updated regardless of this PRD.

## Success Criteria

1. Every open issue carries either a `priority:*` + `size:*` pair or `needs-triage`, maintained without anyone remembering to do it.
2. An issue being worked on by a dispatched agent is identifiable from the GitHub issue list alone, before any PR exists.
3. A dispatch failure never leaves a claim behind.
4. **A stale claim never permanently wedges an issue** — after the TTL, the issue is dispatchable again with no manual intervention, on a machine that has never seen the original claim.
5. `tests/e2e_issue_dispatch_real.rs` stays green across consecutive runs against the live fixture, and leaves it in its original state.
6. Claim-gating can be disabled in config without a rebuild.

## Milestones

### Phase 1: Triage (no app code)

- [ ] **M1.1** — Label vocabulary created; `manual-review` resolved (kept with a description, or deleted).
- [ ] **M1.2** — `triage-issues` skill written, carrying the vocabulary and the uncertainty rule.
- [ ] **M1.3** — Schedule recipe documented in `docs/scheduled-tasks.md`, including the `working_dir`-is-this-repo and `--command`-required constraints, and the note that the schedule itself is not versioned.
- [ ] **M1.4** — First scheduled run triages the existing backlog; a human spot-checks a sample for whether the classifications are worth trusting.

### Phase 2: Visible claim (display-only)

- [ ] **M2.1** — Pure argv builders + unit tests for assign/unassign.
- [ ] **M2.2** — Claim written after a successful spawn; best-effort at every step, never fatal. **Decide and implement the release policy** — release only deck-created claims (via #425's ownership marker), or do not release and let the TTL expire it. An unconditional `--remove-assignee @me` is not acceptable: it deletes a human's own assignment.
- [ ] **M2.3** — `gh` stub learns `issue edit`, with tests asserting the claim is written on dispatch, **and asserting whatever M2.2 decided about close**: that a deck-created claim is cleared, or — under the no-release policy — that closing a tab performs *no* `--remove-assignee` call at all. Both are testable; what is not acceptable is leaving the close path unasserted. **The stub currently `exit 1`s on unrecognised argv (`tests/e2e_issue_dispatch.rs:101-102`), so a best-effort write would otherwise be silently swallowed and every existing test would stay green while proving nothing** — the stub arms are what make this milestone real.
- [ ] **M2.4** — `tests/e2e_issue_dispatch_real.rs` verified against the live fixture (`vfarcic/dot-agent-deck-tests` issue #1) across two consecutive runs, leaving it unassigned afterwards. **Under the no-release policy the fixture is not left unassigned by the deck**, so the test must do it explicitly in teardown — restoring the fixture is the test's own responsibility, not evidence that release works.
- [ ] **M2.5** — Docs updated.

### Phase 3: Claim-aware dispatch

- [ ] **M3.1** — Claim state derived from live GitHub at decision time (assignee + age of the most recent `assigned` event); no deck-local claim bookkeeping anywhere. **Empirically confirm first** that a redundant `--add-assignee` emits no new `assigned` event — the TTL design is void if that assumption is wrong in either direction.
- [ ] **M3.2** — Third signal wired into `dispatch_decision` with an exhaustive truth table; short-circuit order preserved so a `gh` failure cannot turn a local skip into a failure.
- [ ] **M3.3** — **Stale claims self-heal.** Blocked on choosing a re-stamp resolution (see Architecture): the mechanism is unresolved, and the assignee-vs-label decision should be settled first. Once chosen: a claim older than the TTL does not gate, the issue dispatches, and the derived age actually moves. Proven by a test that dispatches against a pre-aged claim written by no local daemon, **and asserts the age advanced afterwards** — without that second assertion the test passes against the broken idempotent-assign design. A second test must cover the **interrupted re-stamp** (first call succeeds, second fails) and assert no claim is destroyed. *This is the milestone the phase is only safe with — treat it as a hard gate, not a nice-to-have.*
- [ ] **M3.4** — Kill switch on `IssueDispatchConfig`, defaulting to off, with validation and a test.
- [ ] **M3.5** — Cross-version manual run per rule 12 (old daemon + new TUI, isolated `DOT_AGENT_DECK_LOG`/sockets/`HOME`), plus the `changelog.d/421.md` fragment recording the mixed-fleet skew.
- [ ] **M3.6** — **The claim can never silently no-op.** Two halves, both required, and both landing with whichever phase first *writes* the claim — so if the label candidate wins and replaces the Phase 2 write, this milestone moves into Phase 2 with it. (a) If the claim is a label, it is provisioned on the dispatch path — once per run, before any claim write, unconditional and never gated on triage; created without `--force` so a repo's own pre-existing label survives, with `gh`'s already-exists rejection treated as success. (b) A failed claim write raises its own notification, distinguishable from a successful dispatch and from a failed one; `tracing::warn!` alone does not satisfy this. Proven by a test that dispatches against a repo carrying **no** labels at all and asserts the claim genuinely *succeeded* — not merely that it was attempted, which is what a test written against the broken design also passes. PR #471's `scheduler/dispatch/020` is that test; reuse it rather than writing a second.

### Phase 4: Ship

- [ ] **M4.1** — `cargo fmt --check` and `cargo clippy --workspace --all-targets --features e2e -- -D warnings` clean; `cargo test-fast` green; `cargo test-e2e` green pre-PR.
- [ ] **M4.2** — Docs complete; `tests/CATALOG.md` entries written with `Scenario:` comments per rule 7.
- [ ] **M4.3** — PR opened, Greptile inline comments read and answered per rule 8.

## Key Files

| File | Why |
| --- | --- |
| `.claude/skills/triage-issues/SKILL.md` | New — vocabulary + uncertainty rule (Phase 1). |
| `src/issue_dispatch.rs` | Pure argv builders; `dispatch_decision` gains its third signal. |
| `src/issue_dispatch_run.rs` | `claim_issue` / `release_issue`; claim written after the `spawn` at `:448`. |
| `src/daemon_protocol.rs` | Release on close, beside `remove_worktree` (`:1501-1510`). |
| `src/config.rs` | `IssueDispatchConfig` (`:586`) gains the TTL + kill switch. |
| `tests/e2e_issue_dispatch.rs` | `gh` stub arms for `issue edit`; claim/release assertions. |
| `tests/e2e_issue_dispatch_real.rs` | Live-fixture safety — the test most likely to be broken by this PRD. |
| `docs/scheduled-tasks.md` | Triage recipe; claim behaviour; TTL; kill switch. |

## Risks and Mitigations

| Risk | Mitigation |
| --- | --- |
| **A stale claim wedges an issue permanently.** The central hazard; the reason issue #421's original "write a label and gate on it" shape was not adopted. | TTL derived from GitHub's own event timestamps, so it needs no local state to survive a restart (M3.1/M3.3). Kill switch (M3.4). Gating off by default until observed. |
| **The TTL clock never advances, so the claim silently stops gating.** Greptile P1 on PR #464. A redundant `--add-assignee @me` is a no-op emitting no new `assigned` event, so the naive re-assign refreshes nothing and the claim reads stale forever. Fails *open* and without a signal — the worst shape a safety mechanism can take. | **The re-stamp mechanism is UNRESOLVED** — see the row below and Architecture. Unassign-then-reassign was the first answer and is *ruled out*, because it reintroduces the destructive case; one of the three recorded candidates must be chosen before Phase 3 is built (M3.3). Confirm the underlying event behaviour empirically before building on it (M3.1). Whatever wins, the M3.3 test must assert the age *advanced*, not merely that a re-dispatch happened. |
| **Releasing a claim deletes a human's own assignment.** Greptile P1 on PR #464. `--remove-assignee @me` cannot distinguish the deck's claim from the user's — same GitHub identity. Also fires when two decks share one account. Lands in Phase 2, the phase otherwise treated as safe. | Release only deck-created claims via #425's ownership marker, or do not release at all and let the TTL expire it (M2.2). An unconditional release is ruled out. |
| **Re-stamping can erase the assignment outright.** Greptile P1 (third) on PR #464, exposed by the fix for the first. Unassign-then-reassign is best-effort like every claim write, so a successful unassign plus a failed reassign leaves the issue unassigned — the destructive case the release policy forbids, via a different door. Non-atomic even on success: a concurrent deck can dispatch in the gap. | Unresolved by design, not patched a third time. Recorded in Architecture with three candidate resolutions; one must be chosen before Phase 3 is built (M3.3). |
| **Assignment is a shared identity, not deck-owned vocabulary.** Root cause of all three P1s above, and the strongest argument against choosing assignment over an `in-progress` label. Three findings in a row tracing to one cause is evidence the mechanism is wrong, not that the patches were. | **The assignee-vs-label decision should be revisited before Phase 3 is built.** A deck-written label has none of the three failure modes — it is attributable, freely re-stampable, and destroys no human signal — at the cost of per-repo vocabulary that `--add-label` requires to exist. Phase 2 (display-only, write-only) is unaffected either way. |
| **The gating label does not exist in the dispatch target, so the claim silently no-ops.** Greptile P1 (fourth) on PR #464. `--add-label` hard-errors on an unknown label, the write is best-effort, so the issue dispatches unclaimed and a second deck duplicates the work — a gate that fails *open* with no signal, the same shape as the TTL row above. Measured: neither `vfarcic/dot-agent-deck` nor `vfarcic/dot-ai` (the docs' own dispatch example) carries `in-progress` today. | Provision on the **dispatch path** — once per run, before any claim, unconditional — not in Phase 1, which offers no ordering guarantee over Phase 3 (M3.6). Report a failed claim through the deck's own notifications, never `tracing::warn!` alone. See "Provisioning the gating label"; PR #471 implements both. |
| **The live e2e fixture is mutated or wedged.** `tests/e2e_issue_dispatch_real.rs` runs against real GitHub, `max_per_run = 1`, on a permanent issue titled "DO NOT CLOSE" (verified: 0 comments, one label). A claim that is also a skip signal makes the second run skip its only issue and go red. | M2.4 gates on two consecutive green runs that leave the fixture unassigned — by explicit teardown, since under the no-release policy the deck itself never clears it. Assignment (unlike a label) needs no per-repo vocabulary, so the fixture repo needs no setup. A label-based claim needs `in-progress` present — which `vfarcic/dot-agent-deck-tests` already carries (verified 2026-08-10, seeded for #471), though M3.6's dispatch-path provisioning makes the fixture's own state irrelevant either way. |
| **Best-effort writes are silently untested.** The `gh` stub `exit 1`s on unknown argv, so a swallowed write leaves every test green. | M2.3 makes the stub arms and the assertions a milestone in their own right. |
| **The triage agent exceeds its remit.** Decided: scoped by prompt only, no argv allowlist. The same `gh issue edit` it needs also takes `--title`, `--body`, `--add-assignee`, `--milestone`; `gh issue close` sits beside it. It runs unattended, on a cron, as the repo owner. | Accepted deliberately. Bounded in practice by M1.4's spot-check before the schedule is trusted, and by the fact that issue edits are fully reversible and auditable in the issue's own event history. Revisit if a run misbehaves. |
| **Writes to third-party repos.** `issue_dispatch` accepts any `owner/name`; the deck has never written to GitHub before. | Assignment fails closed and best-effort on repos where the user lacks write access. Documented as a behaviour change. |
| **Mixed-fleet skew.** Old daemons do not claim; new daemons skip on claims. | Changelog fragment + rule 12 cross-version run (M3.5). TTL bounds the divergence. |

## Open Questions

1. **What TTL?** Long enough that a legitimately slow agent is not preempted, short enough that a dead claim clears within a working day. A starting proposal is 24h, revisited after M1.4-style observation.
2. **Should the deck reconcile claims it did not write?** An issue assigned by a *human* reads as claimed. That is arguably correct on the **read** side — a human working an issue is a real claim — but it means assigning yourself quietly opts an issue out of dispatch. The **write** side is not a question but a defect, now tracked in Risks and M2.2: release must never delete an assignment the deck cannot prove it made.
3. **Does triage want a `sort` interaction with dispatch?** Once priorities exist, `query = "is:open label:priority:high sort:created-asc"` becomes the natural dispatch filter. PRD #222 already proposes a first-class `sort` field; these two should be aligned rather than solved twice.
4. **How does this PRD reconcile with [PR #471](https://github.com/vfarcic/dot-agent-deck/pull/471)?** — **the blocking question.** #471 ("claim dispatched issues and triage them on dispatch", by @prageethw, open, ~2.3k lines) implements Phases 1–2 of this issue independently, and its design differs from this PRD on almost every axis. It is not a merge conflict — the two touch disjoint files — but a design disagreement that must be settled by the two maintainers rather than by whichever PR merges second.

   | Axis | This PRD | PR #471 |
   | --- | --- | --- |
   | Claim mechanism | assignee (`--add-assignee @me`) | `in-progress` label |
   | Claim expiry | TTL derived from the age of the most recent `assigned` event; a stale claim stops gating | **none** — the label gates until a human removes it or the issue closes |
   | Kill switch | required (M3.4), gating off by default | none; the label always gates |
   | Claimant record | out of scope (identity mechanism shown unworkable) | a posted comment naming task + host + timestamp |
   | Triage trigger | plain `[[scheduled_tasks]]` entry → `/triage-issues` repo skill | `triage = true` on `issue_dispatch`, plus a `schedule add --triage` flag |
   | Triage coverage | the whole backlog (Success Criterion 1) | only issues a fire actually dispatches |
   | Label separator | `priority:high` | `priority-high` |
   | Skill artifact | `.claude/skills/triage-issues/SKILL.md` | none — the instruction is appended to the dispatch prompt |

   Three of these deserve explicit attention:

   - **#471 ships the shape this PRD names as its central hazard.** The first Risks row says a stale claim wedging an issue permanently is "the reason issue #421's original 'write a label and gate on it' shape was not adopted". #471 is that shape with no TTL and no kill switch, and documents the wedge as intended behaviour: *"subsequent fires keep skipping the issue until someone removes the label."* That is the single substantive disagreement — everything else is vocabulary or packaging. It cuts both ways: #471's label is *attributable* and so avoids all three P1s that sank the assignee design, while this PRD's TTL is what stops a claim from wedging a backlog. **The two are complementary, not mutually exclusive: a deck-owned label with a TTL and a kill switch is neither PR's current design and is very likely the right answer.**
   - **#471 adopts two things this PRD puts out of scope** — the `in-progress` label (an exclusion this PRD has since reopened on its own evidence, so the two now agree) and a sticky claim comment (still excluded here, on the grounds that it notifies every watcher and resets the `actions/stale` clock — a concern #471 does not address).
   - **#471's triage cannot satisfy Success Criterion 1.** Triaging only dispatched issues never labels the backlog, so "every open issue carries a `priority:*`/`size:*` pair or `needs-triage`" stays unmet no matter how long #471 runs. The two triage designs are for different jobs and could coexist, but the criterion belongs to the scheduled-skill design.
   - **On one axis #471 is simply ahead, and this PRD has been corrected to match it: provisioning the gating label.** #471's `ensure_claim_label` runs unconditionally once per dispatch run, and `NotifyEvent::IssueClaimFailed` makes a failed claim visible instead of burying it in a log — both reached through #471's own review, and both now required here as M3.6. This is worth noticing beyond the one fix: the axes above read as a design disagreement, but on the question of what happens when a best-effort write *fails*, the implementation had been more careful than the PRD. That is an argument for reconciling the two rather than picking a winner.

   Note also that #471's code comments cite `PRD #421 M1.0`, `M2.0` and `M3.0` — milestone numbers that do not exist in this document (which runs M1.1–M4.3). #471 was developed against a different PRD on the author's fork, so **whichever design wins, those references need repointing** or they will mislead every future reader.
