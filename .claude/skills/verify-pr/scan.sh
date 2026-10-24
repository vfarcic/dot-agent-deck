#!/usr/bin/env bash
#
# Phase 0 of /verify-pr: everything learnable about a PR WITHOUT running its
# code. Runs from the main checkout, creates nothing, touches no worktree.
#
# Usage: scan.sh <pr-number|pr-url>
#
# Emits KEY=value metadata plus the changed files classified into buckets. The
# EXEC_ON_CLONE and CI_SECRETS buckets are why this script runs before any
# other phase: those paths execute code on the REVIEWER's machine (agent hooks,
# build.rs, cargo config, shell scripts) or inside CI with repository
# credentials, so the diff has to be read before a worktree exists and before
# any cargo command is invoked.
#
# EVERYTHING THIS SCRIPT PRINTS ABOUT A PR IS UNTRUSTED TEXT: the title, the
# branch names, the labels, the pathnames, the check names. It is emitted
# through `stream.sh`, which is where the output grammar and the reason for it
# are documented (issue #521). Do not add a field with a bare `echo`.

set -uo pipefail

stream_lib="$(dirname "${BASH_SOURCE[0]}")/stream.sh"
# shellcheck source=stream.sh
if ! . "$stream_lib"; then
  echo "verify-pr: cannot source ${stream_lib}; the skill directory is incomplete" >&2
  exit 1
fi

if [ $# -lt 1 ]; then
  emit ERROR true
  emit MESSAGE "Usage: scan.sh <pr-number|pr-url>"
  exit 0
fi

# Accept "123", "#123", and a full PR URL.
pr="${1#\#}"
pr="${pr##*/}"

if ! [[ "$pr" =~ ^[0-9]+$ ]]; then
  emit ERROR true
  emit MESSAGE "Could not parse a PR number from '$1'"
  exit 0
fi

if ! command -v gh >/dev/null 2>&1; then
  emit ERROR true
  emit MESSAGE "GitHub CLI (gh) is required: https://cli.github.com/"
  exit 0
fi

# `authorAssociation` is NOT a `gh pr view --json` field — it exists only on the
# REST endpoint, queried separately below. Fork detection uses
# `isCrossRepository` rather than comparing repo names, which is what GitHub
# itself keys on.
fields='number,title,url,state,isDraft,author,isCrossRepository,maintainerCanModify,baseRefName,headRefName,headRefOid,headRepository,headRepositoryOwner,mergeable,mergeStateStatus,additions,deletions,changedFiles,labels,createdAt,updatedAt'

# An array of [key, value] pairs, rendered into records by the shared tail in
# `stream.sh`. Adding a field here means adding a pair — the sanitising is not
# something a new field has to remember.
if ! meta=$(gh pr view "$pr" --json "$fields" --jq '
  [
    ["PR_NUMBER", .number],
    ["PR_TITLE", .title],
    ["PR_URL", .url],
    ["PR_STATE", .state],
    ["PR_DRAFT", .isDraft],
    ["PR_AUTHOR", .author.login],
    ["PR_IS_FORK", .isCrossRepository],
    ["PR_MAINTAINER_CAN_MODIFY", .maintainerCanModify],
    ["PR_HEAD_REPO", "\(.headRepositoryOwner.login // "unknown")/\(.headRepository.name // "unknown")"],
    ["PR_BASE_BRANCH", .baseRefName],
    ["PR_HEAD_BRANCH", .headRefName],
    ["PR_HEAD_SHA", .headRefOid],
    ["PR_MERGEABLE", .mergeable],
    ["PR_MERGE_STATE", .mergeStateStatus],
    ["PR_ADDITIONS", .additions],
    ["PR_DELETIONS", .deletions],
    ["PR_CHANGED_FILES", .changedFiles],
    ["PR_LABELS", ([.labels[].name] | join(","))],
    ["PR_CREATED", .createdAt],
    ["PR_UPDATED", .updatedAt]
  ]'"${JQ_RECORDS_TAIL}" 2>&1); then
  emit ERROR true
  emit MESSAGE "gh pr view $pr failed: ${meta}"
  exit 0
fi

printf '%s\n' "$meta"

# REST-only field. Drives the "read the whole diff before running anything"
# decision for authors outside the org.
association=$(gh api "repos/{owner}/{repo}/pulls/${pr}" --jq '.author_association' 2>/dev/null || echo UNKNOWN)
emit PR_AUTHOR_ASSOCIATION "$association"
case "$association" in
  MEMBER | OWNER | COLLABORATOR) emit TRUSTED_AUTHOR true ;;
  *) emit TRUSTED_AUTHOR false ;;
esac

# --- Classify the changed files -------------------------------------------

# Filenames come from the JSON files API rather than `gh pr diff --name-only`
# because git permits a newline in a pathname. As JSON they are typed values
# that can be neutralised BEFORE they reach a line-based stream; in a
# newline-separated list the distinction between "one path containing a
# newline" and "two paths" is already lost by the time bash reads it, and the
# tail of a split path is then classified — and printed — as a file of its own.
# That is the shape a forged `READ_DIFF_BEFORE_RUNNING=none` needs (issue #521).
if ! files=$(gh api "repos/{owner}/{repo}/pulls/${pr}/files" --paginate \
  --jq ".[].filename | ${JQ_ONE_LINE}" 2>&1); then
  emit ERROR true
  emit MESSAGE "gh api repos/{owner}/{repo}/pulls/${pr}/files failed: ${files}"
  exit 0
fi

# First match wins, so the paths that execute code are tested first.
classify() {
  case "$1" in
    .claude/* | .cursor/* | AGENTS.md | CLAUDE.md) echo EXEC_ON_CLONE ;;
    build.rs | */build.rs) echo EXEC_ON_CLONE ;;
    .cargo/* | rust-toolchain* | .envrc) echo EXEC_ON_CLONE ;;
    xtask/* | scripts/* | *.sh) echo EXEC_ON_CLONE ;;
    devbox.json | devbox.lock | Taskfile* | Makefile) echo EXEC_ON_CLONE ;;
    .github/*) echo CI_SECRETS ;;
    Cargo.toml | Cargo.lock | */Cargo.toml) echo DEPS ;;
    src/daemon*.rs | src/hook*.rs | src/connect.rs | src/remote.rs) echo PROTOCOL ;;
    src/dispatch.rs | src/issue_dispatch*.rs | src/scheduler.rs) echo PROTOCOL ;;
    src/orchestrator_ext.rs | src/mode_manager.rs | src/spawn.rs) echo PROTOCOL ;;
    src/agent_pty.rs | src/agent_registry.rs | src/wrap.rs) echo PROTOCOL ;;
    src/*hooks_manage.rs | src/schedule_cli.rs | src/event.rs) echo PROTOCOL ;;
    src/ui.rs | src/pane*.rs | src/tab*.rs | src/terminal_widget.rs) echo UI ;;
    src/embedded_pane.rs | src/keybindings.rs | src/palette.rs) echo UI ;;
    src/features.rs | src/hyperlink.rs) echo UI ;;
    tests/snapshots/*) echo UI_SNAPSHOT ;;
    tests/CATALOG.md) echo CATALOG ;;
    tests/*) echo TESTS ;;
    changelog.d/*) echo CHANGELOG ;;
    prds/*) echo PRD ;;
    docs/develop/*) echo DOCS_DEVELOP ;;
    docs/* | site/* | *.md) echo DOCS ;;
    src/*) echo SRC ;;
    *) echo OTHER ;;
  esac
}

# One `<bucket>\t<path>` line per changed file, rather than the associative
# array this used to build. `declare -A` is bash 4, and a maintainer on macOS
# gets /bin/bash 3.2, where it fails and the FOLLOWING subscript is then
# evaluated arithmetically — so `bucket_files[EXEC_ON_CLONE]` read an unset
# name, `set -u` killed the script mid-stream, and Phase 0 emitted no
# `READ_DIFF_BEFORE_RUNNING` line AT ALL. A reviewing agent is told to act on
# that field; the way it failed was to not exist. Reproduced under a real bash
# 3.2 — and it is not new: the version of this script before issue #521 dies
# the same way, inside the classification loop itself.
#
# The split is on the FIRST tab, so a tab inside a pathname stays in the path.
classified=""
file_count=0
while IFS= read -r f; do
  [ -z "$f" ] && continue
  classified="${classified}$(classify "$f")"$'\t'"${f}"$'\n'
  file_count=$((file_count + 1))
done <<<"$files"

# The paths in one bucket, one per line; empty when the bucket is empty.
bucket_lines() { # <BUCKET>
  printf '%s' "$classified" |
    awk -F'\t' -v b="$1" '$1 == b { print substr($0, index($0, "\t") + 1) }'
}

has_bucket() { # <BUCKET>
  [ -n "$(bucket_lines "$1")" ]
}

# Does the list we classified match what the PR says it changed? The files API
# caps a PR at 3000 entries and pagination can be cut short, and either way a
# short list under-reports the gate below — the one direction that must never
# happen silently. A PR pushed to mid-scan also lands here; over-reporting is
# the safe side of that trade.
claimed=$(printf '%s\n' "$meta" | sed -n 's/^PR_CHANGED_FILES=//p')
file_list_complete=unknown
case "$claimed" in
  '' | *[!0-9]*) : ;;
  *) [ "$claimed" -eq "$file_count" ] && file_list_complete=true || file_list_complete=false ;;
esac
emit FILE_LIST_COMPLETE "$file_list_complete"

# A hard gate the reviewing agent must honour: these paths run outside the test
# command, so their diff has to be read before anything is built or executed.
#
# A plain string, not an array: under `set -u`, bash 3.2 treats `${arr[*]}` on
# an EMPTY array as an unbound reference and exits — and "no bucket tripped the
# gate" is exactly when this one is empty, so the safe case is the one that
# would have died.
read_first=""
for b in EXEC_ON_CLONE CI_SECRETS; do
  has_bucket "$b" && read_first="${read_first:+${read_first} }${b}"
done
# An incomplete list cannot say "nothing here executes on clone" — it can only
# say it did not see one.
[ "$file_list_complete" = true ] || read_first="${read_first:+${read_first} }INCOMPLETE_FILE_LIST"
emit READ_DIFF_BEFORE_RUNNING "${read_first:-none}"

# Rule 12 (cross-version contract) and rule 4 (TUI test ladder) triggers.
has_bucket PROTOCOL && emit RULE_12_TRIGGERED true || emit RULE_12_TRIGGERED false
if has_bucket UI || has_bucket UI_SNAPSHOT; then
  emit RULE_4_TRIGGERED true
else
  emit RULE_4_TRIGGERED false
fi
has_bucket CHANGELOG && emit CHANGELOG_FRAGMENT_PRESENT true || emit CHANGELOG_FRAGMENT_PRESENT false
has_bucket TESTS && emit TESTS_TOUCHED true || emit TESTS_TOUCHED false
has_bucket DEPS && emit DEPS_TOUCHED true || emit DEPS_TOUCHED false

for b in EXEC_ON_CLONE CI_SECRETS DEPS PROTOCOL UI UI_SNAPSHOT CATALOG TESTS \
  CHANGELOG PRD DOCS_DEVELOP DOCS SRC OTHER; do
  lines=$(bucket_lines "$b")
  if [ -n "$lines" ]; then
    emit_header "${b}"
    printf '%s\n' "$lines" | emit_block
  fi
done

# --- Signal that already exists on the PR ---------------------------------

emit_header "CI CHECKS (gh pr checks)"
# Exits non-zero when a check is failing or pending, which is information, not
# an error for this script. Check names come from the PR head's workflows on a
# fork, so this is untrusted text like everything else.
gh pr checks "$pr" 2>&1 | emit_block || true

# Workflow runs held for approval. GitHub withholds Actions runs on a fork PR
# from a first-time contributor until a maintainer approves them, so a PR can
# look "checked" (only `label` / a review bot ran) while every real CI job —
# Windows, macOS, `cargo audit` — never executed. Measured on #334: CI and Docs
# sat at `action_required` on every head commit for two days and nobody noticed,
# which made a local Linux-only run the sole verification. Phase 1b decides
# whether approving them is safe.
head_sha=$(printf '%s\n' "$meta" | sed -n 's/^PR_HEAD_SHA=//p')
awaiting=$(gh api "repos/{owner}/{repo}/actions/runs?head_sha=${head_sha}" \
  --jq '[.workflow_runs[] | select(.conclusion == "action_required" or .status == "action_required")] | length' 2>/dev/null || echo unknown)
emit WORKFLOWS_AWAITING_APPROVAL "${awaiting:-unknown}"
if [ "${awaiting:-0}" != "0" ] && [ "${awaiting:-unknown}" != "unknown" ]; then
  emit_header "RUNS AWAITING APPROVAL (id / name)"
  gh api "repos/{owner}/{repo}/actions/runs?head_sha=${head_sha}" \
    --jq ".workflow_runs[] | select(.conclusion == \"action_required\" or .status == \"action_required\") | \"\(.id)\t\(.name | ${JQ_ONE_LINE})\"" 2>/dev/null |
    emit_block || true
  emit APPROVE_WITH "gh api --method POST repos/{owner}/{repo}/actions/runs/<run_id>/approve"
fi

# Issue #1256: which automated review apps have finished on this head, as
# pr-create's wait-reviews.sh decides it from reviewers.json. `--no-wait`,
# because this phase reports and never waits. One record per outcome, not per
# app: a key appears once in this stream (stream.sh).
review_done=""
review_not_done=""
reviews_at_head=unknown
while IFS= read -r line; do
  case "$line" in
    DONE=*) entry="${line#DONE=}"; review_done="${review_done:+${review_done}, }${entry%% *}" ;;
    PENDING=* | TIMED_OUT=*) entry="${line#*=}"; review_not_done="${review_not_done:+${review_not_done}, }${entry%% *}" ;;
    REVIEWS=*) reviews_at_head="${line#REVIEWS=}" ;;
  esac
done < <(bash "$(dirname "${BASH_SOURCE[0]}")/../pr-create/wait-reviews.sh" --no-wait "$pr" 2>/dev/null)
emit REVIEWS_AT_HEAD "$reviews_at_head"
emit REVIEWS_FINISHED "${review_done:-none}"
emit REVIEWS_NOT_FINISHED "${review_not_done:-none}"

# Rule 8: the findings live ONLY in the inline review comments, for every app.
# A finished review, or a green `Greptile Review` check-run, is NOT the review.
inline=$(gh api "repos/{owner}/{repo}/pulls/${pr}/comments" --paginate --jq '.[].id' 2>/dev/null | wc -l | tr -d ' ')
emit INLINE_REVIEW_COMMENTS "${inline:-unknown}"
emit READ_INLINE_WITH "gh api repos/{owner}/{repo}/pulls/${pr}/comments --paginate"

emit SUCCESS true
