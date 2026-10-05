#!/usr/bin/env bash
set -uo pipefail

# Say where PRD #<n> lives: in a file, or in its GitHub issue (issue #1591).
#
# Usage:
#   prd-source.sh <number>               file in this checkout first, then the issue
#   prd-source.sh --issue-only <number>  the issue only (the caller already looked for a file)
#   prd-source.sh --check-body           read an issue body on stdin and judge only that
#
# Output is one KEY=value record per line, and no value ever contains a
# newline or another control character: the title and the reason come from
# text anyone can write, so they are flattened before they are printed. Read a
# key with `sed -n 's/^KEY=//p' | head -1`.
#
#   SOURCE=file|issue|none   where the PRD lives; `none` means it is not a PRD yet
#   FILE=prds/<n>-<slug>.md  only when SOURCE=file
#   TITLE=...                the file's first heading, or the issue title
#   STATE=OPEN|CLOSED        the issue's state, when the issue was read
#   PRD_CONTENT=yes|no       whether the issue body carries PRD content
#   AUTHOR_TRUSTED=yes|no    whether the issue was opened by an owner, member or collaborator
#   REASON=...               why SOURCE is `none`, or why PRD_CONTENT is `no`
#
# An issue body "carries PRD content" when, outside fenced code blocks, it has
# a heading starting with "Problem", a heading starting with "Solution", and a
# heading starting with "Milestones" with at least one task-list item
# (`- [ ] …`, `- [x] …`, `- [~] …`, `- [!] …`) before the next heading of the
# same or a higher level. A one-line issue, or the old stub that only pointed
# at a `prds/` file, has none of that and is refused.
#
# An issue PRD also has to have been opened by someone with write access
# (author_association OWNER, MEMBER or COLLABORATOR). The author of an issue
# can rewrite its body at any time, and unlike a `prds/` file the body never
# passes review, so a body a stranger controls is never treated as a PRD.
#
# Exit status is 0 whenever a verdict was printed, including SOURCE=none, and
# non-zero only when the script could not decide (bad usage, `gh` failed).
# `--check-body` is the exception: it exits 0 for PRD content and 1 without.

flatten() { # strip control characters, so a value can never end its own record
  printf '%s' "$1" | tr -d '\000-\037\177'
}

# Judge a body on stdin. Prints PRD_CONTENT=… and, when it is `no`, REASON=….
check_body() {
  awk '
    BEGIN { fence = 0; problem = 0; solution = 0; milestones = 0; inm = 0; mlevel = 0; boxes = 0 }
    /^ *(```|~~~)/ { fence = !fence; next }
    fence { next }
    /^ *#+[ \t]/ {
      line = $0
      sub(/^ */, "", line)
      match(line, /^#+/)
      level = RLENGTH
      text = substr(line, level + 1)
      sub(/^[ \t]+/, "", text)
      text = tolower(text)
      if (inm && level <= mlevel) inm = 0
      if (text ~ /^problem/) problem = 1
      if (text ~ /^solution/) solution = 1
      if (text ~ /^milestones/) { milestones = 1; inm = 1; mlevel = level }
      next
    }
    inm && /^[ \t]*[-*+] \[[ xX~!]\][ \t]+[^ \t]/ { boxes++ }
    END {
      missing = ""
      if (!problem) missing = missing " Problem"
      if (!solution) missing = missing " Solution"
      if (!milestones) missing = missing " Milestones"
      if (missing != "") {
        print "PRD_CONTENT=no"
        print "REASON=no heading for:" missing
        exit 1
      }
      if (boxes == 0) {
        print "PRD_CONTENT=no"
        print "REASON=the Milestones section has no task-list item"
        exit 1
      }
      print "PRD_CONTENT=yes"
      exit 0
    }
  '
}

usage() {
  echo "SOURCE=none"
  echo "REASON=usage: prd-source.sh <number> | --issue-only <number> | --check-body"
  exit 2
}

mode=auto
case "${1:-}" in
  --check-body)
    check_body
    exit $?
    ;;
  --issue-only)
    mode=issue
    shift
    ;;
esac

n="${1:-}"
case "$n" in
  '' | *[!0-9]*) usage ;;
esac

if [ "$mode" = auto ]; then
  file=$(find prds/ -maxdepth 1 -name "${n}-*.md" -print -quit 2>/dev/null || true)
  if [ -n "$file" ]; then
    # The heading shapes in use: "# PRD #123: Title", "## PRD #123 - Title", "# Title".
    title=$(head -1 "$file" | sed -E 's/^#+ *(PRD *#?[0-9]* *[:\-] *)?//')
    echo "SOURCE=file"
    echo "FILE=$(flatten "$file")"
    echo "TITLE=$(flatten "$title")"
    exit 0
  fi
fi

if ! issue=$(gh api "repos/{owner}/{repo}/issues/${n}" 2>&1); then
  echo "SOURCE=none"
  echo "REASON=$(flatten "gh could not read issue #${n}: ${issue}")"
  exit 1
fi

# One jq call per field, so a newline inside the title stays inside the title
# until `flatten` removes it, rather than shifting every field after it.
field() { printf '%s' "$issue" | jq -r "$1"; }
if ! kind=$(field 'if .pull_request then "pr" else "issue" end' 2>&1); then
  echo "SOURCE=none"
  echo "REASON=$(flatten "issue #${n} did not parse as JSON: ${kind}")"
  exit 1
fi
title=$(field '.title // ""')
state=$(field '.state // "" | ascii_upcase')
association=$(field '.author_association // ""')

echo "TITLE=$(flatten "$title")"
echo "STATE=$(flatten "$state")"

if [ "$kind" = pr ]; then
  echo "PRD_CONTENT=no"
  echo "AUTHOR_TRUSTED=no"
  echo "SOURCE=none"
  echo "REASON=#${n} is a pull request, not an issue"
  exit 0
fi

case "$association" in
  OWNER | MEMBER | COLLABORATOR) trusted=yes ;;
  *) trusted=no ;;
esac

verdict=$(printf '%s' "$issue" | jq -r '.body // ""' | check_body)
content=$(printf '%s\n' "$verdict" | sed -n 's/^PRD_CONTENT=//p' | head -1)
why=$(printf '%s\n' "$verdict" | sed -n 's/^REASON=//p' | head -1)

echo "PRD_CONTENT=$content"
echo "AUTHOR_TRUSTED=$trusted"

if [ "$content" != yes ]; then
  echo "SOURCE=none"
  echo "REASON=$(flatten "issue #${n} carries no PRD content: ${why}")"
  exit 0
fi
if [ "$trusted" != yes ]; then
  echo "SOURCE=none"
  echo "REASON=$(flatten "issue #${n} was opened by a non-collaborator (${association:-unknown}), so its body is not a PRD")"
  exit 0
fi

echo "SOURCE=issue"
exit 0
