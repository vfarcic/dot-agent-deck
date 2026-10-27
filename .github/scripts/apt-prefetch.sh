#!/usr/bin/env bash
# Issue #1934 — download the .debs that `apt-get install <args>` would install
# into apt's archive cache, with a time limit on each attempt, so that the
# install which follows finds them there and downloads nothing.
#
# Usage: apt-prefetch.sh <apt-get install arguments>
#   e.g. apt-prefetch.sh --no-install-recommends libfoo libbar
# Pass the same arguments the install will get, so both resolve the same set.
# The caller runs `apt-get update` first; this script runs it again only after
# changing the mirror list.
#
# Why: on some GitHub runners the image's first apt mirror,
# azure.archive.ubuntu.com, serves .debs at a few kB/s while the same runner's
# index fetch and its other downloads are fast. apt's own timeout fires only
# when no data arrives at all, so a trickle never trips it: `desktop-browser`
# and `desktop-driver` sat on such a mirror until their 25-minute step limits,
# where a healthy runner fetches the browser job's 142 MB in 5-8 s (17-30
# MB/s). apt reached the list's next mirror only after two stalled tries of
# one file, minutes apart.
#
# So each attempt here gets APT_PREFETCH_ATTEMPT_LIMIT (3 minutes, enough for
# 142 MB at 0.8 MB/s, a twentieth of the slowest healthy rate), and after the first one
# that does not finish, the Azure mirror is dropped from the runner's mirror
# list so the remaining attempts fetch from the next mirror in it
# (archive.ubuntu.com on the images this was written against). Every attempt
# keeps what earlier ones finished — whole .debs stay in the archive cache and
# apt resumes a partial one — so a retry fetches only the remainder. Which
# mirror serves a file does not change what is trusted: apt checks every .deb
# against the index whose signature it verified with Ubuntu's archive key.
set -euo pipefail

if [ "$#" -eq 0 ]; then
  echo "usage: $0 <apt-get install arguments>" >&2
  exit 2
fi

attempts=${APT_PREFETCH_ATTEMPTS:-3}
limit=${APT_PREFETCH_ATTEMPT_LIMIT:-3m}
mirrors=/etc/apt/apt-mirrors.txt

if [ -r "$mirrors" ]; then
  echo "apt mirror list ($mirrors):"
  sed 's/^/  /' "$mirrors"
fi

for attempt in $(seq 1 "$attempts"); do
  started=$SECONDS
  # `timeout` runs as root so it can signal apt-get itself; --kill-after covers
  # an apt-get that ignores the TERM.
  if sudo timeout --kill-after=30s "$limit" \
    apt-get install --download-only -y -q "$@"; then
    echo "apt prefetch: done on attempt $attempt/$attempts in $((SECONDS - started))s."
    exit 0
  else
    status=$?
  fi
  if [ "$status" -eq 124 ]; then
    reason="did not finish within $limit"
  else
    reason="exited $status"
  fi
  echo "::warning::apt prefetch attempt $attempt/$attempts $reason after $((SECONDS - started))s."
  if [ -f "$mirrors" ] && grep -q 'azure\.archive\.ubuntu\.com' "$mirrors" \
    && grep -qv 'azure\.archive\.ubuntu\.com' "$mirrors"; then
    sudo sed -i '/azure\.archive\.ubuntu\.com/d' "$mirrors"
    echo "apt prefetch: dropped the Azure mirror; the remaining attempts use:"
    sed 's/^/  /' "$mirrors"
    # A failed refresh leaves the indexes already fetched, which the next
    # mirror serves the same files for, so it is no reason to stop retrying.
    if ! sudo apt-get update -qq; then
      echo "::warning::apt prefetch: apt-get update failed after the mirror change; retrying with the existing indexes."
    fi
  fi
done

echo "::error::apt prefetch: $attempts attempts of $limit each did not fetch the packages; the mirrors are too slow on this runner."
exit 1
