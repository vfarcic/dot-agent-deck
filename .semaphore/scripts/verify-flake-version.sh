#!/usr/bin/env bash
#
# Port of the "Verify the flake version matches the release" step in
# .github/workflows/release.yml.
#
# flake.nix pins the released version in a let-binding and hands it to the build
# as DAD_VERSION, because a `nix run github:...` build gets a source tarball
# with no .git for build.rs to describe and would otherwise fall back to the
# 0.1.0 placeholder. That pin is a hand-maintained line: a release that bumps
# the tag without bumping the flake ships a source build reporting the wrong
# version. Fail the release loudly here rather than publish the mismatch.
#
# Usage: verify-flake-version.sh <version>
set -euo pipefail
export LC_ALL=C

VERSION="${1:?Usage: verify-flake-version.sh <version>}"

# A grep/sed over flake.nix, not `nix eval`. The eval is the robust read but it
# needs Nix on the agent, and this block is short metadata work: installing Nix
# and evaluating the flake would put minutes on the critical path of every
# release to read one string literal. The `Nix flake check` block in
# semaphore.yml already proves the flake evaluates and builds; all this has to
# do is compare a version.
#
# The cheap read is only worth it if it cannot silently match nothing, or match
# the wrong line, and pass. So the pattern is anchored to a whole line and the
# match count is asserted to be exactly 1.
matches=$(grep -cE '^[[:space:]]*version = "[^"]+";[[:space:]]*$' flake.nix || true)
if [ "${matches:-0}" -ne 1 ]; then
  echo "ERROR: expected exactly one anchored 'version = \"...\";' line in flake.nix, found ${matches:-0}. The extraction pattern no longer fits the file, so the version pin cannot be checked." >&2
  exit 1
fi

# Same pattern as the guard, character for character: a `[^"]*` extractor would
# be a strict superset of a `[^"]+` guard, so it could also print a
# `version = "";` line the guard did not count.
FLAKE_VERSION=$(sed -nE 's/^[[:space:]]*version = "([^"]+)";[[:space:]]*$/\1/p' flake.nix)
if [ -z "$FLAKE_VERSION" ]; then
  echo "ERROR: the pinned version in flake.nix extracted as an empty string." >&2
  exit 1
fi
if [ "$FLAKE_VERSION" != "$VERSION" ]; then
  echo "ERROR: flake.nix pins version '$FLAKE_VERSION' but the release is '$VERSION'. Bump the version let-binding in flake.nix so the Nix build reports the release it actually is." >&2
  exit 1
fi

# A matching pin is necessary but not sufficient. `version` is only a
# let-binding, and everything above still passes if the derivation has stopped
# handing it to the build: `DAD_VERSION = "0.1.0";` next to `version = "0.35.7";`
# agrees with the tag and ships a binary reporting the placeholder. So assert
# the wire too, under the same anchoring discipline as the pin.
wired=$(grep -cE '^[[:space:]]*DAD_VERSION = version;[[:space:]]*$' flake.nix || true)
if [ "${wired:-0}" -ne 1 ]; then
  echo "ERROR: expected exactly one anchored 'DAD_VERSION = version;' line in flake.nix, found ${wired:-0}. The pinned version is no longer wired to the build, so the Nix build reports whatever that env value now says rather than the release it claims to be." >&2
  exit 1
fi

echo "OK: flake.nix pins '$FLAKE_VERSION', which matches the release, and hands it to the build as DAD_VERSION."
