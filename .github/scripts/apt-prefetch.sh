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
# 142 MB at 0.8 MB/s, a twentieth of the slowest healthy rate), and after each
# one that does not finish, the mirror at the top of the runner's mirror list is
# dropped so the next attempt fetches from the one after it. It used to drop
# only azure.archive.ubuntu.com, the first mirror on the images this was written
# against. The ubuntu-26.04 image lists a regional EC2 mirror first instead
# (us-east-2.ec2.archive.ubuntu.com on 2026-11-18), and when that one trickled,
# all three attempts stayed on it and `desktop-driver` went red on `main` (run
# 44262375554) with archive.ubuntu.com unused second in the list. A mirror is
# dropped only while another archive mirror stays below it:
# security.ubuntu.com, last in the list, serves only the security pocket, so
# it is never the one left alone.
#
# A runner with no mirror list gets the same move another way. RWX's
# ubuntu:24.04 image names one mirror, us-east-2.ec2.archive.ubuntu.com, in
# /etc/apt/sources.list.d/ubuntu.sources. On 2026-11-19 RWX's `desktop-driver`
# fetched from it at 58.8 kB/s for ten minutes and then got 503s (PR #2386's
# run 01a1f3a3d831), with nowhere else to go. So when there is no mirror list
# and that file points the archive at a regional *.archive.ubuntu.com mirror,
# a stalled attempt rewrites those URIs to archive.ubuntu.com. A
# security.ubuntu.com stanza, and GitHub's `mirror+file:` URIs, do not match
# and are left alone.
#
# Every attempt keeps what earlier ones finished — whole .debs stay in the archive cache and
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
# Overridable for xtask/linkage-check's apt_prefetch tests only.
mirrors=${APT_PREFETCH_MIRRORS:-/etc/apt/apt-mirrors.txt}
sources=${APT_PREFETCH_SOURCES:-/etc/apt/sources.list.d/ubuntu.sources}
# A regional archive mirror's URL (a host ending in .archive.ubuntu.com, so
# archive.ubuntu.com itself does not match), as one of the URLs a deb822
# `URIs:` field lists; the field may list several (Qodo on #2386).
regional='https?://[^[:space:]/]+\.archive\.ubuntu\.com/ubuntu/?'
fallback=http://archive.ubuntu.com/ubuntu/

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
  # The archive mirrors below the first one; security.ubuntu.com is not one.
  others=0
  if [ -f "$mirrors" ]; then
    others=$(awk 'NR > 1 && NF && !/security\.ubuntu\.com/ { n++ } END { print n + 0 }' "$mirrors")
  fi
  changed=false
  if [ "$attempt" -lt "$attempts" ] && [ "$others" -gt 0 ]; then
    dropped=$(head -n 1 "$mirrors" | cut -f1)
    sudo sed -i 1d "$mirrors"
    echo "apt prefetch: dropped $dropped; the remaining attempts use:"
    sed 's/^/  /' "$mirrors"
    changed=true
  elif [ "$attempt" -lt "$attempts" ] && [ ! -f "$mirrors" ] && [ -f "$sources" ] \
    && grep -Eq "^URIs:(.*[[:space:]])?$regional([[:space:]]|\$)" "$sources"; then
    echo "apt prefetch: $sources named a regional mirror; the remaining attempts use $fallback:"
    # Each regional URL in a URIs field becomes the fallback; the field's
    # other URLs stay. The loop catches two regional URLs one space apart,
    # where the first match consumes the space the second needs.
    sudo sed -E -i -e '/^URIs:/{' -e ':a' \
      -e "s#(^URIs:|[[:space:]])$regional([[:space:]]|\$)#\1$fallback\2#" \
      -e 'ta' -e '}' "$sources"
    grep '^URIs:' "$sources" | sed 's/^/  /'
    changed=true
  fi
  if [ "$changed" = true ]; then
    # A failed refresh leaves the indexes already fetched, which the next
    # mirror serves the same files for, so it is no reason to stop retrying.
    if ! sudo apt-get update -qq; then
      echo "::warning::apt prefetch: apt-get update failed after the mirror change; retrying with the existing indexes."
    fi
  fi
done

echo "::error::apt prefetch: $attempts attempts of $limit each did not fetch the packages; the mirrors are too slow on this runner."
exit 1
