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
#       --out/--publish. Clean-skips when nothing is in scope, naming WHICH of
#       the two causes it hit: nothing changed, or things changed but carry no
#       ` [reel]` marker (issue #735 — the two call for opposite responses).
#
# Selection rule (file-level granularity, robustness over cleverness):
#   A recording dir `<RECORDINGS_DIR>/<id>/` is IN SCOPE iff
#     1. it contains a `full-stream.cast` — the e2e proxy. L1 render tests emit a
#        `test.md` but NO cast, so they are excluded by construction; and
#     2. its catalog id carries the OPT-IN `[reel]` eligibility MARKER (see below); and
#     3. the source file named in its `test.md` `**Source:**` line was changed on
#        this branch vs `<MAIN_REF>`. Matching is by FILE BASENAME against
#        `git diff --name-only <MAIN_REF>` restricted to `*.rs` — basename match
#        sidesteps the test.md "<immediate-parent>/<file>" path quirk and is
#        robust for the flat `tests/*.rs` (and `src/*.rs`) layout this repo uses.
#
# Reel-eligibility MARKER (opt-in, committed, explicit — PRD #20):
#   Having a cast just means a test is PTY-attached; it does NOT mean it belongs
#   in the reel. A clip exists so a human can watch and validate REAL behavior, so
#   only tests that genuinely spin up a real agent (spawn -> agent -> work) should
#   ship. Eligibility is therefore OPT-IN: an author marks a test by appending a
#   trailing ` [reel]` tag to its `##### <id> — …` line in CATALOG.md. The DEFAULT
#   (no tag, or an id absent from the catalog) is NOT eligible, so synthetic /
#   stand-in tests (cat, scripted echo, recorder stubs, terminal-probe, synthesized
#   hook events) never auto-select as clips even when they have a cast and their
#   source changed. The marker lives on the catalog line the adapter ALREADY parses
#   for ordering — no gitignored artifact, no Rust macro change. Both `select`
#   (concern a) and `assemble` (concern b) enforce it, so an injected id list can
#   no more smuggle an unmarked test in than a cast-less one.
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
MAIN_REF="${REEL_ADAPTER_MAIN_REF:-origin/main}"
ENGINE="${REEL_ADAPTER_ENGINE:-$SCRIPT_DIR/../demo-reel/reel.sh}"

SKIP_MSG="skipped: no e2e tests changed on this branch"

# Where `select_ids` records the ids it dropped for a MISSING ` [reel]` marker —
# the near-misses that cleared every other gate (they have a cast, and their
# source changed on this branch), so they are exactly what a reader would
# otherwise go hunting for. A FILE rather than a variable because `reel` reads
# `select_ids` through a process substitution, which runs it in a subshell whose
# variables never make it back. Empty (the default, and what `build.sh select`
# uses) means "record nothing".
INELIGIBLE_LOG=""

die() { echo "demo-reel-adapter: $*" >&2; exit 1; }

# The clean-skip line. BOTH skip paths write no manifest, invoke no engine and
# exit 0 — only the WORDING differs, and it must, because the two causes call for
# opposite responses (issue #735):
#   * nothing in scope at all      -> a reel was never possible; nothing to do.
#   * in scope but not [reel]-marked -> a deliberate opt-out is in force, and the
#     reader may want to know WHICH tests and whether the marker is rightly
#     absent. Naming the ids is what makes that case actionable; the old generic
#     wording sent readers to debug the diff gate, which was working fine.
# Composed in ONE place, called from both paths, so the two messages cannot drift
# back into saying the same wrong thing.
#
# Exclusions arrive as TWO buckets separated by a literal `--`, because a list can
# lose ids to either gate and the composed reason has to survive BOTH firing at
# once (PR #778 review). An id is never `--` itself — ids are single recording-dir
# names, and `assemble` rejects `/` and `..` before this is ever called.
#   usage: skip_message <scope-phrase> [cast-less-id...] -- [unmarked-id...]
skip_message() {
  local scope="$1"; shift
  # Declared one per line: macOS ships /bin/bash 3.2.57 and this file already
  # carries one bash-3.2 scar (issue #593's `mapfile`), so array declarations
  # stay in the single-target form the rest of the script uses.
  local cast_less=()
  local unmarked=()
  local past_sep="" arg
  for arg in "$@"; do
    if [[ -z "$past_sep" && "$arg" == "--" ]]; then past_sep=1; continue; fi
    if [[ -n "$past_sep" ]]; then unmarked+=("$arg"); else cast_less+=("$arg"); fi
  done

  # Nothing was dropped for a missing marker, so the scope gate really is the
  # whole story. A cast-less-ONLY list lands here deliberately: "no e2e tests
  # changed" is literally true of a list that contains no e2e test (a dir with no
  # cast is an L1 render test, not a clip candidate), and each such id has already
  # had its own naming diagnostic on stderr mid-loop.
  if [[ ${#unmarked[@]} -eq 0 ]]; then
    printf '%s\n' "$SKIP_MSG"
    return 0
  fi

  printf '%s\n' "skipped: ${#unmarked[@]} e2e test(s) $scope, but none is reel-eligible — no [reel] marker in $CATALOG_FILE for: $(join_ids "${unmarked[@]}")"
  # The marker is not the whole reason when the same list ALSO lost ids before
  # they ever reached the marker gate: saying only "none is reel-eligible" would
  # scope that verdict over the cast-less ones too and send a reader to add a
  # marker that would change nothing. This PR exists because a skip named the
  # wrong reason — so name both when both apply.
  if [[ ${#cast_less[@]} -gt 0 ]]; then
    printf '%s\n' "  (a further ${#cast_less[@]} id(s) in the same list were dropped EARLIER, for an unrelated reason — no full-stream.cast, so not an e2e clip at all and no [reel] marker would help: $(join_ids "${cast_less[@]}"))"
  fi
  printf '%s\n' "  ([reel] is OPT-IN and its absence is usually the right answer: a clip exists so a human can watch REAL behavior, so only a test that genuinely spins up a real agent is marked — a stand-in (cat, scripted echo, recorder stubs, synthesized hook events) stays unmarked and never becomes a clip. See CLAUDE.md rule 4.)"
}

# Render an id list for a human: "a, b, c". Used for both of skip_message's
# buckets, so the two read identically.
join_ids() {
  local out="" id
  for id in "$@"; do out="${out:+$out, }$id"; done
  printf '%s' "$out"
}

usage() {
  cat <<EOF
Usage:
  build.sh [reel] [--out OUT.mp4] [--publish] [--manifest PATH] [--title TITLE]
      Select in-scope e2e tests, build a manifest, and invoke the engine.
      Clean-skips (no manifest, no engine, exit 0) when no e2e tests changed —
      or, when e2e tests DID change but none carries the [reel] marker, skips
      just as cleanly while naming those tests instead.
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
      network). Excludes any ID without a full-stream.cast, or whose catalog id
      lacks the trailing [reel] eligibility marker; orders by catalog id.
      Clean-skips when no ID resolves to a reel-eligible e2e clip, naming any ID
      dropped for a missing [reel] marker — and, when the list also lost IDs for
      having no cast, naming those separately rather than blaming the marker for
      the whole skip.

Environment overrides:
  REEL_ADAPTER_RECORDINGS_DIR  (default: .dot-agent-deck/recordings)
  REEL_ADAPTER_CATALOG         (default: tests/CATALOG.md)
  REEL_ADAPTER_MAIN_REF        (default: origin/main)
  REEL_ADAPTER_ENGINE          (default: <skill>/../demo-reel/reel.sh)
EOF
}

# --------------------------------------------------------------------------
# Pure helpers (no git, no network) — drive concern (b).
# --------------------------------------------------------------------------

# Title = the test.md H1, minus the leading "# ". A trailing ` [reel]`
# eligibility marker is stripped: the `cargo xtask docs` generator copies the
# catalog headline (marker and all) verbatim into the H1, but the marker is an
# internal selection signal — it must never appear on the card. Quoting the
# pattern makes bash strip it literally rather than as a `[...]` glob class.
extract_title() {
  local md="$1" line
  line="$(grep -m1 '^# ' "$md" 2>/dev/null || true)"
  line="${line#"# "}"
  printf '%s' "${line%" [reel]"}"
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

# Reel-eligible? True iff the id's `##### <id> — …` line in CATALOG.md ends with
# the trailing ` [reel]` marker. Opt-in: the default (no marker, or the id absent
# from the catalog) returns false, so only explicitly-marked tests are published.
# The ` [reel]` on the RHS of `==` is QUOTED, so bash matches it literally rather
# than as a `[...]` glob character class.
catalog_reel_eligible() {
  local want="$1" line
  line="$(awk -v want="$want" '
    /^##### / {
      l=$0; sub(/^##### /,"",l); id=l; sub(/ —.*/,"",id)
      if (id==want) { print; exit }
    }' "$CATALOG_FILE")"
  [[ "$line" == *" [reel]" ]]
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
  # Ids dropped for a missing ` [reel]` marker, so the clean-skip below can name
  # the real reason instead of blaming the diff (issue #735) — and, separately,
  # the ids dropped for having no cast at all. BOTH buckets are tracked because a
  # hand-written id list can populate both at once, and a skip that named only
  # the marker bucket would scope "none is reel-eligible" over cast-less ids too
  # (PR #778 review). `reel` never mixes them — `select_ids` filters cast-less
  # dirs out before `assemble` is called — but the standalone
  # `build.sh assemble <id...>` subcommand takes raw ids with no such filtering.
  local ineligible=()
  local cast_less=()
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
      cast_less+=("$id")
      continue
    fi
    [[ -f "$md" ]] || die "missing test.md for '$id': $md"
    title="$(extract_title "$md")"
    [[ -n "$title" ]] || die "no H1 title in $md"
    # catid is matched against CATALOG.md ids, so derive it from the RAW title
    # (ids are plain ASCII — no entities); only the card-bound text is decoded.
    catid="$(extract_catalog_id "$title")"
    # Opt-in eligibility: an id without the ` [reel]` marker is a candidate cast
    # (PTY-attached) but NOT a reel clip. Excluded at assembly too, so an injected
    # id list can't smuggle an unmarked test past selection's own marker check.
    if ! catalog_reel_eligible "$catid"; then
      echo "demo-reel-adapter: excluding '$id' (catalog id '$catid' has no [reel] marker — not reel-eligible)" >&2
      ineligible+=("$id")
      continue
    fi
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
    skip_message "in the given list" \
      ${cast_less[@]+"${cast_less[@]}"} -- ${ineligible[@]+"${ineligible[@]}"}
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
  local changed base md id src catid
  # The default ref is `origin/main`, so refresh the remote-tracking ref first —
  # a local `main` can lag the true remote tip and over-select tests already
  # merged upstream. Best-effort: offline / no remote just falls back to whatever
  # ref exists (the merge-base below degrades to an empty diff). Attempted ONLY
  # when MAIN_REF names an `origin/*` remote-tracking ref, so an overridden or
  # local ref (as the acceptance test uses) never touches the network.
  if [[ "$MAIN_REF" == origin/* ]]; then
    git fetch --no-tags --quiet origin "${MAIN_REF#origin/}" 2>/dev/null || true
  fi
  # Diff against the MERGE-BASE of MAIN_REF and HEAD, not the MAIN_REF tip: if
  # main advanced after this branch was cut, diffing the tip would report files
  # changed on main as "changed here" and over-select. The `-- '*.rs'` pathspec
  # both restricts the diff to Rust sources and terminates option parsing, so a
  # stray REEL_ADAPTER_MAIN_REF value cannot be read as a git option.
  base="$(git merge-base "$MAIN_REF" HEAD 2>/dev/null || true)"
  changed="$(git diff --name-only "$base" -- '*.rs' 2>/dev/null | sed -E 's#.*/##' | sort -u || true)"
  [[ -d "$RECORDINGS_DIR" ]] || return 0
  # The three gates are ANDed, so evaluation ORDER cannot change WHICH ids are
  # selected — but it does change which ones the marker gate gets to talk about,
  # so the marker is checked LAST (issue #735). Checked first, it fires for every
  # unmarked recording dir on disk (dozens), which is noise; checked last it
  # fires only for a test that WOULD have been selected — cast on disk, source
  # changed on this branch, marker absent. Those are the near-misses worth
  # naming, and the ones `INELIGIBLE_LOG` hands to the caller so a clean skip can
  # state its real reason instead of blaming the diff.
  for md in "$RECORDINGS_DIR"/*/test.md; do
    [[ -f "$md" ]] || continue
    id="$(basename "$(dirname "$md")")"
    [[ -f "$RECORDINGS_DIR/$id/full-stream.cast" ]] || continue   # (1) e2e proxy
    src="$(extract_source_basename "$md")"
    [[ -n "$src" ]] || continue
    printf '%s\n' "$changed" | grep -Fxq "$src" || continue       # (3) changed vs main
    catid="$(extract_catalog_id "$(extract_title "$md")")"
    if ! catalog_reel_eligible "$catid"; then                     # (2) [reel] marker
      # Same diagnostic assemble() emits, so the marker gate explains itself
      # wherever it fires rather than only at assembly.
      echo "demo-reel-adapter: excluding '$id' (catalog id '$catid' has no [reel] marker — not reel-eligible)" >&2
      if [[ -n "$INELIGIBLE_LOG" ]]; then
        printf '%s\n' "$id" >> "$INELIGIBLE_LOG"
      fi
      continue
    fi
    printf '%s\n' "$id"
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
    # Give select_ids somewhere to record the ids it drops for a missing [reel]
    # marker, so an empty selection can say WHICH of the two causes it hit
    # (issue #735). Read and removed immediately below rather than left to the
    # trap: this path ends in `exec`, which replaces the process without running
    # EXIT traps, so the trap only covers a `die`/interrupt before that point.
    INELIGIBLE_LOG="$(mktemp)"
    trap 'rm -f "${INELIGIBLE_LOG:-}"' EXIT INT TERM
    # A read loop rather than `mapfile -t scope`, which is bash 4: macOS ships
    # /bin/bash 3.2.57, where `mapfile` is not a builtin at all and this died
    # with `mapfile: command not found` (exit 127) before selecting anything
    # (issue #593, the same portability class as the release assembler's
    # `declare -A`). Equivalent here because `select_ids` terminates every id
    # with a newline, so no unterminated final line can be dropped.
    scope=()
    while IFS= read -r id; do
      scope+=("$id")
    done < <(select_ids)
    ineligible=()
    while IFS= read -r id; do
      ineligible+=("$id")
    done < "$INELIGIBLE_LOG"
    rm -f "$INELIGIBLE_LOG"
    if [[ ${#scope[@]} -eq 0 ]]; then
      # Still a CLEAN skip — no manifest, no engine, exit 0. Only the wording
      # changes: an empty `ineligible` means nothing was in scope at all, a
      # non-empty one means tests changed but are deliberately not reel-eligible.
      # The cast-less bucket is EMPTY here and always will be: `select_ids`'s gate
      # (1) drops cast-less dirs itself, so this path never sees one. The `--`
      # still leads, so both call sites pass the same two-bucket shape.
      skip_message "changed on this branch" -- ${ineligible[@]+"${ineligible[@]}"}
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
