#!/usr/bin/env bash
# Did one attempt of a CI run fail only because GitHub never gave a job a runner?
#
# On 2026-11-16 GitHub's hosted macOS arm64 pool ran short. In four `ci.yml`
# runs (three `main` pushes and a dispatch), `build-macos-tests` and
# `build-macos-clippy` jobs sat queued for exactly 15 minutes and were then
# cancelled with the annotation "The job was not acquired by Runner of type
# hosted even after multiple attempts". Their aggregate `build-macos` went red,
# `main` read red, `notify-main-red` sent its alert, and the agent reviewer
# skipped a pull request whose dispatched run carried the red check. Not one
# line of the change had been tested. A plain `gh run rerun --failed` turned each
# of those runs green once runners were free.
#
# This script tells that case apart from every other red, so that
# `.github/workflows/rerun-unacquired.yml` can rerun it and `notify-main-red`
# can hold its alert for the attempt that will follow. It answers
# UNACQUIRED_ONLY only when ALL of these hold for the attempt's completed jobs:
#
#   * at least one job is cancelled, never had a runner, and carries GitHub's
#     "not acquired" annotation;
#   * every other job that did not pass or skip is one of the aggregates below,
#     which fail whenever a job they need fails and run no code of their own.
#
# Any other failed or cancelled job (a test that failed, a job cancelled after
# a runner took it, a timeout) makes it answer OTHER, so a real red is never
# rerun away or kept quiet. If GitHub rewords the annotation, the answer becomes
# OTHER for these runs too: the failure mode is the old behaviour (a red run, an
# alert, a rerun by hand), never a hidden red.
#
# Jobs still in progress are ignored, which is what lets `notify-main-red` ask
# about its own attempt while it runs.
#
# Usage:  scripts/ci-unacquired-runners.sh <run-id> <attempt>
# Env:    GH_TOKEN (actions: read and checks: read), GITHUB_REPOSITORY
#         (default vfarcic/dot-agent-deck)
# Prints one line, `UNACQUIRED_ONLY <jobs>` or `OTHER <jobs>`.
# Exit:   0 UNACQUIRED_ONLY, 1 OTHER, 2 the jobs or an annotation could not be
#         read (treat as OTHER: no rerun, alert as usual).

set -euo pipefail

# The aggregate jobs in ci.yml: each one only reads `needs` and fails when one
# of them did not pass. Keep in step with ci.yml.
AGGREGATES='["build","build-macos","build-windows","e2e-deterministic"]'

# GitHub's annotation on a job it gave up queueing.
NOT_ACQUIRED='was not acquired by Runner'

run="${1:?usage: ci-unacquired-runners.sh <run-id> <attempt>}"
attempt="${2:?usage: ci-unacquired-runners.sh <run-id> <attempt>}"
[[ "$run" =~ ^[0-9]+$ && "$attempt" =~ ^[0-9]+$ ]] || { echo "OTHER run id and attempt must be numbers" ; exit 2; }
repo="${GITHUB_REPOSITORY:-vfarcic/dot-agent-deck}"

jobs="$(gh api --paginate "repos/$repo/actions/runs/$run/attempts/$attempt/jobs?per_page=100" --jq '.jobs[]' | jq -s .)" \
  || { echo "OTHER could not read the jobs of run $run attempt $attempt"; exit 2; }

# The completed jobs that neither passed nor skipped, minus the aggregates.
bad="$(jq -c --argjson agg "$AGGREGATES" '
  [ .[] | select(.status == "completed")
        | select(.conclusion != "success" and .conclusion != "skipped" and .conclusion != "neutral")
        | select(.name as $n | $agg | index($n) | not) ]' <<<"$jobs")"

if [ "$(jq length <<<"$bad")" -eq 0 ]; then
  echo "OTHER no failed job other than an aggregate"
  exit 1
fi

unacquired=()
other=()
# Separated by US (0x1f), not a tab: bash folds runs of a whitespace IFS, so the
# empty runner name of exactly the job this looks for would vanish.
while IFS=$'\x1f' read -r name conclusion runner check_url; do
  if [ "$conclusion" = cancelled ] && [ -z "$runner" ]; then
    notes="$(gh api "${check_url}/annotations" --jq '.[].message')" \
      || { echo "OTHER could not read the annotations of $name"; exit 2; }
    if grep -qF "$NOT_ACQUIRED" <<<"$notes"; then
      unacquired+=("$name")
      continue
    fi
  fi
  other+=("$name=$conclusion")
done < <(jq -r '.[] | [.name, .conclusion, (.runner_name // ""), .check_run_url] | join("\u001f")' <<<"$bad")

if [ "${#other[@]}" -eq 0 ]; then
  ( IFS=,; echo "UNACQUIRED_ONLY ${unacquired[*]}" )
  exit 0
fi
( IFS=,; echo "OTHER ${other[*]}" )
exit 1
