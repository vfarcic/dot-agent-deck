#!/usr/bin/env bash
#
# Prove the devbox environment is usable: every tool the dev shell promises
# resolves and runs. CI's `devbox` job invokes this inside `devbox run`, which
# is the only check that can observe a devbox.json / devbox.lock change —
# every other job installs its toolchain directly (dtolnay/rust-toolchain,
# taiki-e/install-action, cargo install) and never touches devbox.
#
# Most of the value is already banked before this script starts: `devbox
# install` has to resolve the lock, so a dropped nixpkgs entry or a missing
# system fails earlier. What this adds is proof the binaries actually RUN,
# rather than merely having been fetched.
#
# Usage: devbox run -- bash scripts/devbox-smoke.sh
#
# It must be invoked with a FILE, not an inline `bash -c '…'` script: devbox
# flattens its trailing args into one string and re-parses them through a
# shell, so a quoted multi-line argument is destroyed before bash sees it
# (devbox 0.17.5 fails with `invalid version 'set'` or `n: command not found`
# depending on where the mangled text lands). A plain filename has no quoting
# to lose.
#
# NOTE: `vals version` proves the binary runs, NOT that the gcpsecrets provider
# is still compiled in — that needs GCP credentials. The devbox init_hook only
# calls `vals env` when USE_VALS is set, which CI never sets, so nothing here
# reads .env.vals.yaml or needs auth.

set -euo pipefail

echo "== rust toolchain =="
rustc --version
cargo --version
cargo clippy --version
rustfmt --version
cargo nextest --version

echo "== repo tooling =="
vals version
gh --version
task --version
jq --version
rg --version

echo "== desktop gui toolchain =="
# The Tauri preview under desktop/ is the only consumer; pinned in devbox.json
# so contributors do not hand-install a JS toolchain (PRD #176).
node --version
pnpm --version

echo "== tauri system libraries =="
# Issue #771. `desktop/src-tauri` became a workspace member in daf94f0, and both
# gates CLAUDE.md mandates carry `--workspace`, so `cargo clippy --workspace
# --all-targets --features e2e` and `cargo test-fast` both build
# `dot-agent-deck-desktop` — which needs GTK 3, WebKitGTK and glib. devbox.json
# carried none of them, so BOTH mandated gates were unrunnable in a devbox shell
# on Linux until the `path:tauri-deps#tauri-deps` entry was added.
#
# This job is the only one that can observe that regressing: every other job
# installs the compile set with apt (ci.yml's `build`) and would stay green with
# devbox.json empty of GTK.
#
# It asserts through pkg-config rather than by looking for files, because
# pkg-config resolution is exactly what the -sys crates do, and because
# `PKG_CONFIG_PATH` — set from devbox.json's `env` block, not from `init_hook`,
# since `devbox run` does not execute the hook — is half of what has to be right.
#
# Linux only: macOS builds Tauri against the system WebKit, so the flake yields
# an empty output there and none of these modules exists.
if [ "$(uname -s)" = "Linux" ]; then
  pkg-config --version
  for mod in glib-2.0 gobject-2.0 gio-2.0 gtk+-3.0 gdk-3.0 gdk-x11-3.0 \
    webkit2gtk-4.1 javascriptcoregtk-4.1 libsoup-3.0 \
    ayatana-appindicator3-0.1 dbus-1 librsvg-2.0 libxdo; do
    # The ASSIGNMENT is what makes this an assertion: `set -e` sees a failed
    # command substitution in a simple assignment and exits, whereas the same
    # substitution inside a printf argument would have its status swallowed by
    # printf's own success.
    version="$(pkg-config --modversion "$mod")"
    printf '  %-26s %s\n' "$mod" "$version"
  done
else
  echo "skipped: not Linux"
fi

echo "== recording toolchain =="
asciinema --version
agg --version
ffmpeg -version | head -1

echo "OK: devbox environment is usable"
