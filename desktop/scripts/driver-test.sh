#!/usr/bin/env bash
#
# Issue #953 — build and run the driver tier locally: the real Tauri window,
# driven by tauri-driver and WebKitWebDriver, against a real sandbox daemon.
# CI's `desktop-driver` job runs the same `pnpm test:driver` on the same
# artifacts; this script is how a contributor gets there in one command.
#
# Usage (from desktop/, inside `devbox shell` or with the apt set installed):
#   sh ./scripts/driver-test.sh              # build both binaries, then run
#   sh ./scripts/driver-test.sh --no-build   # run against what is already built
#   DAD_DRIVER_REPEAT=5 sh ./scripts/driver-test.sh --no-build   # flake check
#
# What it resolves, in order, and why each one is here rather than in the
# harness:
#
#   * tauri-driver: `DAD_DRIVER_TAURI_DRIVER`, else PATH. Its version is pinned
#     ONCE, as `TAURI_DRIVER_VERSION:` in .github/workflows/ci.yml (the line
#     renovate.json's tauri-driver manager bumps), and this script reads it from
#     there rather than restating it, so a local run and CI cannot silently
#     disagree about which driver ran.
#   * WebKitWebDriver: `DAD_DRIVER_NATIVE_DRIVER`, else PATH, else the `bin/`
#     beside the WebKitGTK that pkg-config resolves. The last one is the devbox
#     case: nixpkgs' webkitgtk ships WebKitWebDriver, and it is then the SAME
#     WebKitGTK the app was linked against, which is the pairing that matters.
#   * An EGL driver, on a devbox (Nix) WebKitGTK only. That WebKitGTK aborts at
#     startup with `Could not create default EGL display: EGL_BAD_PARAMETER`
#     on a non-NixOS host: Nix's libglvnd looks for vendor files under its own
#     prefix and /run/opengl-driver, and neither exists there. The remedy is
#     mesa from the SAME nixpkgs revision tauri-deps/flake.lock pins, pointed at
#     through glvnd's own override variables. It is resolved here, not added to
#     devbox.json, because tauri-deps/flake.nix deliberately ships no shared
#     libraries (see its header) and only this tier needs one.

set -euo pipefail

cd "$(dirname "$0")/.."
REPO_ROOT=$(cd .. && pwd)

build=1
if [ "${1:-}" = "--no-build" ]; then
  build=0
fi

pin=$(sed -nE 's/^[[:space:]]*TAURI_DRIVER_VERSION:[[:space:]]*([0-9]+\.[0-9]+\.[0-9]+)[[:space:]]*$/\1/p' \
  "$REPO_ROOT/.github/workflows/ci.yml" | head -n 1)
if [ -z "$pin" ]; then
  echo "driver-test: no TAURI_DRIVER_VERSION pin found in .github/workflows/ci.yml" >&2
  exit 1
fi

tauri_driver=${DAD_DRIVER_TAURI_DRIVER:-$(command -v tauri-driver || true)}
if [ -z "$tauri_driver" ]; then
  echo "driver-test: tauri-driver not found. Install the pinned version:" >&2
  echo "  cargo install tauri-driver --locked --version $pin" >&2
  exit 1
fi
# tauri-driver has no `--version`, so the version is read from the install
# record of the cargo root it lives under — which is where `cargo install`
# puts it, and the only way it is installed here or in CI.
# `|| true`: cargo exits non-zero for a root with no install record at all,
# and under `set -e` plus `pipefail` that would end the script here silently
# instead of reaching the refusal below that says why.
have=$( (cargo install --list --root "$(dirname "$(dirname "$tauri_driver")")" 2>/dev/null || true) |
  sed -nE 's/^tauri-driver v([0-9.]+):$/\1/p')
if [ -z "$have" ]; then
  if [ -z "${DAD_DRIVER_SKIP_PIN_CHECK:-}" ]; then
    echo "driver-test: cannot tell which tauri-driver $tauri_driver is (no cargo install record); CI pins $pin." >&2
    echo "  cargo install tauri-driver --locked --version $pin" >&2
    echo "  or set DAD_DRIVER_SKIP_PIN_CHECK=1 to run an unverified driver deliberately" >&2
    exit 1
  fi
  echo "driver-test: DAD_DRIVER_SKIP_PIN_CHECK set; running an unverified tauri-driver ($tauri_driver), CI pins $pin" >&2
elif [ "$have" != "$pin" ]; then
  echo "driver-test: $tauri_driver is $have, CI pins $pin:" >&2
  echo "  cargo install tauri-driver --locked --version $pin" >&2
  exit 1
fi
export DAD_DRIVER_TAURI_DRIVER=$tauri_driver

webkit_libdir=$(pkg-config --variable=libdir webkit2gtk-4.1 2>/dev/null || true)
native=${DAD_DRIVER_NATIVE_DRIVER:-$(command -v WebKitWebDriver || true)}
if [ -z "$native" ] && [ -n "$webkit_libdir" ] && [ -x "$webkit_libdir/../bin/WebKitWebDriver" ]; then
  native=$(cd "$webkit_libdir/../bin" && pwd)/WebKitWebDriver
fi
if [ -z "$native" ]; then
  echo "driver-test: WebKitWebDriver not found (apt: webkit2gtk-driver; devbox: beside pkg-config's webkit2gtk-4.1)" >&2
  exit 1
fi
export DAD_DRIVER_NATIVE_DRIVER=$native

case "$webkit_libdir" in
  /nix/store/*)
    if [ -z "${__EGL_VENDOR_LIBRARY_FILENAMES:-}" ]; then
      rev=$(jq -r '.nodes.nixpkgs.locked.rev' "$REPO_ROOT/tauri-deps/flake.lock")
      echo "driver-test: resolving mesa from nixpkgs $rev for the Nix WebKitGTK's EGL" >&2
      mesa=$(nix --extra-experimental-features 'nix-command flakes' build --no-link --print-out-paths \
        "github:NixOS/nixpkgs/$rev#mesa")
      export __EGL_VENDOR_LIBRARY_FILENAMES=$mesa/share/glvnd/egl_vendor.d/50_mesa.json
      export LIBGL_DRIVERS_PATH=$mesa/lib/dri
      export GBM_BACKENDS_PATH=$mesa/lib/gbm
    fi
    ;;
esac

if [ "$build" = 1 ]; then
  (cd "$REPO_ROOT" && cargo build --locked --bin dot-agent-deck)
  # `--no-bundle`: the binary is what tauri-driver launches, and a .deb would
  # only add the bundling prerequisites desktop-gui.md says nothing else needs.
  # The seam variable is what compiles `window.__dadDriver` into THIS bundle.
  VITE_DAD_DRIVER_SEAM=1 pnpm tauri build --debug --no-bundle
fi

runs=${DAD_DRIVER_REPEAT:-1}
passed=0
for i in $(seq 1 "$runs"); do
  echo "driver-test: run $i of $runs" >&2
  if [ -n "${DISPLAY:-}" ] && [ -z "${DAD_DRIVER_XVFB:-}" ]; then
    status=0
    pnpm test:driver || status=$?
  else
    status=0
    # 96 DPI, not xvfb's default 100: see the devicePixelRatio check in
    # driver/harness.ts for what a scaled display does to every click.
    xvfb-run -a -s "-screen 0 1920x1080x24 -dpi 96" pnpm test:driver || status=$?
  fi
  if [ "$status" = 0 ]; then
    passed=$((passed + 1))
  fi
done
echo "driver-test: $passed of $runs runs passed" >&2
[ "$passed" = "$runs" ]
