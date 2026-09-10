#!/usr/bin/env bash
#
# Per-worktree attribution sampler (issue #906).
#
# Companion to sample-box.sh, which counts toolchain processes machine-wide
# and therefore sees THAT two builds overlapped but not WHOSE. This resolves
# each process to the worktree it is building by reading /proc/<pid>/cwd, so
# concurrency can be measured from the box rather than inferred from agents'
# self-reported gate windows.
#
# Why that matters: dispatching two agents together does not make them build
# together. They drift out of phase within minutes, and the expensive gates are
# a small fraction of wall time -- so a level labelled "N=2" can contain almost
# no real contention. Without this, a run showing N=1-like gate times is
# ambiguous between "concurrency is free" and "the gates never met".
#
# Usage: sample-attribution.sh [--interval SECONDS] [--out FILE] [--root DIR]
#
# Reads only /proc. Starts nothing, holds no link slot.

set -u

interval=5
out="attribution.tsv"
root="${HOME}/code"

while [ $# -gt 0 ]; do
    case "$1" in
        --interval) interval="${2:?}"; shift 2 ;;
        --out)      out="${2:?}"; shift 2 ;;
        --root)     root="${2:?}"; shift 2 ;;
        -h|--help)  sed -n '2,18p' "$0"; exit 0 ;;
        *) echo "sample-attribution.sh: unknown argument: $1" >&2; exit 2 ;;
    esac
done

case "$interval" in ''|*[!0-9]*|0) echo "--interval must be a positive integer" >&2; exit 2 ;; esac

if [ ! -e "$out" ]; then
    printf 'epoch\tiso\tworktree\tprocs\trss_kB_min\tbuilding\n' > "$out"
fi

echo "sample-attribution.sh: sampling every ${interval}s into $out" >&2
trap 'echo "sample-attribution.sh: stopped" >&2; exit 0' INT TERM

while :; do
    epoch=$(date -u +%s)
    iso=$(date -u +%Y-%m-%dT%H:%M:%SZ)

    # comm + pid + rss for every toolchain process, then resolve cwd per pid.
    # A process that exits between the ps and the readlink simply drops out;
    # that is a sampling miss, not an error, so failures are silent.
    #
    # Same comm list as sample-box.sh's toolchain_rss_kb, and a LOWER BOUND for
    # the same reason -- the driver reports `comm=gcc` even when invoked as
    # `cc`, so matching only `cc` matches nothing; a name not listed here
    # contributes nothing and looks like an idle worktree. Keep the two lists
    # in step: they are compared against each other during a run.
    ps -eo pid=,comm=,rss= 2>/dev/null | awk '
        $2=="rustc"||$2=="cargo"||$2=="ld"||$2=="ld.lld"||$2=="collect2"||$2=="cc1"||$2=="cc1plus"||$2=="mold"||
        $2=="cc"||$2=="gcc"||$2=="clang"||$2=="c++"||$2=="g++"||$2=="clang++" {print $1, $3}' \
    | while read -r pid rss; do
        cwd=$(readlink "/proc/$pid/cwd" 2>/dev/null) || continue
        printf '%s\t%s\n' "$cwd" "$rss"
      done \
    | awk -v root="$root" '
        BEGIN {
          # Match on "<root>/" rather than a bare "<root>", so the prefix test
          # cannot straddle a path component: with root=/home/u/code, a bare
          # prefix also matched /home/u/code2/..., folding an unrelated build
          # into this run under an empty-string key. A silently wrong number is
          # exactly what this protocol exists to prevent.
          #
          # Trailing slashes are stripped first, so the separator is appended
          # once however the caller spelled --root. The old fixed +2 offset
          # assumed no trailing slash and ate the first character of the
          # worktree name when tab-completion supplied one.
          sub(/\/+$/, "", root)
          prefix = root "/"
        }
        {
          wt = $1
          # collapse any path inside a worktree to the worktree root itself
          if (substr(wt, 1, length(prefix)) == prefix) {
            rest = substr(wt, length(prefix) + 1)
            n = index(rest, "/")
            wt = (n ? substr(rest, 1, n - 1) : rest)
          } else {
            wt = "other"
          }
          # The root itself, and a stray "<root>//x", leave nothing to name.
          if (wt == "") { wt = "other" }
          procs[wt]++; rss[wt] += $2
        }
        END { for (w in procs) printf "%s\t%s\t%s\n", w, procs[w], rss[w] }' \
    | while IFS=$'\t' read -r wt procs rsskb; do
        printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$epoch" "$iso" "$wt" "$procs" "$rsskb" "yes" >> "$out"
      done

    sleep "$interval"
done
