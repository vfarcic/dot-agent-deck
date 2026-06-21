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
#   reel.sh MANIFEST [--out OUT.mp4] [--title TITLE] [--publish]
#
set -euo pipefail

SCRIPT_NAME="$(basename "$0")"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# All hosting lives in this one script (see Uploader decision); reel.sh never
# names a host. Only invoked when --publish is requested with credentials set.
UPLOAD_SCRIPT="$SCRIPT_DIR/upload.sh"

# Re-times each .cast clip BEFORE agg renders it: rewrites event timestamps so
# typing replays at a readable cadence, operation repaints are held, and dead idle
# waits are clamped. This is what makes a machine-speed e2e cast watchable; it
# replaces the old blunt global CLIP_SPEED slowdown (see retime.sh and CLIP_SPEED
# below). Repo-agnostic — operates on any .cast.
RETIME_SCRIPT="$SCRIPT_DIR/retime.sh"

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
# Clips render through agg at their RECORDED terminal grid and FONT_SIZE, so a
# clip looks exactly as captured. Cards are deliberately different: each is
# painted on a SMALLER fixed grid (CARD_COLS x CARD_ROWS) at a larger
# CARD_FONT_SIZE, so the title/description fill more of the frame and read BIGGER
# than they would on the clip's wide grid. Cards are therefore NOT pixel-identical
# to clips — instead the ffmpeg scale+pad NORMALIZE pass (step 4 below) fits every
# segment to one common resolution/fps/pixfmt, which is what keeps the concat
# seamless even though cards and clips render at different sizes.
THEME="asciinema"
FONT_SIZE=16
FPS=30
# Cap idle gaps inside a real clip so e2e waits don't make the reel drag.
CLIP_IDLE=2
# Watchable cadence now comes from the cast RE-TIMER (retime.sh), which rewrites
# each .cast clip's event timestamps BEFORE agg renders it — spreading coincident
# bursts (typing, repaints) and clamping dead idle waits, which a single global
# agg --speed could never do. So clips render at CLIP_SPEED 1.0 (real time of the
# RETIMED cast). CLIP_SPEED is kept as a tunable escape hatch: a global multiplier
# layered ON TOP of the re-timer (< 1 = slower still, > 1 = faster) for the rare
# clip that wants a uniform nudge. Cards are static stills and are NEVER slowed
# (render_cast defaults speed to 1). Pre-rendered gif/mp4 clips bypass agg (and
# the re-timer) entirely, so neither affects them.
CLIP_SPEED="${CLIP_SPEED:-1.0}"
# A card is one static frame. agg only needs a brief span to paint it, so the
# synthetic card cast holds for CARD_RENDER_SPAN seconds and is rendered with a
# small idle limit (CARD_IDLE); agg collapses that static span to a single
# painted frame, which we then freeze. The card's ON-SCREEN hold is applied
# AFTERWARD at the ffmpeg level (loop that still to an exact duration) — NOT via
# agg's timeline — so a long card actually holds its full time instead of agg
# collapsing the static tail.
CARD_IDLE=2
CARD_RENDER_SPAN="0.6"
# Each card's ON-SCREEN hold is a FLAT CARD_HOLD seconds (env-overridable),
# regardless of how much text it carries. A fixed, deliberately short hold keeps
# the reel moving; a viewer who wants to read a long description pauses the video
# rather than the reel parking on every long card. (This replaces the old hold
# that scaled with the rendered content-line count and was capped at 16s — the
# content-line count still drives the card GRID HEIGHT so long text doesn't clip,
# only the hold stopped depending on it.)
CARD_HOLD="${CARD_HOLD:-4}"
# Card geometry: a SMALL fixed grid painted at a larger font so the text renders
# big relative to the clip. CARD_WRAP (< CARD_COLS) word-wraps text to a
# comfortable measure, leaving generous side margins. The TITLE is centered; the
# DESCRIPTION is rendered as a LEFT-ALIGNED, vertically-centered multi-line block
# (one line per sentence / source line / list item, with hanging indents on
# bullets — see CARD_AWK) so it reads as prose instead of one wrapped wall of
# text. CARD_ROWS is a MINIMUM: a description with more lines than fit grows the
# grid taller (CARD_AWK reports the effective height) rather than clipping, and
# the normalize pass letterboxes/scales the result. The grid/font are tuned so a
# typical card's native canvas stays just UNDER a clip's native size — the card
# is scaled UP to the clip's resolution by the normalize pass instead of driving
# that resolution up (which would upscale the clip). The same fixed grid serves
# cast AND gif/mp4 entries (a gif/mp4 carries no terminal grid of its own).
CARD_COLS=84
CARD_ROWS=28
CARD_WRAP=58
CARD_FONT_SIZE=22

usage() {
  cat <<EOF
Usage: $SCRIPT_NAME MANIFEST [--out OUT.mp4] [--title TITLE] [--publish]

Stitch a manifest of {title, description, clip} entries into one narrated MP4
(title/description card, then clip, repeated in order). With --publish, upload
the result unlisted to YouTube and print the URL.

Arguments:
  MANIFEST        Path to a manifest.json: a non-empty JSON array of objects,
                  each { "title": ..., "description": ..., "clip": <path> }
                  where clip is an existing .cast, .gif, or .mp4 file.

Options:
  --out OUT.mp4   Path to write the stitched MP4 (default: reel.mp4).
  --title TITLE   Title for the uploaded video (only used with --publish).
                  Default: the basename of --out without its extension
                  (e.g. "reel" for reel.mp4).
  --publish       Upload the MP4 unlisted to YouTube and print the URL.
                  Requires ${CRED_VARS[*]} in the environment.
  -h, --help      Show this help and exit.

Examples:
  $SCRIPT_NAME manifest.json --out reel.mp4
  $SCRIPT_NAME manifest.json --out reel.mp4 --publish
  $SCRIPT_NAME manifest.json --out reel.mp4 --title "My demo reel" --publish
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
# Empty means "derive from --out at publish time" (basename without extension);
# a caller (e.g. the adapter) may set an explicit, descriptive title via --title.
TITLE=""
PUBLISH=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    -h|--help) usage; exit 0 ;;
    --out)
      [[ $# -ge 2 ]] || usage_error "--out requires a path argument"
      OUT="$2"; shift 2 ;;
    --out=*) OUT="${1#*=}"; shift ;;
    --title)
      [[ $# -ge 2 ]] || usage_error "--title requires a value argument"
      TITLE="$2"; shift 2 ;;
    --title=*) TITLE="${1#*=}"; shift ;;
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

# Paint a card frame as styled terminal text: a BOLD bright-cyan CENTERED title
# above a BRIGHT-WHITE, LEFT-ALIGNED, vertically-centered multi-line description
# (high contrast — never dim). The description is broken into readable lines
# instead of one wrapped paragraph: split on sentence boundaries (.?! + space),
# on any source line breaks, and on leading list markers (-, *, "N."), with a
# hanging indent for bullet continuations; each resulting line is then wrapped to
# CARD_WRAP. The block is vertically centered, and if it has more lines than fit
# the grid grows taller (the effective height is reported back via CARD_META) so
# long text scales/letterboxes rather than clipping. Reads geometry/strings from
# the environment so arbitrary punctuation/quotes in the text pass through
# untouched. Emits the raw escape sequence (no trailing newline) — cursor
# positioning only, so the bottom row never scrolls.
CARD_AWK="$WORKDIR/card.awk"
cat > "$CARD_AWK" <<'AWK'
function sp(n,   s) { s = ""; while (n-- > 0) s = s " "; return s }

# Word-wrap s into pieces of at most `width` chars, appended to arr starting at
# index cnt+1 (hard-splitting any word longer than width). Returns the new count.
function wrap_line(s, width, arr, cnt,    n, i, words, line, word) {
  if (width < 1) width = 1
  n = split(s, words, /[ \t]+/)
  line = ""
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

# Append one logical description line to dl[], wrapped to the description column
# (maxw). `prefix` precedes the first physical line (a list marker, or empty) and
# `indent` spaces hang under it on every continuation line.
function add_desc(text, prefix, indent,    pieces, m, j) {
  m = wrap_line(text, maxw - indent, pieces, 0)
  if (m == 0) { dl[++nd] = prefix; return }   # keep an intentionally blank line
  for (j = 1; j <= m; j++) {
    if (j == 1) dl[++nd] = prefix pieces[j]
    else        dl[++nd] = sp(indent) pieces[j]
  }
}

# Turn one source segment (already split on newlines) into one-or-more logical
# description lines: a leading list marker (-, *, or "N.") becomes a hanging-
# indented bullet kept on its own line; otherwise the segment is split on
# sentence boundaries (.?! followed by whitespace) into one line per sentence.
function add_segment(seg,    marker, rest, parts, k, np) {
  if (match(seg, /^[ \t]*([-*]|[0-9]+\.)[ \t]+/)) {
    marker = substr(seg, RSTART, RLENGTH)
    rest   = substr(seg, RSTART + RLENGTH)
    sub(/^[ \t]+/, "", marker); sub(/[ \t]+$/, "", marker)
    marker = marker " "                       # normalize to "<sym> "
    add_desc(rest, marker, length(marker))
    return
  }
  # Mark every sentence end (.?! + whitespace) with a sentinel, then split on it.
  gsub(/[.?!]+[ \t]+/, "&\001", seg)
  np = split(seg, parts, /\001/)
  for (k = 1; k <= np; k++) {
    if (parts[k] ~ /[^ \t]/) add_desc(parts[k], "", 0)
  }
}

function emit(row, col, text, sgr) {
  if (col < 1) col = 1
  out = out ESC "[" row ";" col "H" ESC "[" sgr "m" text ESC "[0m"
}

BEGIN {
  W = ENVIRON["CARD_W"] + 0
  MINH = ENVIRON["CARD_H"] + 0                  # minimum rows; grown if text overflows
  ESC = "\033"
  # Wrap width is an explicit measure (CARD_WRAP), decoupled from the grid width
  # so text sits in a comfortable column with wide side margins. Clamp to [10, W].
  maxw = ENVIRON["CARD_WRAP"] + 0
  if (maxw < 10) maxw = 10
  if (maxw > W) maxw = W

  nt = wrap_line(ENVIRON["CARD_TITLE"], maxw, tl, 0)

  nd = 0
  nseg = split(ENVIRON["CARD_DESC"], segs, /\n/)   # preserve source line breaks
  for (s = 1; s <= nseg; s++) add_segment(segs[s])

  block = nt + 1 + nd                          # title + blank gap + description
  H = MINH
  if (block + 2 > H) H = block + 2             # grow rows (1 row top+bottom margin) so nothing clips
  start = int((H - block) / 2) + 1             # vertically center the block
  if (start < 1) start = 1
  dcol = int((W - maxw) / 2) + 1               # left edge of the centered description column
  if (dcol < 1) dcol = 1

  out = ESC "[?25l" ESC "[2J"                  # hide cursor, clear screen
  row = start
  for (i = 1; i <= nt; i++) {                  # bold bright-cyan title, each line centered
    emit(row, int((W - length(tl[i])) / 2) + 1, tl[i], "1;96"); row++
  }
  row++                                                          # blank gap
  for (i = 1; i <= nd; i++) { emit(row, dcol, dl[i], "97"); row++ }  # bright-white left-aligned body
  printf "%s", out

  # Report the effective height and total content lines so the caller can size
  # the cast header and recompute the on-screen hold from the rendered content.
  if (ENVIRON["CARD_META"] != "")
    printf "%d %d\n", H, nt + nd > ENVIRON["CARD_META"]
}
AWK

# Synthesize a one-frame card .cast for the given title/description: paint at
# t=0, then a no-op a beat later (CARD_RENDER_SPAN) so agg has a span to render
# the painted frame. The passed `h` is a MINIMUM number of rows — CARD_AWK may
# grow the grid taller to fit a long description and reports the EFFECTIVE height
# (plus the content-line count) via CARD_META; the .cast header is written with
# that effective height, and the function echoes "<eff_rows> <content_lines>" so
# the caller can recompute the on-screen hold. The card's on-screen HOLD is
# deliberately NOT encoded here — it is enforced later at the ffmpeg level (a
# single still looped to an exact duration), immune to agg collapsing a static
# tail.
make_card_cast() {
  local w="$1" h="$2" title="$3" desc="$4" out="$5" payload eff_h content_lines
  local meta="$WORKDIR/card_meta"
  payload="$(CARD_W="$w" CARD_H="$h" CARD_WRAP="$CARD_WRAP" CARD_META="$meta" CARD_TITLE="$title" CARD_DESC="$desc" awk -f "$CARD_AWK")"
  read -r eff_h content_lines < "$meta"
  rm -f "$meta"
  {
    printf '{"version": 2, "width": %d, "height": %d, "env": {"TERM": "xterm-256color"}}\n' "$w" "$eff_h"
    printf '[0.0, "o", %s]\n' "$(printf '%s' "$payload" | jq -Rs .)"
    printf '[%s, "o", %s]\n' "$CARD_RENDER_SPAN" "$(printf '\033[H' | jq -Rs .)"
  } > "$out"
  printf '%s %s\n' "$eff_h" "$content_lines"
}

# One agg invocation for both cards and clips; the caller varies the idle limit,
# the font size (cards render larger via CARD_FONT_SIZE — see below) AND the
# playback speed, so the card and clip canvases differ on purpose and the
# normalize pass reconciles them into one uniform stream. `font` defaults to
# FONT_SIZE so the clip path keeps rendering at its recorded size; `speed`
# defaults to 1 so cards (static stills) are never slowed — only the clip path
# passes CLIP_SPEED to slow the action. The `--` terminates option parsing so an
# untrusted clip path like "-foo.cast" is taken as the positional input, never
# mistaken for an agg option.
render_cast() {
  local cast="$1" gif="$2" idle="$3" font="${4:-$FONT_SIZE}" speed="${5:-1}"
  # Capture agg's stderr to a temp file (under WORKDIR, so the EXIT trap cleans
  # it up) instead of discarding it. stdout stays /dev/null and the success path
  # stays quiet, but on a NON-ZERO agg exit we surface agg's real error here —
  # otherwise an agg failure only manifests later as an opaque ffmpeg error on
  # the missing/garbled gif. This covers both the card and the .cast clip render
  # (the clip path calls this same function).
  local err="$WORKDIR/agg.err"
  if ! agg --theme "$THEME" --font-size "$font" --fps-cap "$FPS" \
    --idle-time-limit "$idle" --speed "$speed" -- "$cast" "$gif" >/dev/null 2>"$err"; then
    cat "$err" >&2
    die "agg failed to render $cast (exit non-zero); see agg error above"
  fi
}

# Probe a media file's "WIDTHxHEIGHT".
probe_wh() {
  ffprobe -v error -select_streams v:0 -show_entries stream=width,height \
    -of csv=s=x:p=0 "$1"
}

# Freeze the single painted still (PNG) from a rendered card gif. agg collapses
# the card's static span to one frame, so grab exactly that frame. This still is
# what the card segment is built from, so the card never flickers and its
# duration is decoupled from agg's timeline.
freeze_still() {
  local gif="$1" png="$2"
  ffmpeg -y -hide_banner -loglevel error -i "$gif" -frames:v 1 -update 1 "$png"
}

n="$(jq 'length' "$MANIFEST")"
note "building reel from $n manifest entr$([[ "$n" -eq 1 ]] && echo y || echo ies)…"

# Render every segment to its NATIVE resolution first (card via agg on its small
# fixed grid, .cast clip via agg at its recorded grid, gif/mp4 used as-is),
# preserving manifest order. The common target resolution is then the max across
# all of them; clips render at full size and the smaller cards are scaled up to
# match in the normalize pass.
natives=()
holds=()          # parallel to natives: hold seconds for a card, empty for a clip
for ((i = 0; i < n; i++)); do
  title="$(jq -r ".[$i].title"       "$MANIFEST")"
  desc="$(jq -r  ".[$i].description" "$MANIFEST")"
  clip="$(jq -r  ".[$i].clip"        "$MANIFEST")"
  ext="$(printf '%s' "${clip##*.}" | tr '[:upper:]' '[:lower:]')"

  # Paint the card on its SMALL fixed grid (CARD_COLS x CARD_ROWS minimum) at the
  # larger CARD_FONT_SIZE — independent of the clip's grid — so the text reads
  # big; the normalize pass below scales it up to the clip's resolution. The grid
  # may grow taller for a long description (make_card_cast echoes the effective
  # row count and the rendered content-line count). Then freeze a single painted
  # still; the on-screen hold is applied later (looping this still to exactly
  # $hold seconds), decoupled from agg's idle handling.
  read -r eff_rows content_lines < <(make_card_cast "$CARD_COLS" "$CARD_ROWS" "$title" "$desc" "$WORKDIR/card_$i.cast")

  # On-screen hold is a FLAT CARD_HOLD seconds — independent of how much text the
  # card carries. A fixed short hold keeps the reel moving; a viewer who wants to
  # read a long description pauses the video rather than the reel parking on every
  # long card. (content_lines above still drives the card GRID HEIGHT so long text
  # doesn't clip — only the hold stopped depending on it.)
  hold="$CARD_HOLD"

  render_cast "$WORKDIR/card_$i.cast" "$WORKDIR/card_$i.gif" "$CARD_IDLE" "$CARD_FONT_SIZE"
  freeze_still "$WORKDIR/card_$i.gif" "$WORKDIR/card_$i.png"
  natives+=("$WORKDIR/card_$i.png")
  holds+=("$hold")

  case "$ext" in
    cast)
      # Re-time the cast (rewrite event timestamps for a watchable cadence — see
      # retime.sh) BEFORE rendering, then render the RETIMED cast through agg at
      # CLIP_SPEED (default 1.0; the re-timer now controls cadence).
      "$RETIME_SCRIPT" "$clip" --out "$WORKDIR/clip_$i.retimed.cast"
      render_cast "$WORKDIR/clip_$i.retimed.cast" "$WORKDIR/clip_$i.gif" "$CLIP_IDLE" "$FONT_SIZE" "$CLIP_SPEED"
      natives+=("$WORKDIR/clip_$i.gif")
      holds+=("") ;;
    gif|mp4)
      natives+=("$clip")       # pre-rendered: fed straight to ffmpeg
      holds+=("") ;;
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
norm_vf="fps=$FPS,scale=$TW:$TH:force_original_aspect_ratio=decrease,pad=$TW:$TH:(ow-iw)/2:(oh-ih)/2:color=black,setsar=1,format=yuv420p"
for idx in "${!natives[@]}"; do
  seg="$WORKDIR/seg_$idx.mp4"
  hold="${holds[$idx]}"
  if [[ -n "$hold" ]]; then
    # Card segment: loop the single painted still to EXACTLY $hold seconds. The
    # duration is enforced HERE (-loop 1 -t), decoupled from agg — so a long
    # card holds its full (capped) time instead of agg collapsing the tail.
    ffmpeg -y -hide_banner -loglevel error -loop 1 -t "$hold" -i "${natives[$idx]}" \
      -vf "$norm_vf" -an -c:v libx264 -pix_fmt yuv420p -r "$FPS" "$seg"
  else
    # Clip segment: normalize the rendered/pre-rendered clip as-is.
    ffmpeg -y -hide_banner -loglevel error -i "${natives[$idx]}" \
      -vf "$norm_vf" -an -c:v libx264 -pix_fmt yuv420p -r "$FPS" "$seg"
  fi
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
    # Title for the upload: the caller-supplied --title verbatim if given,
    # otherwise derived from the reel filename. The engine is repo-agnostic and
    # has no notion of a PRD, so a descriptive title is the caller's job (the
    # adapter composes one); --title is how it reaches the upload. Runtime upload
    # errors pass through from upload.sh unswallowed.
    reel_title="${TITLE:-$(basename "$OUT" .mp4)}"
    reel_desc="$(jq -r 'map("• " + .title) | join("\n")' "$MANIFEST")"
    url="$("$UPLOAD_SCRIPT" "$OUT" "$reel_title" "$reel_desc")"
    echo "$url"
    note "published unlisted: $url"
  fi
fi
