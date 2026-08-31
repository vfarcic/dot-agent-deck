#!/usr/bin/env bash
#
# Phase 2 of /verify-pr: run every automated gate this repo has, in one pass,
# without stopping at the first failure — a review needs the whole picture, not
# the first thing that broke.
#
# Usage: checks.sh [--dir <worktree>] [--no-e2e] [--only <a,b,c>] [--filter <expr>]
#
#   --dir     worktree to run in (default: current directory)
#   --no-e2e  skip lane 1 of the e2e tier (CLAUDE.md rule 5). Lane 1 needs no
#             credentials, but it still spawns real binaries and PTYs, so it
#             costs minutes
#   --only    comma-separated subset of: fmt,clippy,build,test-fast,
#             linkage-check,windows-cross,audit,e2e
#   --filter  test-name filter passed to the test steps, for rule 6's
#             rerun-one-test loop (e.g. --filter lifecycle_001)
#
# RUN THIS IN THE BACKGROUND. The full suite takes far longer than a foreground
# tool call allows. Progress is appended to `<out>/summary.tsv` as each step
# finishes and `<out>/DONE` appears at the end, so the caller can poll instead
# of blocking.
#
# The `KEY=value` output grammar is shared with `scan.sh` / `setup.sh` and is
# documented in `stream.sh` (issue #521).
#
# Exit code is 0 when every executed step passed, 1 otherwise.

set -uo pipefail

stream_lib="$(dirname "${BASH_SOURCE[0]}")/stream.sh"
# shellcheck source=stream.sh
if ! . "$stream_lib"; then
  echo "verify-pr: cannot source ${stream_lib}; the skill directory is incomplete" >&2
  exit 1
fi

dir="."
run_e2e=true
only=""
filter=""

while [ $# -gt 0 ]; do
  case "$1" in
    --dir)
      dir="${2:-}"
      shift 2
      ;;
    --no-e2e)
      run_e2e=false
      shift
      ;;
    --only)
      only="${2:-}"
      shift 2
      ;;
    --filter)
      filter="${2:-}"
      shift 2
      ;;
    *)
      emit ERROR true
      emit MESSAGE "Unknown argument '$1'"
      exit 1
      ;;
  esac
done

if [ ! -d "$dir" ]; then
  emit ERROR true
  emit MESSAGE "No such directory: ${dir}"
  exit 1
fi
dir=$(cd "$dir" && pwd)

if [ ! -f "${dir}/Cargo.toml" ]; then
  emit ERROR true
  emit MESSAGE "${dir} is not a Rust workspace root (no Cargo.toml)"
  exit 1
fi

out="${dir}/target/verify-pr"
logs="${out}/logs"
mkdir -p "$logs"
summary="${out}/summary.tsv"
rm -f "${out}/DONE"
printf 'step\tstatus\tseconds\tlog\tnote\n' >"$summary"

wanted() {
  [ -z "$only" ] && return 0
  case ",${only}," in *",$1,"*) return 0 ;; *) return 1 ;; esac
}

overall=0

record() { # <step> <status> <seconds> <log> <note>
  printf '%s\t%s\t%s\t%s\t%s\n' "$1" "$2" "$3" "$4" "${5:-}" >>"$summary"
  printf '[%s] %s (%ss) %s\n' "$2" "$1" "$3" "${5:-}"
  case "$2" in FAIL) overall=1 ;; esac
}

skip() { record "$1" SKIPPED 0 - "$2"; }

run_step() { # <step> <command-string> [note-on-pass]
  local name="$1" cmd="$2" note="${3:-}"
  local log="${logs}/${name}.log"
  local start=$SECONDS
  ( cd "$dir" && eval "$cmd" ) >"$log" 2>&1
  local rc=$?
  local secs=$((SECONDS - start))
  if [ $rc -eq 0 ]; then
    record "$name" PASS "$secs" "$log" "$note"
  else
    record "$name" FAIL "$secs" "$log" "exit ${rc}; ${note}"
  fi
  return $rc
}

# --- Environment facts the report has to state ----------------------------

{
  emit DIR "${dir}"
  emit HEAD "$(git -C "$dir" rev-parse --short HEAD 2>/dev/null || echo unknown)"
  emit IN_DEVBOX "${DEVBOX_SHELL_ENABLED:-0}"
  # `head -1` on every one of these: `cargo nextest --version` prints three
  # lines, and the reader wants one. `emit` would fold the rest onto the same
  # line rather than let them forge records, but "first line of the version
  # banner" is what this field means.
  emit RUSTC "$(rustc --version 2>/dev/null | head -1 || echo missing)"
  emit CARGO "$(cargo --version 2>/dev/null | head -1 || echo missing)"
  emit NEXTEST "$(cargo nextest --version 2>/dev/null | head -1 || echo missing)"
  emit CARGO_AUDIT "$(cargo audit --version 2>/dev/null | head -1 || echo missing)"
  # Upper-cased so the key is a record key by the grammar in `stream.sh`;
  # `AGENT_claude=present` looked like one without being one. Via `tr` rather
  # than `${cli^^}`: this script has no other bash-4 construct, and a reviewer
  # on macOS gets /bin/bash 3.2.
  for cli in claude opencode codex pi; do
    key="AGENT_$(printf '%s' "$cli" | tr '[:lower:]' '[:upper:]')"
    if command -v "$cli" >/dev/null 2>&1; then
      emit "$key" present
    else
      emit "$key" MISSING
    fi
  done
  emit PLATFORM "$(uname -s)/$(uname -m)"
} | tee "${out}/env.txt"
echo

if ! command -v cargo >/dev/null 2>&1; then
  emit ERROR true
  emit MESSAGE "cargo is not on PATH"
  exit 1
fi

have_nextest=true
cargo nextest --version >/dev/null 2>&1 || have_nextest=false

# --- The gates, cheapest first -------------------------------------------

build_ok=true

# CI: `cargo fmt --check` (CLAUDE.md rule 2).
wanted fmt && { run_step fmt "cargo fmt --check" || true; } || skip fmt "not in --only"

# CLAUDE.md rule 2, and the Linux `build` job in ci.yml matches this exactly.
# All four flags matter. Both of #407's: without `--all-targets` no test
# target is built at all, and without `--features e2e` every `tests/e2e_*.rs`
# compiles to an empty crate — so the bare `cargo clippy` this used to run
# reported clean over a build that never contained the e2e code under review.
# That is exactly the wrong answer for a PR review to give. And `--workspace`
# (issue #436): without it cargo selects the root package alone, so a PR
# touching only `xtask/*` — linkage-check, the docs generator, the `spec`
# macro — was reviewed against a lint that never read a line of it.
#
# `e2e-live` (issue #502) is the same hole reopened for the 24 credentialed
# files, which now open with
# `#![cfg(all(feature = "e2e", feature = "e2e-live"))]` and are empty crates
# under `--features e2e` alone. This gate is where a reviewer catches a break
# in them: the e2e step below runs LANE 1 only, so without this second feature
# a PR touching a real-agent test would be reviewed against no compilation of
# it at all.
#
# This is a type-check and lint, so naming `e2e-live` costs no credential and
# runs no live test. build-windows/build-macos still run bare `cargo clippy`
# (the L2 tier is Unix-only), so a Linux-only lint here is expected and
# correct.
wanted clippy && { run_step clippy "cargo clippy --workspace --all-targets --features e2e,e2e-live -- -D warnings" || true; } || skip clippy "not in --only"

if wanted build; then
  run_step build "cargo build --release" || build_ok=false
else
  skip build "not in --only"
fi

# Tests cannot say anything useful about a tree that does not compile, so they
# are reported BLOCKED rather than burning minutes to restate the build failure.
test_filter=""
[ -n "$filter" ] && test_filter=" ${filter}"

if wanted test-fast; then
  if [ "$build_ok" != true ]; then
    record test-fast BLOCKED 0 - "build failed; fix that first"
  elif [ "$have_nextest" != true ]; then
    record test-fast BLOCKED 0 - "cargo-nextest missing: enter 'devbox shell' or 'cargo install cargo-nextest --locked'"
  else
    # `--workspace` (issue #489) mirrors the `test-fast` alias and CI. Without
    # it cargo selects the root package alone and the `xtask/*` members' tests
    # never run, so a reviewer's gate would be narrower than the author's.
    run_step test-fast "cargo nextest run --workspace${test_filter}" "rule 5 fast tier" || true
  fi
else
  skip test-fast "not in --only"
fi

# Rule 7: catalog<->test linkage, function-name prefixes, no raw sleeps in
# e2e_*.rs, and the `/// Scenario:` doc comment on every #[spec] test.
if wanted linkage-check; then
  if [ "$build_ok" != true ]; then
    record linkage-check BLOCKED 0 - "build failed; fix that first"
  else
    run_step linkage-check "cargo xtask linkage-check" "rule 7" || true
  fi
else
  skip linkage-check "not in --only"
fi

# The only local proxy for CI's build-windows job. Type-check only, so it cannot
# replace that job's clippy run — but it catches the common #[cfg(unix)] break.
if wanted windows-cross; then
  if [ ! -x "${dir}/scripts/windows-cross-check.sh" ]; then
    skip windows-cross "scripts/windows-cross-check.sh not present/executable"
  elif [ ! -d "${WINDOWS_CROSS_CHECK_TOOLCHAIN:-$HOME/.rustup/toolchains/stable-x86_64-unknown-linux-gnu}" ]; then
    skip windows-cross "no rustup stable toolchain; see docs/develop/windows-cross-check.md"
  else
    run_step windows-cross "scripts/windows-cross-check.sh" "type-check only; CI still owns Windows clippy" || true
  fi
else
  skip windows-cross "not in --only"
fi

# CI's `security` job. Not auto-installed: that would mutate the reviewer's
# machine and take minutes mid-review.
if wanted audit; then
  if ! cargo audit --version >/dev/null 2>&1; then
    skip audit "cargo-audit missing: 'cargo install cargo-audit --locked' (CI runs it, so this gap is CI-covered)"
  else
    run_step audit "cargo audit" || true
  fi
else
  skip audit "not in --only"
fi

# --- e2e lane 1: the deterministic tier that exercises the product -------
#
# LANE 1 ONLY (issue #502, CLAUDE.md rule 5). This step runs `--features e2e`,
# i.e. the 47 `tests/e2e_*.rs` files that need no agent credential. The other
# 24 need one and are lane 2, which runs from `.github/workflows/e2e-live.yml`
# — per-merge on `main`, on `workflow_dispatch`, and on a PR when the
# `run-live-e2e` label is applied. This skill deliberately does NOT run lane 2:
# a reviewer's machine would need its own agent credentials, the run costs real
# tokens, and those tests are the flakiest signal in the repo. If the PR under
# review touches real-agent paths, label it `run-live-e2e` and read that
# workflow's run rather than running it here — docs/develop/e2e-lanes.md has
# the how, including why a GREEN lane-2 run can still mean almost nothing.

if [ "$run_e2e" != true ]; then
  skip e2e "--no-e2e was passed — SAY SO in the report; do not present the run as complete"
elif ! wanted e2e; then
  skip e2e "not in --only"
elif [ "$build_ok" != true ]; then
  record e2e BLOCKED 0 - "build failed; fix that first"
elif [ "$have_nextest" != true ]; then
  record e2e BLOCKED 0 - "cargo-nextest missing"
else
  # `--success-output=final` is NOT cosmetic. Tests that cannot run print
  # `SKIP: <reason>` and return NORMALLY, so nextest counts them as PASSED.
  # Without this flag nextest suppresses passing tests' output and those skips
  # are invisible — a green run that proved nothing. See `REQUIRE_REAL_E2E_ENV`
  # in tests/common/mod.rs. Lane 1 needs no credentials, so a SKIP here is a
  # missing local tool or an unmet host precondition rather than an absent key —
  # which makes it MORE interesting, not less: CI's `e2e-deterministic` job is
  # expected to run these for real.
  # `--workspace` (issue #489): same reason as the test-fast step above — keep
  # this in lockstep with the `test-e2e` alias in .cargo/config.toml. Note that
  # alias is lane 1 exactly; `test-e2e-live` is the superset and is not run
  # here.
  run_step e2e "cargo nextest run --workspace --features e2e --success-output=final${test_filter}" "rule 5 lane 1 (deterministic e2e)" || true

  e2e_log="${logs}/e2e.log"
  if [ -f "$e2e_log" ]; then
    # The leading `[[:space:]]*` is load-bearing, NOT defensive padding:
    # nextest INDENTS captured test output by four spaces under
    # `--success-output=final`, so a `^SKIP: ` anchored at column 0 matches
    # nothing and this detector silently reports 0 on a run that really did
    # skip. That is exactly what happened while verifying #391 and #467 —
    # four real-agent tests skipped ("Codex could not reach model
    # gpt-5.1-codex-mini …"), were counted as PASSED, and the ATTENTION row
    # below never appeared (issues #452, #490). The same pattern appears at
    # both match sites; keep them in sync, or the row's count will describe a
    # different set of lines than the file it points at.
    #
    # `|| true`, not `|| echo 0`: grep -c already PRINTS "0" when it matches
    # nothing and only then exits 1, so `|| echo 0` produces "0\n0" and the
    # numeric test below dies with "integer expression expected".
    #
    # DELIBERATELY MARKER-LESS, and this is the one place the pattern does NOT
    # match e2e-live.yml byte for byte. Since #502/#785 `_skip_if_err` in
    # tests/common/mod.rs prints `SKIP: [e2e] <reason>`, and that workflow's
    # summary requires the `[e2e]` marker so its count is not polluted by the
    # xtask and unit-test `SKIP:` lines a `--workspace` run also selects. This
    # script keeps the broader pattern for two reasons: it runs against whatever
    # branch a contributor's PR is on, including branches that predate the
    # marker, where requiring it would silently report 0 and re-open #452/#490;
    # and its output is a local file for the human running /verify-pr, not a
    # public job summary, so over-counting here costs a second of reading rather
    # than a wrong answer on a trusted surface. The consequence to know when
    # reading the row below: on a workspace missing `bash`, `jq`, `node` or
    # `python3` this count includes those tools' own skips.
    skips=$(grep -cE '^[[:space:]]*SKIP: ' "$e2e_log" 2>/dev/null || true)
    [[ "$skips" =~ ^[0-9]+$ ]] || skips=0
    emit E2E_RUNTIME_SKIPS "$skips" | tee -a "${out}/env.txt"
    if [ "$skips" -gt 0 ]; then
      # `sed` strips nextest's indent so the file reads as bare `SKIP: …`
      # lines; it runs before `sort -u` so reasons that differ only by
      # indentation still collapse to one entry. Note `sort -u` dedupes: N
      # tests failing the same precondition report as N in the row above but
      # one line here, which is the intended reading (occurrences vs reasons).
      grep -E '^[[:space:]]*SKIP: ' "$e2e_log" | sed 's/^[[:space:]]*//' | sort -u >"${out}/e2e-skips.txt"
      record e2e-real-coverage ATTENTION 0 "${out}/e2e-skips.txt" \
        "${skips} real-agent test(s) skipped and still counted as PASSED. If any covers this PR's surface, rerun it with DOT_AGENT_DECK_REQUIRE_REAL_E2E=1 and treat 'cannot run' as UNVERIFIED, not green."
    fi
  fi
fi

echo "$overall" >"${out}/DONE"
echo
emit SUMMARY_FILE "${summary}"
emit OVERALL "$([ $overall -eq 0 ] && echo PASS || echo FAIL)"
exit $overall
