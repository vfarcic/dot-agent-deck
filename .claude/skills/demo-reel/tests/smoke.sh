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
#   * the duration is at least the sum of the per-card hold durations.
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

# Expected agg-rendered canvas for the fixtures' 80x24 cast geometry with the
# engine's pinned render constants (theme/font-size/fps in reel.sh) and the
# devbox-pinned agg. Override via env if the toolchain legitimately changes.
EXPECTED_W="${EXPECTED_W:-790}"
EXPECTED_H="${EXPECTED_H:-560}"

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

# Duration must be at least the sum of the per-card holds. Recompute the holds
# from the manifest with the SAME rule the engine uses: max(3, ceil(words/3))
# over each entry's title + description words.
sum_holds="$(
  jq -r '.[] | .title + " " + .description' "$MANIFEST" \
    | awk '{ h = int((NF + 2) / 3); if (h < 3) h = 3; s += h } END { print s }'
)"
DUR="$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$OUT")"
awk -v d="$DUR" -v m="$sum_holds" 'BEGIN { exit !(d + 0 >= m + 0) }' \
  || fail "duration ${DUR}s < sum of card holds ${sum_holds}s"

echo "SMOKE PASS: ${W}x${H} ${PIXFMT} ${FR}, 1 uniform video stream, duration=${DUR}s (>= card holds ${sum_holds}s)"
echo "--- ffprobe ($OUT) ---"
ffprobe -hide_banner "$OUT" 2>&1 | sed -n '/Input #0/,$p'
