#!/usr/bin/env bash
#
# Pick one random area for a /code-cleanup unit to examine, or check that a
# unit's changes stay inside its mode's owned set.
#
# Usage: pick-area.sh <code|tests|instructions> [--findings <file>] [--off-limits <file>]
#        pick-area.sh <code|tests|instructions> --check [--off-limits <file>]
#
# Run from inside the unit's own worktree, after `git fetch origin`. Prints:
#
#   MODE=<mode>
#   SOURCE=draw
#   CANDIDATES=<files in the mode's owned set that survived the filters>
#   SKIPPED_BUSY=<owned files skipped because an open PR or a running unit touches them>
#   SKIPPED_RECENT=<owned files skipped because a recent cleanup PR touched them>
#   SKIPPED_OFF_LIMITS=<owned files skipped because the runner put them off limits>
#   FILE=<path>
#   LINE=<a random line in FILE; the area is the item that encloses it>
#
# or `ERROR=<reason>` and a non-zero exit.
#
# With `--findings <file>` (the output of find-targets.sh code: `TARGET=` lines
# and the `FINDINGS=` total; `TOOL=` lines are ignored) it first filters the tools' findings instead: a finding
# survives when the mode's --check set owns its file, no open PR or running unit
# touches that file, and no off-limits pattern matches it. The recent-cleanup
# filter does not apply: a finding the tools still report was not fixed by the
# recent PR, and one a reviewer rejected belongs in the tool's recorded false
# positives, not in a retry window. When any survive it prints
#
#   MODE=<mode>
#   SOURCE=findings
#   FINDINGS=<TARGET lines read>
#   SKIPPED_NOT_OWNED=, SKIPPED_BUSY=, SKIPPED_OFF_LIMITS=   (findings dropped, per filter)
#   CANDIDATES=<findings that survived>
#   TARGET=<tool><TAB><path><TAB><line><TAB><what>   (one per surviving finding)
#   FILE=<path of one surviving finding, drawn at random>
#   LINE=<its line>
#   FINDING=<its tool><TAB><what>
#
# and when none survive it prints `FINDINGS=` and `FINDINGS_KEPT=0` and falls
# back to the random draw above, so a unit whose findings were all filtered
# out still gets an area. A findings file holding an `ERROR=` line, or with no
# `FINDINGS=` total matching its `TARGET=` lines, is a tool run that failed or
# was cut short: that is an `ERROR=` here, never a draw.
#
# `--off-limits <file>` names paths no unit may draw or change, one per line
# (blank lines and `#` comments ignored), as the runner passed them. A line is a
# path or a shell glob in which `*` also matches `/`; it covers the files it
# matches and everything under a directory it matches, so `docs/develop/` and
# `src/inbox*.rs` both work. There is no brace expansion: write `{a,b}` as two
# lines. Off-limits files are skipped like busy ones in the draw, and in
# `--check` each changed one prints `OFF_LIMITS=<path>` and fails the check.
#
# With `--check` it reads every file changed since the merge-base with
# origin/main — committed, staged, unstaged and untracked, with renames split
# into their deletion and addition so both paths are judged — and prints
# `OUTSIDE=<path>` for each one the mode does not own, exiting 1 if there is
# any. In tests mode it also prints `SNAPSHOT=<path>` for each changed insta
# snapshot: the mode may rename or delete one with the test it belongs to, but
# a changed snapshot BODY is a behaviour change, so the unit confirms each by
# hand. The check sets are wider than the pick sets: a file nobody should open
# as an area (tests/CATALOG.md, a snapshot, a Cargo.toml) can still be one a
# change must touch. The code mode's check set includes the manifests and lock
# files and desktop/knip.jsonc, because a dependency finding from
# find-targets.sh is fixed there and a false positive is recorded there.
#
# The pick is weighted by size in BYTES, not lines, so a 44k-line file comes up
# far more often than a small helper, and CLAUDE.md — a few hundred lines, each
# a whole paragraph — weighs what its text weighs. The line is drawn uniformly
# by byte offset inside the chosen file, so a long function is likelier to be
# hit than a short one, for the same reason.
#
# The draw applies four filters, in this order:
#   1. the mode's OWNED set (SKILL.md "The three modes"), minus files no
#      cleanup should open as an area (snapshots, the catalog, generated or
#      binary files);
#   2. BUSY files: every file an open PR changes, and every file a running
#      dispatch unit on this machine has changed, committed or not, with both
#      sides of a rename counted as busy. A running
#      unit is a linked worktree on an `agent/dispatch-*` branch; this unit's
#      own worktree is left out. A unit on another machine that has not pushed
#      is invisible here — accepted, since its PR will show up in (2) once it
#      opens and the cleanup diff is small;
#   3. RECENT files: every file the 20 most recent `cleanup`-labelled PRs
#      changed, in any state, so a closed-unmerged cleanup is not retried at
#      once either;
#   4. OFF-LIMITS files: whatever `--off-limits` names.
#
# A failed GitHub lookup for (2) or (3) stops the draw with ERROR= rather than
# drawing from a filter that silently lost entries.
#
# PR file names come from GitHub and on a public repository a fork chooses
# them. They are only ever used as exact-match exclusion lines here and are
# never printed, so they cannot reach the caller's terminal or context.

set -uo pipefail

die() { echo "ERROR=$*"; exit 1; }

mode="${1:-}"
case "$mode" in
  code|tests|instructions) ;;
  *) die "usage: pick-area.sh <code|tests|instructions> [--check]" ;;
esac
shift
check='' findings='' off_limits=''
while [ $# -gt 0 ]; do
  case "$1" in
    --check) check=--check; shift ;;
    --findings) [ $# -ge 2 ] || die "--findings needs a file"; findings="$2"; shift 2 ;;
    --off-limits) [ $# -ge 2 ] || die "--off-limits needs a file"; off_limits="$2"; shift 2 ;;
    *) die "unknown option: $1" ;;
  esac
done
[ -z "$check" ] || [ -z "$findings" ] || die "--findings is for the draw, not --check"
# Resolve both before the cd below, so a relative path means the caller's cwd.
for f in "$findings" "$off_limits"; do
  [ -z "$f" ] || [ -r "$f" ] || die "cannot read $f"
done
[ -z "$findings" ] || findings=$(cd "$(dirname "$findings")" && pwd)/$(basename "$findings")
[ -z "$off_limits" ] || off_limits=$(cd "$(dirname "$off_limits")" && pwd)/$(basename "$off_limits")

command -v jq >/dev/null || die "jq not found"
command -v gh >/dev/null || die "gh not found"

top=$(git rev-parse --show-toplevel 2>/dev/null) || die "not inside a git checkout"
cd "$top" || die "cannot cd to $top"

tmp=$(mktemp -d) || die "mktemp failed"
trap 'rm -rf "$tmp"' EXIT

# The listed paths (one per line in $2) that mode $1 may NOT change: the
# complement of its --check set.
not_owned() {
  case "$1" in
    code)
      grep -vE '^(src|desktop/src-tauri/src|desktop/src|xtask/[^/]+/src)/|^(Cargo\.(toml|lock)|xtask/[^/]+/Cargo\.toml|desktop/src-tauri/Cargo\.toml|desktop/package\.json|desktop/pnpm-lock\.yaml|desktop/knip\.jsonc)$' "$2"
      grep -E '\.test\.tsx?$|\.snap$' "$2"
      ;;
    tests)
      grep -vE '^(tests|xtask/[^/]+/tests|desktop/src-tauri/tests|desktop/driver|desktop/e2e)/|^desktop/src/.*\.test\.tsx?$|^\.config/nextest\.toml$' "$2"
      ;;
    instructions)
      grep -vE '^CLAUDE\.md$|^docs/develop/|^\.claude/skills/[^/]+/[^/]+\.md$' "$2"
      ;;
  esac
  return 0
}

# The off-limits patterns, one per array entry, with comments, blank lines and
# surrounding whitespace dropped and a trailing `/` removed.
patterns=()
if [ -n "$off_limits" ]; then
  while IFS= read -r p || [ -n "$p" ]; do
    p="${p%%#*}"
    p="${p#"${p%%[![:space:]]*}"}"
    p="${p%"${p##*[![:space:]]}"}"
    p="${p%/}"
    [ -n "$p" ] && patterns+=("$p")
  done <"$off_limits"
fi

# The listed paths (one per line in $1) that an off-limits pattern covers. The
# right-hand side of `==` is left unquoted on purpose: that is what makes it a
# glob, and inside `[[ ]]` a `*` matches `/` too.
off_limits_hits() {
  local f p
  [ ${#patterns[@]} -gt 0 ] || return 0
  while IFS= read -r f; do
    for p in "${patterns[@]}"; do
      # shellcheck disable=SC2053
      if [[ $f == $p || $f == $p/* ]]; then
        echo "$f"
        break
      fi
    done
  done <"$1"
}

if [ "$check" = --check ]; then
  base=$(git merge-base origin/main HEAD) || die "no merge-base with origin/main; run git fetch origin"
  { git diff --no-renames --name-only "$base"
    git ls-files --others --exclude-standard; } | sort -u >"$tmp/changed"
  not_owned "$mode" "$tmp/changed" >"$tmp/out"
  [ "$mode" = tests ] && grep -E '\.snap$' "$tmp/changed" | sed 's/^/SNAPSHOT=/'
  sort -u "$tmp/out" | sed 's/^/OUTSIDE=/' | tee "$tmp/report"
  off_limits_hits "$tmp/changed" | sed 's/^/OFF_LIMITS=/' | tee -a "$tmp/report"
  echo "CHANGED=$(wc -l <"$tmp/changed")"
  [ -s "$tmp/report" ] && exit 1
  exit 0
fi

# 1. The owned set, as tracked files.
case "$mode" in
  code)
    git ls-files -- src desktop/src-tauri/src desktop/src 'xtask/*/src' \
      | grep -E '\.(rs|ts|tsx|css)$' \
      | grep -vE '\.test\.tsx?$' >"$tmp/owned"
    ;;
  tests)
    git ls-files -- tests 'xtask/*/tests' desktop/src-tauri/tests desktop/driver desktop/e2e desktop/src \
      | grep -E '(^(tests|xtask/[^/]+/tests|desktop/src-tauri/tests)/.*\.rs$)|(^desktop/(driver|e2e)/.*\.tsx?$)|(^desktop/src/.*\.test\.tsx?$)' \
      >"$tmp/owned"
    ;;
  instructions)
    # Skill Markdown one level down only (a SKILL.md and its siblings), the
    # same rule --check applies: nested Markdown under a skill is a test
    # fixture, not an instruction (demo-reel-adapter/tests/fixtures/...).
    git ls-files -- CLAUDE.md .claude/skills docs/develop \
      | grep -E '^CLAUDE\.md$|^docs/develop/.*\.md$|^\.claude/skills/[^/]+/[^/]+\.md$' >"$tmp/owned"
    ;;
esac
[ -s "$tmp/owned" ] || die "the $mode owned set is empty; has the layout moved?"

# 2. Busy files: open PRs ... (a renamed file's old path is `previous_filename`)
files_jq='.[] | .filename, (.previous_filename // empty)'
repo=$(gh repo view --json nameWithOwner --jq .nameWithOwner) || die "gh repo view failed"
gh pr list --repo "$repo" --state open --limit 300 --json number --jq '.[].number' >"$tmp/open" \
  || die "gh pr list failed"
[ "$(wc -l <"$tmp/open")" -lt 300 ] || die "300 or more open PRs; raise the limit before trusting the busy filter"
: >"$tmp/busy"
while read -r n; do
  [ -n "$n" ] || continue
  gh api "repos/$repo/pulls/$n/files" --paginate --jq "$files_jq" >>"$tmp/busy" \
    || die "cannot read the files of PR #$n"
done <"$tmp/open"

# ... and running dispatch units on this machine.
git worktree list --porcelain | awk '
  /^worktree /{wt=substr($0,10)}
  /^branch refs\/heads\/agent\/dispatch-/{print wt "\t" substr($0,19)}' \
  | while IFS=$'\t' read -r wt branch; do
      [ "$wt" = "$top" ] && continue
      git diff --no-renames --name-only "origin/main...$branch" 2>/dev/null
      git -C "$wt" diff --no-renames --name-only HEAD 2>/dev/null
    done >>"$tmp/busy"

# 3. Recent cleanup PRs. Read the list into a file first: a `die` inside a
# piped loop would only end the pipeline's subshell.
gh pr list --repo "$repo" --label cleanup --state all --limit 20 --json number --jq '.[].number' \
  >"$tmp/recent-prs" || die "cannot list recent cleanup PRs"
: >"$tmp/recent"
while read -r n; do
  [ -n "$n" ] || continue
  gh api "repos/$repo/pulls/$n/files" --paginate --jq "$files_jq" >>"$tmp/recent" \
    || die "cannot read the files of cleanup PR #$n"
done <"$tmp/recent-prs"

sort -u "$tmp/busy" -o "$tmp/busy"
sort -u "$tmp/recent" -o "$tmp/recent"
seed=$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')

# The tools' findings first, when the unit passed them. Each filter's drop is
# counted on its own so the unit can report why a finding was not handed out.
if [ -n "$findings" ]; then
  # A failed or cut-short tool run is refused here, never turned into a draw:
  # find-targets.sh prints FINDINGS= last, and only when every tool succeeded.
  tool_error=$(grep -m1 '^ERROR=' "$findings")
  [ -z "$tool_error" ] || die "$findings records a failed tool run (${tool_error#ERROR=}); fix that, do not draw without the findings"
  declared=$(sed -n 's/^FINDINGS=\([0-9][0-9]*\)$/\1/p' "$findings" | tail -n 1)
  [ -n "$declared" ] || die "$findings has no FINDINGS= total, so find-targets.sh did not finish; rerun it"
  grep '^TARGET=' "$findings" | sed 's/^TARGET=//' >"$tmp/targets" || true
  [ "$(wc -l <"$tmp/targets")" -eq "$declared" ] \
    || die "$findings declares FINDINGS=$declared but holds $(wc -l <"$tmp/targets") TARGET= lines"
  awk -F'\t' 'NF >= 4 && $3 ~ /^[0-9]+$/' "$tmp/targets" >"$tmp/targets.ok"
  [ "$(wc -l <"$tmp/targets.ok")" -eq "$(wc -l <"$tmp/targets")" ] \
    || die "$findings has a TARGET= line that is not <tool><TAB><path><TAB><line><TAB><what>"
  cut -f2 "$tmp/targets" | sort -u >"$tmp/target-files"
  not_owned "$mode" "$tmp/target-files" | sort -u >"$tmp/t-not-owned"
  off_limits_hits "$tmp/target-files" | sort -u >"$tmp/t-off-limits"
  # One pass, so a finding is counted against the first filter that drops it.
  awk -F'\t' -v no="$tmp/t-not-owned" -v busy="$tmp/busy" -v off="$tmp/t-off-limits" \
      -v kept="$tmp/kept" -v counts="$tmp/t-counts" '
    BEGIN {
      while ((getline f < no) > 0) drop_no[f] = 1
      while ((getline f < busy) > 0) drop_busy[f] = 1
      while ((getline f < off) > 0) drop_off[f] = 1
    }
    $2 in drop_no { n_no++; next }
    $2 in drop_busy { n_busy++; next }
    $2 in drop_off { n_off++; next }
    { print > kept; n_kept++ }
    END {
      printf "%d\t%d\t%d\t%d\n", n_no, n_busy, n_off, n_kept > counts
    }' "$tmp/targets"
  IFS=$'\t' read -r t_no t_busy t_off t_kept <"$tmp/t-counts"
  t_total=$(wc -l <"$tmp/targets")
  if [ "$t_kept" -gt 0 ]; then
    drawn=$(awk -v seed="$seed" 'BEGIN { srand(seed) } { l[NR] = $0 } END { print l[int(rand() * NR) + 1] }' "$tmp/kept")
    IFS=$'\t' read -r d_tool d_file d_line d_what <<<"$drawn"
    echo "MODE=$mode"
    echo "SOURCE=findings"
    echo "FINDINGS=$t_total"
    echo "SKIPPED_NOT_OWNED=$t_no"
    echo "SKIPPED_BUSY=$t_busy"
    echo "SKIPPED_OFF_LIMITS=$t_off"
    echo "CANDIDATES=$t_kept"
    sed 's/^/TARGET=/' "$tmp/kept"
    echo "FILE=$d_file"
    echo "LINE=$d_line"
    printf 'FINDING=%s\t%s\n' "$d_tool" "$d_what"
    exit 0
  fi
  echo "FINDINGS=$t_total"
  echo "FINDINGS_KEPT=0"
fi

# Otherwise the random draw, through filters 2 to 4.
off_limits_hits "$tmp/owned" | sort -u >"$tmp/off"
grep -vxFf "$tmp/busy" "$tmp/owned" >"$tmp/free1" || true
grep -vxFf "$tmp/recent" "$tmp/free1" >"$tmp/free2" || true
grep -vxFf "$tmp/off" "$tmp/free2" >"$tmp/free" || true

skipped_busy=$(( $(wc -l <"$tmp/owned") - $(wc -l <"$tmp/free1") ))
skipped_recent=$(( $(wc -l <"$tmp/free1") - $(wc -l <"$tmp/free2") ))
skipped_off=$(( $(wc -l <"$tmp/free2") - $(wc -l <"$tmp/free") ))
[ -s "$tmp/free" ] || die "every owned file is busy, recently cleaned or off limits (busy $skipped_busy, recent $skipped_recent, off limits $skipped_off)"

# Weighted draw: sizes in bytes, one random offset across their sum.
while IFS= read -r f; do
  printf '%s\t%s\n' "$(wc -c <"$f")" "$f"
done <"$tmp/free" >"$tmp/sized"

pick=$(awk -F'\t' -v seed="$seed" '
  BEGIN { srand(seed) }
  { size[NR] = $1; name[NR] = $2; total += $1 }
  END {
    if (total == 0) exit 1
    r = int(rand() * total)
    for (i = 1; i <= NR; i++) {
      if (r < size[i]) { print r "\t" name[i]; exit }
      r -= size[i]
    }
  }' "$tmp/sized") || die "the free files total zero bytes"

offset=${pick%%$'\t'*}
file=${pick#*$'\t'}
line=$(( $(head -c "$offset" "$file" | wc -l) + 1 ))

echo "MODE=$mode"
echo "SOURCE=draw"
echo "CANDIDATES=$(wc -l <"$tmp/free")"
echo "SKIPPED_BUSY=$skipped_busy"
echo "SKIPPED_RECENT=$skipped_recent"
echo "SKIPPED_OFF_LIMITS=$skipped_off"
echo "FILE=$file"
echo "LINE=$line"
