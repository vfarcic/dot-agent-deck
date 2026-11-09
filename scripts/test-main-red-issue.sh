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
# Fake gh: state lives in $FAKE_STATE. Only the calls main-red-issue.sh makes.
set -euo pipefail
S="$FAKE_STATE"
act() { echo "$*" >>"$S/actions"; }
arg_after() { local want="$1"; shift; while [ $# -gt 0 ]; do [ "$1" = "$want" ] && { echo "$2"; return; }; shift; done; }
case "$1 $2" in
  "api "*)
    path="$2"
    case "$path" in
      */compare/*) pair="${path##*/compare/}"; cat "$S/compare/$pair" 2>/dev/null || true ;;
      */actions/workflows/*) cat "$S/latest_green" 2>/dev/null || true ;;
    esac ;;
  "issue list")
    state="$(arg_after --state "$@")"
    if [ "$state" = open ]; then sort -n "$S/open" 2>/dev/null | head -1; else cat "$S/recent_closed" 2>/dev/null || true; fi ;;
  "issue view") cat "$S/issues/$3" ;;
  "issue comment") act "comment $3"; printf '%s\n' "$(arg_after --body "$@")" >>"$S/issues/$3" ;;
  "issue reopen") act "reopen $3"; echo "$3" >>"$S/open" ;;
  "issue close")
    act "close $3"; printf '%s\n' "$(arg_after --comment "$@")" >>"$S/issues/$3"
    grep -vx "$3" "$S/open" >"$S/open.new" || true; mv "$S/open.new" "$S/open" ;;
  "issue create")
    n="$(cat "$S/next")"; echo $((n + 1)) >"$S/next"; act "create $n"
    printf '%s\n' "$(arg_after --body "$@")" >"$S/issues/$n"
    echo "$n" >>"$S/open"
    # A concurrent run that opened a lower-numbered issue meanwhile.
    if [ -f "$S/race" ]; then cat "$S/race" >>"$S/open"; fi
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
  : >"$FAKE_STATE/open"; : >"$FAKE_STATE/actions"; echo 10 >"$FAKE_STATE/next"
}
ahead() { echo "${3:-ahead}" >"$FAKE_STATE/compare/$1...$2"; }   # $2 relative to $1
issue() { echo "$1" >>"$FAKE_STATE/open"; printf '%s\n' "$2" >"$FAKE_STATE/issues/$1"; }
run() {
  PATH="$WORK/bin:$PATH" GH_REPO=o/r COMMIT_SHA="$2" RUN_URL=https://run RUN_ATTEMPT=1 \
    FAILED_JOBS=test COMMIT_SUBJECT=subj "$SUT" "$1" 2>"$FAKE_STATE/stderr" || {
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
echo 5 >"$FAKE_STATE/recent_closed"; printf '%s\n' "$(red_marker aaaaaaa1)" >"$FAKE_STATE/issues/5"
run red bbbbbbb2
expect_actions "reopen 5;comment 5;"

reset "red racing another red run: the higher-numbered issue closes itself"
echo 3 >"$FAKE_STATE/race"; printf '%s\n' "$(red_marker 9999999a)" >"$FAKE_STATE/issues/3"
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

reset "green, nothing open: no writes"
run green ddddddd4
expect_actions ""

reset "unknown state argument: exits 0, no writes"
run sideways aaaaaaa1
expect_actions ""

if [ "$fails" -ne 0 ]; then echo "$fails scenario(s) failed"; exit 1; fi
echo "all scenarios passed"
