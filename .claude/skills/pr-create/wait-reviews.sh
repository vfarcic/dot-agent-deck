#!/usr/bin/env bash
#
# Wait, within a bound, for every automated review app in reviewers.json to
# finish reviewing a pull request's current head (issue #1256).
#
# Usage: wait-reviews.sh [--no-wait] <pr-number>
#
# Run it from inside the repository checkout: `gh` resolves {owner}/{repo} from
# the git remote. It prints one KEY=value record per line, once it has decided:
#
#   HEAD=<sha>                 the head commit the verdict is about
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
# Two kinds of reviewer, told apart by their signal:
#
#   - one that reviews EVERY PUSH: a `comment` signal with `names_head`, or a
#     `check_run` or `review` signal, all of which are tied to the head. Its
#     bound starts when this script starts, and again whenever the head moves,
#     because a push is what it reviews. A commit's own date says nothing about
#     when it was pushed or when a review was asked for.
#   - one that reviews ONCE, when the PR opens: a `comment` signal without
#     `names_head`. Its bound counts from the PR's creation, so a later push
#     never buys it another wait; there is no review left to come.
#
# A `comment` signal with `settle_minutes` is for an app that names the head in
# its comment BEFORE it has finished that head's review (Qodo, issue #2507: 130
# of its 340 inline findings on 150 PRs were posted 47 to 714 s after its
# comment first named their head). Naming the head is then necessary but not
# enough; the comment counts once one of these also holds:
#
#   - the app has posted an inline review comment, not a reply, written on the
#     head (`original_commit_id`): on every head measured, its findings for
#     the head arrived in one batch, so once one is there the rest are;
#   - the comment was never edited and is `first_review_settle_minutes` old: the
#     app's first review on the PR, which it posts in one step;
#   - the comment's last edit is `settle_minutes` old.
#
# reviewers.json's note on the entry has the measurements behind those numbers.
#
# The head is read again on every poll and once more before the verdict. When
# it moves, the reviewers that review every push start over on the new head.
# A GitHub read that fails after the first one is retried on the next poll;
# the bound still applies. The read before the verdict is retried up to three
# times, and if it still fails the script stops with ERROR= rather than give a
# verdict about a head it could not confirm (Qodo's finding on PR #1882).
#
# Exit status is 0 whenever REVIEWS= was printed, and 2 with ERROR= on bad usage,
# a malformed reviewers.json, or a pull request that cannot be read.
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
      or (.signal.settle_minutes != null and (.signal.kind != "comment" or .signal.names_head != true
                                              or (.signal.settle_minutes | type) != "number" or .signal.settle_minutes <= 0
                                              or (.signal.first_review_settle_minutes | type) != "number"
                                              or .signal.first_review_settle_minutes < 0))
      or (.signal.settle_minutes == null and .signal.first_review_settle_minutes != null)
      or (.signal.kind == "check_run" and ((.signal.name | type) != "string" or .signal.name == ""
                                           or (.signal.app | type) != "string" or .signal.app == ""))
    )
  | (.name // "an unnamed entry")' "$registry" 2>/dev/null) || die "$registry is not valid JSON"
[ -z "$bad" ] || die "malformed reviewer in $registry: $(printf '%s' "$bad" | head -1)"

tmp=$(mktemp -d) || die "mktemp failed"
trap 'rm -rf "$tmp"' EXIT

count=$(jq '.reviewers | length' "$registry")
names=()
once=()
minutes=()
for ((i = 0; i < count; i++)); do
  names[i]=$(jq -r --argjson i "$i" '.reviewers[$i].name' "$registry")
  once[i]=$(jq -r --argjson i "$i" '.reviewers[$i].signal | if .kind == "comment" and ((.names_head // false) | not) then "yes" else "no" end' "$registry")
  minutes[i]=$(jq -r --argjson i "$i" '.reviewers[$i].timeout_minutes' "$registry")
done

# Read the PR's head and creation time. Only the first read may end the script.
read_pr() {
  if gh pr view "$pr" --json headRefOid,createdAt >"$tmp/pr.json" 2>/dev/null &&
    new_head=$(jq -re '.headRefOid' "$tmp/pr.json" 2>/dev/null) &&
    new_created=$(jq -re '.createdAt | fromdateiso8601' "$tmp/pr.json" 2>/dev/null); then
    return 0
  fi
  return 1
}

# Read the PR again before a verdict, retrying a failed read; 1 when every
# attempt failed.
confirm_pr() {
  local tries=1
  until read_pr; do
    [ "$tries" -lt 3 ] || return 1
    tries=$((tries + 1))
    sleep "$poll"
  done
}

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

# Inline comments are read only from the head commit's committer date on, so a
# poll reads what changed since the head was made rather than the PR's whole
# review history (Qodo's finding on PR #2544). A finding on the head is written
# after the commit, so it falls inside that window unless the committer's
# clock ran ahead; then the settle period still ends the wait. When the date
# cannot be read, every page is read.
since_head=
since_for=
# Sets $since_head for the current head, reading it once per head.
read_since_head() {
  [ "$since_for" = "$head" ] && return
  since_for="$head"
  since_head=$(gh api "repos/{owner}/{repo}/commits/$head" --jq .commit.committer.date 2>/dev/null) || since_head=
  printf '%s\n' "$since_head" | grep -qE '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:]{8}Z$' || since_head=
}

read_pr || die "cannot read pull request #$pr"
head="$new_head"
created="$new_created"
head_since=$(date +%s)
status=()
evidence=()
for ((i = 0; i < count; i++)); do status[i]=pending; done

while :; do
  kinds=$(for ((i = 0; i < count; i++)); do
    [ "${status[i]}" = pending ] && jq -r --argjson i "$i" '.reviewers[$i].signal | .kind, (if .settle_minutes != null then "inline" else empty end)' "$registry"
  done | sort -u)
  echo '[]' >"$tmp/comments.json"
  echo '[]' >"$tmp/checks.json"
  echo '[]' >"$tmp/reviews.json"
  echo '[]' >"$tmp/inline.json"
  case "$kinds" in *comment*) fetch comments 'add // []' api "repos/{owner}/{repo}/issues/$pr/comments" --paginate ;; esac
  case "$kinds" in *inline*)
    read_since_head
    fetch inline 'add // []' api "repos/{owner}/{repo}/pulls/$pr/comments?per_page=100${since_head:+&since=$since_head}" --paginate ;;
  esac
  case "$kinds" in *check_run*) fetch checks '[.[].check_runs[]]' api "repos/{owner}/{repo}/commits/$head/check-runs?per_page=100" --paginate ;; esac
  case "$kinds" in *review*) fetch reviews 'add // []' api "repos/{owner}/{repo}/pulls/$pr/reviews" --paginate ;; esac

  now=$(date +%s)
  waiting=no
  for ((i = 0; i < count; i++)); do
    [ "${status[i]}" = pending ] || continue
    found=$(jq -r -n --argjson i "$i" --arg head "$head" --argjson now "$now" \
      --slurpfile reg "$registry" --slurpfile c "$tmp/comments.json" \
      --slurpfile k "$tmp/checks.json" --slurpfile v "$tmp/reviews.json" \
      --slurpfile p "$tmp/inline.json" '
      def age($t): if ($t | type) == "string" then $now - ($t | fromdateiso8601) else -1 end;
      ($reg[0].reviewers[$i]) as $r
      | ($r.signal) as $s
      | ([ $p[0][] | select(.user.login == $r.login and .in_reply_to_id == null
                            and .original_commit_id == $head) ] | length > 0) as $findings
      | (if $s.kind == "comment" then
           [ $c[0][]
             | select(.user.login == $r.login)
             | select((.body // "") | contains($s.contains))
             | select(($s.names_head // false | not) or ((.body // "") | contains($head)))
             | select($s.settle_minutes == null
                      or $findings
                      or (.created_at == .updated_at
                          and age(.created_at) >= $s.first_review_settle_minutes * 60)
                      or age(.updated_at) >= $s.settle_minutes * 60) ]
         elif $s.kind == "check_run" then
           [ $k[0][] | select(.name == $s.name and .app.slug == $s.app and .status == "completed") ]
         else
           [ $v[0][] | select(.user.login == $r.login and .state != "PENDING" and .commit_id == $head) ]
         end)
      | if length == 0 then "" else (.[0].html_url // "(no url)") end')
    if [ -n "$found" ]; then
      status[i]=done
      evidence[i]="$found"
      continue
    fi
    if [ "${once[i]}" = yes ]; then since="$created"; else since="$head_since"; fi
    deadline=$(jq -n --argjson s "$since" --argjson m "${minutes[i]}" '$s + $m * 60 | floor')
    if [ "$now" -ge "$deadline" ]; then
      status[i]=timed_out
    elif [ "$wait_for_it" = yes ]; then
      waiting=yes
    fi
  done

  if [ "$waiting" = no ]; then
    # About to decide: make sure the verdict is about the head as it is now.
    confirm_pr || die "cannot read pull request #$pr to confirm its head before the verdict"
    [ "$new_head" != "$head" ] || break
  else
    sleep "$poll"
    read_pr || continue
    [ "$new_head" != "$head" ] || continue
  fi

  # The head moved: what reviews every push starts over on the new one.
  echo "wait-reviews: the head moved from $head to $new_head; waiting again for the new head" >&2
  head="$new_head"
  head_since=$(date +%s)
  for ((i = 0; i < count; i++)); do
    [ "${once[i]}" = yes ] && [ "${status[i]}" = done ] && continue
    status[i]=pending
  done
done

echo "HEAD=$head"
incomplete=0
for ((i = 0; i < count; i++)); do
  case "${status[i]}" in
    done) echo "DONE=${names[i]} ${evidence[i]}" ;;
    timed_out) echo "TIMED_OUT=${names[i]} ${minutes[i]}m"; incomplete=1 ;;
    *) echo "PENDING=${names[i]}"; incomplete=1 ;;
  esac
done
if [ "$incomplete" -eq 0 ]; then
  echo "REVIEWS=complete"
else
  echo "REVIEWS=incomplete"
fi
