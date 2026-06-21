#!/usr/bin/env bash
#
# reel.sh — demo-reel engine entrypoint (repo-agnostic).
#
# Turns a manifest of {title, description, clip} entries into a single
# narrated MP4 (title/description card, then the clip, repeated in order),
# and optionally uploads it unlisted to YouTube. The engine knows nothing
# about Rust, tests, PRDs, or any specific repo — its only input is a
# manifest.json. See SKILL.md for the full contract.
#
# Usage:
#   reel.sh MANIFEST [--out OUT.mp4] [--publish]
#
set -euo pipefail

SCRIPT_NAME="$(basename "$0")"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# All hosting lives in this one script (see Uploader decision); reel.sh never
# names a host. Only invoked when --publish is requested with credentials set.
UPLOAD_SCRIPT="$SCRIPT_DIR/upload.sh"

# Single source of truth for the dev doc that explains prerequisite and
# credential setup (created by a later docs milestone). Referenced from
# actionable failure messages so the runtime stays short.
DEV_DOC="docs/develop/demo-reel.md"

# CLIs the engine ALWAYS shells out to: agg/ffmpeg render and stitch and
# ffprobe measures the rendered segments (it ships with ffmpeg but is checked
# explicitly). jq is checked separately and earlier because it is needed to read
# the manifest itself. curl is NOT here: only upload.sh uses it, so it is
# required only with --publish (checked in the pre-flight below) — a stitch-only
# run must not demand it.
REQUIRED_CLIS=(agg ffmpeg ffprobe)

# Env vars carrying the YouTube Data API v3 OAuth credentials. Only needed
# with --publish. Sourced from vals/.env.vals.yaml in this repo; never
# hardcoded. Documented in SKILL.md.
CRED_VARS=(YOUTUBE_CLIENT_ID YOUTUBE_CLIENT_SECRET YOUTUBE_REFRESH_TOKEN)

# ---- Render constants ---------------------------------------------------
# The SAME agg font/theme/fps are used for every card and every .cast clip, so
# cards are pixel-identical to clips by construction (ffmpeg concat needs a
# uniform resolution/fps/pixfmt). Changing any of these changes both together.
THEME="asciinema"
FONT_SIZE=16
FPS=30
# Cap idle gaps inside a real clip so e2e waits don't make the reel drag.
CLIP_IDLE=2
# Geometry for a gif/mp4 entry's card: those clips carry no terminal grid, so
# the card is painted at this sensible default and every segment is normalized
# to a common resolution afterwards (the ffmpeg scale+pad safety net).
DEFAULT_COLS=100
DEFAULT_ROWS=30

usage() {
  cat <<EOF
Usage: $SCRIPT_NAME MANIFEST [--out OUT.mp4] [--publish]

Stitch a manifest of {title, description, clip} entries into one narrated MP4
(title/description card, then clip, repeated in order). With --publish, upload
the result unlisted to YouTube and print the URL.

Arguments:
  MANIFEST        Path to a manifest.json: a non-empty JSON array of objects,
                  each { "title": ..., "description": ..., "clip": <path> }
                  where clip is an existing .cast, .gif, or .mp4 file.

Options:
  --out OUT.mp4   Path to write the stitched MP4 (default: reel.mp4).
  --publish       Upload the MP4 unlisted to YouTube and print the URL.
                  Requires ${CRED_VARS[*]} in the environment.
  -h, --help      Show this help and exit.

Examples:
  $SCRIPT_NAME manifest.json --out reel.mp4
  $SCRIPT_NAME manifest.json --out reel.mp4 --publish
EOF
}

# Usage error: bad invocation. Prints the reason and full usage, exits 2.
usage_error() {
  echo "$SCRIPT_NAME: error: $*" >&2
  echo >&2
  usage >&2
  exit 2
}

# Runtime error: valid invocation but something is wrong. Exits 1.
die() {
  echo "$SCRIPT_NAME: error: $*" >&2
  exit 1
}

# Non-fatal note to stderr (stdout stays reserved for machine-readable output
# such as the published URL).
note() {
  echo "$SCRIPT_NAME: $*" >&2
}

# ---- 1. Parse arguments -------------------------------------------------
MANIFEST=""
OUT="reel.mp4"
PUBLISH=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    -h|--help) usage; exit 0 ;;
    --out)
      [[ $# -ge 2 ]] || usage_error "--out requires a path argument"
      OUT="$2"; shift 2 ;;
    --out=*) OUT="${1#*=}"; shift ;;
    --publish) PUBLISH=1; shift ;;
    # --publish is a boolean flag and takes no value; reject the =VALUE form
    # explicitly (rather than letting it fall through to "unknown option") so
    # it is handled as deliberately as --out=.
    --publish=*) usage_error "--publish is a flag and takes no value (got '$1')" ;;
    --) shift; break ;;
    -*) usage_error "unknown option: $1" ;;
    *)
      [[ -z "$MANIFEST" ]] || usage_error "unexpected extra argument: $1"
      MANIFEST="$1"; shift ;;
  esac
done

[[ -n "$MANIFEST" ]] || usage_error "missing required MANIFEST argument"

# ---- 2. Validate the request (manifest) --------------------------------
# jq reads the manifest, so it is required before anything else can run.
command -v jq >/dev/null 2>&1 \
  || die "jq is required to read the manifest but is not on PATH — see $DEV_DOC, or ask the agent to set it up"

[[ -f "$MANIFEST" ]] || die "manifest file not found: $MANIFEST"

jq empty "$MANIFEST" 2>/dev/null || die "manifest is not valid JSON: $MANIFEST"

manifest_type="$(jq -r 'type' "$MANIFEST")"
[[ "$manifest_type" == "array" ]] \
  || die "manifest must be a JSON array of entries, got $manifest_type: $MANIFEST"

[[ "$(jq 'length' "$MANIFEST")" -gt 0 ]] \
  || die "manifest is empty: it must contain at least one {title, description, clip} entry: $MANIFEST"

# Every entry must be an object (checked first so the field checks below can
# safely index into each entry without jq erroring on a non-object).
not_object="$(jq -r '
  to_entries
  | map(select((.value | type) != "object"))
  | if length > 0 then "entry \(.[0].key) is not a JSON object" else empty end
' "$MANIFEST")"
[[ -z "$not_object" ]] || die "$not_object: $MANIFEST"

# Every entry needs non-empty string title, description, and clip.
bad_fields="$(jq -r '
  to_entries
  | map(select(
      ((.value.title       | type) != "string") or ((.value.title       | length) == 0)
      or ((.value.description | type) != "string") or ((.value.description | length) == 0)
      or ((.value.clip        | type) != "string") or ((.value.clip        | length) == 0)
    ))
  | if length > 0 then "entry \(.[0].key) needs non-empty string title, description, and clip" else empty end
' "$MANIFEST")"
[[ -z "$bad_fields" ]] || die "$bad_fields: $MANIFEST"

# clip must point at a .cast / .gif / .mp4 (format-agnostic on purpose).
bad_ext="$(jq -r '
  to_entries
  | map(select((.value.clip | ascii_downcase | test("\\.(cast|gif|mp4)$")) | not))
  | if length > 0 then "entry \(.[0].key) clip \"\(.[0].value.clip)\" must be a .cast, .gif, or .mp4 file" else empty end
' "$MANIFEST")"
[[ -z "$bad_ext" ]] || die "$bad_ext: $MANIFEST"

# Each referenced clip must exist (paths resolved relative to CWD).
while IFS= read -r clip; do
  [[ -f "$clip" ]] || die "clip not found: $clip (referenced in $MANIFEST)"
done < <(jq -r '.[].clip' "$MANIFEST")

# ---- 3. Pre-flight: required CLIs are a HARD failure --------------------
# Missing render/stitch tools mean nothing can run, so fail fast and name them
# all in one pass. With --publish, curl is required too and is an equally HARD
# failure (a missing CLI is never graceful-degraded — only missing CREDENTIALS
# are, handled in step 5: a --publish run with missing creds still produces the
# local mp4 and only skips the upload).
clis=("${REQUIRED_CLIS[@]}")
[[ "$PUBLISH" -eq 1 ]] && clis+=(curl)
missing=()
for cli in "${clis[@]}"; do
  command -v "$cli" >/dev/null 2>&1 || missing+=("CLI '$cli' (not on PATH)")
done
if [[ ${#missing[@]} -gt 0 ]]; then
  {
    echo "$SCRIPT_NAME: missing prerequisite(s):"
    printf '  - %s\n' "${missing[@]}"
    echo "Install the above, then retry. See $DEV_DOC, or ask the agent to set them up."
  } >&2
  exit 1
fi

# Decide whether the upload can happen. Missing creds are recorded, not fatal.
missing_creds=()
if [[ "$PUBLISH" -eq 1 ]]; then
  for var in "${CRED_VARS[@]}"; do
    [[ -n "${!var:-}" ]] || missing_creds+=("$var")
  done
fi

# ---- 4. Build the reel -------------------------------------------------
WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT INT TERM

# Paint a card frame as styled terminal text: bold title, dim word-wrapped,
# block-centered description. Reads geometry/strings from the environment so
# arbitrary punctuation/quotes in the text pass through untouched. Emits the
# raw escape sequence (no trailing newline) — cursor positioning only, so the
# bottom row never scrolls.
CARD_AWK="$WORKDIR/card.awk"
cat > "$CARD_AWK" <<'AWK'
function wrap(s, width, arr,    n, i, words, line, cnt, word) {
  cnt = 0; line = ""
  n = split(s, words, /[ \t]+/)
  for (i = 1; i <= n; i++) {
    word = words[i]
    if (word == "") continue
    while (length(word) > width) {            # hard-split a word longer than the line
      if (line != "") { arr[++cnt] = line; line = "" }
      arr[++cnt] = substr(word, 1, width)
      word = substr(word, width + 1)
    }
    if (line == "") line = word
    else if (length(line) + 1 + length(word) <= width) line = line " " word
    else { arr[++cnt] = line; line = word }
  }
  if (line != "") arr[++cnt] = line
  return cnt
}
function emit(row, text, sgr,    col, len) {
  len = length(text)
  col = int((W - len) / 2) + 1                # center horizontally
  if (col < 1) col = 1
  out = out ESC "[" row ";" col "H" ESC "[" sgr "m" text ESC "[0m"
}
BEGIN {
  W = ENVIRON["CARD_W"] + 0
  H = ENVIRON["CARD_H"] + 0
  ESC = "\033"
  margin = 4
  maxw = W - 2 * margin
  if (maxw < 10) maxw = (W > 10 ? W : 10)
  nt = wrap(ENVIRON["CARD_TITLE"], maxw, tl)
  nd = wrap(ENVIRON["CARD_DESC"], maxw, dl)
  block = nt + 1 + nd                          # title + blank gap + description
  start = int((H - block) / 2) + 1             # center the block vertically
  if (start < 1) start = 1
  out = ESC "[?25l" ESC "[2J"                  # hide cursor, clear screen
  row = start
  for (i = 1; i <= nt; i++) { emit(row, tl[i], "1"); row++ }   # 1 = bold title
  row++                                                         # blank gap
  for (i = 1; i <= nd; i++) { emit(row, dl[i], "2"); row++ }   # 2 = dim body
  printf "%s", out
}
AWK

# Read a .cast header's terminal geometry ("W H"), falling back to the default.
read_cast_geom() {
  local cast="$1" hdr w h
  hdr="$(head -n1 "$cast")"
  w="$(printf '%s' "$hdr" | jq -r '.width // empty' 2>/dev/null || true)"
  h="$(printf '%s' "$hdr" | jq -r '.height // empty' 2>/dev/null || true)"
  [[ "$w" =~ ^[0-9]+$ && "$h" =~ ^[0-9]+$ ]] || { w="$DEFAULT_COLS"; h="$DEFAULT_ROWS"; }
  echo "$w $h"
}

# Synthesize a card .cast at WxH for the given title/description: paint at t=0,
# then a no-op at t=HOLD so the static frame is held that long. agg's
# --idle-time-limit is set above HOLD so the hold is not truncated.
make_card_cast() {
  local w="$1" h="$2" title="$3" desc="$4" hold="$5" out="$6" payload
  payload="$(CARD_W="$w" CARD_H="$h" CARD_TITLE="$title" CARD_DESC="$desc" awk -f "$CARD_AWK")"
  {
    printf '{"version": 2, "width": %d, "height": %d, "env": {"TERM": "xterm-256color"}}\n' "$w" "$h"
    printf '[0.0, "o", %s]\n' "$(printf '%s' "$payload" | jq -Rs .)"
    printf '[%d.0, "o", %s]\n' "$hold" "$(printf '\033[H' | jq -Rs .)"
  } > "$out"
}

# The SAME agg invocation for cards and clips — only timing knobs vary, never
# font/theme/size/fps, so the rendered pixel grid stays identical. The `--`
# terminates option parsing so an untrusted clip path like "-foo.cast" is taken
# as the positional input, never mistaken for an agg option.
render_cast() {
  local cast="$1" gif="$2" idle="$3"
  # Capture agg's stderr to a temp file (under WORKDIR, so the EXIT trap cleans
  # it up) instead of discarding it. stdout stays /dev/null and the success path
  # stays quiet, but on a NON-ZERO agg exit we surface agg's real error here —
  # otherwise an agg failure only manifests later as an opaque ffmpeg error on
  # the missing/garbled gif. This covers both the card and the .cast clip render
  # (the clip path calls this same function).
  local err="$WORKDIR/agg.err"
  if ! agg --theme "$THEME" --font-size "$FONT_SIZE" --fps-cap "$FPS" \
    --idle-time-limit "$idle" -- "$cast" "$gif" >/dev/null 2>"$err"; then
    cat "$err" >&2
    die "agg failed to render $cast (exit non-zero); see agg error above"
  fi
}

# Probe a media file's "WIDTHxHEIGHT".
probe_wh() {
  ffprobe -v error -select_streams v:0 -show_entries stream=width,height \
    -of csv=s=x:p=0 "$1"
}

n="$(jq 'length' "$MANIFEST")"
note "building reel from $n manifest entr$([[ "$n" -eq 1 ]] && echo y || echo ies)…"

# Render every segment to its NATIVE resolution first (card via agg, .cast clip
# via agg, gif/mp4 used as-is), preserving manifest order. The common target
# resolution is then the max across all of them, so nothing is upscaled.
natives=()
hold_total=0
for ((i = 0; i < n; i++)); do
  title="$(jq -r ".[$i].title"       "$MANIFEST")"
  desc="$(jq -r  ".[$i].description" "$MANIFEST")"
  clip="$(jq -r  ".[$i].clip"        "$MANIFEST")"
  ext="$(printf '%s' "${clip##*.}" | tr '[:upper:]' '[:lower:]')"

  # Card geometry matches the clip's terminal grid for a .cast; gif/mp4 clips
  # have none, so the card uses the default and the normalize pass aligns them.
  if [[ "$ext" == "cast" ]]; then
    read -r cols rows < <(read_cast_geom "$clip")
  else
    cols="$DEFAULT_COLS"; rows="$DEFAULT_ROWS"
  fi

  # Hold scales to text length: max(3s, ceil(words/3)) so longer cards stay
  # readable. (words+2)/3 is integer-ceil of words/3.
  words="$(printf '%s %s' "$title" "$desc" | wc -w | tr -d '[:space:]')"
  hold=$(( (words + 2) / 3 ))
  (( hold < 3 )) && hold=3
  hold_total=$(( hold_total + hold ))

  make_card_cast "$cols" "$rows" "$title" "$desc" "$hold" "$WORKDIR/card_$i.cast"
  render_cast "$WORKDIR/card_$i.cast" "$WORKDIR/card_$i.gif" "$((hold + 1))"
  natives+=("$WORKDIR/card_$i.gif")

  case "$ext" in
    cast)
      render_cast "$clip" "$WORKDIR/clip_$i.gif" "$CLIP_IDLE"
      natives+=("$WORKDIR/clip_$i.gif") ;;
    gif|mp4)
      natives+=("$clip") ;;   # pre-rendered: fed straight to ffmpeg
  esac
done

# Target resolution = max width/height across all native segments, rounded up
# to even values (yuv420p / libx264 require even dimensions).
TW=0; TH=0
for f in "${natives[@]}"; do
  wh="$(probe_wh "$f")"
  w="${wh%x*}"; h="${wh#*x}"
  (( w > TW )) && TW="$w"
  (( h > TH )) && TH="$h"
done
(( TW % 2 )) && TW=$(( TW + 1 ))
(( TH % 2 )) && TH=$(( TH + 1 ))

# Normalize each native to the common target (scale preserving aspect ratio,
# then pad/letterbox to TWxTH), at a constant fps and yuv420p — the safety net
# that guarantees every segment shares resolution/fps/pixfmt for a seamless
# concat, even if a clip was recorded at a different terminal size.
#
# The untrusted clip path reaches ffmpeg only as the argument to `-i`, which
# binds the very next token as its filename — so a leading-dash path is taken
# as the input, not an option (verified). ffmpeg has no `--` option terminator,
# so the agg-style `--` hardening doesn't apply here.
list="$WORKDIR/concat.txt"
: > "$list"
for idx in "${!natives[@]}"; do
  seg="$WORKDIR/seg_$idx.mp4"
  ffmpeg -y -hide_banner -loglevel error -i "${natives[$idx]}" \
    -vf "fps=$FPS,scale=$TW:$TH:force_original_aspect_ratio=decrease,pad=$TW:$TH:(ow-iw)/2:(oh-ih)/2:color=black,setsar=1,format=yuv420p" \
    -an -c:v libx264 -pix_fmt yuv420p -r "$FPS" "$seg"
  printf "file '%s'\n" "$seg" >> "$list"
done

# Concat the uniform segments into one stream. Inputs already share codec,
# resolution, fps and pixfmt, so a stream copy yields a single seamless video
# track (no re-encode, no resolution/format seam between segments).
mkdir -p "$(dirname "$OUT")"
ffmpeg -y -hide_banner -loglevel error -f concat -safe 0 -i "$list" -c copy "$OUT"
note "reel written to $OUT (${TW}x${TH}, ${FPS}fps)"

# ---- 5. Publish (graceful degrade) -------------------------------------
# Stitch-only runs stop here. With --publish: upload if creds are present;
# otherwise keep the local mp4 and report the skip rather than failing — the
# artifact is the valuable part, the upload is best-effort.
if [[ "$PUBLISH" -eq 1 ]]; then
  if [[ ${#missing_creds[@]} -gt 0 ]]; then
    note "reel is at $OUT; could not publish (missing ${missing_creds[*]} — see $DEV_DOC, or ask the agent to set them up)"
  else
    # Title/description for the upload are derived from the reel itself; the
    # engine is repo-agnostic and has no notion of a PRD. Runtime upload errors
    # pass through from upload.sh unswallowed.
    reel_title="$(basename "$OUT" .mp4)"
    reel_desc="$(jq -r 'map("• " + .title) | join("\n")' "$MANIFEST")"
    url="$("$UPLOAD_SCRIPT" "$OUT" "$reel_title" "$reel_desc")"
    echo "$url"
    note "published unlisted: $url"
  fi
fi
