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
#   (b) ASSEMBLY — "build manifest.json from a given list of IDs" (reads each
#       test.md + CATALOG.md and each recording's provenance.json, emits JSON;
#       no network):
#       `build.sh assemble [ID...] [--manifest PATH]`
#       Deterministic and fixture-testable, with ONE impure edge since issue
#       #808: it needs the commit to check provenance against, which is one
#       `git rev-parse HEAD`. `REEL_ADAPTER_EXPECT_COMMIT` overrides that, which
#       is what keeps the acceptance test offline, and the resolution is LAZY —
#       a list that reaches no provenance check never touches git.
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
# CAST PROVENANCE (issue #808) — the fourth gate, enforced at ASSEMBLY:
#   The three gates above are a `test -f` plus two STATIC facts about the test,
#   so they say nothing about the ARTIFACT: a cast an older revision left on disk
#   satisfies every one of them. PR #805 made the harness discard the previous
#   run's artifacts at launch and on the runtime-skip path, which closes the
#   routes that REACH those call sites — and two do not. A FILTERED run (the
#   normal way anyone works, and what CLAUDE.md rule 5 asks for) never selects
#   the test at all, so nothing discards its older cast; and `skip_unless!`
#   evaluates its preflight BEFORE `_skip_if_err` is entered, so a kill inside a
#   `check_*_available` — or inside an importer it calls — lands before the
#   skip-path discard. Provenance covers both at once, which is why the answer
#   was not a third discard.
#
#   The harness writes `provenance.json` beside each dump. `assemble` REFUSES a
#   clip on three of its fields and REPORTS the rest:
#     * refused when the sidecar is ABSENT, unparseable, of an unknown `schema`,
#       or missing a required field — so a pre-provenance cast and a dump that
#       died before its sidecar are both unpublishable;
#     * refused when `outcome` is not `passed` — a failure dump is a diagnostic,
#       not a clip;
#     * refused when `commit` is not the revision this reel is being built at.
#   Reported, gating nothing: `run_id` (one value per nextest run; a reel
#   legitimately spans several filtered runs at one commit, so differing ids WARN
#   rather than refuse), `build_id`, `recorded_at_unix`, `redaction_version` and
#   `dirty`.
#
#   What that proves, stated no wider than it is: a selected clip was written by
#   a harness built from THIS commit, by a run that was not unwinding a panic. It
#   does NOT prove the clip came from the LATEST run — a passing clip recorded at
#   this commit by an earlier run is accepted, and correctly so, since the code
#   that produced it is the code under test. Under `dirty` even that narrows: one
#   commit then covers more than one working state, which is why the dirty flag
#   is reported loudly. It says nothing about whether the cast's CONTENT is
#   redacted (the harness blocklist is best-effort, issue #810), and it is not a
#   signature — the sidecar sits in the same gitignored directory as the cast, so
#   whoever can write one can write the other.
#
#   Enforced at ASSEMBLY ONLY, and that is enough rather than a shortcut: every
#   route to a manifest goes through `assemble`, both the `reel` pipeline and the
#   standalone `build.sh assemble <id...>` an injected id list would use. The
#   marker gate is duplicated in `select_ids` for a different reason — so its
#   near-miss diagnostic fires during selection too — not because assembly's copy
#   is insufficient.
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

# The commit every recording's provenance must name (issue #808). Empty — the
# default — means "resolve it from git at the moment the first provenance check
# needs it", which is what the `reel` pipeline does. Setting it is an OPERATOR
# ASSERTION in the same class as `clean-e2e-tmp --ignore-liveness`: it is how the
# acceptance test stays offline and deterministic, and pointing it at the wrong
# value defeats the commit gate entirely. It cannot LOOSEN anything by accident,
# though — a wrong value refuses every clip rather than admitting a stale one.
EXPECT_COMMIT="${REEL_ADAPTER_EXPECT_COMMIT:-}"

# `provenance.json` schema this adapter understands. The harness's
# `RECORDING_PROVENANCE_SCHEMA` (tests/common/mod.rs) must match, and
# `tests/harness_isolation.rs` fails the fast tier when the two drift.
PROVENANCE_SCHEMA=1

# Shortest sha accepted as identifying a commit, mirroring the harness's
# `MIN_BUILD_COMMIT_LEN`. Git's auto-abbreviation floor is 7, so this refuses
# nothing a normal `git rev-parse --short HEAD` produces while refusing an
# abbreviation short enough that a prefix match would be weak evidence.
MIN_COMMIT_LEN=7

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
# Exclusions arrive as THREE buckets separated by literal `--`s, because a list
# can lose ids to any of the gates and the composed reason has to survive more
# than one firing at once (PR #778 review, extended for issue #808's provenance
# gate). An id is never `--` itself — ids are single recording-dir names, and
# `assemble` rejects `/` and `..` before this is ever called.
#   usage: skip_message <scope-phrase> [cast-less...] -- [unmarked...] -- [stale...]
skip_message() {
  local scope="$1"; shift
  # Declared one per line: macOS ships /bin/bash 3.2.57 and this file already
  # carries one bash-3.2 scar (issue #593's `mapfile`), so array declarations
  # stay in the single-target form the rest of the script uses.
  local cast_less=()
  local unmarked=()
  local stale=()
  local bucket=0 arg
  for arg in "$@"; do
    if [[ "$arg" == "--" ]]; then bucket=$((bucket + 1)); continue; fi
    case "$bucket" in
      0) cast_less+=("$arg") ;;
      1) unmarked+=("$arg") ;;
      *) stale+=("$arg") ;;
    esac
  done

  # Nothing reached the marker or provenance gate, so the scope gate really is
  # the whole story. A cast-less-ONLY list lands here deliberately: "no e2e tests
  # changed" is literally true of a list that contains no e2e test (a dir with no
  # cast is an L1 render test, not a clip candidate), and each such id has already
  # had its own naming diagnostic on stderr mid-loop.
  if [[ ${#unmarked[@]} -eq 0 && ${#stale[@]} -eq 0 ]]; then
    printf '%s\n' "$SKIP_MSG"
    return 0
  fi

  # The headline names the gate that dropped the most ids' worth of the reader's
  # attention, and the marker wording is kept EXACTLY as issue #735 established
  # it. A stale-only list gets its own headline rather than borrowing that one:
  # "none is reel-eligible" would send a reader to add a marker that changes
  # nothing, which is the same misattribution #735 was about, in a new bucket.
  if [[ ${#unmarked[@]} -gt 0 ]]; then
    printf '%s\n' "skipped: ${#unmarked[@]} e2e test(s) $scope, but none is reel-eligible — no [reel] marker in $CATALOG_FILE for: $(join_ids "${unmarked[@]}")"
  else
    printf '%s\n' "skipped: ${#stale[@]} e2e test(s) $scope, but none has a recording whose provenance checks out — nothing was published"
  fi
  # The marker is not the whole reason when the same list ALSO lost ids before
  # they ever reached the marker gate: saying only "none is reel-eligible" would
  # scope that verdict over the cast-less ones too and send a reader to add a
  # marker that would change nothing. This PR exists because a skip named the
  # wrong reason — so name both when both apply.
  if [[ ${#cast_less[@]} -gt 0 ]]; then
    printf '%s\n' "  (a further ${#cast_less[@]} id(s) in the same list were dropped EARLIER, for an unrelated reason — no full-stream.cast, so not an e2e clip at all and no [reel] marker would help: $(join_ids "${cast_less[@]}"))"
  fi
  # Same discipline for the provenance bucket: it is a THIRD unrelated reason,
  # and a marker will not fix it either. The per-id reason is already on stderr,
  # so this names the ids and the remedy rather than repeating each verdict.
  if [[ ${#stale[@]} -gt 0 ]]; then
    printf '%s\n' "  (${#stale[@]} id(s) were dropped by the CAST PROVENANCE gate — the recording on disk is not proven to come from a passing run of the current revision, so it may predate this branch entirely (issue #808): $(join_ids "${stale[@]}"). Each id's own verdict is on stderr above. Re-record with DOT_AGENT_DECK_RECORD=1 cargo test-e2e-live <filter> at this commit.)"
  fi
  if [[ ${#unmarked[@]} -gt 0 ]]; then
    printf '%s\n' "  ([reel] is OPT-IN and its absence is usually the right answer: a clip exists so a human can watch REAL behavior, so only a test that genuinely spins up a real agent is marked — a stand-in (cat, scripted echo, recorder stubs, synthesized hook events) stays unmarked and never becomes a clip. See CLAUDE.md rule 4.)"
  fi
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
      Build manifest.json from the given recording-dir IDs (no network; one
      `git rev-parse HEAD` unless REEL_ADAPTER_EXPECT_COMMIT is set). Excludes
      any ID without a full-stream.cast, any whose catalog id lacks the trailing
      [reel] eligibility marker, and any whose provenance.json is absent or
      records a different commit or a non-passing outcome (issue #808); orders by
      catalog id. Clean-skips when no ID resolves to a publishable clip, naming
      the IDs each gate dropped separately rather than blaming one gate for the
      whole skip.

Environment overrides:
  REEL_ADAPTER_RECORDINGS_DIR  (default: .dot-agent-deck/recordings)
  REEL_ADAPTER_CATALOG         (default: tests/CATALOG.md)
  REEL_ADAPTER_MAIN_REF        (default: origin/main)
  REEL_ADAPTER_ENGINE          (default: <skill>/../demo-reel/reel.sh)
  REEL_ADAPTER_EXPECT_COMMIT   (default: git rev-parse HEAD) — the commit each
                               recording's provenance must name. An operator
                               assertion; a wrong value refuses every clip.
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
# Cast provenance (issue #808).
# --------------------------------------------------------------------------

# The commit every recording's provenance must name. `REEL_ADAPTER_EXPECT_COMMIT`
# wins; otherwise `git rev-parse HEAD`. Resolved ONCE per run into `EXPECT_COMMIT`
# and LAZILY — only when an id actually reaches the provenance gate — so an
# empty, cast-less or all-unmarked list still needs no git at all.
#
# FAILS CLOSED. With no override and no resolvable HEAD there is nothing to check
# provenance against, and the only two options are to publish unchecked or to
# stop. It stops: `die`, not a warning, because a warning would leave the run
# publishing exactly the casts this gate exists to refuse.
resolve_expected_commit() {
  [[ -n "$EXPECT_COMMIT" ]] && return 0
  EXPECT_COMMIT="$(git rev-parse HEAD 2>/dev/null || true)"
  [[ -n "$EXPECT_COMMIT" ]] || die "cannot determine the commit to check cast provenance against: \
\`git rev-parse HEAD\` failed here and REEL_ADAPTER_EXPECT_COMMIT is unset. Refusing to \
publish unverified recordings — run from the repo checkout, or set \
REEL_ADAPTER_EXPECT_COMMIT=<sha> if you know what revision these casts came from."
  [[ ${#EXPECT_COMMIT} -ge $MIN_COMMIT_LEN ]] || die "the commit to check cast provenance \
against ('$EXPECT_COMMIT') is shorter than $MIN_COMMIT_LEN characters, so a prefix match \
against it would not identify a revision"
}

# Verdict on one recording's `provenance.json`.
#
# Returns 0 when the clip may be published and sets `PROV_SUMMARY` (plus
# `PROV_RUN_ID` and `PROV_DIRTY` for the caller's cross-clip reporting); returns
# 1 and sets `PROV_REASON` to a human sentence otherwise. Globals rather than
# stdout because `assemble` runs in the main shell and needs several values back;
# a command substitution would also swallow the exit status distinction.
#
# Every branch below is a REFUSAL — there is no "warn and include" path, because
# the whole point is that the previous behaviour (`test -f`) already included
# everything.
PROV_REASON=""
PROV_SUMMARY=""
PROV_RUN_ID=""
PROV_DIRTY=""
check_provenance() {
  local id="$1"
  local file="$RECORDINGS_DIR/$id/provenance.json"
  PROV_REASON=""; PROV_SUMMARY=""; PROV_RUN_ID=""; PROV_DIRTY=""

  if [[ ! -f "$file" ]]; then
    PROV_REASON="no provenance.json beside the cast — the recording predates the provenance sidecar, or its dump died before writing it (the sidecar is written LAST for exactly this reason)"
    return 1
  fi

  # One jq call for every field, so a malformed file fails once here rather than
  # eight times below. `// ""` turns an absent field into an empty string, which
  # the required-field check then names.
  #
  # Joined on US (0x1f), NOT a tab, and the difference is load-bearing: bash
  # treats tab as IFS *whitespace*, so a run of them collapses to one delimiter
  # and every field after an empty one shifts left — which silently read the
  # build_id as the commit and admitted a sidecar with no commit at all. A
  # non-whitespace IFS delimits once per occurrence, so empty fields survive.
  # 0x1f cannot occur in any of these values (jq would escape it in JSON).
  local fields
  if ! fields="$(jq -r '
        [ ((.schema // "") | tostring),
          (.outcome // ""),
          (.commit // ""),
          (.build_id // ""),
          (.run_id // ""),
          ((.recorded_at_unix // "") | tostring),
          ((.redaction_version // "") | tostring),
          ((.dirty // false) | tostring) ] | join("\u001f")' "$file" 2>/dev/null)"; then
    PROV_REASON="provenance.json is not readable JSON — a corrupt sidecar cannot vouch for a cast"
    return 1
  fi

  local schema outcome commit build_id run_id recorded_at redaction_version dirty
  IFS=$'\037' read -r schema outcome commit build_id run_id recorded_at redaction_version dirty <<EOF
$fields
EOF

  if [[ "$schema" != "$PROVENANCE_SCHEMA" ]]; then
    PROV_REASON="provenance.json declares schema '$schema', and this adapter only understands $PROVENANCE_SCHEMA — refusing rather than guessing what its fields mean"
    return 1
  fi

  # Required-field completeness, checked as a set so the message names every
  # missing one at once. A sidecar missing a field is a sidecar this adapter
  # cannot read the way it was written, whether the gap is a harness bug or a
  # hand-edit.
  local missing=""
  [[ -n "$outcome" ]]           || missing="${missing:+$missing, }outcome"
  [[ -n "$commit" ]]            || missing="${missing:+$missing, }commit"
  [[ -n "$build_id" ]]          || missing="${missing:+$missing, }build_id"
  [[ -n "$run_id" ]]            || missing="${missing:+$missing, }run_id"
  [[ -n "$recorded_at" ]]       || missing="${missing:+$missing, }recorded_at_unix"
  [[ -n "$redaction_version" ]] || missing="${missing:+$missing, }redaction_version"
  if [[ -n "$missing" ]]; then
    PROV_REASON="provenance.json is missing required field(s): $missing"
    return 1
  fi

  if [[ "$outcome" != "passed" ]]; then
    PROV_REASON="the recording came from a run whose outcome was '$outcome', not 'passed' — a failure dump is a diagnostic, not a clip"
    return 1
  fi

  # The commit gate. An EMPTY commit never reaches here — the required-field
  # check above already refused it — and that matters, because the harness writes
  # "" whenever `DAD_BUILD_ID` carries no usable sha (a git-less or shallow
  # build, or an operator-injected id). A binary that cannot say which commit it
  # came from cannot support a provenance claim, so "" is a refusal rather than a
  # wildcard.
  resolve_expected_commit
  # Prefix match in whichever direction is shorter, because one side is a short
  # sha (`DAD_BUILD_ID`'s abbreviation, whose length follows core.abbrev) and the
  # other is normally a full 40. The shorter side must still be long enough to
  # identify a revision.
  local short="$commit" long="$EXPECT_COMMIT"
  if [[ ${#EXPECT_COMMIT} -lt ${#commit} ]]; then short="$EXPECT_COMMIT"; long="$commit"; fi
  if [[ ${#short} -lt $MIN_COMMIT_LEN ]]; then
    PROV_REASON="provenance.json records commit '$commit', which is shorter than $MIN_COMMIT_LEN characters — too short to identify a revision"
    return 1
  fi
  if [[ "$long" != "$short"* ]]; then
    PROV_REASON="the recording was made at commit $commit, but this reel is being built at ${EXPECT_COMMIT:0:12} — the cast comes from a DIFFERENT revision, possibly one predating the current redaction (issues #502/#785)"
    return 1
  fi

  PROV_RUN_ID="$run_id"
  PROV_DIRTY="$dirty"
  PROV_SUMMARY="commit $commit · run $run_id · build $build_id · redaction v$redaction_version · recorded $(human_time "$recorded_at")"
  [[ "$dirty" == "true" ]] && PROV_SUMMARY="$PROV_SUMMARY · TREE WAS DIRTY"
  return 0
}

# Epoch seconds -> something a human can read, degrading to the raw number.
# The harness records epoch seconds because it has no date-formatting dependency;
# formatting belongs wherever `date` exists. GNU takes `-d @N`, BSD/macOS takes
# `-r N`, and a host with neither still gets the number.
human_time() {
  local epoch="$1"
  date -u -d "@$epoch" +'%Y-%m-%dT%H:%M:%SZ' 2>/dev/null \
    || date -u -r "$epoch" +'%Y-%m-%dT%H:%M:%SZ' 2>/dev/null \
    || printf 'epoch %s' "$epoch"
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
  # Issue #808: ids whose recording failed the provenance gate. A third bucket
  # rather than folding them into `ineligible`, because "not reel-eligible" is a
  # statement about the TEST (its catalog marker) while this is a statement about
  # the ARTIFACT on disk, and the two call for opposite responses — one may want
  # a marker added, the other wants a re-record and never a marker.
  local stale=()
  local run_ids="" any_dirty=""
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
    # Provenance LAST, for the same reason the marker gate is checked last in
    # `select_ids`: its verdict is only worth printing about an id that would
    # otherwise have been published. An unmarked test's stale cast is not a
    # near-miss anyone needs to hear about.
    if ! check_provenance "$id"; then
      echo "demo-reel-adapter: excluding '$id' ($PROV_REASON)" >&2
      stale+=("$id")
      continue
    fi
    echo "demo-reel-adapter: including '$id' — provenance OK: $PROV_SUMMARY" >&2
    # Collected for the two cross-clip notes below. Newline-delimited in a
    # string rather than an array so `sort -u` can count the distinct ids
    # without a bash-4 associative array (macOS ships /bin/bash 3.2.57).
    run_ids="$run_ids$PROV_RUN_ID
"
    [[ "$PROV_DIRTY" == "true" ]] && any_dirty=1
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
      ${cast_less[@]+"${cast_less[@]}"} -- ${ineligible[@]+"${ineligible[@]}"} \
      -- ${stale[@]+"${stale[@]}"}
    return 0
  fi

  # Some ids passed provenance and some did not: the manifest is still written
  # from the ones that did, because dropping a whole reel over one stale clip
  # would only teach people to bypass the gate. Named here so the omission is
  # visible in the same place the manifest is announced, not only mid-loop.
  if [[ ${#stale[@]} -gt 0 ]]; then
    echo "demo-reel-adapter: WARNING — ${#stale[@]} clip(s) were left OUT by the provenance gate and are NOT in this reel: $(join_ids "${stale[@]}"). Re-record them at this commit if the reel is meant to include them." >&2
  fi

  # Two cross-clip notes, both advisory. Neither can be a refusal without
  # refusing something correct: a reel legitimately assembles clips from several
  # FILTERED runs at one commit (which is how CLAUDE.md rule 5 asks people to
  # work), and recording from a dirty tree is the ordinary dogfood case.
  local distinct_runs
  distinct_runs="$(printf '%s' "$run_ids" | sort -u | grep -c . || true)"
  if [[ "${distinct_runs:-0}" -gt 1 ]]; then
    echo "demo-reel-adapter: NOTE — these clips come from $distinct_runs distinct recording runs (all at the expected commit). Legitimate for clips recorded by separate filtered runs; worth a look if you expected one run." >&2
  fi
  if [[ -n "$any_dirty" ]]; then
    echo "demo-reel-adapter: WARNING — at least one clip was recorded from a DIRTY tree, so its commit does not fully identify the code that produced it (two working states share one commit). Watch the reel before flipping it public." >&2
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
#
# Deliberately does NOT check cast provenance (issue #808). "In scope" is a
# statement about the TEST — did its source change on this branch, is it marked,
# is it an e2e test at all — and provenance is a statement about the ARTIFACT,
# which is `assemble`'s business. Keeping the split means `build.sh select` stays
# a pure scope query with no git-HEAD dependency of its own, and there is exactly
# one place the publish decision is made. Nothing escapes through the gap: every
# route to a manifest runs `assemble`.
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
      # (1) drops cast-less dirs itself, so this path never sees one. The STALE
      # bucket is empty for a different reason — provenance is checked at
      # ASSEMBLY, so a stale id is still SELECTED here and reaches `assemble`,
      # which then clean-skips with its own three-bucket message. Both trailing
      # `--`s still lead, so every call site passes the same shape.
      skip_message "changed on this branch" -- ${ineligible[@]+"${ineligible[@]}"} --
      exit 0
    fi
    rm -f "$manifest"
    assemble "$manifest" "${scope[@]}"
    # assemble clean-skipped: either every selected dir turned out to be
    # cast-less, or — since issue #808 — every one of them failed the provenance
    # gate. `assemble` has already said which on stdout, naming the ids.
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
