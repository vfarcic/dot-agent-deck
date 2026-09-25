#!/usr/bin/env bash
#
# smoke.sh — re-runnable acceptance smoke for the demo-reel ENGINE.
#
# Builds a reel from a tiny self-contained fixture (2 hand-written .cast clips
# + a manifest) in STITCH-ONLY mode (no --publish, no network, no credentials)
# and asserts the stitched MP4 with ffprobe:
#
#   * the output file is non-empty;
#   * it carries exactly ONE video stream at the expected resolution — a single
#     uniform stream is the proof there is no resolution/fps/pixfmt seam between
#     the card and clip segments;
#   * the pixel format is yuv420p and the frame rate is a constant 30/1;
#   * the duration is at least the sum of the per-card hold durations, AND no
#     longer than the engine's own bound on it (card holds + each clip's re-timing
#     budget + a small per-clip allowance for its final-frame hold). That upper
#     bound is the regression guard for PRD #339, where a 15.5s cast rendered as a
#     161s video: the re-timer mistook the render loop's per-frame tails (which
#     print nothing and outnumber real keystrokes ~35:1) for typed characters and
#     gave each its own 100ms step;
#   * the duration matches card holds + each retimed clip + the final-frame hold
#     the engine sizes for it, to within half a second. That is the guard for
#     issue #365, where agg's default 3s last-frame hold stacked on each clip's
#     own tail (see CLIP_LAST_FRAME / CLIP_FINAL_DWELL in reel.sh).
#
# It needs only agg + ffmpeg/ffprobe (already in devbox.json). It is LOCAL-ONLY
# and never runs in CI. The real YouTube upload is NOT exercised here — that
# path is verified by code review and a documented one-line manual step
# (see SKILL.md).
#
# Run via: task reel-smoke   (or directly: .claude/skills/demo-reel/tests/smoke.sh)
#
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REEL="$HERE/../reel.sh"
FIXTURES="$HERE/fixtures"
MANIFEST="$FIXTURES/manifest.json"

# Expected stitched-canvas resolution = the engine's FIXED 16:9 output canvas
# (REEL_W x REEL_H in reel.sh), independent of the fixtures. It used to be the
# per-axis MAX native across all segments, which is what produced PRD #339's
# 1140x1142 portrait reel — width from the card, height from a portrait clip, an
# aspect belonging to neither. Mirror the engine's env overrides so the two stay in
# lock-step.
EXPECTED_W="${REEL_W:-1920}"
EXPECTED_H="${REEL_H:-1080}"

fail() { echo "SMOKE FAIL: $*" >&2; exit 1; }

# Output to a throwaway dir so the smoke is freely re-runnable and leaves no
# artifact behind.
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
OUT="$TMP/reel.mp4"

# Stitch only — must succeed with no credentials in the environment. Run from
# the fixtures dir because clip paths in the manifest are relative to CWD.
( cd "$FIXTURES" && "$REEL" "manifest.json" --out "$OUT" )

# --- assertions --------------------------------------------------------
[[ -s "$OUT" ]] || fail "output file missing or empty: $OUT"

# Exactly one video stream (no seam): more than one would mean the segments
# did not concat into a single uniform track.
nstreams="$(ffprobe -v error -select_streams v -show_entries stream=index -of csv=p=0 "$OUT" | wc -l | tr -d '[:space:]')"
[[ "$nstreams" -eq 1 ]] || fail "expected exactly 1 video stream, found $nstreams"

IFS=',' read -r W H PIXFMT FR < <(
  ffprobe -v error -select_streams v:0 \
    -show_entries stream=width,height,pix_fmt,avg_frame_rate \
    -of "csv=p=0" "$OUT"
)
[[ "$W" == "$EXPECTED_W" && "$H" == "$EXPECTED_H" ]] \
  || fail "resolution ${W}x${H} != expected ${EXPECTED_W}x${EXPECTED_H}"
[[ "$PIXFMT" == "yuv420p" ]] || fail "pix_fmt '$PIXFMT' != yuv420p"
[[ "$FR" == "30/1" ]]        || fail "avg_frame_rate '$FR' != 30/1"

# Duration must be at least the sum of the per-card holds. The engine holds every
# card a FLAT CARD_HOLD seconds (default 4), independent of text length, so the
# lower bound is simply the entry count times that hold. Mirror the engine's
# CARD_HOLD env override so the two stay in lock-step.
CARD_HOLD="${CARD_HOLD:-4}"
sum_holds="$(jq --argjson h "$CARD_HOLD" 'length * $h' "$MANIFEST")"
DUR="$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$OUT")"
awk -v d="$DUR" -v m="$sum_holds" 'BEGIN { exit !(d + 0 >= m + 0) }' \
  || fail "duration ${DUR}s < sum of card holds ${sum_holds}s"

# ...and at MOST the bound the engine promises. retime.sh caps each clip at
# max(MIN_BUDGET, MAX_STRETCH x the clip's own duration), so the whole reel cannot
# exceed the card holds plus those per-clip caps plus the final-frame hold (at
# most the larger of CLIP_LAST_FRAME and CLIP_FINAL_DWELL) and one frame-rounding
# second per clip. Mirror the engine's defaults so the two stay in lock-step. This
# is the assertion that fails loudly if the re-timer ever again turns a short cast
# into a slideshow.
MAX_STRETCH="${MAX_STRETCH:-1.6}"
MIN_BUDGET="${MIN_BUDGET:-8}"
CLIP_LAST_FRAME="${CLIP_LAST_FRAME:-1}"
CLIP_FINAL_DWELL="${CLIP_FINAL_DWELL:-2}"
CLIP_SPEED="${CLIP_SPEED:-1.0}"
max_hold="$(awk -v lf="$CLIP_LAST_FRAME" -v fd="$CLIP_FINAL_DWELL" 'BEGIN { print (lf > fd ? lf : fd) }')"
# Per clip: max(MIN_BUDGET, MAX_STRETCH x its own duration), + max_hold for the
# final frame + 1s of frame/encoder rounding. Clip paths are relative to the
# fixtures dir (that is where the engine ran), so resolve them from there. The
# fixture casts are sub-second, so MIN_BUDGET is their binding cap; for a real
# multi-second cast the MAX_STRETCH term dominates.
max_dur="$(cd "$FIXTURES" && {
  total="$(awk -v c="$(jq 'length' "$MANIFEST")" -v h="$CARD_HOLD" 'BEGIN { print c * h }')"
  while IFS= read -r clip; do
    if [[ "$clip" == *.cast ]]; then
      cdur="$(jq -sr '.[1:] | if length == 0 then 0 else .[-1][0] end' "$clip")"
    else
      cdur="$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$clip")"
    fi
    total="$(awk -v t="$total" -v d="${cdur:-0}" -v ms="$MAX_STRETCH" -v mb="$MIN_BUDGET" -v mh="$max_hold" \
      'BEGIN { b = d * ms; if (b < mb) b = mb; print t + b + mh + 1 }')"
  done < <(jq -r '.[].clip' "$MANIFEST")
  printf '%s' "$total"
})"
awk -v d="$DUR" -v m="$max_dur" 'BEGIN { exit !(d + 0 <= m + 0) }' \
  || fail "duration ${DUR}s > engine's own bound ${max_dur}s — a segment is being stretched (see retime.sh MAX_STRETCH/MIN_BUDGET)"

# ...and, tightly, what the engine should have produced: card holds + each clip's
# RETIMED duration (at CLIP_SPEED) + the final-frame hold it sizes from the
# retimed cast's tail — max(CLIP_LAST_FRAME, CLIP_FINAL_DWELL - tail). Only .cast
# clips are accounted for; the fixture has no gif/mp4. Rendering and encoding
# round each segment to whole frames, so half a second of slack is generous,
# while agg's default 3s hold creeping back would overshoot it on the first clip.
RETIME="$HERE/../retime.sh"
expected="$(cd "$FIXTURES" && {
  total="$sum_holds"
  while IFS= read -r clip; do
    retimed="$TMP/expected.retimed.cast"
    "$RETIME" "$clip" --out "$retimed"
    rdur="$(jq -sr '.[1:] | if length == 0 then 0 else .[-1][0] end' "$retimed")"
    tail_s="$("$RETIME" --trailing "$retimed")"
    total="$(awk -v t="$total" -v d="$rdur" -v tl="$tail_s" -v sp="$CLIP_SPEED" -v lf="$CLIP_LAST_FRAME" -v fd="$CLIP_FINAL_DWELL" \
      'BEGIN { need = fd - tl / sp; h = (need > lf ? need : lf); print t + d / sp + h }')"
  done < <(jq -r '.[].clip' "$MANIFEST")
  printf '%s' "$total"
})"
awk -v d="$DUR" -v e="$expected" 'BEGIN { x = d - e; if (x < 0) x = -x; exit !(x <= 0.5) }' \
  || fail "duration ${DUR}s is not within 0.5s of the expected ${expected}s — a clip's final-frame hold is not what CLIP_LAST_FRAME/CLIP_FINAL_DWELL size it to (issue #365)"

echo "SMOKE PASS: ${W}x${H} ${PIXFMT} ${FR}, 1 uniform video stream, duration=${DUR}s (card holds ${sum_holds}s <= dur <= bound ${max_dur}s; expected ${expected}s)"
echo "--- ffprobe ($OUT) ---"
ffprobe -hide_banner "$OUT" 2>&1 | sed -n '/Input #0/,$p'
