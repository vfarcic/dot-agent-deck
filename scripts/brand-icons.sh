#!/bin/sh
# Regenerate every derived copy of the Agent Deck mark from its one master,
# assets/brand/logo.svg: the docs navbar logo, the desktop rail badge, the
# full Tauri icon set, and the docs favicon. Run from anywhere; needs the
# desktop's node dependencies (`pnpm --dir desktop install`). The lockups are
# separate -- scripts/brand-lockups.py -- because they need a font toolchain.
# docs/develop/brand-assets.md has the rest.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
master="$root/assets/brand/logo.svg"
icons="$root/desktop/src-tauri/icons"

cp "$master" "$root/site/static/img/logo.svg"
mkdir -p "$root/desktop/src/assets"
cp "$master" "$root/desktop/src/assets/logo.svg"

# `pnpm exec`, not `pnpm tauri`: the latter runs the `pretauri` hook, which
# builds and syncs the daemon binary -- irrelevant to icons and slow.
pnpm --dir "$root/desktop" exec tauri icon "$master" --output "$icons"

# `tauri icon` also writes Android and iOS sets and the Windows Store tiles
# (Square*Logo.png, StoreLogo.png). This app ships none of those packages, so
# they would only be unreferenced binaries in the tree. What stays is exactly
# what desktop/src-tauri/tauri.conf.json's `bundle.icon` lists.
rm -rf "$icons/android" "$icons/ios" "$icons"/Square*Logo.png "$icons/StoreLogo.png"

# The Windows .ico carries 16/24/32/48/64/256px frames, which is exactly what
# a favicon wants, so the docs site reuses it rather than building its own.
cp "$icons/icon.ico" "$root/site/static/img/favicon.ico"

echo "brand assets regenerated from $master"
