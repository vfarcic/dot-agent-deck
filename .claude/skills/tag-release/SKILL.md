---
name: tag-release
description: Cut a release from the accumulated changelog fragments by dispatching the Tag Release workflow, then prune the worktrees and branches whose work it contains. Run when ready to cut a release.
user-invocable: true
---

# Cut a release

Propose a version from the changelog fragments, get the user to confirm it, then dispatch `.github/workflows/tag-release.yml`, which bumps the `flake.nix` pin, commits it to `main` and creates the tag. `release.yml` fires on that tag push and does everything after it — changelog, builds, GitHub Release, Homebrew, Scoop, docs.

**You do not push to `main` and you do not create the tag by hand.** Both used to be steps in this skill, and both were a human pushing straight at a ruleset-protected branch: for an admin that succeeds, printing `remote: Bypassed rule violations for refs/heads/main`, which CLAUDE.md rule 8 names as worse than a rejection. Issue #1089 moved them into a workflow that pushes under the `RELEASE_TOKEN` admin PAT — the same sanctioned path `release.yml` and `docs-publish.yml` already use for their own direct pushes.

This skill is **project-local and owned here** (CLAUDE.md rule 13). It was forked from the `dot-ai-tag-release` mirror because its most load-bearing step was already dot-agent-deck-specific and a sync would have deleted it. Edit it freely; do not expect upstream fixes.

## When to use

Several PRs have merged with changelog fragments in `changelog.d/` and you are ready to cut a release. It is a separate activity from any PR workflow — never run it as part of one.

## Step 1 — Analyze

Run this from a checkout of `main` that is level with `origin/main`, because `analyze.sh` reads the working tree's `changelog.d/`. Record the SHA: it is what binds the release to the tree you are about to review.

```bash
git fetch --quiet origin main
git rev-parse origin/main          # record this — it becomes expected_head in Step 3
bash .claude/skills/tag-release/analyze.sh
bash .claude/skills/tag-release/cert-expiry.sh
```

Stop and show the user the `MESSAGE` if `analyze.sh` exits non-zero or prints `ERROR=true`. If it prints `NO_FRAGMENTS=true`, there is nothing to release.

`cert-expiry.sh` is advisory and never blocks a release: it reads what the last release run's `desktop-sign` job logged about the Developer ID Application certificate (issue #1326). That job warns from 30 days before expiry, but only in a log and an annotation on a run that is usually green, so this is the one place the warning reliably reaches a person.

## Step 2 — Confirm the version with the user

**This is the human checkpoint, and it is the only one.** Everything after it is automated and the tag is irreversible. Present:

1. `CURRENT_VERSION` and `PROPOSED_VERSION`, with the `BUMP_TYPE` that produced it.
2. The `FRAGMENTS` list with their types, so the user can see what the bump was derived from.
3. `SKIP_CI`, if it is `true` — `main`'s tip carries a skip marker, and a tag pointing at such a commit would stop `release.yml` running at all. Usually this resolves itself, because the pin commit becomes the new tip and carries no marker; the workflow inserts an empty preparation commit only in the case where the pin was already correct and so no commit was made. (`analyze.sh`'s check is advisory and matches only the bracket markers; the workflow's own check is the binding one and also covers GitHub's `skip-checks: true` trailer.)

4. The certificate, from `cert-expiry.sh`: `CERT_NOT_AFTER` whenever it is printed, and every `CERT_MESSAGE` line when `CERT_CHECK=warning`. A warning means the certificate needs renewing, which is a manual task for the maintainer — `docs/develop/desktop-signing.md`, **Certificate expiry** — and from 24 hours before expiry `desktop-sign` fails and the release ships with no macOS `.dmg`. Report `CERT_CHECK=unknown` as "not checked", not as clear. `CERT_CHECK=clear` with no `CERT_NOT_AFTER` means the run it read built the `.dmg` unsigned, so there was no certificate to check. `CERT_PASSED_OVER` lists newer runs whose `desktop-sign` failed before reaching the certificate check; the answer comes from an older run, so mention them.

Ask the user to confirm the version or give you a different one. Ask for a one- or two-sentence summary of the release for the tag message, or offer one drawn from the fragments.

## Step 3 — Dispatch the release

```bash
gh workflow run tag-release.yml --ref main \
  -f version=<X.Y.Z> \
  -f expected_head=<the SHA from Step 1> \
  -f tag_message="<the one- or two-sentence summary>"
```

`version` carries **no leading `v`** (the tag is `v0.40.2`, the flake pin is `0.40.2`; the workflow trims a `v` if you type one anyway). It must equal what the fragments compute, or the workflow refuses without committing or tagging anything — that re-derivation catches a typo, a stale reading of Step 1, and a fragment that landed in between **and changed the bump**. To release a version that deliberately differs from the computed one, add `-f allow_mismatch=true`, and say in your report why.

**Always pass `expected_head`.** It is optional in the workflow only so a dispatch from the GitHub UI still works, and omitting it downgrades the checkpoint: a fragment of the *same* bump class merging between Step 1 and the job leaves the computed version identical, so the version check passes and the release carries content nobody reviewed. With the SHA, the workflow refuses instead and you re-run Step 1 against the new tip. The run's job summary also lists the fragments it actually released, read from the tree it tagged.

## Step 4 — Watch the run

Pick the run **this dispatch** created, not merely the newest one — the workflow serialises on a `tag-release` concurrency group, so a queued second dispatch is exactly the case where `.[0]` watches the wrong thing:

```bash
before=$(gh run list --workflow=tag-release.yml --limit 1 --json databaseId --jq '.[0].databaseId // 0')
# ... dispatch as in Step 3 ...
until run=$(gh run list --workflow=tag-release.yml --limit 20 --json databaseId \
              --jq "[.[].databaseId] | map(select(. > ${before})) | min // empty") && [ -n "$run" ]; do
  sleep 3
done
gh run watch "$run"
```

Run IDs increase, so "the lowest id greater than the one that existed before I dispatched" is this dispatch's run even when others queue behind it. Capture `before` **before** the `gh workflow run` in Step 3.

Then report to the user: the tag, its URL, and that `release.yml` is now running and will generate the release notes from the fragments. `ci.yml` also runs against the tagged tree, because the pin commit deliberately carries no skip marker — a red there is worth surfacing even though nothing gates on it.

## If it fails halfway

The workflow pushes twice — the pin commit to `main`, then the tag — so a failure between them leaves a real state. Read where it stopped before doing anything.

- **It refused before the pin push** (missing `RELEASE_TOKEN`, dispatched off `main`, `main` moved since the SHA you inspected, invalid version, tag already exists, no fragments, version mismatch, a `flake.nix` assertion). Nothing was committed, pushed or tagged. Fix the cause and dispatch again — for the moved-`main` case that means re-running Step 1, since the fragment list you reviewed is no longer the one that would ship.
- **The pin push was rejected because `main` advanced.** Same state as above: nothing tagged. The workflow deliberately does not rebase and retry, because a rebase would move the pin onto a tree nobody inspected whose fragments may no longer compute this version. Dispatch again; it re-derives from the new tip.
- **The pin landed but the tag push failed.** `main` carries `chore: pin flake version to v<X.Y.Z>` and there is no tag and no release. Do not revert it. Dispatch again with the same version: the fragments are untouched so it computes the same value, the pin edit is a no-op so no second commit is made, and it retries the tag.
- **The tag landed but `release.yml` failed.** Which recovery depends on whether its `prepare` job finished, because `prepare` commits the assembled changelog and consumes `changelog.d/`. If the fragments are still there, delete the tag (`git push origin :refs/tags/v<X.Y.Z>`) and the GitHub Release if one was created, fix the cause, and dispatch this workflow again. If the fragments are gone, the tag and the pin are already correct — re-run `release.yml` from its own `workflow_dispatch` with `version=<X.Y.Z>` instead, and do not re-dispatch this one (it would refuse with `NO_FRAGMENTS`).

## Step 5 — Clean up merged worktrees and branches

Once the release is tagged, the branches and worktrees whose work it contains are done.

```bash
bash .claude/skills/tag-release/cleanup.sh
```

The script is detection-only — it never removes a worktree or deletes a branch (it does run `git fetch --prune`, which refreshes local remote-tracking refs and touches the remote not at all). If it prints `NOTHING_TO_CLEAN=true`, say so and finish. Otherwise present the `WORKTREES`, `LOCAL_BRANCHES` and `REMOTE_BRANCHES` lists and get explicit confirmation before deleting anything.

**This step is destructive — always show the full list and get explicit confirmation first.** The script excludes the default branch and the branch and worktree you are standing in. The open-PR guard and the squash-merge half of the detection both come from `gh pr list`, so they hold only where `gh` is on PATH and authenticated: without it the script silently falls back to the ancestry test alone, which offers fewer branches but also stops protecting one that has an open PR. Check that `gh` works before trusting a list.

After confirmation, process the items **in this order**:

1. Remove each worktree. This must come before deleting its branch, because a branch checked out in a worktree cannot be deleted:

   ```bash
   git worktree remove [worktree_path]
   ```

   If a worktree has uncommitted changes git refuses. Report it and skip rather than reaching for `--force`, unless the user explicitly asks.

2. Delete each local branch. Each entry is `<branch> <sha>`, where the SHA is the tip `cleanup.sh` vetted; confirm the branch still points at it and then use `-D`:

   ```bash
   [ "$(git rev-parse [branch])" = "[sha]" ] && git branch -D [branch]
   ```

   **`-D`, and the `-d` this used to recommend was not the safety net it read as.** `git branch -d` tests *ancestry* — is this tip reachable from the branch's upstream, or from `HEAD` — and a squash merge never lands the branch's commits on `main`, so it refuses every correctly squash-merged branch and hints the operator straight to `-D`. Measured 2026-09-14: it would have refused all 13 correctly-merged dispatch branches. Re-measured while forking this skill on PR #1081's merged head: `error: the branch 'agent/dispatch-prd-220' is not fully merged`. A check that is wrong that often does not make anyone careful — it teaches them to type `-D` on everything, including the one branch that genuinely was not merged. The same trap is on the record at `src/main.rs:238`, where the deck's own `worktree` reclaim command says it *"never inspects git ancestry for merge state — squash-merges never enter `main`'s ancestry, and an ancestor branch with no PR must never be removed"*. Note that command drops the ancestry test in **both** directions, and this one does not: it removes worktree *directories*, which can hold work that is nowhere else, so an unmerged ancestor branch matters to it. Deleting a branch whose tip is reachable from `origin/main` loses no commit, so that arm is kept here.

   The check that does hold is the one `cleanup.sh` already made, from PR state rather than ancestry: it offers a branch only when its tip is either reachable from `origin/<default>` or is exactly the head SHA of a merged, same-repo PR, and never when an open PR points at that name. The SHA comparison above is what carries that verdict to the delete — it is what catches the branch having moved since the scan, which is the one thing the scan cannot see. So delete only names `cleanup.sh` printed, and re-run it rather than reusing a stale list.

3. Delete each remote branch, gated the same way against its vetted SHA:

   ```bash
   [ "$(git rev-parse refs/remotes/origin/[branch])" = "[sha]" ] && git push origin --delete [branch]
   ```

   That compares the remote-tracking ref `cleanup.sh` refreshed with its own `git fetch --prune`, so it catches a list gone stale in your hands but not the remote advancing since that fetch. Re-run `cleanup.sh` rather than working from an old list.

Finally, prune stale worktree metadata:

```bash
git worktree prune
```

## Guidelines

- **Review the fragments before proposing a version.** The bump is derived from their types, so a mistyped type changes the release.
- **The bump policy is `docs/develop/versioning.md`.** `analyze.sh` is the implementation of the table there, including the pre-1.0 recalibration where `breaking` bumps the *minor* and `feature`/`bugfix` are patches. Do not restate the policy — read it there, and if you change one, change both.
- **Keep the tag message to one or two sentences.** The GitHub Release body comes from the fragments, not from here.
- **Clean up only after tagging**, never before.
