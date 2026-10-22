#!/usr/bin/env bash
#
# The frequency guard for /code-cleanup: which modes are due a unit, given when
# each mode's last cleanup PR merged.
#
# Usage: due-modes.sh [--window-hours <n>] [<mode>...]
#
# With no modes it judges all three. Prints, per mode in the order given:
#
#   DUE=<mode>
#   SKIP=<mode><TAB>#<PR number><TAB><merged at, UTC>
#
# A mode is skipped when a `cleanup`-labelled PR from one of its units merged
# within the window (default 24 hours, the default SKILL.md states). A PR is
# attributed to a mode by its head branch, `agent/dispatch-cleanup-<mode>-...`,
# the name SKILL.md step 3 gives every unit; a `cleanup` PR on any other branch
# names no mode and is ignored. GitHub is asked only for the cleanup PRs merged
# within the window, up to 200 of them, so the answer does not depend on the
# order in which older PRs were created.
#
# A failed GitHub lookup stops with ERROR= and a non-zero exit rather than
# reporting every mode as due.

set -uo pipefail

die() { echo "ERROR=$*"; exit 1; }

window=24
if [ "${1:-}" = --window-hours ]; then
  [ $# -ge 2 ] || die "--window-hours needs a number"
  window="$2"
  shift 2
fi
printf '%s\n' "$window" | grep -qE '^[0-9]+$' || die "--window-hours takes whole hours, not: $window"

modes=("$@")
[ ${#modes[@]} -gt 0 ] || modes=(code tests instructions)
for m in "${modes[@]}"; do
  case "$m" in
    code|tests|instructions) ;;
    *) die "unknown mode: $m (code, tests or instructions)" ;;
  esac
done

command -v jq >/dev/null || die "jq not found"
command -v gh >/dev/null || die "gh not found"

tmp=$(mktemp) || die "mktemp failed"
trap 'rm -f "$tmp"' EXIT

repo=$(gh repo view --json nameWithOwner --jq .nameWithOwner) || die "gh repo view failed"
since=$(jq -nr --argjson h "$window" '(now - $h * 3600) | floor | todate') || die "cannot compute the window start"
gh pr list --repo "$repo" --label cleanup --state merged --search "merged:>=$since" --limit 200 \
  --json number,headRefName,mergedAt >"$tmp" || die "cannot list merged cleanup PRs"
jq -e 'type == "array"' "$tmp" >/dev/null 2>&1 || die "gh pr list returned something other than a list"

for m in "${modes[@]}"; do
  last=$(jq -r --arg m "$m" '
    [ .[] | select(.mergedAt != null)
          | select(.headRefName | startswith("agent/dispatch-cleanup-" + $m + "-")) ]
    | sort_by(.mergedAt) | last // empty
    | "\(.number)\t\(.mergedAt)\t\(.mergedAt | fromdateiso8601)"' "$tmp") \
    || die "cannot read the merge times"
  if [ -n "$last" ]; then
    IFS=$'\t' read -r number merged_at merged_epoch <<<"$last"
    if [ $(( $(date +%s) - merged_epoch )) -lt $(( window * 3600 )) ]; then
      printf 'SKIP=%s\t#%s\t%s\n' "$m" "$number" "$merged_at"
      continue
    fi
  fi
  echo "DUE=$m"
done
