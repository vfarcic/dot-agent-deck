# Telemetry analysis

This page is the procedure for the recurring analysis of this project's telemetry, CI and GitHub data. It covers how a round runs, the traps the first round hit, and the queries worth keeping. Each round has its own issue. That issue carries the round's phases, its decisions and one comment per source; this page holds what stays true from round to round (CLAUDE.md rule 13).

The rounds so far:

- **Round 1: #1980.** Window 2026-10-30 21:15 → 2026-11-03 03:10 UTC. It filed its improvements as issues labelled `telemetry`, listed in the index comment on #1980.
- **Round 2: #2112.** It starts once the round 1 improvements have taken effect (rules below).

## Why it repeats, and when the next round may start

A round measures this project's development (agent runs, gates, CI, PR flow, builds, tests), then ranks improvements and files the chosen ones. The next round checks whether each improvement moved its number. #1980's "The next round" section has the rules. In short:

- **A round's data starts when its predecessor's last improvement took effect.** That needs every improvement issue closed, either fixed or deliberately dropped.
  - A skill, CI, dev-tooling or process change takes effect when it is merged or adopted.
  - A product change takes effect when it is released and the daemon on the box has restarted on that release.
  - Each improvement issue says which kind it is.
- **Its analysis starts at least a month after that date.** Later rounds also start at least three months after the previous round's analysis.
- **It compares against the baselines written in the improvement issues,** not against a re-run of the earlier round's queries. Honeycomb's retention will not keep an old window, so each issue carries its numbers and its queries.

No reminder mechanism exists yet (the maintainer deferred the choice on 2026-11-03). Until one does, check the label filter of the previous round by hand.

## The phases

| phase | what | where |
| --- | --- | --- |
| 1. Data check | Confirm each source still reports what the baselines were measured from, and note what changed: agent versions, datasets, the collector, the CI backend | a comment on the round's issue |
| 2. Window | Fix the window and list its in-window splits: releases, backend switches, changes in the work mix | the round's issue |
| 3. Analysis | One comment per source, with numbers, queries and sample sizes; a verdict on each of the previous round's improvement issues | comments on the round's issue, and on each improvement issue |
| 4. Improve | Post a ranked list, ask the maintainer which items to file, then file each chosen one as its own issue | the round's issue, then one issue each |
| 5. Next round | File the next round's issue and link it; set up or confirm the reminder | the round's issue |

**Phase 3, how it was run in round 1.** One coordinator owned the window, the splits and the ranking. Subagents took one slice each: agent runs, instructions, gates/machine/daemon/volume, PR flow/`main`/rework/Renovate, CI/tests, and builds/coverage/mutants. Each wrote a draft comment and a candidate-improvements file. The coordinator checked every draft for CLAUDE.md rule 17 absolutes and for arithmetic before posting, and fixed several: a miscount, a time-zone slip, two over-claims. Check every draft the same way: the subagents were confident about each of those mistakes.

**What a source comment carries:**

- the window and its splits;
- the source and when it was queried;
- every number with its query (Honeycomb `query_url`, the `gh api` or GraphQL call, or the `rwx` command) and its sample size;
- for every number an improvement might be measured by, its conditions next to it: agents running at once, host load, the mix of agents and roles, and hardware.

Findings about agents, roles and hand-offs are **directional** and carry their n. A GitHub comment is capped at 65,536 characters; split a long comment rather than cutting it.

**How an improvement is recorded.** #1980's section of that name has the layout:

- the metric, preferring a rate;
- the baseline value and its query;
- the conditions;
- which direction is better, with no target;
- the numbers it must not make worse.

Round 1's issues also give the cost, the type (which sets when the change takes effect) and a caveats section. Label each improvement `telemetry`, every round alike; the index comment on the round's issue lists which ones that round filed. Give it a `size:` label for its cost and an `impact:high`, `impact:medium` or `impact:low` label for its expected benefit alone, with an "Impact" section in the body saying why, so a reader can sort by either and disagree with a rating. A difference within noise for the sample size is reported as "no measurable change".

## Sources

| source | how | notes |
| --- | --- | --- |
| Honeycomb, environment `test` | the Honeycomb MCP server (`.mcp.json`): `run_query`, `list_spans`, `get_span_details`, `get_triggers`, `list_boards` | datasets `dot-agent-deck`, `claude-code`, `codex_cli_rs`, `opencode`, `pi`, `metrics`, `renovate`. If the server is unavailable, stop: round 1 deliberately did not substitute Dash0, which started mid-window (2026-10-31 14:46 UTC) |
| GitHub | `gh api` (REST) and GraphQL | PR dump with reviews, threads, comments and timeline; `ci.yml` runs and per-attempt jobs; JUnit artifacts of GitHub runs |
| RWX | `rwx runs list --repository dot-agent-deck --definition .rwx/ci.yml --json`, `rwx results <run-id> --json`, `rwx logs <task-attempt-id>` | per-test results only from the task logs (see traps) |
| Builds, coverage, mutants | `rwx sandbox exec -- <command>` with `DAD_REMOTE_EXEC=rwx`, never locally on the shared box | a snapshot of the code at a commit, not of the window: say so in the comment |

## Traps

### Honeycomb

- **Attribute agent telemetry by `deck.run.id`** (#1980's rule), and exclude events without it from per-run figures. In round 1, 60 % of OpenCode's events had none: they came from OpenCode processes outside deck panes.
- **`agent.run` is emitted only when a run ends.** Runs still open at the window's end have no root span; count them from their child spans.
  - Group child spans by their own `deck.agent` and `deck.role`, not by joining to a root that may not exist.
  - For "runs open at once", sample the roots plus the open runs. Honeycomb's `CONCURRENCY` on closed `agent.run` roots misses the open ones.
- **`list_spans` covers at most 24 hours.** Use `run_query` for the whole window.
- **`$timestamp` cannot be used in a calculated field.** Do time arithmetic on downloaded rows.
- **On the `metrics` dataset, Honeycomb takes `LAST` per time bucket before aggregating.** A percentile over a coarse granularity is a percentile of bucket-end samples. Round 1's load average read max 29.8 at 6-hour buckets and 93.6 at 5-minute buckets. Use 5-minute buckets (the floor for a 24-hour query), query per day and combine.
- **The query API refuses a bare `COUNT` on `metrics`,** so that dataset's event volume was not measured. For the other datasets use `usage_mode` queries: raw ingested events, with every dataset at sample rate 1.
- **Shell spans closed at run end.** After v0.49.4 (2026-10-31 14:38 UTC, #1835), exclude `deck.operation.end=run_end` from shell-time figures. Before it, those spans held most of the shell time, so pre-fix shell-time figures are not comparable.
- **Spans with no program.** `deck.operation.program` is absent on about 43 % of `agent.operation` spans, where the classifier declined the command shape. No dataset round 1 checked carries command text, so those shapes cannot be sampled (#2088).
- **Agent versions can break a source mid-window.** OpenCode 2.2.5 (2026-10-31 15:09 UTC) broke OpenCode's deck data for the rest of round 1's window. Record such splits in phase 2.
- **Skill names are redacted.** Claude Code exports `skill_activated` with `skill.name=custom_skill` for project skills (#2087).
- **`cost_usd` is an estimate.** It is Claude Code's API-price estimate. On a subscription account it is a relative measure, not spend.
- **`deck.instructions.commit` moves on script-only commits** (#2086). Check what changed between two values before comparing them.
- **The `renovate` dataset is not the hosted Renovate.** In round 1 it held one local trace, so Renovate figures come from its PRs.
- **Triggers and boards:** round 1 found none in `test` (#2080). Check with `get_triggers` and `list_boards`, not from an issue's checklist.

### GitHub

- **A check rollup is a commit's current state.** A red that a re-run cleared reads green, so a count of red commits is a lower bound.
- **Commit times carry their own offset.** `git log` printed the `.github/ci-backend` switch as `2026-10-27T21:11:27+01:00`, which is 20:11 UTC; round 1 first wrote 21:11 and had to correct two comments. Convert to UTC before comparing with a window.
- **The CI backend for PRs is `.github/ci-backend`, read per PR.** It is not `dev.remote_exec`, which says where an agent's gates ran. A PR on RWX shows its Linux verdict as `RWX: linux`, and `build`/`e2e-deterministic` read skipped.
- **`workflow_dispatch` runs of `ci.yml` are agents' pre-PR sweeps.** Report them apart from push and PR runs: in round 1 they were 65 % of GitHub job-minutes, and most of their e2e failures were in test files that existed only on their branches.

### RWX

- **Round 1 got per-test results from task logs.** `rwx results` gave task outcomes, not JUnit, so round 1 parsed nextest's `PASS/FAIL/FLAKY/TRY n … [ t s] binary test` lines from `rwx logs`, and recovered no TypeScript per-test data (#2101).
- **The e2e shards also run the fast tier,** because the archive is built with `--workspace --features e2e`. Per-test counts from the shards include the library tests (#2091).

### Compile measurements in the sandbox

- **An `rwx sandbox exec` rebuilds the root crate** (21 of 21 measured, about 86 s with nothing changed), because of `build.rs`'s git watches (#2072). For a settled incremental measurement, run inside one exec with `GIT_OPTIONAL_LOCKS=0` and with `.git/packed-refs` present. Until #2072 is fixed, report which you measured.
- **One exec is killed at 30 minutes, and the whole sandbox with it** (#2077). Split anything longer: `cargo mutants --shard k/n`, filters.
- **Do not run `git pack-refs --all` in a shared sandbox.** In round 1 it pruned the branch ref, and the next exec failed to reset (exit 128) until the sandbox idled out.
- **Estimate `cargo-mutants` before running it.**
  - `cargo mutants --list --workspace` compiles nothing. On 2026-11-03 it listed 39,422 mutants, about 21 days of sandbox time.
  - Bound the run to named files and functions, and say what you left out.
  - Use `--copy-vcs true` with `GIT_OPTIONAL_LOCKS=0`: without them each mutant compiles the root crate twice.
  - Do not narrow the build phase with `-C --lib`: 7 lib tests then failed the unmutated baseline.
- **Round 1's fast-tier coverage covers the root package's `src/`.**
  - `cargo llvm-cov`'s report left out the desktop and xtask sources; why was not established.
  - Exclude the in-crate `#[cfg(test)] mod` blocks, which were 44 % of instrumented lines.
  - The e2e harness clears the environment, which drops `LLVM_PROFILE_FILE`. Children then write `default_*.profraw` into the checkout root, `rwx sandbox exec` pulls them back, and they must be deleted. Lane-1 coverage needs that fixed first.
- **The scratchpad is a tmpfs** (CLAUDE.md rule 14). Large downloads (GitHub JUnit artifacts, RWX logs) ran it short of space in round 1. Keep only the summaries, and never put a `target/` there.

## Queries worth keeping

Honeycomb `query_url`s expire with the data, so these are written as specs: they name the spans and the shape of the query. Confirm a column's exact name with `get_span_details` or `find_columns` before running one. Each one runs over the window (`start_time`/`end_time`), environment `test`. Round 1's results are the baselines in its improvement issues.

| measures | dataset | spec |
| --- | --- | --- |
| gate failure rate per gate run | `dot-agent-deck` | `affected_checks.run` spans; `COUNT` by the span's outcome and `gates.runner.used`; raw rows with `deck.run.id` for gate runs per agent run; per-command failures from `affected_checks.command`'s `gates.command.outcome` / `gates.command.exit_code` |
| gate command durations | `dot-agent-deck` | `affected_checks.command`; `P50`/`P90(duration_ms)` by `gates.command`, `gates.command.runner` |
| runs open at once, by period | `dot-agent-deck` | `agent.run` roots plus the open runs' children (first/last child timestamp by `deck.run.id`), sampled per minute locally. A run with no root is counted only up to its last exported child, so an open run that sat idle until the window's end is undercounted: for open runs this is a lower bound |
| interventions per orchestrator hour | `dot-agent-deck` | `deck.intervention` events by `trace.parent_id` and kind; match each event's `trace.parent_id` to the `trace.span_id` of its `agent.run` span, which carries the run's `deck.run.id` and duration (a separate query listing `agent.run`'s `trace.span_id` and `deck.run.id`) |
| hand-off outcomes and latency | `dot-agent-deck` | `agent.handoff` by outcome; `deck.handoff.taken` events; delegate → taken from raw rows |
| status changes per run, per agent | `dot-agent-deck` | `agent.status` `COUNT` by `deck.agent`, `deck.run.id` |
| shell spans with no program; `run_end` share | `dot-agent-deck` | `agent.operation` `COUNT` and `SUM(duration_ms)` by `deck.agent`, `deck.operation.program exists`, `deck.operation.end` |
| question waits (D2) | `claude-code` | hook events where the hook is `PermissionRequest` and the tool `AskUserQuestion`; durations from raw rows |
| first-request prompt size | `claude-code` | `api_request` with `query_source=repl_main_thread`, the lowest `event.sequence` per session (≤ 75); the sum of its input, cache-read and cache-creation token columns |
| cost and `prompt_suggestion` share | `claude-code` | `api_request` `SUM(cost_usd)` by `query_source` |
| 429s and rate-limit stops | `claude-code`, `dot-agent-deck` | `api_error` by `status_code`; `agent.run` with `deck.run.termination=failed` by `error.type` |
| event volume, run-id share, relay bypass | environment-wide `usage_mode` | `COUNT` by `service.name`, `meta.signal_type`, `deck.run.id exists`; a `COUNT` of the relay filter's dropped span names should be 0 |
| host load | `metrics` | `system.cpu.load_average.*` and `system.memory.utilization` (`state=used`), `host.name=inmotion`, 5-minute granularity, one query per day |
| per-run CPU, memory, disk | `metrics` | the collector's `deck.run.cpu.time`, `deck.run.memory.pss`, `deck.run.processes` by `deck.run.id`, and `deck.worktree.disk.usage`; 5-minute granularity |

GitHub and RWX, as commands:

```sh
# CI runs on main (push) and their per-attempt jobs
gh api "repos/vfarcic/dot-agent-deck/actions/workflows/ci.yml/runs?branch=main&event=push&created=<from>..<to>" --paginate
gh api "repos/vfarcic/dot-agent-deck/actions/runs/<run-id>/jobs?filter=all&per_page=100" --paginate
# reviewer cadence
gh api "repos/vfarcic/dot-agent-deck/actions/workflows/pr-review-batch.yml/runs?created=<from>..<to>" --paginate --jq '.workflow_runs[]|[.event,.conclusion]|@tsv'
# RWX PR CI: one page of at most 100 runs, newest first. Pass the result's NextCursor as
# --cursor and keep paging until the oldest run listed is older than the window's start,
# or the window is incomplete
rwx runs list --repository dot-agent-deck --definition .rwx/ci.yml --limit 100 --json [--cursor <NextCursor>]
rwx results <run-id> --json
rwx logs <task-attempt-id> --output <dir>
```

The PR dump is one GraphQL query: `repository.pullRequests(orderBy: UPDATED_AT DESC)` with `reviews(first:100){author state submittedAt body}`, `reviewThreads(first:100){isResolved comments(first:1){author}}`, `comments(first:100)`, `timelineItems(READY_FOR_REVIEW_EVENT, REOPENED_EVENT, CLOSED_EVENT, …)`, `mergedBy` and `mergeCommit`. Check the caps after fetching: in round 1 the largest PR had 89 reviews, so `first:100` truncated nothing. Renovate PRs' check history comes from `pullRequest(number:N){commits(last:30){nodes{commit{statusCheckRollup{state contexts(first:100){…}}}}}}`.
