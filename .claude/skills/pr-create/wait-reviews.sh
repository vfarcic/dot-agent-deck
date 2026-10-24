#!/usr/bin/env bash
#
# Wait, within a bound, for every automated review app in reviewers.json to
# finish reviewing a pull request's current head (issue #1256).
#
# Usage: wait-reviews.sh [--no-wait] <pr-number>
#
# Run it from inside the repository checkout: `gh` resolves {owner}/{repo} from
# the git remote. It prints one KEY=value record per line:
#
#   HEAD=<sha>                 the head commit it waited on
#   DONE=<name> <url>          that reviewer has finished; <url> is its evidence
#   TIMED_OUT=<name> <n>m      no finished review within that reviewer's bound
#   PENDING=<name>             --no-wait only: not finished yet, bound not spent
#   REVIEWS=complete|incomplete
#
# REVIEWS=complete means every configured reviewer finished. `incomplete` is a
# result, not a failure: an out-of-quota or uninstalled app produces nothing at
# all, so the caller proceeds and reports which review it did not get (CLAUDE.md
# rule 8: bound the wait, never block a merge on it). Finished means finished
# REVIEWING; the findings are still read from the inline comments
# (`gh api repos/{owner}/{repo}/pulls/<n>/comments --paginate`), which this
# script does not do.
#
# Each reviewer's bound is counted from the later of the PR's creation and the
# head commit's committer date, so calling the script again after a wait that
# already ran does not start the clock over. A GitHub read that fails is retried
# on the next poll; the bound still applies.
#
# Exit status is 0 whenever REVIEWS= was printed, and 2 with ERROR= on bad usage,
# a malformed reviewers.json, or a pull request that cannot be read at all.
#
# REVIEWERS_FILE overrides the registry path and REVIEW_WAIT_POLL_SECS the poll
# interval (default 30); both exist for the tests in
# xtask/linkage-check/src/review_wait.rs.

set -uo pipefail

die() { echo "ERROR=$*"; exit 2; }

wait_for_it=yes
if [ "${1:-}" = --no-wait ]; then
  wait_for_it=no
  shift
fi
[ $# -eq 1 ] || die "usage: wait-reviews.sh [--no-wait] <pr-number>"
pr="$1"
printf '%s\n' "$pr" | grep -qE '^[0-9]+$' || die "not a pull request number: $pr"

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd) || die "cannot find the script's directory"
registry="${REVIEWERS_FILE:-$here/reviewers.json}"
poll="${REVIEW_WAIT_POLL_SECS:-30}"
printf '%s\n' "$poll" | grep -qE '^[0-9]+$' || die "REVIEW_WAIT_POLL_SECS takes whole seconds, not: $poll"

command -v jq >/dev/null || die "jq not found"
command -v gh >/dev/null || die "gh not found"

[ -r "$registry" ] || die "cannot read $registry"
jq -e '.reviewers | type == "array" and length > 0' "$registry" >/dev/null 2>&1 ||
  die "$registry has no reviewers list"
bad=$(jq -r '
  .reviewers[]
  | select(
      (.name | type) != "string" or (.login | type) != "string"
      or ((.timeout_minutes | type) != "number") or .timeout_minutes <= 0
      or (.signal.kind as $k | ["comment", "check_run", "review"] | index($k) | not)
      or (.signal.kind == "comment" and ((.signal.contains | type) != "string" or .signal.contains == ""))
      or (.signal.kind == "check_run" and ((.signal.name | type) != "string" or .signal.name == ""))
    )
  | (.name // "an unnamed entry")' "$registry" 2>/dev/null) || die "$registry is not valid JSON"
[ -z "$bad" ] || die "malformed reviewer in $registry: $(printf '%s' "$bad" | head -1)"

tmp=$(mktemp -d) || die "mktemp failed"
trap 'rm -rf "$tmp"' EXIT

gh pr view "$pr" --json headRefOid,createdAt,commits >"$tmp/pr.json" 2>/dev/null ||
  die "cannot read pull request #$pr"
head=$(jq -r '.headRefOid // empty' "$tmp/pr.json")
[ -n "$head" ] || die "pull request #$pr has no head commit"
start=$(jq -r '[.createdAt, (.commits[-1].committedDate // .createdAt)] | map(fromdateiso8601) | max' "$tmp/pr.json" 2>/dev/null) ||
  die "cannot read the PR's creation or head commit time"
echo "HEAD=$head"

count=$(jq '.reviewers | length' "$registry")
pending=$(seq 0 $((count - 1)) | tr '\n' ' ')
timed_out=0

# Read one GitHub surface into $tmp/<name>.json as a flat array. A failed read
# leaves an empty array, so that reviewer stays unfinished until the next poll.
fetch() {
  local name="$1" flatten="$2"
  shift 2
  if gh "$@" >"$tmp/$name.raw" 2>/dev/null &&
    jq -s "$flatten" "$tmp/$name.raw" >"$tmp/$name.json" 2>/dev/null; then
    return
  fi
  echo "wait-reviews: reading $name failed; retrying on the next poll" >&2
  echo '[]' >"$tmp/$name.json"
}

while :; do
  kinds=$(for i in $pending; do jq -r --argjson i "$i" '.reviewers[$i].signal.kind' "$registry"; done | sort -u)
  echo '[]' >"$tmp/comments.json"
  echo '[]' >"$tmp/checks.json"
  echo '[]' >"$tmp/reviews.json"
  case "$kinds" in *comment*) fetch comments 'add // []' api "repos/{owner}/{repo}/issues/$pr/comments" --paginate ;; esac
  case "$kinds" in *check_run*) fetch checks '[.[].check_runs[]]' api "repos/{owner}/{repo}/commits/$head/check-runs?per_page=100" --paginate ;; esac
  case "$kinds" in *review*) fetch reviews 'add // []' api "repos/{owner}/{repo}/pulls/$pr/reviews" --paginate ;; esac

  now=$(date +%s)
  still=""
  for i in $pending; do
    name=$(jq -r --argjson i "$i" '.reviewers[$i].name' "$registry")
    evidence=$(jq -r -n --argjson i "$i" --arg head "$head" \
      --slurpfile reg "$registry" --slurpfile c "$tmp/comments.json" \
      --slurpfile k "$tmp/checks.json" --slurpfile v "$tmp/reviews.json" '
      ($reg[0].reviewers[$i]) as $r
      | ($r.signal) as $s
      | (if $s.kind == "comment" then
           [ $c[0][]
             | select(.user.login == $r.login)
             | select((.body // "") | contains($s.contains))
             | select(($s.names_head // false | not) or ((.body // "") | contains($head))) ]
         elif $s.kind == "check_run" then
           [ $k[0][] | select(.name == $s.name and .status == "completed") ]
         else
           [ $v[0][] | select(.user.login == $r.login and .state != "PENDING" and .commit_id == $head) ]
         end)
      | if length == 0 then "" else (.[0].html_url // "(no url)") end')
    if [ -n "$evidence" ]; then
      echo "DONE=$name $evidence"
      continue
    fi
    minutes=$(jq -r --argjson i "$i" '.reviewers[$i].timeout_minutes' "$registry")
    deadline=$(jq -n --argjson s "$start" --argjson m "$minutes" '$s + $m * 60 | floor')
    if [ "$now" -ge "$deadline" ]; then
      echo "TIMED_OUT=$name ${minutes}m"
      timed_out=1
    elif [ "$wait_for_it" = no ]; then
      echo "PENDING=$name"
      timed_out=1
    else
      still="$still $i"
    fi
  done
  pending="$still"
  [ -n "${pending// /}" ] || break
  sleep "$poll"
done

if [ "$timed_out" -eq 0 ]; then
  echo "REVIEWS=complete"
else
  echo "REVIEWS=incomplete"
fi
