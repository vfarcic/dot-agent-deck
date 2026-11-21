#!/usr/bin/env bash
# Host screenshots for a pull request's description (issue #2438).
#
#   .claude/skills/pr-create/pr-screenshots.sh <pr-number> <image>...
#
# GitHub has no API for the image uploads its web editor does, so the images go
# to a branch of their own, `pr-screenshots`, which shares no history with
# `main` and is never merged: each file lands at `<pr-number>/<file name>` in a
# new commit on top of it, built with git plumbing so the working tree, the
# index and the current branch are left alone. The script prints one Markdown
# image line per file, with a URL pinned to that commit, ready to paste into the
# PR body. Uploading a file of the same name again replaces it on the branch
# and prints a new URL; a body that still carries the old URL keeps showing the
# old image.
#
# Another agent may push to the branch between this script's fetch and its
# push; the push is then rejected as a non-fast-forward, and the script fetches
# again and rebuilds its commit on the new tip, up to five times.
#
# PR_SCREENSHOTS_REMOTE overrides the remote (default `origin`); a path or URL
# works too, which is how the script is tried against a scratch bare repo.

set -euo pipefail

branch=pr-screenshots
remote=${PR_SCREENSHOTS_REMOTE:-origin}

if [ "$#" -lt 2 ]; then
  echo "usage: $0 <pr-number> <image>..." >&2
  exit 2
fi
pr=$1
shift
case "$pr" in
  '' | *[!0-9]*) echo "ERROR: the PR number must be digits, got: $pr" >&2; exit 2 ;;
esac

declare -a names=()
for f in "$@"; do
  [ -f "$f" ] || { echo "ERROR: no such file: $f" >&2; exit 2; }
  name=$(basename -- "$f")
  case "$name" in
    *[!A-Za-z0-9._-]*) echo "ERROR: rename it to letters, digits, '.', '_' and '-' only: $name" >&2; exit 2 ;;
  esac
  case "$name" in
    *.png | *.jpg | *.jpeg | *.gif | *.webp) ;;
    *) echo "ERROR: not a .png, .jpg, .jpeg, .gif or .webp image: $name" >&2; exit 2 ;;
  esac
  for seen in "${names[@]+"${names[@]}"}"; do
    [ "$seen" != "$name" ] || { echo "ERROR: two files named $name" >&2; exit 2; }
  done
  names+=("$name")
done

repo=$(gh repo view --json nameWithOwner --jq .nameWithOwner)
tmp=$(mktemp -d)
tip_ref=refs/pr-screenshots-upload/tip-$$
cleanup() {
  rm -rf "$tmp"
  git update-ref -d "$tip_ref" 2>/dev/null || true
}
trap cleanup EXIT
export GIT_INDEX_FILE="$tmp/index"

commit=""
for attempt in 1 2 3 4 5; do
  # Exit status 2 from ls-remote means the branch does not exist yet; any other
  # failure (network, auth) is not that, and stops the script.
  parent=""
  if git ls-remote --exit-code --heads "$remote" "$branch" >/dev/null; then
    git fetch -q "$remote" "+refs/heads/$branch:$tip_ref"
    parent=$(git rev-parse "$tip_ref")
    git read-tree "$parent"
  else
    status=$?
    [ "$status" -eq 2 ] || { echo "ERROR: could not read $remote (git ls-remote exited $status)" >&2; exit 1; }
    git read-tree --empty
  fi
  i=0
  for f in "$@"; do
    blob=$(git hash-object -w -- "$f")
    git update-index --add --cacheinfo "100644,$blob,$pr/${names[$i]}"
    i=$((i + 1))
  done
  tree=$(git write-tree)
  if [ -n "$parent" ]; then
    commit=$(git commit-tree "$tree" -p "$parent" -m "Screenshots for PR #$pr")
  else
    commit=$(git commit-tree "$tree" -m "Screenshots for pull request descriptions (see .claude/skills/pr-create/pr-screenshots.sh)")
  fi
  if git push -q "$remote" "$commit:refs/heads/$branch" 2>"$tmp/push.err"; then
    break
  fi
  if [ "$attempt" -eq 5 ]; then
    cat "$tmp/push.err" >&2
    echo "ERROR: the push to $branch was rejected five times" >&2
    exit 1
  fi
  echo "push to $branch rejected (attempt $attempt), fetching its new tip and retrying" >&2
  rm -f "$GIT_INDEX_FILE"
  sleep $((attempt * 2))
done

for name in "${names[@]}"; do
  echo "![${name%.*}](https://raw.githubusercontent.com/$repo/$commit/$pr/$name)"
done
