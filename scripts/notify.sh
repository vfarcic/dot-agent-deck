#!/usr/bin/env bash
# Agent-driven notification helper — PRD #126 dogfood (no deck code).
#
# Usage:  scripts/notify.sh <gate|done|blocked> <role> <message...>
# Example: scripts/notify.sh gate orchestrator 'test plan posted for PRD #126'
#
# FIRE-AND-FORGET. This script never blocks and never fails its caller: it always
# exits 0, even with no network, no curl, and no arguments. Callers must ignore
# its result and continue.
#
# Every invocation TRIES to append TWO records to the expectation log
# (.dot-agent-deck/notify-log.md, gitignored):
#
#   1. `reached`  — written BEFORE the send is attempted, so it survives a dead
#                   network, a missing curl, or the agent being killed mid-send.
#   2. `send=...` — written AFTER the attempt, with the transport outcome.
#
# Both rows carry the same invocation id, so `reached` and `send=...` stay
# correlatable when two agents notify in the same second.
#
# The log's Markdown header is created atomically, so parallel agents cannot each
# write one — see init_log below.
#
# Logging is BEST-EFFORT: a read-only checkout, a bad DOT_AGENT_DECK_NOTIFY_LOG,
# or a full filesystem can leave zero or one row. Such a failure never changes
# the exit code, but it IS reported on stderr ("notify.sh: could not append …"),
# so "the agent never invoked the helper" stays distinguishable from "the helper
# ran but could not log".
#
# The delta between the two counts is the experiment's data. See
# docs/develop/notifications-dogfood.md for the reconciliation procedure.
#
# The destination is a PUBLIC ntfy.sh topic: anyone who knows the topic name can
# read it and publish to it. Send only workflow status (role, event kind, a short
# human sentence). Never send secrets, tokens, diffs, or file contents.

set -u

server="${DOT_AGENT_DECK_NOTIFY_SERVER:-https://ntfy.sh}"
topic="${DOT_AGENT_DECK_NOTIFY_TOPIC:-dot-agent-deck-notify-0c0d15e13936d122}"

script_dir="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
repo_root="$(dirname -- "$script_dir")"
log="${DOT_AGENT_DECK_NOTIFY_LOG:-$repo_root/.dot-agent-deck/notify-log.md}"

kind="${1:-unknown}"
role="${2:-unknown}"
shift 2 2>/dev/null || true
message="${*:-(no message)}"

# One event must be one line: collapse newlines and pipes so the log stays a
# valid Markdown table and stays greppable.
sanitize() { printf '%s' "$1" | tr '\n\r|' '   ' | tr -s ' '; }
kind="$(sanitize "$kind")"
role="$(sanitize "$role")"
message="$(sanitize "$message")"
# The topic comes from the environment and lands in both the request URL and the
# log's detail column, so it gets the same treatment; spaces additionally go
# away because a topic name cannot contain one and a URL must not.
topic="$(sanitize "$topic" | tr -d ' ')"

now() { date -u +%Y-%m-%dT%H:%M:%SZ; }

# One id per invocation, generated before the first append and carried on BOTH
# rows, so `reached` and `send=...` stay correlatable when two agents notify in
# the same second. Epoch + PID + $RANDOM: PID alone is not enough because a
# sandboxed agent runs in its own PID namespace and reliably gets a low PID
# (Codex reports PID 2), so PIDs collide across agents.
invocation="$(printf 'inv-%x-%x-%04x' "$(date -u +%s)" "$$" "$((RANDOM))")"

note() { printf 'notify.sh: %s\n' "$1" >&2; }
log_failures=0

# Create the log with its header AT MOST ONCE, even when several agents notify at
# the same instant — the normal case in this orchestration. A duplicate header
# would break the line-based reconciliation, so the header is published
# ATOMICALLY instead of being guarded by a check-then-write: each racer writes the
# header into a private temp file and then hard-links it into place, and `ln`
# fails if the log already exists. Exactly one racer wins; the losers fail
# silently and go straight to appending their row. Because the log only becomes
# visible complete, no racer can append a row into a half-written header either.
# Nothing here takes a lock or waits, so it cannot block a caller.
header() {
  printf '# Notify expectation log — PRD #126 dogfood\n\n'
  printf 'Appended by `scripts/notify.sh`. Two records per notifiable moment, sharing one invocation id: `reached` (written first) and `send=...` (written after the send attempt). Logging is best-effort — see the script header. Reconcile per `docs/develop/notifications-dogfood.md`.\n\n'
  printf '| timestamp (UTC) | invocation | role | kind | record | detail |\n'
  printf '|---|---|---|---|---|---|\n'
}

init_log() {
  tmp="${log}.init.${invocation}"
  if ( set -C; header >"$tmp" ) 2>/dev/null; then
    # Losing this `ln` is the expected outcome for every racer but one: it means
    # another invocation already published an identical header. Not a failure.
    ln -- "$tmp" "$log" 2>/dev/null || true
  fi
  rm -f -- "$tmp" 2>/dev/null || true
  # A filesystem without hard links (some network/FUSE mounts) fails every `ln`,
  # which would leave the log headerless forever. Fall back to an exclusive
  # create, which is still non-blocking and still admits only one writer.
  if [ ! -s "$log" ]; then
    ( set -C; header >"$log" ) 2>/dev/null || true
  fi
}

# Logging is best-effort — it must never change the exit code — but a failure is
# reported on stderr so "never invoked" stays distinguishable from "invoked but
# could not log".
append() { # append <record> <detail>
  if ! mkdir -p -- "$(dirname -- "$log")" 2>/dev/null; then
    log_failures=$((log_failures + 1))
    note "could not create the log directory for ${log}; '$1' row not recorded"
    return 1
  fi
  if [ ! -s "$log" ]; then
    init_log
    # A missing header does not cost us the row, so it is not counted as a lost
    # row — but it is still worth saying, because the table will render oddly.
    # The usual cause is a leftover zero-byte log: an atomic create cannot claim a
    # file that already exists, so delete it and the next invocation rebuilds it.
    if [ ! -s "$log" ]; then
      note "could not initialize the log header in ${log} (delete the file if it exists but is empty); rows are appended without one"
    fi
  fi
  if ! printf '| %s | %s | %s | %s | %s | %s |\n' \
    "$(now)" "$invocation" "$role" "$kind" "$1" "$2" >>"$log" 2>/dev/null; then
    log_failures=$((log_failures + 1))
    note "could not append the '$1' row to ${log}"
    return 1
  fi
  return 0
}

# Record 1: the notifiable moment was reached.
append 'reached' "$message"

# Record 2: the send attempt.
if ! command -v curl >/dev/null 2>&1; then
  append 'send=skipped' 'curl not on PATH'
else
  # --data-raw, never --data-binary: --data-binary treats a leading `@` as "read
  # this local file", so a message like `@/etc/passwd` would POST file contents
  # to the public topic. --data-raw sends the `@` literally.
  http="$(
    curl --silent --show-error --max-time 5 --output /dev/null \
      --write-out '%{http_code}' \
      --header "Title: dot-agent-deck ${kind} (${role})" \
      --data-raw "$message" \
      "${server%/}/${topic}" 2>/dev/null
  )"
  curl_status=$?

  if [ "$curl_status" -eq 0 ] && [ "$http" = "200" ]; then
    append 'send=ok' "http=200 topic=${topic}"
  else
    append 'send=failed' "http=${http:-none} curl_exit=${curl_status} topic=${topic}"
  fi
fi

if [ "$log_failures" -ne 0 ]; then
  note "${log_failures} of 2 expectation-log rows were lost for ${invocation}; reconciliation of this invocation is incomplete (exit code deliberately unchanged)"
fi

exit 0
