#!/usr/bin/env bash
set -euo pipefail

# Create a git worktree for PRD work with a descriptive branch name.
# Usage: create.sh <prd-number> [prd-title]
#
# The script locates the PRD through ../prd-start/prd-source.sh, the one place
# that decides where a PRD lives: prds/<number>-*.md when that file exists, and
# the PRD's GitHub issue otherwise (issue #1591: a new PRD lives in its issue
# and has no file). If prd-title is not provided, it is taken from there.
# This script validates everything and creates the worktree, or reports errors.

if [ $# -lt 1 ]; then
  echo "ERROR=true"
  echo "MESSAGE=Usage: create.sh <prd-number> [prd-title]"
  exit 0
fi

prd_number="$1"
prd_title="${2:-}"

# --- Locate the PRD, and resolve its title if not provided ---
#
# Always looked up, even with a title supplied, so that an issue number that
# cannot be read stops here instead of getting a worktree, and PRD_SOURCE is
# reported either way.

located=$(bash "$(dirname "$0")/../prd-start/prd-source.sh" "$prd_number" || true)
prd_source=$(printf '%s\n' "$located" | sed -n 's/^SOURCE=//p' | head -1)
found_title=$(printf '%s\n' "$located" | sed -n 's/^TITLE=//p' | head -1)
if [ -z "$found_title" ]; then
  echo "ERROR=true"
  echo "MESSAGE=No PRD file matches prds/${prd_number}-*.md and issue #${prd_number} could not be read: $(printf '%s\n' "$located" | sed -n 's/^REASON=//p' | head -1)"
  exit 0
fi
if [ -z "$prd_title" ]; then
  # An issue title carries the "PRD:" / "PRD #123:" prefix a file heading does.
  prd_title=$(echo "$found_title" | sed -E 's/^PRD *#?[0-9]* *[:\-] *//')
fi

# --- Generate branch name ---

slug=$(echo "$prd_title" \
  | tr '[:upper:]' '[:lower:]' \
  | tr ' ' '-' \
  | sed 's/[^a-z0-9.\-]//g' \
  | sed 's/--*/-/g' \
  | sed 's/^-//;s/-$//' \
  | cut -c1-50)

branch_name="prd-${prd_number}-${slug}"

# --- Compute worktree path ---

if ! repo_root=$(git rev-parse --show-toplevel 2>&1); then
  echo "ERROR=true"
  echo "MESSAGE=Not in a git repository: ${repo_root}"
  exit 0
fi
repo_name=$(basename "$repo_root")
worktree_path="../${repo_name}-${branch_name}"

# --- Validate ---

errors=()

if git show-ref --verify --quiet "refs/heads/${branch_name}" 2>/dev/null; then
  errors+=("Branch '${branch_name}' already exists")
fi

if [ -d "$worktree_path" ]; then
  errors+=("Worktree path '${worktree_path}' already exists")
fi

if git worktree list --porcelain 2>/dev/null | grep -q "^branch refs/heads/${branch_name}$"; then
  errors+=("Worktree for '${branch_name}' is already registered")
fi

if [ ${#errors[@]} -gt 0 ]; then
  echo "ERROR=true"
  echo "BRANCH_NAME=${branch_name}"
  echo "WORKTREE_PATH=${worktree_path}"
  echo "ERRORS:"
  for err in "${errors[@]}"; do
    echo "  ${err}"
  done
  exit 0
fi

# --- Create worktree ---

default_branch=$(git symbolic-ref refs/remotes/origin/HEAD 2>/dev/null | sed 's@^refs/remotes/origin/@@' || echo "main")

if ! output=$(git worktree add "${worktree_path}" -b "${branch_name}" "${default_branch}" 2>&1); then
  echo "ERROR=true"
  echo "BRANCH_NAME=${branch_name}"
  echo "WORKTREE_PATH=${worktree_path}"
  echo "ERRORS:"
  echo "  ${output}"
  exit 0
fi

echo "SUCCESS=true"
echo "BRANCH_NAME=${branch_name}"
echo "WORKTREE_PATH=${worktree_path}"
echo "PRD_TITLE=${prd_title}"
echo "PRD_SOURCE=${prd_source}"
echo "GIT_OUTPUT=${output}"
