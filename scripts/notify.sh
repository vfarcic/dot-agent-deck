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
# Every invocation appends TWO records to the expectation log
# (.dot-agent-deck/notify-log.md, gitignored):
#
#   1. `reached`  — written BEFORE the send is attempted, so it survives a dead
#                   network, a missing curl, or the agent being killed mid-send.
#   2. `send=...` — written AFTER the attempt, with the transport outcome.
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

now() { date -u +%Y-%m-%dT%H:%M:%SZ; }

append() { # append <record> <detail>
  mkdir -p -- "$(dirname -- "$log")" 2>/dev/null || return 0
  if [ ! -s "$log" ]; then
    {
      printf '# Notify expectation log — PRD #126 dogfood\n\n'
      printf 'Appended by `scripts/notify.sh`. Two records per notifiable moment: `reached` (written first, always) and `send=...` (written after the send attempt). Reconcile per `docs/develop/notifications-dogfood.md`.\n\n'
      printf '| timestamp (UTC) | role | kind | record | detail |\n'
      printf '|---|---|---|---|---|\n'
    } >>"$log" 2>/dev/null || return 0
  fi
  printf '| %s | %s | %s | %s | %s |\n' "$(now)" "$role" "$kind" "$1" "$2" >>"$log" 2>/dev/null || true
}

# Record 1: the notifiable moment was reached.
append 'reached' "$message"

# Record 2: the send attempt.
if ! command -v curl >/dev/null 2>&1; then
  append 'send=skipped' 'curl not on PATH'
  exit 0
fi

http="$(
  curl --silent --show-error --max-time 5 --output /dev/null \
    --write-out '%{http_code}' \
    --header "Title: dot-agent-deck ${kind} (${role})" \
    --data-binary "$message" \
    "${server%/}/${topic}" 2>/dev/null
)"
curl_status=$?

if [ "$curl_status" -eq 0 ] && [ "$http" = "200" ]; then
  append 'send=ok' "http=200 topic=${topic}"
else
  append 'send=failed' "http=${http:-none} curl_exit=${curl_status} topic=${topic}"
fi

exit 0
