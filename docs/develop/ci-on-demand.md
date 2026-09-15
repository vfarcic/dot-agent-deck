# Running the full CI matrix on demand, instead of on this box

Issue #896. `.github/workflows/ci.yml` carries a bare `workflow_dispatch:` trigger, so any pushed branch can have the full matrix run on GitHub's runners with no local CPU at all. Nothing said so anywhere an agent would read it. The trigger itself is no secret — `release.yml`, `tag-release.yml` and `docs-publish.yml` are each described as `workflow_dispatch`-able in [`governance.md`](governance.md) and in their own skills — but `ci.yml`'s was written about nowhere, so every dispatched unit compiled everything locally, on a box already running the other units. This page is the mechanism, the commands, and the harder half: when to reach for it, and when reaching for it makes things worse.

**It adds coverage; it does not subtract work.** Nothing here removes a local gate — the section below says why it cannot — and what it moves onto a runner is the optional sweeps, not the mandatory ones. [`build-gate.md`](build-gate.md) bounds how much of what stays here runs at once, and issue [#864](https://github.com/vfarcic/dot-agent-deck/issues/864) — `sccache`, a shared `CARGO_TARGET_DIR`, and the untouched `[profile.dev] debug` setting — is the change that attacks the cost itself. Reach for this when the box is the constraint today; reach for #864 when the question is how many agents this box can hold.

## The commands

```bash
git push -u origin <branch>
gh workflow run ci.yml --ref <branch>                 # prints the new run's URL
gh run watch <run-id>                                 # follow it
gh run view <run-id> --log-failed                     # read only what broke
```

**Take the run id from the URL that `gh workflow run` prints, not from a listing.** The command exits as soon as the dispatch is accepted, and on `gh` 2.98.0 it prints the new run's URL, whose last path segment is the run id. The obvious-looking alternative, `gh run list --workflow ci.yml --branch <branch> --limit 1`, is wrong twice over. For the first seconds the dispatch is not listed at all, so the newest run is the *previous* one — measured on 2026-09-15, the list was empty immediately after the dispatch and reported the run as `queued` about eleven seconds later. And a branch with an open pull request also carries `pull_request` runs, which are usually newer: measured on this branch, with the dispatch already complete, that exact command returned the PR's run `35025714262` on head `435e3358` rather than the dispatch `35025264144` on head `385e342d`. An agent that trusted it would have read a different run, on a different commit, as the answer to its dispatch. (Neither is a rejected dispatch — an empty list one second in means nothing.)

If you must list — a run started in another session, say — pin the event and check the head:

```bash
gh run list --workflow ci.yml --branch <branch> --event workflow_dispatch --limit 1 \
  --json databaseId,headSha,status,conclusion
```

That block is not a sketch: it was run end to end against this repository on 2026-09-15 while this page was being written, from a dispatch worktree on `agent/dispatch-issue-896`, and produced run [`35025264144`](https://github.com/vfarcic/dot-agent-deck/actions/runs/35025264144).

## What a dispatch actually runs

Eleven of `ci.yml`'s twelve jobs: `changes`, `desktop-web`, `desktop-browser`, `build`, `e2e-deterministic`, `windows-cross-check`, `build-windows`, `build-macos`, `security`, `nix` and `devbox`. The twelfth, `notify-main-red`, is gated on `github.event_name == 'push' && github.ref == 'refs/heads/main'` and is silent here by design.

That list is read off a real dispatch rather than off the file: run [`35025264144`](https://github.com/vfarcic/dot-agent-deck/actions/runs/35025264144), a `workflow_dispatch` on this repository on 2026-09-15, ran exactly those eleven and reported `notify-main-red` as `skipped`, finishing green in 8.8 minutes. Note the shape of that last part before reading a job list yourself — a completed run reports **twelve** jobs, the twelfth `skipped`, while a run still in progress lists only the eleven, because a skipped job appears only once its condition has resolved. So filter rather than counting:

```bash
gh run view <run-id> --json jobs --jq '[.jobs[] | select(.conclusion != "skipped") | .name]'
```

A job added to `ci.yml` later joins that output without anyone editing this paragraph, which is the point of giving the command rather than only the list.

The full matrix, not a subset. The `changes` job skips the Rust jobs for a Renovate PR that touched only `devbox.*` or only the flake, and it reads `github.event.pull_request.user.login` to decide — which a `workflow_dispatch` payload does not carry, so the author check fails, the job exits early with `devbox_only=false` / `flake_only=false`, and every downstream `if:` passes. That is the same fail-safe the `push`-to-`main` runs rely on, and its own comment in `ci.yml` says so.

Five of the eleven are the contexts the `main-protected` ruleset requires: `build`, `build-macos`, `build-windows`, `security`, `e2e-deterministic`. Read from the ruleset on 2026-09-15 and matching `scripts/apply-branch-protection.sh`'s `REQUIRED_CHECKS` default; re-read them with `gh api repos/{owner}/{repo}/rulesets/<id> --jq '.rules[] | select(.type=="required_status_checks") | .parameters.required_status_checks[].context'` rather than trusting this sentence.

## What it will not cancel

Since issue #1088 the concurrency key is `ci-${{ github.event_name == 'pull_request' && github.ref || github.run_id }}` with `cancel-in-progress: ${{ github.event_name == 'pull_request' }}`, so a `workflow_dispatch` run gets a group keyed on its own `github.run_id` and never cancels. Two dispatches on one branch both complete, and a dispatch and a PR run on the same branch are in different groups and ignore each other.

Observed on this branch on 2026-09-15, and the run list makes both halves visible at once. Dispatch `35025264144` ran 21:22:45 → 21:31:33; the pull-request run `35025450527` started at 21:24:40 and was **cancelled** at 21:28:34 by the next push's run `35025714262`, which then ran to success at 21:36:19. So the two `pull_request` runs did cancel each other, exactly as the key intends — while the dispatch, overlapping both of them for nine minutes, was neither cancelled nor cancelling and finished green. (Before #1088 the key was `ci-${{ github.ref }}` with an unconditional `cancel-in-progress: true`, where a second dispatch on one branch *would* have killed the first. Issue #896 was written against that version and its note about self-cancellation no longer applies.)

The cost of that is the other direction: a dispatch you no longer care about keeps a runner busy until it finishes or you cancel it with `gh run cancel <run-id>`.

## It relieves no local gate, because it runs after all of them

A dispatch is **additional, post-commit** coverage. That is not a caveat bolted on — it is forced by the order of events. CLAUDE.md rule 2's `cargo fmt --check` and `cargo clippy --workspace --all-targets --features e2e,e2e-live -- -D warnings` are a **pre-commit** gate, and there is nothing to dispatch until a commit exists, so by the time this page's first command is available those two have already had to pass. Rule 5's `cargo test-fast` stays a per-task obligation and this page does not move it.

So "run CI instead" is never a thing anyone does here, and a task text that implies otherwise is telling a unit to skip a mandated gate. What a dispatch actually buys is the part **no local gate covers at all** — `build-macos`, `build-windows`, `e2e-deterministic`, `nix`, `devbox`, `security`, `desktop-web`, `desktop-browser`, `windows-cross-check` — at a moment of your choosing rather than only when you open the pull request, and without adding a second broad local sweep to a box that is already busy running the mandatory ones.

## What it does not cover, and cannot

- **Lane 2 — anything reaching a real agent.** No test that reaches a real agent runs on a runner, and that is a decision rather than a gap (CLAUDE.md rule 5, [`e2e-lanes.md`](e2e-lanes.md)). `cargo test-e2e-live <filter>` is yours to run whatever CI reports.
- **The demo reel's `.cast` recordings.** CI records none; they are produced locally by running the relevant tests under `DOT_AGENT_DECK_RECORD=1` (PRD #180).
- **Anything you have not committed and pushed.** A runner builds the pushed commit, so a green run attests to that commit and not to your working tree. A local gate cannot make that particular mistake, which is worth remembering when the thing you are about to dispatch is a fix you have only just written.

## When to reach for it

- **The box is loaded**, and the sweep you are about to run is one of the optional ones. The mandatory gates are already local and already paid for; what you can decline is a *second* broad local pass — a local lane-1 `cargo test-e2e`, a `scripts/windows-cross-check.sh`, a release build — on top of them. Several units compiling at once is the condition issue [#863](https://github.com/vfarcic/dot-agent-deck/issues/863) measured to `io full avg300=65.95` — two-thirds of a five-minute window with every runnable task blocked on disk, the CPU idle at `cpu some avg300=0.08`, and `ld` then invoking the OOM killer. A unit that has just been dispatched into a fresh worktree is the worst case: it has no `target/` at all, so its first `cargo clippy --workspace --all-targets --features e2e,e2e-live` is a cold build of the whole graph.
- **A platform you do not have.** On a Linux host there is no local counterpart to `build-macos` at all: `scripts/` holds no Darwin cross-check, and the three test aliases compile for the host and nothing else. `build-windows` has a partial one — `scripts/windows-cross-check.sh` type-checks the workspace for `x86_64-pc-windows-msvc` from a Linux host ([`windows-cross-check.md`](windows-cross-check.md)) — but it runs `cargo check` rather than linking, runs no test and no clippy for that target, and cannot be held to `--features e2e` today. Both jobs are required contexts, so a break in either blocks the merge whether or not anything local could have seen it.
- **A final pre-PR sweep.** Opening the PR runs the same matrix anyway, so this buys the answer earlier rather than an extra check — worth it when you would otherwise be waiting on the PR to find out, or when the alternative is running the optional broad sweeps here.

## When not to

**Never as the per-edit gate.** CLAUDE.md rule 2's `cargo fmt --check` plus `cargo clippy --workspace --all-targets --features e2e,e2e-live -- -D warnings` and rule 5's `cargo test-fast` stay local. Warm, that clippy is **9.3–10.4s** (rule 2, measured 2026-08-31 on 16 cores) against a CI round trip whose median is **9.5 minutes** — 40 to 60 times slower, depending on how loaded the box is when you measure the local side (14.3s warm here on 2026-09-15 under load, against rule 2's 9.3–10.4s on a quiet box). Nothing about a remote gate makes an inner loop that slow acceptable, and a unit that dispatches CI after each edit will spend its whole run waiting.

**Never as a substitute for rule 2 or rule 5.** It cannot be one, per the section above, and a task text that reads as though it could is worse than saying nothing — it hands a unit a licence to commit without the gate that is supposed to precede the commit.

**The expensive half is compiling, not testing**, so "just run the tests in CI" fixes little. Warm, `cargo test-fast`'s entire wall clock is the 25–30s CLAUDE.md rule 5 measures, because the build in front of the tests is then a no-op; cold, it is minutes, and the tests are still seconds of them. And lane 1 of the e2e tier is already off this box — issue #502 moved it to CI on every PR precisely because N units each running it was self-defeating.

## The numbers

Everything in this table was either measured for issue #896 on 2026-09-15 or is cited to where CLAUDE.md records it. Nothing is carried over from issue #896's own text, which measured a differently loaded box on a different day.

| | value | where it came from |
| --- | --- | --- |
| a `ci.yml` run, median | **9.5 min** | the 37 completed runs among the last 60 listed, 2026-09-15: `gh run list --workflow ci.yml --limit 60 --json conclusion,createdAt,updatedAt` |
| the same: min / p90 / max | 7.3 / 16.4 / 26.7 min | same sample |
| this page's own dispatch | **8.8 min**, green | run `35025264144`, 2026-09-15 |
| rule 2's clippy, warm, quiet box | 9.3–10.4 s | CLAUDE.md rule 2, measured 2026-08-31 on 16 cores |
| rule 2's clippy, warm, this box under load | **14.3 s** | measured for #896, 2026-09-15, same worktree once warm |
| rule 2's clippy, cold, in a fresh dispatch worktree on a loaded box | **2m42s** | measured for #896, 2026-09-15, no `target/` at all, `/proc/pressure/io` at `full avg60=49.23` |
| `cargo test-fast`, same worktree and box, wall | **4m48s** | measured for #896, 2026-09-15, immediately after that clippy run |
| the same run: executing the 3118 tests | **37.1 s** | nextest's own summary line from that run |
| `cargo test-fast`, whole wall clock, warm cache, quiet box | 25–30 s | CLAUDE.md rule 5's own measurements, 2026-08-31 — warm, the build in front of the tests is a no-op |

Three things that table is saying. **The gap between the two `test-fast` rows is the whole argument**: 37.1 seconds of that 4m48s was running tests and the rest was the build in front of them, so a remote gate aimed at "the tests" would relocate about an eighth of the cost (37.1s of 288.3s). **The clippy figure is the cheap gate's cold cost, not the expensive one's** — `clippy` type-checks and never links, while `cargo test-fast` compiles for real and links the test binaries, which is the work [`build-gate.md`](build-gate.md) bounds and the work an OOM kill lands on. And **neither local figure is a true cold worst case**: the `test-fast` run followed the clippy run in the same worktree, so the registry was already unpacked and some artifacts were already there. A unit's genuine first build is worse than 4m48s, not better — which only widens the gap the table is about.

The p90-to-max spread is worth reading before planning around the median: a queued runner or a slow `build-macos` turns a 9-minute answer into a 26-minute one, and nothing in the dispatch tells you which you are getting.

## Where an agent is told this

CLAUDE.md rule 5 carries the short version, next to the three tiers, because that is where a reader asks what they owe and on which machine. The three dispatch skills — [`issue-queue`](../../.claude/skills/issue-queue/SKILL.md), [`prd-queue`](../../.claude/skills/prd-queue/SKILL.md) and [`pr-review-queue`](../../.claude/skills/pr-review-queue/SKILL.md) — put a line in the task text they compose, so a dispatched unit learns it the same way it learns the gates. What those four carry is the relieves-nothing framing, the *when*, and the one or two numbers that make the case; the mechanism, the samples behind the numbers and the narrowings live here, and that is the split to preserve when any of them is edited.
