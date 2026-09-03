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
# --all-targets --features e2e,e2e-live` and `cargo test-fast` both build
# `dot-agent-deck-desktop` — which needs GTK 3, WebKitGTK and glib. devbox.json
# carried none of them, so BOTH mandated gates were unrunnable in a devbox shell
# on Linux until the `path:tauri-deps#tauri-deps` entry was added.
#
# This job is the only one that can observe that regressing: every other job
# installs the compile set with apt (ci.yml's `build`) and would stay green with
# devbox.json empty of GTK.
#
# The assertion lives in its own script so it can be TESTED. Issue #815: what
# used to be here resolved each module with `pkg-config --modversion` and
# printed the result, which passes just as happily against the host's
# `/usr/lib/pkgconfig` as against the store — so the one failure mode that
# actually shipped was invisible to it. `devbox-check-gtk.sh` asserts the
# resolved `libdir` is under /nix/store instead, and
# `xtask/linkage-check/src/devbox_gtk_origin.rs` drives it under a stubbed
# pkg-config to prove it still rejects a `/usr/lib` answer.
#
# Note what does NOT have to be right for this to pass, contrary to what this
# comment claimed before #815: `PKG_CONFIG_PATH`. Nix's pkg-config WRAPPER —
# which is what `pkg-config@0.29.2` puts on PATH — discards the caller's
# `PKG_CONFIG_PATH` outright and rebuilds it from the role-mangled
# `PKG_CONFIG_PATH_FOR_TARGET` that its own setup hook populates from the
# profile. devbox.json's `env` block is a fallback for a non-wrapper
# pkg-config, not the mechanism; the load-bearing part is the package entry.
bash "$(dirname "$0")/devbox-check-gtk.sh"

echo "== recording toolchain =="
asciinema --version
agg --version
ffmpeg -version | head -1

echo "OK: devbox environment is usable"
