#!/usr/bin/env bash
# Which parts of CI a change needs, for both CI systems: `.github/workflows/ci.yml`
# (its `changes` job) and `.rwx/ci.yml` (its `classify` task) call this, so the
# two cannot drift apart (docs/develop/rwx.md).
#
# Usage: scripts/ci-changes.sh <event> <author> < changed-paths
#   <event>   pull_request, push, workflow_dispatch, cli (an `rwx run`) or cron
#             (RWX's daily run on main)
#   <author>  the pull request's author login; empty for anything else
#   stdin     the changed paths, one per line, the files a PR view lists. Empty
#             stdin means "unknown", which selects everything.
#
# Prints `key=value` lines:
#   devbox_only  a Renovate PR touching only devbox.json/devbox.lock: the Rust
#                jobs and the nix job are skipped
#   flake_only   a Renovate PR touching only flake.nix/flake.lock: the Rust jobs
#                are skipped, nix runs
#   binary       the change feeds the binary or the cross-version harness, so
#                `cross-version` runs (a PR only; a manual run always does)
#   cargo        Cargo.toml, Cargo.lock, a vendored crate, the cargo
#                configuration, a toolchain file or a CI definition changed, so
#                the aarch64 cross-build runs
set -euo pipefail

event=${1:?usage: ci-changes.sh <event> <author> < changed-paths}
author=${2-}
files=$(cat)

devbox_only=false
flake_only=false
if [ "$event" = pull_request ] && [ "$author" = "renovate[bot]" ] && [ -n "$files" ]; then
  devbox_only=true
  flake_only=true
  while IFS= read -r f; do
    case "$f" in
      devbox.json | devbox.lock) flake_only=false ;;
      flake.nix | flake.lock) devbox_only=false ;;
      *)
        echo "'$f' is outside devbox and outside the flake -> full CI" >&2
        devbox_only=false
        flake_only=false
        break
        ;;
    esac
  done <<< "$files"
fi

binary=false
cargo=false
case "$event" in
  workflow_dispatch | cli | cron)
    binary=true
    cargo=true
    ;;
  pull_request)
    if [ -z "$files" ]; then
      echo "no files reported -> treating everything as changed" >&2
      binary=true
      cargo=true
    fi
    while IFS= read -r f; do
      case "$f" in
        src/* | build.rs | build_version_resolve.rs | Cargo.toml | Cargo.lock | \
          .cargo/* | scripts/link-gate.sh | scripts/build-gate.sh | \
          rust-toolchain | rust-toolchain.toml | vendor/* | xtask/cross-version/*)
          binary=true
          ;;
      esac
      case "$f" in
        Cargo.toml | Cargo.lock | vendor/* | .cargo/* | rust-toolchain | rust-toolchain.toml | \
          .github/workflows/aarch64-crossbuild-check.yml | .rwx/ci.yml)
          cargo=true
          ;;
      esac
    done <<< "$files"
    ;;
esac

echo "devbox_only=$devbox_only"
echo "flake_only=$flake_only"
echo "binary=$binary"
echo "cargo=$cargo"
