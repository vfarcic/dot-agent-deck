#!/usr/bin/env bash
# Run one lane-1 shard of the e2e nextest archive: `e2e-shard.sh <shard> <cmd…>`
# runs `<cmd…>` with that shard's selection appended. `<shard>` is `1`-`4` or
# `version`; `<cmd…>` is the `cargo nextest run --archive-file …` invocation.
# Both CI backends call this — `.github/workflows/ci.yml`'s `e2e` matrix and
# `.rwx/ci.yml`'s `e2e` task — so the selection lives in one place.
#
# WHAT A SHARD RUNS (issues #2091, #2108). The archive is
# `cargo nextest archive --workspace --features e2e`, so it holds every test
# binary in the workspace, the library's and the non-e2e integration tests'
# included. Those already run in the fast tier (`cargo test-fast`, the `test`
# job/task); running them again here was 37% of the shards' summed test time.
# So the shards select:
#
#   * every test in a `tests/e2e_*.rs` binary — `binary(/^e2e_/)`;
#   * every test elsewhere in the root package in a module whose name starts
#     with `e2e_only` (the root package is the one with an `e2e` feature) —
#     the tests that exist only in the `e2e` build (`#[cfg(feature = "e2e")]`
#     in `src/` or a non-e2e integration test), which the fast tier never
#     compiles. linkage-check rule 19 (`xtask/linkage-check/src/e2e_shard_selection.rs`)
#     fails the build when such a test sits outside an `e2e_only…` module, and
#     when this file stops naming either selector.
#
# `lifecycle/version/001` runs a nested cold `cargo build` (72-370s in CI) and
# set the floor of whichever partition drew it, so it runs alone in the
# `version` shard and the other four split the rest with `count:N/4`. Every
# selected test runs in exactly one shard: the four `count` partitions are
# disjoint and cover the filtered set, and the `version` shard takes the one
# test the other four exclude.
set -euo pipefail

if [ "$#" -lt 2 ]; then
  echo "usage: $0 <1|2|3|4|version> <cargo nextest run …>" >&2
  exit 2
fi
shard=$1
shift

version='test(=version_001_injected_build_env_reaches_the_binary)'
lane1='binary(/^e2e_/) | (package(dot-agent-deck) & test(/(^|::)e2e_only[a-z0-9_]*::/))'

case "$shard" in
  version)
    exec "$@" -E "$version"
    ;;
  1 | 2 | 3 | 4)
    exec "$@" --partition "count:$shard/4" -E "($lane1) & not $version"
    ;;
  *)
    echo "$0: unknown shard '$shard' (expected 1-4 or version)" >&2
    exit 2
    ;;
esac
