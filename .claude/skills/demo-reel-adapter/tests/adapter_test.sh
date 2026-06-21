#!/usr/bin/env bash
#
# adapter_test.sh — re-runnable acceptance for the demo-reel ADAPTER (PRD #180 M2.1).
#
# PURE shell: NO git, NO agg/ffmpeg, NO network — so this MAY run in CI (unlike
# the engine smoke and the reel step itself, which are local-only). It drives the
# adapter's deterministic concern (b) — `build.sh assemble` — against a tiny
# fixture and asserts:
#
#   (i)  given a list of IDs, the emitted manifest has the right
#        titles/descriptions/clip paths IN CATALOG ORDER and EXCLUDES the
#        cast-less L1 entry;
#   (ii) given an empty in-scope list, it CLEAN-SKIPS — no manifest, exit 0, and
#        the skip message.
#
# The fixture under tests/fixtures/recordings/ has two e2e dirs (alpha, beta;
# each has a full-stream.cast) and one L1 dir (gamma; test.md but NO cast). The
# CATALOG.md fixture orders them 001=beta, 002=alpha, 003=gamma, so feeding
# `alpha beta gamma` and getting back `[beta, alpha]` proves both ordering and
# the L1 exclusion at once.
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

# --- (i) assemble alpha beta gamma -> [beta(001), alpha(002)]; gamma excluded --
MAN="$TMP/manifest.json"
"$BUILD" assemble --manifest "$MAN" alpha beta gamma >/dev/null

[[ -s "$MAN" ]] || fail "(i) manifest not written"

len="$(jq 'length' "$MAN")"
[[ "$len" -eq 2 ]] || fail "(i) expected 2 entries, got $len (L1 gamma must be excluded)"

t0="$(jq -r '.[0].title' "$MAN")"
t1="$(jq -r '.[1].title' "$MAN")"
[[ "$t0" == "mouse/button/001 — Beta renders its label." ]]  || fail "(i) entry 0 title: '$t0'"
[[ "$t1" == "mouse/button/002 — Alpha renders its label." ]] || fail "(i) entry 1 title: '$t1'"

d0="$(jq -r '.[0].description' "$MAN")"
d1="$(jq -r '.[1].description' "$MAN")"
[[ "$d0" == "Beta scenario: start the app and confirm the beta widget renders its label." ]]   || fail "(i) entry 0 desc: '$d0'"
[[ "$d1" == "Alpha scenario: start the app and confirm the alpha widget renders its label." ]] || fail "(i) entry 1 desc: '$d1'"

c0="$(jq -r '.[0].clip' "$MAN")"
c1="$(jq -r '.[1].clip' "$MAN")"
[[ "$c0" == "$FIX/recordings/beta/full-stream.cast" ]]  || fail "(i) entry 0 clip: '$c0'"
[[ "$c1" == "$FIX/recordings/alpha/full-stream.cast" ]] || fail "(i) entry 1 clip: '$c1'"

if jq -e '[.[].clip] | any(. | test("gamma"))' "$MAN" >/dev/null; then
  fail "(i) cast-less L1 'gamma' leaked into the manifest"
fi
echo "PASS (i): 2 entries in catalog order (beta, alpha), L1 gamma excluded, fields correct"

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

echo "ADAPTER TEST PASS"
