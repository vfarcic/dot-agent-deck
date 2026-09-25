#!/usr/bin/env bash
#
# retime.sh — rewrite an asciinema v2 cast's event timestamps so it plays back
# at a watchable cadence. Repo-agnostic: it operates on ANY .cast and knows
# nothing about Rust, ratatui, or this repo (the only repo-specific knowledge is
# the SIZE_THRESHOLD default, calibrated below, which is overridable).
#
# WHY this exists (see PRD #180): e2e casts are recorded at machine speed, so
# their event stream can have a pathological cadence — instantaneous bursts (a
# keypress plus the full repaint it triggers land within a millisecond) separated
# by short real waits (daemon startup, polling, debounce). A single global
# `agg --speed` can't fix that: slowing everything stretches the waits into dead
# air and still can't SPREAD coincident events apart. This re-timer rebuilds the
# timeline from the event payloads instead, so the engine can render at 1.0.
#
# CONTRACT (the invariant that matters — see the 10.4x regression below):
#   The re-timer RE-DISTRIBUTES time; it does not manufacture it. Time reclaimed
#   from dead air is re-spent on holding operations, so the output totals about
#   max(MIN_BUDGET, the INPUT duration) — and never more than
#   max(MIN_BUDGET, MAX_STRETCH x input), which is a hard ceiling. When there is no
#   dead air to reclaim, nothing is held and the cast plays at roughly real time.
#   No cast can ever come out as a slideshow.
#
# HOW an event is classified (three kinds, by payload SIZE *and* CONTENT):
#   * op   — a LARGE payload (> SIZE_THRESHOLD bytes) is a full-region repaint
#            (opening a deck, a form, switching panes). Consecutive large chunks
#            within COALESCE_GAP are one logical repaint and coalesce into a
#            single step. An op is HELD (up to OP_HOLD) before the next step, so
#            the new state is actually visible — budget permitting.
#   * type — a small payload that actually PRINTS something is a typed character
#            (ratatui emits a minimal diff per keypress). Each gets its own step,
#            at least TYPE_GAP apart, so typing replays at a readable speed.
#   * tick — a small payload that prints NOTHING: pure control sequences (SGR
#            reset, show-cursor, cursor-position). This is the render loop's
#            per-frame tail, not a keystroke. A tick keeps its ORIGINAL gap
#            (clamped to IDLE_CAP) — it is never spread.
#
# The tick/type split is why content matters and size alone does not. The
# published PRD #339 reel turned a 15.5s cast into a 161s video (10.4x) because
# the old classifier called EVERY small payload a keystroke — and a ratatui render
# loop emits a per-frame tail (SGR reset + show-cursor + cursor-position) that
# prints nothing and vastly outnumbers real keystrokes, so nearly every event in
# the cast was given its own fabricated 100ms typing step, with each coalesced
# repaint then held 1.4s unconditionally on top. Measured on that test's
# replacement recording (28.7s; 1621 events = 1565 ticks + 44 typed chars + 12
# repaints) the old code yields 172.4s, of which 1609 x 0.1s = 160.9s is pure
# fabricated typing cadence; this version yields 32.3s. Both failure modes are now
# closed — content-aware classification, and OP_HOLD granted only out of reclaimed
# slack under the duration budget above.
#
# Usage:
#   retime.sh [INPUT.cast] [--out OUT.cast] [--trailing]
#     INPUT.cast   path to read (default: stdin)
#     --out PATH   path to write the retimed cast (default: stdout)
#     --trailing   instead of re-timing, print the seconds between the cast's
#                  LAST VISIBLE CHANGE (its last op or type event) and its end —
#                  how long the final state is already on screen before the
#                  renderer's last-frame hold begins. The engine runs this on the
#                  RETIMED cast to size that hold (reel.sh CLIP_FINAL_DWELL). It
#                  uses the same classifier as the re-timer, so the two agree on
#                  what a tick is.
#
# Tunables (env-overridable, like the engine's CLIP_SPEED — all in SECONDS except
# SIZE_THRESHOLD, which is in BYTES, and MAX_STRETCH, which is a ratio):
#   SIZE_THRESHOLD  payload byte size above which an event is an operation
#                   repaint rather than a small per-frame diff      (default 80)
#   TYPE_GAP        minimum gap between successive typed chars        (default 0.1)
#   OP_HOLD         MAXIMUM hold after an operation repaint, granted
#                   only out of reclaimed slack                       (default 1.4)
#   IDLE_CAP        max output gap kept for any single gap            (default 0.4)
#   MAX_STRETCH     HARD ceiling: output duration <= this x the input  (default 1.6)
#   MIN_BUDGET      ...but always allow at least this many seconds,
#                   so a sub-second synthetic cast can still expand   (default 8)
#   COALESCE_GAP    max gap between large chunks that are one repaint (default 0.05)
#
# (IDLE_THRESHOLD is gone: every gap is now simply clamped to IDLE_CAP, so there
# is no separate "is this a real wait" threshold to tune.)
#
set -euo pipefail

SCRIPT_NAME="$(basename "$0")"

SIZE_THRESHOLD="${SIZE_THRESHOLD:-80}"
TYPE_GAP="${TYPE_GAP:-0.1}"
OP_HOLD="${OP_HOLD:-1.4}"
IDLE_CAP="${IDLE_CAP:-0.4}"
MAX_STRETCH="${MAX_STRETCH:-1.6}"
MIN_BUDGET="${MIN_BUDGET:-8}"
COALESCE_GAP="${COALESCE_GAP:-0.05}"

usage_error() { echo "$SCRIPT_NAME: error: $*" >&2; exit 2; }

IN=""
OUT=""
TRAILING=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --out)   [[ $# -ge 2 ]] || usage_error "--out requires a path argument"; OUT="$2"; shift 2 ;;
    --out=*) OUT="${1#*=}"; shift ;;
    --trailing) TRAILING=1; shift ;;
    -h|--help) echo "Usage: $SCRIPT_NAME [INPUT.cast] [--out OUT.cast] [--trailing]"; exit 0 ;;
    --) shift; break ;;
    -*) usage_error "unknown option: $1" ;;
    *)  [[ -z "$IN" ]] || usage_error "unexpected extra argument: $1"; IN="$1"; shift ;;
  esac
done

command -v jq >/dev/null 2>&1 || { echo "$SCRIPT_NAME: error: jq is required but not on PATH" >&2; exit 1; }

# The event classifier, shared by the re-timing program and --trailing so they
# cannot disagree about what a tick is. Needs --arg e/b (ESC/BEL, below).
JQ_DEFS='
  # Everything the payload actually PRINTS, with terminal control sequences
  # removed: OSC first (it swallows a text argument of its own), then CSI, then
  # any other two-character ESC sequence. What survives is real screen content.
  def printed:
      gsub("\($e)\\][^\($b)\($e)]*(\($b)|\($e)\\\\)?"; "")
    | gsub("\($e)\\[[0-9;:?<>=!]*[ -/]*[@-~]"; "")
    | gsub("\($e)."; "");
  # A payload "prints" iff something visible (not whitespace, not a control
  # character) survives that stripping. A keystroke does; a render-loop tail of
  # SGR-reset + show-cursor + cursor-position does not.
  def prints: printed | test("[^[:space:][:cntrl:]]");
  # op / type / tick, as described in the header.
  def kind($st): if (.data | utf8bytelength) > $st then "op" elif (.data | prints) then "type" else "tick" end;
'

if [[ "$TRAILING" -eq 1 ]]; then
  # Seconds from the last non-tick event to the last event. A cast with no
  # visible change at all reports the timestamp of its last event; an empty cast
  # reports 0.
  jq -sr \
    --arg e "$(printf '\033')" \
    --arg b "$(printf '\007')" \
    --argjson st "$SIZE_THRESHOLD" "$JQ_DEFS"'
    (.[1:] | map({t: .[0], data: .[2]})) as $evs
    | (if ($evs | length) == 0 then 0 else $evs[-1].t end) as $end
    | ([ $evs[] | select(kind($st) != "tick") | .t ] | last // 0) as $visible
    | $end - $visible
  ' "${IN:-/dev/stdin}" > "${OUT:-/dev/stdout}"
  exit 0
fi

# Read the whole cast (small — KB-scale e2e recordings). The cast is a SEQUENCE of
# JSON values: a header object on line 1, then one "[t, code, data]" array per
# line. `jq -s` slurps that sequence into a single array so the program can see
# the header (.[0]) and the events (.[1:]) together; `-c` then prints each output
# value back out on its own line, reproducing the one-value-per-line cast format.
#
# The re-timing runs entirely inside jq so payload byte sizing (utf8bytelength)
# and re-encoding (correct JSON string escaping) are exact. Four passes:
#   1. STEPS   — fold consecutive events into steps: each small event is its own
#                type/tick step; consecutive large events within COALESCE_GAP
#                merge into one "op" step (their payloads concatenate, reproducing
#                the original byte stream as a single logical repaint).
#   2. BASE    — give every step its base gap: the original gap clamped to
#                IDLE_CAP, floored at TYPE_GAP for a typed char only. This pass
#                never inflates a tick or an op, so a cast that is already
#                watchable keeps its own cadence.
#   3. BUDGET  — reconcile with the two limits. Over the hard ceiling (typing
#                floors on a burst-typed cast) -> shrink every gap proportionally.
#                Otherwise -> share whatever the hold budget leaves over equally
#                across the post-operation hold slots, up to OP_HOLD each, so
#                reclaimed dead air (and only that) becomes visible holds.
#   4. CLOCK   — accumulate the final gaps into absolute timestamps (ms-rounded).
# ESC / BEL as jq --arg values rather than literal control bytes in this file, so
# the script stays plain ASCII and reviewable (same reason reel.sh's awk spells
# ESC as "\033"). They are interpolated into the control-sequence regexes below.
jq -sc \
  --arg e "$(printf '\033')" \
  --arg b "$(printf '\007')" \
  --argjson st "$SIZE_THRESHOLD" \
  --argjson tg "$TYPE_GAP" \
  --argjson oh "$OP_HOLD" \
  --argjson ic "$IDLE_CAP" \
  --argjson ms "$MAX_STRETCH" \
  --argjson mb "$MIN_BUDGET" \
  --argjson cg "$COALESCE_GAP" "$JQ_DEFS"'
  .[0] as $header
  | (.[1:] | map({t: .[0], code: .[1], data: .[2],
                  size: (.[2] | utf8bytelength)} | . + {kind: kind($st)})) as $evs
  | (if ($evs | length) == 0 then 0 else $evs[-1].t end) as $orig
  # Two distinct limits, and keeping them separate is the whole point:
  #   $hold_budget — what the output may total once operation holds are added. It
  #     is the ORIGINAL duration (or MIN_BUDGET, whichever is larger), so holds are
  #     paid for out of time RECLAIMED from dead air rather than added on top. A
  #     cast with no dead air gets no holds and plays at roughly real time; a cast
  #     that is half idle waits spends that half on making its operations visible.
  #   $ceiling — the hard upper bound, MAX_STRETCH x the input. Nothing may exceed
  #     it, including the base gaps themselves (a burst-typed cast whose TYPE_GAP
  #     floors add up past it gets compressed back down).
  # MIN_BUDGET floors both so a sub-second synthetic cast can still be expanded
  # into something watchable.
  | ([$mb, $orig] | max) as $hold_budget
  | ([$mb, ($orig * $ms)] | max) as $ceiling

  # Pass 1: fold events into steps (coalescing chunked operation repaints).
  | (reduce $evs[] as $e ([];
      (.[-1]) as $last
      | $e.kind as $kind
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

  # Pass 2: base gap per step — the original gap, clamped, and floored only for a
  # genuine typed character. Nothing else is ever stretched here.
  | ([ range(0; ($steps | length)) as $i
       | ($steps[$i]) as $s
       | (if $i == 0 then $s.first_t else ($s.first_t - $steps[$i - 1].last_t) end) as $gap0
       | ([([$gap0, $ic] | min), 0] | max) as $clamped
       | if $s.kind == "type" then ([$clamped, $tg] | max) else $clamped end
     ]) as $base
  | ($base | add // 0) as $d0

  # Pass 3: reconcile with the budget. The hold slots are the steps that FOLLOW an
  # operation — holding there is what makes the repaint linger on screen.
  | ([ range(1; ($steps | length)) | select($steps[. - 1].kind == "op") ]) as $slots
  | (if $d0 > $ceiling and $d0 > 0 then
       # Over the hard ceiling: shrink everything proportionally. This is the
       # structural guarantee that no cast can render as a slideshow.
       $base | map(. * ($ceiling / $d0))
     else
       ($hold_budget - $d0) as $slack
       | (if ($slots | length) > 0 and $slack > 0
          then ([$oh, ($slack / ($slots | length))] | min) else 0 end) as $extra
       | if $extra <= 0 then $base
         else
           ($slots | map({(tostring): true}) | add // {}) as $slotset
           | [ range(0; ($base | length)) as $i
               | if $slotset[$i | tostring] then ([$base[$i], $extra] | max) else $base[$i] end ]
         end
     end) as $gaps

  # Pass 4: accumulate the gaps into absolute timestamps, rounded to ms.
  | (reduce range(0; ($gaps | length)) as $i ({out: [], t: 0};
      ((((.t + $gaps[$i]) * 1000) | round) / 1000) as $nt
      | {out: (.out + [[$nt, $steps[$i].code, $steps[$i].data]]), t: $nt}
    ) | .out) as $retimed

  | $header, ($retimed[])
' "${IN:-/dev/stdin}" > "${OUT:-/dev/stdout}"
