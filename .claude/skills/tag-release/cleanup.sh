#!/usr/bin/env bash
set -euo pipefail

# Detect worktrees and local/remote branches whose work has already been merged,
# so the release skill can prune them after tagging.
#
# This script is detection-only: it never removes a worktree or deletes a branch.
# It does run `git fetch --prune` to refresh remote-tracking refs, which only
# updates local bookkeeping and never modifies the remote.

# --- Determine the default branch ---
default_branch="main"
if ref=$(git symbolic-ref --quiet refs/remotes/origin/HEAD 2>/dev/null); then
  default_branch="${ref#refs/remotes/origin/}"
fi

# --- Refresh remote-tracking refs (drops refs for branches deleted upstream) ---
git fetch --prune --quiet origin 2>/dev/null || true

current_branch=$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo "")
current_worktree=$(git rev-parse --show-toplevel 2>/dev/null || echo "")

# --- Gather PR state ---
#
# Merge detection is per-COMMIT, not per-branch-name. A name alone proves
# nothing: Renovate reuses one branch name across many PRs, so a merged
# `renovate/foo` PR leaves that name looking "merged" long after the branch has
# been recreated at a new, unmerged tip for the next open PR. Judging by name
# there offers a live PR's branch up for deletion.
#
# Held as newline-delimited strings rather than `declare -A` associative arrays,
# which are bash 4. macOS ships /bin/bash 3.2.57, where `declare -A` does not go
# quietly unsupported: the assignment degrades to an ordinary INDEXED one, the
# subscript is evaluated arithmetically, and `set -u` kills the script on that
# line with a message naming a variable that appears nowhere in it. The same
# defect was fixed in `scripts/assemble-changelog.sh`, `verify-pr/scan.sh` and
# `demo-reel-adapter/build.sh` (issues #521, #582); this file was the one
# instance knowingly left behind, because it was a `dot-ai-` synced mirror and a
# fix would have been reverted by the next sync. Issue #1089 forked it, so the
# reason expired and the fix lands here.
#
# A newline is the one delimiter that is safe: git refuses a ref name containing
# one, so no branch name can forge a record boundary.
merged_pairs=$'\n'   # lines of "<branch name> <head sha>" for merged PRs
open_names=$'\n'     # lines of "<branch name>" with an open PR, never candidates

if command -v gh >/dev/null 2>&1; then
  # Head SHAs of merged same-repo PRs. Covers squash & rebase merges, where the
  # branch's commits never land verbatim on the default branch and so are
  # invisible to an ancestry test. Cross-repo (fork) PRs are excluded: their
  # head names describe branches in the fork, not ours, so a merged fork PR for
  # `fix/thing` says nothing about our own `fix/thing`.
  while IFS=$'\t' read -r name sha; do
    if [ -z "$name" ] || [ -z "$sha" ]; then continue; fi
    merged_pairs="${merged_pairs}${name} ${sha}"$'\n'
  done < <(gh pr list --state merged --limit 200 --json headRefName,headRefOid,isCrossRepository \
             --jq '.[] | select(.isCrossRepository | not) | [.headRefName, .headRefOid] | @tsv' 2>/dev/null || true)

  # Any open PR protects its head branch. Deleting the branch closes the PR, so
  # this is the guard that matters most. Fork PRs are deliberately INCLUDED
  # here: matching a fork's head name against ours can only over-protect (we
  # keep a branch we might have pruned), which is the safe direction to err.
  # The limit is higher than the merged query's on purpose: truncating merged
  # PRs just leaves a branch unoffered, but truncating OPEN ones would offer a
  # live PR's branch for deletion.
  while IFS= read -r b; do
    if [ -z "$b" ]; then continue; fi
    open_names="${open_names}${b}"$'\n'
  done < <(gh pr list --state open --limit 1000 --json headRefName --jq '.[].headRefName' 2>/dev/null || true)
fi

# is_merged <branch-name> <ref-to-its-tip>
# Resolves the ref's OWN tip, so a local branch and its same-named remote are
# judged independently -- `origin/foo` may carry unmerged commits that local
# `foo` does not. On success the vetted tip is left in `vetted_tip`, which the
# caller records beside the branch so the delete step can confirm the branch has
# not moved since (see the skill's cleanup step).
vetted_tip=""
is_merged() {
  local name="$1" ref="$2" tip mname msha
  vetted_tip=""
  case "$open_names" in
    *$'\n'"$name"$'\n'*) return 1 ;;
  esac
  tip=$(git rev-parse --verify --quiet "$ref") || return 1
  if [ -z "$tip" ]; then return 1; fi
  # Real merge or fast-forward: the tip is already reachable from the default.
  if git merge-base --is-ancestor "$tip" "refs/remotes/origin/${default_branch}" 2>/dev/null; then
    vetted_tip="$tip"
    return 0
  fi
  # Squash/rebase merge: the tip must still be exactly what the merged PR
  # carried. A recreated or advanced branch has moved on and is not merged.
  while IFS=' ' read -r mname msha; do
    if [ "$mname" = "$name" ] && [ "$msha" = "$tip" ]; then
      vetted_tip="$tip"
      return 0
    fi
  done <<< "$merged_pairs"
  return 1
}

# --- Worktrees on merged branches (never the current worktree, never default) ---
#
# The default-branch exclusion is NOT redundant with the local-branch loop below,
# which has always had its own. Without it this loop offers the MAIN CHECKOUT for
# removal whenever the script is run from a linked worktree: the main tree sits on
# `main`, `main`'s tip is by definition an ancestor of `origin/main`, and the only
# other guard here is "not the worktree I am standing in". Measured on this repo
# while forking the skill (issue #1089) — a run from a dispatch worktree printed
# `WORKTREES: /home/vfarcic/code/dot-agent-deck|main`. `git worktree remove`
# refuses a main working tree, so the blast radius was a confusing failure rather
# than a deletion; but the skill presents this list for confirmation, and a list
# that routinely contains the main checkout teaches the operator to confirm
# without reading. It also made the skill's own "cleanup.sh already excludes the
# default branch" a false claim.
worktrees_out=()
wt_path=""
while IFS= read -r line; do
  case "$line" in
    "worktree "*) wt_path="${line#worktree }" ;;
    "branch refs/heads/"*)
      br="${line#branch refs/heads/}"
      if [ "$wt_path" != "$current_worktree" ] && [ "$br" != "$default_branch" ] \
         && is_merged "$br" "refs/heads/${br}"; then
        worktrees_out+=("${wt_path}|${br}")
      fi
      ;;
    "") wt_path="" ;;
  esac
done < <(git worktree list --porcelain 2>/dev/null || true)

# --- Local branches that are merged (never current / default) ---
# A branch checked out in another worktree is still listed here; it is only
# deletable once its worktree is removed, hence the worktree-first ordering in
# the skill's cleanup step.
local_out=()
while IFS= read -r b; do
  [ -z "$b" ] && continue
  [ "$b" = "$default_branch" ] && continue
  [ "$b" = "$current_branch" ] && continue
  if is_merged "$b" "refs/heads/${b}"; then local_out+=("${b} ${vetted_tip}"); fi
done < <(git branch --format='%(refname:short)' 2>/dev/null || true)

# --- Remote branches that are merged (never default) ---
remote_out=()
while IFS= read -r b; do
  b="${b#origin/}"
  [ -z "$b" ] && continue
  [ "$b" = "HEAD" ] && continue
  [ "$b" = "$default_branch" ] && continue
  if is_merged "$b" "refs/remotes/origin/${b}"; then remote_out+=("${b} ${vetted_tip}"); fi
done < <(git branch -r --format='%(refname:short)' 2>/dev/null | grep '^origin/' || true)

# --- Output structured summary ---
#
# Each branch is printed as `<name> <sha>`, where the SHA is the tip this script
# actually vetted. That is not decoration: it is what lets the delete step verify
# a squash-merged branch, which `git branch -d` cannot (it tests ancestry, and a
# squash merge never puts the branch's commits on the default branch). The skill
# compares the branch's current tip against this value before deleting it.
echo "DEFAULT_BRANCH=${default_branch}"

total=$(( ${#worktrees_out[@]} + ${#local_out[@]} + ${#remote_out[@]} ))
if [ "$total" -eq 0 ]; then
  echo "NOTHING_TO_CLEAN=true"
  exit 0
fi
echo "NOTHING_TO_CLEAN=false"

echo "WORKTREES:"
for w in "${worktrees_out[@]:-}"; do [ -n "$w" ] && echo "  ${w}"; done

echo "LOCAL_BRANCHES:"
for b in "${local_out[@]:-}"; do [ -n "$b" ] && echo "  ${b}"; done

echo "REMOTE_BRANCHES:"
for b in "${remote_out[@]:-}"; do [ -n "$b" ] && echo "  ${b}"; done

exit 0
