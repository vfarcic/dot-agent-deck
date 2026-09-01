#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/../.." && pwd)
profile=${1:-release}
target_triple=${TAURI_ENV_TARGET_TRIPLE:-$(rustc -vV | sed -n 's/^host: //p')}

if [ -z "$target_triple" ]; then
  echo "could not determine the Rust target triple" >&2
  exit 1
fi

case "$profile" in
  debug)
    cargo build --locked --manifest-path "$repo_root/Cargo.toml" --bin dot-agent-deck --target "$target_triple"
    artifact_dir=debug
    ;;
  release)
    cargo build --locked --release --manifest-path "$repo_root/Cargo.toml" --bin dot-agent-deck --target "$target_triple"
    artifact_dir=release
    ;;
  *)
    echo "usage: $0 [debug|release]" >&2
    exit 2
    ;;
esac

# Windows needs the suffix in BOTH places and each one is fatal on its own:
# cargo emits `dot-agent-deck.exe` on a `*-pc-windows-*` target, so the copy
# below has no source without it; and Tauri resolves an `externalBin` entry as
# `{path}-{target_triple}{ext}` with ext=`.exe` on Windows, so a file staged
# under the bare name does not satisfy the manifest even once the source is
# found. Covered by `xtask/linkage-check/src/sidecar_staging.rs`.
case "$target_triple" in
  *-pc-windows-*) exe_suffix=.exe ;;
  *)              exe_suffix= ;;
esac

source_binary="$repo_root/target/$target_triple/$artifact_dir/dot-agent-deck$exe_suffix"
destination_dir="$repo_root/desktop/src-tauri/binaries"
destination_binary="$destination_dir/dot-agent-deck-$target_triple$exe_suffix"

mkdir -p "$destination_dir"
cp "$source_binary" "$destination_binary"
chmod 755 "$destination_binary"
echo "prepared matching daemon sidecar: $destination_binary"
