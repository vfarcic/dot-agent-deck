#!/usr/bin/env bash
#
# upload.sh — isolated YouTube hosting for the demo-reel engine.
#
# This is the ONLY file that knows how the reel is hosted. Swapping YouTube for
# another host means rewriting this one script; reel.sh never names a host.
#
# It uploads an MP4 to YouTube as PRIVATE via the YouTube Data API v3 and
# prints the resulting watch URL on stdout.
#
# PRIVATE IS THE DEFAULT ON PURPOSE, and it is a security boundary rather than a
# preference. A reel clip is only eligible if the run spun up a REAL agent, so
# every cast it stitches was recorded by a process holding live credentials, and
# the watch URL goes into the PR body *and* the changelog fragment, which flows
# into the public release notes. Unlisted means "anyone with the link can watch",
# so uploading unlisted put a credential-bearing recording on an automated public
# URL with no human in between. Private means only the channel owner (and
# accounts they name) can watch it — the owner can always watch their OWN private
# videos, so an unattended agent still uploads and still hands over a working
# link; beyond that owner and the accounts they deliberately share it with,
# nothing is visible until a human flips it. The video id survives a private ->
# unlisted flip, so the link already in the PR and the changelog starts working
# with no re-upload.
#
# What private does NOT do: the upload has already happened, so the bytes are on
# Google's servers either way. Private CONTAINS a leak to the owner's own
# account; it does not undo one.
#
# `--privacy` is the explicit escape hatch for asking for another status. The
# engine (reel.sh --publish, and so the adapter) never passes it, so the
# automated path cannot produce anything but a private video.
#
# Credentials are read from the environment (sourced from vals/.env.vals.yaml in
# this repo; never hardcoded):
#
#   YOUTUBE_CLIENT_ID      OAuth client id
#   YOUTUBE_CLIENT_SECRET  OAuth client secret
#   YOUTUBE_REFRESH_TOKEN  OAuth refresh token (minted once via human consent)
#
# Runtime upload errors (expired/revoked token, quota exhausted, API disabled)
# are only knowable here, at upload time. This script passes the API's raw
# error body THROUGH to stderr and exits non-zero rather than swallowing it.
#
# Usage:
#   upload.sh [--privacy private|unlisted|public] VIDEO.mp4 [TITLE] [DESCRIPTION]
#
set -euo pipefail

SCRIPT_NAME="$(basename "$0")"

die() {
  echo "$SCRIPT_NAME: error: $*" >&2
  exit 1
}

USAGE="usage: $SCRIPT_NAME [--privacy private|unlisted|public] VIDEO.mp4 [TITLE] [DESCRIPTION]"

# The YouTube privacy status the video is CREATED with. Private unless a caller
# says otherwise, for the reason in the header — this default is the only thing
# that keeps an automated upload of a credential-bearing recording off a
# publicly-reachable URL.
PRIVACY="private"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --privacy)
      [[ $# -ge 2 ]] || die "--privacy needs a value (private|unlisted|public)"
      PRIVACY="$2"
      shift 2
      ;;
    --privacy=*)
      PRIVACY="${1#--privacy=}"
      shift
      ;;
    --) shift; break ;;
    -*) die "unknown option: $1 ($USAGE)" ;;
    *) break ;;
  esac
done

VIDEO="${1:-}"
TITLE="${2:-Demo reel}"
DESCRIPTION="${3:-}"

# Validated against the three statuses the API defines rather than passed
# through: a typo ("privat", "Private") would otherwise be rejected by the API
# only AFTER the bytes were sent, or — worse for a future API revision — accepted
# as something wider than intended.
case "$PRIVACY" in
  private | unlisted | public) ;;
  *) die "invalid --privacy value: $PRIVACY (expected private, unlisted or public)" ;;
esac

[[ -n "$VIDEO" ]] || die "$USAGE"
[[ -f "$VIDEO" ]] || die "video file not found: $VIDEO"

for var in YOUTUBE_CLIENT_ID YOUTUBE_CLIENT_SECRET YOUTUBE_REFRESH_TOKEN; do
  [[ -n "${!var:-}" ]] || die "missing required credential: $var"
done

command -v curl >/dev/null 2>&1 || die "curl is required but is not on PATH"
command -v jq   >/dev/null 2>&1 || die "jq is required but is not on PATH"

# Every temp file — secret material (the OAuth values, the bearer header) and
# curl scratch (response headers) — is tracked here and removed on ANY exit,
# including Ctrl-C (INT) and termination (TERM), so secrets never linger on disk.
TMP_FILES=()
cleanup() { [[ ${#TMP_FILES[@]} -gt 0 ]] && rm -f "${TMP_FILES[@]}"; }
trap cleanup EXIT INT TERM

# Create a fresh 0600 temp file and store its path in the named variable.
# Secrets are handed to curl via @file forms instead of on the command line,
# because anything in argv is world-readable through `ps` and
# /proc/PID/cmdline for as long as the process runs.
new_tmp() {               # new_tmp VARNAME
  local -n _slot="$1"
  _slot="$(mktemp)"
  TMP_FILES+=("$_slot")
  chmod 600 "$_slot"
}

# ---- 1. Refresh token -> short-lived access token ----------------------
# A plain OAuth token-endpoint POST. --fail-with-body makes curl exit non-zero
# on an HTTP error while still printing the response body, so a revoked client
# or bad refresh token surfaces the API's own error text rather than a blank.
#
# The three OAuth secrets are written to 0600 files (no trailing newline) and
# passed via `--data-urlencode key@file`, which reads the value from the file —
# keeping the secrets out of this process's argv.
new_tmp client_id_file;     printf '%s' "$YOUTUBE_CLIENT_ID"     > "$client_id_file"
new_tmp client_secret_file; printf '%s' "$YOUTUBE_CLIENT_SECRET" > "$client_secret_file"
new_tmp refresh_token_file; printf '%s' "$YOUTUBE_REFRESH_TOKEN" > "$refresh_token_file"

token_response="$(
  curl --silent --show-error --fail-with-body \
    --request POST "https://oauth2.googleapis.com/token" \
    --data-urlencode "client_id@${client_id_file}" \
    --data-urlencode "client_secret@${client_secret_file}" \
    --data-urlencode "refresh_token@${refresh_token_file}" \
    --data-urlencode "grant_type=refresh_token"
)" || die "token refresh failed: ${token_response}"

access_token="$(printf '%s' "$token_response" | jq -r '.access_token // empty')"
# A 2xx body that nonetheless carries no access_token may itself hold token
# material in another field, so fail WITHOUT echoing the body (the genuine
# curl-failure path above keeps passing the API error through — it has no token).
[[ -n "$access_token" ]] || die "token refresh returned a 2xx response with no access_token (body withheld — it may contain a token)"

# The bearer header is written as a complete "Authorization: Bearer <token>"
# line to a 0600 file and passed via `curl -H @file`, keeping the access token
# off the command line in the two API calls below.
new_tmp auth_header_file
printf 'Authorization: Bearer %s' "$access_token" > "$auth_header_file"

# ---- 2. Start a resumable upload session -------------------------------
# The metadata sets privacyStatus from $PRIVACY, which is `private` unless the
# caller explicitly asked for another status. The initiating POST returns the
# upload URL in the Location response header (captured to a header file) while
# the response body is captured to a variable — on success the body is empty,
# on failure --fail-with-body puts the API's error JSON there so it is passed
# through rather than discarded.
metadata="$(
  jq -n --arg title "$TITLE" --arg desc "$DESCRIPTION" --arg privacy "$PRIVACY" \
    '{snippet: {title: $title, description: $desc}, status: {privacyStatus: $privacy}}'
)"

new_tmp header_file

init_body="$(
  curl --silent --show-error --fail-with-body -D "$header_file" \
    --request POST \
    "https://www.googleapis.com/upload/youtube/v3/videos?uploadType=resumable&part=snippet,status" \
    -H "@${auth_header_file}" \
    -H "Content-Type: application/json; charset=UTF-8" \
    -H "X-Upload-Content-Type: video/*" \
    --data "$metadata"
)" || die "failed to start resumable upload: ${init_body}"

upload_url="$(
  tr -d '\r' < "$header_file" \
    | awk 'tolower($1) == "location:" { print $2; exit }'
)"
[[ -n "$upload_url" ]] || die "resumable upload returned no Location header (body: ${init_body})"

# ---- 3. Upload the bytes ----------------------------------------------
# PUT the file to the session URL. The success body carries the video id.
upload_response="$(
  curl --silent --show-error --fail-with-body \
    --request PUT "$upload_url" \
    -H "@${auth_header_file}" \
    -H "Content-Type: video/*" \
    --data-binary "@${VIDEO}"
)" || die "upload failed: ${upload_response}"

video_id="$(printf '%s' "$upload_response" | jq -r '.id // empty')"
[[ -n "$video_id" ]] || die "upload returned no video id: ${upload_response}"

# The only thing on stdout is the watch URL, so callers can capture it cleanly.
echo "https://youtu.be/${video_id}"
