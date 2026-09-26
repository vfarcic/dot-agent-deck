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
#   CERT_NOT_AFTER=<the certificate's notAfter, when the log carries it>
#   CERT_MESSAGE=<level: message>   (one line per certificate annotation)

unknown() {
  echo "CERT_CHECK=unknown"
  echo "CERT_MESSAGE=$1"
  exit 0
}

command -v gh > /dev/null 2>&1 || unknown "gh is not on PATH, so the last release run could not be read."
repo=$(gh repo view --json nameWithOwner --jq .nameWithOwner 2> /dev/null) || unknown "gh could not resolve this repository."

# The newest completed release run whose desktop-sign job actually ran. A run
# dispatched with skip_desktop, or one that failed before the desktop chain,
# has none, so a few are searched.
runs=$(gh run list --repo "$repo" --workflow=release.yml --status completed --limit 10 \
  --json databaseId --jq '.[].databaseId' 2> /dev/null) || unknown "gh could not list release.yml runs."
job=""
for run in $runs; do
  job=$(gh api "repos/$repo/actions/runs/$run/jobs?per_page=100" \
    --jq '.jobs[] | select(.name == "desktop-sign" and (.conclusion == "success" or .conclusion == "failure")) | .id' 2> /dev/null | head -n 1)
  [ -n "$job" ] && break
done
[ -n "$job" ] || unknown "none of the last 10 completed release.yml runs has a desktop-sign job that ran."
echo "CERT_RUN=https://github.com/$repo/actions/runs/$run"

# The job logs the date on every signed run; an unsigned run has no line.
not_after=$(gh run view "$run" --repo "$repo" --job "$job" --log 2> /dev/null \
  | grep -oE 'notAfter=[A-Z][a-z]{2} +[0-9]+ [0-9:]+ [0-9]{4} GMT' | head -n 1)
[ -n "$not_after" ] && echo "CERT_NOT_AFTER=${not_after#notAfter=}"

annotations=$(gh api "repos/$repo/check-runs/$job/annotations" \
  --jq '.[] | select(.message | contains("Developer ID Application certificate")) | "\(.annotation_level): \(.message)"' 2> /dev/null) \
  || unknown "gh could not read the desktop-sign job's annotations."
if [ -n "$annotations" ]; then
  echo "CERT_CHECK=warning"
  while IFS= read -r line; do
    echo "CERT_MESSAGE=$line"
  done <<< "$annotations"
else
  echo "CERT_CHECK=clear"
fi
