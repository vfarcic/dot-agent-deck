#!/usr/bin/env bash
#
# Phase 2 of /verify-pr: run every automated gate this repo has, in one pass,
# without stopping at the first failure — a review needs the whole picture, not
# the first thing that broke.
#
# Usage: checks.sh [--dir <worktree>] [--no-e2e] [--only <a,b,c>] [--filter <expr>]
#
#   --dir     worktree to run in (default: current directory)
#   --no-e2e  skip the e2e tier (CLAUDE.md rule 5's expensive tier: spawns real
#             binaries and hits real LLM APIs, so it costs money and minutes)
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
# Exit code is 0 when every executed step passed, 1 otherwise.

set -uo pipefail

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
      echo "ERROR=true"
      echo "MESSAGE=Unknown argument '$1'"
      exit 1
      ;;
  esac
done

if [ ! -d "$dir" ]; then
  echo "ERROR=true"
  echo "MESSAGE=No such directory: ${dir}"
  exit 1
fi
dir=$(cd "$dir" && pwd)

if [ ! -f "${dir}/Cargo.toml" ]; then
  echo "ERROR=true"
  echo "MESSAGE=${dir} is not a Rust workspace root (no Cargo.toml)"
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
  echo "DIR=${dir}"
  echo "HEAD=$(git -C "$dir" rev-parse --short HEAD 2>/dev/null || echo unknown)"
  echo "IN_DEVBOX=${DEVBOX_SHELL_ENABLED:-0}"
  # `head -1` on every one of these: `cargo nextest --version` prints three
  # lines, which would break the KEY=value shape of this file.
  echo "RUSTC=$(rustc --version 2>/dev/null | head -1 || echo missing)"
  echo "CARGO=$(cargo --version 2>/dev/null | head -1 || echo missing)"
  echo "NEXTEST=$(cargo nextest --version 2>/dev/null | head -1 || echo missing)"
  echo "CARGO_AUDIT=$(cargo audit --version 2>/dev/null | head -1 || echo missing)"
  for cli in claude opencode codex pi; do
    if command -v "$cli" >/dev/null 2>&1; then
      echo "AGENT_${cli}=present"
    else
      echo "AGENT_${cli}=MISSING"
    fi
  done
  echo "PLATFORM=$(uname -s)/$(uname -m)"
} | tee "${out}/env.txt"
echo

if ! command -v cargo >/dev/null 2>&1; then
  echo "ERROR=true"
  echo "MESSAGE=cargo is not on PATH"
  exit 1
fi

have_nextest=true
cargo nextest --version >/dev/null 2>&1 || have_nextest=false

# --- The gates, cheapest first -------------------------------------------

build_ok=true

# CI: `cargo fmt --check` (CLAUDE.md rule 2).
wanted fmt && { run_step fmt "cargo fmt --check" || true; } || skip fmt "not in --only"

# CLAUDE.md rule 2, and the Linux `build` job in ci.yml matches this exactly.
# All three flags matter. Both of #407's: without `--all-targets` no test
# target is built at all, and without `--features e2e` every `tests/e2e_*.rs`
# compiles to an empty crate — so the bare `cargo clippy` this used to run
# reported clean over a build that never contained the e2e code under review.
# That is exactly the wrong answer for a PR review to give. And `--workspace`
# (issue #436): without it cargo selects the root package alone, so a PR
# touching only `xtask/*` — linkage-check, the docs generator, the `spec`
# macro — was reviewed against a lint that never read a line of it.
#
# The e2e step further down still runs the tier itself; this one only
# type-checks and lints it, in seconds, before the expensive steps start.
#
# build-windows/build-macos still run bare `cargo clippy` (the L2 tier is
# Unix-only), so a Linux-only lint here is expected and correct.
wanted clippy && { run_step clippy "cargo clippy --workspace --all-targets --features e2e -- -D warnings" || true; } || skip clippy "not in --only"

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

# --- e2e: the tier that actually exercises the product -------------------

if [ "$run_e2e" != true ]; then
  skip e2e "--no-e2e was passed — SAY SO in the report; do not present the run as complete"
elif ! wanted e2e; then
  skip e2e "not in --only"
elif [ "$build_ok" != true ]; then
  record e2e BLOCKED 0 - "build failed; fix that first"
elif [ "$have_nextest" != true ]; then
  record e2e BLOCKED 0 - "cargo-nextest missing"
else
  # `--success-output=final` is NOT cosmetic. Real-agent tests that cannot run
  # (agent CLI absent, credentials expired) print `SKIP: <reason>` and return
  # NORMALLY, so nextest counts them as PASSED. Without this flag nextest
  # suppresses passing tests' output and those skips are invisible — a green
  # e2e run that proved nothing. See `REQUIRE_REAL_E2E_ENV` in tests/common/mod.rs.
  # `--workspace` (issue #489): same reason as the test-fast step above — keep
  # this in lockstep with the `test-e2e` alias in .cargo/config.toml.
  run_step e2e "cargo nextest run --workspace --features e2e --success-output=final${test_filter}" "rule 5 e2e tier" || true

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
    skips=$(grep -cE '^[[:space:]]*SKIP: ' "$e2e_log" 2>/dev/null || true)
    [[ "$skips" =~ ^[0-9]+$ ]] || skips=0
    printf 'E2E_RUNTIME_SKIPS=%s\n' "$skips" >>"${out}/env.txt"
    printf 'E2E_RUNTIME_SKIPS=%s\n' "$skips"
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
echo "SUMMARY_FILE=${summary}"
echo "OVERALL=$([ $overall -eq 0 ] && echo PASS || echo FAIL)"
exit $overall
