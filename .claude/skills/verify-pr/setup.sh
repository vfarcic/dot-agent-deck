#!/usr/bin/env bash
#
# Phase 1 of /verify-pr: check a contributor's PR out into its own worktree so
# the full check suite can run without colliding with other agents.
#
# Usage: setup.sh <pr-number> [--force]
#        setup.sh <pr-number> --baseline
#
# Deliberately a SIBLING of `.claude/skills/worktree-prd/create.sh`
# rather than a caller of it: that script exists to start NEW work, so it
# derives the branch name from the PRD (its `prds/<n>-*.md`, else its issue)
# and always branches from `main`.
# Reviewing a PR needs the opposite — a branch pinned to the contributor's head
# commit, fork included. The conventions are kept identical on purpose: same
# `../<repo>-<suffix>` path scheme, same validate-then-create ordering, same
# `KEY=value` / `ERROR=true` output contract, so both read the same way.
#
# The output grammar and why every value is sanitised live in `stream.sh`
# (issue #521). This script emits a PR title, a branch name and git's own error
# text, all of which a contributor writes.
#
# `--baseline` creates a second, detached worktree at the merge-base. Use it to
# answer "does this check already fail without the PR?" before blaming the PR
# for a red result.
#
# Logs and metadata go under `<worktree>/target/verify-pr/`, which is gitignored
# (`/target`), so the worktree stays clean and `git worktree remove` never needs
# `--force` at teardown.

set -uo pipefail

stream_lib="$(dirname "${BASH_SOURCE[0]}")/stream.sh"
# shellcheck source=stream.sh
if ! . "$stream_lib"; then
  echo "verify-pr: cannot source ${stream_lib}; the skill directory is incomplete" >&2
  exit 1
fi

if [ $# -lt 1 ]; then
  emit ERROR true
  emit MESSAGE "Usage: setup.sh <pr-number> [--force|--baseline]"
  exit 0
fi

# Kept because `shift` is about to consume it: the error message below quotes
# what the caller actually typed, and under `set -u` reading `$1` after the
# shift aborted the script mid-record — `ERROR=true` with no `MESSAGE`, which
# reads as a truncated stream rather than a usage error.
pr_arg="$1"
pr="${1#\#}"
pr="${pr##*/}"
shift

force=false
baseline=false
for arg in "$@"; do
  case "$arg" in
    --force) force=true ;;
    --baseline) baseline=true ;;
    *)
      emit ERROR true
      emit MESSAGE "Unknown argument '$arg'"
      exit 0
      ;;
  esac
done

if ! [[ "$pr" =~ ^[0-9]+$ ]]; then
  emit ERROR true
  emit MESSAGE "Could not parse a PR number from '$pr_arg'"
  exit 0
fi

for tool in git gh; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    emit ERROR true
    emit MESSAGE "${tool} is required"
    exit 0
  fi
done

if ! repo_root=$(git rev-parse --show-toplevel 2>&1); then
  emit ERROR true
  emit MESSAGE "Not in a git repository: ${repo_root}"
  exit 0
fi
repo_name=$(basename "$repo_root")

default_branch="main"
if ref=$(git symbolic-ref --quiet refs/remotes/origin/HEAD 2>/dev/null); then
  default_branch="${ref#refs/remotes/origin/}"
fi

# Refresh the default branch's remote-tracking ref with an explicit refspec so
# the later merge and merge-base are computed against current origin.
git fetch origin "+refs/heads/${default_branch}:refs/remotes/origin/${default_branch}" --quiet 2>/dev/null || true

# --- PR metadata -----------------------------------------------------------

# `authorAssociation` is REST-only, not a `gh pr view --json` field.
#
# One line, fields joined by `stream.sh`'s `FIELD_SEP`, with the shared
# sanitiser mapped over every one of them so a field added to the array
# inherits it. Without that, a title containing a newline would end the line
# early and `read` below would bind the remaining variables to nothing — the
# same class of defect as the forged records in `scan.sh` (issue #521), landing
# here as silent truncation instead. See `FIELD_SEP` for why the separator is
# not a tab.
if ! pr_meta=$(gh pr view "$pr" --json headRefName,headRefOid,baseRefName,author,mergeable,mergeStateStatus,title --jq '
  [.headRefName, .headRefOid, .baseRefName, .author.login, .mergeable, .mergeStateStatus, .title]
  | '"${JQ_FIELDS}" 2>&1); then
  emit ERROR true
  emit MESSAGE "gh pr view $pr failed: ${pr_meta}"
  exit 0
fi

IFS="$FIELD_SEP" read -r head_branch head_sha base_branch author mergeable merge_state title <<<"$pr_meta"
association=$(gh api "repos/{owner}/{repo}/pulls/${pr}" --jq '.author_association' 2>/dev/null || echo UNKNOWN)

# --- Baseline mode ---------------------------------------------------------

if [ "$baseline" = true ]; then
  base_path="../${repo_name}-pr-${pr}-base"
  # Fetched into FETCH_HEAD rather than a named remote-tracking ref on purpose:
  # a named ref would outlive the baseline worktree and quietly accumulate one
  # stale `origin/pr-<n>-head` per PR ever reviewed, which the teardown in
  # SKILL.md does not clean up. FETCH_HEAD is transient.
  if ! git fetch origin "refs/pull/${pr}/head" --quiet 2>&1; then
    emit ERROR true
    emit MESSAGE "Could not fetch refs/pull/${pr}/head"
    exit 0
  fi
  merge_base=$(git merge-base "origin/${default_branch}" FETCH_HEAD 2>/dev/null)
  if [ -z "$merge_base" ]; then
    emit ERROR true
    emit MESSAGE "Could not compute a merge-base for PR ${pr}"
    exit 0
  fi
  if [ -d "$base_path" ]; then
    emit ERROR true
    emit MESSAGE "Baseline worktree '${base_path}' already exists"
    exit 0
  fi
  if ! out=$(git worktree add --detach "$base_path" "$merge_base" 2>&1); then
    emit ERROR true
    emit MESSAGE "${out}"
    exit 0
  fi
  mkdir -p "${base_path}/target/verify-pr"
  emit SUCCESS true
  emit MODE baseline
  emit BASELINE_PATH "${base_path}"
  emit MERGE_BASE "${merge_base}"
  emit NOTE "Run the same failing check here; if it fails at the merge-base too, the PR did not cause it."
  exit 0
fi

# --- Branch and path ------------------------------------------------------

worktree_path="../${repo_name}-pr-${pr}"

# Prefer the PR's own head-branch name: `/tag-release`'s cleanup detection
# matches merged PRs by `headRefName`, so a worktree named this way is found
# automatically after the PR merges — including after a squash merge, where the
# commits never land verbatim on main.
#
# Two hard exclusions, because the fetch below is a FORCED update of whatever
# branch is named here:
#
#   1. The default branch. A fork PR raised from the contributor's own `main`
#      reports `headRefName=main` (PR #360 is exactly this), and reusing that
#      name would overwrite the reviewer's local `main` with the PR head.
#   2. Any name that already exists locally. It belongs to somebody else's work.
#
# Either case falls back to `pr-<n>-verify`, which this skill owns by
# convention, so force-updating it is safe.
branch_name="$head_branch"
if [ "$branch_name" = "$default_branch" ] ||
  git show-ref --verify --quiet "refs/heads/${branch_name}" 2>/dev/null; then
  branch_name="pr-${pr}-verify"
fi

# Only the skill-owned fallback name may be recycled, and only on --force.
if git show-ref --verify --quiet "refs/heads/${branch_name}" 2>/dev/null && [ "$force" != true ]; then
  emit ERROR true
  emit BRANCH_NAME "${branch_name}"
  emit MESSAGE "Branch '${branch_name}' already exists. Pass --force to reset it to PR ${pr}'s current head."
  exit 0
fi

# --- Validate (mirrors create.sh) ----------------------------------------

errors=()

if [ -d "$worktree_path" ]; then
  if [ "$force" = true ]; then
    if ! out=$(git worktree remove "$worktree_path" 2>&1); then
      errors+=("Could not remove existing worktree '${worktree_path}': ${out} (it has local changes — remove it by hand if they are disposable)")
    fi
  else
    errors+=("Worktree path '${worktree_path}' already exists (pass --force to recreate it, or review the existing one)")
  fi
fi

if git worktree list --porcelain 2>/dev/null | grep -q "^branch refs/heads/${branch_name}$"; then
  errors+=("Branch '${branch_name}' is checked out in another worktree")
fi

if [ ${#errors[@]} -gt 0 ]; then
  emit ERROR true
  emit BRANCH_NAME "${branch_name}"
  emit WORKTREE_PATH "${worktree_path}"
  # Free text: these carry git's own error output, which is multi-line.
  emit_header ERRORS
  printf '%s\n' "${errors[@]}" | emit_block
  exit 0
fi

# --- Fetch the PR head ----------------------------------------------------

# `refs/pull/<n>/head` is served by the BASE repo, so this works for forks with
# no extra remote — and it pins the review to the exact commit the PR proposes.
if ! out=$(git fetch origin "+refs/pull/${pr}/head:refs/heads/${branch_name}" 2>&1); then
  emit ERROR true
  emit MESSAGE "Could not fetch refs/pull/${pr}/head into ${branch_name}: ${out}"
  exit 0
fi

fetched_sha=$(git rev-parse "$branch_name" 2>/dev/null)
if [ "$fetched_sha" != "$head_sha" ]; then
  emit WARNING "Fetched ${fetched_sha} but the API reports head ${head_sha}; the PR may have been pushed to mid-fetch. Re-run with --force."
fi

if ! out=$(git worktree add "$worktree_path" "$branch_name" 2>&1); then
  emit ERROR true
  emit MESSAGE "${out}"
  exit 0
fi

# Per `/worktree-prd` Step 3: local settings are untracked, so a fresh worktree
# starts without them. Skip silently when absent.
if [ -f "${repo_root}/.claude/settings.local.json" ]; then
  cp "${repo_root}/.claude/settings.local.json" "${worktree_path}/.claude/settings.local.json" 2>/dev/null || true
fi

out_dir="${worktree_path}/target/verify-pr"
mkdir -p "${out_dir}/logs"

merge_base=$(git merge-base "origin/${default_branch}" "$branch_name" 2>/dev/null)

# --- Verify the MERGE RESULT, not the bare head ---------------------------

# CI tests the merge commit, so a PR that is green in isolation can still break
# main. Merging here reproduces what will actually land.
merge_result="clean"
merge_output=$(git -C "$worktree_path" -c user.name="verify-pr" -c user.email="verify-pr@local" \
  merge --no-edit "origin/${default_branch}" 2>&1)
merge_status=$?
if [ $merge_status -ne 0 ]; then
  merge_result="conflict"
  git -C "$worktree_path" merge --abort 2>/dev/null || true
fi

behind=$(git rev-list --count "${branch_name}..origin/${default_branch}" 2>/dev/null || echo unknown)

{
  emit PR_NUMBER "${pr}"
  emit PR_TITLE "${title}"
  emit PR_AUTHOR "${author}"
  emit PR_AUTHOR_ASSOCIATION "${association}"
  emit PR_HEAD_SHA "${head_sha}"
  emit PR_BASE_BRANCH "${base_branch}"
  emit PR_HEAD_BRANCH "${head_branch}"
  emit BRANCH_NAME "${branch_name}"
  emit WORKTREE_PATH "${worktree_path}"
  emit DEFAULT_BRANCH "${default_branch}"
  emit MERGE_BASE "${merge_base}"
  emit MERGE_RESULT "${merge_result}"
  emit COMMITS_BEHIND_MAIN "${behind}"
  emit GH_MERGEABLE "${mergeable}"
  emit GH_MERGE_STATE "${merge_state}"
} | tee "${out_dir}/meta.env"

emit OUT_DIR "${out_dir}"
if [ "$merge_result" = "conflict" ]; then
  # An indented block rather than the `KEY<<EOF … EOF` shape this used to
  # print: git names the conflicting paths in that output, and a repo can
  # contain a file called `EOF` — a delimiter a contributor can write is not a
  # delimiter. Indentation needs no terminator to be unambiguous.
  emit_header "MERGE CONFLICT OUTPUT"
  printf '%s\n' "$merge_output" | emit_block
  emit NOTE "origin/${default_branch} does NOT merge cleanly; the merge was aborted and the worktree sits at the PR head. Checks below therefore describe the head, not what would land."
fi
emit SUCCESS true
