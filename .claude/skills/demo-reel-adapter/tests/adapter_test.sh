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
#   (iii) the `reel` path on a branch that DID change an e2e test whose catalog
#         entry is unmarked skips with that eligibility message too — never the
#         "no e2e tests changed" one, which is the issue #735 defect;
#   (iv)  the `reel` path on a branch that changed a MARKED test still selects,
#         assembles and invokes the engine (a stub), and reports no near-miss.
#
# WHY (iii)/(iv) shell out to git: the defect in #735 lives in the SELECTION half,
# which exists to run `git diff`, so covering it anywhere else covers something
# else. They follow the same discipline CLAUDE.md rule 5 sets for the `xtask`
# real-git tests — every repository is built inside the test's own `mktemp -d`
# with the ambient git configuration switched off, so nothing can read or write
# the checkout this runs in, and there is no network and no sleep. They SKIP
# (without failing) where git is unavailable.
#
# The fixture under tests/fixtures/recordings/ has FOUR dirs:
#   * alpha, beta — e2e dirs WITH a cast whose catalog entry carries ` [reel]`;
#   * gamma       — an L1 dir (test.md but NO cast), so excluded by construction;
#   * delta       — an e2e dir WITH a cast but whose catalog entry is UNMARKED,
#                   so excluded by the opt-in reel-eligibility marker.
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

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
fail() { echo "ADAPTER TEST FAIL: $*" >&2; exit 1; }
# `says <needle> <haystack>` — a predicate, so assertions read as `if says …` /
# `says … || fail`, never as `grep … && fail`, whose exit status is a `set -e`
# trap in its own right.
says() { printf '%s\n' "$2" | grep -qF -- "$1"; }

# The two skip messages must stay DISTINCT — collapsing them back into one is the
# issue #735 regression, so every skip assertion below checks both directions.
NOTHING_CHANGED="skipped: no e2e tests changed on this branch"
NOT_ELIGIBLE="but none is reel-eligible"

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

echo "ADAPTER TEST PASS"
