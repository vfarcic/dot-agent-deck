#!/usr/bin/env bash
#
# retime.sh — rewrite an asciinema v2 cast's event timestamps so it plays back
# at a watchable cadence. Repo-agnostic: it operates on ANY .cast and knows
# nothing about Rust, ratatui, or this repo (the only repo-specific knowledge is
# the SIZE_THRESHOLD default, calibrated below, which is overridable).
#
# WHY this exists (see PRD #180): e2e casts are recorded at machine speed, so
# their event stream has a pathological cadence — instantaneous bursts (a keypress
# plus the full repaint it triggers land within a millisecond) separated by short
# real waits (daemon startup, polling, debounce). A single global `agg --speed`
# can't fix that: slowing everything stretches the waits into dead air and still
# can't SPREAD coincident events apart. This re-timer rebuilds the timeline from
# the event payloads instead, so the engine can render at speed 1.0:
#
#   * IDLE   — an original gap above IDLE_THRESHOLD is a real wait; clamp the
#              output gap to IDLE_CAP so dead air is killed but a pause still reads
#              as a pause.
#   * TYPING — a SMALL-payload event is a single typed character (ratatui emits a
#              minimal diff per keypress). Give each its own step spaced TYPE_GAP
#              apart, so typing replays at a natural, readable speed.
#   * OPERATION — a LARGE-payload event is a full-region repaint (opening a deck,
#              a form, switching panes). Consecutive large chunks within
#              COALESCE_GAP are one logical repaint, so coalesce them into a single
#              step, then HOLD the resulting state OP_HOLD before the next event so
#              the operation is actually visible.
#
# CLASSIFICATION is by output-payload SIZE. In this repo's real casts char diffs
# top out at ~48 bytes and the smallest operation repaint is ~106 bytes — a clean,
# wide gap — so the default SIZE_THRESHOLD (80) separates them cleanly. agg's own
# static last-frame hold is left intact, so the final state still lingers.
#
# Usage:
#   retime.sh [INPUT.cast] [--out OUT.cast]
#     INPUT.cast   path to read (default: stdin)
#     --out PATH   path to write the retimed cast (default: stdout)
#
# Tunables (env-overridable, like the engine's CLIP_SPEED — all in SECONDS except
# SIZE_THRESHOLD, which is in BYTES):
#   SIZE_THRESHOLD  payload byte size at/below which an event is a typed char
#                   rather than an operation repaint                  (default 80)
#   TYPE_GAP        gap between successive typed chars (typing cadence) (default 0.1)
#   OP_HOLD         hold AFTER an operation repaint before the next step (default 1.4)
#   IDLE_CAP        max output gap kept for a real idle wait           (default 0.4)
#   IDLE_THRESHOLD  original gap at/above which a wait is "real" idle   (default 0.3)
#   COALESCE_GAP    max gap between large chunks that are one repaint   (default 0.05)
#
set -euo pipefail

SCRIPT_NAME="$(basename "$0")"

SIZE_THRESHOLD="${SIZE_THRESHOLD:-80}"
TYPE_GAP="${TYPE_GAP:-0.1}"
OP_HOLD="${OP_HOLD:-1.4}"
IDLE_CAP="${IDLE_CAP:-0.4}"
IDLE_THRESHOLD="${IDLE_THRESHOLD:-0.3}"
COALESCE_GAP="${COALESCE_GAP:-0.05}"

usage_error() { echo "$SCRIPT_NAME: error: $*" >&2; exit 2; }

IN=""
OUT=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --out)   [[ $# -ge 2 ]] || usage_error "--out requires a path argument"; OUT="$2"; shift 2 ;;
    --out=*) OUT="${1#*=}"; shift ;;
    -h|--help) echo "Usage: $SCRIPT_NAME [INPUT.cast] [--out OUT.cast]"; exit 0 ;;
    --) shift; break ;;
    -*) usage_error "unknown option: $1" ;;
    *)  [[ -z "$IN" ]] || usage_error "unexpected extra argument: $1"; IN="$1"; shift ;;
  esac
done

command -v jq >/dev/null 2>&1 || { echo "$SCRIPT_NAME: error: jq is required but not on PATH" >&2; exit 1; }

# Read the whole cast (small — KB-scale e2e recordings). The cast is a SEQUENCE of
# JSON values: a header object on line 1, then one "[t, code, data]" array per
# line. `jq -s` slurps that sequence into a single array so the program can see
# the header (.[0]) and the events (.[1:]) together; `-c` then prints each output
# value back out on its own line, reproducing the one-value-per-line cast format.
#
# The re-timing runs entirely inside jq so payload byte sizing (utf8bytelength)
# and re-encoding (correct JSON string escaping) are exact. Two passes over the
# events, both as `reduce`:
#   1. STEPS  — fold consecutive events into steps: a small event is its own
#               "type" step; consecutive large events within COALESCE_GAP merge
#               into one "op" step (their payloads concatenate, reproducing the
#               original byte stream as a single logical repaint).
#   2. CLOCK  — walk the steps assigning a fresh output timestamp to each: the gap
#               before a step is the idle clamp for a real wait, else the typing
#               cadence, and is widened to OP_HOLD whenever the PREVIOUS step was
#               an operation (so each repaint is held before whatever follows).
jq -sc \
  --argjson st "$SIZE_THRESHOLD" \
  --argjson tg "$TYPE_GAP" \
  --argjson oh "$OP_HOLD" \
  --argjson ic "$IDLE_CAP" \
  --argjson it "$IDLE_THRESHOLD" \
  --argjson cg "$COALESCE_GAP" '
  .[0] as $header
  | (.[1:] | map({t: .[0], code: .[1], data: .[2], size: (.[2] | utf8bytelength)})) as $evs

  # Pass 1: fold events into steps (coalescing chunked operation repaints).
  | (reduce $evs[] as $e ([];
      (.[-1]) as $last
      | (if $e.size > $st then "op" else "type" end) as $kind
      | if ($last != null) and ($kind == "op") and ($last.kind == "op")
           and (($e.t - $last.last_t) <= $cg) then
          # continuation chunk of the same repaint: merge into the last op step
          .[0:-1] + [ $last + {
            last_t: $e.t,
            data:  ($last.data + $e.data),
            size:  ($last.size + $e.size)
          } ]
        else
          . + [ {kind: $kind, code: $e.code, first_t: $e.t, last_t: $e.t, data: $e.data, size: $e.size} ]
        end
    )) as $steps

  # Pass 2: assign fresh output timestamps to each step.
  | (reduce range(0; ($steps | length)) as $i ({out: [], t: 0, prev: null};
      ($steps[$i]) as $s
      | (if .prev == null then
            # Lead-in: keep the cast initial delay, capped to IDLE_CAP.
            ([$s.first_t, $ic] | min)
         else
            ($s.first_t - .prev.last_t) as $gap0
            | (if $gap0 >= $it then ([$gap0, $ic] | min)      # real wait -> clamp
               else $tg end) as $base                         # bunched -> typing cadence
            | (if .prev.kind == "op" then ([$base, $oh] | max) # hold after an operation
               else $base end)
         end) as $delta
      | (((.t + $delta) * 1000 | round) / 1000) as $nt        # round to ms, tidy output
      | {out: (.out + [[$nt, $s.code, $s.data]]), t: $nt, prev: $s}
    ) | .out) as $retimed

  | $header, ($retimed[])
' "${IN:-/dev/stdin}" > "${OUT:-/dev/stdout}"
