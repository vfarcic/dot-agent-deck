#!/usr/bin/env bash
#
# Run a command on RWX with `/`, `/var`, `/tmp` and `/var/tmp` on a filesystem
# the composer probe's export path rule accepts, so the e2e tests that need an
# input box proved empty take the supported export-chain branch there, as they
# do on GitHub's runners (docs/develop/rwx.md, "The export chain").
#
# Usage: scripts/rwx-export-root.sh COMMAND [ARG...]
#
# Why: `verify_export` (src/composer_probe/path_rule.rs) checks every directory
# from `/` down to the export against its filesystem allow-list (ext2/3/4, XFS,
# Btrfs, tmpfs). An RWX task's `/` is overlayfs, so no export anywhere in the
# task passes and the deck, correctly, never claims the box empty. The rule is
# not the thing to change; the task's root is.
#
# What it does: in a private mount namespace (only this command and its
# children see it), it builds a new root on one fresh tmpfs. `/` and `/var` are
# plain directories on it, owned by root and 0755; `/tmp` and `/var/tmp` are
# empty directories on it, owned by root and 1777, as on a stock Linux box.
# Every other entry of `/` and of `/var` is the task's own, bind-mounted
# (recursively) or re-created as the same symlink, so the workspace, `$HOME`,
# the toolchain, `/proc`, `/dev` and `/sys` are where they were. It then
# chroots into that root as the calling user, with the calling user's groups,
# working directory and exact environment, and runs the command. Nothing the
# command writes under `/tmp` or `/var/tmp` outlives it (the tmpfs is
# RAM-backed and goes with the namespace).
#
# It needs passwordless `sudo` with CAP_SYS_ADMIN and CAP_SYS_CHROOT, which an
# RWX task's sudo has (measured 2026-11-03), and refuses to run anywhere `RWX`
# is not `true`: on a developer's box or a GitHub runner `/` is already on a
# supported filesystem, and this is not something to run with sudo there.
#
# DAD_EXPORT_ROOT_SIZE sets the tmpfs's size (default 50%, of RAM).

set -euo pipefail

die() {
  echo "rwx-export-root: $*" >&2
  exit 1
}

script=$(readlink -f "$0")

case ${DAD_EXPORT_ROOT_STAGE:-outer} in
outer)
  [ $# -ge 1 ] || die "usage: rwx-export-root.sh COMMAND [ARG...]"
  [ "$(uname -s)" = Linux ] || die "Linux only"
  [ "${RWX:-}" = true ] || die "RWX is not 'true': this is for RWX tasks and sandboxes only"
  sudo -n true 2>/dev/null || die "needs passwordless sudo"
  # The exact environment, NUL-separated, restored after the chroot: sudo
  # resets PATH and HOME and drops what its env_delete list names.
  envfile=$(mktemp)
  env -0 > "$envfile"
  exec sudo -n env \
    DAD_EXPORT_ROOT_STAGE=mount \
    DAD_EXPORT_ROOT_ENV="$envfile" \
    DAD_EXPORT_ROOT_UID="$(id -u)" \
    DAD_EXPORT_ROOT_GID="$(id -g)" \
    DAD_EXPORT_ROOT_GROUPS="$(id -G | tr ' ' ,)" \
    DAD_EXPORT_ROOT_CWD="$PWD" \
    DAD_EXPORT_ROOT_SIZE="${DAD_EXPORT_ROOT_SIZE:-50%}" \
    unshare --mount --propagation private -- "$script" "$@"
  ;;

mount)
  # Root, inside the private mount namespace.
  root=/.dad-export-root
  mkdir -p "$root"
  mount -t tmpfs -o "mode=0755,size=$DAD_EXPORT_ROOT_SIZE,nosuid,nodev,exec" dad-export-root "$root"

  # Mirror every entry of SRC into DEST except the names after it.
  mirror() {
    local src=$1 dest=$2 path name skip
    shift 2
    shopt -s nullglob dotglob
    for path in "$src"/*; do
      name=${path##*/}
      for skip in "$@"; do
        if [ "$name" = "$skip" ]; then continue 2; fi
      done
      if [ -L "$path" ]; then
        ln -s "$(readlink "$path")" "$dest/$name"
      elif [ -d "$path" ]; then
        mkdir "$dest/$name"
        mount --rbind "$path" "$dest/$name"
      else
        touch "$dest/$name"
        mount --bind "$path" "$dest/$name"
      fi
    done
    shopt -u nullglob dotglob
  }

  mirror "" "$root" "${root#/}" tmp var
  mkdir -m 0755 "$root/var"
  mirror /var "$root/var" tmp
  mkdir -m 1777 "$root/tmp" "$root/var/tmp"

  # Hand the environment file to the user inside the new root, where the run
  # stage reads and removes it.
  inner_env=/tmp/.dad-export-root-env.$$
  install -m 0600 -o "$DAD_EXPORT_ROOT_UID" -g "$DAD_EXPORT_ROOT_GID" \
    "$DAD_EXPORT_ROOT_ENV" "$root$inner_env"
  rm -f -- "$DAD_EXPORT_ROOT_ENV"

  exec chroot --userspec="$DAD_EXPORT_ROOT_UID:$DAD_EXPORT_ROOT_GID" \
    --groups="$DAD_EXPORT_ROOT_GROUPS" "$root" \
    /usr/bin/env -i \
    DAD_EXPORT_ROOT_STAGE=run \
    DAD_EXPORT_ROOT_ENV="$inner_env" \
    DAD_EXPORT_ROOT_CWD="$DAD_EXPORT_ROOT_CWD" \
    /bin/bash "$script" "$@"
  ;;

run)
  # The calling user again, inside the new root.
  envfile=$DAD_EXPORT_ROOT_ENV
  cwd=$DAD_EXPORT_ROOT_CWD
  unset DAD_EXPORT_ROOT_STAGE DAD_EXPORT_ROOT_ENV DAD_EXPORT_ROOT_CWD
  while IFS= read -r -d '' kv; do
    name=${kv%%=*}
    # Skip what bash cannot export by name (exported functions, for one).
    if [[ $name =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]]; then export "$kv"; fi
  done < "$envfile"
  rm -f -- "$envfile"
  cd "$cwd"
  exec "$@"
  ;;

*)
  die "unknown stage: $DAD_EXPORT_ROOT_STAGE"
  ;;
esac
