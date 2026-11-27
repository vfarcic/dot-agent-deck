#!/usr/bin/env bash
# Whether a dispatch unit's name is free to use (issue #2509).
#
#   unit-name-free.sh <unit-name> <owner/repo>
#
# `dispatch` creates the branch `agent/dispatch-<unit-name>`. A name is taken
# when that branch exists locally, when it exists on `origin`, or when any pull
# request, open, merged or closed, ever had it as its head: a unit's branch is
# usually deleted once its pull request merges, and a new branch under the same
# name then picks up the old pull request wherever a pull request is found by
# its head branch's name.
#
# Prints one line and exits with its status:
#   FREE                  0
#   TAKEN: <why>          1
#   UNKNOWN: <why>        2   a lookup failed; treat it as taken
set -u

name="${1:?usage: unit-name-free.sh <unit-name> <owner/repo>}"
repo="${2:?usage: unit-name-free.sh <unit-name> <owner/repo>}"
branch="agent/dispatch-$name"

if git show-ref --verify --quiet "refs/heads/$branch"; then
    echo "TAKEN: local branch $branch exists"
    exit 1
fi

git ls-remote --exit-code --heads origin "refs/heads/$branch" >/dev/null 2>&1
rc=$?
if [ "$rc" = 0 ]; then
    echo "TAKEN: branch $branch exists on origin"
    exit 1
elif [ "$rc" != 2 ]; then
    echo "UNKNOWN: git ls-remote origin failed (exit $rc)"
    exit 2
fi

if ! prs=$(gh pr list --repo "$repo" --head "$branch" --state all --json number --jq 'map("#\(.number)") | join(" ")'); then
    echo "UNKNOWN: gh pr list failed"
    exit 2
fi
if [ -n "$prs" ]; then
    echo "TAKEN: pull request(s) $prs used $branch"
    exit 1
fi
echo FREE
