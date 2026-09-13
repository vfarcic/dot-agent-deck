#!/usr/bin/env bash
# Reap orphaned processes this machine's agent tooling leaves behind (issue #1015).
#
# Machine-level and independent of the deck: it knows nothing about panes, roles
# or the daemon, and reads only /proc. It is a NET, not a fix — with the polling
# MCP server gone (see scripts/notify.sh) it should never fire for that cause.
#
# Defaults to a DRY RUN, like `cargo xtask clean-e2e-tmp`, because everything it
# does is irreversible. Pass --apply to actually signal anything.
#
# Three rules, all scoped to processes that are BOTH owned by the invoking user
# AND orphaned (PPid 1). Nothing else is ever a candidate:
#
#   mcp    (default on)  a positively identified stdio MCP server that has been
#                        orphaned. These are quiet (~63 MB, 0% CPU) but immortal,
#                        because their polling loop holds the event loop open
#                        forever — and a WEDGED one is caught here too, by shape
#                        rather than by CPU, so the default rule still covers the
#                        incident this script was written for.
#   spin   (OPT-IN)      any orphan sustaining CPU at or above --cpu across two
#                        samples. OFF by default and NOT used by the timer: it
#                        cannot tell a wedged agent from a detached build, backup
#                        or compute job you started on purpose, and the installed
#                        timer runs with --apply. Interactive diagnosis only.
#                        --include-spin turns it on.
#   stale  (OPT-IN)      any remaining orphan older than --stale-age. Off for the
#                        same reason, only more so: "old and orphaned" alone is a
#                        weak signal. --include-stale turns it on.
#
# Usage:
#   scripts/reap-orphans.sh                      # dry run, mcp only
#   scripts/reap-orphans.sh --apply              # actually reap
#   scripts/reap-orphans.sh --include-spin       # dry run, + the CPU rule
#   scripts/reap-orphans.sh --include-stale      # dry run, + the age catch-all
#   scripts/reap-orphans.sh --include-spin --cpu 80 --min-age 30

set -uo pipefail

CPU_THRESHOLD=50      # percent of ONE core, sustained across both samples
MIN_AGE_MIN=10        # a candidate must be at least this old
STALE_AGE_MIN=360     # --include-stale: age for the catch-all rule (6h)
SAMPLE_SECS=3
APPLY=0
INCLUDE_SPIN=0
INCLUDE_STALE=0

while [ $# -gt 0 ]; do
  case "$1" in
    --apply)         APPLY=1; shift ;;
    --include-spin)  INCLUDE_SPIN=1; shift ;;
    --include-stale) INCLUDE_STALE=1; shift ;;
    --cpu)           CPU_THRESHOLD="$2"; shift 2 ;;
    --min-age)       MIN_AGE_MIN="$2"; shift 2 ;;
    --stale-age)     STALE_AGE_MIN="$2"; shift 2 ;;
    --sample)        SAMPLE_SECS="$2"; shift 2 ;;
    -h|--help)       sed -n '2,40p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

# NEVER kill these, even when they match a rule and are orphaned.
#
# `dot-agent-deck daemon serve` is the one that matters most and the one most
# likely to match: it is PPid 1 BY DESIGN (it setsids away from the TUI that
# spawned it), and its in-memory role maps exist nowhere else, so killing it
# abandons every running orchestration. CLAUDE.md rule 15 has the full cost.
NEVER_KILL_RE='^(dot-agent-deck|ssh-agent|gpg-agent|dbus.*|systemd.*|pipewire.*|wireplumber|pulseaudio|tmux.*|screen|Xorg|Xwayland|gnome.*|kde.*|plasma.*|dockerd|containerd.*|tailscaled|cron|sshd)$'

# Matched against the executable NAME (/proc/<pid>/comm and argv[0]'s basename),
# never against the whole command line. Matching the full cmdline exempts any
# process whose PATH merely contains one of these words — a scratch directory
# under a checkout named dot-agent-deck made a live test spinner immune, which
# is how this was found.

# Known stdio MCP servers. Orphaned, these never exit on their own.
#
# Identification is deliberately two-part: the EXECUTABLE must be a node runtime
# AND the command line must name a known package. Matching a bare substring like
# "mcp-server" anywhere in the command line would reap any unrelated script whose
# path or arguments happen to contain it, at 0% CPU — the same defect class as
# the never-kill list above, in the opposite direction.
MCP_EXEC_RE='^(node|npm|npx)$'
MCP_PKG_RE='(telegram-mcp-bot|coderabbitai-mcp|@modelcontextprotocol/server-|mcp-server-|/mcp-server\.js)'

UID_SELF="$(id -u)"
NOW_TICKS="$(awk '{print int($1)}' /proc/uptime)"
HZ="$(getconf CLK_TCK 2>/dev/null || echo 100)"

# /proc/<pid>/stat's SECOND field is the comm wrapped in parentheses, and it can
# contain spaces — "npm exec telegr", "npm exec codera" and "npm exec @model" are
# all real, and are precisely the MCP servers this script targets. Positional awk
# on the raw line therefore returns the wrong field silently: measured, `$4` on a
# process whose comm held a space returned the STATE ('S') instead of the PPID.
# Everything after the LAST ')' splits safely, so read every field from there.
stat_after_comm() {
  local line; line="$(cat "/proc/$1/stat" 2>/dev/null)" || return 1
  [ -z "$line" ] && return 1
  printf '%s' "${line##*) }"
}

# proc(5) numbers fields from 1 including pid and comm, so original field N is
# field N-2 of the remainder: ppid 4 -> 2, utime 14 -> 12, stime 15 -> 13,
# starttime 22 -> 20.
stat_field() {
  local rest; rest="$(stat_after_comm "$1")" || return 1
  printf '%s' "$rest" | awk -v n="$(( $2 - 2 ))" '{print $n}'
}

proc_ppid()  { stat_field "$1" 4; }
cpu_ticks()  { local rest; rest="$(stat_after_comm "$1")" || return 1
               printf '%s' "$rest" | awk '{print $12+$13}'; }

# Age in minutes, from starttime (clock ticks since boot).
proc_age_min() {
  local st; st="$(stat_field "$1" 22)" || return 1
  [ -z "$st" ] && return 1
  echo $(( (NOW_TICKS - st / HZ) / 60 ))
}

candidates=()
for pid in $(ls /proc 2>/dev/null | grep -E '^[0-9]+$'); do
  [ -r "/proc/$pid/stat" ] || continue
  # own processes only
  [ "$(stat -c %u "/proc/$pid" 2>/dev/null)" = "$UID_SELF" ] || continue
  # orphans only
  [ "$(proc_ppid "$pid")" = "1" ] || continue
  cmd="$(tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null)"
  [ -z "$cmd" ] && continue
  comm="$(cat "/proc/$pid/comm" 2>/dev/null)"
  argv0base="$(basename "$(printf '%s' "$cmd" | awk '{print $1}')" 2>/dev/null)"
  echo "$comm" | grep -qE "$NEVER_KILL_RE" && continue
  echo "$argv0base" | grep -qE "$NEVER_KILL_RE" && continue
  candidates+=("$pid")
done

if [ ${#candidates[@]} -eq 0 ]; then
  echo "No orphaned user processes outside the never-kill set. Nothing to do."
  exit 0
fi

# Sample CPU once, wait, sample again — a single reading cannot tell a spin from
# a burst, and the spin rule is the one that reaps a process that looks healthy.
declare -A t0
for pid in "${candidates[@]}"; do t0[$pid]="$(cpu_ticks "$pid")"; done
sleep "$SAMPLE_SECS"

reap=()
declare -A why
declare -A starttime
for pid in "${candidates[@]}"; do
  [ -r "/proc/$pid/stat" ] || continue
  a="${t0[$pid]:-}"; b="$(cpu_ticks "$pid")"
  [ -z "$a" ] || [ -z "$b" ] && continue
  pct=$(( (b - a) * 100 / (SAMPLE_SECS * HZ) ))
  age="$(proc_age_min "$pid")" || continue
  # MATCH against the FULL command line. Truncating here and matching the
  # truncated string silently misses any package name that sits past the cut —
  # and npx cache paths are long, so that is the common case, not the edge one.
  # Truncation is for DISPLAY only, at the point of printing.
  cmd="$(tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null)"

  is_mcp=0
  comm="$(cat "/proc/$pid/comm" 2>/dev/null)"
  argv0base="$(basename "$(printf '%s' "$cmd" | awk '{print $1}')" 2>/dev/null)"
  if { echo "$comm" | grep -qE "$MCP_EXEC_RE" || echo "$argv0base" | grep -qE "$MCP_EXEC_RE"; } \
     && echo "$cmd" | grep -qE "$MCP_PKG_RE"; then is_mcp=1; fi

  # Identity token for the escalation below: starttime is stable for the life of
  # a process and is not reused with the pid.
  starttime[$pid]="$(stat_field "$pid" 22)"

  if [ "$is_mcp" -eq 1 ] && [ "$age" -ge "$MIN_AGE_MIN" ]; then
    reap+=("$pid"); why[$pid]="orphaned MCP server (${pct}% cpu), age ${age}m"
  elif [ "$INCLUDE_SPIN" -eq 1 ] && [ "$pct" -ge "$CPU_THRESHOLD" ] && [ "$age" -ge "$MIN_AGE_MIN" ]; then
    reap+=("$pid"); why[$pid]="spin ${pct}% of a core, age ${age}m"
  elif [ "$INCLUDE_STALE" -eq 1 ] && [ "$age" -ge "$STALE_AGE_MIN" ]; then
    reap+=("$pid"); why[$pid]="stale orphan, age ${age}m"
  fi
done

if [ ${#reap[@]} -eq 0 ]; then
  echo "Checked ${#candidates[@]} orphaned process(es); none matched a rule."
  exit 0
fi

echo "Matched ${#reap[@]} of ${#candidates[@]} orphaned process(es):"
for pid in "${reap[@]}"; do
  printf '  pid %-8s %-34s %s\n' "$pid" "${why[$pid]}" \
    "$(tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null | cut -c1-70)"
done

if [ "$APPLY" -ne 1 ]; then
  echo
  echo "DRY RUN — nothing was signalled. Re-run with --apply to reap these."
  exit 0
fi

for pid in "${reap[@]}"; do
  # SIGTERM first. A wedged event loop cannot run its own handler, which is
  # exactly why the escalation is not optional: the observed spinner ignored
  # SIGTERM and needed SIGKILL.
  kill -TERM "$pid" 2>/dev/null
done
sleep 5
for pid in "${reap[@]}"; do
  if ! kill -0 "$pid" 2>/dev/null; then
    echo "reaped pid $pid (SIGTERM)"
    continue
  fi
  # A pid alive five seconds later is not necessarily the SAME process: the
  # original may have exited and the number been reused. SIGKILL is unblockable,
  # so escalating on the number alone can destroy an innocent bystander. Compare
  # starttime, which a reused pid does not carry over.
  now_start="$(stat_field "$pid" 22)"
  if [ -n "$now_start" ] && [ "$now_start" != "${starttime[$pid]:-}" ]; then
    echo "skipped pid $pid (pid reused since selection — NOT killed)"
    continue
  fi
  kill -KILL "$pid" 2>/dev/null
  echo "reaped pid $pid (SIGKILL — ignored SIGTERM)"
done
