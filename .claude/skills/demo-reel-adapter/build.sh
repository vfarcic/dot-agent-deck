#!/usr/bin/env bash
#
# build.sh — dot-agent-deck demo-reel ADAPTER (PRD #180, M2.1).
#
# Repo-specific glue that turns this repo's per-test recording artifacts into a
# manifest.json and hands it to the repo-agnostic ENGINE (../demo-reel/reel.sh).
# The ONLY contract with the engine is manifest.json — a JSON array of
# {title, description, clip}. The engine knows nothing about Rust, #[spec],
# CATALOG.md, or recordings; the adapter knows nothing about agg/ffmpeg/YouTube.
#
# Two concerns are deliberately separated so the deterministic half is
# fixture-testable without git or the network:
#
#   (a) SELECTION — "which recording dirs are in scope" (needs git):
#       `build.sh select`  -> prints the in-scope recording-dir IDs, one per line.
#
#   (b) ASSEMBLY — "build manifest.json from a given list of IDs" (pure: reads
#       each test.md + CATALOG.md, emits JSON; no git, no network):
#       `build.sh assemble [ID...] [--manifest PATH]`
#
#   reel (default) — run (a), then (b), then invoke the engine forwarding
#       --out/--publish. Clean-skips when nothing is in scope.
#
# Selection rule (file-level granularity, robustness over cleverness):
#   A recording dir `<RECORDINGS_DIR>/<id>/` is IN SCOPE iff
#     1. it contains a `full-stream.cast` — the e2e proxy. L1 render tests emit a
#        `test.md` but NO cast, so they are excluded by construction; and
#     2. the source file named in its `test.md` `**Source:**` line was changed on
#        this branch vs `<MAIN_REF>`. Matching is by FILE BASENAME against
#        `git diff --name-only <MAIN_REF>` restricted to `*.rs` — basename match
#        sidesteps the test.md "<immediate-parent>/<file>" path quirk and is
#        robust for the flat `tests/*.rs` (and `src/*.rs`) layout this repo uses.
#
# Card text is lifted from test.md only (no test-body parsing):
#   title       <- the H1 line, minus the leading "# " (e.g. "mouse/button/001 — …")
#   description <- the "## Scenario" paragraph(s), collapsed to one line
#   catalog id  <- the part of the H1 before the first " — " (used for ordering)
#
# Ordering: entries are sorted by their catalog id's line position in CATALOG.md
# (the authoritative order); ids absent from the catalog sort last.
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# --- configuration (env-overridable so the pure path is fixture-testable) ---
RECORDINGS_DIR="${REEL_ADAPTER_RECORDINGS_DIR:-.dot-agent-deck/recordings}"
CATALOG_FILE="${REEL_ADAPTER_CATALOG:-tests/CATALOG.md}"
MAIN_REF="${REEL_ADAPTER_MAIN_REF:-main}"
ENGINE="${REEL_ADAPTER_ENGINE:-$SCRIPT_DIR/../demo-reel/reel.sh}"

SKIP_MSG="skipped: no e2e tests changed on this branch"

die() { echo "demo-reel-adapter: $*" >&2; exit 1; }

usage() {
  cat <<EOF
Usage:
  build.sh [reel] [--out OUT.mp4] [--publish] [--manifest PATH] [--title TITLE]
      Select in-scope e2e tests, build a manifest, and invoke the engine.
      Clean-skips (no manifest, no engine, exit 0) when no e2e tests changed.
      Composes a descriptive video title ('<repo> · PRD #<prd> · PR #<pr> —
      <desc>') and forwards it to the engine; --title TITLE overrides that
      composition verbatim (for manual/dogfood runs).
  build.sh title [--title TITLE]
      Print the title the reel pipeline would pass to the engine on the current
      branch (the composed title, or --title verbatim). Dry-run: no manifest, no
      engine, no upload.
  build.sh select
      Print the in-scope recording-dir IDs (one per line). Uses git.
  build.sh assemble [ID...] [--manifest PATH]
      Build manifest.json from the given recording-dir IDs (pure: no git, no
      network). Excludes any ID without a full-stream.cast; orders by catalog id.
      Clean-skips when no ID resolves to an e2e clip.

Environment overrides:
  REEL_ADAPTER_RECORDINGS_DIR  (default: .dot-agent-deck/recordings)
  REEL_ADAPTER_CATALOG         (default: tests/CATALOG.md)
  REEL_ADAPTER_MAIN_REF        (default: main)
  REEL_ADAPTER_ENGINE          (default: <skill>/../demo-reel/reel.sh)
EOF
}

# --------------------------------------------------------------------------
# Pure helpers (no git, no network) — drive concern (b).
# --------------------------------------------------------------------------

# Title = the test.md H1, minus the leading "# ".
extract_title() {
  local md="$1" line
  line="$(grep -m1 '^# ' "$md" 2>/dev/null || true)"
  printf '%s' "${line#"# "}"
}

# Catalog id = the part of the H1 before the first " — " (em dash separator).
extract_catalog_id() {
  local title="$1"
  printf '%s' "${title%% — *}"
}

# Description = the text under "## Scenario" up to the next "## " heading, with
# blank lines dropped and collapsed to a single line.
extract_description() {
  local md="$1"
  awk '
    /^## Scenario[[:space:]]*$/ { inblk=1; next }
    inblk && /^## / { inblk=0 }
    inblk { print }
  ' "$md" | awk 'NF' | tr '\n' ' ' | sed -E 's/  +/ /g; s/^ //; s/ $//'
}

# Decode the small set of HTML entities this repo's test.md generator emits, so
# card text shows literal characters ([ ] & < > ' "). This is repo-specific (the
# generator HTML-escapes), so decoding lives in the ADAPTER — the engine keeps
# painting its manifest text verbatim. Both named and numeric (decimal, with
# optional leading zeros) forms are handled. `&amp;` / `&#38;` are decoded LAST
# so an escaped entity is not re-decoded into something else.
html_decode() {
  sed -E '
    s/&#0*91;/[/g
    s/&#0*93;/]/g
    s/&lt;/</g;       s/&#0*60;/</g
    s/&gt;/>/g;       s/&#0*62;/>/g
    s/&#0*39;/'\''/g; s/&apos;/'\''/g
    s/&quot;/"/g;     s/&#0*34;/"/g
    s/&amp;/\&/g;     s/&#0*38;/\&/g
  '
}

# Source-file basename from the test.md "**Source:**" line
# (`<dir>/<file>::<fn>` inside backticks) — used by selection only.
extract_source_basename() {
  local md="$1" src
  src="$(grep -m1 '^\*\*Source:\*\*' "$md" 2>/dev/null | sed -E 's/.*`([^`]*)`.*/\1/' || true)"
  src="${src%%::*}"        # drop ::fn_name
  printf '%s' "${src##*/}"  # basename
}

# Catalog ordinal for an id: 1-based line order of its `##### <id> — …` entry in
# CATALOG.md; 999999 when the id is not catalogued (sorts last).
catalog_ord() {
  local want="$1" n=0 id
  while IFS= read -r id; do
    n=$((n + 1))
    [[ "$id" == "$want" ]] && { printf '%s' "$n"; return; }
  done < <(awk '/^##### / { l=$0; sub(/^##### /,"",l); sub(/ —.*/,"",l); print l }' "$CATALOG_FILE")
  printf '999999'
}

# --------------------------------------------------------------------------
# Concern (b): assemble manifest.json from an explicit list of recording IDs.
# Pure — reads test.md + CATALOG.md only. Excludes cast-less (L1) IDs. Orders by
# catalog id. Clean-skips (prints SKIP_MSG, writes NO manifest, exit 0) when
# nothing is in scope.
# --------------------------------------------------------------------------
assemble() {
  local manifest="$1"; shift
  local rows id md cast title catid desc ord obj title_dec desc_dec
  rows="$(mktemp)"
  # The rows scratch file is removed on the normal exit paths below, but a
  # validation `die` can abort mid-loop — so also clean it up on any exit
  # (including Ctrl-C / TERM) to avoid leaking it. `${rows:-}` keeps the trap
  # safe under `set -u` once the function has returned and the local is gone
  # (the normal paths have already removed the file by then).
  trap 'rm -f "${rows:-}"' EXIT INT TERM

  for id in "$@"; do
    # Reject path-traversal in an id before it becomes a filesystem path: an id
    # is a single recording-dir name, never a path, so '/' or '..' is invalid.
    if [[ "$id" == */* || "$id" == *..* ]]; then
      die "invalid recording id '$id': must not contain '/' or '..'"
    fi
    cast="$RECORDINGS_DIR/$id/full-stream.cast"
    md="$RECORDINGS_DIR/$id/test.md"
    if [[ ! -f "$cast" ]]; then
      echo "demo-reel-adapter: excluding '$id' (no full-stream.cast — not an e2e clip)" >&2
      continue
    fi
    [[ -f "$md" ]] || die "missing test.md for '$id': $md"
    title="$(extract_title "$md")"
    [[ -n "$title" ]] || die "no H1 title in $md"
    # catid is matched against CATALOG.md ids, so derive it from the RAW title
    # (ids are plain ASCII — no entities); only the card-bound text is decoded.
    catid="$(extract_catalog_id "$title")"
    desc="$(extract_description "$md")"
    ord="$(catalog_ord "$catid")"
    title_dec="$(printf '%s' "$title" | html_decode)"
    desc_dec="$(printf '%s' "$desc" | html_decode)"
    obj="$(jq -nc --arg t "$title_dec" --arg d "$desc_dec" --arg c "$cast" \
      '{title:$t, description:$d, clip:$c}')"
    printf '%010d\t%s\t%s\n' "$ord" "$id" "$obj" >> "$rows"
  done

  if [[ ! -s "$rows" ]]; then
    rm -f "$rows"
    echo "$SKIP_MSG"
    return 0
  fi

  # Sort by catalog ordinal (zero-padded) then by id for determinism; strip the
  # sort keys and fold the per-entry objects into a JSON array.
  sort -k1,1 -k2,2 "$rows" | cut -f3- | jq -s '.' > "$manifest"
  rm -f "$rows"
  echo "demo-reel-adapter: wrote $(jq 'length' "$manifest") entries to $manifest" >&2
  # The manifest path is an informational note, so it goes to STDERR like the
  # line above. STDOUT stays clean so the engine's URL-on-stdout contract holds
  # in the combined --publish flow (where reel.sh prints the YouTube URL there).
  printf '%s\n' "$manifest" >&2
}

# --------------------------------------------------------------------------
# Concern (a): print the in-scope recording-dir IDs (one per line).
# --------------------------------------------------------------------------
select_ids() {
  local changed base md id src
  # Diff against the MERGE-BASE of MAIN_REF and HEAD, not the MAIN_REF tip: if
  # main advanced after this branch was cut, diffing the tip would report files
  # changed on main as "changed here" and over-select. The `-- '*.rs'` pathspec
  # both restricts the diff to Rust sources and terminates option parsing, so a
  # stray REEL_ADAPTER_MAIN_REF value cannot be read as a git option.
  base="$(git merge-base "$MAIN_REF" HEAD 2>/dev/null || true)"
  changed="$(git diff --name-only "$base" -- '*.rs' 2>/dev/null | sed -E 's#.*/##' | sort -u || true)"
  [[ -d "$RECORDINGS_DIR" ]] || return 0
  for md in "$RECORDINGS_DIR"/*/test.md; do
    [[ -f "$md" ]] || continue
    id="$(basename "$(dirname "$md")")"
    [[ -f "$RECORDINGS_DIR/$id/full-stream.cast" ]] || continue   # (1) e2e proxy
    src="$(extract_source_basename "$md")"
    [[ -n "$src" ]] || continue
    if printf '%s\n' "$changed" | grep -Fxq "$src"; then          # (2) changed vs main
      printf '%s\n' "$id"
    fi
  done
}

# --------------------------------------------------------------------------
# Compose a descriptive reel title for the engine's --title (repo-specific).
# Format:  '<repo> · PRD #<prd> · PR #<pr> — <short desc>'
#   repo      <- basename of the origin remote URL, minus a trailing '.git'.
#   prd       <- digits after the leading 'prd-' in the current branch name.
#   pr        <- open PR number for this branch (gh); OMITTED when there is none.
#   short desc<- H1 of prds/<prd>-*.md, minus a leading 'PRD #<n>:' prefix.
# Every piece degrades gracefully: a missing repo/prd/pr drops just its segment,
# and a missing PRD heading falls back to a sane default — composition never
# errors, so a manual/dogfood run on an off-pattern branch still yields a title
# (or the caller overrides the whole thing with --title).
compose_title() {
  local repo prd pr desc branch prd_file head

  repo="$(git remote get-url origin 2>/dev/null || true)"
  repo="${repo%.git}"      # strip a trailing .git
  repo="${repo##*/}"       # basename (works for https and scp-style remotes)

  branch="$(git rev-parse --abbrev-ref HEAD 2>/dev/null || true)"
  prd="$(printf '%s' "$branch" | sed -nE 's/^prd-([0-9]+).*/\1/p')"

  # Open PR number for the current branch, if any. No PR -> gh exits non-zero
  # and pr stays empty, so the ' · PR #<pr>' segment is simply omitted.
  pr="$(gh pr view --json number --jq '.number' 2>/dev/null || true)"

  desc=""
  if [[ -n "$prd" ]]; then
    prd_file="$(ls "prds/${prd}-"*.md 2>/dev/null | head -1 || true)"
    if [[ -n "$prd_file" && -f "$prd_file" ]]; then
      # H1, minus the leading '# ', then minus a leading 'PRD #<n>:' prefix.
      desc="$(grep -m1 '^# ' "$prd_file" 2>/dev/null | sed -E 's/^# +//; s/^PRD #?[0-9]+:[[:space:]]*//')"
    fi
  fi
  [[ -n "$desc" ]] || desc="demo reel"

  head="${repo:-repo}"
  [[ -n "$prd" ]] && head="$head · PRD #$prd"
  [[ -n "$pr"  ]] && head="$head · PR #$pr"
  printf '%s — %s' "$head" "$desc"
}

# --------------------------------------------------------------------------
# Dispatch + arg parsing.
# --------------------------------------------------------------------------
cmd="reel"
case "${1:-}" in
  select|assemble|reel|title) cmd="$1"; shift ;;
  -h|--help) usage; exit 0 ;;
esac

out=""
publish=""
manifest="manifest.json"
# Empty means "compose a descriptive title from repo/branch/PR/PRD"; a caller may
# pass --title VALUE to override that composition verbatim (for manual/dogfood
# runs where the branch/PRD don't match the clips). Forwarded to the engine.
title=""
ids=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --out)      out="${2:?--out needs a value}"; shift 2 ;;
    --publish)  publish=1; shift ;;
    --manifest) manifest="${2:?--manifest needs a value}"; shift 2 ;;
    --title)    title="${2:?--title needs a value}"; shift 2 ;;
    -h|--help)  usage; exit 0 ;;
    --*)        die "unknown option: $1" ;;
    *)          ids+=("$1"); shift ;;
  esac
done

case "$cmd" in
  select)
    select_ids
    ;;

  assemble)
    assemble "$manifest" ${ids[@]+"${ids[@]}"}
    ;;

  title)
    # Dry-run inspection: print the title the reel pipeline would pass to the
    # engine on the current branch (the --title override verbatim, otherwise the
    # composed title). No selection, no manifest, no engine — safe to run anytime.
    printf '%s\n' "${title:-$(compose_title)}"
    ;;

  reel)
    mapfile -t scope < <(select_ids)
    if [[ ${#scope[@]} -eq 0 ]]; then
      echo "$SKIP_MSG"
      exit 0
    fi
    rm -f "$manifest"
    assemble "$manifest" "${scope[@]}"
    # assemble clean-skipped (every selected dir turned out to be cast-less).
    [[ -f "$manifest" ]] || exit 0
    [[ -x "$ENGINE" ]] || die "engine not found or not executable: $ENGINE"
    # Compose a descriptive title unless the caller pinned one with --title, and
    # always forward it to the engine so the uploaded video is named for the PRD
    # rather than the default 'reel' basename.
    reel_title="${title:-$(compose_title)}"
    engine_args=("$manifest" --title "$reel_title")
    [[ -n "$out" ]] && engine_args+=(--out "$out")
    [[ -n "$publish" ]] && engine_args+=(--publish)
    echo "demo-reel-adapter: invoking engine: $ENGINE ${engine_args[*]}" >&2
    exec "$ENGINE" "${engine_args[@]}"
    ;;
esac
