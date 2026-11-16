#!/usr/bin/env bash
# Download the per-test JUnit files of one RWX run of `.rwx/ci.yml` (issue
# #2101).
#
#   scripts/rwx-junit.sh <run-id> [<out-dir>]
#
# Writes `<out-dir>/<task-key>/junit.xml` for every task of the run that
# reported tests and kept a `junit.xml` artifact (`test`, `e2e-1` … `e2e-4`,
# `e2e-version`, `desktop-web`, `desktop-browser`, `desktop-driver`), and prints
# one line per such task: `<task-key> <state>`, where the state is
# `downloaded`, `missing` (the task reported tests but kept no `junit.xml`:
# a run from before #2101 merged, or a task that died before its trap ran) or
# `failed` (the artifact lookup or the download failed; RWX's message goes to
# stderr). A task's earlier `junit.xml` in `<out-dir>` is removed first, so a
# file left there is always this call's. `<out-dir>` defaults to
# `rwx-junit-<run-id>`. Exits non-zero when any task is not `downloaded`.
#
# The run id is the last segment of the `RWX: linux` link `gh pr checks <n>`
# prints. Only the latest attempt of each task is read (a nextest retry inside
# that attempt is in its file, as `<flakyFailure>`).
#
# Why an artifact and not `rwx results`: RWX parses each task's
# `test-results` output into its UI and the `TestCount` / `FailedTestCount`
# fields of `rwx results --json`, but with CLI v3.36.1 neither that command,
# `rwx artifacts` nor the `rwx mcp serve` tools (only `get_run_test_failures`)
# returned the parsed tests, so `.rwx/ci.yml` copies each file into
# `$RWX_ARTIFACTS` as well (docs/develop/rwx.md, "Per-test results").
set -euo pipefail

if [[ $# -lt 1 || $# -gt 2 ]]; then
  echo "usage: $0 <run-id> [<out-dir>]" >&2
  exit 2
fi
run=$1
out=${2:-rwx-junit-$run}

results=$(rwx results "$run" --json 2>/dev/null) || {
  echo "rwx results $run failed; check the run id and \`rwx whoami\`" >&2
  exit 1
}

rows=$(jq -r '
  .Tasks[] | recurse(.Subtasks[]?)
  | select(.TaskType == "command" and (.TestCount // 0) > 0)
  | [.Key, .ID] | @tsv' <<<"$results")

if [[ -z $rows ]]; then
  echo "run $run has no task that reported tests" >&2
  exit 1
fi

errfile=$(mktemp)
trap 'rm -f "$errfile"' EXIT

status=0
while IFS=$'\t' read -r key id; do
  dest="$out/$key/junit.xml"
  # A file from an earlier call for this run must not pass for this one's.
  rm -f "$dest"
  # A task with no artifact at all answers `Artifacts for task … not found`
  # and exits non-zero; any other failure of the lookup is `failed`, not
  # `missing`, with RWX's own message on stderr.
  # stderr apart from the JSON: the CLI also prints notices there, such as
  # "A new release of rwx is available".
  if ! listing=$(rwx artifacts list "$id" --json 2>"$errfile"); then
    if grep -q 'not found' "$errfile"; then
      echo "$key missing"
    else
      echo "$key failed"
      printf '%s: rwx artifacts list %s: %s\n' "$key" "$id" "$(cat "$errfile")" >&2
    fi
    status=1
    continue
  fi
  if ! has=$(jq -r '[.Artifacts[]? | select(.Key == "junit.xml")] | length' <<<"$listing" 2>/dev/null); then
    echo "$key failed"
    printf '%s: unreadable artifact listing: %s\n' "$key" "$listing" >&2
    status=1
    continue
  fi
  if [[ $has == 0 ]]; then
    echo "$key missing"
    status=1
    continue
  fi
  mkdir -p "$out/$key"
  if err=$(rwx artifacts download "$id" junit.xml --output "$dest.part" 2>&1 >/dev/null) \
      && mv "$dest.part" "$dest"; then
    echo "$key downloaded"
  else
    rm -f "$dest.part"
    echo "$key failed"
    printf '%s: rwx artifacts download %s: %s\n' "$key" "$id" "$err" >&2
    status=1
  fi
done <<<"$rows"

exit "$status"
