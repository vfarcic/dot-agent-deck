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

echo "== recording toolchain =="
asciinema --version
agg --version
ffmpeg -version | head -1

echo "OK: devbox environment is usable"
