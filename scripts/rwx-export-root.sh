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
# user's groups, working directory and environment variables (those bash can
# export by name), and runs the command. A variable that points into the
# task's own `/tmp` or `/var/tmp` (TMPDIR, SSH_AUTH_SOCK) now points into the
# new, empty ones.
#
# When the command exits, or this script is stopped by TERM, HUP or INT (which
# it forwards to the command and then waits for it), it unmounts every bind
# and then deletes only what it made: the contents of the new `/tmp` and
# `/var/tmp`, the entries it created, and the directory itself. It never
# removes anything recursively but those two temp directories, and removes
# nothing at all unless the mount table says nothing is mounted under the
# root any more. If it cannot read the table, or a mount will not detach, it
# leaves the root in place and says so ("left ... in place").
#
# What it cannot clean up: a SIGKILL of the mount stage, or a crash of the
# task, leaves the `dad-export-root.*` directory on the task-data mount, with
# whatever the run wrote to its `/tmp` and `/var/tmp`. `cargo xtask
# clean-e2e-tmp` does not look there. In a CI task it goes with the task; in a
# sandbox it stays until the sandbox stops (docs/develop/rwx.md says how to
# find and remove one).
#
# The environment is never written to disk: the mount stage, as root, reads
# it from `/proc/<pid>/environ` of the `sudo` this script execs into (same
# pid), and hands the run stage an open descriptor to it.
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
# A success of this script does not prove the tests took the supported
# branch: lane 1 passes on the unsupported one too. The `export-chain branch:
# supported` lines that `probe_026` and `probe_046` print are the check.
#
# DAD_EXPORT_ROOT_BASE overrides the directory the root is made in; it must be
# on ext2/3/4, XFS or Btrfs.

set -euo pipefail

die() {
  echo "rwx-export-root: $*" >&2
  exit 1
}

warn() {
  echo "rwx-export-root: $*" >&2
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
  # Canonical, so the mount stage's check of the mount table, which lists
  # canonical paths, compares like with like.
  canonical=$(readlink -e -- "$base") || die "$base does not exist"
  base=$canonical
  [ -d "$base" ] || die "$base is not a directory"
  case $(stat -f -c %T "$base") in
  xfs | ext2/ext3 | btrfs) ;;
  *) die "$base is on $(stat -f -c %T "$base"), not a disk-backed filesystem the export path rule accepts" ;;
  esac
  # `exec` keeps this pid, so the mount stage finds the caller's environment
  # in /proc/$$/environ of the sudo below: sudo resets PATH and HOME and drops
  # what its env_delete list names, and passing it any other way would put it
  # on disk or in an argv every user can read.
  exec sudo -n env \
    DAD_EXPORT_ROOT_STAGE=mount \
    DAD_EXPORT_ROOT_BASE="$base" \
    DAD_EXPORT_ROOT_PID="$$" \
    DAD_EXPORT_ROOT_UID="$(id -u)" \
    DAD_EXPORT_ROOT_GID="$(id -g)" \
    DAD_EXPORT_ROOT_GROUPS="$(id -G | tr ' ' ,)" \
    DAD_EXPORT_ROOT_CWD="$PWD" \
    unshare --mount --propagation private -- "$script" "$@"
  ;;

mount)
  # Root, inside the private mount namespace.
  root=
  child=
  interrupted=
  made=()

  # 0: something is mounted at or under $root; 1: nothing is; 2: cannot tell.
  # Reads this namespace's mountinfo, whose mount points escape space, tab,
  # newline and backslash as octal, so $root is escaped the same way first.
  mounted_under_root() {
    local info mp enc=$root
    enc=${enc//\\/\\134}
    enc=${enc// /\\040}
    enc=${enc//$'\t'/\\011}
    enc=${enc//$'\n'/\\012}
    info=$(cat /proc/self/mountinfo) || return 2
    [ -n "$info" ] || return 2
    while read -r _ _ _ _ mp _; do
      if [ "$mp" = "$enc" ] || [[ $mp == "$enc"/* ]]; then return 0; fi
    done <<<"$info"
    return 1
  }

  # Unmount the root and every bind under it, then, only if the mount table
  # shows nothing left under it, delete what this script made and nothing
  # else: a recursive removal through a bind still in place would delete the
  # task's own files.
  cleanup() {
    local state=0 entry i
    [ -n "$root" ] || return 0
    umount -R "$root" 2>/dev/null || umount -R -l "$root" 2>/dev/null || true
    mounted_under_root || state=$?
    case $state in
    0)
      warn "left $root in place: a mount under it did not detach"
      return 0
      ;;
    2)
      warn "left $root in place: could not read the mount table"
      return 0
      ;;
    esac
    # The two temp directories are the only ones the run could fill, and
    # nothing is ever mounted on them.
    rm -rf --one-file-system -- "$root/tmp" "$root/var/tmp" || true
    for ((i = ${#made[@]} - 1; i >= 0; i--)); do
      entry=${made[i]}
      if [ -L "$entry" ] || [ -f "$entry" ]; then
        rm -f -- "$entry" || true
      elif [ -d "$entry" ]; then
        rmdir -- "$entry" || true
      fi
    done
    if [ -d "$root/var" ]; then rmdir -- "$root/var" || true; fi
    if ! rmdir -- "$root"; then
      warn "left $root in place: it holds something this script did not make"
    fi
  }

  # A signal before the command starts ends the script (and so runs
  # cleanup); once it runs, the signal goes to the command, and the loop
  # below waits for it to exit before cleaning up.
  on_signal() {
    if [ -n "$child" ]; then
      interrupted=1
      kill -s "$1" "$child" 2>/dev/null || true
    else
      exit $((128 + $(kill -l "$1")))
    fi
  }

  trap cleanup EXIT
  trap 'on_signal TERM' TERM
  trap 'on_signal HUP' HUP
  trap 'on_signal INT' INT

  # The caller's environment, from the sudo the outer stage exec'd into.
  # Check it is an ancestor first, so a wrong pid is a refusal rather than
  # somebody else's environment.
  pid=$$
  while [ "$pid" != "$DAD_EXPORT_ROOT_PID" ]; do
    [ "$pid" -gt 1 ] || die "pid $DAD_EXPORT_ROOT_PID is not an ancestor of the mount stage"
    pid=$(sed -n 's/^PPid:[[:space:]]*//p' "/proc/$pid/status")
    [ -n "$pid" ] || die "cannot read the parent of a mount-stage ancestor"
  done
  exec 3<"/proc/$DAD_EXPORT_ROOT_PID/environ"

  root=$(mktemp -d "$DAD_EXPORT_ROOT_BASE/dad-export-root.XXXXXX")
  chmod 0755 "$root"
  # A mount of its own, so `/` inside it is a mount point as everywhere else:
  # code that finds a path's filesystem in /proc/mounts finds none for a
  # chroot that is a plain directory (xtask-cross-version's
  # the_root_filesystem_is_identified). Every bind below hangs off it, so
  # one recursive unmount detaches them all.
  mount --bind "$root" "$root"

  # Mirror every entry of SRC into DEST except the names after it, recording
  # each entry it creates for cleanup.
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
        made+=("$dest/$name")
      elif [ -d "$path" ]; then
        mkdir "$dest/$name"
        made+=("$dest/$name")
        mount --rbind "$path" "$dest/$name"
      else
        touch "$dest/$name"
        made+=("$dest/$name")
        mount --bind "$path" "$dest/$name"
      fi
    done
    shopt -u nullglob dotglob
  }

  mirror "" "$root" tmp var
  mkdir -m 0755 "$root/var"
  mirror /var "$root/var" tmp
  mkdir -m 1777 "$root/tmp" "$root/var/tmp"

  # In the background so the traps above can forward a signal; `<&0` keeps
  # stdin, which a background command otherwise gets as /dev/null, and
  # `--default-signal` undoes the SIGINT/SIGQUIT ignore bash gives one.
  chroot --userspec="$DAD_EXPORT_ROOT_UID:$DAD_EXPORT_ROOT_GID" \
    --groups="$DAD_EXPORT_ROOT_GROUPS" "$root" \
    /usr/bin/env -i --default-signal=INT,QUIT \
    DAD_EXPORT_ROOT_STAGE=run \
    DAD_EXPORT_ROOT_CWD="$DAD_EXPORT_ROOT_CWD" \
    /bin/bash "$script" "$@" <&0 &
  child=$!
  exec 3<&-
  status=0
  while :; do
    interrupted=
    if wait "$child"; then status=0; else status=$?; fi
    # A forwarded signal interrupts `wait` before the command has exited.
    [ -n "$interrupted" ] || break
  done
  exit "$status"
  ;;

run)
  # The calling user again, inside the new root; descriptor 3 is the
  # caller's environment, NUL-separated.
  cwd=$DAD_EXPORT_ROOT_CWD
  unset DAD_EXPORT_ROOT_STAGE DAD_EXPORT_ROOT_CWD
  count=0
  while IFS= read -r -d '' kv; do
    name=${kv%%=*}
    # Skip what bash cannot export by name (exported functions, for one).
    if [[ $name =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]]; then
      export "$kv"
      count=$((count + 1))
    fi
  done <&3
  exec 3<&-
  [ "$count" -gt 0 ] || die "read no environment from the mount stage"
  cd "$cwd"
  exec "$@"
  ;;

*)
  die "unknown stage: $DAD_EXPORT_ROOT_STAGE"
  ;;
esac
