#!/usr/bin/env bash
# shellcheck disable=SC2016 # the backticks in the issue text are Markdown, not command substitutions
# Keep ONE "main is red" issue in step with `main`'s post-merge CI (issue #2073).
#
# Called by the `notify-main-red` job in `.github/workflows/ci.yml` on every
# push run on `main` that finished with a verdict: `red` when a job it needs
# failed, `green` when none failed and none was cancelled. A run with a
# cancelled job and no failure is no verdict, and the job does not call this.
#
# Why an issue: the job's Telegram send needs two secrets that are not
# registered on this repository, so before this script a red `main` produced an
# annotation and a job summary that notified nobody. An open issue carrying the
# `main-red` label is something a person is notified about (the repository's
# watchers) and something an agent can check before it dispatches work
# (`.claude/skills/dispatch-base`, "Check whether `main` is red").
#
# Lifecycle, and the two properties it exists to hold:
#
#   red    An open issue gets a comment, never a second issue; a closed one
#          closed within REOPEN_WINDOW_HOURS is reopened rather than replaced;
#          otherwise a new one is opened. Two red runs racing past the "is one
#          open?" read can each open one, so after creating, the script keeps
#          the LOWEST-numbered open issue and closes its own if it is not that
#          one. A red on a commit that a later, already-green push run has
#          superseded (a re-run of an old commit) records nothing.
#   green  Closes the open issue only when the green commit is the same as, or
#          a descendant of, EVERY red commit recorded since the issue last
#          opened. A green on an older commit (runs finish out of order, and a
#          re-run of an old commit can pass) leaves it open.
#
# So a flaky red followed by a green re-run of the same commit opens and closes
# one issue; the next red within the window reopens that same issue instead of
# opening a second.
#
# The commits each issue records live in HTML-comment markers in its body and
# comments (`<!-- main-red-sha: <sha> -->`, `<!-- main-green-sha: <sha> -->`),
# which is what the green check reads back. Ancestry comes from GitHub's compare
# API, so the job needs no git history.
#
# NEVER FAILS THE BUILD: like `scripts/notify.sh`, it always exits 0. It now
# runs on green runs too, and an error here must not turn a green `main` red.
#
# Usage:  scripts/main-red-issue.sh red|green
# Env:    GH_TOKEN / GH_REPO  for `gh` (the job passes the run's GITHUB_TOKEN,
#                             which needs `issues: write`)
#         COMMIT_SHA          the run's head commit (required)
#         RUN_URL             link to the run (required)
#         RUN_ATTEMPT         the run's attempt number (default 1)
#         FAILED_JOBS         comma-separated failing jobs (red only)
#         COMMIT_SUBJECT      the commit's subject line (optional)
#         DRY_RUN=1           read for real, print every write instead of doing it
#         REOPEN_WINDOW_HOURS default 24
#         MAIN_RED_LABEL      default main-red

set -uo pipefail   # deliberately NOT -e: this script must never fail a workflow

STATE="${1:-}"
LABEL="${MAIN_RED_LABEL:-main-red}"
WINDOW_HOURS="${REOPEN_WINDOW_HOURS:-24}"
ATTEMPT="${RUN_ATTEMPT:-1}"
SHA="${COMMIT_SHA:-}"
SHORT="${SHA:0:8}"

log() { printf 'main-red-issue: %s\n' "$*" >&2; }

# Every write goes through here, so DRY_RUN=1 exercises every read and decision
# against the real repository while changing nothing.
write() {
  if [ "${DRY_RUN:-}" = "1" ]; then
    printf 'DRY_RUN would run:' >&2; printf ' %q' "$@" >&2; printf '\n' >&2
    # A dry-run "create" has no URL to print; give the caller a placeholder.
    [ "${2:-} ${3:-}" = "issue create" ] && echo "https://example.invalid/issues/0"
    return 0
  fi
  "$@"
}

case "$STATE" in red|green) ;; *) log "usage: $0 red|green (got '${STATE}') — nothing done"; exit 0 ;; esac
if [ -z "$SHA" ] || [ -z "${RUN_URL:-}" ]; then
  log "COMMIT_SHA and RUN_URL are required — nothing done"; exit 0
fi
REPO="${GH_REPO:-}"
if [ -z "$REPO" ]; then
  REPO="$(gh repo view --json nameWithOwner -q .nameWithOwner 2>/dev/null || true)"
fi
if [ -z "$REPO" ]; then log "cannot tell which repository — nothing done"; exit 0; fi
export GH_REPO="$REPO"

# compare BASE HEAD -> ahead | behind | identical | diverged | "" (unknown).
# "ahead" means HEAD is a descendant of BASE.
compare() {
  gh api "repos/$REPO/compare/$1...$2" -q .status 2>/dev/null || true
}

# The lowest-numbered open issue carrying the label, or "".
open_issue() {
  gh issue list --label "$LABEL" --state open --limit 20 --json number \
    -q 'map(.number) | sort | .[0] // empty' 2>/dev/null || true
}

# Red commits recorded since the issue last opened: walk the body and every
# comment in order, append on a red marker, reset on a green one.
recorded_reds() {
  local n="$1"
  gh issue view "$n" --json body,comments \
    -q '[.body] + [.comments[].body] | .[]' 2>/dev/null \
  | grep -oE '<!-- main-(red|green)-sha: [0-9a-f]{7,40} -->' \
  | awk '/main-green-sha/ { n = 0; next } { split($0, a, " "); reds[++n] = a[3] } END { for (i = 1; i <= n; i++) print reds[i] }'
}

red_text() {
  printf '`main` is red at %s (%s), run attempt %s.\n\nFailed: %s\n\nRun: %s\n\n<!-- main-red-sha: %s -->\n' \
    "$SHORT" "${COMMIT_SUBJECT:-subject unavailable}" "$ATTEMPT" \
    "${FAILED_JOBS:-unknown}" "$RUN_URL" "$SHA"
}

if [ "$STATE" = "red" ]; then
  # A re-run of an old commit can fail after a later push already went green;
  # that is history, not a red `main`, and opening an issue for it would flap.
  latest_green="$(gh api "repos/$REPO/actions/workflows/ci.yml/runs?branch=main&event=push&status=success&per_page=1" \
    -q '.workflow_runs[0].head_sha // empty' 2>/dev/null || true)"
  if [ -n "$latest_green" ] && [ "$(compare "$SHA" "$latest_green")" = "ahead" ]; then
    log "red at $SHORT, but a later commit (${latest_green:0:8}) is already green on main — nothing recorded"
    exit 0
  fi

  body="$(red_text)"
  n="$(open_issue)"
  if [ -n "$n" ]; then
    write gh issue comment "$n" --body "$body" >/dev/null && log "recorded red at $SHORT on open issue #$n"
    exit 0
  fi

  # Reopen a recently closed one rather than open a second: a flaky
  # red/green/red sequence stays in one thread.
  cutoff="$(date -u -d "-${WINDOW_HOURS} hours" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null \
    || date -u -v-"${WINDOW_HOURS}"H +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || true)"
  recent=""
  if [ -n "$cutoff" ]; then
    recent="$(gh issue list --label "$LABEL" --state closed --limit 5 --json number,closedAt \
      -q "map(select(.closedAt >= \"$cutoff\")) | sort_by(.closedAt) | last | .number // empty" 2>/dev/null || true)"
  fi
  if [ -n "$recent" ]; then
    write gh issue reopen "$recent" >/dev/null \
      && write gh issue comment "$recent" --body "Reopened: $body" >/dev/null \
      && log "reopened #$recent for red at $SHORT"
    exit 0
  fi

  write gh label create "$LABEL" --force --color B60205 \
    --description "main's post-merge CI is red (notify-main-red, issue #2073)" >/dev/null || true
  issue_body="$(printf '%s\n\n---\n\nOpened by the `notify-main-red` job. It records each further red push here, closes this issue when a push at or after every recorded red commit goes green, and reopens it if `main` goes red again within %s hours.\n\n**Before fixing this, assign yourself** (`gh issue edit <n> --add-assignee @me`) or link your PR here, so the next agent or person sees the red is owned and does not start a duplicate fix. The dispatching skills check this issue before they dispatch (`.claude/skills/dispatch-base`). Owning a red is CLAUDE.md rule 6; watching a merge'"'"'s run is rule 8 and `.claude/skills/merge-pr`.\n' "$body" "$WINDOW_HOURS")"
  url="$(write gh issue create --title "main is red" --label "$LABEL" --body "$issue_body" || true)"
  created="${url##*/}"
  if [ -z "$created" ]; then log "could not open the issue for red at $SHORT"; exit 0; fi
  log "opened #$created for red at $SHORT"

  # Race: another red run may have opened one between our read and our create.
  # Everyone keeps the lowest number, so exactly the extra ones close themselves.
  [ "${DRY_RUN:-}" = "1" ] && exit 0
  keep="$(open_issue)"
  if [ -n "$keep" ] && [ "$keep" != "$created" ]; then
    gh issue comment "$keep" --body "$body" >/dev/null || true
    gh issue close "$created" --reason "not planned" \
      --comment "Duplicate of #$keep, opened by a concurrent run; this red is recorded there." >/dev/null || true
    log "closed #$created as a duplicate of #$keep"
  fi
  exit 0
fi

# green
n="$(open_issue)"
if [ -z "$n" ]; then log "green at $SHORT, no open issue — nothing to do"; exit 0; fi
reds="$(recorded_reds "$n")"
if [ -z "$reds" ]; then
  log "green at $SHORT, but #$n records no red commit — leaving it for a person"
  exit 0
fi
while IFS= read -r red; do
  status="$(compare "$red" "$SHA")"
  case "$status" in
    ahead|identical) ;;
    *) log "green at $SHORT is not at or after recorded red ${red:0:8} (${status:-unknown}) — #$n stays open"; exit 0 ;;
  esac
done <<<"$reds"

write gh issue close "$n" --reason completed --comment "$(printf '`main` is green again at %s, run attempt %s: %s\n\n<!-- main-green-sha: %s -->\n' \
  "$SHORT" "$ATTEMPT" "$RUN_URL" "$SHA")" >/dev/null && log "closed #$n: green at $SHORT"
exit 0
