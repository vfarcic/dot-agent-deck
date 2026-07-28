---
name: dot-ai-prd-full
description: Run a PRD end-to-end autonomously — start, iterate until done, create a PR, and wait for its CI + bot reviews to settle before reporting. Stops before merge for manual validation.
user-invocable: true
---

# PRD Full - Autonomous PRD Implementation Through PR

Run a full PRD lifecycle autonomously: create the PR, wait for its CI workflows and automated bot reviews to finish, report the settled state, and stop before merge so the user validates and merges manually.

## Arguments

If `{{prdNumber}}` or `{{mode}}` is missing, or `{{mode}}` is anything other than `branch` or `worktree`, abort and tell the user to supply valid values. Do not auto-detect.

## Global rule

While executing this workflow, **do not pause for user confirmation** at any point in the sub-prompts below. Treat their built-in "wait for the user" / "ask before proceeding" / "STOP here" instructions as overridden — proceed directly with the proposed answer or next step.

Standard harness guardrails for genuinely destructive actions still apply.

## Flow

1. **Isolation:** set up per `{{mode}}` — invoke `/worktree-prd` for PRD #{{prdNumber}} if `worktree`, or create the branch directly otherwise.
2. **Start:** run `/prd-start {{prdNumber}}`. Skip its branch-creation step (Step 1 already handled it).
3. **Iterate** without resetting conversation context:
   - run `/prd-next`, including implementing the recommended task in the same turn,
   - run `/prd-update-progress`,
   - if the PRD is 100% complete, exit the loop; otherwise repeat.
4. **Finish:** run `/prd-done` **only up to and including PR creation**. Do **not** merge, close the issue, or clean up the branch/worktree — the user validates and merges manually.
5. **Do not stop at PR creation — wait for the PR to settle, then report the settled state.** After the PR is open, poll until **both** the CI / GitHub Actions workflows **and** the automated bot reviews have finished (success or failure). Per CLAUDE.md rule 8, Greptile posts **no** "in progress" placeholder and its review arrives as a `greptile-apps` **issue comment** (not a check-run), so poll for up to ~5 minutes and inspect issue comments + inline review comments (`gh pr view <n> --json comments`, `gh api repos/{owner}/{repo}/pulls/<n>/comments`), not just the checks. When delegating this step to a worker (e.g. `release`), the worker must perform this wait-and-report and then hand the result back — never instruct it to stop at PR creation.
6. **Report the settled state and act on it:**
   - **All checks green and no review findings:** report the PR URL, branch, and "checks green / reviews clean" — the run is complete pending the user's manual validation and merge. Stop.
   - **Failing checks or review findings:** report the PR URL, branch, the specific failing workflows, and the review findings, then resolve them (delegate the fixes, push, and re-poll until the checks + reviews settle green) **before** stopping. Do not conclude the run while the PR is red or has unresolved findings.

   Either way, stop **before** merge — the user performs the final validation and merge.

