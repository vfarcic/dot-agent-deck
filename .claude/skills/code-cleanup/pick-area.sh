#!/usr/bin/env bash
#
# Pick one random area for a /code-cleanup unit to examine, or check that a
# unit's changes stay inside its mode's owned set.
#
# Usage: pick-area.sh <code|tests|instructions>
#        pick-area.sh <code|tests|instructions> --check
#
# Run from inside the unit's own worktree, after `git fetch origin`. Prints:
#
#   MODE=<mode>
#   CANDIDATES=<files in the mode's owned set that survived the filters>
#   SKIPPED_BUSY=<owned files skipped because an open PR or a running unit touches them>
#   SKIPPED_RECENT=<owned files skipped because a recent cleanup PR touched them>
#   FILE=<path>
#   LINE=<a random line in FILE; the area is the item that encloses it>
#
# or `ERROR=<reason>` and a non-zero exit.
#
# With `--check` it reads every file changed since the merge-base with
# origin/main — committed, staged, unstaged and untracked, with renames split
# into their deletion and addition so both paths are judged — and prints
# `OUTSIDE=<path>` for each one the mode does not own, exiting 1 if there is
# any. In tests mode it also prints `SNAPSHOT=<path>` for each changed insta
# snapshot: the mode may rename or delete one with the test it belongs to, but
# a changed snapshot BODY is a behaviour change, so the unit confirms each by
# hand. The check sets are wider than the pick sets: a file nobody should open
# as an area (tests/CATALOG.md, a snapshot) can still be one a change must
# touch.
#
# The pick is weighted by size in BYTES, not lines, so a 44k-line file comes up
# far more often than a small helper, and CLAUDE.md — a few hundred lines, each
# a whole paragraph — weighs what its text weighs. The line is drawn uniformly
# by byte offset inside the chosen file, so a long function is likelier to be
# hit than a short one, for the same reason.
#
# Three filters, in this order:
#   1. the mode's OWNED set (SKILL.md "The three modes"), minus files no
#      cleanup should open as an area (snapshots, the catalog, generated or
#      binary files);
#   2. BUSY files: every file an open PR changes, and every file a running
#      dispatch unit on this machine has changed, committed or not. A running
#      unit is a linked worktree on an `agent/dispatch-*` branch; this unit's
#      own worktree is left out. A unit on another machine that has not pushed
#      is invisible here — accepted, since its PR will show up in (2) once it
#      opens and the cleanup diff is small;
#   3. RECENT files: every file the 20 most recent `cleanup`-labelled PRs
#      changed, in any state, so a closed-unmerged cleanup is not retried at
#      once either.
#
# PR file names come from GitHub and on a public repository a fork chooses
# them. They are only ever used as exact-match exclusion lines here and are
# never printed, so they cannot reach the caller's terminal or context.

set -uo pipefail

die() { echo "ERROR=$*"; exit 1; }

mode="${1:-}"
case "$mode" in
  code|tests|instructions) ;;
  *) die "usage: pick-area.sh <code|tests|instructions> [--check]" ;;
esac
check="${2:-}"
case "$check" in
  ''|--check) ;;
  *) die "unknown option: $check" ;;
esac

command -v jq >/dev/null || die "jq not found"
command -v gh >/dev/null || die "gh not found"

top=$(git rev-parse --show-toplevel 2>/dev/null) || die "not inside a git checkout"
cd "$top" || die "cannot cd to $top"

tmp=$(mktemp -d) || die "mktemp failed"
trap 'rm -rf "$tmp"' EXIT

if [ "$check" = --check ]; then
  base=$(git merge-base origin/main HEAD) || die "no merge-base with origin/main; run git fetch origin"
  { git diff --no-renames --name-only "$base"
    git ls-files --others --exclude-standard; } | sort -u >"$tmp/changed"
  case "$mode" in
    code)
      grep -vE '^(src|desktop/src-tauri/src|desktop/src|xtask/[^/]+/src)/' "$tmp/changed" >"$tmp/out"
      grep -E '\.test\.tsx?$|\.snap$' "$tmp/changed" >>"$tmp/out"
      ;;
    tests)
      grep -vE '^(tests|xtask/[^/]+/tests|desktop/src-tauri/tests|desktop/driver|desktop/e2e)/|^desktop/src/.*\.test\.tsx?$|^\.config/nextest\.toml$' \
        "$tmp/changed" >"$tmp/out"
      grep -E '\.snap$' "$tmp/changed" | sed 's/^/SNAPSHOT=/'
      ;;
    instructions)
      grep -vE '^CLAUDE\.md$|^docs/develop/|^\.claude/skills/[^/]+/[^/]+\.md$' "$tmp/changed" >"$tmp/out"
      grep -E '^\.claude/skills/dot-ai-' "$tmp/changed" >>"$tmp/out"
      ;;
  esac
  sort -u "$tmp/out" | sed 's/^/OUTSIDE=/' | tee "$tmp/report"
  echo "CHANGED=$(wc -l <"$tmp/changed")"
  [ -s "$tmp/report" ] && exit 1
  exit 0
fi

# 1. The owned set, as tracked files.
case "$mode" in
  code)
    git ls-files -- src desktop/src-tauri/src desktop/src 'xtask/*/src' \
      | grep -E '\.(rs|ts|tsx|css)$' \
      | grep -vE '\.test\.tsx?$' >"$tmp/owned"
    ;;
  tests)
    git ls-files -- tests 'xtask/*/tests' desktop/src-tauri/tests desktop/driver desktop/e2e desktop/src \
      | grep -E '(^(tests|xtask/[^/]+/tests|desktop/src-tauri/tests)/.*\.rs$)|(^desktop/(driver|e2e)/.*\.tsx?$)|(^desktop/src/.*\.test\.tsx?$)' \
      >"$tmp/owned"
    ;;
  instructions)
    git ls-files -- CLAUDE.md .claude/skills docs/develop \
      | grep -E '\.md$' \
      | grep -vE '^\.claude/skills/dot-ai-' >"$tmp/owned"
    ;;
esac
[ -s "$tmp/owned" ] || die "the $mode owned set is empty; has the layout moved?"

# 2. Busy files: open PRs ...
repo=$(gh repo view --json nameWithOwner --jq .nameWithOwner) || die "gh repo view failed"
gh pr list --repo "$repo" --state open --limit 300 --json number --jq '.[].number' >"$tmp/open" \
  || die "gh pr list failed"
[ "$(wc -l <"$tmp/open")" -lt 300 ] || die "300 or more open PRs; raise the limit before trusting the busy filter"
: >"$tmp/busy"
while read -r n; do
  [ -n "$n" ] || continue
  gh api "repos/$repo/pulls/$n/files" --paginate --jq '.[].filename' >>"$tmp/busy" \
    || die "cannot read the files of PR #$n"
done <"$tmp/open"

# ... and running dispatch units on this machine.
git worktree list --porcelain | awk '
  /^worktree /{wt=substr($0,10)}
  /^branch refs\/heads\/agent\/dispatch-/{print wt "\t" substr($0,19)}' \
  | while IFS=$'\t' read -r wt branch; do
      [ "$wt" = "$top" ] && continue
      git diff --name-only "origin/main...$branch" 2>/dev/null
      git -C "$wt" status --porcelain --untracked-files=no 2>/dev/null | cut -c4- | sed 's/.* -> //'
    done >>"$tmp/busy"

# 3. Recent cleanup PRs.
: >"$tmp/recent"
gh pr list --repo "$repo" --label cleanup --state all --limit 20 --json number --jq '.[].number' \
  2>/dev/null | while read -r n; do
    [ -n "$n" ] || continue
    gh api "repos/$repo/pulls/$n/files" --paginate --jq '.[].filename' 2>/dev/null
  done >"$tmp/recent"

sort -u "$tmp/busy" -o "$tmp/busy"
sort -u "$tmp/recent" -o "$tmp/recent"
grep -vxFf "$tmp/busy" "$tmp/owned" >"$tmp/free1" || true
grep -vxFf "$tmp/recent" "$tmp/free1" >"$tmp/free" || true

skipped_busy=$(( $(wc -l <"$tmp/owned") - $(wc -l <"$tmp/free1") ))
skipped_recent=$(( $(wc -l <"$tmp/free1") - $(wc -l <"$tmp/free") ))
[ -s "$tmp/free" ] || die "every owned file is busy or recently cleaned (busy $skipped_busy, recent $skipped_recent)"

# Weighted draw: sizes in bytes, one random offset across their sum.
while IFS= read -r f; do
  printf '%s\t%s\n' "$(wc -c <"$f")" "$f"
done <"$tmp/free" >"$tmp/sized"

seed=$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')
pick=$(awk -F'\t' -v seed="$seed" '
  BEGIN { srand(seed) }
  { size[NR] = $1; name[NR] = $2; total += $1 }
  END {
    if (total == 0) exit 1
    r = int(rand() * total)
    for (i = 1; i <= NR; i++) {
      if (r < size[i]) { print r "\t" name[i]; exit }
      r -= size[i]
    }
  }' "$tmp/sized") || die "the free files total zero bytes"

offset=${pick%%$'\t'*}
file=${pick#*$'\t'}
line=$(( $(head -c "$offset" "$file" | wc -l) + 1 ))

echo "MODE=$mode"
echo "CANDIDATES=$(wc -l <"$tmp/free")"
echo "SKIPPED_BUSY=$skipped_busy"
echo "SKIPPED_RECENT=$skipped_recent"
echo "FILE=$file"
echo "LINE=$line"
