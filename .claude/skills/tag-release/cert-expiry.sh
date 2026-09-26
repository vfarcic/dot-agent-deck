#!/usr/bin/env bash
set -uo pipefail

# Report what the last release run's `desktop-sign` job said about the Developer
# ID Application certificate, so a maintainer cutting the next release sees it
# (issue #1326). The job warns from 30 days before expiry and fails from 24
# hours before, but the warning lands only in a job log and an annotation on a
# green run, which nobody opens.
#
# ADVISORY and READ-ONLY. It reads run metadata, check-run annotations and one
# job log through `gh`; it reads no secret and changes nothing. It always exits
# 0 -- a failure to look is reported as CERT_CHECK=unknown, never as a refusal,
# because nothing about cutting a release should depend on it. Unlike
# analyze.sh it is NOT run by tag-release.yml, and must not be: that job holds
# the RELEASE_TOKEN admin PAT.
#
# Output:
#   CERT_CHECK=warning|clear|unknown
#   CERT_RUN=<url of the release run it read>
#   CERT_PASSED_OVER=<newer runs whose desktop-sign failed before the check>
#   CERT_NOT_AFTER=<the certificate's notAfter, when the log carries it>
#   CERT_MESSAGE=<level: message>   (one line per certificate annotation)

unknown() {
  echo "CERT_CHECK=unknown"
  echo "CERT_MESSAGE=$1"
  exit 0
}

command -v gh > /dev/null 2>&1 || unknown "gh is not on PATH, so the last release run could not be read."
repo=$(gh repo view --json nameWithOwner --jq .nameWithOwner 2> /dev/null) || unknown "gh could not resolve this repository."

# The newest completed release run whose desktop-sign job says something about
# the certificate. A run dispatched with skip_desktop, or one that failed before
# the desktop chain, has no such job; a job that FAILED before the certificate
# check (no app artifact, half-registered credentials) says nothing about the
# certificate either, so the search goes on past it rather than reporting it
# as clear and hiding what an older run said. A job that SUCCEEDED with no
# certificate line built the .dmg unsigned, which is a real answer.
runs=$(gh run list --repo "$repo" --workflow=release.yml --status completed --limit 10 \
  --json databaseId --jq '.[].databaseId' 2> /dev/null) || unknown "gh could not list release.yml runs."
passed_over=""
for run in $runs; do
  job="" conclusion=""
  read -r job conclusion < <(gh api "repos/$repo/actions/runs/$run/jobs?per_page=100" \
    --jq '.jobs[] | select(.name == "desktop-sign" and (.conclusion == "success" or .conclusion == "failure")) | "\(.id) \(.conclusion)"' 2> /dev/null | head -n 1)
  [ -n "${job:-}" ] || continue
  url="https://github.com/$repo/actions/runs/$run"
  # The job logs the date on every signed run that reached the check.
  log=$(gh run view "$run" --repo "$repo" --job "$job" --log 2> /dev/null) \
    || unknown "gh could not read the desktop-sign log of $url."
  not_after=$(printf '%s\n' "$log" | grep -oE 'notAfter=[A-Z][a-z]{2} +[0-9]+ [0-9:]+ [0-9]{4} GMT' | head -n 1)
  annotations=$(gh api "repos/$repo/check-runs/$job/annotations" \
    --jq '.[] | select(.message | contains("Developer ID Application certificate")) | "\(.annotation_level): \(.message)"' 2> /dev/null) \
    || unknown "gh could not read the desktop-sign annotations of $url."
  if [ -z "$not_after" ] && [ -z "$annotations" ] && [ "$conclusion" != "success" ]; then
    passed_over="$passed_over $url"
    continue
  fi
  echo "CERT_RUN=$url"
  [ -n "$passed_over" ] && echo "CERT_PASSED_OVER=${passed_over# }"
  [ -n "$not_after" ] && echo "CERT_NOT_AFTER=${not_after#notAfter=}"
  if [ -n "$annotations" ]; then
    echo "CERT_CHECK=warning"
    while IFS= read -r line; do
      echo "CERT_MESSAGE=$line"
    done <<< "$annotations"
  else
    echo "CERT_CHECK=clear"
  fi
  exit 0
done
unknown "none of the last 10 completed release.yml runs has a desktop-sign job that reached the certificate check or built unsigned.${passed_over:+ Failed before the check:$passed_over}"
