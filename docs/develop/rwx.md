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

## Sandboxed test runs for agents

With `DAD_REMOTE_EXEC=rwx`, `cargo xtask affected-checks --run` runs its **test commands** (`cargo test-fast` and other `cargo nextest` invocations) through `rwx sandbox exec --`, in the sandbox `.rwx/sandbox.yml` defines, and stops the sandbox afterwards. `fmt` and `clippy` stay local, since they are fast warm and gate the commit.

It runs locally instead, and prints a note saying why, when the `rwx` CLI is missing, `rwx whoami` fails, `.rwx/sandbox.yml` is absent, or the variable names anything other than `rwx`. An unset variable prints nothing and changes nothing.

`DAD_REMOTE_EXEC_KEEP=1` leaves the sandbox running after the checks, for a follow-up `rwx sandbox exec`; it stops on its own after 15 minutes without activity. Check `rwx sandbox list` when you are done: `rwx sandbox stop` can time out and leave the sandbox active, which is why `--run` checks the listing and stops it a second time.

The sandbox sees uncommitted changes in the worktree, so it can stand in for a local run before a commit. It is not a lane-2 runner: no agent credential is placed in it, and CLAUDE.md rule 5's reasons for keeping credentials out of remote runners apply to it as much as to CI.
