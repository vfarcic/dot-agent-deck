#!/usr/bin/env bash
#
# Assert that Tauri's GTK/WebKit stack resolves through pkg-config to Nix's
# copy — not the host distribution's — inside a devbox environment on Linux.
#
# Usage: devbox run -- bash scripts/devbox-check-gtk.sh
#        (scripts/devbox-smoke.sh calls it; CI's `devbox` job runs that.)
#
# WHY THIS EXISTS, AND WHY `--modversion` ALONE IS NOT ENOUGH (issue #815).
#
# Issue #771 failed LOUDLY: no GTK anywhere meant `cargo clippy` stopped with
# `The system library 'glib-2.0' required by crate 'glib-sys' was not found`.
# #780 fixed that by freezing the transitive pkg-config closure into
# `tauri-deps/flake.nix`. What replaced #771 was a SILENT failure of the same
# shape, and it is strictly worse: when pkg-config answers from the host's
# `/usr/lib/pkgconfig` instead of the store, every step that reports success
# reports success — `pkg-config --exists gtk+-3.0` passes, `--modversion`
# prints a real version, the compile links clean, `cargo clippy --workspace
# --all-targets` goes green — and the first thing to notice is a test binary
# refusing to start, tens of minutes later, with
#
#     libgdk-3.so.0: cannot open shared object file: No such file or directory
#
# naming a library nobody configured. A devbox shell runs under Nix glibc,
# whose loader cache does not exist (`ldconfig -p` returns zero entries), so
# `/usr/lib` is invisible to the dynamic linker however it got populated.
#
# So the assertion is on the RESOLVED `libdir`, not on the module's presence.
# That is the exact value the answer turns into: a `-sys` build script emits
# `cargo:rustc-link-search=native=<libdir>`, rustc passes it to Nix's `ld`
# wrapper, and the wrapper mints a matching `-rpath` for every `-L` UNDER
# `/nix/store` and for no other. A `/usr/lib/x86_64-linux-gnu` answer is
# therefore the precise moment the binary loses its RUNPATH — which is what
# makes this the right place to fail, rather than at load time.
#
# It also catches the case that actually bit: a STALE devbox environment. See
# the remedy printed on failure below.
#
# Linux only. macOS builds Tauri against the system WebKit, so the flake yields
# an empty output there and none of these modules exists.

set -euo pipefail

if [ "$(uname -s)" != "Linux" ]; then
  echo "  skipped: not Linux"
  exit 0
fi

# The same set `ci.yml`'s `build` job installs with apt, and the set
# `tauri-deps/flake.nix` freezes. Keep the two in step.
MODULES=(
  glib-2.0 gobject-2.0 gio-2.0
  gtk+-3.0 gdk-3.0 gdk-x11-3.0
  webkit2gtk-4.1 javascriptcoregtk-4.1 libsoup-3.0
  ayatana-appindicator3-0.1 dbus-1 librsvg-2.0 libxdo
)

resolved_pkg_config="$(command -v pkg-config || true)"
if [ -z "$resolved_pkg_config" ]; then
  echo "FAIL: no pkg-config on PATH at all." >&2
  echo "      devbox.json pins pkg-config@0.29.2, so this means the devbox" >&2
  echo "      environment is not active or not current." >&2
  exit 1
fi
printf '  %-26s %s\n' "pkg-config" "$resolved_pkg_config"

failures=()
for mod in "${MODULES[@]}"; do
  if ! version="$(pkg-config --modversion "$mod" 2>/dev/null)"; then
    failures+=("$mod: not found by pkg-config at all")
    continue
  fi
  # `libdir` rather than `prefix`: every one of these 13 defines it, `libxdo`
  # reports an EMPTY prefix, and libdir is the value that becomes the linker's
  # -L flag and therefore the binary's RUNPATH.
  libdir="$(pkg-config --variable=libdir "$mod" 2>/dev/null || true)"
  case "$libdir" in
    /nix/store/?*)
      printf '  %-26s %-12s %s\n' "$mod" "$version" "$libdir"
      ;;
    *)
      failures+=("$mod: $version resolved libdir='${libdir:-<empty>}', which is not under /nix/store")
      ;;
  esac
done

if [ "${#failures[@]}" -ne 0 ]; then
  {
    echo
    echo "FAIL: pkg-config resolved Tauri's GTK stack OUTSIDE /nix/store."
    echo
    for f in ${failures[@]+"${failures[@]}"}; do
      echo "  - $f"
    done
    echo
    echo "This is issue #815's silent failure, caught at the point the wrong"
    echo "answer is given. Left alone it links dot-agent-deck-desktop against"
    echo "the host's GTK, which cannot load under Nix glibc, and surfaces much"
    echo "later as: libgdk-3.so.0: cannot open shared object file"
    echo
    echo "MOST LIKELY CAUSE: a stale devbox environment."
    echo "  \`devbox run\` REUSES an already-computed devbox environment when one"
    echo "  is inherited — DEVBOX_PATH_STACK is the trigger — and then neither"
    echo "  re-installs the profile nor re-applies devbox.json's \`env\` block."
    echo "  So a shell entered before devbox.json last changed keeps a profile"
    echo "  with no tauri-deps and no Nix pkg-config, and every nested"
    echo "  \`devbox run\` inside it inherits that staleness silently."
    echo
    echo "REMEDY: leave every nested devbox shell, then re-enter one:"
    echo "    exit                     # until DEVBOX_PATH_STACK is unset"
    echo "    devbox install           # refresh the profile"
    echo "    devbox shell             # or a fresh \`devbox run -- …\`"
    echo
    echo "If it still fails from a clean shell, devbox.json has genuinely lost"
    echo "its \`path:tauri-deps#tauri-deps\` entry or \`pkg-config@0.29.2\`, or"
    echo "tauri-deps/flake.nix stopped yielding the closure. Do NOT paper over"
    echo "it with LD_LIBRARY_PATH: issue #771 measured that pointing it at"
    echo "/usr/lib fixes the Rust gates and breaks Nix's node with"
    echo "\`undefined symbol: uv_tcp_keepalive_ex\`, taking pnpm with it."
  } >&2
  exit 1
fi

echo "  OK: all ${#MODULES[@]} modules resolve under /nix/store"
