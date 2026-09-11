#!/usr/bin/env bash
# Send one fire-and-forget Telegram notification (issue #1015).
#
# Replaces the `telegram` MCP server that used to carry these messages. That
# server long-polled `getUpdates` to support an inbound side this repo forbids
# reading, and the polling loop was the sole reason the process outlived its
# parent: orphaned to PPid 1 it kept spinning, once for 36 hours at ~104% of a
# core. A curl call has no resident process, so there is nothing to orphan,
# nothing to contend for the bot token's single poller slot, and nothing to reap.
#
# Two properties that used to be instructions an agent had to follow are now
# structural, which is the main reason this is a script and not a prompt rule:
#
#   * `chat_id` is always explicit. The MCP send tools fell back to the MOST
#     RECENTLY ACTIVE chat when it was omitted, so anyone who messaged the bot
#     first received the next notification. Here it is a required parameter and
#     an unset TELEGRAM_CHAT_ID skips the send instead of retargeting it.
#   * There is no inbound path at all. `getUpdates` was an unauthenticated
#     inbound channel and therefore a prompt-injection route; this script can
#     only send, so the rule cannot be broken by an agent that ignores it.
#
# Fire-and-forget: ALWAYS exits 0. A failed send is a lost notification, never a
# workflow event. Never blocks longer than TELEGRAM_TIMEOUT_SECS.
#
# Usage:  scripts/notify.sh "dot-agent-deck PRD #123 — DONE: merged & closed"
#         echo "message" | scripts/notify.sh
#
# Env:    TELEGRAM_BOT_TOKEN  required; without it the send is skipped
#         TELEGRAM_CHAT_ID    required; without it the send is skipped
#         TELEGRAM_TIMEOUT_SECS  default 10
#         NOTIFY_LOG          default .dot-agent-deck/notify-log.md

set -uo pipefail   # deliberately NOT -e: this script must never fail a workflow

MESSAGE="${1:-}"
if [ -z "$MESSAGE" ] && [ ! -t 0 ]; then MESSAGE="$(cat)"; fi

TIMEOUT="${TELEGRAM_TIMEOUT_SECS:-10}"
LOG="${NOTIFY_LOG:-.dot-agent-deck/notify-log.md}"
TS="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

# Append one line to the expectation log. Best-effort: never delays or blocks.
log_line() {
  local outcome="$1"
  mkdir -p "$(dirname "$LOG")" 2>/dev/null || return 0
  printf '| %s | %s | %s |\n' "$TS" "${MESSAGE:0:80}" "$outcome" >>"$LOG" 2>/dev/null || true
}

if [ -z "$MESSAGE" ]; then
  echo "notify: refusing to send an empty message" >&2
  log_line "send=skipped: empty message"
  exit 0
fi

# Both are required. Skipping beats retargeting: see the chat_id note above.
if [ -z "${TELEGRAM_BOT_TOKEN:-}" ]; then
  echo "notify: TELEGRAM_BOT_TOKEN unset — skipping send" >&2
  log_line "send=skipped: TELEGRAM_BOT_TOKEN unset"
  exit 0
fi
if [ -z "${TELEGRAM_CHAT_ID:-}" ]; then
  echo "notify: TELEGRAM_CHAT_ID unset — skipping send" >&2
  log_line "send=skipped: TELEGRAM_CHAT_ID unset"
  exit 0
fi

if ! command -v curl >/dev/null 2>&1; then
  echo "notify: curl not found — skipping send" >&2
  log_line "send=skipped: curl not found"
  exit 0
fi

# --data-urlencode keeps the message out of the URL, so it survives newlines and
# shell metacharacters. The token stays in the URL path, so never echo RESPONSE
# on a failure path that could include the request line.
RESPONSE="$(
  curl --silent --show-error --max-time "$TIMEOUT" \
    --data-urlencode "chat_id=${TELEGRAM_CHAT_ID}" \
    --data-urlencode "text=${MESSAGE}" \
    "https://api.telegram.org/bot${TELEGRAM_BOT_TOKEN}/sendMessage" 2>&1
)"
CURL_RC=$?

if [ $CURL_RC -ne 0 ]; then
  echo "notify: send failed (curl exit $CURL_RC)" >&2
  log_line "send=failed: curl exit $CURL_RC"
  exit 0
fi

# Pull message_id without requiring jq — it closes the "sent but never arrived"
# gap, so it is worth extracting even on a host with no JSON tooling.
MESSAGE_ID="$(printf '%s' "$RESPONSE" | grep -o '"message_id":[0-9]*' | head -1 | cut -d: -f2)"

if [ -n "$MESSAGE_ID" ]; then
  echo "notify: sent (message_id=$MESSAGE_ID)"
  log_line "message_id=$MESSAGE_ID"
else
  # Telegram reports application errors in the body with HTTP 200, so a
  # successful curl is not a successful send. Report the description only.
  DESC="$(printf '%s' "$RESPONSE" | grep -o '"description":"[^"]*"' | head -1 | cut -d: -f2- | tr -d '"')"
  echo "notify: send rejected by Telegram: ${DESC:-unknown error}" >&2
  log_line "send=failed: ${DESC:-unknown error}"
fi

exit 0
