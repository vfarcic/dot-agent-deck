#!/usr/bin/env bash
# Issue #959 stress: run live_014 in a fresh process many times, in parallel,
# and capture stacks for any run that stalls. One execution per CI job is the
# real-world rate (~1 stall in ~450 executions), so a few thousand here should
# reproduce it if the stall is a property of the test rather than of that one
# runner.
set -uo pipefail
BIN="$1"; PER_WORKER="$2"; WORKERS="$3"; STALL_S="${4:-25}"
TEST=daemon_protocol::tests::live_014_spawn_time_is_observed_and_additive
OUT="${TMPDIR:-/tmp}/959-stress"; mkdir -p "$OUT"

snapshot() { # $1 = pid of the stalled test process
  local pid=$1
  echo "--- ps (the process and its children) ---"
  ps -A -o pid=,ppid=,stat=,command= 2>/dev/null | awk -v p="$pid" '$1==p || $2==p'
  if [ "$(uname)" = "Darwin" ]; then
    echo "--- sample $pid ---"
    /usr/bin/sample "$pid" 3 -mayDie 2>&1 | sed -n '1,90p'
    for c in $(ps -A -o pid=,ppid= 2>/dev/null | awk -v p="$pid" '$2==p {print $1}'); do
      echo "--- sample child $c ---"
      /usr/bin/sample "$c" 2 -mayDie 2>&1 | sed -n '1,60p'
    done
  else
    for t in /proc/"$pid"/task/*; do
      echo "  thread $(basename "$t") state=$(awk '{print $3}' "$t/stat" 2>/dev/null) wchan=$(cat "$t/wchan" 2>/dev/null)"
      echo "    stack-top: $(head -3 "$t/stack" 2>/dev/null | tr '\n' ' ')"
    done
    for c in $(ps -A -o pid=,ppid= 2>/dev/null | awk -v p="$pid" '$2==p {print $1}'); do
      echo "  child $c state=$(awk '{print $3}' /proc/"$c"/stat 2>/dev/null) wchan=$(cat /proc/"$c"/wchan 2>/dev/null) cmd=$(tr '\0' ' ' </proc/"$c"/cmdline 2>/dev/null)"
    done
  fi
}

worker() {
  local id=$1 stalls=0 slowest=0
  for ((i = 0; i < PER_WORKER; i++)); do
    local log="$OUT/w$id.log"
    "$BIN" --exact "$TEST" --nocapture >"$log" 2>&1 &
    local pid=$! waited=0
    while kill -0 "$pid" 2>/dev/null; do
      sleep 0.2
      waited=$((waited + 1))
      if [ "$waited" -gt $((STALL_S * 5)) ]; then
        stalls=$((stalls + 1))
        {
          echo "=== STALL #$stalls worker=$id iteration=$i pid=$pid after ${STALL_S}s ==="
          echo "--- the test's own phase markers ---"
          cat "$log"
          snapshot "$pid"
          echo "=== end stall #$stalls ==="
        } >>"$OUT/stalls.txt" 2>&1
        kill -9 "$pid" 2>/dev/null
        break
      fi
    done
    [ "$waited" -gt "$slowest" ] && slowest=$waited
    wait "$pid" 2>/dev/null
  done
  echo "worker=$id iterations=$PER_WORKER stalls=$stalls slowest=$((slowest * 200))ms" >>"$OUT/summary.txt"
}

: >"$OUT/summary.txt"; : >"$OUT/stalls.txt"
for ((w = 0; w < WORKERS; w++)); do worker "$w" & done
wait
echo "================ SUMMARY ================"
cat "$OUT/summary.txt"
total=$(awk -F'stalls=' '{split($2,a," "); s+=a[1]} END {print s+0}' "$OUT/summary.txt")
echo "TOTAL STALLS: $total out of $((PER_WORKER * WORKERS)) executions"
echo "================ STALLS ================="
cat "$OUT/stalls.txt"
