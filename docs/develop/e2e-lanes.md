# The e2e lanes

Issue #502 split the L2 e2e suite into two CI lanes by whether a test needs a working agent credential. **CLAUDE.md rule 5 is the policy** — which lane runs where, and why there is no longer a pre-PR obligation to run the tier in full. This page is the operational half: how to actually drive them, and what a green run does and does not prove.

## What each lane is

| | lane 1 | lane 2 |
| --- | --- | --- |
| command | `cargo test-e2e` | `cargo test-e2e-live` |
| cargo features | `e2e` | `e2e,e2e-live` |
| files | the 47 `tests/e2e_*.rs` that need no agent credential | **all 71** — lane 1 *plus* the 24 that do |
| where | the `e2e-deterministic` job in `.github/workflows/ci.yml` | the `live` job in `.github/workflows/e2e-live.yml` |
| when | every PR | per-merge on `main`, `workflow_dispatch`, and a `run-live-e2e`-labelled PR |
| secrets | none in the job's environment at all | `ANTHROPIC_API_KEY`, environment-scoped |
| timeout | 60 min | 90 min |

`cargo test-e2e-live` is a **superset** run, not a live-only run. The 24 credentialed files open with `#![cfg(all(feature = "e2e", feature = "e2e-live"))]`, so `e2e-live` without `e2e` compiles every e2e file to an empty crate; there is deliberately no live-only alias. That is also why rule 2's clippy gate names both features — with only `--features e2e` those 24 files are type-checked by nothing.

Neither lane is a required status check. The required set is still `build`, `build-macos`, `build-windows` and `security`; rule 8 records why the two new jobs are deliberately advisory.

## Running the live lane on a PR

```sh
gh pr edit <n> --add-label run-live-e2e
```

The label exists already (`run-live-e2e`, "Run the credentialed live e2e lane on this PR"). `labeled` is in the workflow's `pull_request` trigger list, so applying it to an already-open PR starts a run immediately; `synchronize` keeps a labelled PR re-running as it is pushed to.

To run it on `main` without waiting for a merge:

```sh
gh workflow run e2e-live.yml
```

Then read the result — and read the SKIP lines, not just the colour. See below.

```sh
gh run list --workflow=e2e-live.yml --limit 5
gh run view <run-id> --log | grep -E '^\s*SKIP: '
```

The run's **summary page** carries the same evidence without a log download: a *Lane 2 runtime skips* section with the count and the distinct reasons. That is where the ran-versus-skipped signal lives, and it exists only because the run step passes `--success-output=final` — a runtime skip is a *successful* test, so under nextest's defaults its `SKIP:` line is suppressed from the log entirely and nothing downstream can count it. The leading `\s*` in the grep above is load-bearing for the same reason `checks.sh` needs it: nextest indents captured output by four spaces, so a `^SKIP: ` anchored at column 0 matches nothing on a run that really did skip.

The job also uploads a JUnit report as `nextest-e2e-live-attempt-<n>` (issue #564), which is what survives a job-level re-run overwriting the conclusion. It carries test names, outcomes, per-test wall clock and the suite counts. Two things it does **not** carry. It has never carried the ran-versus-skipped distinction, for the reason above — read the run summary for that, not the artifact. And since issue #785 it carries no test output at all: `scripts/junit-strip-output.py` rebuilds the report from a per-element attribute whitelist before upload, so `<system-out>`, `<system-err>`, every `<failure>`/`<error>` message and body, and every text node are gone by construction. That is because nextest's JUnit defaults store stdout **and** stderr for failed and retried tests, the file is written by the cargo process holding `ANTHROPIC_API_KEY`, and GitHub's secret masking covers log rendering but not files copied out by `actions/upload-artifact` — which on this public repository any logged-in reader can download. Run `python3 scripts/junit-strip-output.py --self-test` to see that proved.

## The `live-e2e` environment

The job declares `environment: live-e2e`. That is the control that makes running a credentialed job on freshly-merged code acceptable at all (issue #785 decision 2): with a **required reviewer** on the environment, a merge *queues* the run and waits for a human click, so merged code never auto-executes with the key. Environments and their protection rules are free on public repositories.

Two properties of it are deliberate and must not be tidied up:

- **No deployment-branch restriction.** Limiting the environment to `main` would stop a branch PR exercising the credentialed path, which is exactly the validate-before-merge property the label trigger exists for.
- **The required reviewer is set on the environment by hand**, by the repository owner. A workflow file cannot declare it, and it is the one control that addresses the threat directly — an environment with no reviewer is a repository secret wearing an environment's name.

As of this writing the environment exists with `deployment_branch_policy: null` and `protection_rules: []`, and **no secret has been set**. Both the secret and the reviewer are owner-only manual steps that remain open on #785. Check the current state with:

```sh
gh api repos/vfarcic/dot-agent-deck/environments --jq '.environments[] | {name, protection_rules, deployment_branch_policy}'
```

## Three things that decide whether a green lane-2 run means anything

### 1. Every credential preflight SKIPS rather than fails

Real-agent tests open with `skip_unless!(check_<agent>_available())`. That macro prints `SKIP: <reason>` and **returns normally**, so nextest counts the test as **passed**. An absent or unusable credential therefore removes coverage silently instead of reddening the job — the failure mode to keep in view when reading a green run here.

One case is now caught rather than absorbed: an **unset or empty `ANTHROPIC_API_KEY`** fails the workflow's first step, before the checkout. Without that guard the whole job could go green with not one API-key-backed test having run — the preflights would skip, and `cargo test-e2e-live` being a superset means lane 1's 47 deterministic files still execute and still pass. It is the same fail-fast shape CLAUDE.md rule 8 describes for `RELEASE_TOKEN`, and it carries the same honest limit: `-z` catches unset and empty, not a key that is expired, revoked or over its spend cap. Note the consequence for forks — GitHub withholds secrets from a fork `pull_request`, so labelling a fork PR now produces a *red* advisory job saying lane 2 cannot run there, which is the intended reading rather than a green tick standing in for coverage that never happened.

`DOT_AGENT_DECK_REQUIRE_REAL_E2E` (see `REQUIRE_REAL_E2E_ENV` in `tests/common/mod.rs`) turns every such skip into a panic. It is **not** set in either workflow. Set it locally when you need "cannot run" to read as UNVERIFIED rather than green:

```sh
DOT_AGENT_DECK_REQUIRE_REAL_E2E=1 cargo test-e2e-live <filter>
```

### 2. With an API key alone, most of lane 2 can only skip

`check_claude_available` (`tests/common/mod.rs`) requires `claude` on PATH **and** a usable `~/.claude/.credentials.json` carrying a `claudeAiOauth` entry, with a macOS Keychain fallback. **It never consults `ANTHROPIC_API_KEY`.** Under #785 decision 1 — an API key, deliberately not the OAuth credential set — every claude-gated test therefore skips.

Measured across the 24 live files, by preflight and by test function:

| preflight | test functions | under an API key alone |
| --- | --- | --- |
| `check_claude_available` | 22 | skip |
| `check_codex_available` | 5 | skip (wants `~/.codex/auth.json` plus a live model probe) |
| `check_pi_available` | 5 | **run** |
| `check_opencode_available` | 2 | skip (wants an opencode `auth.json`) |
| `check_devin_available` | 1 | skip (wants a Devin `credentials.toml` plus `devin auth status`) |

So **5 of 35 gated tests exercise a real agent** as the lane stands. `check_pi_available` (defined per-file in `tests/e2e_pi_*.rs`) is the outlier because it wants `pi` on PATH and a non-empty `ANTHROPIC_API_KEY` and nothing else.

The workflow installs `claude` even though it cannot pass its preflight, and the reason is diagnostic rather than hopeful: the SKIP line then reads "credentials not found at `~/.claude/.credentials.json`" rather than "CLI not installed", which is what distinguishes a preflight that needs widening from a runner that needs provisioning. `codex`, `opencode` and `devin` are deliberately not installed — their preflights want their own providers' credentials, which an Anthropic key cannot satisfy, so installing them would cost minutes and change nothing but the wording of eight SKIP lines.

**Read a lane-2 run's SKIP lines, not just its colour.**

### 3. The two npm pins are not tracked by Renovate

`e2e-live.yml` pins `@anthropic-ai/claude-code` and `@earendil-works/pi-coding-agent` to exact versions. `renovate.json`'s customManagers cover the `toolchain:` pins and the `cargo-nextest` pin under `.github/workflows/`, not npm packages, so **nothing bumps these two**. They need a deliberate manual bump, and agent CLIs move fast.

## Maintenance notes

- **Both timeouts are derived, not measured.** Nothing had ever run the e2e tier on a GitHub runner before these jobs existed, so lane 1's 60 minutes comes from the tier's own declared kill windows (`lifecycle/version/001` alone carries a 120s x 10 window and pays a cold nested dependency build) and lane 2's 90 from that plus the real-agent files, several of which sit at 300-540s. Re-tune both downward once honest runs exist; do not raise either to paper over a hang.
- **`check-pin-lockstep.sh` now covers this workflow.** `e2e-live.yml` carries both a `toolchain:` and a `cargo-nextest` pin, so the script's site counts went from 7 and 4 to **9 and 5**. `pin_lockstep.rs` runs it inside `cargo test-fast`, so a drifted pin here goes red on the per-task gate.
- **The `live` job's `if:` is an ALLOWLIST of event names, so adding a trigger takes two edits.** Putting a `workflow_run`, a `schedule` or a `repository_dispatch` in the `on:` block is not enough — the job also has to name it in the condition, or it silently never runs. That is deliberate for the one job in the repository that holds an agent credential: a new trigger that does not run is a missing line and an obvious one to add, while a new trigger that runs *with the key* would be a security decision nobody made. Fail closed, not open.
- **Lane 2 has a repository-wide concurrency group with `cancel-in-progress: false`.** These runs spend real tokens against one account, so two overlapping runs race for the same rate limit and can fail each other for reasons unrelated to the code. A run that is halfway through has already spent the money; let it finish and queue the next.
- **The build and the run are separate steps on purpose.** `cargo nextest list` builds the test binaries with **no secret in the environment**, because `build.rs` is first on #785's list of surfaces a credentialed job exposes — arbitrary code at compile time, ahead of any test. The run step then finds everything fresh. Be precise about what that buys: it is a freshness guarantee, not a hermetic seal. `cargo nextest archive` plus `--archive-file` would be one, at the cost of a workspace-remap round trip this lane cannot validate until the secret exists.
