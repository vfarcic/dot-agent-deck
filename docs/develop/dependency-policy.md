# Dependency-aware execution: cases, vocabulary, simulation and recommendation

This page is the evidence for PRD #629's discovery. It covers real task sets from this repository's own agent work, a minimal dependency vocabulary built on the identities that exist today, which decisions could be deterministic and which should stay advisory or with agents and people, a simulation of three policies against the sampled cases, and a **Revise** recommendation. The decision itself belongs to the maintainer.

**This page describes `origin/main` at `508613d1` (2026-10-30, after v0.49.3).** PRD #1666 part 3 is rewriting dispatch, the dispatched-units registry, dispatch returns and the daemon's restart behaviour, which are the places a product-level check would sit. This discovery therefore changed no code and committed no prototype. The cases come from Git and GitHub history, which part 3 does not change.

## How the evidence was gathered

- **Premise check.** The PRD (written 2026-08-21) was read against the current source, the related issues' state and `git log`. The result is posted on the issue ([progress comment](https://github.com/vfarcic/dot-agent-deck/issues/629#issuecomment-6534482292)) and summarised in the next section.
- **Cases.** These are reconstructed from pull requests, their commit graphs (merge commits re-merged with `git merge-tree --write-tree` in a bare mirror holding `refs/pull/*/head`), issue bodies, and the skills' own accounts of the incidents that produced their rules. A case taken from a skill's account and not re-derived from the records says so.
- **Overlap statistics.** These cover the 300 most recently merged `agent/*` PRs (merged 2026-09-26 to 2026-10-30). Two PRs overlap when their open windows intersect and they share at least one changed file. A PR counts as conflicted when re-merging the parents of one of its merge commits conflicts.
- **Declared dependencies.** The bodies of the issues those 300 PRs close were searched for ordering phrases ("depends on #n", "after #n merges", "sequenced after", "builds on", "blocked by", "port … onto"), and every hit was read by hand.
- **Who did it.** An agent did the reconstruction and the classification, and the times are the PRs' timestamps. The PRD's evidence protocol has the maintainer judging whether a risk was visible before spawning. That judgement is the agent's here, and each case shows the signal it relied on so that it can be checked.

## What the premise check changed

- **The product still has no dependency concept.** `dot-agent-deck dispatch` takes a name, a task and a shape, and cuts the unit from the caller's `HEAD`. It has no dependency, base or ordering option, and its exit status asserts only that the request was admitted ([`dispatcher-mode.md`](dispatcher-mode.md)). Scheduled issue dispatch selects by label, caps fires per run and claims issues; no dependency or overlap handling was found in it. Orchestration states one ordering rule: one task at a time per worker, while different workers may run in parallel.
- **#628 closed with Revise** (a durable execution journal with parent and attempt links and no dependency graph, [`work-identity.md`](work-identity.md)). So a dependency edge cannot point at a deck work id today, and #1889 (the journal) is sequenced after part 3. #628 found that none of its ten sampled runs needed a dependency edge. Cases C3, C4 and C5 below are counterexamples from task sets it did not sample.
- **#174 (cross-project dispatch) is open and unbuilt**, so its DAG and cycle semantics cannot be sampled from real runs. Its proposed `dispatch --to` also collides with the `dispatch` command that #220 shipped.
- **This repository has built a dependency and conflict policy for its own agent work, in skills rather than in the product.** Each rule followed a dated incident (see the table at the end of the next section). That makes the skills the incumbent policy this discovery has to beat, so the question is no longer "should a policy exist" but "which parts of it belong in the deck".

## Milestone 1: task sets and failure cases

### The cases

| id | kind | refs | shared area | what happened | signal at dispatch time |
| --- | --- | --- | --- | --- | --- |
| C1 | independent | PRs #1904, #1905, #1907, #1908 (2026-10-25) | none | Four units in different subsystems. Each merged in 0.5–0.8 h with no merge from `main`, and `main` stayed green. | none needed |
| C2 | independent, files overlap | #1461–#1468 (seven PRs, 2026-10-01) | three pairs share a file (`tests/delegate_prompt_injection.rs`, `src/agent_pty.rs`, `docs/develop/desktop-gui.md`) | Merged back to back with no merge commits, and all seven `main` runs were green. | file overlap, which turned out harmless |
| C3 | direct dependency | PRD #1542 → PR #1561, PRD #1497 → PR #1617, replacement PRD #1631 → PR #1698 | permission-prompt detection | #1561 was approved, then closed unmerged (open about 135 h) because it conflicted with #1617 in 33 files. The maintainer decided to port it after #1617 merged rather than resolve it, and #1698 then needed four conflicted merges. | #1542's body says it was "planned with #1497 so both detect questions the same way": a coordination note, not an ordering edge |
| C4 | hidden dependencies and overlap | #1347, #1346, #1342, #1338 (issues #447, #714, #1045, #708; 2026-09-26) | `src/state.rs`, `src/agent_pty.rs`, `src/daemon.rs`, `src/hook.rs`, desktop DTO | Stacked and merged bottom-up. It needed a semantic fix commit ("#447 x #714"), and #1342 had 11 conflicted merges. #1045 was a cross-cutting rename. | partial: #447 names the files, and #1045 is visibly a rename sweep |
| C5 | serial chain run in parallel | #1981 → ten PRs, #1988 to #2021 (2026-10-28/29) | `src/telemetry/`, `docs/telemetry.md` | The issue says "one issue and one PR (maintainer's decision)", yet it shipped as ten PRs, two of which conflicted with their predecessors. Each merged in 1–5 h. | yes: every milestone is in one module |
| C6 | semantic break | #1546 × #1557 → fix #1585 (2026-10-04) | a `StartAgent` field against a test literal | Both PRs were green, and `main`'s test targets stopped compiling. This led to #1610. | none: the subjects were unrelated |
| C7 | semantic break | #1558 × #1524 → fix #1594 (2026-10-05) | an `AgentRecord` field against a `src/ui.rs` literal | Five CI jobs went red on `main`. | none |
| C8 | semantic break | #1335 × #1341 → fix #1361 (2026-09-26) | a function signature against a `src/main.rs` test | `main`'s bin tests stopped compiling. | none (#530 was a docs issue) |
| C9 | predictable overlap | PRD #1419 → #1443 against eight docs PRs (2026-09-29) | the user docs pages | #1443 (84 files, "docs rewritten for agents") landed first. Each of the other eight then needed a conflicted merge and stayed open 28–63 h. | yes: a sweep across `docs/` |
| C10 | predictable overlap | PRDs #1258, #1401, #1631 → #1672, #1680, #1698 (2026-10-10..14) | desktop DTO, `fixture.ts`, voice commands | 3, 5 and 4 conflicted merges; each PR was open 43–52 h. | area level: three desktop features adding DTO fields |
| C11 | stale base | the same three PRDs, first dispatch (2026-10-10) | — | Cut from a feature branch 19 commits behind `main`. #1631's unit lacked #1617 although its task said it was present. All three were stopped and re-dispatched (skill account, `dispatch-base`). | yes: the reply said `cut from prd/1487-shared-daemon-upgrade`, and #1631's own title names #1617 |
| C12 | stale base | PRDs #740 and #745 (2026-08-30) | `desktop/` | Cut from a local `main` six commits behind, which lacked the commit that adds `desktop/`. They were re-dispatched with no original commit kept (skill account). | yes: base distance |
| C13 | premise moved | revert #1363 against #1360 and Renovate #1358 (2026-09-26/27) | Tauri crate versus npm versions | Merging the revert would have re-broken the desktop build. An advisory check caught it. | none |
| C14 | duplicate | #495 (ad hoc) and #496 (`/issue-queue`, #490) (2026-08-10/11) | `verify-pr/checks.sh` | The same fix was written twice, and #495 was closed as superseded. | partial: the duplicate issue subjects matched, but the ad-hoc unit named no issue |
| C15 | duplicated fix of a red | #1329, #1330, #1331, #1333, #1340 (2026-09-26) | `src/remote_tunnel.rs` | Five branches fixed the same flaky probe test. After #1330 merged, the others all needed merges. | none: the red appeared mid-run |
| C16 | shared runtime | three units, #428 occurrence 5 (2026-09-15) | the production daemon | One unit's `pkill -f "daemon serve"` stopped nine panes. | none: neither files nor edges show it |
| C17 | nested dispatch, sequential | `code-cleanup` units #1582–#1584, #1660/#1661, #1916/#1917 | disjoint by mode, plus an off-limits list | No conflicts, and each unit merged at most once from `main`. | yes: `pick-area.sh` excludes files touched by open PRs, running units and the off-limits list |
| C18 | intentionally serial | PRD #1666 parts (#1735 → #1817 → #1826 → #1898); #1889 and #1890 "sequenced after PRD #1666 part 3"; the part 3 off-limits boundary | messaging and dispatch core | The parts merged in order, and the dependants are still held. | yes: declared, as prose |
| C19 | serial, folded | #882 → #883; PR #884 closed so both shipped in #895 (2026-09-04/05) | PTY size policy | The decision and the implementation were serialised into one PR. | yes: declared |

**Nested dispatch has no case of a unit dispatching units.** C17 is a dispatcher dispatching after its run, and orchestrators delegate to their own workers rather than dispatching. **Cross-project dispatch has no case at all**, because #174 is unbuilt.

### Overlap and conflict across 300 merged `agent/*` PRs

| set | PRs | with a conflicted merge | median open → merge |
| --- | --- | --- | --- |
| all | 300 | 49 (16%) | 6.3 h |
| shares a file with a concurrent `agent/*` PR, excluding `tests/CATALOG.md` | 184 (61%) | 43 (23%) | 11.7 h |
| shares no file, excluding `tests/CATALOG.md` | 116 | 6 (5%) | 1.6 h |

- 42 of the 49 conflicted PRs conflicted in a file shared with a concurrent `agent/*` PR.
- Excluding `tests/CATALOG.md`, 495 concurrent pairs share a file. A shared file later appeared in a conflicted merge in at most 162 of them (33%), so about two thirds of overlapping pairs never conflicted.
- Conflict hotspots: `tests/CATALOG.md` (16 PRs), `src/state.rs` (10) and `src/daemon_protocol.rs` (10).
- **Confound:** a PR that stays open longer has more chances both to overlap and to need a merge. PRs merged within 2 h had 38 overlapping and none conflicted; PRs open over 24 h had 41 overlapping and 19 conflicted. So the open-time gap is partly a cause of the conflicts and partly a result of them.
- **Not counted:** semantic breaks (C6–C8) cause no textual conflict, so they appear in none of these numbers. Partners are limited to merged `agent/*` PRs.

### Declared dependencies

Of the 300 PRs, 260 close or name an issue. The bodies of those issues held 11 matches for an ordering phrase. Read by hand, 5 PRs (2%) had a real ordering declaration: #1811 (needs #918), #1698 (after #1617), #1680 (needs #1492), #1505 (needs PR #1440), and #1823/#1824 (a later step of #1821 after #1666). The other matches were prose about history or scope. In every real case, the dependency had landed before the dependent PR was opened, or the PR did only the steps that did not need it.

This sample is biased toward success: it holds only merged PRs. The declared dependencies that were violated (C3's thrown-away #1561, and C11's three stopped units) never produced a merged PR, so they appear only in the cases above. The two findings that matter for a policy:

- **Declarations are rare and written as prose.** A phrase search over prose produced as many false matches as true ones, so a policy cannot rely on parsing free text. It needs a declaration in a fixed form.
- **When a dependency was declared, the violations happened at dispatch, not at merge.** The dispatched unit was cut from a base that lacked its dependency (C11), or ran alongside the work it depended on (C3).

### Incidents that produced the incumbent skill rules

| rule | where | date | incident |
| --- | --- | --- | --- |
| eliminate in-flight work; cluster duplicates; dispatch coupled issues as one unit | `/issue-queue` steps 3–4, carried into `/prd-queue` | 2026-08-11 | C14, plus the duplicate pairs #470/#489 and #452/#490 |
| fast-forward the dispatch base | `/dispatch-base` (then inside the queues) | 2026-08-31 | C12 |
| simulate merge order with `git merge-tree`; spot duplicated fixes; land a sweep last; build `main` after a batch | `/land-prs` step 3 | 2026-09-27 | C4, C8, C13, C15 |
| bug-fix units merge their own PR so later units start from an updated base | `/issue-queue` step 8 | 2026-10-03 | maintainer decision |
| overlap check before every merge | `/merge-pr`, CLAUDE.md rule 8 | 2026-10-07 | C6, C7 (#1610) |
| one base step for every dispatch; read the `cut from` clause | `/dispatch-base` | 2026-10-11 | C11 (#1638) |
| disjoint file sets per cleanup mode; off-limits list | `/code-cleanup` | 2026-10-04, 2026-10-22 | C17 |
| never `pkill -f` a pattern the production daemon matches | CLAUDE.md rule 12 | after 2026-09-15 | C16 |

## Milestone 2: the smallest dependency vocabulary

It has to work across orchestration, dispatch and cross-project work, and without #628's outcome. So it names only identities that exist today and that survive a daemon restart: an **issue or PR number** (the forge's), a **branch** and a **path set** (Git's). A dispatch name is a label, not an identity: it is reused, and the deck keeps the unit record only in memory. A deck work id from #1889 can be added later as a fourth kind of endpoint without changing the edge kinds.

**Edges.** Two edge kinds carry an order. The other two say that units must not overlap.

| edge | meaning | satisfied when | checkable from |
| --- | --- | --- | --- |
| `needs <ref>` | this work requires `<ref>`'s change in its base | the PR that closes `<ref>` (or `<ref>` itself, for a PR) is merged and its merge commit is an ancestor of this unit's base | forge + `git merge-base --is-ancestor` |
| `lands-after <ref>` | this work may run now but must merge after `<ref>` (a sweep or rename, C4's #1045, C9's #1443) | `<ref>` is merged or closed | forge |
| `same-work <ref>` | a duplicate or coupled item: one unit does both | the unit's PR closes both | forge (closing references) |
| `reserves <paths>` | while this effort is live, no other unit changes these paths (the part 3 boundary, the cleanup modes' disjoint sets) | another unit's diff touches none of them | `git diff --name-only` |

**Unit states**, each with the edge or check that put it there:

- **ready**: no unsatisfied `needs`, and no `reserves` conflict predicted.
- **waiting on `<ref>`**: an unsatisfied `needs` edge.
- **stale base**: the base lacks a `needs` dependency (an invariant), or it is behind the default branch (advisory, with the distance).
- **superseded by `<unit>`**: another live unit claims the same issue.
- **invalidated**: a `needs` target was closed without merging, so the dependant's premise has to be re-read by an agent or a person.

**Cycles.** With these edge kinds a cycle can arise only through `needs` and `lands-after`. Detecting one is a graph check at declaration time, and a cycle is refused with the path printed. No real cycle was observed, because no edges are recorded mechanically today.

**Declaring an edge.** Edges are declared in a fixed form, so that nothing parses prose: one line per edge in the issue body (for example `Needs: #1617`), or a dispatch option later. Agents may *propose* edges, for example when a PRD's plan names one, but an edge counts only once it is written in that form.

## Milestone 3: invariants, policy, advisory signals and judgement

| class | decision | evidence that it pays | false-positive risk | where it can run today |
| --- | --- | --- | --- | --- |
| **invariant** | refuse (or hold) a dispatch whose base lacks a declared `needs` dependency | C11 (3 units redone) and C3, had the edge been declared | none observed: a declared edge is either satisfied or not | a dispatching skill; in the product, `dispatch` (part 3's files) |
| **invariant** | at most one live unit per issue | C14; the 2026-08-11 duplicates | low, but only for units that name their issue; C14's ad-hoc unit did not | the queues' in-flight scan; in the product, the dispatched-units registry (part 3's) plus the forge |
| **invariant** | a unit's diff touches no path another live effort `reserves` | the part 3 boundary; C17 | none, by construction | `pick-area.sh --check`, `git diff` against the reserved list |
| **invariant** | merge only when CI has covered the merged result for the files the PR changes | C6, C7, C8 | measured by the overlap check: its own rule says it can request an update CI already covered | `/merge-pr`'s overlap check; or a GitHub merge queue or strict up-to-date rule, which would catch C6–C8 outright at the cost of more CI round trips |
| **invariant** | the declared edges form no cycle | none observed | none | at declaration |
| **configurable policy** | whether an unsatisfied `needs` blocks the spawn or only warns; whether a stale base blocks; the concurrency cap (cost and stop budgets stay with #635) | C11, C12; #863's I/O saturation from parallel builds | depends on the setting | skills; later, project config |
| **advisory signal** | predicted or actual file and area overlap with concurrent units | C4, C5, C9, C10 | high: two thirds of overlapping pairs never conflicted (C2) | in-flight branches' `git diff --name-only`; issue text anchors |
| **advisory signal** | base distance from the default branch | C12 | low | `git rev-list --count` |
| **advisory signal** | landing order by `git merge-tree` simulation; land a sweep last | C4, C9, C15 | low for textual conflicts; blind to semantic ones | `/land-prs` step 3 |
| **agent or person** | decomposition; whether two issues are duplicates or coupled; semantic conflicts across files; whether a moved premise still holds | C3's "port, don't resolve" call, C13, C14, C15 | — | the orchestrator, the dispatcher, the maintainer |

**The explanation contract.** Every defer, refusal, warning or readiness change states five things:

1. the rule (one of the rows above);
2. the edge or signal that triggered it;
3. the evidence, as refs, shas and paths;
4. what would clear it;
5. how to override it, and who may.

For example: `held: needs #1617 — PR #1617 merged at 00:36 as a1b2c3d, not an ancestor of base 0935e2a7 (prd/1487-shared-daemon-upgrade). Clears when the base contains a1b2c3d. Override: --ignore-needs #1617.` An override is recorded with its reason. Whether an agent may override, or only a person, belongs with #634's follow-up #1886, which scopes what each caller of the deck may do.

**Where a dependency acts.** The evidence settles the PRD's "spawn, delivery or integration" question as follows:

- At **spawn** for `needs`, because the observed cost of a missing dependency was the whole unit (C3, C11).
- At **integration** for `lands-after`, `reserves` and overlap.
- **Never at delivery**: no case involved a task that should have been held after the unit had started.

## Milestone 4: simulation

Three policies were applied to every case, each using only what was knowable when the decision was taken. A policy has *identified* a case when it would have refused, held or warned before the cost was incurred.

- **A, the explicit DAG** (deterministic): the five invariants above, plus `needs` blocking the spawn.
- **B, signal-assisted**: A, plus the advisory signals (overlap, base distance, landing simulation), which warn and never block.
- **H, a hard overlap block**: refuse a dispatch whose predicted files overlap a live unit's.

| case | cost incurred | A | B | H |
| --- | --- | --- | --- | --- |
| C1 | none | passes (correct) | passes | passes |
| C2 | none | passes (correct) | warns on 3 pairs (noise) | **blocks 3 pairs wrongly** |
| C3 | an approved PR thrown away | identifies *only if* the edge was declared in the fixed form; the substance was in the body | warns (same area) | blocks |
| C4 | 11 conflicted merges, a semantic fix | no edge, so it passes | warns: shared files, plus land the rename last | blocks |
| C5 | 2 conflicts | identifies if the milestones were declared as `needs` chain | warns | blocks |
| C6–C8 | red `main` three times | integration invariant (merged-result CI) identifies all three; the overlap check alone identifies C7 incidentally | same as A | misses (different files) |
| C9 | 8 conflicted merges, 28–63 h open | `reserves` identifies, if the docs rewrite had declared one | warns (sweep across `docs/`), land last | blocks 8 units |
| C10 | 12 conflicted merges | passes | warns (area) | blocks |
| C11 | 3 units redone | identifies *only if* `needs #1617` was declared in the fixed form; the dependency was in #1631's title | warns: base not on the default branch, 19 behind | — |
| C12 | 2 units redone | no edge; the base-distance setting decides | warns (6 behind) | — |
| C13 | caught by an advisory check | passes | misses | misses |
| C14 | a duplicate PR | **misses**: the ad-hoc unit named no issue | warns only if the subjects are compared (agent judgement) | misses |
| C15 | 5 duplicated fixes | misses | misses | misses |
| C16 | 9 panes stopped | out of scope (#634, #1886) | — | — |
| C17 | none | passes; `reserves` keeps the modes apart | passes | passes |
| C18 | none (held by hand) | holds #1889 and #1890 with an explanation, and `reserves` the part 3 paths | same | blocks broadly |
| C19 | none | holds #883 until #882 | same | — |

**Totals against the PRD's `Go` bar** ("prevents or identifies at least three observed conflict or rework cases while wrongly blocking at most one clearly independent task set"):

- **A identifies, with no declaration needed:** C6, C7 and C8 at integration, through the merged-result invariant. That is three cases, with no wrong block among C1, C2 and C17. It identifies **no case at spawn** without a declaration, because an edge counts only once it is written in the fixed form (Milestone 2).
- **All of A's spawn-time value depends on declarations nobody wrote.** C3, C5, C9 and C11 are caught only if an edge had been declared in a form a machine reads. The dependency was in the text in C3 and C11 (and C11's unit was also visibly cut from a feature branch, which the advisory base check flags), but the sample shows declarations in about 2% of units, all as prose.
- **None of A's unconditional catches needs the deck.** C6–C8 need the merged-result invariant, which is a ruleset or merge-queue setting, not deck code; today's overlap check (#1610) approximates it, and per `/merge-pr` would have flagged C7 only through unrelated files. So until edges are declared, the deck adds little that the skills and the ruleset cannot do.
- **H fails the bar outright.** It wrongly blocks C2's independent pairs and, across the 300 PRs, would have serialised 141 overlapping PRs that never conflicted in order to prevent at most 43 that did.
- **B's warnings point at the most expensive failures in the sample** (C4, C9, C10: 31 conflicted merges and PRs held open for days), at the cost of noise on C2-like sets. Those failures were predictable at dispatch time from the issue text.
- **The identity model has one gap that matters.** An ad-hoc dispatch need not name its issue, so neither the duplicate invariant nor a `needs` edge can see it (C14). The identities the other identified cases needed were issue and PR numbers, branches and paths, which survive a restart and need no work graph.

## Comparison and recommendation

**Recommendation: Revise.** Ship dependency and conflict *visibility* and the declaration habit first, outside the product. Enforce in the deck only after declarations exist in practice and part 3 has merged.

**Why not Go.** The bar's arithmetic is met only at its floor: three cases are identified with no wrong block. But all three are integration-time catches that need a ruleset setting, not the deck. The deck-side spawn invariant catches nothing in the sample without a declaration, and nothing declares edges in a machine-readable form today; the one stale-base case it would have caught with one (C11) is what `/dispatch-base` (#1638) now guards against. Building product enforcement now would also mean editing `dispatch`, the dispatched-units registry and the protocol, all of which part 3 owns. The Go bar's "identity model that works without a work graph" holds for issue-named units but not for ad-hoc dispatch.

**Why not Stop.** Git and prompt-level planning did not expose the relevant decisions reliably. Declared dependencies were ignored at dispatch (C3, C11). The costliest failures (C4, C9, C10) were predictable from the issue text, and nothing surfaced them until the merge. Concurrent PRs that shared files conflicted at four to five times the rate of those that did not (23% against 5%), and stayed open about seven times longer at the median, although part of that gap is because open time breeds overlap.

**What Revise means, in order** (each step is a candidate follow-up PRD; none is created until the maintainer decides):

1. **Declare dependencies in a fixed form, and check them before a dispatch.** Add a `Needs:` / `Lands-after:` / `Reserves:` line convention to issue and PRD bodies, and a check (an `xtask` or a skill script) that `/issue-queue`, `/prd-queue` and an ad-hoc dispatch run first. It would refuse a unit whose base lacks a `needs` dependency, report `waiting on`, `stale base` or `invalidated` with the explanation contract above, and refuse a cycle. No product change. It makes C3, C5, C11 and C19 deterministic once the edges are written.
2. **An advisory overlap report at dispatch time.** Before a batch, list the files that in-flight `agent/*` branches change, and the predicted area of each candidate from its issue text. Flag sweeps and renames to land last, and suggest folding units into one (as `/issue-queue`'s coupling rule does) or delaying one. This extends `/land-prs`' `merge-tree` simulation from merge time to dispatch time. It warns and never blocks (C4, C9, C10).
3. **Decide the merged-result invariant at the ruleset level.** Weigh a GitHub merge queue, or `strict_required_status_checks_policy: true`, against today's overlap check. The first two catch C6–C8 outright, at the price of a CI round trip (median 9.5 min) per queued merge. This is a repository setting, not deck code. Whether a merge queue falls under the PRD's out-of-scope line ("merging branches automatically as part of scheduling") is the maintainer's call: it merges PRs that are already approved, and schedules no work.
4. **Product enforcement, after part 3 and #1889.** A capability-gated, optional `needs` field on the dispatch request, and the unit's issue as an identity on `dispatch` so that the one-unit-per-issue rule covers ad-hoc units (C14). Both would be recorded in the execution journal, with the unit's card showing `waiting on #n` and its reason in both clients (CLAUDE.md rule 22). These are additive optional fields: no `PROTOCOL_VERSION` bump, per CLAUDE.md rule 18.

**What turns this into Go.** Step 1 has run for long enough that declarations appear on the units where they matter, and the check has refused or held at least three units that would otherwise have been redone, with no wrong refusal that needed an override. At that point, moving the check into `dispatch` (step 4) is enforcement of a habit that exists, not a guess.

**Out of scope, unchanged:** maximising concurrency, a universal planner, budgets (#635), a cluster scheduler, and the deck merging branches. C16 (one unit's command stopping the shared daemon) is an authority problem for #634 and #1886, not a dependency problem.

## Caveats

- One agent reconstructed and classified the cases. The PRD asks a maintainer to judge what was visible before spawning, and that pass has not happened.
- The 300-PR sample holds merged PRs only, and so undercounts violated dependencies and duplicates, whose PRs were closed.
- The overlap window is the PR's open time, not the branch's. Re-merging with today's git can differ from what an agent saw.
- The simulation counts a case as identified when a policy would have warned or held. It does not prove the warning would have been acted on.
