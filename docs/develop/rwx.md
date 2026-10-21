# RWX: the Linux CI backend switch and sandboxed test runs

This repository can use [RWX](https://www.rwx.com) in two places, each behind its own switch. Both are off unless someone turns them on, and turning either off restores the plain GitHub Actions and local setup without touching anything else.

| switch | where | on | off |
| --- | --- | --- | --- |
| Linux CI backend for pull requests | the file `.github/ci-backend` | `rwx` | `gha`, or delete the file |
| test commands of `cargo xtask affected-checks --run` in an RWX sandbox | the environment variable `DAD_REMOTE_EXEC` | `rwx` | unset |

## The CI backend switch

`.github/ci-backend` decides which system runs the Linux jobs (`fmt`, `clippy`, `build-release`, `test`, `e2e-build`, the `e2e` shards and the `build` / `e2e-deterministic` aggregators) **for a pull request**. Pushes to `main` stay on GitHub Actions whatever the file says, so `main`'s post-merge verification and the cache saves from #1743 do not move.

Both systems trigger on every pull request and both read the file at the commit they test:

- **GitHub Actions** (`.github/workflows/ci.yml`): the `changes` job reads the file through the API and exposes `linux_backend`. When it says `rwx`, each Linux job's `if:` is false, so the job is skipped — and a job skipped by its own `if:` satisfies a required check. `build-macos`, `build-windows`, `security`, `cross-version` and every other workflow ignore the switch.
- **RWX** (`.rwx/ci.yml`): the `backend` task reads the file and writes `active`. Unless the file says `rwx`, every check task is skipped and the run passes in seconds, so RWX's check can be required whichever side is active.

Flipping the switch is a one-line pull request, and it takes effect on that pull request itself.

### What to know before turning it on

- **The ruleset has to name RWX's check.** `build` and `e2e-deterministic` are required contexts; with the switch on they pass as skipped, so the Linux verdict is RWX's check, and nothing blocks a merge on it until it is a required context too. Changing the ruleset needs an admin (`scripts/apply-branch-protection.sh`), and the agent reviewer's fallback list in `.github/scripts/pr_review_common.py` names the same contexts.
- **Switch back before the credits run out.** A pull request that flips the file back to `gha` still runs on RWX until it merges; if RWX can no longer run, that PR's required RWX check never passes, and merging it needs an admin bypass.
- **RWX tests the PR's head commit; GitHub Actions tests the PR merged into its base.** A PR that is green on its head and red once merged with a moved `main` is caught by `main`'s push run, which stays on GitHub Actions, and by the overlap check before a merge (CLAUDE.md rule 8).
- **A pull request from a fork needs someone to start its RWX run by hand** in the RWX UI.
- **Tool caches live in the vault `dot_agent_deck_rwx_eval`**, whose "Allow public repositories" setting must stay on for GitHub-triggered runs on this public repository.

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
