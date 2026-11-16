#!/usr/bin/env bash
# Dev box cleanup, installed by scripts/box/bootstrap.sh as ~/.local/bin/dad-box-cleanup.
# Runs from the dad-box-cleanup.timer user unit, every 30 minutes. Complements
# `cargo xtask clean-targets`, which removes whole idle target/ trees but only
# when a dispatch or release run invokes it (docs/develop/target-cleanup.md). Frees what parallel dispatch
# units leave behind, skipping what a process was found using. That check reads
# /proc once per run (scratchpad files are checked again right before removal),
# so a file opened after it is not seen:
#   1. stale test binaries: extensionless executables >20 MB in any
#      */target/*/deps under a ~/code/dot-agent-deck* worktree with no build
#      running, older than STALE_MIN minutes, not currently executed by any
#      process (cargo keeps every old copy forever). Libraries (*.so proc-macros,
#      *.rlib) are never touched: cargo does not rebuild a deleted proc-macro and
#      the next build fails with "can't find crate" (2026-10-17);
#   2. incremental caches: */target/debug/incremental of a worktree with no
#      cargo/rustc/linker process in it and no target writes for IDLE_MIN min;
#   3. scratchpad copies: files >100 MB in Claude session scratchpads on the
#      /tmp tmpfs, older than SCRATCH_MIN minutes, not executed or held open.
set -u
CODE_ROOT="${CODE_ROOT:-$HOME/code}"
STALE_MIN="${STALE_MIN:-360}"
IDLE_MIN="${IDLE_MIN:-60}"
SCRATCH_MIN="${SCRATCH_MIN:-60}"
LOG="${LOG:-$HOME/.local/state/dad-box-cleanup.log}"
mkdir -p "$(dirname "$LOG")"

# The filesystem the worktrees are on, which on a box with a separate /home is not /.
free_g() { df --output=avail -BG "$CODE_ROOT" | tail -1 | tr -dc 0-9; }
tmp_pct() { df --output=pcent /tmp | tail -1 | tr -dc 0-9; }
before=$(free_g); tmp_before=$(tmp_pct)

# Every executable currently running, and every file currently held open.
in_use=$(mktemp); trap 'rm -f "$in_use"' EXIT
for p in /proc/[0-9]*; do
  readlink "$p/exe" 2>/dev/null
  for fd in "$p"/fd/*; do readlink "$fd" 2>/dev/null; done
done | sort -u > "$in_use"
is_used() { grep -qxF -- "$1" "$in_use"; }

# A build is running in a worktree when a build tool's cwd is inside it, or when
# anything holds a file under its target/ open: cargo keeps target/*/.cargo-lock
# open for the whole build, including one started elsewhere with --manifest-path.
build_running_in() {
  local wt="$1" p
  grep -qF -- "$wt/target/" "$in_use" && return 0
  for p in /proc/[0-9]*; do
    case "$(readlink "$p/cwd" 2>/dev/null)" in
      "$wt"|"$wt"/*)
        case "$(cat "$p/comm" 2>/dev/null)" in cargo*|rustc|ld*|cc|clippy-driver) return 0;; esac;;
    esac
  done
  return 1
}

bins=0
for deps in "$CODE_ROOT"/dot-agent-deck*/target/*/deps; do
  [ -d "$deps" ] || continue
  # A symlinked target/ is another tree's, which this worktree's checks do not cover.
  [ -L "${deps%/target/*}/target" ] && continue
  build_running_in "${deps%/target/*}" && continue
  while IFS= read -r -d '' f; do
    is_used "$f" && continue
    rm -f -- "$f" && bins=$((bins + 1))
  done < <(find "$deps" -maxdepth 1 -type f -executable ! -name '*.*' -size +20M -mmin +"$STALE_MIN" -print0 2>/dev/null)
done

incr=0
for wt in "$CODE_ROOT"/dot-agent-deck*/; do
  wt=${wt%/}
  inc="$wt/target/debug/incremental"
  [ -d "$inc" ] || continue
  [ -L "$wt/target" ] && continue
  build_running_in "$wt" && continue
  # Idle means no write near the top of target/ and none anywhere in the cache,
  # whose session files sit deeper than the first check reaches.
  [ -n "$(find "$wt/target" -maxdepth 3 -newermt "-$IDLE_MIN min" -print -quit 2>/dev/null)" ] && continue
  [ -n "$(find "$inc" -newermt "-$IDLE_MIN min" -print -quit 2>/dev/null)" ] && continue
  rm -rf -- "$inc" && incr=$((incr + 1))
done

# Whether any process holds $1 open now, read fresh rather than from the snapshot.
open_now() {
  local fd
  for fd in /proc/[0-9]*/fd/*; do [ "$(readlink "$fd" 2>/dev/null)" = "$1" ] && return 0; done
  return 1
}

scratch=0
while IFS= read -r -d '' f; do
  is_used "$f" && continue
  open_now "$f" && continue
  rm -f -- "$f" && scratch=$((scratch + 1))
done < <(find /tmp/claude-"$(id -u)" -path '*/scratchpad/*' -type f -size +100M -mmin +"$SCRATCH_MIN" -print0 2>/dev/null)

after=$(free_g); tmp_after=$(tmp_pct)
echo "$(date -Is) stale_bins=$bins incremental_dirs=$incr scratch_files=$scratch disk_free=${before}G->${after}G tmpfs=${tmp_before}%->${tmp_after}%" >> "$LOG"
