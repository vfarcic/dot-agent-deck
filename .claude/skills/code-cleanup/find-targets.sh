#!/usr/bin/env bash
#
# Run the pinned static tools a /code-cleanup unit starts from, and print what
# they find in the one format `pick-area.sh --findings` reads.
#
# Usage: find-targets.sh code
#        find-targets.sh tests --file <production .rs file> [--re <regex>] -- <nextest args>
#        find-targets.sh versions
#
# Every command first prints TOOL=<name> <version> for each tool it runs.
#
# code: runs cargo-shear over the Rust workspace and knip over desktop/, and
# prints one line per finding, then the count:
#
#   TARGET=<tool><TAB><path from the repository root><TAB><line><TAB><what>
#   FINDINGS=<n>
#
# Recorded false positives are the tools' own configuration, not a list here:
# cargo-shear reads `[package.metadata.cargo-shear] ignored` (or the workspace
# form) in the manifest that declares the dependency, and knip reads
# desktop/knip.jsonc. Ownership, busy files and off-limits paths are
# pick-area.sh's filters; this script reports everything the tools report.
#
# tests: runs cargo-mutants in place over one production file, optionally only
# the mutants whose names match --re, testing each with nextest and the args
# after `--`, which must select the tests meant to cover that code (an
# unfiltered run is the full-tree run the skill rules out). Prints
#
#   MUTANT=<caught|missed|timeout|unviable><TAB><file:line:col><TAB><mutation>
#   CAUGHT=<n> MISSED=<n> TIMEOUT=<n> UNVIABLE=<n>   (one per line)
#
# A missed mutant is a defect none of the selected tests catches. It is
# evidence for the tests mode's mutation proof, not a target list. The run
# mutates the file in the working tree (`--in-place`, so the build stays in this
# worktree's target/ rather than a copy under the system temp directory) and
# cargo-mutants restores it after each mutant; the script refuses a file with
# uncommitted changes and checks it is unchanged afterwards.
#
# versions: checks every pinned tool, printing only the TOOL= lines.
#
# A tool that is missing or at a version other than its pin stops the run with
# ERROR= and the command that installs the pin, and a non-zero exit: a run that
# silently fell back to the random draw would look like a tree with no findings.
#
# The pins below are read by a Renovate customManager (renovate.json), which
# keys on the `# renovate:` comment above each one.

set -uo pipefail

# renovate: datasource=crate depName=cargo-shear
CARGO_SHEAR_VERSION=1.14.0
# renovate: datasource=crate depName=cargo-mutants
CARGO_MUTANTS_VERSION=27.1.0
# renovate: datasource=npm depName=knip
KNIP_VERSION=6.42.1

die() { echo "ERROR=$*"; exit 1; }

command -v jq >/dev/null || die "jq not found"
top=$(git rev-parse --show-toplevel 2>/dev/null) || die "not inside a git checkout"
cd "$top" || die "cannot cd to $top"

# The version a cargo subcommand reports: the last word of its first line
# (`Version: 1.14.0`, `cargo-mutants 27.1.0`).
cargo_tool_version() {
  cargo "$1" --version 2>/dev/null | head -n 1 | awk '{print $NF}'
}

require_cargo_tool() { # <crate> <subcommand> <pin>
  local have
  have=$(cargo_tool_version "$2")
  [ -n "$have" ] || die "$1 is not installed; run: cargo install $1 --version $3 --locked"
  [ "$have" = "$3" ] || die "$1 is $have, the pin is $3; run: cargo install $1 --version $3 --locked"
  echo "TOOL=$1 $have"
}

require_knip() {
  command -v pnpm >/dev/null || die "pnpm not found; knip runs through pnpm dlx (devbox provides pnpm)"
  [ -d desktop/node_modules ] \
    || die "desktop/node_modules is missing, and knip needs it to resolve imports; run: pnpm --dir desktop install --frozen-lockfile"
  echo "TOOL=knip $KNIP_VERSION"
}

tmp=$(mktemp -d) || die "mktemp failed"
trap 'rm -rf "$tmp"' EXIT

cmd="${1:-}"
case "$cmd" in
  versions)
    require_cargo_tool cargo-shear shear "$CARGO_SHEAR_VERSION"
    require_cargo_tool cargo-mutants mutants "$CARGO_MUTANTS_VERSION"
    require_knip
    ;;

  code)
    require_cargo_tool cargo-shear shear "$CARGO_SHEAR_VERSION"
    require_knip

    # cargo-shear exits 1 when it finds something, so its verdict is the JSON.
    cargo shear --format json >"$tmp/shear.json" 2>"$tmp/shear.err"
    jq -e '.findings | type == "array"' "$tmp/shear.json" >/dev/null 2>&1 \
      || die "cargo shear produced no findings list: $(head -c 300 "$tmp/shear.err" | tr '\n' ' ')"
    # A finding carries a byte offset into its manifest; the unit needs a line.
    jq -r '.findings[] | [.file, (.location.offset // 0), ((.message + "; " + (.help // "")) | gsub("[\t\r\n]"; " "))] | @tsv' \
      "$tmp/shear.json" >"$tmp/shear.tsv"
    while IFS=$'\t' read -r file offset what; do
      [ -f "$file" ] || die "cargo shear names $file, which is not a file in this checkout"
      line=$(( $(head -c "$offset" "$file" | wc -l) + 1 ))
      printf 'TARGET=cargo-shear\t%s\t%s\t%s\n' "$file" "$line" "$what"
    done <"$tmp/shear.tsv" >"$tmp/targets"

    # knip exits 1 when it finds something too, so again the JSON decides.
    (cd desktop && pnpm dlx "knip@$KNIP_VERSION" --reporter json) >"$tmp/knip.json" 2>"$tmp/knip.err"
    jq -e '.issues | type == "array"' "$tmp/knip.json" >/dev/null 2>&1 \
      || die "knip produced no issues list: $(tail -c 300 "$tmp/knip.err" | tr '\n' ' ')"
    jq -r '
      def clean: tostring | gsub("[\t\r\n]"; " ");
      .issues[] as $i
      | ( (if (($i.files // []) | length) > 0 then {k: "files", n: "unused file", l: 1} else empty end),
          ( ["dependencies", "devDependencies", "optionalPeerDependencies", "unlisted",
             "unresolved", "binaries", "exports", "types", "enumMembers",
             "namespaceMembers", "duplicates", "catalog"][] as $k
            | ($i[$k] // [])
            | (if type == "array" then .[] else (to_entries[] | .value | if type == "array" then .[] else . end) end)
            | (if type == "array"
                 then {n: (map(.name) | join(", ")), l: (.[0].line // 1)}
                 else {n: .name, l: (.line // 1)} end)
            | . + {k: $k} ) )
      | "TARGET=knip\tdesktop/\($i.file | clean)\t\(.l)\t\(.k): \(.n | clean)"' \
      "$tmp/knip.json" >>"$tmp/targets" || die "cannot read knip's JSON report"

    cat "$tmp/targets"
    echo "FINDINGS=$(wc -l <"$tmp/targets")"
    ;;

  tests)
    shift
    file='' re=''
    while [ $# -gt 0 ]; do
      case "$1" in
        --file) file="${2:-}"; shift 2 || die "--file needs a path" ;;
        --re) re="${2:-}"; shift 2 || die "--re needs a regex" ;;
        --) shift; break ;;
        *) die "unknown option: $1" ;;
      esac
    done
    [ -n "$file" ] || die "usage: find-targets.sh tests --file <production .rs file> [--re <regex>] -- <nextest args>"
    [ $# -gt 0 ] || die "name the covering tests after \`--\` (a nextest filter such as -E 'test(/^lifecycle_00/)'); an unfiltered run tests every mutant against the whole package"
    printf '%s\n' "$file" | grep -qE '^(src|desktop/src-tauri/src|xtask/[^/]+/src)/.*\.rs$' \
      || die "$file is not a production Rust file (src/, desktop/src-tauri/src/ or xtask/*/src/)"
    [ -f "$file" ] || die "$file does not exist"
    git diff --quiet HEAD -- "$file" || die "$file has uncommitted changes; a mutation run must start from a clean file"
    require_cargo_tool cargo-mutants mutants "$CARGO_MUTANTS_VERSION"

    out="$top/target/code-cleanup"
    mkdir -p "$out" || die "cannot create $out"
    args=(--in-place --test-tool nextest --no-shuffle -f "$file" -o "$out")
    [ -n "$re" ] && args+=(-F "$re")
    cargo mutants "${args[@]}" -- "$@" >"$tmp/mutants.log" 2>&1
    status=$?
    git diff --quiet HEAD -- "$file" \
      || die "$file was left changed by the mutation run; restore it with: git restore -- $file"
    # 0: every mutant caught; 2: some missed; 3: some timed out. Anything else
    # (a failed baseline, a usage error) is not a result.
    case "$status" in
      0|2|3) ;;
      *) die "cargo mutants exited $status: $(tail -n 5 "$tmp/mutants.log" | tr '\n' ' ')" ;;
    esac
    : >"$tmp/counts"
    for outcome in caught missed timeout unviable; do
      list="$out/mutants.out/$outcome.txt"
      n=0
      if [ -f "$list" ]; then
        while IFS= read -r m; do
          [ -n "$m" ] || continue
          printf 'MUTANT=%s\t%s\t%s\n' "$outcome" "${m%%: *}" "${m#*: }"
          n=$((n + 1))
        done <"$list"
      fi
      echo "$(printf '%s' "$outcome" | tr '[:lower:]' '[:upper:]')=$n" >>"$tmp/counts"
    done
    cat "$tmp/counts"
    ;;

  *) die "usage: find-targets.sh <code|tests|versions> (see the header)" ;;
esac
