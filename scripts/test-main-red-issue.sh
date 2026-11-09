#!/usr/bin/env bash
# Scenario tests for scripts/main-red-issue.sh (issue #2073), run against a fake
# `gh` that keeps issues in a temp directory. No network, no token.
#
# Each scenario sets the repository's state (open/closed issues, which commit
# is ahead of which, the latest green push run), runs the script once, and
# checks which writes it made. Exits non-zero on the first failed check.
#
# Usage: scripts/test-main-red-issue.sh

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SUT="$HERE/main-red-issue.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

mkdir -p "$WORK/bin"
cat >"$WORK/bin/gh" <<'FAKE'
#!/usr/bin/env bash
# Fake gh: each issue is $FAKE_STATE/issues/<n>.json ({state, author, body,
# comments: [{author, body}]}), and `-q` expressions run through real jq.
# Only the calls main-red-issue.sh makes.
set -euo pipefail
S="$FAKE_STATE"
act() { echo "$*" >>"$S/actions"; }
arg_after() { local want="$1"; shift; while [ $# -gt 0 ]; do [ "$1" = "$want" ] && { echo "$2"; return; }; shift; done; }
f() { echo "$S/issues/$1.json"; }
upd() { local n="$1"; shift; jq "$@" "$(f "$n")" >"$(f "$n").new" && mv "$(f "$n").new" "$(f "$n")"; }
add_comment() { upd "$1" --arg a "${FAKE_AUTHOR:-github-actions}" --arg b "$2" '.comments += [{author: {login: $a}, body: $b}]'; }
case "$1 $2" in
  "api "*)
    path="$2"
    case "$path" in
      */compare/*) pair="${path##*/compare/}"; cat "$S/compare/$pair" 2>/dev/null || true ;;
      */actions/workflows/*) cat "$S/latest_green" 2>/dev/null || true ;;
    esac ;;
  "issue list")
    if [ "$(arg_after --state "$@")" = open ]; then
      for j in "$S"/issues/*.json; do [ -e "$j" ] || continue
        [ "$(jq -r .state "$j")" = OPEN ] && basename "$j" .json; done | sort -n | head -1
    else cat "$S/recent_closed" 2>/dev/null || true; fi ;;
  "issue view") jq -r "$(arg_after -q "$@")" "$(f "$3")" ;;
  "issue comment")
    act "comment $3"
    # A concurrent green run closing the issue before this comment lands.
    if [ -f "$S/close_before_comment" ]; then rm "$S/close_before_comment"; upd "$3" '.state = "CLOSED"'; fi
    add_comment "$3" "$(arg_after --body "$@")" ;;
  "issue reopen") act "reopen $3"; upd "$3" '.state = "OPEN"' ;;
  "issue close")
    act "close $3"
    # A concurrent red run's comment landing between the read and the close.
    if [ -f "$S/comment_before_close" ]; then add_comment "$3" "$(cat "$S/comment_before_close")"; rm "$S/comment_before_close"; fi
    add_comment "$3" "$(arg_after --comment "$@")"; upd "$3" '.state = "CLOSED"' ;;
  "issue create")
    n="$(cat "$S/next")"; echo $((n + 1)) >"$S/next"; act "create $n"
    jq -n --arg b "$(arg_after --body "$@")" '{state: "OPEN", author: {login: "github-actions"}, body: $b, comments: []}' >"$(f "$n")"
    # A concurrent run that opened a lower-numbered issue meanwhile.
    if [ -f "$S/race" ]; then upd "$(cat "$S/race")" '.state = "OPEN"'; fi
    echo "https://github.com/o/r/issues/$n" ;;
  "label create") act "label" ;;
  *) echo "fake gh: unexpected: $*" >&2; exit 1 ;;
esac
FAKE
chmod +x "$WORK/bin/gh"

fails=0
scenario=""
reset() {
  scenario="$1"
  export FAKE_STATE="$WORK/state"
  rm -rf "$FAKE_STATE"; mkdir -p "$FAKE_STATE/issues" "$FAKE_STATE/compare"
  : >"$FAKE_STATE/actions"; echo 10 >"$FAKE_STATE/next"
}
ahead() { echo "${3:-ahead}" >"$FAKE_STATE/compare/$1...$2"; }   # $2 relative to $1
# issue <n> <body> [state] [author]
issue() {
  jq -n --arg s "${3:-OPEN}" --arg a "${4:-github-actions}" --arg b "$2" \
    '{state: $s, author: {login: $a}, body: $b, comments: []}' >"$FAKE_STATE/issues/$1.json"
}
# comment <n> <author> <body>
comment() {
  jq --arg a "$2" --arg b "$3" '.comments += [{author: {login: $a}, body: $b}]' \
    "$FAKE_STATE/issues/$1.json" >"$FAKE_STATE/x" && mv "$FAKE_STATE/x" "$FAKE_STATE/issues/$1.json"
}
state_of() { jq -r .state "$FAKE_STATE/issues/$1.json"; }
expect_state() {
  if [ "$(state_of "$1")" = "$2" ]; then echo "ok   [$scenario] #$1 is $2"
  else echo "FAIL [$scenario] #$1 expected $2, got $(state_of "$1")"; fails=$((fails + 1)); fi
}
run() {
  PATH="$WORK/bin:$PATH" GH_REPO=o/r COMMIT_SHA="$2" RUN_URL=https://run RUN_ATTEMPT=1 \
    FAILED_JOBS=test COMMIT_SUBJECT="${COMMIT_SUBJECT_OVERRIDE:-subj}" "$SUT" "$1" 2>"$FAKE_STATE/stderr" || {
      echo "FAIL [$scenario]: script exited non-zero"; fails=$((fails + 1)); }
}
expect_actions() {
  local got; got="$(tr '\n' ';' <"$FAKE_STATE/actions")"
  if [ "$got" = "$1" ]; then echo "ok   [$scenario] $got"
  else echo "FAIL [$scenario] expected '$1', got '$got'"; sed 's/^/     /' "$FAKE_STATE/stderr"; fails=$((fails + 1)); fi
}
red_marker() { printf '<!-- main-red-sha: %s -->' "$1"; }
green_marker() { printf '<!-- main-green-sha: %s -->' "$1"; }

reset "red, nothing open: opens one issue"
run red aaaaaaa1
expect_actions "label;create 10;"

reset "red, one open: comments, never a second issue"
issue 7 "$(red_marker aaaaaaa1)"
run red bbbbbbb2
expect_actions "comment 7;"

reset "red on a commit a later green push superseded: records nothing"
echo ccccccc3 >"$FAKE_STATE/latest_green"; ahead aaaaaaa1 ccccccc3
run red aaaaaaa1
expect_actions ""

reset "red, one closed within the window: reopens it"
echo 5 >"$FAKE_STATE/recent_closed"; issue 5 "$(red_marker aaaaaaa1)" CLOSED
run red bbbbbbb2
expect_actions "reopen 5;comment 5;"

reset "red racing another red run: the higher-numbered issue closes itself"
echo 3 >"$FAKE_STATE/race"; issue 3 "$(red_marker 9999999a)" PENDING
run red aaaaaaa1
expect_actions "label;create 10;comment 3;close 10;"

reset "green at or after every recorded red: closes"
issue 7 "$(red_marker aaaaaaa1)
$(red_marker bbbbbbb2)"
ahead aaaaaaa1 ddddddd4; ahead bbbbbbb2 ddddddd4
run green ddddddd4
expect_actions "close 7;"

reset "green re-run of the very commit that was red: closes"
issue 7 "$(red_marker aaaaaaa1)"
ahead aaaaaaa1 aaaaaaa1 identical
run green aaaaaaa1
expect_actions "close 7;"

reset "green on a commit older than a recorded red: stays open"
issue 7 "$(red_marker aaaaaaa1)
$(red_marker bbbbbbb2)"
ahead aaaaaaa1 ccccccc3; ahead bbbbbbb2 ccccccc3 behind
run green ccccccc3
expect_actions ""

reset "green when the compare call fails: stays open"
issue 7 "$(red_marker aaaaaaa1)"
run green ddddddd4
expect_actions ""

reset "green after a reopen counts only the reds since the last green"
issue 7 "$(red_marker 0000000a)
$(green_marker 0000000b)
$(red_marker aaaaaaa1)"
ahead aaaaaaa1 ddddddd4   # no entry for 0000000a: it must not be consulted
run green ddddddd4
expect_actions "close 7;"

reset "green ignores markers from anyone but the workflow's account"
issue 7 "$(red_marker aaaaaaa1)"
comment 7 someone "$(green_marker 0000000b) $(red_marker 0000000a)"
ahead 0000000a ddddddd4   # would close if the forged reset counted
ahead aaaaaaa1 ddddddd4 behind
run green ddddddd4
expect_actions ""

reset "green closes an issue whose author is spelled the REST way, github-actions[bot]"
issue 7 "$(red_marker aaaaaaa1)" OPEN "github-actions[bot]"
ahead aaaaaaa1 ddddddd4
run green ddddddd4
expect_actions "close 7;"

reset "an override spelled github-actions[bot] matches the GraphQL login"
issue 7 "$(red_marker aaaaaaa1)"
ahead aaaaaaa1 ddddddd4
MAIN_RED_BOT='github-actions[bot]' run green ddddddd4
expect_actions "close 7;"

reset "a marker in the commit subject cannot reset the reds"
COMMIT_SUBJECT_OVERRIDE="x $(green_marker 0000000b) y"
issue 7 "$(red_marker aaaaaaa1)"
run red bbbbbbb2
unset COMMIT_SUBJECT_OVERRIDE
# Were the subject's marker honoured, it would reset away aaaaaaa1, which
# this green does not cover, and the issue would close.
ahead aaaaaaa1 ccccccc3 behind; ahead bbbbbbb2 ccccccc3
: >"$FAKE_STATE/actions"
run green ccccccc3
expect_actions ""

reset "red recorded on an issue a concurrent green closed: reopens it"
issue 7 "$(red_marker aaaaaaa1)"
touch "$FAKE_STATE/close_before_comment"
run red bbbbbbb2
expect_actions "comment 7;reopen 7;comment 7;"
expect_state 7 OPEN

reset "red landing between a green's read and its close: reopens it"
issue 7 "$(red_marker aaaaaaa1)"
red_marker bbbbbbb2 >"$FAKE_STATE/comment_before_close"
ahead aaaaaaa1 ccccccc3; ahead bbbbbbb2 ccccccc3 behind
run green ccccccc3
expect_actions "close 7;reopen 7;comment 7;"
expect_state 7 OPEN
# and the next green that covers it closes it for good
ahead aaaaaaa1 ddddddd4; ahead bbbbbbb2 ddddddd4
: >"$FAKE_STATE/actions"
run green ddddddd4
expect_actions "close 7;"

reset "green, nothing open: no writes"
run green ddddddd4
expect_actions ""

reset "unknown state argument: exits 0, no writes"
run sideways aaaaaaa1
expect_actions ""

if [ "$fails" -ne 0 ]; then echo "$fails scenario(s) failed"; exit 1; fi
echo "all scenarios passed"
