#!/usr/bin/env bash
#
# adapter_test.sh — re-runnable acceptance for the demo-reel ADAPTER (PRD #180 M2.1).
#
# NO agg/ffmpeg, NO network — so this MAY run in CI (unlike the engine smoke and
# the reel step itself, which are local-only). Sections (i)–(ii) are PURE shell
# and drive the adapter's deterministic concern (b), `build.sh assemble`, against
# a tiny fixture; sections (iii)–(iv) drive the `reel` path, which needs concern
# (a) and therefore git. They assert:
#
#   (i)   given a list of IDs, the emitted manifest has the right
#         titles/descriptions/clip paths IN CATALOG ORDER, EXCLUDES the cast-less
#         L1 entry, and EXCLUDES a cast-bearing entry that is NOT reel-marked;
#   (ii)  given an empty in-scope list, it CLEAN-SKIPS — no manifest, exit 0, and
#         the "nothing changed" skip message;
#   (ii-b) an L1-only list CLEAN-SKIPS with that same message;
#   (ii-c) an unmarked-cast-only list CLEAN-SKIPS just as cleanly but with the
#         DIFFERENT, reel-eligibility message that NAMES the excluded id;
#   (ii-d) a MIXED list — one id dropped for having no cast, one for having no
#         marker — names BOTH reasons rather than attributing the whole skip to
#         the marker (PR #778 review; reachable only via standalone `assemble`);
#   (ii-e) a marked e2e test with NO cast is reported as UNRECORDED, with the
#         DOT_AGENT_DECK_RECORD=1 remedy, while an unmarked one is reported as
#         unmarked — never "no e2e tests changed" (issue #1683) — and a list
#         holding both headlines the unrecorded one;
#   (iii) the `reel` path on a branch that DID change an e2e test whose catalog
#         entry is unmarked skips with that eligibility message too — never the
#         "no e2e tests changed" one, which is the issue #735 defect;
#   (iv)  the `reel` path on a branch that changed a MARKED test still selects,
#         assembles and invokes the engine (a stub), and reports no near-miss;
#   (vi)  the `reel` path on a branch whose changed e2e tests have NO recording
#         says so, counts them, names the remedy and prints the per-id
#         diagnostic (issue #1683's repro); a branch that changed only an L1
#         test still says "no e2e tests changed"; a reel built from the other
#         changed tests warns that the unrecorded one is missing; and a skip
#         after assembly still counts the unrecorded one;
#   (vii) `build.sh title` on a `prd-<n>` branch with no prds/<n>-*.md takes the
#         description from the PRD issue's title through a stub `gh` (issue
#         #1821), strips its prefix and control characters without evaluating
#         it, falls back to "demo reel" when gh fails, and still prefers a
#         prds/ file when one exists;
#   (v)   the CAST PROVENANCE gate (issue #808) refuses a clip whose sidecar is
#         absent, corrupt, of an unknown schema, missing a required field,
#         recorded at a different commit, or recorded from a non-passing run —
#         while a valid sidecar publishes, a stale one does not take its
#         siblings down with it, and a dirty/multi-run reel publishes with a
#         warning rather than a refusal.
#
# WHY (iii)/(iv) shell out to git: the defect in #735 lives in the SELECTION half,
# which exists to run `git diff`, so covering it anywhere else covers something
# else. They follow the same discipline CLAUDE.md rule 5 sets for the `xtask`
# real-git tests — every repository is built inside the test's own `mktemp -d`
# with the ambient git configuration switched off, so nothing can read or write
# the checkout this runs in, and there is no network and no sleep. They SKIP
# (without failing) where git is unavailable.
#
# WHY (v) mutates COPIES of the fixture: the provenance gate has one accepting
# state and seven refusing ones, and committing a fixture dir per refusal would
# bury the interesting file among near-identical trees. Each case instead copies
# the committed fixture into $TMP and edits exactly one field of one
# provenance.json, so the diff between "publishes" and "refused" is visible in
# the test body. Still pure shell — a copy and a `jq`, no git and no network.
#
# The fixture under tests/fixtures/recordings/ has SIX dirs:
#   * alpha, beta — e2e dirs WITH a cast whose catalog entry carries ` [reel]`;
#   * gamma       — an L1 dir (test.md but NO cast), so excluded by construction;
#   * delta       — an e2e dir WITH a cast but whose catalog entry is UNMARKED,
#                   so excluded by the opt-in reel-eligibility marker;
#   * epsilon     — a MARKED e2e dir holding only test.md: an unrecorded test;
#   * zeta        — an UNMARKED e2e dir holding only test.md.
# Each cast-bearing dir also carries a `provenance.json` recording a synthetic
# commit, which `REEL_ADAPTER_EXPECT_COMMIT` below pins — so every section stays
# independent of the real checkout's HEAD and needs no git to accept a clip.
# The CATALOG.md fixture orders them 001=beta, 002=alpha, 003=gamma, 004=delta, so
# feeding `alpha beta gamma delta` and getting back exactly `[beta, alpha]` proves
# ordering, the L1 exclusion, AND the unmarked-cast exclusion at once. alpha and
# beta share one **Source:** file; delta deliberately names its OWN, so (iii) can
# change the unmarked test's source WITHOUT changing a marked one's.
#
# Run via: task reel-adapter-test
#   (or directly: .claude/skills/demo-reel-adapter/tests/adapter_test.sh)
#
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BUILD="$HERE/../build.sh"
FIX="$HERE/fixtures"

export REEL_ADAPTER_RECORDINGS_DIR="$FIX/recordings"
export REEL_ADAPTER_CATALOG="$FIX/CATALOG.md"

# The commit the fixture provenance records (issue #808). Pinning it is what
# keeps every section below offline and independent of the checkout's HEAD;
# (v-i)/(v-j) unset it deliberately to exercise the git-resolution path and its
# fail-closed edge.
FIXTURE_COMMIT="aaaaaaabbbbbbbccccccc"
export REEL_ADAPTER_EXPECT_COMMIT="$FIXTURE_COMMIT"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
fail() { echo "ADAPTER TEST FAIL: $*" >&2; exit 1; }
# `says <needle> <haystack>` — a predicate, so assertions read as `if says …` /
# `says … || fail`, never as `grep … && fail`, whose exit status is a `set -e`
# trap in its own right.
says() { printf '%s\n' "$2" | grep -qF -- "$1"; }

# `rec_copy <case>` — a private copy of the fixture recordings tree, echoed so a
# case can mutate one provenance.json without touching the committed fixture (or
# any other case). Issue #808.
rec_copy() {
  local dest="$TMP/rec-$1"
  rm -rf "$dest"
  mkdir -p "$dest"
  cp -R "$FIX/recordings/." "$dest/"
  printf '%s' "$dest"
}

# `prov_set <tree> <dir> <jq-filter>` — rewrite one recording's provenance.json
# through jq. A temp file + mv because jq cannot edit in place.
prov_set() {
  local file="$1/$2/provenance.json"
  jq "$3" "$file" > "$file.new"
  mv "$file.new" "$file"
}

# The two skip messages must stay DISTINCT — collapsing them back into one is the
# issue #735 regression, so every skip assertion below checks both directions.
NOTHING_CHANGED="skipped: no e2e tests changed on this branch"
NOT_ELIGIBLE="but none is reel-eligible"
NOT_RECORDED="but none has a recording"

# --- (i) assemble alpha beta gamma delta -> [beta(001), alpha(002)];
#         gamma excluded (no cast), delta excluded (cast but NOT [reel]-marked) --
MAN="$TMP/manifest.json"
"$BUILD" assemble --manifest "$MAN" alpha beta gamma delta >/dev/null

[[ -s "$MAN" ]] || fail "(i) manifest not written"

len="$(jq 'length' "$MAN")"
[[ "$len" -eq 2 ]] || fail "(i) expected 2 entries, got $len (L1 gamma AND unmarked delta must be excluded)"

# Beta's fixture title/description carry HTML entities (&#91;label&#93;, &amp;) —
# the adapter must HTML-decode them to literal characters on the card while
# leaving the catalog id (the part before " — ", used for ordering) untouched.
t0="$(jq -r '.[0].title' "$MAN")"
t1="$(jq -r '.[1].title' "$MAN")"
[[ "$t0" == "mouse/button/001 — Beta renders its [label]." ]]  || fail "(i) entry 0 title (entities must decode): '$t0'"
[[ "$t1" == "mouse/button/002 — Alpha renders its label." ]]   || fail "(i) entry 1 title: '$t1'"

d0="$(jq -r '.[0].description' "$MAN")"
d1="$(jq -r '.[1].description' "$MAN")"
[[ "$d0" == "Beta scenario: start the app & confirm the beta widget renders its [label]." ]] || fail "(i) entry 0 desc (entities must decode): '$d0'"
[[ "$d1" == "Alpha scenario: start the app and confirm the alpha widget renders its label." ]] || fail "(i) entry 1 desc: '$d1'"

c0="$(jq -r '.[0].clip' "$MAN")"
c1="$(jq -r '.[1].clip' "$MAN")"
[[ "$c0" == "$FIX/recordings/beta/full-stream.cast" ]]  || fail "(i) entry 0 clip: '$c0'"
[[ "$c1" == "$FIX/recordings/alpha/full-stream.cast" ]] || fail "(i) entry 1 clip: '$c1'"

if jq -e '[.[].clip] | any(. | test("gamma"))' "$MAN" >/dev/null; then
  fail "(i) cast-less L1 'gamma' leaked into the manifest"
fi
if jq -e '[.[].clip] | any(. | test("delta"))' "$MAN" >/dev/null; then
  fail "(i) unmarked cast-bearing 'delta' leaked into the manifest (missing [reel] marker must exclude it)"
fi
echo "PASS (i): 2 entries in catalog order (beta, alpha); L1 gamma AND unmarked delta excluded; fields correct"

# --- (ii) empty in-scope list -> clean skip (no manifest, exit 0, skip message) --
MAN2="$TMP/skip.json"
out="$("$BUILD" assemble --manifest "$MAN2")"
[[ ! -e "$MAN2" ]] || fail "(ii) manifest must NOT be written on a clean skip"
says "$NOTHING_CHANGED" "$out" || fail "(ii) missing skip message; got: '$out'"
if says "$NOT_ELIGIBLE" "$out"; then
  fail "(ii) nothing was excluded for a missing marker, so the eligibility message must NOT appear; got: '$out'"
fi
echo "PASS (ii): empty list clean-skips (no manifest, exit 0, 'nothing changed' message)"

# --- (ii-b) a list of only L1 (cast-less) ids also clean-skips ------------------
MAN3="$TMP/skip2.json"
out3="$("$BUILD" assemble --manifest "$MAN3" gamma)"
[[ ! -e "$MAN3" ]] || fail "(ii-b) manifest must NOT be written when only L1 ids are given"
says "$NOTHING_CHANGED" "$out3" || fail "(ii-b) missing skip message for L1-only list; got: '$out3'"
if says "$NOT_ELIGIBLE" "$out3"; then
  fail "(ii-b) gamma was dropped for having no cast, not for the marker — the eligibility message must NOT appear; got: '$out3'"
fi
echo "PASS (ii-b): L1-only list clean-skips with the 'nothing changed' message"

# --- (ii-c) only UNMARKED cast-bearing ids -> clean skip, ELIGIBILITY message ---
# delta has a full-stream.cast but its catalog entry lacks the ` [reel]` marker,
# so an all-unmarked list resolves to zero reel-eligible clips. It must still
# clean-skip (no manifest, exit 0) — but say WHY, naming delta, rather than claim
# nothing changed (issue #735).
MAN4="$TMP/skip3.json"
out4="$("$BUILD" assemble --manifest "$MAN4" delta 2>/dev/null)"
[[ ! -e "$MAN4" ]] || fail "(ii-c) manifest must NOT be written when only unmarked cast ids are given"
says "$NOT_ELIGIBLE" "$out4" || fail "(ii-c) skip must state the reel-eligibility reason; got: '$out4'"
says "no [reel] marker" "$out4" || fail "(ii-c) skip must name the missing [reel] marker; got: '$out4'"
says "delta" "$out4" || fail "(ii-c) skip must NAME the excluded id, which is what makes it actionable; got: '$out4'"
if says "$NOTHING_CHANGED" "$out4"; then
  fail "(ii-c) the two skip messages must not collapse back into one (issue #735); got: '$out4'"
fi
echo "PASS (ii-c): unmarked-cast-only list clean-skips with the eligibility message, naming delta"

# --- (ii-d) MIXED exclusions -> the skip names BOTH reasons, not just one -------
# `gamma delta` loses one id to EACH gate: gamma has no cast (never reaches the
# marker check), delta has a cast but no ` [reel]` marker. The composed reason has
# to account for both. Before the PR #778 review fix it named only the marker
# bucket, so "none is reel-eligible" silently scoped over gamma as well and a
# reader chasing it would have added a marker that changes nothing.
#
# Unreachable from the automated `reel` pipeline — `select_ids` filters cast-less
# dirs out before `assemble` sees them — so this covers the standalone
# `build.sh assemble <id...>` subcommand, which takes raw ids with no filtering.
MAN5="$TMP/skip4.json"
out5="$("$BUILD" assemble --manifest "$MAN5" gamma delta 2>/dev/null)"
[[ ! -e "$MAN5" ]] || fail "(ii-d) manifest must NOT be written when no id resolves to a clip"
says "no [reel] marker" "$out5" || fail "(ii-d) skip must still name the marker reason; got: '$out5'"
says "delta" "$out5" || fail "(ii-d) skip must name the marker-rejected id; got: '$out5'"
says "no full-stream.cast" "$out5" \
  || fail "(ii-d) skip must ALSO name the cast-less reason, not attribute the whole skip to markers; got: '$out5'"
says "gamma" "$out5" \
  || fail "(ii-d) skip must NAME the cast-less id too — omitting it is the defect this case guards; got: '$out5'"
if says "$NOTHING_CHANGED" "$out5"; then
  fail "(ii-d) a marker-rejected id is present, so this must not fall back to the generic message; got: '$out5'"
fi
echo "PASS (ii-d): mixed cast-less + unmarked list names BOTH reasons (gamma and delta)"

# --- (ii-e) e2e tests with NO cast -> unrecorded vs unmarked (issue #1683) ------
# epsilon and zeta are e2e tests (their source is an `e2e_*.rs` file) whose dirs
# hold only test.md. Before #1683 both fell into the L1 bucket and the skip said
# "no e2e tests changed". epsilon is marked, so recording it is the remedy;
# zeta is not, so recording it would change nothing and it is reported as
# unmarked instead.
MAN_E="$TMP/skip-unrecorded.json"
ERR_E="$TMP/skip-unrecorded.err"
outE="$("$BUILD" assemble --manifest "$MAN_E" epsilon 2>"$ERR_E")"
[[ ! -e "$MAN_E" ]] || fail "(ii-e) manifest must NOT be written when the only id has no recording"
says "skipped: 1 e2e test(s) in the given list, $NOT_RECORDED" "$outE" \
  || fail "(ii-e) skip must count the unrecorded e2e test; got: '$outE'"
says "DOT_AGENT_DECK_RECORD=1" "$outE" || fail "(ii-e) skip must name the recording remedy; got: '$outE'"
says "epsilon" "$outE" || fail "(ii-e) skip must NAME the unrecorded id; got: '$outE'"
if says "$NOTHING_CHANGED" "$outE"; then
  fail "(ii-e) epsilon is an e2e test — 'no e2e tests changed' is the issue #1683 defect; got: '$outE'"
fi
says "excluding 'epsilon' (e2e test with no full-stream.cast" "$(cat "$ERR_E")" \
  || fail "(ii-e) the per-id diagnostic must call epsilon an unrecorded e2e test; stderr: $(cat "$ERR_E")"
if says "not an e2e clip" "$(cat "$ERR_E")"; then
  fail "(ii-e) epsilon IS an e2e test, so the L1 wording must not appear; stderr: $(cat "$ERR_E")"
fi

outZ="$("$BUILD" assemble --manifest "$MAN_E" zeta 2>/dev/null)"
[[ ! -e "$MAN_E" ]] || fail "(ii-e) manifest must NOT be written for an unmarked unrecorded id"
says "$NOT_ELIGIBLE" "$outZ" || fail "(ii-e) an unmarked unrecorded test must be reported as unmarked; got: '$outZ'"
says "zeta" "$outZ" || fail "(ii-e) skip must NAME zeta; got: '$outZ'"
if says "DOT_AGENT_DECK_RECORD=1" "$outZ"; then
  fail "(ii-e) recording zeta would not make it a clip, so the skip must not suggest it; got: '$outZ'"
fi

# A list holding BOTH must headline the unrecorded one: it is marked, so "none is
# reel-eligible" would be false and would hide the remedy (PR #1763 review).
outEZ="$("$BUILD" assemble --manifest "$MAN_E" epsilon zeta 2>/dev/null)"
says "skipped: 1 e2e test(s) in the given list, $NOT_RECORDED" "$outEZ" \
  || fail "(ii-e) a marked unrecorded test must take the headline over an unmarked one; got: '$outEZ'"
if says "$NOT_ELIGIBLE" "$outEZ"; then
  fail "(ii-e) epsilon IS reel-eligible, so 'none is reel-eligible' must not appear; got: '$outEZ'"
fi
says "have no [reel] marker in" "$outEZ" || fail "(ii-e) the unmarked id must still be named separately; got: '$outEZ'"
says "zeta" "$outEZ" || fail "(ii-e) skip must NAME zeta in its parenthetical; got: '$outEZ'"
echo "PASS (ii-e): a marked e2e test with no cast is reported as unrecorded (with the remedy), an unmarked one as unmarked, and a mix headlines the unrecorded one"

# --- (v) CAST PROVENANCE (issue #808) ------------------------------------------
# Selection is a `test -f` plus two STATIC facts about the test, so a cast an
# older revision left on disk satisfies every gate above. PR #805's discards
# close the routes that reach their own call sites; a FILTERED run and a kill
# inside `skip_unless!`'s preflight do not. These cases drive the gate that
# covers both — the harness's `provenance.json` sidecar, checked at assembly.
#
# Section (i) is already the accepting case: it publishes alpha and beta, whose
# committed sidecars record $FIXTURE_COMMIT with outcome `passed`. Everything
# below is a refusal, one mutated field at a time, on a private copy.
PROV_GATE="CAST PROVENANCE gate"

# (v-a) NO sidecar at all — a cast written before the sidecar existed, or a dump
# that died before writing it (which is why the harness writes it LAST).
REC="$(rec_copy no-sidecar)"
rm -f "$REC/alpha/provenance.json" "$REC/beta/provenance.json"
MANV="$TMP/prov-none.json"
ERRV="$TMP/prov-none.err"
outv="$(REEL_ADAPTER_RECORDINGS_DIR="$REC" "$BUILD" assemble --manifest "$MANV" alpha beta 2>"$ERRV")"
[[ ! -e "$MANV" ]] || fail "(v-a) a cast with NO provenance sidecar must not be published"
says "$PROV_GATE" "$outv" || fail "(v-a) the skip must name the provenance gate; got: '$outv'"
says "alpha" "$outv" || fail "(v-a) the skip must NAME the refused ids; got: '$outv'"
says "no provenance.json" "$(cat "$ERRV")" \
  || fail "(v-a) the per-id verdict must say the sidecar is absent; stderr: $(cat "$ERRV")"
if says "$NOTHING_CHANGED" "$outv"; then
  fail "(v-a) these tests DID change and ARE marked — claiming nothing changed is the issue #735 misattribution in a new bucket; got: '$outv'"
fi
if says "$NOT_ELIGIBLE" "$outv"; then
  fail "(v-a) both ids ARE [reel]-marked, so blaming the marker would send a reader to add one that changes nothing; got: '$outv'"
fi
echo "PASS (v-a): a cast with no provenance.json is refused, and the skip names the provenance gate (not the diff, not the marker)"

# (v-b) sidecar records a DIFFERENT commit — the residual route this issue is
# about: a filtered run never touched this test, so its older cast survived.
REC="$(rec_copy other-commit)"
prov_set "$REC" alpha '.commit = "ffffffff11111112222222"'
prov_set "$REC" beta  '.commit = "ffffffff11111112222222"'
MANV="$TMP/prov-commit.json"
ERRV="$TMP/prov-commit.err"
outv="$(REEL_ADAPTER_RECORDINGS_DIR="$REC" "$BUILD" assemble --manifest "$MANV" alpha beta 2>"$ERRV")"
[[ ! -e "$MANV" ]] || fail "(v-b) a cast from a different revision must not be published"
says "DIFFERENT revision" "$(cat "$ERRV")" \
  || fail "(v-b) the per-id verdict must say the revision differs; stderr: $(cat "$ERRV")"
says "ffffffff11111112222222" "$(cat "$ERRV")" \
  || fail "(v-b) the verdict must name the commit it found; stderr: $(cat "$ERRV")"
says "$PROV_GATE" "$outv" || fail "(v-b) the skip must name the provenance gate; got: '$outv'"
echo "PASS (v-b): a cast recorded at another commit is refused, naming both shas"

# (v-c) outcome is not `passed` — a FAILURE dump, which is what `Drop` writes
# whenever a test panics, so it is the commonest artifact on disk by far.
REC="$(rec_copy failed-run)"
prov_set "$REC" alpha '.outcome = "failed"'
prov_set "$REC" beta  '.outcome = "failed"'
MANV="$TMP/prov-failed.json"
ERRV="$TMP/prov-failed.err"
outv="$(REEL_ADAPTER_RECORDINGS_DIR="$REC" "$BUILD" assemble --manifest "$MANV" alpha beta 2>"$ERRV")"
[[ ! -e "$MANV" ]] || fail "(v-c) a failure dump must not be published as a clip"
says "not 'passed'" "$(cat "$ERRV")" \
  || fail "(v-c) the per-id verdict must name the outcome; stderr: $(cat "$ERRV")"
says "$PROV_GATE" "$outv" || fail "(v-c) the skip must name the provenance gate; got: '$outv'"
echo "PASS (v-c): a recording whose run did not pass is refused"

# (v-d) unknown schema — refuse rather than guess what the fields mean.
REC="$(rec_copy bad-schema)"
prov_set "$REC" alpha '.schema = 99'
prov_set "$REC" beta  '.schema = 99'
MANV="$TMP/prov-schema.json"
ERRV="$TMP/prov-schema.err"
outv="$(REEL_ADAPTER_RECORDINGS_DIR="$REC" "$BUILD" assemble --manifest "$MANV" alpha beta 2>"$ERRV")"
[[ ! -e "$MANV" ]] || fail "(v-d) an unknown provenance schema must not be published"
says "only understands" "$(cat "$ERRV")" \
  || fail "(v-d) the per-id verdict must say the schema is unsupported; stderr: $(cat "$ERRV")"
echo "PASS (v-d): an unsupported provenance schema is refused"

# (v-e) corrupt JSON, and (v-e2) a required field removed. Both are "this
# sidecar cannot vouch for anything", whether the cause is a truncated write or
# a hand-edit.
REC="$(rec_copy corrupt)"
printf 'not json at all {{{' > "$REC/alpha/provenance.json"
printf 'not json at all {{{' > "$REC/beta/provenance.json"
MANV="$TMP/prov-corrupt.json"
ERRV="$TMP/prov-corrupt.err"
outv="$(REEL_ADAPTER_RECORDINGS_DIR="$REC" "$BUILD" assemble --manifest "$MANV" alpha beta 2>"$ERRV")"
[[ ! -e "$MANV" ]] || fail "(v-e) a corrupt provenance sidecar must not be published"
says "not readable JSON" "$(cat "$ERRV")" \
  || fail "(v-e) the per-id verdict must say the sidecar is unreadable; stderr: $(cat "$ERRV")"
echo "PASS (v-e): a corrupt provenance sidecar is refused"

REC="$(rec_copy missing-field)"
prov_set "$REC" alpha 'del(.commit)'
prov_set "$REC" beta  'del(.run_id)'
MANV="$TMP/prov-missing.json"
ERRV="$TMP/prov-missing.err"
outv="$(REEL_ADAPTER_RECORDINGS_DIR="$REC" "$BUILD" assemble --manifest "$MANV" alpha beta 2>"$ERRV")"
[[ ! -e "$MANV" ]] || fail "(v-e2) a provenance sidecar missing a required field must not be published"
says "missing required field(s): commit" "$(cat "$ERRV")" \
  || fail "(v-e2) the verdict must name the missing field; stderr: $(cat "$ERRV")"
says "missing required field(s): run_id" "$(cat "$ERRV")" \
  || fail "(v-e2) run_id is required for the audit trail even though it gates nothing; stderr: $(cat "$ERRV")"
echo "PASS (v-e2): a provenance sidecar missing a required field is refused, naming the field"

# (v-f) ONE stale clip must not take its siblings down with it. The gate is
# per-clip: beta still publishes, and the omission of alpha is stated where the
# manifest is announced rather than only mid-loop.
REC="$(rec_copy one-stale)"
rm -f "$REC/alpha/provenance.json"
MANV="$TMP/prov-partial.json"
ERRV="$TMP/prov-partial.err"
REEL_ADAPTER_RECORDINGS_DIR="$REC" "$BUILD" assemble --manifest "$MANV" alpha beta 2>"$ERRV" >/dev/null
[[ -s "$MANV" ]] || fail "(v-f) beta's provenance is valid, so a manifest must still be written"
lenv="$(jq 'length' "$MANV")"
[[ "$lenv" -eq 1 ]] || fail "(v-f) expected 1 entry (beta only), got $lenv"
if jq -e '[.[].clip] | any(. | test("alpha"))' "$MANV" >/dev/null; then
  fail "(v-f) the stale 'alpha' clip leaked into the manifest"
fi
says "WARNING" "$(cat "$ERRV")" || fail "(v-f) the omission must be warned about; stderr: $(cat "$ERRV")"
says "alpha" "$(cat "$ERRV")" || fail "(v-f) the warning must name the omitted clip; stderr: $(cat "$ERRV")"
echo "PASS (v-f): the gate is per-clip — one stale sidecar drops one clip, not the reel"

# (v-g) a DIRTY tree publishes with a warning. It must not refuse: recording
# from a dirty tree is the ordinary dogfood case. But `commit` then covers more
# than one working state, so the fact has to reach the operator.
REC="$(rec_copy dirty)"
prov_set "$REC" alpha '.dirty = true | .build_id = "0.39.2-gaaaaaaabbbbbbbccccccc-dirty"'
MANV="$TMP/prov-dirty.json"
ERRV="$TMP/prov-dirty.err"
REEL_ADAPTER_RECORDINGS_DIR="$REC" "$BUILD" assemble --manifest "$MANV" alpha beta 2>"$ERRV" >/dev/null
lenv="$(jq 'length' "$MANV")"
[[ "$lenv" -eq 2 ]] || fail "(v-g) a dirty-tree recording must still publish (got $lenv entries)"
says "DIRTY tree" "$(cat "$ERRV")" \
  || fail "(v-g) a dirty-tree clip must be warned about, since its commit no longer identifies the code; stderr: $(cat "$ERRV")"
says "TREE WAS DIRTY" "$(cat "$ERRV")" \
  || fail "(v-g) the per-clip provenance line must carry the dirty marker; stderr: $(cat "$ERRV")"
echo "PASS (v-g): a dirty-tree recording publishes but is warned about"

# (v-h) clips from two DIFFERENT runs at the same commit publish with a note.
# This is the case that must not become a refusal: CLAUDE.md rule 5 asks people
# to run filtered tests, so a reel assembled from several filtered runs is the
# expected shape, not an anomaly.
REC="$(rec_copy two-runs)"
prov_set "$REC" alpha '.run_id = "99999999-8888-7777-6666-555555555555"'
MANV="$TMP/prov-tworuns.json"
ERRV="$TMP/prov-tworuns.err"
REEL_ADAPTER_RECORDINGS_DIR="$REC" "$BUILD" assemble --manifest "$MANV" alpha beta 2>"$ERRV" >/dev/null
lenv="$(jq 'length' "$MANV")"
[[ "$lenv" -eq 2 ]] || fail "(v-h) clips from separate runs at one commit must publish (got $lenv entries)"
says "2 distinct recording runs" "$(cat "$ERRV")" \
  || fail "(v-h) differing run ids must be NOTED (never refused); stderr: $(cat "$ERRV")"
echo "PASS (v-h): clips from two runs at one commit publish, with the run count noted"

# (v-i) FAIL CLOSED with nothing to check against: no override, and a cwd where
# `git rev-parse HEAD` cannot answer. The only alternatives are to publish
# unchecked or to stop, and it must stop — a warning here would publish exactly
# the casts the gate exists to refuse. GIT_CEILING_DIRECTORIES bounds git's
# upward walk at $TMP and GIT_DIR is cleared, so an ambient git environment
# cannot smuggle a real repository in (CLAUDE.md rule 5's #834 lesson).
mkdir -p "$TMP/not-a-repo"
MANV="$TMP/prov-nogit.json"
ERRV="$TMP/prov-nogit.err"
rcv=0
(cd "$TMP/not-a-repo" && env -u REEL_ADAPTER_EXPECT_COMMIT -u GIT_DIR \
    GIT_CEILING_DIRECTORIES="$TMP" \
    "$BUILD" assemble --manifest "$MANV" alpha beta) >/dev/null 2>"$ERRV" || rcv=$?
[[ "$rcv" -ne 0 ]] || fail "(v-i) with no commit to check against the adapter must FAIL, not publish unchecked"
[[ ! -e "$MANV" ]] || fail "(v-i) no manifest may be written when provenance cannot be checked at all"
says "cannot determine the commit" "$(cat "$ERRV")" \
  || fail "(v-i) the failure must say what it could not determine; stderr: $(cat "$ERRV")"
echo "PASS (v-i): unable to resolve a commit to check against, the adapter dies instead of publishing unchecked"

# (v-j) with the override unset but git ABLE to answer, the expected commit comes
# from `git rev-parse HEAD` — proving the default path is live rather than only
# ever exercised through the test's own pin. The fixture records a synthetic
# commit, so the real HEAD refuses it, which is the observable half.
if command -v git >/dev/null 2>&1 && git -C "$HERE" rev-parse HEAD >/dev/null 2>&1; then
  MANV="$TMP/prov-realhead.json"
  ERRV="$TMP/prov-realhead.err"
  outv="$( (cd "$HERE" && env -u REEL_ADAPTER_EXPECT_COMMIT "$BUILD" assemble --manifest "$MANV" alpha beta) 2>"$ERRV" )"
  [[ ! -e "$MANV" ]] || fail "(v-j) the fixture's synthetic commit is not this checkout's HEAD, so nothing may publish"
  says "DIFFERENT revision" "$(cat "$ERRV")" \
    || fail "(v-j) with no override the gate must compare against the real HEAD; stderr: $(cat "$ERRV")"
  says "$PROV_GATE" "$outv" || fail "(v-j) the skip must name the provenance gate; got: '$outv'"
  echo "PASS (v-j): with no override the expected commit is resolved from git HEAD"
else
  echo "SKIP (v-j): git cannot resolve HEAD here"
fi

# --- git-backed sections: the `reel` path (selection + assembly + engine) -------
if ! command -v git >/dev/null 2>&1; then
  echo "SKIP (iii)/(iv): git not available"
  echo "ADAPTER TEST PASS"
  exit 0
fi

# A throwaway repo built entirely inside $TMP, with the ambient git configuration
# switched off so it can neither read nor write the checkout this test runs in
# (CLAUDE.md rule 5's discipline for the xtask real-git tests). No network: the
# adapter only fetches when MAIN_REF names an `origin/*` ref, and this passes a
# local branch. `mainline` stands in for main; each section cuts its own branch
# off it and changes exactly one source file.
export GIT_CONFIG_GLOBAL="$TMP/no-such-gitconfig"
export GIT_CONFIG_SYSTEM="$TMP/no-such-gitconfig"
export GIT_CONFIG_NOSYSTEM=1
export HOME="$TMP/home"
export XDG_CONFIG_HOME="$TMP/home/.config"
mkdir -p "$HOME"

REPO="$TMP/repo"
mkdir -p "$REPO/tests"
git -C "$REPO" init -q >/dev/null 2>&1
git -C "$REPO" config user.email "adapter-test@example.invalid"
git -C "$REPO" config user.name "adapter test"
git -C "$REPO" config commit.gpgsign false
# The two source files the fixture recordings name in their **Source:** lines.
echo "// base" > "$REPO/tests/e2e_mouse_button.rs"   # alpha + beta (both [reel]-marked)
echo "// base" > "$REPO/tests/e2e_mouse_delta.rs"    # delta (cast, but NOT marked)
echo "// base" > "$REPO/tests/e2e_mouse_epsilon.rs"  # epsilon (marked, NO cast)
echo "// base" > "$REPO/tests/render_mouse_button.rs" # gamma (L1, no cast)
git -C "$REPO" add -A
git -C "$REPO" commit -qm "base"
git -C "$REPO" branch -q mainline
export REEL_ADAPTER_MAIN_REF="mainline"

# A stub engine, so (iv) can prove the engine IS invoked without agg/ffmpeg.
ENGINE_LOG="$TMP/engine.log"
cat > "$TMP/engine.sh" <<'STUB'
#!/usr/bin/env bash
printf 'ENGINE CALLED: %s\n' "$*" >> "$ENGINE_LOG"
STUB
chmod +x "$TMP/engine.sh"
export REEL_ADAPTER_ENGINE="$TMP/engine.sh"
export ENGINE_LOG

# --- (iii) branch changed ONLY the unmarked test's source -> eligibility skip ---
# This is issue #735 exactly: e2e tests DID change, all three of the other gates
# passed, and only the opt-in marker was absent. The skip stays clean (no
# manifest, no engine, exit 0) but must name the real cause.
git -C "$REPO" checkout -q -b feature-unmarked mainline
echo "// changed" >> "$REPO/tests/e2e_mouse_delta.rs"
git -C "$REPO" commit -qam "touch the unmarked test"

MAN5="$TMP/reel-unmarked.json"
ERR5="$TMP/reel-unmarked.err"
rc=0
out5="$(cd "$REPO" && "$BUILD" reel --manifest "$MAN5" 2>"$ERR5")" || rc=$?
[[ "$rc" -eq 0 ]] || fail "(iii) the skip must stay CLEAN (exit 0), got exit $rc; stderr: $(cat "$ERR5")"
[[ ! -e "$MAN5" ]] || fail "(iii) manifest must NOT be written on a clean skip"
[[ ! -e "$ENGINE_LOG" ]] || fail "(iii) engine must NOT be invoked on a clean skip"
says "$NOT_ELIGIBLE" "$out5" || fail "(iii) skip must state the reel-eligibility reason; got: '$out5'"
says "delta" "$out5" || fail "(iii) skip must NAME the changed-but-ineligible test; got: '$out5'"
if says "$NOTHING_CHANGED" "$out5"; then
  fail "(iii) e2e tests DID change — claiming otherwise is the issue #735 defect; got: '$out5'"
fi
# Selection must also explain itself per id, the way assembly already did — that
# is the half of #735 where an unmarked id was dropped by a bare `continue`.
says "has no [reel] marker" "$(cat "$ERR5")" \
  || fail "(iii) selection must emit the per-id exclusion diagnostic; stderr: $(cat "$ERR5")"
echo "PASS (iii): reel path names the missing [reel] marker (not 'no e2e tests changed') and still clean-skips"

# --- (iv) branch changed a MARKED test's source -> selects, assembles, engine ---
# The mirror image, so the near-miss reporting cannot start swallowing real work:
# alpha/beta are marked and their source changed, so a reel IS built. delta's own
# source is untouched here, so an unmarked-but-UNCHANGED test is not reported —
# only near-misses that would otherwise have been selected are.
git -C "$REPO" checkout -q -b feature-marked mainline
echo "// changed" >> "$REPO/tests/e2e_mouse_button.rs"
git -C "$REPO" commit -qam "touch the marked tests"

MAN6="$TMP/reel-marked.json"
out6="$(cd "$REPO" && "$BUILD" reel --manifest "$MAN6" --title "stub title" 2>/dev/null)"
# --title is pinned so title composition never runs: it shells out to `gh`, and
# this test must stay offline.
[[ -s "$MAN6" ]] || fail "(iv) manifest must be written when a [reel]-marked test changed"
len6="$(jq 'length' "$MAN6")"
[[ "$len6" -eq 2 ]] || fail "(iv) expected 2 entries (beta, alpha), got $len6"
[[ -s "$ENGINE_LOG" ]] || fail "(iv) engine must be invoked when a reel is built"
grep -qF "ENGINE CALLED" "$ENGINE_LOG" || fail "(iv) engine stub not called: $(cat "$ENGINE_LOG")"
if says "$NOT_ELIGIBLE" "$out6"; then
  fail "(iv) delta's source did not change, so it is out of scope — not a near-miss to report; got: '$out6'"
fi
echo "PASS (iv): reel path selects the marked tests, writes the manifest, and invokes the engine"

# --- (vi) changed e2e tests with NO recording (issue #1683) ---------------------
# The issue's repro: the branch changes an e2e test whose recording dir holds
# only test.md. The skip stays clean, but must say the test has no recording,
# count it, name the remedy and print the per-id diagnostic — not claim that no
# e2e tests changed.
git -C "$REPO" checkout -q -b feature-unrecorded mainline
echo "// changed" >> "$REPO/tests/e2e_mouse_epsilon.rs"
git -C "$REPO" commit -qam "touch the unrecorded test"

rm -f "$ENGINE_LOG"
MAN7="$TMP/reel-unrecorded.json"
ERR7="$TMP/reel-unrecorded.err"
rc=0
out7="$(cd "$REPO" && "$BUILD" reel --manifest "$MAN7" 2>"$ERR7")" || rc=$?
[[ "$rc" -eq 0 ]] || fail "(vi) the skip must stay CLEAN (exit 0), got exit $rc; stderr: $(cat "$ERR7")"
[[ ! -e "$MAN7" ]] || fail "(vi) manifest must NOT be written on a clean skip"
[[ ! -e "$ENGINE_LOG" ]] || fail "(vi) engine must NOT be invoked on a clean skip"
says "skipped: 1 e2e test(s) changed on this branch, $NOT_RECORDED" "$out7" \
  || fail "(vi) skip must count the changed tests that have no recording; got: '$out7'"
says "DOT_AGENT_DECK_RECORD=1" "$out7" || fail "(vi) skip must say how to record them; got: '$out7'"
says "epsilon" "$out7" || fail "(vi) skip must NAME the unrecorded test; got: '$out7'"
if says "$NOTHING_CHANGED" "$out7"; then
  fail "(vi) e2e tests DID change — claiming otherwise is the issue #1683 defect; got: '$out7'"
fi
says "excluding 'epsilon' (e2e test with no full-stream.cast" "$(cat "$ERR7")" \
  || fail "(vi) selection must print the per-id diagnostic; stderr: $(cat "$ERR7")"

# An L1-only change still reads "no e2e tests changed": gamma has no cast by
# construction, and that claim is true of it.
git -C "$REPO" checkout -q -b feature-l1 mainline
echo "// changed" >> "$REPO/tests/render_mouse_button.rs"
git -C "$REPO" commit -qam "touch the L1 test"
out8="$(cd "$REPO" && "$BUILD" reel --manifest "$TMP/reel-l1.json" 2>"$TMP/reel-l1.err")"
says "$NOTHING_CHANGED" "$out8" || fail "(vi) an L1-only change must still say no e2e tests changed; got: '$out8'"
if says "gamma" "$(cat "$TMP/reel-l1.err")"; then
  fail "(vi) an L1 test with no cast is not a near-miss and must stay silent; stderr: $(cat "$TMP/reel-l1.err")"
fi

# A reel built from the OTHER changed tests still says the unrecorded one is
# missing from it, so a partial reel cannot pass for a complete one.
git -C "$REPO" checkout -q -b feature-partial mainline
echo "// changed" >> "$REPO/tests/e2e_mouse_button.rs"
echo "// changed" >> "$REPO/tests/e2e_mouse_epsilon.rs"
git -C "$REPO" commit -qam "touch a recorded and an unrecorded test"
rm -f "$ENGINE_LOG"
MAN9="$TMP/reel-partial.json"
ERR9="$TMP/reel-partial.err"
(cd "$REPO" && "$BUILD" reel --manifest "$MAN9" --title "stub title" >/dev/null 2>"$ERR9")
[[ "$(jq 'length' "$MAN9")" -eq 2 ]] || fail "(vi) the recorded tests must still be published; got: $(cat "$MAN9")"
[[ -s "$ENGINE_LOG" ]] || fail "(vi) engine must be invoked when a reel is built"
says "have NO recording and are NOT in this reel: epsilon" "$(cat "$ERR9")" \
  || fail "(vi) a partial reel must warn about the unrecorded test; stderr: $(cat "$ERR9")"

# And when the recorded ones then fail provenance, the skip after assembly still
# counts the unrecorded one rather than reporting only the stale ids.
rm -f "$ENGINE_LOG"
MAN10="$TMP/reel-stale-unrecorded.json"
out10="$(cd "$REPO" && REEL_ADAPTER_EXPECT_COMMIT="ddddddd" "$BUILD" reel --manifest "$MAN10" 2>/dev/null)"
[[ ! -e "$MAN10" ]] || fail "(vi) no clip passes provenance, so no manifest may be written"
says "none has a recording whose provenance checks out" "$out10" \
  || fail "(vi) the stale clips must still headline the skip; got: '$out10'"
says "further e2e test(s) have no recording at all" "$out10" \
  || fail "(vi) the skip must still count the unrecorded test; got: '$out10'"
says "epsilon" "$out10" || fail "(vi) the skip must name the unrecorded test; got: '$out10'"
echo "PASS (vi): changed e2e tests with no recording are counted, named and given the DOT_AGENT_DECK_RECORD=1 remedy"

# --- (vii) the title's <short desc> when there is no prds/ file (issue #1821) ---
# Since issue #1591 a PRD lives in its issue and has no prds/<n>-*.md, so the
# description falls back to the issue title. A stub `gh` on PATH stands in for
# GitHub (no network); `build.sh title` prints the composed title and nothing
# else. The branch name is what supplies the PRD number.
GH_BIN="$TMP/gh-bin"
GH_LOG="$TMP/gh.log"
mkdir -p "$GH_BIN"
cat > "$GH_BIN/gh" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$GH_LOG"
if [[ "$1 $2" == "issue view" && -z "${GH_STUB_FAIL:-}" ]]; then
  printf '%s\n' "$GH_STUB_TITLE"
  exit 0
fi
exit 1
STUB
chmod +x "$GH_BIN/gh"
export GH_LOG
title_on() { (cd "$REPO" && PATH="$GH_BIN:$PATH" "$BUILD" title); }

git -C "$REPO" checkout -q -b prd-4242-title mainline

# (vii-a) no file: the issue title, minus its 'PRD: ' prefix.
rm -f "$GH_LOG"
t="$(GH_STUB_TITLE='PRD: Stubbed issue title' title_on)"
[[ "$t" == "repo · PRD #4242 — Stubbed issue title" ]] \
  || fail "(vii-a) with no prds/ file the description must come from the issue title; got: '$t'"
says "issue view 4242" "$(cat "$GH_LOG")" || fail "(vii-a) the issue looked up must be the branch's PRD; gh calls: $(cat "$GH_LOG")"

# (vii-b) a 'PRD #<n>:' prefix is stripped too, and a title is text, never code:
# control characters go, and shell syntax is printed rather than run.
t="$(GH_STUB_TITLE=$'PRD #4242: Run $(touch '"$TMP"$'/pwned) `id`\tnow\r' title_on)"
[[ "$t" == "repo · PRD #4242 — Run \$(touch $TMP/pwned) \`id\`now" ]] \
  || fail "(vii-b) the title must be stripped of its prefix and control characters and kept verbatim otherwise; got: '$t'"
[[ ! -e "$TMP/pwned" ]] || fail "(vii-b) the issue title was evaluated as shell"

# (vii-c) gh failing (offline, no auth, no such issue) still yields a title.
t="$(GH_STUB_FAIL=1 title_on)"
[[ "$t" == "repo · PRD #4242 — demo reel" ]] || fail "(vii-c) a failed lookup must fall back to 'demo reel'; got: '$t'"

# (vii-d) a prds/ file still wins, and GitHub is not asked.
mkdir -p "$REPO/prds"
printf '# PRD #4242: From the file\n\nbody\n' > "$REPO/prds/4242-title.md"
rm -f "$GH_LOG"
t="$(GH_STUB_TITLE='PRD: From the issue' title_on)"
rm -rf "$REPO/prds"
[[ "$t" == "repo · PRD #4242 — From the file" ]] || fail "(vii-d) a prds/ file must still win over the issue title; got: '$t'"
if says "issue view" "$(cat "$GH_LOG" 2>/dev/null)"; then
  fail "(vii-d) with a prds/ file the issue must not be looked up; gh calls: $(cat "$GH_LOG")"
fi
echo "PASS (vii): the reel description falls back to the PRD issue title (prefix and control characters stripped, never evaluated), then to 'demo reel'; a prds/ file still wins"

echo "ADAPTER TEST PASS"
