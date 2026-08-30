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
# `--workspace` is load-bearing, for the same reason it is on the clippy and
# nextest gates (CLAUDE.md rules 2 and 5): cargo's default target selection is
# the ROOT PACKAGE ALONE, so without it this script type-checks none of the
# workspace members. That was a false negative, not a gap in theory — this job
# passed on PR #416 while `build-windows` failed to compile
# `dot-agent-deck-desktop` for Windows with E0277, which is precisely the class
# of break this script exists to catch from Linux in ~1 minute instead of ~9 on
# a Windows runner.
#
# Run it with no arguments. Extra args pass through to `cargo check`, but
# `--features e2e` is not a gate yet: no `tests/e2e_*.rs` carries a file-level
# `#![cfg(unix)]` while the L2 helpers they call are per-item `#[cfg(unix)]`, so
# it reports the L2 tier's standing Unix-only status as dozens of E0425s. CI's
# Windows job does not compile those targets either. Tracked by #164.
#
# Only ERRORS matter. CI's Windows job runs `cargo clippy -- -D warnings`
# without `--all-targets`, so test-target warnings do not fail it. That stayed
# true when issue #407 moved the LINUX job to
# `cargo clippy --all-targets --features e2e -- -D warnings`: the Windows and
# macOS jobs deliberately did not follow, for the same #164 reason as above.

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

# Stand-in Windows resource compiler. `tauri-build` runs `tauri-winres` to
# compile the app icon into a `.res`, which needs `llvm-rc`; a Linux runner has
# no reason to carry one, and without it the build script panics with
# `NotAttempted("llvm-rc")` before any type-checking happens. Same reasoning as
# the CC/AR shims above: `cargo check` never links, so nothing ever reads the
# `.res` we fabricate. It must be named exactly `llvm-rc` because tauri-winres
# looks the tool up on PATH by name rather than through an env override.
RC_SHIM="$SHIM_DIR/llvm-rc"
cat >"$RC_SHIM" <<'SHIM_EOF'
#!/bin/sh
# Accept llvm-rc's output spellings (`/fo X`, `-fo X`, `/foX`, `-foX`) and
# create an empty file there. Case-insensitive because rc accepts `/FO` too.
out=""
want_out=0
for arg in "$@"; do
    if [ "$want_out" = 1 ]; then
        out="$arg"
        want_out=0
        continue
    fi
    case "$arg" in
        /[fF][oO]|-[fF][oO]) want_out=1 ;;
        /[fF][oO]*) out="${arg#???}" ;;
        -[fF][oO]*) out="${arg#???}" ;;
    esac
done
# No output path means a version probe; success is the whole answer.
[ -n "$out" ] || exit 0
case "$out" in
    */*) mkdir -p "${out%/*}" || exit 1 ;;
esac
: >"$out"
SHIM_EOF

chmod +x "$CC_SHIM" "$AR_SHIM" "$RC_SHIM"

# A dedicated target dir: this toolchain is a different rustc version than the
# nix one, so sharing `target/` would make each run invalidate the other's cache
# and force a full rebuild of the next `cargo test-fast`.
#
# Defaults under the XDG cache dir, NOT $TMPDIR: `/tmp` is a RAM-backed tmpfs on
# some machines, where this ~1 GB target dir is charged against memory and swap
# instead of disk. On the maintainer's box it was found holding 949 MB of a
# completely full 8 GB swap, and tmpfs pages cannot be reclaimed like page cache —
# only deleting the files frees them. A build cache also *wants* to survive
# reboots, which `/tmp` does not, so this is the better default on every host
# rather than a workaround for one.
#
# Falls back to $TMPDIR only when neither XDG_CACHE_HOME nor HOME is set, so a
# stripped environment still gets a usable path instead of `/.cache/...`.
# Override with WINDOWS_CROSS_CHECK_TARGET_DIR.
_cache_root="${XDG_CACHE_HOME:-${HOME:+$HOME/.cache}}"
TARGET_DIR="${WINDOWS_CROSS_CHECK_TARGET_DIR:-${_cache_root:-${TMPDIR:-/tmp}}/dot-agent-deck/win-check}"
mkdir -p "$TARGET_DIR"

echo "==> cargo check --workspace --tests --target $TARGET (target-dir: $TARGET_DIR)"
# The compiler and archiver overrides are per-target and their names are
# computed, so they go through `env` — bash only recognises a literal
# `name=value` as an assignment prefix, and would try to *execute* an expanded
# one as a command. Being per-target is what makes them safe: a build script
# compiling something for the *host* still gets the real toolchain.
env \
    "PATH=$SHIM_DIR:$PATH" \
    "CC_${TARGET//-/_}=$CC_SHIM" \
    "AR_${TARGET//-/_}=$AR_SHIM" \
    WINDOWS_CROSS_CHECK_EMPTY_OBJ="$EMPTY_OBJ" \
    RUSTC="$TOOLCHAIN/bin/rustc" \
    CARGO_TARGET_DIR="$TARGET_DIR" \
    "$TOOLCHAIN/bin/cargo" check --workspace --tests --target "$TARGET" "$@"
