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
# What it does: it makes a directory on the task's disk-backed XFS mount (the
# parent of `$RWX_ARTIFACTS`, `/opt/rwx/task-data/<task id>`, which RWX backs
# with the same disk as the task's overlay), and in a private mount namespace,
# which only this command and its children see, builds a new root there. `/`
# and `/var` are plain directories on it, owned by root and 0755; `/tmp` and
# `/var/tmp` are empty directories on it, owned by root and 1777, as on a stock
# Linux box. Every other entry of `/` and of `/var` is the task's own,
# bind-mounted (recursively) or re-created as the same symlink, so the
# workspace, `$HOME`, the toolchain, `/proc`, `/dev` and `/sys` are where they
# were. It then chroots into that root as the calling user, with the calling
# user's groups, working directory and exact environment, and runs the
# command. When the command exits it unmounts every bind, and removes the
# directory only once nothing is mounted under it.
#
# Not a tmpfs: one e2e shard leaves gigabytes of test roots under `/var/tmp`,
# which filled an 8 GB tmpfs (RWX run 01a1a2c522be782eb2580f0d1d1e6f3b), and
# code that refuses to put scratch on a RAM-backed `/` (CLAUDE.md rule 14)
# refused it.
#
# It needs passwordless `sudo` with CAP_SYS_ADMIN and CAP_SYS_CHROOT, which an
# RWX task's sudo has (measured 2026-11-03), and refuses to run anywhere `RWX`
# is not `true`: on a developer's box or a GitHub runner `/` is already on a
# supported filesystem, and this is not something to run with sudo there.
#
# DAD_EXPORT_ROOT_BASE overrides the directory the root is made in; it must be
# on ext2/3/4, XFS or Btrfs.

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
  base=${DAD_EXPORT_ROOT_BASE:-}
  if [ -z "$base" ]; then
    [ -n "${RWX_ARTIFACTS:-}" ] || die "RWX_ARTIFACTS is unset; set DAD_EXPORT_ROOT_BASE"
    base=$(dirname "$RWX_ARTIFACTS")
  fi
  case $(stat -f -c %T "$base") in
  xfs | ext2/ext3 | btrfs) ;;
  *) die "$base is on $(stat -f -c %T "$base"), not a disk-backed filesystem the export path rule accepts" ;;
  esac
  # The exact environment, NUL-separated, restored after the chroot: sudo
  # resets PATH and HOME and drops what its env_delete list names.
  envfile=$(mktemp)
  env -0 > "$envfile"
  exec sudo -n env \
    DAD_EXPORT_ROOT_STAGE=mount \
    DAD_EXPORT_ROOT_BASE="$base" \
    DAD_EXPORT_ROOT_ENV="$envfile" \
    DAD_EXPORT_ROOT_UID="$(id -u)" \
    DAD_EXPORT_ROOT_GID="$(id -g)" \
    DAD_EXPORT_ROOT_GROUPS="$(id -G | tr ' ' ,)" \
    DAD_EXPORT_ROOT_CWD="$PWD" \
    unshare --mount --propagation private -- "$script" "$@"
  ;;

mount)
  # Root, inside the private mount namespace.
  root=$(mktemp -d "$DAD_EXPORT_ROOT_BASE/dad-export-root.XXXXXX")
  chmod 0755 "$root"
  mounted=()

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
        mounted+=("$dest/$name")
      else
        touch "$dest/$name"
        mount --bind "$path" "$dest/$name"
        mounted+=("$dest/$name")
      fi
    done
    shopt -u nullglob dotglob
  }

  # Unmount every bind, then remove the root only if nothing is left mounted
  # under it: removing it with a bind still in place would delete the task's
  # own files through that bind.
  cleanup() {
    local target ok=1
    for target in "${mounted[@]}"; do
      umount -R "$target" 2>/dev/null || umount -R -l "$target" 2>/dev/null || ok=0
    done
    if [ "$ok" = 1 ] && ! findmnt -rn -o TARGET | grep -q -F -- "$root/"; then
      rm -rf --one-file-system -- "$root"
    else
      echo "rwx-export-root: left $root in place: a mount under it did not detach" >&2
    fi
  }

  mirror "" "$root" tmp var
  mkdir -m 0755 "$root/var"
  mirror /var "$root/var" tmp
  mkdir -m 1777 "$root/tmp" "$root/var/tmp"

  # Hand the environment file to the user inside the new root, where the run
  # stage reads and removes it.
  inner_env=/tmp/.dad-export-root-env.$$
  install -m 0600 -o "$DAD_EXPORT_ROOT_UID" -g "$DAD_EXPORT_ROOT_GID" \
    "$DAD_EXPORT_ROOT_ENV" "$root$inner_env"
  rm -f -- "$DAD_EXPORT_ROOT_ENV"

  status=0
  chroot --userspec="$DAD_EXPORT_ROOT_UID:$DAD_EXPORT_ROOT_GID" \
    --groups="$DAD_EXPORT_ROOT_GROUPS" "$root" \
    /usr/bin/env -i \
    DAD_EXPORT_ROOT_STAGE=run \
    DAD_EXPORT_ROOT_ENV="$inner_env" \
    DAD_EXPORT_ROOT_CWD="$DAD_EXPORT_ROOT_CWD" \
    /bin/bash "$script" "$@" || status=$?
  cleanup
  exit "$status"
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
