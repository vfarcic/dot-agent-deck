#!/usr/bin/env bash
# Install the pinned real OpenCode 1.x and 2.x side by side, for the
# credential-free lane-1 tests that start each one in a deck pane with no
# model (issue #2051, tests/e2e_opencode_real_binary.rs).
#
#   scripts/install-pinned-opencode.sh
#
# The pins are scripts/opencode-pins/v1 and v2: a package.json naming the exact
# version and the package-lock.json that records each tarball's integrity hash,
# installed with `npm ci`, so npm refuses a tarball whose hash differs.
# Renovate's npm manager bumps them; scripts/box/bootstrap.sh pins the same
# versions for a dev box, and xtask/linkage-check's `opencode_pins` tests fail
# when the two disagree.
#
# Each goes where scripts/box/bootstrap.sh puts it, which is where the test
# harness looks (`common::pinned_opencode_bin`, tests/common/lib.rs):
#
#   1.x  ~/.local/share/dot-agent-deck/opencode-1/bin/opencode
#   2.x  ~/.local/share/dot-agent-deck/opencode-2/node_modules/.bin/opencode
#
# Neither is put on PATH. A prefix already holding the pinned version is left
# alone, so a dev box that ran bootstrap.sh is not reinstalled. Each package's
# postinstall puts its platform binary in place, as bootstrap.sh relies on too;
# the version check at the end fails the run if a binary is not the pinned one.
#
# Installing a binary starts nothing: no model, provider or credential is
# involved here, and CLAUDE.md rule 5 is kept by what the tests do with it.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
pins="$here/opencode-pins"
root="${HOME:?}/.local/share/dot-agent-deck"

# The version each pin's package.json names, read without jq or node.
pinned_version() {
  sed -En 's#^ *"(opencode-ai|@opencode/cli)": *"([0-9][0-9.]*)".*#\2#p' "$pins/$1/package.json"
}

# What a binary's `--version` reports, as X.Y.Z: 2.x prints `opencode v2.2.5`,
# 1.x prints `1.19.5`.
reported_version() {
  "$1" --version 2>/dev/null | head -n1 | sed 's/^opencode //; s/^v//'
}

install_pin() {
  local pin=$1 prefix=$2 bin=$3 package=$4 want
  want=$(pinned_version "$pin")
  [ -n "$want" ] || { echo "$0: no pinned version in $pins/$pin/package.json" >&2; exit 1; }
  if [ "$(reported_version "$bin")" = "$want" ]; then
    echo "OpenCode $want already at $bin"
    return
  fi
  mkdir -p "$prefix"
  cp "$pins/$pin/package.json" "$pins/$pin/package-lock.json" "$prefix/"
  npm ci --prefix "$prefix" --no-fund --no-audit
  # An npm that holds install scripts back (`ignore-scripts`, or a newer npm's
  # `allowScripts` policy, which only warns as of npm 11) leaves the package's
  # placeholder in place of the binary. This package's postinstall is the one
  # script the install needs, so it is run here by name, as bootstrap.sh does.
  if [ "$(reported_version "$bin")" != "$want" ]; then
    (cd "$prefix/node_modules/$package" && node ./postinstall.mjs)
  fi
  local got
  got=$(reported_version "$bin")
  if [ "$got" != "$want" ]; then
    echo "$0: $bin reports '${got:-nothing}', not the pinned OpenCode $want" >&2
    exit 1
  fi
  echo "OpenCode $want installed at $bin"
}

install_pin v1 "$root/opencode-1" "$root/opencode-1/node_modules/.bin/opencode" opencode-ai
# bootstrap.sh's stable path for the 1.x binary, which the harness resolves.
mkdir -p "$root/opencode-1/bin"
ln -sfn ../node_modules/.bin/opencode "$root/opencode-1/bin/opencode"
install_pin v2 "$root/opencode-2" "$root/opencode-2/node_modules/.bin/opencode" @opencode/cli
