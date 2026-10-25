#!/usr/bin/env bash
#
# `cargo audit` against the upstream releases of the crates under `vendor/`.
#
# A crate that `[patch.crates-io]` replaces with a path copy has no `source` in
# Cargo.lock, and cargo-audit 0.22.2 (rustsec 0.33.0) skips every lock entry
# without a crates.io source (`Database::query_vulnerabilities`). So the
# ordinary `cargo audit` stops reporting advisories for those crates, crossterm
# today, without saying so. This audits a copy of Cargo.lock in which each
# vendored crate's entry carries the crates.io source again, which brings it
# back into the scan.
#
# What that checks: whether RustSec has an advisory for the UPSTREAM version the
# copy was taken from. It does not authenticate the copy, and an advisory it
# reports may or may not apply to the patched code; read the advisory against
# the patch (`vendor/<crate>/VENDORED.md`). The ordinary `cargo audit` still
# runs on its own for the real dependency graph; this adds to it.
#
# Usage: scripts/audit-vendored-upstream.sh [cargo-audit options...]
# The options go to `cargo audit` as they are, e.g. `--db <dir> --no-fetch`.

set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
view=$(mktemp -d)
trap 'rm -rf "$view"' EXIT

names=()
for manifest in "$root"/vendor/*/Cargo.toml; do
  [ -e "$manifest" ] || continue
  names+=("$(basename "$(dirname "$manifest")")")
done
if [ "${#names[@]}" -eq 0 ]; then
  echo "audit-vendored-upstream: nothing under vendor/ to audit"
  exit 0
fi

# One `[[package]]` block at a time: a vendored crate's block without a
# `source` line gets the crates.io source after its `version` line.
awk -v names=" ${names[*]} " '
  function flush(   i) {
    if (n == 0) return
    if (index(names, " " name " ") && !has_source) {
      for (i = 1; i <= n; i++) {
        print lines[i]
        if (lines[i] ~ /^version = /)
          print "source = \"registry+https://github.com/rust-lang/crates.io-index\""
      }
      restored[name]++
    } else {
      for (i = 1; i <= n; i++) print lines[i]
    }
    n = 0; name = ""; has_source = 0
  }
  /^\[\[package\]\]$/ { flush() }
  /^$/ { flush(); print; next }
  {
    lines[++n] = $0
    if ($0 ~ /^name = /) { name = $3; gsub(/"/, "", name) }
    if ($0 ~ /^source = /) has_source = 1
  }
  END {
    flush()
    split(names, wanted, " ")
    for (i in wanted) {
      if (wanted[i] == "") continue
      if (restored[wanted[i]] != 1) {
        printf "audit-vendored-upstream: Cargo.lock has %d path entries for %s, expected 1\n", restored[wanted[i]], wanted[i] > "/dev/stderr"
        failed = 1
      }
    }
    exit failed
  }
' "$root/Cargo.lock" > "$view/Cargo.lock"

echo "audit-vendored-upstream: auditing the upstream releases of: ${names[*]}"
cargo audit --file "$view/Cargo.lock" "$@"
