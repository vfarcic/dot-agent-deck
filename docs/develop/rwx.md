# RWX: the Linux CI backend switch and sandboxed test runs

This repository can use [RWX](https://www.rwx.com) in two places, each behind its own switch. Both are off unless someone turns them on, and turning either off restores the plain GitHub Actions and local setup without touching anything else.

| switch | where | on | off |
| --- | --- | --- | --- |
| Linux CI backend for pull requests | the file `.github/ci-backend` | `rwx` | `gha`, or delete the file |
| test commands of `cargo xtask affected-checks --run` in an RWX sandbox | the environment variable `DAD_REMOTE_EXEC` | `rwx` | unset |

## The CI backend switch

**Where each job runs.** `.github/ci-backend` decides which system runs a **pull request's** Linux jobs. Pushes to `main` run everything on GitHub Actions whatever the file says, so `main`'s post-merge verification and the cache saves from #1743 never depend on RWX.

| job | pull request, `ci-backend=rwx` | pull request, `ci-backend=gha` | push to `main` |
| --- | --- | --- | --- |
| every Linux job in `ci.yml` (`fmt`, `clippy`, `build-release`, `test`, `e2e-build`, the `e2e` shards, `desktop-web`, `desktop-browser`, `desktop-driver`, `windows-cross-check`, `security`, `nix`, `devbox`) and `aarch64-crossbuild-check.yml` | RWX | GitHub Actions | GitHub Actions |
| `cross-version` | GitHub Actions | GitHub Actions | GitHub Actions |
| `build-macos` | not run | not run | GitHub Actions |
| `build-windows` | `if: false` until Windows ships | | |
| `changes`, `notify-main-red`, `docs.yml`, the reviewer, labeling, releases | GitHub Actions | GitHub Actions | GitHub Actions |

The exceptions, and why:

- **`cross-version` stays on GitHub Actions.** Its harness contains the builds in unprivileged bubblewrap namespaces, and inside an RWX task bwrap cannot set up the network namespace (measured 2026-10-22: `bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted`).
- **`docs.yml` stays.** `docs-preview.yml` deploys the preview from its uploaded artifact through `workflow_run`, which only a GitHub Actions run can feed.
- **`build-macos` runs on `main`, not on pull requests.** RWX has no macOS runners, and on a pull request it was the slowest required check (5 to 18 minutes) for a 3% rate of macOS-only failures (545 runs, 2026-10-06 to 10-21). A macOS-only defect now surfaces on `main`: whoever merged watches that run and owns a red (the `merge-pr` skill), `notify-main-red` opens an issue, and `tag-release.yml` refuses to release a `main` whose run is not green.

**How the switch works.** Both systems trigger on every pull request and both read the file at the **PR's head commit**, so they always make the same choice (GitHub Actions tests the merge commit, but reading the switch there would let a PR cut before `main` switched leave both sides skipping):

- **GitHub Actions** (`.github/workflows/ci.yml`): the `changes` job reads the file through the API and exposes `linux_backend`. When it says `rwx`, each Linux job's `if:` is false, so the job is skipped, and a job skipped by its own `if:` satisfies a required check. `aarch64-crossbuild-check.yml` has its own copy of that read.
- **RWX** (`.rwx/ci.yml`): the `backend` task reads the file. Unless it says `rwx`, every check task is skipped and the run passes in seconds, so RWX's check, `RWX: linux`, can be required whichever side is active.
- **Both skip the same jobs for the same change**: `scripts/ci-changes.sh` holds the path classification (a Renovate PR touching only the devbox or flake files, whether the change feeds the binary, whether the manifests changed), and both call it. GitHub Actions takes the PR's files from the API (a renamed file by both its paths); RWX computes them against the merge-base with the PR's base with `git diff --no-renames`, which is the same list.

Flipping the switch is a one-line pull request, and it takes effect on that pull request itself.

### What to know before turning it on

- **The ruleset has to require `RWX: linux`.** With the switch on, `build` and `e2e-deterministic` pass as skipped, so nothing blocks a merge on the Linux verdict until RWX's check is required too. `scripts/apply-branch-protection.sh`'s default list carries it, and the agent reviewer reads commit statuses (which is how RWX reports) as well as check runs. Applying the ruleset needs an admin, and only after `.rwx/ci.yml` is on `main`: a pull request whose branch lacks it gets no RWX run, so `RWX: linux` never reports and it cannot merge until it merges `main`.
- **Switch back before the credits run out.** A pull request that flips the file back to `gha` still runs on RWX until it merges; if RWX can no longer run, that PR's required RWX check never passes, and merging it needs an admin bypass. The same holds for removing `RWX: linux` from the ruleset.
- **RWX tests the PR's head commit; GitHub Actions tests the PR merged into its base.** A PR that is green on its head and red once merged with a moved `main` is caught by the overlap check before a merge (CLAUDE.md rule 8) and by `main`'s push run.
- **A pull request from a fork needs someone to start its RWX run by hand** in the RWX UI.
- **Tool caches live in the vault `dot_agent_deck_rwx_eval`, which must be LOCKED with `main` as its only permitted branch** (RWX UI: Vaults, then the vault, then Access Control), and whose "Allow public repositories" setting must stay on for GitHub-triggered runs on this public repository. RWX lets a run write a tool cache only when the vault is unlocked for it, so with that setting a pull request's run reads the caches and cannot write them, and a PR's build code cannot plant files that a later PR restores (the counterpart of `ci.yml` saving Rust caches only from `main`, #1743). The caches are written by `.rwx/ci.yml`'s daily cron (`main-tool-caches`, 04:00 UTC), which RWX runs on `main` and which keeps them inside their 48-hour lifetime. Until that first cron run after a lock, PRs build without caches (slower, not broken).
- **Versions stay in step with local.** `.rwx/ci.yml` and `.rwx/sandbox.yml` pin the toolchain a third time, after `devbox.json` and the workflows. renovate.json's RWX customManagers bump them in the same grouped PRs as the other copies (every RWX bump is held for a human), and `scripts/check-pin-lockstep.sh` fails `cargo test-fast` when the Rust toolchain, nextest, pnpm or go-task in `.rwx/` disagree with `devbox.json`. RWX's own package versions (`git/clone`, `nodejs/install`, `rust-lang/install`) have no Renovate datasource and are bumped by hand.
- **Reading a red:** RWX's failures are not in `gh run view`. `gh pr checks <n>` lists `RWX: linux` with a link to the run, and `rwx results <run-id>` (or `rwx logs <task-id>`) shows what failed.

## Sandboxed runs for agents

The point is twofold: the checks finish sooner, and the machine the agents run on stays free for them. With `DAD_REMOTE_EXEC=rwx`, `cargo xtask affected-checks --run` runs **every `cargo` command except `cargo fmt`** (so clippy and the test tiers, every compile) through `rwx sandbox exec --`, in the sandbox `.rwx/sandbox.yml` defines. `cargo fmt` stays local because it compiles nothing.

Measured on 2026-10-21, the full gates for a one-line Rust change, from an M-series Mac:

| | wall | CPU used locally |
| --- | --- | --- |
| all local | 512 s | 1,168 s |
| sandbox, cold (new sandbox) | 270 s | 15 s |
| sandbox, warm (kept from the previous run) | 143 s | 15 s |

The local run also failed ten tests that pass serially and on Linux, which is the machine being contended rather than the code.

It runs locally instead, and prints a note saying why, when the `rwx` CLI is missing, `rwx whoami` fails, `.rwx/sandbox.yml` is absent, or the variable names anything other than `rwx`. An unset variable prints nothing and changes nothing.

### For agents

When `DAD_REMOTE_EXEC=rwx` is set, run the commit gates through `cargo xtask affected-checks --run` rather than command by command, and run any other heavy `cargo` command (a filtered `cargo test-e2e`, a build) as `rwx sandbox exec -- <command>`. **Never run lane 2 (`cargo test-e2e-live`) or anything that reaches a real agent in the sandbox**: no agent credential is placed there, and CLAUDE.md rule 5's reasons for keeping credentials off remote runners apply to it as much as to CI.

### Stopping

A sandbox is billed per second while active, idle included, so it is stopped as soon as the work is done: `--run` stops it when it finishes, and after an ad-hoc `rwx sandbox exec` run `rwx sandbox stop` yourself. The next run starts a fresh sandbox (the table's cold row), which is the right trade, since fixing a failure can take any amount of time. `DAD_REMOTE_EXEC_KEEP=1` leaves it running for a tight loop of reruns (the warm row). Either way the sandbox stops itself after 5 minutes without activity; a running command counts as activity, so that cannot cut a long run off. Check `rwx sandbox list` when you are done: `rwx sandbox stop` can time out and leave the sandbox active, which is why `--run` checks the listing and stops it a second time.

The sandbox sees uncommitted changes in the worktree, so it stands in for a local run before a commit.
