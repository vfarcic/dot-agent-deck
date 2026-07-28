#!/usr/bin/env bash
#
# Type-check the whole workspace — tests included — for x86_64-pc-windows-msvc
# from a Linux host, so a Windows-only compile break is caught before CI's
# `build-windows` job finds it. `cargo check` does not link, so no MSVC linker
# is needed.
#
# See docs/develop/windows-cross-check.md for why each piece below is necessary
# and for how to fix what this finds (per-item `#[cfg(unix)]` vs. a file-level
# `#![cfg(unix)]`).
#
# Usage: scripts/windows-cross-check.sh [extra cargo args…]
#   e.g. scripts/windows-cross-check.sh --features e2e
#
# Only ERRORS matter. CI's Windows job runs `cargo clippy -- -D warnings`
# without `--all-targets`, so test-target warnings do not fail it.

set -euo pipefail

TARGET=x86_64-pc-windows-msvc
TOOLCHAIN="${WINDOWS_CROSS_CHECK_TOOLCHAIN:-$HOME/.rustup/toolchains/stable-x86_64-unknown-linux-gnu}"

if [[ ! -x "$TOOLCHAIN/bin/cargo" ]]; then
    echo "error: no rustup toolchain at $TOOLCHAIN" >&2
    echo "The devbox/nix cargo on PATH is Linux-only and ships no $TARGET rust-std." >&2
    echo "Install rustup, then: rustup target add $TARGET" >&2
    exit 1
fi

if [[ ! -d "$TOOLCHAIN/lib/rustlib/$TARGET" ]]; then
    echo "error: $TOOLCHAIN has no rust-std for $TARGET" >&2
    echo "Run: rustup target add $TARGET" >&2
    exit 1
fi

# devbox exports AR=ar globally and cc-rs honours it even for an MSVC target, so
# ring's build script hands GNU ar MSVC-style flags and it aborts on
# `ar: invalid option -- ':'`. Rewrite `-out:X` / `-nologo` into `ar crs X …`.
# `cargo check` never links, so the archive only has to exist.
SHIM="$(mktemp -t lib-exe-shim.XXXXXX.sh)"
trap 'rm -f "$SHIM"' EXIT
cat >"$SHIM" <<'SHIM_EOF'
#!/bin/sh
out=""
objs=""
for arg in "$@"; do
    case "$arg" in
        -out:*|/out:*) out="${arg#*:}" ;;
        -nologo|/nologo) ;;
        *) objs="$objs $arg" ;;
    esac
done
[ -z "$out" ] && exec ar "$@"
# shellcheck disable=SC2086
exec ar crs "$out" $objs
SHIM_EOF
chmod +x "$SHIM"

# A dedicated target dir: this toolchain is a different rustc version than the
# nix one, so sharing `target/` would make each run invalidate the other's cache
# and force a full rebuild of the next `cargo test-fast`.
TARGET_DIR="${WINDOWS_CROSS_CHECK_TARGET_DIR:-${TMPDIR:-/tmp}/dot-agent-deck-win-check}"

echo "==> cargo check --tests --target $TARGET (target-dir: $TARGET_DIR)"
# The archiver override is per-target and its name is computed, so it goes
# through `env` — bash only recognises a literal `name=value` as an assignment
# prefix, and would try to *execute* an expanded one as a command.
env \
    "AR_${TARGET//-/_}=$SHIM" \
    RUSTC="$TOOLCHAIN/bin/rustc" \
    CARGO_TARGET_DIR="$TARGET_DIR" \
    "$TOOLCHAIN/bin/cargo" check --tests --target "$TARGET" "$@"
