#!/usr/bin/env bash
#
# adapter_test.sh — re-runnable acceptance for the demo-reel ADAPTER (PRD #180 M2.1).
#
# PURE shell: NO git, NO agg/ffmpeg, NO network — so this MAY run in CI (unlike
# the engine smoke and the reel step itself, which are local-only). It drives the
# adapter's deterministic concern (b) — `build.sh assemble` — against a tiny
# fixture and asserts:
#
#   (i)   given a list of IDs, the emitted manifest has the right
#         titles/descriptions/clip paths IN CATALOG ORDER, EXCLUDES the cast-less
#         L1 entry, and EXCLUDES a cast-bearing entry that is NOT reel-marked;
#   (ii)  given an empty in-scope list, it CLEAN-SKIPS — no manifest, exit 0, and
#         the skip message;
#   (ii-b/c) an L1-only list AND an unmarked-cast-only list each CLEAN-SKIP too.
#
# The fixture under tests/fixtures/recordings/ has FOUR dirs:
#   * alpha, beta — e2e dirs WITH a cast whose catalog entry carries ` [reel]`;
#   * gamma       — an L1 dir (test.md but NO cast), so excluded by construction;
#   * delta       — an e2e dir WITH a cast but whose catalog entry is UNMARKED,
#                   so excluded by the opt-in reel-eligibility marker.
# The CATALOG.md fixture orders them 001=beta, 002=alpha, 003=gamma, 004=delta, so
# feeding `alpha beta gamma delta` and getting back exactly `[beta, alpha]` proves
# ordering, the L1 exclusion, AND the unmarked-cast exclusion at once.
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

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
fail() { echo "ADAPTER TEST FAIL: $*" >&2; exit 1; }

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
printf '%s\n' "$out" | grep -qF "skipped: no e2e tests changed on this branch" \
  || fail "(ii) missing skip message; got: '$out'"
echo "PASS (ii): empty list clean-skips (no manifest, exit 0, skip message)"

# --- (ii-b) a list of only L1 (cast-less) ids also clean-skips ------------------
MAN3="$TMP/skip2.json"
out3="$("$BUILD" assemble --manifest "$MAN3" gamma)"
[[ ! -e "$MAN3" ]] || fail "(ii-b) manifest must NOT be written when only L1 ids are given"
printf '%s\n' "$out3" | grep -qF "skipped: no e2e tests changed on this branch" \
  || fail "(ii-b) missing skip message for L1-only list; got: '$out3'"
echo "PASS (ii-b): L1-only list clean-skips"

# --- (ii-c) a list of only UNMARKED cast-bearing ids also clean-skips -----------
# delta has a full-stream.cast but its catalog entry lacks the ` [reel]` marker,
# so an all-unmarked list resolves to zero reel-eligible clips and clean-skips.
MAN4="$TMP/skip3.json"
out4="$("$BUILD" assemble --manifest "$MAN4" delta)"
[[ ! -e "$MAN4" ]] || fail "(ii-c) manifest must NOT be written when only unmarked cast ids are given"
printf '%s\n' "$out4" | grep -qF "skipped: no e2e tests changed on this branch" \
  || fail "(ii-c) missing skip message for unmarked-cast-only list; got: '$out4'"
echo "PASS (ii-c): unmarked-cast-only list clean-skips"

echo "ADAPTER TEST PASS"
