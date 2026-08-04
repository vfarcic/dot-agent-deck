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
#
# Run it with no arguments. Extra args pass through to `cargo check`, but
# `--features e2e` is not a gate yet: no `tests/e2e_*.rs` carries a file-level
# `#![cfg(unix)]` while the L2 helpers they call are per-item `#[cfg(unix)]`, so
# it reports the L2 tier's standing Unix-only status as dozens of E0425s. CI's
# Windows job does not compile those targets either. Tracked by #164.
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

# devbox exports CC=gcc and AR=ar globally and cc-rs honours both even for an
# MSVC target, so a native Linux toolchain gets handed a Windows cross-compile
# and both native-build stages break:
#
#   * COMPILE — `aws-lc-sys` (rustls' default `aws-lc-rs` provider, in the tree
#     since #269 moved reqwest to 0.13) compiles ~600 C files, and Linux gcc
#     reads Linux system headers, so it dies on `unknown type name
#     'pthread_rwlock_t'` — Windows has no pthreads. (#368)
#   * ARCHIVE — GNU `ar` is handed MSVC `lib.exe` flags and aborts on
#     `ar: invalid option -- ':'`.
#
# `cargo check` never links, so nothing ever *reads* either artefact: an object
# only has to be a valid archive member, and an archive only has to exist. So
# shim both — CC hands back one prebuilt empty object for every compile, AR
# rewrites `-out:X` / `-nologo` into `ar crs X …`. That skips the ~600-file C
# build entirely and leaves the real work, type-checking Rust against the
# target's pre-generated bindings, untouched.
#
# The native libraries these shims produce are deliberately unusable. Nothing
# but a non-linking `cargo check` may be run through them, which is why the
# `cargo` invocation at the bottom hardcodes the subcommand.
SHIM_DIR="$(mktemp -d -t dad-win-check-shims.XXXXXX)"
trap 'rm -rf "$SHIM_DIR"' EXIT

# One real object, from an empty translation unit, compiled by the host compiler
# for the host. Built once so the CC shim is ~600 copies rather than ~600
# compiler runs.
EMPTY_OBJ="$SHIM_DIR/empty.o"
if ! "${CC:-cc}" -x c /dev/null -c -o "$EMPTY_OBJ" 2>"$SHIM_DIR/cc.log"; then
    echo "error: host C compiler '${CC:-cc}' could not build an empty object" >&2
    cat "$SHIM_DIR/cc.log" >&2
    echo "Set CC to a working host compiler, or install one." >&2
    exit 1
fi

CC_SHIM="$SHIM_DIR/cc-shim.sh"
cat >"$CC_SHIM" <<'SHIM_EOF'
#!/bin/sh
# Stand-in C compiler: copy the prebuilt empty object to whatever output path
# was asked for. Accepts both GNU (`-o X`, `-oX`) and MSVC (`-FoX`, `/FoX`)
# spellings, because cc-rs picks the flag dialect from the target, not from us.
out=""
want_out=0
for arg in "$@"; do
    if [ "$want_out" = 1 ]; then
        out="$arg"
        want_out=0
        continue
    fi
    case "$arg" in
        -o) want_out=1 ;;
        -o*) out="${arg#-o}" ;;
        -Fo*|/Fo*) out="${arg#???}" ;;
    esac
done
# No output path means a probe run (cc-rs testing whether a flag is supported),
# and there is nothing to fake — reporting success is the whole answer.
[ -n "$out" ] || exit 0
case "$out" in
    */*) mkdir -p "${out%/*}" || exit 1 ;;
esac
exec cp "${WINDOWS_CROSS_CHECK_EMPTY_OBJ:?}" "$out"
SHIM_EOF

AR_SHIM="$SHIM_DIR/ar-shim.sh"
cat >"$AR_SHIM" <<'SHIM_EOF'
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

chmod +x "$CC_SHIM" "$AR_SHIM"

# A dedicated target dir: this toolchain is a different rustc version than the
# nix one, so sharing `target/` would make each run invalidate the other's cache
# and force a full rebuild of the next `cargo test-fast`.
TARGET_DIR="${WINDOWS_CROSS_CHECK_TARGET_DIR:-${TMPDIR:-/tmp}/dot-agent-deck-win-check}"

echo "==> cargo check --tests --target $TARGET (target-dir: $TARGET_DIR)"
# The compiler and archiver overrides are per-target and their names are
# computed, so they go through `env` — bash only recognises a literal
# `name=value` as an assignment prefix, and would try to *execute* an expanded
# one as a command. Being per-target is what makes them safe: a build script
# compiling something for the *host* still gets the real toolchain.
env \
    "CC_${TARGET//-/_}=$CC_SHIM" \
    "AR_${TARGET//-/_}=$AR_SHIM" \
    WINDOWS_CROSS_CHECK_EMPTY_OBJ="$EMPTY_OBJ" \
    RUSTC="$TOOLCHAIN/bin/rustc" \
    CARGO_TARGET_DIR="$TARGET_DIR" \
    "$TOOLCHAIN/bin/cargo" check --tests --target "$TARGET" "$@"
