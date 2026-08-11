#!/usr/bin/env bash
#
# Build, verify and publish ONE release target. Port of the `build` job in
# .github/workflows/release.yml.
#
# WHY A SCRIPT AND NOT A MATRIX WITH SEVERAL AXES: the GHA matrix carries FOUR
# paired values per entry (target, os, artifact_suffix, use_cross). Semaphore's
# `matrix:` multiplies its axes rather than zipping them, so expressing the same
# pairing in YAML would generate 4x2x2 nonsense combinations. One axis (TARGET)
# plus the lookup below is the faithful translation.
#
# Inputs:
#   TARGET       required. Rust target triple.
#   DAD_VERSION  required. The version being released (issue #250).
set -euo pipefail

TARGET="${TARGET:?TARGET must be set}"
: "${DAD_VERSION:?DAD_VERSION must be set}"
export DAD_VERSION

case "$TARGET" in
  x86_64-unknown-linux-gnu)  SUFFIX=linux-amd64;  USE_CROSS=false ;;
  aarch64-unknown-linux-gnu) SUFFIX=linux-arm64;  USE_CROSS=true  ;;
  x86_64-apple-darwin)       SUFFIX=darwin-amd64; USE_CROSS=false ;;
  aarch64-apple-darwin)      SUFFIX=darwin-arm64; USE_CROSS=false ;;
  *) echo "ERROR: unknown release target '$TARGET'" >&2; exit 1 ;;
esac

if [ "$USE_CROSS" = "true" ]; then
  command -v cross >/dev/null 2>&1 || cargo install cross --locked
  # `cross` runs cargo inside a container and forwards only the CARGO_*/CROSS_*
  # host variables by default, so DAD_VERSION has to be named explicitly.
  #
  # TWO INDEPENDENT CHANNELS, deliberately. CROSS_BUILD_ENV_PASSTHROUGH is
  # cross's own allowlist; CROSS_CONTAINER_OPTS is appended to cross's
  # `docker run` argv and forwards the value below cross's config layer. `cargo
  # install cross --locked` is NOT version-pinned, so a future cross release
  # could rename or retire its passthrough allowlist — and that line is the only
  # thing standing between a release and a mislabeled ARM64 binary. Both degrade
  # safely: with DAD_VERSION empty, build.rs treats blank as "not injected".
  export CROSS_BUILD_ENV_PASSTHROUGH=DAD_VERSION
  export CROSS_CONTAINER_OPTS="-e DAD_VERSION"
  cross build --release --target "$TARGET"
else
  cargo build --release --target "$TARGET"
fi

BIN="target/${TARGET}/release/dot-agent-deck"

# Issue #250 is "an artifact published under one version while reporting
# another", so the release path asserts that it did not just do that.
#
# SCOPED TO THE CROSS LEG, matching GHA. The native legs get DAD_VERSION from
# the job environment with no boundary to lose it at; the container is the only
# place the value travels between environments. This check runs on the HOST
# against the finished artifact, so it cannot be defeated by the failure it is
# looking for — which a strictness flag inside build.rs could be, since that
# would have to be forwarded too.
if [ "$USE_CROSS" = "true" ]; then
  # An empty needle would make `grep -F` match anything, so the vacuous pass is
  # rejected explicitly rather than relying on the SemVer gate upstream.
  if [ -z "$DAD_VERSION" ]; then
    echo "ERROR: Refusing to publish: the prepared release version is empty, so the artifact cannot be verified." >&2
    exit 1
  fi
  # Checked separately so a build that produced no binary reports that, rather
  # than being blamed on the injection.
  if [ ! -f "$BIN" ]; then
    echo "ERROR: Refusing to publish: expected ${TARGET} binary at ${BIN}, but it does not exist." >&2
    exit 1
  fi
  # `env!("DAD_VERSION")` is a &'static str reached at runtime, so the exact
  # version is in the binary's rodata. When injection is lost the value is a
  # different string entirely and the count is 0.
  if ! grep -a -q -F -- "$DAD_VERSION" "$BIN"; then
    echo "ERROR: ${TARGET} artifact does not contain the version being released (${DAD_VERSION}). The injected DAD_VERSION did not reach the build script inside the cross container, so this binary reports a different version than the tag it would be published under (issue #250). Refusing to publish it." >&2
    exit 1
  fi
  echo "OK: ${TARGET} artifact reports ${DAD_VERSION}"
fi

mkdir -p dist
cp "$BIN" "dist/dot-agent-deck-${SUFFIX}"

# GHA equivalent: actions/upload-artifact, later collected by `finalize` with
# download-artifact + merge-multiple. `workflow` scope is the Semaphore
# counterpart — it survives across blocks within this workflow.
artifact push workflow "dist/dot-agent-deck-${SUFFIX}" --force
echo "OK: pushed dist/dot-agent-deck-${SUFFIX}"
